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

impl WinInfo {
    /// Where a popup goes on its parent (window points): where the Mac has it, unless that's
    /// nowhere on the parent. The Mac keeps its windows side by side on its virtual display, so
    /// a panel it centres on that display (Quick Look's) can sit far off its window: placed by
    /// that offset here it opened off the side of the screen. Centred on the parent instead.
    pub fn spot_on(&self, parent_w: f32, parent_h: f32) -> (f32, f32) {
        let apart = self.rel_x >= parent_w || self.rel_x + self.w <= 0.0 || self.rel_y >= parent_h || self.rel_y + self.h <= 0.0;
        if apart {
            (((parent_w - self.w) / 2.0).round(), ((parent_h - self.h) / 2.0).max(0.0).round())
        } else {
            (self.rel_x, self.rel_y)
        }
    }
}

pub enum UiEvent {
    Connected(String),
    Disconnected(String),
    /// Streaming switched on (true) or paused (false), here or on the Mac.
    Streaming(bool),
    WindowAdded(WinInfo),
    WindowChanged { wid: u32, title: String, w: f32, h: f32, rel_x: f32, rel_y: f32 },
    WindowRemoved(u32),
    /// (old wid, new window): the Mac swapped that window for this one at the same frame (a
    /// tabbed window's tab switched). The window here stays and becomes the new one's.
    WindowReplaced(u32, WinInfo),
    /// The app won't let this window be resized; where its app put it on the Mac's screen
    /// (fractions of the room there: 0 left/top, 1 right/bottom), if said.
    /// The bool: fixed (false: a floating panel that can be resized, floated all the same).
    WindowFixed(u32, Option<(f32, f32)>, bool),
    AppIcon { bundle_id: String, png: Vec<u8> },
    Cursor { hot_x: f32, hot_y: f32, scale: f32, png: Vec<u8> },
    /// The Mac's key window changed (sent too as a session starts).
    Focused(u32),
    /// Square or rounded corners was switched (the tray): open windows follow now.
    CornersSetting,
    /// Keeping Mac windows at least as big as their app allows was switched (config.rs).
    /// Hyprland's view of this client's windows: (class, title, floating) each. It doesn't tell
    /// a window that it's tiled, as other desktops do (see hyprland::watch_tiling).
    Tiling(Vec<(String, String, bool)>),
    /// Close / minimise / zoom button rects (x, y, w, h) in window points.
    WindowButtons(u32, [[f32; 4]; 3]),
    MoveStarted(u32),
    /// (window, press id): that press can start a window move right away.
    MoveReady(u32, u32),
    /// (window, x, y, drags): hovering that point (window points) of the title-bar strip, the
    /// Mac found a spot that drags the window (or not), ahead of any press.
    DragSpot(u32, f32, f32, bool),
    ToggleMaximize(u32),
    Minimize(u32),
    /// An app launched here that already has windows: bring its newest back (unminimised, in
    /// front), with the launcher's activation token if it gave one.
    Raise { bundle_id: String, token: Option<String> },
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
            UiEvent::WindowReplaced(..) => "window replaced",
            UiEvent::WindowFixed(..) => "window fixed",
            UiEvent::AppIcon { .. } => "app icon",
            UiEvent::Cursor { .. } => "cursor",
            UiEvent::Focused(_) => "focused",
            UiEvent::CornersSetting => "corners setting",
            UiEvent::Tiling(_) => "tiling",
            UiEvent::WindowButtons(..) => "window buttons",
            UiEvent::MoveStarted(_) => "move started",
            UiEvent::MoveReady(..) => "move ready",
            UiEvent::DragSpot(..) => "drag spot",
            UiEvent::ToggleMaximize(_) => "toggle maximize",
            UiEvent::Minimize(_) => "minimize",
            UiEvent::Raise { .. } => "raise",
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

impl NetCmd {
    /// The Mac window `wid` resized to `size` (points), as it's shown here.
    pub fn resize(wid: u32, size: (u32, u32)) -> Self {
        NetCmd::Control(W::new(t::RESIZE).u32(wid).f32(size.0 as f32).f32(size.1 as f32).finish())
    }
}

pub type UiTx = smithay_client_toolkit::reexports::calloop::channel::Sender<UiEvent>;

/// The way to the window side, for the few things that start elsewhere (a setting in the tray).
static UI: std::sync::Mutex<Option<UiTx>> = std::sync::Mutex::new(None);

/// Whether streaming is on, as the UI was last told: 2 not told yet, taken as streaming.
static STREAMING_TOLD: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);

/// Tells the UI whether streaming is on, if that changed: as the Mac confirms its settings, and
/// first the moment Pause or Resume is chosen here. A Pause must find the UI paused before the
/// Mac lets go of the windows, or they go instead of staying as stills: when the Mac's
/// confirmation came after its window removals (seen on GNOME, right after a Stop and Start on
/// the Mac's menu, 4 s late), every window went.
pub fn tell_streaming(now: bool) {
    let before = STREAMING_TOLD.swap(now as u8, std::sync::atomic::Ordering::SeqCst);
    if before != now as u8 && !(before == 2 && now) {
        tell_ui(UiEvent::Streaming(now));
    }
}

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
/// How long a picture takes from its capture on the Mac to being shown here (µs, smoothed).
static VIDEO_LAG_US: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// When the last picture was shown (µs since EPOCH), and the time between pictures (smoothed).
static LAST_PICTURE_US: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static PICTURE_GAP_US: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Pictures are moving now (a video playing, something scrolling): over 12 a second lately.
fn pictures_moving() -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let since = EPOCH.elapsed().as_micros() as i64 - LAST_PICTURE_US.load(Relaxed);
    since < 200_000 && (1..83_000).contains(&PICTURE_GAP_US.load(Relaxed))
}

/// How much later than the pictures the sound may be heard and still seem to go with them.
const SYNC_SLACK_US: i64 = 60_000;

/// What sound spends on its way to being heard, besides the sound waiting for the player (µs):
/// the network to here (the last pings' average), the time this Mac took to hand it over, and
/// the player's own buffer.
fn sound_way_us() -> i64 {
    use std::sync::atomic::Ordering::Relaxed;
    let p = *PINGS.lock().unwrap();
    let transit = if p.0 > 0 { (p.4 / p.0 as i64).max(0) } else { 0 };
    transit + MAC_AUDIO_AGE_US.load(Relaxed) + PLAYER_LATENCY_MS as i64 * 1000
}

/// The most sound kept waiting for the player that still lets it be heard with the pictures
/// (give or take SYNC_SLACK_US), while pictures are moving; None otherwise. The cushion against
/// pieces arriving unevenly (audio_task) stays within it then: during a Quick Look video, pieces
/// came up to 173 ms apart, the cushion grew to 183 ms and the sound trailed the picture by
/// ~0.2 s. A gap now and then is the lesser harm there; with nothing moving (music), the full
/// cushion keeps the sound whole.
fn cushion_room_us() -> Option<u64> {
    if !pictures_moving() {
        return None;
    }
    let video = VIDEO_LAG_US.load(std::sync::atomic::Ordering::Relaxed);
    (video > 0).then(|| (video + SYNC_SLACK_US - sound_way_us()).max(0) as u64)
}

/// A picture captured at `capture_us` (the Mac's clock) was just shown: how long that took,
/// for keeping the sound in step with the pictures (audio_task). Smoothed, so one slow frame
/// doesn't move the sound.
pub fn video_shown(capture_us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let now = EPOCH.elapsed().as_micros() as i64;
    // How often pictures come (smoothed), for whether something is moving (`pictures_moving`).
    let last = LAST_PICTURE_US.swap(now, Relaxed);
    if last > 0 {
        let gap = (now - last).min(1_000_000);
        let old = PICTURE_GAP_US.load(Relaxed);
        PICTURE_GAP_US.store(if old == 0 { gap } else { old + (gap - old) / 8 }, Relaxed);
    }
    let Some((_, offset)) = *BEST_PING.lock().unwrap() else { return };
    let lag = now - (capture_us as i64 - offset);
    if !(0..1_000_000).contains(&lag) {
        return; // the clocks aren't matched up yet
    }
    let old = VIDEO_LAG_US.load(std::sync::atomic::Ordering::Relaxed);
    let new = if old == 0 { lag } else { old + (lag - old) / 32 };
    VIDEO_LAG_US.store(new, std::sync::atomic::Ordering::Relaxed);
}

/// How long after it played the Mac hands its sound over (µs), as it measures and sends with
/// each piece (ScreenCaptureKit: ~55 ms on macOS 26). Smoothed; 0 from a Mac that doesn't say.
static MAC_AUDIO_AGE_US: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// How much sound the player is kept ahead by (µs): what the network has lately made pieces
/// wait, so a late one doesn't run it dry (see audio_task). 0 until the first pieces have come.
static AUDIO_CUSHION_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The buffer we ask the player for (pw-cat's --latency, pacat's --latency-msec).
const PLAYER_LATENCY_MS: u64 = 20;
/// The sound's format (48 kHz, s16, stereo, as the player is told): bytes a millisecond.
const AUDIO_BYTES_PER_MS: usize = 48 * 2 * 2;

/// How much later than it arrives the sound should play, so it's heard with the pictures it
/// goes with, all from what's measured on this connection: the pictures' lag from capture to
/// screen, less what sound spends on its way (the network to here, the sound already waiting
/// for the player, `queued_us`, and the player's own buffer) and the time this Mac took to
/// hand it over at all. Never more than the pictures' lag.
fn audio_delay(queued_us: u64) -> std::time::Duration {
    use std::sync::atomic::Ordering::Relaxed;
    let video = VIDEO_LAG_US.load(Relaxed);
    let sound = sound_way_us() + queued_us as i64;
    std::time::Duration::from_micros((video - sound).clamp(0, video.max(0)) as u64)
}

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
/// The address the Mac in use was reached at, while connected.
static CONNECTED_TO: std::sync::Mutex<Option<SocketAddr>> = std::sync::Mutex::new(None);
/// The address each Mac (by fingerprint) was last reached at: still shown once it goes offline,
/// rather than the name it was linked by.
static LAST_REACHED: std::sync::Mutex<Vec<(String, SocketAddr)>> = std::sync::Mutex::new(Vec::new());
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
    // From the config: the Mac in use can change while Sweeflow runs (linking, switching).
    let f = crate::config::Config::load_or_default().map(|c| c.host_fingerprint).unwrap_or_default();
    (busy(&f), locked(&f))
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

/// The address the Mac in use is connected at, while it is.
pub fn connected_to() -> Option<SocketAddr> {
    *CONNECTED_TO.lock().unwrap()
}

/// The address the Mac with `fingerprint` was last reached at by this run, connected or not.
pub fn last_reached(fingerprint: &str) -> Option<SocketAddr> {
    LAST_REACHED.lock().unwrap().iter().find(|(f, _)| f == fingerprint).map(|(_, a)| *a)
}

/// How this computer reaches a Mac at `host` ("address:port", or a name with its port), for
/// the word beside it: "tailscale"; "vm" (a Mac in a virtual machine on this computer: this
/// computer's own address with the VM's ports forwarded, or a VM network's bridge); "lan";
/// "internet"; or "" (a name that doesn't say).
pub fn route_kind(host: &str) -> &'static str {
    use std::net::IpAddr;
    let ip = host.parse::<SocketAddr>().map(|a| a.ip()).ok().or_else(|| host.parse::<IpAddr>().ok());
    let Some(ip) = ip else {
        let name = host.rsplit_once(':').map_or(host, |(n, _)| n).trim_end_matches('.').to_ascii_lowercase();
        return if name.ends_with(".ts.net") {
            "tailscale"
        } else if name.ends_with(".local") {
            "lan"
        } else if name == "localhost" {
            "vm"
        } else {
            ""
        };
    };
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                "vm"
            } else if o[0] == 100 && (o[1] & 0xc0) == 64 {
                "tailscale"
            } else if vm_bridge(v4) {
                "vm"
            } else if v4.is_private() || v4.is_link_local() {
                "lan"
            } else {
                "internet"
            }
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if v6.is_loopback() {
                "vm"
            } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                "tailscale"
            } else if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 {
                "lan"
            } else {
                "internet"
            }
        }
    }
}

/// How a Mac is reached, in a few words (`kind` from `route_kind`): "Local: 192.168.1.20",
/// "Remote: 100.64.0.3" (Tailscale or the internet), "VM: 127.0.0.1"; empty if not known.
pub fn route_words(kind: &str, address: &str) -> String {
    let word = match kind {
        "lan" => "Local",
        "tailscale" | "internet" => "Remote",
        "vm" => "VM",
        _ => return String::new(),
    };
    let address = address.trim();
    if address.is_empty() { word.to_string() } else { format!("{word}: {address}") }
}

/// The address is on a virtual machines' network: the interface the kernel routes it through
/// is a VM bridge here (libvirt's virbr, VirtualBox's vboxnet, VMware's vmnet, a tap), or this
/// computer is itself a virtual machine and the address is its way out (its host, the Mac).
fn vm_bridge(ip: std::net::Ipv4Addr) -> bool {
    let routes = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    let bridge = route_iface(&routes, ip).is_some_and(|i| ["virbr", "vboxnet", "vmnet", "vnet", "tap", "incusbr", "lxdbr"].iter().any(|p| i.starts_with(p)));
    bridge || (default_gateway(&routes) == Some(ip) && in_a_vm())
}

/// The default route's gateway in /proc/net/route's table.
fn default_gateway(routes: &str) -> Option<std::net::Ipv4Addr> {
    routes.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.get(1) == Some(&"00000000") && f.get(7) == Some(&"00000000")).then(|| u32::from_str_radix(f.get(2)?, 16).ok().map(|v| std::net::Ipv4Addr::from(v.to_le_bytes())))?
    })
}

/// This computer is a virtual machine (as its firmware says: QEMU, UTM, Parallels, VMware,
/// VirtualBox, Apple's Virtualization).
fn in_a_vm() -> bool {
    static VM: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        ["sys_vendor", "product_name"].iter().any(|f| {
            let v = std::fs::read_to_string(format!("/sys/class/dmi/id/{f}")).unwrap_or_default().to_lowercase();
            ["qemu", "parallels", "vmware", "virtualbox", "innotek", "apple virtualization", "kvm", "utm"].iter().any(|n| v.contains(n))
        })
    });
    *VM
}

/// The interface /proc/net/route's table sends `ip` through (the longest matching prefix, the
/// default route left out: that's the way to everything else).
fn route_iface(routes: &str, ip: std::net::Ipv4Addr) -> Option<&str> {
    let ip = u32::from_be_bytes(ip.octets());
    // Addresses in the table are the network's bytes printed as a little-endian number.
    let field = |h: &str| u32::from_str_radix(h, 16).ok().map(|v| u32::from_be_bytes(v.to_le_bytes()));
    routes
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (dest, mask) = (field(f.get(1)?)?, field(f.get(7)?)?);
            (mask != 0 && ip & mask == dest).then_some((mask.count_ones(), f[0]))
        })
        .max_by_key(|r| r.0)
        .map(|r| r.1)
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
            // Whether this round learnt anything: an announcement seen, or the Mac asked now. A
            // failed ask is kept for 10 s; counted again each round, one dropped answer made
            // the Mac "not online" (Looks wants two misses in a row).
            let mut fresh = found;
            if !found {
                // Not seen announcing itself: asked at its last address instead (a virtual
                // machine's network carries no announcements, some Wi-Fi drops them).
                // The Mac in use more often: a Start pressed just after it was stopped elsewhere
                // was refused as "busy" for up to 10 s.
                let every = if *fp == in_use { Duration::from_secs(4) } else { Duration::from_secs(10) };
                let stale = probed.get(fp.as_str()).is_none_or(|(at, _)| at.elapsed() >= every);
                if stale {
                    let mac = crate::config::Config::load_or_default().ok().and_then(|c| c.macs.into_iter().find(|m| &m.fingerprint == fp));
                    let answer = mac.as_ref().and_then(crate::pair::probe);
                    probed.insert(fp.clone(), (Instant::now(), answer));
                    fresh = true;
                }
                if let Some((_, Some(p))) = probed.get(fp.as_str()) {
                    found = true;
                    said = *p;
                }
            }
            // Start pressed on the Mac in use, for this computer: started here too.
            if *fp == in_use && said.start && offline() {
                log::info!("started on the Mac: starting here too");
                set_offline(false);
                crate::tray::refresh();
            }
            for (set, on) in [(&BUSY, said.busy), (&LOCKED, said.locked)] {
                let mut set = set.lock().unwrap();
                changed |= if on { set.insert(fp.clone()) } else { set.remove(fp) };
            }
            if !fresh && !found {
                continue;
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
    crate::tray::set_elsewhere(false);
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
/// computer. (A look is a hello the Mac turns away:
/// the other computer doesn't notice.)
fn busy_wait(_checks: u32) -> Duration {
    // Every 5 s: streaming here starts within moments of it stopping there (backing off to
    // 30 s left Sweeflow showing "Busy" for half a minute after the Mac was free).
    Duration::from_secs(5)
}

/// How long to wait before asking again a Mac that disconnected this computer: 5 s at first
/// (Connect on the Mac is answered quickly), then 10, then every 20 s.
fn refused_wait(checks: u32) -> Duration {
    Duration::from_secs((5u64 << checks.min(2)).min(20))
}

/// The notifications: on first finding the Mac in use, and when it's free again. Neither names
/// the other computer (for privacy, nothing here knows or keeps which one it is).
fn busy_message(mac: &str) -> (String, String) {
    (format!("{mac} is busy"), format!("It's streaming elsewhere. Stop it there first; Sweeflow streams here once {mac} is free."))
}

fn free_message(mac: &str) -> (String, String) {
    (format!("{mac} is free: streaming here now"), "It stopped streaming elsewhere.".into())
}

/// The Mac turned this computer away: another one has it. Said once (not at every look). Older
/// Macs name that computer in Busy: not read, so its name is neither shown nor kept.
fn mac_busy() {
    if !crate::tray::elsewhere() {
        log::info!("the Mac is streaming elsewhere: waiting until it's free");
        let (summary, body) = busy_message(&crate::tray::mac_display_name());
        std::thread::spawn(move || notify(&summary, &body));
    }
    crate::tray::set_elsewhere(true);
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
                    if let Some(rest) = line.strip_prefix("launch ") {
                        // "launch <bundle id> [activation token]" (GNOME's tokens hold the
                        // entry's name, spaces and all: the rest of the line).
                        let mut parts = rest.trim().splitn(2, ' ');
                        let bid = parts.next().unwrap_or_default().to_string();
                        let token = parts.next().map(str::to_string).filter(|t| !t.is_empty());
                        if CONNECTED.load(std::sync::atomic::Ordering::SeqCst) {
                            log::info!("launching {bid} on the Mac");
                            tell_ui(UiEvent::Raise { bundle_id: bid.clone(), token });
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
    let mut refused_checks = 0;
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
                if offline() || crate::tray::elsewhere() {
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
        if crate::tray::elsewhere() {
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
        if crate::tray::mac_refused() {
            // Disconnected on the Mac: it lets this computer in again only after Connect there,
            // so asking every few seconds for good only fills its log. Less often the longer it
            // lasts (Stop or Start here asks again at once).
            let until = tokio::time::Instant::now() + refused_wait(refused_checks);
            refused_checks += 1;
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
        refused_checks = 0;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(3));
    }
}

/// Connects to the Mac in use wherever it answers first: at its last known address, and a moment
/// later at once at the other addresses it said it has (a network with no Bonjour: a VM's) and
/// where its announcement on the network says it is. All at once, so an address gone stale (a
/// new DHCP lease) costs nothing; it waited 5 s on every Start when tried one after another. The
/// config is read afresh for these: the address reached last becomes its address there.
async fn reach(cfg: &Config, endpoint: &quinn::Endpoint) -> Result<(SocketAddr, quinn::Connection)> {
    let fresh = Config::load_or_default().ok();
    let mac = fresh.as_ref().unwrap_or(cfg).macs.iter().find(|m| m.fingerprint == cfg.host_fingerprint)
        .or_else(|| cfg.macs.iter().find(|m| m.fingerprint == cfg.host_fingerprint));
    let places = mac.map(crate::config::places).unwrap_or_else(|| vec![cfg.host.clone()]);
    let saved = places[0].clone();
    // The last known address first: when it and another both answer (the local network and a
    // VPN), it's the one used.
    const HEAD_START: Duration = Duration::from_millis(300);
    let mut tries = tokio::task::JoinSet::new();
    for (i, place) in places.into_iter().enumerate() {
        let endpoint = endpoint.clone();
        tries.spawn(async move {
            let attempt = async {
                if i > 0 {
                    tokio::time::sleep(HEAD_START).await;
                }
                // Its name (Air.local) can take a while over multicast; numbers resolve at once.
                let wait = Duration::from_secs(if i == 0 { 5 } else { 2 });
                let addrs: Vec<SocketAddr> = tokio::time::timeout(wait, tokio::net::lookup_host(&place)).await.context("not found")??.collect();
                // The Mac's name can resolve to IPv6 addresses first; IPv4 is what the local
                // network (and a VPN's local-network exception) reliably carries.
                let addr = *addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).context("host did not resolve")?;
                // Not for ever: a handshake the Mac never answers would hold Start up indefinitely.
                let conn = tokio::time::timeout(Duration::from_secs(5), endpoint.connect(addr, "sweeflow")?).await.context("no answer")?;
                anyhow::Ok((addr, conn.context("connect")?))
            };
            let result = attempt.await;
            (place, result)
        });
    }
    // Where the network says it is now, found by its certificate.
    let fp = cfg.host_fingerprint.clone();
    let ep = endpoint.clone();
    tries.spawn(async move {
        let attempt = async {
            tokio::time::sleep(HEAD_START).await;
            let addr = tokio::task::spawn_blocking(move || crate::pair::locate(&fp, Duration::from_secs(2))).await?.context("not announced")?;
            let conn = tokio::time::timeout(Duration::from_secs(5), ep.connect(addr, "sweeflow")?).await.context("no answer")?;
            anyhow::Ok((addr, conn.context("connect")?))
        };
        (String::new(), attempt.await)
    });
    let mut saved_err = None;
    while let Some(done) = tries.join_next().await {
        let Ok((place, result)) = done else { continue };
        match result {
            Ok((addr, conn)) => {
                if place != saved {
                    let how = if place.is_empty() { format!("found the Mac at {addr}") } else { format!("reached the Mac at {place}") };
                    match &saved_err {
                        Some(e) => log::info!("{saved} not reachable ({e:#}); {how}"),
                        None => log::info!("{saved} slower to answer; {how}"),
                    }
                    // Its address from now on.
                    crate::config::remember_addresses(&cfg.host_fingerprint, Vec::new(), Some(addr.to_string()));
                }
                // The other attempts end with the set.
                return Ok((addr, conn));
            }
            Err(e) if place == saved => saved_err = Some(e),
            Err(_) => {}
        }
    }
    Err(saved_err.unwrap_or_else(|| anyhow::anyhow!("{saved}: no answer")))
}

async fn session(
    cfg: &Config,
    endpoint: &quinn::Endpoint,
    ui: &UiTx,
    cmds: &mut mpsc::UnboundedReceiver<NetCmd>,
) -> Result<()> {
    let (addr, conn) = reach(cfg, endpoint).await?;
    log::info!("connected to {addr}, rtt {:?}", conn.rtt());
    let ip = addr.ip();
    std::thread::spawn(move || crate::wake::remember(ip));
    *CONNECTED_TO.lock().unwrap() = Some(addr);
    {
        let mut last = LAST_REACHED.lock().unwrap();
        last.retain(|(f, _)| *f != cfg.host_fingerprint);
        last.push((cfg.host_fingerprint.clone(), addr));
    }
    struct Online;
    impl Drop for Online {
        fn drop(&mut self) {
            *CONNECTED_TO.lock().unwrap() = None;
            set_online(false);
            crate::tray::connected(None);
        }
    }
    // A look at a Mac in use by another computer: online only once it lets this one in (the
    // app menu's Mac apps don't come and go at every look), and launches wait.
    let busy = crate::tray::elsewhere();
    if !busy {
        set_online(true);
    }
    set_reachable(true);
    let _online = Online;

    let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
    let hello = W::new(t::HELLO).u16(proto::VERSION).f32(cfg.scale as f32).f32(0.0);   // (was the capture scale: the screen's own scale now goes with SCREEN_SIZE)
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
                if ty == t::CLIP_FILES {
                    // The Mac's clipboard files, being pasted here (see clipboard::wait_for_files).
                    let _ = tokio::task::spawn_blocking(move || crate::clipboard::files_arrived(&body)).await;
                    continue;
                }
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
        let ctx = Arc::new(tokio::sync::Mutex::new(ctx));
        // Messages from the clipboard code (a paste fetching the Mac's files).
        let (ftx, mut frx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        crate::clipboard::set_clip_stream(Some(ftx));
        let out = ctx.clone();
        tokio::spawn(async move {
            while let Some(m) = frx.recv().await {
                if out.lock().await.write_all(&m).await.is_err() {
                    break;
                }
            }
        });
        (ctx, task)
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

/// A WINDOW_ADDED body (also the tail of WINDOW_REPLACED's).
fn read_win_info(r: &mut R) -> Result<WinInfo> {
    Ok(WinInfo {
        wid: r.u32()?,
        pid: r.i32()?,
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

fn handle_control(ty: u8, body: &[u8], ui: &UiTx) -> Result<()> {
    let mut r = R::new(body);
    let ev = match ty {
        t::ADDRESSES => {
            let n = r.u16()?;
            let mut all = Vec::new();
            for _ in 0..n {
                all.push(r.str()?);
            }
            std::thread::spawn(move || {
                if let Ok(c) = crate::config::Config::load_or_default() {
                    crate::config::remember_addresses(&c.host_fingerprint, all, None);
                }
            });
            return Ok(());
        }
        t::WELCOME => {
            let _v = r.u16()?;
            let name = r.str()?;
            // Newer hosts say what sort of Mac they are (its icon here).
            if let Ok(kind) = r.str() {
                crate::tray::set_mac_kind(&kind);
            }
            if crate::tray::elsewhere() {
                log::info!("the Mac is free again: streaming here");
                crate::tray::set_elsewhere(false);
                set_online(true);
                let (summary, body) = free_message(&crate::tray::mac_display_name());
                std::thread::spawn(move || notify(&summary, &body));
            }
            crate::tray::connected(Some(name.clone()));
            UiEvent::Connected(name)
        }
        t::WINDOW_ADDED => UiEvent::WindowAdded(read_win_info(&mut r)?),
        t::WINDOW_REPLACED => {
            let old = r.u32()?;
            UiEvent::WindowReplaced(old, read_win_info(&mut r)?)
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
        t::WINDOW_FIXED => {
            let wid = r.u32()?;
            let spot = r.f32().and_then(|x| Ok((x, r.f32()?))).ok();
            let fixed = r.u8().map_or(true, |f| f != 0);
            UiEvent::WindowFixed(wid, spot, fixed)
        }
        t::APP_ICON => UiEvent::AppIcon { bundle_id: r.str()?, png: r.bytes()?.to_vec() },
        t::CURSOR => UiEvent::Cursor { hot_x: r.f32()?, hot_y: r.f32()?, scale: r.f32()?, png: r.bytes()?.to_vec() },
        t::FOCUSED => UiEvent::Focused(r.u32()?),
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
        t::DRAG_SPOT => UiEvent::DragSpot(r.u32()?, r.f32()?, r.f32()?, r.u8()? != 0),
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
                // The list above went out before the icons were written: on a first connection
                // there were none yet, and every app showed a blank icon until something else
                // changed. Again, with them.
                crate::tray::refresh();
            });
            return Ok(());
        }
        t::BUSY => {
            mac_busy();
            return Ok(());
        }
        t::MAC_DISCONNECTED => {
            // As this computer's own Stop: the Mac stays there, online, for Start on either side.
            log::info!("stopped on the Mac: stopped here too, until Start (here or there)");
            set_offline(true);
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
            tell_streaming(crate::tray::streaming());
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
                    // Pictures decoded on the CPU, for a GPU that can't: X11 draws them as
                    // textures, Wayland shows them from shared memory.
                    match Decoder::new_or_software(&render_node, !crate::x11ui::wanted()) {
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

/// The command that plays raw 48 kHz stereo 16-bit audio from a file: PipeWire's `pw-cat` where
/// it takes raw audio (PipeWire 1.2 and later), else PulseAudio's `pacat` (PipeWire answers as
/// PulseAudio; Ubuntu 24.04 / Mint 22 ship PipeWire 1.0), else ALSA's `aplay`. As its own
/// "sweeflow" stream in the mixer where the player can name it.
fn audio_player() -> Vec<String> {
    let has = |cmd: &str| std::process::Command::new(cmd).arg("--help").output().ok();
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if has("pw-cat").is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("--raw")) {
        return v(&["pw-cat", "--playback", "--raw", "--format", "s16", "--rate", "48000", "--channels", "2", "--latency", &*format!("{PLAYER_LATENCY_MS}ms"),
                   "-P", "{ application.name = \"sweeflow\" media.name = \"Mac audio\" node.name = \"sweeflow\" }"]);
    }
    if has("pacat").is_some() {
        log::info!("audio: through pacat (this PipeWire's pw-cat takes no raw audio)");
        return v(&["pacat", "--playback", "--raw", "--format=s16le", "--rate=48000", "--channels=2", &*format!("--latency-msec={PLAYER_LATENCY_MS}"),
                   "--client-name=sweeflow", "--stream-name=Mac audio"]);
    }
    log::info!("audio: through aplay");
    v(&["aplay", "-q", "-t", "raw", "-f", "S16_LE", "-r", "48000", "-c", "2"])
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
    // Quietly: on a first connect there's none to stop, which systemctl says in the journal.
    let stop = || {
        let _ = std::process::Command::new("systemctl").args(["--user", "stop", UNIT]).stderr(std::process::Stdio::null()).status();
    };
    stop();
    let started = std::process::Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect", "--unit", UNIT])
        .args(audio_player())
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
            let _ = std::process::Command::new("systemctl").args(["--user", "stop", UNIT]).stderr(std::process::Stdio::null()).status();
        }
    }
    let _player = Player;

    // Blocking FIFO writes happen on their own thread so the network reader never stalls.
    // Each piece is held back until it's due (audio_delay: in step with the pictures); one
    // that is late by more than a little is dropped, so a shorter delay takes effect at once
    // instead of the player's buffer growing.
    let (tx, prx) = std::sync::mpsc::sync_channel::<(std::time::Instant, Vec<u8>)>(256);
    let writer = std::thread::spawn(move || {
        use std::sync::atomic::Ordering::Relaxed;
        use std::time::{Duration, Instant};
        // Opening blocks until pw-cat opens its end.
        let Ok(mut pipe) = std::fs::OpenOptions::new().write(true).open(&fifo) else { return };
        // What waits in the pipe for the player, in bytes (FIONREAD works on either end).
        let queued = |p: &std::fs::File| -> usize {
            use std::os::fd::AsRawFd;
            let mut n: libc::c_int = 0;
            if unsafe { libc::ioctl(p.as_raw_fd(), libc::FIONREAD, &mut n) } == 0 { n.max(0) as usize } else { 0 }
        };
        let us_of = |bytes: usize| (bytes as u64 * 1000) / AUDIO_BYTES_PER_MS as u64;
        let bytes_of = |us: u64| (us * AUDIO_BYTES_PER_MS as u64 / 1000) as usize;
        // The cushion in bytes: what the network has made pieces wait lately (AUDIO_CUSHION_US),
        // within what keeps the sound with moving pictures (cushion_room_us), and never less
        // than two pieces.
        let cushion = |piece: usize| {
            let us = AUDIO_CUSHION_US.load(Relaxed);
            bytes_of(cushion_room_us().map_or(us, |room| us.min(room))).max(2 * piece)
        };
        let mut dropped = 0u32;
        // Run dry (or just started): written again only once the cushion's worth is ready.
        let mut priming = true;
        let mut piece = 0usize;
        let mut held: std::collections::VecDeque<(Instant, Vec<u8>)> = Default::default();
        // The delay in use: it follows the target only once that has been off it by more than
        // a piece for a while, as each change is heard (a gap when it grows, a skip when it
        // shrinks).
        let mut delay = Duration::ZERO;
        let mut off_since: Option<Instant> = None;
        loop {
            match prx.recv_timeout(Duration::from_millis(2)) {
                Ok(p) => {
                    piece = p.1.len();
                    held.push_back(p)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
            if piece == 0 {
                continue;
            }
            let now = Instant::now();
            let in_pipe = queued(&pipe);
            let target = audio_delay(us_of(in_pipe));
            if target.abs_diff(delay) > Duration::from_micros(us_of(piece)) {
                let since = *off_since.get_or_insert(now);
                if now - since > Duration::from_secs(2) {
                    delay = target;
                    off_since = None;
                    log::info!("audio: held {} ms after arriving, to go with the pictures", delay.as_millis());
                }
            } else {
                off_since = None;
            }
            if !priming && in_pipe == 0 {
                priming = true;
            }
            if priming {
                let ready: usize = held.iter().filter(|(arrived, _)| *arrived + delay <= now).map(|(_, p)| p.len()).sum();
                if ready < cushion(piece) {
                    continue;
                }
                priming = false;
            }
            while let Some((arrived, _)) = held.front() {
                let due = *arrived + delay;
                if due > now {
                    break;
                }
                let (_, pcm) = held.pop_front().unwrap();
                // Later than the cushion covers: dropped, so a shorter delay takes effect now.
                if now - due > Duration::from_micros(us_of(cushion(piece))) {
                    continue;
                }
                // The pipe is the player's cushion against pieces coming unevenly: kept to the
                // cushion and a piece more (and refilled to the cushion when it runs dry,
                // above). Unbounded (64 KB, a third of a second), whatever piled up in it once
                // (a burst at the start, network jitter) stayed as delay for good, the player
                // draining it only as fast as it plays: the sound came 150-300 ms after its
                // picture. Made small instead (4 KB), the player ran dry 75 times a second.
                if queued(&pipe) + pcm.len() > cushion(piece) + piece {
                    dropped += 1;
                    if dropped.is_power_of_two() {
                        log::info!("audio: {dropped} pieces dropped to keep the player's queue short");
                    }
                    continue;
                }
                if pipe.write_all(&pcm).is_err() {
                    return;
                }
            }
        }
    });
    log::info!("audio: playing Mac app sound");
    let (mut last_piece, mut pieces, mut gap_max, mut gap_logged) =
        (None::<std::time::Instant>, 0u32, std::time::Duration::ZERO, std::time::Instant::now());
    while let Ok(Some((ty, body))) = proto::read_msg(&mut rx).await {
        if ty != t::AUDIO {
            continue;
        }
        let mut r = R::new(&body);
        let Ok(pcm) = r.bytes().map(<[u8]>::to_vec) else { continue };
        use std::sync::atomic::Ordering::Relaxed;
        // How long after it played the Mac got it (newer Macs say): smoothed.
        if let Ok(age) = r.u32() {
            let old = MAC_AUDIO_AGE_US.load(Relaxed);
            MAC_AUDIO_AGE_US.store(if old == 0 { age as i64 } else { old + (age as i64 - old) / 16 }, Relaxed);
        }
        // The cushion follows how long pieces wait between them: up at once to the longest
        // wait (over a bumpy Wi-Fi pieces of 20 ms came up to 250 ms apart, then all at once),
        // back down every 10 s halfway towards the longest of those 10 s, to no less than two
        // pieces. A fixed cushion either ran dry on such waits (gaps) or kept the sound late.
        let now = std::time::Instant::now();
        let piece_us = (pcm.len() as u64 * 1000) / AUDIO_BYTES_PER_MS as u64;
        // (A wait of over a second is the sound stopping and starting again, not the network.)
        if let Some(prev) = last_piece.filter(|p| now - *p < std::time::Duration::from_secs(1)) {
            let gap = now - prev;
            gap_max = gap_max.max(gap);
            pieces += 1;
            let want = gap.as_micros() as u64 + piece_us;
            if want > AUDIO_CUSHION_US.load(Relaxed) {
                AUDIO_CUSHION_US.store(want, Relaxed);
            }
        }
        last_piece = Some(now);
        if gap_logged.elapsed() > std::time::Duration::from_secs(10) && pieces > 0 {
            let old = AUDIO_CUSHION_US.load(Relaxed);
            let new = ((old + gap_max.as_micros() as u64 + piece_us) / 2).max(2 * piece_us).min(old);
            AUDIO_CUSHION_US.store(new, Relaxed);
            let room = cushion_room_us().map_or(String::new(), |r| format!(" (at most {} ms while pictures move)", r / 1000));
            log::info!("audio: longest wait between pieces {} ms: cushion {} → {} ms{room}; the Mac hands sound over {} ms after it plays",
                       gap_max.as_millis(), old / 1000, new / 1000, MAC_AUDIO_AGE_US.load(Relaxed) / 1000);
            (pieces, gap_max, gap_logged) = (0, std::time::Duration::ZERO, std::time::Instant::now());
        }
        // If the player falls behind, drop audio rather than letting latency build up.
        let _ = tx.try_send((now, pcm));
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
    fn route_words_say_where() {
        assert_eq!(route_words("lan", "192.168.1.20"), "Local: 192.168.1.20");
        assert_eq!(route_words("tailscale", "100.64.0.3"), "Remote: 100.64.0.3");
        assert_eq!(route_words("internet", "8.8.8.8"), "Remote: 8.8.8.8");
        assert_eq!(route_words("vm", "127.0.0.1"), "VM: 127.0.0.1");
        assert_eq!(route_words("vm", ""), "VM");
        assert_eq!(route_words("", "mac.example.com"), "");
    }

    /// What handle_control makes of one message from the Mac.
    fn control_event(msg: Vec<u8>) -> UiEvent {
        use smithay_client_toolkit::reexports::calloop::{channel, EventLoop};
        let (tx, rx) = channel::channel();
        handle_control(msg[4], &msg[5..], &tx).unwrap();
        let mut got = Vec::new();
        let mut el: EventLoop<Vec<UiEvent>> = EventLoop::try_new().unwrap();
        el.handle().insert_source(rx, |e, _, got: &mut Vec<UiEvent>| {
            if let channel::Event::Msg(e) = e {
                got.push(e);
            }
        }).unwrap();
        el.dispatch(Some(Duration::ZERO), &mut got).unwrap();
        assert_eq!(got.len(), 1);
        got.pop().unwrap()
    }

    #[test]
    fn window_replaced_carries_the_old_id_and_the_new_window() {
        let added = |m: W| m.u32(42).i32(7).str("com.apple.finder").str("Finder").str("Downloads").f32(800.0).f32(500.0).u32(0).f32(1.0).f32(2.0).u8(0);
        let check = |i: &WinInfo| {
            assert_eq!((i.wid, i.pid, i.bundle_id.as_str(), i.app_name.as_str(), i.title.as_str()), (42, 7, "com.apple.finder", "Finder", "Downloads"));
            assert_eq!((i.w, i.h, i.parent, i.rel_x, i.rel_y, i.kind), (800.0, 500.0, 0, 1.0, 2.0, 0));
        };
        match control_event(added(W::new(t::WINDOW_REPLACED).u32(41)).finish()) {
            UiEvent::WindowReplaced(41, info) => check(&info),
            e => panic!("got {}", e.kind()),
        }
        match control_event(added(W::new(t::WINDOW_ADDED)).finish()) {
            UiEvent::WindowAdded(info) => check(&info),
            e => panic!("got {}", e.kind()),
        }
    }

    #[test]
    fn route_kinds() {
        assert_eq!(route_kind("100.64.0.3:44120"), "tailscale");
        assert_eq!(route_kind("[fd7a:115c:a1e0::1]:44120"), "tailscale");
        assert_eq!(route_kind("mac.tail1234.ts.net:44120"), "tailscale");
        assert_eq!(route_kind("192.168.1.20:44120"), "lan");
        assert_eq!(route_kind("10.0.0.5:44120"), "lan");
        assert_eq!(route_kind("Air.local:44120"), "lan");
        assert_eq!(route_kind("127.0.0.1:44120"), "vm");
        assert_eq!(route_kind("localhost:44120"), "vm");
        assert_eq!(route_kind("8.8.8.8:44120"), "internet");
        assert_eq!(route_kind("100.128.0.1:44120"), "internet");
        assert_eq!(route_kind("mac.example.com:44120"), "");
    }

    #[test]
    fn routes_through_the_longest_prefix() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                     wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                     wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n\
                     virbr0\t007AA8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(route_iface(table, "192.168.122.40".parse().unwrap()), Some("virbr0"));
        assert_eq!(route_iface(table, "192.168.1.9".parse().unwrap()), Some("wlan0"));
        assert_eq!(route_iface(table, "8.8.8.8".parse().unwrap()), None);
        assert_eq!(default_gateway(table), Some("192.168.1.1".parse().unwrap()));
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
    fn a_busy_mac_is_looked_at_every_few_seconds() {
        let waits: Vec<u64> = (0..6).map(|n| busy_wait(n).as_secs()).collect();
        assert_eq!(waits, [5, 5, 5, 5, 5, 5]);
    }

    #[test]
    fn a_mac_that_disconnected_this_computer_is_asked_less_and_less_often() {
        let waits: Vec<u64> = (0..7).map(|n| refused_wait(n).as_secs()).collect();
        assert_eq!(waits, [5, 10, 20, 20, 20, 20, 20]);
    }

    #[test]
    fn the_notifications_say_the_mac_is_busy() {
        assert_eq!(busy_message("Studio").0, "Studio is busy");
        assert!(busy_message("Studio").1.starts_with("It's streaming elsewhere"));
        assert_eq!(free_message("Studio").0, "Studio is free: streaming here now");
        assert!(!free_message("Studio").1.contains("workstation"));
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
