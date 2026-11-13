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
    let script = format!(
        "set -e; d={SUPPORT}; mkdir -p \"$d\"; touch \"$d/clients\"; \
         if grep -q '^{client_fp}' \"$d/clients\"; then echo known; else echo '{client_fp}' >> \"$d/clients\"; echo new; fi; \
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
    cfg.add_mac(config::PairedMac { name, service: String::new(), host: format!("{host}:44120"), fingerprint: host_fp }, true);
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
    // Before the service starts: a bar that looks an icon up before its file is there (Omarchy's)
    // keeps showing a placeholder.
    write_icons(&home)?;
    run(&["restart", "sweeflow.service"])?;
    install_icon_and_widget(&home)?;
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
    ("dev.sweeflow.panel/contents/ui/MacRow.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/MacRow.qml")),
    ("dev.sweeflow.panel/contents/icons/sweeflow-tray.svg", LOGO),
    ("dev.sweeflow.panel/contents/icons/sweeflow-app.svg", APP_ICON),
    ("dev.sweeflow.panel/contents/icons/dock.svg", include_str!("../plasmoid/dev.sweeflow.panel/contents/icons/dock.svg")),
];

/// Sweeflow in the app menu: opens it (see `open`). Also how desktops match Sweeflow's own
/// windows (the wizard, app id "sweeflow", by the file's name) to the logo. No StartupWMClass:
/// with it, Plasma's task manager takes every window of the process (the Mac apps too) for
/// Sweeflow and shows them all with its logo.
pub fn write_launcher(home: &std::path::Path) -> Result<()> {
    let apps = home.join(".local/share/applications");
    std::fs::create_dir_all(&apps)?;
    let file = apps.join("sweeflow.desktop");
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Sweeflow\nGenericName=Mac apps\nComment=Mac apps on this desktop\n\
         Exec={} open\nIcon=sweeflow\nTerminal=false\nStartupNotify=false\n\
         Categories=Network;RemoteAccess;\nKeywords=mac;macos;stream;\n",
        home.join(".local/bin/sweeflow").display()
    );
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

/// The icons, into the user's icon theme folder: the logo ("sweeflow") and the tray's
/// ("sweeflow-symbolic").
fn write_icons(home: &std::path::Path) -> Result<()> {
    let icons = home.join(".local/share/icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&icons)?;
    std::fs::write(icons.join("sweeflow.svg"), APP_ICON)?;
    let symbolic = home.join(".local/share/icons/hicolor/symbolic/apps");
    std::fs::create_dir_all(&symbolic)?;
    std::fs::write(symbolic.join("sweeflow-symbolic.svg"), TRAY_ICON)?;
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
