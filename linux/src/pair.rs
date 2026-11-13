//! Finding Macs on the local network (Bonjour) and linking with one.
//!
//! Nothing is typed. This computer asks the Mac, both work out the same short code from what each
//! sees of the connection (both certificates, and a fresh random number from each side), and the
//! person at the Mac clicks Allow if the two screens agree. A computer answering in between holds
//! different certificates on its two sides, so its two codes differ. The Mac commits to its number
//! before it sees ours, and we send ours before we see the Mac's, so neither number can be picked
//! to steer the code.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::{
    config::{Config, PairedMac},
    net::{self, HostCheck},
    proto::{self, t, R, W},
};

/// Always advertised by a running Mac host (what streaming connects to).
pub const STREAM_SERVICE: &str = "_sweeflow._udp.local.";
/// Advertised by a running Mac host that takes link requests (an older one advertised it only
/// while its code window was open).
pub const PAIR_SERVICE: &str = "_sweeflow-pair._udp.local.";
/// Where the Mac's linking listener is (for a Mac typed in by address).
pub const PAIR_PORT: u16 = 44121;

/// The linking exchange's version: in the listener's ALPN, and said again in the request, so a
/// Mac that speaks another one answers with that instead of nothing.
pub const LINK_VERSION: u16 = 2;
const LINK_ALPN: &[u8] = b"sweeflow-pair/2";
/// What a Mac from before codes were compared listens for (it showed a code to type).
const OLD_ALPN: &[u8] = b"sweeflow-pair/1";
/// How long the person at the Mac has to answer.
const ANSWER_WAIT: Duration = Duration::from_secs(120);

/// The code's characters: no 0 or O, no 1, I or L, which read alike.
const ALPHABET: &[u8; 31] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// The code both screens show while linking, as `XXX-XXX`: SHA-256 over a label, both
/// certificates' fingerprints (hex, as each side saw them in the handshake) and both sides'
/// nonces; its first 8 bytes as a number, written as 6 characters of `ALPHABET`.
/// The Mac's `LinkCode.of` is the same sum (mac/Sources/SweeflowHost/Pairing.swift).
pub fn link_code(client_fingerprint: &str, host_fingerprint: &str, client_nonce: &[u8; 32], host_nonce: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"sweeflow-link/2");
    h.update(client_fingerprint.to_ascii_lowercase().as_bytes());
    h.update(host_fingerprint.to_ascii_lowercase().as_bytes());
    h.update(client_nonce);
    h.update(host_nonce);
    let d = h.finalize();
    let n = ALPHABET.len() as u64;
    let mut v = u64::from_be_bytes(d[..8].try_into().expect("8 bytes")) % n.pow(6);
    let mut c = [0u8; 6];
    for slot in c.iter_mut().rev() {
        *slot = ALPHABET[(v % n) as usize];
        v /= n;
    }
    let c = String::from_utf8_lossy(&c);
    format!("{}-{}", &c[..3], &c[3..])
}

/// This computer's name, as the Mac's "wants to link" prompt shows it.
pub fn this_computer() -> String {
    let h = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let h = h.trim();
    if h.is_empty() { "Linux".into() } else { h.into() }
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

/// The Macs advertising streaming on the network within `wait`: the start of each one's
/// certificate fingerprint (16 hex digits, as `locate` matches them).
pub fn seen_on_network(wait: Duration) -> std::collections::HashSet<String> {
    let seen: Arc<Mutex<std::collections::HashSet<String>>> = Arc::default();
    let s2 = seen.clone();
    let browser = browse(STREAM_SERVICE, move |f| {
        if let Found::Up(m) = f {
            s2.lock().unwrap().insert(m.id);
        }
    });
    if browser.is_ok() {
        std::thread::sleep(wait);
    }
    drop(browser);
    std::mem::take(&mut *seen.lock().unwrap())
}

/// How asking a Mac to link ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Linked(PairedMac),
    /// The person at the Mac clicked Don't Allow.
    Refused,
    /// The Mac is asking about another computer.
    Busy,
    /// This computer was refused there a few minutes ago.
    Wait,
    /// The Mac's screen is locked.
    Locked,
    /// Both already have each other.
    AlreadyLinked,
    /// Nobody answered the Mac's question in time.
    NoAnswer,
    /// The Mac's Sweeflow is from before this exchange.
    TooOld,
    /// The Mac's Sweeflow is from after it.
    TooNew,
    /// No Sweeflow answered at that address.
    Unreachable,
    /// The person here cancelled.
    Cancelled,
}

impl Outcome {
    /// What to tell the person when it didn't link. None when it did, or they cancelled.
    pub fn message(&self, mac: &str) -> Option<String> {
        Some(match self {
            Outcome::Linked(_) | Outcome::Cancelled => return None,
            Outcome::Refused => format!("{mac} didn't allow this computer."),
            Outcome::Busy => format!("{mac} is busy with another request. Try again in a moment."),
            Outcome::Wait => format!("{mac} asked to wait a few minutes before trying again."),
            Outcome::Locked => format!("{mac} is locked. Unlock it and try again."),
            Outcome::AlreadyLinked => format!("{mac} is already linked to this computer."),
            Outcome::NoAnswer => format!("No answer from {mac}."),
            Outcome::TooOld => format!("Sweeflow on {mac} is too old for this. Update it from the first page."),
            Outcome::TooNew => format!("Sweeflow on this computer is too old for {mac}. Update it and try again."),
            Outcome::Unreachable => format!("Couldn't reach {mac}. Check that Sweeflow is open on it and up to date."),
        })
    }
}

/// Asks the Mac at `found` to link. `on_code` gets the code to show here once both sides have
/// it. Returns once the person at the Mac has answered, `cancel` is notified, or two minutes
/// have passed.
pub async fn link(found: &FoundMac, cancel: &Notify, on_code: impl FnOnce(&str)) -> Result<Outcome> {
    let seen: Arc<Mutex<Option<String>>> = Arc::default();
    let seen2 = seen.clone();
    // Any Mac may answer: that it's the right one is what the code is compared for.
    let check: HostCheck = Arc::new(move |got: &[u8]| {
        *seen2.lock().unwrap() = Some(hex::encode(got));
        Ok(())
    });
    let endpoint = net::client_endpoint(check, LINK_ALPN)?;
    let connected = tokio::select! {
        c = endpoint.connect(found.addr, "sweeflow")? => c,
        _ = cancel.notified() => return Ok(Outcome::Cancelled),
    };
    let conn = match connected {
        Ok(c) => c,
        Err(e) => {
            log::info!("couldn't connect to {} to link: {e}", found.name);
            return Ok(tokio::select! {
                old = speaks_old(found.addr) => if old { Outcome::TooOld } else { Outcome::Unreachable },
                _ = cancel.notified() => Outcome::Cancelled,
            });
        }
    };
    let host_fingerprint = seen.lock().unwrap().clone().context("no Mac certificate")?;
    let client_fingerprint = crate::config::client_fingerprint()?;
    // Holding this very certificate already: the Mac can skip its question if it has ours too.
    let known = Config::load_or_default().is_ok_and(|c| c.macs.iter().any(|m| m.fingerprint == host_fingerprint));
    log::info!("connected to {} to link", found.name);
    let outcome = tokio::select! {
        o = exchange(&conn, found, &client_fingerprint, &host_fingerprint, known, on_code) => o,
        _ = cancel.notified() => Ok(Outcome::Cancelled),
        _ = tokio::time::sleep(ANSWER_WAIT) => Ok(Outcome::NoAnswer),
    };
    // Tell the Mac we're gone (it takes its question down), and let that leave before the
    // caller drops the runtime under it.
    conn.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    outcome
}

/// The exchange on a connection to the Mac's linking listener (see PROTOCOL.md). The
/// fingerprints are the two certificates of that connection; `known` says this computer has the
/// Mac's already.
async fn exchange(
    conn: &quinn::Connection,
    found: &FoundMac,
    client_fingerprint: &str,
    host_fingerprint: &str,
    known: bool,
    on_code: impl FnOnce(&str),
) -> Result<Outcome> {
    let answer = |body: &[u8], code_shown: bool| outcome_of(body, found, host_fingerprint, known, code_shown);

    let (mut tx, mut rx) = conn.open_bi().await?;
    tx.write_all(&W::new(t::PAIR_REQUEST).u16(LINK_VERSION).str(&this_computer()).u8(known as u8).finish()).await?;
    let commitment = match next(&mut rx).await? {
        (t::PAIR_COMMIT, body) => R::new(&body).bytes()?.to_vec(),
        (t::PAIR_RESULT, body) => return answer(&body, false),
        (ty, _) => bail!("unexpected message {ty:#x} from the Mac"),
    };
    let mut ours = [0u8; 32];
    rustls::crypto::ring::default_provider().secure_random.fill(&mut ours).map_err(|_| anyhow::anyhow!("no random numbers"))?;
    tx.write_all(&W::new(t::PAIR_NONCE).bytes(&ours).finish()).await?;
    let theirs: [u8; 32] = match next(&mut rx).await? {
        (t::PAIR_REVEAL, body) => R::new(&body).bytes()?.try_into().context("the Mac's nonce isn't 32 bytes")?,
        (t::PAIR_RESULT, body) => return answer(&body, false),
        (ty, _) => bail!("unexpected message {ty:#x} from the Mac"),
    };
    // The nonce it promised before it saw ours: one chosen afterwards could be picked to make
    // this code match another one.
    if Sha256::digest(theirs).as_slice() != commitment.as_slice() {
        bail!("the Mac's nonce isn't the one it committed to");
    }
    on_code(&link_code(client_fingerprint, host_fingerprint, &ours, &theirs));
    log::info!("asked {} to allow this computer", found.name);
    match next(&mut rx).await? {
        (t::PAIR_RESULT, body) => answer(&body, true),
        (ty, _) => bail!("unexpected message {ty:#x} from the Mac"),
    }
}

/// The Mac's next message.
async fn next(rx: &mut quinn::RecvStream) -> Result<(u8, Vec<u8>)> {
    proto::read_msg(rx).await?.context("the Mac closed the connection")
}

/// Reads the Mac's PAIR_RESULT. "Linked" only counts once the code was shown here (the person
/// at the Mac had something to compare), and "already linked" only for a Mac this computer has.
fn outcome_of(body: &[u8], found: &FoundMac, host_fingerprint: &str, known: bool, code_shown: bool) -> Result<Outcome> {
    let mut r = R::new(body);
    let status = r.u8()?;
    // The name goes into the config file and onto the screen: one line, of a sensible length.
    let name: String = r.str()?.chars().filter(|c| !c.is_control()).take(60).collect();
    let port = r.u16()?;
    let version = r.u16()?;
    Ok(match status {
        0 if code_shown => {
            let host = if found.hostname.is_empty() { found.addr.ip().to_string() } else { found.hostname.clone() };
            Outcome::Linked(PairedMac {
                name: if name.is_empty() { found.name.clone() } else { name },
                service: found.name.clone(),
                host: format!("{host}:{port}"),
                fingerprint: host_fingerprint.to_string(),
            })
        }
        1 => Outcome::Refused,
        2 => Outcome::Busy,
        3 => Outcome::Wait,
        4 => Outcome::Locked,
        5 if known => Outcome::AlreadyLinked,
        6 if version < LINK_VERSION => Outcome::TooOld,
        6 => Outcome::TooNew,
        s => bail!("unexpected answer {s} from the Mac"),
    })
}

/// Whether a Sweeflow from before this exchange answers at `addr` (it listens while its code
/// window is open), to say so rather than "couldn't reach it".
async fn speaks_old(addr: SocketAddr) -> bool {
    let check: HostCheck = Arc::new(|_: &[u8]| Ok(()));
    let Ok(endpoint) = net::client_endpoint(check, OLD_ALPN) else { return false };
    let Ok(connecting) = endpoint.connect(addr, "sweeflow") else { return false };
    let Ok(Ok(conn)) = tokio::time::timeout(Duration::from_secs(3), connecting).await else { return false };
    conn.close(0u32.into(), b"too old");
    let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    true
}

/// `sweeflow pair --text`: the same flow as the wizard, in a terminal.
pub fn run_text() -> Result<()> {
    use std::io::{BufRead, Write};
    println!("Looking for Macs running Sweeflow on this network…");
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
        bail!("no Mac running Sweeflow was found on this network (a VPN can hide it: allow local network access)");
    }
    for (i, m) in macs.iter().enumerate() {
        println!("  {}. {} ({})", i + 1, m.name, m.addr.ip());
    }
    let pick = if macs.len() == 1 {
        0
    } else {
        print!("Which Mac? ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim().parse::<usize>().ok().filter(|n| (1..=macs.len()).contains(n)).context("no such Mac")? - 1
    };
    let mac = &macs[pick];
    let rt = tokio::runtime::Runtime::new()?;
    let show = |code: &str| println!("Check that {} shows the code {code}, then click Allow there.", mac.name);
    match rt.block_on(link(mac, &Notify::new(), show))? {
        Outcome::Linked(p) => {
            let mut cfg = Config::load_or_default()?;
            println!("Linked to {} ({}).", p.name, p.host);
            let first = cfg.host.is_empty();
            cfg.add_mac(p, first);
            cfg.save()?;
            Ok(())
        }
        other => bail!("{}", other.message(&mac.name).unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const HOST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn codes_match_the_mac() {
        // Same inputs and codes as the Mac's LinkCodeTests (mac/Tests/SweeflowHostTests).
        assert_eq!(link_code(CLIENT, HOST, &[0x11; 32], &[0x22; 32]), "QAM-3TF");
        assert_eq!(link_code(HOST, CLIENT, &[0x11; 32], &[0x22; 32]), "7M6-U9E");
        assert_eq!(link_code(CLIENT, HOST, &[0x22; 32], &[0x11; 32]), "Q8G-DRE");
        assert_eq!(link_code(&"0".repeat(64), &"f".repeat(64), &[0; 32], &[0xff; 32]), "KFW-AZV");
        // Fingerprints count the same in either case.
        assert_eq!(link_code(&CLIENT.to_uppercase(), HOST, &[0x11; 32], &[0x22; 32]), "QAM-3TF");
    }

    #[test]
    fn codes_are_two_groups_of_three_without_look_alikes() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..=255u8 {
            let code = link_code(CLIENT, HOST, &[i; 32], &[i.wrapping_mul(7); 32]);
            let c: Vec<char> = code.chars().collect();
            assert_eq!(c.len(), 7, "{code}");
            assert_eq!(c[3], '-', "{code}");
            for ch in c.iter().enumerate().filter(|(n, _)| *n != 3).map(|(_, ch)| *ch) {
                assert!(ch.is_ascii_uppercase() || ch.is_ascii_digit(), "{code}");
                assert!(!"0O1IL".contains(ch), "{code}");
            }
            seen.insert(code);
        }
        // A change in either nonce changes the code.
        assert!(seen.len() > 250);
    }

    fn found() -> FoundMac {
        FoundMac { name: "Air".into(), id: String::new(), hostname: "air.local".into(), addr: "192.168.1.20:44121".parse().unwrap(), stream_port: 44120 }
    }

    fn result(status: u8) -> Vec<u8> {
        W::new(t::PAIR_RESULT).u8(status).str("Sam’s Air\n").u16(44120).u16(LINK_VERSION).finish()[5..].to_vec()
    }

    #[test]
    fn linked_only_counts_once_the_code_was_shown() {
        let m = found();
        assert!(outcome_of(&result(0), &m, HOST, false, false).is_err());
        let Outcome::Linked(p) = outcome_of(&result(0), &m, HOST, false, true).unwrap() else { panic!("not linked") };
        assert_eq!(p, PairedMac { name: "Sam’s Air".into(), service: "Air".into(), host: "air.local:44120".into(), fingerprint: HOST.into() });
    }

    #[test]
    fn already_linked_only_counts_for_a_mac_this_computer_has() {
        let m = found();
        assert!(outcome_of(&result(5), &m, HOST, false, false).is_err());
        assert_eq!(outcome_of(&result(5), &m, HOST, true, false).unwrap(), Outcome::AlreadyLinked);
    }

    #[test]
    fn refusals_read_as_what_to_tell_the_person() {
        let m = found();
        let said = |status| outcome_of(&result(status), &m, HOST, false, false).unwrap().message("Air").unwrap();
        assert_eq!(said(1), "Air didn't allow this computer.");
        assert_eq!(said(2), "Air is busy with another request. Try again in a moment.");
        assert_eq!(said(3), "Air asked to wait a few minutes before trying again.");
        assert_eq!(said(4), "Air is locked. Unlock it and try again.");
        // The other side's version says which of the two is behind.
        let versioned = |v: u16| W::new(t::PAIR_RESULT).u8(6).str("Air").u16(44120).u16(v).finish()[5..].to_vec();
        assert_eq!(outcome_of(&versioned(1), &m, HOST, false, false).unwrap(), Outcome::TooOld);
        assert_eq!(outcome_of(&versioned(3), &m, HOST, false, false).unwrap(), Outcome::TooNew);
        assert_eq!(Outcome::NoAnswer.message("Air").unwrap(), "No answer from Air.");
        assert_eq!(Outcome::TooOld.message("Air").unwrap(), "Sweeflow on Air is too old for this. Update it from the first page.");
    }

    /// A connection to a stand-in for the Mac's linking listener on this computer, which runs
    /// `mac` on the stream the exchange opens.
    async fn to_a_mac<F>(mac: impl FnOnce(quinn::SendStream, quinn::RecvStream) -> F + Send + 'static) -> (quinn::Endpoint, quinn::Connection)
    where
        F: std::future::Future<Output = ()> + Send,
    {
        let ck = rcgen::generate_simple_self_signed(vec!["sweeflow".to_string()]).unwrap();
        let cert = ck.cert.der().clone();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());
        let server = quinn::Endpoint::server(quinn::ServerConfig::with_single_cert(vec![cert.clone()], key).unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let (tx, rx) = conn.accept_bi().await.unwrap();
            mac(tx, rx).await;
            conn.closed().await;
        });
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap());
        let conn = client.connect(addr, "sweeflow").unwrap().await.unwrap();
        (client, conn)
    }

    /// The Mac's side up to its prompt: commits to `committed`, takes our nonce, reveals
    /// `revealed`. Returns the code its prompt would show.
    async fn mac_asks(tx: &mut quinn::SendStream, rx: &mut quinn::RecvStream, committed: [u8; 32], revealed: [u8; 32]) -> String {
        let (ty, body) = proto::read_msg(rx).await.unwrap().unwrap();
        assert_eq!(ty, t::PAIR_REQUEST);
        let mut r = R::new(&body);
        assert_eq!((r.u16().unwrap(), r.str().unwrap(), r.u8().unwrap()), (LINK_VERSION, this_computer(), 0));
        tx.write_all(&W::new(t::PAIR_COMMIT).bytes(Sha256::digest(committed).as_slice()).finish()).await.unwrap();
        let (ty, body) = proto::read_msg(rx).await.unwrap().unwrap();
        assert_eq!(ty, t::PAIR_NONCE);
        let ours: [u8; 32] = R::new(&body).bytes().unwrap().try_into().unwrap();
        tx.write_all(&W::new(t::PAIR_REVEAL).bytes(&revealed).finish()).await.unwrap();
        link_code(CLIENT, HOST, &ours, &revealed)
    }

    #[tokio::test]
    async fn the_exchange_shows_the_macs_code_and_links_on_allow() {
        let (code_tx, code_rx) = tokio::sync::oneshot::channel();
        let (_client, conn) = to_a_mac(|mut tx, mut rx| async move {
            let code = mac_asks(&mut tx, &mut rx, [7; 32], [7; 32]).await;
            code_tx.send(code).unwrap();
            tx.write_all(&W::new(t::PAIR_RESULT).u8(0).str("Air").u16(44120).u16(LINK_VERSION).finish()).await.unwrap();
        })
        .await;
        let mut shown = String::new();
        let outcome = exchange(&conn, &found(), CLIENT, HOST, false, |c| shown = c.to_string()).await.unwrap();
        assert_eq!(shown, code_rx.await.unwrap());
        assert!(matches!(outcome, Outcome::Linked(p) if p.fingerprint == HOST && p.name == "Air"));
    }

    #[tokio::test]
    async fn a_nonce_other_than_the_committed_one_ends_it_with_no_code_shown() {
        let (_client, conn) = to_a_mac(|mut tx, mut rx| async move {
            mac_asks(&mut tx, &mut rx, [7; 32], [8; 32]).await;
            tx.write_all(&W::new(t::PAIR_RESULT).u8(0).str("Air").u16(44120).u16(LINK_VERSION).finish()).await.unwrap();
        })
        .await;
        let mut shown = false;
        assert!(exchange(&conn, &found(), CLIENT, HOST, false, |_| shown = true).await.is_err());
        assert!(!shown);
    }

    #[tokio::test]
    async fn a_refusal_straight_away_is_the_answer() {
        let (_client, conn) = to_a_mac(|mut tx, mut rx| async move {
            proto::read_msg(&mut rx).await.unwrap().unwrap();
            tx.write_all(&W::new(t::PAIR_RESULT).u8(2).str("Air").u16(44120).u16(LINK_VERSION).finish()).await.unwrap();
        })
        .await;
        assert_eq!(exchange(&conn, &found(), CLIENT, HOST, false, |_| panic!("no code to show")).await.unwrap(), Outcome::Busy);
    }

    #[test]
    fn instance_names_lose_the_service() {
        assert_eq!(instance_name("Sam’s MacBook Air._sweeflow._udp.local.", STREAM_SERVICE), "Sam’s MacBook Air");
    }
}
