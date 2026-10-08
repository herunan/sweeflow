//! Finding Macs on the local network (Bonjour) and pairing with one by the code it shows.
//!
//! The Mac's code is the first 40 bits of its certificate's SHA-256, so checking the certificate
//! against the typed code proves we reached that Mac (a fake one would need a key whose hash
//! starts the same). The Mac, in turn, asks the person there to allow us, showing our own code.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::{
    config::PairedMac,
    net::{self, HostCheck},
    proto::{self, t, R, W},
};

/// Always advertised by a running Mac host (what streaming connects to).
pub const STREAM_SERVICE: &str = "_sweeflow._udp.local.";
/// Advertised only while the Mac is in pairing mode.
pub const PAIR_SERVICE: &str = "_sweeflow-pair._udp.local.";
/// Where the Mac's pairing listener is (for a Mac typed in by address).
pub const PAIR_PORT: u16 = 44121;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The short code for a certificate fingerprint (hex), as `XXXX-XXXX`.
pub fn code_of(fingerprint_hex: &str) -> String {
    let v = fingerprint_hex.get(..10).and_then(|h| u64::from_str_radix(h, 16).ok()).unwrap_or(0);
    let c: String = (0..8).map(|i| ALPHABET[((v >> (35 - 5 * i)) & 31) as usize] as char).collect();
    format!("{}-{}", &c[..4], &c[4..])
}

/// What someone typed, as a code (Crockford base32: case, dashes and spaces don't matter,
/// O reads as 0 and I/L as 1). None if it can't be one.
pub fn normalize_code(input: &str) -> Option<String> {
    let mut c = String::new();
    for ch in input.chars().filter(|c| !c.is_whitespace() && *c != '-') {
        let ch = match ch.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            ch => ch,
        };
        if !ALPHABET.contains(&(ch as u8)) {
            return None;
        }
        c.push(ch);
    }
    (c.len() == 8).then(|| format!("{}-{}", &c[..4], &c[4..]))
}

/// This computer's name, as the Mac's "Allow …?" prompt shows it.
pub fn this_computer() -> String {
    let h = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let h = h.trim();
    if h.is_empty() { "Linux".into() } else { h.into() }
}

/// This computer's own code (shown next to the Mac's "Allow" prompt to compare).
pub fn our_code() -> Result<String> {
    Ok(code_of(&crate::config::client_fingerprint()?))
}

/// A Mac found on the network.
#[derive(Clone, Debug, PartialEq)]
pub struct FoundMac {
    /// Its Bonjour instance name (the Mac's computer name).
    pub name: String,
    /// First 16 hex digits of its certificate fingerprint.
    pub id: String,
    /// Its .local host name.
    pub hostname: String,
    /// Where the advertised service listens.
    pub addr: SocketAddr,
    /// The streaming port (from the TXT record).
    pub stream_port: u16,
}

pub enum Found {
    Up(FoundMac),
    Gone(String),
}

/// Browses for Macs advertising `service` until the returned handle is dropped.
pub struct Browser {
    daemon: ServiceDaemon,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

pub fn browse(service: &'static str, mut on: impl FnMut(Found) + Send + 'static) -> Result<Browser> {
    let daemon = ServiceDaemon::new().context("starting mDNS")?;
    let rx = daemon.browse(service).context("browsing mDNS")?;
    std::thread::Builder::new().name("mdns".into()).spawn(move || {
        while let Ok(ev) = rx.recv() {
            match ev {
                ServiceEvent::ServiceResolved(s) => {
                    // IPv4 first: it's what the local network (and a VPN's LAN exception) carries.
                    let mut ips: Vec<IpAddr> = s.get_addresses_v4().into_iter().map(IpAddr::V4).collect();
                    if ips.is_empty() {
                        ips = s.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                    }
                    let Some(ip) = ips.first().copied() else { continue };
                    let port = s.get_port();
                    on(Found::Up(FoundMac {
                        name: instance_name(s.get_fullname(), service),
                        id: s.get_property_val_str("id").unwrap_or_default().to_string(),
                        hostname: s.get_hostname().trim_end_matches('.').to_string(),
                        addr: SocketAddr::new(ip, port),
                        stream_port: s.get_property_val_str("port").and_then(|p| p.parse().ok()).unwrap_or(44120),
                    }));
                }
                ServiceEvent::ServiceRemoved(_, fullname) => on(Found::Gone(instance_name(&fullname, service))),
                ServiceEvent::SearchStopped(_) => break,
                _ => {}
            }
        }
    })?;
    Ok(Browser { daemon })
}

/// "Sam’s MacBook Air._sweeflow._udp.local." → "Sam’s MacBook Air".
fn instance_name(fullname: &str, service: &str) -> String {
    fullname.strip_suffix(service).unwrap_or(fullname).trim_end_matches('.').replace("\\.", ".").replace("\\ ", " ")
}

/// Finds a paired Mac's current streaming address by its fingerprint (waits up to `wait`).
pub fn locate(fingerprint: &str, wait: Duration) -> Option<SocketAddr> {
    let id = fingerprint.get(..16)?.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let _b = browse(STREAM_SERVICE, move |f| {
        if let Found::Up(m) = f {
            if m.id == id {
                let _ = tx.send(m.addr);
            }
        }
    })
    .ok()?;
    rx.recv_timeout(wait).ok()
}

pub enum Outcome {
    Paired(PairedMac),
    Denied,
}

/// Pairs with the Mac in pairing mode at `found`, checking it against the typed `code`.
/// Returns once the person at the Mac has allowed (or refused) us.
pub async fn pair(found: &FoundMac, code: &str) -> Result<Outcome> {
    let want = normalize_code(code).context("that isn't a code: it has 8 letters and digits, like K7QF-M2XA")?;
    let seen: Arc<Mutex<Option<String>>> = Arc::default();
    let seen2 = seen.clone();
    let check: HostCheck = Arc::new(move |got: &[u8]| {
        let fp = hex::encode(got);
        let ok = code_of(&fp) == want;
        *seen2.lock().unwrap() = Some(fp);
        if ok { Ok(()) } else { Err("the code doesn't match".into()) }
    });
    let endpoint = net::client_endpoint(check, b"sweeflow-pair/1")?;
    let conn = match endpoint.connect(found.addr, "sweeflow")?.await {
        Ok(c) => c,
        Err(e) => {
            if seen.lock().unwrap().is_some() {
                bail!("That code doesn't match {}. Check the code shown on the Mac.", found.name);
            }
            return Err(e).with_context(|| format!("couldn't reach {}", found.name));
        }
    };
    let fingerprint = seen.lock().unwrap().clone().context("no Mac certificate")?;
    log::info!("connected to {} for pairing", found.name);
    let (mut tx, mut rx) = conn.open_bi().await?;
    tx.write_all(&W::new(t::PAIR_REQUEST).u16(proto::VERSION).str(&this_computer()).finish()).await?;
    log::info!("asked {} to allow this computer", found.name);
    // The Mac now asks its person; give them time.
    let reply = tokio::time::timeout(Duration::from_secs(300), proto::read_msg(&mut rx))
        .await
        .context("the Mac didn't answer in time")??;
    let Some((t::PAIR_RESULT, body)) = reply else { bail!("the Mac closed the connection") };
    let mut r = R::new(&body);
    let ok = r.u8()? != 0;
    let mac_name = r.str()?;
    let port = r.u16().unwrap_or(found.stream_port);
    conn.close(0u32.into(), b"paired");
    if !ok {
        return Ok(Outcome::Denied);
    }
    let host = if found.hostname.is_empty() { found.addr.ip().to_string() } else { found.hostname.clone() };
    Ok(Outcome::Paired(PairedMac { name: mac_name, service: found.name.clone(), host: format!("{host}:{port}"), fingerprint }))
}

/// `sweeflow pair --text`: the same flow as the wizard, in a terminal.
pub fn run_text() -> Result<()> {
    use std::io::{BufRead, Write};
    println!("Looking for Macs ready to pair (on the Mac: Sweeflow menu → Pair with Linux…)");
    let found: Arc<Mutex<Vec<FoundMac>>> = Arc::default();
    let f2 = found.clone();
    let _b = browse(PAIR_SERVICE, move |f| {
        let mut all = f2.lock().unwrap();
        match f {
            Found::Up(m) => {
                all.retain(|o| o.name != m.name);
                all.push(m);
            }
            Found::Gone(n) => all.retain(|o| o.name != n),
        }
    })?;
    std::thread::sleep(Duration::from_secs(3));
    let macs = found.lock().unwrap().clone();
    if macs.is_empty() {
        bail!("no Mac is in pairing mode on this network (a VPN can hide it: allow local network access)");
    }
    for (i, m) in macs.iter().enumerate() {
        println!("  {}. {} ({})", i + 1, m.name, m.addr.ip());
    }
    let stdin = std::io::stdin();
    let mut line = String::new();
    let pick = if macs.len() == 1 {
        0
    } else {
        print!("Which Mac? ");
        std::io::stdout().flush()?;
        stdin.lock().read_line(&mut line)?;
        line.trim().parse::<usize>().ok().filter(|n| (1..=macs.len()).contains(n)).context("no such Mac")? - 1
    };
    let mac = &macs[pick];
    print!("Code shown on {}: ", mac.name);
    std::io::stdout().flush()?;
    line.clear();
    stdin.lock().read_line(&mut line)?;
    println!("Now click Allow on the Mac (it shows this computer's code {}).", our_code()?);
    let rt = tokio::runtime::Runtime::new()?;
    match rt.block_on(pair(mac, &line))? {
        Outcome::Denied => bail!("the Mac didn't allow this computer"),
        Outcome::Paired(p) => {
            let mut cfg = crate::config::Config::load_or_default()?;
            println!("Paired with {} ({}).", p.name, p.host);
            let first = cfg.host.is_empty();
            cfg.add_mac(p, first);
            cfg.save()?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_the_mac() {
        // Same vectors as the Mac's PairingCode (mac/Sources/SweeflowHost/Pairing.swift).
        assert_eq!(code_of(&"0".repeat(64)), "0000-0000");
        assert_eq!(code_of(&format!("ffffffffff{}", "0".repeat(54))), "ZZZZ-ZZZZ");
        assert_eq!(code_of("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"), "04HM-ASW9");
    }

    #[test]
    fn typed_codes_are_forgiving() {
        assert_eq!(normalize_code("04hm asw9").as_deref(), Some("04HM-ASW9"));
        assert_eq!(normalize_code("o4HM-ASW9").as_deref(), Some("04HM-ASW9"));
        assert_eq!(normalize_code("I1L0-0000").as_deref(), Some("1110-0000"));
        assert_eq!(normalize_code("04HM-ASW"), None);
        assert_eq!(normalize_code("04HM-ASWU"), None);
    }

    #[test]
    fn instance_names_lose_the_service() {
        assert_eq!(instance_name("Sam’s MacBook Air._sweeflow._udp.local.", STREAM_SERVICE), "Sam’s MacBook Air");
    }
}
