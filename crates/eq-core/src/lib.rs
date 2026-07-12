//! Core engine for an EverQuest log-driven overlay.
//!
//! The design deliberately separates the *pure, testable* pieces (parsing,
//! trigger matching, config) from the *side-effecting* pieces (file tailing,
//! audio, threading):
//!
//! ```text
//!   log file --> Tailer --> raw line --> parse_line --> Engine::process
//!                                                          |
//!                                                          v
//!                                              EngineEvent (Trigger / Timer)
//!                                                          |
//!                    +-------------------------------------+
//!                    |                                     |
//!                    v                                     v
//!               CLI printer (v1)                    overlay GUI (later)
//! ```
//!
//! `Engine::process` takes a `&str` and returns which triggers fired. It does
//! no I/O and no timekeeping, so it is trivial to unit test. All the messy
//! real-world concerns (notify events, byte offsets, rotation, audio devices)
//! live in [`tailer`] and [`pipeline`].
//!
//! The seam for the future overlay is [`pipeline::spawn_pipeline`], which takes
//! an `mpsc::Sender<EngineEvent>`. The CLI passes a sender whose receiver prints
//! to stdout; the overlay will pass one whose receiver draws timer bars.

pub mod audio;
pub mod config;
pub mod duration;
pub mod events;
pub mod parser;
pub mod pipeline;
pub mod spelldb;
pub mod tailer;
pub mod triggers;

pub use config::{char_server_from_log, Config};
pub use duration::{duration_seconds, duration_ticks};
pub use events::{EngineEvent, TimerEvent, TriggerEvent};
pub use parser::{parse_line, LogLine};
pub use pipeline::{
    normalize_zone, parse_secs, spawn_pipeline, spawn_pipeline_with_control, Control,
    PipelineHandle, PipelineOptions,
};
pub use spelldb::{SpellDb, SpellInfo};
pub use tailer::Tailer;
pub use triggers::{interpolate, DurationSpec, Engine, Fired, TimerSpec, Trigger};
