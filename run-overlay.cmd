@echo off
rem Launch the EQ overlay. It is a tray app now (no console): look for the
rem yellow clock icon in the system tray - Settings and Quit live there.
rem Working dir matters: the config + rares.toml are found next to this script.
cd /d "%~dp0"
start "" "C:\rust-build\eqov2\debug\eq-overlay-gui.exe"
