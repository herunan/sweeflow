//! Launcher entries for the Mac's apps, so they show up in KDE's app menu / KRunner and the
//! taskbar can tell each Mac app apart (it matches windows to entries by StartupWMClass).

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
}

/// The Mac display window's entry (see setup::write_launcher), kept when Mac apps' go.
pub const DISPLAY_ENTRY: &str = "sweeflow-display.desktop";

/// Where a Mac app's icon is saved (as PNG).
pub fn icon_path(bundle_id: &str) -> PathBuf {
    data_dir().join(format!("sweeflow/icons/{bundle_id}.png"))
}

/// Keeps ~/.local/share/applications/sweeflow-*.desktop in sync with the Mac's installed apps.
pub fn sync(apps: &[(String, String, Vec<u8>)]) -> Result<()> {
    let apps_dir = data_dir().join("applications");
    let icon_dir = data_dir().join("sweeflow/icons");
    fs::create_dir_all(&apps_dir)?;
    fs::create_dir_all(&icon_dir)?;
    write_menu()?;
    let exe = std::env::current_exe()?;
    let mut keep = std::collections::HashSet::from([DISPLAY_ENTRY.to_string()]);
    for (bid, name, png) in apps {
        // Bundle ids are reverse-DNS, safe for file names; guard anyway.
        if bid.is_empty() || bid.contains('/') {
            continue;
        }
        let icon = icon_path(bid);
        if !png.is_empty() && fs::read(&icon).ok().as_deref() != Some(png.as_slice()) {
            fs::write(&icon, png)?;
        }
        let file = format!("sweeflow-{bid}.desktop");
        // No Comment/GenericName: macOS has no description for its apps, and a made-up one
        // would be the same on every entry.
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName={name} (Mac)\nExec={} launch {bid}\nIcon={}\n\
             StartupWMClass=sweeflow-{bid}\nCategories=X-Sweeflow;\nKeywords=mac;macos;\nTerminal=false\n{}",
            exe.display(),
            icon.display(),
            if VISIBLE.load(Ordering::SeqCst) { "" } else { "NoDisplay=true\n" }
        );
        let path = apps_dir.join(&file);
        if fs::read_to_string(&path).ok().as_deref() != Some(entry.as_str()) {
            fs::write(&path, entry)?;
        }
        keep.insert(file);
    }
    // Apps uninstalled on the Mac.
    for e in fs::read_dir(&apps_dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("sweeflow-") && name.ends_with(".desktop") && !keep.contains(&name) {
            let _ = fs::remove_file(e.path());
        }
    }
    log::info!("launcher entries for {} Mac apps", keep.len());
    refresh_menu();
    Ok(())
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
}

/// A "Mac Apps" folder in the app menu (like Chromium's "Chromium Apps"), holding every entry
/// in the X-Sweeflow category.
fn write_menu() -> Result<()> {
    let dirs = data_dir().join("desktop-directories");
    fs::create_dir_all(&dirs)?;
    write_if_changed(
        &dirs.join("sweeflow.directory"),
        "[Desktop Entry]\nVersion=1.0\nType=Directory\nName=Mac Apps\nIcon=computer-laptop\n",
    )?;
    let merged = config_dir().join("menus/applications-merged");
    fs::create_dir_all(&merged)?;
    write_if_changed(
        &merged.join("sweeflow.menu"),
        "<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\"\n    \
         \"http://www.freedesktop.org/standards/menu-spec/menu-1.0.dtd\">\n\
         <!-- Written by sweeflow -->\n<Menu>\n    <Name>Applications</Name>\n    <Menu>\n        \
         <Name>sweeflow</Name>\n        <Directory>sweeflow.directory</Directory>\n        \
         <Include>\n            <Category>X-Sweeflow</Category>\n        </Include>\n    </Menu>\n</Menu>\n",
    )
}

fn write_if_changed(path: &std::path::Path, content: &str) -> Result<()> {
    if fs::read_to_string(path).ok().as_deref() != Some(content) {
        fs::write(path, content)?;
    }
    Ok(())
}

/// Whether the entries are shown (see `set_visible`), for entries written later.
static VISIBLE: AtomicBool = AtomicBool::new(true);

/// Shows or hides every Mac app entry (hidden while the Mac isn't connected, or switched off).
pub fn set_visible(visible: bool) {
    VISIBLE.store(visible, Ordering::SeqCst);
    let apps_dir = data_dir().join("applications");
    let Ok(entries) = fs::read_dir(&apps_dir) else { return };
    let mut changed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("sweeflow-") && name.ends_with(".desktop")) {
            continue;
        }
        let Ok(text) = fs::read_to_string(e.path()) else { continue };
        let without: String = text.lines().filter(|l| !l.starts_with("NoDisplay=")).map(|l| format!("{l}\n")).collect();
        let new = if visible { without } else { format!("{without}NoDisplay=true\n") };
        if new != text && fs::write(e.path(), new).is_ok() {
            changed += 1;
        }
    }
    if changed > 0 {
        log::info!("Mac app entries {}", if visible { "shown" } else { "hidden" });
        refresh_menu();
    }
}

/// Tells KDE to pick up the changes now rather than whenever it notices.
fn refresh_menu() {
    let _ = std::process::Command::new("kbuildsycoca6")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("sweeflow.sock")
}

/// `sweeflow launch <bundle id>`: asks the running client to open the app on the Mac.
pub fn launch(bundle_id: &str) -> Result<()> {
    use std::io::Write;
    let mut s = std::os::unix::net::UnixStream::connect(socket_path())
        .map_err(|e| anyhow::anyhow!("sweeflow isn't running ({e}); start it with `systemctl --user start sweeflow`"))?;
    writeln!(s, "launch {bundle_id}")?;
    Ok(())
}
