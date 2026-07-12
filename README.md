# EQ Overlay

A transparent, click-through timer overlay for **EverQuest Legends** (and other
EQEmu-family servers). It tails your log file and draws what an enchanter
actually needs on top of the game:

- **Spell bars for every detrimental spell you cast** — zero trigger setup.
  The app reads the game's own spell data, so the land message, level-scaled
  duration, and real spell-gem icon all come along for free. Mote-ranked
  spells ("Mesmerization III") resolve automatically.
- **Rare respawn timers** with a shared, community-editable database,
  one-button in-game adds, and respawn times that calibrate themselves as you
  camp.
- **Live DPS** and the current zone in a BeOS-style title tab.

It reads the log file only — no memory reading, no injection, no automation.
Same class of tool as GINA or nparse. (Check your server's third-party-tool
policy if you're unsure.)

## Contents

- [Quick start](#quick-start)
- [The overlay](#the-overlay)
- [Rares: the shared database](#rares-the-shared-database)
- [Settings window](#settings-window)
- [Configuration](#configuration)
- [Building from source](#building-from-source)
  - [Workspace layout](#workspace-layout)
  - [Try it without EverQuest](#try-it-without-everquest)
- [Troubleshooting](#troubleshooting)
- [License](#license)

## Quick start

1. Grab the latest release zip and unpack it anywhere (keep the files
   together).
2. Run `eq-overlay-gui.exe`.
   - Windows SmartScreen will warn about an unknown publisher the first time:
     **More info → Run anyway**. The app is unsigned; the source is right here
     if you'd rather build it yourself.
3. First run auto-detects your EverQuest Legends install. If it can't find it,
   the settings window opens — pick the game folder (the one containing
   `spells_us.txt`).
4. In game, make sure logging is on: `/log on`.

That's it. The app lives in the **system tray** (yellow clock icon). There is
no console window — status lives in the settings window, reachable from the
tray icon, the taskbar, or **Alt-Tab** ("EQ Overlay").

## The overlay

- **Spell bars**: `[icon] target (spell) [countdown]`, color-coded by effect
  (mez, slow, root, calm, DoT, …). Identically-named mobs share one bar with
  an `x2` badge that counts down as each breaks. Re-casting refreshes the bar
  (the spurious "worn off" EQ logs on a refresh is understood), while a real
  break — "`<mob> has been awakened by <attacker>`" — clears it instantly.
- **Spawn bars**: killing a tracked rare (by *anyone* in the zone) starts a
  gold countdown with a small analog clock — the hand sweeps one revolution
  and lands on 12 as the rare comes due. It chimes once, shows a green **UP**
  for a minute, then tidies itself away.
- Zoning (or entering a fresh instance) clears every bar — a new instance
  spawns its rares up, so stale timers never lie to you.
- The overlay never captures the mouse. Position/size live in the config.

## Rares: the shared database

Rare respawns live in **`rares.toml`** next to the app — a plain, shareable
file. Three ways to manage it:

**In game** (the fun way). Join the command channel once:

```
/autojoin eqov
```

Then, right after killing a named:

| you type | effect |
|---|---|
| `add` | track the last mob you killed (5:00 default) |
| `add 4:25` | same, with the respawn time |
| `add 4:25 Baron Telyx V`Zher` | track by name |
| `remove` | undo the most recent add |
| `remove <name>` | untrack by name |

Tip: put `/1 add` in a **social macro** on your hotbar — one button, no
typing.

Because `eqov` is a normal chat channel, **everyone in it shares adds**: an
`add`/`remove` that includes the mob's name updates the database of every
channel member, live. Prefer privacy? Set your own channel name in Settings →
Setup.

**In the app**: the Rares tab lists everything tracked (hover a name for
notes, ✕ to remove) plus your recent kills with one-click **Add** — backdated
to the actual kill, so the countdown is right even if you add it late.

**By hand**: it's TOML. Edit, share, merge — duplicates collapse safely on
load, and re-adds update in place.

Respawn times **calibrate themselves**: while you camp a rare, the app watches
kill-to-kill gaps and tightens the timer toward the observed cycle. It only
ever shrinks, and gaps that span a zone or instance change never count.

## Settings window

| tab | what's there |
|---|---|
| **Status** | log file + character/server, zone, level, spells tracked, active bars |
| **Rares** | the respawn database + recent-kills quick add |
| **Spells** | everything you've landed this session, with live durations |
| **Setup** | game folder, command channel, config location |

## Configuration

`config.toml` is generated on first run and rewritten by the settings window;
you rarely need to touch it. The interesting keys:

```toml
[general]
log_dir = 'C:\...\EverQuest Legends\Logs'   # newest eqlog_*.txt is followed
icon_dir = 'C:\...\uifiles\default'          # SpellsNN.tga icon sheets
player_level = 24                            # fallback; auto-updates from dings
command_channel = "eqov"                     # in-game command channel
# log_path = '...'                           # pin one specific log instead
# rare_db = 'rares.toml'                     # shareable respawn DB

[overlay]
x = 20
y = 95
width = 340
height = 480
```

See `config.example.toml` for the fully annotated version, including custom
`[[triggers]]` (regex → sound alerts / fixed timers) if you want extras.

## Building from source

Windows only — the transparent click-through window requires the wgpu /
DirectComposition backend (OpenGL cannot composite it; it renders invisible).

Prereqs: [rustup](https://rustup.rs) plus the Microsoft C++ Build Tools
(`winget install Microsoft.VisualStudio.2022.BuildTools`).

```powershell
cargo build --release -p eq-overlay-gui
```

Run the exe with the repo root as the working directory (that's where
`config.example.toml` and `rares.toml` are found), or ship it in a folder with
those files. If you build under OneDrive, point builds elsewhere first:
`setx CARGO_TARGET_DIR C:\rust-build\eq-overlay` (new shell after).

### Workspace layout

```
crates/
├─ eq-core/         # engine: tailer, parser, spell DB, triggers, pipeline
├─ eq-cli/          # `eqoverlay` — tail/gen CLI for testing without the game
└─ eq-overlay-gui/  # the tray app + overlay + settings window
```

### Try it without EverQuest

```powershell
# Terminal A - fake log lines:
cargo run -p eq-cli -- gen --log test.log --interval-ms 1200

# Terminal B - tail and match:
cargo run -p eq-cli -- tail --config config.example.toml --log test.log --no-audio
```

`cargo test` runs the full suite (parser, tailer, spell DB, calibration, and
the in-game command grammar).

## Troubleshooting

- **No bars?** Check the Status tab: right character's log? Logging on
  (`/log on`)? Run EQ in **borderless windowed** — nothing can draw over
  exclusive fullscreen.
- **A mez bar flashed and vanished** — the mez broke or didn't stick; the log
  said so and the overlay agreed. Believe the overlay.
- **New spell rank seems short?** Rank durations are learned from the first
  clean, unbroken wear-off; until then the bar errs short (the safe direction
  for crowd control).
- **Smart App Control** (rare): SAC-enforced Windows refuses unsigned exes
  entirely — build from source in that case.

## License

MIT — see [LICENSE](LICENSE).
