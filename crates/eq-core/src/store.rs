//! What the overlay has *learned* by watching you play, kept across restarts.
//!
//! The two TOML files stay exactly what they are: `config.toml` is settings you
//! edit, `rares.toml` is a rare list you hand to a friend. Neither wants to be
//! rewritten a hundred times a session by machinery, and neither reads well once
//! it is. This database is the other half — measurements the overlay takes on its
//! own, which are append-heavy, uninteresting to read, and worth nothing to
//! anyone else.
//!
//! Today that means **spell durations**. The client's spell file only carries base
//! ranks, so a mote-ranked spell ("Alacrity IV", which does not exist in
//! `spells_us.txt` at all) can only be timed by watching one land and wear off.
//! Before this, that measurement lived in memory and died with the process, so
//! every session re-learned the same spells from scratch.
//!
//! Nothing in here is authored by you and nothing is worth sharing — a duration
//! measured at your level, on your rank of a spell, means nothing on someone
//! else's character. That's what separates it from the TOML files: this is a
//! *cache of observations*, so it is always regenerable. Delete the file and it
//! rebuilds itself by playing. Since it can't be hand-edited, the settings window
//! owns the repair path (forget one spell, or forget everything).
//!
//! Everything here degrades to None on failure: a missing, locked, or corrupt
//! database must never stop the overlay from drawing bars.

use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

/// Default file name, created next to `config.toml`.
pub const DB_FILE: &str = "eq-overlay.db";

pub struct Store {
    conn: Connection,
}

/// One learned spell duration, for display in the settings window.
#[derive(Debug, Clone)]
pub struct LearnedRow {
    pub spell: String,
    /// Longest clean land->wear-off measured, in seconds.
    pub seconds: u64,
    /// How many cycles have been measured — low counts are less trustworthy.
    pub observations: u64,
}

impl Store {
    /// Open (creating if needed) the learning database. Returns None if the file
    /// can't be opened or the schema can't be created — the caller carries on
    /// without persistence rather than failing to start.
    pub fn open(path: &Path) -> Option<Self> {
        let conn = Connection::open(path)
            .map_err(|e| eprintln!("  store   disabled ({e})"))
            .ok()?;
        let s = Self { conn };
        s.migrate().map_err(|e| eprintln!("  store   disabled ({e})")).ok()?;
        Some(s)
    }

    /// In-memory store, for tests.
    #[cfg(test)]
    pub fn open_memory() -> Option<Self> {
        let s = Self { conn: Connection::open_in_memory().ok()? };
        s.migrate().ok()?;
        Some(s)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        // `seconds` is the longest CLEAN land->wear-off we've measured for this
        // spell — the same "grow, never shrink" rule the in-memory version used,
        // since an early break can only ever measure short.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS spell_duration (
                 spell        TEXT PRIMARY KEY,
                 seconds      INTEGER NOT NULL,
                 observations INTEGER NOT NULL DEFAULT 1,
                 updated_at   TEXT    NOT NULL
             );
             -- One row per observed kill-to-kill gap on a tracked rare. The old
             -- in-memory calibration compared only CONSECUTIVE kills and forgot
             -- everything on a zone change, so a camp's worth of evidence was
             -- thrown away every time you left. Keeping the samples lets the
             -- estimate improve across sessions instead of restarting.
             CREATE TABLE IF NOT EXISTS respawn_sample (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 rare    TEXT    NOT NULL,
                 zone    TEXT,
                 seconds INTEGER NOT NULL,
                 seen_at TEXT    NOT NULL
             );
             CREATE INDEX IF NOT EXISTS respawn_sample_rare ON respawn_sample(rare);",
        )
    }

    /// Record one observed kill-to-kill gap for a rare.
    pub fn record_respawn_sample(&self, rare: &str, zone: Option<&str>, seconds: u64) {
        let _ = self.conn.execute(
            "INSERT INTO respawn_sample (rare, zone, seconds, seen_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            rusqlite::params![rare.to_lowercase(), zone, seconds as i64],
        );
    }

    /// Best respawn estimate for a rare: the SMALLEST gap ever observed.
    ///
    /// Gaps only ever over-estimate — you have to notice the rare, walk back, and
    /// kill it again, and any of that can take arbitrarily long. So the minimum
    /// across every sample is the closest anyone has gotten to the true timer, and
    /// more samples can only tighten it. `min_samples` guards against trusting a
    /// single fluke.
    pub fn respawn_estimate(&self, rare: &str, min_samples: u64) -> Option<u64> {
        self.conn
            .query_row(
                "SELECT MIN(seconds), COUNT(*) FROM respawn_sample WHERE rare = ?1",
                rusqlite::params![rare.to_lowercase()],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok()
            .and_then(|(min, n)| match (min, n) {
                (Some(m), n) if m > 0 && n as u64 >= min_samples => Some(m as u64),
                _ => None,
            })
    }

    /// How many respawn samples are on file (settings window).
    pub fn sample_count(&self) -> u64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM respawn_sample", [], |r| r.get::<_, i64>(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0) as u64
    }

    /// Every learned duration, for seeding the pipeline at startup.
    pub fn load_durations(&self) -> HashMap<String, u64> {
        let mut out = HashMap::new();
        let Ok(mut stmt) = self.conn.prepare("SELECT spell, seconds FROM spell_duration") else {
            return out;
        };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        else {
            return out;
        };
        for row in rows.flatten() {
            if row.1 > 0 {
                out.insert(row.0, row.1 as u64);
            }
        }
        out
    }

    /// Record a measured duration. Keeps the LONGEST seen (a broken or dispelled
    /// effect measures short, and would otherwise ratchet the bar down), but
    /// always counts the observation so the row shows how much evidence there is.
    pub fn record_duration(&self, spell: &str, seconds: u64) {
        let _ = self.conn.execute(
            "INSERT INTO spell_duration (spell, seconds, observations, updated_at)
             VALUES (?1, ?2, 1, datetime('now'))
             ON CONFLICT(spell) DO UPDATE SET
                 seconds      = MAX(seconds, excluded.seconds),
                 observations = observations + 1,
                 updated_at   = datetime('now')",
            rusqlite::params![spell, seconds as i64],
        );
    }

    /// Every learned duration with its evidence, newest first — what the Spells
    /// tab lists so a bad measurement can be found and forgotten.
    pub fn learned_rows(&self) -> Vec<LearnedRow> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT spell, seconds, observations FROM spell_duration
             ORDER BY updated_at DESC, spell",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok(LearnedRow {
                spell: r.get(0)?,
                seconds: r.get::<_, i64>(1)?.max(0) as u64,
                observations: r.get::<_, i64>(2)?.max(0) as u64,
            })
        }) else {
            return Vec::new();
        };
        rows.flatten().collect()
    }

    /// Drop one spell's learned duration — the fix for a single bad measurement,
    /// since the file itself can't be opened and corrected by hand. The bar falls
    /// back to the spell file's base rank and re-learns from the next clean cycle.
    pub fn forget_duration(&self, spell: &str) -> bool {
        self.conn
            .execute("DELETE FROM spell_duration WHERE spell = ?1", rusqlite::params![spell])
            .is_ok()
    }

    /// How many spells have a learned duration (shown in the settings window).
    pub fn duration_count(&self) -> u64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM spell_duration", [], |r| r.get::<_, i64>(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0) as u64
    }

    /// Forget every learned duration — the escape hatch if a bad measurement ever
    /// sticks (bars fall back to the spell file's base rank and re-learn).
    pub fn clear_durations(&self) -> bool {
        self.conn.execute("DELETE FROM spell_duration", []).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_persist_and_only_ever_grow() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.duration_count(), 0);

        s.record_duration("Alacrity", 300);
        assert_eq!(s.load_durations().get("Alacrity"), Some(&300));

        // A longer clean measurement wins (the mote rank finally ran to term).
        s.record_duration("Alacrity", 1295);
        assert_eq!(s.load_durations().get("Alacrity"), Some(&1295));

        // A short one (broken early) must NOT ratchet the bar back down.
        s.record_duration("Alacrity", 42);
        assert_eq!(s.load_durations().get("Alacrity"), Some(&1295));

        // ...but it still counts as evidence.
        let obs: i64 = s
            .conn
            .query_row("SELECT observations FROM spell_duration WHERE spell='Alacrity'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(obs, 3);

        assert_eq!(s.duration_count(), 1);

        // A single bad measurement can be dropped on its own (the file can't be
        // hand-edited, so this is the repair path).
        s.record_duration("Clarity", 900);
        assert!(s.forget_duration("Alacrity"));
        assert!(s.load_durations().get("Alacrity").is_none());
        assert_eq!(s.load_durations().get("Clarity"), Some(&900));
        assert_eq!(s.learned_rows().len(), 1);

        assert!(s.clear_durations());
        assert!(s.load_durations().is_empty());
    }

    #[test]
    fn respawn_estimate_takes_the_smallest_gap_once_there_is_evidence() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.respawn_estimate("ghoul assassin", 2), None);

        // Gaps always over-estimate (you have to notice and re-kill), so the
        // smallest one seen is the best guess.
        s.record_respawn_sample("Ghoul Assassin", Some("Old Guk"), 900);
        // One sample isn't enough to trust.
        assert_eq!(s.respawn_estimate("ghoul assassin", 2), None);
        s.record_respawn_sample("ghoul assassin", Some("Old Guk"), 700);
        assert_eq!(s.respawn_estimate("ghoul assassin", 2), Some(700));
        // A later, longer gap can't loosen it back up.
        s.record_respawn_sample("ghoul assassin", Some("Old Guk"), 1800);
        assert_eq!(s.respawn_estimate("GHOUL ASSASSIN", 2), Some(700));
        assert_eq!(s.sample_count(), 3);
    }
}
