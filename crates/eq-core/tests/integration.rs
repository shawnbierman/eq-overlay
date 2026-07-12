//! End-to-end test of the pure path: raw line -> parse -> engine -> triggers.
//! Exercises the public API the same way the pipeline does, minus threads/I/O.

use eq_core::{parse_line, Config, DurationSpec, Engine};

const CONFIG: &str = r#"
[[triggers]]
name = "Complete Heal"
pattern = "You begin casting Complete Heal"
timer_seconds = 10
timer_label = "CH cast"

[[triggers]]
name = "Big hit taken"
pattern = 'hits YOU for (\d+) points of damage'
regex = true

[[triggers]]
name = "Discipline used"
pattern = 'You activate .+ Discipline'
regex = true
timer_seconds = 900
"#;

fn fired_names(engine: &Engine, raw: &str) -> Vec<String> {
    let line = parse_line(raw);
    engine
        .process(&line.message)
        .iter()
        .map(|f| f.trigger.name.clone())
        .collect()
}

#[test]
fn full_config_matches_expected_lines() {
    let engine = Engine::new(&Config::parse(CONFIG).unwrap()).unwrap();

    assert_eq!(
        fired_names(&engine, "[Wed Jul 05 16:17:20 2026] You begin casting Complete Heal."),
        vec!["Complete Heal"]
    );
    assert_eq!(
        fired_names(
            &engine,
            "[Wed Jul 05 16:17:21 2026] a Cursed Wraith hits YOU for 1247 points of damage."
        ),
        vec!["Big hit taken"]
    );
    assert_eq!(
        fired_names(&engine, "[Wed Jul 05 16:17:22 2026] You activate Fortitude Discipline."),
        vec!["Discipline used"]
    );
    assert!(fired_names(&engine, "[Wed Jul 05 16:17:23 2026] Soandso engages you!").is_empty());
}

#[test]
fn timer_specs_are_attached() {
    let engine = Engine::new(&Config::parse(CONFIG).unwrap()).unwrap();
    let line = parse_line("[Wed Jul 05 16:17:20 2026] You begin casting Complete Heal.");
    let fired = engine.process(&line.message);

    let timer = fired[0].trigger.timer.as_ref().expect("CH trigger should carry a timer");
    assert!(matches!(timer.duration, DurationSpec::Fixed(10)));
    assert_eq!(timer.label, "CH cast");
}
