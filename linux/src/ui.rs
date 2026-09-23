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
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_subsurface, wl_surface},
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
    },
    xdg::{
        shell::client::xdg_positioner,
        toplevel_icon::v1::client::{
            xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
            xdg_toplevel_icon_v1::XdgToplevelIconV1,
        },
    },
};

use crate::{
    decode::GpuFrame,
    net::{NetCmd, UiEvent, WinInfo},
    proto::{t, W},
};

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
    _color: Option<WpColorRepresentationSurfaceV1>,
    _inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    configured: bool,
    /// Logical size of the window as configured by the compositor.
    size: (u32, u32),
    /// Last size we asked the Mac for, and when (to ignore stale Mac-side sizes mid-resize).
    requested: Option<((u32, u32), Instant)>,
    floating: bool,
    suspended: bool,
    /// Frame waiting for the first configure.
    pending: Option<GpuFrame>,
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
    loop_handle: LoopHandle<'static, App>,

    net: UnboundedSender<NetCmd>,
    windows: HashMap<u32, Win>,
    surfaces: HashMap<wayland_client::backend::ObjectId, u32>,
    /// wl_buffer → the GPU frame it shows; dropped on release.
    in_use: HashMap<wayland_client::backend::ObjectId, GpuFrame>,
    icons: HashMap<String, (XdgToplevelIconV1, wl_buffer::WlBuffer)>,
    icon_pngs: HashMap<String, Vec<u8>>,
    background: Option<wl_buffer::WlBuffer>,

    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    pointer_focus: Option<u32>,
    kb_focus: Option<u32>,
    scroll_active: bool,
    wheel_accum: (f64, f64),
    scale: f64,
    host: String,
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
        seat: None,
        keyboard: None,
        pointer: None,
        cursor_device: None,
        pointer_focus: None,
        kb_focus: None,
        scroll_active: false,
        wheel_accum: (0.0, 0.0),
        scale,
        host,
    };

    // A 1x1 opaque buffer stretched behind the video; shows while a resize is catching up.
    let bg = app.single_pixel.get()?.create_u32_rgba_buffer(0x1e1e_1e1e, 0x1e1e_1e1e, 0x1e1e_1e1e, u32::MAX, &qh, ());
    app.background = Some(bg);

    let qh2 = qh.clone();
    event_loop
        .handle()
        .insert_source(ui_rx, move |ev, _, app: &mut App| {
            if let channel::Event::Msg(ev) = ev {
                app.on_net(ev, &qh2);
            }
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    log::info!("sweeflow client started, connecting to {}", app.host);
    loop {
        event_loop.dispatch(Duration::from_millis(100), &mut app)?;
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
            UiEvent::Disconnected(why) => log::warn!("disconnected: {why}"),
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
            UiEvent::Cursor { .. } => {}
            UiEvent::Focused(_) => {}
            UiEvent::Frame(wid, frame) => self.present(wid, frame, qh),
        }
    }

    fn add_window(&mut self, info: WinInfo, qh: &QueueHandle<Self>) {
        let wid = info.wid;
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
            let window = self.xdg.create_window(surface, WindowDecorations::RequestServer, qh);
            window.set_title(display_title(&info.app_name, &info.title));
            window.set_app_id(format!("sweeflow-{}", info.bundle_id));
            window.set_min_size(Some((100, 60)));
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
        let color = self.color_rep.as_ref().map(|m| {
            let c = m.get_surface(&video, qh, ());
            // VideoToolbox encodes BT.709 limited range.
            c.set_coefficients_and_range(crs::Coefficients::Bt709, crs::Range::Limited);
            c
        });
        let inhibitor = match (&role, &self.inhibit_mgr, &self.seat) {
            (Role::Toplevel(_), Some(m), Some(seat)) => Some(m.inhibit_shortcuts(&base, seat, qh, ())),
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
                _color: color,
                _inhibitor: inhibitor,
                configured: false,
                size,
                requested: None,
                floating: true,
                suspended: false,
                pending: None,
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
        base.attach(self.background.as_ref(), 0, 0);
        w.base_viewport.set_destination(bw as i32, bh as i32);
        base.damage_buffer(0, 0, i32::MAX, i32::MAX);
        base.commit();
    }

    fn present(&mut self, wid: u32, frame: GpuFrame, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
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
        let px_per_pt = if w.info.w > 0.0 { (frame.width as f64 / w.info.w as f64).round().max(1.0) } else { s };
        let vis_w = (frame.width as f64 / px_per_pt).min(bw as f64 / s);
        let vis_h = (frame.height as f64 / px_per_pt).min(bh as f64 / s);
        w.video_viewport.set_source(0.0, 0.0, vis_w * px_per_pt, vis_h * px_per_pt);
        w.video_viewport.set_destination((vis_w * s).round().max(1.0) as i32, (vis_h * s).round().max(1.0) as i32);
        w.video.attach(Some(&buffer), 0, 0);
        w.video.damage_buffer(0, 0, frame.width as i32, frame.height as i32);
        w.video.commit();
        self.in_use.insert(buffer.id(), frame);
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

    fn wid_for(&self, s: &wl_surface::WlSurface) -> Option<u32> {
        self.surfaces.get(&s.id()).copied()
    }
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
        if was_suspended != w.suspended {
            // Minimised (or fully hidden): stop encoding on the Mac to save bandwidth.
            let _ = self.net.send(NetCmd::Video(wid, W::new(t::PAUSE).u8(w.suspended as u8).finish()));
        }
        let pending = w.pending.take();
        self.draw_base(wid, qh);
        if let Some(f) = pending {
            self.present(wid, f, qh);
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
        self.draw_base(wid, qh);
        if let Some(f) = pending {
            self.present(wid, f, qh);
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
                    if let Some(mgr) = &self.cursor_mgr {
                        if self.cursor_device.is_none() {
                            self.cursor_device = Some(mgr.get_pointer(pointer, qh, ()));
                        }
                        if let Some(d) = &self.cursor_device {
                            d.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
                        }
                    }
                    self.send(W::new(t::POINTER_MOVE).u32(wid).f32(x).f32(y).finish());
                }
                PointerEventKind::Leave { .. } => {
                    if self.pointer_focus == Some(wid) {
                        self.pointer_focus = None;
                    }
                }
                PointerEventKind::Motion { .. } => {
                    self.send(W::new(t::POINTER_MOVE).u32(wid).f32(x).f32(y).finish());
                }
                PointerEventKind::Press { button, .. } | PointerEventKind::Release { button, .. } => {
                    let pressed = matches!(e.kind, PointerEventKind::Press { .. });
                    self.send(
                        W::new(t::POINTER_BUTTON).u32(wid).f32(x).f32(y).u8(mac_button(button)).u8(pressed as u8).finish(),
                    );
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
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(wid) = self.wid_for(surface) {
            self.kb_focus = Some(wid);
            self.send(W::new(t::FOCUS).u32(wid).finish());
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {
        self.kb_focus = None;
        self.send(W::new(t::RELEASE_ALL).finish());
    }

    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, e: KeyEvent) {
        self.send(W::new(t::KEY).u32(e.raw_code).u8(1).finish());
    }

    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, e: KeyEvent) {
        self.send(W::new(t::KEY).u32(e.raw_code).u8(2).finish());
    }

    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, e: KeyEvent) {
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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
wayland_client::delegate_noop!(App: ignore WpColorRepresentationManagerV1);
wayland_client::delegate_noop!(App: ignore WpColorRepresentationSurfaceV1);
wayland_client::delegate_noop!(App: ignore ZwpKeyboardShortcutsInhibitManagerV1);
wayland_client::delegate_noop!(App: ignore XdgToplevelIconManagerV1);
wayland_client::delegate_noop!(App: ignore XdgToplevelIconV1);
wayland_client::delegate_noop!(App: ignore WpCursorShapeManagerV1);
wayland_client::delegate_noop!(App: ignore WpCursorShapeDeviceV1);

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for App {
    fn event(_: &mut Self, _: &ZwpKeyboardShortcutsInhibitorV1, e: zwp_keyboard_shortcuts_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        log::debug!("shortcut inhibitor: {e:?}");
    }
}
