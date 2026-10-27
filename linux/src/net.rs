//! QUIC connection to the Mac host. Runs on its own tokio runtime; talks to the Wayland thread
//! through channels.

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

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
    WindowAdded(WinInfo),
    WindowChanged { wid: u32, title: String, w: f32, h: f32, rel_x: f32, rel_y: f32 },
    WindowRemoved(u32),
    AppIcon { bundle_id: String, png: Vec<u8> },
    Cursor { hot_x: f32, hot_y: f32, scale: f32, png: Vec<u8> },
    Focused,
    /// Close / minimise / zoom button rects (x, y, w, h) in window points.
    WindowButtons(u32, [[f32; 4]; 3]),
    MoveStarted(u32),
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
            UiEvent::WindowAdded(_) => "window added",
            UiEvent::WindowChanged { .. } => "window changed",
            UiEvent::WindowRemoved(_) => "window removed",
            UiEvent::AppIcon { .. } => "app icon",
            UiEvent::Cursor { .. } => "cursor",
            UiEvent::Focused => "focused",
            UiEvent::WindowButtons(..) => "window buttons",
            UiEvent::MoveStarted(_) => "move started",
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
}

pub type UiTx = smithay_client_toolkit::reexports::calloop::channel::Sender<UiEvent>;

pub fn spawn(cfg: Config, ui: UiTx) -> mpsc::UnboundedSender<NetCmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = NET_TX.set(tx.clone());
    listen_local(tx.clone());
    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(run(cfg, ui, rx));
        })
        .unwrap();
    tx
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

/// Local socket for `sweeflow launch <bundle id>` (used by the Mac app launchers).
/// Launch requests made while the Mac is offline, delivered on (re)connect.
static PENDING_LAUNCHES: std::sync::Mutex<Vec<(String, std::time::Instant)>> = std::sync::Mutex::new(Vec::new());
/// The Mac's lock state as last reported (kept across reconnects).
static MAC_LOCKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CONNECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    loop {
        match session(&cfg, &endpoint, &ui, &mut cmds).await {
            Ok(()) => return, // UI went away
            Err(e) => {
                log::warn!("connection lost: {e:#}");
                let _ = ui.send(UiEvent::Disconnected(format!("{e:#}")));
            }
        }
        // Drop commands meant for the dead connection.
        while cmds.try_recv().is_ok() {}
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
    set_online(true);
    let _online = Online;

    let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
    ctl_tx.write_all(&W::new(t::HELLO).u16(proto::VERSION).f32(cfg.scale as f32).f32(cfg.capture_scale as f32).finish()).await?;
    let held: Vec<String> = PENDING_LAUNCHES.lock().unwrap().drain(..).map(|(b, _)| b).collect();
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
            while let Ok(d) = conn.read_datagram().await {
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
        t::SCREEN_LOCKED => {
            let locked = r.u8()? != 0;
            log::info!("Mac screen {}", if locked { "locked" } else { "unlocked" });
            // Once per lock, not on every reconnect while it stays locked (a sleeping Mac wakes
            // briefly now and then), and not at all while streaming is switched off.
            let was = MAC_LOCKED.swap(locked, std::sync::atomic::Ordering::SeqCst);
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
        t::PONG => return Ok(()),
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
            loop {
                let ready = match frx.recv_timeout(std::time::Duration::from_millis(20)) {
                    Ok(f) => order.push(f, std::time::Instant::now()),
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
                match dec.as_mut().unwrap().decode(&f.data, f.capture_us) {
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
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let (cert, key) = crate::config::client_identity()?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedHost { check, provider }))
        .with_client_auth_cert(vec![cert], PrivateKeyDer::Pkcs8(key.into()))?;
    tls.alpn_protocols = vec![alpn.to_vec()];

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
