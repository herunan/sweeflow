//! `sweeflow setup user@mac`: installs the Mac host over SSH and pairs the two machines.
//! SSH already authenticates both ends, so it doubles as the pairing channel: our client
//! certificate fingerprint goes to the Mac's allow-list and the Mac's fingerprint comes back.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};

use crate::config::{self, Config};

// The Mac host's sources, embedded by build.rs so the Linux binary is all you need.
include!(concat!(env!("OUT_DIR"), "/mac_files.rs"));

const REMOTE_SRC: &str = "~/.sweeflow/src";
const SUPPORT: &str = "\"$HOME/Library/Application Support/sweeflow\"";

fn ssh(target: &str, script: &str) -> Result<String> {
    ssh_io(target, script, Stdio::piped())
}

/// Like `ssh` but lets the remote output through to the terminal (for long builds).
fn ssh_visible(target: &str, script: &str) -> Result<()> {
    ssh_io(target, script, Stdio::inherit()).map(|_| ())
}

fn ssh_io(target: &str, script: &str, stdout: Stdio) -> Result<String> {
    let mut child = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", target, "bash", "-s"])
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(Stdio::inherit())
        .spawn()
        .context("running ssh")?;
    {
        let mut si = child.stdin.take().unwrap();
        si.write_all(script.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("remote command failed on {target}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn setup(target: &str) -> Result<()> {
    let host = target.rsplit('@').next().unwrap_or(target);
    println!("==> checking SSH access to {target}");
    let check = ssh(target, "sw_vers -productVersion; xcode-select -p >/dev/null 2>&1 && echo tools-ok || echo tools-missing\n")
        .context("could not SSH into the Mac. Enable System Settings → General → Sharing → Remote Login, then `ssh-copy-id` your key")?;
    let mut lines = check.lines();
    println!("    macOS {}", lines.next().unwrap_or("?"));
    if lines.next() != Some("tools-ok") {
        bail!("Xcode Command Line Tools are missing on the Mac. Run `xcode-select --install` there, then re-run setup.");
    }

    println!("==> uploading host sources");
    // One tar stream over one SSH session.
    let mut tar = Vec::new();
    for (path, content) in MAC_FILES {
        append_tar(&mut tar, path, content, if path.ends_with(".sh") { 0o755 } else { 0o644 });
    }
    tar.extend_from_slice(&[0u8; 1024]);
    let mut child = Command::new("ssh")
        .args(["-o", "BatchMode=yes", target, &format!("rm -rf {REMOTE_SRC} && mkdir -p {REMOTE_SRC} && tar -x -C {REMOTE_SRC} -f -")])
        .stdin(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(&tar)?;
    if !child.wait()?.success() {
        bail!("uploading the host sources failed");
    }

    println!("==> building and starting Sweeflow on the Mac (first build takes a minute)");
    ssh_visible(target, &format!("cd {REMOTE_SRC} && ./install.sh 2>&1 | sed 's/^/    /'; exit ${{PIPESTATUS[0]}}\n"))?;

    println!("==> pairing");
    let client_fp = config::client_fingerprint()?;
    // Listed on the Mac by this computer's name (as linking does), not a code.
    let me: String = crate::pair::this_computer().chars().filter(|c| c.is_alphanumeric() || " -_.".contains(*c)).collect();
    let script = format!(
        "set -e; d={SUPPORT}; mkdir -p \"$d\"; touch \"$d/clients\"; \
         if grep -q '^{client_fp}' \"$d/clients\"; then echo known; else echo '{client_fp} {me}' >> \"$d/clients\"; echo new; fi; \
         for i in $(seq 1 20); do [ -s \"$d/fingerprint\" ] && break; sleep 0.5; done; cat \"$d/fingerprint\"\n"
    );
    let out = ssh(target, &script)?;
    let mut out = out.lines();
    let already_linked = out.next() == Some("known");
    let host_fp = out.next().unwrap_or("").trim().to_string();
    if host_fp.len() != 64 {
        bail!("could not read the Mac's certificate fingerprint (is Sweeflow running? see ~/Library/Logs/sweeflow.log on the Mac)");
    }

    let mut cfg = Config::load_or_default()?;
    let name = host.trim_end_matches(".local").to_string();
    // An update of a Mac already linked leaves alone which Mac is in use (and whether it's
    // stopped); a newly linked one becomes the one in use, as linking does.
    let in_list = cfg.macs.iter().any(|m| m.fingerprint == host_fp);
    cfg.add_mac(config::PairedMac { name, host: format!("{host}:44120"), fingerprint: host_fp, ..Default::default() }, !(already_linked && in_list));
    cfg.save()?;
    println!("==> paired. Config written to {}", Config::path().display());
    if already_linked {
        println!("==> updated (already linked, so the Mac keeps its permissions)");
        return Ok(());
    }
    println!();
    println!("On the Mac, allow Sweeflow in System Settings → Privacy & Security under both");
    println!("  • Screen & System Audio Recording");
    println!("  • Accessibility");
    println!("Then run `sweeflow` here.");
    Ok(())
}

/// Starts the client for the active Mac: restarts it if it runs as a service already, else
/// installs it (so it also starts with the desktop from now on).
pub fn start_client() -> Result<()> {
    let installed = Command::new("systemctl")
        .args(["--user", "cat", "sweeflow.service"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if installed {
        let ok = Command::new("systemctl").args(["--user", "restart", "sweeflow.service"]).status()?.success();
        if !ok {
            bail!("couldn't restart sweeflow (see `journalctl --user -u sweeflow`)");
        }
        Ok(())
    } else {
        install()
    }
}

/// Installs this binary to ~/.local/bin (unless a package put it under /usr: see
/// installed_binary) and a systemd user service that runs it with the
/// desktop session (restarting it if it ever exits).
/// `sweeflow install` has put the user service in place.
pub fn service_installed() -> bool {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .is_some_and(|h| h.join(".config/systemd/user/sweeflow.service").exists())
}

/// The binary the service, launchers and shortcuts run: the system's copy where a package put
/// it (under /usr: the package's upgrades then reach it), else the user's own in ~/.local/bin,
/// where `install` copies it.
pub fn installed_binary(home: &std::path::Path) -> std::path::PathBuf {
    match std::env::current_exe() {
        Ok(me) if me.starts_with("/usr/") => me,
        _ => home.join(".local/bin/sweeflow"),
    }
}

pub fn install() -> Result<()> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    let bin_dir = home.join(".local/bin");
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let target = installed_binary(&home);
    let me = std::env::current_exe()?;
    // Installed by a package now, after a copy of its own: that copy comes first on PATH, and
    // `sweeflow` typed in a terminal would have run it, old, for good.
    let own = bin_dir.join("sweeflow");
    if target != own && own.exists() {
        let _ = std::fs::remove_file(&own);
    }
    if me != target {
        std::fs::create_dir_all(&bin_dir)?;
        // Copy to a temp name then rename, so a running copy is replaced atomically.
        let tmp = bin_dir.join(".sweeflow.new");
        std::fs::copy(&me, &tmp)?;
        std::fs::rename(&tmp, &target)?;
    }
    std::fs::write(
        unit_dir.join("sweeflow.service"),
        format!(
            "[Unit]\n\
             Description=sweeflow: Mac windows on this desktop\n\
             PartOf=graphical-session.target\n\
             After=graphical-session.target\n\n\
             [Service]\n\
             ExecStart={}\n\
             Restart=always\n\
             RestartSec=2\n\
             Environment=RUST_LOG=info,zbus=warn,tracing=warn\n\n\
             [Install]\n\
             WantedBy=graphical-session.target\n",
            target.display()
        ),
    )?;
    let run = |args: &[&str]| Command::new("systemctl").arg("--user").args(args).status();
    run(&["daemon-reload"])?;
    if !run(&["enable", "sweeflow.service"])?.success() {
        bail!("could not enable the sweeflow service");
    }
    // Desktops whose session doesn't start systemd's graphical-session.target (Cinnamon, MATE,
    // Xfce…) never started the service at login: an autostart entry hands it the session's
    // display and starts it. Where the target does start it, this is a no-op.
    let autostart = home.join(".config/autostart");
    std::fs::create_dir_all(&autostart)?;
    std::fs::write(
        autostart.join("sweeflow.desktop"),
        "[Desktop Entry]\nType=Application\nName=Sweeflow\nComment=Mac windows on this desktop\nIcon=sweeflow\n\
         Exec=sh -c 'dbus-update-activation-environment --systemd DISPLAY XAUTHORITY WAYLAND_DISPLAY XDG_CURRENT_DESKTOP; systemctl --user start sweeflow.service'\n\
         NoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
    )?;
    // Before the service starts: a bar that looks an icon up before its file is there (Omarchy's)
    // keeps showing a placeholder.
    write_icons(&home)?;
    run(&["restart", "sweeflow.service"])?;
    install_icon_and_widget(&home)?;
    // Where the desktop has the Global Shortcuts portal, the running client asks for the keys
    // that way (globalkeys.rs); elsewhere they go into the desktop's own settings.
    if !crate::globalkeys::available() {
        register_open_key(&home);
        register_stream_keys(&home);
        if std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("Cinnamon") {
            cinnamon_commands_now(&home);
        }
    }
    if std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("GNOME") {
        gnome_utilities_folder(true);
    }
    println!("==> sweeflow installed to {} and running as a user service", target.display());
    println!("    logs: journalctl --user -u sweeflow -f");
    Ok(())
}

/// `sweeflow uninstall`: undoes what `install` and the running client set up for this user, so
/// nothing is left running a binary a removed package took with it, nor launchers and shortcuts
/// for it. Every step on its own: one that fails says so and the rest still run. The package's
/// own files (under /usr) are its package manager's. The pairing with the Mac (the config
/// folder) stays unless `purge`: installed again, Sweeflow knows its Mac.
pub fn uninstall(purge: bool) -> Result<()> {
    crate::hyprland::find_session();
    let home = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    let kde = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("KDE");
    let rm = |path: &std::path::Path| remove_path(&home, path);
    let warn = |what: &str| println!("    warning: {what}");

    // First, so the client doesn't write its launchers and icons again while they go.
    let units = home.join(".config/systemd/user");
    if units.join("sweeflow.service").exists() {
        let systemctl = |args: &[&str]| {
            Command::new("systemctl").arg("--user").args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
        };
        if systemctl(&["disable", "--now", "sweeflow.service"]) {
            println!("    stopped and disabled the sweeflow user service");
        } else {
            warn("couldn't stop the sweeflow service (systemctl --user disable --now sweeflow)");
        }
        rm(&units.join("sweeflow.service"));
        // The link `enable` made, should systemctl not have taken it away.
        rm(&units.join("graphical-session.target.wants/sweeflow.service"));
        if !systemctl(&["daemon-reload"]) {
            warn("couldn't reload systemd's user units (systemctl --user daemon-reload)");
        }
        // A service that kept failing is still listed until this.
        let _ = systemctl(&["reset-failed", "sweeflow.service"]);
    }
    rm(&home.join(".config/autostart/sweeflow.desktop"));

    // The shortcuts, before the widget: Plasma's open key belongs to it.
    let widgets = if kde { plasma_widgets().unwrap_or_default() } else { Vec::new() };
    unregister_plasma_keys(&home, kde, &widgets);
    unregister_gsettings_keys("GNOME", GNOME_KEYS, "custom-keybindings", gnome_ours, |p| format!("{GNOME_KEYS}.custom-keybinding:{p}"));
    unregister_gsettings_keys("Cinnamon", CINNAMON_KEYS, "custom-list", cinnamon_ours, |id| cinnamon_schema(id));
    if std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("GNOME") {
        gnome_utilities_folder(false);
    }
    if crate::hyprland::running() {
        match crate::hyprland::unbind_ours() {
            Ok(0) => {}
            Ok(n) => println!("    removed Sweeflow's {n} Hyprland key bindings"),
            Err(e) => warn(&format!("couldn't remove Sweeflow's Hyprland key bindings: {e:#}")),
        }
    }

    if kde {
        // Taken out of the trays (and off any panel it was dragged onto). What earlier versions
        // put on the panels goes too, as `install` does.
        let script = format!(
            r#"panels().forEach(function(p){{ p.widgets("{WIDGET}").forEach(function(w){{ w.remove(); print("ok"); }});
            p.widgets("org.kde.plasma.systemtray").forEach(function(w){{
            w.currentConfigGroup = ["Applets"]; var ids = w.configGroups.slice();
            for (var i = 0; i < ids.length; i++) {{ w.currentConfigGroup = ["Applets", String(ids[i])];
                if (String(w.readConfig("plugin", "")) == "{WIDGET}") {{ w.currentConfigGroup = ["Applets", String(ids[i]), "Shortcuts"]; w.writeConfig("global", ""); }} }}
            w.currentConfigGroup = ["General"];
            ["extraItems","knownItems"].forEach(function(k){{ var v = w.readConfig(k, []); if (typeof v === "string") v = v.split(",");
                var at = v.indexOf("{WIDGET}"); if (at >= 0) {{ v.splice(at, 1); w.writeConfig(k, v); print("ok"); }} }});
            w.reloadConfig(); }}); }});"#
        );
        match plasma_script(&script) {
            Ok(out) if out.contains("ok") => println!("    removed the Sweeflow widget from Plasma's system tray"),
            Ok(_) => {}
            Err(e) => warn(&format!("couldn't take the Sweeflow widget out of the system tray ({e:#}): it's gone from the next login")),
        }
        crate::placement::retire_old_choices();
        match crate::placement::remove_menu_panel() {
            Ok(true) => println!("    removed the top bar Sweeflow made for the Global Menu"),
            Ok(false) => {}
            Err(e) => warn(&format!("couldn't remove the Global Menu's top bar: {e:#}")),
        }
    }
    let plasmoids = home.join(".local/share/plasma/plasmoids");
    for widget in [WIDGET, "dev.sweeflow.dock", "dev.sweeflow.running"] {
        rm(&plasmoids.join(widget));
    }

    // The launchers: Sweeflow's own (setup.rs) under the home folder, the Mac apps' (launchers.rs)
    // under XDG_DATA_HOME. The same folder unless that is set elsewhere.
    let data = config::xdg_home("XDG_DATA_HOME", ".local/share");
    let mut shares = vec![home.join(".local/share"), data];
    shares.dedup();
    for share in &shares {
        let apps = share.join("applications");
        let mut mac_apps = 0;
        for e in std::fs::read_dir(&apps).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == LAUNCHER || name == crate::launchers::DISPLAY_ENTRY {
                rm(&e.path());
            } else if is_our_entry(&name) {
                match std::fs::remove_file(e.path()) {
                    Ok(()) => mac_apps += 1,
                    Err(err) => warn(&format!("couldn't remove {}: {err}", tilde(&home, &e.path()))),
                }
            }
        }
        if mac_apps > 0 {
            println!("    removed {mac_apps} Mac app launchers from {}", tilde(&home, &apps));
        }
        rm(&share.join("desktop-directories/sweeflow.directory"));
        // The Mac apps' icons (and the Mac installer earlier versions put there).
        rm(&share.join("sweeflow"));
    }
    rm(&config::xdg_home("XDG_CONFIG_HOME", ".config").join("menus/applications-merged/sweeflow.menu"));
    if kde {
        // KDE's app menu drops them now rather than whenever it notices.
        let _ = crate::desktop::run_quiet(&mut Command::new("kbuildsycoca6"));
    }

    let hicolor = home.join(".local/share/icons/hicolor");
    let mut icons = 0;
    for dir in ["scalable/apps", "symbolic/apps", "scalable/status"] {
        for e in std::fs::read_dir(hicolor.join(dir)).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("sweeflow") && name.ends_with(".svg") {
                match std::fs::remove_file(e.path()) {
                    Ok(()) => icons += 1,
                    Err(err) => warn(&format!("couldn't remove {}: {err}", tilde(&home, &e.path()))),
                }
            }
        }
    }
    if icons > 0 {
        println!("    removed {icons} Sweeflow icons from {}", tilde(&home, &hicolor));
        // So desktops look at the theme folder again (see write_icons).
        if let Ok(f) = std::fs::File::open(&hicolor) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
    }

    rm(&config::xdg_home("XDG_CACHE_HOME", ".cache").join("sweeflow"));
    // Running from it is fine: the running program keeps the file until it exits.
    rm(&home.join(".local/bin/sweeflow"));
    rm(&home.join(".local/bin/.sweeflow.new"));

    let dir = config::dir();
    if purge {
        rm(&dir);
    } else {
        // The notes that the shortcuts were set once (see `register_key`): they are gone now, so
        // installed again, Sweeflow sets them again.
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if is_key_note(&e.file_name().to_string_lossy()) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        if dir.exists() {
            println!("    kept {} (the pairing with your Mac): `sweeflow uninstall --purge` removes it too", tilde(&home, &dir));
        }
    }
    println!("==> sweeflow uninstalled for this user");
    if std::path::Path::new("/usr/bin/sweeflow").exists() {
        println!("    (/usr/bin/sweeflow is the package's: your package manager removes it)");
    }
    Ok(())
}

/// Removes a file or a whole folder for `uninstall`, with a line saying so; nothing when it
/// isn't there.
fn remove_path(home: &std::path::Path, path: &std::path::Path) {
    let removed = match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        _ => std::fs::remove_file(path),
    };
    match removed {
        Ok(()) => println!("    removed {}", tilde(home, path)),
        Err(e) => println!("    warning: couldn't remove {}: {e}", tilde(home, path)),
    }
}

/// A path as ~/… when it is in the home folder: shorter lines.
fn tilde(home: &std::path::Path, path: &std::path::Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// One of the entries Sweeflow writes in the applications folder: its own (`write_launcher`),
/// the Mac apps' (`launchers::sync`, `sweeflow-<bundle id>.desktop` and `sweeflow-menu-…`), and
/// one of those half written (launchers::write_whole's temporary file).
fn is_our_entry(name: &str) -> bool {
    name == LAUNCHER
        || (name.starts_with("sweeflow-") && name.ends_with(".desktop"))
        || (name.starts_with(".sweeflow-") && name.ends_with(".tmp"))
}

/// The notes `register_key` leaves in the config folder, one per shortcut set ("open-key.gnome",
/// "toggle-pause-key-y.plasma"…).
fn is_key_note(name: &str) -> bool {
    name.starts_with("open-key.") || STREAM_KEYS.iter().any(|k| name.starts_with(&format!("{}-key-", k.id)))
}

/// Sweeflow's custom shortcuts on Plasma go: the streaming toggles (the app menu entry's
/// actions), the "launch" key earlier versions gave the entry, and the tray widget's own key.
/// Through Plasma's shortcut service while it runs (it would write its own copy back over the
/// file), and out of its file either way.
/// One of Sweeflow's keys changed in its Shortcuts window ("open", "pause", "stop"): the
/// shortcut Sweeflow wrote into the desktop's own settings (GNOME before the portal, Cinnamon)
/// gets the new key. Where the portal or Hyprland's bindings have the keys, nothing here.
pub fn apply_own_key(name: &str) {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    // Plasma and GNOME through the portal: Sweeflow gives the desktop the key (globalkeys.rs).
    if desktop.contains("KDE") && crate::globalkeys::in_use() {
        return crate::globalkeys::plasma_set(name);
    }
    if desktop.contains("GNOME") && crate::globalkeys::in_use() {
        return crate::globalkeys::gnome_set();
    }
    let id = match name {
        "open" => None,
        "pause" => Some("toggle-pause"),
        "stop" => Some("toggle-stream"),
        _ => return,
    };
    let key = crate::keys::own_key(name);
    let text = key.as_ref().map(|k| gtk_accel(k, mac_keys_remapped())).unwrap_or_default();
    let result = if desktop.contains("GNOME") && !crate::globalkeys::in_use() {
        let path = id.map_or(GNOME_KEY_PATH.to_string(), gnome_path);
        gsettings(&["set", &format!("{GNOME_KEYS}.custom-keybinding:{path}"), "binding", &gvariant_str(&text)])
    } else if desktop.contains("Cinnamon") {
        let cid = id.map_or(CINNAMON_OPEN.to_string(), cinnamon_id);
        let list = if text.is_empty() { "@as []".to_string() } else { format!("[{}]", gvariant_str(&text)) };
        let r = gsettings(&["set", &cinnamon_schema(&cid), "binding", &list]);
        // Cinnamon takes a changed key up only with its list (the old key went on working).
        cinnamon_reread();
        r
    } else {
        return;
    };
    match result {
        Ok(_) => log::info!("{name}: the desktop's shortcut is {text} now"),
        Err(e) => log::warn!("{name}: couldn't give the desktop's shortcut its new key: {e:#}"),
    }
}

/// A key as GNOME and Cinnamon write one: "<Super><Alt>o". Behind a Mac-style remapper ⌘
/// arrives as Ctrl outside the Mac windows: that's the key set.
pub(crate) fn gtk_accel(k: &crate::deskkeys::UserKey, remapped: bool) -> String {
    let mut out = String::new();
    if k.logo && !remapped {
        out.push_str("<Super>");
    }
    if k.ctrl || (k.logo && remapped) {
        out.push_str("<Control>");
    }
    if k.alt {
        out.push_str("<Alt>");
    }
    if k.shift {
        out.push_str("<Shift>");
    }
    let key = match k.key.as_str() {
        "Space" => "space".to_string(),
        "Enter" => "Return".into(),
        "Esc" => "Escape".into(),
        "PageUp" => "Page_Up".into(),
        "PageDown" => "Page_Down".into(),
        "Backspace" => "BackSpace".into(),
        "ScrollLock" => "Scroll_Lock".into(),
        one if one.chars().count() == 1 => one.to_lowercase(),
        other => other.to_string(),
    };
    out + &key
}

/// The shortcuts Sweeflow wrote into the desktop's own settings (Plasma's, GNOME's), gone:
/// its keys go through the Global Shortcuts portal now (globalkeys.rs).
pub fn drop_desktop_keys() {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return };
    let kde = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("KDE");
    if kde {
        let widgets = plasma_widgets().unwrap_or_default();
        unregister_plasma_keys(&home, kde, &widgets);
    }
    if std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("GNOME") {
        unregister_gsettings_keys("GNOME", GNOME_KEYS, "custom-keybindings", gnome_ours, |p| format!("{GNOME_KEYS}.custom-keybinding:{p}"));
    }
}

fn unregister_plasma_keys(home: &std::path::Path, kde: bool, widgets: &[(i64, String, String)]) {
    let actions: Vec<&str> = STREAM_KEYS.iter().map(|k| k.id).chain(["_launch"]).collect();
    let mut removed = 0;
    if kde {
        if let Ok(bus) = zbus::blocking::Connection::session() {
            let unregister = |component: &str, action: &str| -> bool {
                kglobalaccel(&bus, "unregister", &(component, action)).ok().and_then(|r| r.body().deserialize::<bool>().ok()).unwrap_or(false)
            };
            removed += actions.iter().filter(|a| unregister(LAUNCHER, a)).count();
            removed += widgets.iter().filter(|w| unregister("plasmashell", &widget_action(&w.1))).count();
        }
    }
    if home.join(".config/kglobalshortcutsrc").exists() {
        for action in &actions {
            let key = ["--file", "kglobalshortcutsrc", "--group", "services", "--group", LAUNCHER, "--key", action, "--delete"];
            let _ = crate::desktop::run_quiet(Command::new("kwriteconfig6").args(key));
        }
    }
    if removed > 0 {
        println!("    removed Sweeflow's Plasma shortcuts");
    }
}

/// Sweeflow's custom shortcuts on GNOME or Cinnamon go: their entries out of the list of custom
/// shortcuts (the user's own stay, in their order), then their settings (`schema` gives each
/// one's). Nothing where the desktop's settings aren't installed.
fn unregister_gsettings_keys(desktop: &str, keys: &str, list_key: &str, ours: fn(&str) -> bool, schema: impl Fn(&str) -> String) {
    let Ok(list) = gsettings(&["get", keys, list_key]) else { return };
    let Some(rest) = without_ours(&list, ours) else { return };
    match gsettings(&["set", keys, list_key, &rest]) {
        Ok(_) => println!("    removed Sweeflow's {desktop} shortcuts"),
        Err(e) => return println!("    warning: couldn't remove Sweeflow's {desktop} shortcuts: {e:#}"),
    }
    for entry in quoted(&list).into_iter().filter(|e| ours(e)) {
        let _ = gsettings(&["reset-recursively", &schema(&entry)]);
    }
}

/// One of Sweeflow's GNOME custom shortcuts' paths: `GNOME_KEY_PATH` and the streaming toggles'
/// next to it (`gnome_path`).
fn gnome_ours(path: &str) -> bool {
    let folder = GNOME_KEY_PATH.trim_end_matches("sweeflow/");
    path.strip_prefix(folder).and_then(|p| p.strip_suffix('/')).is_some_and(|name| !name.contains('/') && cinnamon_ours(name))
}

/// One of Sweeflow's Cinnamon custom shortcuts (`CINNAMON_OPEN`, `cinnamon_id`); also the last
/// part of its GNOME ones' paths.
fn cinnamon_ours(id: &str) -> bool {
    id == CINNAMON_OPEN || id.starts_with(&format!("{CINNAMON_OPEN}-"))
}

/// A list as gsettings prints it without Sweeflow's entries, the others as they were, written
/// as gsettings takes it. None when none of Sweeflow's are in it.
fn without_ours(list: &str, ours: fn(&str) -> bool) -> Option<String> {
    let all = quoted(list);
    let rest: Vec<String> = all.iter().filter(|e| !ours(e)).map(|e| gvariant_str(e)).collect();
    if rest.len() == all.len() {
        return None;
    }
    // An empty list says its type: gsettings can't tell it from the bare brackets.
    Some(if rest.is_empty() { "@as []".to_string() } else { format!("[{}]", rest.join(", ")) })
}

pub fn status() -> Result<()> {
    println!("config:       {}", Config::path().display());
    match Config::load() {
        Ok(c) => {
            println!("host:         {}", c.host);
            println!("host cert:    {}", c.host_fingerprint);
            println!("scale:        {}", c.scale);
            println!("render node:  {}", c.render_node);
        }
        Err(e) => println!("not configured: {e}"),
    }
    println!("client cert:  {}", config::client_fingerprint()?);
    Ok(())
}

/// Appends one regular file to a ustar archive.
fn append_tar(out: &mut Vec<u8>, path: &str, data: &[u8], mode: u32) {
    let mut h = [0u8; 512];
    h[..path.len()].copy_from_slice(path.as_bytes());
    let octal = |buf: &mut [u8], v: u64| {
        let s = format!("{:0width$o}\0", v, width = buf.len() - 1);
        buf.copy_from_slice(s.as_bytes());
    };
    octal(&mut h[100..108], mode as u64);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], data.len() as u64);
    octal(&mut h[136..148], 0);
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let s = format!("{:06o}\0 ", sum);
    h[148..156].copy_from_slice(s.as_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(data);
    let pad = (512 - data.len() % 512) % 512;
    out.extend(std::iter::repeat_n(0u8, pad));
}

/// The logo as a plain silhouette in the theme's text colour: the tray widget's.
const LOGO: &str = include_str!("../../assets/sweeflow.svg");
/// The logo for trays outside Plasma (GNOME with AppIndicator, Omarchy's bar…): in one colour, so
/// the bar draws it in its own text colour, as Plasma's widget does.
const TRAY_ICON: &str = include_str!("../../assets/sweeflow-symbolic.svg");
/// The app icon (white on a violet rounded square): app menu, Sweeflow's windows.
const APP_ICON: &str = include_str!("../../assets/sweeflow-app.svg");
/// The Plasma widgets' files (published path inside ~/.local/share/plasma/plasmoids, contents),
/// built in so the binary installs them.
const WIDGET_FILES: &[(&str, &str)] = &[
    ("dev.sweeflow.panel/metadata.json", include_str!("../plasmoid/dev.sweeflow.panel/metadata.json")),
    ("dev.sweeflow.panel/contents/ui/main.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/main.qml")),
    ("dev.sweeflow.panel/contents/ui/ToggleRow.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/ToggleRow.qml")),
    ("dev.sweeflow.panel/contents/ui/VioletSwitch.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/VioletSwitch.qml")),
    ("dev.sweeflow.panel/contents/ui/FieldFrame.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/FieldFrame.qml")),
    ("dev.sweeflow.panel/contents/ui/SfButton.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/SfButton.qml")),
    ("dev.sweeflow.panel/contents/icons/sweeflow-tray.svg", LOGO),
    ("dev.sweeflow.panel/contents/icons/sweeflow-app.svg", APP_ICON),
    ("dev.sweeflow.panel/contents/icons/dock.svg", include_str!("../plasmoid/dev.sweeflow.panel/contents/icons/dock.svg")),
];

/// Sweeflow in the app menu: opens it (see `open`). Also how desktops match Sweeflow's own
/// windows (the wizard, app id "sweeflow", by the file's name) to the logo. No StartupWMClass:
/// with it, Plasma's task manager takes every window of the process (the Mac apps too) for
/// Sweeflow and shows them all with its logo.
///
/// Its actions are the streaming toggles (`STREAM_KEYS`): on Plasma their shortcuts belong to
/// them (System Settings → Shortcuts → Sweeflow), and the app menu offers them on the entry.
pub fn write_launcher(home: &std::path::Path) -> Result<()> {
    let apps = home.join(".local/share/applications");
    std::fs::create_dir_all(&apps)?;
    let file = apps.join(LAUNCHER);
    let entry = launcher_entry(&installed_binary(home).display().to_string());
    if std::fs::read_to_string(&file).ok().as_deref() != Some(entry.as_str()) {
        std::fs::write(file, entry)?;
    }
    // The Mac display window (app id "sweeflow-display"): the logo in its title bar and taskbar
    // entry. Hidden: it's opened from the tray.
    let display = apps.join(crate::launchers::DISPLAY_ENTRY);
    let entry = "[Desktop Entry]\nType=Application\nName=Mac display\nIcon=sweeflow\nNoDisplay=true\n";
    if std::fs::read_to_string(&display).ok().as_deref() != Some(entry) {
        std::fs::write(display, entry)?;
    }
    Ok(())
}

/// The app menu entry's text (see `write_launcher`), for the installed binary at `bin`.
fn launcher_entry(bin: &str) -> String {
    let ids: String = STREAM_KEYS.iter().map(|k| format!("{};", k.id)).collect();
    let actions: String =
        STREAM_KEYS.iter().map(|k| format!("\n[Desktop Action {}]\nName={}\nExec={bin} {}\nIcon={}\n", k.id, k.name, k.id, k.icon)).collect();
    format!(
        "[Desktop Entry]\nType=Application\nName=Sweeflow\nGenericName=Mac apps\nComment=Mac apps on this desktop\n\
         Exec={bin} open\nIcon=sweeflow\nTerminal=false\nStartupNotify=false\n\
         Categories=Utility;\nKeywords=mac;macos;stream;\nActions={ids}\n{actions}"
    )
}

/// `sweeflow toggle-pause` / `toggle-stream`, what the shortcuts run: the running client's
/// toggle (tray::panel_do "pause-toggle" / "stream-toggle").
#[cfg(test)]
mod packaged_entry_tests {
    /// The packages' own menu entry (packaging/sweeflow.desktop) is the one `sweeflow install` writes:
    /// it said Internet and had no Pause/Stop actions until Sweeflow first ran.
    #[test]
    fn the_packages_entry_is_sweeflows() {
        assert_eq!(include_str!("../packaging/sweeflow.desktop"), super::launcher_entry("sweeflow"));
    }
}

pub fn toggle(action: &str) -> Result<()> {
    let bus = zbus::blocking::Connection::session()?;
    bus.call_method(Some("dev.sweeflow.Client"), "/dev/sweeflow/Panel", Some("dev.sweeflow.Panel"), "Do", &(action, ""))
        .map_err(|e| anyhow::anyhow!("sweeflow isn't running (`sweeflow open` starts it): {e}"))?;
    Ok(())
}

/// The service is running.
fn service_active() -> bool {
    Command::new("systemctl").args(["--user", "is-active", "--quiet", "sweeflow"]).status().is_ok_and(|s| s.success())
}

/// `sweeflow open`, what the app menu entry runs: installs Sweeflow the first time, starts it if
/// it isn't running, and shows it: the tray widget's popup, or "Link a Mac" while none is paired.
pub fn open() -> Result<()> {
    if !service_installed() {
        install()?;
        // The service opens "Link a Mac" itself when there's no Mac yet: Sweeflow's own window on
        // top of it would hide its steps (the first open after installing a package did that).
        if Config::load_or_default()?.host.is_empty() {
            return Ok(());
        }
    } else if Config::load_or_default()?.host.is_empty() {
        if !service_active() {
            // Asks for a Mac as it starts.
            Command::new("systemctl").args(["--user", "start", "sweeflow"]).status()?;
            return Ok(());
        }
        return crate::wizard::run();
    } else if !service_active() {
        let ok = Command::new("systemctl").args(["--user", "start", "sweeflow"]).status()?.success();
        anyhow::ensure!(ok, "couldn't start sweeflow (see `journalctl --user -u sweeflow`)");
    }
    // Without the Plasma widget (GNOME, Hyprland): Sweeflow's own window, which waits for the
    // client to answer by itself.
    if !crate::tray::widget_installed() {
        return crate::panelwin::run();
    }
    // As the widget's shortcut does, when it has one (a click on its icon: the Mac window's
    // Cmd+Opt+O, ui.rs, does just what the key does elsewhere).
    if press_widget_shortcut() {
        return Ok(());
    }
    // Pop the tray widget open, once the client is there to ask.
    let bus = zbus::blocking::Connection::session()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let r = bus.call_method(Some("dev.sweeflow.Client"), "/dev/sweeflow/Panel", Some("dev.sweeflow.Panel"), "Do", &("show", ""));
        match r {
            Ok(_) => return Ok(()),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(250)),
            Err(e) => bail!("sweeflow didn't answer: {e}"),
        }
    }
}

/// A widget Plasma already has on a panel changed. Plasma keeps running the copy it loaded when
/// it started (the taskbar icons went without the Sweeflow badge that way, until the next
/// login), so its shell is restarted: the panels are gone for a second or two, nothing else.
fn reload_widgets() {
    let systemctl = |args: &[&str]| Command::new("systemctl").arg("--user").args(args).status().is_ok_and(|s| s.success());
    if systemctl(&["is-active", "--quiet", "plasma-plasmashell.service"]) {
        if systemctl(&["restart", "plasma-plasmashell.service"]) {
            println!("    (Plasma's panels restarted to load the updated Sweeflow widgets)");
            return;
        }
    }
    println!("    (the Sweeflow widgets were updated: log out and in again to see the changes)");
}

/// The tray logo with a status dot in its empty bottom-right corner (the sweet runs from bottom
/// left to top right): filled, or a ring (paused, not online).
pub(crate) fn tray_icon(colour: &str, hollow: bool) -> String {
    let dot = if hollow {
        format!(r#"<circle cx="105.5" cy="103.6" r="13" fill="none" stroke="{colour}" stroke-width="10"/>"#)
    } else {
        format!(r#"<circle cx="105.5" cy="103.6" r="18" fill="{colour}"/>"#)
    };
    TRAY_ICON.replacen("</svg>", &format!("  {dot}\n</svg>"), 1)
}

/// The icons, into the user's icon theme folder: the logo ("sweeflow") and the tray's
/// ("sweeflow-symbolic").
fn write_icons(home: &std::path::Path) -> Result<()> {
    let icons = home.join(".local/share/icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&icons)?;
    std::fs::write(icons.join("sweeflow.svg"), APP_ICON)?;
    let symbolic = home.join(".local/share/icons/hicolor/symbolic/apps");
    std::fs::create_dir_all(&symbolic)?;
    // The tray's logo with the dot for where the Mac stands, one per state (tray::icon_name),
    // the same as Plasma's widget draws: solid, the dot in the main page's colours. Full colour
    // (a bar recolours a symbolic icon, dot and all, to its text colour). Where the tray finds
    // them (icon_theme_path).
    for (name, colour, hollow) in crate::tray::STATE_DOTS {
        // And a twin under a second name, for showing it again (see tray::TWIN).
        for icon in [crate::tray::tray_icon_of(name), format!("{}{}", crate::tray::tray_icon_of(name), crate::tray::TWIN)] {
            std::fs::write(symbolic.join(format!("{icon}.svg")), tray_icon(colour, hollow))?;
        }
    }
    for old in ["sweeflow-symbolic.svg", "sweeflow-paused-symbolic.svg"] {
        let _ = std::fs::remove_file(symbolic.join(old));
    }
    // The tray menu's dot for where the Mac stands (tray::state_dot), as named icons: GNOME's
    // tray extension can't show a menu item's picture sent as data (the whole menu fails).
    let status = home.join(".local/share/icons/hicolor/scalable/status");
    std::fs::create_dir_all(&status)?;
    for (name, colour, hollow) in crate::tray::STATE_DOTS {
        let shape = if hollow {
            format!(r#"<circle cx="8" cy="8" r="3.7" fill="none" stroke="{colour}" stroke-width="1.3"/>"#)
        } else {
            format!(r#"<circle cx="8" cy="8" r="4.5" fill="{colour}"/>"#)
        };
        let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">{shape}</svg>"#);
        std::fs::write(status.join(format!("{name}.svg")), svg)?;
    }
    // Desktops look again at a theme folder when its date changes (a new file deeper in doesn't
    // change it): what `touch` on it does.
    if let Ok(f) = std::fs::File::open(home.join(".local/share/icons/hicolor")) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    Ok(())
}

/// The logo (icon theme name "sweeflow"), and on Plasma the tray widget (linux/plasmoid), added
/// to every system tray. Plasma keeps its own copy of the panel config, so the tray is changed
/// through its scripting interface rather than the file.
fn install_icon_and_widget(home: &std::path::Path) -> Result<()> {
    write_icons(home)?;
    write_launcher(home)?;
    if !std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("KDE") {
        return Ok(());
    }
    crate::placement::retire_old_choices();
    let plasmoids = home.join(".local/share/plasma/plasmoids");
    let (mut updated, mut new) = (false, false);
    for (path, contents) in WIDGET_FILES {
        let file = plasmoids.join(path);
        std::fs::create_dir_all(file.parent().context("widget path")?)?;
        match std::fs::read_to_string(&file) {
            Ok(old) if old == *contents => continue,
            Ok(_) => updated = true,
            Err(_) => new = true,
        }
        std::fs::write(file, contents)?;
    }
    if updated {
        reload_widgets();
    }
    let script = r#"panels().forEach(function(p){ p.widgets("org.kde.plasma.systemtray").forEach(function(w){
        w.currentConfigGroup = ["General"];
        ["extraItems","knownItems"].forEach(function(k){ var v = w.readConfig(k, []); if (typeof v === "string") v = v.split(",");
            if (v.indexOf("dev.sweeflow.panel") < 0) { v.push("dev.sweeflow.panel"); w.writeConfig(k, v); } });
        w.reloadConfig(); }); });"#;
    let ok = crate::desktop::plasma_eval(script).status().is_ok_and(|s| s.success());
    if !ok {
        println!("    (couldn't add the Sweeflow widget to the system tray: add it from the tray's settings)");
    }
    if new && !updated {
        announce_widget();
    }
    Ok(())
}

/// Tells Plasma's shell the widget is installed, as KDE's package tool does once it has put one
/// in place: the system trays then load it at once. A shell that is running only knows the
/// widgets there were when it started, so a first install otherwise showed no Sweeflow in the
/// tray until Plasma's shell was restarted (the whole desktop went black for a moment).
fn announce_widget() {
    let sent = zbus::blocking::Connection::session().and_then(|bus| {
        bus.emit_signal(None::<&str>, "/KPackage/Plasma/Applet", "org.kde.plasma.kpackage", "packageInstalled", &(WIDGET,))
    });
    if let Err(e) = sent {
        println!("    (couldn't tell Plasma the Sweeflow widget is installed: {e})");
    }
}

/// Cmd+Opt+O opens Sweeflow from any window, as a click on its tray icon does: on Plasma the
/// tray widget's popup (the widget's own shortcut), on GNOME Sweeflow's window (a custom
/// shortcut for `sweeflow open`). (Hyprland has its binding, hyprland.rs, and while a Mac
/// window has the focus the key is caught there on every desktop, ui.rs.)
///
/// Set once, and only while nothing else has the key. After that it is the user's: a note in
/// the config folder says it was done, so a later install doesn't bring back a shortcut they
/// changed or removed (delete the note to have it set again).
fn register_open_key(home: &std::path::Path) {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let remapped = mac_keys_remapped();
    let (does, doing) = ("opens Sweeflow", "opening Sweeflow");
    if desktop.contains("KDE") {
        // (open-key.kde was the note of the app menu entry's shortcut, which this replaces.)
        let key = plasma_key('O', remapped).0;
        register_key("open-key.plasma-widget", &key, 'O', remapped, does, doing, || plasma_open_key(home, remapped));
    } else if desktop.contains("GNOME") {
        register_key("open-key.gnome", &shown_key('O', remapped), 'O', remapped, does, doing, || gnome_open_key(home, remapped));
    } else if desktop.contains("Cinnamon") {
        let key = gnome_key_text('O', remapped);
        register_key("open-key.cinnamon", &shown_key('O', remapped), 'O', remapped, does, doing, || {
            cinnamon_custom_key(home, CINNAMON_OPEN, "Sweeflow", "open", &key)
        });
    }
}

/// A shortcut for one of the streaming toggles: Cmd+Opt+<letter> runs `sweeflow <id>`.
pub struct StreamKey {
    /// The command's argument; also the app menu entry's action and the end of the GNOME path.
    pub id: &'static str,
    pub letter: char,
    /// What the desktop's shortcut settings call it.
    pub name: &'static str,
    /// The app menu action's icon.
    icon: &'static str,
    does: &'static str,
    doing: &'static str,
    pub old: char,
}

/// Cmd+Opt+Y pauses or resumes, Cmd+Opt+Z stops or starts (tray::toggle_action). Not P or X:
/// with Opt pressed a moment late, Cmd+P and Cmd+X are other things on Omarchy and GNOME (P
/// switches monitors on GNOME); nothing of the desktops' has Cmd+Y or Cmd+Z.
/// `old`: the letter an earlier version set, replaced while it's still that one.
pub const STREAM_KEYS: [StreamKey; 2] = [
    StreamKey {
        id: "toggle-pause",
        letter: 'Y',
        old: 'P',
        name: "Pause or resume streaming",
        icon: "media-playback-pause",
        does: "pauses or resumes streaming",
        doing: "pausing or resuming streaming",
    },
    StreamKey {
        id: "toggle-stream",
        letter: 'Z',
        old: 'X',
        name: "Stop or start streaming",
        icon: "media-playback-stop",
        does: "stops or starts streaming",
        doing: "stopping or starting streaming",
    },
];

/// Cmd+Opt+Y and Cmd+Opt+Z (`STREAM_KEYS`) from any window, in the desktop's own shortcut
/// settings, where the user can give them other keys: on Plasma the app menu entry's actions
/// (System Settings → Keyboard → Shortcuts → Sweeflow), on GNOME custom shortcuts (Settings →
/// Keyboard → View and Customize Shortcuts → Custom Shortcuts). Hyprland binds them while
/// Sweeflow runs (hyprland.rs); in a Mac window they are caught there (ui.rs). Same rules as the
/// open key: only while the key is free, once (a note per key).
fn register_stream_keys(home: &std::path::Path) {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let remapped = mac_keys_remapped();
    let mut set = false;
    for k in &STREAM_KEYS {
        if desktop.contains("KDE") {
            // (A new note per letter: the one for the old letter said it was set once.)
            let note = format!("{}-key-{}.plasma", k.id, k.letter.to_ascii_lowercase());
            set |= register_key(&note, &plasma_key(k.letter, remapped).0, k.letter, remapped, k.does, k.doing, || plasma_action_key(k, remapped));
        } else if desktop.contains("GNOME") {
            let note = format!("{}-key-{}.gnome", k.id, k.letter.to_ascii_lowercase());
            let key = gnome_key_text(k.letter, remapped);
            set |= register_key(&note, &shown_key(k.letter, remapped), k.letter, remapped, k.does, k.doing, || {
                // Ours still on the old letter: the new one replaces it.
                let path = gnome_path(k.id);
                let theirs = gsettings(&["get", &format!("{GNOME_KEYS}.custom-keybinding:{path}"), "binding"]).unwrap_or_default();
                if binds(&theirs, &gnome_key_text(k.old, remapped)) {
                    gsettings(&["set", &format!("{GNOME_KEYS}.custom-keybinding:{path}"), "binding", &gvariant_str(&key)])?;
                    return Ok(OpenKey::Set { now: true });
                }
                gnome_custom_key(home, &path, k.name, k.id, &key)
            });
        } else if desktop.contains("Cinnamon") {
            let note = format!("{}-key-{}.cinnamon", k.id, k.letter.to_ascii_lowercase());
            let key = gnome_key_text(k.letter, remapped);
            set |= register_key(&note, &shown_key(k.letter, remapped), k.letter, remapped, k.does, k.doing, || {
                cinnamon_custom_key(home, &cinnamon_id(k.id), k.name, k.id, &key)
            });
        }
    }
    if set && desktop.contains("KDE") {
        println!("    (other keys for them: System Settings → Keyboard → Shortcuts → Sweeflow)");
    } else if set && desktop.contains("GNOME") {
        println!("    (other keys for them: Settings → Keyboard → View and Customize Shortcuts → Custom Shortcuts)");
    } else if set && desktop.contains("Cinnamon") {
        println!("    (other keys for them: System Settings → Keyboard → Shortcuts → Custom Shortcuts)");
    } else if crate::hyprland::running() {
        // The client binds them as it starts (hyprland.rs), so which ones it could isn't known here.
        let keys: Vec<String> = STREAM_KEYS.iter().map(|k| format!("SUPER + ALT + {} {}", k.letter, k.does)).collect();
        println!("    SUPER + ALT + O opens Sweeflow from any window; {} where the key isn't bound already", keys.join(", "));
        println!("    (Hyprland bindings Sweeflow adds while it runs; Sweeflow's window lists the ones it has. For other keys,");
        println!("    bind `sweeflow open`, `sweeflow toggle-pause` or `sweeflow toggle-stream` in your Hyprland config)");
    }
}

/// Sets one of Sweeflow's shortcuts with `set`, once (see `register_open_key`), and says how it
/// went. True when it was set now.
fn register_key(note: &str, key: &str, letter: char, remapped: bool, does: &str, doing: &str, set: impl FnOnce() -> Result<OpenKey>) -> bool {
    let note = config::dir().join(note);
    if note.exists() {
        return false;
    }
    // What's pressed is Cmd+Opt+<letter> either way.
    let key = if remapped { format!("Super+Alt+{letter} ({key} outside Mac windows, as your key remapper turns it into Ctrl)") } else { key.to_string() };
    let set_now = match set() {
        Ok(OpenKey::Set { now: true }) => {
            println!("    {key} {does} from any window");
            true
        }
        // No note yet: the next install looks again whether it is still there.
        Ok(OpenKey::Set { now: false }) => {
            println!("    {key} {does} from any window, from the next login");
            return true;
        }
        Ok(OpenKey::Kept) => false,
        Ok(OpenKey::Taken(by)) => {
            println!("    ({key} is already used by {by}: no shortcut set for {doing})");
            return false;
        }
        Err(e) => {
            println!("    (couldn't set {key} for {doing}: {e:#})");
            return false;
        }
    };
    let _ = std::fs::create_dir_all(config::dir());
    let _ = std::fs::write(note, format!("{key}\n"));
    set_now
}

/// Cmd+Opt+<letter> as the desktop sees it, for the install's lines (GNOME's way of writing it).
fn shown_key(letter: char, remapped: bool) -> String {
    format!("{}+Alt+{letter}", if remapped { "Ctrl" } else { "Super" })
}

/// A Mac-style key remapper is running (Toshy, Kinto): outside the Mac windows (which it leaves
/// alone, as a remote desktop) it turns Cmd into Ctrl, so the desktop sees Cmd+Opt+O as
/// Ctrl+Alt+O, and a shortcut on Super+Alt+O never fires. The shortcut is then put on the key
/// that arrives. Running: its config is there and its key mapper is (the process that reads that
/// config: xwaykeyz for Toshy, xkeysnail for Kinto), checked again every few seconds.
pub fn mac_keys_remapped() -> bool {
    remapper().is_some()
}

/// The Mac-style key remapper running, by name ("Toshy", "Kinto"), checked again every few seconds.
pub fn remapper() -> Option<&'static str> {
    use std::sync::Mutex;
    static LAST: Mutex<Option<(std::time::Instant, Option<&'static str>)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if let Some((at, which)) = *last {
        if at.elapsed() < std::time::Duration::from_secs(3) {
            return which;
        }
    }
    let which = remapper_running();
    *last = Some((std::time::Instant::now(), which));
    which
}

fn remapper_running() -> Option<&'static str> {
    let config = config::xdg_home("XDG_CONFIG_HOME", ".config");
    let known = [("Toshy", config.join("toshy/toshy_config.py")), ("Kinto", config.join("kinto/kinto.py"))];
    let configs: Vec<(&'static str, String)> = known.iter().filter(|(_, c)| c.exists()).map(|(n, c)| (*n, c.display().to_string())).collect();
    if configs.is_empty() {
        return None;
    }
    // A process of this user whose command line names one of those configs.
    let procs = std::fs::read_dir("/proc").ok()?;
    let me = unsafe { libc::getuid() };
    procs.flatten().find_map(|p| {
        use std::os::unix::fs::MetadataExt;
        if p.metadata().map(|m| m.uid()).ok() != Some(me) {
            return None;
        }
        let cmd = std::fs::read(p.path().join("cmdline")).ok()?;
        let cmd = String::from_utf8_lossy(&cmd);
        if !(cmd.contains("xwaykeyz") || cmd.contains("keyszer") || cmd.contains("xkeysnail")) {
            return None;
        }
        configs.iter().find(|(_, c)| cmd.contains(c.as_str())).map(|(n, _)| *n)
    })
}

/// Behind a Mac-style remapper, whether the keyboard typed on is a PC one: Toshy's modmaps
/// differ by keyboard (an Apple one's ⌘ is Super, a PC one's is Alt; see
/// globalkeys::as_linux_gets). Toshy's own choice when the person set one (its tray's
/// "Keyboard type"), else what it would decide from the keyboards plugged in: a PC one when
/// none is an Apple one. An Apple keyboard next to a PC one (a laptop's own and a Magic
/// Keyboard): Apple, the one more likely typed on with a remapper.
pub fn pc_keyboard() -> bool {
    let config = config::xdg_home("XDG_CONFIG_HOME", ".config");
    if let Some(kind) = std::fs::read(config.join("toshy/toshy_user_preferences.sqlite")).ok().and_then(|db| toshy_override(&db)) {
        return kind != "Apple";
    }
    let devices = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
    pc_keyboards_only(&devices)
}

/// The keyboard type set in Toshy's preferences (an SQLite file: the row's name and value sit
/// next to each other in it), unless it's left to Toshy ("Auto-Adapt").
fn toshy_override(db: &[u8]) -> Option<&'static str> {
    const NAME: &[u8] = b"override_kbtype";
    let mut at = 0;
    while let Some(i) = db[at..].windows(NAME.len()).position(|w| w == NAME) {
        let rest = &db[at + i + NAME.len()..];
        if let Some(kind) = ["Apple", "Windows", "IBM", "Chromebook"].into_iter().find(|k| rest.starts_with(k.as_bytes())) {
            return Some(kind);
        }
        at += i + NAME.len();
    }
    None
}

/// Keyboards plugged in (/proc/bus/input/devices), none of them an Apple one by Toshy's list.
/// Virtual ones (a remapper's own, a software KVM's) don't count.
fn pc_keyboards_only(devices: &str) -> bool {
    let names: Vec<String> = devices
        .lines()
        .filter_map(|l| l.strip_prefix("N: Name=\""))
        .map(|n| n.trim_end_matches('"').to_lowercase())
        .filter(|n| n.contains("keyboard"))
        .filter(|n| !["virtual", "xwaykeyz", "keyszer", "uinput", "deskflow", "barrier", "synergy", "input-remapper"].iter().any(|v| n.contains(v)))
        .collect();
    let apple = |n: &String| ["apple", "magic keyboard", "mx keys mac", "usb to adb", "hp touchpad wireless keyboard"].iter().any(|a| n.contains(a));
    !names.is_empty() && !names.iter().any(apple)
}

/// How setting the desktop's shortcut went.
enum OpenKey {
    /// Ours now; `now`: it works already, without logging in again.
    Set { now: bool },
    /// Sweeflow's shortcut was there already, with our key or one the user gave it: left alone.
    Kept,
    /// Something else has the key (its name): left alone.
    Taken(String),
}

/// Cmd+Opt+<letter> on Plasma, as written in its config and as Qt counts it (Qt::META or
/// Qt::CTRL, | Qt::ALT | Qt::Key_<letter>): how KGlobalAccel is asked.
fn plasma_key(letter: char, remapped: bool) -> (String, i32) {
    let letter = letter.to_ascii_uppercase();
    let (name, cmd) = if remapped { ("Ctrl", 0x0400_0000) } else { ("Meta", 0x1000_0000) };
    (format!("{name}+Alt+{letter}"), cmd | 0x0800_0000 | letter as i32)
}

/// What KGlobalAccel says about a shortcut: (action, its name, component, its name, context,
/// its name, keys, default keys).
type ShortcutInfo = (String, String, String, String, String, String, Vec<i32>, Vec<i32>);

/// One of the streaming toggles' shortcuts on Plasma: the key of the app menu entry's action
/// (see `launcher_entry`), as System Settings sets one (KGlobalAccel keeps it in
/// kglobalshortcutsrc, [services][sweeflow.desktop], and runs the action's Exec when pressed).
/// It shows there as "Sweeflow" → `k.name`.
fn plasma_action_key(k: &StreamKey, remapped: bool) -> Result<OpenKey> {
    let entry = ["--file", "kglobalshortcutsrc", "--group", "services", "--group", LAUNCHER, "--key", k.id];
    let there = Command::new("kreadconfig6").args(entry).output().context("kreadconfig6")?;
    // A key the user gave it (or "none"), or an earlier install did; not the old letter an
    // earlier version set (replaced by the new one).
    let there = String::from_utf8_lossy(&there.stdout).trim().to_string();
    let old = [plasma_key(k.old, false).0, plasma_key(k.old, true).0];
    if !there.is_empty() && !old.iter().any(|o| there.split([',', '\t']).next() == Some(o.as_str())) {
        return Ok(OpenKey::Kept);
    }
    let (_, code) = plasma_key(k.letter, remapped);
    let key = (vec![code, 0, 0, 0],);
    let bus = zbus::blocking::Connection::session()?;
    let others: Vec<ShortcutInfo> = kglobalaccel(&bus, "globalShortcutsByKey", &(&key, (0i32,)))?.body().deserialize()?;
    if let Some(i) = others.into_iter().find(|i| !(i.2 == LAUNCHER && i.0 == k.id)) {
        return Ok(OpenKey::Taken(if i.1 == i.3 { i.3 } else { format!("{} ({})", i.3, i.1) }));
    }
    // KDE's app database has to know the entry's actions first.
    let _ = Command::new("kbuildsycoca6").stdout(Stdio::null()).stderr(Stdio::null()).status();
    // A string list (as), not four strings: a Vec, which zvariant sends as an array.
    let id: Vec<&str> = vec![LAUNCHER, k.id, "Sweeflow", k.name];
    let told = || -> zbus::Result<bool> {
        kglobalaccel(&bus, "doRegister", &(&id,))?;
        kglobalaccel(&bus, "setForeignShortcutKeys", &(&id, vec![&key]))?;
        let keys: Vec<(Vec<i32>,)> = kglobalaccel(&bus, "shortcutKeys", &(&id,))?.body().deserialize()?;
        Ok(keys.iter().any(|k| k.0.first() == Some(&code)))
    };
    let now = told().map_err(|e| anyhow::anyhow!("Plasma's shortcut service: {e}"))?;
    anyhow::ensure!(now, "Plasma's shortcut service didn't take it");
    Ok(OpenKey::Set { now })
}
/// The app menu entry (see `write_launcher`), whose "launch" action earlier versions gave the
/// key.
const LAUNCHER: &str = "sweeflow.desktop";
/// The keys earlier versions gave the app menu entry.
const OLD_LAUNCH_KEYS: [&str; 2] = ["Meta+Alt+O", "Ctrl+Alt+O"];
const WIDGET: &str = "dev.sweeflow.panel";

/// A call to Plasma's shortcut service (KGlobalAccel; inside KWin on Wayland).
fn kglobalaccel<B>(bus: &zbus::blocking::Connection, method: &str, body: &B) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zvariant::DynamicType,
{
    bus.call_method(Some("org.kde.kglobalaccel"), "/kglobalaccel", Some("org.kde.KGlobalAccel"), method, body)
}

/// Runs a script in Plasma's shell (its scripting interface) and gives what it printed.
fn plasma_script(script: &str) -> Result<String> {
    let out = crate::desktop::plasma_eval(script).stderr(Stdio::null()).output().context("qdbus6")?;
    anyhow::ensure!(out.status.success(), "Plasma's shell didn't run the script");
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The Sweeflow widgets in the system trays, as (screen, applet id, its shortcut). A tray's
/// widgets are kept inside the tray's own config, under Applets/<id>.
fn plasma_widgets() -> Result<Vec<(i64, String, String)>> {
    let script = format!(
        r#"panels().forEach(function(p){{ p.widgets("org.kde.plasma.systemtray").forEach(function(w){{
        w.currentConfigGroup = ["Applets"]; var ids = w.configGroups.slice();
        for (var i = 0; i < ids.length; i++) {{ var id = String(ids[i]); w.currentConfigGroup = ["Applets", id];
            if (String(w.readConfig("plugin", "")) == "{WIDGET}") {{ w.currentConfigGroup = ["Applets", id, "Shortcuts"];
                print(p.screen + "\t" + id + "\t" + w.readConfig("global", "") + "\n"); }} }} }}); }});"#
    );
    Ok(parse_widgets(&plasma_script(&script)?))
}

fn parse_widgets(printed: &str) -> Vec<(i64, String, String)> {
    printed
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let screen = f.next()?.trim().parse().ok()?;
            let id = f.next()?.trim();
            (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit())).then(|| (screen, id.to_string(), f.next().unwrap_or("").trim().to_string()))
        })
        .collect()
}

/// The widget a single key opens: the one on the first screen, or else the first one found.
fn main_widget(widgets: &[(i64, String, String)]) -> Option<&(i64, String, String)> {
    widgets.iter().find(|w| w.0 == 0).or(widgets.first())
}

/// The shortcut Plasma gives a widget that has one ("activate widget <id>", in plasmashell's
/// shortcuts): what pressing the key does, a click on the tray icon (it opens the popup, and
/// closes it when open).
fn widget_action(id: &str) -> String {
    format!("activate widget {id}")
}

/// The Sweeflow tray widget's own shortcut, which Plasma gives to the widget's popup like a
/// click on its icon. The widget gives it to itself (Plasma takes that at once and keeps it in
/// the widget's config): the key is left for it in WIDGET_KEY, which the client passes on.
/// (Written into the config here instead, it needed Plasma's shell restarted, and the whole
/// desktop went black for a moment.)
/// Earlier versions gave the key to the app menu entry instead (`sweeflow open`): that one is
/// taken away when it still has the key they gave it.
fn plasma_open_key(_home: &std::path::Path, remapped: bool) -> Result<OpenKey> {
    // Just added to the trays (install_icon_and_widget): there in a moment.
    let mut widgets = plasma_widgets()?;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut tries = 0;
    while widgets.is_empty() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(250));
        widgets = plasma_widgets()?;
        // Told again now and then: told straight after the tray got it on its list, the shell
        // may not have taken it up yet.
        tries += 1;
        if widgets.is_empty() && tries % 4 == 0 {
            announce_widget();
        }
    }
    // Still not there (Plasma can take a while to load a new widget): the key waits for the
    // first Sweeflow widget that comes up ("*"), which gives it to itself then.
    let id = main_widget(&widgets).map_or("*", |w| w.1.as_str()).to_string();
    // One the user gave it (or an earlier install did).
    if widgets.iter().any(|w| !w.2.is_empty()) {
        return Ok(OpenKey::Kept);
    }
    let (key_text, code) = plasma_key('O', remapped);
    // A key sequence on D-Bus: up to four keys, the unused ones 0.
    let key = (vec![code, 0, 0, 0],);
    let bus = zbus::blocking::Connection::session()?;
    // 0: the shortcuts with exactly this key. Not counting the app menu entry's, ours.
    let others: Vec<ShortcutInfo> =kglobalaccel(&bus, "globalShortcutsByKey", &(&key, (0i32,)))?.body().deserialize()?;
    if let Some(i) = others.into_iter().find(|i| i.2 != LAUNCHER) {
        return Ok(OpenKey::Taken(if i.1 == i.3 { i.3 } else { format!("{} ({})", i.3, i.1) }));
    }

    let launch = ["--file", "kglobalshortcutsrc", "--group", "services", "--group", LAUNCHER, "--key", "_launch"];
    let there = Command::new("kreadconfig6").args(launch).output().context("kreadconfig6")?;
    if old_launch_keys(&String::from_utf8_lossy(&there.stdout)) {
        if let Err(e) = kglobalaccel(&bus, "unregister", &(LAUNCHER, "_launch")) {
            log::warn!("taking the key from the app menu entry: {e}");
        }
        let _ = Command::new("kwriteconfig6").args(launch).arg("--delete").status();
    }

    std::fs::create_dir_all(config::dir())?;
    std::fs::write(config::dir().join(WIDGET_KEY), format!("{id}\t{key_text}\n"))?;
    Ok(OpenKey::Set { now: true })
}

/// The key setup leaves for the tray widget to give itself (applet id, a tab, the key as Qt
/// writes it). The widget says when it has, and the client removes the file.
pub const WIDGET_KEY: &str = "widget-open-key";

/// What WIDGET_KEY holds, as (applet id, key).
pub fn widget_key() -> Option<(String, String)> {
    let text = std::fs::read_to_string(config::dir().join(WIDGET_KEY)).ok()?;
    let (id, key) = text.trim().split_once('\t')?;
    Some((id.trim().to_string(), key.trim().to_string()))
}

/// The app menu entry's shortcut (as kreadconfig6 prints it, several keys a tab apart) is the
/// one earlier versions gave it: nothing but those keys.
fn old_launch_keys(value: &str) -> bool {
    let keys: Vec<&str> = value.trim().split('\t').map(str::trim).filter(|k| !k.is_empty()).collect();
    !keys.is_empty() && keys.iter().all(|k| OLD_LAUNCH_KEYS.contains(k))
}

/// Opens the tray widget's popup the way its shortcut does, when it has one: as a click on the
/// icon would (on the widget the key belongs to). False when there is none to use.
pub fn press_widget_shortcut() -> bool {
    let Ok(widgets) = plasma_widgets() else { return false };
    let Some((_, id, _)) = main_widget(&widgets).filter(|w| !w.2.is_empty()) else { return false };
    let action = widget_action(id);
    let pressed = || -> zbus::Result<bool> {
        let bus = zbus::blocking::Connection::session()?;
        let shell = |method: &str, body: &(&str,)| {
            bus.call_method(Some("org.kde.kglobalaccel"), "/component/plasmashell", Some("org.kde.kglobalaccel.Component"), method, body)
        };
        // Plasma's shell registers the shortcut as it loads the widget: not there yet, none.
        let names: Vec<String> = shell("shortcutNames", &("default",))?.body().deserialize()?;
        if !names.contains(&action) {
            return Ok(false);
        }
        shell("invokeShortcut", &(action.as_str(),))?;
        Ok(true)
    };
    pressed().unwrap_or_else(|e| {
        log::debug!("the widget's shortcut: {e}");
        false
    })
}

const GNOME_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
/// Where Sweeflow's custom shortcut is kept (the ones made in GNOME's Settings are custom0,
/// custom1…).
const GNOME_KEY_PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/sweeflow/";
const GNOME_KEY: &str = "<Super><Alt>o";
/// The key with a Mac-style key remapper (see `mac_keys_remapped`): Cmd+Opt+O as it arrives.
const GNOME_REMAPPED_KEY: &str = "<Control><Alt>o";

fn gsettings(args: &[&str]) -> Result<String> {
    let out = Command::new("gsettings").args(args).stderr(Stdio::null()).output().context("gsettings")?;
    anyhow::ensure!(out.status.success(), "gsettings {} failed", args.join(" "));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Cmd+Opt+<letter> as GNOME writes a key: "<Super><Alt>p", or with a Mac-style key remapper
/// (see `mac_keys_remapped`) the key that arrives, "<Control><Alt>p".
fn gnome_key_text(letter: char, remapped: bool) -> String {
    format!("<{}><Alt>{}", if remapped { "Control" } else { "Super" }, letter.to_ascii_lowercase())
}

/// Where a streaming toggle's custom shortcut is kept: next to the open key's.
fn gnome_path(id: &str) -> String {
    format!("{}-{id}/", GNOME_KEY_PATH.trim_end_matches('/'))
}

fn gnome_open_key(home: &std::path::Path, remapped: bool) -> Result<OpenKey> {
    gnome_custom_key(home, GNOME_KEY_PATH, "Sweeflow", "open", if remapped { GNOME_REMAPPED_KEY } else { GNOME_KEY })
}

/// A custom shortcut as GNOME's Settings → Keyboard makes them: its name, command (`sweeflow
/// <arg>`) and key under a path of its own, and that path in the list of custom shortcuts. GNOME
/// takes it up at once.
fn gnome_custom_key(home: &std::path::Path, our_path: &str, name: &str, arg: &str, key: &str) -> Result<OpenKey> {
    let list = gsettings(&["get", GNOME_KEYS, "custom-keybindings"])?;
    let paths = quoted(&list);
    // Ours from an earlier install, whatever key the user has given it since.
    if paths.iter().any(|p| p == our_path) {
        return Ok(OpenKey::Kept);
    }
    // Who else has the key: another custom shortcut, or one of GNOME's own (every setting is
    // looked at: a key is written the same way wherever it is kept).
    for path in &paths {
        let other = format!("{GNOME_KEYS}.custom-keybinding:{path}");
        if gsettings(&["get", &other, "binding"]).is_ok_and(|b| binds(&b, key)) {
            let name = gsettings(&["get", &other, "name"]).ok().and_then(|n| quoted(&n).into_iter().next());
            return Ok(OpenKey::Taken(name.filter(|n| !n.is_empty()).unwrap_or_else(|| "another custom shortcut".into())));
        }
    }
    for line in gsettings(&["list-recursively"])?.lines() {
        // "schema key value"
        let mut words = line.splitn(3, ' ');
        if let (Some(_), Some(name), Some(value)) = (words.next(), words.next(), words.next()) {
            if binds(value, key) {
                return Ok(OpenKey::Taken(name.to_string()));
            }
        }
    }
    // The shortcut first, then its place in the list, so GNOME never sees half of it.
    let ours = format!("{GNOME_KEYS}.custom-keybinding:{our_path}");
    let command = format!("{} {arg}", command_word(&installed_binary(home).display().to_string()));
    gsettings(&["set", &ours, "name", &gvariant_str(name)])?;
    gsettings(&["set", &ours, "command", &gvariant_str(&command)])?;
    gsettings(&["set", &ours, "binding", &gvariant_str(key)])?;
    if let Some(list) = with_path(&list, our_path) {
        gsettings(&["set", GNOME_KEYS, "custom-keybindings", &list])?;
    }
    Ok(OpenKey::Set { now: true })
}

const CINNAMON_KEYS: &str = "org.cinnamon.desktop.keybindings";
/// Sweeflow's custom shortcuts on Cinnamon, by the names in its list of custom ones (the ones its
/// settings make are custom0, custom1…).
const CINNAMON_OPEN: &str = "sweeflow";

fn cinnamon_id(id: &str) -> String {
    format!("{CINNAMON_OPEN}-{id}")
}

fn cinnamon_schema(id: &str) -> String {
    format!("{CINNAMON_KEYS}.custom-keybinding:/org/cinnamon/desktop/keybindings/custom-keybindings/{id}/")
}

/// A custom shortcut as Cinnamon's System Settings → Keyboard → Shortcuts makes them: name,
/// command and keys under `id`, and `id` in its list of custom shortcuts (Cinnamon takes it up
/// at once). Only while nothing else has the key.
fn cinnamon_custom_key(home: &std::path::Path, id: &str, name: &str, arg: &str, key: &str) -> Result<OpenKey> {
    let list = gsettings(&["get", CINNAMON_KEYS, "custom-list"])?;
    let ids = quoted(&list);
    let command = format!("{} {arg}", command_word(&installed_binary(home).display().to_string()));
    // Ours from an earlier install, whatever key the user has given it since.
    if ids.iter().any(|i| i == id) {
        return Ok(OpenKey::Kept);
    }
    for other in &ids {
        if gsettings(&["get", &cinnamon_schema(other), "binding"]).is_ok_and(|b| binds(&b, key)) {
            let name = gsettings(&["get", &cinnamon_schema(other), "name"]).ok().and_then(|n| quoted(&n).into_iter().next());
            return Ok(OpenKey::Taken(name.filter(|n| !n.is_empty()).unwrap_or_else(|| "another custom shortcut".into())));
        }
    }
    for line in gsettings(&["list-recursively"])?.lines() {
        let mut words = line.splitn(3, ' ');
        if let (Some(_), Some(name), Some(value)) = (words.next(), words.next(), words.next()) {
            if binds(value, key) {
                return Ok(OpenKey::Taken(name.to_string()));
            }
        }
    }
    let ours = cinnamon_schema(id);
    gsettings(&["set", &ours, "name", &gvariant_str(name)])?;
    gsettings(&["set", &ours, "command", &gvariant_str(&command)])?;
    gsettings(&["set", &ours, "binding", &format!("[{}]", gvariant_str(key))])?;
    let mut ids = ids;
    ids.push(id.to_string());
    gsettings(&["set", CINNAMON_KEYS, "custom-list", &format!("[{}]", ids.iter().map(|i| gvariant_str(i)).collect::<Vec<_>>().join(", "))])?;
    Ok(OpenKey::Set { now: true })
}

/// GNOME's Utilities folder in the app grid is a list of apps (Ubuntu's, CachyOS's), not a
/// category: Sweeflow is put in it (or taken out), as dragging it there does; else it sat loose
/// in the grid although it's a utility.
const GNOME_UTILITIES: &str = "org.gnome.desktop.app-folders.folder:/org/gnome/desktop/app-folders/folders/Utilities/";

fn gnome_utilities_folder(add: bool) {
    let Ok(list) = gsettings(&["get", GNOME_UTILITIES, "apps"]) else { return };
    let mut apps = quoted(&list);
    // No such folder (its list empty and unnamed): left alone.
    if apps.is_empty() && gsettings(&["get", GNOME_UTILITIES, "name"]).map_or(true, |n| quoted(&n).is_empty()) {
        return;
    }
    let ours = "sweeflow.desktop".to_string();
    let has = apps.contains(&ours);
    if add == has {
        return;
    }
    if add {
        apps.push(ours);
    } else {
        apps.retain(|a| *a != ours);
    }
    let value = format!("[{}]", apps.iter().map(|a| gvariant_str(a)).collect::<Vec<_>>().join(", "));
    if gsettings(&["set", GNOME_UTILITIES, "apps", &value]).is_ok() && add {
        println!("    Sweeflow is in the app grid's Utilities folder");
    }
}

/// Each install: Sweeflow's Cinnamon shortcuts from an earlier one run this install's program
/// (install.sh's was ~/.local/bin/sweeflow, the .deb's /usr/bin/sweeflow), and the list is set
/// again, which is what makes Cinnamon read them: it went on running the old one, gone, so the
/// keys did nothing until the next login. (The keys stay as the person has them.)
/// At each start of the service too: a package upgrade (or a move from install.sh's copy to the
/// .deb's) runs no step of the user's, and Cinnamon's keys went on running the old program.
pub fn desktop_commands_now(home: &std::path::Path) {
    if std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("Cinnamon") {
        cinnamon_commands_now(home);
    }
}

fn cinnamon_commands_now(home: &std::path::Path) {
    let Ok(list) = gsettings(&["get", CINNAMON_KEYS, "custom-list"]) else { return };
    let ids = quoted(&list);
    let program = command_word(&installed_binary(home).display().to_string());
    let mut changed = false;
    for (id, arg) in [(CINNAMON_OPEN.to_string(), "open"), (cinnamon_id("toggle-pause"), "toggle-pause"), (cinnamon_id("toggle-stream"), "toggle-stream")] {
        if !ids.contains(&id) {
            continue;
        }
        let ours = cinnamon_schema(&id);
        let command = format!("{program} {arg}");
        let had = gsettings(&["get", &ours, "command"]).ok().and_then(|c| quoted(&c).into_iter().next()).unwrap_or_default();
        if had != command && gsettings(&["set", &ours, "command", &gvariant_str(&command)]).is_ok() {
            changed = true;
        }
    }
    if changed {
        // Rereading the list isn't enough: Cinnamon keeps a binding whose key didn't change,
        // with its old command. So the keys go, are read, come back, and are read again.
        let mut keys = Vec::new();
        for id in ids.iter().filter(|i| cinnamon_ours(i)) {
            let ours = cinnamon_schema(id);
            if let Ok(binding) = gsettings(&["get", &ours, "binding"]) {
                let _ = gsettings(&["set", &ours, "binding", "@as []"]);
                keys.push((ours, binding.trim().to_string()));
            }
        }
        cinnamon_reread();
        for (ours, binding) in keys {
            let _ = gsettings(&["set", &ours, "binding", &binding]);
        }
        cinnamon_reread();
        println!("    Sweeflow's Cinnamon shortcuts run {program} now");
    }
}

/// Has Cinnamon read its custom shortcuts again: it does on a change to their list only (a
/// shortcut's own settings changing isn't noticed). The list with an entry more, then as it was,
/// as Cinnamon's own settings do (an entry it skips: "__dummy__").
fn cinnamon_reread() {
    let Ok(list) = gsettings(&["get", CINNAMON_KEYS, "custom-list"]) else { return };
    let ids = quoted(&list);
    let all = |ids: &[String]| format!("[{}]", ids.iter().map(|i| gvariant_str(i)).collect::<Vec<_>>().join(", "));
    let mut more = ids.clone();
    more.push("__dummy__".into());
    let _ = gsettings(&["set", CINNAMON_KEYS, "custom-list", &all(&more)]);
    let _ = gsettings(&["set", CINNAMON_KEYS, "custom-list", &all(&ids)]);
}

/// The keys Cinnamon has now for Sweeflow's custom shortcuts (open, pause, stop), as it writes
/// them; None for one that isn't there or has no key. (For showing them.)
pub(crate) fn cinnamon_bindings() -> [Option<String>; 3] {
    let Ok(list) = gsettings(&["get", CINNAMON_KEYS, "custom-list"]) else { return Default::default() };
    let ids = quoted(&list);
    let binding = |id: &str| -> Option<String> {
        if !ids.iter().any(|i| i == id) {
            return None;
        }
        let value = gsettings(&["get", &cinnamon_schema(id), "binding"]).ok()?;
        quoted(&value).into_iter().find(|k| gnome_key(k).is_some())
    };
    [binding(CINNAMON_OPEN), binding(&cinnamon_id(STREAM_KEYS[0].id)), binding(&cinnamon_id(STREAM_KEYS[1].id))]
}

/// The keys GNOME has now for Sweeflow's custom shortcuts (open, pause, stop), as it writes
/// them ("<Super><Alt>o"); None for one that isn't there or has no key. (For showing them.)
pub(crate) fn gnome_bindings() -> [Option<String>; 3] {
    let Ok(list) = gsettings(&["get", GNOME_KEYS, "custom-keybindings"]) else { return Default::default() };
    let paths = quoted(&list);
    let binding = |path: &str| -> Option<String> {
        if !paths.iter().any(|p| p == path) {
            return None;
        }
        let value = gsettings(&["get", &format!("{GNOME_KEYS}.custom-keybinding:{path}"), "binding"]).ok()?;
        quoted(&value).into_iter().find(|k| gnome_key(k).is_some())
    };
    [binding(GNOME_KEY_PATH), binding(&gnome_path(STREAM_KEYS[0].id)), binding(&gnome_path(STREAM_KEYS[1].id))]
}

/// The keys Plasma has now (KGlobalAccel's) for opening Sweeflow (the tray widget's shortcut)
/// and for the two streaming toggles. None for one with no key. (For showing them.)
pub(crate) fn plasma_bindings() -> [Option<crate::keys::Key>; 3] {
    let Ok(bus) = zbus::blocking::Connection::session() else { return Default::default() };
    let keys_of = |component: &str, action: &str| -> Option<crate::keys::Key> {
        let id: Vec<&str> = vec![component, action, "", ""];
        let keys: Vec<(Vec<i32>,)> = kglobalaccel(&bus, "shortcutKeys", &(&id,)).ok()?.body().deserialize().ok()?;
        keys.into_iter().find_map(|k| k.0.first().copied().filter(|&c| c != 0)).and_then(crate::keys::Key::from_qt)
    };
    // The widgets come from a script in Plasma's shell: asked at most every 5 minutes (each
    // script is work for plasmashell, and this is read every few seconds).
    static WIDGETS: std::sync::Mutex<Option<(std::time::Instant, Vec<(i64, String, String)>)>> = std::sync::Mutex::new(None);
    let widgets = {
        let mut cache = WIDGETS.lock().unwrap();
        if !cache.as_ref().is_some_and(|c| c.0.elapsed() < std::time::Duration::from_secs(300)) {
            // Only a read that found the widget is kept: an empty one (the shell busy or
            // restarting) is asked again next time.
            match plasma_widgets() {
                Ok(found) if !found.is_empty() => *cache = Some((std::time::Instant::now(), found)),
                _ => {}
            }
        }
        cache.as_ref().map(|c| c.1.clone()).unwrap_or_default()
    };
    let open = main_widget(&widgets).and_then(|(_, id, text)| {
        // Its config's key as the fallback, for a shell that hasn't registered it yet.
        keys_of("plasmashell", &widget_action(id)).or_else(|| crate::keys::Key::from_qt_text(text))
    });
    [open, keys_of(LAUNCHER, STREAM_KEYS[0].id), keys_of(LAUNCHER, STREAM_KEYS[1].id)]
}

/// The strings in a value as gsettings prints it: `['a', 'b']`, `'a'`, or `@as []` (none).
fn quoted(value: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\'' && c != '"' {
            continue;
        }
        let mut text = String::new();
        while let Some(d) = chars.next() {
            match d {
                '\\' => text.extend(chars.next()),
                d if d == c => break,
                d => text.push(d),
            }
        }
        found.push(text);
    }
    found
}

/// A string as gsettings takes it.
fn gvariant_str(text: &str) -> String {
    format!("'{}'", text.replace('\\', r"\\").replace('\'', r"\'"))
}

/// The list of custom shortcuts with ours added at the end, the others as they were. None when
/// it is in the list already.
fn with_path(list: &str, path: &str) -> Option<String> {
    let mut paths = quoted(list);
    if paths.iter().any(|p| p == path) {
        return None;
    }
    paths.push(path.to_string());
    Some(format!("[{}]", paths.iter().map(|p| gvariant_str(p)).collect::<Vec<_>>().join(", ")))
}

/// A GNOME key ("<Super><Alt>o") as its modifiers in a fixed order and its key, however it was
/// written: any order, any case, Mod4 for Super, Mod1 for Alt, Primary for Control. None for
/// text that isn't a key.
pub(crate) fn gnome_key(text: &str) -> Option<(Vec<String>, String)> {
    let mut mods = Vec::new();
    let mut rest = text.trim();
    while let Some(after) = rest.strip_prefix('<') {
        let (name, after) = after.split_once('>')?;
        mods.push(match name.to_lowercase().as_str() {
            "mod4" => "super".to_string(),
            "mod1" => "alt".to_string(),
            "primary" | "control" | "ctl" => "ctrl".to_string(),
            name => name.to_string(),
        });
        rest = after;
    }
    if rest.is_empty() || rest.contains(|c: char| c.is_whitespace() || c == '<' || c == '>') {
        return None;
    }
    mods.sort();
    mods.dedup();
    Some((mods, rest.to_lowercase()))
}

/// The value (as gsettings prints it) has this key among its strings.
fn binds(value: &str, key: &str) -> bool {
    let key = gnome_key(key);
    key.is_some() && quoted(value).iter().any(|k| gnome_key(k) == key)
}

/// The program's path as one word of a command line (GNOME splits a shortcut's command the way
/// a shell does): as it is when it has nothing a shell would trip on, in single quotes otherwise.
fn command_word(path: &str) -> String {
    if !path.is_empty() && path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_a_pc_keyboard_from_an_apple_one() {
        let dev = |names: &[&str]| names.iter().map(|n| format!("I: Bus=0003\nN: Name=\"{n}\"\nH: Handlers=kbd\n\n")).collect::<String>();
        assert!(!pc_keyboards_only(&dev(&["Apple Inc. Magic Keyboard with Touch ID", "XWayKeyz (virtual) Keyboard", "libvirtualhid Keyboard"])));
        assert!(pc_keyboards_only(&dev(&["AT Translated Set 2 keyboard", "XWayKeyz (virtual) Keyboard"])));
        assert!(!pc_keyboards_only(&dev(&["AT Translated Set 2 keyboard", "Magic Keyboard"])));
        // Nothing to go by: as before (Apple).
        assert!(!pc_keyboards_only(&dev(&["Power Button", "mac-trackpad-deskflow-bridge keyboard"])));
        assert_eq!(toshy_override(b"..\x1doverride_kbtypeAuto-Adapt.."), None);
        assert_eq!(toshy_override(b"override_kbtype\x00..\x1doverride_kbtypeWindows.."), Some("Windows"));
    }

    #[test]
    fn reads_the_strings_gsettings_prints() {
        assert!(quoted("@as []").is_empty());
        assert!(quoted("[]\n").is_empty());
        assert_eq!(quoted("'<Super><Alt>o'\n"), ["<Super><Alt>o"]);
        assert_eq!(quoted("['/a/custom0/', '/a/custom1/']"), ["/a/custom0/", "/a/custom1/"]);
        // A name with a quote in it: printed in the other quotes, or escaped.
        assert_eq!(quoted(r#""Sam's terminal""#), ["Sam's terminal"]);
        assert_eq!(quoted(r"['it\'s', 'a\\b']"), ["it's", r"a\b"]);
        assert_eq!(quoted("uint32 5"), Vec::<String>::new());
    }

    #[test]
    fn our_path_joins_the_custom_shortcuts_once() {
        let ours = GNOME_KEY_PATH;
        assert_eq!(with_path("@as []", ours), Some(format!("['{ours}']")));
        let others = "['/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/', \
                      '/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom1/']";
        let added = with_path(others, ours).unwrap();
        // The others stay, in their order, ours after them.
        assert_eq!(added, format!("{}, '{ours}']", others.trim_end_matches(']')));
        assert_eq!(quoted(&added).len(), 3);
        // A second install finds it there and writes nothing.
        assert_eq!(with_path(&added, ours), None);
        assert_eq!(with_path(&format!("['{ours}']"), ours), None);
    }

    #[test]
    fn uninstall_takes_only_ours_out_of_the_custom_shortcuts() {
        let folder = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings";
        let list = format!(
            "['{folder}/custom0/', '{GNOME_KEY_PATH}', '{}', '{folder}/custom1/', '{}']",
            gnome_path("toggle-pause"),
            gnome_path("toggle-stream")
        );
        // The user's stay, in their order.
        assert_eq!(without_ours(&list, gnome_ours), Some(format!("['{folder}/custom0/', '{folder}/custom1/']")));
        assert_eq!(without_ours(&format!("['{GNOME_KEY_PATH}']"), gnome_ours).as_deref(), Some("@as []"));
        // Nothing of ours: nothing to write.
        assert_eq!(without_ours(&format!("['{folder}/custom0/']"), gnome_ours), None);
        assert_eq!(without_ours("@as []", gnome_ours), None);
        // Only Sweeflow's own paths, not ones that merely start alike.
        assert!(!gnome_ours(&format!("{folder}/sweeflowish/")));
        assert!(!gnome_ours("/elsewhere/sweeflow/"));
        assert!(!gnome_ours(&format!("{folder}/sweeflow-x/deeper/")));
        // Cinnamon's list, by name; a name with a quote is written back as it was read.
        let cinnamon = r"['custom0', 'sweeflow', 'sweeflow-toggle-pause', 'it\'s mine', 'sweeflow-toggle-stream']";
        assert_eq!(without_ours(cinnamon, cinnamon_ours).as_deref(), Some(r"['custom0', 'it\'s mine']"));
        assert!(cinnamon_ours(&cinnamon_id("toggle-pause")) && !cinnamon_ours("sweeflowish"));
    }

    #[test]
    fn uninstall_knows_its_own_files() {
        for ours in ["sweeflow.desktop", "sweeflow-display.desktop", "sweeflow-com.apple.mail.desktop", "sweeflow-menu-com.apple.mail.desktop", ".sweeflow-com.apple.mail.desktop.42.tmp"] {
            assert!(is_our_entry(ours), "{ours}");
        }
        for theirs in ["sweeflowy.desktop", "org.kde.dolphin.desktop", "sweeflow-notes.txt", "mimeapps.list"] {
            assert!(!is_our_entry(theirs), "{theirs}");
        }
        for note in ["open-key.plasma-widget", "open-key.kde", "open-key.gnome", "open-key.cinnamon", "toggle-pause-key-y.plasma", "toggle-stream-key-x.gnome"] {
            assert!(is_key_note(note), "{note}");
        }
        // The pairing and the settings stay.
        for kept in ["config.toml", "client.crt", "mac-address", "pinned", "apps.json", "global-menu-panel"] {
            assert!(!is_key_note(kept), "{kept}");
        }
        let home = std::path::Path::new("/home/me");
        assert_eq!(tilde(home, &home.join(".local/bin/sweeflow")), "~/.local/bin/sweeflow");
        assert_eq!(tilde(home, std::path::Path::new("/xdg/data/applications")), "/xdg/data/applications");
    }

    #[test]
    fn strings_are_written_as_gsettings_reads_them() {
        assert_eq!(gvariant_str("Sweeflow"), "'Sweeflow'");
        assert_eq!(gvariant_str("/home/me/.local/bin/sweeflow open"), "'/home/me/.local/bin/sweeflow open'");
        assert_eq!(gvariant_str(r"it's a\b"), r"'it\'s a\\b'");
        // And read back the same.
        let odd = r"'/home/o'\''brien/bin/sweeflow' open";
        assert_eq!(quoted(&gvariant_str(odd)), [odd]);
    }

    #[test]
    fn the_same_key_however_it_is_written() {
        assert!(binds("['<Super><Alt>o']", GNOME_KEY));
        assert!(binds("'<Alt><Super>O'", GNOME_KEY));
        assert!(binds("['<Super>Up', '<Mod1><Mod4>o']", GNOME_KEY));
        assert!(binds("['<Primary><Shift>Escape']", "<Control><Shift>escape"));
        // Other keys, other modifiers, and settings that aren't keys.
        assert!(!binds("['<Super>o']", GNOME_KEY));
        assert!(!binds("['<Super><Alt>p']", GNOME_KEY));
        assert!(!binds("['<Super><Alt><Shift>o']", GNOME_KEY));
        assert!(!binds("@as []", GNOME_KEY));
        assert!(!binds("'o'", GNOME_KEY));
        assert!(!binds("'Open <Super><Alt>o here'", GNOME_KEY));
        assert!(!binds("true", GNOME_KEY));
        assert!(!binds("['']", GNOME_KEY));
        assert_eq!(gnome_key("<Alt><Super>O"), Some((vec!["alt".to_string(), "super".to_string()], "o".to_string())));
        assert_eq!(gnome_key("<Super>"), None);
        assert_eq!(gnome_key("<Super"), None);
    }

    #[test]
    fn finds_the_tray_widgets_plasma_printed() {
        let printed = "1\t80\t\n0\t55\tCtrl+Alt+O\nnot a line\n0\tx1\t\n";
        let widgets = parse_widgets(printed);
        assert_eq!(widgets, [(1, "80".to_string(), String::new()), (0, "55".to_string(), "Ctrl+Alt+O".to_string())]);
        // The first screen's, wherever it was listed.
        assert_eq!(main_widget(&widgets).unwrap().1, "55");
        assert_eq!(main_widget(&widgets[..1]).unwrap().1, "80");
        assert!(main_widget(&[]).is_none());
        assert_eq!(widget_action("55"), "activate widget 55");
    }

    #[test]
    fn the_app_menu_entry_keeps_a_key_the_user_gave_it() {
        assert!(old_launch_keys("Meta+Alt+O\n"));
        assert!(old_launch_keys("Ctrl+Alt+O\tMeta+Alt+O\n"));
        assert!(!old_launch_keys(""));
        assert!(!old_launch_keys("none"));
        assert!(!old_launch_keys("Meta+Alt+S"));
        assert!(!old_launch_keys("Meta+Alt+O\tMeta+S"));
    }

    #[test]
    fn plasma_keys_as_qt_counts_them() {
        assert_eq!(plasma_key('O', false), ("Meta+Alt+O".to_string(), 0x1800_004f));
        assert_eq!(plasma_key('O', true), ("Ctrl+Alt+O".to_string(), 0x0c00_004f));
        assert_eq!(plasma_key('P', false), ("Meta+Alt+P".to_string(), 0x1800_0050));
        assert_eq!(plasma_key('x', false), ("Meta+Alt+X".to_string(), 0x1800_0058));
        assert_eq!(plasma_key('X', true), ("Ctrl+Alt+X".to_string(), 0x0c00_0058));
        assert!(binds("['<Primary><Alt>o']", GNOME_REMAPPED_KEY));
    }

    #[test]
    fn the_streaming_keys_as_each_desktop_writes_them() {
        assert_eq!(gnome_key_text('O', false), GNOME_KEY);
        assert_eq!(gnome_key_text('O', true), GNOME_REMAPPED_KEY);
        assert_eq!(gnome_key_text('P', false), "<Super><Alt>p");
        assert_eq!(gnome_key_text('X', true), "<Control><Alt>x");
        assert!(binds("['<Mod4><Mod1>X']", &gnome_key_text('X', false)));
        assert!(!binds("['<Super><Alt>o']", &gnome_key_text('P', false)));
        assert_eq!(shown_key('P', false), "Super+Alt+P");
        assert_eq!(shown_key('X', true), "Ctrl+Alt+X");
        // Their own paths, next to the open key's, each a different one.
        assert_eq!(gnome_path("toggle-pause"), "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/sweeflow-toggle-pause/");
        assert_ne!(gnome_path("toggle-stream"), gnome_path("toggle-pause"));
        let keys: Vec<(&str, char, &str)> = STREAM_KEYS.iter().map(|k| (k.id, k.letter, k.name)).collect();
        assert_eq!(keys, [("toggle-pause", 'Y', "Pause or resume streaming"), ("toggle-stream", 'Z', "Stop or start streaming")]);
    }

    #[test]
    fn the_app_menu_entry_has_the_toggles_as_actions() {
        let entry = launcher_entry("/home/me/.local/bin/sweeflow");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("\nExec=/home/me/.local/bin/sweeflow open\n"));
        assert!(entry.contains("\nActions=toggle-pause;toggle-stream;\n"));
        assert!(entry.contains(
            "\n[Desktop Action toggle-pause]\nName=Pause or resume streaming\nExec=/home/me/.local/bin/sweeflow toggle-pause\nIcon=media-playback-pause\n"
        ));
        assert!(entry.contains("\n[Desktop Action toggle-stream]\nName=Stop or start streaming\nExec=/home/me/.local/bin/sweeflow toggle-stream\n"));
        // The main entry's keys all come before the first action's group.
        let main = entry.split("\n[Desktop Action").next().unwrap();
        assert!(main.contains("Keywords=") && main.contains("Actions="));
    }

    #[test]
    fn the_command_is_one_word_and_open() {
        assert_eq!(command_word("/home/me/.local/bin/sweeflow"), "/home/me/.local/bin/sweeflow");
        assert_eq!(command_word("/home/my name/.local/bin/sweeflow"), "'/home/my name/.local/bin/sweeflow'");
        assert_eq!(command_word("/home/o'brien/bin/sweeflow"), r"'/home/o'\''brien/bin/sweeflow'");
    }
}
