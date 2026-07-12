//! The egui app: drain `EngineEvent`s, keep a list of live countdown timers, and
//! paint them as EQ-style bars — a dark slot with the real game spell icon (from
//! the `SpellsNN.tga` sheets, with a colour-coded square fallback), a depleting
//! fill, the target name, and a right-aligned countdown. Transparent elsewhere.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, Frame, Pos2, Rect, RichText, Rounding, Stroke,
    TextureHandle, TextureOptions, Vec2,
};
use eq_core::EngineEvent;
use tray_icon::menu::{MenuEvent, MenuId};
use tray_icon::{TrayIcon, TrayIconEvent};

/// The tray icon, its menu item ids, and the channels its events arrive on.
/// The receivers are filled in once the egui context exists (the event
/// handlers also wake the UI, so a tray click reacts instantly even while the
/// overlay idles at 2 fps).
pub struct Tray {
    pub _icon: TrayIcon,
    pub settings_id: MenuId,
    pub quit_id: MenuId,
    pub menu_rx: Option<Receiver<MenuEvent>>,
    pub tray_rx: Option<Receiver<TrayIconEvent>>,
}

/// Everything main() learned at startup that the settings window shows or
/// needs for saving.
pub struct StartupInfo {
    /// Where "Save & Restart" writes the config.
    pub config_save_path: PathBuf,
    /// The EverQuest folder currently in use (derived from the config).
    pub game_dir: Option<PathBuf>,
    /// True when this launch auto-detected the game and wrote a fresh config.
    pub game_dir_autodetected: bool,
    pub log_path: Option<PathBuf>,
    pub char_server: Option<(String, String)>,
    pub spells_tracked: Option<usize>,
    pub rares_tracked: usize,
    /// False = no config/log yet; the settings window opens automatically.
    pub pipeline_running: bool,
    /// Overlay geometry (x, y, width, height) — preserved on config rewrite.
    pub overlay: (i32, i32, u32, u32),
    /// Taskbar / alt-tab icon for the settings window (the tray art).
    pub window_icon: std::sync::Arc<egui::IconData>,
    /// The merged rare database (rares.toml + personal entries), for display.
    pub rares: Vec<eq_core::config::RareConfig>,
    /// Private chat channel watched for in-game commands (default "eqov").
    pub command_channel: String,
    /// Master sound switch (config `[audio] enabled`, default true).
    pub audio_enabled: bool,
    /// Spawn-chime sound id or .wav path (config `[audio] spawn_sound`).
    pub spawn_sound: String,
    /// Per-zone default respawns (normalized zone -> secs), live-updated by
    /// the in-game `zone` command.
    pub zone_respawn: HashMap<String, u64>,
}

/// If a timer's clear (wear-off) line is missed, drop it this long past its
/// estimated duration so it can never linger forever.
const CLEAR_GRACE: Duration = Duration::from_secs(15);

/// Rolling window for the DPS readout: `sum(damage in last DPS_WINDOW) /
/// DPS_WINDOW`. Naturally decays to 0 a few seconds after combat stops.
const DPS_WINDOW: Duration = Duration::from_secs(10);

/// A spawn timer chimes once when it pops, then keeps showing "UP" for only this
/// long before dropping itself — so a rare you're not camping doesn't leave a
/// dead window on screen forever.
const RESPAWN_UP_GRACE: Duration = Duration::from_secs(60);

/// When a spawn timer pops it also FLASHES for this long (at the rate below)
/// so the moment catches the eye even with sound off.
const RESPAWN_FLASH_SECS: f32 = 3.0;
const RESPAWN_FLASH_HZ: f32 = 2.0;

/// Lands on the same key within this window of the bar STARTING are one AE
/// volley hitting several same-named mobs (they arrive in the same second) —
/// each one counts. A land after this is a RE-CAST refreshing the bar instead.
const VOLLEY_WINDOW: Duration = Duration::from_secs(2);

/// Re-casting on a still-affected mob makes EQ log the new land AND a spurious
/// "worn off" for the instance being replaced (0-2s later). A fade this soon
/// after a refresh is that artifact — the mob is still mezzed — so swallow it.
/// (A fade paired with a FRESH bar is different: that's a mez that didn't
/// stick, and it must clear the bar — new bars never arm this swallow.)
const REFRESH_FADE_SWALLOW: Duration = Duration::from_secs(2);

/// EQ spell-icon sheets are a 6×6 grid of 40px icons (36 per `SpellsNN.tga`).
const ICONS_PER_SHEET: u32 = 36;
const ICON_PX: u32 = 40;

/// One live timer being counted down on screen.
struct ActiveTimer {
    key: String,
    label: String,
    /// The spell that started this timer (shown in the bar to disambiguate
    /// effects that share an icon, e.g. Languid Pace vs Drowsy).
    spell: String,
    icon: Option<u32>,
    duration: Duration,
    started_at: Instant, // from the engine, so the countdown is accurate
    /// Spawn timers only: set once we've played the "it's up" chime, so it fires
    /// exactly once per pop (a re-kill replaces the timer, re-arming this).
    alerted: bool,
    /// When the spawn timer popped — drives the attention flash.
    up_at: Option<Instant>,
    /// How many same-named mobs currently share this bar. EQ logs carry no
    /// per-mob id, so identically-named mobs collapse to one key; we +1 on each
    /// volley land and -1 on each wear-off so the bar lives until the LAST one
    /// clears (not the first). 1 for the normal single-target case.
    count: u32,
    /// Set when a RE-CAST refreshes this bar: fades arriving before this are
    /// the log's replace-artifact ("worn off" of the old instance) and are
    /// ignored — the mob is still affected, on the new, refreshed countdown.
    swallow_until: Option<Instant>,
    /// When a fade was swallowed as a presumed replace-artifact. If an
    /// "awakened by" line follows, the fade was actually a REAL damage break
    /// of the fresh mez (breaks log both lines; clean refreshes log neither) —
    /// the deferred decrement is applied then.
    swallowed_at: Option<Instant>,
    /// When an "awakened by" break line arrived with no swallowed fade to
    /// apply it to. Stock EQEmu logs the awaken BEFORE the worn-off (this
    /// server logs it after); either order must break through the swallow.
    broken_at: Option<Instant>,
}

impl ActiveTimer {
    fn remaining(&self) -> Duration {
        self.duration.saturating_sub(self.started_at.elapsed())
    }
    fn fraction(&self) -> f32 {
        if self.duration.is_zero() {
            return 0.0;
        }
        (self.remaining().as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }
}

/// Lazily loads + crops + uploads EQ spell icons from the `SpellsNN.tga` sheets,
/// caching each (including failures, so we don't retry a bad path every frame).
struct IconCache {
    dir: Option<PathBuf>,
    prefix: String,
    cache: HashMap<u32, Option<TextureHandle>>,
}

impl IconCache {
    fn new(dir: Option<PathBuf>, prefix: String) -> Self {
        Self { dir, prefix, cache: HashMap::new() }
    }

    fn texture(&mut self, ctx: &egui::Context, idx: u32) -> Option<TextureHandle> {
        if let Some(hit) = self.cache.get(&idx) {
            return hit.clone();
        }
        let tex = self.load(ctx, idx);
        self.cache.insert(idx, tex.clone());
        tex
    }

    fn load(&self, ctx: &egui::Context, idx: u32) -> Option<TextureHandle> {
        let dir = self.dir.as_ref()?;
        let sheet = idx / ICONS_PER_SHEET + 1;
        let img = image::open(dir.join(format!("{}{:02}.tga", self.prefix, sheet)))
            .ok()?
            .to_rgba8();

        let cell = idx % ICONS_PER_SHEET;
        let (x0, y0) = ((cell % 6) * ICON_PX, (cell / 6) * ICON_PX);
        if x0 + ICON_PX > img.width() || y0 + ICON_PX > img.height() {
            return None;
        }

        let mut px = Vec::with_capacity((ICON_PX * ICON_PX * 4) as usize);
        for y in 0..ICON_PX {
            for x in 0..ICON_PX {
                px.extend_from_slice(&img.get_pixel(x0 + x, y0 + y).0);
            }
        }
        let ci = ColorImage::from_rgba_unmultiplied([ICON_PX as usize, ICON_PX as usize], &px);
        Some(ctx.load_texture(format!("eqicon{idx}"), ci, TextureOptions::LINEAR))
    }
}

pub struct OverlayApp {
    rx: Receiver<EngineEvent>,
    timers: Vec<ActiveTimer>,
    icons: IconCache,
    /// Current zone (from the log), shown in the title tab. Empty until known.
    zone: String,
    /// Current player level (startup scan + dings), for the settings window.
    level: Option<u32>,
    /// (received_at, amount) for recent player damage, pruned to `DPS_WINDOW`.
    dmg: Vec<(Instant, u64)>,
    /// Last time we re-asserted always-on-top (the game window can otherwise
    /// steal the top of the topmost band when it's focused / relaunched).
    topmost_at: Instant,
    info: StartupInfo,
    tray: Option<Tray>,
    /// The settings window ALWAYS exists (that's what puts the app in the
    /// taskbar and alt-tab, findable while the game is fullscreen); it starts
    /// minimized unless this is a first run. Closing it just re-minimizes.
    settings_start_minimized: bool,
    /// One-shot: restore + focus the settings window on the next frame
    /// (tray click, or first run).
    focus_settings: bool,
    /// A newly picked (not yet saved) EverQuest folder in the settings window.
    pending_game_dir: Option<PathBuf>,
    /// The command-channel name as edited in Setup (saved on Save & Restart).
    channel_edit: String,
    /// Which settings tab is showing.
    tab: Tab,
    /// Session spell activity for the Spells tab: name -> (bars started, last
    /// bar seconds). Fed by Timer events, so it reflects learned durations.
    spell_stats: HashMap<String, (u32, u64)>,
    /// One-shot: install the BeOS widget style on the first frame.
    styled: bool,
    /// Live channel into the pipeline for UI-driven rare add/remove.
    control_tx: Option<std::sync::mpsc::Sender<eq_core::Control>>,
    /// Sounds on/off + which spawn-chime sound. Applied live; auto-saved.
    audio_enabled: bool,
    spawn_sound: String,
    /// Recent kills (newest first, deduped by name) — the Rares tab's
    /// one-click "Add" candidates. (name, when it died)
    recent_kills: Vec<(String, Instant)>,
    /// Respawn input for UI adds ("4:25" / "265"); empty = 5:00 default.
    add_secs_edit: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Status,
    Rares,
    Spells,
    Audio,
    Setup,
    About,
}

// ── BeOS R5 palette for the settings window ─────────────────────────────────
const BEOS_PANEL: Color32 = Color32::from_rgb(216, 216, 216);
const BEOS_PANEL_DK: Color32 = Color32::from_rgb(184, 184, 184);
const BEOS_YELLOW: Color32 = Color32::from_rgb(255, 203, 0);
const BEOS_YELLOW_HI: Color32 = Color32::from_rgb(255, 233, 128);
const INK: Color32 = Color32::from_rgb(20, 20, 20);
const DIM: Color32 = Color32::from_rgb(90, 90, 90);
const WARN: Color32 = Color32::from_rgb(163, 45, 20);
const ACCENT: Color32 = Color32::from_rgb(112, 78, 0);
const EDGE: Color32 = Color32::from_rgb(96, 96, 96);
const BEVEL_HI: Color32 = Color32::from_rgb(248, 248, 248);
const BEVEL_LO: Color32 = Color32::from_rgb(152, 152, 152);

/// Light, square-cornered, BeOS-flavoured widget styling for the settings
/// window. Global to the egui context — safe because the overlay viewport
/// paints everything with explicit colours and never uses themed widgets.
fn apply_beos_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::light();
    v.panel_fill = BEOS_PANEL;
    v.window_fill = BEOS_PANEL;
    v.extreme_bg_color = Color32::WHITE;
    v.faint_bg_color = Color32::from_rgb(204, 204, 204);
    v.override_text_color = Some(INK);
    v.selection.bg_fill = Color32::from_rgb(178, 196, 222);
    v.selection.stroke = Stroke::new(1.0, INK);
    v.window_rounding = Rounding::ZERO;
    v.menu_rounding = Rounding::ZERO;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = Rounding::ZERO;
        w.fg_stroke = Stroke::new(1.0, INK);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BEOS_PANEL_DK);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(232, 232, 232);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, EDGE);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(242, 242, 242);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, EDGE);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(200, 200, 200);
    v.widgets.active.bg_stroke = Stroke::new(1.0, EDGE);
    ctx.set_visuals(v);
}

impl OverlayApp {
    pub fn new(
        rx: Receiver<EngineEvent>,
        icon_dir: Option<PathBuf>,
        icon_sheet: String,
        info: StartupInfo,
        tray: Option<Tray>,
        control_tx: Option<std::sync::mpsc::Sender<eq_core::Control>>,
    ) -> Self {
        Self {
            rx,
            timers: Vec::new(),
            icons: IconCache::new(icon_dir, icon_sheet),
            zone: String::new(),
            level: None,
            dmg: Vec::new(),
            topmost_at: Instant::now(),
            // Nothing to tail? Bring settings up immediately so the first-run
            // experience is "pick your EverQuest folder", not a blank screen.
            settings_start_minimized: info.pipeline_running,
            focus_settings: !info.pipeline_running,
            tab: if info.pipeline_running { Tab::Status } else { Tab::Setup },
            channel_edit: info.command_channel.clone(),
            audio_enabled: info.audio_enabled,
            spawn_sound: info.spawn_sound.clone(),
            info,
            tray,
            pending_game_dir: None,
            spell_stats: HashMap::new(),
            styled: false,
            control_tx,
            recent_kills: Vec::new(),
            add_secs_edit: String::new(),
        }
    }

    /// Pull everything waiting on the channel and fold it into UI state.
    fn ingest(&mut self) {
        // Track whether the timer SET changed (add/remove/refresh). Between
        // changes the sort order is static — every countdown loses time at the
        // same rate, so pairwise remaining() differences are constant — which
        // lets us sort only on change instead of every frame.
        let mut changed = false;
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                EngineEvent::Timer(t) => {
                    changed = true;
                    let is_respawn = t.key.starts_with("respawn:");
                    if !is_respawn && !t.trigger.is_empty() {
                        let e = self.spell_stats.entry(t.trigger.clone()).or_insert((0, 0));
                        e.0 += 1;
                        e.1 = t.duration.as_secs();
                    }
                    if let Some(x) = self.timers.iter_mut().find(|x| x.key == t.key) {
                        // Same key already live. Within VOLLEY_WINDOW of the bar
                        // starting it's the same AE cast landing on ANOTHER
                        // same-named mob — count it, so one mob's wear-off can't
                        // clear the bar while others are still held. Later than
                        // that it's a RE-CAST refreshing the effect: same mobs
                        // (count unchanged), and EQ will log a spurious "worn
                        // off" for the replaced instance in ~a second — arm the
                        // swallow so it can't kill the freshly refreshed bar.
                        // (Respawn: just a re-kill — restart, no counting.)
                        if !is_respawn {
                            if x.started_at.elapsed() < VOLLEY_WINDOW {
                                x.count += 1;
                            } else {
                                x.swallow_until = Some(Instant::now() + REFRESH_FADE_SWALLOW);
                                x.swallowed_at = None;
                                x.broken_at = None;
                            }
                        }
                        x.label = t.label;
                        x.spell = t.trigger;
                        x.icon = t.icon;
                        x.duration = t.duration;
                        x.started_at = t.started_at;
                        x.alerted = false;
                        x.up_at = None;
                    } else {
                        self.timers.push(ActiveTimer {
                            key: t.key,
                            label: t.label,
                            spell: t.trigger,
                            icon: t.icon,
                            duration: t.duration,
                            started_at: t.started_at,
                            alerted: false,
                            up_at: None,
                            count: 1,
                            swallow_until: None,
                            swallowed_at: None,
                            broken_at: None,
                        });
                    }
                }
                EngineEvent::ClearTimer { key } => {
                    if let Some(x) = self.timers.iter_mut().find(|x| x.key == key) {
                        let awakened_first = x
                            .broken_at
                            .take()
                            .is_some_and(|t| t.elapsed() < REFRESH_FADE_SWALLOW);
                        if !awakened_first && x.swallow_until.is_some_and(|t| Instant::now() < t) {
                            // Fade right after a re-cast: presumed to be the
                            // replace-artifact "worn off" of the old instance —
                            // keep the refreshed bar. Remember it though: if an
                            // "awakened by" break line follows, it was real.
                            x.swallowed_at = Some(Instant::now());
                        } else {
                            // One instance genuinely wore off / broke (a
                            // preceding "awakened by" overrides any swallow):
                            // drop the count; the bar goes once the LAST one
                            // clears.
                            x.count = x.count.saturating_sub(1);
                            changed = true;
                        }
                    }
                    self.timers.retain(|x| x.count > 0);
                }
                EngineEvent::MezBroken { target } => {
                    // Explicit damage-break line. Its worn-off companion is
                    // logged adjacent (after it on this server, before it on
                    // stock EQEmu). If the companion was already swallowed as a
                    // presumed refresh-artifact, it was actually real — apply
                    // the deferred decrement now. If the companion hasn't
                    // arrived yet, mark the bar so the imminent fade breaks
                    // through the swallow. (A fade that already decremented
                    // normally needs nothing — avoiding a double count.)
                    let tgt = target.to_lowercase();
                    for x in self.timers.iter_mut() {
                        let bar_tgt =
                            x.key.splitn(2, ':').nth(1).unwrap_or_default().to_lowercase();
                        if bar_tgt != tgt || split_label(&x.label).1 != "Mez" {
                            continue;
                        }
                        if x.swallowed_at.take().is_some_and(|t| t.elapsed() < REFRESH_FADE_SWALLOW)
                        {
                            x.count = x.count.saturating_sub(1);
                            changed = true;
                        } else if x.swallow_until.is_some_and(|t| Instant::now() < t) {
                            x.broken_at = Some(Instant::now());
                        }
                    }
                    self.timers.retain(|x| x.count > 0);
                }
                EngineEvent::ClearTarget { target } => {
                    // Every death arrives here — remember it as an "Add"
                    // candidate for the Rares tab (newest first, deduped).
                    let lower = target.to_lowercase();
                    self.recent_kills.retain(|(n, _)| n.to_lowercase() != lower);
                    self.recent_kills.insert(0, (target.clone(), Instant::now()));
                    self.recent_kills.truncate(10);
                    // A mob died: clear EVERY bar on that name (and its pet)
                    // outright — NOT a decrement. A kill is unambiguous, and since
                    // same-named mobs share one key, a lingering count would leave
                    // a stale bar sitting on a corpse (e.g. a Tashani bar on a
                    // priestess you just killed, when several priestesses were
                    // tagged). Wear-offs (ClearTimer) still decrement, so a pack
                    // mez'd at once keeps its bar until the LAST one breaks — only
                    // a confirmed kill wipes the name's bars. An EQ pet despawns
                    // silently (no log line) when its owner is killed, so clear the
                    // owner's pet too: pet names embed the owner ("the thaumaturgist"
                    // -> "the thaumaturgist pet"). Case-insensitive (death lines
                    // capitalize the mob name; land lines don't). Respawn timers are
                    // STARTED by a death, never cleared by one — skip them.
                    let owner = target.to_lowercase();
                    let before = self.timers.len();
                    self.timers.retain(|x| {
                        if x.key.starts_with("respawn:") {
                            return true;
                        }
                        let tgt = x.key.splitn(2, ':').nth(1).unwrap_or_default().to_lowercase();
                        !(tgt == owner || is_pet_of(&tgt, &owner))
                    });
                    changed |= self.timers.len() != before;
                }
                EngineEvent::Zone { name } => {
                    // Zoning is a clean slate: every live timer references a mob
                    // in the zone you just left — debuffs on those mobs, and
                    // rare-respawn countdowns for that specific zone/instance.
                    // None carry over (mobs don't follow you across a zone line;
                    // a fresh instance spawns its rares up), so drop them all.
                    // Without this a respawn timer's 30-min grace keeps a stale
                    // countdown on screen in a brand-new instance where the rare
                    // is already standing there. (Harmless at startup: the
                    // pipeline's initial Zone event arrives before any timers.)
                    self.timers.clear();
                    self.zone = name;
                    changed = true;
                }
                EngineEvent::Damage { amount } => {
                    self.dmg.push((Instant::now(), amount));
                }
                EngineEvent::Level { level } => {
                    self.level = Some(level);
                }
                EngineEvent::RareAdded { name, respawn_seconds, zone } => {
                    // Keep the Rares tab live: replace any same-named entry.
                    let lower = name.to_lowercase();
                    self.info.rares.retain(|r| r.name.to_lowercase() != lower);
                    self.info.rares.push(eq_core::config::RareConfig {
                        name,
                        respawn_seconds,
                        icon: None,
                        zone,
                        notes: Some("added in-game".to_string()),
                    });
                }
                EngineEvent::RareRemoved { name } => {
                    let lower = name.to_lowercase();
                    self.info.rares.retain(|r| r.name.to_lowercase() != lower);
                }
                EngineEvent::RareUpdated { name, respawn_seconds } => {
                    let lower = name.to_lowercase();
                    for r in &mut self.info.rares {
                        if r.name.to_lowercase() == lower {
                            r.respawn_seconds = respawn_seconds;
                        }
                    }
                }
                EngineEvent::ZoneDefaultSet { zone, respawn_seconds } => match respawn_seconds {
                    Some(s) => {
                        self.info.zone_respawn.insert(zone, s);
                    }
                    None => {
                        self.info.zone_respawn.remove(&zone);
                    }
                },
                EngineEvent::Trigger(_) => {} // no feed in the clean UI
            }
        }

        // A timer normally ends via its clear (the wear-off line). If that line
        // is ever missed, drop it a short grace past its estimate.
        let before = self.timers.len();
        self.timers.retain(|t| {
            let grace = if t.key.starts_with("respawn:") { RESPAWN_UP_GRACE } else { CLEAR_GRACE };
            t.started_at.elapsed() < t.duration + grace
        });
        changed |= self.timers.len() != before;

        // Debuffs first, most urgent (least remaining) on top; spawn timers
        // below. Only on change — the order can't drift on its own.
        if changed {
            self.timers.sort_by_key(|t| (t.key.starts_with("respawn:"), t.remaining()));
        }
    }

    /// React to tray-menu / tray-icon events (Settings…, Quit, left-click).
    fn poll_tray(&mut self, ctx: &egui::Context) {
        let Some(t) = &self.tray else { return };
        let (mut open, mut quit) = (false, false);
        if let Some(rx) = &t.menu_rx {
            while let Ok(ev) = rx.try_recv() {
                if ev.id() == &t.settings_id {
                    open = true;
                } else if ev.id() == &t.quit_id {
                    quit = true;
                }
            }
        }
        if let Some(rx) = &t.tray_rx {
            while let Ok(ev) = rx.try_recv() {
                if let TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                } = ev
                {
                    open = true;
                }
            }
        }
        if open {
            self.focus_settings = true;
        }
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The settings window (its own native viewport). It exists for the whole
    /// app lifetime so the app always has a taskbar button and an alt-tab
    /// entry — findable from inside the game. "Closing" it minimizes instead.
    ///
    /// Undecorated + transparent: the window chrome is OUR paint — a BeOS R5
    /// window with a yellow title tab (draggable; its little box minimizes)
    /// over a beveled gray panel.
    fn settings_window(&mut self, ctx: &egui::Context) {
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("eqov-settings"),
            egui::ViewportBuilder::default()
                .with_title("EQ Overlay")
                .with_inner_size([440.0, 396.0])
                .with_resizable(false)
                .with_decorations(false)
                .with_transparent(true)
                .with_taskbar(true)
                .with_icon(self.info.window_icon.clone())
                .with_always_on_top(),
            |ctx2, _class| {
                // Normal launches tuck the window away immediately (it keeps
                // its taskbar button); first runs leave it up front instead.
                if self.settings_start_minimized {
                    self.settings_start_minimized = false;
                    ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                if self.focus_settings {
                    self.focus_settings = false;
                    ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx2.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                egui::CentralPanel::default()
                    .frame(Frame::none())
                    .show(ctx2, |ui| self.beos_window(ui, ctx2));
                if ctx2.input(|i| i.viewport().close_requested()) {
                    // Keep the window (and the taskbar button) alive — X just
                    // tucks it away.
                    ctx2.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
            },
        );
    }

    /// Paint the BeOS window chrome (yellow tab + beveled panel), handle
    /// dragging and the tab's minimize box, then lay the tabbed content inside.
    fn beos_window(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let rect = ui.max_rect();
        let p = ui.painter().clone();

        const TAB_H: f32 = 27.0;
        let title_font = FontId::proportional(13.5);
        let tw = ctx.fonts(|f| {
            f.layout_no_wrap("EQ Overlay".to_owned(), title_font.clone(), INK).size().x
        });
        let box_s = 14.0;
        let tab_w = (10.0 + box_s + 9.0 + tw + 16.0).min(rect.width());
        let tab = Rect::from_min_size(rect.min, Vec2::new(tab_w, TAB_H));
        let body = Rect::from_min_max(Pos2::new(rect.min.x, rect.min.y + TAB_H - 1.0), rect.max);

        // Panel: gray fill, black border, raised bevel.
        p.rect_filled(body, Rounding::ZERO, BEOS_PANEL);
        p.line_segment(
            [body.min + Vec2::new(1.0, 1.5), Pos2::new(body.max.x - 1.0, body.min.y + 1.5)],
            Stroke::new(1.0, BEVEL_HI),
        );
        p.line_segment(
            [body.min + Vec2::new(1.5, 1.0), Pos2::new(body.min.x + 1.5, body.max.y - 1.0)],
            Stroke::new(1.0, BEVEL_HI),
        );
        p.line_segment(
            [Pos2::new(body.min.x + 1.0, body.max.y - 1.5), body.max - Vec2::new(1.0, 1.5)],
            Stroke::new(1.0, BEVEL_LO),
        );
        p.line_segment(
            [Pos2::new(body.max.x - 1.5, body.min.y + 1.0), body.max - Vec2::new(1.5, 1.0)],
            Stroke::new(1.0, BEVEL_LO),
        );
        p.rect_stroke(body, Rounding::ZERO, Stroke::new(1.0, INK));

        // Yellow title tab.
        p.rect_filled(tab, Rounding::ZERO, BEOS_YELLOW);
        p.line_segment(
            [tab.min + Vec2::new(1.0, 1.5), Pos2::new(tab.max.x - 1.0, tab.min.y + 1.5)],
            Stroke::new(1.0, BEOS_YELLOW_HI),
        );
        p.rect_stroke(tab, Rounding::ZERO, Stroke::new(1.0, INK));
        // Merge the tab into the panel (erase the shared border segment).
        p.line_segment(
            [
                Pos2::new(tab.min.x + 1.0, tab.max.y - 0.5),
                Pos2::new(tab.max.x - 1.0, tab.max.y - 0.5),
            ],
            Stroke::new(1.0, BEOS_PANEL),
        );

        // The little BeOS box on the tab: click = tuck away (minimize).
        let cb = Rect::from_min_size(
            Pos2::new(tab.min.x + 9.0, tab.center().y - box_s / 2.0),
            Vec2::new(box_s, box_s),
        );
        let cb_resp = ui.interact(cb, ui.id().with("beos-box"), egui::Sense::click());
        let cb_fill =
            if cb_resp.hovered() { Color32::from_rgb(255, 240, 170) } else { BEOS_YELLOW_HI };
        p.rect_filled(cb, Rounding::ZERO, cb_fill);
        p.line_segment(
            [cb.min + Vec2::new(1.0, 1.5), Pos2::new(cb.max.x - 1.0, cb.min.y + 1.5)],
            Stroke::new(1.0, Color32::WHITE),
        );
        p.line_segment(
            [Pos2::new(cb.min.x + 1.0, cb.max.y - 1.5), cb.max - Vec2::new(1.0, 1.5)],
            Stroke::new(1.0, Color32::from_rgb(190, 150, 0)),
        );
        p.rect_stroke(cb, Rounding::ZERO, Stroke::new(1.0, INK));
        if cb_resp.clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        p.text(
            Pos2::new(cb.max.x + 9.0, tab.center().y),
            Align2::LEFT_CENTER,
            "EQ Overlay",
            title_font,
            INK,
        );

        // Drag anywhere on the tab (except the box) to move the window.
        let drag_zone = Rect::from_min_max(Pos2::new(cb.max.x + 2.0, tab.min.y), tab.max);
        let drag = ui.interact(drag_zone, ui.id().with("beos-drag"), egui::Sense::drag());
        if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        // Content inside the panel.
        let content = Rect::from_min_max(
            body.min + Vec2::new(14.0, 12.0),
            body.max - Vec2::new(14.0, 12.0),
        );
        let mut cui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(content)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        self.settings_ui(&mut cui, ctx);
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.spacing_mut().item_spacing = egui::vec2(10.0, 6.0);
        self.beos_tabs(ui);
        ui.add_space(8.0);
        match self.tab {
            Tab::Status => self.tab_status(ui),
            Tab::Rares => self.tab_rares(ui),
            Tab::Spells => self.tab_spells(ui),
            Tab::Audio => self.tab_audio(ui),
            Tab::Setup => self.tab_setup(ui, ctx),
            Tab::About => self.tab_about(ui),
        }
    }

    /// Classic connected tab strip: the active tab is taller, panel-coloured,
    /// and the baseline breaks under it so it fuses with the page below.
    fn beos_tabs(&mut self, ui: &mut egui::Ui) {
        const STRIP_H: f32 = 24.0;
        let (strip, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), STRIP_H), egui::Sense::hover());
        let p = ui.painter().clone();
        let font = FontId::proportional(12.5);
        let mut x = strip.min.x + 2.0;
        let mut active_span = (strip.min.x, strip.min.x);

        for (t, label) in [
            (Tab::Status, "Status"),
            (Tab::Rares, "Rares"),
            (Tab::Spells, "Spells"),
            (Tab::Audio, "Audio"),
            (Tab::Setup, "Setup"),
            (Tab::About, "About"),
        ] {
            let tw = ui.ctx().fonts(|f| {
                f.layout_no_wrap(label.to_owned(), font.clone(), INK).size().x
            });
            let w = tw + 22.0;
            let active = self.tab == t;
            let top = if active { strip.min.y } else { strip.min.y + 3.0 };
            let r = Rect::from_min_max(Pos2::new(x, top), Pos2::new(x + w, strip.max.y));
            let resp = ui.interact(r, ui.id().with(("beos-tab", label)), egui::Sense::click());
            if resp.clicked() {
                self.tab = t;
            }
            let fill = if active {
                BEOS_PANEL
            } else if resp.hovered() {
                Color32::from_rgb(205, 205, 205)
            } else {
                Color32::from_rgb(196, 196, 196)
            };
            p.rect_filled(r, Rounding::ZERO, fill);
            // Top + sides; the bottom is the shared baseline (skipped if active).
            p.line_segment([r.min, Pos2::new(r.max.x, r.min.y)], Stroke::new(1.0, INK));
            p.line_segment([r.min, Pos2::new(r.min.x, r.max.y)], Stroke::new(1.0, INK));
            p.line_segment([Pos2::new(r.max.x, r.min.y), r.max], Stroke::new(1.0, INK));
            p.line_segment(
                [r.min + Vec2::new(1.0, 1.5), Pos2::new(r.max.x - 1.0, r.min.y + 1.5)],
                Stroke::new(1.0, if active { BEVEL_HI } else { Color32::from_rgb(224, 224, 224) }),
            );
            p.text(
                r.center(),
                Align2::CENTER_CENTER,
                label,
                font.clone(),
                if active { INK } else { DIM },
            );
            if active {
                active_span = (r.min.x, r.max.x);
            }
            x += w + 4.0;
        }

        // Baseline across the strip, broken under the active tab.
        let yb = strip.max.y - 0.5;
        p.line_segment(
            [Pos2::new(strip.min.x, yb), Pos2::new(active_span.0, yb)],
            Stroke::new(1.0, INK),
        );
        p.line_segment(
            [Pos2::new(active_span.1, yb), Pos2::new(strip.max.x, yb)],
            Stroke::new(1.0, INK),
        );
    }

    fn tab_status(&self, ui: &mut egui::Ui) {
        let dim = DIM;
        let warn = WARN;
        egui::Grid::new("eqov-status").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            ui.colored_label(dim, "Log");
            match &self.info.log_path {
                Some(p) => {
                    let file = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    match &self.info.char_server {
                        Some((c, s)) => ui.label(format!("{c} on {s}   ({file})")),
                        None => ui.label(file),
                    };
                }
                None => {
                    ui.colored_label(warn, "no log file found — turn logging on with /log in game");
                }
            }
            ui.end_row();

            ui.colored_label(dim, "Zone");
            ui.label(if self.zone.is_empty() { "—".to_string() } else { self.zone.clone() });
            ui.end_row();

            ui.colored_label(dim, "Level");
            ui.label(self.level.map(|l| l.to_string()).unwrap_or_else(|| "—".into()));
            ui.end_row();

            ui.colored_label(dim, "Spells");
            match self.info.spells_tracked {
                Some(n) => ui.label(format!("{n} auto-tracked from the game files")),
                None => ui.colored_label(warn, "spell files not found — check the Setup tab"),
            };
            ui.end_row();

            ui.colored_label(dim, "Rares");
            ui.label(format!("{} with respawn timers", self.info.rares_tracked));
            ui.end_row();

            ui.colored_label(dim, "Active bars");
            ui.label(self.timers.len().to_string());
            ui.end_row();
        });

        ui.add_space(6.0);
        ui.label(
            RichText::new("The overlay never captures the mouse. Reopen this window any time from the tray icon or Alt-Tab.")
                .size(10.5)
                .color(dim),
        );
    }

    fn tab_rares(&mut self, ui: &mut egui::Ui) {
        let dim = DIM;
        let gold = ACCENT;
        let can_edit = self.control_tx.is_some();

        // Live countdown per tracked rare (from the active respawn bars).
        let due: HashMap<String, Duration> = self
            .timers
            .iter()
            .filter_map(|t| {
                t.key
                    .strip_prefix("respawn:")
                    .map(|n| (n.to_string(), t.remaining()))
            })
            .collect();

        // ── Tracked rares: the one place a rare is removed ────────────────
        ui.label(RichText::new("Tracked").color(INK).strong().size(12.0));
        if self.info.rares.is_empty() {
            ui.colored_label(dim, "Nothing tracked yet — add from your recent kills below.");
        } else {
            egui::ScrollArea::vertical().id_salt("tracked").max_height(132.0).show(ui, |ui| {
                egui::Grid::new("eqov-rares")
                    .num_columns(5)
                    .spacing([14.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.colored_label(dim, RichText::new("Rare").size(11.0));
                        ui.colored_label(dim, RichText::new("Zone").size(11.0));
                        ui.colored_label(dim, RichText::new("Respawn").size(11.0));
                        ui.colored_label(dim, RichText::new("Due").size(11.0));
                        ui.label("");
                        ui.end_row();
                        let mut remove: Option<String> = None;
                        for r in &self.info.rares {
                            let name = ui.colored_label(gold, &r.name);
                            if let Some(n) = &r.notes {
                                name.on_hover_text(n);
                            }
                            ui.label(r.zone.clone().unwrap_or_else(|| "—".into()));
                            ui.label(fmt_remaining(Duration::from_secs(r.respawn_seconds)));
                            match due.get(&r.name.to_lowercase()) {
                                Some(rem) if rem.as_secs() == 0 => {
                                    ui.colored_label(Color32::from_rgb(30, 120, 40), "UP");
                                }
                                Some(rem) => {
                                    ui.label(fmt_remaining(*rem));
                                }
                                None => {
                                    ui.colored_label(dim, "—");
                                }
                            }
                            if can_edit
                                && ui
                                    .small_button("✕")
                                    .on_hover_text("stop tracking this rare")
                                    .clicked()
                            {
                                remove = Some(r.name.clone());
                            }
                            ui.end_row();
                        }
                        if let (Some(name), Some(tx)) = (remove, &self.control_tx) {
                            let _ = tx.send(eq_core::Control::RemoveRare { name });
                        }
                    });
            });
        }

        ui.add_space(4.0);
        ui.separator();

        // ── Recent kills: ONLY untracked mobs — pure add candidates ──────
        let zone_default = self
            .info
            .zone_respawn
            .get(&eq_core::normalize_zone(&self.zone))
            .copied();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Add from recent kills").color(INK).strong().size(12.0));
            ui.add_space(8.0);
            ui.colored_label(dim, "respawn");
            let hint = zone_default
                .map(|s| fmt_remaining(Duration::from_secs(s)))
                .unwrap_or_else(|| "5:00".into());
            ui.add(
                egui::TextEdit::singleline(&mut self.add_secs_edit)
                    .desired_width(52.0)
                    .hint_text(hint),
            );
            if let Some(s) = zone_default {
                ui.colored_label(
                    dim,
                    format!("zone default: {}", fmt_remaining(Duration::from_secs(s))),
                );
            }
        });
        let tracked: Vec<String> =
            self.info.rares.iter().map(|r| r.name.to_lowercase()).collect();
        let candidates: Vec<(String, Instant)> = self
            .recent_kills
            .iter()
            .filter(|(n, _)| !tracked.contains(&n.to_lowercase()))
            .cloned()
            .collect();
        if candidates.is_empty() {
            ui.colored_label(
                dim,
                if self.recent_kills.is_empty() {
                    "Nothing killed yet this session."
                } else {
                    "All recent kills are already tracked."
                },
            );
        } else {
            // None = let the pipeline apply the zone default (then 5:00).
            let secs = eq_core::parse_secs(self.add_secs_edit.trim());
            egui::ScrollArea::vertical().id_salt("kills").max_height(92.0).show(ui, |ui| {
                egui::Grid::new("eqov-kills").num_columns(3).spacing([14.0, 3.0]).show(
                    ui,
                    |ui| {
                        let mut add: Option<(String, Instant)> = None;
                        for (name, at) in &candidates {
                            ui.label(name);
                            ui.colored_label(
                                dim,
                                format!("{} ago", fmt_remaining(at.elapsed())),
                            );
                            if can_edit && ui.small_button("Add").clicked() {
                                add = Some((name.clone(), *at));
                            }
                            ui.end_row();
                        }
                        if let (Some((name, at)), Some(tx)) = (add, &self.control_tx) {
                            let _ = tx.send(eq_core::Control::AddRare {
                                name,
                                respawn_seconds: secs,
                                killed_at: Some(at),
                            });
                        }
                    },
                );
            });
        }

        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "In game: /join {ch}, then  add  /  add 4:25  /  remove  after a kill — a social \
                 macro with  /1 add  makes it one button.  zone 9:30  sets this zone's default \
                 respawn (bare adds use it). Named adds sync to the whole channel; respawns \
                 tighten automatically as you camp.",
                ch = self.info.command_channel,
            ))
            .size(10.5)
            .color(dim),
        );
    }

    fn tab_spells(&self, ui: &mut egui::Ui) {
        let dim = DIM;
        if self.spell_stats.is_empty() {
            ui.colored_label(dim, "Nothing tracked yet this session — land a spell on something.");
        } else {
            let mut rows: Vec<(&String, &(u32, u64))> = self.spell_stats.iter().collect();
            rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(b.0)));
            egui::ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                egui::Grid::new("eqov-spells")
                    .num_columns(3)
                    .spacing([18.0, 5.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.colored_label(dim, RichText::new("Spell").size(11.0));
                        ui.colored_label(dim, RichText::new("Bars").size(11.0));
                        ui.colored_label(dim, RichText::new("Duration").size(11.0));
                        ui.end_row();
                        for (name, (count, secs)) in rows {
                            ui.label(name);
                            ui.label(count.to_string());
                            ui.label(fmt_remaining(Duration::from_secs(*secs)));
                            ui.end_row();
                        }
                    });
            });
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Every detrimental spell you cast is tracked automatically from the game files. \
                 Durations start at the base rank and auto-learn upward when a spell wears off \
                 naturally — mote-ranked spells calibrate themselves after one clean wear-off.",
            )
            .size(10.5)
            .color(dim),
        );
    }

    fn tab_setup(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let dim = DIM;
        let warn = WARN;

        ui.label(RichText::new("EverQuest folder").color(INK).strong().size(12.5));
        let shown = self.pending_game_dir.clone().or_else(|| self.info.game_dir.clone());
        match &shown {
            Some(d) => {
                ui.label(d.display().to_string());
                if self.pending_game_dir.is_some() {
                    if crate::is_game_dir(shown.as_ref().unwrap()) {
                        ui.colored_label(dim, "new folder — save to apply");
                    } else {
                        ui.colored_label(warn, "spells_us.txt isn't in that folder — is it the right one?");
                    }
                } else if self.info.game_dir_autodetected {
                    ui.colored_label(dim, "auto-detected on first start");
                }
            }
            None => {
                ui.colored_label(warn, "not set — pick the game's install folder");
            }
        }

        ui.add_space(2.0);
        if ui.button("Change…").clicked() {
            let mut dlg = rfd::FileDialog::new().set_title("Pick your EverQuest Legends folder");
            if let Some(d) = shown.as_ref().and_then(|d| d.parent()) {
                dlg = dlg.set_directory(d);
            }
            if let Some(dir) = dlg.pick_folder() {
                self.pending_game_dir = Some(dir);
            }
        }

        ui.add_space(8.0);
        ui.label(RichText::new("Command channel").color(INK).strong().size(12.5));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.channel_edit).desired_width(140.0));
            ui.colored_label(
                dim,
                format!("join it in game:  /join {}", self.channel_edit.trim()),
            );
        });
        ui.colored_label(
            dim,
            "The chat channel watched for in-game commands. The default, eqov, is shared by \
             the community: a named add from ANY member (add 4:25 Baron Telyx V`Zher) goes \
             into everyone's database. Use a name of your own instead for a private list.",
        );

        ui.add_space(8.0);
        let channel_dirty = {
            let t = self.channel_edit.trim();
            !t.is_empty() && t != self.info.command_channel
        };
        let dirty = self.pending_game_dir.is_some() || channel_dirty;
        let has_dir = self.pending_game_dir.is_some() || self.info.game_dir.is_some();
        if ui.add_enabled(dirty && has_dir, egui::Button::new("Save & Restart")).clicked() {
            self.save_and_restart(ctx);
        }

        ui.add_space(6.0);
        ui.separator();
        ui.colored_label(dim, format!("Config file: {}", self.info.config_save_path.display()));
        ui.label(
            RichText::new("Everything else is automatic: spell tracking, death clears, level scaling, and the rares database.")
                .size(10.5)
                .color(dim),
        );
    }

    fn tab_audio(&mut self, ui: &mut egui::Ui) {
        let dim = DIM;
        let mut changed = false;

        ui.label(RichText::new("Sounds").color(INK).strong().size(12.5));
        if ui.checkbox(&mut self.audio_enabled, "Play sounds").changed() {
            changed = true;
        }
        ui.colored_label(dim, "Covers the rare-spawn chime and any custom trigger sounds.");

        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new("Spawn chime").color(INK).strong().size(12.5));
        ui.add_enabled_ui(self.audio_enabled, |ui| {
            let is_wav = self.spawn_sound.to_ascii_lowercase().ends_with(".wav");
            for (id, label) in [
                ("default", "Chime — two-tone ding-dong"),
                ("asterisk", "Ding — single bright ping"),
                ("exclamation", "Alert — triple beep"),
                ("critical", "Urgent — low warble"),
            ] {
                if ui.radio(!is_wav && self.spawn_sound == id, label).clicked() {
                    self.spawn_sound = id.to_string();
                    play_spawn_alert(&self.spawn_sound); // instant preview
                    changed = true;
                }
            }
            ui.horizontal(|ui| {
                if ui.radio(is_wav, "Custom .wav").clicked() && !is_wav {
                    // Radio alone does nothing until a file is picked.
                }
                if ui.button("Choose…").clicked() {
                    if let Some(f) = rfd::FileDialog::new()
                        .set_title("Pick a .wav for the spawn chime")
                        .add_filter("wav", &["wav"])
                        .pick_file()
                    {
                        self.spawn_sound = f.display().to_string();
                        play_spawn_alert(&self.spawn_sound);
                        changed = true;
                    }
                }
                if ui.button("Test").clicked() {
                    play_spawn_alert(&self.spawn_sound);
                }
            });
            if is_wav {
                ui.colored_label(dim, &self.spawn_sound);
            }
        });

        ui.add_space(6.0);
        ui.colored_label(dim, "Changes apply immediately and are saved automatically.");

        if changed {
            self.persist_config();
        }
    }

    /// Rewrite the config with the CURRENT saved values plus live audio
    /// settings — used by the Audio tab's auto-save (no restart involved).
    fn persist_config(&self) {
        let Some(gd) = self.pending_game_dir.as_ref().or(self.info.game_dir.as_ref()) else {
            return; // first-run without a game dir: nothing sensible to write yet
        };
        let (x, y, w, h) = self.info.overlay;
        if let Err(e) = crate::write_config_file(
            &self.info.config_save_path,
            gd,
            x,
            y,
            w,
            h,
            self.level.unwrap_or(1),
            &self.info.command_channel,
            self.audio_enabled,
            &self.spawn_sound,
        ) {
            log::error!("failed to write {}: {e}", self.info.config_save_path.display());
        }
    }

    fn tab_about(&self, ui: &mut egui::Ui) {
        let dim = DIM;
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // The app icon, drawn big: yellow tab with a clock face.
            let (logo, _) = ui.allocate_exact_size(Vec2::new(52.0, 52.0), egui::Sense::hover());
            let p = ui.painter();
            p.rect_filled(logo, Rounding::ZERO, BEOS_YELLOW);
            p.line_segment(
                [logo.min + Vec2::new(1.0, 1.5), Pos2::new(logo.max.x - 1.0, logo.min.y + 1.5)],
                Stroke::new(1.0, BEOS_YELLOW_HI),
            );
            p.rect_stroke(logo, Rounding::ZERO, Stroke::new(1.0, INK));
            let c = logo.center();
            let r = 16.0;
            p.circle_filled(c, r, Color32::from_rgb(16, 18, 22));
            p.circle_stroke(c, r, Stroke::new(1.5, INK));
            let hand = Color32::from_rgb(247, 242, 205);
            p.line_segment([c, c + Vec2::new(0.0, -(r - 4.0))], Stroke::new(2.0, hand));
            p.line_segment([c, c + Vec2::new(r * 0.45, 0.0)], Stroke::new(2.0, hand));
            p.circle_filled(c, 1.6, hand);

            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(RichText::new("EQ Overlay").color(INK).strong().size(20.0));
                ui.colored_label(dim, format!("version {}", env!("CARGO_PKG_VERSION")));
            });
        });

        ui.add_space(10.0);
        ui.separator();
        ui.label("Log-driven spell and rare-respawn timers for EverQuest Legends.");
        ui.add_space(4.0);
        egui::Grid::new("eqov-about").num_columns(2).spacing([16.0, 5.0]).show(ui, |ui| {
            ui.colored_label(dim, "How it works");
            ui.label("reads the game's files on disk (log + spell data) — never memory, packets, or input");
            ui.end_row();
            ui.colored_label(dim, "Built with");
            ui.label("Rust, egui + wgpu");
            ui.end_row();
            ui.colored_label(dim, "License");
            ui.label("MIT — free to use, share, and modify");
            ui.end_row();
            ui.colored_label(dim, "Interface");
            ui.label("lovingly borrowed from BeOS R5");
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Spell data, durations, and icons come from the game's own files. \
                 Rare respawns live in rares.toml — share it with your friends.",
            )
            .size(10.5)
            .color(dim),
        );
    }

    /// Write the config with the current Setup choices, spawn a fresh instance
    /// of the app (it re-reads everything), and close this one.
    fn save_and_restart(&self, ctx: &egui::Context) {
        let Some(gd) = self.pending_game_dir.as_ref().or(self.info.game_dir.as_ref()) else {
            return;
        };
        let (x, y, w, h) = self.info.overlay;
        let lvl = self.level.unwrap_or(1);
        let chan = {
            let t = self.channel_edit.trim();
            if t.is_empty() { "eqov" } else { t }
        };
        if let Err(e) = crate::write_config_file(
            &self.info.config_save_path,
            gd,
            x,
            y,
            w,
            h,
            lvl,
            chan,
            self.audio_enabled,
            &self.spawn_sound,
        ) {
            log::error!("failed to write {}: {e}", self.info.config_save_path.display());
            return;
        }
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).spawn();
        }
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
    }
}

impl eframe::App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0] // transparent window
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            self.styled = true;
            apply_beos_style(ctx);
        }
        self.poll_tray(ctx);
        self.ingest();
        self.settings_window(ctx);

        // Keep the overlay above EQ: re-assert always-on-top every second. A
        // focused/relaunched game window can otherwise climb above us in the
        // topmost band and hide the bars (the engine keeps working, unseen).
        if self.topmost_at.elapsed() >= Duration::from_secs(1) {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
            self.topmost_at = Instant::now();
        }

        // The instant a rare's respawn timer pops, chime once. It then lingers on
        // "UP" for RESPAWN_UP_GRACE (60s) before the retain() below drops it.
        for t in &mut self.timers {
            if t.key.starts_with("respawn:") && !t.alerted && t.remaining().as_secs_f32() <= 0.5 {
                t.alerted = true;
                t.up_at = Some(Instant::now());
                if self.audio_enabled {
                    play_spawn_alert(&self.spawn_sound);
                }
            }
        }

        // Roll the DPS window forward and compute the current figure. It decays
        // to 0 a few seconds after combat stops (no new damage events).
        self.dmg.retain(|(t, _)| t.elapsed() < DPS_WINDOW);
        let dps = if self.dmg.is_empty() {
            0
        } else {
            (self.dmg.iter().map(|(_, a)| *a).sum::<u64>() as f32 / DPS_WINDOW.as_secs_f32()).round()
                as u64
        };

        // Pre-fetch icon textures (needs &mut self.icons) before the draw
        // closure borrows self immutably.
        let mut icon_texs: HashMap<u32, TextureHandle> = HashMap::new();
        let idxs: Vec<u32> = self.timers.iter().filter_map(|t| t.icon).collect();
        for idx in idxs {
            if let std::collections::hash_map::Entry::Vacant(e) = icon_texs.entry(idx) {
                if let Some(t) = self.icons.texture(ctx, idx) {
                    e.insert(t);
                }
            }
        }

        egui::CentralPanel::default()
            .frame(Frame::none())
            .show(ctx, |ui| {
                let painter = ui.painter();
                let area = ui.max_rect();

                let pad = 6.0;
                let x = area.min.x + pad;
                let w = (area.width() - 2.0 * pad).min(300.0);
                let mut y = area.min.y + pad;

                // ── BeOS-style yellow window tab: zone (left) + live DPS (right) ──
                let title = if self.zone.is_empty() { "eq-overlay" } else { self.zone.as_str() };
                let tfont = FontId::proportional(11.5);
                let tw = ctx.fonts(|f| f.layout_no_wrap(title.to_owned(), tfont.clone(), Color32::BLACK).size().x);
                let dps_txt = (dps > 0).then(|| format!("{} dps", fmt_dps(dps)));
                let dfont = FontId::monospace(10.5);
                let dw = dps_txt
                    .as_ref()
                    .map(|t| ctx.fonts(|f| f.layout_no_wrap(t.clone(), dfont.clone(), Color32::BLACK).size().x))
                    .unwrap_or(0.0);
                let tab_h = 17.0;
                let inner = tw + if dps_txt.is_some() { 14.0 + dw } else { 0.0 };
                let tab_w = (inner + 16.0).clamp(58.0, w);
                let tab = Rect::from_min_size(Pos2::new(x, y), Vec2::new(tab_w, tab_h));
                let tab_round = Rounding { nw: 3.0, ne: 3.0, sw: 0.0, se: 0.0 };

                painter.rect_filled(tab, tab_round, Color32::from_rgba_unmultiplied(246, 208, 58, 240));
                // Bevel: bright top edge, shadowed bottom edge (classic BeOS tab).
                painter.line_segment(
                    [Pos2::new(tab.min.x + 2.0, tab.min.y + 0.5), Pos2::new(tab.max.x - 2.0, tab.min.y + 0.5)],
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 244, 176, 240)),
                );
                painter.line_segment(
                    [Pos2::new(tab.min.x + 1.0, tab.max.y - 0.5), Pos2::new(tab.max.x - 1.0, tab.max.y - 0.5)],
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(176, 138, 22, 240)),
                );
                painter.rect_stroke(tab, tab_round, Stroke::new(1.0, Color32::from_rgba_unmultiplied(48, 38, 6, 240)));
                painter.text(
                    Pos2::new(tab.min.x + 8.0, tab.center().y),
                    Align2::LEFT_CENTER,
                    title,
                    tfont,
                    Color32::from_rgb(30, 24, 4),
                );
                if let Some(t) = &dps_txt {
                    // Dark red reads clearly on the yellow tab.
                    painter.text(
                        Pos2::new(tab.max.x - 8.0, tab.center().y),
                        Align2::RIGHT_CENTER,
                        t,
                        dfont,
                        Color32::from_rgb(122, 22, 10),
                    );
                }
                // Active-timer count: dim, just past the tab's right edge.
                if !self.timers.is_empty() {
                    painter.text(
                        Pos2::new(tab.max.x + 7.0, tab.center().y),
                        Align2::LEFT_CENTER,
                        self.timers.len().to_string(),
                        FontId::proportional(10.0),
                        Color32::from_white_alpha(130),
                    );
                }
                y += tab_h + 4.0;

                let slot_h = 26.0;
                let gap = 3.0;
                // Extra vertical space between the spell section (top) and the
                // spawn-timer section (bottom). The sort already groups them.
                let section_gap = 9.0;
                let isz = 20.0;
                let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));

                let mut prev_respawn = None;
                for t in &self.timers {
                    let is_respawn = t.key.starts_with("respawn:");
                    if prev_respawn == Some(false) && is_respawn {
                        y += section_gap;
                    }
                    prev_respawn = Some(is_respawn);

                    let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, slot_h));
                    let (name, tag) = split_label(&t.label);
                    let cat = category(tag);
                    let up = is_respawn && t.remaining().as_secs_f32() <= 0.5;
                    // Fresh pop: blink for a few seconds so it catches the eye.
                    let flash = up
                        && t.up_at.is_some_and(|at| {
                            let e = at.elapsed().as_secs_f32();
                            e < RESPAWN_FLASH_SECS
                                && ((e * RESPAWN_FLASH_HZ * 2.0) as i32) % 2 == 0
                        });
                    let frac = if up { 1.0 } else { t.fraction() };
                    let expiring = !is_respawn && t.remaining().as_secs_f32() <= 5.0;
                    let round = Rounding::same(3.0);

                    // Slot background.
                    painter.rect_filled(rect, round, Color32::from_rgba_unmultiplied(8, 10, 14, 228));

                    // Depleting fill: category colour, or red in the last 5s.
                    let fill_w = ((w - 2.0) * frac).max(0.0);
                    if fill_w > 0.0 {
                        let fill = Rect::from_min_size(rect.min + Vec2::new(1.0, 1.0), Vec2::new(fill_w, slot_h - 2.0));
                        let c = if flash {
                            Color32::from_rgb(150, 255, 160) // pop flash = bright green
                        } else if up {
                            Color32::from_rgb(70, 190, 90) // UP = green
                        } else if is_respawn {
                            Color32::from_rgb(212, 166, 46) // respawning = gold
                        } else if expiring {
                            Color32::from_rgb(200, 66, 58)
                        } else {
                            cat.color
                        };
                        let alpha = if flash { 225 } else { 160 };
                        painter.rect_filled(fill, round, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha));
                    }

                    // Icon: real game gem if we have it, else a colour-coded square.
                    let ir = Rect::from_min_size(
                        Pos2::new(rect.min.x + 3.0, rect.center().y - isz / 2.0),
                        Vec2::new(isz, isz),
                    );
                    match t.icon.and_then(|i| icon_texs.get(&i)) {
                        Some(tex) => {
                            painter.image(tex.id(), ir, uv, Color32::WHITE);
                            painter.rect_stroke(ir, Rounding::same(2.0), Stroke::new(1.0, Color32::from_black_alpha(120)));
                        }
                        None if is_respawn => {
                            // Spawn timers get a little analog clock: the hand
                            // starts at 12 on the kill, sweeps once around the
                            // dial over the respawn, and lands back on 12 as
                            // the rare pops (rim turns green at UP).
                            let c = ir.center();
                            let r = isz / 2.0 - 1.0;
                            let rim = if flash {
                                Color32::from_rgb(170, 255, 180)
                            } else if up {
                                Color32::from_rgb(70, 190, 90)
                            } else {
                                Color32::from_rgb(212, 166, 46)
                            };
                            painter.circle_filled(c, r, Color32::from_rgba_unmultiplied(10, 12, 16, 235));
                            painter.circle_stroke(c, r, Stroke::new(1.3, rim));
                            for k in 0..4 {
                                let a = k as f32 * std::f32::consts::FRAC_PI_2;
                                let dir = Vec2::new(a.cos(), a.sin());
                                painter.line_segment(
                                    [c + dir * (r - 1.2), c + dir * (r - 3.0)],
                                    Stroke::new(1.0, Color32::from_white_alpha(110)),
                                );
                            }
                            let swept = if up { 0.0 } else { (1.0 - t.fraction()) * std::f32::consts::TAU };
                            let hand_color = Color32::from_rgb(247, 242, 205);
                            let m = swept - std::f32::consts::FRAC_PI_2;
                            painter.line_segment(
                                [c, c + Vec2::new(m.cos(), m.sin()) * (r - 2.5)],
                                Stroke::new(1.6, hand_color),
                            );
                            let h = swept / 12.0 - std::f32::consts::FRAC_PI_2;
                            painter.line_segment(
                                [c, c + Vec2::new(h.cos(), h.sin()) * (r * 0.45)],
                                Stroke::new(1.6, hand_color),
                            );
                            painter.circle_filled(c, 1.2, hand_color);
                        }
                        None => {
                            painter.rect_filled(ir, Rounding::same(2.0), cat.color);
                            painter.rect_stroke(ir, Rounding::same(2.0), Stroke::new(1.0, Color32::from_black_alpha(150)));
                            painter.text(ir.center(), Align2::CENTER_CENTER, cat.abbr, FontId::proportional(10.0), Color32::from_white_alpha(240));
                        }
                    }

                    // Countdown on the right; measure it so text can't overrun it.
                    // Spawn timers read "UP" (green) once they hit zero.
                    let rem = if up { "UP".to_string() } else { fmt_remaining(t.remaining()) };
                    let rem_font = FontId::monospace(13.0);
                    let rem_w = ctx.fonts(|f| f.layout_no_wrap(rem.clone(), rem_font.clone(), Color32::WHITE).size().x);
                    painter.text(
                        Pos2::new(rect.max.x - 6.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        &rem,
                        rem_font,
                        if flash {
                            Color32::WHITE
                        } else if up {
                            Color32::from_rgb(150, 250, 160)
                        } else {
                            Color32::from_rgb(247, 242, 205)
                        },
                    );

                    // "xN" badge when several same-named mobs share this bar (the
                    // log carries no per-mob id, so e.g. two "ice boned skeleton"
                    // collapse to one keyed bar; N = how many still have the effect).
                    let count_txt = (t.count > 1).then(|| format!("x{}", t.count));
                    let count_font = FontId::proportional(11.0);
                    let count_w = count_txt
                        .as_ref()
                        .map(|s| ctx.fonts(|f| f.layout_no_wrap(s.clone(), count_font.clone(), Color32::WHITE).size().x))
                        .unwrap_or(0.0);
                    if let Some(s) = &count_txt {
                        painter.text(
                            Pos2::new(rect.max.x - 6.0 - rem_w - 8.0, rect.center().y),
                            Align2::RIGHT_CENTER,
                            s,
                            count_font,
                            Color32::from_rgb(255, 226, 138),
                        );
                    }

                    // Monster name + spell name, clipped to the gap left of the timer.
                    let tl = ir.max.x + 6.0;
                    let count_gap = if count_txt.is_some() { count_w + 8.0 } else { 0.0 };
                    let tr = (rect.max.x - 6.0 - rem_w - 8.0 - count_gap).max(tl);
                    let cp = painter.with_clip_rect(Rect::from_min_max(
                        Pos2::new(tl, rect.min.y),
                        Pos2::new(tr, rect.max.y),
                    ));
                    let name_font = FontId::proportional(13.0);
                    cp.text(
                        Pos2::new(tl, rect.center().y),
                        Align2::LEFT_CENTER,
                        name,
                        name_font.clone(),
                        Color32::from_rgb(236, 236, 240),
                    );
                    if !t.spell.is_empty() {
                        let name_w =
                            ctx.fonts(|f| f.layout_no_wrap(name.to_owned(), name_font, Color32::WHITE).size().x);
                        cp.text(
                            Pos2::new(tl + name_w + 6.0, rect.center().y + 0.5),
                            Align2::LEFT_CENTER,
                            format!("({})", t.spell),
                            FontId::proportional(10.5),
                            Color32::from_white_alpha(160),
                        );
                    }

                    // BeOS-ish raised bevel: bright top edge, shadowed bottom edge.
                    painter.line_segment(
                        [Pos2::new(rect.min.x + 1.0, rect.min.y + 0.5), Pos2::new(rect.max.x - 1.0, rect.min.y + 0.5)],
                        Stroke::new(1.0, Color32::from_white_alpha(38)),
                    );
                    painter.line_segment(
                        [Pos2::new(rect.min.x + 1.0, rect.max.y - 0.5), Pos2::new(rect.max.x - 1.0, rect.max.y - 0.5)],
                        Stroke::new(1.0, Color32::from_black_alpha(90)),
                    );
                    painter.rect_stroke(rect, round, Stroke::new(1.0, Color32::from_black_alpha(60)));
                    y += slot_h + gap;
                }

            });

        // Animate at 20 fps only while something on screen is moving; idle
        // drops to a slow keepalive tick (topmost re-assert) instead of
        // presenting frames next to a 4K game all night. 20 fps is visually
        // lossless here: countdown text ticks once a second and the fill edge
        // moves ~a pixel per frame (anti-aliased) — while every EVENT (new bar,
        // clear, damage) repaints instantly via the forwarder in main.rs.
        let animating = !self.timers.is_empty() || !self.dmg.is_empty();
        ctx.request_repaint_after(Duration::from_millis(if animating { 50 } else { 500 }));
    }
}

/// Colour + short tag for an effect, keyed off the label's `[Tag]`.
struct Cat {
    color: Color32,
    abbr: &'static str,
}

fn category(tag: &str) -> Cat {
    let (r, g, b, abbr) = match tag {
        "Mez" => (150, 92, 220, "Mz"),
        "Slow" => (70, 120, 220, "Sl"),
        "Snare" => (60, 165, 172, "Sn"),
        "Root" => (162, 112, 58, "Rt"),
        "DoT" => (95, 172, 82, "Dt"),
        "Blind" => (206, 188, 74, "Bl"),
        "Calm" => (92, 166, 212, "Ca"),
        "Numb" => (128, 122, 168, "Nb"),
        "Choke" => (72, 142, 96, "Ck"),
        "Debuff" => (192, 88, 82, "Db"),
        "Spawn" => (212, 166, 46, "R"), // rare respawn timer
        _ => (132, 132, 148, "•"),
    };
    Cat { color: Color32::from_rgb(r, g, b), abbr }
}

/// Split "orc centurion [Slow]" into ("orc centurion", "Slow").
fn split_label(label: &str) -> (&str, &str) {
    match label.rfind(" [") {
        Some(i) => (&label[..i], label[i + 2..].trim_end_matches(']')),
        None => (label, ""),
    }
}

/// True if `candidate` is `owner`'s pet. EQ pet names embed the owner's, and a
/// pet dies with no log line when its owner is killed — so an owner's death must
/// also clear its pet's bars. Exact-length match, so it can't over-clear a
/// different mob that merely starts with the same words.
fn is_pet_of(candidate: &str, owner: &str) -> bool {
    const SUFFIXES: [&str; 9] = [
        " pet", "`s pet", "'s pet", " warder", "`s warder", " familiar", "`s familiar",
        " ward", "`s ward",
    ];
    SUFFIXES.iter().any(|s| {
        candidate.len() == owner.len() + s.len()
            && candidate.starts_with(owner)
            && &candidate[owner.len()..] == *s
    })
}

#[cfg(test)]
mod tests {
    use super::is_pet_of;

    #[test]
    fn matches_owner_pets_but_not_lookalikes() {
        assert!(is_pet_of("the thaumaturgist pet", "the thaumaturgist"));
        assert!(is_pet_of("fippy`s pet", "fippy"));
        assert!(is_pet_of("jarel`s warder", "jarel"));
        // Not a pet: a different, longer-named mob.
        assert!(!is_pet_of("the thaumaturgist warlord", "the thaumaturgist"));
        assert!(!is_pet_of("a skeleton knight", "a skeleton"));
        assert!(!is_pet_of("the thaumaturgist", "the thaumaturgist"));
    }
}

/// A short, non-blocking alert the instant a rare's respawn timer pops.
///
/// The built-in sounds are SYNTHESIZED (tiny in-memory WAVs, generated once) —
/// not MessageBeep system sounds, because Windows 11's default scheme maps
/// Default/Asterisk/Exclamation to nearly identical sounds. `winmm!PlaySoundW`
/// plays them async from memory; a `.wav` path plays from disk. No audio
/// device is held open and no asset files are shipped.
#[cfg(windows)]
fn play_spawn_alert(kind: &str) {
    #[link(name = "winmm")]
    extern "system" {
        fn PlaySoundW(psz_sound: *const u16, hmod: isize, fdw_sound: u32) -> i32;
    }
    const SND_ASYNC: u32 = 0x0001;
    const SND_NODEFAULT: u32 = 0x0002;
    const SND_MEMORY: u32 = 0x0004;
    const SND_FILENAME: u32 = 0x0002_0000;

    if kind.to_ascii_lowercase().ends_with(".wav") {
        let wide: Vec<u16> = kind.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            PlaySoundW(wide.as_ptr(), 0, SND_FILENAME | SND_ASYNC | SND_NODEFAULT);
        }
        return;
    }
    let data = builtin_sound(kind);
    unsafe {
        PlaySoundW(data.as_ptr() as *const u16, 0, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

#[cfg(not(windows))]
fn play_spawn_alert(_kind: &str) {}

/// The four built-in chimes, synthesized once and cached for the app's
/// lifetime (the buffer must outlive the async playback).
fn builtin_sound(kind: &str) -> &'static [u8] {
    use std::sync::OnceLock;
    static CHIME: OnceLock<Vec<u8>> = OnceLock::new();
    static DING: OnceLock<Vec<u8>> = OnceLock::new();
    static ALERT: OnceLock<Vec<u8>> = OnceLock::new();
    static URGENT: OnceLock<Vec<u8>> = OnceLock::new();
    match kind {
        // Single bright ping.
        "asterisk" => DING.get_or_init(|| synth_wav(&[(1318.5, 380)], 0)),
        // Insistent triple beep.
        "exclamation" => ALERT.get_or_init(|| synth_wav(&[(988.0, 110), (988.0, 110), (988.0, 170)], 70)),
        // Low two-pitch warble — hard to miss.
        "critical" => URGENT.get_or_init(|| {
            synth_wav(&[(392.0, 150), (311.1, 150), (392.0, 150), (311.1, 240)], 15)
        }),
        // Default: a pleasant two-tone ding-dong.
        _ => CHIME.get_or_init(|| synth_wav(&[(880.0, 220), (659.3, 340)], 25)),
    }
    .as_slice()
}

/// Render a sequence of (frequency Hz, duration ms) sine notes — each with an
/// exponential decay so they ring like a bell rather than buzz — into a 16-bit
/// mono 22.05 kHz WAV in memory.
fn synth_wav(notes: &[(f32, u32)], gap_ms: u32) -> Vec<u8> {
    const SR: u32 = 22_050;
    let mut samples: Vec<i16> = Vec::new();
    for (i, (freq, ms)) in notes.iter().enumerate() {
        if i > 0 {
            samples.extend(std::iter::repeat(0).take((SR * gap_ms / 1000) as usize));
        }
        let n = (SR * ms / 1000) as usize;
        let dur = *ms as f32 / 1000.0;
        for t in 0..n {
            let time = t as f32 / SR as f32;
            let attack = (time / 0.004).min(1.0); // 4ms ramp-in kills the click
            let decay = (-5.0 * time / dur).exp();
            let v = (time * freq * std::f32::consts::TAU).sin() * attack * decay * 0.6;
            samples.push((v * i16::MAX as f32) as i16);
        }
    }
    samples.extend(std::iter::repeat(0).take((SR / 40) as usize)); // tail pad

    let data_len = samples.len() * 2;
    let mut w = Vec::with_capacity(44 + data_len);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&SR.to_le_bytes());
    w.extend_from_slice(&(SR * 2).to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// "342", "1.2k" — compact DPS for the title tab.
fn fmt_dps(dps: u64) -> String {
    if dps >= 1000 {
        format!("{:.1}k", dps as f32 / 1000.0)
    } else {
        dps.to_string()
    }
}

/// "9s", "1:23", "15:00".
fn fmt_remaining(d: Duration) -> String {
    let secs = d.as_secs_f32().ceil() as u64;
    if secs >= 60 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}
