//! Loads EverQuest's spell database so the pipeline can resolve *any* spell you
//! cast — its land message, level-scaled duration, and icon — with no
//! hand-maintained trigger list. This is what lets the overlay follow through on
//! "You begin casting X." automatically.
//!
//! Two `^`-delimited files (found next to `uifiles/`), joined by spell id:
//!   `spells_us.txt`     — [0]=id [1]=name [11]=duration formula [12]=base ticks
//!                         [28]=goodEffect (0=detrimental, 1/2=beneficial) [75]=icon
//!   `spells_us_str.txt` — [0]=id [3]=CASTEDMETXT (shown when it lands on YOU,
//!                         e.g. "You feel strong.") [4]=CASTEDOTHERTXT (appended
//!                         after the target on land, " has been mesmerized.")
//!                         [5]=WOROFFMETXT (shown when it fades from YOU,
//!                         "Your strength fades.")
//!
//! We keep two kinds of durationed spell, each worth a countdown bar:
//!   * *detrimental* spells that land on a target — keyed off the CASTEDOTHERTXT
//!     land line (`<target> has been mesmerized.`); the debuff bars on mobs.
//!   * *beneficial* buffs that land on YOU — keyed off the CASTEDMETXT self-land
//!     line and cleared by the WOROFFMETXT self-fade line; the buff bars on you.

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
    /// Detrimental spells only: text appended after the target on landing, e.g.
    /// " has been mesmerized." The land line is exactly `"<target>" + land_suffix`.
    /// Empty for buffs — they land on you, matched via `self_land` instead.
    pub land_suffix: String,
    /// True for beneficial buffs. Buffs are tracked on YOU (self_land/self_fade)
    /// and MUST never be matched through `land_suffix` — a buff's suffix is empty,
    /// which would otherwise match every line. Callers key off this flag.
    pub beneficial: bool,
    /// Buffs only: the whole line shown when this buff lands on YOU, e.g.
    /// "You feel strong." (spells_us_str.txt field [3]). Empty for debuffs.
    pub self_land: String,
    /// Buffs only: the whole line shown when this buff fades from YOU, e.g.
    /// "Your strength fades." (field [5]). Empty for debuffs.
    pub self_fade: String,
    /// Spell id (field [0]), for deterministic tie-breaking.
    pub id: i64,
    /// Lowest level any class can cast this at (fields [36..=51], one per class;
    /// 255/0 = that class can't). None when no class can cast it at all. Used to
    /// disambiguate buffs that share a self-land line: a level-18 character did
    /// not just get hit by a level-44 bard song.
    pub min_level: Option<u32>,
}

/// Class-level columns in `spells_us.txt`: 16 consecutive fields, one per class,
/// holding the level that class gets the spell (255 — or 0 — means never).
const CLASS_LEVEL_FIELDS: std::ops::RangeInclusive<usize> = 36..=51;

/// Lowest level at which ANY class can cast this row, or None if none can.
fn min_class_level(f: &[&str]) -> Option<u32> {
    CLASS_LEVEL_FIELDS
        .filter_map(|i| f.get(i)?.trim().parse::<u32>().ok())
        .filter(|&lv| lv > 0 && lv < 255)
        .min()
}

#[derive(Debug, Default)]
pub struct SpellDb {
    by_name: HashMap<String, SpellInfo>,
    /// self-land line -> every buff that shares it. EQ reuses one message across
    /// unrelated spells ("Your mind sharpens." is a level-44 bard song AND two
    /// enchanter buffs), so the winner is chosen at match time from your cast
    /// history and level — see [`SpellDb::match_self_land`].
    by_self_land: HashMap<String, Vec<String>>,
    /// self-fade line -> every buff name that uses it. A fade clears whichever of
    /// those bars is up (the rest are no-ops), so fade lines shared across a rank
    /// family still clear correctly.
    by_self_fade: HashMap<String, Vec<String>>,
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
            // Buffs never resolve here: their land_suffix is empty (`strip_suffix`
            // would match every line) and they land on you, not a target.
            if info.beneficial {
                continue;
            }
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

    /// Resolve a buff from its self-land line ("You feel strong.") alone — for a
    /// buff someone else cast on YOU, or one whose cast we never saw (an AA or a
    /// clicky logs no "You begin casting"). A full-line exact match, so ordinary
    /// combat text can't trip it.
    ///
    /// EQ reuses land messages across unrelated spells, so pick the best of the
    /// candidates, in this order:
    ///   1. one you've actually cast this session — that's certainly the one;
    ///   2. one your level can have, taking the HIGHEST such requirement (you
    ///      cast the best rank you own, and it rules out e.g. a level-44 bard
    ///      song landing on a level-18 character);
    ///   3. failing both (a buff from a higher-level caster), the lowest id —
    ///      the classic base spell of the family.
    pub fn match_self_land(
        &self,
        msg: &str,
        player_level: u32,
        cast_history: &HashSet<String>,
    ) -> Option<&SpellInfo> {
        let names = self.by_self_land.get(msg.trim())?;
        names
            .iter()
            .filter_map(|n| self.by_name.get(n))
            .max_by_key(|info| {
                let cast_by_you = cast_history.contains(&info.name);
                let castable = info.min_level.is_some_and(|lv| lv <= player_level);
                (
                    cast_by_you,
                    castable,
                    if castable { info.min_level.unwrap_or(0) } else { 0 },
                    // Lowest id wins the remaining ties (Reverse via negation).
                    -info.id,
                )
            })
    }

    /// Every buff name whose self-fade line ("Your strength fades.") is this line
    /// (empty if none) — so the overlay can clear whichever of those bars is up.
    pub fn match_self_fade(&self, msg: &str) -> &[String] {
        self.by_self_fade.get(msg.trim()).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Load + join the two spell files.
    pub fn load(db_path: &Path, str_path: &Path) -> Result<Self> {
        // id -> (self-land [3], other-land [4], self-fade [5]) from the string
        // file. Detrimental bars key off other-land; buff bars off self-land/fade.
        let str_text = std::fs::read_to_string(str_path)
            .with_context(|| format!("reading {}", str_path.display()))?;
        let mut strings: HashMap<i64, (String, String, String)> = HashMap::new();
        for line in str_text.lines() {
            if line.starts_with('#') {
                continue; // header
            }
            let f: Vec<&str> = line.split('^').collect();
            if f.len() <= 4 {
                continue;
            }
            if let Ok(id) = f[0].parse::<i64>() {
                let self_land = f[3].to_string();
                let other_land = f[4].to_string();
                let self_fade = f.get(5).map(|s| s.to_string()).unwrap_or_default();
                strings.insert(id, (self_land, other_land, self_fade));
            }
        }

        let db_text = std::fs::read_to_string(db_path)
            .with_context(|| format!("reading {}", db_path.display()))?;
        let mut by_name: HashMap<String, SpellInfo> = HashMap::new();
        // Buff line indices, accumulated across every beneficial row (all ranks,
        // not just the one kept in `by_name`): self-land -> all names sharing it
        // (the winner is picked at match time), self-fade -> all names using it.
        let mut land_cands: HashMap<String, Vec<String>> = HashMap::new();
        let mut fade_cands: HashMap<String, Vec<String>> = HashMap::new();
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
            let (self_land, other_land, self_fade) = match strings.get(&id) {
                Some(t) => t.clone(),
                None => continue, // no strings => nothing to match on.
            };
            let base: i64 = f[12].parse().unwrap_or(0);
            let formula: i64 = f[11].parse().unwrap_or(0);
            // No duration at all => no bar. `base` is the usual source, but the
            // permanent formulas carry their duration in the FORMULA and leave
            // base at 0 (Lesser Shielding). Dropping those lost the spell from
            // the candidate pool, so "You feel armored." resolved to the only
            // survivor — a level-54 spell a level-12 character can't have.
            if base <= 0 && !matches!(formula, 50 | 51) {
                continue;
            }
            let icon: Option<u32> = f[75].parse().ok().filter(|&i| i > 0);
            // goodEffect: 0 = detrimental, 1/2 = beneficial. Pacify/lull spells are
            // flagged BENEFICIAL even though you cast them on enemies, so their
            // land line still makes a debuff bar — no buff shares that wording.
            let beneficial = f[28] != "0" && !is_pacify_land(&other_land);
            // First spell of a given name wins — ids ascend by era, so the lowest
            // (classic) rank, which is the one a low-level player casts, is kept.
            let info = if beneficial {
                // A buff needs a self-land line to detect it landing on you.
                if self_land.trim().is_empty() {
                    continue;
                }
                // Record this row in the land/fade indices (every rank counts).
                let v = land_cands.entry(self_land.trim().to_string()).or_default();
                if !v.iter().any(|n| n == name) {
                    v.push(name.to_string());
                }
                let fade_key = self_fade.trim();
                if !fade_key.is_empty() {
                    let v = fade_cands.entry(fade_key.to_string()).or_default();
                    if !v.iter().any(|n| n == name) {
                        v.push(name.to_string());
                    }
                }
                SpellInfo {
                    name: name.to_string(),
                    formula,
                    base,
                    icon,
                    land_suffix: String::new(),
                    beneficial: true,
                    self_land,
                    self_fade,
                    id,
                    min_level: min_class_level(&f),
                }
            } else {
                // A debuff needs a lands-on-other line for the bar.
                if other_land.trim().is_empty() {
                    continue;
                }
                SpellInfo {
                    name: name.to_string(),
                    formula,
                    base,
                    icon,
                    land_suffix: other_land,
                    beneficial: false,
                    self_land: String::new(),
                    self_fade: String::new(),
                    id,
                    min_level: min_class_level(&f),
                }
            };
            by_name.entry(name.to_string()).or_insert(info);
        }

        Ok(Self { by_name, by_self_land: land_cands, by_self_fade: fade_cands })
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
        db_line_lv(id, name, formula, base, good, icon, &[])
    }
    /// `classes` = (class field index within 36..=51, level) pairs; every other
    /// class column is 255 ("never gets it"), as in the real file.
    fn db_line_lv(
        id: &str,
        name: &str,
        formula: &str,
        base: &str,
        good: &str,
        icon: &str,
        classes: &[(usize, u32)],
    ) -> String {
        let mut f = vec![String::new(); 173];
        f[0] = id.into();
        f[1] = name.into();
        f[11] = formula.into();
        f[12] = base.into();
        f[28] = good.into();
        f[75] = icon.into();
        for i in CLASS_LEVEL_FIELDS {
            f[i] = "255".into();
        }
        for &(i, lv) in classes {
            f[i] = lv.to_string();
        }
        f.join("^")
    }
    const BRD: usize = 43;
    const ENC: usize = 49;
    fn str_line(id: &str, casted_other: &str) -> String {
        str_line_full(id, "", casted_other, "")
    }
    fn str_line_full(id: &str, self_me: &str, casted_other: &str, wornoff_me: &str) -> String {
        let mut f = vec![String::new(); 6];
        f[0] = id.into();
        f[3] = self_me.into();
        f[4] = casted_other.into();
        f[5] = wornoff_me.into();
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

    #[test]
    fn keeps_beneficial_buffs_and_indexes_self_lines() {
        let dir = tempfile::tempdir().unwrap();
        let dbp = dir.path().join("spells_us.txt");
        let strp = dir.path().join("spells_us_str.txt");

        // Mirrors the real file: "Your mind sharpens." is shared by a level-44
        // BARD song (lowest id!) and two enchanter buffs at levels 11 and 17.
        let db = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            db_line("278", "Spirit of Wolf", "3", "360", "1", "4"), // buff w/ self-land => kept
            db_line("500", "Clarity", "7", "100", "1", "10"), // beneficial but NO self-land => skipped
            db_line_lv("745", "Cassindra's Elegy", "7", "2", "1", "11", &[(BRD, 44)]),
            db_line_lv("2561", "Intellectual Advancement", "11", "270", "1", "141", &[(ENC, 11)]),
            db_line_lv("2562", "Intellectual Superiority", "11", "270", "1", "141", &[(ENC, 17)]),
            db_line("187", "Enthrall", "8", "8", "0", "35"), // detrimental => kept, not a buff
        );
        let strf = format!(
            "#SPELLINDEX^a^b^c^d^e\n{}\n{}\n{}\n{}\n{}\n{}\n",
            str_line_full(
                "278",
                "You feel the spirit of wolf enter you.",
                " is surrounded by a brief lupine aura.",
                "The spirit of wolf leaves you.",
            ),
            str_line("500", " feels clear."), // only other-land, no self-land
            str_line_full("745", "Your mind sharpens.", " looks smart.", "Your mind dulls."),
            str_line_full("2561", "Your mind sharpens.", " looks smart.", "Your mind dulls."),
            str_line_full("2562", "Your mind sharpens.", " looks smart.", "Your mind dulls."),
            str_line("187", " has been enthralled."),
        );
        std::fs::File::create(&dbp).unwrap().write_all(db.as_bytes()).unwrap();
        std::fs::File::create(&strp).unwrap().write_all(strf.as_bytes()).unwrap();

        let sd = SpellDb::load(&dbp, &strp).unwrap();

        // The buff is kept, flagged beneficial, with an empty land_suffix so it
        // can never resolve through the debuff path.
        let sow = sd.get("Spirit of Wolf").expect("SoW kept");
        assert!(sow.beneficial);
        assert_eq!(sow.land_suffix, "");
        assert_eq!(sow.self_land, "You feel the spirit of wolf enter you.");
        assert_eq!(sow.self_fade, "The spirit of wolf leaves you.");
        assert!(sd.get("Clarity").is_none(), "beneficial with no self-land is dropped");

        // Self-land / self-fade resolve the buff by its lines.
        let none = HashSet::new();
        assert_eq!(
            sd.match_self_land("You feel the spirit of wolf enter you.", 18, &none).unwrap().name,
            "Spirit of Wolf"
        );
        assert!(sd
            .match_self_fade("The spirit of wolf leaves you.")
            .contains(&"Spirit of Wolf".to_string()));

        // A buff never resolves through the debuff land path, even if its name is
        // in cast history and its OTHER-land line appears (buffing a groupmate).
        let hist: HashSet<String> = ["Spirit of Wolf".to_string()].into_iter().collect();
        assert!(sd
            .match_land_cast("Groupmate is surrounded by a brief lupine aura.", &hist)
            .is_none());

        // The shared-line case. At level 18 the level-44 bard song is impossible,
        // and of the two enchanter buffs you qualify for, the HIGHER one is the
        // rank you'd actually be casting.
        assert_eq!(
            sd.match_self_land("Your mind sharpens.", 18, &none).unwrap().name,
            "Intellectual Superiority"
        );
        // Below 17 you can only have the lower rank.
        assert_eq!(
            sd.match_self_land("Your mind sharpens.", 12, &none).unwrap().name,
            "Intellectual Advancement"
        );
        // Having actually cast one settles it outright — even a level-44 song
        // (a 44 bard really can be the one who cast it).
        let bard: HashSet<String> = ["Cassindra's Elegy".to_string()].into_iter().collect();
        assert_eq!(
            sd.match_self_land("Your mind sharpens.", 50, &bard).unwrap().name,
            "Cassindra's Elegy"
        );
        // A shared self-fade returns ALL candidates, so whichever bar is up clears.
        let dull = sd.match_self_fade("Your mind dulls.");
        assert!(dull.contains(&"Intellectual Superiority".to_string()));
        assert!(dull.contains(&"Cassindra's Elegy".to_string()));
    }
}
