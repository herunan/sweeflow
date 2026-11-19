//! Launcher entries for the Mac's apps, two each: a menu entry (`sweeflow-menu-<bundle id>`),
//! shown in KDE's app menu / KRunner with the plain icon, and a hidden taskbar entry
//! (`sweeflow-<bundle id>`, the windows' app_id, which the taskbar matches first) that carries the
//! icon. The taskbar takes a window's icon from its entry.
//!
//! Only one of the two names the windows' class (StartupWMClass): GNOME's shell matches windows
//! to apps by that before file names, and with two entries claiming it each window went to
//! whichever it had read first, so the Ubuntu Dock showed an app twice (the windows there at
//! connect in one, later ones in the other). The menu entry has it, so the windows go to the
//! entry that can be pinned; KDE's taskbar matches the taskbar entry by its file name first.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;

fn data_dir() -> PathBuf {
    crate::config::xdg_home("XDG_DATA_HOME", ".local/share")
}

/// The Mac display window's entry (see setup::write_launcher), kept when Mac apps' go.
pub const DISPLAY_ENTRY: &str = "sweeflow-display.desktop";

/// The Mac app's icon in the taskbar: the same as everywhere else. (It carried a Sweeflow badge
/// once, as a copy under icons/badged; that is gone, and so are the copies.)
pub fn task_icon_path(bundle_id: &str) -> PathBuf {
    original_path(bundle_id)
}

/// Where a Mac app's icon is saved (as PNG), as the Mac sent it.
pub fn original_path(bundle_id: &str) -> PathBuf {
    data_dir().join(format!("sweeflow/icons/{bundle_id}.png"))
}

/// The badged copies an earlier version made, and its mark in an entry that still points at one.
fn old_badged_dir() -> PathBuf {
    data_dir().join("sweeflow/icons/badged")
}
const OLD_BADGED: &str = "/sweeflow/icons/badged/";

/// In the taskbar entry of an app that isn't among the Mac's installed apps (see `seen`).
const SEEN_MARK: &str = "X-Sweeflow-Seen=true\n";

/// A Mac app has a window here, and this is its icon. The Mac lists the apps in its
/// Applications folders only, so one that lives elsewhere (inside Xcode, in a subfolder, a
/// menu-bar app's window, a system tool) had no taskbar entry, and the taskbar showed its
/// window's own icon, without the Quit action. Such an
/// app gets a taskbar entry of its own here (never in the app menu: only its window says it
/// exists). Slow (it has KDE reread the entries): not for the UI thread. True when the entry was
/// just written (and KDE has reread them).
pub fn seen(bid: &str, name: &str, png: &[u8]) -> Result<bool> {
    // Bundle ids are reverse-DNS, safe for file names; guard anyway.
    if bid.is_empty() || bid.contains('/') || png.is_empty() {
        return Ok(false);
    }
    let apps_dir = data_dir().join("applications");
    let path = apps_dir.join(task_file(bid));
    // (One from a version with the badge is written again, with the plain icon.)
    if fs::read_to_string(&path).is_ok_and(|old| !old.contains(OLD_BADGED)) {
        return Ok(false);
    }
    fs::create_dir_all(&apps_dir)?;
    fs::create_dir_all(data_dir().join("sweeflow/icons"))?;
    let original = original_path(bid);
    if fs::read(&original).ok().as_deref() != Some(png) {
        fs::write(&original, png)?;
    }
    let exe = std::env::current_exe()?;
    let text = entry(&exe, bid, name, &task_icon_path(bid), false).replacen("NoDisplay=true\n", &format!("NoDisplay=true\n{SEEN_MARK}"), 1);
    write_whole(&path, &text)?;
    log::info!("taskbar entry for {name} ({bid}): not among the Mac's listed apps");
    refresh_menu();
    Ok(true)
}

/// How long GNOME's shell takes, at most, to read an entry just written (it reads them a few ms
/// after the change, longer while it takes in a whole first connection's worth).
const GNOME_READS_IN: std::time::Duration = std::time::Duration::from_millis(1200);

/// Before a Mac app's window is shown on GNOME: waits until the shell has read the app's entry,
/// if it was only just written. GNOME matches a window to its app once, as the window appears,
/// and never again when entries change; a window that came first stayed an app of its own
/// ("sweeflow-dev.herdr.app"), and for such a window GNOME skips its permission store, so
/// "Allow inhibiting shortcuts" came up for it (and for each further such window) although
/// Sweeflow grants it for its Mac apps. Waits only for a fresh entry: once per app.
pub fn wait_until_read(bid: &str) {
    if cfg!(test) || !crate::desktop::is("GNOME") || bid.is_empty() || bid.contains('/') {
        return;
    }
    let dir = data_dir().join("applications");
    let newest = [task_file(bid), menu_file(bid)]
        .iter()
        .filter_map(|f| fs::metadata(dir.join(f)).and_then(|m| m.modified()).ok())
        .max();
    let Some(age) = newest.and_then(|t| t.elapsed().ok()) else { return };
    if age < GNOME_READS_IN {
        log::info!("{bid}: waiting for GNOME to read its new entry before its window");
        std::thread::sleep(GNOME_READS_IN - age);
    }
}

/// Keeps ~/.local/share/applications/sweeflow-*.desktop in sync with the Mac's installed apps.
pub fn sync(apps: &[(String, String, Vec<u8>)]) -> Result<()> {
    let apps_dir = data_dir().join("applications");
    let icon_dir = data_dir().join("sweeflow/icons");
    fs::create_dir_all(&apps_dir)?;
    fs::create_dir_all(&icon_dir)?;
    let _ = fs::remove_dir_all(old_badged_dir());
    write_menu()?;
    let exe = std::env::current_exe()?;
    let mut keep = std::collections::HashSet::from([DISPLAY_ENTRY.to_string()]);
    for (bid, name, png) in apps {
        // Bundle ids are reverse-DNS, safe for file names; guard anyway.
        if bid.is_empty() || bid.contains('/') {
            continue;
        }
        let original = original_path(bid);
        if !png.is_empty() && fs::read(&original).ok().as_deref() != Some(png.as_slice()) {
            fs::write(&original, png)?;
        }
        // The taskbar's: always hidden from the menu.
        let entries = [
            (task_file(bid), entry(&exe, bid, name, &task_icon_path(bid), false).replacen(&wm_class_line(bid), "", 1)),
            (menu_file(bid), entry(&exe, bid, name, &original, VISIBLE.load(Ordering::SeqCst))),
        ];
        for (file, entry) in entries {
            let path = apps_dir.join(&file);
            if fs::read_to_string(&path).ok().as_deref() != Some(entry.as_str()) {
                write_whole(&path, &entry)?;
            }
            keep.insert(file);
        }
    }
    // Apps uninstalled on the Mac.
    for e in fs::read_dir(&apps_dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("sweeflow-") && name.ends_with(".desktop") && !keep.contains(&name) {
            // An app the Mac doesn't list, known by its window (see `seen`): kept.
            if fs::read_to_string(e.path()).is_ok_and(|t| t.contains(SEEN_MARK)) {
                continue;
            }
            let _ = fs::remove_file(e.path());
        }
    }
    log::info!("launcher entries for {} Mac apps", (keep.len() - 1) / 2);
    refresh_menu();
    Ok(())
}

/// The entry the taskbar matches a Mac app's windows to (their app_id is `sweeflow-<bundle id>`).
fn task_file(bid: &str) -> String {
    format!("sweeflow-{bid}.desktop")
}

/// The entry in the app menu and KRunner.
fn menu_file(bid: &str) -> String {
    format!("sweeflow-menu-{bid}.desktop")
}

/// Which Mac app an entry's file is for, and whether it's the menu one (None: not a Mac app's).
fn entry_of(file: &str) -> Option<(&str, bool)> {
    if file == DISPLAY_ENTRY {
        return None;
    }
    let rest = file.strip_prefix("sweeflow-")?.strip_suffix(".desktop")?;
    Some(match rest.strip_prefix("menu-") {
        Some(bid) => (bid, true),
        None => (rest, false),
    })
}

/// An entry's claim to the windows of `bid` (see the top).
fn wm_class_line(bid: &str) -> String {
    format!("StartupWMClass=sweeflow-{bid}\n")
}

/// The Quit action's name: "Quit on Mac" on GNOME, whose dash adds its own "Quit" under it (that
/// one only closes the app's windows here), else "Quit".
fn quit_label() -> &'static str {
    let gnome = crate::desktop::is("GNOME");
    if gnome { "Quit on Mac" } else { "Quit" }
}

/// A Mac app's launcher entry. Its Quit action quits the app on the Mac (KDE's task manager and
/// GNOME's dash offer it on the app's right-click menu, window or not).
fn entry(exe: &std::path::Path, bid: &str, name: &str, icon: &std::path::Path, visible: bool) -> String {
    // No Comment/GenericName: macOS has no description for its apps, and a made-up one
    // would be the same on every entry.
    format!(
        "[Desktop Entry]\nType=Application\nName={name} (Mac)\nExec={exe} launch {bid}\nIcon={}\n\
         StartupWMClass=sweeflow-{bid}\nStartupNotify=true\nCategories=X-Sweeflow;\nKeywords=mac;macos;\nTerminal=false\nActions=quit;\n{}\
         \n[Desktop Action quit]\nName={quit}\nIcon=application-exit\nExec={exe} quit {bid}\n",
        icon.display(),
        if visible { "" } else { "NoDisplay=true\n" },
        exe = exe.display(),
        quit = quit_label(),
    )
}

/// An entry's text shown or hidden: NoDisplay goes at the end of the main group, before any
/// action groups (a key after them would belong to the last one).
fn with_visibility(text: &str, visible: bool) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with("NoDisplay=")).collect();
    let mut end = lines.iter().position(|l| l.starts_with("[Desktop Action")).unwrap_or(lines.len());
    while end > 0 && lines[end - 1].trim().is_empty() && end < lines.len() {
        end -= 1;
    }
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i == end && !visible {
            out.push_str("NoDisplay=true\n");
        }
        out.push_str(l);
        out.push('\n');
    }
    if end == lines.len() && !visible {
        out.push_str("NoDisplay=true\n");
    }
    out
}

fn config_dir() -> PathBuf {
    crate::config::xdg_home("XDG_CONFIG_HOME", ".config")
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

/// Shows or hides every Mac app's menu entry (hidden while the Mac isn't connected, or switched
/// off). The taskbar entries stay hidden.
pub fn set_visible(visible: bool) {
    VISIBLE.store(visible, Ordering::SeqCst);
    let apps_dir = data_dir().join("applications");
    let Ok(entries) = fs::read_dir(&apps_dir) else { return };
    let mut changed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((_, menu)) = entry_of(&name) else { continue };
        let Ok(text) = fs::read_to_string(e.path()) else { continue };
        let new = with_visibility(&text, visible && menu);
        if new != text && write_whole(&e.path(), &new).is_ok() {
            changed += 1;
        }
    }
    if changed > 0 {
        log::info!("Mac app entries {}", if visible { "shown" } else { "hidden" });
        refresh_menu();
    }
}

/// An entry written whole: a new file renamed over the old, so a launcher reading the folder
/// meanwhile sees the old entry or the new one, never half of one, and every folder watcher
/// hears of it (a rename into the folder, not only a change to a file).
fn write_whole(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Not ending in .desktop: not an entry while it's being written.
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Changes to the entries, counted: a later change makes an earlier one's settle nudge moot.
static CHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Tells the launchers to pick up the changes: KDE now (rather than whenever it notices), and
/// every launcher once more when the changes have settled. Omarchy's reads which entries are
/// hidden with a scan that ignores changes made while it runs: hidden and shown again quickly
/// (Pause, Resume), its menu kept the Mac apps hidden for minutes, until some other entry
/// changed. So a moment after the last change, one of ours is written again.
fn refresh_menu() {
    if cfg!(test) {
        return;
    }
    let _ = std::process::Command::new("kbuildsycoca6")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let n = CHANGES.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if CHANGES.load(Ordering::SeqCst) != n {
            return;
        }
        let path = data_dir().join("applications").join(DISPLAY_ENTRY);
        if let Ok(text) = fs::read_to_string(&path) {
            let _ = write_whole(&path, &text);
        }
    });
}

pub fn socket_path() -> PathBuf {
    crate::config::runtime_dir().join("sweeflow.sock")
}

/// `sweeflow launch <bundle id>`: asks the running client to open the app on the Mac.
pub fn launch(bundle_id: &str) -> Result<()> {
    // The launcher's activation token (Wayland; DESKTOP_STARTUP_ID on X11): what lets the client
    // bring the app's window to the front, unminimised, when the app already runs.
    let token = std::env::var("XDG_ACTIVATION_TOKEN")
        .or_else(|_| std::env::var("DESKTOP_STARTUP_ID"))
        .ok()
        .filter(|t| !t.is_empty() && !t.contains('\n'));
    match token {
        Some(t) => tell(&format!("launch {bundle_id} {t}")),
        None => tell(&format!("launch {bundle_id}")),
    }
}

/// `sweeflow quit <bundle id>` (the launchers' Quit action): asks the running client to quit
/// the app on the Mac.
pub fn quit(bundle_id: &str) -> Result<()> {
    tell(&format!("quit {bundle_id}"))
}

/// Sends one line to the running client's socket (see net.rs `listen_local`).
fn tell(line: &str) -> Result<()> {
    use std::io::Write;
    let mut s = std::os::unix::net::UnixStream::connect(socket_path())
        .map_err(|e| anyhow::anyhow!("sweeflow isn't running ({e}); start it with `systemctl --user start sweeflow`"))?;
    writeln!(s, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_writes_a_taskbar_entry_and_a_menu_entry() {
        let dir = std::env::temp_dir().join(format!("sweeflow-launchers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // Only this test reads these (the paths are looked up on each call).
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.join("data"));
            std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        }
        let png = include_bytes!("../assets/traffic-lights.png").to_vec();
        sync(&[("com.apple.mail".into(), "Mail".into(), png)]).unwrap();
        let apps = dir.join("data/applications");
        let task = fs::read_to_string(apps.join("sweeflow-com.apple.mail.desktop")).unwrap();
        let menu = fs::read_to_string(apps.join("sweeflow-menu-com.apple.mail.desktop")).unwrap();
        assert!(task.contains("NoDisplay=true"), "{task}");
        assert!(!menu.contains("NoDisplay=true"), "{menu}");
        assert!(task.contains(&format!("Icon={}", original_path("com.apple.mail").display())), "{task}");
        assert!(menu.contains(&format!("Icon={}", original_path("com.apple.mail").display())), "{menu}");
        // One entry names the windows' class: the menu one (GNOME matches by it first).
        assert!(menu.contains("StartupWMClass=sweeflow-com.apple.mail\n"), "{menu}");
        assert!(!task.contains("StartupWMClass"), "{task}");
        // An app the Mac doesn't list, seen with a window: a taskbar entry only, kept when the listed apps are synced again. A listed app's is left alone.
        let png = include_bytes!("../assets/traffic-lights.png").to_vec();
        seen("com.apple.iphonesimulator", "Simulator", &png).unwrap();
        seen("com.apple.mail", "Mail", &png).unwrap();
        let sim = fs::read_to_string(apps.join("sweeflow-com.apple.iphonesimulator.desktop")).unwrap();
        assert!(sim.contains("NoDisplay=true\nX-Sweeflow-Seen=true\n\n[Desktop Action quit]"), "{sim}");
        assert!(sim.contains("StartupWMClass=sweeflow-com.apple.iphonesimulator\n"), "{sim}");
        assert!(sim.contains(&format!("Icon={}", original_path("com.apple.iphonesimulator").display())), "{sim}");
        assert!(!apps.join("sweeflow-menu-com.apple.iphonesimulator.desktop").exists());
        assert_eq!(fs::read_to_string(apps.join("sweeflow-com.apple.mail.desktop")).unwrap(), task);
        sync(&[("com.apple.mail".into(), "Mail".into(), png)]).unwrap();
        assert!(apps.join("sweeflow-com.apple.iphonesimulator.desktop").exists());
        // Hiding the menu (the Mac away) keeps the taskbar entry hidden; showing it again too.
        set_visible(false);
        set_visible(true);
        assert!(fs::read_to_string(apps.join("sweeflow-com.apple.mail.desktop")).unwrap().contains("NoDisplay=true"));
        assert!(!fs::read_to_string(apps.join("sweeflow-menu-com.apple.mail.desktop")).unwrap().contains("NoDisplay=true"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_files_tell_taskbar_from_menu() {
        assert_eq!(entry_of(&task_file("com.apple.mail")), Some(("com.apple.mail", false)));
        assert_eq!(entry_of(&menu_file("com.apple.mail")), Some(("com.apple.mail", true)));
        assert_eq!(entry_of(DISPLAY_ENTRY), None);
        assert_eq!(entry_of("org.kde.dolphin.desktop"), None);
        // The taskbar's entry is the one named after the windows' app_id.
        assert_eq!(task_file("com.apple.mail"), "sweeflow-com.apple.mail.desktop");
    }

    #[test]
    fn hiding_and_showing_an_entry_matches_writing_it_so() {
        let (exe, icon) = (std::path::Path::new("/usr/bin/sweeflow"), std::path::Path::new("/i/com.apple.Notes.png"));
        let shown = entry(exe, "com.apple.Notes", "Notes", icon, true);
        let hidden = entry(exe, "com.apple.Notes", "Notes", icon, false);
        assert!(shown.contains("Actions=quit;\n") && shown.contains("\n[Desktop Action quit]\nName=Quit\nIcon=application-exit\nExec=/usr/bin/sweeflow quit com.apple.Notes\n"));
        // NoDisplay in the main group, not the action's.
        assert!(hidden.find("NoDisplay=true").unwrap() < hidden.find("[Desktop Action quit]").unwrap());
        assert_eq!(with_visibility(&shown, false), hidden);
        assert_eq!(with_visibility(&hidden, true), shown);
        assert_eq!(with_visibility(&hidden, false), hidden);
    }

    #[test]
    fn entries_without_actions_get_no_display_at_the_end() {
        let old = "[Desktop Entry]\nType=Application\nName=Notes (Mac)\n";
        assert_eq!(with_visibility(old, false), format!("{old}NoDisplay=true\n"));
        assert_eq!(with_visibility(&format!("{old}NoDisplay=true\n"), true), old);
    }
}
