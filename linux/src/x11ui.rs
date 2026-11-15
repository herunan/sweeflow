//! X11 frontend: one X window per Mac window, as ui.rs does on Wayland (Linux Mint's Cinnamon,
//! any X11 session). Picked at start (see `wanted`); it takes the same UiEvents and sends the
//! same NetCmds as ui.rs, whose window logic it follows (sizes asked of the Mac, refusals, fixed
//! windows, hide-on-close, paused stills, local traffic lights, title-bar drags).
//!
//! What differs is how a window is shown. A Mac window is a window without the desktop's frame
//! (_MOTIF_WM_HINTS) on a 32-bit visual, so its rounded corners and whatever the picture doesn't
//! cover show the desktop behind; menus and popovers (kind 1) are override-redirect windows
//! kept at their spot on the parent; dialogs and fixed-size windows are _NET_WM_WINDOW_TYPE_DIALOG,
//! transient for their window where ui.rs gives them a parent. Everything in a window is drawn
//! with OpenGL ES (x11gl.rs): the decoder's dmabufs imported through EGL, the corner tiles, the
//! traffic lights and stills as textures. Moves and resizes go to the window manager with
//! _NET_WM_MOVERESIZE.
//!
//! Phase 2, not here yet: the clipboard (UiEvent::Clipboard; X selections), drag and drop of
//! files (XDND; UiEvent::DragBegan / DragData), and registering Sweeflow's shortcuts on Cinnamon.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context as _, Result};
use smithay_client_toolkit::reexports::calloop::{
    channel::{self, Channel},
    generic::Generic,
    timer::{TimeoutAction, Timer},
    EventLoop, Interest, LoopHandle, Mode, PostAction,
};
use tokio::sync::mpsc::UnboundedSender;
use x11rb::{
    connection::{Connection, RequestConnection as _},
    properties::{WmHints, WmSizeHints, WmSizeHintsSpecification},
    protocol::{
        randr::ConnectionExt as _,
        render::{self, ConnectionExt as _},
        shape::{self, ConnectionExt as _},
        xinput::{self, ConnectionExt as _},
        xkb::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _, EventMask, PropMode},
        Event,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    CURRENT_TIME,
};

use crate::{
    decode::GpuFrame,
    lights,
    menus::Menus,
    net::{NetCmd, UiEvent, WinInfo},
    proto::{t, W},
    ui::{display_title, edge_at, mac_button, refused, resize_wait, Edge, Refused, paused_still, px_per_pt, shown_part, square_tiles, PausedStill, KEY_SPACE, MIN_SIZE},
    x11gl::{Blend, Gl, Quad, Target, Tex, Video},
};

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_CHANGE_STATE,
        WM_CLIENT_MACHINE,
        UTF8_STRING,
        _NET_WM_PING,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_ICON,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_WM_WINDOW_TYPE_POPUP_MENU,
        _NET_WM_STATE,
        _NET_WM_STATE_MODAL,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_HIDDEN,
        _NET_ACTIVE_WINDOW,
        _NET_WM_MOVERESIZE,
        _NET_WORKAREA,
        _MOTIF_WM_HINTS,
        _KDE_NET_WM_APPMENU_SERVICE_NAME,
        _KDE_NET_WM_APPMENU_OBJECT_PATH,
        XdndAware,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndSelection,
        XdndTypeList,
        XdndActionCopy,
        SWEEFLOW_DROP,
        INCR,
    }
}

/// Whether to use X11 rather than Wayland: a session with only an X display, or
/// `SWEEFLOW_BACKEND=x11` (XWayland, for testing). `SWEEFLOW_BACKEND=wayland` forces Wayland.
pub fn wanted() -> bool {
    choose(std::env::var("SWEEFLOW_BACKEND").ok().as_deref(), std::env::var_os("WAYLAND_DISPLAY").is_some(), std::env::var_os("DISPLAY").is_some())
}

fn choose(forced: Option<&str>, wayland: bool, x11: bool) -> bool {
    match forced {
        Some("x11") => true,
        Some("wayland") => false,
        _ => !wayland && x11,
    }
}

/// Modifiers held, as the key's event says (X's core modifier bits).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Mods {
    logo: bool,
    alt: bool,
    ctrl: bool,
    shift: bool,
}

impl Mods {
    fn from_state(state: u16) -> Mods {
        // Shift, Control, Mod1 (Alt), Mod4 (Super) on every common layout.
        Mods { shift: state & 0x01 != 0, ctrl: state & 0x04 != 0, alt: state & 0x08 != 0, logo: state & 0x40 != 0 }
    }
}

/// Cmd+Opt+<letter> alone (Super+Alt here): the keys ui.rs catches in Mac windows. `keysym`:
/// what the layout puts on the key, unshifted.
fn cmd_opt_letter(m: Mods, keysym: u32) -> Option<char> {
    if !(m.logo && m.alt && !m.ctrl && !m.shift) {
        return None;
    }
    let k = if (0x41..=0x5a).contains(&keysym) { keysym + 0x20 } else { keysym };
    (0x61..=0x7a).contains(&k).then(|| k as u8 as char)
}

/// X keycode → the evdev code the Mac is sent (what Wayland gives as the raw code).
fn evdev_code(x_keycode: u8) -> u32 {
    (x_keycode as u32).saturating_sub(8)
}

/// X pointer button → Linux button code (BTN_*), for the Mac's numbering (ui::mac_button).
fn linux_button(x: u8) -> Option<u32> {
    Some(match x {
        1 => 0x110, // left
        2 => 0x112, // middle
        3 => 0x111, // right
        8 => 0x113, // side (back)
        9 => 0x114, // extra (forward)
        4..=7 => return None, // the wheel
        b => 0x110 + b as u32 - 1,
    })
}


impl Edge {
    fn cursor(self) -> &'static str {
        match self {
            Edge::TopLeft => "top_left_corner",
            Edge::Top => "top_side",
            Edge::TopRight => "top_right_corner",
            Edge::Right => "right_side",
            Edge::BottomRight => "bottom_right_corner",
            Edge::Bottom => "bottom_side",
            Edge::BottomLeft => "bottom_left_corner",
            Edge::Left => "left_side",
        }
    }
}

const MOVERESIZE_MOVE: u32 = 8;


/// Where a floating window goes for a spot on the Mac's screen (fractions of the room around
/// it), in a work area (x, y, w, h).
fn spot_position(area: (i32, i32, i32, i32), size: (u32, u32), spot: (f32, f32)) -> (i32, i32) {
    let (fx, fy) = (spot.0.clamp(0.0, 1.0) as f64, spot.1.clamp(0.0, 1.0) as f64);
    let room = |a: i32, s: u32| (a - s as i32).max(0) as f64;
    (area.0 + (fx * room(area.2, size.0)).round() as i32, area.1 + (fy * room(area.3, size.1)).round() as i32)
}

/// A picture waiting for the GPU: pixels to upload with the window's next drawing.
#[derive(Default)]
struct Slot {
    pending: Option<(u32, u32, Vec<u8>)>,
    tex: Option<Tex>,
}

struct Corners {
    /// Corner tile size in frame pixels (0 = square corners).
    t_px: u32,
    /// The tiles as the Mac sent them, and whether those shown were squared (the setting).
    raw: Option<(u32, Vec<u8>)>,
    squared: bool,
    /// The tiles in use (their alpha shapes a paused still too).
    last: Option<(u32, Vec<u8>)>,
    /// The four tiles stacked (TL, TR, BL, BR), t × 4t.
    slot: Slot,
}

struct Still {
    slot: Slot,
    /// Where it goes: position, the part of its picture (px) and its size in the window.
    pos: (i32, i32),
    src: (f64, f64),
    dst: (i32, i32),
}

struct Win {
    info: WinInfo,
    xid: u32,
    popup: bool,
    target: Option<Target>,
    /// Mapped (the first configure, on Wayland).
    configured: bool,
    /// Size of the window (as ui.rs's), and the size X last said it has.
    size: (u32, u32),
    x_size: (u32, u32),
    min_size: (u32, u32),
    max_size: Option<(u32, u32)>,
    fixed: bool,
    floated: bool,
    first_shape: Option<f64>,
    spot: Option<(f32, f32)>,
    refusal: Option<((u32, u32), (u32, u32))>,
    followed: bool,
    requested: Option<((u32, u32), Instant)>,
    floating: bool,
    maximized: bool,
    /// Minimised here (_NET_WM_STATE_HIDDEN).
    suspended: bool,
    buttons: [[f32; 4]; 3],
    lights: Slot,
    lights_drawn: Option<(lights::State, [f64; 4])>,
    corners: Option<Corners>,
    dark_titlebar: bool,
    mac_minimized: bool,
    lights_hover: bool,
    lights_pressed: Option<(usize, bool)>,
    /// A frame waiting to be shown (not mapped yet, or hidden for closing).
    pending: Option<GpuFrame>,
    /// The frame to import with the next drawing.
    next: Option<GpuFrame>,
    /// The frame shown, imported.
    shown: Option<(GpuFrame, Video)>,
    picture: Option<((u32, u32), u8)>,
    still: Option<Still>,
    hidden: Option<Instant>,
    pending_raw: Option<(u16, u16, u8, u64, Vec<u8>)>,
    /// A lossless picture shown (popups), and its pixels per point.
    raw: Option<(Slot, u8)>,
    stale: bool,
    popup_wait: bool,
    /// Where a popup was dragged to on its window (window points), kept from then on over where
    /// the Mac has it (Quick Look dragged by its bar).
    moved_to: Option<(f32, f32)>,
    popup_settled: bool,
    check_pending: bool,
    display: bool,
    fit: (f64, f64, f64),
}

/// The screenshot selection being dragged (as ui.rs's Selection).
struct Selection {
    wid: u32,
    start: (f64, f64),
    now: (f64, f64),
}

/// A scroll valuator of a pointer device (XInput 2).
#[derive(Clone, Copy)]
struct ScrollAxis {
    number: u16,
    vertical: bool,
    increment: f64,
    last: Option<f64>,
}

/// A file drag out of a Mac window, carried on to Linux apps (XDND, Sweeflow the source): the
/// Mac window it came from, the Linux window under the pointer that takes drops (and whether it
/// said it would take this one), the files once the Mac has sent them (as a URI list), and
/// requests for them waiting on that.
struct XdndOut {
    wid: u32,
    source: u32,
    target: Option<u32>,
    accepted: bool,
    dropped: bool,
    uris: Option<std::sync::Arc<Vec<u8>>>,
    waiting: Vec<(u32, u32, u32, u32)>,
    started: Instant,
}

/// A drag from a Linux app over a Mac window (XDND): who's dragging, what it offers, where it is,
/// and, once dropped, the types still to read and what was read.
struct XdndIn {
    source: u32,
    types: Vec<String>,
    target: Option<(u32, f64, f64)>,
    to_read: Vec<String>,
    read: Vec<(String, Vec<u8>)>,
}

pub struct App {
    conn: RustConnection,
    screen: usize,
    root: u32,
    atoms: Atoms,
    gl: Gl,
    depth: u8,
    colormap: u32,
    cursors: Option<x11rb::cursor::Handle>,
    named_cursors: HashMap<&'static str, u32>,
    mac_cursor: Option<u32>,
    argb_format: Option<render::Pictformat>,
    shape: bool,
    /// XInput 2 scroll valuators by device; None: no XInput 2 (wheel buttons instead).
    xi_scroll: Option<HashMap<u16, (bool, Vec<ScrollAxis>)>>,
    /// Smooth scrolling under way (phase began), and when it last moved.
    scroll_active: Option<(u32, Instant)>,
    keysyms: (u8, u8, Vec<u32>),
    menus: Option<Menus>,
    loop_handle: LoopHandle<'static, App>,
    net: UnboundedSender<NetCmd>,
    /// The CLIPBOARD selection, kept on its own thread (x11clip.rs); None in the self-test.
    clip: Option<crate::x11clip::Clipboard>,
    windows: HashMap<u32, Win>,
    by_xid: HashMap<u32, u32>,
    pending_fixed: HashMap<u32, (Option<(f32, f32)>, bool)>,
    icon_pngs: HashMap<String, Vec<u8>>,
    noted_apps: HashSet<String>,
    screen_select: bool,
    selection: Option<Selection>,
    left_down: Option<u32>,
    mac_drag: Option<u32>,
    pointer_at: (u32, f64, f64),
    pointer_root: (i16, i16),
    pointer_focus: Option<u32>,
    kb_focus: Option<u32>,
    /// Focus while windows arrive after (re)connecting (see arrival.rs).
    arrival: crate::arrival::Arrival,
    edge_cursor: bool,
    meta_down: u8,
    shift_down: u8,
    keys_down: HashSet<u8>,
    swallowed: Vec<u32>,
    press_id: u32,
    press_at: (f64, f64),
    move_ready: bool,
    drag_spot: Option<(u32, f32, f32, bool, Instant)>,
    /// The pointer device of the last XInput 2 press (its implicit grab is released for WM moves).
    xi_pointer: Option<u16>,
    /// The keyboard is ours (see sync_keyboard_grab).
    kb_grabbed: bool,
    /// The keyboard was let go for the desktop's own menu (Super+Space): not taken back until then.
    grab_hold_off: Option<std::time::Instant>,
    /// A drag from Linux over a Mac window, or being dropped on one.
    xdnd_in: Option<XdndIn>,
    /// A Mac file drag carried on outside its window.
    xdnd_out: Option<XdndOut>,
    /// The number of files the Mac drag carries, and the picture following the pointer.
    drag_count: usize,
    drag_pic: Option<(u32, (i32, i32))>,
    press_time: Instant,
    press_known: bool,
    moved_here: Option<u32>,
    /// A popup being dragged (Quick Look by its bar), and the point of it held (px): moved here,
    /// as the window manager moves windows but not popups (override-redirect).
    popup_drag: Option<(u32, f64, f64)>,
    local_press: Option<u8>,
    scale: f64,
    host: String,
    connected: bool,
    paused: bool,
    closed_offline: HashSet<u32>,
    menu_panel_shown: Option<bool>,
    hostname: String,
    /// The self-test's look at a window: (window, pixels and size once drawn).
    probe: Option<(u32, Option<(Vec<u8>, (u32, u32))>)>,
    /// The self-test's corners, whatever the setting.
    rounded_test: bool,
}

pub fn run(net_cfg: crate::config::Config) -> Result<()> {
    let (ui_tx, ui_rx): (channel::Sender<UiEvent>, Channel<UiEvent>) = channel::channel();
    let scale = net_cfg.scale;
    let host = net_cfg.host.clone();
    let event_loop: EventLoop<App> = EventLoop::try_new()?;
    // The window side first: no X display, no point connecting to the Mac.
    let mut app = App::new(scale, host, None, event_loop.handle())?;
    let net = crate::net::spawn(net_cfg, ui_tx);
    app.net = net.clone();
    // The clipboard thread needs the network's sender, which only exists from here.
    app.clip = crate::x11clip::start(net.clone());
    crate::tray::set_net(net.clone());
    crate::pair::watch_links();
    app.menus = match Menus::new(net) {
        Ok(m) => Some(m),
        Err(e) => {
            log::warn!("no D-Bus session: Mac menus won't appear in the Global Menu ({e})");
            None
        }
    };
    log::info!("sweeflow client started (X11), connecting to {}", app.host);
    app.run_loop(event_loop, ui_rx, None)
}

impl App {
    fn new(scale: f64, host: String, net: Option<UnboundedSender<NetCmd>>, loop_handle: LoopHandle<'static, App>) -> Result<App> {
        let (conn, screen) = RustConnection::connect(None).context("couldn't connect to the X display")?;
        let atoms = Atoms::new(&conn)?.reply()?;
        let s = &conn.setup().roots[screen];
        let root = s.root;
        // 32-bit TrueColor visuals: windows that are see-through where nothing is drawn.
        let argb: Vec<u32> = s
            .allowed_depths
            .iter()
            .filter(|d| d.depth == 32)
            .flat_map(|d| d.visuals.iter().filter(|v| v.class == xproto::VisualClass::TRUE_COLOR).map(|v| v.visual_id))
            .collect();
        let gl = Gl::new(&argb)?;
        let depth = s.allowed_depths.iter().find(|d| d.visuals.iter().any(|v| v.visual_id == gl.visual)).map_or(s.root_depth, |d| d.depth);
        let colormap = conn.generate_id()?;
        conn.create_colormap(xproto::ColormapAlloc::NONE, colormap, root, gl.visual)?;
        let shape = conn.extension_information(shape::X11_EXTENSION_NAME)?.is_some();
        let cursors = x11rb::resource_manager::new_from_default(&conn)
            .ok()
            .and_then(|db| x11rb::cursor::Handle::new(&conn, screen, &db).ok().and_then(|c| c.reply().ok()));
        let argb_format = conn.render_query_pict_formats().ok().and_then(|c| c.reply().ok()).and_then(|r| {
            r.formats
                .iter()
                .find(|f| f.depth == 32 && f.type_ == render::PictType::DIRECT && f.direct.alpha_mask == 0xff && f.direct.alpha_shift == 24 && f.direct.red_shift == 16)
                .map(|f| f.id)
        });
        // Key repeats as presses alone (no release between), so a held key is known.
        if conn.xkb_use_extension(1, 0).ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.supported) {
            let f = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
            let _ = conn.xkb_per_client_flags(xkb::ID::USE_CORE_KBD.into(), f, f, 0u32.into(), 0u32.into(), 0u32.into());
        }
        let xi = conn
            .extension_information(xinput::X11_EXTENSION_NAME)?
            .is_some()
            .then(|| conn.xinput_xi_query_version(2, 1).ok().and_then(|c| c.reply().ok()))
            .flatten()
            .is_some_and(|v| (v.major_version, v.minor_version) >= (2, 1));
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();
        let mut app = App {
            screen,
            root,
            atoms,
            gl,
            depth,
            colormap,
            cursors,
            named_cursors: HashMap::new(),
            mac_cursor: None,
            argb_format,
            shape,
            xi_scroll: xi.then(HashMap::new),
            scroll_active: None,
            keysyms: (8, 0, Vec::new()),
            menus: None,
            loop_handle,
            clip: None,
            net: net.unwrap_or_else(|| tokio::sync::mpsc::unbounded_channel().0),
            windows: HashMap::new(),
            by_xid: HashMap::new(),
            pending_fixed: HashMap::new(),
            icon_pngs: HashMap::new(),
            noted_apps: HashSet::new(),
            screen_select: false,
            selection: None,
            left_down: None,
            mac_drag: None,
            pointer_at: (0, 0.0, 0.0),
            pointer_root: (0, 0),
            pointer_focus: None,
            kb_focus: None,
            arrival: Default::default(),
            edge_cursor: false,
            meta_down: 0,
            shift_down: 0,
            keys_down: HashSet::new(),
            swallowed: Vec::new(),
            press_id: 0,
            press_at: (0.0, 0.0),
            move_ready: false,
            drag_spot: None,
            xi_pointer: None,
            kb_grabbed: false,
            grab_hold_off: None,
            xdnd_in: None,
            xdnd_out: None,
            drag_count: 1,
            drag_pic: None,
            press_time: Instant::now(),
            press_known: false,
            moved_here: None,
            popup_drag: None,
            local_press: None,
            scale,
            host,
            connected: false,
            paused: false,
            closed_offline: HashSet::new(),
            menu_panel_shown: None,
            hostname,
            probe: None,
            rounded_test: false,
            conn,
        };
        app.load_keymap();
        if app.xi_scroll.is_some() {
            app.query_scroll_devices();
        }
        if !app.gl.alpha {
            log::warn!("no 32-bit visual: rounded corners and see-through parts show black");
        }
        Ok(app)
    }

    fn run_loop(mut self, mut event_loop: EventLoop<App>, ui_rx: Channel<UiEvent>, until: Option<Instant>) -> Result<()> {
        event_loop
            .handle()
            .insert_source(ui_rx, |ev, _, app: &mut App| {
                if let channel::Event::Msg(ev) = ev {
                    let kind = ev.kind();
                    let t0 = Instant::now();
                    app.on_net(ev);
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    if ms > 30.0 {
                        log::warn!("UI stall: handling {kind} took {ms:.0} ms");
                    }
                }
            })
            .map_err(|e| anyhow!("{e}"))?;
        // The X socket wakes the loop; its events are read after each turn (some arrive while
        // waiting for a reply, already read off the socket).
        let fd = {
            use std::os::fd::AsRawFd;
            // SAFETY: the connection (and its socket) lives as long as the loop.
            unsafe { std::os::fd::BorrowedFd::borrow_raw(self.conn.stream().as_raw_fd()) }.try_clone_to_owned()?
        };
        event_loop
            .handle()
            .insert_source(Generic::new(fd, Interest::READ, Mode::Level), |_, _, _| Ok(PostAction::Continue))
            .map_err(|e| anyhow!("{e}"))?;
        loop {
            self.drain_x()?;
            self.conn.flush()?;
            if until.is_some_and(|u| Instant::now() >= u) {
                return Ok(());
            }
            let t0 = Instant::now();
            event_loop.dispatch(Duration::from_millis(100), &mut self)?;
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            if ms > 130.0 {
                log::warn!("UI stall: event loop iteration took {ms:.0} ms");
            }
        }
    }

    fn drain_x(&mut self) -> Result<()> {
        while let Some(ev) = self.conn.poll_for_event().context("X connection lost")? {
            self.on_x(ev);
        }
        Ok(())
    }

    fn send(&self, m: Vec<u8>) {
        let _ = self.net.send(NetCmd::Control(m));
    }

    fn on_net(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Connected(name) => {
                log::info!("connected to {name}");
                self.connected = true;
                self.arrival.connected();
                self.send_screen_size();
                for w in self.windows.values_mut() {
                    w.stale = true;
                }
                self.expire_stale_soon();
            }
            UiEvent::Streaming(on) => {
                self.paused = !on;
                log::info!("streaming {}", if on { "resumed" } else { "paused: windows stay as stills" });
                if on {
                    for w in self.windows.values_mut() {
                        if w.still.is_some() {
                            w.stale = true;
                        }
                    }
                    self.expire_stale_soon();
                } else {
                    self.freeze_all();
                }
            }
            UiEvent::Disconnected(why) => {
                self.connected = false;
                log::warn!("disconnected: {why}");
                self.screen_select = false;
                self.end_selection();
                let on_purpose = crate::net::offline() || crate::tray::mac_refused() || crate::tray::elsewhere().is_some();
                let gone: Vec<u32> = self.windows.values().filter(|w| on_purpose || w.popup).map(|w| w.info.wid).collect();
                for wid in gone {
                    self.remove_window(wid);
                }
                self.freeze_all();
            }
            UiEvent::WindowAdded(info) => self.add_window(info),
            UiEvent::WindowChanged { wid, title, w, h, rel_x, rel_y } => {
                let Some(win) = self.windows.get_mut(&wid) else { return };
                let retitle = title != win.info.title;
                let resized = (w, h) != (win.info.w, win.info.h);
                let moved = (rel_x, rel_y) != (win.info.rel_x, win.info.rel_y);
                win.info.title = title;
                win.info.w = w;
                win.info.h = h;
                win.info.rel_x = rel_x;
                win.info.rel_y = rel_y;
                if retitle && !win.popup {
                    let (xid, t) = (win.xid, display_title(&win.info.app_name, &win.info.title));
                    self.set_title(xid, &t);
                }
                self.clip_input(wid);
                if moved && self.windows.get(&wid).is_some_and(|w| w.popup) {
                    self.place_popup(wid);
                }
                if resized {
                    self.draw(wid);
                }
                self.adopt_mac_size(wid);
            }
            UiEvent::WindowRemoved(wid) if self.paused && self.windows.get(&wid).is_some_and(|w| !w.popup) => {
                self.freeze(wid);
                let _ = self.net.send(NetCmd::Unsubscribe(wid));
            }
            UiEvent::WindowRemoved(wid) => self.remove_window(wid),
            UiEvent::WindowFixed(wid, spot, fixed) if !self.windows.contains_key(&wid) => {
                self.pending_fixed.insert(wid, (spot, fixed));
            }
            UiEvent::WindowFixed(_, _, false) => {}
            UiEvent::WindowFixed(wid, spot, true) => {
                let Some(w) = self.windows.get_mut(&wid).filter(|w| !w.display) else { return };
                w.fixed = true;
                w.spot = spot;
                log::info!("window {wid}: its app won't let it be resized: kept at its own size");
                self.hold_mac_size(wid);
                if let Some(spot) = spot {
                    self.place_at_spot(wid, spot);
                }
                self.draw(wid);
            }
            UiEvent::AppIcon { bundle_id, png } => {
                self.icon_pngs.insert(bundle_id.clone(), png);
                let wids: Vec<u32> = self.windows.values().filter(|w| w.info.bundle_id == bundle_id).map(|w| w.info.wid).collect();
                for wid in wids {
                    self.apply_icon(wid);
                }
            }
            UiEvent::Cursor { hot_x, hot_y, scale, png } => self.set_mac_cursor(hot_x, hot_y, scale, &png),
            UiEvent::Focused(wid) => {
                if self.arrival.mac_focused(wid) && self.kb_focus != Some(wid) {
                    self.activate(wid);
                }
            }
            UiEvent::CornersSetting => self.apply_corners_setting(),
            // Hyprland's own (it never runs under X11).
            UiEvent::Tiling(_) => {}
            UiEvent::WindowButtons(wid, rects) => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.buttons = rects;
                }
                if self.drag_spot.is_some_and(|s| s.0 == wid) {
                    self.drag_spot = None;
                }
                self.update_lights(wid);
            }
            UiEvent::MoveReady(wid, id) => {
                if id == self.press_id && self.left_down == Some(wid) {
                    self.move_ready = true;
                    let (x, y) = (self.pointer_at.1, self.pointer_at.2);
                    if self.pointer_at.0 == wid && self.moved_enough(x, y) {
                        self.move_here(wid);
                    }
                }
            }
            UiEvent::DragSpot(wid, x, y, drags) => self.drag_spot = Some((wid, x, y, drags, Instant::now())),
            UiEvent::MoveStarted(wid) if self.moved_here.take() == Some(wid) => {}
            UiEvent::MoveStarted(wid) => {
                // Only while the press that started it is still held (see ui.rs).
                if self.left_down != Some(wid) {
                    log::warn!("window {wid}: title-bar drag reported after the button was released: not moved");
                } else if let Some(w) = self.windows.get(&wid) {
                    if w.popup {
                        // A popup (Quick Look) is moved here, from where the pointer is now.
                        self.popup_drag = Some((wid, self.pointer_at.1, self.pointer_at.2));
                    } else {
                        let xid = w.xid;
                        self.wm_moveresize(xid, MOVERESIZE_MOVE);
                    }
                }
            }
            UiEvent::ToggleMaximize(wid) => self.toggle_maximize(wid),
            UiEvent::Clipboard(offers) => {
                if let Some(c) = &self.clip {
                    c.set(offers);
                }
            }
            UiEvent::DragBegan { wid, names } => {
                log::info!("Mac drag in {wid}: {names:?}");
                self.mac_drag = Some(wid);
                self.drag_count = names.len().max(1);
            }
            UiEvent::DragData(uris) => {
                let uris = std::sync::Arc::new(uris);
                let waiting = self.xdnd_out.as_mut().map(|o| {
                    o.uris = Some(uris.clone());
                    std::mem::take(&mut o.waiting)
                });
                for (requestor, property, target, time) in waiting.unwrap_or_default() {
                    self.xdnd_answer(requestor, property, target, time, Some(&uris));
                }
            }
            UiEvent::MenuTree { pid, json } => {
                if let Some(m) = self.menus.as_mut() {
                    m.update(pid, &json);
                }
            }
            UiEvent::ScreenSelect(on) => {
                log::info!("Mac screenshot selection {}", if on { "started" } else { "ended" });
                self.screen_select = on;
                if !on {
                    self.end_selection();
                }
            }
            UiEvent::Raise { bundle_id, .. } => self.raise_app(&bundle_id),
            UiEvent::Minimize(wid) => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.mac_minimized = true;
                    let (xid, popup) = (w.xid, w.popup);
                    if !popup {
                        self.iconify(xid);
                    }
                }
            }
            UiEvent::Frame(wid, frame) => self.present(wid, frame),
            UiEvent::RawFrame { wid, w, h, px_per_pt, capture_us, bgra } => self.present_raw(wid, w, h, px_per_pt, capture_us, bgra),
            UiEvent::Corners { wid, t_px, tiles } => self.set_corners(wid, t_px, tiles),
            UiEvent::BlindShow { id, app, buttons, menu: true, .. } => crate::blind::show_menu(id, &app, &buttons, self.net.clone()),
            UiEvent::BlindShow { id, app, title, texts, buttons, menu: false } => crate::blind::show(id, &app, &title, &texts, &buttons, self.net.clone()),
        }
    }

    // --- windows ----------------------------------------------------------------------------

    fn add_window(&mut self, info: WinInfo) {
        let wid = info.wid;
        self.arrival.window_added();
        if self.closed_offline.remove(&wid) {
            log::info!("window {wid} was closed while the Mac was away: closing it there");
            self.send(W::new(t::CLOSE).u32(wid).finish());
        }
        log::info!("window {wid} added: {} kind {} parent {} {}x{} at {},{}", info.app_name, info.kind, info.parent, info.w, info.h, info.rel_x, info.rel_y);
        self.unfreeze(wid);
        let hidden: Vec<u32> = self.windows.values().filter(|w| w.hidden.is_some() && (w.info.pid == info.pid || w.info.wid == wid)).map(|w| w.info.wid).collect();
        for h in hidden {
            self.unhide(h, "its app showed another window instead of closing");
        }
        if self.windows.get(&wid).is_some_and(|w| (w.info.kind == 1) != (info.kind == 1)) {
            log::info!("window {wid}: now kind {} (was {}): made anew", info.kind, self.windows[&wid].info.kind);
            self.remove_window(wid);
        }
        if let Some(w) = self.windows.get_mut(&wid) {
            w.stale = false;
            w.info = info;
            w.requested = None;
            let _ = self.net.send(NetCmd::Subscribe(wid));
            self.adopt_mac_size(wid);
            return;
        }
        if let Err(e) = self.create_window(info) {
            log::warn!("window {wid}: couldn't make its X window: {e:#}");
            return;
        }
        let _ = self.net.send(NetCmd::Subscribe(wid));
    }

    fn create_window(&mut self, info: WinInfo) -> Result<()> {
        let wid = info.wid;
        let popup = info.kind == 1 && self.windows.contains_key(&info.parent);
        let display = info.kind == 3;
        if display {
            crate::tray::set_display_shown(true);
        }
        let size = if display {
            ((info.w * 0.6).max(320.0) as u32, (info.h * 0.6).max(160.0) as u32)
        } else {
            (info.w.max(1.0) as u32, info.h.max(1.0) as u32)
        };
        let mac_size = (info.w.round().max(1.0) as u32, info.h.round().max(1.0) as u32);
        let xid = self.conn.generate_id()?;
        let mut mask = EventMask::EXPOSURE
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::PROPERTY_CHANGE
            | EventMask::FOCUS_CHANGE
            | EventMask::ENTER_WINDOW
            | EventMask::LEAVE_WINDOW
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE;
        if self.xi_scroll.is_none() {
            mask |= EventMask::POINTER_MOTION;
        }
        let (x, y) = if popup { self.popup_origin(&info).unwrap_or((0, 0)) } else { (0, 0) };
        let aux = xproto::CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(self.colormap)
            .override_redirect(popup as u32)
            .event_mask(mask);
        self.conn.create_window(self.depth, xid, self.root, x as i16, y as i16, size.0 as u16, size.1 as u16, 0, xproto::WindowClass::INPUT_OUTPUT, self.gl.visual, &aux)?;
        if self.xi_scroll.is_some() {
            // Pointer motion through XInput 2: with it the scroll valuators (smooth scrolling).
            // Buttons too: a press then grabs the pointer for XInput 2, and motion keeps coming
            // while it's held (with a classic press, XInput 2 motion stopped during a drag:
            // title-bar drags never became window moves).
            let m = xinput::EventMask {
                deviceid: 1, // all master devices
                mask: vec![xinput::XIEventMask::MOTION | xinput::XIEventMask::BUTTON_PRESS | xinput::XIEventMask::BUTTON_RELEASE],
            };
            self.conn.xinput_xi_select_events(xid, &[m])?;
        }
        let a = &self.atoms;
        let title = if display { format!("Mac display — {}", info.title) } else { display_title(&info.app_name, &info.title) };
        let class = if display { "sweeflow-display".to_string() } else { format!("sweeflow-{}", info.bundle_id) };
        self.conn.change_property8(PropMode::REPLACE, xid, xproto::AtomEnum::WM_CLASS, xproto::AtomEnum::STRING, format!("{class}\0{class}\0").as_bytes())?;
        self.conn.change_property32(PropMode::REPLACE, xid, a._NET_WM_PID, xproto::AtomEnum::CARDINAL, &[std::process::id()])?;
        // Files dragged from Linux apps can be dropped on it (XDND, version 5).
        self.conn.change_property32(PropMode::REPLACE, xid, a.XdndAware, xproto::AtomEnum::ATOM, &[5])?;
        self.conn.change_property8(PropMode::REPLACE, xid, a.WM_CLIENT_MACHINE, xproto::AtomEnum::STRING, self.hostname.as_bytes())?;
        self.conn.change_property32(PropMode::REPLACE, xid, a.WM_PROTOCOLS, xproto::AtomEnum::ATOM, &[a.WM_DELETE_WINDOW, a._NET_WM_PING])?;
        let mut hints = WmHints::new();
        hints.input = Some(!popup);
        hints.set(&self.conn, xid)?;
        let mut fixed_now = false;
        let mut floated_now = false;
        let mut fixed_spot = None;
        let mut size_hints = WmSizeHints::new();
        let mut kind = a._NET_WM_WINDOW_TYPE_NORMAL;
        let mut place = None;
        let mut over_parent: Option<(i32, i32)> = None;
        if popup {
            kind = a._NET_WM_WINDOW_TYPE_POPUP_MENU;
        } else if display {
            size_hints.min_size = Some((320, 160));
        } else {
            // The Mac window brings its own title bar and traffic lights: no frame around it.
            self.conn.change_property32(PropMode::REPLACE, xid, a._MOTIF_WM_HINTS, a._MOTIF_WM_HINTS, &[2, 0, 0, 0, 0])?;
            if let Some((spot, fixed)) = self.pending_fixed.remove(&wid) {
                // A dialog from the start (as ui.rs): floated, not tiled; a fixed one exactly
                // its size; transient for the window it lies in.
                if fixed {
                    size_hints.min_size = Some((mac_size.0 as i32, mac_size.1 as i32));
                    size_hints.max_size = Some((mac_size.0 as i32, mac_size.1 as i32));
                    fixed_now = true;
                }
                kind = a._NET_WM_WINDOW_TYPE_DIALOG;
                let parent = self
                    .windows
                    .get(&info.parent)
                    .filter(|p| {
                        !p.popup
                            && info.rel_x >= -1.0
                            && info.rel_y >= -1.0
                            && info.rel_x + info.w <= p.info.w + 1.0
                            && info.rel_y + info.h <= p.info.h + 1.0
                    })
                    .map(|p| p.xid);
                if let Some(p) = parent {
                    self.conn.change_property32(PropMode::REPLACE, xid, xproto::AtomEnum::WM_TRANSIENT_FOR, xproto::AtomEnum::WINDOW, &[p])?;
                    self.conn.change_property32(PropMode::REPLACE, xid, a._NET_WM_STATE, xproto::AtomEnum::ATOM, &[a._NET_WM_STATE_MODAL])?;
                    // Over its window where the Mac has it (a "Save changes?" sheet): Cinnamon
                    // put such a dialog in a corner of the screen.
                    if let Ok(r) = self.conn.translate_coordinates(p, self.root, 0, 0).map_err(anyhow::Error::from).and_then(|c| Ok(c.reply()?)) {
                        over_parent = Some((r.dst_x as i32 + info.rel_x.round() as i32, r.dst_y as i32 + info.rel_y.round() as i32));
                    }
                }
                // Nowhere on its window (a panel the Mac centred on its own display, Quick
                // Look's, far off the window there): centred on the window here instead.
                if let (None, None, Some(p)) = (parent, spot, self.windows.get(&info.parent).filter(|p| !p.popup)) {
                    let (rx, ry) = info.spot_on(p.info.w, p.info.h);
                    if let Ok(r) = self.conn.translate_coordinates(p.xid, self.root, 0, 0).map_err(anyhow::Error::from).and_then(|c| Ok(c.reply()?)) {
                        let (ax, ay, aw, ah) = self.work_area();
                        let x = (r.dst_x as i32 + rx.round() as i32).min(ax + aw - info.w.round() as i32).max(ax);
                        let y = (r.dst_y as i32 + ry.round() as i32).min(ay + ah - info.h.round() as i32).max(ay);
                        self.conn.change_property32(PropMode::REPLACE, xid, xproto::AtomEnum::WM_TRANSIENT_FOR, xproto::AtomEnum::WINDOW, &[p.xid])?;
                        over_parent = Some((x, y));
                    }
                }
                floated_now = true;
                fixed_spot = spot;
                if let (Some(spot), None) = (spot, parent) {
                    place = Some(spot);
                }
            } else if info.kind != 2 {
                size_hints.min_size = Some((MIN_SIZE.0 as i32, MIN_SIZE.1 as i32));
            }
        }
        if let Some(p) = over_parent {
            size_hints.position = Some((WmSizeHintsSpecification::UserSpecified, p.0, p.1));
            self.conn.configure_window(xid, &xproto::ConfigureWindowAux::new().x(p.0).y(p.1))?;
        }
        if let Some(spot) = place {
            let p = spot_position(self.work_area(), size, spot);
            size_hints.position = Some((WmSizeHintsSpecification::UserSpecified, p.0, p.1));
            self.conn.configure_window(xid, &xproto::ConfigureWindowAux::new().x(p.0).y(p.1))?;
        }
        size_hints.set_normal_hints(&self.conn, xid)?;
        self.conn.change_property32(PropMode::REPLACE, xid, a._NET_WM_WINDOW_TYPE, xproto::AtomEnum::ATOM, &[kind])?;
        if let (false, false, Some(menus)) = (popup, display, &self.menus) {
            // KDE's Global Menu on X11 finds the app's menu by these.
            self.conn.change_property8(PropMode::REPLACE, xid, a._KDE_NET_WM_APPMENU_SERVICE_NAME, xproto::AtomEnum::STRING, menus.service().as_bytes())?;
            self.conn.change_property8(PropMode::REPLACE, xid, a._KDE_NET_WM_APPMENU_OBJECT_PATH, xproto::AtomEnum::STRING, Menus::object_path(info.pid).as_bytes())?;
        }
        self.set_title(xid, &title);
        if let Some(c) = self.mac_cursor.or_else(|| self.named_cursor("left_ptr")) {
            self.conn.change_window_attributes(xid, &xproto::ChangeWindowAttributesAux::new().cursor(c))?;
        }
        // EGL has a connection of its own: the window must be on the server before it looks.
        self.conn.get_input_focus()?.reply()?;
        let target = self.gl.target(xid)?;
        self.conn.map_window(xid)?;
        if popup {
            self.conn.configure_window(xid, &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE))?;
        }
        let info_kind = info.kind;
        self.by_xid.insert(xid, wid);
        self.windows.insert(
            wid,
            Win {
                first_shape: Some(info.w as f64 / info.h.max(1.0) as f64),
                info,
                xid,
                popup,
                target: Some(target),
                configured: false,
                size,
                x_size: size,
                min_size: if fixed_now { mac_size } else if info_kind == 2 || info_kind == 3 { (1, 1) } else { MIN_SIZE },
                max_size: fixed_now.then_some(mac_size),
                fixed: fixed_now,
                floated: floated_now,
                spot: fixed_spot,
                refusal: None,
                followed: false,
                requested: None,
                floating: true,
                maximized: false,
                suspended: false,
                buttons: [[0.0; 4]; 3],
                lights: Slot::default(),
                lights_drawn: None,
                corners: None,
                dark_titlebar: true,
                mac_minimized: false,
                lights_hover: false,
                lights_pressed: None,
                pending: None,
                next: None,
                shown: None,
                picture: None,
                still: None,
                hidden: None,
                pending_raw: None,
                raw: None,
                stale: false,
                popup_wait: false,
                moved_to: None,
                popup_settled: false,
                check_pending: false,
                display,
                fit: (1.0, 0.0, 0.0),
            },
        );
        self.clip_input(wid);
        if !popup {
            self.apply_icon(wid);
        }
        Ok(())
    }

    fn set_title(&self, xid: u32, title: &str) {
        let _ = self.conn.change_property8(PropMode::REPLACE, xid, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING, title.as_bytes());
        let _ = self.conn.change_property8(PropMode::REPLACE, xid, xproto::AtomEnum::WM_NAME, xproto::AtomEnum::STRING, title.as_bytes());
    }

    fn remove_window(&mut self, wid: u32) {
        if self.selection.as_ref().is_some_and(|s| s.wid == wid) {
            self.end_selection();
        }
        self.unfreeze(wid);
        let Some(mut w) = self.windows.remove(&wid) else { return };
        log::info!("window {wid} removed ({})", w.info.app_name);
        if w.display {
            crate::tray::set_display_shown(false);
        }
        let _ = self.net.send(NetCmd::Unsubscribe(wid));
        self.by_xid.remove(&w.xid);
        if self.pointer_focus == Some(wid) {
            self.pointer_focus = None;
        }
        if self.kb_focus == Some(wid) {
            self.kb_focus = None;
        }
        if !self.windows.values().any(|o| o.info.pid == w.info.pid) {
            crate::menus::app_gone(w.info.pid);
        }
        let children: Vec<u32> = self.windows.values().filter(|c| c.info.parent == wid).map(|c| c.info.wid).collect();
        for c in children {
            self.remove_window(c);
        }
        // GL objects go while a window of ours is current (this one, if it's the only one).
        if let Some(t) = w.target.as_mut() {
            let _ = self.gl.begin(t, (1, 1), [0.0; 4]);
        }
        if let Some((_, v)) = w.shown.take() {
            self.gl.drop_video(v);
        }
        for slot in [Some(&mut w.lights), w.corners.as_mut().map(|c| &mut c.slot), w.raw.as_mut().map(|r| &mut r.0), w.still.as_mut().map(|s| &mut s.slot)].into_iter().flatten() {
            if let Some(t) = slot.tex.take() {
                self.gl.drop_tex(t);
            }
        }
        if let Some(t) = w.target.take() {
            self.gl.drop_target(t);
        }
        let _ = self.conn.destroy_window(w.xid);
    }

    fn schedule_check(&mut self, wid: u32) {
        if let Some(w) = self.windows.get_mut(&wid) {
            Self::arm_check(&self.loop_handle, &mut w.check_pending, wid);
        }
    }

    fn arm_check(lh: &LoopHandle<'static, App>, pending: &mut bool, wid: u32) {
        if *pending {
            return;
        }
        *pending = true;
        let _ = lh.insert_source(Timer::from_duration(Duration::from_millis(3200)), move |_, _, app| {
            let Some(w) = app.windows.get_mut(&wid) else { return TimeoutAction::Drop };
            w.check_pending = false;
            if w.requested.is_some_and(|(_, at)| at.elapsed() < Duration::from_millis(3000)) {
                app.schedule_check(wid);
            } else {
                app.adopt_mac_size(wid);
            }
            TimeoutAction::Drop
        });
    }

    /// A window its app won't let be resized: exactly the Mac's size here, smallest and largest.
    fn hold_mac_size(&mut self, wid: u32) {
        let Some(w) = self.windows.get_mut(&wid).filter(|w| !w.popup) else { return };
        let mac = (w.info.w.round().max(1.0) as u32, w.info.h.round().max(1.0) as u32);
        if w.min_size != mac || w.max_size != Some(mac) {
            w.min_size = mac;
            w.max_size = Some(mac);
            let mut h = WmSizeHints::new();
            h.min_size = Some((mac.0 as i32, mac.1 as i32));
            h.max_size = Some((mac.0 as i32, mac.1 as i32));
            let _ = h.set_normal_hints(&self.conn, w.xid);
        }
        w.size = mac;
        let xid = w.xid;
        self.resize_x(xid, mac);
    }

    fn resize_x(&self, xid: u32, size: (u32, u32)) {
        let _ = self.conn.configure_window(xid, &xproto::ConfigureWindowAux::new().width(size.0.max(1)).height(size.1.max(1)));
    }

    fn adopt_mac_size(&mut self, wid: u32) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.display {
            return;
        }
        let mac = (w.info.w.round() as u32, w.info.h.round() as u32);
        if mac == w.size || !w.configured {
            w.followed |= mac == w.size;
            return;
        }
        if let Some((_, at)) = w.requested {
            if at.elapsed() < resize_wait(!w.popup && !w.floating) {
                return;
            }
        }
        if !w.popup {
            if w.fixed {
                let before = w.size;
                let spot = w.spot;
                self.hold_mac_size(wid);
                if let Some(spot) = spot.filter(|_| self.windows.get(&wid).is_some_and(|w| w.size != before)) {
                    self.place_at_spot(wid, spot);
                }
                self.draw(wid);
                return;
            }
            if w.floating {
                // A window the window manager doesn't tile (on Cinnamon, every one) follows its
                // Mac window: one that opens small and grows (Preview zooming a picture open)
                // grows here too. (Resizing it here asks the Mac first: `requested` above.)
                w.size = mac;
                let xid = w.xid;
                self.resize_x(xid, mac);
                self.draw(wid);
                return;
            }
            let size = w.size;
            match refused(size, mac, w.followed, w.requested, w.refusal, w.floated, w.first_shape) {
                None => {}
                Some(Refused::FloatAtMacSize) => {
                    log::info!("window {wid}: keeps its shape ({}x{}, asked {}x{}): floating it at that size", mac.0, mac.1, size.0, size.1);
                    let info = w.info.clone();
                    let buttons = w.buttons;
                    self.pending_fixed.insert(wid, (None, false));
                    self.remove_window(wid);
                    self.add_window(info);
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.buttons = buttons;
                    }
                    self.update_lights(wid);
                    return;
                }
                Some(Refused::AskOnceMore) => {
                    w.refusal = Some((size, mac));
                    w.requested = Some((size, Instant::now()));
                    let _ = self.net.send(NetCmd::resize(wid, size));
                    Self::arm_check(&self.loop_handle, &mut w.check_pending, wid);
                    return;
                }
                Some(Refused::Wait) => return,
            }
        }
        if w.popup && !w.popup_settled {
            // macOS animates popovers closing: a real change is still there a moment later.
            if !w.popup_wait {
                w.popup_wait = true;
                let _ = self.loop_handle.insert_source(Timer::from_duration(Duration::from_millis(200)), move |_, _, app| {
                    if let Some(w) = app.windows.get_mut(&wid) {
                        w.popup_wait = false;
                        w.popup_settled = true;
                    }
                    app.adopt_mac_size(wid);
                    if let Some(w) = app.windows.get_mut(&wid) {
                        w.popup_settled = false;
                    }
                    TimeoutAction::Drop
                });
            }
            return;
        }
        if w.popup || w.floating {
            w.size = mac;
            let xid = w.xid;
            self.resize_x(xid, mac);
            if self.windows.get(&wid).is_some_and(|w| w.popup) {
                self.place_popup(wid);
            }
            self.clip_input(wid);
            self.draw(wid);
            return;
        }
        let size = w.size;
        w.requested = Some((size, Instant::now()));
        w.followed = false;
        log::info!("window {wid}: {}x{} on the Mac, {}x{} here: asking again", mac.0, mac.1, size.0, size.1);
        let _ = self.net.send(NetCmd::resize(wid, size));
        if mac.0.abs_diff(size.0) > 40 || mac.1.abs_diff(size.1) > 40 {
            Self::arm_check(&self.loop_handle, &mut w.check_pending, wid);
        }
    }

    /// Clicks land on the window only where the Mac window is (and nowhere while it's hidden
    /// for closing): what shows through the rest gets them.
    fn clip_input(&self, wid: u32) {
        let Some(w) = self.windows.get(&wid) else { return };
        if !self.shape || w.popup || w.display {
            return;
        }
        let mac = (w.info.w.round().max(1.0) as u32, w.info.h.round().max(1.0) as u32);
        let rects: Vec<xproto::Rectangle> = if w.hidden.is_some() {
            Vec::new()
        } else {
            vec![xproto::Rectangle { x: 0, y: 0, width: mac.0.min(w.size.0) as u16, height: mac.1.min(w.size.1) as u16 }]
        };
        let _ = self.conn.shape_rectangles(shape::SO::SET, shape::SK::INPUT, xproto::ClipOrdering::UNSORTED, w.xid, 0, 0, &rects);
    }

    /// The root position of a popup: its parent's, plus where the Mac put it on the parent.
    fn popup_origin(&self, info: &WinInfo) -> Option<(i32, i32)> {
        let parent = self.windows.get(&info.parent)?;
        let r = self.conn.translate_coordinates(parent.xid, self.root, 0, 0).ok()?.reply().ok()?;
        let moved = self.windows.get(&info.wid).and_then(|w| w.moved_to);
        let (rx, ry) = moved.unwrap_or_else(|| info.spot_on(parent.info.w, parent.info.h));
        let (mut x, mut y) = (r.dst_x as i32 + rx.round() as i32, r.dst_y as i32 + ry.round() as i32);
        // And on the screen, whole if it fits.
        if let Some(g) = self.conn.get_geometry(self.root).ok().and_then(|c| c.reply().ok()) {
            x = x.min(g.width as i32 - info.w.round() as i32).max(0);
            y = y.min(g.height as i32 - info.h.round() as i32).max(0);
        }
        Some((x, y))
    }

    fn place_popup(&self, wid: u32) {
        let Some(w) = self.windows.get(&wid).filter(|w| w.popup) else { return };
        if let Some((x, y)) = self.popup_origin(&w.info) {
            let _ = self.conn.configure_window(w.xid, &xproto::ConfigureWindowAux::new().x(x).y(y).stack_mode(xproto::StackMode::ABOVE));
        }
        // Its own popups (a submenu) follow it.
        let kids: Vec<u32> = self.windows.values().filter(|c| c.info.parent == wid && c.popup).map(|c| c.info.wid).collect();
        for k in kids {
            self.place_popup(k);
        }
    }

    fn present(&mut self, wid: u32, frame: GpuFrame) {
        if self.paused && self.windows.get(&wid).is_some_and(|w| w.still.is_some()) {
            let _ = self.net.send(NetCmd::Video(wid, W::new(t::FRAME_ACK).u64(frame.capture_us).finish()));
            return;
        }
        if let Some(w) = self.windows.get_mut(&wid).filter(|w| w.hidden.is_some()) {
            let _ = self.net.send(NetCmd::Video(wid, W::new(t::FRAME_ACK).u64(frame.capture_us).finish()));
            w.pending = Some(frame);
            return;
        }
        self.unfreeze(wid);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.dark_titlebar != frame.dark_titlebar {
            w.dark_titlebar = frame.dark_titlebar;
            w.lights_drawn = None;
        }
        // Always ack so the Mac keeps streaming, even if this frame is dropped.
        let ack = W::new(t::FRAME_ACK).u64(frame.capture_us).finish();
        crate::net::video_shown(frame.capture_us);
        let _ = self.net.send(NetCmd::Video(wid, ack));
        if !w.configured {
            w.pending = Some(frame);
            return;
        }
        if w.suspended {
            return;
        }
        w.picture = Some((frame.content, frame.px_per_pt));
        w.next = Some(frame);
        self.update_lights(wid);
        self.draw(wid);
    }

    #[allow(clippy::too_many_arguments)]
    fn present_raw(&mut self, wid: u32, pw: u16, ph: u16, ppp: u8, capture_us: u64, bgra: Vec<u8>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let _ = self.net.send(NetCmd::Video(wid, W::new(t::FRAME_ACK).u64(capture_us).finish()));
        if !w.configured {
            w.pending_raw = Some((pw, ph, ppp, capture_us, bgra));
            return;
        }
        let slot = w.raw.get_or_insert_with(|| (Slot::default(), ppp));
        slot.1 = ppp;
        slot.0.pending = Some((pw as u32, ph as u32, bgra));
        self.draw(wid);
    }

    fn set_corners(&mut self, wid: u32, t_px: u32, tiles: Vec<u8>) {
        let square = self.square_corners();
        let Some(w) = self.windows.get_mut(&wid).filter(|w| !w.popup) else { return };
        let c = w.corners.get_or_insert_with(|| Corners { t_px: 0, raw: None, squared: false, last: None, slot: Slot::default() });
        if c.t_px != t_px {
            log::info!("window {wid}: corner radius {t_px}px");
        }
        c.t_px = t_px;
        c.raw = (t_px > 0).then(|| (t_px, tiles.clone()));
        // No see-through windows: square corners whatever the setting.
        c.squared = square;
        let px = if t_px > 0 && c.squared { square_tiles(t_px as usize, tiles) } else { tiles };
        c.last = (t_px > 0).then(|| (t_px, px.clone()));
        c.slot.pending = (t_px > 0).then_some((t_px, t_px * 4, px));
    }

    /// Square corners: the setting, or windows that can't be see-through.
    fn square_corners(&self) -> bool {
        !self.gl.alpha || (crate::config::square_corners() && !self.rounded_test)
    }

    fn apply_corners_setting(&mut self) {
        let square = self.square_corners();
        let mut redraw = Vec::new();
        for w in self.windows.values_mut() {
            let Some(c) = w.corners.as_mut().filter(|c| c.squared != square) else { continue };
            c.squared = square;
            let Some((t, raw)) = c.raw.clone() else { continue };
            let px = if square { square_tiles(t as usize, raw) } else { raw };
            c.last = Some((t, px.clone()));
            c.slot.pending = Some((t, t * 4, px));
            redraw.push(w.info.wid);
        }
        for wid in redraw {
            self.draw(wid);
        }
    }

    /// Draws a window as it is now: its picture (cut off at its edge as ui.rs's lay_out does),
    /// the corner tiles on its real corners, a paused still, the traffic lights, the selection.
    fn draw(&mut self, wid: u32) {
        let s = self.scale;
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if !w.configured {
            return;
        }
        let Some(target) = w.target.as_mut() else { return };
        let clear = if w.display { [0x1e as f32 / 255.0, 0x1e as f32 / 255.0, 0x1e as f32 / 255.0, 1.0] } else { [0.0; 4] };
        if let Err(e) = self.gl.begin(target, w.x_size, clear) {
            log::warn!("window {wid}: can't draw: {e:#}");
            return;
        }
        // Pictures that came since the last drawing go to the GPU now.
        let mut retired = None;
        if let Some(frame) = w.next.take() {
            match self.gl.import(&frame) {
                Ok(v) => retired = w.shown.replace((frame, v)),
                Err(e) => log::warn!("window {wid}: frame not shown: {e:#}"),
            }
        }
        for slot in [Some(&mut w.lights), w.corners.as_mut().map(|c| &mut c.slot), w.raw.as_mut().map(|r| &mut r.0), w.still.as_mut().map(|s| &mut s.slot)].into_iter().flatten() {
            if let Some((pw, ph, px)) = slot.pending.take() {
                if let Some(old) = slot.tex.take() {
                    self.gl.drop_tex(old);
                }
                match self.gl.upload(pw, ph, &px) {
                    Ok(t) => slot.tex = Some(t),
                    Err(e) => log::warn!("window {wid}: picture not uploaded: {e:#}"),
                }
            }
        }
        let gl = &self.gl;
        if w.hidden.is_none() {
            if w.display {
                if let Some((st, tex)) = w.still.as_ref().and_then(|st| st.slot.tex.as_ref().map(|t| (st, t))) {
                    let src = [0.0, 0.0, st.src.0 as f32 / tex.size.0 as f32, st.src.1 as f32 / tex.size.1 as f32];
                    gl.draw_tex(tex, Quad { dst: [st.pos.0 as f32, st.pos.1 as f32, st.dst.0 as f32, st.dst.1 as f32], src }, Blend::Replace);
                } else if let Some((frame, v)) = &w.shown {
                    let (k, ox, oy) = w.fit;
                    let (dw, dh) = (w.info.w as f64 * k, w.info.h as f64 * k);
                    let src = [0.0, 0.0, frame.content.0 as f32 / v.size.0 as f32, frame.content.1 as f32 / v.size.1 as f32];
                    gl.draw_video(v, Quad { dst: [ox as f32, oy as f32, dw as f32, dh as f32], src });
                }
            } else if let Some(((fw, fh), reported)) = w.picture.filter(|_| w.shown.is_some() || w.still.is_some()) {
                let ppp = px_per_pt(reported, fw, w.info.w, s);
                // Never more of the frame than the Mac window itself (see ui.rs's lay_out).
                let (fw, fh) = if w.info.w > 0.0 && w.info.h > 0.0 {
                    (fw.min((w.info.w as f64 * ppp).round() as u32).max(1), fh.min((w.info.h as f64 * ppp).round() as u32).max(1))
                } else {
                    (fw, fh)
                };
                let ((sw, sh), (dw, dh), (cut_r, cut_b)) = shown_part((fw, fh), ppp, w.size, s);
                if let Some(st) = &w.still {
                    if let Some(tex) = &st.slot.tex {
                        let src = [0.0, 0.0, st.src.0 as f32 / tex.size.0 as f32, st.src.1 as f32 / tex.size.1 as f32];
                        gl.draw_tex(tex, Quad { dst: [st.pos.0 as f32, st.pos.1 as f32, st.dst.0 as f32, st.dst.1 as f32], src }, Blend::Replace);
                    }
                } else if let Some((_, v)) = &w.shown {
                    let src = [0.0, 0.0, (sw / v.size.0 as f64) as f32, (sh / v.size.1 as f64) as f32];
                    gl.draw_video(v, Quad { dst: [0.0, 0.0, dw as f32, dh as f32], src });
                    // Rounded corners on the window's real corners: the Mac's own tiles there.
                    if let Some((c, tex)) = w.corners.as_ref().and_then(|c| c.slot.tex.as_ref().map(|t| (c, t))) {
                        let t = c.t_px;
                        let l = ((t as f64 / ppp) * s).round() as i32;
                        if t > 0 && sw as u32 > 3 * t && sh as u32 > 3 * t && l > 0 && dw > 2 * l && dh > 2 * l {
                            let rounded = [true, !cut_r, !cut_b, !cut_r && !cut_b];
                            let pos = [(0, 0), (dw - l, 0), (0, dh - l), (dw - l, dh - l)];
                            for i in (0..4).filter(|&i| rounded[i]) {
                                let q = Quad {
                                    dst: [pos[i].0 as f32, pos[i].1 as f32, l as f32, l as f32],
                                    src: [0.0, i as f32 / 4.0, 1.0, (i + 1) as f32 / 4.0],
                                };
                                gl.draw_tex(tex, q, Blend::Replace);
                            }
                        }
                    }
                }
            } else if let Some((Slot { tex: Some(tex), .. }, ppp)) = &w.raw {
                // A lossless picture (a menu): drawn from its corner, as much as fits.
                let (fw, fh) = tex.size;
                let (bw, bh) = w.size;
                let px_per_pt = px_per_pt(*ppp, fw, w.info.w, s);
                let vis_w = (fw as f64 / px_per_pt).min(bw as f64 / s);
                let vis_h = (fh as f64 / px_per_pt).min(bh as f64 / s);
                let src = [0.0, 0.0, (vis_w * px_per_pt / fw as f64) as f32, (vis_h * px_per_pt / fh as f64) as f32];
                gl.draw_tex(tex, Quad { dst: [0.0, 0.0, (vis_w * s).round() as f32, (vis_h * s).round() as f32], src }, Blend::Replace);
            }
            if let (Some(tex), Some((_, rect))) = (&w.lights.tex, w.lights_drawn) {
                let dst = [(rect[0] * s).round() as f32, (rect[1] * s).round() as f32, (rect[2] * s).round() as f32, (rect[3] * s).round() as f32];
                gl.draw_tex(tex, Quad { dst, src: [0.0, 0.0, 1.0, 1.0] }, Blend::Over);
            }
            if let Some(sel) = self.selection.as_ref().filter(|s| s.wid == wid) {
                let (x0, y0) = (sel.start.0.min(sel.now.0).round() as f32, sel.start.1.min(sel.now.1).round() as f32);
                let (x1, y1) = (sel.start.0.max(sel.now.0).round() as f32, sel.start.1.max(sel.now.1).round() as f32);
                let (sw, sh) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
                // Premultiplied, as ui.rs's: a grey veil, and an edge that shows on any content.
                gl.draw_solid([x0, y0, sw, sh], [0.12, 0.12, 0.12, 0.25]);
                let edge = [0.35, 0.35, 0.35, 0.9];
                for r in [[x0, y0, sw, 1.0], [x0, y0 + sh - 1.0, sw, 1.0], [x0, y0, 1.0, sh], [x0 + sw - 1.0, y0, 1.0, sh]] {
                    gl.draw_solid(r, edge);
                }
            }
        }
        if self.probe.as_ref().is_some_and(|p| p.0 == wid) {
            let px = gl.read_back();
            self.probe = Some((wid, Some((px, w.x_size))));
        }
        let Some(target) = w.target.as_ref() else { return };
        self.gl.end(target);
        if let Some((_, v)) = retired {
            self.gl.drop_video(v);
        }
    }

    fn note_app(&mut self, wid: u32) {
        let Some(w) = self.windows.get(&wid) else { return };
        if w.popup || w.display {
            return;
        }
        let bid = w.info.bundle_id.clone();
        let Some(png) = self.icon_pngs.get(&bid) else { return };
        if !self.noted_apps.insert(bid.clone()) {
            return;
        }
        let (name, png) = (w.info.app_name.clone(), png.clone());
        std::thread::spawn(move || {
            if let Err(e) = crate::launchers::seen(&bid, &name, &png) {
                log::warn!("taskbar entry for {bid}: {e:#}");
            }
        });
    }

    /// The app's icon as the window's _NET_WM_ICON (taskbars, Alt+Tab).
    fn apply_icon(&mut self, wid: u32) {
        self.note_app(wid);
        let Some(w) = self.windows.get(&wid).filter(|w| !w.popup) else { return };
        let Some(png) = self.icon_pngs.get(&w.info.bundle_id) else { return };
        let Ok(data) = net_wm_icon(png) else { return };
        let _ = self.conn.change_property32(PropMode::REPLACE, w.xid, self.atoms._NET_WM_ICON, xproto::AtomEnum::CARDINAL, &data);
    }

    fn schedule_menu_panel(&self) {
        let _ = self.loop_handle.insert_source(Timer::from_duration(Duration::from_millis(150)), |_, _, app| {
            let want = app.kb_focus.is_some();
            if app.menu_panel_shown != Some(want) {
                app.menu_panel_shown = Some(want);
                crate::ui::set_menu_panel_visible(want);
            }
            TimeoutAction::Drop
        });
    }

    fn spot_drags(&self, wid: u32, x: f32, y: f32) -> bool {
        const NEAR: f32 = 6.0;
        const FRESH: Duration = Duration::from_secs(30);
        self.drag_spot.is_some_and(|(w, sx, sy, drags, at)| w == wid && drags && (sx - x).abs() <= NEAR && (sy - y).abs() <= NEAR && at.elapsed() <= FRESH)
    }

    fn moved_enough(&self, x: f64, y: f64) -> bool {
        let (px, py) = self.press_at;
        (x - px).abs() >= 2.0 || (y - py).abs() >= 2.0
    }

    /// The window manager moves (or resizes) the window from where the pointer is now.
    fn wm_moveresize(&mut self, xid: u32, direction: u32) {
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        // And the keyboard (held for the Mac's shortcuts, sync_keyboard_grab): Muffin takes it
        // for a move too (Esc ends one) and gives the move up when it can't, so no Mac window
        // moved on Cinnamon. Taken again when the pointer or the focus comes back after.
        if self.kb_grabbed {
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            self.kb_grabbed = false;
        }
        // The press's XInput 2 grab too, or the window manager can't take the pointer.
        if let Some(dev) = self.xi_pointer {
            let _ = self.conn.xinput_xi_ungrab_device(CURRENT_TIME, dev);
        }
        let (x, y) = self.pointer_root;
        let ev = xproto::ClientMessageEvent::new(32, xid, self.atoms._NET_WM_MOVERESIZE, [x as i32 as u32, y as i32 as u32, direction, 1, 1]);
        let _ = self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
        let _ = self.conn.flush();
    }

    fn move_here(&mut self, wid: u32) {
        self.move_ready = false;
        let Some(w) = self.windows.get(&wid) else { return };
        if w.popup {
            self.popup_drag = Some((wid, self.press_at.0, self.press_at.1));
        } else {
            let xid = w.xid;
            self.wm_moveresize(xid, MOVERESIZE_MOVE);
        }
        self.left_down = None;
        self.mac_drag = None;
        self.moved_here = Some(wid);
        self.send(W::new(t::MOVE_TAKEN).u32(wid).finish());
        let (_, x, y) = self.pointer_at;
        log::debug!(
            "window {wid}: move started {:.0} ms after the press, {:+.0},{:+.0} px from it ({})",
            self.press_time.elapsed().as_secs_f64() * 1000.0,
            x - self.press_at.0,
            y - self.press_at.1,
            if self.press_known { "a spot known from the hover" } else { "on the Mac's answer to the press" }
        );
    }

    fn end_selection(&mut self) {
        if let Some(sel) = self.selection.take() {
            self.draw(sel.wid);
        }
    }

    /// Tells the Mac how big our largest monitor is.
    fn send_screen_size(&self) {
        if !self.connected {
            return;
        }
        let s = &self.conn.setup().roots[self.screen];
        let best = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.monitors.iter().map(|m| (m.width as u32, m.height as u32)).max_by_key(|(w, h)| w * h))
            .unwrap_or((s.width_in_pixels as u32, s.height_in_pixels as u32));
        let (w, h) = ((best.0 as f64 / self.scale) as u16, (best.1 as f64 / self.scale) as u16);
        self.send(W::new(t::SCREEN_SIZE).u16(w).u16(h).finish());
    }

    /// The desktop's work area (x, y, w, h): the screen less its panels.
    fn work_area(&self) -> (i32, i32, i32, i32) {
        let s = &self.conn.setup().roots[self.screen];
        let whole = (0, 0, s.width_in_pixels as i32, s.height_in_pixels as i32);
        let Some(r) = self.conn.get_property(false, self.root, self.atoms._NET_WORKAREA, xproto::AtomEnum::CARDINAL, 0, 4).ok().and_then(|c| c.reply().ok()) else {
            return whole;
        };
        let v: Vec<u32> = r.value32().map(|i| i.collect()).unwrap_or_default();
        if v.len() < 4 {
            return whole;
        }
        (v[0] as i32, v[1] as i32, v[2] as i32, v[3] as i32)
    }

    /// Puts a floating window at its spot on the screen, as its app put it on the Mac's (X lets
    /// a window place itself, so no desktop script is needed as on Wayland).
    fn place_at_spot(&self, wid: u32, spot: (f32, f32)) {
        let Some(w) = self.windows.get(&wid) else { return };
        let (x, y) = spot_position(self.work_area(), w.size, spot);
        let _ = self.conn.configure_window(w.xid, &xproto::ConfigureWindowAux::new().x(x).y(y));
    }

    fn close(&mut self, wid: u32) {
        if self.connected && !self.paused {
            self.send(W::new(t::CLOSE).u32(wid).finish());
            self.hide(wid);
        } else {
            self.closed_offline.insert(wid);
            self.remove_window(wid);
        }
    }

    const HIDE_FOR: Duration = Duration::from_millis(1500);

    /// As ui.rs's hide: gone from the screen at once (nothing drawn, no input), the window and
    /// its taskbar entry kept until the Mac says it's gone.
    fn hide(&mut self, wid: u32) {
        self.unfreeze(wid);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.hidden.is_some() || !w.configured || w.display || w.popup {
            return;
        }
        let since = Instant::now();
        w.hidden = Some(since);
        if let Some((frame, v)) = w.shown.take() {
            w.pending = Some(frame);
            w.picture = None;
            // Dropped while drawing (below) with a window current.
            w.next = None;
            self.retire_video(wid, v);
        }
        self.clip_input(wid);
        self.draw(wid);
        log::info!("window {wid}: closing: hidden until the Mac confirms");
        let _ = self.loop_handle.insert_source(Timer::from_duration(Self::HIDE_FOR), move |_, _, app| {
            if app.windows.get(&wid).is_some_and(|w| w.hidden == Some(since)) {
                app.unhide(wid, "its app hasn't closed it");
            }
            TimeoutAction::Drop
        });
    }

    /// Frees an imported picture (with the window current).
    fn retire_video(&mut self, wid: u32, v: Video) {
        if let Some(t) = self.windows.get_mut(&wid).and_then(|w| w.target.as_mut()) {
            let _ = self.gl.begin(t, (1, 1), [0.0; 4]);
        }
        self.gl.drop_video(v);
    }

    fn unhide(&mut self, wid: u32, why: &str) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.hidden.take().is_none() {
            return;
        }
        log::info!("window {wid}: back: {why}");
        self.clip_input(wid);
        let pending = self.windows.get_mut(&wid).and_then(|w| w.pending.take());
        match pending {
            Some(frame) => self.present(wid, frame),
            None => self.draw(wid),
        }
        self.update_lights(wid);
    }

    fn expire_stale_soon(&self) {
        let _ = self.loop_handle.insert_source(Timer::from_duration(Duration::from_secs(3)), |_, _, app| {
            if !app.paused {
                let stale: Vec<u32> = app.windows.iter().filter(|(_, w)| w.stale).map(|(k, _)| *k).collect();
                for wid in stale {
                    app.remove_window(wid);
                }
                app.closed_offline.clear();
            }
            TimeoutAction::Drop
        });
    }

    fn freeze_all(&mut self) {
        let wids: Vec<u32> = self.windows.iter().filter(|(_, w)| !w.popup).map(|(k, _)| *k).collect();
        for wid in wids {
            self.freeze(wid);
        }
    }

    /// The window's last picture in black and white with a pause sign, over its video (ui.rs's).
    fn freeze(&mut self, wid: u32) {
        let s = self.scale;
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.still.is_some() || w.popup {
            return;
        }
        let Some((frame, _)) = &w.shown else { return };
        let (fw, fh) = (frame.width, frame.height);
        let Some(PausedStill { px, pos, src, dst }) =
            paused_still(frame, &w.info, w.display, w.fit, w.size, s, w.corners.as_ref().and_then(|c| c.last.as_ref()))
        else {
            return log::warn!("window {wid}: couldn't read its picture back for the paused still");
        };
        w.still = Some(Still { slot: Slot { pending: Some((fw, fh, px)), tex: None }, pos, src, dst });
        log::info!("window {wid}: paused still shown");
        self.draw(wid);
    }

    fn unfreeze(&mut self, wid: u32) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let Some(st) = w.still.take() else { return };
        if let Some(tex) = st.slot.tex {
            if let Some(t) = w.target.as_mut() {
                let _ = self.gl.begin(t, (1, 1), [0.0; 4]);
            }
            self.gl.drop_tex(tex);
        }
        self.draw(wid);
    }

    fn toggle_maximize(&self, wid: u32) {
        let Some(w) = self.windows.get(&wid).filter(|w| !w.popup) else { return };
        if !(w.floating || w.maximized) {
            return;
        }
        let a = &self.atoms;
        let ev = xproto::ClientMessageEvent::new(32, w.xid, a._NET_WM_STATE, [if w.maximized { 0 } else { 1 }, a._NET_WM_STATE_MAXIMIZED_VERT, a._NET_WM_STATE_MAXIMIZED_HORZ, 1, 0]);
        let _ = self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
    }

    /// The user pressed a key or a button: a focus kept from the Mac while windows arrived is
    /// passed on first (see arrival.rs).
    fn user_pressed(&mut self) {
        if let Some(wid) = self.arrival.input(self.kb_focus) {
            self.send(W::new(t::FOCUS).u32(wid).finish());
        }
    }

    /// Asks the window manager to focus one of the windows (the Mac's key window as they
    /// arrive), as a pager would (source 2).
    fn activate(&self, wid: u32) {
        let Some(w) = self.windows.get(&wid).filter(|w| !w.popup && !w.display) else { return };
        log::info!("window {wid}: focused here, as the Mac's key window");
        let ev = xproto::ClientMessageEvent::new(32, w.xid, self.atoms._NET_ACTIVE_WINDOW, [2, 0, 0, 0, 0]);
        let _ = self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
        let _ = self.conn.flush();
    }

    /// A launch of an app that already has windows here: its newest one (a minimised one first)
    /// back in front, asked of the window manager as a pager would (source 2).
    fn raise_app(&self, bundle_id: &str) {
        let Some(w) = self
            .windows
            .values()
            .filter(|w| w.info.bundle_id == bundle_id && !w.popup && !w.display)
            .max_by_key(|w| (w.mac_minimized, w.info.wid))
        else {
            return;
        };
        log::info!("launch of {bundle_id}: bringing window {} to the front", w.info.wid);
        let ev = xproto::ClientMessageEvent::new(32, w.xid, self.atoms._NET_ACTIVE_WINDOW, [2, 0, 0, 0, 0]);
        let _ = self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
        let _ = self.conn.flush();
    }

    fn iconify(&self, xid: u32) {
        let ev = xproto::ClientMessageEvent::new(32, xid, self.atoms.WM_CHANGE_STATE, [3 /* IconicState */, 0, 0, 0, 0]);
        let _ = self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
    }

    fn to_mac(&self, wid: u32, x: f64, y: f64) -> (f32, f32) {
        match self.windows.get(&wid) {
            Some(w) if w.display => {
                let (k, ox, oy) = w.fit;
                (((x - ox) / k).clamp(0.0, w.info.w as f64 - 1.0) as f32, ((y - oy) / k).clamp(0.0, w.info.h as f64 - 1.0) as f32)
            }
            _ => (x as f32, y as f32),
        }
    }

    fn edge_of(&self, wid: u32, x: f64, y: f64) -> Option<Edge> {
        let w = self.windows.get(&wid)?;
        // The display view has the desktop's frame to resize by.
        if w.popup || w.maximized || w.display || w.fixed {
            return None;
        }
        edge_at(w.size, x, y)
    }

    /// Redraws a window's traffic lights if their state changed.
    fn update_lights(&mut self, wid: u32) {
        let active = self.kb_focus == Some(wid);
        let pointer_over = self.pointer_focus == Some(wid);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.popup || w.hidden.is_some() {
            return;
        }
        let video = w.floated && !w.fixed && w.info.kind == 0;
        let Some(rect) = lights::group_rect(&w.buttons).filter(|_| !video || pointer_over) else {
            if w.lights_drawn.take().is_some() {
                self.draw(wid);
            }
            return;
        };
        let state = lights::State { active, hover: w.lights_hover, pressed: w.lights_pressed.and_then(|(i, inside)| inside.then_some(i)), dark_titlebar: w.dark_titlebar };
        if w.lights_drawn == Some((state, rect)) && (w.lights.tex.is_some() || w.lights.pending.is_some()) {
            return;
        }
        let (pw, ph, px) = lights::render(&w.buttons, rect, state);
        w.lights.pending = Some((pw, ph, px));
        w.lights_drawn = Some((state, rect));
        self.draw(wid);
    }

    fn lights_pointer(&mut self, wid: u32, x: f64, y: f64) {
        let over = self.button_at(wid, x, y);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let in_group = lights::group_rect(&w.buttons).is_some_and(|r| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]);
        w.lights_hover = in_group || w.lights_pressed.is_some();
        if let Some((i, _)) = w.lights_pressed {
            w.lights_pressed = Some((i, over == Some(i)));
        }
        self.update_lights(wid);
    }

    fn over_lights(&self, wid: u32) -> bool {
        self.windows.get(&wid).is_some_and(|w| w.lights_hover)
    }

    fn button_at(&self, wid: u32, x: f64, y: f64) -> Option<usize> {
        let w = self.windows.get(&wid)?;
        w.buttons.iter().position(|r| r[2] > 0.0 && x >= r[0] as f64 && x < (r[0] + r[2]) as f64 && y >= r[1] as f64 && y < (r[1] + r[3]) as f64)
    }

    fn named_cursor(&mut self, name: &'static str) -> Option<u32> {
        if let Some(c) = self.named_cursors.get(name) {
            return Some(*c);
        }
        let c = self.cursors.as_ref()?.load_cursor(&self.conn, name).ok()?;
        self.named_cursors.insert(name, c);
        Some(c)
    }

    fn set_window_cursor(&self, xid: u32, c: u32) {
        let _ = self.conn.change_window_attributes(xid, &xproto::ChangeWindowAttributesAux::new().cursor(c));
    }

    fn set_edge_cursor(&mut self, wid: u32, edge: Option<Edge>) {
        let Some(xid) = self.windows.get(&wid).map(|w| w.xid) else { return };
        match edge {
            Some(e) => {
                if let Some(c) = self.named_cursor(e.cursor()) {
                    self.set_window_cursor(xid, c);
                    self.edge_cursor = true;
                }
            }
            None if self.edge_cursor => {
                self.edge_cursor = false;
                if let Some(c) = self.mac_cursor.or_else(|| self.named_cursor("left_ptr")) {
                    self.set_window_cursor(xid, c);
                }
            }
            None => {}
        }
    }

    /// The Mac's cursor (drawn by the X server, so it moves with no delay) on every Mac window.
    fn set_mac_cursor(&mut self, hot_x: f32, hot_y: f32, scale: f32, png: &[u8]) {
        let Some(format) = self.argb_format else { return };
        let Ok((w, h, bgra)) = crate::ui::decode_png_argb(png) else { return };
        // The picture is `scale` pixels per point; here a point is self.scale pixels.
        let k = (scale as f64 / self.scale).round().max(1.0) as u32;
        let (cw, ch, px) = downscale(w, h, &bgra, k);
        let (hx, hy) = ((hot_x as f64 * self.scale).round().clamp(0.0, cw as f64 - 1.0) as u16, (hot_y as f64 * self.scale).round().clamp(0.0, ch as f64 - 1.0) as u16);
        let made = (|| -> Result<u32> {
            let pixmap = self.conn.generate_id()?;
            self.conn.create_pixmap(32, pixmap, self.root, cw as u16, ch as u16)?;
            let gc = self.conn.generate_id()?;
            self.conn.create_gc(gc, pixmap, &xproto::CreateGCAux::new())?;
            self.conn.put_image(xproto::ImageFormat::Z_PIXMAP, pixmap, gc, cw as u16, ch as u16, 0, 0, 0, 32, &px)?;
            let picture = self.conn.generate_id()?;
            self.conn.render_create_picture(picture, pixmap, format, &render::CreatePictureAux::new())?;
            let cursor = self.conn.generate_id()?;
            self.conn.render_create_cursor(cursor, picture, hx, hy)?;
            self.conn.render_free_picture(picture)?;
            self.conn.free_gc(gc)?;
            self.conn.free_pixmap(pixmap)?;
            Ok(cursor)
        })();
        let Ok(cursor) = made else { return };
        let old = self.mac_cursor.replace(cursor);
        if !self.edge_cursor {
            for w in self.windows.values() {
                self.set_window_cursor(w.xid, cursor);
            }
        }
        if let Some(old) = old {
            let _ = self.conn.free_cursor(old);
        }
    }

    fn app_window_focused(&self) -> bool {
        self.kb_focus.and_then(|wid| self.windows.get(&wid)).is_some_and(|w| w.info.kind != 3 && !w.popup)
    }

    fn load_keymap(&mut self) {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        if let Some(r) = self.conn.get_keyboard_mapping(min, max - min + 1).ok().and_then(|c| c.reply().ok()) {
            self.keysyms = (min, r.keysyms_per_keycode, r.keysyms);
        }
    }

    /// The key's first keysym (unshifted, first group).
    fn keysym(&self, keycode: u8) -> u32 {
        let (min, per, syms) = &self.keysyms;
        let i = (keycode.saturating_sub(*min) as usize) * *per as usize;
        syms.get(i).copied().unwrap_or(0)
    }

    // --- X events ----------------------------------------------------------------------------

    /// A Mac file drag has left its window with the button still held: carried on as an XDND
    /// drag from here (the Mac is told it's taken; the files come when a Linux app asks).
    fn xdnd_take(&mut self, wid: u32) {
        let Some(xid) = self.windows.get(&wid).map(|w| w.xid) else { return };
        let _ = self.conn.set_selection_owner(xid, self.atoms.XdndSelection, CURRENT_TIME);
        self.xdnd_out = Some(XdndOut { wid, source: xid, target: None, accepted: false, dropped: false, uris: None, waiting: Vec::new(), started: Instant::now() });
        self.mac_drag = None;
        self.send(W::new(t::DRAG_TAKEN).u32(wid).finish());
        log::info!("took over the Mac drag from window {wid}");
        self.drag_picture_show();
        self.xdnd_track();
    }

    /// A picture under the pointer while a file drag is out (as on Wayland): a window that
    /// takes no input, kept just below and right of the pointer.
    fn drag_picture_show(&mut self) {
        let (w, h, px) = crate::ui::drag_picture_pixels(1, self.drag_count);
        let Ok(xid) = self.conn.generate_id() else { return };
        let aux = xproto::CreateWindowAux::new().background_pixel(0).border_pixel(0).colormap(self.colormap).override_redirect(1);
        if self.conn.create_window(self.depth, xid, self.root, 0, 0, w as u16, h as u16, 0, xproto::WindowClass::INPUT_OUTPUT, self.gl.visual, &aux).is_err() {
            return;
        }
        // Clicks and drops go through it (to what's under it).
        let _ = self.conn.shape_rectangles(x11rb::protocol::shape::SO::SET, x11rb::protocol::shape::SK::INPUT, xproto::ClipOrdering::UNSORTED, xid, 0, 0, &[]);
        let _ = self.conn.map_window(xid);
        if let Ok(gc) = self.conn.generate_id() {
            let _ = self.conn.create_gc(gc, xid, &xproto::CreateGCAux::new());
            let _ = self.conn.put_image(xproto::ImageFormat::Z_PIXMAP, xid, gc, w as u16, h as u16, 0, 0, 0, self.depth, &px);
            let _ = self.conn.free_gc(gc);
        }
        self.drag_pic = Some((xid, (w, h)));
        let _ = self.conn.flush();
    }

    fn drag_picture_hide(&mut self) {
        if let Some((xid, _)) = self.drag_pic.take() {
            let _ = self.conn.destroy_window(xid);
            let _ = self.conn.flush();
        }
    }

    /// The window under the pointer that takes drops (XdndAware), not one of ours.
    fn xdnd_window_at(&self) -> Option<u32> {
        let mut w = self.root;
        for _ in 0..8 {
            let r = self.conn.query_pointer(w).ok()?.reply().ok()?;
            if r.child == x11rb::NONE {
                return None;
            }
            w = r.child;
            if self.by_xid.contains_key(&w) {
                return None;
            }
            let aware = self.conn.get_property(false, w, self.atoms.XdndAware, xproto::AtomEnum::ATOM, 0, 1).ok().and_then(|c| c.reply().ok());
            if aware.is_some_and(|a| a.value_len > 0) {
                return Some(w);
            }
        }
        None
    }

    /// Tells the window under the pointer about the drag (enter / position; leave for the one
    /// it left).
    fn xdnd_track(&mut self) {
        // One dropped long ago whose target never finished: forgotten.
        if self.xdnd_out.as_ref().is_some_and(|o| o.dropped && o.started.elapsed() > Duration::from_secs(120)) {
            self.xdnd_out = None;
        }
        let (rx, ry) = self.pointer_root;
        if let Some((pic, _)) = self.drag_pic {
            let _ = self.conn.configure_window(pic, &xproto::ConfigureWindowAux::new().x(rx as i32 + 14).y(ry as i32 + 14).stack_mode(xproto::StackMode::ABOVE));
        }
        let target = self.xdnd_window_at();
        let a = &self.atoms;
        let uri = self.atom("text/uri-list");
        let Some(o) = self.xdnd_out.as_mut() else { return };
        let msg = |to: u32, kind: u32, d: [u32; 5]| xproto::ClientMessageEvent::new(32, to, kind, d);
        if o.target != target {
            if let Some(old) = o.target {
                let _ = self.conn.send_event(false, old, EventMask::NO_EVENT, msg(old, a.XdndLeave, [o.source, 0, 0, 0, 0]));
            }
            if let Some(new) = target {
                let _ = self.conn.send_event(false, new, EventMask::NO_EVENT, msg(new, a.XdndEnter, [o.source, 5 << 24, uri, 0, 0]));
            }
            o.target = target;
            o.accepted = false;
        }
        if let Some(t) = o.target {
            let at = ((rx as u16 as u32) << 16) | (ry as u16 as u32);
            let _ = self.conn.send_event(false, t, EventMask::NO_EVENT, msg(t, a.XdndPosition, [o.source, 0, at, CURRENT_TIME, a.XdndActionCopy]));
        }
        let _ = self.conn.flush();
    }

    /// The button let go during a drag out: dropped on the window under it if it takes it,
    /// otherwise the drag ends with nothing dropped.
    fn xdnd_release(&mut self) {
        self.drag_picture_hide();
        let a = &self.atoms;
        let Some(o) = self.xdnd_out.as_mut() else { return };
        match (o.target, o.accepted) {
            (Some(t), true) => {
                let ev = xproto::ClientMessageEvent::new(32, t, a.XdndDrop, [o.source, 0, CURRENT_TIME, 0, 0]);
                let _ = self.conn.send_event(false, t, EventMask::NO_EVENT, ev);
                o.dropped = true;
                log::info!("drag out of Mac window {}: dropped on window {t:#x}", o.wid);
            }
            (target, _) => {
                if let Some(t) = target {
                    let ev = xproto::ClientMessageEvent::new(32, t, a.XdndLeave, [o.source, 0, 0, 0, 0]);
                    let _ = self.conn.send_event(false, t, EventMask::NO_EVENT, ev);
                }
                log::info!("drag out: let go where nothing takes it");
                self.xdnd_out = None;
            }
        }
        let _ = self.conn.flush();
    }

    /// A Linux app asks for the dropped files: answered now if the Mac has sent them, else once
    /// it has (asked for on the first request, as on Wayland).
    fn xdnd_request(&mut self, e: xproto::SelectionRequestEvent) {
        let property = if e.property == x11rb::NONE { e.target } else { e.property };
        let targets = self.atom("TARGETS");
        let uri = self.atom("text/uri-list");
        if e.target == targets {
            let _ = self.conn.change_property32(PropMode::REPLACE, e.requestor, property, xproto::AtomEnum::ATOM, &[targets, uri]);
            self.xdnd_notify(e.requestor, e.target, property, e.time);
            return;
        }
        let Some(o) = self.xdnd_out.as_mut() else {
            self.xdnd_notify(e.requestor, e.target, x11rb::NONE, e.time);
            return;
        };
        if e.target != uri {
            self.xdnd_notify(e.requestor, e.target, x11rb::NONE, e.time);
            return;
        }
        if let Some(uris) = o.uris.clone() {
            self.xdnd_answer(e.requestor, property, e.target, e.time, Some(&uris));
        } else {
            if o.waiting.is_empty() {
                let _ = self.net.send(NetCmd::ClipStream(W::new(t::DRAG_FETCH).finish()));
            }
            o.waiting.push((e.requestor, property, e.target, e.time));
        }
    }

    fn xdnd_answer(&self, requestor: u32, property: u32, target: u32, time: u32, uris: Option<&std::sync::Arc<Vec<u8>>>) {
        match uris {
            Some(u) => {
                let _ = self.conn.change_property8(PropMode::REPLACE, requestor, property, target, u);
                self.xdnd_notify(requestor, target, property, time);
            }
            None => self.xdnd_notify(requestor, target, x11rb::NONE, time),
        }
    }

    fn xdnd_notify(&self, requestor: u32, target: u32, property: u32, time: u32) {
        let ev = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time,
            requestor,
            selection: self.atoms.XdndSelection,
            target,
            property,
        };
        let _ = self.conn.send_event(false, requestor, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }

    fn atom_name(&self, atom: u32) -> String {
        self.conn.get_atom_name(atom).ok().and_then(|c| c.reply().ok()).map(|r| String::from_utf8_lossy(&r.name).into_owned()).unwrap_or_default()
    }

    fn atom(&self, name: &str) -> u32 {
        self.conn.intern_atom(false, name.as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom).unwrap_or(0)
    }

    /// XDND messages from a Linux app dragging over a Mac window (the ones that drop files on
    /// it): enter (what it offers), position (where; it's told whether we take it), leave, drop.
    fn xdnd_message(&mut self, window: u32, kind: u32, d: [u32; 5]) {
        let a = &self.atoms;
        let source = d[0];
        if kind == a.XdndEnter {
            let types: Vec<u32> = if d[1] & 1 != 0 {
                self.conn
                    .get_property(false, source, a.XdndTypeList, xproto::AtomEnum::ATOM, 0, 1024)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().map(|v| v.collect()))
                    .unwrap_or_default()
            } else {
                d[2..5].iter().copied().filter(|&t| t != 0).collect()
            };
            let types = types.into_iter().map(|t| self.atom_name(t)).collect();
            self.xdnd_in = Some(XdndIn { source, types, target: None, to_read: Vec::new(), read: Vec::new() });
        } else if kind == a.XdndPosition {
            let wid = self.wid_of(window);
            let (rx, ry) = ((d[2] >> 16) as i16, (d[2] & 0xffff) as i16);
            let local = self.conn.translate_coordinates(self.root, window, rx, ry).ok().and_then(|c| c.reply().ok()).map(|r| (r.dst_x as f64, r.dst_y as f64));
            let accept = match (&mut self.xdnd_in, wid, local) {
                (Some(x), Some(wid), Some((lx, ly))) if crate::tray::allows_drag() && crate::clipboard::drop_mime(&x.types).is_some() => {
                    x.target = Some((wid, lx, ly));
                    true
                }
                _ => false,
            };
            let status = xproto::ClientMessageEvent::new(32, source, self.atoms.XdndStatus, [window, if accept { 1 | 2 } else { 2 }, 0, 0, if accept { self.atoms.XdndActionCopy } else { 0 }]);
            let _ = self.conn.send_event(false, source, EventMask::NO_EVENT, status);
            let _ = self.conn.flush();
        } else if kind == a.XdndLeave {
            self.xdnd_in = None;
        } else if kind == a.XdndDrop {
            let Some(x) = self.xdnd_in.as_mut() else { return };
            if x.target.is_none() {
                self.xdnd_finish(window, false);
                return;
            }
            // Everything worth reading, one type at a time (SelectionNotify, xdnd_data).
            x.to_read = crate::clipboard::drop_reads(&x.types);
            self.xdnd_read_next(window, d[2]);
        }
    }

    fn xdnd_read_next(&mut self, window: u32, time: u32) {
        let Some(mime) = self.xdnd_in.as_mut().and_then(|x| x.to_read.first().cloned()) else {
            self.xdnd_dropped(window);
            return;
        };
        let target = self.atom(&mime);
        let _ = self.conn.convert_selection(window, self.atoms.XdndSelection, target, self.atoms.SWEEFLOW_DROP, time);
        let _ = self.conn.flush();
    }

    /// One type of a drop has arrived: the next, or, with all in, the drop goes to the Mac.
    fn xdnd_data(&mut self, e: xproto::SelectionNotifyEvent) {
        let Some(x) = self.xdnd_in.as_mut() else { return };
        let mime = if x.to_read.is_empty() { return } else { x.to_read.remove(0) };
        if e.property != x11rb::NONE {
            if let Ok(Ok(r)) = self.conn.get_property(true, e.requestor, self.atoms.SWEEFLOW_DROP, xproto::AtomEnum::ANY, 0, (crate::clipboard::MAX_BYTES / 4) as u32).map(|c| c.reply()) {
                if r.type_ != self.atoms.INCR && !r.value.is_empty() {
                    if let Some(x) = self.xdnd_in.as_mut() {
                        x.read.push((mime, r.value));
                    }
                }
            }
        }
        self.xdnd_read_next(e.requestor, e.time);
    }

    /// Everything of a drop read: sent to the Mac like a Wayland drop (DROP_FILES), and the
    /// source told it's done.
    fn xdnd_dropped(&mut self, window: u32) {
        let Some(x) = self.xdnd_in.take() else { return };
        let ok = !x.read.is_empty();
        if let Some((wid, lx, ly)) = x.target {
            let (mx, my) = self.to_mac(wid, lx, ly);
            let net = self.net.clone();
            let data = x.read;
            std::thread::spawn(move || {
                let items = crate::clipboard::drop_items(data);
                if items.is_empty() {
                    log::info!("nothing usable in the drop on Mac window {wid}");
                    return;
                }
                log::info!("dropping {:?} on Mac window {wid}", items.iter().map(|i| (&i.kind, &i.name)).collect::<Vec<_>>());
                let mut w = W::new(t::DROP_FILES).u32(wid).f32(mx).f32(my).u16(items.len() as u16);
                for it in &items {
                    w = w.str(&it.kind).str(&it.name).bytes(&it.data);
                }
                let _ = net.send(NetCmd::ClipStream(w.finish()));
            });
        }
        self.xdnd_in = Some(XdndIn { source: x.source, types: Vec::new(), target: None, to_read: Vec::new(), read: Vec::new() });
        self.xdnd_finish(window, ok);
        self.xdnd_in = None;
    }

    fn xdnd_finish(&mut self, window: u32, ok: bool) {
        let Some(source) = self.xdnd_in.as_ref().map(|x| x.source) else { return };
        let ev = xproto::ClientMessageEvent::new(32, source, self.atoms.XdndFinished, [window, ok as u32, if ok { self.atoms.XdndActionCopy } else { 0 }, 0, 0]);
        let _ = self.conn.send_event(false, source, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
        self.xdnd_in = None;
    }

    fn wid_of(&self, xid: u32) -> Option<u32> {
        self.by_xid.get(&xid).copied()
    }

    fn on_x(&mut self, ev: Event) {
        match ev {
            Event::Expose(e) if e.count == 0 => {
                if let Some(wid) = self.wid_of(e.window) {
                    self.draw(wid);
                }
            }
            Event::MapNotify(e) => {
                if let Some(wid) = self.wid_of(e.window) {
                    self.configure(wid, true);
                }
            }
            Event::ConfigureNotify(e) => {
                let Some(wid) = self.wid_of(e.window) else { return };
                let size = (e.width as u32, e.height as u32);
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.x_size = size;
                }
                self.configure(wid, false);
                // Its popups stay where they are on it.
                let kids: Vec<u32> = self.windows.values().filter(|c| c.info.parent == wid && c.popup).map(|c| c.info.wid).collect();
                for k in kids {
                    self.place_popup(k);
                }
            }
            Event::PropertyNotify(e) if e.atom == self.atoms._NET_WM_STATE => {
                if let Some(wid) = self.wid_of(e.window) {
                    self.read_state(wid);
                }
            }
            Event::ClientMessage(e) => {
                let d = e.data.as_data32();
                if e.type_ == self.atoms.WM_PROTOCOLS && d[0] == self.atoms.WM_DELETE_WINDOW {
                    if let Some(wid) = self.wid_of(e.window) {
                        self.close(wid);
                    }
                } else if e.type_ == self.atoms.XdndStatus {
                    if let Some(o) = self.xdnd_out.as_mut().filter(|o| o.target == Some(d[0])) {
                        o.accepted = d[1] & 1 != 0;
                    }
                } else if e.type_ == self.atoms.XdndFinished {
                    if self.xdnd_out.as_ref().is_some_and(|o| o.target == Some(d[0])) {
                        log::info!("drag out: done ({})", if d[1] & 1 != 0 { "taken" } else { "refused" });
                        self.xdnd_out = None;
                    }
                } else if [self.atoms.XdndEnter, self.atoms.XdndPosition, self.atoms.XdndLeave, self.atoms.XdndDrop].contains(&e.type_) {
                    self.xdnd_message(e.window, e.type_, d);
                } else if e.type_ == self.atoms.WM_PROTOCOLS && d[0] == self.atoms._NET_WM_PING {
                    let mut reply = e;
                    reply.window = self.root;
                    let _ = self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, reply);
                }
            }
            Event::MappingNotify(_) => self.load_keymap(),
            Event::SelectionNotify(e) if e.selection == self.atoms.XdndSelection => self.xdnd_data(e),
            Event::SelectionRequest(e) if e.selection == self.atoms.XdndSelection => self.xdnd_request(e),
            Event::EnterNotify(e) => {
                let Some(wid) = self.wid_of(e.event) else { return };
                self.pointer_root = (e.root_x, e.root_y);
                let (px, py) = (e.event_x as f64, e.event_y as f64);
                self.pointer_focus = Some(wid);
                self.sync_keyboard_grab();
                if self.xi_scroll.is_some() {
                    // Scroll valuators moved while the pointer was elsewhere.
                    self.query_scroll_devices();
                }
                self.lights_pointer(wid, px, py);
                if !self.over_lights(wid) {
                    let (x, y) = self.to_mac(wid, px, py);
                    self.send(W::new(t::POINTER_MOVE).u32(wid).f32(x).f32(y).finish());
                }
            }
            Event::LeaveNotify(e) => {
                let Some(wid) = self.wid_of(e.event) else { return };
                // (Not a grab's: a window move or a button's grab moves no pointer anywhere.)
                if self.pointer_focus == Some(wid) && e.mode == xproto::NotifyMode::NORMAL {
                    self.pointer_focus = None;
                    self.sync_keyboard_grab();
                }
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.lights_hover = false;
                }
                self.update_lights(wid);
            }
            Event::MotionNotify(e) => {
                if let Some(wid) = self.wid_of(e.event) {
                    self.pointer_root = (e.root_x, e.root_y);
                    self.motion(wid, e.event_x as f64, e.event_y as f64);
                }
            }
            Event::XinputMotion(e) => {
                let fp = |v: i32| v as f64 / 65536.0;
                if let Some(wid) = self.wid_of(e.event) {
                    self.pointer_root = (fp(e.root_x) as i16, fp(e.root_y) as i16);
                    self.scroll(wid, &e);
                    self.motion(wid, fp(e.event_x), fp(e.event_y));
                }
            }
            Event::XinputDeviceChanged(_) => self.query_scroll_devices(),
            Event::XinputButtonPress(e) | Event::XinputButtonRelease(e) => {
                let fp = |v: i32| v as f64 / 65536.0;
                let down = e.event_type == xinput::BUTTON_PRESS_EVENT;
                if let Some(wid) = self.wid_of(e.event) {
                    self.pointer_root = (fp(e.root_x) as i16, fp(e.root_y) as i16);
                    self.xi_pointer = Some(e.deviceid);
                    self.button(wid, e.detail as u8, down, fp(e.event_x), fp(e.event_y));
                }
            }
            Event::ButtonPress(e) => {
                if let Some(wid) = self.wid_of(e.event) {
                    self.pointer_root = (e.root_x, e.root_y);
                    self.button(wid, e.detail, true, e.event_x as f64, e.event_y as f64);
                }
            }
            Event::ButtonRelease(e) => {
                if let Some(wid) = self.wid_of(e.event) {
                    self.pointer_root = (e.root_x, e.root_y);
                    self.button(wid, e.detail, false, e.event_x as f64, e.event_y as f64);
                }
            }
            Event::FocusIn(e) => {
                if e.detail == xproto::NotifyDetail::POINTER || e.mode == xproto::NotifyMode::GRAB {
                    return;
                }
                if let Some(wid) = self.wid_of(e.event) {
                    self.focus_in(wid);
                }
            }
            Event::FocusOut(e) => {
                // Grabs come and go without the focus moving (ours, taken as a Mac window gets the
                // focus; or the desktop's for a moment): only a real move counts, including one
                // made while our grab holds (the window manager focusing a clicked window).
                if e.detail == xproto::NotifyDetail::POINTER || e.mode == xproto::NotifyMode::UNGRAB || e.mode == xproto::NotifyMode::GRAB {
                    return;
                }
                if self.wid_of(e.event).is_some() {
                    self.focus_out();
                }
            }
            Event::KeyPress(e) => self.key(e.detail, true, e.state.into()),
            Event::KeyRelease(e) => self.key(e.detail, false, e.state.into()),
            Event::Error(e) => log::debug!("X error: {e:?}"),
            _ => {}
        }
    }

    /// The window was mapped (first) or its size or state changed: as ui.rs's configure.
    fn configure(&mut self, wid: u32, mapped: bool) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if !mapped && !w.configured {
            return; // until it's mapped, sizes are ours
        }
        let first = !w.configured;
        w.configured = true;
        if w.popup {
            w.size = w.x_size;
        } else if w.display {
            let size = w.x_size;
            let k = (size.0 as f64 / w.info.w.max(1.0) as f64).min(size.1 as f64 / w.info.h.max(1.0) as f64);
            w.size = size;
            w.fit = (k, (size.0 as f64 - w.info.w as f64 * k) / 2.0, (size.1 as f64 - w.info.h as f64 * k) / 2.0);
        } else {
            let max = w.max_size.unwrap_or((u32::MAX, u32::MAX));
            let size = (w.x_size.0.clamp(w.min_size.0.min(max.0), max.0), w.x_size.1.clamp(w.min_size.1.min(max.1), max.1));
            if size != w.size || (first && size != (w.info.w as u32, w.info.h as u32)) {
                w.size = size;
                w.followed = false;
                w.requested = Some((size, Instant::now()));
                let _ = self.net.send(NetCmd::resize(wid, size));
                Self::arm_check(&self.loop_handle, &mut w.check_pending, wid);
            }
        }
        let pending = w.pending.take();
        let pending_raw = w.pending_raw.take();
        self.clip_input(wid);
        if let Some(f) = pending {
            self.present(wid, f);
        }
        if let Some((pw, ph, ppp, us, px)) = pending_raw {
            self.present_raw(wid, pw, ph, ppp, us, px);
        }
        self.update_lights(wid);
        self.draw(wid);
    }

    /// _NET_WM_STATE changed: maximised, fullscreen, minimised.
    fn read_state(&mut self, wid: u32) {
        let Some(xid) = self.windows.get(&wid).map(|w| w.xid) else { return };
        let Some(r) = self.conn.get_property(false, xid, self.atoms._NET_WM_STATE, xproto::AtomEnum::ATOM, 0, 64).ok().and_then(|c| c.reply().ok()) else { return };
        let states: Vec<u32> = r.value32().map(|i| i.collect()).unwrap_or_default();
        let a = &self.atoms;
        let has = |s: u32| states.contains(&s);
        let maximized = (has(a._NET_WM_STATE_MAXIMIZED_VERT) && has(a._NET_WM_STATE_MAXIMIZED_HORZ)) || has(a._NET_WM_STATE_FULLSCREEN);
        let suspended = has(a._NET_WM_STATE_HIDDEN);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        w.maximized = maximized;
        w.floating = !maximized;
        let was = w.suspended;
        w.suspended = suspended;
        if was && !suspended && w.mac_minimized {
            // Restored from the taskbar: restore it on the Mac too.
            w.mac_minimized = false;
            let _ = self.net.send(NetCmd::Control(W::new(t::RESTORE).u32(wid).finish()));
        }
        if was != suspended {
            let _ = self.net.send(NetCmd::Video(wid, W::new(t::PAUSE).u8(suspended as u8).finish()));
        }
    }

    fn motion(&mut self, wid: u32, px: f64, py: f64) {
        let (x, y) = self.to_mac(wid, px, py);
        self.pointer_at = (wid, px, py);
        if let Some(sel) = self.selection.as_mut().filter(|s| s.wid == wid) {
            sel.now = (px, py);
            self.draw(wid);
        }
        // A Mac file drag carried on outside its window: the Linux window under the pointer.
        if self.xdnd_out.is_some() {
            self.xdnd_track();
            return;
        }
        if self.mac_drag == Some(wid) && self.left_down == Some(wid) {
            let inside = self.windows.get(&wid).is_some_and(|w| px >= 0.0 && py >= 0.0 && px < w.size.0 as f64 && py < w.size.1 as f64);
            if !inside {
                self.xdnd_take(wid);
                return;
            }
        }
        let edge = self.edge_of(wid, px, py);
        self.set_edge_cursor(wid, edge);
        self.lights_pointer(wid, px, py);
        if self.windows.get(&wid).is_some_and(|w| w.lights_pressed.is_some()) || self.over_lights(wid) {
            return;
        }
        if self.left_down == Some(wid) && self.moved_enough(px, py) && !self.move_ready {
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3 {
                log::debug!("window {wid}: dragging with no move ready");
            }
        }
        if self.move_ready && self.left_down == Some(wid) && self.moved_enough(px, py) {
            self.move_here(wid);
            return;
        }
        if let Some((dw, gx, gy)) = self.popup_drag {
            if dw == wid {
                self.drag_popup(wid, gx, gy);
            }
            return;
        }
        self.send(W::new(t::POINTER_MOVE).u32(wid).f32(x).f32(y).finish());
    }

    /// The dragged popup to where the pointer has it (the point held under the pointer).
    fn drag_popup(&mut self, wid: u32, gx: f64, gy: f64) {
        let Some(w) = self.windows.get(&wid) else { return };
        let (xid, parent) = (w.xid, w.info.parent);
        let (x, y) = (self.pointer_root.0 as i32 - gx.round() as i32, self.pointer_root.1 as i32 - gy.round() as i32);
        let _ = self.conn.configure_window(xid, &xproto::ConfigureWindowAux::new().x(x).y(y));
        let _ = self.conn.flush();
        // Kept on its window from now on (placed again there when the window moves).
        let origin = self.windows.get(&parent).and_then(|p| self.conn.translate_coordinates(p.xid, self.root, 0, 0).ok()?.reply().ok());
        if let (Some(o), Some(w)) = (origin, self.windows.get_mut(&wid)) {
            w.moved_to = Some(((x - o.dst_x as i32) as f32, (y - o.dst_y as i32) as f32));
        }
    }

    fn button(&mut self, wid: u32, xb: u8, down: bool, px: f64, py: f64) {
        if down {
            self.user_pressed();
        }
        let (x, y) = self.to_mac(wid, px, py);
        if (4..=7).contains(&xb) {
            // The wheel as buttons, where XInput 2 doesn't give scroll valuators.
            if down && self.xi_scroll.as_ref().is_none_or(|d| d.values().all(|(_, axes)| axes.is_empty())) {
                let (dx, dy) = match xb {
                    4 => (0.0, 1.0),
                    5 => (0.0, -1.0),
                    6 => (1.0, 0.0),
                    _ => (-1.0, 0.0),
                };
                self.send(W::new(t::SCROLL).u32(wid).f32(x).f32(y).f32(dx).f32(dy).u8(0).u8(0).finish());
            }
            return;
        }
        let Some(button) = linux_button(xb) else { return };
        if down {
            if xb == 1 {
                if let Some(edge) = self.edge_of(wid, px, py) {
                    if let Some(xid) = self.windows.get(&wid).map(|w| w.xid) {
                        self.wm_moveresize(xid, edge as u32);
                    }
                    return;
                }
                if let Some(b) = self.button_at(wid, px, py) {
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.lights_pressed = Some((b, true));
                    }
                    self.update_lights(wid);
                    self.local_press = Some(xb);
                    return;
                }
                self.left_down = Some(wid);
                self.mac_drag = None;
                self.press_id = self.press_id.wrapping_add(1);
                self.press_at = (px, py);
                self.pointer_at = (wid, px, py);
                self.move_ready = self.spot_drags(wid, x, y);
                log::debug!("window {wid}: left press at {x:.0},{y:.0} (drag spot {:?}): {}", self.drag_spot.map(|s| (s.1, s.2, s.3)), if self.move_ready { "moves on the first motion" } else { "the Mac's" });
                self.press_time = Instant::now();
                self.press_known = self.move_ready;
                self.moved_here = None;
                if self.screen_select {
                    self.selection = Some(Selection { wid, start: (px, py), now: (px, py) });
                    self.draw(wid);
                }
            }
            self.send(W::new(t::POINTER_BUTTON).u32(wid).f32(x).f32(y).u8(mac_button(button)).u8(1).u32(self.press_id).finish());
        } else {
            if self.local_press == Some(xb) {
                self.local_press = None;
                let pressed = self.windows.get_mut(&wid).and_then(|w| w.lights_pressed.take());
                if let Some((b, true)) = pressed {
                    match b {
                        0 => self.close(wid),
                        1 => {
                            if let Some(xid) = self.windows.get(&wid).map(|w| w.xid) {
                                self.iconify(xid);
                            }
                        }
                        _ => self.toggle_maximize(wid),
                    }
                }
                self.lights_pointer(wid, px, py);
                return;
            }
            if xb == 1 && self.xdnd_out.as_ref().is_some_and(|o| !o.dropped) {
                // The drag out ends here: dropped where a Linux app said it would take it.
                self.left_down = None;
                self.xdnd_release();
                return;
            }
            if xb == 1 {
                self.left_down = None;
                self.mac_drag = None;
                self.move_ready = false;
                self.end_selection();
                // The end of a popup's drag: the Mac let go of its press when it began.
                if self.popup_drag.take().is_some() {
                    return;
                }
            }
            self.send(W::new(t::POINTER_BUTTON).u32(wid).f32(x).f32(y).u8(mac_button(button)).u8(0).finish());
        }
    }

    /// The pointer devices' scroll valuators, with their values now.
    fn query_scroll_devices(&mut self) {
        let Some(r) = self.conn.xinput_xi_query_device(0u16 /* all devices */).ok().and_then(|c| c.reply().ok()) else { return };
        let mut map = HashMap::new();
        for d in r.infos {
            let name = String::from_utf8_lossy(&d.name).to_lowercase();
            let touchpad = ["touchpad", "trackpad", "synaptics", "magic"].iter().any(|n| name.contains(n));
            let mut values = HashMap::new();
            let mut axes = Vec::new();
            for c in &d.classes {
                match &c.data {
                    xinput::DeviceClassData::Valuator(v) => {
                        values.insert(v.number, fp3232(v.value));
                    }
                    xinput::DeviceClassData::Scroll(s) => axes.push(ScrollAxis {
                        number: s.number,
                        vertical: s.scroll_type == xinput::ScrollType::VERTICAL,
                        increment: fp3232(s.increment).abs().max(1e-6),
                        last: None,
                    }),
                    _ => {}
                }
            }
            for a in &mut axes {
                a.last = values.get(&a.number).copied();
            }
            if !axes.is_empty() {
                map.insert(d.deviceid, (touchpad, axes));
            }
        }
        self.xi_scroll = Some(map);
    }

    /// Scrolling from XInput 2's scroll valuators: a touchpad's as smooth (pixel) scrolling
    /// with phases, a wheel's as notches (fractions too, from high-resolution wheels).
    fn scroll(&mut self, wid: u32, e: &xinput::MotionEvent) {
        let Some(devices) = self.xi_scroll.as_mut() else { return };
        let Some((touchpad, axes)) = devices.get_mut(&e.sourceid) else { return };
        // The valuators in the event, in order of their bits.
        let mut values = Vec::new();
        let mut k = 0;
        for (word, bits) in e.valuator_mask.iter().enumerate() {
            for bit in 0..32 {
                if bits & (1 << bit) != 0 {
                    if let Some(v) = e.axisvalues.get(k) {
                        values.push(((word * 32 + bit) as u16, fp3232(*v)));
                    }
                    k += 1;
                }
            }
        }
        let (mut dx, mut dy) = (0.0, 0.0);
        let mut notches = (0.0, 0.0);
        for a in axes.iter_mut() {
            let Some(&(_, v)) = values.iter().find(|(n, _)| *n == a.number) else { continue };
            let Some(last) = a.last.replace(v) else { continue };
            let d = v - last;
            if a.vertical {
                dy += d;
                notches.1 += d / a.increment;
            } else {
                dx += d;
                notches.0 += d / a.increment;
            }
        }
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let touchpad = *touchpad;
        let (x, y) = self.to_mac(wid, self.pointer_at.1, self.pointer_at.2);
        if touchpad {
            let phase = if self.scroll_active.is_some() { 2 } else { 1 };
            self.scroll_active = Some((wid, Instant::now()));
            self.send(W::new(t::SCROLL).u32(wid).f32(x).f32(y).f32(-dx as f32).f32(-dy as f32).u8(1).u8(phase).finish());
            self.arm_scroll_end();
        } else {
            self.send(W::new(t::SCROLL).u32(wid).f32(x).f32(y).f32(-notches.0 as f32).f32(-notches.1 as f32).u8(0).u8(0).finish());
        }
    }

    /// X says nothing when fingers lift: the scroll ends once it has rested a moment.
    fn arm_scroll_end(&self) {
        const REST: Duration = Duration::from_millis(80);
        let _ = self.loop_handle.insert_source(Timer::from_duration(REST), |_, _, app| {
            let Some((wid, at)) = app.scroll_active else { return TimeoutAction::Drop };
            if at.elapsed() < REST {
                return TimeoutAction::Drop; // a later timer ends it
            }
            app.scroll_active = None;
            let (x, y) = app.to_mac(wid, app.pointer_at.1, app.pointer_at.2);
            app.send(W::new(t::SCROLL).u32(wid).f32(x).f32(y).f32(0.0).f32(0.0).u8(1).u8(3).finish());
            TimeoutAction::Drop
        });
    }

    fn focus_in(&mut self, wid: u32) {
        if self.kb_focus == Some(wid) {
            return;
        }
        log::debug!("keyboard focus → {wid}: focusing it on the Mac");
        let before = self.kb_focus.replace(wid);
        if let Some(b) = before {
            self.update_lights(b);
        }
        if let Some(info) = self.windows.get(&wid).map(|w| &w.info).filter(|i| i.kind != 3) {
            crate::menus::focused(info.pid, &info.app_name);
        }
        match self.arrival.focus(wid) {
            Ok(()) => self.send(W::new(t::FOCUS).u32(wid).finish()),
            Err(mac_key) => {
                if let Some(k) = mac_key {
                    self.activate(k);
                }
            }
        }
        self.update_lights(wid);
        self.schedule_menu_panel();
        self.sync_keyboard_grab();
    }

    /// Every key to the Mac while one of its windows has the focus and the pointer is over a
    /// Mac window, as the shortcuts inhibitor does on Wayland: the window manager's own keys
    /// (Cinnamon takes Super for its menu, and with it every Cmd shortcut) wait meanwhile. With
    /// the pointer elsewhere (the panel, another window) the keyboard is let go: the panel's
    /// menus, which need it, open, and Alt+Tab works.
    fn sync_keyboard_grab(&mut self) {
        // (And while a modifier is still held: Super+Tab, Tab, Tab… all go to the switcher.)
        let held_off = self.grab_hold_off.is_some_and(|t| t > std::time::Instant::now() || self.meta_down > 0);
        let want = self.kb_focus.zip(self.pointer_focus).and_then(|(f, _)| self.windows.get(&f).map(|w| w.xid)).filter(|_| !held_off);
        match (want, self.kb_grabbed) {
            (Some(xid), false) => {
                let r = self
                    .conn
                    .grab_keyboard(true, xid, CURRENT_TIME, xproto::GrabMode::ASYNC, xproto::GrabMode::ASYNC)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| r.status);
                self.kb_grabbed = r == Some(xproto::GrabStatus::SUCCESS);
                if !self.kb_grabbed {
                    log::debug!("keyboard not taken ({r:?}): the desktop keeps its keys");
                }
            }
            (None, true) => {
                let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
                let _ = self.conn.flush();
                self.kb_grabbed = false;
            }
            _ => {}
        }
    }

    fn focus_out(&mut self) {
        let was = self.kb_focus.take();
        self.sync_keyboard_grab();
        log::debug!("keyboard focus left {was:?}");
        if let Some(wid) = was {
            self.update_lights(wid);
        }
        self.schedule_menu_panel();
        self.meta_down = 0;
        self.shift_down = 0;
        self.keys_down.clear();
        self.swallowed.clear();
        self.send(W::new(t::RELEASE_ALL).finish());
    }

    fn key(&mut self, keycode: u8, down: bool, state: u16) {
        if down {
            self.user_pressed();
        }
        let code = evdev_code(keycode);
        let is_meta = code == 125 || code == 126;
        let is_shift = code == 42 || code == 54;
        if !down {
            self.keys_down.remove(&keycode);
            if is_meta {
                self.meta_down = self.meta_down.saturating_sub(1);
            }
            if is_shift {
                self.shift_down = self.shift_down.saturating_sub(1);
            }
            if let Some(i) = self.swallowed.iter().position(|&k| k == code) {
                self.swallowed.remove(i);
                return;
            }
            self.send(W::new(t::KEY).u32(code).u8(0).finish());
            return;
        }
        if !self.keys_down.insert(keycode) {
            // Held: the key's repeat.
            if self.swallowed.contains(&code) {
                return;
            }
            self.send(W::new(t::KEY).u32(code).u8(2).finish());
            return;
        }
        if is_meta {
            self.meta_down += 1;
        }
        if is_shift {
            self.shift_down += 1;
        }
        let mods = Mods::from_state(state);
        match cmd_opt_letter(mods, self.keysym(keycode)) {
            Some('u') if self.app_window_focused() => {
                crate::ui::open_global_menu();
                self.swallowed.push(code);
                return;
            }
            Some('o') => {
                crate::ui::run_own("open", "Sweeflow");
                self.swallowed.push(code);
                return;
            }
            Some(c @ ('y' | 'z')) => {
                let toggle = if c == 'y' { "pause-toggle" } else { "stream-toggle" };
                std::thread::spawn(move || crate::tray::panel_do(toggle, ""));
                self.swallowed.push(code);
                return;
            }
            _ => {}
        }
        if code == KEY_SPACE && self.meta_down > 0 && self.shift_down == 0 && crate::keys::desktop().contains("Cinnamon") {
            // The desktop's menu takes the keyboard to open: let it go first (the focus moving
            // there then releases the Mac's keys, see focus_out).
            self.grab_hold_off = Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
            self.sync_keyboard_grab();
            crate::ui::open_search();
            self.swallowed.push(code);
            return;
        }
        // Keys for windows and the desktop stay the desktop's (deskkeys): the keyboard is let
        // go, and the key pressed again for the window manager (XTest), whose binding runs; one
        // it doesn't use comes back to the Mac window as usual.
        let m = crate::deskkeys::Mods { logo: self.meta_down > 0, alt: state & 8 != 0, ctrl: state & 4 != 0, shift: self.shift_down > 0 };
        if self.kb_grabbed && crate::deskkeys::is_desktop_key(code, m) {
            use x11rb::protocol::xtest::ConnectionExt as _;
            self.grab_hold_off = Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
            self.sync_keyboard_grab();
            let _ = self.conn.xtest_fake_input(xproto::KEY_PRESS_EVENT, keycode, CURRENT_TIME, self.root, 0, 0, 0);
            let _ = self.conn.xtest_fake_input(xproto::KEY_RELEASE_EVENT, keycode, CURRENT_TIME, self.root, 0, 0, 0);
            let _ = self.conn.flush();
            log::debug!("key {code} with {m:?}: handed to the window manager");
            self.swallowed.push(code);
            return;
        }
        self.send(W::new(t::KEY).u32(code).u8(1).finish());
    }
}

fn fp3232(v: xinput::Fp3232) -> f64 {
    v.integral as f64 + v.frac as f64 / 4294967296.0
}

/// Premultiplied BGRA made `k` times smaller (each pixel the mean of a k × k block).
fn downscale(w: u32, h: u32, px: &[u8], k: u32) -> (u32, u32, Vec<u8>) {
    if k <= 1 {
        return (w, h, px.to_vec());
    }
    let (nw, nh) = ((w / k).max(1), (h / k).max(1));
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let mut sum = [0u32; 4];
            let mut n = 0;
            for yy in y * k..((y + 1) * k).min(h) {
                for xx in x * k..((x + 1) * k).min(w) {
                    let i = ((yy * w + xx) * 4) as usize;
                    for c in 0..4 {
                        sum[c] += px[i + c] as u32;
                    }
                    n += 1;
                }
            }
            let o = ((y * nw + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (sum[c] / n.max(1)) as u8;
            }
        }
    }
    (nw, nh, out)
}

/// An icon (PNG) as _NET_WM_ICON wants it: width, height, then ARGB pixels (not premultiplied),
/// at most 256 pixels wide.
fn net_wm_icon(png: &[u8]) -> Result<Vec<u32>> {
    let (w, h, bgra) = crate::ui::decode_png_argb(png)?;
    let k = w.div_ceil(256).max(1);
    let (w, h, bgra) = downscale(w, h, &bgra, k);
    let mut out = Vec::with_capacity((w * h + 2) as usize);
    out.extend([w, h]);
    for p in bgra.chunks_exact(4) {
        let a = p[3] as u32;
        let un = |c: u8| if a == 0 { 0 } else { (c as u32 * 255 / a).min(255) };
        out.push(a << 24 | un(p[2]) << 16 | un(p[1]) << 8 | un(p[0]));
    }
    Ok(out)
}

// --- self-test -------------------------------------------------------------------------------

/// `sweeflow x11-selftest [seconds]`: with no Mac, shows made-up windows the way a Mac's would
/// be: a video picture (a VA-API dmabuf in NV12, another in P010), drawn through EGL with
/// rounded corner tiles and traffic lights, and a lossless popup. Checks the colours drawn,
/// leaves a screenshot of each window in the runtime folder (with `import`, if installed), and
/// closes them after a couple of seconds.
pub fn selftest(args: &[String]) -> Result<()> {
    let seconds: f64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(2.0);
    let event_loop: EventLoop<App> = EventLoop::try_new()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(1.0, "self-test".into(), Some(tx), event_loop.handle())?;
    app.rounded_test = true;
    let node = crate::config::default_render_node();
    // Bands of BT.709 limited-range colours: red, green, blue, white, mid grey.
    let bands = [(63u8, 102u8, 240u8), (173, 42, 26), (32, 240, 118), (235, 128, 128), (126, 128, 128)];
    let expected = [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255], [128, 128, 128]];
    let (fw, fh) = (640u32, 400u32);
    let band = |x: u32| bands[(x * bands.len() as u32 / fw) as usize];
    let mut failures = Vec::new();
    let mut shots = Vec::new();
    for (wid, ten_bit) in [(1u32, false), (2, true)] {
        let frame = crate::decode::synthetic_frame(&node, (fw, fh), ten_bit, false, |x, _| band(x))?;
        log::info!("self-test: {} picture from {node}: {} planes, modifier {:#x}", if ten_bit { "P010" } else { "NV12" }, frame.planes.len(), frame.planes[0].modifier);
        let info = WinInfo {
            wid,
            pid: 1,
            app_name: "Sweeflow self-test".into(),
            bundle_id: "test.sweeflow".into(),
            title: if ten_bit { "P010".into() } else { "NV12".into() },
            w: (fw / 2) as f32,
            h: (fh / 2) as f32,
            rel_x: 0.0,
            rel_y: 0.0,
            kind: 0,
            parent: 0,
        };
        app.on_net(UiEvent::WindowAdded(info));
        // Round corner tiles: transparent outside a quarter circle, white inside.
        let t = 16u32;
        let mut tiles = vec![0u8; (t * t * 4 * 4) as usize];
        for i in 0..4u32 {
            for y in 0..t {
                for x in 0..t {
                    let (cx, cy) = (if i % 2 == 0 { t } else { 0 }, if i < 2 { t } else { 0 });
                    let d = ((x as f64 + 0.5 - cx as f64).powi(2) + (y as f64 + 0.5 - cy as f64).powi(2)).sqrt();
                    let a = if d <= t as f64 { 255 } else { 0 };
                    let o = ((i * t * t + y * t + x) * 4) as usize;
                    tiles[o..o + 4].copy_from_slice(&[a, a, a, a]);
                }
            }
        }
        app.on_net(UiEvent::Corners { wid, t_px: t, tiles });
        app.on_net(UiEvent::WindowButtons(wid, [[8.0, 30.0, 14.0, 14.0], [28.0, 30.0, 14.0, 14.0], [48.0, 30.0, 14.0, 14.0]]));
        let mut frame = frame;
        frame.px_per_pt = 2;
        app.windows.get_mut(&wid).ok_or_else(|| anyhow!("window {wid} wasn't made"))?.pending = Some(frame);
    }
    // A popup on the first window: a lossless half-transparent magenta square.
    let popup = WinInfo { wid: 3, pid: 1, app_name: "Sweeflow self-test".into(), bundle_id: "test.sweeflow".into(), title: "menu".into(), w: 60.0, h: 40.0, rel_x: 100.0, rel_y: 120.0, kind: 1, parent: 1 };
    app.on_net(UiEvent::WindowAdded(popup));
    let magenta: Vec<u8> = (0..60 * 40).flat_map(|_| [128u8, 0, 128, 128]).collect();
    app.on_net(UiEvent::RawFrame { wid: 3, w: 60, h: 40, px_per_pt: 1, capture_us: 7, bgra: magenta });

    let mut event_loop = event_loop;
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    // The windows get mapped and drawn; then each one's drawing is read back.
    pump(&mut app, &mut event_loop, Duration::from_millis(700))?;
    let mut checks = Vec::new();
    for wid in [1u32, 2, 3] {
        app.probe = Some((wid, None));
        app.draw(wid);
        checks.push((wid, app.probe.take().and_then(|p| p.1)));
    }
    // Paused: the windows become black-and-white stills (read back from the GPU).
    app.on_net(UiEvent::Streaming(false));
    app.probe = Some((1, None));
    app.draw(1);
    match app.probe.take().and_then(|p| p.1) {
        Some((px, (w, _))) => {
            let i = (((fh / 4) * w + w / 10) * 4) as usize;
            let p = &px[i..i + 4];
            println!("paused still, red band: RGBA {p:?} (expected grey, about [54, 54, 54, 255])");
            if !(p[0] == p[1] && p[1] == p[2] && p[0].abs_diff(54) <= 4 && p[3] == 255) {
                failures.push(format!("paused still: {p:?}"));
            }
        }
        None => failures.push("paused still never drawn".into()),
    }
    let runtime = crate::config::runtime_dir();
    // Rounded corners show the desktop through them (unless the setting squares them).
    let rounded = !app.square_corners();
    println!("corners: {}", if rounded { "rounded (see-through)" } else { "square (the setting, or no 32-bit visual)" });
    for (wid, got) in checks {
        let Some((px, (w, h))) = got else {
            failures.push(format!("window {wid}: never drawn"));
            continue;
        };
        let at = |x: u32, y: u32| -> [u8; 4] {
            let i = ((y * w + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        if wid == 3 {
            let p = at(w / 2, h / 2);
            println!("popup {wid}: {w}x{h}, centre RGBA {p:?} (expected [128, 0, 128, 128])");
            if p.iter().zip([128u8, 0, 128, 128]).any(|(a, b)| a.abs_diff(b) > 3) {
                failures.push(format!("popup: centre {p:?}"));
            }
        } else {
            println!("window {wid} ({}): {w}x{h}", if wid == 2 { "P010" } else { "NV12" });
            for (i, e) in expected.iter().enumerate() {
                let x = (w * (2 * i as u32 + 1)) / (2 * expected.len() as u32);
                // Half way down the picture (the window here may be taller: a tiling desktop).
                let p = at(x, (fh / 4).min(h - 1));
                let ok = p[..3].iter().zip(e).all(|(a, b)| a.abs_diff(*b) <= 6) && p[3] == 255;
                println!("  band {i}: RGBA {p:?}, expected {e:?}: {}", if ok { "ok" } else { "WRONG" });
                if !ok {
                    failures.push(format!("window {wid} band {i}: {p:?} not {e:?}"));
                }
            }
            let corner = at(0, (fh / 2).min(h) - 1);
            println!("  picture's bottom-left corner pixel RGBA {corner:?}, top-left {:?} (expected see-through: alpha 0)", at(0, 0));
            if rounded && at(0, 0)[3] != 0 {
                failures.push(format!("window {wid}: top-left corner not see-through"));
            }
            if rounded && corner[3] != 0 {
                failures.push(format!("window {wid}: corner not see-through ({corner:?})"));
            }
            let light = at(15, 37);
            println!("  close button pixel RGBA {light:?} (expected the traffic light's red, not the video's)");
            if light[..3] == expected[0] {
                failures.push(format!("window {wid}: no traffic light drawn"));
            }
        }
        let path = runtime.join(format!("sweeflow-x11-selftest-{wid}.png"));
        crate::ui::dump_bgra(&path.to_string_lossy(), w, h, &px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect::<Vec<u8>>());
        shots.push(path.clone());
        if let Some(xid) = app.windows.get(&wid).map(|w| w.xid) {
            let shot = runtime.join(format!("sweeflow-x11-selftest-{wid}-screen.png"));
            let r = std::process::Command::new("import").args(["-window", &xid.to_string()]).arg(&shot).status();
            if r.is_ok_and(|s| s.success()) {
                shots.push(shot);
            }
        }
    }
    // The rest of the time on screen, then everything closes.
    let left = deadline.saturating_duration_since(Instant::now());
    pump(&mut app, &mut event_loop, left)?;
    let mut sent = HashMap::<&str, u32>::new();
    while let Ok(m) = rx.try_recv() {
        let k = match m {
            NetCmd::Control(_) => "control",
            NetCmd::Video(..) => "video (frame acks)",
            NetCmd::Subscribe(_) => "subscribe",
            NetCmd::Unsubscribe(_) => "unsubscribe",
            _ => "other",
        };
        *sent.entry(k).or_default() += 1;
    }
    println!("sent to the (absent) Mac: {sent:?}");
    for wid in [3, 2, 1] {
        app.remove_window(wid);
    }
    app.conn.flush()?;
    for s in &shots {
        println!("picture: {}", s.display());
    }
    if failures.is_empty() {
        println!("X11 self-test passed");
        Ok(())
    } else {
        anyhow::bail!("X11 self-test failed:\n  {}", failures.join("\n  "))
    }
}

/// Runs the loop for `time` (the self-test's).
fn pump(app: &mut App, lp: &mut EventLoop<'static, App>, time: Duration) -> Result<()> {
    let until = Instant::now() + time;
    loop {
        app.drain_x()?;
        app.conn.flush()?;
        if Instant::now() >= until {
            return Ok(());
        }
        lp.dispatch(Duration::from_millis(20), app)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_choice() {
        assert!(!choose(None, true, true), "Wayland session (with XWayland): Wayland");
        assert!(choose(None, false, true), "X11 session: X11");
        assert!(!choose(None, false, false), "neither: Wayland (fails as before)");
        assert!(choose(Some("x11"), true, true));
        assert!(!choose(Some("wayland"), false, true));
    }

    #[test]
    fn keys() {
        assert_eq!(evdev_code(38), 30, "A: X keycode 38 is evdev KEY_A (30)");
        assert_eq!(evdev_code(133), 125, "left Super is KEY_LEFTMETA");
        let m = Mods::from_state(0x40 | 0x08);
        assert_eq!(m, Mods { logo: true, alt: true, ctrl: false, shift: false });
        assert_eq!(cmd_opt_letter(m, 0x75), Some('u'));
        assert_eq!(cmd_opt_letter(m, 0x55), Some('u'), "a capital keysym too");
        assert_eq!(cmd_opt_letter(Mods::from_state(0x40 | 0x08 | 0x01), 0x75), None, "with Shift");
        assert_eq!(cmd_opt_letter(Mods::from_state(0x40), 0x75), None, "Super alone");
        assert_eq!(cmd_opt_letter(m, 0xff0d), None, "not a letter");
        // Caps Lock (0x02) and Num Lock (Mod2, 0x10) don't count.
        assert_eq!(cmd_opt_letter(Mods::from_state(0x40 | 0x08 | 0x02 | 0x10), 0x79), Some('y'));
    }

    #[test]
    fn buttons() {
        assert_eq!(linux_button(1).map(mac_button), Some(0));
        assert_eq!(linux_button(3).map(mac_button), Some(1));
        assert_eq!(linux_button(2).map(mac_button), Some(2));
        assert_eq!(linux_button(8), Some(0x113));
        assert_eq!(linux_button(4), None);
    }

    #[test]
    fn edges_and_spots() {
        assert_eq!(edge_at((400, 300), 2.0, 2.0), Some(Edge::TopLeft));
        assert_eq!(edge_at((400, 300), 200.0, 298.0), Some(Edge::Bottom));
        assert_eq!(edge_at((400, 300), 398.0, 290.0), Some(Edge::BottomRight));
        assert_eq!(edge_at((400, 300), 200.0, 150.0), None);
        assert_eq!(spot_position((0, 30, 1920, 1050), (400, 300), (0.0, 0.0)), (0, 30));
        assert_eq!(spot_position((0, 30, 1920, 1050), (400, 300), (1.0, 1.0)), (1520, 780));
        assert_eq!(spot_position((0, 0, 300, 200), (400, 300), (0.5, 0.5)), (0, 0), "bigger than the room");
    }

    #[test]
    fn icons_and_cursors_scale_down() {
        let px = vec![255u8; 4 * 4 * 4];
        let (w, h, out) = downscale(4, 4, &px, 2);
        assert_eq!((w, h, out.len()), (2, 2, 16));
        // Half-transparent white, premultiplied → ARGB unpremultiplied.
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 1, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&[255, 255, 255, 128]).unwrap();
        }
        let icon = net_wm_icon(&png).unwrap();
        assert_eq!(icon[..2], [1, 1]);
        assert_eq!(icon[2] >> 24, 128);
        assert!((icon[2] & 0xff) >= 254, "{:#x}", icon[2]);
    }
}
