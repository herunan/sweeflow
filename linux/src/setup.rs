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
    let script = format!(
        "set -e; d={SUPPORT}; mkdir -p \"$d\"; touch \"$d/clients\"; \
         grep -qx '{client_fp}' \"$d/clients\" || echo '{client_fp}' >> \"$d/clients\"; \
         for i in $(seq 1 20); do [ -s \"$d/fingerprint\" ] && break; sleep 0.5; done; cat \"$d/fingerprint\"\n"
    );
    let host_fp = ssh(target, &script)?.trim().to_string();
    if host_fp.len() != 64 {
        bail!("could not read the Mac's certificate fingerprint (is Sweeflow running? see ~/Library/Logs/sweeflow.log on the Mac)");
    }

    let mut cfg = Config::load_or_default()?;
    let name = host.trim_end_matches(".local").to_string();
    cfg.add_mac(config::PairedMac { name, service: String::new(), host: format!("{host}:44120"), fingerprint: host_fp }, true);
    cfg.save()?;
    println!("==> paired. Config written to {}", Config::path().display());
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
    run(&["restart", "sweeflow.service"])?;
    install_icon_and_widget(&home)?;
    println!("==> sweeflow installed to {} and running as a user service", target.display());
    println!("    logs: journalctl --user -u sweeflow -f");
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

const LOGO: &str = include_str!("../../assets/sweeflow.svg");
/// The Plasma widgets' files (published path inside ~/.local/share/plasma/plasmoids, contents),
/// built in so the binary installs them.
const WIDGET_FILES: &[(&str, &str)] = &[
    ("dev.sweeflow.panel/metadata.json", include_str!("../plasmoid/dev.sweeflow.panel/metadata.json")),
    ("dev.sweeflow.panel/contents/ui/main.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/main.qml")),
    ("dev.sweeflow.panel/contents/ui/ToggleRow.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/ToggleRow.qml")),
    ("dev.sweeflow.panel/contents/ui/MacRow.qml", include_str!("../plasmoid/dev.sweeflow.panel/contents/ui/MacRow.qml")),
    ("dev.sweeflow.panel/contents/icons/sweeflow.svg", LOGO),
    ("dev.sweeflow.panel/contents/icons/dock.svg", include_str!("../plasmoid/dev.sweeflow.panel/contents/icons/dock.svg")),
    ("dev.sweeflow.dock/metadata.json", include_str!("../plasmoid/dev.sweeflow.dock/metadata.json")),
    ("dev.sweeflow.dock/contents/ui/main.qml", include_str!("../plasmoid/dev.sweeflow.dock/contents/ui/main.qml")),
];

/// The logo (icon theme name "sweeflow"), and on Plasma the tray widget (linux/plasmoid), added
/// to every system tray. Plasma keeps its own copy of the panel config, so the tray is changed
/// through its scripting interface rather than the file.
fn install_icon_and_widget(home: &std::path::Path) -> Result<()> {
    let icons = home.join(".local/share/icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&icons)?;
    std::fs::write(icons.join("sweeflow.svg"), LOGO)?;
    // Hidden: only so desktops can match Sweeflow's own windows (the wizard, app id
    // "sweeflow") to their icon. Not a launcher: the client runs as a service.
    let apps = home.join(".local/share/applications");
    std::fs::create_dir_all(&apps)?;
    std::fs::write(
        apps.join("sweeflow.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Sweeflow\nComment=Add a Mac\nExec={} pair\nIcon=sweeflow\nNoDisplay=true\n",
            home.join(".local/bin/sweeflow").display()
        ),
    )?;
    if !std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("KDE") {
        return Ok(());
    }
    let plasmoids = home.join(".local/share/plasma/plasmoids");
    for (path, contents) in WIDGET_FILES {
        let file = plasmoids.join(path);
        std::fs::create_dir_all(file.parent().context("widget path")?)?;
        std::fs::write(file, contents)?;
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
