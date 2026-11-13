//! QUIC connection to the Mac host. Runs on its own tokio runtime; talks to the Wayland thread
//! through channels.

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::{Duration, Instant}};

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::frames::Encoded;
use crate::{
    config::Config,
    decode::{Decoder, GpuFrame},
    proto::{self, t, R, W},
};

#[derive(Debug, Clone)]
pub struct WinInfo {
    pub wid: u32,
    pub pid: i32,
    pub bundle_id: String,
    pub app_name: String,
    pub title: String,
    pub w: f32,
    pub h: f32,
    pub parent: u32,
    pub rel_x: f32,
    pub rel_y: f32,
    pub kind: u8,
}

pub enum UiEvent {
    Connected(String),
    Disconnected(String),
    /// Streaming switched on (true) or paused (false), here or on the Mac.
    Streaming(bool),
    WindowAdded(WinInfo),
    WindowChanged { wid: u32, title: String, w: f32, h: f32, rel_x: f32, rel_y: f32 },
    WindowRemoved(u32),
    AppIcon { bundle_id: String, png: Vec<u8> },
    Cursor { hot_x: f32, hot_y: f32, scale: f32, png: Vec<u8> },
    Focused,
    /// Square or rounded corners was switched (the tray): open windows follow now.
    CornersSetting,
    /// Hyprland's view of this client's windows: (class, title, floating) each. It doesn't tell
    /// a window that it's tiled, as other desktops do (see hyprland::watch_tiling).
    Tiling(Vec<(String, String, bool)>),
    /// Close / minimise / zoom button rects (x, y, w, h) in window points.
    WindowButtons(u32, [[f32; 4]; 3]),
    MoveStarted(u32),
    /// (window, press id): that press can start a window move right away.
    MoveReady(u32, u32),
    ToggleMaximize(u32),
    Minimize(u32),
    /// macOS's screenshot selection started (true) or ended: drawn here, as the Mac's can't be captured.
    ScreenSelect(bool),
    /// An app's menu bar (JSON from the Mac).
    MenuTree { pid: i32, json: Vec<u8> },
    Frame(u32, GpuFrame),
    /// A file drag started in a Mac window (file names).
    DragBegan { wid: u32, names: Vec<String> },
    /// The files of the Mac drag, saved locally, as a URI list.
    DragData(Vec<u8>),
    /// Mac clipboard contents, ready to offer: MIME type → bytes (files already saved).
    Clipboard(std::collections::HashMap<String, Arc<Vec<u8>>>),
    /// Rounded-corner tiles for a window: four `t_px`² premultiplied BGRA squares (TL, TR, BL, BR).
    Corners { wid: u32, t_px: u32, tiles: Vec<u8> },
    /// A Mac window that can't be captured: show a stand-in (see blind.rs).
    BlindShow { id: u32, app: String, title: String, texts: Vec<String>, buttons: Vec<String>, menu: bool },
    /// Lossless premultiplied BGRA (popups/menus), already decompressed.
    RawFrame { wid: u32, w: u16, h: u16, px_per_pt: u8, capture_us: u64, bgra: Vec<u8> },
}

impl UiEvent {
    /// Short name for diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            UiEvent::Connected(_) => "connected",
            UiEvent::Disconnected(_) => "disconnected",
            UiEvent::Streaming(_) => "streaming",
            UiEvent::WindowAdded(_) => "window added",
            UiEvent::WindowChanged { .. } => "window changed",
            UiEvent::WindowRemoved(_) => "window removed",
            UiEvent::AppIcon { .. } => "app icon",
            UiEvent::Cursor { .. } => "cursor",
            UiEvent::Focused => "focused",
            UiEvent::CornersSetting => "corners setting",
            UiEvent::Tiling(_) => "tiling",
            UiEvent::WindowButtons(..) => "window buttons",
            UiEvent::MoveStarted(_) => "move started",
            UiEvent::MoveReady(..) => "move ready",
            UiEvent::ToggleMaximize(_) => "toggle maximize",
            UiEvent::Minimize(_) => "minimize",
            UiEvent::ScreenSelect(_) => "screen select",
            UiEvent::MenuTree { .. } => "menu tree",
            UiEvent::Frame(..) => "video frame",
            UiEvent::Clipboard(_) => "clipboard",
            UiEvent::DragBegan { .. } => "drag began",
            UiEvent::DragData(_) => "drag data",
            UiEvent::Corners { .. } => "corners",
            UiEvent::BlindShow { .. } => "blind",
            UiEvent::RawFrame { .. } => "popup frame",
        }
    }
}

pub enum NetCmd {
    /// A framed control-stream message.
    Control(Vec<u8>),
    Subscribe(u32),
    Unsubscribe(u32),
    /// A framed message for a window's video stream.
    Video(u32, Vec<u8>),
    /// Linux clipboard contents for the Mac.
    Clipboard(Vec<crate::clipboard::ClipItem>),
    /// A framed message for the clipboard stream (drag and drop data).
    ClipStream(Vec<u8>),
    /// Stop or Start was chosen (see `set_offline`): look again.
    Recheck,
    /// Leaving on purpose (switching Macs, quitting): say so to the Mac, then close (the sender
    /// is told when that's done).
    Goodbye(std::sync::mpsc::Sender<()>),
}

pub type UiTx = smithay_client_toolkit::reexports::calloop::channel::Sender<UiEvent>;

/// The way to the window side, for the few things that start elsewhere (a setting in the tray).
static UI: std::sync::Mutex<Option<UiTx>> = std::sync::Mutex::new(None);

pub fn tell_ui(event: UiEvent) {
    if let Some(ui) = UI.lock().unwrap().as_ref() {
        let _ = ui.send(event);
    }
}

pub fn spawn(cfg: Config, ui: UiTx) -> mpsc::UnboundedSender<NetCmd> {
    *UI.lock().unwrap() = Some(ui.clone());
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = NET_TX.set(tx.clone());
    listen_local(tx.clone());
    let fingerprint = cfg.host_fingerprint.clone();
    std::thread::Builder::new().name("presence".into()).spawn(move || watch_presence(&fingerprint)).unwrap();
    // A ping on the control stream every 2 s: the round trip through QUIC and the Mac's
    // session queue, logged now and then (latency figures).
    let ping_tx = tx.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        if CONNECTED.load(std::sync::atomic::Ordering::Relaxed) {
            let us = EPOCH.elapsed().as_micros() as u64;
            let _ = ping_tx.send(NetCmd::Control(W::new(t::PING).u64(us).finish()));
        }
    });
    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(run(cfg, ui, rx));
        })
        .unwrap();
    tx
}

static EPOCH: std::sync::LazyLock<std::time::Instant> = std::sync::LazyLock::new(std::time::Instant::now);
static PINGS: std::sync::Mutex<(u32, u64, u64, i64, i64)> = std::sync::Mutex::new((0, 0, 0, 0, 0));
/// Quickest ping round trip seen (µs) and the Mac's clock minus ours then.
static BEST_PING: std::sync::Mutex<Option<(i64, i64)>> = std::sync::Mutex::new(None);

/// Each window's decoder input, for frames arriving as datagrams.
static FRAME_SINKS: std::sync::LazyLock<std::sync::Mutex<HashMap<u32, std::sync::mpsc::Sender<Encoded>>>> =
    std::sync::LazyLock::new(Default::default);
/// For decoders to ask the Mac for a keyframe (after a lost frame or a decode error).
static NET_TX: std::sync::OnceLock<mpsc::UnboundedSender<NetCmd>> = std::sync::OnceLock::new();

fn request_keyframe(wid: u32) {
    if let Some(tx) = NET_TX.get() {
        let _ = tx.send(NetCmd::Video(wid, W::new(t::REQUEST_KEYFRAME).finish()));
    }
}

/// Local socket for `sweeflow launch <bundle id>` and `sweeflow quit <bundle id>` (used by the
/// Mac app launchers).
/// Launch requests made while the Mac is offline, delivered on (re)connect.
static PENDING_LAUNCHES: std::sync::Mutex<Vec<(String, std::time::Instant)>> = std::sync::Mutex::new(Vec::new());
/// The Mac's lock state as last reported (kept across reconnects).
static MAC_LOCKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CONNECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Stopped on purpose (Stop): no connection until Start, so the Mac is free for another
/// computer. Kept across restarts, in a file beside the config.
static OFFLINE: std::sync::LazyLock<std::sync::atomic::AtomicBool> =
    std::sync::LazyLock::new(|| std::sync::atomic::AtomicBool::new(offline_file().exists()));
/// Whether the Mac in use can be reached: 0 not known yet, 1 no, 2 yes. Apart from streaming:
/// it is shown while stopped too (see `watch_presence`).
static REACHABLE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// The Mac in use (its fingerprint): the others are only ever looked for on the network.
static IN_USE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// The other linked Macs found online (true) or not (false), by fingerprint; missing: not known yet.
static OTHERS_ONLINE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, bool>>> = std::sync::LazyLock::new(Default::default);
/// Linked Macs this computer isn't streaming from that say they're streaming to a computer
/// (their network announcement), by fingerprint.
static BUSY: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);
/// The same, with their screen locked.
static LOCKED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);

/// A linked Mac this computer isn't connected to has its screen locked.
pub fn locked(fingerprint: &str) -> bool {
    LOCKED.lock().unwrap().contains(fingerprint)
}

/// What the Mac in use says of itself while stopped here: (busy streaming elsewhere, locked).
pub fn in_use_says() -> (bool, bool) {
    IN_USE.get().map_or((false, false), |f| (busy(f), locked(f)))
}

/// A linked Mac this computer isn't connected to is streaming to another computer.
pub fn busy(fingerprint: &str) -> bool {
    BUSY.lock().unwrap().contains(fingerprint)
}

fn offline_file() -> std::path::PathBuf {
    // The file's name is from when Stop was called Disconnect: kept, so the choice made then
    // still holds.
    crate::config::dir().join("disconnected")
}

/// The Mac in use is online (reachable on the network): None until known.
pub fn mac_online() -> Option<bool> {
    match REACHABLE.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        n => Some(n == 2),
    }
}

/// Whether a linked Mac is online: None until known.
pub fn online(fingerprint: &str) -> Option<bool> {
    if IN_USE.get().is_some_and(|f| f == fingerprint) {
        return mac_online();
    }
    OTHERS_ONLINE.lock().unwrap().get(fingerprint).copied()
}

fn set_reachable(online: bool) {
    let n = if online { 2 } else { 1 };
    if REACHABLE.swap(n, std::sync::atomic::Ordering::SeqCst) != n {
        log::info!("the Mac is {}", if online { "online" } else { "not online" });
        crate::tray::refresh();
    }
}

/// Looks of one Mac on the network, in a row: it counts as online once seen, and as not online
/// only after two looks in a row miss it (Wi-Fi drops multicast now and then).
#[derive(Default)]
struct Looks {
    misses: u32,
}

impl Looks {
    /// What one more look says: online, not online, or nothing new yet.
    fn after(&mut self, found: bool) -> Option<bool> {
        self.misses = if found { 0 } else { self.misses + 1 };
        if found {
            Some(true)
        } else if self.misses >= 2 {
            Some(false)
        } else {
            None
        }
    }
}

/// Whether each linked Mac is online, for the list of Macs and for the Mac in use while stopped.
/// While stopped, Sweeflow holds no connection to the Mac: it serves one computer at a time, and
/// even a connection closed straight after its handshake would end another computer's session
/// there (and the other linked Macs may well be serving another computer). A running Mac host
/// always advertises its streaming service on the local network (Bonjour, with the start of its
/// certificate's fingerprint), so every few seconds Sweeflow looks for that instead, which asks
/// nothing of any Mac's session. While started, the connection says for the Mac in use.
fn watch_presence(in_use: &str) {
    let _ = IN_USE.set(in_use.to_string());
    let mut looks: HashMap<String, Looks> = HashMap::new();
    // Macs asked directly (not seen on the network), and when: every 10 s at most each.
    let mut probed: HashMap<String, (Instant, Option<crate::pair::Presence>)> = HashMap::new();
    loop {
        // Read each time: Macs are linked and unlinked while Sweeflow runs.
        let linked: Vec<String> =
            crate::config::Config::load_or_default().map(|c| c.macs.into_iter().map(|m| m.fingerprint).collect()).unwrap_or_default();
        let wanted: Vec<&String> = linked.iter().filter(|f| *f != in_use || offline()).collect();
        if wanted.is_empty() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let seen = crate::pair::seen_on_network(Duration::from_secs(2));
        let mut changed = false;
        for fp in wanted {
            let id = fp.get(..16).unwrap_or_default();
            let mut found = seen.contains_key(id);
            // Busy: only while this computer isn't the one it streams to (stopped, or another Mac).
            let mut said = seen.get(id).copied().unwrap_or_default();
            if !found {
                // Not seen announcing itself: asked at its last address instead (a virtual
                // machine's network carries no announcements, some Wi-Fi drops them).
                let stale = probed.get(fp.as_str()).is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(10));
                if stale {
                    let mac = crate::config::Config::load_or_default().ok().and_then(|c| c.macs.into_iter().find(|m| &m.fingerprint == fp));
                    let answer = mac.as_ref().and_then(crate::pair::probe);
                    probed.insert(fp.clone(), (Instant::now(), answer));
                }
                if let Some((_, Some(p))) = probed.get(fp.as_str()) {
                    found = true;
                    said = *p;
                }
            }
            for (set, on) in [(&BUSY, said.busy), (&LOCKED, said.locked)] {
                let mut set = set.lock().unwrap();
                changed |= if on { set.insert(fp.clone()) } else { set.remove(fp) };
            }
            let Some(now) = looks.entry(fp.clone()).or_default().after(found) else { continue };
            if fp == in_use {
                // Started meanwhile: then the connection says, not this.
                if offline() {
                    set_reachable(now);
                }
            } else if OTHERS_ONLINE.lock().unwrap().insert(fp.clone(), now) != Some(now) {
                changed = true;
            }
        }
        looks.retain(|f, _| linked.contains(f));
        probed.retain(|f, _| linked.contains(f));
        OTHERS_ONLINE.lock().unwrap().retain(|f, _| linked.contains(f));
        // The Mac in use while started: the connection says (Busy), not this.
        BUSY.lock().unwrap().retain(|f| linked.contains(f) && (f != in_use || offline()));
        LOCKED.lock().unwrap().retain(|f| linked.contains(f) && (f != in_use || offline()));
        if changed {
            crate::tray::refresh();
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Tells the Mac this computer is leaving on purpose (so it tidies up at once, without waiting
/// for the connection to time out), and closes the connection. Waits a moment at most.
pub fn goodbye() {
    let Some(tx) = NET_TX.get() else { return };
    if !CONNECTED.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    if tx.send(NetCmd::Goodbye(done_tx)).is_ok() {
        let _ = done_rx.recv_timeout(std::time::Duration::from_millis(800));
    }
}

pub fn offline() -> bool {
    OFFLINE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Connected, and the Mac's screen is locked (its windows are paused until it's unlocked). The
/// Mac says on every connect and on every change, so this is current while connected.
pub fn mac_locked() -> bool {
    CONNECTED.load(std::sync::atomic::Ordering::SeqCst) && MAC_LOCKED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Stops (the Mac's windows go, the connection closes and the Mac is free for another computer)
/// and stays so, or starts again.
pub fn set_offline(on: bool) {
    // Stop ends waiting for a Mac in use elsewhere; Start looks afresh.
    crate::tray::set_elsewhere(None);
    if OFFLINE.swap(on, std::sync::atomic::Ordering::SeqCst) == on {
        return;
    }
    log::info!("{}", if on { "stopping, as asked" } else { "starting again, as asked" });
    let _ = if on { std::fs::write(offline_file(), "") } else { std::fs::remove_file(offline_file()) };
    if let Some(tx) = NET_TX.get() {
        let _ = tx.send(NetCmd::Recheck);
    }
}

/// This computer's name, as the Mac tells another computer it's streaming here.
fn this_computer() -> String {
    let h = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let h = h.trim();
    if h.is_empty() { "a Linux computer".into() } else { h.into() }
}

/// How long to wait before looking again whether the Mac is still streaming to another
/// computer, after `checks` looks: 5 s, rising to 30 s. (A look is a hello the Mac turns away:
/// the other computer doesn't notice.)
fn busy_wait(checks: u32) -> Duration {
    Duration::from_secs((5u64 << checks.min(3)).min(30))
}

/// Busy's payload: the computer the Mac is streaming to.
fn parse_busy(body: &[u8]) -> Result<String> {
    R::new(body).str()
}

/// The notifications: on first finding the Mac in use, and when it's free again.
fn busy_message(mac: &str, _other: &str) -> (String, String) {
    (format!("{mac} is busy"), format!("It's streaming elsewhere. Stop it there first; Sweeflow streams here once {mac} is free."))
}

fn free_message(mac: &str, other: &str) -> (String, String) {
    (format!("{mac} is free: streaming here now"), format!("{other} stopped streaming."))
}

/// The Mac turned this computer away: another one has it. Said once (not at every look).
fn mac_busy(other: String) {
    if crate::tray::elsewhere().is_none() {
        log::info!("the Mac is streaming to {other}: waiting until it's free");
        let (summary, body) = busy_message(&crate::tray::mac_display_name(), &other);
        std::thread::spawn(move || notify(&summary, &body));
    }
    crate::tray::set_elsewhere(Some(other));
}

/// Mac apps only appear in the app menu while the Mac is connected (after a grace period, so a
/// brief network blip doesn't make them flicker).
/// Connection changes, for `watch_launcher_visibility`.
static ONLINE_EVENTS: std::sync::OnceLock<std::sync::mpsc::Sender<bool>> = std::sync::OnceLock::new();

fn set_online(online: bool) {
    CONNECTED.store(online, std::sync::atomic::Ordering::SeqCst);
    if let Some(tx) = ONLINE_EVENTS.get() {
        let _ = tx.send(online);
    }
}

/// The Mac apps' launcher settings changed: show or hide the entries to match.
pub fn launcher_setting_changed() {
    set_online(CONNECTED.load(std::sync::atomic::Ordering::SeqCst));
}

/// Shows the Mac apps in the app menu while connected (unless switched off in the tray); hides
/// them once the Mac has been gone for a grace period (a network blip doesn't make them
/// flicker). Woken by connection and setting changes.
fn watch_launcher_visibility() {
    let (tx, rx) = std::sync::mpsc::channel();
    if ONLINE_EVENTS.set(tx).is_err() {
        return;
    }
    let grace = std::time::Duration::from_secs(20);
    // Until the first connection, the grace period applies from start.
    let mut online = CONNECTED.load(std::sync::atomic::Ordering::SeqCst);
    crate::launchers::set_visible(true);
    loop {
        let next = if online {
            rx.recv().ok()
        } else {
            match rx.recv_timeout(grace) {
                Ok(o) => Some(o),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    crate::launchers::set_visible(false);
                    // Offline: wait for the Mac without a timeout.
                    rx.recv().ok()
                }
                Err(_) => None,
            }
        };
        let Some(o) = next else { return };
        online = o;
        if online {
            crate::launchers::set_visible(crate::tray::launcher_wanted());
        }
    }
}

fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=sweeflow", "--icon=computer-laptop", summary, body])
        .status();
}

fn listen_local(tx: mpsc::UnboundedSender<NetCmd>) {
    use std::io::BufRead;
    let path = crate::launchers::socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = match std::os::unix::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("launch socket unavailable: {e}");
            return;
        }
    };
    std::thread::Builder::new()
        .name("launch-socket".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                for line in std::io::BufReader::new(conn).lines().map_while(Result::ok) {
                    if let Some(bid) = line.strip_prefix("launch ") {
                        let bid = bid.trim().to_string();
                        if CONNECTED.load(std::sync::atomic::Ordering::SeqCst) {
                            log::info!("launching {bid} on the Mac");
                            let _ = tx.send(NetCmd::Control(W::new(t::LAUNCH_APP).str(&bid).finish()));
                            if mac_locked() {
                                let name = crate::tray::app_name(&bid);
                                std::thread::spawn(move || {
                                    notify(
                                        "Your Mac is locked",
                                        &format!("{name} opens here once you unlock it (Touch ID or password on the Mac)."),
                                    )
                                });
                            }
                        } else {
                            // Hold it until we reconnect; tell the user if that takes too long.
                            log::info!("Mac offline; holding launch of {bid}");
                            PENDING_LAUNCHES.lock().unwrap().push((bid.clone(), std::time::Instant::now()));
                            std::thread::spawn(move || {
                                std::thread::sleep(std::time::Duration::from_secs(15));
                                let mut p = PENDING_LAUNCHES.lock().unwrap();
                                if let Some(i) = p.iter().position(|(b, _)| *b == bid) {
                                    p.remove(i);
                                    drop(p);
                                    notify("Mac not connected", &format!("Couldn't open {bid}: sweeflow can't reach your Mac."));
                                }
                            });
                        }
                    } else if let Some(bid) = line.strip_prefix("quit ") {
                        // A launcher's Quit action. Not held for later like launches: quitting
                        // an app some time after asking would only surprise.
                        let bid = bid.trim();
                        if CONNECTED.load(std::sync::atomic::Ordering::SeqCst) {
                            log::info!("quitting {bid} on the Mac");
                            let _ = tx.send(NetCmd::Control(W::new(t::QUIT_APP).str(bid).finish()));
                        } else {
                            log::info!("Mac offline; not quitting {bid}");
                        }
                    }
                }
            }
        })
        .unwrap();
}

async fn run(cfg: Config, ui: UiTx, mut cmds: mpsc::UnboundedReceiver<NetCmd>) {
    std::thread::spawn(watch_launcher_visibility);
    let endpoint = match make_endpoint(&cfg) {
        Ok(e) => e,
        Err(e) => {
            let _ = ui.send(UiEvent::Disconnected(format!("{e:#}")));
            return;
        }
    };
    let mut backoff = Duration::from_millis(250);
    // Looks at a Mac in use by another computer, in a row.
    let mut busy_checks = 0;
    loop {
        if offline() {
            let _ = ui.send(UiEvent::Disconnected("stopped, as asked".into()));
            // Any command wakes this to look again; only Start (a Recheck) changes anything.
            while offline() {
                if cmds.recv().await.is_none() {
                    return;
                }
            }
            backoff = Duration::from_millis(250);
        }
        match session(&cfg, &endpoint, &ui, &mut cmds).await {
            Ok(()) => return, // UI went away
            Err(e) => {
                let _ = ui.send(UiEvent::Disconnected(format!("{e:#}")));
                // Stopped on purpose, or turned away (the Mac was there a moment ago): still online.
                if offline() || crate::tray::elsewhere().is_some() {
                    log::info!("connection closed: {e:#}");
                } else {
                    log::warn!("connection lost: {e:#}");
                    set_reachable(false);
                }
            }
        }
        // Drop commands meant for the dead connection.
        while cmds.try_recv().is_ok() {}
        if offline() {
            continue;
        }
        if crate::tray::elsewhere().is_some() {
            // In use by another computer: look again in a while (Stop here ends the waiting).
            let until = tokio::time::Instant::now() + busy_wait(busy_checks);
            busy_checks += 1;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(until) => break,
                    cmd = cmds.recv() => {
                        if cmd.is_none() { return; }
                        if offline() { break; }
                    }
                }
            }
            continue;
        }
        busy_checks = 0;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(3));
    }
}

async fn session(
    cfg: &Config,
    endpoint: &quinn::Endpoint,
    ui: &UiTx,
    cmds: &mut mpsc::UnboundedReceiver<NetCmd>,
) -> Result<()> {
    // The Mac's name can resolve to IPv6 addresses first; IPv4 is what the local network (and a
    // VPN's local-network exception) reliably carries.
    let saved = async {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host(&cfg.host).await?.collect();
        let addr = *addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).context("host did not resolve")?;
        anyhow::Ok((addr, endpoint.connect(addr, "sweeflow")?.await.context("connect")?))
    };
    let (addr, conn) = match saved.await {
        Ok(c) => c,
        Err(e) => {
            // The Mac may have a new address: look for it on the network by its certificate.
            let fp = cfg.host_fingerprint.clone();
            let Some(addr) = tokio::task::spawn_blocking(move || crate::pair::locate(&fp, Duration::from_secs(2))).await?
            else {
                return Err(e);
            };
            log::info!("{} not reachable ({e:#}); found the Mac at {addr}", cfg.host);
            (addr, endpoint.connect(addr, "sweeflow")?.await.context("connect")?)
        }
    };
    log::info!("connected to {addr}, rtt {:?}", conn.rtt());
    let ip = addr.ip();
    std::thread::spawn(move || crate::wake::remember(ip));
    struct Online;
    impl Drop for Online {
        fn drop(&mut self) {
            set_online(false);
            crate::tray::connected(None);
        }
    }
    // A look at a Mac in use by another computer: online only once it lets this one in (the
    // app menu's Mac apps don't come and go at every look), and launches wait.
    let busy = crate::tray::elsewhere().is_some();
    if !busy {
        set_online(true);
    }
    set_reachable(true);
    let _online = Online;

    let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
    let hello = W::new(t::HELLO).u16(proto::VERSION).f32(cfg.scale as f32).f32(cfg.capture_scale as f32);
    ctl_tx.write_all(&hello.str(&this_computer()).finish()).await?;
    let held: Vec<String> = if busy { Vec::new() } else { PENDING_LAUNCHES.lock().unwrap().drain(..).map(|(b, _)| b).collect() };
    for bid in held {
        log::info!("launching {bid} (requested while offline)");
        ctl_tx.write_all(&W::new(t::LAUNCH_APP).str(&bid).finish()).await?;
    }

    let ui2 = ui.clone();
    let mut reader = tokio::spawn(async move {
        while let Some((ty, body)) = proto::read_msg(&mut ctl_rx).await? {
            if let Err(e) = handle_control(ty, &body, &ui2) {
                log::warn!("bad control message {ty:#x}: {e}");
            }
        }
        anyhow::Ok(())
    });

    // Delta video frames, as QUIC datagrams: put back together, then to their window's decoder.
    let datagram_task = {
        let conn = conn.clone();
        tokio::spawn(async move {
            let mut r = crate::frames::Reassembler::default();
            let mut since = std::time::Instant::now();
            while let Ok(d) = conn.read_datagram().await {
                if since.elapsed().as_secs() >= 5 && r.frames > 0 {
                    log::debug!(
                        "datagram frames: {} in {} pieces, first piece → last {:.1} ms on average, worst {:.1} ms",
                        r.frames, r.pieces, r.spread_us as f64 / r.frames as f64 / 1000.0, r.worst_us as f64 / 1000.0
                    );
                    (r.frames, r.pieces, r.spread_us, r.worst_us) = (0, 0, 0, 0);
                    since = std::time::Instant::now();
                }
                match r.push(&d, std::time::Instant::now()) {
                    Some((wid, Ok(f))) => {
                        if let Some(tx) = FRAME_SINKS.lock().unwrap().get(&wid) {
                            let _ = tx.send(f);
                        }
                    }
                    Some((wid, Err(e))) => log::warn!("window {wid}: bad datagram frame: {e}"),
                    None => {}
                }
            }
        })
    };

    // Mac app audio → PipeWire.
    let audio = {
        let (mut atx, arx) = conn.open_bi().await?;
        atx.write_all(&W::new(t::AUDIO_SUBSCRIBE).finish()).await?;
        tokio::spawn(audio_task(atx, arx))
    };

    // Clipboard: its own stream so large files never hold up input.
    let (clip_tx, clip_task) = {
        let (mut ctx, mut crx) = conn.open_bi().await?;
        ctx.write_all(&W::new(t::CLIPBOARD_SUBSCRIBE).finish()).await?;
        let ui3 = ui.clone();
        let task = tokio::spawn(async move {
            while let Ok(Some((ty, body))) = proto::read_msg(&mut crx).await {
                if ty == t::DRAG_DATA {
                    // The dragged Mac files: save them, hand the UI a URI list.
                    let parsed = crate::clipboard::decode(&body);
                    if let Ok(items) = parsed {
                        let offers = tokio::task::spawn_blocking(move || crate::clipboard::offers_for(&items, "drags")).await;
                        let uris = offers.ok().and_then(|o| o.get("text/uri-list").map(|v| v.to_vec())).unwrap_or_default();
                        let _ = ui3.send(UiEvent::DragData(uris));
                    }
                    continue;
                }
                if ty != t::CLIPBOARD {
                    continue;
                }
                match crate::clipboard::decode(&body) {
                    Ok(items) => {
                        // Saving files can take a while; keep it off the UI thread.
                        let offers = tokio::task::spawn_blocking(move || crate::clipboard::offers_for(&items, "clipboard")).await;
                        if let Ok(offers) = offers {
                            let _ = ui3.send(UiEvent::Clipboard(offers));
                        }
                    }
                    Err(e) => log::warn!("bad clipboard message: {e}"),
                }
            }
        });
        (Arc::new(tokio::sync::Mutex::new(ctx)), task)
    };

    let mut video: HashMap<u32, (quinn::SendStream, tokio::task::JoinHandle<()>)> = HashMap::new();
    let render_node = cfg.render_node.clone();
    let result = loop {
        tokio::select! {
            r = &mut reader => break r.map_err(anyhow::Error::from).and_then(|r| r).and(Err(anyhow::anyhow!("host closed the connection"))),
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                match cmd {
                    NetCmd::Recheck if offline() => {
                        let _ = ctl_tx.write_all(&W::new(t::GOODBYE).finish()).await;
                        let _ = ctl_tx.finish();
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        break Err(anyhow::anyhow!("stopped, as asked"));
                    }
                    NetCmd::Recheck => {}
                    NetCmd::Goodbye(done) => {
                        let _ = ctl_tx.write_all(&W::new(t::GOODBYE).finish()).await;
                        let _ = ctl_tx.finish();
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        conn.close(0u32.into(), b"bye");
                        let _ = done.send(());
                        break Err(anyhow::anyhow!("leaving"));
                    }
                    NetCmd::Control(m) => ctl_tx.write_all(&m).await?,
                    NetCmd::Subscribe(wid) => {
                        // A window minimised on the Mac loses its capture; its task has ended.
                        if video.get(&wid).is_some_and(|(_, task)| !task.is_finished()) { continue; }
                        let t0 = std::time::Instant::now();
                        // Opening waits for the Mac to allow another stream: never let that hold
                        // up input and acknowledgements queued behind it.
                        let (mut tx, rx) = match tokio::time::timeout(Duration::from_secs(2), conn.open_bi()).await {
                            Ok(r) => r?,
                            Err(_) => {
                                log::warn!("window {wid}: the Mac allows no more streams on this connection; reconnecting");
                                anyhow::bail!("out of QUIC streams");
                            }
                        };
                        let opened = t0.elapsed();
                        tx.write_all(&W::new(t::SUBSCRIBE).u32(wid).finish()).await?;
                        log::info!("window {wid}: video stream opened in {opened:?}, subscribed in {:?}", t0.elapsed());
                        let task = tokio::spawn(video_task(wid, rx, ui.clone(), render_node.clone()));
                        video.insert(wid, (tx, task));
                    }
                    NetCmd::Unsubscribe(wid) => {
                        if let Some((mut tx, task)) = video.remove(&wid) {
                            let _ = tx.finish();
                            task.abort();
                        }
                    }
                    NetCmd::Clipboard(_) if !crate::tray::allows_clipboard() => {}
                    NetCmd::Clipboard(items) => {
                        let tx = clip_tx.clone();
                        tokio::spawn(async move {
                            let msg = crate::clipboard::encode(&items);
                            if let Err(e) = tx.lock().await.write_all(&msg).await {
                                log::warn!("clipboard send failed: {e}");
                            }
                        });
                    }
                    NetCmd::ClipStream(msg) => {
                        let tx = clip_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tx.lock().await.write_all(&msg).await {
                                log::warn!("clipboard stream send failed: {e}");
                            }
                        });
                    }
                    NetCmd::Video(wid, m) => {
                        if let Some((tx, _)) = video.get_mut(&wid) {
                            if tx.write_all(&m).await.is_err() { video.remove(&wid); }
                        }
                    }
                }
            }
        }
    };
    for (_, (_, task)) in video.drain() {
        task.abort();
    }
    audio.abort();
    clip_task.abort();
    datagram_task.abort();
    conn.close(0u32.into(), b"bye");
    result
}

fn handle_control(ty: u8, body: &[u8], ui: &UiTx) -> Result<()> {
    let mut r = R::new(body);
    let ev = match ty {
        t::WELCOME => {
            let _v = r.u16()?;
            let name = r.str()?;
            // Newer hosts say what sort of Mac they are (its icon here).
            if let Ok(kind) = r.str() {
                crate::tray::set_mac_kind(&kind);
            }
            if let Some(other) = crate::tray::elsewhere() {
                log::info!("the Mac is free again: streaming here");
                crate::tray::set_elsewhere(None);
                set_online(true);
                let (summary, body) = free_message(&crate::tray::mac_display_name(), &other);
                std::thread::spawn(move || notify(&summary, &body));
            }
            crate::tray::connected(Some(name.clone()));
            UiEvent::Connected(name)
        }
        t::WINDOW_ADDED => {
            let wid = r.u32()?;
            let pid = r.i32()?;
            UiEvent::WindowAdded(WinInfo {
                wid,
                pid,
                bundle_id: r.str()?,
                app_name: r.str()?,
                title: r.str()?,
                w: r.f32()?,
                h: r.f32()?,
                parent: r.u32()?,
                rel_x: r.f32()?,
                rel_y: r.f32()?,
                kind: r.u8()?,
            })
        }
        t::WINDOW_CHANGED => UiEvent::WindowChanged {
            wid: r.u32()?,
            title: r.str()?,
            w: r.f32()?,
            h: r.f32()?,
            rel_x: r.f32()?,
            rel_y: r.f32()?,
        },
        t::WINDOW_REMOVED => UiEvent::WindowRemoved(r.u32()?),
        t::APP_ICON => UiEvent::AppIcon { bundle_id: r.str()?, png: r.bytes()?.to_vec() },
        t::CURSOR => UiEvent::Cursor { hot_x: r.f32()?, hot_y: r.f32()?, scale: r.f32()?, png: r.bytes()?.to_vec() },
        t::FOCUSED => {
            r.u32()?;
            UiEvent::Focused
        }
        t::WINDOW_BUTTONS => {
            let wid = r.u32()?;
            let mut rects = [[0f32; 4]; 3];
            for rect in &mut rects {
                for v in rect.iter_mut() {
                    *v = r.f32()?;
                }
            }
            UiEvent::WindowButtons(wid, rects)
        }
        t::MOVE_STARTED => UiEvent::MoveStarted(r.u32()?),
        t::MOVE_READY => UiEvent::MoveReady(r.u32()?, r.u32()?),
        t::TOGGLE_MAXIMIZE => UiEvent::ToggleMaximize(r.u32()?),
        t::MINIMIZE => UiEvent::Minimize(r.u32()?),
        t::SCREEN_SELECT => UiEvent::ScreenSelect(r.u8()? != 0),
        t::DRAG_BEGAN => {
            let wid = r.u32()?;
            let n = r.u16()?;
            let names = (0..n).map(|_| r.str()).collect::<Result<Vec<_>>>()?;
            UiEvent::DragBegan { wid, names }
        }
        t::MENU_TREE => UiEvent::MenuTree { pid: r.i32()?, json: r.bytes()?.to_vec() },
        t::APP_LIST => {
            let n = r.u16()?;
            let mut apps = Vec::with_capacity(n as usize);
            for _ in 0..n {
                apps.push((r.str()?, r.str()?, r.bytes()?.to_vec()));
            }
            crate::tray::set_apps(apps.iter().map(|(b, n, _)| (b.clone(), n.clone())).collect());
            std::thread::spawn(move || {
                if let Err(e) = crate::launchers::sync(&apps) {
                    log::warn!("launcher entries: {e:#}");
                }
            });
            return Ok(());
        }
        t::BUSY => {
            mac_busy(parse_busy(body)?);
            return Ok(());
        }
        t::MAC_DISCONNECTED => {
            log::info!("the Mac disconnected this computer: refused until it connects again");
            crate::tray::set_mac_refused(true);
            return Ok(());
        }
        t::FORGOTTEN => {
            log::info!("the Mac forgot this computer: forgetting it too");
            crate::tray::mac_forgot_us();
            return Ok(());
        }
        t::SCREEN_LOCKED => {
            let locked = r.u8()? != 0;
            log::info!("Mac screen {}", if locked { "locked" } else { "unlocked" });
            // Once per lock, not on every reconnect while it stays locked (a sleeping Mac wakes
            // briefly now and then), and not at all while streaming is switched off.
            let was = MAC_LOCKED.swap(locked, std::sync::atomic::Ordering::SeqCst);
            if locked != was {
                crate::tray::refresh();
            }
            if locked && !was && crate::tray::streaming() {
                std::thread::spawn(|| {
                    notify("Your Mac is locked", "Mac windows are paused until you unlock it (Touch ID or password on the Mac).")
                });
            }
            return Ok(());
        }
        t::BLIND_SHOW => {
            let id = r.u32()?;
            let (app, title) = (r.str()?, r.str()?);
            let texts = (0..r.u16()?).map(|_| r.str()).collect::<Result<Vec<_>>>()?;
            let buttons = (0..r.u16()?).map(|_| r.str()).collect::<Result<Vec<_>>>()?;
            // Newer hosts say whether it's a menu.
            let menu = r.u8().unwrap_or(0) == 1;
            log::info!("{app} {} {id} can't be shown here: stand-in with {} item(s): {title:?} {texts:?} {buttons:?}",
                       if menu { "menu" } else { "window" }, buttons.len());
            UiEvent::BlindShow { id, app, title, texts, buttons, menu }
        }
        t::SETTINGS => {
            crate::tray::set_settings(r.bytes()?);
            // Compared with what the UI was last told (the tray already shows a change made here
            // before the Mac confirms it). 2 = not told yet, taken as streaming.
            static TOLD: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            let now = crate::tray::streaming();
            let before = TOLD.swap(now as u8, std::sync::atomic::Ordering::SeqCst);
            if before != now as u8 && !(before == 2 && now) {
                let _ = ui.send(UiEvent::Streaming(now));
            }
            return Ok(());
        }
        t::RUNNING_APPS => {
            let n = r.u16()?;
            let mut running = Vec::with_capacity(n as usize);
            for _ in 0..n {
                running.push((r.str()?, r.u8()?));
            }
            crate::tray::set_running(running);
            return Ok(());
        }
        t::BLIND_GONE => {
            crate::blind::gone(r.u32()?);
            return Ok(());
        }
        t::PONG => {
            let now = EPOCH.elapsed().as_micros() as i64;
            let sent = r.u64()? as i64;
            let rtt = now - sent;
            // The Mac's clock when it got the ping: with the offset between the clocks taken
            // from the quickest round trip seen, how the time splits between the two ways.
            let (to_mac, from_mac) = match r.u64() {
                Ok(mac) => {
                    let mac = mac as i64;
                    let mut best = BEST_PING.lock().unwrap();
                    if best.map_or(true, |(r, _)| rtt < r) {
                        *best = Some((rtt, mac - (sent + rtt / 2)));
                    }
                    let offset = best.unwrap().1;
                    (mac - offset - sent, now - (mac - offset))
                }
                Err(_) => (0, 0),
            };
            let mut p = PINGS.lock().unwrap();
            *p = (p.0 + 1, p.1 + rtt as u64, p.2.max(rtt as u64), p.3 + to_mac, p.4 + from_mac);
            if p.0 >= 15 {
                let n = p.0 as f64 * 1000.0;
                log::info!(
                    "control round trip {:.1} ms on average (to the Mac {:.1}, back {:.1}), worst {:.1} ms",
                    p.1 as f64 / n, p.3 as f64 / n, p.4 as f64 / n, p.2 as f64 / 1000.0
                );
                *p = (0, 0, 0, 0, 0);
            }
            return Ok(());
        }
        _ => {
            log::debug!("ignoring control message {ty:#x}");
            return Ok(());
        }
    };
    ui.send(ev).map_err(|_| anyhow::anyhow!("ui gone"))
}


/// Receives encoded frames for one window and decodes them on a dedicated thread, so a slow
/// decode never stalls the network reader.
async fn video_task(wid: u32, mut rx: quinn::RecvStream, ui: UiTx, render_node: String) {
    // Unbounded is fine: the host never has more than a couple of frames in flight.
    let (ftx, frx) = std::sync::mpsc::channel::<Encoded>();
    // Frames also arrive as datagrams (see the session's datagram task).
    FRAME_SINKS.lock().unwrap().insert(wid, ftx.clone());
    struct Unregister(u32);
    impl Drop for Unregister {
        fn drop(&mut self) {
            FRAME_SINKS.lock().unwrap().remove(&self.0);
        }
    }
    let _unregister = Unregister(wid);
    let ui2 = ui.clone();
    std::thread::Builder::new()
        .name(format!("decode-{wid}"))
        .spawn(move || {
            // Created on the first HEVC frame; popups only ever send lossless frames.
            let mut dec: Option<Decoder> = None;
            let mut waiting_for_key = true;
            let mut order = crate::frames::Orderer::default();
            let mut reported_lost = 0;
            let mut last_report = std::time::Instant::now();
            // Where the time goes on this side: waiting to be put in order, and decoding.
            let mut arrived: std::collections::HashMap<u64, std::time::Instant> = Default::default();
            let (mut n, mut wait_us, mut dec_us, mut max_us) = (0u32, 0u128, 0u128, 0u128);
            let mut stats_since = std::time::Instant::now();
            loop {
                let ready = match frx.recv_timeout(std::time::Duration::from_millis(20)) {
                    Ok(f) => {
                        arrived.insert(f.capture_us, std::time::Instant::now());
                        order.push(f, std::time::Instant::now())
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => order.tick(std::time::Instant::now()),
                    Err(_) => break,
                };
                if ready.request_key {
                    request_keyframe(wid);
                }
                if order.lost != reported_lost && last_report.elapsed().as_secs() >= 10 {
                    log::info!("window {wid}: {} frame(s) lost in transit so far", order.lost);
                    reported_lost = order.lost;
                    last_report = std::time::Instant::now();
                }
                for f in ready.frames {
                if f.lossless {
                    let expected = f.w as usize * f.h as usize * 4;
                    match lz4_flex::decompress(&f.data, expected) {
                        Ok(px) if px.len() == expected => {
                            let ev = UiEvent::RawFrame { wid, w: f.w, h: f.h, px_per_pt: f.px_per_pt, capture_us: f.capture_us, bgra: px };
                            if ui2.send(ev).is_err() {
                                return;
                            }
                        }
                        other => log::warn!("window {wid}: bad lossless frame ({:?})", other.err()),
                    }
                    continue;
                }
                if waiting_for_key && !f.key {
                    continue;
                }
                waiting_for_key = false;
                if dec.is_none() {
                    match Decoder::new(&render_node) {
                        Ok(d) => dec = Some(d),
                        Err(e) => {
                            log::error!("window {wid}: {e:#}");
                            return;
                        }
                    }
                }
                let t0 = std::time::Instant::now();
                let decoded = dec.as_mut().unwrap().decode(&f.data, f.capture_us);
                if let Some(a) = arrived.remove(&f.capture_us) {
                    let total = a.elapsed().as_micros();
                    n += 1;
                    wait_us += t0.duration_since(a).as_micros();
                    dec_us += t0.elapsed().as_micros();
                    max_us = max_us.max(total);
                }
                arrived.retain(|_, a| a.elapsed().as_secs() < 2);
                if stats_since.elapsed().as_secs() >= 5 && n > 0 {
                    log::debug!(
                        "window {wid}: {n} frames, arrival→decoded: waiting {:.1} ms, decoding {:.1} ms (worst total {:.1} ms)",
                        wait_us as f64 / n as f64 / 1000.0, dec_us as f64 / n as f64 / 1000.0, max_us as f64 / 1000.0
                    );
                    (n, wait_us, dec_us, max_us) = (0, 0, 0, 0);
                    stats_since = std::time::Instant::now();
                }
                match decoded {
                    Ok(Some(mut frame)) => {
                        frame.dark_titlebar = f.dark;
                        frame.px_per_pt = f.px_per_pt;
                        if f.w > 0 && f.h > 0 {
                            frame.content = ((f.w as u32).min(frame.width), (f.h as u32).min(frame.height));
                        }
                        // The Mac knows the encoder's range; FFmpeg's VA-API frames don't report it.
                        frame.full_range = f.full_range;
                        if ui2.send(UiEvent::Frame(wid, frame)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("window {wid}: decode error {e:#}; waiting for keyframe");
                        waiting_for_key = true;
                        request_keyframe(wid);
                    }
                }
                }
            }
        })
        .unwrap();

    loop {
        let msg = match proto::read_msg(&mut rx).await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => {
                log::debug!("video stream {wid} ended: {e:#}");
                break;
            }
        };
        let (ty, body) = msg;
        if ty == t::CORNERS {
            let parsed = (|| -> Result<_> {
                let mut r = R::new(&body);
                let t_px = r.u16()? as u32;
                let data = r.bytes()?;
                let tiles = if t_px == 0 {
                    Vec::new()
                } else {
                    lz4_flex::decompress(data, (t_px * t_px * 16) as usize)?
                };
                Ok((t_px, tiles))
            })();
            match parsed {
                Ok((t_px, tiles)) => {
                    let _ = ui.send(UiEvent::Corners { wid, t_px, tiles });
                }
                Err(e) => log::warn!("bad corners for {wid}: {e}"),
            }
            continue;
        }
        if ty != t::FRAME {
            continue;
        }
        let parsed = crate::frames::parse_frame(&body);
        match parsed {
            Ok(f) => {
                if ftx.send(f).is_err() {
                    break;
                }
            }
            Err(e) => log::warn!("bad frame: {e}"),
        }
    }
}

/// Plays the Mac's audio through PipeWire (`pw-cat`), as its own "sweeflow" stream in the mixer.
///
/// The player runs as a transient systemd user unit reading a FIFO, not as our child: KDE's
/// taskbar marks every window of the process tree that plays audio, and all Mac windows belong
/// to this process, so a child player would put a speaker badge on every Mac app.
async fn audio_task(_keep_open: quinn::SendStream, mut rx: quinn::RecvStream) {
    use std::io::Write;
    let fifo = crate::launchers::socket_path().with_file_name("sweeflow-audio.fifo");
    let _ = std::fs::remove_file(&fifo);
    if !std::process::Command::new("mkfifo").arg(&fifo).status().is_ok_and(|s| s.success()) {
        log::warn!("audio disabled: cannot create {}", fifo.display());
        return;
    }
    const UNIT: &str = "sweeflow-audio";
    let _ = std::process::Command::new("systemctl").args(["--user", "stop", UNIT]).status();
    let started = std::process::Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect", "--unit", UNIT, "pw-cat", "--playback", "--raw"])
        .args(["--format", "s16", "--rate", "48000", "--channels", "2", "--latency", "20ms"])
        .args(["-P", "{ application.name = \"sweeflow\" media.name = \"Mac audio\" node.name = \"sweeflow\" }"])
        .arg(&fifo)
        .status();
    if !started.is_ok_and(|s| s.success()) {
        log::warn!("audio disabled: cannot start pw-cat");
        return;
    }
    /// Stops the player however this task ends (including being aborted on disconnect).
    struct Player;
    impl Drop for Player {
        fn drop(&mut self) {
            let _ = std::process::Command::new("systemctl").args(["--user", "stop", UNIT]).status();
        }
    }
    let _player = Player;

    // Blocking FIFO writes happen on their own thread so the network reader never stalls.
    let (tx, prx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
    let writer = std::thread::spawn(move || {
        // Opening blocks until pw-cat opens its end.
        let Ok(mut pipe) = std::fs::OpenOptions::new().write(true).open(&fifo) else { return };
        while let Ok(pcm) = prx.recv() {
            if pipe.write_all(&pcm).is_err() {
                break;
            }
        }
    });
    log::info!("audio: playing Mac app sound");
    while let Ok(Some((ty, body))) = proto::read_msg(&mut rx).await {
        if ty != t::AUDIO {
            continue;
        }
        let Ok(pcm) = R::new(&body).bytes().map(<[u8]>::to_vec) else { continue };
        // If the player falls behind, drop audio rather than letting latency build up.
        let _ = tx.try_send(pcm);
    }
    drop(tx);
    let _ = writer.join();
}

// --- TLS -----------------------------------------------------------------------------------

/// Decides whether a Mac's certificate (its SHA-256) is the one we want: the paired Mac's
/// fingerprint when streaming, the code the person typed when pairing.
pub type HostCheck = Arc<dyn Fn(&[u8]) -> Result<(), String> + Send + Sync>;

struct PinnedHost {
    check: HostCheck,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl std::fmt::Debug for PinnedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PinnedHost")
    }
}

impl ServerCertVerifier for PinnedHost {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = Sha256::digest(end.as_ref());
        (self.check)(got.as_slice()).map_err(rustls::Error::General)?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn make_endpoint(cfg: &Config) -> Result<quinn::Endpoint> {
    let want = hex::decode(&cfg.host_fingerprint).context("bad host fingerprint in config")?;
    let check: HostCheck = Arc::new(move |got: &[u8]| {
        if got == want.as_slice() {
            Ok(())
        } else {
            Err(format!("host certificate {} does not match the paired Mac; pair it again", hex::encode(got)))
        }
    });
    client_endpoint(check, b"sweeflow/1")
}

/// A QUIC client endpoint with our client certificate, accepting the Mac `check` approves.
pub fn client_endpoint(check: HostCheck, alpn: &[u8]) -> Result<quinn::Endpoint> {
    client_endpoint_offering(check, &[alpn])
}

/// The same, offering several ALPNs (the server picks one).
pub fn client_endpoint_offering(check: HostCheck, alpns: &[&[u8]]) -> Result<quinn::Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let (cert, key) = crate::config::client_identity()?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedHost { check, provider }))
        .with_client_auth_cert(vec![cert], PrivateKeyDer::Pkcs8(key.into()))?;
    tls.alpn_protocols = alpns.iter().map(|a| a.to_vec()).collect();

    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
    transport.stream_receive_window((16u32 << 20).into());
    transport.receive_window((64u32 << 20).into());
    transport.initial_rtt(Duration::from_millis(10));
    let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    client.transport_config(Arc::new(transport));

    let mut ep = quinn::Endpoint::client("[::]:0".parse()?)?;
    ep.set_default_client_config(client);
    Ok(ep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_decodes() {
        let m = W::new(t::BUSY).str("Größe-VM").finish();
        assert_eq!(m[4], 0x06);
        assert_eq!(parse_busy(&m[5..]).unwrap(), "Größe-VM");
        assert!(parse_busy(&[]).is_err(), "Busy names the other computer");
    }

    #[test]
    fn hello_ends_with_the_computer_name() {
        // What the Mac reads after the older fields.
        let m = W::new(t::HELLO).u16(1).f32(1.0).f32(0.0).str("box").finish();
        let mut r = R::new(&m[5..]);
        (r.u16().unwrap(), r.f32().unwrap(), r.f32().unwrap());
        assert_eq!(r.str().unwrap(), "box");
    }

    #[test]
    fn a_busy_mac_is_looked_at_less_and_less_often() {
        let waits: Vec<u64> = (0..6).map(|n| busy_wait(n).as_secs()).collect();
        assert_eq!(waits, [5, 10, 20, 30, 30, 30]);
    }

    #[test]
    fn the_notifications_say_the_mac_is_busy() {
        assert_eq!(busy_message("Studio", "workstation").0, "Studio is busy");
        assert!(busy_message("Studio", "workstation").1.starts_with("It's streaming elsewhere"));
        assert_eq!(free_message("Studio", "workstation").0, "Studio is free: streaming here now");
    }

    #[test]
    fn a_mac_is_not_online_only_after_two_missed_looks() {
        let mut looks = Looks::default();
        assert_eq!(looks.after(false), None);
        assert_eq!(looks.after(false), Some(false));
        assert_eq!(looks.after(false), Some(false));
        assert_eq!(looks.after(true), Some(true));
        // One look missed after being seen: still online, as far as anyone can tell.
        assert_eq!(looks.after(false), None);
        assert_eq!(looks.after(true), Some(true));
    }
}
