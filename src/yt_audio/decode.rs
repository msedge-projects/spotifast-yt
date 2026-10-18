//! Unwrapping and decoding of YouTube's streamed audio containers.
//!
//! Symphonia demuxes both containers YouTube serves (fragmented MP4 with
//! AAC, and WebM with Opus or the rare legacy Vorbis) and decodes AAC and
//! Vorbis itself. Opus has no symphonia decoder, so those packets go
//! through the pure-Rust Opus decoder instead. Everything leaves here as
//! interleaved stereo f64 at the chain's 44.1 kHz: mono is duplicated,
//! other rates go through the app's own stateful resampler, and the result
//! is cut into packets of about 512 frames.
//!
//! A seek is symphonia's accurate seek (which lands at the last packet
//! boundary at or before the target) followed by decoding and discarding
//! up to the exact time, computed from packet timestamps, so VBR streams
//! stay exact.

use std::io;

use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{
    CODEC_TYPE_AAC, CODEC_TYPE_OPUS, CODEC_TYPE_VORBIS, Decoder as SymphoniaDecoder, DecoderOptions,
};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, Packet, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

use rusty_opus::OpusDecoder;

use crate::resample::Resampler;
use crate::yt_audio::source::{self, LinkFailure};

/// Frames per outgoing packet. librespot's packets run 4 to 13 ms and the
/// sink's queue limits are counted in packets, so this keeps the same
/// backpressure feel (about 12 ms of sound per packet).
pub const CHUNK_FRAMES: usize = 512;

/// The chain's rate; everything leaving this module is in it.
const CHAIN_RATE: u32 = librespot_playback::SAMPLE_RATE;

/// The most samples per channel one Opus packet can hold (120 ms at
/// 48 kHz). The decoder writes into a buffer sized for the maximum.
const OPUS_MAX_FRAMES: usize = 5760;

/// Consecutive packet failures after which a stream counts as failed.
const ERROR_LIMIT: u32 = 10;

/// What the worker reports about a started stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    pub container: &'static str,
    pub codec: &'static str,
    /// The stream's own rate, before the conversion to 44.1 kHz.
    pub rate: u32,
    /// Whether the resampler runs for this stream.
    pub resampled: bool,
    /// The container's own duration when it states one.
    pub duration_ms: Option<u64>,
}

/// Why a stream could not play.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// This container or codec is not something the engine reads: the
    /// caller should fall through to the next resolved candidate.
    #[error("unsupported stream format: {0}")]
    Unsupported(String),
    /// The signed link died (expired or refused): only a fresh resolve
    /// helps.
    #[error("the stream link stopped working")]
    Expired,
    /// Anything else that broke mid-stream: a renewal or retry may help.
    #[error("{0}")]
    Stream(String),
}

/// One open stream: the demuxer, the decoder, and the conversion to the
/// chain's format.
pub struct Stream {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: FrameDecoder,
    track_id: u32,
    /// The track's units, for placing packet timestamps on a clock.
    time_base: Option<TimeBase>,
    /// The stream's own rate, before the conversion to the chain rate.
    rate: u32,
    /// Channels as decoded; anything but stereo is normalized on the way
    /// out.
    channels: usize,
    resampler: Option<Resampler>,
    /// Reused buffer for mono-to-stereo duplication.
    stereo: Vec<f32>,
    /// Converted samples not yet handed out.
    pending: Vec<f64>,
    /// Frames still to discard after a seek, in source frames.
    skip_frames: f64,
    /// After a seek, the first packet names where the reader landed and
    /// fixes `skip_frames`; until then this holds the target.
    skip_target_ms: f64,
    /// A seek is waiting for its first packet.
    armed: bool,
    decode_failures: u32,
    opened: Opened,
}

/// The decoder behind the demuxer: symphonia's own, or the pure-Rust Opus
/// decoder that symphonia does not have. Each keeps its own conversion
/// buffer so a decode borrows only the decoder.
enum FrameDecoder {
    Symphonia {
        decoder: Box<dyn SymphoniaDecoder>,
        sample: Option<SampleBuffer<f32>>,
        spec: Option<(u32, Channels)>,
    },
    Opus {
        decoder: Box<OpusDecoder>,
        out: Vec<f32>,
        channels: usize,
    },
}

impl Stream {
    /// Opens a stream. `hint` is the resolver's mime type when it had one;
    /// it only guides the container probe.
    pub fn open<S: MediaSource + 'static>(source: S, hint: Option<&str>) -> Result<Self, Error> {
        let mut probe_hint = Hint::new();
        if let Some(hint) = hint {
            probe_hint.mime_type(hint);
            if let Some(extension) = extension_for(hint) {
                probe_hint.with_extension(extension);
            }
        }
        let stream = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
        let probed = symphonia::default::get_probe()
            .format(
                &probe_hint,
                stream,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(open_error)?;
        let format = probed.format;

        let track = format
            .default_track()
            .ok_or_else(|| Error::Unsupported("the stream has no audio track".to_string()))?;
        let track_id = track.id;
        let time_base = track.codec_params.time_base;
        let params = track.codec_params.clone();
        let rate = params
            .sample_rate
            .ok_or_else(|| Error::Unsupported("the stream states no sample rate".to_string()))?;
        let channels = params
            .channels
            .map(|channels| channels.count())
            .unwrap_or(2);
        let container = container_for(hint, params.codec);

        let decoder = match params.codec {
            CODEC_TYPE_OPUS => FrameDecoder::Opus {
                decoder: Box::new(OpusDecoder::new(48_000, channels.clamp(1, 2)).map_err(
                    |error| Error::Stream(format!("the Opus decoder refused: {error}")),
                )?),
                out: Vec::new(),
                channels: channels.clamp(1, 2),
            },
            CODEC_TYPE_AAC | CODEC_TYPE_VORBIS => FrameDecoder::Symphonia {
                decoder: symphonia::default::get_codecs()
                    .make(&params, &DecoderOptions::default())
                    .map_err(|error| Error::Unsupported(error.to_string()))?,
                sample: None,
                spec: None,
            },
            codec => {
                return Err(Error::Unsupported(format!("codec 0x{codec:x}")));
            }
        };

        let resampler = Resampler::new(rate, CHAIN_RATE, 2);
        let duration_ms = rate_checked_duration(&params, rate);
        let opened = Opened {
            container,
            codec: codec_name(params.codec),
            rate,
            resampled: resampler.is_some(),
            duration_ms,
        };
        Ok(Self {
            format,
            decoder,
            track_id,
            time_base,
            rate,
            channels,
            resampler,
            stereo: Vec::new(),
            pending: Vec::new(),
            skip_frames: 0.0,
            skip_target_ms: 0.0,
            armed: false,
            decode_failures: 0,
            opened,
        })
    }

    /// What the stream turned out to be.
    pub fn opened(&self) -> Opened {
        self.opened
    }

    /// The next packet-sized chunk of interleaved stereo f64 at 44.1 kHz,
    /// or `None` at the end of the stream.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<f64>>, Error> {
        loop {
            if self.pending.len() >= CHUNK_FRAMES * 2 {
                let chunk: Vec<f64> = self.pending.drain(..CHUNK_FRAMES * 2).collect();
                return Ok(Some(chunk));
            }
            match self.fill_once()? {
                Fill::More => continue,
                Fill::End => {
                    if self.pending.is_empty() {
                        return Ok(None);
                    }
                    let chunk: Vec<f64> = self.pending.drain(..).collect();
                    return Ok(Some(chunk));
                }
            }
        }
    }

    /// Repositions to `position_ms`; the next chunks start there.
    pub fn seek(&mut self, position_ms: u64) -> Result<(), Error> {
        let time = Time {
            seconds: position_ms / 1000,
            frac: (position_ms % 1000) as f64 / 1000.0,
        };
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(stream_error)?;
        // The decoder's state belongs to the old position, and the
        // resampler's running filter must not carry sound across the cut.
        self.decoder.reset();
        self.resampler = Resampler::new(self.rate, CHAIN_RATE, 2);
        self.pending.clear();
        self.decode_failures = 0;
        self.skip_frames = 0.0;
        self.skip_target_ms = position_ms as f64;
        // The first packet after the seek fixes how far before the target
        // the reader actually landed.
        self.armed = true;
        Ok(())
    }
}

impl FrameDecoder {
    /// Decodes one packet into interleaved f32 at the source rate. The
    /// slice is valid until the next call.
    fn decode(&mut self, packet: &Packet) -> Result<&[f32], SymphoniaError> {
        match self {
            FrameDecoder::Symphonia {
                decoder,
                sample,
                spec,
            } => {
                let buffer = decoder.decode(packet)?;
                let buffer_spec = *buffer.spec();
                let key = (buffer_spec.rate, buffer_spec.channels);
                let reusable = *spec == Some(key)
                    && sample
                        .as_ref()
                        .is_some_and(|sample| sample.capacity() >= buffer.capacity() as u64);
                if !reusable {
                    *sample = Some(SampleBuffer::<f32>::new(
                        buffer.capacity() as u64,
                        buffer_spec,
                    ));
                    *spec = Some(key);
                }
                let sample = sample.as_mut().expect("just built");
                sample.copy_interleaved_ref(buffer);
                Ok(sample.samples())
            }
            FrameDecoder::Opus {
                decoder,
                out,
                channels,
            } => {
                let needed = OPUS_MAX_FRAMES * *channels;
                if out.len() < needed {
                    out.resize(needed, 0.0);
                }
                let decoded =
                    decoder
                        .decode(packet.buf(), OPUS_MAX_FRAMES, out)
                        .map_err(|error| {
                            SymphoniaError::DecodeError(format!(
                                "the Opus decoder refused: {error}"
                            ))
                        })?;
                Ok(&out[..decoded * *channels])
            }
        }
    }

    fn reset(&mut self) {
        match self {
            FrameDecoder::Symphonia { decoder, .. } => decoder.reset(),
            FrameDecoder::Opus {
                decoder, channels, ..
            } => {
                // A fresh Opus decoder has the pristine state a seek needs.
                if let Ok(fresh) = OpusDecoder::new(48_000, *channels) {
                    **decoder = fresh;
                }
            }
        }
    }
}

enum Fill {
    More,
    End,
}

impl Stream {
    /// Decodes one packet's worth of sound into `pending` (or reports the
    /// end of the stream).
    fn fill_once(&mut self) -> Result<Fill, Error> {
        let packet = match self.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(Fill::End);
            }
            Err(SymphoniaError::ResetRequired) => {
                return Err(Error::Stream("the stream's format changed".to_string()));
            }
            Err(error) => return Err(stream_error(error)),
        };
        if packet.track_id() != self.track_id {
            return Ok(Fill::More);
        }

        if self.armed {
            self.armed = false;
            self.skip_frames = match self.time_base.as_ref() {
                Some(time_base) => {
                    let start_ms = time_ms(time_base, packet.ts());
                    (self.skip_target_ms - start_ms).max(0.0) * f64::from(self.rate) / 1000.0
                }
                None => 0.0,
            };
        }

        let samples = match self.decoder.decode(&packet) {
            Ok(samples) => samples,
            Err(SymphoniaError::DecodeError(_)) => {
                self.decode_failures += 1;
                return self.packet_failed();
            }
            Err(SymphoniaError::ResetRequired) => {
                // Codec parameters changed; rebuild the decoder state.
                self.decoder.reset();
                self.decode_failures += 1;
                return self.packet_failed();
            }
            Err(error) => return Err(stream_error(error)),
        };
        self.decode_failures = 0;

        let source_channels = self.channels.max(1);
        let mut interleaved = samples;

        // Discard what lies before the seek target.
        if self.skip_frames > 0.0 {
            let frame_count = (interleaved.len() / source_channels) as f64;
            if self.skip_frames >= frame_count {
                self.skip_frames -= frame_count;
                return Ok(Fill::More);
            }
            let whole = self.skip_frames.floor() as usize;
            interleaved = &interleaved[whole * source_channels..];
            self.skip_frames = 0.0;
        }

        // Normalize to stereo (YouTube audio is stereo; a mono stream is
        // duplicated, and anything wider keeps its first channel).
        let stereo: &[f32] = if source_channels == 2 {
            interleaved
        } else {
            self.stereo.clear();
            self.stereo
                .reserve((interleaved.len() / source_channels) * 2);
            for frame in interleaved.chunks_exact(source_channels) {
                let sample = frame[0];
                self.stereo.push(sample);
                self.stereo.push(sample);
            }
            &self.stereo
        };

        // Convert to the chain rate and then to the sink's f64 samples.
        match self.resampler.as_mut() {
            Some(resampler) => {
                let converted = resampler.process(stereo);
                self.pending
                    .extend(converted.iter().map(|sample| f64::from(*sample)));
            }
            None => {
                self.pending
                    .extend(stereo.iter().map(|sample| f64::from(*sample)));
            }
        }
        Ok(Fill::More)
    }

    /// Counts one failed packet; enough of them in a row fail the stream.
    fn packet_failed(&mut self) -> Result<Fill, Error> {
        if self.decode_failures >= ERROR_LIMIT {
            return Err(Error::Stream(
                "the stream's audio stopped decoding".to_string(),
            ));
        }
        Ok(Fill::More)
    }
}

/// Milliseconds for a packet timestamp in the track's own units.
fn time_ms(time_base: &TimeBase, ts: u64) -> f64 {
    let time = time_base.calc_time(ts);
    time.seconds as f64 * 1000.0 + time.frac * 1000.0
}

/// The container's stated duration, when it has one.
fn rate_checked_duration(
    params: &symphonia::core::codecs::CodecParameters,
    rate: u32,
) -> Option<u64> {
    params
        .n_frames
        .filter(|_| rate > 0)
        .map(|frames| frames.saturating_mul(1000) / u64::from(rate))
}

fn codec_name(codec: u32) -> &'static str {
    match codec {
        CODEC_TYPE_AAC => "aac",
        CODEC_TYPE_OPUS => "opus",
        CODEC_TYPE_VORBIS => "vorbis",
        _ => "unknown",
    }
}

fn container_for(hint: Option<&str>, codec: u32) -> &'static str {
    if let Some(hint) = hint {
        if hint.contains("webm") || hint.contains("matroska") {
            return "webm";
        }
        if hint.contains("mp4") || hint.contains("m4a") {
            return "mp4";
        }
    }
    match codec {
        CODEC_TYPE_AAC => "mp4",
        CODEC_TYPE_OPUS | CODEC_TYPE_VORBIS => "webm",
        _ => "unknown",
    }
}

/// The file extension the probe should try for a mime type.
fn extension_for(hint: &str) -> Option<&'static str> {
    if hint.contains("webm") {
        Some("webm")
    } else if hint.contains("matroska") || hint.contains("mka") {
        Some("mka")
    } else if hint.contains("mp4") || hint.contains("m4a") || hint.contains("aac") {
        Some("m4a")
    } else {
        None
    }
}

fn open_error(error: SymphoniaError) -> Error {
    match error {
        SymphoniaError::Unsupported(message) => Error::Unsupported(message.to_string()),
        other => stream_error(other),
    }
}

fn stream_error(error: SymphoniaError) -> Error {
    if let SymphoniaError::IoError(io) = &error
        && source::link_failure(io) == Some(LinkFailure::Expired)
    {
        return Error::Expired;
    }
    Error::Stream(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek};

    const AAC: &[u8] = include_bytes!("../../testdata/audio/aac.m4a");
    const OPUS: &[u8] = include_bytes!("../../testdata/audio/opus.webm");

    /// An in-memory media source for the fixtures.
    struct Mem(io::Cursor<Vec<u8>>);

    impl Read for Mem {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Seek for Mem {
        fn seek(&mut self, from: io::SeekFrom) -> io::Result<u64> {
            self.0.seek(from)
        }
    }

    impl MediaSource for Mem {
        fn is_seekable(&self) -> bool {
            true
        }

        fn byte_len(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
    }

    fn open(bytes: &'static [u8], hint: &str) -> Stream {
        Stream::open(Mem(io::Cursor::new(bytes.to_vec())), Some(hint)).expect("open the fixture")
    }

    fn drain(stream: &mut Stream) -> Vec<f64> {
        let mut all = Vec::new();
        while let Some(chunk) = stream.next_chunk().expect("decode a chunk") {
            all.extend_from_slice(&chunk);
        }
        all
    }

    fn frames(samples: &[f64]) -> u64 {
        (samples.len() / 2) as u64
    }

    fn loudest(samples: &[f64]) -> f64 {
        samples
            .iter()
            .fold(0.0f64, |loudest, sample| loudest.max(sample.abs()))
    }

    #[test]
    fn the_aac_fixture_decodes_to_its_duration() {
        let mut stream = open(AAC, "audio/mp4; codecs=\"mp4a.40.2\"");
        let opened = stream.opened();
        assert_eq!(opened.container, "mp4");
        assert_eq!(opened.codec, "aac");
        assert_eq!(opened.rate, 44100);
        assert!(!opened.resampled);
        let samples = drain(&mut stream);
        let milliseconds = frames(&samples) * 1000 / 44100;
        assert!(
            (2950..=3090).contains(&milliseconds),
            "aac fixture lasted {milliseconds} ms"
        );
        assert!(
            loudest(&samples) > 0.01,
            "the aac fixture decoded to silence"
        );
    }

    #[test]
    fn the_opus_fixture_resamples_to_the_chain_rate() {
        let mut stream = open(OPUS, "audio/webm; codecs=\"opus\"");
        let opened = stream.opened();
        assert_eq!(opened.container, "webm");
        assert_eq!(opened.codec, "opus");
        assert_eq!(opened.rate, 48000);
        assert!(opened.resampled);
        let samples = drain(&mut stream);
        let milliseconds = frames(&samples) * 1000 / 44100;
        assert!(
            (2940..=3080).contains(&milliseconds),
            "opus fixture lasted {milliseconds} ms"
        );
        assert!(
            loudest(&samples) > 0.01,
            "the opus fixture decoded to silence"
        );
    }

    #[test]
    fn seeking_lands_near_the_end_of_the_aac_fixture() {
        let mut stream = open(AAC, "audio/mp4");
        stream.seek(2800).expect("seek");
        let samples = drain(&mut stream);
        let milliseconds = frames(&samples) * 1000 / 44100;
        assert!(
            (100..=400).contains(&milliseconds),
            "after seeking to 2800 ms, {milliseconds} ms remained"
        );
        assert!(loudest(&samples) > 0.01, "the seek decoded to silence");
    }

    #[test]
    fn seeking_mid_way_leaves_the_rest_of_the_opus_fixture() {
        let mut stream = open(OPUS, "audio/webm");
        stream.seek(1500).expect("seek");
        let samples = drain(&mut stream);
        let milliseconds = frames(&samples) * 1000 / 44100;
        assert!(
            (1350..=1650).contains(&milliseconds),
            "after seeking to 1500 ms, {milliseconds} ms remained"
        );
    }

    #[test]
    fn an_unreadable_stream_is_unsupported() {
        let bytes = vec![0u8; 256];
        let error =
            Stream::open(Mem(io::Cursor::new(bytes)), None).expect_err("garbage must not open");
        assert!(
            matches!(error, Error::Unsupported(_)),
            "garbage became {error:?}"
        );
    }
}
