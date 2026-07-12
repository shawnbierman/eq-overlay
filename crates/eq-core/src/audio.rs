//! Fire-and-forget sound playback via `rodio`.
//!
//! `rodio`'s `OutputStream` owns the OS audio device and is `!Send`, so an
//! `AudioPlayer` must be created and used on a single thread. The pipeline
//! creates it on its worker thread and never moves it. The `_stream` field must
//! stay alive for the whole session or playback silently stops.

use anyhow::{Context, Result};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub struct AudioPlayer {
    _stream: OutputStream,
    handle: OutputStreamHandle,
}

impl AudioPlayer {
    /// Open the default output device. Fails on headless machines / no device;
    /// callers are expected to degrade gracefully (log + run without audio).
    pub fn new() -> Result<Self> {
        let (stream, handle) =
            OutputStream::try_default().context("no default audio output device")?;
        Ok(Self { _stream: stream, handle })
    }

    /// Play `path` without blocking. Playback mixes on rodio's own thread and
    /// overlaps with other sounds. Any error is logged, never propagated — a
    /// missing sound file must not take down the tail loop.
    pub fn play(&self, path: &Path) {
        if let Err(e) = self.try_play(path) {
            eprintln!("[warn] failed to play {}: {e}", path.display());
        }
    }

    fn try_play(&self, path: &Path) -> Result<()> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let source = Decoder::new(BufReader::new(file)).context("decode audio")?;
        self.handle
            .play_raw(source.convert_samples())
            .context("submit audio to output")?;
        Ok(())
    }
}
