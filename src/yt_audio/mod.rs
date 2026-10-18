//! In-process playback for YouTube audio.
//!
//! The YouTube engine plays without a child process: a ranged HTTP source
//! (`source`), a demux and decode pipeline (`decode`), and a worker thread
//! (`worker`) that feeds the app's own sink chain. See
//! `docs/superpowers/specs/2026-10-18-native-youtube-player-design.md`.

pub mod decode;
pub mod source;
pub mod worker;

#[cfg(test)]
mod tests {
    use super::decode::Stream;
    use super::source::RangeSource;
    use std::time::Instant;

    /// The live end-to-end check, run by hand on a machine that can
    /// resolve YouTube (it needs the network, so it is ignored by
    /// default):
    ///
    /// ```text
    /// cargo test --lib --no-default-features live_pipeline -- --ignored --nocapture
    /// ```
    ///
    /// It resolves a song, opens the stream, decodes a stretch of sound,
    /// seeks forward, and decodes more. The burst fetching means the
    /// decode of ten seconds of sound finishes far quicker than ten
    /// seconds of wall time, which the printed figure shows.
    #[test]
    #[ignore = "needs the network and a resolvable YouTube query"]
    fn live_pipeline_resolves_decodes_and_seeks() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let resolved = runtime
            .block_on(async {
                let native = crate::yt_resolver::NativeResolver::new()
                    .map_err(|error| format!("{error:#}"))?;
                native
                    .resolve("danza rota")
                    .await
                    .map_err(|error| format!("{error:#}"))
            })
            .expect("the live resolve");
        println!(
            "resolved: mime {:?}, {} bytes",
            resolved.mime,
            resolved
                .content_length
                .map_or("unknown".to_string(), |length| length.to_string())
        );

        let source =
            RangeSource::new(resolved.url.clone(), resolved.content_length).expect("a source");
        source.open().expect("open");
        let opened_at = Instant::now();
        let mut stream = Stream::open(source, resolved.mime.as_deref()).expect("decode");
        let opened = stream.opened();
        println!(
            "opened: {} in {} at {} Hz{}",
            opened.codec,
            opened.container,
            opened.rate,
            if opened.resampled { ", resampled" } else { "" }
        );

        // Ten seconds of sound, watching the wall clock.
        let mut frames = 0u64;
        let mut loudest = 0.0f64;
        while frames < 44_100 * 10 {
            let chunk = stream.next_chunk().expect("a chunk").expect("sound");
            frames += (chunk.len() / 2) as u64;
            for sample in &chunk {
                loudest = loudest.max(sample.abs());
            }
        }
        let decoded_ms = frames * 1000 / 44_100;
        println!(
            "decoded {decoded_ms} ms of sound in {} ms of wall time, loudest {loudest:.3}",
            opened_at.elapsed().as_millis()
        );
        assert!(loudest > 0.01, "the first ten seconds were silence");

        // Forward, inside the fetched window, and keep decoding.
        let sought_at = Instant::now();
        stream.seek(30_000).expect("seek");
        let mut after = 0u64;
        let mut loudest_after = 0.0f64;
        while after < 44_100 * 5 {
            let chunk = stream.next_chunk().expect("a chunk").expect("sound");
            after += (chunk.len() / 2) as u64;
            for sample in &chunk {
                loudest_after = loudest_after.max(sample.abs());
            }
        }
        println!(
            "seeked to 30000 ms, decoded five more seconds in {} ms, loudest {loudest_after:.3}",
            sought_at.elapsed().as_millis()
        );
        assert!(loudest_after > 0.01, "the sound after the seek was silence");
    }
}
