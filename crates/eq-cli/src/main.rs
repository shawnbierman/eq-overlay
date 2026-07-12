//! `eqoverlay` — v1 CLI. Two subcommands:
//!
//!   eqoverlay tail --config config.toml --log path\to\eqlog.txt
//!   eqoverlay gen  --log path\to\eqlog.txt          (writes fake EQ lines)
//!
//! Run `gen` in one terminal and `tail` in another to see triggers fire without
//! EverQuest running.

mod gen;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use eq_core::{spawn_pipeline, Config, EngineEvent, PipelineOptions};

#[derive(Parser)]
#[command(name = "eqoverlay", about = "EverQuest log trigger engine (v1 core)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Tail an EQ log, match triggers, and print fired triggers + timer starts.
    Tail {
        /// Path to the config TOML.
        #[arg(short, long, default_value = "config.example.toml")]
        config: PathBuf,
        /// Path to the EQ log file. Overrides `general.log_path` in the config.
        #[arg(short, long)]
        log: Option<PathBuf>,
        /// Process the whole file first, instead of only newly-appended lines.
        #[arg(long)]
        from_beginning: bool,
        /// Don't open the audio device (use if you have no sound files/device).
        #[arg(long)]
        no_audio: bool,
        /// Exit automatically after this many seconds. Unset = run until Ctrl-C.
        /// Handy for scripted demos/tests.
        #[arg(long)]
        for_secs: Option<u64>,
    },

    /// Append realistic sample EQ lines to a file so you can test without EQ.
    Gen {
        /// File to append sample lines to (created if missing).
        #[arg(short, long)]
        log: PathBuf,
        /// Milliseconds between lines.
        #[arg(long, default_value_t = 1500)]
        interval_ms: u64,
        /// Number of lines to write, then stop. 0 = run until Ctrl-C.
        #[arg(long, default_value_t = 0)]
        count: u64,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Tail { config, log, from_beginning, no_audio, for_secs } => {
            tail(config, log, from_beginning, no_audio, for_secs)
        }
        Cmd::Gen { log, interval_ms, count } => gen::run(&log, interval_ms, count),
    }
}

fn tail(
    config: PathBuf,
    log: Option<PathBuf>,
    from_beginning: bool,
    no_audio: bool,
    for_secs: Option<u64>,
) -> Result<()> {
    let cfg = Config::load(&config)?;

    let log_path = cfg
        .resolve_log_path(log)
        .context("no log path: pass --log, or set general.log_path / general.log_dir in the config")?;

    println!(
        "Tailing {} ({})",
        log_path.display(),
        if from_beginning { "from beginning" } else { "new lines only" }
    );
    match for_secs {
        Some(s) => println!("Loaded {} trigger(s). Running for {s}s.\n", cfg.triggers.len()),
        None => println!("Loaded {} trigger(s). Press Ctrl-C to quit.\n", cfg.triggers.len()),
    }

    let (tx, rx) = channel();
    let opts = PipelineOptions {
        from_beginning,
        enable_audio: !no_audio,
        ..Default::default()
    };
    let handle = spawn_pipeline(cfg, log_path, opts, tx)?;

    // Print events as they arrive. Runs until Ctrl-C, or until `--for-secs`
    // elapses. recv_timeout (rather than `for ev in rx`) lets us honor the
    // deadline even while no events are arriving.
    let deadline = for_secs.map(|s| Instant::now() + Duration::from_secs(s));
    loop {
        let wait = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(rem) if !rem.is_zero() => rem,
                _ => break, // deadline reached
            },
            None => Duration::from_secs(3600),
        };
        match rx.recv_timeout(wait) {
            Ok(EngineEvent::Trigger(t)) => {
                println!("{}  [TRIGGER] {:<20}  {}", t.at.format("%H:%M:%S"), t.trigger, t.message);
            }
            Ok(EngineEvent::Timer(t)) => {
                println!(
                    "{}  [TIMER  ] {:<24}  {}s",
                    t.started_wall.format("%H:%M:%S"),
                    t.label,
                    t.duration.as_secs()
                );
            }
            Ok(EngineEvent::ClearTimer { key }) => {
                println!("          [CLEAR  ] {key}");
            }
            Ok(EngineEvent::ClearTarget { target }) => {
                println!("          [CLEAR* ] all timers on {target}");
            }
            Ok(EngineEvent::MezBroken { target }) => {
                println!("          [BREAK  ] mez broken on {target}");
            }
            Ok(EngineEvent::Zone { name }) => {
                println!("          [ZONE   ] {name}");
            }
            Ok(EngineEvent::Level { level }) => {
                println!("          [LEVEL  ] {level}");
            }
            Ok(EngineEvent::RareAdded { name, respawn_seconds, .. }) => {
                println!("          [RARE + ] {name} ({respawn_seconds}s)");
            }
            Ok(EngineEvent::RareRemoved { name }) => {
                println!("          [RARE - ] {name}");
            }
            Ok(EngineEvent::RareUpdated { name, respawn_seconds }) => {
                println!("          [RARE ~ ] {name} respawn calibrated to {respawn_seconds}s");
            }
            Ok(EngineEvent::ZoneDefaultSet { zone, respawn_seconds }) => match respawn_seconds {
                Some(s) => println!("          [ZONE = ] {zone} default respawn {s}s"),
                None => println!("          [ZONE = ] {zone} default respawn cleared"),
            },
            Ok(EngineEvent::Damage { .. }) => {} // summed by the GUI; too noisy to print
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // In infinite mode we only reach here if the pipeline stopped — surface its
    // error. In --for-secs mode the pipeline is still running; just exit.
    if for_secs.is_none() {
        return handle
            .join
            .join()
            .map_err(|_| anyhow::anyhow!("pipeline thread panicked"))?;
    }
    Ok(())
}
