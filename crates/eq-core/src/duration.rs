//! EverQuest spell-duration formulas.
//!
//! EQ buff/debuff durations are level-scaled: each spell has a *duration
//! formula* (an id) and a *base* duration (a cap, in ticks). The runtime value
//! is `min(formula(level), base)` ticks, and **1 tick = 6 seconds**.
//!
//! Reference: <https://docs.eqemu.io/server/spells/buff-duration-formulas/>
//! (the standard EQEmu `CalcBuffDuration_formula`). The common leveling-range
//! formulas below are validated; exotic ones fall back to the base.
//!
//! Example — Mesmerize is formula 6 (`level/2 + 2`), base 5:
//! `duration_ticks(8, 6, 5) = min(8/2+2, 5) = min(6, 5) = 5` ticks = 30 s,
//! which matches observed EQ Legends logs (flat ~30 s from L6 up).

pub const SECONDS_PER_TICK: u64 = 6;

/// What EQEmu hands back for the "permanent" duration formulas (50/51): a buff
/// that runs until you zone, die, or right-click it off. Far longer than any
/// play session, so consumers treat a timer this long as having no countdown.
pub const PERMANENT_TICKS: i64 = 72_000;

/// True if `secs` came from a permanent formula — show no countdown for it.
pub fn is_permanent(secs: u64) -> bool {
    secs >= (PERMANENT_TICKS as u64) * SECONDS_PER_TICK
}

/// Duration in **ticks** for a spell cast at `level`, using EQ `formula` and
/// `base` (the base/cap, in ticks). Result is capped at `base` (when `base > 0`)
/// and never negative.
pub fn duration_ticks(level: i64, formula: i64, base: i64) -> i64 {
    let raw = match formula {
        0 => 0,
        1 => level / 2,
        2 => level / 2 + 5,
        3 => level * 30,
        4 => {
            if base > 0 {
                base
            } else {
                50
            }
        }
        5 => base, // short fixed
        6 => level / 2 + 2,
        7 => level,
        8 => level + 10,
        9 => level * 2 + 10,
        10 => level * 3 + 10,
        11 => (level + 3) * 30,
        12 => level / 4,
        15 => base,
        // "Until you zone, die, or click it off" — EQEmu returns 72000 ticks for
        // these. Lesser Shielding and ~600 other buffs use them WITH base = 0, so
        // they must not fall through to the base (which would mean "no duration"
        // and lose the spell entirely).
        50 | 51 => return PERMANENT_TICKS,
        // Exotic formulas: fall back to the base cap.
        _ => base,
    };

    let capped = if base > 0 { raw.min(base) } else { raw };
    capped.max(0)
}

/// Same as [`duration_ticks`] but converted to whole seconds.
pub fn duration_seconds(level: i64, formula: i64, base: i64) -> u64 {
    (duration_ticks(level, formula, base).max(0) as u64) * SECONDS_PER_TICK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesmerize_formula_6_base_5_matches_log() {
        // Capped at 5 ticks (30 s) from L6 up — matches observed L8–L10 ~30 s.
        assert_eq!(duration_ticks(8, 6, 5), 5);
        assert_eq!(duration_ticks(9, 6, 5), 5);
        assert_eq!(duration_ticks(10, 6, 5), 5);
        assert_eq!(duration_seconds(10, 6, 5), 30);
        // Below the cap it scales.
        assert_eq!(duration_ticks(4, 6, 5), 4); // 4/2+2 = 4
        assert_eq!(duration_ticks(2, 6, 5), 3); // 2/2+2 = 3
    }

    #[test]
    fn permanent_formulas_ignore_a_zero_base() {
        // Lesser Shielding: formula 50, base 0. Reading the base would say "no
        // duration" and drop the spell, which is how "You feel armored." ended up
        // resolving to a level-54 spell.
        assert_eq!(duration_ticks(12, 50, 0), PERMANENT_TICKS);
        assert_eq!(duration_ticks(60, 51, 0), PERMANENT_TICKS);
        assert!(is_permanent(duration_seconds(12, 50, 0)));
        // Ordinary buffs are never mistaken for permanent.
        assert!(!is_permanent(duration_seconds(34, 3, 360))); // SoW, 36 min
    }

    #[test]
    fn caps_at_base_and_floors_at_zero() {
        assert_eq!(duration_ticks(60, 1, 10), 10); // 60/2=30, capped to 10
        assert_eq!(duration_ticks(1, 7, 0), 1); // no base => uncapped
        assert_eq!(duration_ticks(-5, 3, 20), 0); // never negative
        assert_eq!(duration_seconds(20, 8, 100), (20 + 10) * 6); // level+10 ticks
    }
}
