//! Hands the Mac its installer: while the "Add a Mac" wizard is open, Sweeflow.dmg is served on
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
    Ok(Server { url: format!("http://{ip}:{PORT}") })
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
        let page = PAGE.as_bytes();
        write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            page.len()
        )?;
        conn.write_all(page)?;
    }
    Ok(())
}

const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Install Sweeflow</title>
<style>
  body { font: 16px -apple-system, system-ui, sans-serif; max-width: 34em; margin: 4em auto; padding: 0 1em; color: #222; }
  @media (prefers-color-scheme: dark) { body { background: #1e1e1e; color: #ddd; } }
  a.button { display: inline-block; margin: 1em 0; padding: .7em 1.4em; border-radius: 8px;
             background: #0a64d8; color: white; text-decoration: none; font-weight: 600; }
  ol li { margin: .6em 0; }
</style></head>
<body>
<h1>Install Sweeflow</h1>
<p>Sweeflow shows this Mac's apps as windows on your Linux computer.</p>
<a class="button" href="/Sweeflow.dmg">Download Sweeflow</a>
<ol>
  <li>Open the downloaded <b>Sweeflow.dmg</b> and drag <b>Sweeflow</b> into <b>Applications</b>.</li>
  <li>Open Sweeflow from Applications. macOS says it can't check it: open
      <b>System Settings → Privacy &amp; Security</b>, scroll down and click <b>Open Anyway</b>.</li>
  <li>Follow Sweeflow's steps. It then shows a code: type that on your Linux computer.</li>
</ol>
</body></html>
"#;
