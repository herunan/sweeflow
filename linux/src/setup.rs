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
// The Mac app's installer, if one was built before this binary (see build.rs).
include!(concat!(env!("OUT_DIR"), "/mac_dmg.rs"));

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

/// Installs this binary to ~/.local/bin and a systemd user service that runs it with the
/// desktop session (restarting it if it ever exits).
/// `sweeflow install` has put the user service in place.
pub fn service_installed() -> bool {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .is_some_and(|h| h.join(".config/systemd/user/sweeflow.service").exists())
}

pub fn install() -> Result<()> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    let bin_dir = home.join(".local/bin");
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&bin_dir)?;
    std::fs::create_dir_all(&unit_dir)?;
    let target = bin_dir.join("sweeflow");
    let me = std::env::current_exe()?;
    if me != target {
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
    register_open_key(&home);
    register_stream_keys(&home);
    install_mac_dmg(&home)?;
    println!("==> sweeflow installed to {} and running as a user service", target.display());
    println!("    logs: journalctl --user -u sweeflow -f");
    Ok(())
}

/// Puts the Mac app's installer where the "Link a Mac" window serves it from (installer.rs).
fn install_mac_dmg(home: &std::path::Path) -> Result<()> {
    let Some(dmg) = MAC_DMG else {
        println!("    (no Mac installer in this build: run ./release-mac.sh first to include one)");
        return Ok(());
    };
    let dir = home.join(".local/share/sweeflow");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("Sweeflow.dmg");
    if std::fs::read(&path).ok().as_deref() != Some(dmg) {
        std::fs::write(&path, dmg)?;
    }
    println!("    Mac installer ready for \"Link a Mac\" ({} MB)", dmg.len() / 1_000_000);
    Ok(())
}

pub fn status() -> Result<()> {
    println!("config:       {}", Config::path().display());
    match Config::load() {
        Ok(c) => {
            println!("host:         {}", c.host);
            println!("host cert:    {}", c.host_fingerprint);
            println!("scale:        {}", c.scale);
            println!("capture:      {}", if c.capture_scale == 0.0 { "native (Retina)".into() } else { format!("{}x", c.capture_scale) });
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
    let entry = launcher_entry(&home.join(".local/bin/sweeflow").display().to_string());
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
         Categories=Network;RemoteAccess;\nKeywords=mac;macos;stream;\nActions={ids}\n{actions}"
    )
}

/// `sweeflow toggle-pause` / `toggle-stream`, what the shortcuts run: the running client's
/// toggle (tray::panel_do "pause-toggle" / "stream-toggle").
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
        // The service opens "Link a Mac" itself when there's no Mac yet.
        install()?;
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
        std::fs::write(symbolic.join(format!("{}.svg", crate::tray::tray_icon_of(name))), tray_icon(colour, hollow))?;
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
    let mut updated = false;
    for (path, contents) in WIDGET_FILES {
        let file = plasmoids.join(path);
        std::fs::create_dir_all(file.parent().context("widget path")?)?;
        match std::fs::read_to_string(&file) {
            Ok(old) if old == *contents => continue,
            Ok(_) => updated = true,
            Err(_) => {}
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
    let ok = std::process::Command::new("qdbus6")
        .args(["org.kde.plasmashell", "/PlasmaShell", "org.kde.PlasmaShell.evaluateScript", script])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        println!("    (couldn't add the Sweeflow widget to the system tray: add it from the tray's settings)");
    }
    Ok(())
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
    } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
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
    let key = if remapped { format!("Cmd+Opt+{letter} ({key} once your key remapper has turned Cmd into Ctrl)") } else { key.to_string() };
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

/// A Mac-style key remapper is installed (Toshy, Kinto): outside the Mac windows (which it
/// leaves alone, as a remote desktop) it turns Cmd into Ctrl, so the desktop sees Cmd+Opt+O as
/// Ctrl+Alt+O, and a shortcut on Super+Alt+O never fires. The shortcut is then put on the key
/// that arrives.
pub fn mac_keys_remapped() -> bool {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    config.is_some_and(|c| c.join("toshy/toshy_config.py").exists() || c.join("kinto/kinto.py").exists())
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
    let out = Command::new("qdbus6")
        .args(["org.kde.plasmashell", "/PlasmaShell", "org.kde.PlasmaShell.evaluateScript", script])
        .stderr(Stdio::null())
        .output()
        .context("qdbus6")?;
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
/// click on its icon. Kept in the widget's config ([Shortcuts] global), which Plasma's shell
/// reads as it starts: the shell is restarted for it (the panels are gone for a second).
/// Earlier versions gave the key to the app menu entry instead (`sweeflow open`): that one is
/// taken away when it still has the key they gave it.
fn plasma_open_key(_home: &std::path::Path, remapped: bool) -> Result<OpenKey> {
    let widgets = plasma_widgets()?;
    let Some((_, id, _)) = main_widget(&widgets) else {
        bail!("the Sweeflow widget isn't in a system tray yet");
    };
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

    let script = format!(
        r#"panels().forEach(function(p){{ p.widgets("org.kde.plasma.systemtray").forEach(function(w){{
        w.currentConfigGroup = ["Applets", "{id}"]; if (String(w.readConfig("plugin", "")) != "{WIDGET}") return;
        w.currentConfigGroup = ["Applets", "{id}", "Shortcuts"]; w.writeConfig("global", "{key_text}"); print("ok"); }}); }});"#
    );
    anyhow::ensure!(plasma_script(&script)?.contains("ok"), "Plasma's shell didn't take the widget's shortcut");
    // The shell writes its config a moment later; it must be on disk when it restarts.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let restarted = Command::new("systemctl")
        .args(["--user", "restart", "plasma-plasmashell.service"])
        .status()
        .is_ok_and(|s| s.success());
    if restarted {
        println!("    (Plasma's panels restarted to take the Sweeflow widget's shortcut)");
    }
    Ok(OpenKey::Set { now: restarted })
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
    let command = format!("{} {arg}", command_word(&home.join(".local/bin/sweeflow").display().to_string()));
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
    let command = format!("{} {arg}", command_word(&home.join(".local/bin/sweeflow").display().to_string()));
    gsettings(&["set", &ours, "name", &gvariant_str(name)])?;
    gsettings(&["set", &ours, "command", &gvariant_str(&command)])?;
    gsettings(&["set", &ours, "binding", &format!("[{}]", gvariant_str(key))])?;
    let mut ids = ids;
    ids.push(id.to_string());
    gsettings(&["set", CINNAMON_KEYS, "custom-list", &format!("[{}]", ids.iter().map(|i| gvariant_str(i)).collect::<Vec<_>>().join(", "))])?;
    Ok(OpenKey::Set { now: true })
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
