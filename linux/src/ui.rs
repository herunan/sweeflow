//! Wayland frontend: one native toplevel per Mac window (popups/menus become xdg_popups).
//!
//! Each window is a base surface filled with a single-pixel background, sized to whatever the
//! compositor (or tiling script) configures, plus a desynchronised subsurface that shows the
//! decoded video 1:1. Video frames are GPU dmabufs straight from the VA-API decoder.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::Result;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    dmabuf::{DmabufFeedback, DmabufHandler, DmabufState},
    output::{OutputHandler, OutputState},
    reexports::{
        csd_frame::WindowState,
        calloop::{
            channel::{self, Channel},
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle,
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState, SimpleGlobal},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            popup::{Popup, PopupConfigure, PopupHandler},
            window::{DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgPositioner, XdgShell, XdgSurface,
        },
        WaylandSurface,
    },
    shm::{raw::RawPool, Shm, ShmHandler},
    subcompositor::SubcompositorState,
};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_subsurface, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::{
    wp::{
        color_representation::v1::client::{
            wp_color_representation_manager_v1::WpColorRepresentationManagerV1,
            wp_color_representation_surface_v1::{self as crs, WpColorRepresentationSurfaceV1},
        },
        cursor_shape::v1::client::{
            wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
            wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
        },
        keyboard_shortcuts_inhibit::zv1::client::{
            zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
            zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
        },
        single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
        presentation_time::client::{
            wp_presentation::WpPresentation,
            wp_presentation_feedback::{self, WpPresentationFeedback},
        },
    },
    xdg::{
        shell::client::{xdg_positioner, xdg_toplevel},
        toplevel_icon::v1::client::{
            xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
            xdg_toplevel_icon_v1::XdgToplevelIconV1,
        },
    },
};

use wayland_protocols_plasma::appmenu::client::{
    org_kde_kwin_appmenu::OrgKdeKwinAppmenu, org_kde_kwin_appmenu_manager::OrgKdeKwinAppmenuManager,
};

use crate::{
    decode::GpuFrame,
    lights,
    menus::Menus,
    net::{NetCmd, UiEvent, WinInfo},
    proto::{t, W},
};

/// Rounded corners: the video is shown as three strips (top, middle, bottom) that leave the four
/// corner squares uncovered; those show transparent corner tiles from the Mac. Strips and tiles
/// are synchronised children of the middle strip, so each frame updates atomically.
struct Corners {
    /// Corner tile size in buffer pixels (0 = square corners).
    t_px: u32,
    top: (wl_subsurface::WlSubsurface, wl_surface::WlSurface, WpViewport),
    bottom: (wl_subsurface::WlSubsurface, wl_surface::WlSurface, WpViewport),
    tiles: Vec<(wl_subsurface::WlSubsurface, wl_surface::WlSurface, WpViewport)>,
    tile_buffers: std::collections::VecDeque<(Vec<wl_buffer::WlBuffer>, RawPool)>,
    /// New tile pixels to upload with the next frame.
    pending: Option<(u32, Vec<u8>)>,
    /// Whether the split layout was used for the last frame.
    split: bool,
    /// Middle strip's offset from the window's top edge (logical px).
    middle_y: i32,
    /// Colour representation of the top and bottom strips (they show the video too).
    colors: Vec<WpColorRepresentationSurfaceV1>,
}

struct Lights {
    surface: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    viewport: WpViewport,
    buffers: std::collections::VecDeque<(wl_buffer::WlBuffer, RawPool)>,
    drawn: Option<(lights::State, [f64; 4])>,
}

enum Role {
    Toplevel(Window),
    Popup(Popup),
}

struct Win {
    info: WinInfo,
    role: Role,
    base_viewport: WpViewport,
    video: wl_surface::WlSurface,
    video_sub: wl_subsurface::WlSubsurface,
    video_viewport: WpViewport,
    color: Option<WpColorRepresentationSurfaceV1>,
    /// Range last declared to the compositor (None until the first frame).
    color_full_range: Option<bool>,
    _inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    configured: bool,
    /// Logical size of the window as configured by the compositor.
    size: (u32, u32),
    /// Last size we asked the Mac for, and when (to ignore stale Mac-side sizes mid-resize).
    requested: Option<((u32, u32), Instant)>,
    floating: bool,
    maximized: bool,
    suspended: bool,
    /// Traffic lights (close, minimise, zoom) in window points.
    buttons: [[f32; 4]; 3],
    /// Our drawing of the traffic lights (a subsurface above the video).
    lights: Option<Lights>,
    corners: Option<Corners>,
    /// Points KWin's Global Menu at this app's menu bar.
    appmenu: Option<OrgKdeKwinAppmenu>,
    dark_titlebar: bool,
    /// Minimised on the Mac (not by us): restoring it here must restore it there.
    mac_minimized: bool,
    /// Pointer over the traffic-light group.
    lights_hover: bool,
    /// Traffic light being pressed, and whether the pointer is still on it.
    lights_pressed: Option<(usize, bool)>,
    /// Frame waiting for the first configure.
    pending: Option<GpuFrame>,
    /// Lossless frame waiting for the first configure.
    pending_raw: Option<(u16, u16, u8, u64, Vec<u8>)>,
    /// Recent shm buffers of lossless frames (kept alive until the compositor is surely done).
    raw_buffers: std::collections::VecDeque<(wl_buffer::WlBuffer, RawPool)>,
    stale: bool,
}

impl Win {
    fn base(&self) -> &wl_surface::WlSurface {
        match &self.role {
            Role::Toplevel(w) => w.wl_surface(),
            Role::Popup(p) => p.wl_surface(),
        }
    }
}

pub struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    subcompositor: SubcompositorState,
    xdg: XdgShell,
    dmabuf: DmabufState,
    shm: Shm,
    viewporter: SimpleGlobal<WpViewporter, 1>,
    single_pixel: SimpleGlobal<WpSinglePixelBufferManagerV1, 1>,
    color_rep: Option<WpColorRepresentationManagerV1>,
    inhibit_mgr: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    icon_mgr: Option<XdgToplevelIconManagerV1>,
    cursor_mgr: Option<WpCursorShapeManagerV1>,
    /// For measuring hover latency (SWEEFLOW_TRACE=1): when traffic-light updates hit the screen.
    presentation: Option<WpPresentation>,
    appmenu_mgr: Option<OrgKdeKwinAppmenuManager>,
    menus: Option<Menus>,
    trace: bool,
    loop_handle: LoopHandle<'static, App>,

    net: UnboundedSender<NetCmd>,
    windows: HashMap<u32, Win>,
    surfaces: HashMap<wayland_client::backend::ObjectId, u32>,
    /// wl_buffer → the GPU frame it shows; dropped on release.
    in_use: HashMap<wayland_client::backend::ObjectId, GpuFrame>,
    icons: HashMap<String, (XdgToplevelIconV1, wl_buffer::WlBuffer)>,
    icon_pngs: HashMap<String, Vec<u8>>,
    background: Option<wl_buffer::WlBuffer>,
    /// Fully transparent 1x1 buffer: base for popups (menus have rounded, translucent edges).
    clear: Option<wl_buffer::WlBuffer>,

    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    pointer_focus: Option<u32>,
    kb_focus: Option<u32>,
    scroll_active: bool,
    wheel_accum: (f64, f64),
    /// Serial of the last pointer enter, needed to set the cursor image.
    enter_serial: u32,
    /// The Mac's current cursor, drawn locally so it moves with zero latency.
    cursor_surface: Option<wl_surface::WlSurface>,
    cursor_buffer: Option<(wl_buffer::WlBuffer, RawPool)>,
    cursor_hot: (i32, i32),
    meta_down: u8,
    shift_down: u8,
    /// Keys we handled locally (e.g. Cmd+Tab) whose release must not reach the Mac either.
    swallowed: Vec<u32>,
    /// Serial of the last button press forwarded to the Mac (for handing drags to KWin).
    press_serial: u32,
    /// A pointer button handled locally (traffic light, resize edge): swallow its release.
    local_press: Option<u32>,
    /// Showing a resize cursor over a window edge instead of the Mac cursor.
    edge_cursor: bool,
    scale: f64,
    host: String,
    connected: bool,
    trace_event_ms: Option<u32>,
    /// Whether the Global Menu panel is currently shown (it only shows for Mac windows).
    menu_panel_shown: Option<bool>,
}

pub fn run(net_cfg: crate::config::Config) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let compositor = CompositorState::bind(&globals, &qh)?;
    let subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)?;
    let bind_opt = |name: &str| globals.contents().with_list(|l| l.iter().any(|g| g.interface == name));

    let (ui_tx, ui_rx): (channel::Sender<UiEvent>, Channel<UiEvent>) = channel::channel();
    let scale = net_cfg.scale;
    let host = net_cfg.host.clone();
    let net = crate::net::spawn(net_cfg, ui_tx);

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        xdg: XdgShell::bind(&globals, &qh)?,
        dmabuf: DmabufState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh)?,
        viewporter: SimpleGlobal::bind(&globals, &qh)?,
        single_pixel: SimpleGlobal::bind(&globals, &qh)?,
        color_rep: bind_opt("wp_color_representation_manager_v1").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        inhibit_mgr: bind_opt("zwp_keyboard_shortcuts_inhibit_manager_v1")
            .then(|| globals.bind(&qh, 1..=1, ()).ok())
            .flatten(),
        icon_mgr: bind_opt("xdg_toplevel_icon_manager_v1").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        cursor_mgr: bind_opt("wp_cursor_shape_manager_v1").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        presentation: bind_opt("wp_presentation").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        appmenu_mgr: bind_opt("org_kde_kwin_appmenu_manager").then(|| globals.bind(&qh, 1..=2, ()).ok()).flatten(),
        menus: None,
        trace: std::env::var_os("SWEEFLOW_TRACE").is_some(),
        compositor,
        subcompositor,
        loop_handle: event_loop.handle(),
        net,
        windows: HashMap::new(),
        surfaces: HashMap::new(),
        in_use: HashMap::new(),
        icons: HashMap::new(),
        icon_pngs: HashMap::new(),
        background: None,
        clear: None,
        seat: None,
        keyboard: None,
        pointer: None,
        cursor_device: None,
        pointer_focus: None,
        kb_focus: None,
        scroll_active: false,
        wheel_accum: (0.0, 0.0),
        enter_serial: 0,
        cursor_surface: None,
        cursor_buffer: None,
        cursor_hot: (0, 0),
        meta_down: 0,
        shift_down: 0,
        swallowed: Vec::new(),
        press_serial: 0,
        local_press: None,
        edge_cursor: false,
        scale,
        host,
        connected: false,
        trace_event_ms: None,
        menu_panel_shown: None,
    };

    // A 1x1 opaque buffer stretched behind the video; shows while a resize is catching up.
    app.menus = match Menus::new(app.net.clone()) {
        Ok(m) => Some(m),
        Err(e) => {
            log::warn!("no D-Bus session: Mac menus won't appear in the Global Menu ({e})");
            None
        }
    };

    let bg = app.single_pixel.get()?.create_u32_rgba_buffer(0x1e1e_1e1e, 0x1e1e_1e1e, 0x1e1e_1e1e, u32::MAX, &qh, ());
    app.background = Some(bg);
    app.clear = Some(app.single_pixel.get()?.create_u32_rgba_buffer(0, 0, 0, 0, &qh, ()));

    let qh2 = qh.clone();
    event_loop
        .handle()
        .insert_source(ui_rx, move |ev, _, app: &mut App| {
            if let channel::Event::Msg(ev) = ev {
                let kind = ev.kind();
                let t0 = Instant::now();
                app.on_net(ev, &qh2);
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                if ms > 30.0 {
                    log::warn!("UI stall: handling {kind} took {ms:.0} ms");
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    log::info!("sweeflow client started, connecting to {}", app.host);
    loop {
        let t0 = Instant::now();
        event_loop.dispatch(Duration::from_millis(100), &mut app)?;
        // A dispatch that returns late without having waited the full timeout was busy.
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if ms > 130.0 {
            log::warn!("UI stall: event loop iteration took {ms:.0} ms");
        }
        if let Some(e) = conn.protocol_error() {
            anyhow::bail!("Wayland protocol error: {e:?}");
        }
    }
}

impl App {
    fn send(&self, m: Vec<u8>) {
        let _ = self.net.send(NetCmd::Control(m));
    }

    fn on_net(&mut self, ev: UiEvent, qh: &QueueHandle<Self>) {
        match ev {
            UiEvent::Connected(name) => {
                log::info!("connected to {name}");
                self.connected = true;
                self.send_screen_size();
                // Windows from before a reconnect are kept (so tiling positions survive a Wi-Fi
                // blip) unless the Mac doesn't re-announce them shortly.
                for w in self.windows.values_mut() {
                    w.stale = true;
                }
                let _ = self.loop_handle.insert_source(Timer::from_duration(Duration::from_secs(3)), |_, _, app| {
                    let stale: Vec<u32> = app.windows.iter().filter(|(_, w)| w.stale).map(|(k, _)| *k).collect();
                    for wid in stale {
                        app.remove_window(wid);
                    }
                    TimeoutAction::Drop
                });
            }
            UiEvent::Disconnected(why) => {
                self.connected = false;
                log::warn!("disconnected: {why}");
            }
            UiEvent::WindowAdded(info) => self.add_window(info, qh),
            UiEvent::WindowChanged { wid, title, w, h, rel_x, rel_y } => {
                if let Some(win) = self.windows.get_mut(&wid) {
                    if let Role::Toplevel(t) = &win.role {
                        if title != win.info.title {
                            t.set_title(display_title(&win.info.app_name, &title));
                        }
                    }
                    win.info.title = title;
                    win.info.w = w;
                    win.info.h = h;
                    win.info.rel_x = rel_x;
                    win.info.rel_y = rel_y;
                    self.adopt_mac_size(wid, qh);
                }
            }
            UiEvent::WindowRemoved(wid) => self.remove_window(wid),
            UiEvent::AppIcon { bundle_id, png } => {
                self.icon_pngs.insert(bundle_id.clone(), png);
                let wids: Vec<u32> =
                    self.windows.values().filter(|w| w.info.bundle_id == bundle_id).map(|w| w.info.wid).collect();
                for wid in wids {
                    self.apply_icon(wid, qh);
                }
            }
            UiEvent::Cursor { hot_x, hot_y, scale, png } => self.set_mac_cursor(hot_x, hot_y, scale, &png, qh),
            UiEvent::Focused(_) => {}
            UiEvent::WindowButtons(wid, rects) => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.buttons = rects;
                }
                self.update_lights(wid, qh);
            }
            UiEvent::MoveStarted(wid) => {
                if let (Some(w), Some(seat)) = (self.windows.get(&wid), &self.seat) {
                    if let Role::Toplevel(t) = &w.role {
                        t.move_(seat, self.press_serial);
                    }
                }
            }
            UiEvent::ToggleMaximize(wid) => self.toggle_maximize(wid),
            UiEvent::MenuTree { pid, json } => {
                if let Some(m) = self.menus.as_mut() {
                    m.update(pid, &json);
                }
            }
            UiEvent::Minimize(wid) => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.mac_minimized = true;
                    if let Role::Toplevel(t) = &w.role {
                        t.set_minimized();
                    }
                }
            }
            UiEvent::Frame(wid, frame) => self.present(wid, frame, qh),
            UiEvent::RawFrame { wid, w, h, px_per_pt, capture_us, bgra } => {
                self.present_raw(wid, w, h, px_per_pt, capture_us, bgra, qh)
            }
            UiEvent::Corners { wid, t_px, tiles } => self.set_corners(wid, t_px, tiles, qh),
        }
    }

    fn add_window(&mut self, info: WinInfo, qh: &QueueHandle<Self>) {
        let wid = info.wid;
        log::info!("window {wid} added: {} kind {} parent {} {}x{} at {},{}",
                   info.app_name, info.kind, info.parent, info.w, info.h, info.rel_x, info.rel_y);
        if let Some(w) = self.windows.get_mut(&wid) {
            // Re-announced after a reconnect: keep the native window, restart the video.
            w.stale = false;
            w.info = info;
            let _ = self.net.send(NetCmd::Subscribe(wid));
            return;
        }
        let (role, base) = if info.kind == 1 && self.windows.contains_key(&info.parent) {
            let parent = &self.windows[&info.parent];
            let parent_xdg = match &parent.role {
                Role::Toplevel(w) => w.xdg_surface().clone(),
                Role::Popup(p) => p.xdg_surface().clone(),
            };
            let pos = XdgPositioner::new(&self.xdg).expect("positioner");
            pos.set_size(info.w.max(1.0) as i32, info.h.max(1.0) as i32);
            pos.set_anchor_rect(info.rel_x.round() as i32, info.rel_y.round() as i32, 1, 1);
            pos.set_anchor(xdg_positioner::Anchor::TopLeft);
            pos.set_gravity(xdg_positioner::Gravity::BottomRight);
            pos.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::None);
            let popup = match Popup::new(&parent_xdg, &pos, qh, &self.compositor, &self.xdg) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("popup for {wid}: {e}");
                    return;
                }
            };
            let s = popup.wl_surface().clone();
            (Role::Popup(popup), s)
        } else {
            let surface = self.compositor.create_surface(qh);
            // The Mac window brings its own title bar and traffic lights; no KDE frame around it.
            let window = self.xdg.create_window(surface, WindowDecorations::RequestClient, qh);
            window.set_title(display_title(&info.app_name, &info.title));
            window.set_app_id(format!("sweeflow-{}", info.bundle_id));
            // Floating panels (kind 2: reminders, palettes) are ordinary, independent windows
            // here too: own taskbar entry, tiled like the rest.
            if info.kind != 2 {
                window.set_min_size(Some((100, 60)));
            }
            window.commit();
            let s = window.wl_surface().clone();
            (Role::Toplevel(window), s)
        };

        let viewporter = self.viewporter.get().expect("wp_viewporter");
        let base_viewport = viewporter.get_viewport(&base, qh, ());
        let (video_sub, video) = self.subcompositor.create_subsurface(base.clone(), qh);
        video_sub.set_desync();
        video_sub.set_position(0, 0);
        let video_viewport = viewporter.get_viewport(&video, qh, ());
        // All pointer input lands on the base surface (window coordinates), however the video
        // is split up on top of it.
        let empty = self.compositor.wl_compositor().create_region(qh, ());
        video.set_input_region(Some(&empty));
        empty.destroy();
        // Only HEVC windows carry YUV video; popups show RGB images, which must not be tagged.
        let is_video = !matches!(role, Role::Popup(_));
        // BT.709 YUV; the range is declared per frame from what the stream signals.
        let color = self.color_rep.as_ref().filter(|_| is_video).map(|m| m.get_surface(&video, qh, ()));
        let inhibitor = match (&role, &self.inhibit_mgr, &self.seat) {
            (Role::Toplevel(_), Some(m), Some(seat)) => Some(m.inhibit_shortcuts(&base, seat, qh, ())),
            _ => None,
        };

        let appmenu = match (&role, &self.appmenu_mgr, &self.menus) {
            (Role::Toplevel(_), Some(mgr), Some(menus)) => {
                let am = mgr.create(&base, qh, ());
                am.set_address(menus.service(), Menus::object_path(info.pid));
                Some(am)
            }
            _ => None,
        };
        self.surfaces.insert(base.id(), wid);
        self.surfaces.insert(video.id(), wid);
        let size = (info.w.max(1.0) as u32, info.h.max(1.0) as u32);
        let is_popup = matches!(role, Role::Popup(_));
        self.windows.insert(
            wid,
            Win {
                info,
                role,
                base_viewport,
                video,
                video_sub,
                video_viewport,
                color,
                color_full_range: None,
                _inhibitor: inhibitor,
                configured: false,
                size,
                requested: None,
                floating: true,
                maximized: false,
                suspended: false,
                buttons: [[0.0; 4]; 3],
                lights: None,
                corners: None,
                appmenu,
                dark_titlebar: true,
                mac_minimized: false,
                lights_hover: false,
                lights_pressed: None,
                pending: None,
                pending_raw: None,
                raw_buffers: Default::default(),
                stale: false,
            },
        );
        if !is_popup {
            self.apply_icon(wid, qh);
        }
        let _ = self.net.send(NetCmd::Subscribe(wid));
    }

    fn remove_window(&mut self, wid: u32) {
        let Some(w) = self.windows.remove(&wid) else { return };
        log::info!("window {wid} removed ({})", w.info.app_name);
        let _ = self.net.send(NetCmd::Unsubscribe(wid));
        self.surfaces.remove(&w.base().id());
        self.surfaces.remove(&w.video.id());
        if self.pointer_focus == Some(wid) {
            self.pointer_focus = None;
        }
        if self.kb_focus == Some(wid) {
            self.kb_focus = None;
        }
        // Children first: a popup must not outlive its parent.
        let children: Vec<u32> = self.windows.values().filter(|c| c.info.parent == wid).map(|c| c.info.wid).collect();
        for c in children {
            self.remove_window(c);
        }
        for (b, _) in &w.raw_buffers {
            b.destroy();
        }
        if let Some(am) = w.appmenu {
            am.release();
        }
        if let Some(c) = w.corners {
            for cr in &c.colors {
                cr.destroy();
            }
            for (bufs, _) in &c.tile_buffers {
                bufs.iter().for_each(|b| b.destroy());
            }
            for (sub, surf, vp) in [c.top, c.bottom].into_iter().chain(c.tiles) {
                vp.destroy();
                sub.destroy();
                surf.destroy();
            }
        }
        if let Some(l) = w.lights {
            for (b, _) in &l.buffers {
                b.destroy();
            }
            l.viewport.destroy();
            l.sub.destroy();
            l.surface.destroy();
        }
        w.video_viewport.destroy();
        w.video_sub.destroy();
        w.video.destroy();
        w.base_viewport.destroy();
        drop(w.role);
    }

    /// The Mac changed the window size itself (app-initiated resize). Follow it if the window
    /// is floating and we are not in the middle of our own resize.
    fn adopt_mac_size(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let mac = (w.info.w.round() as u32, w.info.h.round() as u32);
        if mac == w.size || !w.configured {
            return;
        }
        if let Some((_, at)) = w.requested {
            if at.elapsed() < Duration::from_millis(1500) {
                return;
            }
        }
        match &w.role {
            Role::Toplevel(_) if w.floating => {
                w.size = mac;
                self.draw_base(wid, qh);
            }
            Role::Popup(p) => {
                let pos = XdgPositioner::new(&self.xdg).expect("positioner");
                pos.set_size(mac.0.max(1) as i32, mac.1.max(1) as i32);
                pos.set_anchor_rect(w.info.rel_x.round() as i32, w.info.rel_y.round() as i32, 1, 1);
                pos.set_anchor(xdg_positioner::Anchor::TopLeft);
                pos.set_gravity(xdg_positioner::Gravity::BottomRight);
                pos.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::None);
                p.reposition(&pos, 0);
                w.size = mac;
                self.draw_base(wid, qh);
            }
            _ => {}
        }
    }

    fn draw_base(&mut self, wid: u32, _qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get(&wid) else { return };
        let (bw, bh) = w.size;
        if let Role::Toplevel(t) = &w.role {
            t.xdg_surface().set_window_geometry(0, 0, bw as i32, bh as i32);
        }
        let base = w.base();
        // Transparent: the window's rounded corners must show the desktop behind them.
        let fill = &self.clear;
        base.attach(fill.as_ref(), 0, 0);
        w.base_viewport.set_destination(bw as i32, bh as i32);
        base.damage_buffer(0, 0, i32::MAX, i32::MAX);
        base.commit();
    }

    fn present(&mut self, wid: u32, frame: GpuFrame, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.dark_titlebar != frame.dark_titlebar {
            w.dark_titlebar = frame.dark_titlebar;
            self.update_lights(wid, qh);
        }
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let full_range = frame.full_range;
        if w.color_full_range != Some(full_range) {
            let range = if full_range { crs::Range::Full } else { crs::Range::Limited };
            // Every surface showing this video (middle and the corner layout's strips) must
            // agree, or the strips show as bars in a different brightness.
            for c in w.color.iter().chain(w.corners.iter().flat_map(|c| c.colors.iter())) {
                c.set_coefficients_and_range(crs::Coefficients::Bt709, range);
            }
            log::info!("window {wid}: video is {} range", if full_range { "full" } else { "limited" });
            w.color_full_range = Some(full_range);
        }
        let capture_us = frame.capture_us;
        // Always ack so the Mac keeps streaming, even if we drop this frame.
        let ack = W::new(t::FRAME_ACK).u64(capture_us).finish();
        let _ = self.net.send(NetCmd::Video(wid, ack));
        if !w.configured {
            w.pending = Some(frame);
            return;
        }
        if w.suspended {
            return;
        }

        let params = match self.dmabuf.create_params(qh) {
            Ok(p) => p,
            Err(e) => {
                log::error!("dmabuf unavailable: {e}");
                return;
            }
        };
        for (i, p) in frame.planes.iter().enumerate() {
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(p.fd) };
            params.add(fd, i as u32, p.offset, p.stride, p.modifier);
        }
        let (buffer, _) = params.create_immed(
            frame.width as i32,
            frame.height as i32,
            frame.format,
            wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
        );

        // Map the frame to the window's size in points (the frame may be 2x Retina pixels; the
        // compositor then downsamples it), cropped to the window if the Mac lags a resize.
        let s = self.scale;
        let (bw, bh) = w.size;
        let (fw, fh) = (frame.width, frame.height);
        let px_per_pt = px_per_pt(frame.px_per_pt, fw, w.info.w, s);
        let vis_w = (fw as f64 / px_per_pt).min(bw as f64 / s);
        let vis_h = (fh as f64 / px_per_pt).min(bh as f64 / s);
        let (dw, dh) = ((vis_w * s).round().max(1.0) as i32, (vis_h * s).round().max(1.0) as i32);

        // Apps like Terminal snap to character cells, so the Mac window can differ from ours by
        // a point or two; the corners then go on the visible area (a sliver may be cropped).
        let t_px = w.corners.as_ref().map_or(0, |c| c.t_px);
        let split = t_px > 0 && (vis_w * px_per_pt) as u32 > 3 * t_px && (vis_h * px_per_pt) as u32 > 3 * t_px;
        let l = ((t_px as f64 / px_per_pt) * s).round() as i32; // corner size, logical px
        let (sw, sh) = ((vis_w * px_per_pt) as f64, (vis_h * px_per_pt) as f64);
        let middle_y = if split { l } else { 0 };
        let base = w.base().clone();
        if let Some(c) = w.corners.as_mut() {
            if c.middle_y != middle_y {
                c.middle_y = middle_y;
                w.video_sub.set_position(0, middle_y);
                base.commit(); // subsurface positions apply with the parent's commit
            }
            if split {
                let t = t_px as f64;
                // Children of the middle strip: positions are relative to it.
                let (tsub, tsurf, tvp) = &c.top;
                tsub.set_position(l, -l);
                tvp.set_source(t, 0.0, sw - 2.0 * t, t);
                tvp.set_destination(dw - 2 * l, l);
                tsurf.attach(Some(&buffer), 0, 0);
                tsurf.damage_buffer(0, 0, fw as i32, fh as i32);
                tsurf.commit();
                let (bsub, bsurf, bvp) = &c.bottom;
                bsub.set_position(l, dh - 2 * l);
                bvp.set_source(t, sh - t, sw - 2.0 * t, t);
                bvp.set_destination(dw - 2 * l, l);
                bsurf.attach(Some(&buffer), 0, 0);
                bsurf.damage_buffer(0, 0, fw as i32, fh as i32);
                bsurf.commit();
                let pos = [(0, -l), (dw - l, -l), (0, dh - 2 * l), (dw - l, dh - 2 * l)];
                let new_tiles = c.pending.take();
                if let Some((tp, px)) = &new_tiles {
                    if let Ok(mut pool) = RawPool::new(px.len(), &self.shm) {
                        pool.mmap().copy_from_slice(px);
                        let n = (*tp * *tp * 4) as usize;
                        let bufs: Vec<_> = (0..4)
                            .map(|i| {
                                pool.create_buffer((i * n) as i32, *tp as i32, *tp as i32, (*tp * 4) as i32,
                                                   wl_shm::Format::Argb8888, (), qh)
                            })
                            .collect();
                        for (i, (_, surf, _)) in c.tiles.iter().enumerate() {
                            surf.attach(Some(&bufs[i]), 0, 0);
                            surf.damage_buffer(0, 0, *tp as i32, *tp as i32);
                        }
                        c.tile_buffers.push_back((bufs, pool));
                        while c.tile_buffers.len() > 3 {
                            if let Some((old, _)) = c.tile_buffers.pop_front() {
                                old.iter().for_each(|b| b.destroy());
                            }
                        }
                    }
                } else if !c.split {
                    // Re-show the current tiles after a stretch without the split layout.
                    if let Some((bufs, _)) = c.tile_buffers.back() {
                        for (i, (_, surf, _)) in c.tiles.iter().enumerate() {
                            surf.attach(Some(&bufs[i]), 0, 0);
                        }
                    }
                }
                for (i, (sub, surf, vp)) in c.tiles.iter().enumerate() {
                    sub.set_position(pos[i].0, pos[i].1);
                    vp.set_destination(l, l);
                    surf.commit();
                }
            } else if c.split {
                for (_, surf, _) in [&c.top, &c.bottom].into_iter().chain(c.tiles.iter()) {
                    surf.attach(None, 0, 0);
                    surf.commit();
                }
            }
            if c.split != split {
                log::info!("window {wid}: rounded corners {} (frame {fw}x{fh}, window {bw}x{bh}, t {t_px})",
                           if split { "on" } else { "off" });
            }
            c.split = split;
        }
        if split {
            let t = t_px as f64;
            w.video_viewport.set_source(0.0, t, sw, sh - 2.0 * t);
            w.video_viewport.set_destination(dw, dh - 2 * l);
        } else {
            w.video_viewport.set_source(0.0, 0.0, sw, sh);
            w.video_viewport.set_destination(dw, dh);
        }
        w.video.attach(Some(&buffer), 0, 0);
        w.video.damage_buffer(0, 0, fw as i32, fh as i32);
        w.video.commit();
        self.in_use.insert(buffer.id(), frame);
    }

    /// New corner tiles from the Mac; shown from the next frame on.
    fn set_corners(&mut self, wid: u32, t_px: u32, tile_px: Vec<u8>, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get(&wid) else { return };
        if !matches!(w.role, Role::Toplevel(_)) {
            return;
        }
        if w.corners.is_none() {
            let video = w.video.clone();
            let viewporter = self.viewporter.get().expect("wp_viewporter").clone();
            let make = || {
                let (sub, surf) = self.subcompositor.create_subsurface(video.clone(), qh);
                let vp = viewporter.get_viewport(&surf, qh, ());
                let empty = self.compositor.wl_compositor().create_region(qh, ());
                surf.set_input_region(Some(&empty));
                empty.destroy();
                (sub, surf, vp)
            };
            let top = make();
            let bottom = make();
            let tiles = (0..4).map(|_| make()).collect();
            let colors: Vec<_> = self
                .color_rep
                .iter()
                .flat_map(|m| [m.get_surface(&top.1, qh, ()), m.get_surface(&bottom.1, qh, ())])
                .collect();
            let range_now = self.windows.get(&wid).and_then(|w| w.color_full_range);
            if let Some(full) = range_now {
                let range = if full { crs::Range::Full } else { crs::Range::Limited };
                for c in &colors {
                    c.set_coefficients_and_range(crs::Coefficients::Bt709, range);
                }
            }
            let corners = Corners {
                t_px: 0,
                top,
                bottom,
                tiles,
                tile_buffers: Default::default(),
                pending: None,
                split: false,
                middle_y: 0,
                colors,
            };
            if let Some(w) = self.windows.get_mut(&wid) {
                w.corners = Some(corners);
            }
        }
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let c = w.corners.as_mut().unwrap();
        if c.t_px != t_px {
            log::info!("window {wid}: corner radius {t_px}px");
        }
        c.t_px = t_px;
        if self.trace && t_px > 0 {
            // Four tiles side by side: TL, TR, BL, BR.
            let n = (t_px * t_px * 4) as usize;
            let mut sheet = vec![0u8; n * 4];
            for i in 0..4usize {
                for y in 0..t_px as usize {
                    let src = &tile_px[i * n + y * t_px as usize * 4..][..t_px as usize * 4];
                    let dst = (y * t_px as usize * 4 + i * t_px as usize) * 4;
                    sheet[dst..dst + t_px as usize * 4].copy_from_slice(src);
                }
            }
            dump_bgra(&format!("/tmp/sweeflow-corners-{wid}.png"), t_px * 4, t_px, &sheet);
        }
        c.pending = (t_px > 0).then_some((t_px, tile_px));
    }

    /// Shows a lossless frame (premultiplied BGRA == wl_shm ARGB8888 in memory) with its alpha.
    #[allow(clippy::too_many_arguments)]
    fn present_raw(&mut self, wid: u32, pw: u16, ph: u16, ppp: u8, capture_us: u64, bgra: Vec<u8>, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let _ = self.net.send(NetCmd::Video(wid, W::new(t::FRAME_ACK).u64(capture_us).finish()));
        if !w.configured {
            w.pending_raw = Some((pw, ph, ppp, capture_us, bgra));
            return;
        }
        let (fw, fh) = (pw as u32, ph as u32);
        if self.trace {
            dump_bgra(&format!("/tmp/sweeflow-popup-{wid}.png"), fw, fh, &bgra);
        }
        let Ok(mut pool) = RawPool::new(bgra.len(), &self.shm) else { return };
        pool.mmap().copy_from_slice(&bgra);
        let buffer = pool.create_buffer(0, fw as i32, fh as i32, (fw * 4) as i32, wl_shm::Format::Argb8888, (), qh);

        let s = self.scale;
        let (bw, bh) = w.size;
        let px_per_pt = px_per_pt(ppp, fw, w.info.w, s);
        log::info!("popup {wid}: frame {fw}x{fh} ppp {ppp} window {bw}x{bh} info {}x{}", w.info.w, w.info.h);
        let vis_w = (fw as f64 / px_per_pt).min(bw as f64 / s);
        let vis_h = (fh as f64 / px_per_pt).min(bh as f64 / s);
        w.video_viewport.set_source(0.0, 0.0, vis_w * px_per_pt, vis_h * px_per_pt);
        w.video_viewport.set_destination((vis_w * s).round().max(1.0) as i32, (vis_h * s).round().max(1.0) as i32);
        w.video.attach(Some(&buffer), 0, 0);
        w.video.damage_buffer(0, 0, fw as i32, fh as i32);
        w.video.commit();
        // KWin uploads shm contents on commit; three generations is plenty of slack.
        w.raw_buffers.push_back((buffer, pool));
        while w.raw_buffers.len() > 3 {
            if let Some((b, _)) = w.raw_buffers.pop_front() {
                b.destroy();
            }
        }
    }

    fn apply_icon(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        let Some(mgr) = &self.icon_mgr else { return };
        let Some(w) = self.windows.get(&wid) else { return };
        let Role::Toplevel(t) = &w.role else { return };
        let bid = w.info.bundle_id.clone();
        if !self.icons.contains_key(&bid) {
            let Some(png) = self.icon_pngs.get(&bid) else { return };
            let Ok((wid_px, hgt_px, argb)) = decode_png_argb(png) else { return };
            let stride = wid_px * 4;
            let Ok(mut pool) = RawPool::new((stride * hgt_px) as usize, &self.shm) else { return };
            pool.mmap().copy_from_slice(&argb);
            let buf = pool.create_buffer(0, wid_px as i32, hgt_px as i32, stride as i32, wl_shm::Format::Argb8888, (), qh);
            let icon = mgr.create_icon(qh, ());
            icon.add_buffer(&buf, 1);
            self.icons.insert(bid.clone(), (icon, buf));
        }
        mgr.set_icon(t.xdg_toplevel(), Some(&self.icons[&bid].0));
    }

    /// Shows the Global Menu panel only while a Mac window has keyboard focus. Debounced, so moving
    /// focus between two Mac windows (leave + enter) doesn't flicker it.
    fn schedule_menu_panel(&self) {
        let _ = self.loop_handle.insert_source(Timer::from_duration(Duration::from_millis(150)), |_, _, app| {
            let want = app.kb_focus.is_some();
            if app.menu_panel_shown != Some(want) {
                app.menu_panel_shown = Some(want);
                set_menu_panel_visible(want);
            }
            TimeoutAction::Drop
        });
    }

    /// Tells the Mac how big our largest monitor is, so its virtual display can match.
    fn send_screen_size(&self) {
        if !self.connected {
            return;
        }
        let best = self
            .output_state
            .outputs()
            .filter_map(|o| self.output_state.info(&o)?.logical_size)
            .max_by_key(|(w, h)| w * h);
        if let Some((w, h)) = best {
            // Mac points == our logical pixels.
            let (w, h) = ((w as f64 / self.scale) as u16, (h as f64 / self.scale) as u16);
            self.send(W::new(t::SCREEN_SIZE).u16(w).u16(h).finish());
        }
    }

    fn toggle_maximize(&self, wid: u32) {
        let Some(w) = self.windows.get(&wid) else { return };
        let Role::Toplevel(t) = &w.role else { return };
        if w.maximized {
            t.unset_maximized();
        } else {
            t.set_maximized();
        }
    }

    /// Resize edge under (x, y) for windows without a KDE frame.
    fn edge_at(&self, wid: u32, x: f64, y: f64) -> Option<xdg_toplevel::ResizeEdge> {
        use xdg_toplevel::ResizeEdge as E;
        let w = self.windows.get(&wid)?;
        if !matches!(w.role, Role::Toplevel(_)) || w.maximized {
            return None;
        }
        const EDGE: f64 = 5.0;
        const CORNER: f64 = 14.0;
        let (bw, bh) = (w.size.0 as f64, w.size.1 as f64);
        let (l, r, t, b) = (x < EDGE, x > bw - EDGE, y < EDGE, y > bh - EDGE);
        let (lc, rc, tc, bc) = (x < CORNER, x > bw - CORNER, y < CORNER, y > bh - CORNER);
        Some(match () {
            _ if (t && lc) || (l && tc) => E::TopLeft,
            _ if (t && rc) || (r && tc) => E::TopRight,
            _ if (b && lc) || (l && bc) => E::BottomLeft,
            _ if (b && rc) || (r && bc) => E::BottomRight,
            _ if t => E::Top,
            _ if b => E::Bottom,
            _ if l => E::Left,
            _ if r => E::Right,
            _ => return None,
        })
    }

    /// Redraws a window's traffic lights if their state changed.
    fn update_lights(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        let active = self.kb_focus == Some(wid);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if !matches!(w.role, Role::Toplevel(_)) {
            return;
        }
        let Some(rect) = lights::group_rect(&w.buttons) else {
            if let Some(l) = w.lights.take() {
                l.viewport.destroy();
                l.sub.destroy();
                l.surface.destroy();
            }
            return;
        };
        let state = lights::State {
            active,
            hover: w.lights_hover,
            pressed: w.lights_pressed.and_then(|(i, inside)| inside.then_some(i)),
            dark_titlebar: w.dark_titlebar,
        };
        if w.lights.as_ref().and_then(|l| l.drawn) == Some((state, rect)) {
            return;
        }
        if w.lights.is_none() {
            let (sub, surface) = self.subcompositor.create_subsurface(w.base().clone(), qh);
            sub.set_desync();
            sub.place_above(&w.video);
            let viewport = self.viewporter.get().expect("wp_viewporter").get_viewport(&surface, qh, ());
            // Input goes to the video surface below; this layer is purely visual.
            let region = self.compositor.wl_compositor().create_region(qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
            w.lights = Some(Lights { surface, sub, viewport, buffers: Default::default(), drawn: None });
        }
        // Sprites are 2x; the compositor downsamples them like the video.
        let (pw, ph, px) = lights::render(&w.buttons, rect, state);
        let Ok(mut pool) = RawPool::new(px.len(), &self.shm) else { return };
        pool.mmap().copy_from_slice(&px);
        let buf = pool.create_buffer(0, pw as i32, ph as i32, (pw * 4) as i32, wl_shm::Format::Argb8888, (), qh);
        let l = w.lights.as_mut().unwrap();
        let s = self.scale;
        l.sub.set_position((rect[0] * s).round() as i32, (rect[1] * s).round() as i32);
        l.viewport.set_destination((rect[2] * s).round() as i32, (rect[3] * s).round() as i32);
        l.surface.attach(Some(&buf), 0, 0);
        l.surface.damage_buffer(0, 0, pw as i32, ph as i32);
        if self.trace {
            if let (Some(p), Some(ev)) = (&self.presentation, self.trace_event_ms.take()) {
                p.feedback(&l.surface, qh, (ev, monotonic_ms()));
            }
        }
        l.surface.commit();
        l.drawn = Some((state, rect));
        l.buffers.push_back((buf, pool));
        while l.buffers.len() > 3 {
            if let Some((b, _)) = l.buffers.pop_front() {
                b.destroy();
            }
        }
        // Subsurface position changes apply on the parent's next commit.
        w.base().commit();
    }

    /// Tracks the pointer over the traffic lights (hover shows the glyphs).
    fn lights_pointer(&mut self, wid: u32, x: f64, y: f64, qh: &QueueHandle<Self>) {
        self.lights_pointer_at(wid, x, y, None, qh)
    }

    /// `event_ms`: the compositor's timestamp of the pointer event (for latency tracing).
    fn lights_pointer_at(&mut self, wid: u32, x: f64, y: f64, event_ms: Option<u32>, qh: &QueueHandle<Self>) {
        self.trace_event_ms = event_ms;
        let over = self.button_at(wid, x, y);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let in_group = lights::group_rect(&w.buttons)
            .is_some_and(|r| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]);
        w.lights_hover = in_group || w.lights_pressed.is_some();
        if let Some((i, _)) = w.lights_pressed {
            w.lights_pressed = Some((i, over == Some(i)));
        }
        self.update_lights(wid, qh);
    }

    /// Which traffic light (0 close, 1 minimise, 2 zoom) is under (x, y), if any.
    fn button_at(&self, wid: u32, x: f64, y: f64) -> Option<usize> {
        let w = self.windows.get(&wid)?;
        w.buttons.iter().position(|r| {
            r[2] > 0.0 && x >= r[0] as f64 && x < (r[0] + r[2]) as f64 && y >= r[1] as f64 && y < (r[1] + r[3]) as f64
        })
    }

    fn set_edge_cursor(&mut self, edge: Option<xdg_toplevel::ResizeEdge>, pointer: &wl_pointer::WlPointer, qh: &QueueHandle<Self>) {
        use wp_cursor_shape_device_v1::Shape;
        use xdg_toplevel::ResizeEdge as E;
        match edge {
            Some(e) => {
                let Some(mgr) = &self.cursor_mgr else { return };
                let shape = match e {
                    E::Top => Shape::NResize,
                    E::Bottom => Shape::SResize,
                    E::Left => Shape::WResize,
                    E::Right => Shape::EResize,
                    E::TopLeft => Shape::NwResize,
                    E::TopRight => Shape::NeResize,
                    E::BottomLeft => Shape::SwResize,
                    _ => Shape::SeResize,
                };
                let d = self.cursor_device.get_or_insert_with(|| mgr.get_pointer(pointer, qh, ()));
                d.set_shape(self.enter_serial, shape);
                self.edge_cursor = true;
            }
            None if self.edge_cursor => {
                self.edge_cursor = false;
                self.apply_cursor();
            }
            None => {}
        }
    }

    fn set_mac_cursor(&mut self, hot_x: f32, hot_y: f32, scale: f32, png: &[u8], qh: &QueueHandle<Self>) {
        let Ok((w, h, argb)) = decode_png_argb(png) else { return };
        let Ok(mut pool) = RawPool::new((w * h * 4) as usize, &self.shm) else { return };
        pool.mmap().copy_from_slice(&argb);
        let buf = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888, (), qh);
        let surface = self.cursor_surface.get_or_insert_with(|| self.compositor.create_surface(qh)).clone();
        let buffer_scale = scale.round().max(1.0) as i32;
        surface.set_buffer_scale(buffer_scale);
        surface.attach(Some(&buf), 0, 0);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        if let Some((old, _)) = self.cursor_buffer.replace((buf, pool)) {
            old.destroy();
        }
        // Hot spot arrives in points; cursor surface coordinates are logical pixels.
        self.cursor_hot = ((hot_x as f64 * self.scale).round() as i32, (hot_y as f64 * self.scale).round() as i32);
        self.apply_cursor();
    }

    fn apply_cursor(&self) {
        if self.pointer_focus.is_none() || self.edge_cursor {
            return;
        }
        let (Some(pointer), Some(surface)) = (&self.pointer, &self.cursor_surface) else { return };
        pointer.set_cursor(self.enter_serial, Some(surface), self.cursor_hot.0, self.cursor_hot.1);
    }

    fn wid_for(&self, s: &wl_surface::WlSurface) -> Option<u32> {
        self.surfaces.get(&s.id()).copied()
    }
}

/// Pixels per point of a frame: as reported by the Mac, else inferred from the window width.
fn px_per_pt(reported: u8, frame_w: u32, window_w_pt: f32, fallback: f64) -> f64 {
    if reported > 0 {
        reported as f64
    } else if window_w_pt > 0.0 {
        (frame_w as f64 / window_w_pt as f64).round().max(1.0)
    } else {
        fallback
    }
}

const BTN_LEFT: u32 = 0x110;
const KEY_TAB: u32 = 15;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;

/// Shows or auto-hides the Plasma panel holding the Global Menu (found by its widget). "Dodge
/// windows" rather than "always visible" so it never reserves space: toggling it must not make
/// the tiling layout jump.
fn set_menu_panel_visible(visible: bool) {
    let mode = if visible { "dodgewindows" } else { "autohide" };
    let script = format!(
        "panels().forEach(function (p) {{ if (p.widgets().some(function (w) {{ return w.type == \"org.kde.plasma.appmenu\"; }})) p.hiding = \"{mode}\"; }});"
    );
    std::thread::spawn(move || {
        let _ = std::process::Command::new("qdbus6")
            .args(["org.kde.plasmashell", "/PlasmaShell", "org.kde.PlasmaShell.evaluateScript", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

/// Opens KWin's window switcher. Our windows inhibit compositor shortcuts (so Cmd+letter reaches
/// the Mac), which also blocks Meta+Tab; invoke it explicitly instead.
fn switch_windows(reverse: bool) {
    let name = if reverse { "Walk Through Windows (Reverse)" } else { "Walk Through Windows" };
    std::thread::spawn(move || {
        let _ = std::process::Command::new("qdbus6")
            .args(["org.kde.kglobalaccel", "/component/kwin", "invokeShortcut", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

/// Diagnostics: writes premultiplied BGRA as an RGBA PNG.
fn dump_bgra(path: &str, w: u32, h: u32, bgra: &[u8]) {
    let rgba: Vec<u8> = bgra.chunks(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    if let Ok(f) = std::fs::File::create(path) {
        let mut enc = png::Encoder::new(f, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut wr) = enc.write_header() {
            let _ = wr.write_image_data(&rgba);
        }
    }
}

fn monotonic_ms() -> f64 {
    let mut ts = [0i64; 2];
    // SAFETY: clock_gettime writes a timespec into the buffer.
    unsafe { clock_gettime(1 /* CLOCK_MONOTONIC */, ts.as_mut_ptr()) };
    ts[0] as f64 * 1000.0 + ts[1] as f64 / 1e6
}

unsafe extern "C" {
    fn clock_gettime(clk: i32, ts: *mut i64) -> i32;
}

fn display_title(app: &str, title: &str) -> String {
    if title.is_empty() || title == app {
        app.to_string()
    } else {
        format!("{title} — {app}")
    }
}

fn decode_png_argb(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf)?;
    let (w, h) = (info.width, info.height);
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for px in buf[..info.buffer_size()].chunks_exact(info.color_type.samples()) {
        let (r, g, b, a) = match px.len() {
            4 => (px[0], px[1], px[2], px[3]),
            3 => (px[0], px[1], px[2], 255),
            2 => (px[0], px[0], px[0], px[1]),
            _ => (px[0], px[0], px[0], 255),
        };
        // wl_shm ARGB8888 is premultiplied, little-endian BGRA in memory.
        let pm = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
        out.extend_from_slice(&[pm(b), pm(g), pm(r), a]);
    }
    Ok((w, h, out))
}

// --- window management -----------------------------------------------------------------

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        if let Some(wid) = self.wid_for(window.wl_surface()) {
            self.send(W::new(t::CLOSE).u32(wid).finish());
        }
    }

    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, window: &Window, cfg: WindowConfigure, _: u32) {
        let Some(wid) = self.wid_for(window.wl_surface()) else { return };
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let was_suspended = w.suspended;
        w.suspended = cfg.state.contains(WindowState::SUSPENDED);
        w.floating = !(cfg.is_maximized() || cfg.is_fullscreen() || cfg.is_tiled());
        w.maximized = cfg.is_maximized() || cfg.is_fullscreen();
        let size = match cfg.new_size {
            (Some(cw), Some(ch)) => (cw.get(), ch.get()),
            _ => w.size,
        };
        let first = !w.configured;
        w.configured = true;
        if size != w.size || (first && size != (w.info.w as u32, w.info.h as u32)) {
            w.size = size;
            w.requested = Some((size, Instant::now()));
            let _ = self.net.send(NetCmd::Control(W::new(t::RESIZE).u32(wid).f32(size.0 as f32).f32(size.1 as f32).finish()));
        }
        if cfg.decoration_mode != DecorationMode::Server && first {
            log::debug!("compositor declined server-side decorations");
        }
        if was_suspended && !w.suspended && w.mac_minimized {
            // Restored from the taskbar: restore it on the Mac too.
            w.mac_minimized = false;
            let _ = self.net.send(NetCmd::Control(W::new(t::RESTORE).u32(wid).finish()));
        }
        if was_suspended != w.suspended {
            // Minimised (or fully hidden): stop encoding on the Mac to save bandwidth.
            let _ = self.net.send(NetCmd::Video(wid, W::new(t::PAUSE).u8(w.suspended as u8).finish()));
        }
        let pending = w.pending.take();
        let pending_raw = w.pending_raw.take();
        self.draw_base(wid, qh);
        if let Some(f) = pending {
            self.present(wid, f, qh);
        }
        if let Some((pw, ph, ppp, us, px)) = pending_raw {
            self.present_raw(wid, pw, ph, ppp, us, px, qh);
        }
    }
}

impl PopupHandler for App {
    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, popup: &Popup, cfg: PopupConfigure) {
        let Some(wid) = self.wid_for(popup.wl_surface()) else { return };
        let Some(w) = self.windows.get_mut(&wid) else { return };
        w.configured = true;
        w.size = (cfg.width.max(1) as u32, cfg.height.max(1) as u32);
        let pending = w.pending.take();
        let pending_raw = w.pending_raw.take();
        self.draw_base(wid, qh);
        if let Some(f) = pending {
            self.present(wid, f, qh);
        }
        if let Some((pw, ph, ppp, us, px)) = pending_raw {
            self.present_raw(wid, pw, ph, ppp, us, px, qh);
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        // The compositor dismissed it; the Mac decides when menus really close.
        let _ = popup;
    }
}

// --- input -----------------------------------------------------------------------------

fn mac_button(linux: u32) -> u8 {
    match linux {
        0x110 => 0, // BTN_LEFT
        0x111 => 1, // BTN_RIGHT
        0x112 => 2, // BTN_MIDDLE
        b => (b.saturating_sub(0x110)).min(31) as u8,
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, pointer: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            let Some(wid) = self.wid_for(&e.surface) else { continue };
            let (x, y) = (e.position.0 as f32, e.position.1 as f32);
            match e.kind {
                PointerEventKind::Enter { serial } => {
                    self.pointer_focus = Some(wid);
                    self.enter_serial = serial;
                    if self.cursor_surface.is_some() {
                        self.apply_cursor();
                    } else if let Some(mgr) = &self.cursor_mgr {
                        let d = self.cursor_device.get_or_insert_with(|| mgr.get_pointer(pointer, qh, ()));
                        d.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
                    }
                    self.lights_pointer(wid, e.position.0, e.position.1, qh);
                    self.send(W::new(t::POINTER_MOVE).u32(wid).f32(x).f32(y).finish());
                }
                PointerEventKind::Leave { .. } => {
                    if self.pointer_focus == Some(wid) {
                        self.pointer_focus = None;
                    }
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.lights_hover = false;
                    }
                    self.update_lights(wid, qh);
                }
                PointerEventKind::Motion { time } => {
                    let edge = self.edge_at(wid, e.position.0, e.position.1);
                    self.set_edge_cursor(edge, pointer, qh);
                    self.lights_pointer_at(wid, e.position.0, e.position.1, Some(time), qh);
                    if self.windows.get(&wid).is_some_and(|w| w.lights_pressed.is_some()) {
                        continue; // dragging off a traffic light: not for the Mac
                    }
                    self.send(W::new(t::POINTER_MOVE).u32(wid).f32(x).f32(y).finish());
                }
                PointerEventKind::Press { button, serial, .. } => {
                    if button == BTN_LEFT {
                        if let Some(edge) = self.edge_at(wid, e.position.0, e.position.1) {
                            if let (Some(w), Some(seat)) = (self.windows.get(&wid), &self.seat) {
                                if let Role::Toplevel(t) = &w.role {
                                    t.resize(seat, serial, edge);
                                }
                            }
                            self.local_press = Some(button);
                            continue;
                        }
                        if let Some(b) = self.button_at(wid, e.position.0, e.position.1) {
                            // Like macOS, the action fires on release, and only if the pointer
                            // is still on the button.
                            if let Some(w) = self.windows.get_mut(&wid) {
                                w.lights_pressed = Some((b, true));
                            }
                            self.update_lights(wid, qh);
                            self.local_press = Some(button);
                            continue;
                        }
                    }
                    self.press_serial = serial;
                    self.send(W::new(t::POINTER_BUTTON).u32(wid).f32(x).f32(y).u8(mac_button(button)).u8(1).finish());
                }
                PointerEventKind::Release { button, .. } => {
                    if self.local_press == Some(button) {
                        self.local_press = None;
                        let pressed = self.windows.get_mut(&wid).and_then(|w| w.lights_pressed.take());
                        if let Some((b, true)) = pressed {
                            // Traffic lights act on the Linux window; on the Mac they would
                            // minimise/fullscreen the window out of the stream.
                            match b {
                                0 => self.send(W::new(t::CLOSE).u32(wid).finish()),
                                1 => {
                                    if let Some(Role::Toplevel(t)) = self.windows.get(&wid).map(|w| &w.role) {
                                        t.set_minimized();
                                    }
                                }
                                _ => self.toggle_maximize(wid),
                            }
                        }
                        self.lights_pointer(wid, e.position.0, e.position.1, qh);
                        continue;
                    }
                    self.send(W::new(t::POINTER_BUTTON).u32(wid).f32(x).f32(y).u8(mac_button(button)).u8(0).finish());
                }
                PointerEventKind::Axis { horizontal, vertical, source, .. } => {
                    let continuous = matches!(
                        source,
                        Some(wl_pointer::AxisSource::Finger) | Some(wl_pointer::AxisSource::Continuous)
                    );
                    if continuous {
                        let stop = vertical.stop || horizontal.stop;
                        let phase = if stop {
                            3
                        } else if !self.scroll_active {
                            1
                        } else {
                            2
                        };
                        self.scroll_active = !stop;
                        let (dx, dy) = (-horizontal.absolute as f32, -vertical.absolute as f32);
                        self.send(W::new(t::SCROLL).u32(wid).f32(x).f32(y).f32(dx).f32(dy).u8(1).u8(phase).finish());
                    } else {
                        // Wheel: whole lines, accumulating high-resolution steps.
                        let v120 = |a: &smithay_client_toolkit::seat::pointer::AxisScroll| {
                            if a.value120 != 0 {
                                a.value120 as f64 / 120.0
                            } else if a.discrete != 0 {
                                a.discrete as f64
                            } else {
                                a.absolute / 15.0
                            }
                        };
                        self.wheel_accum.0 += v120(&horizontal);
                        self.wheel_accum.1 += v120(&vertical);
                        let lx = self.wheel_accum.0.trunc();
                        let ly = self.wheel_accum.1.trunc();
                        if lx != 0.0 || ly != 0.0 {
                            self.wheel_accum.0 -= lx;
                            self.wheel_accum.1 -= ly;
                            self.send(
                                W::new(t::SCROLL).u32(wid).f32(x).f32(y).f32(-lx as f32).f32(-ly as f32).u8(0).u8(0).finish(),
                            );
                        }
                    }
                }
            }
        }
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(wid) = self.wid_for(surface) {
            self.kb_focus = Some(wid);
            self.send(W::new(t::FOCUS).u32(wid).finish());
            self.update_lights(wid, qh);
        }
        self.schedule_menu_panel();
    }

    fn leave(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {
        let was = self.kb_focus.take();
        if let Some(wid) = was {
            self.update_lights(wid, qh);
        }
        self.schedule_menu_panel();
        self.meta_down = 0;
        self.shift_down = 0;
        self.swallowed.clear();
        self.send(W::new(t::RELEASE_ALL).finish());
    }

    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, e: KeyEvent) {
        match e.raw_code {
            KEY_LEFTMETA | KEY_RIGHTMETA => self.meta_down += 1,
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.shift_down += 1,
            _ => {}
        }
        if e.raw_code == KEY_TAB && self.meta_down > 0 {
            // Cmd+Tab switches between *all* desktop windows, like it does everywhere else.
            switch_windows(self.shift_down > 0);
            self.swallowed.push(e.raw_code);
            return;
        }
        self.send(W::new(t::KEY).u32(e.raw_code).u8(1).finish());
    }

    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, e: KeyEvent) {
        if self.swallowed.contains(&e.raw_code) {
            if e.raw_code == KEY_TAB {
                switch_windows(self.shift_down > 0);
            }
            return;
        }
        self.send(W::new(t::KEY).u32(e.raw_code).u8(2).finish());
    }

    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, e: KeyEvent) {
        match e.raw_code {
            KEY_LEFTMETA | KEY_RIGHTMETA => self.meta_down = self.meta_down.saturating_sub(1),
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.shift_down = self.shift_down.saturating_sub(1),
            _ => {}
        }
        if let Some(i) = self.swallowed.iter().position(|&k| k == e.raw_code) {
            self.swallowed.remove(i);
            return;
        }
        self.send(W::new(t::KEY).u32(e.raw_code).u8(0).finish());
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.seat.get_or_insert(seat);
    }
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        self.seat.get_or_insert(seat.clone());
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self
                .seat_state
                .get_keyboard_with_repeat(qh, &seat, None, self.loop_handle.clone(), Box::new(|_, _, _| {}))
                .ok();
        }
        if cap == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
            if let Some(d) = self.cursor_device.take() {
                d.destroy();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

// --- buffers ---------------------------------------------------------------------------

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }
    fn dmabuf_feedback(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, _: DmabufFeedback) {}
    fn created(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, _: wl_buffer::WlBuffer) {}
    fn failed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1) {
        log::error!("compositor rejected a dmabuf");
    }
    fn released(&mut self, _: &Connection, _: &QueueHandle<Self>, buffer: &wl_buffer::WlBuffer) {
        self.in_use.remove(&buffer.id());
        buffer.destroy();
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.send_screen_size();
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.send_screen_size();
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

impl AsMut<SimpleGlobal<WpViewporter, 1>> for App {
    fn as_mut(&mut self) -> &mut SimpleGlobal<WpViewporter, 1> {
        &mut self.viewporter
    }
}
impl AsMut<SimpleGlobal<WpSinglePixelBufferManagerV1, 1>> for App {
    fn as_mut(&mut self) -> &mut SimpleGlobal<WpSinglePixelBufferManagerV1, 1> {
        &mut self.single_pixel
    }
}

delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);

// Protocol objects with no (interesting) events.
wayland_client::delegate_noop!(App: ignore WpViewporter);
wayland_client::delegate_noop!(App: ignore WpViewport);
wayland_client::delegate_noop!(App: ignore WpSinglePixelBufferManagerV1);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(App: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(App: ignore WpColorRepresentationManagerV1);
wayland_client::delegate_noop!(App: ignore WpColorRepresentationSurfaceV1);
wayland_client::delegate_noop!(App: ignore ZwpKeyboardShortcutsInhibitManagerV1);
wayland_client::delegate_noop!(App: ignore XdgToplevelIconManagerV1);
wayland_client::delegate_noop!(App: ignore XdgToplevelIconV1);
wayland_client::delegate_noop!(App: ignore WpCursorShapeManagerV1);
wayland_client::delegate_noop!(App: ignore WpCursorShapeDeviceV1);

wayland_client::delegate_noop!(App: ignore WpPresentation);
wayland_client::delegate_noop!(App: ignore OrgKdeKwinAppmenuManager);
wayland_client::delegate_noop!(App: ignore OrgKdeKwinAppmenu);

/// Logs how long a traffic-light hover update took: input event → our commit → on screen.
impl Dispatch<WpPresentationFeedback, (u32, f64)> for App {
    fn event(_: &mut Self, _: &WpPresentationFeedback, e: wp_presentation_feedback::Event, d: &(u32, f64), _: &Connection, _: &QueueHandle<Self>) {
        if let wp_presentation_feedback::Event::Presented { tv_sec_hi, tv_sec_lo, tv_nsec, .. } = e {
            let shown = (((tv_sec_hi as u64) << 32 | tv_sec_lo as u64) as f64) * 1000.0 + tv_nsec as f64 / 1e6;
            let (event_ms, committed) = *d;
            // Event timestamps are 32-bit milliseconds of the same monotonic clock.
            let ev = (shown as u64 & !0xffff_ffff) as f64 + event_ms as f64;
            log::info!("hover latency: event→commit {:.1} ms, commit→screen {:.1} ms, total {:.1} ms",
                       committed - ev, shown - committed, shown - ev);
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for App {
    fn event(_: &mut Self, _: &ZwpKeyboardShortcutsInhibitorV1, e: zwp_keyboard_shortcuts_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        log::debug!("shortcut inhibitor: {e:?}");
    }
}
