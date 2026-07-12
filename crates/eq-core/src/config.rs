//! TOML configuration: a list of triggers plus optional general settings.
//!
//! Example:
//!
//! ```toml
//! [general]
//! log_path = "C:\\Users\\you\\...\\eqlog_Toon_server.txt"  # optional
//!
//! [[triggers]]
//! name = "Complete Heal"
//! pattern = "You begin casting Complete Heal"
//! regex = false
//! timer_seconds = 10
//! timer_label = "CH cast"
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub overlay: OverlayConfig,
    /// Spell database: each `[[spell]]` auto-expands into land + wear-off triggers.
    #[serde(default, rename = "spell")]
    pub spells: Vec<SpellConfig>,
    /// Rare/named mobs to show a respawn countdown for. Killing one starts a
    /// timer; at 0 the bar reads "UP".
    #[serde(default, rename = "rare")]
    pub rares: Vec<RareConfig>,
    #[serde(default)]
    pub triggers: Vec<TriggerConfig>,
    #[serde(default)]
    pub audio: AudioConfig,
    /// Resolved path of the shareable rare DB (set by `load`, even when the
    /// file doesn't exist yet) — where in-game `add` commands append entries.
    #[serde(skip)]
    pub rare_db_path: Option<PathBuf>,
}

/// Sound settings (the GUI's Audio tab writes these).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    /// Master switch: false silences the spawn chime and trigger sounds.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Spawn-chime sound: "default" | "asterisk" | "exclamation" | "critical",
    /// or a path to a .wav file.
    #[serde(default)]
    pub spawn_sound: Option<String>,
}

/// A rare/named mob to track a respawn timer for. When it's slain (by anyone),
/// a bar counts down `respawn_seconds`; when it hits 0 the mob should be up.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RareConfig {
    /// Exact mob name as it appears in "You have slain X!" / "X has been slain by".
    pub name: String,
    /// Respawn time in seconds (death → next spawn).
    pub respawn_seconds: u64,
    /// Optional spell-icon index for the bar; a colour-coded square otherwise.
    #[serde(default)]
    pub icon: Option<u32>,
    /// Zone the rare is in — informational, for the shareable DB.
    #[serde(default)]
    pub zone: Option<String>,
    /// Free-form notes (placeholder info, "contested", etc.).
    #[serde(default)]
    pub notes: Option<String>,
}

/// Overlay window placement, in screen pixels. Used by `eq-overlay-gui`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OverlayConfig {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        // Top-left, just below EQ's built-in FPS/latency HUD.
        Self { x: 20, y: 95, width: 340, height: 480 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    /// Optional default log path. A `--log` CLI flag overrides this.
    #[serde(default)]
    pub log_path: Option<String>,
    /// Your character's current level — the starting point for level-scaled
    /// spell durations. Auto-updates whenever the log shows `Welcome to level
    /// N!`, so this only needs to be roughly right when you launch mid-session.
    #[serde(default)]
    pub player_level: Option<u32>,
    /// Folder holding EQ's icon sheets (usually the game's `uifiles/default`).
    /// When set, bars show real spell icons.
    #[serde(default)]
    pub icon_dir: Option<String>,
    /// Icon sheet set / prefix: "Spells" (modern buff icons, default) or
    /// "gemicons" (classic / old-school gems). Sheets are `<prefix>NN.tga`.
    #[serde(default)]
    pub icon_sheet: Option<String>,
    /// Path to EQ's `spells_us.txt`. When present (or derivable from `icon_dir`,
    /// which normally lives at `<eq>/uifiles/default`), every *detrimental* spell
    /// you cast is tracked automatically — its land line, level-scaled duration,
    /// and icon all come from the game files, so no `[[spell]]` list is needed.
    #[serde(default)]
    pub spell_file: Option<String>,
    /// Path to EQ's `spells_us_str.txt` (land / wear-off message strings).
    #[serde(default)]
    pub spell_str_file: Option<String>,
    /// Path to a shareable rare-respawn database (a TOML file of `[[rare]]`
    /// entries). Relative paths resolve next to this config. Default `rares.toml`.
    /// This keeps community respawn data out of your personal config.
    #[serde(default)]
    pub rare_db: Option<String>,
    /// Directory of EQ logs. If neither `--log` nor `log_path` is set, the overlay
    /// tails the most-recently-written `eqlog_*.txt` here — so it follows whichever
    /// character/server you last played (Rivervale, Qeynos, …) with no reconfig.
    #[serde(default)]
    pub log_dir: Option<String>,
    /// Name of the private chat channel watched for in-game commands like
    /// `add` (rare respawns). Join it in game with `/join <name>`. Pick a name
    /// nobody else uses — channel members see each other's messages. Default
    /// "eqov".
    #[serde(default)]
    pub command_channel: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerConfig {
    /// Human-readable name shown when the trigger fires.
    pub name: String,
    /// The substring (or regex, if `regex = true`) to match against each line's
    /// message body (the text after the `[timestamp]`).
    pub pattern: String,
    /// If true, `pattern` is compiled as a regular expression. If false (the
    /// default), it is matched as a plain, case-sensitive substring.
    #[serde(default)]
    pub regex: bool,
    /// Optional path to a sound file (wav/mp3/flac/ogg) to play when this fires.
    #[serde(default)]
    pub sound: Option<String>,
    /// Optional timer duration in seconds. When present, firing emits a
    /// `TimerEvent` the overlay can count down.
    #[serde(default)]
    pub timer_seconds: Option<u64>,
    /// EQ level-scaled duration: the spell's *duration formula* id. Combined
    /// with `duration_base` (cap, in ticks) the bar length is computed from
    /// your level as `min(formula(level), base) × 6s`. Overrides `timer_seconds`.
    #[serde(default)]
    pub duration_formula: Option<i64>,
    /// Base/cap duration in ticks (6s each) for `duration_formula`.
    #[serde(default)]
    pub duration_base: Option<i64>,
    /// Optional label for the timer. May contain capture placeholders like
    /// `{1}` (first regex group). Defaults to the trigger `name`.
    #[serde(default)]
    pub timer_label: Option<String>,
    /// Identity for the timer, supporting `{1}` capture placeholders. Timers
    /// with the same key replace each other. Defaults to the interpolated label,
    /// so per-target timers should set e.g. `key = "mez:{1}"`.
    #[serde(default)]
    pub key: Option<String>,
    /// If set, this trigger CLEARS active timers whose key matches (also
    /// supports `{1}`). Used for "<spell> has worn off of <target>" lines.
    #[serde(default)]
    pub clears: Option<String>,
    /// If set, this trigger removes ALL timers on the given target (every
    /// `spell:<target>` key), supporting `{1}`. Used for death lines.
    #[serde(default)]
    pub clears_target: Option<String>,
    /// EQ spell-icon index for this timer's bar (into `SpellsNN.tga`).
    #[serde(default)]
    pub icon: Option<u32>,
    /// Start a timer whose length is auto-learned (observed land→wear-off,
    /// running max) rather than configured. Set by the `[[spell]]` expansion.
    #[serde(default)]
    pub auto_duration: bool,
}

/// A spell to auto-track. Give its `name` and the `land` line (target = `{1}`);
/// the "Your <name> spell has worn off of <target>." clear is derived. With no
/// duration set, the bar length is auto-learned from land→wear-off observations.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpellConfig {
    pub name: String,
    /// Regex for the "landed" line; capture group 1 = the target name.
    pub land: String,
    /// Short tag for the bar label (e.g. "Mez"). Defaults to `name`.
    #[serde(default)]
    pub short: Option<String>,
    #[serde(default)]
    pub timer_seconds: Option<u64>,
    #[serde(default)]
    pub duration_formula: Option<i64>,
    #[serde(default)]
    pub duration_base: Option<i64>,
    /// EQ spell-icon index (field [76] in spells_us.txt).
    #[serde(default)]
    pub icon: Option<u32>,
}

impl Config {
    /// Read and parse a config file from disk, then merge in the shareable rare
    /// database (if present) so community respawn data stays out of the config.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let mut cfg =
            Self::parse(&text).with_context(|| format!("in config file: {}", path.display()))?;
        cfg.merge_rare_db(path.parent())?;
        Ok(cfg)
    }

    /// Append entries from the shareable rare DB (`[general] rare_db`, default
    /// `rares.toml` next to the config). A missing file is fine — the DB is
    /// optional. Personal `[[rare]]` entries in the config win over DB ones with
    /// the same name (they're kept after, so they overwrite in the lookup map).
    fn merge_rare_db(&mut self, config_dir: Option<&Path>) -> Result<()> {
        let rel = self.general.rare_db.clone().unwrap_or_else(|| "rares.toml".into());
        let path = if Path::new(&rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            config_dir.unwrap_or_else(|| Path::new(".")).join(rel)
        };
        // Remember the path even when the file is missing — the in-game `add`
        // command creates it on first use.
        self.rare_db_path = Some(path.clone());
        if !path.exists() {
            return Ok(());
        }
        #[derive(Deserialize, Default)]
        struct RareDb {
            #[serde(default, rename = "rare")]
            rares: Vec<RareConfig>,
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read rare DB: {}", path.display()))?;
        let db: RareDb = toml::from_str(&text)
            .with_context(|| format!("failed to parse rare DB: {}", path.display()))?;
        // DB entries first, then existing config entries — so config overrides DB.
        let mut merged = db.rares;
        merged.append(&mut self.rares);
        // Dedupe by name (case-insensitive): the LAST entry wins (config over
        // DB, newer over older), keeping the first occurrence's position so a
        // hand-ordered file doesn't reshuffle in the UI.
        let mut order: Vec<String> = Vec::new();
        let mut by_name: std::collections::HashMap<String, RareConfig> =
            std::collections::HashMap::new();
        for r in merged {
            let key = r.name.to_lowercase();
            if !by_name.contains_key(&key) {
                order.push(key.clone());
            }
            by_name.insert(key, r);
        }
        self.rares = order
            .into_iter()
            .filter_map(|k| by_name.remove(&k))
            .collect();
        Ok(())
    }

    /// Resolve which log to tail: explicit `cli_log` wins, then `[general]
    /// log_path`, then the newest `eqlog_*.txt` in `[general] log_dir` (so the
    /// overlay follows whichever character/server you last played).
    pub fn resolve_log_path(&self, cli_log: Option<PathBuf>) -> Option<PathBuf> {
        if cli_log.is_some() {
            return cli_log;
        }
        if let Some(p) = &self.general.log_path {
            return Some(PathBuf::from(p));
        }
        self.general
            .log_dir
            .as_ref()
            .and_then(|d| newest_eqlog(Path::new(d)))
    }

    /// Parse a config from a TOML string. Expands any `[[spell]]` entries into
    /// their land + auto-derived wear-off triggers.
    pub fn parse(text: &str) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text).context("failed to parse TOML config")?;
        cfg.expand_spells();
        Ok(cfg)
    }

    /// Turn each `[[spell]]` into a land trigger (starts a per-target timer) plus
    /// an auto-derived "Your <name> spell has worn off of <target>." clear.
    fn expand_spells(&mut self) {
        for s in std::mem::take(&mut self.spells) {
            let short = s.short.clone().unwrap_or_else(|| s.name.clone());
            let key = format!("{}:{{1}}", s.name);
            let has_duration = (s.duration_formula.is_some() && s.duration_base.is_some())
                || s.timer_seconds.is_some();

            self.triggers.push(TriggerConfig {
                name: s.name.clone(),
                pattern: s.land.clone(),
                regex: true,
                sound: None,
                timer_seconds: s.timer_seconds,
                duration_formula: s.duration_formula,
                duration_base: s.duration_base,
                timer_label: Some(format!("{{1}} [{short}]")),
                key: Some(key.clone()),
                clears: None,
                clears_target: None,
                icon: s.icon,
                auto_duration: !has_duration,
            });
            self.triggers.push(TriggerConfig {
                name: format!("{} off", s.name),
                pattern: format!(r"Your {} spell has worn off of (.+?)\.", regex::escape(&s.name)),
                regex: true,
                sound: None,
                timer_seconds: None,
                duration_formula: None,
                duration_base: None,
                timer_label: None,
                key: None,
                clears: Some(key),
                clears_target: None,
                icon: None,
                auto_duration: false,
            });
        }
    }
}

/// Parse `eqlog_<Character>_<server>.txt` into (character, server) — EQ names
/// every log this way. Character names can't contain underscores, so the first
/// `_` after the prefix is the separator even if the server name has more.
pub fn char_server_from_log(path: &Path) -> Option<(String, String)> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("eqlog_")?;
    let (ch, srv) = rest.split_once('_')?;
    (!ch.is_empty() && !srv.is_empty()).then(|| (ch.to_string(), srv.to_string()))
}

/// The most-recently-modified `eqlog_*.txt` in `dir` (ignoring "- Copy" backups)
/// — used to auto-follow whichever character/server was last played.
fn newest_eqlog(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("eqlog_") && n.ends_with(".txt") && !n.contains("Copy"))
                .unwrap_or(false)
        })
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rare_db_merge_dedupes_by_name_keeping_last() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rares.toml"),
            "[[rare]]\nname = \"Baron\"\nrespawn_seconds = 300\n\n\
             [[rare]]\nname = \"Priest\"\nrespawn_seconds = 450\n\n\
             [[rare]]\nname = \"baron\"\nrespawn_seconds = 265\n",
        )
        .unwrap();
        let cfgp = dir.path().join("config.toml");
        std::fs::write(&cfgp, "").unwrap();
        let cfg = Config::load(&cfgp).unwrap();
        assert_eq!(cfg.rares.len(), 2, "duplicate Baron collapsed");
        // First occurrence's position, LAST occurrence's values.
        assert_eq!(cfg.rares[0].name, "baron");
        assert_eq!(cfg.rares[0].respawn_seconds, 265);
        assert_eq!(cfg.rares[1].name, "Priest");
    }

    #[test]
    fn parses_minimal_and_full_triggers() {
        let cfg = Config::parse(
            r#"
            [[triggers]]
            name = "plain"
            pattern = "hello"

            [[triggers]]
            name = "full"
            pattern = "world"
            regex = true
            sound = "sounds/a.wav"
            timer_seconds = 30
            timer_label = "the timer"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.triggers.len(), 2);
        assert_eq!(cfg.triggers[0].name, "plain");
        assert!(!cfg.triggers[0].regex);
        assert_eq!(cfg.triggers[0].sound, None);

        assert!(cfg.triggers[1].regex);
        assert_eq!(cfg.triggers[1].timer_seconds, Some(30));
        assert_eq!(cfg.triggers[1].timer_label.as_deref(), Some("the timer"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = Config::parse(
            r#"
            [[triggers]]
            name = "x"
            pattern = "y"
            typo_field = true
            "#,
        )
        .unwrap_err();
        // `{:#}` renders anyhow's full cause chain (Display alone shows only our
        // top-level context). The CLI surfaces the same detail via Debug.
        let chain = format!("{err:#}");
        assert!(chain.contains("typo_field"), "expected unknown-field error, got: {chain}");
    }
}
