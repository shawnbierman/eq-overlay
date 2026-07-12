//! File tailer: watch an EQ log with `notify`, read only newly-appended bytes,
//! and survive truncation / rotation.
//!
//! Design notes:
//!
//! * We watch the log's **parent directory** (non-recursive), not the file
//!   itself. This is more robust: it works when the log doesn't exist yet, and
//!   it survives rotation (delete + recreate at the same path), which watching a
//!   single inode does not.
//! * The byte-offset bookkeeping lives in [`TailState`], which is pure I/O with
//!   no notify dependency, so it can be unit tested against ordinary temp files.
//! * `next_batch` is event-driven but also wakes on a short timeout, so a missed
//!   or coalesced filesystem event costs at most `poll_interval` of latency
//!   rather than a stuck timer.

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::Duration;

/// Tracks how far we've read so we only ever process new bytes. Pure I/O — no
/// notify, no threads — which is what makes it testable.
struct TailState {
    path: PathBuf,
    offset: u64,
    /// Bytes of a not-yet-terminated final line, carried to the next read.
    leftover: Vec<u8>,
}

impl TailState {
    /// Read whatever has been appended since last time and return complete
    /// lines. Handles truncation/rotation by resetting to the start of the file.
    ///
    /// MUST open the file and read the size through the HANDLE
    /// (`file.metadata()`), never stat by path: on Windows, a path stat reads
    /// the DIRECTORY ENTRY, which updates lazily for a file another process
    /// (EQ) holds open for appending — the size can read stale for a long
    /// time, silently freezing the tail. (A stat-by-path fast-path shipped
    /// once and made the overlay miss lines; tests didn't catch it because
    /// test writers close the file after each append, which flushes the
    /// directory entry.) A handle open+stat is a few microseconds — nothing,
    /// even at a 25 ms poll.
    fn poll(&mut self) -> std::io::Result<Vec<String>> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            // File momentarily absent (e.g. mid-rotation) — nothing to do yet.
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };

        let len = file.metadata()?.len();

        // Shorter than where we were reading => truncated in place, or a fresh
        // (rotated) file now sits at this path. Start over from the beginning.
        if len < self.offset {
            self.offset = 0;
            self.leftover.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.offset))?;
        let to_read = len - self.offset;
        // Read up to the length we observed; anything appended after this point
        // is picked up on the next poll. This avoids racing a concurrent writer.
        let mut buf = Vec::with_capacity(to_read as usize);
        let n = (&mut file).take(to_read).read_to_end(&mut buf)?;
        self.offset += n as u64;

        Ok(self.split_lines(buf))
    }

    /// Combine carried-over bytes with `buf`, split on `\n`, and stash any
    /// trailing partial line for next time. Tolerates CRLF and non-UTF-8.
    fn split_lines(&mut self, buf: Vec<u8>) -> Vec<String> {
        let mut data = std::mem::take(&mut self.leftover);
        data.extend_from_slice(&buf);

        let mut lines = Vec::new();
        let mut start = 0;
        for i in 0..data.len() {
            if data[i] == b'\n' {
                let mut end = i;
                if end > start && data[end - 1] == b'\r' {
                    end -= 1; // strip CR from CRLF
                }
                lines.push(String::from_utf8_lossy(&data[start..end]).into_owned());
                start = i + 1;
            }
        }
        self.leftover = data[start..].to_vec();
        lines
    }
}

pub struct Tailer {
    state: TailState,
    /// Kept alive so the watch stays active; dropping it stops notifications.
    _watcher: RecommendedWatcher,
    rx: Receiver<()>,
    poll_interval: Duration,
}

impl Tailer {
    /// Start tailing `path`. If `from_beginning` is false, we skip whatever is
    /// already in the file and only report lines appended after this call.
    pub fn new(path: &Path, from_beginning: bool, poll_interval: Duration) -> Result<Self> {
        let path = path.to_path_buf();

        let offset = if from_beginning {
            0
        } else {
            // Handle-based size for the same reason as poll(): a path stat can
            // be stale for a file the game holds open.
            File::open(&path)
                .and_then(|f| f.metadata())
                .map(|m| m.len())
                .unwrap_or(0)
        };

        let watch_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let file_name = path.file_name().map(|n| n.to_os_string());

        let (tx, rx) = channel::<()>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                // Only wake for events touching our file (or when the backend
                // gives no paths). The timeout in `next_batch` is the safety net
                // for anything this filter drops.
                let relevant = match &file_name {
                    Some(name) => {
                        event.paths.is_empty()
                            || event.paths.iter().any(|p| p.file_name() == Some(name.as_os_str()))
                    }
                    None => true,
                };
                if relevant {
                    let _ = tx.send(());
                }
            }
        })
        .context("failed to create filesystem watcher")?;

        watcher
            .watch(&watch_dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch directory: {}", watch_dir.display()))?;

        Ok(Self {
            state: TailState { path, offset, leftover: Vec::new() },
            _watcher: watcher,
            rx,
            poll_interval,
        })
    }

    /// Block until the log changes or `poll_interval` elapses, then return any
    /// newly-completed lines. May return an empty vec (spurious wakeup / no new
    /// complete line yet).
    pub fn next_batch(&mut self) -> Result<Vec<String>> {
        match self.rx.recv_timeout(self.poll_interval) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher stopped unexpectedly");
            }
        }
        // Coalesce any additional pending notifications into this one poll.
        while self.rx.try_recv().is_ok() {}

        self.state.poll().context("failed reading log file")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn append(path: &Path, s: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    fn state(path: &Path) -> TailState {
        TailState { path: path.to_path_buf(), offset: 0, leftover: Vec::new() }
    }

    #[test]
    fn reads_only_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eqlog.txt");
        std::fs::write(&path, "").unwrap();

        let mut st = state(&path);
        assert!(st.poll().unwrap().is_empty());

        append(&path, "line one\nline two\n");
        assert_eq!(st.poll().unwrap(), vec!["line one", "line two"]);

        // Nothing new -> nothing returned, offset unchanged.
        assert!(st.poll().unwrap().is_empty());

        append(&path, "line three\n");
        assert_eq!(st.poll().unwrap(), vec!["line three"]);
    }

    #[test]
    fn buffers_partial_lines_across_polls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eqlog.txt");
        std::fs::write(&path, "").unwrap();
        let mut st = state(&path);

        append(&path, "partial"); // no newline yet
        assert!(st.poll().unwrap().is_empty());

        append(&path, " continued\n");
        assert_eq!(st.poll().unwrap(), vec!["partial continued"]);
    }

    #[test]
    fn handles_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eqlog.txt");
        std::fs::write(&path, "a\r\nb\r\n").unwrap();
        let mut st = state(&path);
        assert_eq!(st.poll().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn resets_on_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eqlog.txt");
        std::fs::write(&path, "old line\n").unwrap();
        let mut st = state(&path);
        assert_eq!(st.poll().unwrap(), vec!["old line"]);

        // Simulate rotation/truncation: file is replaced with shorter content.
        std::fs::write(&path, "fresh\n").unwrap();
        assert_eq!(st.poll().unwrap(), vec!["fresh"]);
    }
}
