//! Sample EQ log generator. Appends realistic, correctly-timestamped lines to a
//! file on an interval so you can exercise the tailer + triggers without EQ.
//!
//! The sample lines are written to match the triggers in `config.example.toml`.

use anyhow::{Context, Result};
use chrono::Local;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

/// A short script of message bodies. Some match example triggers; a couple are
/// deliberate noise so you can see non-matching lines being ignored.
const SAMPLE_LINES: &[&str] = &[
    "orc centurion has been mesmerized.",                    // -> Mez timer starts
    "a Cursed Wraith hits YOU for 1247 points of damage.",   // -> Big hit taken (sound)
    "a bandit looks less aggressive.",                       // -> Lull timer starts
    "You say, 'Hail, a guard of Qeynos'",                    // noise
    "Your spell is interrupted.",                            // -> Spell interrupted (sound)
    "Your Mesmerize spell has worn off of orc centurion.",   // -> clears the Mez timer
    "Soandso engages you!",                                  // noise
    "Your Lull spell has worn off of a bandit.",             // -> clears the Lull timer
];

pub fn run(log: &Path, interval_ms: u64, count: u64) -> Result<()> {
    if let Some(parent) = log.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).ok();
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("failed to open {}", log.display()))?;

    println!(
        "Writing sample EQ lines to {} every {}ms (Ctrl-C to stop).",
        log.display(),
        interval_ms
    );

    let mut i: u64 = 0;
    loop {
        let msg = SAMPLE_LINES[(i as usize) % SAMPLE_LINES.len()];
        // EQ's own timestamp format, generated live.
        let stamp = Local::now().format("%a %b %d %H:%M:%S %Y");
        let line = format!("[{stamp}] {msg}\n");

        file.write_all(line.as_bytes())?;
        file.flush()?;
        print!("wrote: {line}");

        i += 1;
        if count != 0 && i >= count {
            break;
        }
        sleep(Duration::from_millis(interval_ms));
    }

    Ok(())
}
