//! Loads EverQuest's spell database so the pipeline can resolve *any* spell you
//! cast — its land message, level-scaled duration, and icon — with no
//! hand-maintained trigger list. This is what lets the overlay follow through on
//! "You begin casting X." automatically.
//!
//! Two `^`-delimited files (found next to `uifiles/`), joined by spell id:
//!   `spells_us.txt`     — [0]=id [1]=name [11]=duration formula [12]=base ticks
//!                         [28]=goodEffect (0=detrimental, 1/2=beneficial) [75]=icon
//!   `spells_us_str.txt` — [0]=id [4]=CASTEDOTHERTXT (the text appended after the
//!                         target when the spell lands, e.g. " has been mesmerized.")
//!
//! We keep only *detrimental* spells that land on a target and have a duration —
//! the ones worth a countdown bar.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Everything the pipeline needs to turn a cast + land into a timer.
#[derive(Debug, Clone)]
pub struct SpellInfo {
    pub name: String,
    /// EQ duration formula id (see `duration::duration_ticks`).
    pub formula: i64,
    /// Base/cap duration in ticks (6 s each).
    pub base: i64,
    /// Spell icon index (field [75]) into the `SpellsNN.tga` sheets.
    pub icon: Option<u32>,
    /// Text appended after the target on landing, e.g. " has been mesmerized.".
    /// The land line is exactly `"<target>" + land_suffix`.
    pub land_suffix: String,
}

#[derive(Debug, Default)]
pub struct SpellDb {
    by_name: HashMap<String, SpellInfo>,
}

impl SpellDb {
    /// Look up a spell by the exact name seen in a `You begin casting X.` line.
    pub fn get(&self, name: &str) -> Option<&SpellInfo> {
        self.by_name.get(name)
    }

    /// Resolve a spell from a land line alone (cast not observed), but ONLY to a
    /// spell the player has actually cast this session (`cast_history`). This
    /// stops a shared/common land message like " staggers." (Crushing Presence /
    /// Soul Bond) or " is struck by a sudden force." (Kneel Test) from spawning a
    /// bar for a proc/test spell the player never casts.
    ///
    /// This runs on EVERY log line, so it iterates the handful of spells the
    /// player has cast — never the whole ~10k-spell DB (the fallback can only
    /// ever resolve to a cast spell anyway, so scanning all land suffixes was
    /// pure waste). Longest suffix wins; name breaks ties so spells sharing a
    /// land message (Lull/Soothe) resolve deterministically.
    pub fn match_land_cast(&self, msg: &str, cast_history: &HashSet<String>) -> Option<&SpellInfo> {
        let mut best: Option<&SpellInfo> = None;
        for name in cast_history {
            let Some(info) = self.by_name.get(name) else { continue };
            let lands = msg
                .strip_suffix(info.land_suffix.as_str())
                .map(|t| {
                    let t = t.trim();
                    !t.is_empty() && t != "You"
                })
                .unwrap_or(false);
            let better = match best {
                None => true,
                Some(b) => {
                    info.land_suffix.len() > b.land_suffix.len()
                        || (info.land_suffix.len() == b.land_suffix.len() && info.name < b.name)
                }
            };
            if lands && better {
                best = Some(info);
            }
        }
        best
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Load + join the two spell files.
    pub fn load(db_path: &Path, str_path: &Path) -> Result<Self> {
        // id -> land suffix (CASTEDOTHERTXT), from the string file.
        let str_text = std::fs::read_to_string(str_path)
            .with_context(|| format!("reading {}", str_path.display()))?;
        let mut land: HashMap<i64, String> = HashMap::new();
        for line in str_text.lines() {
            if line.starts_with('#') {
                continue; // header
            }
            let f: Vec<&str> = line.split('^').collect();
            if f.len() > 4 {
                if let Ok(id) = f[0].parse::<i64>() {
                    let suffix = f[4];
                    if !suffix.trim().is_empty() {
                        land.insert(id, suffix.to_string());
                    }
                }
            }
        }

        let db_text = std::fs::read_to_string(db_path)
            .with_context(|| format!("reading {}", db_path.display()))?;
        let mut by_name: HashMap<String, SpellInfo> = HashMap::new();
        for line in db_text.lines() {
            let f: Vec<&str> = line.split('^').collect();
            if f.len() <= 75 {
                continue;
            }
            let id: i64 = match f[0].parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let name = f[1];
            if name.is_empty() {
                continue;
            }
            let land_suffix = match land.get(&id) {
                Some(s) => s.clone(),
                None => continue, // no "lands on other" text => nothing to match on.
            };
            // Track detrimental spells (goodEffect 0). Pacify/lull spells are
            // flagged BENEFICIAL (goodEffect 1) in EQ's data even though you cast
            // them on enemies, so also include anything with a pacify land message
            // — no buff shares that wording.
            if f[28] != "0" && !is_pacify_land(&land_suffix) {
                continue;
            }
            let base: i64 = f[12].parse().unwrap_or(0);
            if base <= 0 {
                continue; // no duration => no bar.
            }
            let formula: i64 = f[11].parse().unwrap_or(0);
            let icon: Option<u32> = f[75].parse().ok().filter(|&i| i > 0);
            // First spell of a given name wins — ids ascend by era, so the lowest
            // (classic) rank, which is the one a low-level player casts, is kept.
            by_name.entry(name.to_string()).or_insert_with(|| SpellInfo {
                name: name.to_string(),
                formula,
                base,
                icon,
                land_suffix,
            });
        }
        Ok(Self { by_name })
    }
}

/// Strip a trailing rank from a cast spell name: "Mesmerization III" ->
/// "Mesmerization", "Ice Comet Rk. II" -> "Ice Comet". Custom servers rank
/// spells server-side; the ranked name appears in "You begin casting X." but
/// does NOT exist in the client's spells_us.txt, and the server logs the
/// wear-off under the BASE name ("Your Mesmerization spell has worn off...").
/// Returns None when there is no rank suffix. Callers should try the exact
/// name first — this only matters once that lookup misses.
pub fn base_spell_name(name: &str) -> Option<&str> {
    let (rest, last) = name.rsplit_once(' ')?;
    // Mote ranks can climb high — accept numerals up to 7 chars ("XXXVIII").
    // All-uppercase roman-charset tokens only; title-case spell words never
    // qualify, and callers try the exact name first anyway.
    let is_roman = !last.is_empty()
        && last.len() <= 7
        && last.chars().all(|c| matches!(c, 'I' | 'V' | 'X' | 'L' | 'C' | 'D' | 'M'));
    if !is_roman {
        return None;
    }
    // Live-style "Rk. II" — drop the "Rk." token too.
    let rest = rest.strip_suffix(" Rk.").unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

/// Pacify / lull line spells are `goodEffect=1` (beneficial) in EQ's data even
/// though you cast them on enemies. Recognise them by their land message — no
/// buff uses this wording — so they still get a bar.
fn is_pacify_land(land: &str) -> bool {
    let l = land.to_ascii_lowercase();
    ["less aggressive", "amiable", "peaceful", "very calm", "calms down"]
        .iter()
        .any(|k| l.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn db_line(id: &str, name: &str, formula: &str, base: &str, good: &str, icon: &str) -> String {
        let mut f = vec![String::new(); 173];
        f[0] = id.into();
        f[1] = name.into();
        f[11] = formula.into();
        f[12] = base.into();
        f[28] = good.into();
        f[75] = icon.into();
        f.join("^")
    }
    fn str_line(id: &str, casted_other: &str) -> String {
        let mut f = vec![String::new(); 6];
        f[0] = id.into();
        f[4] = casted_other.into();
        f.join("^")
    }

    #[test]
    fn keeps_only_detrimental_landing_durationed_spells() {
        let dir = tempfile::tempdir().unwrap();
        let dbp = dir.path().join("spells_us.txt");
        let strp = dir.path().join("spells_us_str.txt");

        let db = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            db_line("187", "Enthrall", "8", "8", "0", "35"), // detrimental => kept
            db_line("501", "Soothe", "8", "25", "1", "39"), // pacify: beneficial-flagged but kept via land
            db_line("500", "Clarity", "7", "100", "1", "10"), // beneficial buff => skipped
            db_line("999", "Firebolt", "0", "0", "0", "50"),  // no duration => skipped
            db_line("42", "Charm", "0", "0", "0", "5"),       // detrimental but no base => skipped
        );
        let strf = format!(
            "#SPELLINDEX^a^b^c^d^e\n{}\n{}\n{}\n",
            str_line("187", " has been enthralled."),
            str_line("501", " looks less aggressive."),
            str_line("500", " feels clear."),
        );
        std::fs::File::create(&dbp).unwrap().write_all(db.as_bytes()).unwrap();
        std::fs::File::create(&strp).unwrap().write_all(strf.as_bytes()).unwrap();

        let sd = SpellDb::load(&dbp, &strp).unwrap();
        assert_eq!(sd.len(), 2, "Enthrall + pacify Soothe should survive");
        let e = sd.get("Enthrall").unwrap();
        assert_eq!(e.formula, 8);
        assert_eq!(e.base, 8);
        assert_eq!(e.icon, Some(35));
        assert_eq!(e.land_suffix, " has been enthralled.");
        assert!(sd.get("Soothe").is_some(), "pacify kept despite goodEffect=1");
        assert!(sd.get("Clarity").is_none()); // beneficial buff
        assert!(sd.get("Firebolt").is_none()); // no duration

        // Ranked cast names resolve to their base spell.
        assert_eq!(base_spell_name("Mesmerization III"), Some("Mesmerization"));
        assert_eq!(base_spell_name("Color Shift IV"), Some("Color Shift"));
        assert_eq!(base_spell_name("Ice Comet Rk. II"), Some("Ice Comet"));
        assert_eq!(base_spell_name("Enthrall XVIII"), Some("Enthrall"));
        // No rank suffix -> None; roman-looking real words don't false-positive
        // in practice because callers try the exact name first.
        assert_eq!(base_spell_name("Enthrall"), None);
        assert_eq!(base_spell_name("Tainted Breath"), None);

        // Land-only fallback: fires only for a spell the player has cast.
        let hist: HashSet<String> = ["Enthrall".to_string()].into_iter().collect();
        assert_eq!(
            sd.match_land_cast("a greater mummy has been enthralled.", &hist).unwrap().name,
            "Enthrall"
        );
        // Same land message, but never cast -> no bogus bar.
        assert!(sd
            .match_land_cast("a greater mummy has been enthralled.", &HashSet::new())
            .is_none());
        assert!(sd.match_land_cast("some unrelated combat line.", &hist).is_none());
    }
}
