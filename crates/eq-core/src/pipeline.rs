//! Wire everything together on a background thread:
//!
//! `Tailer` -> `parse_line` -> `Engine::process` -> side effects + `EngineEvent`s
//!
//! `spawn_pipeline` is the integration seam. Give it a `Sender<EngineEvent>` and
//! it drives the whole tail loop, sending events to your receiver. The v1 CLI's
//! receiver prints to stdout; the future overlay's receiver will draw timers.

use anyhow::{Context, Result};
use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::AudioPlayer;
use crate::config::{Config, General};
use crate::events::{EngineEvent, TimerEvent, TriggerEvent};
use crate::duration::duration_seconds;
use crate::parser::parse_line;
use crate::spelldb::{SpellDb, SpellInfo};
use crate::tailer::Tailer;
use crate::triggers::{interpolate, DurationSpec, Engine};
use regex::Regex;

/// Initial bar length for an auto-duration spell we haven't observed yet.
const DEFAULT_AUTO_SECS: u64 = 90;

/// Respawn auto-calibration: a kill-to-kill gap includes noticing + engaging +
/// killing, so the true respawn is roughly the gap minus this.
const ENGAGE_BUFFER_SECS: u64 = 15;
/// Gaps shorter than this aren't a respawn cycle (same-name double kill etc.).
const MIN_CALIBRATION_GAP_SECS: u64 = 60;
/// Never auto-calibrate a respawn below this.
const MIN_RESPAWN_SECS: u64 = 45;

/// How long a "You begin casting X." stays armed waiting for its land line.
const PENDING_TTL: Duration = Duration::from_secs(20);

/// Commands the UI can send INTO the running pipeline (same effects as the
/// in-game chat commands — live rares map + shareable DB file + events out).
#[derive(Debug, Clone)]
pub enum Control {
    AddRare {
        name: String,
        /// None = use the current zone's default respawn (then 5:00).
        respawn_seconds: Option<u64>,
        /// When the mob died, if known — the respawn bar is backdated to it so
        /// adding from the "recent kills" list keeps the countdown accurate.
        killed_at: Option<Instant>,
    },
    RemoveRare { name: String },
    /// Edit a tracked rare's respawn time in place (no new spawn bar).
    SetRespawn { name: String, respawn_seconds: u64 },
}

#[derive(Debug, Clone)]
pub struct PipelineOptions {
    /// Process the whole file before tailing, instead of only new lines.
    pub from_beginning: bool,
    /// Initialize the audio device. If false, sound actions are skipped.
    pub enable_audio: bool,
    /// Max latency before we poll the file even without a filesystem event.
    pub poll_interval: Duration,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        Self {
            from_beginning: false,
            enable_audio: true,
            // Appends normally arrive via a filesystem notification (instant);
            // this only bounds worst-case latency if an event is missed. The
            // idle poll is a single stat — no open — so a tight interval is
            // essentially free (~40 stats/sec).
            poll_interval: Duration::from_millis(25),
        }
    }
}

pub struct PipelineHandle {
    /// Joins the worker thread. In normal live-tail operation the thread runs
    /// until the process exits or the event receiver is dropped; joining is
    /// mainly for surfacing a startup/watcher error.
    pub join: JoinHandle<Result<()>>,
    /// How many spells the auto-tracker resolved from the game files (None =
    /// spell files not found, cast tracking disabled). For status displays.
    pub spells_tracked: Option<usize>,
    /// How many rares have respawn timers configured.
    pub rares_tracked: usize,
}

/// Compile the config, then spawn the tail+match loop on a worker thread.
///
/// Config/regex errors surface *synchronously* (before the thread starts) so
/// the caller sees them immediately.
pub fn spawn_pipeline(
    config: Config,
    log_path: PathBuf,
    opts: PipelineOptions,
    events_tx: Sender<EngineEvent>,
) -> Result<PipelineHandle> {
    let (_ctl, ctl_rx) = std::sync::mpsc::channel();
    drop(_ctl);
    spawn_pipeline_with_control(config, log_path, opts, ctl_rx, events_tx)
}

/// `spawn_pipeline` plus a control channel for UI-driven changes.
pub fn spawn_pipeline_with_control(
    config: Config,
    log_path: PathBuf,
    opts: PipelineOptions,
    control_rx: Receiver<Control>,
    events_tx: Sender<EngineEvent>,
) -> Result<PipelineHandle> {
    let engine = Engine::new(&config).context("failed to build trigger engine")?;
    let initial_level = config.general.player_level.unwrap_or(1);
    let spell_db = load_spell_db(&config.general);
    // name (lowercased) -> (respawn seconds, icon) for rare respawn timers.
    let rares: HashMap<String, (u64, Option<u32>)> = config
        .rares
        .iter()
        .map(|r| (r.name.to_lowercase(), (r.respawn_seconds, r.icon)))
        .collect();

    // Startup status (stdout, alongside the caller's banner).
    match &spell_db {
        Some(db) => println!("  spells  {} detrimental spells auto-tracked", db.len()),
        None => println!("  spells  auto-tracking OFF (spell files not found)"),
    }
    println!("  rares   {} tracked for respawn timers", rares.len());

    let spells_tracked = spell_db.as_ref().map(|d| d.len());
    let rares_tracked = rares.len();
    let rare_db_path = config.rare_db_path.clone();
    let cmd_channel =
        config.general.command_channel.clone().unwrap_or_else(|| "eqov".to_string());
    let zone_respawn = config.zone_respawn.clone();
    let join = thread::spawn(move || {
        run(
            engine,
            log_path,
            opts,
            initial_level,
            spell_db,
            rares,
            rare_db_path,
            cmd_channel,
            zone_respawn,
            control_rx,
            events_tx,
        )
    });
    Ok(PipelineHandle { join, spells_tracked, rares_tracked })
}

#[allow(clippy::too_many_arguments)]
fn run(
    engine: Engine,
    log_path: PathBuf,
    opts: PipelineOptions,
    initial_level: u32,
    spell_db: Option<SpellDb>,
    mut rares: HashMap<String, (u64, Option<u32>)>,
    rare_db_path: Option<PathBuf>,
    cmd_channel: String,
    mut zone_respawn: HashMap<String, u64>,
    control_rx: Receiver<Control>,
    events_tx: Sender<EngineEvent>,
) -> Result<()> {
    // Audio device is created here because rodio's stream is !Send.
    let audio = if opts.enable_audio {
        match AudioPlayer::new() {
            Ok(a) => Some(a),
            Err(e) => {
                eprintln!("[warn] audio disabled: {e}");
                None
            }
        }
    } else {
        None
    };

    // Warn once per missing sound file instead of spamming on every match.
    let mut missing_sound_warned: HashSet<PathBuf> = HashSet::new();

    let mut tailer = Tailer::new(&log_path, opts.from_beginning, opts.poll_interval)
        .context("failed to start tailer")?;

    // Level + zone both come from lines written only ON the event ("Welcome to
    // level N!" / "You have entered X."), so tailing new-lines-only would miss the
    // current values. Do ONE whole-log back-scan up front for the last of each
    // (`scan_current`) — it MUST read the whole file, not a tail window: after a
    // long fight in one zone the last zone-in can be megabytes back. Live lines in
    // the loop below keep both current after launch.
    let level_re = Regex::new(r"Welcome to level (\d+)!").expect("valid level regex");
    let zone_re = Regex::new(r"You have entered (.+?)\.").expect("valid zone regex");
    let (start_zone, start_level) = scan_current(&log_path, &zone_re, &level_re);
    let mut current_zone = start_zone.clone();
    let mut player_level = start_level.unwrap_or(initial_level);
    match start_level {
        Some(l) => println!("  level   {l} (last ding in the log)"),
        None => println!("  level   {player_level} (config default - no ding found in the log)"),
    }
    match start_zone {
        Some(name) => {
            println!("  zone    {name} (from log)");
            if send(&events_tx, EngineEvent::Zone { name }).is_none() {
                return Ok(());
            }
        }
        None => println!("  zone    unknown (no zone-in found in the log yet)"),
    }
    if send(&events_tx, EngineEvent::Level { level: player_level }).is_none() {
        return Ok(());
    }

    // Cast-driven auto-tracking (spell DB): "You begin casting X." arms X's land
    // line; when it lands we start a bar with the DB's duration + icon. A generic
    // "Your X spell has worn off of T." clears it. No per-spell config required.
    let cast_re = Regex::new(r"You begin (?:casting|singing) (.+)\.").expect("valid cast regex");
    let wornoff_re =
        Regex::new(r"Your (.+?) spell has worn off of (.+?)\.").expect("valid wear-off regex");
    // "<target> has been awakened by <attacker>." — the game's explicit
    // mez-break line. A damage break logs BOTH this and the worn-off line; a
    // re-cast refresh logs only a worn-off. The overlay uses the difference to
    // keep refreshed bars alive without ever missing a real break.
    let awaken_re =
        Regex::new(r"^(.+?) has been awakened by .+\.").expect("valid awaken regex");
    let mut pending: Vec<(SpellInfo, Instant)> = Vec::new();
    // Spells the player has cast this session — gates the land-only fallback so a
    // common combat message can't spawn a bar for a spell they never cast.
    let mut cast_history: HashSet<String> = HashSet::new();
    // A failed cast must disarm its pending land, or the arm lingers (up to
    // PENDING_TTL) and another caster's same spell landing nearby would be
    // claimed as ours (standard practice: nparse/EQTool cancel pending on these).
    let castfail_re = Regex::new(r"^Your (?:.+? )?spell (?:is interrupted\.|fizzles!)")
        .expect("valid cast-fail regex");

    // Your own damage output, for the DPS meter: melee + direct spells ("You
    // slash X for N points of ... damage") and DoT ticks ("X has taken N damage
    // from your Spell."). "You <verb> ... for N" is always YOU dealing damage
    // (incoming hits read "X hits YOU for N").
    let hit_re = Regex::new(r"^You [A-Za-z]+ .+? for (\d+) points? of .*damage").expect("hit regex");
    let dot_re = Regex::new(r"has taken (\d+) damage from your ").expect("dot regex");

    // Rare respawn timers: on a tracked rare's death (by anyone) start a
    // countdown to its next spawn.
    let slain_you_re = Regex::new(r"^You have slain (.+?)!").expect("slain regex");
    let slain_by_re = Regex::new(r"^(.+?) has been slain by ").expect("slain-by regex");
    let mut last_slain: Option<String> = None;
    // Most recent in-game `add` (own or remote) — what a bare `remove` undoes.
    let mut last_added: Option<String> = None;
    // Last kill time per tracked rare, for respawn auto-calibration. Cleared
    // on every zone-in: a fresh zone/instance spawns rares up, so a cross-zone
    // "gap" says nothing about the respawn cycle.
    let mut rare_kill_at: HashMap<String, Instant> = HashMap::new();

    // In-game commands via a chat channel: `add [m:ss|secs] [mob name]`
    // registers a rare. Your own bare `add` uses the LAST MOB SLAIN ("kill it,
    // type add"). The default channel (eqov) is community-shared: OTHER
    // members' adds are honored too — but only with an explicit mob name
    // (their "last kill" isn't in your log) — so named adds sync the rare DB
    // across everyone in the channel.
    let chan_cmd_re = channel_cmd_regex(&cmd_channel);
    let chan_other_re = channel_cmd_other_regex(&cmd_channel);

    // Auto-learned durations (spell -> max observed seconds), and the land time
    // of each active keyed effect so we can measure land->wear-off.
    let mut learned: HashMap<String, u64> = HashMap::new();
    let mut land_times: HashMap<String, Instant> = HashMap::new();

    loop {
        // UI-driven changes (Rares tab buttons). Handled between batches, so
        // worst-case latency is one poll interval.
        while let Ok(c) = control_rx.try_recv() {
            match c {
                Control::AddRare { name, respawn_seconds, killed_at } => {
                    last_added = Some(name.clone());
                    let secs = respawn_seconds
                        .or_else(|| zone_default(&zone_respawn, current_zone.as_deref()))
                        .unwrap_or(300);
                    if do_add(
                        &mut rares,
                        &rare_db_path,
                        &events_tx,
                        &name,
                        secs,
                        current_zone.clone(),
                        None,
                        killed_at.unwrap_or_else(Instant::now),
                    )
                    .is_none()
                    {
                        return Ok(());
                    }
                }
                Control::RemoveRare { name } => {
                    match do_remove(&mut rares, &rare_db_path, &events_tx, &name) {
                        None => return Ok(()),
                        Some(true) => {
                            if last_added
                                .as_deref()
                                .is_some_and(|l| l.eq_ignore_ascii_case(&name))
                            {
                                last_added = None;
                            }
                        }
                        Some(false) => {}
                    }
                }
                Control::SetRespawn { name, respawn_seconds } => {
                    let lower = name.to_lowercase();
                    if let Some(&(_, icon)) = rares.get(&lower) {
                        rares.insert(lower, (respawn_seconds, icon));
                        if let Some(p) = &rare_db_path {
                            update_rare_secs_in_file(p, &name, respawn_seconds);
                        }
                        if send(
                            &events_tx,
                            EngineEvent::RareUpdated { name, respawn_seconds },
                        )
                        .is_none()
                        {
                            return Ok(());
                        }
                    }
                }
            }
        }

        let lines = tailer.next_batch()?;
        for raw in lines {
            let parsed = parse_line(&raw);

            if let Some(c) = level_re.captures(&parsed.message) {
                if let Ok(l) = c[1].parse::<u32>() {
                    player_level = l;
                    if send(&events_tx, EngineEvent::Level { level: l }).is_none() {
                        return Ok(());
                    }
                }
            }

            if let Some(c) = zone_re.captures(&parsed.message) {
                let z = c[1].trim();
                if is_zone_name(z) {
                    current_zone = Some(z.to_string());
                    rare_kill_at.clear();
                    if send(&events_tx, EngineEvent::Zone { name: z.to_string() }).is_none() {
                        return Ok(());
                    }
                }
            }

            // --- cast-driven auto-tracking from the spell DB ---
            if let Some(db) = &spell_db {
                // You cast a detrimental spell: arm its land line. Ranked casts
                // ("Mesmerization III") aren't in the client spell file — fall
                // back to the base name, which is also the name the server uses
                // in the wear-off line, so keys align.
                if let Some(c) = cast_re.captures(&parsed.message) {
                    let cast_name = c[1].trim();
                    let base = crate::spelldb::base_spell_name(cast_name);
                    let hit = db.get(cast_name).or_else(|| base.and_then(|b| db.get(b)));
                    if let Some(info) = hit {
                        let mut info = info.clone();
                        if let Some(b) = base {
                            info.name = b.to_string();
                        }
                        cast_history.insert(info.name.clone());
                        pending.retain(|(_, at)| at.elapsed() < PENDING_TTL);
                        pending.push((info, Instant::now()));
                    }
                }
                // The cast failed — its land line is never coming; disarm.
                if !pending.is_empty() && castfail_re.is_match(&parsed.message) {
                    pending.clear();
                }
                // A debuff landed: prefer the cast-armed spell (exact), else fall
                // back to the land message alone — the cast may not have been seen
                // (overlay launched mid-fight, or the cast line was missed).
                let landed: Option<SpellInfo> = if let Some(idx) = pending.iter().position(|(info, _)| {
                    parsed
                        .message
                        .strip_suffix(&info.land_suffix)
                        .map(|t| !t.trim().is_empty() && t != "You")
                        .unwrap_or(false)
                }) {
                    Some(pending.remove(idx).0)
                } else {
                    db.match_land_cast(&parsed.message, &cast_history).cloned()
                };
                if let Some(info) = landed {
                    let target = parsed
                        .message
                        .strip_suffix(&info.land_suffix)
                        .map(|t| t.trim_end().to_string())
                        .unwrap_or_default();
                    // Formula gives the BASE rank's duration; mote-ranked spells
                    // ("Mesmerization III") run longer, and the ranked data isn't
                    // in the client files. Grow the bar to the longest measured
                    // land->wear-off (an early break can only measure SHORT, so
                    // max() never shrinks it).
                    let base_secs = duration_seconds(player_level as i64, info.formula, info.base);
                    let secs = base_secs.max(learned.get(&info.name).copied().unwrap_or(0));
                    if !target.is_empty() && secs > 0 {
                        let key = format!("{}:{}", info.name, target);
                        land_times.insert(key.clone(), Instant::now());
                        if send(
                            &events_tx,
                            EngineEvent::Timer(TimerEvent {
                                key,
                                trigger: info.name.clone(),
                                icon: info.icon,
                                label: format!("{target} [{}]", categorize(&info.name, &info.land_suffix)),
                                duration: Duration::from_secs(secs),
                                started_at: Instant::now(),
                                started_wall: Local::now(),
                            }),
                        )
                        .is_none()
                        {
                            return Ok(());
                        }
                    }
                }
            }

            // A spell wore off a target — clear its bar (works for any spell).
            if let Some(c) = wornoff_re.captures(&parsed.message) {
                let spell = c[1].trim();
                let key = format!("{}:{}", spell, c[2].trim());
                // Learn the spell's REAL duration from land->wear-off, so
                // mote-ranked spells outgrow the base-rank formula. Guarded to
                // plausible values: a wear-off long after the known duration
                // means the land was missed (or it's a stale key), not a rank
                // bonus — learning it would poison every future bar.
                if let Some(t0) = land_times.remove(&key) {
                    let actual = t0.elapsed().as_secs();
                    if let Some(info) = spell_db.as_ref().and_then(|db| db.get(spell)) {
                        let base_secs =
                            duration_seconds(player_level as i64, info.formula, info.base);
                        let known = base_secs.max(learned.get(spell).copied().unwrap_or(0));
                        // Plausibility cap, with a 2-minute floor: some spells
                        // carry garbage duration data (Entrancing Lights: base
                        // = 1 tick = 6s while the real mez runs far longer),
                        // and a 2x-of-known cap alone would reject the true
                        // value forever. Stale-key inflation is prevented by
                        // clearing land_times on death (below).
                        let cap = (known * 2 + 15).max(120);
                        if actual > 0 && actual <= cap {
                            let e = learned.entry(spell.to_string()).or_insert(0);
                            *e = (*e).max(actual);
                        }
                    }
                }
                if send(&events_tx, EngineEvent::ClearTimer { key }).is_none() {
                    return Ok(());
                }
            }

            // A mez was broken by damage (explicit break line).
            if let Some(c) = awaken_re.captures(&parsed.message) {
                let target = c[1].trim().to_string();
                if send(&events_tx, EngineEvent::MezBroken { target }).is_none() {
                    return Ok(());
                }
            }

            // --- your damage output (fed to the DPS meter) ---
            if let Some(c) = hit_re
                .captures(&parsed.message)
                .or_else(|| dot_re.captures(&parsed.message))
            {
                if let Ok(amount) = c[1].parse::<u64>() {
                    if send(&events_tx, EngineEvent::Damage { amount }).is_none() {
                        return Ok(());
                    }
                }
            }

            // --- a mob died (killed by anyone) ---
            if let Some(name) = slain_you_re
                .captures(&parsed.message)
                .or_else(|| slain_by_re.captures(&parsed.message))
                .map(|c| c[1].to_string())
            {
                last_slain = Some(name.clone());
                // Death clears every bar on the mob — built into the engine, so
                // a generated config needs no boilerplate triggers for it.
                if send(&events_tx, EngineEvent::ClearTarget { target: name.clone() }).is_none() {
                    return Ok(());
                }
                // Also drop its pending land->wear-off measurements: a corpse
                // never produces a wear-off, and a stale entry would poison
                // duration learning if the key were ever reused.
                let dead_suffix = format!(":{}", name.to_lowercase());
                land_times.retain(|k, _| !k.to_lowercase().ends_with(&dead_suffix));
                // Tracked rare: maybe tighten its respawn from the camped
                // kill-to-kill gap, then start the countdown.
                let lower = name.to_lowercase();
                if let Some(&(mut secs, icon)) = rares.get(&lower) {
                    if let Some(prev) = rare_kill_at.get(&lower) {
                        let gap = prev.elapsed().as_secs();
                        let candidate = gap.saturating_sub(ENGAGE_BUFFER_SECS);
                        // Tighten-only: gaps overestimate the respawn (they
                        // include re-engage time), so the smallest plausible
                        // gap is the best estimate and can only shrink it.
                        if gap >= MIN_CALIBRATION_GAP_SECS
                            && candidate >= MIN_RESPAWN_SECS
                            && candidate < secs
                        {
                            secs = candidate;
                            rares.insert(lower.clone(), (secs, icon));
                            if let Some(p) = &rare_db_path {
                                update_rare_secs_in_file(p, &name, secs);
                            }
                            if send(
                                &events_tx,
                                EngineEvent::RareUpdated {
                                    name: name.clone(),
                                    respawn_seconds: secs,
                                },
                            )
                            .is_none()
                            {
                                return Ok(());
                            }
                        }
                    }
                    rare_kill_at.insert(lower.clone(), Instant::now());
                    if send(
                        &events_tx,
                        EngineEvent::Timer(TimerEvent {
                            key: format!("respawn:{lower}"),
                            trigger: String::new(), // no "(spell)" on spawn bars
                            icon,
                            label: format!("{name} [Spawn]"),
                            duration: Duration::from_secs(secs),
                            started_at: Instant::now(),
                            started_wall: Local::now(),
                        }),
                    )
                    .is_none()
                    {
                        return Ok(());
                    }
                }
            }

            // --- in-game "add rare" command (own message, or another channel
            //     member's — the shared-database case) ---
            let cmd = chan_cmd_re
                .captures(&parsed.message)
                .map(|c| (None, c[1].trim().to_string()))
                .or_else(|| {
                    chan_other_re
                        .captures(&parsed.message)
                        .map(|c| (Some(c[1].to_string()), c[2].trim().to_string()))
                });
            if let Some((sender, cmd)) = cmd {
                if let Some(rest) = cmd.strip_prefix("add").filter(|r| r.is_empty() || r.starts_with(' ')) {
                    let (given, explicit_name) = parse_add(rest.trim());
                    let secs = given
                        .or_else(|| zone_default(&zone_respawn, current_zone.as_deref()))
                        .unwrap_or(300);
                    // A remote bare `add` refers to THEIR last kill — which we
                    // can't see — so remote adds need an explicit name.
                    let name = match &sender {
                        None => explicit_name.or_else(|| last_slain.clone()),
                        Some(_) => explicit_name,
                    };
                    if let Some(name) = name {
                        last_added = Some(name.clone());
                        // Zone is only trustworthy for our own adds; the mob
                        // was (typically) just killed, so start the bar now.
                        let zone =
                            if sender.is_none() { current_zone.clone() } else { None };
                        if do_add(
                            &mut rares,
                            &rare_db_path,
                            &events_tx,
                            &name,
                            secs,
                            zone,
                            sender.as_deref(),
                            Instant::now(),
                        )
                        .is_none()
                        {
                            return Ok(());
                        }
                    }
                } else if let Some(rest) =
                    cmd.strip_prefix("remove").filter(|r| r.is_empty() || r.starts_with(' '))
                {
                    let rest = rest.trim();
                    // Bare `remove` (own only) undoes the most recent add —
                    // or drops the last mob slain. Remote removes need a name,
                    // same rule as remote adds.
                    let name = if rest.is_empty() {
                        match &sender {
                            None => last_added.clone().or_else(|| last_slain.clone()),
                            Some(_) => None,
                        }
                    } else {
                        Some(rest.to_string())
                    };
                    if let Some(name) = name {
                        match do_remove(&mut rares, &rare_db_path, &events_tx, &name) {
                            None => return Ok(()),
                            Some(true) => {
                                if last_added
                                    .as_deref()
                                    .is_some_and(|l| l.eq_ignore_ascii_case(&name))
                                {
                                    last_added = None;
                                }
                            }
                            Some(false) => {}
                        }
                    }
                } else if let Some(rest) =
                    cmd.strip_prefix("zone").filter(|r| r.is_empty() || r.starts_with(' '))
                {
                    // `zone 9:30` — set the CURRENT zone's default respawn
                    // (what a bare `add` uses here); `zone clear` removes it.
                    // Own messages only: your zone is yours.
                    if sender.is_none() {
                        if let Some(z) = current_zone.as_deref().map(normalize_zone) {
                            let rest = rest.trim();
                            let new = if rest.eq_ignore_ascii_case("clear") {
                                zone_respawn.remove(&z);
                                Some(None)
                            } else {
                                parse_secs(rest).map(|s| {
                                    zone_respawn.insert(z.clone(), s);
                                    Some(s)
                                })
                            };
                            if let Some(secs) = new {
                                if let Some(p) = &rare_db_path {
                                    upsert_zone_respawn_in_file(p, &z, secs);
                                }
                                if send(
                                    &events_tx,
                                    EngineEvent::ZoneDefaultSet {
                                        zone: z,
                                        respawn_seconds: secs,
                                    },
                                )
                                .is_none()
                                {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }

            for fired in engine.process(&parsed.message) {
                let trig = fired.trigger;
                let caps = &fired.captures;

                // --- action: clear timers (e.g. a spell wore off / broke) ---
                if let Some(clears) = &trig.clears {
                    let key = interpolate(clears, caps);
                    // Learn this spell's real duration from land->wear-off — take
                    // the max so an early break never shrinks the estimate.
                    if let Some(t0) = land_times.remove(&key) {
                        let actual = t0.elapsed().as_secs();
                        if actual > 0 {
                            let spell = key.split(':').next().unwrap_or(&key).to_string();
                            let entry = learned.entry(spell).or_insert(0);
                            *entry = (*entry).max(actual);
                        }
                    }
                    if send(&events_tx, EngineEvent::ClearTimer { key }).is_none() {
                        return Ok(());
                    }
                }

                // --- action: clear ALL timers on a target (the mob died) ---
                if let Some(ct) = &trig.clears_target {
                    let target = interpolate(ct, caps);
                    if send(&events_tx, EngineEvent::ClearTarget { target }).is_none() {
                        return Ok(());
                    }
                }

                // A pure clear/death trigger stays silent: no sound, feed, timer.
                if (trig.clears.is_some() || trig.clears_target.is_some())
                    && trig.timer.is_none()
                    && trig.sound.is_none()
                {
                    continue;
                }

                // --- side effect: sound (only when audio is active) ---
                if let (Some(audio), Some(sound)) = (&audio, &trig.sound) {
                    if sound.exists() {
                        audio.play(sound);
                    } else if missing_sound_warned.insert(sound.clone()) {
                        eprintln!(
                            "[warn] sound file not found, '{}' will fire silently: {}",
                            trig.name,
                            sound.display()
                        );
                    }
                }

                // --- event: trigger fired (feed) ---
                if send(
                    &events_tx,
                    EngineEvent::Trigger(TriggerEvent {
                        trigger: trig.name.clone(),
                        message: parsed.message.to_string(),
                        at: Local::now(),
                    }),
                )
                .is_none()
                {
                    return Ok(()); // receiver dropped; shut down cleanly.
                }

                // --- event: timer start (keyed; level-scaled or auto-learned) ---
                if let Some(timer) = &trig.timer {
                    let label = interpolate(&timer.label, caps);
                    let key = trig
                        .key
                        .as_ref()
                        .map(|k| interpolate(k, caps))
                        .unwrap_or_else(|| label.clone());
                    let spell = key.split(':').next().unwrap_or(&key).to_string();

                    let configured = match &timer.duration {
                        DurationSpec::Fixed(s) => *s,
                        DurationSpec::Formula { formula, base_ticks } => {
                            duration_seconds(player_level as i64, *formula, *base_ticks)
                        }
                        DurationSpec::Auto => 0,
                    };
                    // Formula/Fixed is authoritative — an effect can only END
                    // early (break), never last longer — so trust it. For
                    // auto-duration spells, learning may only GROW the bar; never
                    // let an early break (common for pacify/mez) shrink it below
                    // the default.
                    let learned_secs = learned.get(&spell).copied().unwrap_or(0);
                    let mut secs = configured.max(learned_secs);
                    if configured == 0 {
                        secs = secs.max(DEFAULT_AUTO_SECS);
                    }

                    land_times.insert(key.clone(), Instant::now());

                    if send(
                        &events_tx,
                        EngineEvent::Timer(TimerEvent {
                            key,
                            trigger: trig.name.clone(),
                            icon: trig.icon,
                            label,
                            duration: Duration::from_secs(secs),
                            started_at: Instant::now(),
                            started_wall: Local::now(),
                        }),
                    )
                    .is_none()
                    {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Send, mapping a dropped receiver to `None` so the caller can stop the loop.
fn send(tx: &Sender<EngineEvent>, ev: EngineEvent) -> Option<()> {
    tx.send(ev).ok()
}

/// Regex matching the player's OWN sends to the command channel:
/// `You tell <channel>:<n>, '<command>'`. Case-insensitive on the channel
/// name; incoming messages read "<Name> tells …" so others can't spoof it.
fn channel_cmd_regex(channel: &str) -> Regex {
    Regex::new(&format!(r"^You tell (?i:{})\S*, '(.+)'$", regex::escape(channel)))
        .expect("valid channel command regex")
}

/// Regex matching OTHER members' sends to the command channel:
/// `<Name> tells <channel>:<n>, '<command>'` — the shared-database path.
fn channel_cmd_other_regex(channel: &str) -> Regex {
    Regex::new(&format!(r"^(\w+) tells (?i:{})\S*, '(.+)'$", regex::escape(channel)))
        .expect("valid channel command regex")
}

/// Grammar of `add`: the time may come before OR after the name — people type
/// both `add 9:30 a frenzied ghoul` and `add a frenzied ghoul 9:30`. Returns
/// (explicit secs, explicit name); a missing time falls back to the zone
/// default, then 5:00.
fn parse_add(rest: &str) -> (Option<u64>, Option<String>) {
    if rest.is_empty() {
        return (None, None);
    }
    if let Some(s) = parse_secs(rest) {
        return (Some(s), None); // just a time
    }
    // Leading time: "9:30 name…"
    if let Some((first, tail)) = rest.split_once(' ') {
        if let Some(s) = parse_secs(first) {
            let t = tail.trim();
            return (Some(s), (!t.is_empty()).then(|| t.to_string()));
        }
    }
    // Trailing time: "name… 9:30"
    if let Some((head, last)) = rest.rsplit_once(' ') {
        if let Some(s) = parse_secs(last) {
            let h = head.trim();
            return (Some(s), (!h.is_empty()).then(|| h.to_string()));
        }
    }
    (None, Some(rest.to_string()))
}

/// Look up the default respawn for the (normalized) current zone.
fn zone_default(zone_respawn: &HashMap<String, u64>, zone: Option<&str>) -> Option<u64> {
    zone.map(normalize_zone).and_then(|z| zone_respawn.get(&z).copied())
}

/// Normalize a zone name so instances share one identity: drop a trailing
/// parenthetical and a trailing instance number — "Befallen 4 (Refined)" and
/// "Befallen 2 (Adaptive)" both become "Befallen".
pub fn normalize_zone(z: &str) -> String {
    let mut s = z.trim();
    if s.ends_with(')') {
        if let Some(i) = s.rfind(" (") {
            s = &s[..i];
        }
    }
    let s = s.trim_end();
    let s = match s.rsplit_once(' ') {
        Some((head, last)) if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) => head,
        _ => s,
    };
    s.trim_end().to_string()
}

/// Upsert (or remove, when `secs` is None) a `[zone_respawn]` entry in the DB
/// file — same byte-preserving surgery as the rare add/remove helpers.
fn upsert_zone_respawn_in_file(path: &Path, zone: &str, secs: Option<u64>) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let new_line = secs.map(|s| format!("\"{}\" = {s}", toml_escape(zone)));
    let mut out = String::new();
    let mut in_table = false;
    let mut table_found = false;
    let mut done = false;

    for line in text.lines() {
        let t = line.trim();
        if t == "[zone_respawn]" {
            in_table = true;
            table_found = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_table {
            if t.starts_with('[') {
                // Table ends here — insert the new entry before leaving it.
                if !done {
                    if let Some(l) = &new_line {
                        out.push_str(l);
                        out.push('\n');
                    }
                    done = true;
                }
                in_table = false;
            } else if let Some(eq) = t.find('=') {
                let key = t[..eq].trim().trim_matches('"');
                if key.eq_ignore_ascii_case(zone) {
                    // Replace (or drop) the existing entry in place, keeping
                    // the key's original casing.
                    if !done {
                        if let Some(s) = secs {
                            out.push_str(&format!("\"{key}\" = {s}\n"));
                        }
                        done = true;
                    }
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if in_table && !done {
        if let Some(l) = &new_line {
            out.push_str(l);
            out.push('\n');
        }
        done = true;
    }
    if !table_found && !done {
        if let Some(l) = &new_line {
            out.push_str("\n[zone_respawn]\n");
            out.push_str(l);
            out.push('\n');
        }
    }
    if let Err(e) = std::fs::write(path, out) {
        eprintln!("[warn] couldn't rewrite rare DB {}: {e}", path.display());
    }
}

/// Register a rare (live map + DB file + events). `started_at` backdates the
/// respawn bar to the actual kill when known. Returns None if the event
/// receiver is gone (shutdown).
#[allow(clippy::too_many_arguments)]
fn do_add(
    rares: &mut HashMap<String, (u64, Option<u32>)>,
    rare_db_path: &Option<PathBuf>,
    events_tx: &Sender<EngineEvent>,
    name: &str,
    secs: u64,
    zone: Option<String>,
    by: Option<&str>,
    started_at: Instant,
) -> Option<()> {
    rares.insert(name.to_lowercase(), (secs, None));
    if let Some(p) = rare_db_path {
        append_rare(p, name, zone.as_deref(), secs, by);
    }
    send(
        events_tx,
        EngineEvent::RareAdded { name: name.to_string(), respawn_seconds: secs, zone },
    )?;
    send(
        events_tx,
        EngineEvent::Timer(TimerEvent {
            key: format!("respawn:{}", name.to_lowercase()),
            trigger: String::new(),
            icon: None,
            label: format!("{name} [Spawn]"),
            duration: Duration::from_secs(secs),
            started_at,
            started_wall: Local::now(),
        }),
    )
}

/// Unregister a rare (live map + DB file + events). `Some(true)` = removed,
/// `Some(false)` = wasn't tracked, `None` = event receiver gone.
fn do_remove(
    rares: &mut HashMap<String, (u64, Option<u32>)>,
    rare_db_path: &Option<PathBuf>,
    events_tx: &Sender<EngineEvent>,
    name: &str,
) -> Option<bool> {
    let lower = name.to_lowercase();
    if rares.remove(&lower).is_none() {
        return Some(false);
    }
    if let Some(p) = rare_db_path {
        remove_rare_from_file(p, name);
    }
    send(events_tx, EngineEvent::RareRemoved { name: name.to_string() })?;
    send(events_tx, EngineEvent::ClearTimer { key: format!("respawn:{lower}") })?;
    Some(true)
}

/// Rewrite ONLY the `respawn_seconds` line of the named `[[rare]]` block —
/// zone, notes, comments, and every other byte stay untouched.
fn update_rare_secs_in_file(path: &Path, name: &str, secs: u64) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut out = String::new();
    let mut in_target_block = false;
    for line in text.lines() {
        let t = line.trim();
        if t == "[[rare]]" {
            in_target_block = false;
        } else if let Some(v) = t.strip_prefix("name = \"").and_then(|v| v.strip_suffix('"')) {
            // Compare against the ESCAPED name: that's how it's stored on disk,
            // so names with special characters still match (a no-op for the
            // ordinary names that escape unchanged).
            in_target_block = v.eq_ignore_ascii_case(&toml_escape(name));
        } else if in_target_block && t.starts_with("respawn_seconds") {
            out.push_str(&format!("respawn_seconds = {secs}\n"));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if let Err(e) = std::fs::write(path, out) {
        eprintln!("[warn] couldn't rewrite rare DB {}: {e}", path.display());
    }
}

/// Remove a `[[rare]]` block by mob name (case-insensitive) from the DB file,
/// leaving every other byte — including comments and hand-edits — untouched.
/// Text surgery instead of parse-and-rewrite so a shared, hand-annotated file
/// never gets reformatted.
fn remove_rare_from_file(path: &Path, name: &str) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut out = String::new();
    let mut block = String::new();
    let mut in_blocks = false;
    let mut drop_block = false;

    for line in text.lines() {
        if line.trim() == "[[rare]]" {
            if in_blocks && !drop_block {
                out.push_str(&block);
            }
            block.clear();
            drop_block = false;
            in_blocks = true;
            block.push_str(line);
            block.push('\n');
        } else if in_blocks {
            let t = line.trim();
            if let Some(v) = t.strip_prefix("name = \"").and_then(|v| v.strip_suffix('"')) {
                // Match the escaped on-disk form (a no-op for ordinary names).
                if v.eq_ignore_ascii_case(&toml_escape(name)) {
                    drop_block = true;
                }
            }
            block.push_str(line);
            block.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if in_blocks && !drop_block {
        out.push_str(&block);
    }
    if let Err(e) = std::fs::write(path, out) {
        eprintln!("[warn] couldn't rewrite rare DB {}: {e}", path.display());
    }
}

/// "265" or "4:25" -> seconds. Also used by the GUI's respawn input box.
pub fn parse_secs(s: &str) -> Option<u64> {
    if let Some((m, sec)) = s.split_once(':') {
        let (m, sec) = (m.parse::<u64>().ok()?, sec.parse::<u64>().ok()?);
        (sec < 60).then_some(m * 60 + sec)
    } else {
        s.parse().ok()
    }
}

/// Escape a string for a TOML basic (double-quoted) value. Mob names and the
/// credited sender arrive from the in-game command channel, which is UNTRUSTED
/// and community-shared (anyone on the server can `/join eqov`). Writing a raw
/// `"` would break out of the string and corrupt `rares.toml` — an invalid file
/// then fails the whole config load on the next launch. Escape `\` and `"` and
/// drop control characters so hostile input can only ever be an inert value.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // TOML forbids raw control chars in basic strings; \n \r \t have
            // escapes, anything else control-ish is simply dropped.
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c.is_control()) => {}
            c => out.push(c),
        }
    }
    out
}

/// Upsert an in-game-added rare into the shareable DB (creating the file if
/// needed): any existing entry with the same name is removed first, so
/// re-adding UPDATES instead of accumulating duplicate blocks.
fn append_rare(path: &Path, name: &str, zone: Option<&str>, secs: u64, by: Option<&str>) {
    use std::io::Write;
    remove_rare_from_file(path, name);
    let mut s = String::new();
    if !path.exists() {
        s.push_str("# Shareable rare / named respawn database - EverQuest Legends.\n");
    }
    s.push_str(&format!("\n[[rare]]\nname = \"{}\"\n", toml_escape(name)));
    if let Some(z) = zone {
        s.push_str(&format!("zone = \"{}\"\n", toml_escape(z)));
    }
    let credit = match by {
        Some(who) => format!("added in-game by {}", toml_escape(who)),
        None => "added in-game".to_string(),
    };
    s.push_str(&format!(
        "respawn_seconds = {secs}\nnotes = \"{credit} {}\"\n",
        Local::now().format("%Y-%m-%d")
    ));
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            let _ = f.write_all(s.as_bytes());
        }
        Err(e) => eprintln!("[warn] couldn't write rare DB {}: {e}", path.display()),
    }
}

/// Reject "You have entered ..." lines that aren't real zone-ins — the game
/// reuses that phrasing for level/PvP/stance/no-levitation area messages.
fn is_zone_name(z: &str) -> bool {
    if z.is_empty() {
        return false;
    }
    let low = z.to_lowercase();
    !(low.starts_with("an area")
        || low.contains("do not")
        || low.contains("stance")
        || low.contains("pvp"))
}

/// One whole-log read to find the CURRENT zone + level: the LAST match of each.
/// Reads the WHOLE file, not a tail window — after a long fight in one zone the
/// last "You have entered X." can be megabytes behind the end. `.last()` (not
/// max) is also right for level: a reused log (a char deleted then recreated with
/// the same name shares the file) holds the old char's higher levels *earlier*.
fn scan_current(path: &Path, zone_re: &Regex, level_re: &Regex) -> (Option<String>, Option<u32>) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return (None, None),
    };
    let text = String::from_utf8_lossy(&bytes);
    let zone = zone_re
        .captures_iter(&text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|z| is_zone_name(z))
        .last();
    let level = level_re
        .captures_iter(&text)
        .filter_map(|c| c[1].parse::<u32>().ok())
        .last();
    (zone, level)
}

/// Short category tag (drives the bar colour + fallback abbreviation) guessed
/// from an auto-tracked spell's name and land message. Falls back to "Debuff".
fn categorize(name: &str, land: &str) -> &'static str {
    let n = name.to_lowercase();
    let l = land.to_lowercase();
    if l.contains("mesmerized") || l.contains("enthralled") || n.contains("mesmer") || n.contains("enthrall") {
        "Mez"
    } else if l.contains("slows down") || l.contains("yawns") || n.contains("slow") {
        "Slow"
    } else if l.contains("darkness") || n.contains("snare") {
        "Snare"
    } else if l.contains("adheres to the ground") || l.contains("entombed") || n.contains("root") {
        "Root"
    } else if l.contains("less aggressive") || n.contains("lull") || n.contains("soothe") || n.contains("pacif") || n.contains("calm") {
        "Calm"
    } else if l.contains("poisoned") || l.contains("pained") || l.contains("diseased") || n.contains("poison") {
        "DoT"
    } else if l.contains("blinded") || n.contains("blind") {
        "Blind"
    } else {
        "Debuff"
    }
}

/// Resolve the two spell-file paths: explicit config wins, else derive from
/// `icon_dir` (which normally sits at `<eq>/uifiles/default`).
fn spell_paths(g: &General) -> Option<(PathBuf, PathBuf)> {
    if let (Some(a), Some(b)) = (&g.spell_file, &g.spell_str_file) {
        return Some((PathBuf::from(a), PathBuf::from(b)));
    }
    let base = Path::new(g.icon_dir.as_ref()?).parent()?.parent()?;
    let db = base.join("spells_us.txt");
    let strf = base.join("spells_us_str.txt");
    (db.exists() && strf.exists()).then_some((db, strf))
}

/// Load the spell DB if we can find it; on failure, log and return None so the
/// pipeline still runs (just without automatic cast tracking).
fn load_spell_db(g: &General) -> Option<SpellDb> {
    let (db, strf) = spell_paths(g)?;
    match SpellDb::load(&db, &strf) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("[warn] spell DB not loaded ({e:#}); cast auto-tracking disabled");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    #[test]
    fn damage_regexes_capture_only_your_damage() {
        let hit = Regex::new(r"^You [A-Za-z]+ .+? for (\d+) points? of .*damage").unwrap();
        let dot = Regex::new(r"has taken (\d+) damage from your ").unwrap();

        // Your melee / spells (note singular "point", crit suffix, "by Spell").
        assert_eq!(&hit.captures("You slash a black wolf for 8 points of damage.").unwrap()[1], "8");
        assert_eq!(&hit.captures("You hit a fragile pet for 5 points of disease damage by Spike of Disease.").unwrap()[1], "5");
        assert_eq!(&hit.captures("You punch a decaying skeleton for 1 point of damage.").unwrap()[1], "1");
        assert_eq!(&hit.captures("You slash a skeleton for 4 points of damage. (Critical)").unwrap()[1], "4");
        // Your DoT ticks.
        assert_eq!(&dot.captures("A black bear has taken 11 damage from your Blood Siphon Strike.").unwrap()[1], "11");

        // NOT your damage: incoming hits, and damage you took.
        assert!(hit.captures("a decaying skeleton hits YOU for 3 points of damage.").is_none());
        assert!(hit.captures("You have taken 8 damage from Clinging Darkness by the thaumaturgist.").is_none());
        assert!(dot.captures("You have taken 8 damage from Clinging Darkness by the thaumaturgist.").is_none());
    }

    #[test]
    fn castfail_regex_matches_both_failure_forms() {
        let re = Regex::new(r"^Your (?:.+? )?spell (?:is interrupted\.|fizzles!)").unwrap();
        assert!(re.is_match("Your Mesmerization spell is interrupted."));
        assert!(re.is_match("Your spell is interrupted."));
        assert!(re.is_match("Your Tashani spell fizzles!"));
        // Not failures: the wear-off and land lines.
        assert!(!re.is_match("Your Mesmerization spell has worn off of a snake."));
        assert!(!re.is_match("a snake has been mesmerized."));
    }

    #[test]
    fn parses_in_game_add_command_pieces() {
        let re = super::channel_cmd_regex("eqov");
        // Own messages to the private channel match; channel number varies.
        assert_eq!(&re.captures("You tell eqov:3, 'add 4:25'").unwrap()[1], "add 4:25");
        assert_eq!(&re.captures("You tell Eqov:1, 'add'").unwrap()[1], "add");
        // Other channels and other people's messages don't.
        assert!(re.captures("You tell NewPlayers:1, 'add 4:25'").is_none());
        assert!(re.captures("Gruffy tells eqov:3, 'add 4:25'").is_none());
        // A custom channel name is honored (and the default no longer matches).
        let re = super::channel_cmd_regex("shawnsecret");
        assert_eq!(&re.captures("You tell ShawnSecret:4, 'add'").unwrap()[1], "add");
        assert!(re.captures("You tell eqov:3, 'add'").is_none());

        // Other channel members' adds (shared DB): sender + command captured.
        let re = super::channel_cmd_other_regex("eqov");
        let c = re.captures("Gruffy tells eqov:3, 'add 4:25 Baron Telyx V`Zher'").unwrap();
        assert_eq!(&c[1], "Gruffy");
        assert_eq!(&c[2], "add 4:25 Baron Telyx V`Zher");
        assert!(re.captures("You tell eqov:3, 'add'").is_none()); // own line not doubled
        assert!(re.captures("Gruffy tells General:1, 'add'").is_none());

        // add grammar: (explicit secs, explicit name)
        assert_eq!(super::parse_add(""), (None, None));
        assert_eq!(super::parse_add("4:25"), (Some(265), None));
        assert_eq!(
            super::parse_add("4:25 Baron Telyx V`Zher"),
            (Some(265), Some("Baron Telyx V`Zher".to_string()))
        );
        assert_eq!(
            super::parse_add("Baron Telyx V`Zher"),
            (None, Some("Baron Telyx V`Zher".to_string()))
        );
        // Trailing time works too — it's how people naturally type it.
        assert_eq!(
            super::parse_add("a frenzied ghoul 9:30"),
            (Some(570), Some("a frenzied ghoul".to_string()))
        );
        assert_eq!(
            super::parse_add("a frenzied ghoul 660"),
            (Some(660), Some("a frenzied ghoul".to_string()))
        );

        assert_eq!(super::parse_secs("265"), Some(265));
        assert_eq!(super::parse_secs("4:25"), Some(265));
        assert_eq!(super::parse_secs("0:45"), Some(45));
        assert_eq!(super::parse_secs("4:99"), None);
        assert_eq!(super::parse_secs("Baron"), None);
    }

    #[test]
    fn hostile_channel_name_cannot_corrupt_the_rare_db() {
        // A griefer in the shared `eqov` channel sends a mob name laced with
        // TOML-breaking characters. It must land as an INERT string value, and
        // the file must still parse afterward — never a broken config load.
        use std::collections::HashMap;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rares.toml");

        let evil = "x\" \nlog_dir = \"/etc/pwned\"\n[[rare]]\nname = \"y";
        super::append_rare(&path, evil, Some("Zone\"Break"), 300, Some("Grief\"er"));
        // A second add exercises the remove-first upsert path over escaped text.
        super::append_rare(&path, evil, None, 120, None);

        let text = std::fs::read_to_string(&path).unwrap();

        // Structural proof of no injection: the ONLY top-level key is `rare`.
        // The `log_dir`/extra `[[rare]]` the attacker embedded must live inside
        // a string value, not become real TOML structure.
        let value: toml::Value = toml::from_str(&text).expect("hostile input must not corrupt the DB");
        let table = value.as_table().unwrap();
        assert_eq!(table.keys().collect::<Vec<_>>(), vec!["rare"], "unexpected top-level keys: {table:?}");

        // And the name survives verbatim through escape + re-parse; the upsert
        // (add-twice) leaves exactly one entry, not two.
        #[derive(serde::Deserialize)]
        struct Db {
            #[serde(default, rename = "rare")]
            rares: Vec<super::super::config::RareConfig>,
            #[serde(default)]
            zone_respawn: HashMap<String, u64>,
        }
        let db: Db = toml::from_str(&text).unwrap();
        assert_eq!(db.rares.len(), 1, "upsert should leave exactly one entry");
        assert_eq!(db.rares[0].name, evil);
        assert!(db.zone_respawn.is_empty());
    }

    #[test]
    fn zone_names_normalize_across_instances() {
        assert_eq!(super::normalize_zone("Befallen 4 (Refined)"), "Befallen");
        assert_eq!(super::normalize_zone("Befallen 2 (Adaptive)"), "Befallen");
        assert_eq!(super::normalize_zone("New Sebilis Expedition 31"), "New Sebilis Expedition");
        assert_eq!(super::normalize_zone("The Ruins of Old Guk"), "The Ruins of Old Guk");
        assert_eq!(super::normalize_zone("North Freeport"), "North Freeport");
    }

    #[test]
    fn zone_respawn_upserts_into_the_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rares.toml");
        std::fs::write(&p, "# header\n\n[[rare]]\nname = \"Priest Amiaz\"\nrespawn_seconds = 450\n")
            .unwrap();

        // Create the table.
        super::upsert_zone_respawn_in_file(&p, "The Ruins of Old Guk", Some(570));
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("[zone_respawn]"));
        assert!(t.contains("\"The Ruins of Old Guk\" = 570"));
        assert!(t.contains("Priest Amiaz"), "rares untouched");

        // Update in place (no duplicate key), add a second zone.
        super::upsert_zone_respawn_in_file(&p, "the ruins of old guk", Some(540));
        super::upsert_zone_respawn_in_file(&p, "Befallen", Some(300));
        let t = std::fs::read_to_string(&p).unwrap();
        assert_eq!(t.matches("The Ruins of Old Guk").count(), 1);
        assert!(t.contains("= 540"));
        assert!(!t.contains("= 570"));
        assert!(t.contains("\"Befallen\" = 300"));

        // The file still parses as a valid rare DB with both sections.
        let cfgp = dir.path().join("config.toml");
        std::fs::write(&cfgp, "").unwrap();
        let cfg = crate::config::Config::load(&cfgp).unwrap();
        assert_eq!(cfg.zone_respawn.get("The Ruins of Old Guk"), Some(&540));
        assert_eq!(cfg.rares.len(), 1);

        // Clear removes the entry.
        super::upsert_zone_respawn_in_file(&p, "Befallen", None);
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(!t.contains("Befallen"));
    }

    #[test]
    fn respawn_calibration_rewrites_only_the_secs_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rares.toml");
        std::fs::write(
            &p,
            "# header\n\n[[rare]]\nname = \"Baron Telyx V`Zher\"\nzone = \"Befallen\"\nrespawn_seconds = 300\nnotes = \"keep me\"\n\n[[rare]]\nname = \"Priest Amiaz\"\nrespawn_seconds = 450\n",
        )
        .unwrap();
        super::update_rare_secs_in_file(&p, "baron telyx v`zher", 265);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("respawn_seconds = 265"));
        assert!(!text.contains("respawn_seconds = 300"));
        assert!(text.contains("respawn_seconds = 450"), "other entry untouched");
        assert!(text.contains("notes = \"keep me\""));
        assert!(text.contains("# header"));
    }

    #[test]
    fn re_adding_a_rare_updates_instead_of_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rares.toml");
        super::append_rare(&p, "Footman of V`Zher", Some("Befallen"), 300, None);
        super::append_rare(&p, "footman of v`zher", None, 265, Some("Gruffy")); // re-add, case-insens
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("[[rare]]").count(), 1, "one block, not two:\n{text}");
        assert!(text.contains("respawn_seconds = 265"));
        assert!(!text.contains("respawn_seconds = 300"));
    }

    #[test]
    fn remove_rare_surgery_preserves_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rares.toml");
        std::fs::write(
            &p,
            "# header comment stays\n\n[[rare]]\nname = \"Footman of V`Zher\"\nzone = \"Befallen\"\nrespawn_seconds = 300\n\n[[rare]]\n# inline comment inside the doomed block\nname = \"a Teir`Dal rogue\"\nrespawn_seconds = 300\n\n[[rare]]\nname = \"Priest Amiaz\"\nrespawn_seconds = 450\nnotes = \"hand edit survives\"\n",
        )
        .unwrap();

        super::remove_rare_from_file(&p, "A TEIR`DAL ROGUE"); // case-insensitive
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("# header comment stays"));
        assert!(after.contains("Footman of V`Zher"));
        assert!(after.contains("Priest Amiaz"));
        assert!(after.contains("hand edit survives"));
        assert!(!after.contains("Teir`Dal rogue"));
        assert!(!after.contains("doomed block"));

        // Removing something absent is a no-op.
        super::remove_rare_from_file(&p, "nonexistent mob");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), after);
    }

    #[test]
    fn awaken_regex_captures_broken_mez_target() {
        let re = Regex::new(r"^(.+?) has been awakened by .+\.").unwrap();
        // Death lines capitalize the mob name; the overlay compares lowercased.
        assert_eq!(
            &re.captures("Ice boned skeleton has been awakened by Yaro.").unwrap()[1],
            "Ice boned skeleton"
        );
        assert_eq!(
            &re.captures("A Teir`Dal rogue has been awakened by Zasektik.").unwrap()[1],
            "A Teir`Dal rogue"
        );
        // Pet chatter about waking is not a break line.
        assert!(re
            .captures("Zasektik told you, 'I am unable to wake ice boned skeleton, Master.'")
            .is_none());
    }
}
