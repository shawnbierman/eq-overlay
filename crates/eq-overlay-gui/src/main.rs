//! `eq-overlay-gui` — a transparent, click-through, always-on-top window that
//! draws the engine's active timers on top of EverQuest.
//!
//! There is NO console window (`windows_subsystem = "windows"`): the app lives
//! in the system tray. The tray menu opens a small settings window showing
//! status (log / char / zone / level / spells / rares) and lets the user pick
//! the EverQuest folder if auto-detection got it wrong.
//!
//! First run: if no config file exists, the EQ install is auto-detected and a
//! `config.toml` is written next to the exe (or the config that was found).

#![windows_subsystem = "windows"]

mod app;
mod updater;

use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;

use app::{OverlayApp, StartupInfo};
use eq_core::{char_server_from_log, spawn_pipeline_with_control, Config, PipelineOptions};

#[derive(Parser)]
#[command(name = "eq-overlay-gui", about = "Transparent click-through EQ timer overlay")]
struct Cli {
    /// Path to the config TOML. Default: config.toml / config.example.toml in
    /// the working directory, then config.toml next to the exe; auto-generated
    /// on first run if none exists.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Path to the EQ log file. Overrides `general.log_path` in the config.
    #[arg(short, long)]
    log: Option<PathBuf>,
    /// Process the whole file first, instead of only newly-appended lines.
    #[arg(long)]
    from_beginning: bool,
    /// Don't open the audio device.
    #[arg(long)]
    no_audio: bool,
}

fn main() -> Result<()> {
    // No console, but keep the logger: RUST_LOG + a debugger/DebugView can
    // still see eframe/wgpu failures.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,wgpu=error,wgpu_hal=error,wgpu_core=error,naga=error"),
    )
    .init();

    let cli = Cli::parse();
    // A previous self-update leaves the old binary renamed aside; tidy it.
    updater::cleanup_old_binary();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));

    // ── Find (or first-run create) the config ─────────────────────────────
    let mut config_path = cli.config.clone().filter(|p| p.exists()).or_else(|| {
        [PathBuf::from("config.toml"), PathBuf::from("config.example.toml")]
            .into_iter()
            .find(|p| p.exists())
            .or_else(|| exe_dir.as_ref().map(|d| d.join("config.toml")).filter(|p| p.exists()))
    });
    let mut game_dir_autodetected = false;
    if config_path.is_none() {
        if let Some(game) = detect_game_dir() {
            let target = exe_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("config.toml");
            let defaults = SavedSettings {
                game: &game,
                overlay: (20, 95, 340, 480),
                player_level: 1,
                // Empty → the pipeline defaults the command channel to your
                // character name (a private channel you join alone).
                command_channel: "",
                audio_enabled: true,
                spawn_sound: "default",
                auto_update: true,
            };
            if write_config_file(&target, &defaults).is_ok() {
                config_path = Some(target);
                game_dir_autodetected = true;
            }
        }
    }

    let cfg = match &config_path {
        Some(p) => Config::load(p).ok(),
        None => None,
    };

    // Where the settings window saves to: never clobber the shareable example —
    // a sibling config.toml takes precedence on the next launch instead.
    let config_save_path = match &config_path {
        Some(p) if p.file_name().map(|n| n == "config.example.toml").unwrap_or(false) => {
            p.with_file_name("config.toml")
        }
        Some(p) => p.clone(),
        None => exe_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("config.toml"),
    };

    // ── Start the pipeline if we have a config + a log ────────────────────
    let (tx, rx) = channel();
    let mut control_tx: Option<std::sync::mpsc::Sender<eq_core::Control>> = None;
    let mut ov = (20i32, 95i32, 340u32, 480u32);
    let mut icon_dir: Option<PathBuf> = None;
    let mut icon_sheet = "Spells".to_string();
    let mut info = StartupInfo {
        config_save_path,
        game_dir: None,
        game_dir_autodetected,
        log_path: None,
        char_server: None,
        spells_tracked: None,
        rares_tracked: 0,
        pipeline_running: false,
        overlay: (20, 95, 340, 480),
        window_icon: std::sync::Arc::new(eframe::egui::IconData {
            rgba: app_icon_rgba(),
            width: 32,
            height: 32,
        }),
        rares: Vec::new(),
        command_channel: String::new(), // set to the character name once the log is known
        audio_enabled: true,
        spawn_sound: "default".to_string(),
        zone_respawn: Default::default(),
        auto_update: true,
    };

    if let Some(cfg) = cfg {
        ov = (cfg.overlay.x, cfg.overlay.y, cfg.overlay.width, cfg.overlay.height);
        icon_dir = cfg.general.icon_dir.clone().map(PathBuf::from);
        icon_sheet = cfg.general.icon_sheet.clone().unwrap_or_else(|| "Spells".to_string());
        info.rares = cfg.rares.clone();
        if let Some(ch) = &cfg.general.command_channel {
            info.command_channel = ch.clone();
        }
        info.audio_enabled = cfg.audio.enabled.unwrap_or(true);
        if let Some(s) = &cfg.audio.spawn_sound {
            info.spawn_sound = s.clone();
        }
        info.zone_respawn = cfg.zone_respawn.clone();
        info.auto_update = cfg.updates.auto_check.unwrap_or(true);
        info.game_dir = icon_dir
            .as_ref()
            .and_then(|d| d.parent())
            .and_then(|d| d.parent())
            .map(Path::to_path_buf);

        if let Some(log_path) = cfg.resolve_log_path(cli.log.clone()) {
            info.char_server = char_server_from_log(&log_path);
            // With no channel pinned in the config, it defaults to your
            // character name (matching the pipeline) — show that in Settings.
            let channel_unset =
                cfg.general.command_channel.as_deref().map(str::trim).unwrap_or("").is_empty();
            if channel_unset {
                if let Some((ch, _)) = &info.char_server {
                    info.command_channel = ch.clone();
                }
            }
            info.log_path = Some(log_path.clone());
            let opts = PipelineOptions {
                from_beginning: cli.from_beginning,
                enable_audio: !cli.no_audio && info.audio_enabled,
                ..Default::default()
            };
            let (ctl_tx, ctl_rx) = channel();
            match spawn_pipeline_with_control(cfg, log_path, opts, ctl_rx, tx) {
                Ok(handle) => {
                    info.spells_tracked = handle.spells_tracked;
                    info.rares_tracked = handle.rares_tracked;
                    info.pipeline_running = true;
                    control_tx = Some(ctl_tx);
                }
                Err(e) => log::error!("pipeline failed to start: {e:#}"),
            }
        }
    }

    info.overlay = ov;

    // ── Tray icon: the app's control surface (Settings… / Quit) ───────────
    // Created on the main thread before the event loop starts; winit's loop
    // pumps its messages. Kept alive by moving it into the app.
    let mut tray = build_tray();

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("eq-overlay")
            .with_position([ov.0 as f32, ov.1 as f32])
            .with_inner_size([ov.2 as f32, ov.3 as f32])
            .with_decorations(false)      // borderless
            .with_transparent(true)       // alpha-blended window
            .with_always_on_top()         // sits over the game
            .with_mouse_passthrough(true) // clicks go through to EQ
            .with_taskbar(false),         // tray is the app's presence
        ..Default::default()
    };

    let result = eframe::run_native(
        "eq-overlay",
        native_options,
        Box::new(move |cc| {
            // Events arrive on an mpsc channel egui knows nothing about; this
            // forwarder wakes the UI the instant one lands, letting the app
            // idle at ~2 fps with zero added latency.
            let ctx = cc.egui_ctx.clone();
            let (fwd_tx, fwd_rx) = channel();
            std::thread::spawn(move || {
                for ev in rx {
                    if fwd_tx.send(ev).is_err() {
                        break; // app gone; dropping rx tells the pipeline to stop
                    }
                    ctx.request_repaint();
                }
            });

            // Tray events likewise: route them through handlers that wake the
            // UI, so a tray click reacts instantly even while idling at 2 fps.
            if let Some(t) = tray.as_mut() {
                let (mtx, mrx) = channel();
                let c = cc.egui_ctx.clone();
                tray_icon::menu::MenuEvent::set_event_handler(Some(move |ev| {
                    let _ = mtx.send(ev);
                    c.request_repaint();
                }));
                t.menu_rx = Some(mrx);

                let (ttx, trx) = channel();
                let c = cc.egui_ctx.clone();
                tray_icon::TrayIconEvent::set_event_handler(Some(move |ev| {
                    let _ = ttx.send(ev);
                    c.request_repaint();
                }));
                t.tray_rx = Some(trx);
            }

            Ok(Box::new(OverlayApp::new(fwd_rx, icon_dir, icon_sheet, info, tray, control_tx))
                as Box<dyn eframe::App>)
        }),
    );
    result.map_err(|e| anyhow::anyhow!("overlay window failed: {e}"))
}

/// Locate the EverQuest Legends install: known default paths first, then a
/// scan of the Daybreak "Installed Games" folder. A dir qualifies if it holds
/// the client spell file (the Logs folder may not exist until /log is on).
fn detect_game_dir() -> Option<PathBuf> {
    const CANDIDATES: [&str; 4] = [
        r"C:\Users\Public\Daybreak Game Company\Installed Games\EverQuest Legends",
        r"C:\Users\Public\Daybreak Game Company\Installed Games\EverQuest",
        r"C:\Program Files (x86)\Daybreak Game Company\EverQuest",
        r"C:\Program Files\Daybreak Game Company\EverQuest",
    ];
    for c in CANDIDATES {
        let p = Path::new(c);
        if is_game_dir(p) {
            return Some(p.to_path_buf());
        }
    }
    let root = Path::new(r"C:\Users\Public\Daybreak Game Company\Installed Games");
    if let Ok(rd) = root.read_dir() {
        for e in rd.flatten() {
            let p = e.path();
            if is_game_dir(&p) {
                return Some(p);
            }
        }
    }
    None
}

pub(crate) fn is_game_dir(p: &Path) -> bool {
    p.join("spells_us.txt").exists()
}

/// Write a fresh config pointing at `game` — used on first run and by the
/// settings window. Everything else (spell tracking, death clears, rares.toml
/// discovery) is automatic, so this is the whole file.
/// Everything the settings window persists, in one place.
pub(crate) struct SavedSettings<'a> {
    pub game: &'a Path,
    pub overlay: (i32, i32, u32, u32),
    pub player_level: u32,
    pub command_channel: &'a str,
    pub audio_enabled: bool,
    pub spawn_sound: &'a str,
    pub auto_update: bool,
}

pub(crate) fn write_config_file(path: &Path, s: &SavedSettings) -> std::io::Result<()> {
    let g = s.game.display();
    let (x, y, width, height) = s.overlay;
    let content = format!(
        "# EQ overlay config (generated; the Settings window rewrites this file).\n\
         # Spell tracking, death clears, and rares.toml discovery are automatic.\n\
         \n\
         [general]\n\
         log_dir = '{g}\\Logs'\n\
         icon_dir = '{g}\\uifiles\\default'\n\
         icon_sheet = \"Spells\"\n\
         player_level = {level}\n\
         {channel_line}\
         [audio]\n\
         enabled = {audio}\n\
         spawn_sound = '{sound}'\n\
         \n\
         [updates]\n\
         auto_check = {auto}\n\
         \n\
         [overlay]\n\
         x = {x}\n\
         y = {y}\n\
         width = {width}\n\
         height = {height}\n",
        level = s.player_level,
        // Omit the line entirely when empty, so the pipeline derives the channel
        // from the character name instead of pinning a stale value.
        channel_line = if s.command_channel.trim().is_empty() {
            "\n".to_string()
        } else {
            format!("command_channel = \"{}\"\n\n", s.command_channel)
        },
        audio = s.audio_enabled,
        sound = s.spawn_sound,
        auto = s.auto_update,
    );
    std::fs::write(path, content)
}

/// Build the tray icon + menu. Returns None (and the app runs tray-less) only
/// if the shell refuses the icon — nothing else depends on it.
fn build_tray() -> Option<app::Tray> {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};

    let menu = Menu::new();
    let settings = MenuItem::new("Settings…", true, None);
    let quit = MenuItem::new("Quit EQ Overlay", true, None);
    menu.append(&settings).ok()?;
    menu.append(&PredefinedMenuItem::separator()).ok()?;
    menu.append(&quit).ok()?;

    let icon = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("EQ Overlay")
        .with_icon(tray_icon_art())
        .build()
        .ok()?;

    Some(app::Tray {
        _icon: icon,
        settings_id: settings.id().clone(),
        quit_id: quit.id().clone(),
        menu_rx: None,
        tray_rx: None,
    })
}

/// 32x32 tray art, drawn in code (no asset file): the overlay's BeOS-yellow
/// tab with a small dark clock face.
fn tray_icon_art() -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(app_icon_rgba(), 32, 32).expect("valid rgba icon")
}

/// The same art as raw RGBA — also used as the settings window's taskbar /
/// alt-tab icon.
pub(crate) fn app_icon_rgba() -> Vec<u8> {
    const S: i32 = 32;
    let mut px = vec![0u8; (S * S * 4) as usize];
    let set = |px: &mut Vec<u8>, x: i32, y: i32, c: [u8; 4]| {
        if (0..S).contains(&x) && (0..S).contains(&y) {
            let i = ((y * S + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&c);
        }
    };
    const YELLOW: [u8; 4] = [246, 208, 58, 255];
    const DARK: [u8; 4] = [48, 38, 6, 255];
    const FACE: [u8; 4] = [16, 18, 22, 255];
    const HAND: [u8; 4] = [247, 242, 205, 255];

    for y in 2..S - 2 {
        for x in 2..S - 2 {
            let edge = x == 2 || y == 2 || x == S - 3 || y == S - 3;
            set(&mut px, x, y, if edge { DARK } else { YELLOW });
        }
    }
    // Clock face: filled disc r=9 centered, with a 12-o'clock and 3-o'clock hand.
    let (cx, cy, r) = (S / 2, S / 2, 9);
    for y in 0..S {
        for x in 0..S {
            let (dx, dy) = (x - cx, y - cy);
            let d2 = dx * dx + dy * dy;
            if d2 <= r * r {
                set(&mut px, x, y, FACE);
            }
            if d2 <= r * r && d2 >= (r - 1) * (r - 1) {
                set(&mut px, x, y, DARK);
            }
        }
    }
    for k in 1..=6 {
        set(&mut px, cx, cy - k, HAND); // minute hand: up
    }
    for k in 1..=4 {
        set(&mut px, cx + k, cy, HAND); // hour hand: right
    }
    set(&mut px, cx, cy, HAND);

    px
}
