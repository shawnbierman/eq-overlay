//! Self-update against GitHub Releases.
//!
//! Check: one unauthenticated API call comparing the latest release tag to the
//! built-in version. Install (explicit click only): download the release zip,
//! unpack it, RENAME the running exe aside (Windows allows renaming a running
//! binary — just not overwriting it), copy the new exe into place, relaunch.
//! Config / rares.toml are separate files and are never touched.

use std::io::Read;
use std::sync::mpsc::Sender;

pub const REPO: &str = "shawnbierman/eq-overlay";

#[derive(Debug)]
pub enum UpdateMsg {
    UpToDate,
    Available { version: String, url: String },
    Status(String),
    Failed(String),
    /// New exe is in place and already launched — close this instance.
    RestartReady,
}

fn parse_ver(v: &str) -> Option<(u32, u32, u32)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let mut it = v.split('.');
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next().unwrap_or("0").parse().ok()?,
    ))
}

fn current_ver() -> (u32, u32, u32) {
    parse_ver(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0))
}

pub fn spawn_check(tx: Sender<UpdateMsg>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let msg = match check() {
            Ok(Some((version, url))) => UpdateMsg::Available { version, url },
            Ok(None) => UpdateMsg::UpToDate,
            Err(e) => UpdateMsg::Failed(format!("check failed: {e}")),
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
    });
}

fn check() -> Result<Option<(String, String)>, Box<dyn std::error::Error>> {
    let resp = ureq::get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .set("User-Agent", "eq-overlay")
        .timeout(std::time::Duration::from_secs(15))
        .call()?;
    let json: serde_json::Value = resp.into_json()?;
    let tag = json["tag_name"].as_str().unwrap_or_default().to_string();
    let latest = parse_ver(&tag).ok_or("unparseable release tag")?;
    if latest <= current_ver() {
        return Ok(None);
    }
    let url = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|a| {
            let name = a["name"].as_str()?;
            if name.ends_with(".zip") {
                a["browser_download_url"].as_str().map(str::to_string)
            } else {
                None
            }
        })
        .ok_or("release has no zip asset")?;
    Ok(Some((tag, url)))
}

pub fn spawn_install(url: String, tx: Sender<UpdateMsg>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let send = |m: UpdateMsg| {
            let _ = tx.send(m);
            ctx.request_repaint();
        };
        if let Err(e) = install(&url, &send) {
            send(UpdateMsg::Failed(format!("update failed: {e}")));
        }
    });
}

fn install(url: &str, send: &dyn Fn(UpdateMsg)) -> Result<(), Box<dyn std::error::Error>> {
    send(UpdateMsg::Status("downloading…".into()));
    let resp = ureq::get(url)
        .set("User-Agent", "eq-overlay")
        .timeout(std::time::Duration::from_secs(180))
        .call()?;
    let mut buf = Vec::new();
    resp.into_reader().take(200 * 1024 * 1024).read_to_end(&mut buf)?;

    let work = std::env::temp_dir().join("eq-overlay-update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let zip = work.join("update.zip");
    std::fs::write(&zip, &buf)?;

    send(UpdateMsg::Status("unpacking…".into()));
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-Command"]).arg(format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        zip.display(),
        work.display()
    ));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let status = cmd.status()?;
    if !status.success() {
        return Err("unzip failed".into());
    }
    let new_exe = work.join("eq-overlay-gui.exe");
    if !new_exe.exists() {
        return Err("new exe missing from the zip".into());
    }

    send(UpdateMsg::Status("installing…".into()));
    let cur = std::env::current_exe()?;
    let old = cur.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    // The running exe can be renamed out of the way, then replaced.
    std::fs::rename(&cur, &old)?;
    if let Err(e) = std::fs::copy(&new_exe, &cur) {
        let _ = std::fs::rename(&old, &cur); // roll back
        return Err(e.into());
    }
    std::process::Command::new(&cur).current_dir(std::env::current_dir()?).spawn()?;
    send(UpdateMsg::RestartReady);
    Ok(())
}

/// Delete the leftover `.old` binary from a previous update, if any.
pub fn cleanup_old_binary() {
    if let Ok(cur) = std::env::current_exe() {
        let _ = std::fs::remove_file(cur.with_extension("exe.old"));
    }
}

#[cfg(test)]
mod tests {
    use super::parse_ver;

    #[test]
    fn version_parsing_and_ordering() {
        assert_eq!(parse_ver("v0.1.3"), Some((0, 1, 3)));
        assert_eq!(parse_ver("0.2.0"), Some((0, 2, 0)));
        assert!(parse_ver("v0.2.0") > parse_ver("v0.1.9"));
        assert!(parse_ver("v1.0.0") > parse_ver("v0.9.9"));
        assert_eq!(parse_ver("junk"), None);
    }
}
