//! The trigger engine.
//!
//! Two combined-scan strategies keep matching at ~O(line length) regardless of
//! how many triggers you configure:
//!
//! * All plain-substring patterns compile into one Aho-Corasick automaton.
//! * All regex patterns compile into one `regex::RegexSet` for the "which
//!   matched" pass; the individual `Regex` is then run only for the (few)
//!   patterns that matched, to pull out capture groups.
//!
//! Capture groups let a trigger produce a *dynamic* label/key — e.g. the mez
//! target name — which powers per-target timers and clear-on-wear-off.

use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::{Context, Result};
use regex::{Regex, RegexSet};
use std::path::PathBuf;

use crate::config::Config;

/// How long a timer's bar should run.
#[derive(Debug, Clone)]
pub enum DurationSpec {
    /// A fixed number of seconds.
    Fixed(u64),
    /// EQ level-scaled: `min(formula(level), base)` ticks × 6 s. The concrete
    /// length is computed at fire time from the player's current level (see
    /// [`crate::duration`]), so the bar stays correct as you level.
    Formula { formula: i64, base_ticks: i64 },
    /// Length auto-learned at runtime from observed land→wear-off durations.
    Auto,
}

#[derive(Debug, Clone)]
pub struct TimerSpec {
    /// Label template; may contain `{0}`..`{9}` capture placeholders.
    pub label: String,
    pub duration: DurationSpec,
}

/// A compiled trigger. The pattern lives in the combined automaton / regex set;
/// this struct carries the *actions* to take when it fires.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub name: String,
    pub pattern: String,
    pub is_regex: bool,
    pub sound: Option<PathBuf>,
    pub timer: Option<TimerSpec>,
    /// Timer identity template (may contain `{N}`). `None` => use the label, so
    /// per-target timers set e.g. `key = "mez:{1}"`.
    pub key: Option<String>,
    /// If set, firing CLEARS active timers whose key matches this template
    /// (e.g. a "<spell> has worn off of <target>" line).
    pub clears: Option<String>,
    /// If set, firing removes ALL timers on this target (any `spell:<target>`
    /// key) — for death lines, where no per-spell wear-off arrives.
    pub clears_target: Option<String>,
    /// EQ spell-icon index for the bar (into the `SpellsNN.tga` sheets).
    pub icon: Option<u32>,
}

/// One trigger that fired against a line, with its captured groups.
/// `captures[0]` is the whole match; `captures[n]` is group `n` ("" if the
/// group didn't participate). Plain-substring triggers yield just `[matched]`.
pub struct Fired<'a> {
    pub trigger: &'a Trigger,
    pub captures: Vec<String>,
}

#[derive(Debug)]
pub struct Engine {
    triggers: Vec<Trigger>,

    ac: Option<AhoCorasick>,
    ac_to_trigger: Vec<usize>,

    regex_set: Option<RegexSet>,
    rx_to_trigger: Vec<usize>,
    /// Parallel to `rx_to_trigger`: compiled regexes, used to extract captures
    /// only for the patterns the RegexSet reports as matching.
    regexes: Vec<Regex>,
}

impl Engine {
    /// Compile all triggers. Errors name the offending trigger on a bad regex.
    pub fn new(config: &Config) -> Result<Self> {
        let mut triggers = Vec::with_capacity(config.triggers.len());

        let mut plain_patterns = Vec::new();
        let mut ac_to_trigger = Vec::new();
        let mut regex_patterns = Vec::new();
        let mut rx_to_trigger = Vec::new();
        let mut regexes = Vec::new();

        for (idx, t) in config.triggers.iter().enumerate() {
            let label = t.timer_label.clone().unwrap_or_else(|| t.name.clone());
            let timer = if t.auto_duration {
                Some(TimerSpec { label, duration: DurationSpec::Auto })
            } else {
                match (t.duration_formula, t.duration_base, t.timer_seconds) {
                    (Some(formula), Some(base_ticks), _) => Some(TimerSpec {
                        label,
                        duration: DurationSpec::Formula { formula, base_ticks },
                    }),
                    (_, _, Some(seconds)) => Some(TimerSpec {
                        label,
                        duration: DurationSpec::Fixed(seconds),
                    }),
                    _ => None,
                }
            };

            if t.regex {
                let re = Regex::new(&t.pattern).with_context(|| {
                    format!("trigger '{}' has an invalid regex: {}", t.name, t.pattern)
                })?;
                regex_patterns.push(t.pattern.clone());
                rx_to_trigger.push(idx);
                regexes.push(re);
            } else {
                plain_patterns.push(t.pattern.clone());
                ac_to_trigger.push(idx);
            }

            triggers.push(Trigger {
                name: t.name.clone(),
                pattern: t.pattern.clone(),
                is_regex: t.regex,
                sound: t.sound.clone().map(PathBuf::from),
                timer,
                key: t.key.clone(),
                clears: t.clears.clone(),
                clears_target: t.clears_target.clone(),
                icon: t.icon,
            });
        }

        let ac = if plain_patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::Standard)
                    .build(&plain_patterns)
                    .context("failed to build substring automaton")?,
            )
        };

        let regex_set = if regex_patterns.is_empty() {
            None
        } else {
            Some(RegexSet::new(&regex_patterns).context("failed to build regex set")?)
        };

        Ok(Self { triggers, ac, ac_to_trigger, regex_set, rx_to_trigger, regexes })
    }

    pub fn triggers(&self) -> &[Trigger] {
        &self.triggers
    }

    pub fn len(&self) -> usize {
        self.triggers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.triggers.is_empty()
    }

    /// Match `message`; return the triggers that fired (config order,
    /// de-duplicated) each with its capture groups. Pure and deterministic.
    pub fn process(&self, message: &str) -> Vec<Fired<'_>> {
        let mut caps: Vec<Option<Vec<String>>> = vec![None; self.triggers.len()];

        if let Some(ac) = &self.ac {
            for m in ac.find_overlapping_iter(message) {
                let ti = self.ac_to_trigger[m.pattern().as_usize()];
                caps[ti].get_or_insert_with(|| vec![message[m.start()..m.end()].to_string()]);
            }
        }

        if let Some(set) = &self.regex_set {
            for i in set.matches(message).iter() {
                let ti = self.rx_to_trigger[i];
                if caps[ti].is_some() {
                    continue;
                }
                let groups = self.regexes[i]
                    .captures(message)
                    .map(|c| {
                        c.iter()
                            .map(|g| g.map(|m| m.as_str().to_string()).unwrap_or_default())
                            .collect()
                    })
                    .unwrap_or_else(|| vec![message.to_string()]);
                caps[ti] = Some(groups);
            }
        }

        self.triggers
            .iter()
            .zip(caps)
            .filter_map(|(t, c)| c.map(|captures| Fired { trigger: t, captures }))
            .collect()
    }
}

/// Substitute `{0}`, `{1}`, … in `template` with `captures[n]` (missing → "").
/// Non-numeric `{...}` is emitted verbatim.
pub fn interpolate(template: &str, captures: &[String]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        let mut inner = String::new();
        let mut closed = false;
        while let Some(&d) = chars.peek() {
            chars.next();
            if d == '}' {
                closed = true;
                break;
            }
            inner.push(d);
        }
        match (closed, inner.parse::<usize>()) {
            (true, Ok(n)) => {
                if let Some(v) = captures.get(n) {
                    out.push_str(v);
                }
            }
            _ => {
                out.push('{');
                out.push_str(&inner);
                if closed {
                    out.push('}');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn engine(toml: &str) -> Engine {
        Engine::new(&Config::parse(toml).unwrap()).unwrap()
    }

    fn names<'a>(fired: &'a [Fired<'a>]) -> Vec<&'a str> {
        fired.iter().map(|f| f.trigger.name.as_str()).collect()
    }

    #[test]
    fn matches_substring_and_regex_in_one_pass() {
        let e = engine(
            r#"
            [[triggers]]
            name = "CH"
            pattern = "You begin casting Complete Heal"

            [[triggers]]
            name = "hit"
            pattern = "hits YOU for (\\d+)"
            regex = true
            "#,
        );

        assert_eq!(names(&e.process("You begin casting Complete Heal.")), vec!["CH"]);
        assert_eq!(
            names(&e.process("a Fire Elemental hits YOU for 412 points of damage.")),
            vec!["hit"]
        );
        assert_eq!(
            names(&e.process("You begin casting Complete Heal ... hits YOU for 5 points")),
            vec!["CH", "hit"]
        );
        assert!(e.process("nothing interesting here").is_empty());
    }

    #[test]
    fn duplicate_substring_fires_once() {
        let e = engine(
            r#"
            [[triggers]]
            name = "rampage"
            pattern = "RAMPAGE"
            "#,
        );
        assert_eq!(names(&e.process("RAMPAGE RAMPAGE RAMPAGE")), vec!["rampage"]);
    }

    #[test]
    fn invalid_regex_names_the_trigger() {
        let err = Engine::new(
            &Config::parse(
                r#"
                [[triggers]]
                name = "broken"
                pattern = "("
                regex = true
                "#,
            )
            .unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("broken"), "error should name the trigger: {err}");
    }

    #[test]
    fn empty_config_matches_nothing() {
        let e = engine("");
        assert!(e.is_empty());
        assert!(e.process("anything").is_empty());
    }

    #[test]
    fn captures_target_and_interpolates() {
        let e = engine(
            r#"
            [[triggers]]
            name = "Mez"
            pattern = '(.+?) has been mesmerized\.'
            regex = true
            timer_seconds = 48
            timer_label = "{1} [Mez]"
            key = "mez:{1}"

            [[triggers]]
            name = "Mez off"
            pattern = 'Your Mesmerize spell has worn off of (.+?)\.'
            regex = true
            clears = "mez:{1}"
            "#,
        );

        let fired = e.process("orc centurion has been mesmerized.");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].trigger.name, "Mez");
        assert_eq!(fired[0].captures[1], "orc centurion");
        assert_eq!(interpolate("mez:{1}", &fired[0].captures), "mez:orc centurion");
        assert_eq!(interpolate("{1} [Mez]", &fired[0].captures), "orc centurion [Mez]");

        let off = e.process("Your Mesmerize spell has worn off of orc centurion.");
        assert_eq!(off[0].trigger.name, "Mez off");
        let key = interpolate(off[0].trigger.clears.as_deref().unwrap(), &off[0].captures);
        assert_eq!(key, "mez:orc centurion");
    }

    #[test]
    fn interpolate_handles_missing_and_literals() {
        // group 1 absent -> empty
        assert_eq!(interpolate("{1} [Mez]", &["whole".to_string()]), " [Mez]");
        assert_eq!(interpolate("no placeholders", &[]), "no placeholders");
        assert_eq!(interpolate("{notnum}", &[]), "{notnum}");
    }
}
