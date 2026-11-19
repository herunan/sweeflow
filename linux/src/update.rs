//! Updates. Each release has `latest.json` (linux/release/build.sh): its version, what's new, and
//! where each app gets it. Sweeflow looks a little after it starts and every few hours, and the
//! tray panel and a notification say when there's a newer one.
//!
//! How it's then updated depends on how it was installed: from Sweeflow's own repo (pacman, apt,
//! dnf), with the system's updates; from install.sh (~/.local/bin), by `sweeflow update`, which
//! downloads the release, checks it against the release's signed SHA256SUMS and puts it in place.
//!
//! The Mac says which version it has when it connects (WELCOME), so the panel can say when the
//! two differ.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;

/// This copy's version and build (0: a development build, which only a higher version updates).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub fn build() -> u32 {
    env!("SWEEFLOW_BUILD").parse().unwrap_or(0)
}

/// The release signing key's public half (sweeflow.asc), as gpgv reads it.
const RELEASE_KEY: &[u8] = include_bytes!("../assets/release-key.gpg");

const FEED: &str = "https://github.com/herunan/sweeflow/releases/latest/download/latest.json";
/// How often to look, after the first look a little after start.
const EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Feed {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub page: String,
    /// When it was released ("2026-11-19").
    #[serde(default)]
    pub date: String,
    /// The Mac app's (Updater.swift reads it).
    #[allow(dead_code)]
    pub mac: Option<Part>,
    pub linux: Option<Part>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Part {
    pub version: String,
    #[serde(default)]
    pub build: u32,
    pub url: String,
    pub sha256: String,
}

/// A newer release than this copy, once seen.
static AVAILABLE: Mutex<Option<Feed>> = Mutex::new(None);
/// The latest release, as last looked at (also when it isn't newer).
static LATEST: Mutex<Option<Feed>> = Mutex::new(None);
/// The version the Mac said it has, while connected.
static MAC_VERSION: Mutex<Option<String>> = Mutex::new(None);

fn feed_url() -> String {
    std::env::var("SWEEFLOW_UPDATE_FEED").unwrap_or_else(|_| FEED.into())
}

/// Whether `theirs` (version, build) is newer than `ours`. A build of 0 is a development build:
/// only a higher version counts as newer then.
pub fn newer(theirs: (&str, u32), ours: (&str, u32)) -> bool {
    let nums = |v: &str| -> Vec<u64> { v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (nums(theirs.0), nums(ours.0));
    match a.cmp(&b) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => ours.1 != 0 && theirs.1 > ours.1,
    }
}

/// Looks now and again, on a thread of its own.
pub fn start() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        loop {
            if let Err(e) = check() {
                log::info!("update check: {e:#}");
            }
            std::thread::sleep(EVERY);
        }
    });
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    let out = Command::new("curl").args(["-fsSL", "--max-time", "60", url]).output().context("curl")?;
    ensure!(out.status.success(), "{url}: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

/// The latest release, if it's newer than this copy. Nothing said or shown (the update window).
pub fn check_quiet() -> Result<Option<Feed>> {
    let body = fetch(&feed_url()).map_err(|e| {
        log::info!("update check: {e:#}");
        anyhow::anyhow!("Sweeflow couldn't reach github.com. Check you're online, then try again.")
    })?;
    let feed: Feed = serde_json::from_slice(&body).context("the release's latest.json doesn't read")?;
    *LATEST.lock().unwrap() = Some(feed.clone());
    let new = feed.linux.as_ref().is_some_and(|l| newer((&l.version, l.build), (VERSION, build())));
    *AVAILABLE.lock().unwrap() = new.then(|| feed.clone());
    Ok(new.then_some(feed))
}

/// A newer release's version, once seen.
pub fn available() -> Option<String> {
    AVAILABLE.lock().unwrap().as_ref().map(|f| f.version.clone())
}

/// The latest release, as last looked at.
pub fn latest() -> Option<Feed> {
    LATEST.lock().unwrap().clone()
}

/// Looks at the latest release: a newer one is noted, and said once (a notification).
pub fn check() -> Result<Option<Feed>> {
    let found = check_quiet();
    crate::tray::refresh();
    let Some(feed) = found? else {
        return Ok(None);
    };
    log::info!("Sweeflow {} is out (this is {VERSION})", feed.version);
    // Said once per version, however often it's looked at or Sweeflow restarts.
    let said = crate::config::dir().join("update-notified");
    if std::fs::read_to_string(&said).ok().as_deref() != Some(feed.version.as_str()) {
        let _ = std::fs::write(&said, &feed.version);
        let how = match how() {
            How::Tarball => "Open Sweeflow to update it.".to_string(),
            other => other.words(),
        };
        let _ = Command::new("notify-send")
            .args(["--app-name=Sweeflow", "--icon=sweeflow", &format!("Sweeflow {} is available", feed.version), &how])
            .status();
    }
    Ok(Some(feed))
}

/// How this copy is updated.
#[derive(Debug, Clone, PartialEq)]
pub enum How {
    /// install.sh's copy in ~/.local/bin: `sweeflow update`.
    Tarball,
    /// From Sweeflow's repo, with the system's updates: the command for it.
    System(&'static str),
    /// A package installed on its own (no repo): a new one from the release.
    Download,
}

impl How {
    pub fn words(&self) -> String {
        match self {
            How::Tarball => "Update it from here.".into(),
            How::System(cmd) => format!("It comes with your system's updates ({cmd})."),
            How::Download => "Get the new package from the release page.".into(),
        }
    }
    pub fn id(&self) -> &'static str {
        match self {
            How::Tarball => "self",
            How::System(_) => "system",
            How::Download => "download",
        }
    }
}

pub fn how() -> How {
    let exe = std::env::current_exe().unwrap_or_default();
    if !exe.starts_with("/usr/") {
        return How::Tarball;
    }
    let has = |p: &str| Path::new(p).exists();
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let pacman = std::fs::read_dir("/var/lib/pacman/local").into_iter().flatten().flatten().any(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        n.starts_with("sweeflow-") && n[9..].starts_with(|c: char| c.is_ascii_digit())
    });
    if pacman {
        let repo = read("/etc/pacman.conf").lines().any(|l| l.trim() == "[sweeflow]");
        return match (repo, has("/usr/bin/omarchy-update")) {
            (true, true) => How::System("omarchy-update"),
            (true, false) => How::System("sudo pacman -Syu"),
            (false, _) => How::Download,
        };
    }
    if has("/var/lib/dpkg/info/sweeflow.list") {
        let repo = std::fs::read_dir("/etc/apt/sources.list.d").into_iter().flatten().flatten().any(|e| {
            e.file_name().to_string_lossy().starts_with("sweeflow")
        });
        return if repo { How::System("sudo apt upgrade") } else { How::Download };
    }
    if Command::new("rpm").args(["-q", "sweeflow"]).output().is_ok_and(|o| o.status.success()) {
        return if has("/etc/yum.repos.d/sweeflow.repo") { How::System("sudo dnf upgrade") } else { How::Download };
    }
    How::Download
}

/// What the Mac said its version is (WELCOME), or nothing (an older Mac app, or disconnected).
pub fn set_mac_version(v: Option<String>) {
    let mut m = MAC_VERSION.lock().unwrap();
    if *m != v {
        *m = v;
        drop(m);
        crate::tray::refresh();
    }
}

/// For the tray panel and the Plasma widget: this copy's version, a newer release if there is one
/// (and how it's updated), and the Mac's version when it differs from this one's.
pub fn panel_json() -> serde_json::Value {
    let available = AVAILABLE.lock().unwrap().clone();
    let mac = MAC_VERSION.lock().unwrap().clone();
    let update = available.map(|f| {
        let how = how();
        serde_json::json!({ "version": f.version, "notes": f.notes, "page": f.page, "how": how.id(), "words": how.words() })
    });
    // The Mac on another version: which is behind.
    let mac_note = mac.filter(|v| v != VERSION).map(|v| {
        if newer((&v, 0), (VERSION, 0)) {
            format!("Your Mac has Sweeflow {v}, newer than this computer's {VERSION}.")
        } else {
            format!("Your Mac has Sweeflow {v}. Update it from Sweeflow's menu on the Mac.")
        }
    });
    serde_json::json!({ "version": VERSION, "update": update, "macNote": mac_note })
}

/// `sweeflow update`: puts the latest release in place of install.sh's copy, checked against the
/// release's signed checksums, and restarts Sweeflow. A packaged copy says how it's updated.
pub fn run() -> Result<()> {
    let Some(feed) = check()? else {
        println!("Sweeflow {VERSION} is the latest version.");
        return Ok(());
    };
    let how = how();
    if how != How::Tarball {
        println!("Sweeflow {} is available. {}", feed.version, how.words());
        if how == How::Download && !feed.page.is_empty() {
            println!("    {}", feed.page);
        }
        return Ok(());
    }
    install_tarball(&feed, &|words| println!("==> {words}"))
}

/// Puts the release in place of install.sh's copy, checked against the release's signed
/// checksums, and restarts Sweeflow. `step` is told each step, in words.
pub fn install_tarball(feed: &Feed, step: &dyn Fn(&str)) -> Result<()> {
    let part = feed.linux.clone().context("the release has nothing for Linux")?;
    let dir = tempdir()?;
    let base = part.url.rsplit_once('/').map(|(b, _)| b.to_string()).context("download address")?;
    step(&format!("Downloading Sweeflow {}…", part.version));
    let sums = fetch(&format!("{base}/SHA256SUMS"))?;
    let sig = fetch(&format!("{base}/SHA256SUMS.sig"))?;
    std::fs::write(dir.join("SHA256SUMS"), &sums)?;
    std::fs::write(dir.join("SHA256SUMS.sig"), &sig)?;
    std::fs::write(dir.join("key.gpg"), RELEASE_KEY)?;
    // Signed by the release key, or not at all.
    let ok = Command::new("gpgv")
        .arg("--keyring")
        .arg(dir.join("key.gpg"))
        .arg(dir.join("SHA256SUMS.sig"))
        .arg(dir.join("SHA256SUMS"))
        .output()
        .context("gpgv (GnuPG) checks the release's signature")?;
    ensure!(ok.status.success(), "the release's checksums aren't signed by Sweeflow's key: not installed");
    let name = part.url.rsplit('/').next().unwrap_or_default().to_string();
    let tarball = fetch(&part.url)?;
    step("Checking the download against the release's signature…");
    let sum = sha256(&tarball)?;
    let listed = String::from_utf8_lossy(&sums).lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == [sum.as_str(), name.as_str()]);
    ensure!(listed && sum == part.sha256, "{name} isn't the file the release lists: not installed");
    std::fs::write(dir.join(&name), &tarball)?;
    let st = Command::new("tar").arg("-xzf").arg(dir.join(&name)).arg("-C").arg(&dir).status()?;
    ensure!(st.success(), "couldn't unpack {name}");
    let new = dir.join(format!("sweeflow-{}", part.version)).join("sweeflow");
    ensure!(new.exists(), "{name} has no sweeflow in it");
    step("Installing…");
    // In place in one step: the running copy goes on running until it restarts.
    let target = std::env::current_exe()?;
    let staged = target.with_extension("new");
    std::fs::copy(&new, &staged)?;
    std::fs::set_permissions(&staged, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    std::fs::rename(&staged, &target)?;
    let _ = std::fs::remove_dir_all(&dir);
    log::info!("Sweeflow {} installed to {}", part.version, target.display());
    step("Restarting Sweeflow…");
    // Its setup again (launcher, widget, keys), as the new copy has it; it restarts the service.
    let st = Command::new(&target).arg("install").status()?;
    if !st.success() {
        bail!("installed, but its setup didn't finish: run `sweeflow install`");
    }
    Ok(())
}

fn sha256(data: &[u8]) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("sha256sum").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn()?;
    child.stdin.take().context("sha256sum")?.write_all(data)?;
    let out = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or_default().to_string())
}

fn tempdir() -> Result<PathBuf> {
    let d = std::env::temp_dir().join(format!("sweeflow-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_releases_are_told_apart() {
        assert!(newer(("0.2.0", 1), ("0.1.0", 500)));
        assert!(!newer(("0.1.0", 500), ("0.2.0", 1)));
        assert!(newer(("0.1.10", 0), ("0.1.9", 0)));
        // The same version, built again: newer by its build number.
        assert!(newer(("0.1.0", 380), ("0.1.0", 372)));
        assert!(!newer(("0.1.0", 372), ("0.1.0", 372)));
        // A development build (0) of the same version isn't nagged.
        assert!(!newer(("0.1.0", 380), ("0.1.0", 0)));
    }

    #[test]
    fn the_feed_reads() {
        let f: Feed = serde_json::from_str(r#"{"version":"0.2.0","notes":"- Faster","page":"p","mac":null,
            "linux":{"version":"0.2.0","build":400,"url":"https://x/sweeflow-0.2.0-x86_64.tar.gz","sha256":"ab"}}"#).unwrap();
        assert_eq!(f.linux.unwrap().build, 400);
        assert!(f.mac.is_none());
    }
}
