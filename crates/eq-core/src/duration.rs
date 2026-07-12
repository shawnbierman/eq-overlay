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
        // Exotic / permanent formulas: fall back to the base cap.
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
    fn caps_at_base_and_floors_at_zero() {
        assert_eq!(duration_ticks(60, 1, 10), 10); // 60/2=30, capped to 10
        assert_eq!(duration_ticks(1, 7, 0), 1); // no base => uncapped
        assert_eq!(duration_ticks(-5, 3, 20), 0); // never negative
        assert_eq!(duration_seconds(20, 8, 100), (20 + 10) * 6); // level+10 ticks
    }
}
