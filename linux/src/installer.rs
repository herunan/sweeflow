//! Hands the Mac its installer: while the "Link a Mac" wizard is open, Sweeflow.dmg is served on
//! the local network, so the Mac only needs Safari and an address (no USB stick, no Terminal).

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
};

use anyhow::{Context, Result};

pub const PORT: u16 = 44130;

pub struct Server {
    /// What to type into Safari on the Mac.
    pub url: String,
    /// The same page over Tailscale, when this computer is on a tailnet: a Mac that isn't on
    /// this network can't reach the address above.
    pub tailscale_url: Option<String>,
}

/// Where a packaged or locally built Sweeflow.dmg lives.
fn find_dmg() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("SWEEFLOW_DMG") {
        candidates.push(p.into());
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/share/sweeflow/Sweeflow.dmg"));
    }
    candidates.push("/usr/share/sweeflow/Sweeflow.dmg".into());
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("Sweeflow.dmg"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// This computer's address on the local network (the one the default route goes out of).
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // nothing is sent: this only picks the route
    Some(s.local_addr().ok()?.ip())
}

/// This computer's Tailscale address, when it has one (Tailscale hands them out of 100.64.0.0/10).
pub fn tailscale_ip() -> Option<std::net::Ipv4Addr> {
    let out = std::process::Command::new("ip").args(["-o", "-4", "addr", "show"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let ip: std::net::Ipv4Addr = l.split_whitespace().nth(3)?.split('/').next()?.parse().ok()?;
        (ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64).then_some(ip)
    })
}

/// Starts serving; stops when the process exits.
pub fn serve() -> Result<Server> {
    let dmg = find_dmg().context("no Sweeflow.dmg found")?;
    let ip = lan_ip().context("not on a network")?;
    let listener = TcpListener::bind(("0.0.0.0", PORT)).with_context(|| format!("port {PORT} is busy"))?;
    log::info!("serving {} at http://{ip}:{PORT}", dmg.display());
    std::thread::Builder::new().name("installer".into()).spawn(move || {
        for conn in listener.incoming().flatten() {
            let dmg = dmg.clone();
            std::thread::spawn(move || {
                if let Err(e) = answer(conn, &dmg) {
                    log::debug!("installer request: {e:#}");
                }
            });
        }
    })?;
    Ok(Server { url: format!("http://{ip}:{PORT}"), tailscale_url: tailscale_ip().map(|ip| format!("http://{ip}:{PORT}")) })
}

fn answer(mut conn: TcpStream, dmg: &PathBuf) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    // Skip the headers.
    let mut line = String::new();
    while reader.read_line(&mut line)? > 2 {
        line.clear();
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    if path == "/Sweeflow.dmg" {
        let data = std::fs::read(dmg)?;
        write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-apple-diskimage\r\nContent-Length: {}\r\n\
             Content-Disposition: attachment; filename=\"Sweeflow.dmg\"\r\nConnection: close\r\n\r\n",
            data.len()
        )?;
        conn.write_all(&data)?;
    } else {
        let page = PAGE.replace("{ICON}", ICON);
        let page = page.as_bytes();
        write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            page.len()
        )?;
        conn.write_all(page)?;
    }
    Ok(())
}

/// The app icon, shown on the page.
const ICON: &str = include_str!("../../assets/sweeflow-app.svg");

const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Install Sweeflow</title>
<style>
  :root { --bg: #f5f5f7; --card: #fff; --text: #1d1d1f; --muted: #6e6e73; --line: #e5e5ea; --accent: #7c3aed; }
  @media (prefers-color-scheme: dark) { :root { --bg: #161618; --card: #232326; --text: #f5f5f7; --muted: #a1a1a6; --line: #333338; } }
  * { box-sizing: border-box; }
  body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--text);
         font: 15px/1.5 -apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, sans-serif; }
  main { width: min(520px, 92vw); padding: 48px 0; text-align: center; }
  .icon svg { width: 112px; height: 112px; filter: drop-shadow(0 10px 24px rgba(109, 40, 217, .35)); }
  h1 { font-size: 30px; font-weight: 700; letter-spacing: -.02em; margin: 18px 0 6px; }
  .lead { color: var(--muted); margin: 0 0 28px; }
  a.button { display: inline-flex; align-items: center; gap: 8px; padding: 13px 26px; border-radius: 12px;
             background: linear-gradient(135deg, #a855f7, #6d28d9); color: #fff; text-decoration: none;
             font-weight: 600; font-size: 16px; box-shadow: 0 6px 18px rgba(109, 40, 217, .35); }
  a.button:hover { filter: brightness(1.08); }
  .size { display: block; color: var(--muted); font-size: 12px; margin-top: 8px; }
  ol { list-style: none; counter-reset: step; padding: 0; margin: 34px 0 0; text-align: left;
       background: var(--card); border-radius: 16px; border: 1px solid var(--line); overflow: hidden; }
  li { counter-increment: step; display: grid; grid-template-columns: 34px 1fr; gap: 12px; padding: 16px 20px; }
  li + li { border-top: 1px solid var(--line); }
  li::before { content: counter(step); width: 28px; height: 28px; border-radius: 50%; display: grid; place-items: center;
               background: color-mix(in srgb, var(--accent) 14%, transparent); color: var(--accent); font-weight: 700; font-size: 14px; }
  li b { display: block; }
  li span { color: var(--muted); font-size: 14px; }
</style></head>
<body><main>
  <div class="icon">{ICON}</div>
  <h1>Install Sweeflow</h1>
  <p class="lead">Your Mac's apps, as windows on your Linux machine.</p>
  <a class="button" href="/Sweeflow.dmg">Download Sweeflow</a>
  <span class="size">For macOS 15 or later</span>
  <ol>
    <li><div><b>Install it</b><span>Open the downloaded Sweeflow.dmg and drag Sweeflow into Applications.</span></div></li>
    <li><div><b>Open it</b><span>Open Sweeflow from Applications. If macOS says it can't check it, go to System Settings → Privacy &amp; Security and click Open Anyway.</span></div></li>
    <li><div><b>Allow and link</b><span>Sweeflow walks you through what it needs. Then choose this Mac on your Linux machine, check that both show the same code, and click Allow here.</span></div></li>
  </ol>
</main></body></html>
"#;
