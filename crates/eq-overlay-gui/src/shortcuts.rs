//! Windows shortcut helpers: a Desktop launcher and a "start with Windows"
//! entry. Both are `.lnk` files created via PowerShell's `WScript.Shell` COM
//! object — the same no-extra-crate shell-out style as the updater. The
//! start-with-Windows toggle is just a shortcut in the user's Startup folder,
//! so its on/off state is a plain file-exists check (no registry access).

use std::path::{Path, PathBuf};

/// Single-quote a string for PowerShell (double any embedded quote).
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn run_powershell(script: &str) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash
    }
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "PowerShell reported a failure creating the shortcut",
        )),
        Err(e) => Err(e),
    }
}

/// Create a `.lnk` at the location given by the PowerShell path expression
/// `lnk_expr`, pointing at the running exe (with its folder as the working
/// directory so config discovery still works) and using the exe's own icon.
fn create_shortcut(lnk_expr: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let exe_s = exe.display().to_string();
    let dir_s = exe.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({lnk}); \
         $s.TargetPath={exe}; $s.WorkingDirectory={dir}; \
         $s.IconLocation={icon}; $s.Description='EQ Overlay'; $s.Save()",
        lnk = lnk_expr,
        exe = ps_quote(&exe_s),
        dir = ps_quote(&dir_s),
        icon = ps_quote(&format!("{exe_s},0")),
    );
    run_powershell(&script)
}

/// Drop an "EQ Overlay.lnk" on the Desktop. PowerShell resolves the Desktop
/// folder via `GetFolderPath`, so a OneDrive-redirected Desktop works too.
pub fn create_desktop_shortcut() -> std::io::Result<()> {
    create_shortcut("(Join-Path ([Environment]::GetFolderPath('Desktop')) 'EQ Overlay.lnk')")
}

/// The startup-folder shortcut path. `%APPDATA%` isn't redirected by OneDrive,
/// so this fixed relative path is reliable for creating and existence-checking.
fn startup_lnk() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        Path::new(&a)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("EQ Overlay.lnk")
    })
}

/// Is the app set to launch on login? (Does its Startup shortcut exist?)
pub fn run_at_login_enabled() -> bool {
    startup_lnk().map(|p| p.exists()).unwrap_or(false)
}

/// Turn "start with Windows" on (create the Startup shortcut) or off (delete
/// it). Off is a plain file removal; on reuses the shared `.lnk` builder.
pub fn set_run_at_login(enable: bool) -> std::io::Result<()> {
    let path = startup_lnk().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no APPDATA to place the startup shortcut")
    })?;
    if enable {
        create_shortcut(&ps_quote(&path.display().to_string()))
    } else if path.exists() {
        std::fs::remove_file(&path)
    } else {
        Ok(())
    }
}
