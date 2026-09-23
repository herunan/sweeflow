//! `sweeflow setup user@mac`: installs the Mac host over SSH and pairs the two machines.
//! SSH already authenticates both ends, so it doubles as the pairing channel: our client
//! certificate fingerprint goes to the Mac's allow-list and the Mac's fingerprint comes back.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};

use crate::config::{self, Config};

/// The Mac host's sources, embedded so the Linux binary is all you need.
const MAC_FILES: &[(&str, &str)] = &[
    ("Package.swift", include_str!("../../mac/Package.swift")),
    ("install.sh", include_str!("../../mac/install.sh")),
    ("Sources/CPrivate/include/CPrivate.h", include_str!("../../mac/Sources/CPrivate/include/CPrivate.h")),
    ("Sources/CPrivate/shim.m", include_str!("../../mac/Sources/CPrivate/shim.m")),
    ("Sources/SweeflowHost/main.swift", include_str!("../../mac/Sources/SweeflowHost/main.swift")),
    ("Sources/SweeflowHost/Wire.swift", include_str!("../../mac/Sources/SweeflowHost/Wire.swift")),
    ("Sources/SweeflowHost/Server.swift", include_str!("../../mac/Sources/SweeflowHost/Server.swift")),
    ("Sources/SweeflowHost/Session.swift", include_str!("../../mac/Sources/SweeflowHost/Session.swift")),
    ("Sources/SweeflowHost/WindowTracker.swift", include_str!("../../mac/Sources/SweeflowHost/WindowTracker.swift")),
    ("Sources/SweeflowHost/WindowStreamer.swift", include_str!("../../mac/Sources/SweeflowHost/WindowStreamer.swift")),
    ("Sources/SweeflowHost/WindowControl.swift", include_str!("../../mac/Sources/SweeflowHost/WindowControl.swift")),
    ("Sources/SweeflowHost/Input.swift", include_str!("../../mac/Sources/SweeflowHost/Input.swift")),
];

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
        append_tar(&mut tar, path, content.as_bytes(), if path.ends_with(".sh") { 0o755 } else { 0o644 });
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

    let cfg = Config {
        host: format!("{host}:44120"),
        host_fingerprint: host_fp,
        scale: Config::load().map(|c| c.scale).unwrap_or(1.0),
        capture_scale: Config::load().map(|c| c.capture_scale).unwrap_or(0.0),
        render_node: config::default_render_node(),
    };
    cfg.save()?;
    println!("==> paired. Config written to {}", Config::path().display());
    println!();
    println!("On the Mac, allow Sweeflow in System Settings → Privacy & Security under both");
    println!("  • Screen & System Audio Recording");
    println!("  • Accessibility");
    println!("Then run `sweeflow` here.");
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
