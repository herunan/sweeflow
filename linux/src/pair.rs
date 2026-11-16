//! Finding Macs on the local network (Bonjour), linking with one, and unlinking.
//!
//! Linking: this computer asks the Mac, the Mac shows a 4-digit code, the person types it here,
//! and the two run a PAKE with it (`crate::pake`) that binds both TLS certificates: each side
//! proves it has the same key before either keeps the other's certificate. A computer answering in
//! between, or one guessing, gets one guess per try and three tries per code shown on the Mac.
//!
//! Unlinking on either side tells the other: this computer sends the Mac "unlink me" (now, or once
//! the Mac is next seen on the network), and asks Macs it isn't streaming from whether it's still
//! linked there (`watch_links`).

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::sync::{mpsc, Notify};

use crate::{
    config::{Config, PairedMac},
    net::{self, HostCheck},
    pake,
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
pub const LINK_VERSION: u16 = 3;
const LINK_ALPN: &[u8] = b"sweeflow-pair/3";
/// Also offered, only so a Mac from before codes were typed again answers "version 2".
const V2_ALPN: &[u8] = b"sweeflow-pair/2";
/// What a Mac from before link version 2 listens for (it showed a code only while its window was open).
const OLD_ALPN: &[u8] = b"sweeflow-pair/1";
/// How long the Mac shows a code for (five minutes), and a little more.
const CODE_WAIT: Duration = Duration::from_secs(330);
/// The code's length.
pub const CODE_DIGITS: usize = 4;

/// What someone typed, as a code: its digits, when there are exactly four (spaces and dashes
/// don't matter). None if it can't be one.
pub fn normalize_code(input: &str) -> Option<String> {
    let kept: String = input.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    (kept.len() == CODE_DIGITS && kept.chars().all(|c| c.is_ascii_digit())).then_some(kept)
}

/// This computer's name, as the Mac's code window shows it ("Enter this code on …").
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
    /// Streaming to a computer now (from the TXT record; a Mac from before it said so: false).
    pub busy: bool,
    /// Its screen is locked (the same).
    pub locked: bool,
    /// Start was pressed on it for the computer whose fingerprint begins with this (16 hex
    /// digits; empty: none).
    pub start: String,
}

/// What a linked Mac says of itself without being connected to: streaming to a computer, and
/// whether its screen is locked.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Presence {
    pub busy: bool,
    pub locked: bool,
    /// Start was pressed on it for this computer.
    pub start: bool,
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
                        busy: s.get_property_val_str("busy") == Some("1"),
                        locked: s.get_property_val_str("locked") == Some("1"),
                        start: s.get_property_val_str("start").unwrap_or_default().to_string(),
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

/// The start of this computer's certificate fingerprint, as a Mac's announcement names it.
fn this_computer_id() -> String {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| crate::config::client_fingerprint().map(|f| f.get(..16).unwrap_or_default().to_string()).unwrap_or_default()).clone()
}

/// The Macs advertising streaming on the network within `wait`: the start of each one's
/// certificate fingerprint (16 hex digits, as `locate` matches them), and what each says of
/// itself.
pub fn seen_on_network(wait: Duration) -> std::collections::HashMap<String, Presence> {
    let seen: Arc<Mutex<std::collections::HashMap<String, Presence>>> = Arc::default();
    let s2 = seen.clone();
    let browser = browse(STREAM_SERVICE, move |f| {
        if let Found::Up(m) = f {
            let start = !m.start.is_empty() && this_computer_id() == m.start;
            s2.lock().unwrap().insert(m.id, Presence { busy: m.busy, locked: m.locked, start });
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
    /// The person at the Mac closed its code window.
    CancelledThere,
    /// The Mac is showing a code for another computer.
    Busy,
    /// The Mac had too many wrong codes lately, and waits before showing another.
    Wait,
    /// The Mac's screen is locked.
    Locked,
    /// The Mac has this computer linked already (unlinked here only, while the Mac was away).
    AlreadyLinked,
    /// This computer has the Mac linked already.
    AlreadyHere,
    /// Three wrong codes: the Mac stopped.
    TooManyWrong,
    /// Nobody typed the code in time.
    Expired,
    /// The Mac didn't answer in time.
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
        let m = self.message_lower(mac)?;
        // "your Mac" (a Mac typed by address) leads some: capitalised there.
        let mut c = m.chars();
        Some(c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or(m))
    }

    fn message_lower(&self, mac: &str) -> Option<String> {
        Some(match self {
            Outcome::Linked(_) | Outcome::Cancelled => return None,
            Outcome::CancelledThere => format!("Linking was cancelled on {mac}."),
            Outcome::Busy => format!("{mac} is showing a code for another computer. Try again in a moment."),
            Outcome::Wait => format!("{mac} had too many wrong codes. Try again in a few minutes."),
            Outcome::Locked => format!("{mac} is locked. Unlock it and try again."),
            Outcome::AlreadyLinked => {
                format!("{mac} still has this computer linked. To link again, unlink this computer on the Mac first (Sweeflow menu → Linux machines).")
            }
            Outcome::AlreadyHere => format!("{mac} is already linked to this computer."),
            Outcome::TooManyWrong => "Too many wrong codes. Try again to get a new code.".into(),
            Outcome::Expired => "The code expired. Try again to get a new one.".into(),
            Outcome::NoAnswer => format!("No answer from {mac}."),
            Outcome::TooOld => format!("Sweeflow on {mac} is too old for this. Update it from the first page."),
            Outcome::TooNew => format!("Sweeflow on this computer is too old for {mac}. Update it and try again."),
            Outcome::Unreachable => format!("Couldn't reach {mac}. Check that Sweeflow is open on it and up to date."),
        })
    }
}

/// Asks the Mac at `found` to link. Each time the Mac shows a code (again, after a wrong one),
/// `on_ask` gets the tries left and whether the last code was wrong, and the code typed is read
/// from `codes`. Returns once linked, refused, `cancel` is notified, or the code has expired.
pub async fn link(found: &FoundMac, cancel: &Notify, codes: &mut mpsc::UnboundedReceiver<String>, on_ask: impl FnMut(u8, bool)) -> Result<Outcome> {
    let seen: Arc<Mutex<Option<String>>> = Arc::default();
    let seen2 = seen.clone();
    // Any Mac may answer: that it's the right one is what the code proves.
    let check: HostCheck = Arc::new(move |got: &[u8]| {
        *seen2.lock().unwrap() = Some(hex::encode(got));
        Ok(())
    });
    let endpoint = net::client_endpoint_offering(check, &[LINK_ALPN, V2_ALPN])?;
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
    log::info!("connected to {} to link", found.name);
    let outcome = if Config::load_or_default().is_ok_and(|c| c.macs.iter().any(|m| m.fingerprint == host_fingerprint)) {
        // Typed in by address: the list says so before this.
        Ok(Outcome::AlreadyHere)
    } else {
        // Unlinked here while the Mac was away: tell it first, or it says "linked already".
        if pending_unlinks().contains(&host_fingerprint) {
            match ask(&conn, t::PAIR_UNLINK).await {
                Ok(9) => done_unlinking(&host_fingerprint),
                other => log::info!("telling {} it was unlinked: {other:?}", found.name),
            }
        }
        tokio::select! {
            o = exchange(&conn, found, &client_fingerprint, &host_fingerprint, codes, on_ask) => o,
            _ = cancel.notified() => Ok(Outcome::Cancelled),
            _ = tokio::time::sleep(CODE_WAIT) => Ok(Outcome::NoAnswer),
        }
    };
    // Tell the Mac we're gone (it takes its code down), and let that leave before the caller
    // drops the runtime under it.
    conn.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    outcome
}

/// The exchange on a connection to the Mac's linking listener (see PROTOCOL.md). The
/// fingerprints are the two certificates of that connection, as this computer sees them.
async fn exchange(
    conn: &quinn::Connection,
    found: &FoundMac,
    client_fingerprint: &str,
    host_fingerprint: &str,
    codes: &mut mpsc::UnboundedReceiver<String>,
    mut on_ask: impl FnMut(u8, bool),
) -> Result<Outcome> {
    let (mut tx, rx) = conn.open_bi().await?;
    tx.write_all(&W::new(t::PAIR_REQUEST).u16(LINK_VERSION).str(&this_computer()).finish()).await?;
    // Read on a task of its own: a message may come while the person is still typing (the code
    // window closed on the Mac), and a read dropped halfway would lose its bytes.
    let mut from_mac = read_all(rx);
    let mut expected_host_tag: Option<[u8; 32]> = None;
    let mut asked = false;
    loop {
        let (ty, body) = from_mac.recv().await.context("the Mac closed the connection")??;
        match ty {
            t::PAIR_SHARE => {
                let mut r = R::new(&body);
                let tries_left = r.u8()?;
                let host_share: [u8; 32] = r.bytes()?.try_into().context("the Mac's share isn't 32 bytes")?;
                on_ask(tries_left, asked);
                asked = true;
                let code = tokio::select! {
                    c = codes.recv() => c.context("no code")?,
                    m = from_mac.recv() => {
                        let (ty, body) = m.context("the Mac closed the connection")??;
                        if ty != t::PAIR_RESULT {
                            bail!("unexpected message {ty:#x} from the Mac");
                        }
                        return outcome_of(&body, found, host_fingerprint, None);
                    }
                };
                let ours = pake::Share::random(&pake::generator(&code, client_fingerprint, host_fingerprint))?;
                let tags = pake::client_tags(&ours, &host_share, client_fingerprint, host_fingerprint)?;
                expected_host_tag = Some(tags.host);
                tx.write_all(&W::new(t::PAIR_GUESS).bytes(&ours.public).bytes(&tags.client).finish()).await?;
                log::info!("sent {} a code", found.name);
            }
            t::PAIR_RESULT => return outcome_of(&body, found, host_fingerprint, expected_host_tag.as_ref()),
            ty => bail!("unexpected message {ty:#x} from the Mac"),
        }
    }
}

/// The Mac's messages on `rx`, as they come.
fn read_all(mut rx: quinn::RecvStream) -> mpsc::UnboundedReceiver<Result<(u8, Vec<u8>)>> {
    let (tx, out) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match proto::read_msg(&mut rx).await {
                Ok(Some(m)) => {
                    if tx.send(Ok(m)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    out
}

/// Reads the Mac's PAIR_RESULT. "Linked" only counts with the Mac's tag for the code this
/// computer sent (`expected`): then both have proved they have the same key.
fn outcome_of(body: &[u8], found: &FoundMac, host_fingerprint: &str, expected: Option<&[u8; 32]>) -> Result<Outcome> {
    let mut r = R::new(body);
    let status = r.u8()?;
    // The name goes into the config file and onto the screen: one line, of a sensible length.
    let name: String = r.str()?.chars().filter(|c| !c.is_control()).take(60).collect();
    let port = r.u16()?;
    let version = r.u16()?;
    Ok(match status {
        0 => {
            let tag = r.bytes().unwrap_or_default();
            if !expected.is_some_and(|e| pake::same(e, tag)) {
                bail!("the Mac couldn't prove it has the code");
            }
            let host = if found.hostname.is_empty() { found.addr.ip().to_string() } else { found.hostname.clone() };
            Outcome::Linked(PairedMac {
                name: if name.is_empty() { found.name.clone() } else { name },
                service: found.name.clone(),
                host: format!("{host}:{port}"),
                fingerprint: host_fingerprint.to_string(),
                ..Default::default()
            })
        }
        1 => Outcome::CancelledThere,
        2 => Outcome::Busy,
        3 => Outcome::Wait,
        4 => Outcome::Locked,
        5 => Outcome::AlreadyLinked,
        6 if version < LINK_VERSION => Outcome::TooOld,
        6 => Outcome::TooNew,
        7 => Outcome::TooManyWrong,
        8 => Outcome::Expired,
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

/// Sends `ty` (PAIR_UNLINK or PAIR_CHECK, no payload) on a stream of its own and returns the
/// status the Mac answers with.
async fn ask(conn: &quinn::Connection, ty: u8) -> Result<u8> {
    Ok(ask_full(conn, ty).await?.0)
}

/// The same, with what the Mac says of itself (None: a Mac from before it said so).
async fn ask_full(conn: &quinn::Connection, ty: u8) -> Result<(u8, Option<Presence>)> {
    let (mut tx, mut rx) = conn.open_bi().await?;
    tx.write_all(&W::new(ty).finish()).await?;
    let reply = tokio::time::timeout(Duration::from_secs(5), proto::read_msg(&mut rx)).await.context("no answer")??;
    let Some((t::PAIR_RESULT, body)) = reply else { bail!("no answer") };
    let _ = tx.finish();
    let mut r = R::new(&body);
    let status = r.u8()?;
    let presence = (|| -> Option<Presence> {
        r.str().ok()?;
        r.u16().ok()?;
        r.u16().ok()?;
        r.bytes().ok()?;
        let busy = r.u8().ok()? != 0;
        let locked = r.u8().is_ok_and(|l| l != 0);
        Some(Presence { busy, locked, start: r.u8().is_ok_and(|s| s != 0) })
    })();
    Ok((status, presence))
}

/// Asks a linked Mac directly whether it's there and busy, for when its network announcement
/// isn't seen (a virtual machine's network, or one that drops multicast): what it says of itself
/// if it answered as this computer's linked Mac; None if it didn't answer.
pub fn probe(mac: &PairedMac) -> Option<Presence> {
    // Its addresses as numbers first (a name ending in .local is looked up over the very
    // announcements that are missing when it comes to this, and that wait was spent before
    // any address was tried), then its names.
    let mut places = crate::config::places(mac);
    places.sort_by_key(|p| {
        let host = p.rsplit_once(':').map_or(p.as_str(), |(h, _)| h).trim_matches(['[', ']']);
        host.parse::<std::net::IpAddr>().is_err()
    });
    places.iter().find_map(|place| probe_at(mac, place))
}

fn probe_at(mac: &PairedMac, place: &str) -> Option<Presence> {
    let host = place.rsplit_once(':').map_or(place, |(h, _)| h).trim_matches(['[', ']']);
    let addr = (host, PAIR_PORT).to_socket_addrs().ok()?.find(|a| a.is_ipv4())?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    let want = mac.fingerprint.clone();
    rt.block_on(async move {
        let check: HostCheck = Arc::new(move |got: &[u8]| if hex::encode(got) == want { Ok(()) } else { Err("not that Mac".into()) });
        let endpoint = net::client_endpoint(check, LINK_ALPN).ok()?;
        let answer = async {
            let conn = tokio::time::timeout(Duration::from_secs(2), endpoint.connect(addr, "sweeflow").ok()?).await.ok()?.ok()?;
            let a = ask_full(&conn, t::PAIR_CHECK).await.ok();
            conn.close(0u32.into(), b"done");
            a
        }
        .await;
        let _ = tokio::time::timeout(Duration::from_millis(500), endpoint.wait_idle()).await;
        // Linked (0) or not, it answered: it's there.
        answer.map(|(_, p)| p.unwrap_or_default())
    })
}

/// Asks the linked Mac at `addr` (whose certificate must be `fingerprint`) one thing: `ty`.
async fn ask_mac(addr: SocketAddr, fingerprint: &str, ty: u8) -> Result<u8> {
    let want = fingerprint.to_string();
    // Only that Mac's answer counts: "not linked" from anyone else would unlink it here.
    let check: HostCheck = Arc::new(move |got: &[u8]| if hex::encode(got) == want { Ok(()) } else { Err("not that Mac".into()) });
    let endpoint = net::client_endpoint(check, LINK_ALPN)?;
    let status = async {
        let conn = tokio::time::timeout(Duration::from_secs(5), endpoint.connect(addr, "sweeflow")?).await.context("no answer")??;
        let status = ask(&conn, ty).await;
        conn.close(0u32.into(), b"done");
        status
    }
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    status
}

/// Macs unlinked here that haven't been told yet: a line each, beside the config, of the
/// fingerprint and then the places it was last known at (host:port of its streaming service).
fn pending_file() -> std::path::PathBuf {
    crate::config::dir().join("unlink-pending")
}

fn pending_entries() -> Vec<(String, Vec<String>)> {
    std::fs::read_to_string(pending_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let fp = words.next()?.to_ascii_lowercase();
            (fp.len() == 64).then(|| (fp, words.map(str::to_string).collect()))
        })
        .collect()
}

pub fn pending_unlinks() -> Vec<String> {
    pending_entries().into_iter().map(|(fp, _)| fp).collect()
}

fn set_pending_entries(all: &[(String, Vec<String>)]) {
    let text: String = all.iter().map(|(f, places)| format!("{f} {}\n", places.join(" ")).replace(" \n", "\n")).collect();
    if let Err(e) = std::fs::write(pending_file(), text) {
        log::warn!("saving the Macs still to tell: {e}");
    }
}

fn done_unlinking(fingerprint: &str) {
    let mut all = pending_entries();
    all.retain(|(f, _)| f != fingerprint);
    set_pending_entries(&all);
    log::info!("told the Mac {} it was unlinked", &fingerprint[..8.min(fingerprint.len())]);
}

/// This computer unlinked `mac`: tells it (off this thread). Noted first, with where it was, so
/// that if it can't be reached now (or Sweeflow restarts meanwhile, as unlinking the Mac in use
/// does) it's told later: at those places (`tell_pending`, at start and now and then), once it's
/// seen on the network (`watch_links`), or when it's linked again.
pub fn tell_unlinked(mac: PairedMac) {
    let mut all = pending_entries();
    all.retain(|(f, _)| *f != mac.fingerprint);
    all.push((mac.fingerprint.clone(), crate::config::places(&mac)));
    set_pending_entries(&all);
    std::thread::spawn(tell_pending);
}

/// Tells each Mac still to be told that it was unlinked, at the places it was last known at
/// (its linking port beside each).
fn tell_pending() {
    static BUSY: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let Ok(_one_at_a_time) = BUSY.try_lock() else { return };
    let Ok(rt) = tokio::runtime::Runtime::new() else { return };
    for (fp, places) in pending_entries() {
        let addrs = places.iter().filter_map(|p| {
            let host = p.rsplit_once(':').map_or(p.as_str(), |(h, _)| h).trim_matches(['[', ']']);
            (host, PAIR_PORT).to_socket_addrs().ok().and_then(|mut a| a.next())
        });
        let mut last = None;
        for addr in addrs {
            match rt.block_on(ask_mac(addr, &fp, t::PAIR_UNLINK)) {
                Ok(9) => {
                    done_unlinking(&fp);
                    last = None;
                    break;
                }
                other => last = Some(format!("{addr}: {other:?}")),
            }
        }
        if let Some(why) = last {
            log::info!("Mac {} not told it was unlinked yet ({why})", &fp[..8]);
        }
    }
}

/// Asks each linked Mac this computer isn't streaming from whether it's still linked there, at
/// its places (the first that answers); one that unlinked this computer is unlinked here too.
fn check_links() {
    let Ok(cfg) = Config::load_or_default() else { return };
    let streaming = crate::net::connected_to().is_some();
    let Ok(rt) = tokio::runtime::Runtime::new() else { return };
    for mac in cfg.macs.iter().filter(|m| !(streaming && m.fingerprint == cfg.host_fingerprint)) {
        let addrs = crate::config::places(mac).into_iter().filter_map(|p| {
            let host = p.rsplit_once(':').map_or(p.as_str(), |(h, _)| h).trim_matches(['[', ']']).to_string();
            (host.as_str(), PAIR_PORT).to_socket_addrs().ok().and_then(|mut a| a.next())
        });
        for addr in addrs {
            match rt.block_on(ask_mac(addr, &mac.fingerprint, t::PAIR_CHECK)) {
                Ok(10) => {
                    log::info!("{} unlinked this computer: unlinking it here too", mac.name);
                    crate::tray::mac_unlinked_us(&mac.fingerprint);
                    break;
                }
                Ok(_) => break,
                Err(_) => {}
            }
        }
    }
}

/// While Sweeflow runs: whenever a Mac shows up on the network, tells it if it was unlinked here,
/// and asks a linked one whether this computer is still linked there (a Mac unlinked this computer
/// while not streaming to it): if not, it's unlinked here too. Through each Mac's linking
/// listener, which leaves its streaming session alone.
pub fn watch_links() {
    // Without Bonjour (a virtual machine's network, Tailscale, the internet) no Mac is ever seen
    // below: at start and every few minutes, Macs unlinked here are told at the places they were
    // last known at, and linked ones asked whether this computer is still linked there.
    std::thread::spawn(|| loop {
        tell_pending();
        check_links();
        std::thread::sleep(Duration::from_secs(180));
    });
    let (tx, rx) = std::sync::mpsc::channel::<FoundMac>();
    let browser = browse(PAIR_SERVICE, move |f| {
        if let Found::Up(m) = f {
            let _ = tx.send(m);
        }
    });
    let browser = match browser {
        Ok(b) => b,
        Err(e) => return log::info!("not watching for unlinked Macs: {e:#}"),
    };
    std::thread::Builder::new()
        .name("links".into())
        .spawn(move || {
            let _browser = browser;
            let Ok(rt) = tokio::runtime::Runtime::new() else { return };
            // When each Mac was last asked: once in a while is enough.
            let mut asked: HashMap<String, Instant> = HashMap::new();
            while let Ok(m) = rx.recv() {
                if m.id.len() < 16 {
                    continue;
                }
                for fp in pending_unlinks().into_iter().filter(|f| f.starts_with(&m.id)) {
                    if let Ok(9) = rt.block_on(ask_mac(m.addr, &fp, t::PAIR_UNLINK)) {
                        done_unlinking(&fp);
                    }
                }
                let linked = Config::load_or_default().ok().and_then(|c| c.macs.into_iter().find(|p| p.fingerprint.starts_with(&m.id)));
                let Some(mac) = linked else { continue };
                if asked.get(&mac.fingerprint).is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
                    continue;
                }
                asked.insert(mac.fingerprint.clone(), Instant::now());
                match rt.block_on(ask_mac(m.addr, &mac.fingerprint, t::PAIR_CHECK)) {
                    Ok(10) => {
                        log::info!("{} unlinked this computer: unlinking it here too", mac.name);
                        crate::tray::mac_unlinked_us(&mac.fingerprint);
                    }
                    Ok(_) => {}
                    Err(e) => log::info!("couldn't ask {} whether it's still linked: {e:#}", mac.name),
                }
            }
        })
        .ok();
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
    let mac = macs[pick].clone();
    let (codes_tx, mut codes) = mpsc::unbounded_channel();
    let name = mac.name.clone();
    let ask = move |tries_left: u8, wrong: bool| {
        if wrong {
            println!("That's not the code. {tries_left} {} left.", if tries_left == 1 { "try" } else { "tries" });
        }
        let codes_tx = codes_tx.clone();
        let name = name.clone();
        // Typed on a thread of its own: the exchange goes on meanwhile.
        std::thread::spawn(move || loop {
            print!("Code shown on {name}: ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            match normalize_code(&line) {
                Some(code) => {
                    let _ = codes_tx.send(code);
                    return;
                }
                None => println!("The code has {CODE_DIGITS} digits."),
            }
        });
    };
    let rt = tokio::runtime::Runtime::new()?;
    match rt.block_on(link(&mac, &Notify::new(), &mut codes, ask))? {
        Outcome::Linked(p) => {
            let mut cfg = Config::load_or_default()?;
            println!("Linked to {} ({}).", p.name, p.host);
            cfg.add_mac(p, true);
            cfg.save()?;
            Ok(())
        }
        other => bail!("{}", other.message(&mac.name).unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pake::tests::{CLIENT, HOST};

    #[test]
    fn typed_codes_are_four_digits() {
        assert_eq!(normalize_code(" 48 21\n").as_deref(), Some("4821"));
        assert_eq!(normalize_code("48-21").as_deref(), Some("4821"));
        assert_eq!(normalize_code("482"), None);
        assert_eq!(normalize_code("48211"), None);
        assert_eq!(normalize_code("48a1"), None);
    }

    fn found() -> FoundMac {
        FoundMac { name: "Air".into(), id: String::new(), hostname: "air.local".into(), addr: "192.168.1.20:44121".parse().unwrap(), stream_port: 44120, busy: false, locked: false, start: String::new() }
    }

    fn result(status: u8, tag: &[u8]) -> Vec<u8> {
        W::new(t::PAIR_RESULT).u8(status).str("Sam’s Air\n").u16(44120).u16(LINK_VERSION).bytes(tag).finish()
    }

    #[test]
    fn linked_only_counts_with_the_macs_tag() {
        let m = found();
        let body = |s, tag: &[u8]| result(s, tag)[5..].to_vec();
        assert!(outcome_of(&body(0, &[1; 32]), &m, HOST, None).is_err());
        assert!(outcome_of(&body(0, &[1; 32]), &m, HOST, Some(&[2; 32])).is_err());
        assert!(outcome_of(&body(0, &[]), &m, HOST, Some(&[1; 32])).is_err());
        let Outcome::Linked(p) = outcome_of(&body(0, &[1; 32]), &m, HOST, Some(&[1; 32])).unwrap() else { panic!("not linked") };
        assert_eq!(p, PairedMac { name: "Sam’s Air".into(), service: "Air".into(), host: "air.local:44120".into(), fingerprint: HOST.into(), ..Default::default() });
    }

    #[test]
    fn refusals_read_as_what_to_tell_the_person() {
        let m = found();
        let said = |status| outcome_of(&result(status, &[])[5..], &m, HOST, None).unwrap().message("Air").unwrap();
        assert_eq!(said(1), "Linking was cancelled on Air.");
        assert_eq!(said(2), "Air is showing a code for another computer. Try again in a moment.");
        assert_eq!(said(3), "Air had too many wrong codes. Try again in a few minutes.");
        assert_eq!(said(4), "Air is locked. Unlock it and try again.");
        assert!(said(5).starts_with("Air still has this computer linked."));
        assert_eq!(said(7), "Too many wrong codes. Try again to get a new code.");
        assert_eq!(said(8), "The code expired. Try again to get a new one.");
        // The other side's version says which of the two is behind; a version 2 Mac sends no tag.
        let versioned = |v: u16| W::new(t::PAIR_RESULT).u8(6).str("Air").u16(44120).u16(v).finish()[5..].to_vec();
        assert_eq!(outcome_of(&versioned(2), &m, HOST, None).unwrap(), Outcome::TooOld);
        assert_eq!(outcome_of(&versioned(4), &m, HOST, None).unwrap(), Outcome::TooNew);
        assert_eq!(Outcome::AlreadyHere.message("Air").unwrap(), "Air is already linked to this computer.");
        assert_eq!(Outcome::AlreadyHere.message("your Mac").unwrap(), "Your Mac is already linked to this computer.");
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

    /// The Mac's side of one try with `code` shown: sends its share, reads the guess, and says
    /// whether it was right, with the tag to answer it with.
    async fn mac_try(tx: &mut quinn::SendStream, rx: &mut quinn::RecvStream, code: &str, tries_left: u8) -> Option<[u8; 32]> {
        let g = pake::generator(code, CLIENT, HOST);
        let ours = pake::Share::random(&g).unwrap();
        tx.write_all(&W::new(t::PAIR_SHARE).u8(tries_left).bytes(&ours.public).finish()).await.unwrap();
        let (ty, body) = proto::read_msg(rx).await.unwrap().unwrap();
        assert_eq!(ty, t::PAIR_GUESS);
        let mut r = R::new(&body);
        let theirs: [u8; 32] = r.bytes().unwrap().try_into().unwrap();
        let tag = r.bytes().unwrap().to_vec();
        let tags = pake::host_tags(&ours, &theirs, CLIENT, HOST).unwrap();
        pake::same(&tags.client, &tag).then_some(tags.host)
    }

    async fn mac_reads_request(rx: &mut quinn::RecvStream) {
        let (ty, body) = proto::read_msg(rx).await.unwrap().unwrap();
        assert_eq!(ty, t::PAIR_REQUEST);
        let mut r = R::new(&body);
        assert_eq!((r.u16().unwrap(), r.str().unwrap()), (LINK_VERSION, this_computer()));
    }

    fn typed(codes: &[&str]) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        for c in codes {
            tx.send(c.to_string()).unwrap();
        }
        rx
    }

    #[tokio::test]
    async fn the_right_code_links_after_a_wrong_one() {
        let (_client, conn) = to_a_mac(|mut tx, mut rx| async move {
            mac_reads_request(&mut rx).await;
            assert!(mac_try(&mut tx, &mut rx, "4821", 3).await.is_none());
            let tag = mac_try(&mut tx, &mut rx, "4821", 2).await.expect("the right code");
            tx.write_all(&result(0, &tag)).await.unwrap();
        })
        .await;
        let mut asked = vec![];
        let outcome = exchange(&conn, &found(), CLIENT, HOST, &mut typed(&["1111", "4821"]), |n, wrong| asked.push((n, wrong))).await.unwrap();
        assert_eq!(asked, [(3, false), (2, true)]);
        assert!(matches!(outcome, Outcome::Linked(p) if p.fingerprint == HOST && p.name == "Sam’s Air"));
    }

    #[tokio::test]
    async fn a_mac_without_the_code_isnt_linked() {
        // It answers "linked" whatever was typed, with a tag made up.
        let (_client, conn) = to_a_mac(|mut tx, mut rx| async move {
            mac_reads_request(&mut rx).await;
            mac_try(&mut tx, &mut rx, "0000", 3).await;
            tx.write_all(&result(0, &[7; 32])).await.unwrap();
        })
        .await;
        assert!(exchange(&conn, &found(), CLIENT, HOST, &mut typed(&["4821"]), |_, _| {}).await.is_err());
    }

    #[tokio::test]
    async fn the_mac_closing_its_code_window_ends_it_while_the_person_types() {
        let (_client, conn) = to_a_mac(|mut tx, mut rx| async move {
            mac_reads_request(&mut rx).await;
            let g = pake::generator("4821", CLIENT, HOST);
            let ours = pake::Share::random(&g).unwrap();
            tx.write_all(&W::new(t::PAIR_SHARE).u8(3).bytes(&ours.public).finish()).await.unwrap();
            tx.write_all(&result(1, &[])).await.unwrap();
        })
        .await;
        // Nothing typed: the channel stays open and empty.
        let (_keep, mut codes) = mpsc::unbounded_channel();
        assert_eq!(exchange(&conn, &found(), CLIENT, HOST, &mut codes, |_, _| {}).await.unwrap(), Outcome::CancelledThere);
    }

    #[tokio::test]
    async fn a_refusal_straight_away_is_the_answer() {
        let (_client, conn) = to_a_mac(|mut tx, mut rx| async move {
            mac_reads_request(&mut rx).await;
            tx.write_all(&result(5, &[])).await.unwrap();
        })
        .await;
        let outcome = exchange(&conn, &found(), CLIENT, HOST, &mut typed(&[]), |_, _| panic!("no code to type")).await.unwrap();
        assert_eq!(outcome, Outcome::AlreadyLinked);
    }

    #[test]
    fn instance_names_lose_the_service() {
        assert_eq!(instance_name("Sam’s MacBook Air._sweeflow._udp.local.", STREAM_SERVICE), "Sam’s MacBook Air");
    }
}
