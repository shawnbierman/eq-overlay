//! Events emitted by the pipeline and consumed downstream (CLI now, overlay
//! later). These are the *only* type the overlay needs to know about — it never
//! touches the tailer or the parser.

use chrono::{DateTime, Local};
use std::time::{Duration, Instant};

/// Anything the pipeline wants a consumer to react to.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A trigger matched a log line. Useful for a scrolling event log / audit.
    Trigger(TriggerEvent),
    /// A trigger asked for a countdown timer to start (or restart, by key).
    Timer(TimerEvent),
    /// A trigger asked to remove active timers whose key matches — e.g. a mez
    /// wore off / broke, so its countdown should disappear immediately.
    ClearTimer { key: String },
    /// Remove ALL active timers on a target (every `spell:<target>` key) — e.g.
    /// the mob died, so no per-spell wear-off line will ever arrive.
    ClearTarget { target: String },
    /// "<target> has been awakened by <attacker>." — the game's unambiguous
    /// mez-BREAK line (always accompanies a damage break, never a clean
    /// refresh). Lets the overlay tell a real break from the spurious
    /// "worn off" a re-cast logs when it replaces its own running mez.
    MezBroken { target: String },
    /// The player zoned into a new area (from the log's "You have entered X."
    /// line). The overlay shows it in the title tab.
    Zone { name: String },
    /// The player's current level (from the startup back-scan, then each ding).
    /// Level-scaled durations use it internally; the settings window shows it.
    Level { level: u32 },
    /// A rare was added to the respawn database from in-game (the private
    /// "eqov" channel `add` command). The settings window updates its list.
    RareAdded { name: String, respawn_seconds: u64, zone: Option<String> },
    /// A rare was removed from the respawn database from in-game (`remove`).
    RareRemoved { name: String },
    /// A rare's respawn time was auto-calibrated (tightened) from observed
    /// kill-to-kill gaps while camping it.
    RareUpdated { name: String, respawn_seconds: u64 },
    /// A per-zone default respawn was set (or cleared, when None) via the
    /// in-game `zone` command. Bare `add`s in that zone use it.
    ZoneDefaultSet { zone: String, respawn_seconds: Option<u64> },
    /// The player dealt `amount` damage (melee, direct spell, or DoT tick). The
    /// overlay sums these over a rolling window for a live DPS readout.
    Damage { amount: u64 },
}

#[derive(Debug, Clone)]
pub struct TriggerEvent {
    /// Name of the trigger that fired.
    pub trigger: String,
    /// The message body of the line that matched.
    pub message: String,
    /// Wall-clock time the match was processed.
    pub at: DateTime<Local>,
}

#[derive(Debug, Clone)]
pub struct TimerEvent {
    /// Identity used to replace/clear this timer (e.g. "mez:orc centurion").
    /// A new Timer with the same key restarts it; a ClearTimer with the same
    /// key removes it.
    pub key: String,
    /// Name of the trigger that started this timer.
    pub trigger: String,
    /// EQ spell-icon index (into the `SpellsNN.tga` sheets), if known.
    pub icon: Option<u32>,
    /// Display label for the timer (e.g. "CH cast", "Disc reuse").
    pub label: String,
    /// How long the timer should run.
    pub duration: Duration,
    /// Monotonic start instant — the overlay computes `remaining = duration -
    /// started_at.elapsed()` each frame. Monotonic clock is immune to wall-clock
    /// adjustments, which matters for an always-on overlay.
    pub started_at: Instant,
    /// Wall-clock start, for human-readable logging.
    pub started_wall: DateTime<Local>,
}

impl TimerEvent {
    /// Time left, saturating at zero. Convenience for consumers.
    pub fn remaining(&self) -> Duration {
        self.duration.saturating_sub(self.started_at.elapsed())
    }
}
