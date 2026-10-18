//! The playback worker: decoded packets streamed into the sink chain.
//!
//! One thread owns the open stream, the sink chain, and the conversion
//! path. The engine on the other side sends commands over a channel and
//! receives [`AudioEvent`]s; every event carries the load's sequence
//! number so the engine can drop answers to a track it has already left.
//! Positions come from the chain's played-frame counter (at the output's
//! own rate), snapshotted per load, seek, and renewal.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use librespot_playback::audio_backend::Sink;
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};
use tokio::sync::mpsc::UnboundedSender;

use crate::sink::AudioControl;
use crate::yt_audio::decode::{self, Opened, Stream};
use crate::yt_audio::source::{self, LinkFailure, RangeSource};

/// How often positions go out while a stream plays.
const POSITION_INTERVAL: Duration = Duration::from_millis(500);

/// How close the played time must come to the decoded time before the end
/// of a stream counts as played out.
const END_SLACK_MS: u64 = 25;

/// How long the drain wait sleeps between checks.
const DRAIN_STEP: Duration = Duration::from_millis(25);

/// What the engine asks the worker to do.
pub enum Command {
    /// Starts `url` at `position_ms`. `fade_out` discards the previous
    /// stream's queued audio with the usual fade (a user-requested
    /// change); natural track changes leave the queue alone.
    Load {
        url: String,
        length: Option<u64>,
        hint: Option<String>,
        position_ms: u64,
        seq: u64,
        fade_out: bool,
    },
    Pause,
    Resume,
    Seek {
        position_ms: u64,
        seq: u64,
    },
    /// Stops the stream and parks the output.
    Stop,
    Shutdown,
}

/// Why a stream could not play.
#[derive(Debug)]
pub enum Failure {
    /// Fall through to the next resolved candidate.
    Unsupported(String),
    /// Renew or retry; the link or the stream broke.
    Stream(String),
    /// The audio output itself stopped working (already reported by the
    /// chain's own error hook).
    Output(String),
}

/// What the worker reports.
#[derive(Debug)]
pub enum AudioEvent {
    Opened {
        seq: u64,
        opened: Opened,
        /// From the load command to the first packet written.
        first_audio_ms: u64,
    },
    Positioned {
        seq: u64,
        position_ms: u64,
    },
    Ended {
        seq: u64,
    },
    Failed {
        seq: u64,
        failure: Failure,
        /// Where playback had reached, for a renewal's resume point.
        position_ms: u64,
    },
}

/// The worker's handle.
pub struct Worker {
    commands: Sender<Command>,
    join: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// Starts the thread; the chain is built on it (a `Box<dyn Sink>` is
    /// not `Send`, but the builder is).
    pub fn spawn(
        build_sink: impl FnOnce() -> Box<dyn Sink> + Send + 'static,
        control: Arc<AudioControl>,
        events: UnboundedSender<AudioEvent>,
    ) -> io::Result<Self> {
        let (commands, receiver) = std::sync::mpsc::channel();
        let join = thread::Builder::new()
            .name("yt-audio".to_string())
            .spawn(move || {
                let sink = build_sink();
                run(sink, control, receiver, events)
            })?;
        Ok(Self {
            commands,
            join: Some(join),
        })
    }

    /// Sends a command; a worker that has gone is ignored.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Asks the thread to stop and waits for it.
    pub fn shutdown(mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// One open stream and what the engine has been told about it.
struct Live {
    seq: u64,
    stream: Stream,
    opened: Opened,
    /// The position the stream was opened at.
    base_ms: u64,
    /// Played milliseconds when the stream started, on the same counter.
    snapshot_ms: u64,
    /// Milliseconds of this stream already decoded.
    decoded_ms: u64,
    /// The last position the engine was told about.
    told_ms: u64,
    /// When the engine was last told.
    served: Instant,
    /// When the load command arrived, for the first-audio figure.
    loaded_at: Instant,
    /// Whether `Opened` has been sent (after the first packet).
    opened_sent: bool,
}

impl Live {
    /// Where playback has reached, from the chain's played-frame counter.
    fn played_ms(&self, control: &AudioControl) -> u64 {
        stream_position(self.base_ms, self.snapshot_ms, played_ms(control))
    }
}

/// A terminal thing that happened to the open stream.
enum Outcome {
    Ended {
        seq: u64,
    },
    Failed {
        seq: u64,
        failure: Failure,
        position_ms: u64,
    },
}

fn run(
    mut sink: Box<dyn Sink>,
    control: Arc<AudioControl>,
    commands: Receiver<Command>,
    events: UnboundedSender<AudioEvent>,
) {
    let mut converter = Converter::new(None);
    let mut live: Option<Live> = None;
    let mut paused = false;

    loop {
        // Commands: awaited while idle or paused, sniffed between packets
        // while playing so a skip lands within one packet's time.
        let command = if live.is_some() && !paused {
            match commands.try_recv() {
                Ok(command) => Some(command),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Command::Shutdown),
            }
        } else {
            match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => Some(Command::Shutdown),
            }
        };

        if let Some(command) = command {
            match command {
                Command::Load {
                    url,
                    length,
                    hint,
                    position_ms,
                    seq,
                    fade_out,
                } => {
                    if fade_out {
                        control.interrupt();
                    }
                    // After a fade the output is replaced and the counter
                    // restarts at zero; a natural change keeps the counter
                    // running, so the snapshot cancels what played before.
                    let snapshot_ms = if fade_out { 0 } else { played_ms(&control) };
                    let loaded_at = Instant::now();
                    match open_stream(&url, length, hint.as_deref(), position_ms) {
                        Ok(stream) => {
                            // The old decoder is gone; sound may flow again.
                            control.track_changed();
                            let opened = stream.opened();
                            live = Some(Live {
                                seq,
                                stream,
                                opened,
                                base_ms: position_ms,
                                snapshot_ms,
                                decoded_ms: 0,
                                told_ms: position_ms,
                                served: Instant::now(),
                                loaded_at,
                                opened_sent: false,
                            });
                            paused = false;
                            let _ = events.send(AudioEvent::Positioned { seq, position_ms });
                        }
                        Err(failure) => {
                            control.track_changed();
                            live = None;
                            paused = false;
                            let _ = events.send(AudioEvent::Failed {
                                seq,
                                failure,
                                position_ms,
                            });
                        }
                    }
                }
                Command::Pause => {
                    if live.is_some() && !paused {
                        paused = true;
                        publish(&mut live, &control, &events, true);
                    }
                }
                Command::Resume => paused = false,
                Command::Seek { position_ms, seq } => {
                    let mut failed: Option<(Failure, u64)> = None;
                    if let Some(current) = &mut live
                        && current.seq == seq
                    {
                        control.seeked();
                        match current.stream.seek(position_ms) {
                            Ok(()) => {
                                current.base_ms = position_ms;
                                current.snapshot_ms = 0;
                                current.decoded_ms = 0;
                                current.told_ms = position_ms;
                                current.served = Instant::now();
                                let _ = events.send(AudioEvent::Positioned { seq, position_ms });
                            }
                            Err(error) => {
                                let was = current.played_ms(&control);
                                control.track_changed();
                                failed = Some((map_decode(error), was));
                            }
                        }
                    }
                    if let Some((failure, position_ms)) = failed {
                        live = None;
                        let _ = events.send(AudioEvent::Failed {
                            seq,
                            failure,
                            position_ms,
                        });
                    }
                }
                Command::Stop => {
                    if live.take().is_some() {
                        let _ = sink.stop();
                    }
                    paused = false;
                }
                Command::Shutdown => {
                    let _ = sink.stop();
                    break;
                }
            }
            continue;
        }

        // Nothing to obey: move one packet (or wait for the queue to play
        // the tail out).
        let mut outcome = None;
        if let Some(current) = &mut live
            && !paused
        {
            match current.stream.next_chunk() {
                Ok(Some(samples)) => {
                    if !current.opened_sent {
                        current.opened_sent = true;
                        let _ = events.send(AudioEvent::Opened {
                            seq: current.seq,
                            opened: current.opened,
                            first_audio_ms: current.loaded_at.elapsed().as_millis() as u64,
                        });
                    }
                    let frames = (samples.len() / NUM_CHANNELS as usize) as u64;
                    let packet = AudioPacket::Samples(samples);
                    match sink.write(packet, &mut converter) {
                        Ok(()) => {
                            current.decoded_ms +=
                                frames.saturating_mul(1000) / u64::from(SAMPLE_RATE);
                        }
                        Err(error) => {
                            outcome = Some(Outcome::Failed {
                                seq: current.seq,
                                failure: Failure::Output(error.to_string()),
                                position_ms: current.played_ms(&control),
                            });
                        }
                    }
                }
                Ok(None) => {
                    let played = current.played_ms(&control);
                    if played + END_SLACK_MS >= current.decoded_ms {
                        outcome = Some(Outcome::Ended { seq: current.seq });
                    } else {
                        thread::sleep(DRAIN_STEP);
                    }
                }
                Err(error) => {
                    outcome = Some(Outcome::Failed {
                        seq: current.seq,
                        failure: map_decode(error),
                        position_ms: current.played_ms(&control),
                    });
                }
            }
        }
        if live.is_some() && !paused {
            publish(&mut live, &control, &events, false);
        }
        match outcome {
            Some(Outcome::Ended { seq }) => {
                live = None;
                let _ = events.send(AudioEvent::Ended { seq });
            }
            Some(Outcome::Failed {
                seq,
                failure,
                position_ms,
            }) => {
                live = None;
                let _ = events.send(AudioEvent::Failed {
                    seq,
                    failure,
                    position_ms,
                });
            }
            None => {}
        }
    }
}

/// Sends the current position when the interval has passed (or `force`).
fn publish(
    live: &mut Option<Live>,
    control: &AudioControl,
    events: &UnboundedSender<AudioEvent>,
    force: bool,
) {
    let Some(current) = live else { return };
    let now = Instant::now();
    if !force && now.duration_since(current.served) < POSITION_INTERVAL {
        return;
    }
    let position_ms = current.played_ms(control);
    if !force && position_ms == current.told_ms {
        current.served = now;
        return;
    }
    current.told_ms = position_ms;
    current.served = now;
    let _ = events.send(AudioEvent::Positioned {
        seq: current.seq,
        position_ms,
    });
}

/// Opens one candidate: the ranged source, its link check, the stream, and
/// the initial seek.
fn open_stream(
    url: &str,
    length: Option<u64>,
    hint: Option<&str>,
    position_ms: u64,
) -> Result<Stream, Failure> {
    let source = RangeSource::new(url, length)
        .map_err(|error| Failure::Stream(format!("the stream could not open: {error}")))?;
    source.open().map_err(|error| {
        if source::link_failure(&error) == Some(LinkFailure::Expired) {
            Failure::Stream("the stream link stopped working".to_string())
        } else {
            Failure::Stream(format!("the stream could not open: {error}"))
        }
    })?;
    let mut stream = Stream::open(source, hint).map_err(map_decode)?;
    if position_ms > 0 {
        stream.seek(position_ms).map_err(map_decode)?;
    }
    Ok(stream)
}

fn map_decode(error: decode::Error) -> Failure {
    match error {
        decode::Error::Unsupported(detail) => Failure::Unsupported(detail),
        decode::Error::Expired => Failure::Stream("the stream link stopped working".to_string()),
        decode::Error::Stream(detail) => Failure::Stream(detail),
    }
}

/// Decodes a few seconds of a stream and reports each channel's own level,
/// for the `probe-audio` verb: what the source itself carries, measured
/// through the same decoding playback uses, before any output.
pub(crate) fn channel_balance(
    url: &str,
    length: Option<u64>,
    hint: Option<&str>,
) -> Result<String, String> {
    let mut stream = open_stream(url, length, hint, 0).map_err(|failure| format!("{failure:?}"))?;
    let target = u64::from(SAMPLE_RATE) * 3;
    let mut frames = 0u64;
    let (mut left, mut right) = (0.0f64, 0.0f64);
    while frames < target {
        match stream.next_chunk() {
            Ok(Some(chunk)) => {
                for frame in chunk.chunks_exact(2) {
                    left += frame[0] * frame[0];
                    right += frame[1] * frame[1];
                }
                frames += (chunk.len() / 2) as u64;
            }
            Ok(None) => break,
            Err(error) => return Err(format!("{error}")),
        }
    }
    if frames == 0 {
        return Err("nothing decoded".to_string());
    }
    let milliseconds = frames * 1000 / u64::from(SAMPLE_RATE);
    let rms = |sum: f64| (sum / frames as f64).sqrt();
    Ok(format!(
        "left rms {:.4}, right rms {:.4} over {milliseconds} ms",
        rms(left),
        rms(right)
    ))
}

/// The chain's played-frame counter in milliseconds; zero while no output
/// has attached yet.
fn played_ms(control: &AudioControl) -> u64 {
    match control.played_frames() {
        Some((frames, rate)) if rate > 0 => frames.saturating_mul(1000) / u64::from(rate),
        _ => 0,
    }
}

/// The position a stream reports: its base plus what played since its
/// snapshot (saturating, so a counter that restarted never goes negative).
fn stream_position(base_ms: u64, snapshot_ms: u64, played_ms: u64) -> u64 {
    base_ms + played_ms.saturating_sub(snapshot_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_playback::audio_backend::{SinkError, SinkResult};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Mutex;

    const AAC: &[u8] = include_bytes!("../testdata/audio/aac.m4a");

    #[test]
    fn positions_add_what_played_since_the_snapshot() {
        assert_eq!(stream_position(0, 0, 1000), 1000);
        assert_eq!(stream_position(90_000, 0, 500), 90_500);
        assert_eq!(
            stream_position(0, 500, 400),
            0,
            "a counter that restarted saturates instead of going backwards"
        );
    }

    /// A sink that only counts the frames it was handed.
    struct FakeSink {
        frames: Arc<Mutex<u64>>,
        packets: Arc<Mutex<u64>>,
    }

    impl Sink for FakeSink {
        fn start(&mut self) -> SinkResult<()> {
            Ok(())
        }

        fn stop(&mut self) -> SinkResult<()> {
            Ok(())
        }

        fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
            let samples = packet
                .samples()
                .map_err(|error| SinkError::OnWrite(error.to_string()))?;
            *self.frames.lock().unwrap_or_else(|p| p.into_inner()) +=
                (samples.len() / NUM_CHANNELS as usize) as u64;
            *self.packets.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            Ok(())
        }
    }

    /// A one-endpoint server that answers every request with the whole
    /// body (the source absorbs the ignored ranges).
    fn serve(bytes: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let address = listener.local_addr().expect("local address");
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0u8; 8192];
                let read = stream.read(&mut buffer).unwrap_or(0);
                if read == 0 {
                    continue;
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(bytes);
            }
        });
        format!("http://{address}/stream")
    }

    fn wait_for(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<AudioEvent>,
        wanted: impl Fn(&AudioEvent) -> bool,
        timeout: Duration,
    ) -> Option<AudioEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match events.try_recv() {
                Ok(event) => {
                    if wanted(&event) {
                        return Some(event);
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
        None
    }

    #[test]
    fn a_load_decodes_the_stream_into_the_chain() {
        let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let frames = Arc::new(Mutex::new(0u64));
        let packets = Arc::new(Mutex::new(0u64));
        let sink_frames = Arc::clone(&frames);
        let sink_packets = Arc::clone(&packets);
        let control = AudioControl::new(crate::sink::DEFAULT_BUFFER_MS);
        let worker = Worker::spawn(
            move || {
                Box::new(FakeSink {
                    frames: sink_frames,
                    packets: sink_packets,
                })
            },
            control,
            events_tx,
        )
        .expect("spawn the worker");

        let url = serve(AAC);
        worker.send(Command::Load {
            url,
            length: Some(AAC.len() as u64),
            hint: Some("audio/mp4".to_string()),
            position_ms: 0,
            seq: 7,
            fade_out: true,
        });

        let opened = wait_for(
            &mut events,
            |event| matches!(event, AudioEvent::Opened { .. }),
            Duration::from_secs(15),
        )
        .expect("a stream must open");
        match opened {
            AudioEvent::Opened {
                seq,
                first_audio_ms,
                ..
            } => {
                assert_eq!(seq, 7);
                assert!(
                    first_audio_ms < 10_000,
                    "first audio took {first_audio_ms} ms"
                );
            }
            _ => unreachable!(),
        }

        // The whole three-second fixture decodes quickly through the fake
        // sink. Wait for the count to settle instead of for a threshold:
        // the decode burst can outrun a polling interval, and the read
        // then lands mid-burst.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut settled = Instant::now();
        let mut seen = 0;
        while Instant::now() < deadline {
            let now = *frames.lock().unwrap_or_else(|p| p.into_inner());
            if now != seen {
                seen = now;
                settled = Instant::now();
            } else if settled.elapsed() > Duration::from_millis(500) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let decoded = *frames.lock().unwrap_or_else(|p| p.into_inner());
        let milliseconds = decoded * 1000 / 44_100;
        assert!(
            (2800..=3120).contains(&milliseconds),
            "the worker decoded {milliseconds} ms"
        );
        worker.shutdown();
    }

    #[test]
    fn an_unreadable_stream_falls_through() {
        let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let frames = Arc::new(Mutex::new(0u64));
        let control = AudioControl::new(crate::sink::DEFAULT_BUFFER_MS);
        let worker = Worker::spawn(
            move || {
                Box::new(FakeSink {
                    frames,
                    packets: Arc::new(Mutex::new(0u64)),
                })
            },
            control,
            events_tx,
        )
        .expect("spawn the worker");

        // Garbage bytes, so the probe cannot identify a format.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let address = listener.local_addr().expect("local address");
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0u8; 8192];
                let _ = stream.read(&mut buffer);
                let head = "HTTP/1.1 200 OK\r\nContent-Length: 256\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&[0u8; 256]);
            }
        });
        worker.send(Command::Load {
            url: format!("http://{address}/stream"),
            length: Some(256),
            hint: None,
            position_ms: 0,
            seq: 1,
            fade_out: true,
        });

        let failed = wait_for(
            &mut events,
            |event| matches!(event, AudioEvent::Failed { .. }),
            Duration::from_secs(15),
        )
        .expect("garbage must fail");
        match failed {
            AudioEvent::Failed { failure, .. } => {
                assert!(
                    matches!(failure, Failure::Unsupported(_)),
                    "garbage became {failure:?}"
                );
            }
            _ => unreachable!(),
        }
        worker.shutdown();
    }
}
