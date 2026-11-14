//! Wayland frontend: one native toplevel per Mac window (popups/menus become xdg_popups).
//!
//! Each window is a base surface filled with a single-pixel background, sized to whatever the
//! compositor (or tiling script) configures, plus a desynchronised subsurface that shows the
//! decoded video 1:1. Video frames are GPU dmabufs straight from the VA-API decoder.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    data_device_manager::{
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{CopyPasteSource, DataSourceHandler, DragSource},
        DataDeviceManagerState, WritePipe,
    },
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
    protocol::{wl_buffer, wl_data_device, wl_data_device_manager::DndAction, wl_data_source, wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_subsurface, wl_surface},
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

use wayland_protocols::xdg::dialog::v1::client::{xdg_dialog_v1::XdgDialogV1, xdg_wm_dialog_v1::XdgWmDialogV1};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
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
    /// The tiles in use (size, pixels): their alpha shapes a paused window's still too.
    last_tiles: Option<(u32, Vec<u8>)>,
    /// The tiles as the Mac sent them (rounded), and whether the ones in use were squared: the
    /// setting can change while the window is open.
    raw_tiles: Option<(u32, Vec<u8>)>,
    squared: bool,
    /// Whether the split layout was used for the last frame.
    split: bool,
    /// Which corner tiles (TL, TR, BL, BR) show: only the window's real corners get one (see
    /// lay_out), and whether the bottom strip does.
    tiles_shown: [bool; 4],
    bottom_shown: bool,
    /// Middle strip's offset from the window's top edge (logical px).
    middle_y: i32,
    /// Colour representation of the top and bottom strips (they show the video too).
    colors: Vec<WpColorRepresentationSurfaceV1>,
}

/// A paused window's still: its last picture in black and white with a pause sign, on a
/// subsurface over the video (the video's own surfaces are YUV with colour settings to match).
struct Still {
    sub: wl_subsurface::WlSubsurface,
    surface: wl_surface::WlSurface,
    viewport: WpViewport,
    buffer: wl_buffer::WlBuffer,
    _pool: RawPool,
}

struct Lights {
    surface: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    viewport: WpViewport,
    buffers: std::collections::VecDeque<(wl_buffer::WlBuffer, RawPool)>,
    /// What is drawn: the state, where, and the video's offset it was placed against.
    drawn: Option<(lights::State, [f64; 4], i32)>,
}

/// Work finished on a helper thread that must continue on the Wayland thread.
pub enum Internal {
    /// The URI list of a drop was read: finish the offer.
    DropRead,
    /// Told to stop (SIGTERM: a restart, Quit): the windows go first, then the process.
    Shutdown,
}

enum Role {
    Toplevel(Window),
    Popup(Popup),
}

/// The screenshot selection being dragged in a window: a veil and a 1 px edge above its video.
/// macOS draws its own in windows that can't be captured, so it's drawn here instead.
struct Selection {
    wid: u32,
    /// Where the drag started, window coordinates.
    start: (f64, f64),
    /// Veil, then top, bottom, left and right edges.
    parts: Vec<(wl_subsurface::WlSubsurface, wl_surface::WlSurface, WpViewport)>,
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
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    configured: bool,
    /// Logical size of the window as configured by the compositor.
    size: (u32, u32),
    /// Smallest size the Mac app turned out to accept (declared to the compositor, so tiling
    /// gives the window at least that instead of cutting it off).
    min_size: (u32, u32),
    /// Largest size the Mac app turned out to accept (it stayed smaller than asked: a video-call app's
    /// meeting window, a window wider than the Mac's display).
    max_size: Option<(u32, u32)>,
    /// The app won't let it be resized (WINDOW_FIXED): it keeps exactly the Mac's size here
    /// (declared as both its smallest and its largest), on every desktop.
    fixed: bool,
    /// Made a dialog so it floats (fixed, a floating panel, or one that keeps its shape).
    floated: bool,
    /// Its shape (width / height) when it first came: one that keeps it whatever it's asked is
    /// a video player (see adopt_mac_size).
    first_shape: Option<f64>,
    /// Where its app put it on the Mac's screen (fractions), for a fixed one: placed there again
    /// when its app changes its size (Calculator switching mode), as it was anchored to its corner.
    spot: Option<(f32, f32)>,
    /// A first refusal (size asked, size the Mac kept), not acted on yet: the Mac may just be
    /// lagging behind a drag. Only the same refusal twice in a row declares a limit.
    refusal: Option<((u32, u32), (u32, u32))>,
    /// The Mac reached the size last asked for (so a later difference is the app's own doing).
    followed: bool,
    /// Last size we asked the Mac for, and when (to ignore stale Mac-side sizes mid-resize).
    requested: Option<((u32, u32), Instant)>,
    floating: bool,
    maximized: bool,
    /// Hyprland has it tiled (it doesn't say so through the window's state; see UiEvent::Tiling):
    /// such a window isn't maximised from its own green button.
    tiled: bool,
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
    /// The buffer showing the latest video frame (its frame is in App::in_use).
    shown: Option<wayland_client::backend::ObjectId>,
    /// That frame's picture: the part of the frame that is the window (px), and its pixels per
    /// point as the Mac reported them. Kept to lay the picture out again when only the window's
    /// size here changed (relayout).
    picture: Option<((u32, u32), u8)>,
    /// Where the video's subsurface (the middle strip) is in the window.
    video_at: (i32, i32),
    /// Paused (or the Mac away): the still shown instead of the video.
    still: Option<Still>,
    /// Asked to close, and hidden since then (picture and lights unmapped, no input) while the
    /// Mac closes it: it goes for good with the Mac's WindowRemoved, and comes back if its app
    /// shows another window instead (a "Save changes?" sheet) or it's still there after a while
    /// (see hide / unhide). Frames meanwhile wait in `pending`.
    hidden: Option<Instant>,
    /// Lossless frame waiting for the first configure.
    pending_raw: Option<(u16, u16, u8, u64, Vec<u8>)>,
    /// Recent shm buffers of lossless frames (kept alive until the compositor is surely done).
    raw_buffers: std::collections::VecDeque<(wl_buffer::WlBuffer, RawPool)>,
    stale: bool,
    /// Popup geometry changes wait a moment (see adopt_mac_size).
    popup_wait: bool,
    popup_settled: bool,
    /// A "did the Mac follow?" look (adopt_mac_size) is scheduled.
    check_pending: bool,
    /// The "Mac display" view (kind 3): letterboxed and scaled to fit, never resized on the Mac.
    display: bool,
    /// The display view's picture: logical px per Mac point, and where it starts (logical px).
    /// (An app's window is drawn 1:1 from its corner.)
    fit: (f64, f64, f64),
    /// Hyprland gave the tile a new size the Mac hasn't been asked for yet, and when (arm_settle).
    settling: Option<Instant>,
}

impl Win {
    fn base(&self) -> &wl_surface::WlSurface {
        match &self.role {
            Role::Toplevel(w) => w.wl_surface(),
            Role::Popup(p) => p.wl_surface(),
        }
    }

    /// A Mac app's window in a Hyprland tile. Hyprland lays a whole workspace out at once, so a
    /// tile's size is asked of the Mac once it has stood for a moment (arm_settle). (Not a popup
    /// shown as a window: its lossless pictures aren't laid out by lay_out.)
    fn in_tile(&self) -> bool {
        self.tiled && !self.display && self.info.kind != 1 && matches!(self.role, Role::Toplevel(_)) && crate::hyprland::running()
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
    /// xdg-dialog-v1: marks a fixed-size Mac window as a dialog (floated, not tiled).
    dialog_mgr: Option<XdgWmDialogV1>,
    /// Windows said to be fixed-size before they were added (WINDOW_FIXED comes first).
    pending_fixed: HashMap<u32, (Option<(f32, f32)>, bool)>,
    icon_mgr: Option<XdgToplevelIconManagerV1>,
    cursor_mgr: Option<WpCursorShapeManagerV1>,
    /// For measuring hover latency (SWEEFLOW_TRACE=1): when traffic-light updates hit the screen.
    presentation: Option<WpPresentation>,
    appmenu_mgr: Option<OrgKdeKwinAppmenuManager>,
    /// xdg-activation: brings a window back to the front (and unminimises it) on a launch.
    activation: Option<XdgActivationV1>,
    /// The desktop's shortcuts let through (no inhibitor) until then, and while a modifier is
    /// still held: a window key was just handed to it (see press_key).
    let_go: Option<Instant>,
    /// Hyprland is in Sweeflow's key mode (hyprland::mac_keys).
    hypr_mac_keys: bool,
    /// Launches whose app had no window here yet: their token, for the window when it comes.
    launch_tokens: HashMap<String, (String, std::time::Instant)>,
    /// Plain sRGB, stated on every surface showing video (KWin would otherwise guess for
    /// 10-bit video, and guesses darker). Usable once the compositor says it's ready.
    srgb: Option<WpImageDescriptionV1>,
    srgb_ready: bool,
    color_mgr: Option<WpColorManagerV1>,
    /// Surfaces already described as sRGB.
    srgb_tagged: HashMap<wayland_client::backend::ObjectId, WpColorManagementSurfaceV1>,
    /// Clipboard access regardless of focus (KDE's ext-data-control).
    data_control: Option<ExtDataControlManagerV1>,
    clip_device: Option<ExtDataControlDeviceV1>,
    clip_source: Option<ExtDataControlSourceV1>,
    /// Selection events to ignore: the echoes of our own set_selection.
    clip_ignore: u32,
    conn: Connection,
    /// Drag and drop (the regular data device).
    ddm: Option<DataDeviceManagerState>,
    dnd_device: Option<DataDevice>,
    /// Linux drag carrying files out of a Mac window.
    drag_source: Option<DragSource>,
    /// The clipboard where there's no ext-data-control (GNOME): the ordinary Wayland clipboard,
    /// which works while a Mac window has the keyboard. What the Mac copied (MIME type → bytes),
    /// our source offering it, and the last keyboard-enter serial (a selection needs one).
    cp_offers: Option<HashMap<String, Arc<Vec<u8>>>>,
    cp_source: Option<CopyPasteSource>,
    kb_serial: u32,
    /// Drop targets waiting for the Mac files of that drag.
    drag_waiting: Vec<WritePipe>,
    drag_uris: Option<Arc<Vec<u8>>>,
    /// How many files the Mac drag carries (its picture shows the number), and the picture's
    /// surface while a drag is out (kept alive with its buffer).
    drag_count: usize,
    drag_icon: Option<(wl_surface::WlSurface, RawPool, wl_buffer::WlBuffer)>,
    /// A file drag that started in this Mac window (reported by the Mac).
    mac_drag: Option<u32>,
    /// Mac window where the left button is down (forwarded to the Mac).
    left_down: Option<u32>,
    pointer_at: (u32, f64, f64),
    /// Mac window under a Linux drag, and where.
    dnd_target: Option<(u32, f64, f64)>,
    internal: channel::Sender<Internal>,
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
    /// Apps looked at for a taskbar entry (see note_app).
    noted_apps: std::collections::HashSet<String>,
    background: Option<wl_buffer::WlBuffer>,
    /// Fully transparent 1x1 buffer: base for popups (menus have rounded, translucent edges).
    clear: Option<wl_buffer::WlBuffer>,
    /// macOS's screenshot selection (Cmd+Shift+4) is up: a left drag draws its rectangle here.
    screen_select: bool,
    /// The selection being dragged (see start_selection).
    selection: Option<Selection>,
    /// Fill and edge of the selection: 1x1 buffers, stretched.
    selection_bufs: Option<(wl_buffer::WlBuffer, wl_buffer::WlBuffer)>,

    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    pointer_focus: Option<u32>,
    kb_focus: Option<u32>,
    scroll_active: bool,
    /// Serial of the last pointer enter, needed to set the cursor image.
    enter_serial: u32,
    /// The Mac's current cursor, drawn locally so it moves with zero latency.
    cursor_surface: Option<wl_surface::WlSurface>,
    cursor_buffer: Option<(wl_buffer::WlBuffer, RawPool)>,
    cursor_hot: (i32, i32),
    meta_down: u8,
    shift_down: u8,
    /// The modifiers as the desktop counts them (for Cmd+Opt+U, which must have no others).
    mods: Modifiers,
    /// Keys we handled locally (e.g. Cmd+Tab) whose release must not reach the Mac either.
    swallowed: Vec<u32>,
    /// Serial of the last button press forwarded to the Mac (for handing drags to KWin).
    press_serial: u32,
    /// Numbers left presses sent to the Mac, so its MOVE_READY answer is matched to its press.
    press_id: u32,
    /// Frames presented, total and worst decoded→presented µs, since when (latency figures).
    present_stats: (u64, u64, u64, std::time::Instant),
    /// Where (surface coordinates) the held left press went down, and whether the Mac said it
    /// can move the window (MOVE_READY): then the first bit of motion starts KWin's move.
    press_at: (f64, f64),
    move_ready: bool,
    /// The Mac's latest word on what's under the pointer in a window's title-bar strip (DRAG_SPOT:
    /// window, point in window points, whether a press there drags the window, when it came).
    /// A press close to that point starts the window's move on its first motion, with no wait
    /// for the Mac: the window is taken hold of where it was pressed (see spot_drags).
    drag_spot: Option<(u32, f32, f32, bool, Instant)>,
    /// When the held left press went down, and whether it was known to drag the window as it
    /// went down (drag_spot) rather than by the Mac's answer to it (for the log in move_here).
    press_time: Instant,
    press_known: bool,
    /// The window Linux started moving itself (the Mac's own MOVE_STARTED for it is moot).
    moved_here: Option<u32>,
    /// A pointer button handled locally (traffic light, resize edge): swallow its release.
    local_press: Option<u32>,
    /// Showing a resize cursor over a window edge instead of the Mac cursor.
    edge_cursor: bool,
    scale: f64,
    host: String,
    connected: bool,
    /// Streaming is paused: windows stay, as stills, until it resumes.
    paused: bool,
    /// Windows closed here while the Mac was away: closed there too once it's back.
    closed_offline: HashSet<u32>,
    /// Popups (a save sheet, a menu) of a window that hasn't shown a picture yet, made once it
    /// has: Hyprland never drew a popup of a window still being set up (a sheet already up when
    /// the windows were announced never showed).
    popups_waiting: HashMap<u32, WinInfo>,
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
    let (int_tx, int_rx): (channel::Sender<Internal>, Channel<Internal>) = channel::channel();
    let scale = net_cfg.scale;
    let host = net_cfg.host.clone();
    let net = crate::net::spawn(net_cfg, ui_tx);
    crate::tray::set_net(net.clone());
    crate::pair::watch_links();
    crate::hyprland::watch_tiling();
    // SIGTERM / SIGINT: the windows are taken down in order first (Internal::Shutdown).
    {
        let tx = int_tx.clone();
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async {
                use tokio::signal::unix::{signal, SignalKind};
                let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
                tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
                let _ = tx.send(Internal::Shutdown);
                // Not answered in time (the loop stuck): go anyway.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                std::process::exit(0);
            });
        });
    }

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
        dialog_mgr: bind_opt("xdg_wm_dialog_v1").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        pending_fixed: Default::default(),
        icon_mgr: bind_opt("xdg_toplevel_icon_manager_v1").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        cursor_mgr: bind_opt("wp_cursor_shape_manager_v1").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        presentation: bind_opt("wp_presentation").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        appmenu_mgr: bind_opt("org_kde_kwin_appmenu_manager").then(|| globals.bind(&qh, 1..=2, ()).ok()).flatten(),
        activation: bind_opt("xdg_activation_v1").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        let_go: None,
        // A key mode left on by a Sweeflow that went away (a crash) is put back.
        hypr_mac_keys: {
            if crate::hyprland::running() {
                crate::hyprland::mac_keys(false);
            }
            false
        },
        launch_tokens: HashMap::new(),
        srgb: None,
        srgb_ready: false,
        color_mgr: bind_opt("wp_color_manager_v1").then(|| globals.bind(&qh, 1..=2, ()).ok()).flatten(),
        srgb_tagged: HashMap::new(),
        data_control: bind_opt("ext_data_control_manager_v1").then(|| globals.bind(&qh, 1..=1, ()).ok()).flatten(),
        clip_device: None,
        clip_source: None,
        clip_ignore: 0,
        conn: conn.clone(),
        ddm: DataDeviceManagerState::bind(&globals, &qh).ok(),
        dnd_device: None,
        drag_source: None,
        cp_offers: None,
        cp_source: None,
        kb_serial: 0,
        drag_waiting: Vec::new(),
        drag_uris: None,
        drag_count: 1,
        drag_icon: None,
        mac_drag: None,
        left_down: None,
        pointer_at: (0, 0.0, 0.0),
        dnd_target: None,
        internal: int_tx,
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
        noted_apps: std::collections::HashSet::new(),
        background: None,
        clear: None,
        screen_select: false,
        selection: None,
        selection_bufs: None,
        seat: None,
        keyboard: None,
        pointer: None,
        cursor_device: None,
        pointer_focus: None,
        kb_focus: None,
        scroll_active: false,
        enter_serial: 0,
        cursor_surface: None,
        cursor_buffer: None,
        cursor_hot: (0, 0),
        meta_down: 0,
        shift_down: 0,
        mods: Modifiers::default(),
        swallowed: Vec::new(),
        press_serial: 0,
        press_id: 0,
        present_stats: (0, 0, 0, std::time::Instant::now()),
        press_at: (0.0, 0.0),
        move_ready: false,
        drag_spot: None,
        press_time: Instant::now(),
        press_known: false,
        moved_here: None,
        local_press: None,
        edge_cursor: false,
        scale,
        host,
        connected: false,
        paused: false,
        closed_offline: HashSet::new(),
        popups_waiting: HashMap::new(),
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
    if let Some(cm) = &app.color_mgr {
        let params = cm.create_parametric_creator(&qh, ());
        params.set_primaries_named(wp_color_manager_v1::Primaries::Srgb);
        params.set_tf_named(wp_color_manager_v1::TransferFunction::Gamma22);
        app.srgb = Some(params.create(&qh, ()));
    }
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

    event_loop
        .handle()
        .insert_source(int_rx, |ev, _, app: &mut App| match ev {
            channel::Event::Msg(Internal::DropRead) => {
                if let Some(offer) = app.dnd_device.as_ref().and_then(|d| d.data().drag_offer()) {
                    offer.finish();
                    offer.destroy();
                }
            }
            channel::Event::Msg(Internal::Shutdown) => {
                // Every window taken down here, children before parents, before the connection
                // closes: left to the compositor, Hyprland 0.56 could crash tearing down a
                // subsurface whose parent was already gone (wl_client_destroy).
                let ids: Vec<u32> = app.windows.keys().copied().collect();
                for wid in ids {
                    app.remove_window(wid);
                }
                let _ = app.conn.flush();
                let _ = app.conn.roundtrip();
                log::info!("stopping: windows taken down");
                if app.hypr_mac_keys {
                    // Not left in Sweeflow's key mode, with the desktop's bindings off.
                    let _ = std::process::Command::new("hyprctl").args(["eval", r#"hl.dispatch(hl.dsp.submap("reset"))"#]).output();
                }
                std::process::exit(0);
            }
            _ => {}
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
                self.expire_stale_soon();
            }
            UiEvent::Streaming(on) => {
                self.paused = !on;
                log::info!("streaming {}", if on { "resumed" } else { "paused: windows stay as stills" });
                if on {
                    // The Mac announces its windows again; ones it doesn't (closed meanwhile) go.
                    for w in self.windows.values_mut() {
                        if w.still.is_some() {
                            w.stale = true;
                        }
                    }
                    self.expire_stale_soon();
                } else {
                    self.freeze_all(qh);
                }
            }
            UiEvent::Disconnected(why) => {
                self.connected = false;
                log::warn!("disconnected: {why}");
                self.screen_select = false;
                self.end_selection();
                // Windows wait for the Mac to come back; menus and popovers don't (nothing would
                // close them meanwhile, and they sit above everything else).
                // Stopped on purpose (Stop here, or the Mac's menu), or the Mac went to another
                // computer: nothing waits, paused stills go too.
                let on_purpose = crate::net::offline() || crate::tray::mac_refused() || crate::tray::elsewhere().is_some();
                let gone: Vec<u32> = self
                    .windows
                    .values()
                    .filter(|w| on_purpose || matches!(w.role, Role::Popup(_)))
                    .map(|w| w.info.wid)
                    .collect();
                for wid in gone {
                    self.remove_window(wid);
                }
                self.freeze_all(qh);
            }
            UiEvent::WindowAdded(info) => self.add_window(info, qh),
            UiEvent::WindowChanged { wid, title, w, h, rel_x, rel_y } => {
                if let Some(i) = self.popups_waiting.get_mut(&wid) {
                    (i.title, i.w, i.h, i.rel_x, i.rel_y) = (title.clone(), w, h, rel_x, rel_y);
                }
                if let Some(win) = self.windows.get_mut(&wid) {
                    if let Role::Toplevel(t) = &win.role {
                        if title != win.info.title {
                            t.set_title(display_title(&win.info.app_name, &title));
                        }
                    }
                    let resized = (w, h) != (win.info.w, win.info.h);
                    win.info.title = title;
                    win.info.w = w;
                    win.info.h = h;
                    win.info.rel_x = rel_x;
                    win.info.rel_y = rel_y;
                    self.clip_input(wid, qh);
                    if let Some(w) = self.windows.get(&wid) {
                        w.base().commit();
                    }
                    if resized {
                        // No more of the picture than the Mac window is now (lay_out).
                        self.relayout(wid, qh);
                    }
                    self.adopt_mac_size(wid, qh);
                }
            }
            // Paused: the Mac lets go of every window; here they stay, as stills.
            UiEvent::WindowRemoved(wid) if self.paused && self.windows.get(&wid).is_some_and(|w| matches!(w.role, Role::Toplevel(_))) => {
                self.freeze(wid, qh);
                let _ = self.net.send(NetCmd::Unsubscribe(wid));
            }
            UiEvent::WindowRemoved(wid) => {
                self.popups_waiting.remove(&wid);
                self.remove_window(wid)
            }
            UiEvent::WindowFixed(wid, spot, fixed) if !self.windows.contains_key(&wid) => {
                // Said before the window is added: it's made a dialog (of its size, if fixed)
                // from the start.
                self.pending_fixed.insert(wid, (spot, fixed));
            }
            UiEvent::WindowFixed(_, _, false) => {} // a floating panel already shown: as it is
            UiEvent::WindowFixed(wid, spot, true) => {
                if let Some(w) = self.windows.get_mut(&wid).filter(|w| !w.display && matches!(w.role, Role::Toplevel(_))) {
                    w.fixed = true;
                    w.spot = spot;
                    log::info!("window {wid}: its app won't let it be resized: kept at its own size");
                    Self::hold_mac_size(w);
                    if let Some(spot) = spot {
                        crate::desktop::place_at_spot(format!("sweeflow-{}", w.info.bundle_id), display_title(&w.info.app_name, &w.info.title), spot);
                    }
                    if crate::hyprland::running() {
                        // Hyprland floats a window whose smallest and largest size are the same
                        // only when it opens; this one was already tiled.
                        crate::hyprland::float_window(format!("sweeflow-{}", w.info.bundle_id), display_title(&w.info.app_name, &w.info.title));
                    }
                    self.draw_base(wid, qh);
                    self.relayout(wid, qh);
                }
            }
            UiEvent::AppIcon { bundle_id, png } => {
                self.icon_pngs.insert(bundle_id.clone(), png);
                let wids: Vec<u32> =
                    self.windows.values().filter(|w| w.info.bundle_id == bundle_id).map(|w| w.info.wid).collect();
                for wid in wids {
                    self.apply_icon(wid, qh);
                }
            }
            UiEvent::Cursor { hot_x, hot_y, scale, png } => self.set_mac_cursor(hot_x, hot_y, scale, &png, qh),
            UiEvent::Focused => {}
            UiEvent::CornersSetting => self.apply_corners_setting(qh),
            UiEvent::Tiling(seen) => {
                let mut changed = Vec::new();
                for (wid, w) in self.windows.iter_mut().filter(|(_, w)| matches!(w.role, Role::Toplevel(_))) {
                    let class = if w.display { "sweeflow-display".to_string() } else { format!("sweeflow-{}", w.info.bundle_id) };
                    let title = display_title(&w.info.app_name, &w.info.title);
                    // By class and title; windows of one app with the same title go together.
                    let same_class: Vec<_> = seen.iter().filter(|(c, _, _)| *c == class).collect();
                    let same_title: Vec<_> = same_class.iter().copied().filter(|(_, t, _)| *t == title).collect();
                    let of = if same_title.is_empty() { &same_class } else { &same_title };
                    if of.is_empty() {
                        continue; // not listed yet
                    }
                    let tiled = of.iter().any(|(_, _, floating)| !floating);
                    if w.tiled != tiled {
                        log::info!("window {wid}: {} on Hyprland", if tiled { "tiled" } else { "floating" });
                        w.tiled = tiled;
                        changed.push(*wid);
                    }
                }
                for wid in changed {
                    self.tiling_changed(wid, qh);
                }
            }
            UiEvent::WindowButtons(wid, rects) => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.buttons = rects;
                }
                // Its title bar changed: what the Mac saw under the pointer there may be stale.
                if self.drag_spot.is_some_and(|s| s.0 == wid) {
                    self.drag_spot = None;
                }
                self.update_lights(wid, qh);
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
            UiEvent::DragSpot(wid, x, y, drags) => {
                self.drag_spot = Some((wid, x, y, drags, Instant::now()));
            }
            UiEvent::MoveStarted(wid) if self.moved_here.take() == Some(wid) => {}
            UiEvent::MoveStarted(wid) => {
                // Only while the press that started it is still held here. Asked for after the
                // release (a busy app is slow to say it's being dragged), KWin's move has no
                // release to end it and keeps the pointer: nothing can be clicked.
                if self.left_down != Some(wid) {
                    log::warn!("window {wid}: title-bar drag reported after the button was released: not moved");
                } else if let (Some(w), Some(seat)) = (self.windows.get(&wid), &self.seat) {
                    if let Role::Toplevel(t) = &w.role {
                        t.move_(seat, self.press_serial);
                    }
                }
            }
            UiEvent::ToggleMaximize(wid) => self.toggle_maximize(wid),
            UiEvent::Clipboard(offers) => self.set_clipboard(offers, qh),
            UiEvent::DragBegan { wid, names } => {
                log::info!(
                    "Mac drag in {wid}: {names:?} (button {}, pointer over {})",
                    if self.left_down == Some(wid) { "held" } else { "already released" },
                    self.pointer_at.0
                );
                self.mac_drag = Some(wid);
                self.drag_count = names.len().max(1);
                self.maybe_take_drag(qh);
            }
            UiEvent::DragData(uris) => {
                let uris = Arc::new(uris);
                for mut pipe in self.drag_waiting.drain(..) {
                    let u = uris.clone();
                    std::thread::spawn(move || {
                        use std::io::Write;
                        let _ = pipe.write_all(&u);
                    });
                }
                self.drag_uris = Some(uris);
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
            UiEvent::Raise { bundle_id, token } => self.raise_app(&bundle_id, token, qh),
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
            UiEvent::BlindShow { id, app, buttons, menu: true, .. } => crate::blind::show_menu(id, &app, &buttons, self.net.clone()),
            UiEvent::BlindShow { id, app, title, texts, buttons, menu: false } => {
                crate::blind::show(id, &app, &title, &texts, &buttons, self.net.clone())
            }
        }
    }

    fn add_window(&mut self, info: WinInfo, qh: &QueueHandle<Self>) {
        let wid = info.wid;
        if self.closed_offline.remove(&wid) {
            // Closed here while the Mac was away: close it there (it comes back if the app asks
            // first, e.g. to save).
            log::info!("window {wid} was closed while the Mac was away: closing it there");
            self.send(W::new(t::CLOSE).u32(wid).finish());
        }
        log::info!("window {wid} added: {} kind {} parent {} {}x{} at {},{}",
                   info.app_name, info.kind, info.parent, info.w, info.h, info.rel_x, info.rel_y);
        // Back (after a pause or a reconnect): its video replaces the still.
        self.unfreeze(wid);
        // A window of the same app while one of its windows is hidden for closing: the app asked
        // something instead of closing (a "Save changes?" sheet, an alert), or this is that
        // window announced again. It shows again, under the sheet.
        let hidden: Vec<u32> =
            self.windows.values().filter(|w| w.hidden.is_some() && (w.info.pid == info.pid || w.info.wid == wid)).map(|w| w.info.wid).collect();
        for h in hidden {
            self.unhide(h, "its app showed another window instead of closing", qh);
        }
        // Announced again as another kind: a window shown on its own that the Mac now knows is
        // attached to another (a "Save changes?" sheet, recognised a moment later), or the other
        // way round. A toplevel can't become a popup: the window is made anew.
        if self.windows.get(&wid).is_some_and(|w| (w.info.kind == 1) != (info.kind == 1)) {
            log::info!("window {wid}: now kind {} (was {}): made anew", info.kind, self.windows[&wid].info.kind);
            self.remove_window(wid);
        }
        if let Some(w) = self.windows.get_mut(&wid) {
            // Re-announced after a reconnect: keep the native window, restart the video, and make
            // sure the Mac window still has this window's size.
            w.stale = false;
            w.info = info;
            w.requested = None;
            let _ = self.net.send(NetCmd::Subscribe(wid));
            self.adopt_mac_size(wid, qh);
            return;
        }
        if info.kind == 1 && self.windows.get(&info.parent).is_some_and(|p| p.shown.is_none() && matches!(p.role, Role::Toplevel(_))) {
            log::info!("popup {wid}: waits for its window {} to be set up", info.parent);
            self.popups_waiting.insert(wid, info);
            return;
        }
        self.popups_waiting.remove(&wid);
        let first_shape = Some(info.w as f64 / info.h.max(1.0) as f64);
        let mut fixed_now = false;
        let mut floated_now = false;
        let mut fixed_spot = None;
        let mac_size = (info.w.round().max(1.0) as u32, info.h.round().max(1.0) as u32);
        let (role, base) = if info.kind == 1 && self.windows.contains_key(&info.parent) {
            let parent = &self.windows[&info.parent];
            let parent_xdg = match &parent.role {
                Role::Toplevel(w) => w.xdg_surface().clone(),
                Role::Popup(p) => p.xdg_surface().clone(),
            };
            let pos = XdgPositioner::new(&self.xdg).expect("positioner");
            pos.set_size(info.w.max(1.0) as i32, info.h.max(1.0) as i32);
            let (x, y) = info.spot_on(parent.info.w, parent.info.h);
            pos.set_anchor_rect(x.round() as i32, y.round() as i32, 1, 1);
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
            // A sheet is said to be fixed before it's added: as a popup it's kept its size anyway.
            self.pending_fixed.remove(&wid);
            (Role::Popup(popup), s)
        } else {
            let surface = self.compositor.create_surface(qh);
            if info.kind == 3 {
                // The Mac display: an ordinary window with a KDE frame (the picture has no title
                // bar of its own), on the Mac desktop with the rest (app id "sweeflow-…").
                let window = self.xdg.create_window(surface, WindowDecorations::RequestServer, qh);
                window.set_title(format!("Mac display — {}", info.title));
                window.set_app_id("sweeflow-display");
                window.set_min_size(Some((320, 160)));
                window.commit();
                let s = window.wl_surface().clone();
                (Role::Toplevel(window), s)
            } else {
            // The Mac window brings its own title bar and traffic lights; no KDE frame around it.
            let window = self.xdg.create_window(surface, WindowDecorations::RequestClient, qh);
            window.set_title(display_title(&info.app_name, &info.title));
            window.set_app_id(format!("sweeflow-{}", info.bundle_id));
            if let Some((spot, fixed)) = self.pending_fixed.remove(&info.wid) {
                // Its app won't let it be resized (a reminder), or it floats on the Mac (picture
                // in picture), or keeps its shape (a video player): a dialog from the start, which
                // desktops and tilers (Karousel, Hyprland) float like a notification instead of
                // tiling; they decide that when a window appears. A fixed one is exactly its size.
                // (Modal: what tilers go by; with no parent window it holds nothing up.)
                if fixed {
                    let size = (info.w.round().max(1.0) as u32, info.h.round().max(1.0) as u32);
                    window.set_min_size(Some(size));
                    window.set_max_size(Some(size));
                    fixed_now = true;
                }
                // Not on GNOME for one with no parent: GNOME doesn't tile (nothing to float it
                // out of), and it won't minimise a dialog (Calculator's yellow light did nothing).
                let gnome = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').any(|d| d.eq_ignore_ascii_case("GNOME"));
                let parented = self.windows.get(&info.parent).is_some();
                if let Some(d) = self.dialog_mgr.as_ref().filter(|_| parented || !gnome) {
                    d.get_xdg_dialog(window.xdg_toplevel(), qh, ()).set_modal();
                }
                // A dialog of one of its app's windows (a "Save changes?" sheet): its parent
                // here too, so the desktop keeps it over that window (GNOME attaches it to it
                // like a sheet) instead of at the spot it had on the Mac's screen.
                // Only one lying inside that window on the Mac: a reminder or a video's floating
                // panel keeps its own place.
                let parent = self.windows.get(&info.parent).filter(|p| {
                    info.rel_x >= -1.0 && info.rel_y >= -1.0
                        && info.rel_x + info.w <= p.info.w + 1.0 && info.rel_y + info.h <= p.info.h + 1.0
                }).and_then(|p| match &p.role {
                    Role::Toplevel(t) => Some(t.xdg_toplevel().clone()),
                    _ => None,
                });
                if let Some(p) = &parent {
                    window.xdg_toplevel().set_parent(Some(p));
                }
                floated_now = true;
                fixed_spot = spot;
                if let (Some(spot), None) = (spot, &parent) {
                    crate::desktop::place_at_spot(
                        format!("sweeflow-{}", info.bundle_id),
                        display_title(&info.app_name, &info.title),
                        spot,
                    );
                }
            } else if info.kind != 2 {
                // Floating panels (kind 2: palettes) are ordinary, independent windows here too:
                // own taskbar entry, tiled like the rest.
                window.set_min_size(Some(MIN_SIZE));
            }
            window.commit();
            let s = window.wl_surface().clone();
            (Role::Toplevel(window), s)
            }
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
        // The Mac gets every shortcut while one of its windows has focus (Super as Cmd), where
        // the desktop allows it. Not on Hyprland: there the desktop's own keys win (the owner's
        // call for Omarchy: its menu on Super+Space, workspaces, closing and moving windows),
        // and whatever Hyprland has no binding for still reaches the Mac. `SWEEFLOW_KEYS=mac`
        // gives the Mac everything there too.
        // (Held only while the pointer is over a Mac window: see sync_inhibitor.)
        if matches!(role, Role::Toplevel(_)) {
            gnome_allow_shortcuts(&info.bundle_id);
        }
        let inhibitor = None;

        let appmenu = match (&role, &self.appmenu_mgr, &self.menus) {
            (Role::Toplevel(_), Some(mgr), Some(menus)) if info.kind != 3 => {
                let am = mgr.create(&base, qh, ());
                am.set_address(menus.service(), Menus::object_path(info.pid));
                Some(am)
            }
            _ => None,
        };
        self.surfaces.insert(base.id(), wid);
        self.surfaces.insert(video.id(), wid);
        let display = info.kind == 3;
        if display {
            crate::tray::set_display_shown(true);
        }
        // The display opens at a comfortable fraction of its size (it's as big as the screen).
        let size = if display {
            ((info.w * 0.6).max(320.0) as u32, (info.h * 0.6).max(160.0) as u32)
        } else {
            (info.w.max(1.0) as u32, info.h.max(1.0) as u32)
        };
        let info_kind = info.kind;
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
                inhibitor,
                configured: false,
                followed: false,
                min_size: if fixed_now { mac_size } else if info_kind == 2 || info_kind == 3 { (1, 1) } else { MIN_SIZE },
                max_size: fixed_now.then_some(mac_size),
                fixed: fixed_now,
                floated: floated_now,
                first_shape,
                spot: fixed_spot,
                refusal: None,
                size,
                requested: None,
                floating: true,
                maximized: false,
                tiled: false,
                suspended: false,
                buttons: [[0.0; 4]; 3],
                lights: None,
                corners: None,
                shown: None,
                picture: None,
                video_at: (0, 0),
                still: None,
                appmenu,
                dark_titlebar: true,
                mac_minimized: false,
                lights_hover: false,
                lights_pressed: None,
                pending: None,
                hidden: None,
                pending_raw: None,
                raw_buffers: Default::default(),
                stale: false,
                popup_wait: false,
                check_pending: false,
                display,
                fit: (1.0, 0.0, 0.0),
                settling: None,
                popup_settled: false,
            },
        );
        if !is_popup {
            self.apply_icon(wid, qh);
            // Launched here moments ago: the launcher's token puts it in front (and ends the
            // launch feedback); without it a compositor may keep it behind.
            let bid = self.windows[&wid].info.bundle_id.clone();
            if let Some((token, at)) = self.launch_tokens.remove(&bid) {
                if at.elapsed() < std::time::Duration::from_secs(20) {
                    if let (Some(a), Role::Toplevel(t)) = (&self.activation, &self.windows[&wid].role) {
                        log::info!("window {wid}: in front with the launch's token");
                        a.activate(token, t.wl_surface());
                    }
                }
            }
        }
        let _ = self.net.send(NetCmd::Subscribe(wid));
    }

    fn remove_window(&mut self, wid: u32) {
        if self.selection.as_ref().is_some_and(|s| s.wid == wid) {
            self.end_selection();
        }
        self.unfreeze(wid);
        self.popups_waiting.retain(|_, i| i.parent != wid);
        let Some(w) = self.windows.remove(&wid) else { return };
        log::info!("window {wid} removed ({})", w.info.app_name);
        if w.display {
            crate::tray::set_display_shown(false);
        }
        let _ = self.net.send(NetCmd::Unsubscribe(wid));
        self.surfaces.remove(&w.base().id());
        self.surfaces.remove(&w.video.id());
        if let Some(c) = self.srgb_tagged.remove(&w.video.id()) {
            c.destroy();
        }
        if let Some(cr) = &w.corners {
            for id in [cr.top.1.id(), cr.bottom.1.id()] {
                if let Some(c) = self.srgb_tagged.remove(&id) {
                    c.destroy();
                }
            }
        }
        if self.pointer_focus == Some(wid) {
            self.pointer_focus = None;
        }
        if self.kb_focus == Some(wid) {
            self.kb_focus = None;
        }
        if !self.windows.values().any(|o| o.info.pid == w.info.pid) {
            crate::menus::app_gone(w.info.pid);
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
        // Dropping the proxy sends nothing: without an explicit destroy KWin keeps the inhibitor,
        // and a window re-created right away (video-call apps do this) gets a protocol error that kills us.
        if let Some(i) = w.inhibitor {
            i.destroy();
        }
        w.video_viewport.destroy();
        w.video_sub.destroy();
        w.video.destroy();
        w.base_viewport.destroy();
        drop(w.role);
    }

    /// The Mac changed the window size itself (app-initiated resize). Follow it if the window
    /// is floating and we are not in the middle of our own resize.
    /// Looks whether the Mac followed the size asked for, once things have settled: one look
    /// per window, put off while sizes keep coming. (A tiling drag resizes every window many
    /// times a second; a look per size piled up, and looks landing mid-drag re-asked sizes and
    /// declared false limits, which made the tiling script lay everything out again.)
    fn schedule_check(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        if let Some(w) = self.windows.get_mut(&wid) {
            Self::arm_check(&self.loop_handle, &mut w.check_pending, wid, qh);
        }
    }

    /// schedule_check for when the window is already borrowed.
    fn arm_check(lh: &LoopHandle<'static, App>, pending: &mut bool, wid: u32, qh: &QueueHandle<Self>) {
        if *pending {
            return;
        }
        *pending = true;
        let qh = qh.clone();
        let _ = lh.insert_source(Timer::from_duration(Duration::from_millis(3200)), move |_, _, app| {
            let Some(w) = app.windows.get_mut(&wid) else { return TimeoutAction::Drop };
            w.check_pending = false;
            if w.requested.is_some_and(|(_, at)| at.elapsed() < Duration::from_millis(3000)) {
                // Still being resized: look later.
                app.schedule_check(wid, &qh);
            } else {
                app.adopt_mac_size(wid, &qh);
            }
            TimeoutAction::Drop
        });
    }

    /// A window its app won't let be resized: its smallest and largest size here are the Mac's,
    /// and so is its size.
    fn hold_mac_size(w: &mut Win) {
        let Role::Toplevel(win) = &w.role else { return };
        let mac = (w.info.w.round().max(1.0) as u32, w.info.h.round().max(1.0) as u32);
        if w.min_size != mac || w.max_size != Some(mac) {
            w.min_size = mac;
            w.max_size = Some(mac);
            win.set_min_size(Some(mac));
            win.set_max_size(Some(mac));
            win.commit();
        }
        w.size = mac;
    }

    /// A Hyprland tile got a new size: the Mac is asked for it once it has stood for a moment
    /// (SETTLE), not at once. Hyprland lays a whole workspace out again in one go, and often
    /// twice within a moment (a size in between, then the tile's): asked for each, the Mac
    /// resized every window twice and was still at the first size when looked at. Meanwhile
    /// the picture shown is laid out in the tile as it is (lay_out). One timer per window; each
    /// new size starts the wait again.
    fn arm_settle(lh: &LoopHandle<'static, App>, settling: &mut Option<Instant>, wid: u32, qh: &QueueHandle<Self>) {
        if settling.replace(Instant::now()).is_some() {
            return; // the timer is running: it goes by when the last size came
        }
        let qh = qh.clone();
        let _ = lh.insert_source(Timer::from_duration(SETTLE), move |_, _, app| {
            let Some(w) = app.windows.get_mut(&wid) else { return TimeoutAction::Drop };
            let Some(since) = w.settling else { return TimeoutAction::Drop };
            let left = SETTLE.saturating_sub(since.elapsed());
            if !left.is_zero() {
                return TimeoutAction::ToDuration(left);
            }
            w.settling = None;
            app.ask_settled_size(wid, &qh);
            TimeoutAction::Drop
        });
    }

    /// The tile's size has stood (arm_settle): the Mac is asked for it.
    fn ask_settled_size(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let size = w.size;
        let mac = (w.info.w.round() as u32, w.info.h.round() as u32);
        if mac == size && w.requested.is_none_or(|(asked, _)| asked == size) {
            // There and back within the moment: the Mac has this size and was asked for no other.
            w.followed = true;
            w.refusal = None;
        } else {
            log::debug!("window {wid}: its tile settled at {}x{}: asking the Mac", size.0, size.1);
            w.requested = Some((size, Instant::now()));
            let _ = self.net.send(NetCmd::Control(W::new(t::RESIZE).u32(wid).f32(size.0 as f32).f32(size.1 as f32).finish()));
            // Check it took (a Mac window that can't follow reports no change).
            Self::arm_check(&self.loop_handle, &mut w.check_pending, wid, qh);
        }
        self.relayout(wid, qh);
    }

    /// Hyprland tiled the window, or let it float (UiEvent::Tiling).
    fn tiling_changed(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if !w.in_tile() {
            return;
        }
        // The Mac is asked for the tile's size, if it has another (a refusal from before the
        // tiling counts no more).
        w.refusal = None;
        self.adopt_mac_size(wid, qh);
    }

    fn adopt_mac_size(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        // Where a popup goes on its parent (looked up before this window is borrowed).
        let parent_size = self.windows.get(&wid).and_then(|w| self.windows.get(&w.info.parent)).map(|p| (p.info.w, p.info.h));
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.display {
            return; // scaled to fit, whatever its size
        }
        let mac = (w.info.w.round() as u32, w.info.h.round() as u32);
        if mac == w.size || !w.configured {
            w.followed |= mac == w.size;
            if mac == w.size && w.in_tile() {
                // It has the tile's size after all: asked again if it leaves it later (below).
                w.refusal = None;
            }
            return;
        }
        if w.settling.is_some() {
            return; // a new size is about to be asked for (arm_settle)
        }
        if let Some((_, at)) = w.requested {
            let wait = if matches!(w.role, Role::Toplevel(_)) && !w.floating { 3000 } else { 1500 };
            if at.elapsed() < Duration::from_millis(wait) {
                return;
            }
        }
        if matches!(w.role, Role::Toplevel(_)) {
            // A window its app won't let be resized: exactly the Mac's size, whatever it is now.
            if w.fixed {
                let before = w.size;
                Self::hold_mac_size(w);
                if let Some(spot) = w.spot.filter(|_| w.size != before) {
                    crate::desktop::place_at_spot(format!("sweeflow-{}", w.info.bundle_id), display_title(&w.info.app_name, &w.info.title), spot);
                }
                self.draw_base(wid, qh);
                self.relayout(wid, qh);
                return;
            }
            // Asked for this size and the app kept another: the window here keeps the
            // desktop's size all the same (no limits are declared from it). What fits of the
            // picture shows, cut off at the window's edge, or the rest is see-through
            // (lay_out). Seen once, it's asked once more first: during a drag (or right after
            // a reconnect) the Mac lags, and a real refusal comes back the same.
            let size = w.size;
            if !w.followed && w.requested.is_some_and(|(r, _)| r == size) {
                // The same answer twice, in the shape the window had (a video player keeps its
                // video's shape): it's opened again as a floating window of the Mac's size, as a
                // floating panel is, instead of a tile it leaves mostly empty.
                let shape = |a: (u32, u32)| a.0 as f64 / a.1.max(1) as f64;
                let kept_shape = w.first_shape.is_some_and(|f| (shape(mac) / f - 1.0).abs() < 0.015);
                if w.refusal == Some((size, mac)) && !w.floated && kept_shape && (shape(size) / shape(mac) - 1.0).abs() > 0.03 {
                    log::info!("window {wid}: keeps its shape ({}x{}, asked {}x{}): floating it at that size", mac.0, mac.1, size.0, size.1);
                    let info = w.info.clone();
                    // What the Mac said of it once, which it won't say again for the new one.
                    let buttons = w.buttons.clone();
                    self.pending_fixed.insert(wid, (None, false));
                    self.remove_window(wid);
                    let (class, title) = (format!("sweeflow-{}", info.bundle_id), display_title(&info.app_name, &info.title));
                    self.add_window(info, qh);
                    if crate::hyprland::running() {
                        // Hyprland tiles a dialog with no parent all the same (the window came
                        // back tiled, in a corner): floated by name, at the Mac's size, centred.
                        crate::hyprland::float_window_sized(class, title, Some(mac));
                    }
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.buttons = buttons;
                    }
                    self.update_lights(wid, qh);
                    return;
                }
                if w.refusal != Some((size, mac)) {
                    w.refusal = Some((size, mac));
                    w.requested = Some((size, Instant::now()));
                    let _ = self.net.send(NetCmd::Control(W::new(t::RESIZE).u32(wid).f32(size.0 as f32).f32(size.1 as f32).finish()));
                    Self::arm_check(&self.loop_handle, &mut w.check_pending, wid, qh);
                }
                return;
            }
        }
        match &w.role {
            Role::Toplevel(_) if w.floating => {
                w.size = mac;
                self.draw_base(wid, qh);
            }
            Role::Popup(_) if !w.popup_settled => {
                // Wait: macOS animates popovers closing (shrinking toward their anchor); followed
                // step by step, the popup would slide away before it disappears. A real change is
                // still there a moment later.
                if !w.popup_wait {
                    w.popup_wait = true;
                    let qh = qh.clone();
                    let _ = self.loop_handle.insert_source(Timer::from_duration(Duration::from_millis(200)), move |_, _, app| {
                        if let Some(w) = app.windows.get_mut(&wid) {
                            w.popup_wait = false;
                            w.popup_settled = true;
                        }
                        app.adopt_mac_size(wid, &qh);
                        if let Some(w) = app.windows.get_mut(&wid) {
                            w.popup_settled = false;
                        }
                        TimeoutAction::Drop
                    });
                }
            }
            Role::Popup(p) => {
                let pos = XdgPositioner::new(&self.xdg).expect("positioner");
                pos.set_size(mac.0.max(1) as i32, mac.1.max(1) as i32);
                let (x, y) = parent_size.map_or((w.info.rel_x, w.info.rel_y), |(pw, ph)| w.info.spot_on(pw, ph));
                pos.set_anchor_rect(x.round() as i32, y.round() as i32, 1, 1);
                pos.set_anchor(xdg_positioner::Anchor::TopLeft);
                pos.set_gravity(xdg_positioner::Gravity::BottomRight);
                pos.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::None);
                p.reposition(&pos, 0);
                w.size = mac;
                self.draw_base(wid, qh);
            }
            Role::Toplevel(_) => {
                let size = w.size;
                // Tiled here, but the app resized itself on the Mac (a browser does, e.g. on opening
                // a tab): ask again for the size it has here. At most every few seconds, so an
                // app that can't be that small (Terminal snaps to character cells) isn't nagged.
                w.requested = Some((size, Instant::now()));
            w.followed = false;
                log::info!("window {wid}: {}x{} on the Mac, {}x{} here: asking again", mac.0, mac.1, size.0, size.1);
                let _ = self.net.send(NetCmd::Control(W::new(t::RESIZE).u32(wid).f32(size.0 as f32).f32(size.1 as f32).finish()));
                // If the Mac doesn't follow (and so reports no change), look again later. Only
                // when far off: an app that snaps to its own steps (Terminal's character cells)
                // is left alone.
                let far = mac.0.abs_diff(size.0) > 40 || mac.1.abs_diff(size.1) > 40;
                if far {
                    Self::arm_check(&self.loop_handle, &mut w.check_pending, wid, qh);
                }
            }
        }
    }

    fn draw_base(&mut self, wid: u32, _qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get(&wid) else { return };
        let (bw, bh) = w.size;
        if let Role::Toplevel(t) = &w.role {
            t.xdg_surface().set_window_geometry(0, 0, bw as i32, bh as i32);
        }
        let base = w.base();
        // Transparent: the window's rounded corners must show the desktop behind them, and so
        // must the part of a window its picture doesn't reach (the Mac hasn't followed a grow
        // yet, or the app goes no bigger). The display view is letterboxed: dark bars.
        let fill = if w.display { &self.background } else { &self.clear };
        base.attach(fill.as_ref(), 0, 0);
        w.base_viewport.set_destination(bw as i32, bh as i32);
        base.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.clip_input(wid, _qh);
        if let Some(w) = self.windows.get(&wid) {
            w.base().commit();
        }
    }

    /// Clicks land on the window only where the Mac window is. A window bigger here than on
    /// the Mac (tiled wider than the app goes) is transparent around it, and must not swallow
    /// the clicks meant for what shows through. Takes effect on the base surface's next commit.
    fn clip_input(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get(&wid) else { return };
        if !matches!(w.role, Role::Toplevel(_)) || w.display {
            return;
        }
        let mac = (w.info.w.round().max(1.0) as i32, w.info.h.round().max(1.0) as i32);
        let base = w.base();
        let region = self.compositor.wl_compositor().create_region(qh, ());
        // Hidden for closing: nothing of it takes a click (what's behind it does).
        if w.hidden.is_none() {
            region.add(0, 0, mac.0.min(w.size.0 as i32), mac.1.min(w.size.1 as i32));
        }
        base.set_input_region(Some(&region));
        region.destroy();
    }

    fn present(&mut self, wid: u32, frame: GpuFrame, qh: &QueueHandle<Self>) {
        if self.paused && self.windows.get(&wid).is_some_and(|w| w.still.is_some()) {
            // Frames still in flight as the Mac lets go (from its own screen by then, at another
            // size): the still stays.
            let _ = self.net.send(NetCmd::Video(wid, W::new(t::FRAME_ACK).u64(frame.capture_us).finish()));
            return;
        }
        if let Some(w) = self.windows.get_mut(&wid).filter(|w| w.hidden.is_some()) {
            // Hidden for closing: the newest picture waits for the window to come back (if it does).
            let _ = self.net.send(NetCmd::Video(wid, W::new(t::FRAME_ACK).u64(frame.capture_us).finish()));
            w.pending = Some(frame);
            return;
        }
        // Moving again: the still goes.
        self.unfreeze(wid);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.dark_titlebar != frame.dark_titlebar {
            w.dark_titlebar = frame.dark_titlebar;
            self.update_lights(wid, qh);
        }
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let full_range = frame.full_range;
        // (An RGB picture has neither; telling the compositor of them would be an error.)
        if !frame.rgb && w.color_full_range != Some(full_range) {
            let range = if full_range { crs::Range::Full } else { crs::Range::Limited };
            // Every surface showing this video (middle and the corner layout's strips) must
            // agree, or the strips show as bars in a different brightness.
            for c in w.color.iter().chain(w.corners.iter().flat_map(|c| c.colors.iter())) {
                c.set_coefficients_and_range(crs::Coefficients::Bt709, range);
            }
            log::info!("window {wid}: video is {} range", if full_range { "full" } else { "limited" });
            w.color_full_range = Some(full_range);
        }
        self.describe_srgb(wid, qh);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let capture_us = frame.capture_us;
        {
            // Decoded → here (the UI thread's turn), for the latency figures.
            let st = &mut self.present_stats;
            let us = frame.decoded_at.elapsed().as_micros() as u64;
            st.0 += 1;
            st.1 += us;
            st.2 = st.2.max(us);
            if st.3.elapsed().as_secs() >= 5 {
                log::debug!("frames: decoded→presented {:.1} ms on average, worst {:.1} ms ({} frames)", st.1 as f64 / st.0 as f64 / 1000.0, st.2 as f64 / 1000.0, st.0);
                *st = (0, 0, 0, std::time::Instant::now());
            }
        }
        // Always ack so the Mac keeps streaming, even if we drop this frame.
        let ack = W::new(t::FRAME_ACK).u64(capture_us).finish();
        // How far behind its capture the picture is shown: the sound waits as long.
        crate::net::video_shown(capture_us);
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

        if w.display {
            // Scaled to fit, centred.
            let (k, ox, oy) = w.fit;
            let (dw, dh) = ((w.info.w as f64 * k).round().max(1.0) as i32, (w.info.h as f64 * k).round().max(1.0) as i32);
            w.video_sub.set_position(ox.round() as i32, oy.round() as i32);
            w.video_viewport.set_source(0.0, 0.0, frame.content.0 as f64, frame.content.1 as f64);
            w.video_viewport.set_destination(dw, dh);
            w.video.attach(Some(&buffer), 0, 0);
            w.video.damage_buffer(0, 0, frame.width as i32, frame.height as i32);
            w.video.commit();
            w.base().commit(); // the subsurface position applies with the parent's commit
            w.shown = Some(buffer.id());
            self.in_use.insert(buffer.id(), frame);
            return;
        }

        let first_picture = w.shown.is_none();
        w.shown = Some(buffer.id());
        w.picture = Some((frame.content, frame.px_per_pt));
        self.in_use.insert(buffer.id(), frame);
        self.lay_out(wid, Some(&buffer), true, qh);
        if first_picture {
            self.add_waiting_popups(wid, qh);
        }
    }

    /// The popups that waited for their window to show (see popups_waiting), each with a fresh
    /// picture: the frames sent meanwhile had no window to go to.
    fn add_waiting_popups(&mut self, parent: u32, qh: &QueueHandle<Self>) {
        let kids: Vec<u32> = self.popups_waiting.values().filter(|i| i.parent == parent).map(|i| i.wid).collect();
        for k in kids {
            if let Some(info) = self.popups_waiting.remove(&k) {
                self.add_window(info, qh);
                let _ = self.net.send(NetCmd::Subscribe(k));
            }
        }
    }

    /// The window's size here changed (or how Hyprland holds it) with no new picture yet: the
    /// one shown is laid out again, so a tile is filled at once instead of when the Mac has
    /// followed.
    fn relayout(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get(&wid) else { return };
        if !w.configured || w.display || w.hidden.is_some() {
            return;
        }
        // The buffer itself is needed only to show it on more surfaces (the corner layout's
        // strips); a compositor that has let go of it still shows the picture.
        let buffer = w
            .shown
            .clone()
            .filter(|id| self.in_use.contains_key(id))
            .and_then(|id| wl_buffer::WlBuffer::from_id(&self.conn, id).ok());
        self.lay_out(wid, buffer.as_ref(), false, qh);
    }

    /// Places the window's picture: `fresh`, a new frame's buffer is shown; otherwise the one
    /// already shown is placed again (relayout).
    fn lay_out(&mut self, wid: u32, buffer: Option<&wl_buffer::WlBuffer>, fresh: bool, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.hidden.is_some() {
            return; // nothing shows until it's back (unhide lays it out again)
        }
        let Some(((fw, fh), reported)) = w.picture else { return };
        // Map the frame to the window's size in points (the frame may be 2x Retina pixels; the
        // compositor then downsamples it), cropped to the window if the Mac lags a resize.
        let s = self.scale;
        let (bw, bh) = w.size;
        let px_per_pt = px_per_pt(reported, fw, w.info.w, s);
        // Never more of the frame than the Mac window itself: right after it shrinks, the capture
        // is still its old size and macOS fills the rest with black. Beyond the window there's
        // nothing to show, so it stays see-through, like the rest of the window here.
        let (fw, fh) = if w.info.w > 0.0 && w.info.h > 0.0 && !w.display {
            (fw.min((w.info.w as f64 * px_per_pt).round() as u32).max(1), fh.min((w.info.h as f64 * px_per_pt).round() as u32).max(1))
        } else {
            (fw, fh)
        };
        // Drawn 1:1 from the window's top-left, never scaled: what doesn't fit (the Mac hasn't
        // followed a shrink yet, or the app goes no smaller than the window here: a Hyprland
        // tile) is cut off at the window's edge, and a picture
        // smaller than the window leaves the rest see-through.
        let ((sw, sh), (dw, dh), (cut_r, cut_b)) = shown_part((fw, fh), px_per_pt, (bw, bh), s);

        // Rounded corners go on the window's real edges only: the left and top always are, the
        // right and bottom unless the picture is cut off there. A cut edge is straight (a
        // rounded tile on it showed a bite of the wrong place), so there the strips run to the
        // window's edge. (Apps like Terminal snap to character cells, so the Mac window can run
        // a point or two past ours: still its edge, and the corners go on the visible area.)
        let t_px = w.corners.as_ref().map_or(0, |c| c.t_px);
        let l = ((t_px as f64 / px_per_pt) * s).round() as i32; // corner size, logical px
        // (Strips that aren't showing yet need the buffer.)
        let was_split = w.corners.as_ref().is_some_and(|c| c.split);
        // Square corners (Omarchy's look) need no strips: the picture is one surface. Strips
        // there also showed a band of an older picture along the top and bottom on Hyprland.
        let split = t_px > 0
            && !crate::config::square_corners()
            && sw as u32 > 3 * t_px
            && sh as u32 > 3 * t_px
            && l > 0
            && dw > 2 * l
            && dh > 2 * l
            && (buffer.is_some() || was_split);
        // TL, TR, BL, BR.
        let rounded = [true, !cut_r, !cut_b, !cut_r && !cut_b];
        let middle_y = if split { l } else { 0 };
        // The bottom strip is there only where the bottom edge is the window's own.
        let bottom = split && !cut_b;
        let base = w.base().clone();
        let at = (0, middle_y);
        let mut relight = false;
        if w.video_at != at {
            w.video_at = at;
            w.video_sub.set_position(at.0, at.1);
            base.commit(); // subsurface positions apply with the parent's commit
            relight = true; // where the lights hang off the video, they moved with it
        }
        if let Some(c) = w.corners.as_mut() {
            // The corners setting changed: the tiles again, from the Mac's own.
            if c.squared != crate::config::square_corners() {
                c.squared = !c.squared;
                if let Some((t, raw)) = c.raw_tiles.clone() {
                    let px = if c.squared { square_tiles(t as usize, raw) } else { raw };
                    c.last_tiles = Some((t, px.clone()));
                    c.pending = Some((t, px));
                }
            }
            if c.middle_y != middle_y {
                c.middle_y = middle_y;
                relight = true;
            }
            if split {
                let t = t_px as f64;
                // Where a strip ends: short of the corner tile, or at the window's edge.
                let end = |tile: bool| if tile { (t, l) } else { (0.0, 0) };
                // A strip takes the buffer with a new frame, or when it wasn't showing one.
                let attach = buffer.filter(|_| fresh || !c.split);
                // Children of the middle strip: positions are relative to it.
                let (tsub, tsurf, tvp) = &c.top;
                let (end_t, end_l) = end(rounded[1]);
                tsub.set_position(l, -l);
                tvp.set_source(t, 0.0, sw - t - end_t, t);
                tvp.set_destination(dw - l - end_l, l);
                if let Some(buffer) = attach {
                    tsurf.attach(Some(buffer), 0, 0);
                    tsurf.damage_buffer(0, 0, fw as i32, fh as i32);
                }
                tsurf.commit();
                let (bsub, bsurf, bvp) = &c.bottom;
                if bottom {
                    let (end_t, end_l) = end(rounded[3]);
                    bsub.set_position(l, dh - 2 * l);
                    bvp.set_source(t, sh - t, sw - t - end_t, t);
                    bvp.set_destination(dw - l - end_l, l);
                    if let Some(buffer) = buffer.filter(|_| fresh || !c.bottom_shown) {
                        bsurf.attach(Some(buffer), 0, 0);
                        bsurf.damage_buffer(0, 0, fw as i32, fh as i32);
                    }
                } else if c.bottom_shown {
                    bsurf.attach(None, 0, 0);
                }
                bsurf.commit();
                c.bottom_shown = bottom;
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
                        c.tile_buffers.push_back((bufs, pool));
                        while c.tile_buffers.len() > 3 {
                            if let Some((old, _)) = c.tile_buffers.pop_front() {
                                old.iter().for_each(|b| b.destroy());
                            }
                        }
                    }
                }
                // The rounded corners' tiles, from the newest set (taken by a tile that shows
                // for the first time since too); a cut corner's shows nothing.
                let tp = c.t_px as i32;
                let bufs = c.tile_buffers.back().map(|(b, _)| b);
                for (i, (sub, surf, vp)) in c.tiles.iter().enumerate() {
                    if rounded[i] {
                        if let Some(bufs) = bufs.filter(|_| new_tiles.is_some() || !c.tiles_shown[i]) {
                            surf.attach(Some(&bufs[i]), 0, 0);
                            surf.damage_buffer(0, 0, tp, tp);
                        }
                        sub.set_position(pos[i].0, pos[i].1);
                        vp.set_destination(l, l);
                    } else if c.tiles_shown[i] {
                        surf.attach(None, 0, 0);
                    }
                    surf.commit();
                }
                c.tiles_shown = rounded;
            } else if c.split {
                for (_, surf, _) in [&c.top, &c.bottom].into_iter().chain(c.tiles.iter()) {
                    surf.attach(None, 0, 0);
                    surf.commit();
                }
                c.tiles_shown = [false; 4];
                c.bottom_shown = false;
            }
            if c.split != split {
                log::info!("window {wid}: rounded corners {} (frame {fw}x{fh}, window {bw}x{bh}, t {t_px})",
                           if split { "on" } else { "off" });
            }
            c.split = split;
        }
        if split {
            let t = t_px as f64;
            let (end_t, end_l) = if bottom { (t, l) } else { (0.0, 0) };
            w.video_viewport.set_source(0.0, t, sw, sh - t - end_t);
            w.video_viewport.set_destination(dw, dh - l - end_l);
        } else {
            w.video_viewport.set_source(0.0, 0.0, sw, sh);
            w.video_viewport.set_destination(dw, dh);
        }
        if let Some(buffer) = buffer.filter(|_| fresh) {
            w.video.attach(Some(buffer), 0, 0);
            w.video.damage_buffer(0, 0, fw as i32, fh as i32);
        }
        w.video.commit();
        // A paused still covers the picture: it goes where the picture went.
        if let Some(st) = w.still.as_ref().filter(|_| !fresh) {
            st.viewport.set_source(0.0, 0.0, sw, sh);
            st.viewport.set_destination(dw, dh);
            st.surface.commit();
            base.commit();
        }
        if relight {
            self.update_lights(wid, qh);
        }
    }

    /// States sRGB on the surfaces showing the window's video (the middle and the strips).
    fn describe_srgb(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        // RGB pictures are left untagged, like the corner tiles beside them: tagged, Hyprland
        // drew the two in different shades.
        if crate::decode::rgb_output() {
            return;
        }
        let (Some(cm), Some(desc)) = (&self.color_mgr, &self.srgb) else { return };
        if !self.srgb_ready {
            return;
        }
        let Some(w) = self.windows.get(&wid) else { return };
        let mut surfaces = vec![w.video.clone()];
        if let Some(c) = &w.corners {
            surfaces.push(c.top.1.clone());
            surfaces.push(c.bottom.1.clone());
        }
        for surf in surfaces {
            if self.srgb_tagged.contains_key(&surf.id()) {
                continue;
            }
            let cms = cm.get_surface(&surf, qh, ());
            cms.set_image_description(desc, wp_color_manager_v1::RenderIntent::Perceptual);
            self.srgb_tagged.insert(surf.id(), cms);
        }
    }

    /// Square or rounded corners was switched: every window's corner tiles again, from the
    /// Mac's own, without waiting for its next picture (an idle window may not send one soon).
    fn apply_corners_setting(&mut self, qh: &QueueHandle<Self>) {
        let square = crate::config::square_corners();
        for w in self.windows.values_mut() {
            let (video, base) = (w.video.clone(), w.base().clone());
            let Some(c) = w.corners.as_mut() else { continue };
            if c.squared == square {
                continue;
            }
            c.squared = square;
            let Some((t, raw)) = c.raw_tiles.clone() else { continue };
            let px = if square { square_tiles(t as usize, raw) } else { raw };
            c.last_tiles = Some((t, px.clone()));
            if !c.split {
                // Not showing tiles now: with the next picture that does.
                c.pending = Some((t, px));
                continue;
            }
            let Ok(mut pool) = RawPool::new(px.len(), &self.shm) else { continue };
            pool.mmap().copy_from_slice(&px);
            let n = (t * t * 4) as usize;
            let bufs: Vec<_> =
                (0..4).map(|i| pool.create_buffer((i * n) as i32, t as i32, t as i32, (t * 4) as i32, wl_shm::Format::Argb8888, (), qh)).collect();
            for (i, (_, surf, _)) in c.tiles.iter().enumerate().filter(|(i, _)| c.tiles_shown[*i]) {
                surf.attach(Some(&bufs[i]), 0, 0);
                surf.damage_buffer(0, 0, t as i32, t as i32);
                surf.commit();
            }
            c.tile_buffers.push_back((bufs, pool));
            while c.tile_buffers.len() > 3 {
                if let Some((old, _)) = c.tile_buffers.pop_front() {
                    old.iter().for_each(|b| b.destroy());
                }
            }
            // The tiles are synchronised children of the video: shown with its commit.
            video.commit();
            base.commit();
        }
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
                last_tiles: None,
                raw_tiles: None,
                squared: false,
                split: false,
                tiles_shown: [false; 4],
                bottom_shown: false,
                middle_y: 0,
                colors,
            };
            if let Some(w) = self.windows.get_mut(&wid) {
                // The lights stay the topmost child of the video (where they are one).
                if let (Some(l), Some((_, last, _)), true) = (&w.lights, corners.tiles.last(), crate::hyprland::running()) {
                    l.sub.place_above(last);
                }
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
        // Square corners (the setting; by default where the desktop frames every window as a
        // plain rectangle and rounded corners would leave gaps inside the frame): filled in.
        c.raw_tiles = (t_px > 0).then(|| (t_px, tile_px.clone()));
        c.squared = crate::config::square_corners();
        let tile_px = if t_px > 0 && c.squared { square_tiles(t_px as usize, tile_px) } else { tile_px };
        c.last_tiles = (t_px > 0).then(|| (t_px, tile_px.clone()));
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
        // Menus only: a PNG of a big one (Quick Look) takes seconds, and this is the UI thread.
        if self.trace && fw * fh <= 1 << 20 {
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
        // And the popup itself: Hyprland showed nothing of a popup's picture until the popup
        // was committed again (a menu or Quick Look soon is, moved or resized; a sheet that
        // just stays never was, and showed as nothing at all).
        w.base().commit();
        // KWin uploads shm contents on commit; three generations is plenty of slack.
        w.raw_buffers.push_back((buffer, pool));
        while w.raw_buffers.len() > 3 {
            if let Some((b, _)) = w.raw_buffers.pop_front() {
                b.destroy();
            }
        }
    }

    /// An app with a window here needs a taskbar entry for its badged icon (see
    /// launchers::seen): looked at once per app, when its window and its icon are both here.
    fn note_app(&mut self, wid: u32) {
        let Some(w) = self.windows.get(&wid) else { return };
        if !matches!(w.role, Role::Toplevel(_)) || w.display {
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

    fn apply_icon(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        self.note_app(wid);
        let Some(mgr) = &self.icon_mgr else { return };
        let Some(w) = self.windows.get(&wid) else { return };
        let Role::Toplevel(t) = &w.role else { return };
        let bid = w.info.bundle_id.clone();
        if !self.icons.contains_key(&bid) {
            let Some(png) = self.icon_pngs.get(&bid) else { return };
            // Plain: the Sweeflow badge is for the taskbar only, which takes the icon from the
            // window's launcher entry (see launchers.rs), not this one (Alt+Tab, title bars).
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

    /// Whether a press at (x, y) (window points) of `wid` is known to drag the window: the Mac
    /// looked under the pointer as it hovered there (DRAG_SPOT) and found the bare title bar or
    /// toolbar (or a spot that dragged the window before). Only an answer for about the same
    /// point counts: a control a few points away (a tab, a toolbar button) must still get its
    /// own press, and a press elsewhere waits for the Mac's word on it (MOVE_READY). A pointer
    /// at rest gets no new looks, so an answer holds a good while; a change to the window's
    /// title bar drops it (WindowButtons).
    fn spot_drags(&self, wid: u32, x: f32, y: f32) -> bool {
        const NEAR: f32 = 6.0;
        const FRESH: Duration = Duration::from_secs(30);
        self.drag_spot.is_some_and(|(w, sx, sy, drags, at)| {
            w == wid && drags && (sx - x).abs() <= NEAR && (sy - y).abs() <= NEAR && at.elapsed() <= FRESH
        })
    }

    /// The pointer has moved far enough from the press to count as a drag (not a click).
    fn moved_enough(&self, x: f64, y: f64) -> bool {
        let (px, py) = self.press_at;
        (x - px).abs() >= 2.0 || (y - py).abs() >= 2.0
    }

    /// A press the Mac said moves the window (MOVE_READY) has started moving: KWin moves it
    /// now, from where the pointer is, rather than once the app has noticed (tens to hundreds
    /// of ms later, by when the pointer is well away from where it took hold). The Mac lets go
    /// of its press.
    fn move_here(&mut self, wid: u32) {
        self.move_ready = false;
        let (Some(w), Some(seat)) = (self.windows.get(&wid), &self.seat) else { return };
        let Role::Toplevel(t) = &w.role else { return };
        t.move_(seat, self.press_serial);
        self.left_down = None;
        self.mac_drag = None;
        self.moved_here = Some(wid);
        self.send(W::new(t::MOVE_TAKEN).u32(wid).finish());
        let (_, x, y) = self.pointer_at;
        log::debug!("window {wid}: move started {:.0} ms after the press, {:+.0},{:+.0} px from it ({})",
                    self.press_time.elapsed().as_secs_f64() * 1000.0, x - self.press_at.0, y - self.press_at.1,
                    if self.press_known { "a spot known from the hover" } else { "on the Mac's answer to the press" });
    }

    /// A file drag started in a Mac window and the pointer (button still down) has left it:
    /// continue it as a Linux drag so it can be dropped in Dolphin, the desktop, etc.
    fn maybe_take_drag(&mut self, qh: &QueueHandle<Self>) {
        let (wid, x, y) = self.pointer_at;
        let (Some(dw), Some(held)) = (self.mac_drag, self.left_down) else { return };
        if dw != wid || held != wid || self.drag_source.is_some() {
            return;
        }
        let Some(w) = self.windows.get(&wid) else { return };
        let (bw, bh) = (w.size.0 as f64, w.size.1 as f64);
        if x >= 0.0 && y >= 0.0 && x < bw && y < bh {
            return; // still inside: the Mac handles drags within its own window
        }
        let (Some(ddm), Some(dev)) = (&self.ddm, &self.dnd_device) else { return };
        let source = ddm.create_drag_and_drop_source(qh, ["text/uri-list"], DndAction::Copy);
        // A picture under the pointer while it's held (a document, with the number of files),
        // or nothing shows that anything is being carried.
        let icon = self.drag_picture(qh);
        source.start_drag(dev, w.base(), icon.as_ref().map(|i| &i.0), self.press_serial);
        self.drag_icon = icon;
        self.drag_source = Some(source);
        self.drag_uris = None;
        self.mac_drag = None;
        self.left_down = None;
        self.send(W::new(t::DRAG_TAKEN).u32(wid).finish());
        log::info!("took over the Mac drag from window {wid}");
    }

    /// The picture a drag out carries: a document (a page with a folded corner) in Sweeflow's
    /// violet, with the number of files on it when there are several. 48 × 56 logical px.
    fn drag_picture(&self, qh: &QueueHandle<Self>) -> Option<(wl_surface::WlSurface, RawPool, wl_buffer::WlBuffer)> {
        let scale = self.scale.max(1.0).ceil() as i32;
        let (w, h, px) = drag_picture_pixels(scale, self.drag_count);
        let mut pool = RawPool::new(px.len(), &self.shm).ok()?;
        pool.mmap().copy_from_slice(&px);
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, (), qh);
        let surface = self.compositor.create_surface(qh);
        surface.set_buffer_scale(scale);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, w, h);
        surface.commit();
        Some((surface, pool, buffer))
    }

    /// Puts the Mac's clipboard on ours.
    fn set_clipboard(&mut self, offers: HashMap<String, Arc<Vec<u8>>>, qh: &QueueHandle<Self>) {
        if self.data_control.is_none() {
            // The ordinary clipboard: taken now if a Mac window has the keyboard, else when one
            // gets it (keyboard enter).
            if !offers.is_empty() {
                self.cp_offers = Some(offers);
                if self.kb_focus.is_some() {
                    self.offer_clipboard(qh);
                }
            }
            return;
        }
        let (Some(mgr), Some(device)) = (&self.data_control, &self.clip_device) else { return };
        if offers.is_empty() {
            return;
        }
        let mimes: Vec<String> = offers.keys().cloned().collect();
        let source = mgr.create_data_source(qh, ClipData(offers));
        for m in mimes {
            source.offer(m);
        }
        device.set_selection(Some(&source));
        self.clip_ignore += 1;
        if let Some(old) = self.clip_source.replace(source) {
            old.destroy();
        }
    }

    /// The Mac's clipboard as the ordinary Wayland selection (no ext-data-control: GNOME).
    fn offer_clipboard(&mut self, qh: &QueueHandle<Self>) {
        let (Some(ddm), Some(device), Some(offers)) = (&self.ddm, &self.dnd_device, &self.cp_offers) else { return };
        let source = ddm.create_copy_paste_source(qh, offers.keys().cloned().collect::<Vec<_>>());
        source.set_selection(device, self.kb_serial);
        self.clip_ignore += 1;
        self.cp_source = Some(source);
    }

    /// The Linux clipboard changed: send it to the Mac (read on a thread: files can be large).
    fn on_selection(&mut self, offer: Option<ExtDataControlOfferV1>) {
        let Some(offer) = offer else { return };
        if self.clip_ignore > 0 {
            self.clip_ignore -= 1;
            offer.destroy();
            return;
        }
        let mimes = offer.data::<OfferMimes>().map(|m| m.0.lock().unwrap().clone()).unwrap_or_default();
        let Some(mime) = crate::clipboard::pick_mime(&mimes) else {
            offer.destroy();
            return;
        };
        let Ok((mut reader, writer)) = std::io::pipe() else { return };
        {
            use std::os::fd::AsFd;
            offer.receive(mime.clone(), writer.as_fd());
        }
        drop(writer);
        let _ = self.conn.flush();
        offer.destroy();
        let net = self.net.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut data = Vec::new();
            if reader.by_ref().take(crate::clipboard::MAX_BYTES as u64).read_to_end(&mut data).is_err() {
                return;
            }
            let items = crate::clipboard::items_from(&mime, data);
            if !items.is_empty() {
                log::info!("clipboard to Mac: {}", mime);
                let _ = net.send(NetCmd::Clipboard(items));
            }
        });
    }

    /// Starts drawing the screenshot selection at (x, y) in a window (see Selection).
    fn start_selection(&mut self, wid: u32, x: f64, y: f64, qh: &QueueHandle<Self>) {
        self.end_selection();
        if self.selection_bufs.is_none() {
            let Ok(sp) = self.single_pixel.get() else { return };
            let v = |f: f64| (f * u32::MAX as f64) as u32;
            // Premultiplied: a gray veil, and an edge that shows on light and dark content alike.
            let fill = sp.create_u32_rgba_buffer(v(0.12), v(0.12), v(0.12), v(0.25), qh, ());
            let edge = sp.create_u32_rgba_buffer(v(0.35), v(0.35), v(0.35), v(0.9), qh, ());
            self.selection_bufs = Some((fill, edge));
        }
        let Some((fill, edge)) = self.selection_bufs.clone() else { return };
        let Some(w) = self.windows.get(&wid) else { return };
        let (base, video) = (w.base().clone(), w.video.clone());
        let viewporter = self.viewporter.get().expect("wp_viewporter").clone();
        let parts = (0..5)
            .map(|i| {
                let (sub, surf) = self.subcompositor.create_subsurface(base.clone(), qh);
                sub.set_desync();
                sub.place_above(&video);
                let vp = viewporter.get_viewport(&surf, qh, ());
                // Purely visual: the drag goes on to the Mac.
                let empty = self.compositor.wl_compositor().create_region(qh, ());
                surf.set_input_region(Some(&empty));
                empty.destroy();
                surf.attach(Some(if i == 0 { &fill } else { &edge }), 0, 0);
                (sub, surf, vp)
            })
            .collect();
        self.selection = Some(Selection { wid, start: (x, y), parts });
        self.draw_selection(x, y);
    }

    /// Draws the selection from where the drag started to (x, y).
    fn draw_selection(&mut self, x: f64, y: f64) {
        let Some(sel) = &self.selection else { return };
        let Some(w) = self.windows.get(&sel.wid) else { return };
        let (x0, y0) = (sel.start.0.min(x).round() as i32, sel.start.1.min(y).round() as i32);
        let (x1, y1) = (sel.start.0.max(x).round() as i32, sel.start.1.max(y).round() as i32);
        let (sw, sh) = ((x1 - x0).max(1), (y1 - y0).max(1));
        let (x1, y1) = (x0 + sw, y0 + sh);
        let rects = [(x0, y0, sw, sh), (x0, y0, sw, 1), (x0, y1 - 1, sw, 1), (x0, y0, 1, sh), (x1 - 1, y0, 1, sh)];
        for ((sub, surf, vp), (rx, ry, rw, rh)) in sel.parts.iter().zip(rects) {
            sub.set_position(rx, ry);
            vp.set_destination(rw, rh);
            surf.damage_buffer(0, 0, 1, 1);
            surf.commit();
        }
        // Subsurface position changes apply on the parent's next commit.
        w.base().commit();
    }

    fn end_selection(&mut self) {
        let Some(sel) = self.selection.take() else { return };
        for (sub, surf, vp) in sel.parts {
            vp.destroy();
            sub.destroy();
            surf.destroy();
        }
        if let Some(w) = self.windows.get(&sel.wid) {
            w.base().commit();
        }
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

    /// Closes a Mac window. With the Mac away (or streaming paused) nothing would answer, so the
    /// window goes now (and the Mac's is closed once it's back). Otherwise it's hidden at once
    /// (see hide): the Mac's own close takes a round trip and a moment of the app's.
    fn close(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        if self.connected && !self.paused {
            self.send(W::new(t::CLOSE).u32(wid).finish());
            self.hide(wid, qh);
        } else {
            self.closed_offline.insert(wid);
            self.remove_window(wid);
        }
    }

    /// How long a window asked to close stays hidden with no word from the Mac before it shows
    /// again: its app didn't close it (asked something the Mac didn't show as a window, or is
    /// slow to), and a window that stays must be seen.
    const HIDE_FOR: Duration = Duration::from_millis(1500);

    /// A window asked to close goes from the screen at once: its picture, lights and corners
    /// are unmapped and it takes no input (what's behind it does), while the window itself, and
    /// its taskbar entry, stay until the Mac says it's gone (remove_window). If instead its app
    /// shows another window (a "Save changes?" sheet: add_window), or nothing has happened after
    /// HIDE_FOR, it shows again where it was (unhide). That's why it isn't unmapped: mapping a
    /// toplevel again takes a new configure cycle and, on most desktops, gives it a new place.
    fn hide(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        self.unfreeze(wid);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.hidden.is_some() || !w.configured || w.display || !matches!(w.role, Role::Toplevel(_)) {
            return;
        }
        let since = Instant::now();
        w.hidden = Some(since);
        // The picture shown waits for the comeback (a newer one replaces it meanwhile: present).
        if let Some(frame) = w.shown.take().and_then(|id| self.in_use.remove(&id)) {
            w.pending = Some(frame);
        }
        // The video goes, and with it the corner strips and tiles (its children; on Hyprland the
        // lights too); elsewhere the lights hang off the base surface.
        w.video.attach(None, 0, 0);
        w.video.commit();
        if let Some(l) = w.lights.as_mut() {
            l.surface.attach(None, 0, 0);
            l.surface.commit();
            l.drawn = None;
        }
        self.clip_input(wid, qh);
        if let Some(w) = self.windows.get(&wid) {
            w.base().commit();
        }
        log::info!("window {wid}: closing: hidden until the Mac confirms");
        let qh = qh.clone();
        let _ = self.loop_handle.insert_source(Timer::from_duration(Self::HIDE_FOR), move |_, _, app| {
            if app.windows.get(&wid).is_some_and(|w| w.hidden == Some(since)) {
                app.unhide(wid, "its app hasn't closed it", &qh);
            }
            TimeoutAction::Drop
        });
    }

    /// A window hidden for closing (hide) shows again, where it was, with its latest picture.
    fn unhide(&mut self, wid: u32, why: &str, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if w.hidden.take().is_none() {
            return;
        }
        log::info!("window {wid}: back: {why}");
        self.clip_input(wid, qh);
        let pending = self.windows.get_mut(&wid).and_then(|w| w.pending.take());
        match pending {
            Some(frame) => self.present(wid, frame, qh),
            None => self.relayout(wid, qh),
        }
        self.update_lights(wid, qh);
        if let Some(w) = self.windows.get(&wid) {
            w.base().commit();
        }
    }

    /// In a moment, removes the windows the Mac hasn't announced again (marked stale), unless
    /// streaming is paused (they stay as stills).
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

    fn freeze_all(&mut self, qh: &QueueHandle<Self>) {
        let wids: Vec<u32> = self.windows.iter().filter(|(_, w)| matches!(w.role, Role::Toplevel(_))).map(|(k, _)| *k).collect();
        for wid in wids {
            self.freeze(wid, qh);
        }
    }

    /// Shows the window's last picture in black and white with a pause sign, over its video.
    fn freeze(&mut self, wid: u32, qh: &QueueHandle<Self>) {
        let Some(w) = self.windows.get(&wid) else { return };
        if w.still.is_some() || !matches!(w.role, Role::Toplevel(_)) {
            return;
        }
        let Some(frame) = w.shown.as_ref().and_then(|id| self.in_use.get(id)) else { return };
        let Some(mut px) = frame.grey() else {
            return log::warn!("window {wid}: couldn't read its picture back for the paused still");
        };
        let (fw, fh) = (frame.width, frame.height);
        // Where the picture sits, as the video's (see present), and which of its corners are
        // the window's own (lay_out).
        let mut rounded = [true; 4];
        let (pos, src, dst) = if w.display {
            let (k, ox, oy) = w.fit;
            ((ox.round() as i32, oy.round() as i32), (frame.content.0 as f64, frame.content.1 as f64),
             ((w.info.w as f64 * k).round().max(1.0) as i32, (w.info.h as f64 * k).round().max(1.0) as i32))
        } else {
            let s = self.scale;
            let ppp = px_per_pt(frame.px_per_pt, frame.content.0, w.info.w, s);
            let (src, dst, (cut_r, cut_b)) = shown_part(frame.content, ppp, w.size, s);
            rounded = [true, !cut_r, !cut_b, !cut_r && !cut_b];
            ((0, 0), src, dst)
        };
        let (sw, sh) = (src.0.max(1.0) as usize, src.1.max(1.0) as usize);
        // Rounded corners: the alpha of the Mac's own corner tiles.
        if let Some((t, tiles)) = w.corners.as_ref().and_then(|c| c.last_tiles.as_ref()) {
            let t = *t as usize;
            if sw > 2 * t && sh > 2 * t {
                let n = t * t * 4;
                for (i, (ox, oy)) in [(0, 0), (sw - t, 0), (0, sh - t), (sw - t, sh - t)].into_iter().enumerate().filter(|(i, _)| rounded[*i]) {
                    for y in 0..t {
                        for x in 0..t {
                            let a = tiles[i * n + (y * t + x) * 4 + 3] as u32;
                            let p = &mut px[((oy + y) * fw as usize + ox + x) * 4..][..4];
                            for c in &mut p[..3] {
                                *c = (*c as u32 * a / 255) as u8;
                            }
                            p[3] = a as u8;
                        }
                    }
                }
            }
        }
        draw_pause(&mut px, fw as usize, sw as f64 / 2.0, sh as f64 / 2.0, (sw.min(sh) as f64 * 0.11).clamp(24.0, 96.0));
        let Ok(mut pool) = RawPool::new(px.len(), &self.shm) else { return };
        pool.mmap().copy_from_slice(&px);
        let buffer = pool.create_buffer(0, fw as i32, fh as i32, (fw * 4) as i32, wl_shm::Format::Argb8888, (), qh);
        let base = w.base().clone();
        let (sub, surface) = self.subcompositor.create_subsurface(base.clone(), qh);
        let viewport = self.viewporter.get().expect("wp_viewporter").get_viewport(&surface, qh, ());
        let empty = self.compositor.wl_compositor().create_region(qh, ());
        surface.set_input_region(Some(&empty));
        empty.destroy();
        sub.set_position(pos.0, pos.1);
        viewport.set_source(0.0, 0.0, src.0, src.1);
        viewport.set_destination(dst.0, dst.1);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, fw as i32, fh as i32);
        surface.commit();
        // Our traffic lights stay on top (closing a paused window works). On Hyprland they
        // hang off the video and are drawn above this anyway (see update_lights).
        if let Some(l) = w.lights.as_ref().filter(|_| !crate::hyprland::running()) {
            l.sub.place_above(&surface);
        }
        base.commit();
        log::info!("window {wid}: paused still shown");
        if let Some(w) = self.windows.get_mut(&wid) {
            w.still = Some(Still { sub, surface, viewport, buffer, _pool: pool });
        }
    }

    fn unfreeze(&mut self, wid: u32) {
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let Some(st) = w.still.take() else { return };
        st.viewport.destroy();
        st.sub.destroy();
        st.surface.destroy();
        st.buffer.destroy();
        w.base().commit();
    }

    /// The desktop's shortcuts go to the Mac (Super as Cmd) only while a Mac window has the
    /// keyboard and the pointer is over a Mac window: the shortcuts inhibitor is held on the
    /// focused one then, and dropped when the pointer leaves (the desktop's own keys work again
    /// at once), or for a moment after a window key was handed to the desktop (let_go).
    fn sync_inhibitor(&mut self, qh: &QueueHandle<Self>) {
        let held = self.meta_down > 0 || self.mods.alt || self.mods.ctrl;
        if let Some(t) = self.let_go {
            if Instant::now() >= t && !held {
                self.let_go = None;
            }
        }
        let target = match (self.kb_focus, self.pointer_focus) {
            (Some(f), Some(p)) if self.let_go.is_none() && self.windows.contains_key(&p) => Some(f),
            _ => None,
        };
        if crate::hyprland::running() {
            // Its own way there (hyprland::mac_keys): no inhibitor.
            if target.is_some() != self.hypr_mac_keys {
                self.hypr_mac_keys = target.is_some();
                crate::hyprland::mac_keys(self.hypr_mac_keys);
            }
            return;
        }
        let (Some(m), Some(seat)) = (self.inhibit_mgr.clone(), self.seat.clone()) else { return };
        for (wid, w) in self.windows.iter_mut() {
            let want = Some(*wid) == target && matches!(w.role, Role::Toplevel(_)) && !w.display;
            if want && w.inhibitor.is_none() {
                w.inhibitor = Some(m.inhibit_shortcuts(w.base(), &seat, qh, ()));
            } else if !want {
                if let Some(i) = w.inhibitor.take() {
                    i.destroy();
                }
            }
        }
    }

    fn inhibiting(&self) -> bool {
        self.windows.values().any(|w| w.inhibitor.is_some())
    }

    /// Hands the desktop its keys back for a moment (and while a modifier stays held).
    fn let_go_now(&mut self, qh: &QueueHandle<Self>) {
        self.let_go = Some(Instant::now() + Duration::from_millis(1500));
        self.sync_inhibitor(qh);
    }

    /// A launch of an app that already has windows here: its newest one (a minimised one first)
    /// back in front. Wayland lets only the compositor do that, given an activation token: the
    /// launcher's, or one asked for now (which a compositor may only take as a hint).
    fn raise_app(&mut self, bundle_id: &str, token: Option<String>, qh: &QueueHandle<Self>) {
        let Some(activation) = &self.activation else { return };
        let Some(w) = self
            .windows
            .values()
            .filter(|w| w.info.bundle_id == bundle_id && matches!(w.role, Role::Toplevel(_)))
            .max_by_key(|w| (w.mac_minimized, w.info.wid))
        else {
            if let Some(t) = token {
                self.launch_tokens.insert(bundle_id.to_string(), (t, std::time::Instant::now()));
            }
            return;
        };
        let Role::Toplevel(t) = &w.role else { return };
        let surface = t.wl_surface().clone();
        log::info!("launch of {bundle_id}: bringing window {} to the front ({})", w.info.wid,
                   if token.is_some() { "launcher's token" } else { "own token" });
        match token {
            Some(t) => activation.activate(t, &surface),
            None => {
                let req = activation.get_activation_token(qh, surface.clone());
                req.set_app_id(format!("sweeflow-{bundle_id}"));
                if let Some(seat) = self.seat_state.seats().next() {
                    if self.kb_serial != 0 {
                        req.set_serial(self.kb_serial, &seat);
                    }
                }
                req.commit();
            }
        }
    }

    fn toggle_maximize(&self, wid: u32) {
        let Some(w) = self.windows.get(&wid) else { return };
        let Role::Toplevel(t) = &w.role else { return };
        // Only a floating window is maximised (and a maximised one put back): one the desktop
        // has tiled or put in a scrolling row stays in its place.
        // (Hyprland's tiled windows are known by `tiled`; elsewhere the window's own state
        // says it, and a maximised one isn't "floating" by that state.)
        if w.tiled || !(w.floating || w.maximized) {
            return;
        }
        // Hyprland is asked directly: Omarchy has it ignore a window's own request.
        if crate::hyprland::toggle_maximise() {
            return;
        }
        if w.maximized {
            t.unset_maximized();
        } else {
            t.set_maximized();
        }
    }

    /// Resize edge under (x, y) for windows without a KDE frame.
    /// Window position (logical px) → Mac window points. The same, except in the display view,
    /// whose picture is scaled and letterboxed.
    fn to_mac(&self, wid: u32, x: f64, y: f64) -> (f32, f32) {
        match self.windows.get(&wid) {
            Some(w) if w.display => {
                let (k, ox, oy) = w.fit;
                (((x - ox) / k).clamp(0.0, w.info.w as f64 - 1.0) as f32, ((y - oy) / k).clamp(0.0, w.info.h as f64 - 1.0) as f32)
            }
            _ => (x as f32, y as f32),
        }
    }

    fn edge_at(&self, wid: u32, x: f64, y: f64) -> Option<xdg_toplevel::ResizeEdge> {
        use xdg_toplevel::ResizeEdge as E;
        let w = self.windows.get(&wid)?;
        // The display view has a KDE frame to resize by.
        if !matches!(w.role, Role::Toplevel(_)) || w.maximized || w.display {
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
        let pointer_over = self.pointer_focus == Some(wid);
        let Some(w) = self.windows.get_mut(&wid) else { return };
        if !matches!(w.role, Role::Toplevel(_)) || w.hidden.is_some() {
            return;
        }
        // A video player (floated because it keeps its shape): shown only while the pointer is
        // over the window, as macOS shows a player's controls, so the video fills it otherwise.
        let video = w.floated && !w.fixed && w.info.kind == 0;
        let Some(rect) = lights::group_rect(&w.buttons).filter(|_| !video || pointer_over) else {
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
        // Hyprland draws a window's surfaces level by level, so the strips and corner tiles
        // (children of the video) land on top of anything that is the video's sibling: the top
        // of the lights was cut off wherever they reach into the top strip. There the lights are
        // a child of the video too, the last one, and placed relative to it.
        let nested = crate::hyprland::running();
        let video_y = if nested { w.corners.as_ref().map_or(0, |c| c.middle_y) } else { 0 };
        if w.lights.as_ref().and_then(|l| l.drawn) == Some((state, rect, video_y)) {
            return;
        }
        if w.lights.is_none() {
            let parent = if nested { w.video.clone() } else { w.base().clone() };
            let (sub, surface) = self.subcompositor.create_subsurface(parent, qh);
            sub.set_desync();
            if !nested {
                sub.place_above(&w.video);
            }
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
        l.sub.set_position((rect[0] * s).round() as i32, (rect[1] * s).round() as i32 - video_y);
        l.viewport.set_destination(((rect[2] * s).round() as i32).max(1), ((rect[3] * s).round() as i32).max(1));
        l.surface.attach(Some(&buf), 0, 0);
        l.surface.damage_buffer(0, 0, pw as i32, ph as i32);
        if self.trace {
            if let (Some(p), Some(ev)) = (&self.presentation, self.trace_event_ms.take()) {
                p.feedback(&l.surface, qh, (ev, monotonic_ms()));
            }
        }
        l.surface.commit();
        l.drawn = Some((state, rect, video_y));
        l.buffers.push_back((buf, pool));
        while l.buffers.len() > 3 {
            if let Some((b, _)) = l.buffers.pop_front() {
                b.destroy();
            }
        }
        // Subsurface position changes apply on the parent's next commit.
        if nested {
            w.video.commit();
        }
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

    /// Whether the pointer is over the traffic lights we draw (see lights_pointer_at).
    fn over_lights(&self, wid: u32) -> bool {
        self.windows.get(&wid).is_some_and(|w| w.lights_hover)
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

    /// The keyboard focus is on a Mac app's own window: not a popup, nor the Mac's display
    /// (no one app's, so it has no menu of its own).
    fn app_window_focused(&self) -> bool {
        self.kb_focus
            .and_then(|wid| self.windows.get(&wid))
            .is_some_and(|w| w.info.kind != 3 && matches!(w.role, Role::Toplevel(_)))
    }
}

/// Pixels per point of a frame: as reported by the Mac, else inferred from the window width.
pub(crate) fn px_per_pt(reported: u8, frame_w: u32, window_w_pt: f32, fallback: f64) -> f64 {
    if reported > 0 {
        reported as f64
    } else if window_w_pt > 0.0 {
        (frame_w as f64 / window_w_pt as f64).round().max(1.0)
    } else {
        fallback
    }
}

/// The part of a picture that shows in its window (frame px, from its top-left), the size it is
/// drawn at (logical px), and whether its right and bottom edges are cut off there (the picture
/// runs past the window by more than a sliver). `content`: the picture in the frame; `s`:
/// logical px per Mac point.
pub(crate) fn shown_part(content: (u32, u32), px_per_pt: f64, window: (u32, u32), s: f64) -> ((f64, f64), (i32, i32), (bool, bool)) {
    let (pw, ph) = (content.0 as f64 / px_per_pt, content.1 as f64 / px_per_pt);
    let (ww, wh) = (window.0 as f64 / s, window.1 as f64 / s);
    let (vis_w, vis_h) = (pw.min(ww), ph.min(wh));
    ((vis_w * px_per_pt, vis_h * px_per_pt), ((vis_w * s).round().max(1.0) as i32, (vis_h * s).round().max(1.0) as i32), (pw > ww + SLIVER, ph > wh + SLIVER))
}

/// How far (Mac points) a picture may run past its window and still count as ending there: an
/// app that sizes itself in its own steps (Terminal's character cells) lands a point or two off
/// the size asked, and its corners stay rounded.
const SLIVER: f64 = 2.0;

/// GNOME asks "Allow inhibiting shortcuts?" the first time each app's window wants the
/// keyboard's shortcuts, and every Mac app is an app of its own there: one prompt per Mac app.
/// The answer is kept in the permission store (what its Allow button writes), so Sweeflow writes
/// it for its own Mac windows before asking. Once per app per run; only on GNOME.
fn gnome_allow_shortcuts(bundle_id: &str) {
    static DONE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    if !std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').any(|d| d.eq_ignore_ascii_case("GNOME")) {
        return;
    }
    {
        let mut done = DONE.lock().unwrap();
        if done.iter().any(|b| b == bundle_id) {
            return;
        }
        done.push(bundle_id.to_string());
    }
    let app = format!("sweeflow-{bundle_id}");
    // Before the window asks (a few ms, once per app): on a thread it lost the race.
    {
        static BUS: std::sync::OnceLock<Option<zbus::blocking::Connection>> = std::sync::OnceLock::new();
        let Some(bus) = BUS.get_or_init(|| zbus::blocking::Connection::session().ok()) else { return };
        // GNOME looks it up by the desktop entry it matched the window to: the app menu's
        // entry (sweeflow-menu-<bundle id>, launchers.rs), which claims the app's windows too and
        // is the one GNOME picks, or the window's own; the bare app id covers a window it
        // couldn't match to one.
        for id in [format!("sweeflow-menu-{bundle_id}.desktop"), format!("{app}.desktop"), app.clone()] {
            let r = bus.call_method(
                Some("org.freedesktop.impl.portal.PermissionStore"),
                "/org/freedesktop/impl/portal/PermissionStore",
                Some("org.freedesktop.impl.portal.PermissionStore"),
                "SetPermission",
                &("gnome", true, "shortcuts-inhibitor", id.as_str(), vec!["GRANTED"]),
            );
            if let Err(e) = r {
                log::debug!("permission store: {e}");
            }
        }
    }
}

/// Smallest a Mac window gets here, unless its app is known to need more.
pub(crate) const MIN_SIZE: (u32, u32) = (100, 60);
/// How long a Hyprland tile's size must stand before the Mac is asked for it (see arm_settle).
const SETTLE: Duration = Duration::from_millis(120);
const BTN_LEFT: u32 = 0x110;
const KEY_TAB: u32 = 15;
const KEY_SPACE: u32 = 57;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;

/// Shows or auto-hides the Plasma panel holding the Global Menu (found by its widget). "Dodge
/// windows" rather than "always visible" so it never reserves space: toggling it must not make
/// the tiling layout jump.
pub(crate) fn set_menu_panel_visible(visible: bool) {
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
pub(crate) fn switch_windows(reverse: bool) {
    let name = if reverse { "Walk Through Windows (Reverse)" } else { "Walk Through Windows" };
    std::thread::spawn(move || {
        let _ = std::process::Command::new("qdbus6")
            .args(["org.kde.kglobalaccel", "/component/kwin", "invokeShortcut", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

/// Cmd+Space: Linux's search (KRunner), not the Mac's Spotlight, wherever the focus is.
pub(crate) fn open_search() {
    std::thread::spawn(|| {
        // Plasma's KRunner; Cinnamon's menu (its search field has the keyboard). GNOME lets no
        // app open its search (org.gnome.Shell.FocusSearch: access denied).
        let mut cmd = if crate::keys::desktop().contains("Cinnamon") {
            let mut c = std::process::Command::new("busctl");
            c.args(["--user", "call", "org.Cinnamon", "/org/Cinnamon", "org.Cinnamon", "Eval", "s",
                    r#"imports.ui.appletManager.getRunningInstancesForUuid("menu@cinnamon.org")[0].openMenu()"#]);
            c
        } else {
            let mut c = std::process::Command::new("qdbus6");
            c.args(["org.kde.krunner", "/App", "org.kde.krunner.App.toggleDisplay"]);
            c
        };
        let _ = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    });
}

/// Cmd+Opt+U (Super+Alt+U here), with no other modifier: the key for the Global menu (M
/// logs out on Omarchy, G is taken there). The letter the layout puts on the key, so it's the
/// key marked U on any Latin layout.
fn is_global_menu_key(mods: &Modifiers, key: Keysym) -> bool {
    mods.logo && mods.alt && !mods.ctrl && !mods.shift && matches!(key, Keysym::u | Keysym::U)
}

/// Cmd+Opt+O (Super+Alt+O), with no other modifier: opens Sweeflow itself (its window, or on
/// Plasma its tray popup), as `sweeflow open` does.
fn is_open_sweeflow_key(mods: &Modifiers, key: Keysym) -> bool {
    mods.logo && mods.alt && !mods.ctrl && !mods.shift && matches!(key, Keysym::o | Keysym::O)
}

/// Cmd+Opt+Y (pause or resume) and Cmd+Opt+Z (stop, or start when stopped), with no other
/// modifier: the tray's toggle for it (tray::panel_do), as the desktop's shortcuts run it.
fn stream_toggle_key(mods: &Modifiers, key: Keysym) -> Option<&'static str> {
    if !(mods.logo && mods.alt && !mods.ctrl && !mods.shift) {
        return None;
    }
    match key {
        Keysym::y | Keysym::Y => Some("pause-toggle"),
        Keysym::z | Keysym::Z => Some("stream-toggle"),
        _ => None,
    }
}

/// Opens the Global menu (`sweeflow menu`, palette.rs): a process of its own, as from the tray.
pub(crate) fn open_global_menu() {
    run_own("menu", "the Global Menu");
}

/// Runs this program with one argument (`menu`, `open`): started off the UI thread, and waited
/// for there, so it doesn't stay behind as a dead process.
pub(crate) fn run_own(arg: &'static str, what: &'static str) {
    std::thread::spawn(move || {
        let started = std::env::current_exe().and_then(|me| std::process::Command::new(me).arg(arg).spawn());
        match started {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(e) => log::warn!("couldn't open {what}: {e}"),
        }
    });
}

/// Diagnostics: writes premultiplied BGRA as an RGBA PNG.
pub(crate) fn dump_bgra(path: &str, w: u32, h: u32, bgra: &[u8]) {
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

pub(crate) fn display_title(app: &str, title: &str) -> String {
    if title.is_empty() || title == app {
        app.to_string()
    } else {
        format!("{title} — {app}")
    }
}

pub(crate) fn decode_png_argb(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
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

/// The four corner tiles (TL, TR, BL, BR; premultiplied BGRA, `t` pixels square each) made
/// opaque: what the rounded corner leaves see-through takes the colour its row has at the
/// tile's inner edge, so the window ends in square corners that continue its own edge.
pub(crate) fn square_tiles(t: usize, mut px: Vec<u8>) -> Vec<u8> {
    for (i, tile) in px.chunks_exact_mut(t * t * 4).take(4).enumerate() {
        // Left tiles meet the window at their right edge, right tiles at their left.
        let inner = if i % 2 == 0 { t - 1 } else { 0 };
        for row in tile.chunks_exact_mut(t * 4) {
            let fill = [row[inner * 4], row[inner * 4 + 1], row[inner * 4 + 2]];
            for p in row.chunks_exact_mut(4) {
                let rest = 255 - p[3] as u32;
                for (c, f) in p[..3].iter_mut().zip(fill) {
                    *c = (*c as u32 + f as u32 * rest / 255).min(255) as u8;
                }
                p[3] = 255;
            }
        }
    }
    px
}

/// A pause sign (a dark disc with two white bars) drawn into an opaque BGRA picture.
/// The picture a drag out of a Mac window carries (both frontends): a document in Sweeflow's
/// violet with the number of files on it when there are several. `scale` px per logical px;
/// 48 × 56 logical px, BGRA premultiplied.
pub(crate) fn drag_picture_pixels(scale: i32, count: usize) -> (i32, i32, Vec<u8>) {
        let (w, h) = (48 * scale, 56 * scale);
        let mut px = vec![0u8; (w * h * 4) as usize];
        let fold = 14.0 * scale as f64;
        let (violet, white) = ([0xf6u8, 0x5c, 0x8b], [0xffu8, 0xff, 0xff]);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
                let m = 2.0 * scale as f64;
                let inside = fx > m && fy > m && fx < w as f64 - m && fy < h as f64 - m && !(fx > w as f64 - m - fold && fy < m + fold && (fx - (w as f64 - m - fold)) > (fy - m));
                if !inside {
                    continue;
                }
                let edge = fx < m + 2.0 * scale as f64 || fy < m + 2.0 * scale as f64 || fx > w as f64 - m - 2.0 * scale as f64 || fy > h as f64 - m - 2.0 * scale as f64;
                let c = if edge { violet } else { white };
                // BGRA, premultiplied (opaque here).
                px[((y * w + x) * 4) as usize..][..4].copy_from_slice(&[c[0], c[1], c[2], 0xff]);
            }
        }
        // The number of files: a violet disc at the bottom right with white bars for the
        // count (up to 9; more shows as 9).
        if count > 1 {
            let (cx, cy, r) = (w as f64 - 14.0 * scale as f64, h as f64 - 14.0 * scale as f64, 11.0 * scale as f64);
            for y in 0..h {
                for x in 0..w {
                    let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                    if d <= r {
                        px[((y * w + x) * 4) as usize..][..4].copy_from_slice(&[violet[0], violet[1], violet[2], 0xff]);
                    }
                }
            }
            let n = count.min(9) as i32;
            let bar_w = (2 * scale).max(1);
            let total = n * bar_w + (n - 1) * scale;
            let x0 = cx as i32 - total / 2;
            for b in 0..n {
                for y in (cy as i32 - 5 * scale)..(cy as i32 + 5 * scale) {
                    for x in (x0 + b * (bar_w + scale))..(x0 + b * (bar_w + scale) + bar_w) {
                        if x >= 0 && y >= 0 && x < w && y < h {
                            px[((y * w + x) * 4) as usize..][..4].copy_from_slice(&[white[0], white[1], white[2], 0xff]);
                        }
                    }
                }
            }
        }
        (w, h, px)
}

pub(crate) fn draw_pause(px: &mut [u8], stride_px: usize, cx: f64, cy: f64, r: f64) {
    let rows = px.len() / 4 / stride_px.max(1);
    let (bar_w, bar_h, gap) = (r * 0.2, r * 0.9, r * 0.2);
    let x0 = (cx - r - 1.0).max(0.0) as usize;
    let y0 = (cy - r - 1.0).max(0.0) as usize;
    let x1 = ((cx + r + 1.0) as usize).min(stride_px);
    let y1 = ((cy + r + 1.0) as usize).min(rows);
    // Coverage of a rounded rectangle (half sizes hx, hy, corner radius rr) at (dx, dy).
    let rect = |dx: f64, dy: f64, hx: f64, hy: f64, rr: f64| {
        let qx = dx.abs() - (hx - rr);
        let qy = dy.abs() - (hy - rr);
        let d = (qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0)) - rr;
        (0.5 - d).clamp(0.0, 1.0)
    };
    for y in y0..y1 {
        for x in x0..x1 {
            let (dx, dy) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
            let disc = (r - dx.hypot(dy) + 0.5).clamp(0.0, 1.0) * 0.6;
            let bars = rect(dx - (gap / 2.0 + bar_w / 2.0), dy, bar_w / 2.0, bar_h / 2.0, bar_w / 2.5)
                .max(rect(dx + (gap / 2.0 + bar_w / 2.0), dy, bar_w / 2.0, bar_h / 2.0, bar_w / 2.5));
            let p = &mut px[(y * stride_px + x) * 4..][..4];
            let a = p[3] as f64 / 255.0;
            for c in &mut p[..3] {
                let v = *c as f64 * (1.0 - disc);
                *c = (v * (1.0 - bars) + 255.0 * a * bars).round() as u8;
            }
        }
    }
}

// --- window management -----------------------------------------------------------------

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, qh: &QueueHandle<Self>, window: &Window) {
        if let Some(wid) = self.wid_for(window.wl_surface()) {
            self.close(wid, qh);
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
        // Within the limits declared (the desktop keeps to them; a Hyprland tile is configured
        // at the tile's size whatever they are, and with them the window keeps the size its
        // app allows, for a window it won't let be resized: see hold_mac_size).
        let size = if w.display {
            size
        } else {
            let max = w.max_size.unwrap_or((u32::MAX, u32::MAX));
            (size.0.clamp(w.min_size.0.min(max.0), max.0), size.1.clamp(w.min_size.1.min(max.1), max.1))
        };
        let first = !w.configured;
        w.configured = true;
        let mut resized = false;
        if w.display {
            // Never resized on the Mac: the picture is scaled into whatever size it's given.
            let k = (size.0 as f64 / w.info.w.max(1.0) as f64).min(size.1 as f64 / w.info.h.max(1.0) as f64);
            // Floating, it keeps the display's proportions: xdg-shell lets a window take less
            // than it's offered, so it settles on the largest size of that shape (and KWin
            // follows while it's dragged). Tiled or maximised, it must fill its space: dark bars.
            let size = if w.floating {
                ((w.info.w as f64 * k).round().max(1.0) as u32, (w.info.h as f64 * k).round().max(1.0) as u32)
            } else {
                size
            };
            w.size = size;
            w.fit = (k, (size.0 as f64 - w.info.w as f64 * k) / 2.0, (size.1 as f64 - w.info.h as f64 * k) / 2.0);
        } else if size != w.size || (first && size != (w.info.w as u32, w.info.h as u32)) {
            w.size = size;
            w.followed = false;
            resized = true;
            if w.in_tile() {
                // A Hyprland tile: asked of the Mac once the size has stood (arm_settle).
                Self::arm_settle(&self.loop_handle, &mut w.settling, wid, qh);
            } else {
                w.requested = Some((size, Instant::now()));
                let _ = self.net.send(NetCmd::Control(W::new(t::RESIZE).u32(wid).f32(size.0 as f32).f32(size.1 as f32).finish()));
                // Check it took (a Mac window that can't follow reports no change).
                Self::arm_check(&self.loop_handle, &mut w.check_pending, wid, qh);
            }
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
        if resized {
            // The picture shown is placed again for the new size at once (what fits of it;
            // the Mac's next frame follows): grown here, a window showed an empty strip until
            // the Mac happened to send one.
            self.relayout(wid, qh);
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
        log::info!("popup {:?}: dismissed by the desktop", self.wid_for(popup.wl_surface()));
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
            let (x, y) = self.to_mac(wid, e.position.0, e.position.1);
            // For the traffic lights (window coordinates).
            let (px, py) = e.position;
            match e.kind {
                PointerEventKind::Enter { serial } => {
                    log::debug!("pointer entered {wid} at {x:.0},{y:.0}");
                    self.pointer_focus = Some(wid);
                    self.sync_inhibitor(qh);
                    self.enter_serial = serial;
                    if self.cursor_surface.is_some() {
                        self.apply_cursor();
                    } else if let Some(mgr) = &self.cursor_mgr {
                        let d = self.cursor_device.get_or_insert_with(|| mgr.get_pointer(pointer, qh, ()));
                        d.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
                    }
                    self.lights_pointer(wid, px, py, qh);
                    if !self.over_lights(wid) {
                        self.send(W::new(t::POINTER_MOVE).u32(wid).f32(x).f32(y).finish());
                    }
                }
                PointerEventKind::Leave { .. } => {
                    log::debug!("pointer left {wid}");
                    if self.pointer_focus == Some(wid) {
                        self.pointer_focus = None;
                    }
                    self.sync_inhibitor(qh);
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.lights_hover = false;
                    }
                    self.update_lights(wid, qh);
                }
                PointerEventKind::Motion { time } => {
                    self.pointer_at = (wid, e.position.0, e.position.1);
                    if self.let_go.is_some() {
                        self.sync_inhibitor(qh);
                    }
                    if self.selection.as_ref().is_some_and(|s| s.wid == wid) {
                        self.draw_selection(e.position.0, e.position.1);
                    }
                    if self.mac_drag.is_some() {
                        self.maybe_take_drag(qh);
                    }
                    let edge = self.edge_at(wid, e.position.0, e.position.1);
                    self.set_edge_cursor(edge, pointer, qh);
                    self.lights_pointer_at(wid, px, py, Some(time), qh);
                    if self.windows.get(&wid).is_some_and(|w| w.lights_pressed.is_some()) {
                        continue; // dragging off a traffic light: not for the Mac
                    }
                    if self.over_lights(wid) {
                        // Drawn and handled here. On the Mac, resting on the green button opens
                        // its tiling menu, which then takes every click until it's dismissed.
                        continue;
                    }
                    if self.move_ready && self.left_down == Some(wid) && self.moved_enough(e.position.0, e.position.1) {
                        self.move_here(wid);
                        continue;
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
                        if let Some(b) = self.button_at(wid, px, py) {
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
                    if button == BTN_LEFT {
                        self.left_down = Some(wid);
                        self.mac_drag = None;
                        self.press_id = self.press_id.wrapping_add(1);
                        self.press_at = e.position;
                        self.pointer_at = (wid, e.position.0, e.position.1);
                        // Known from the hover to be on the bare title bar: the window is
                        // taken hold of right here, on the first motion, with no round trip.
                        self.move_ready = self.spot_drags(wid, x, y);
                        self.press_time = Instant::now();
                        self.press_known = self.move_ready;
                        if self.move_ready {
                            log::debug!("window {wid}: press on a known drag spot at {x:.0},{y:.0}: moves on the first motion");
                        }
                        self.moved_here = None;
                        if self.screen_select {
                            self.start_selection(wid, e.position.0, e.position.1, qh);
                        }
                    }
                    self.send(W::new(t::POINTER_BUTTON).u32(wid).f32(x).f32(y).u8(mac_button(button)).u8(1).u32(self.press_id).finish());
                }
                PointerEventKind::Release { button, .. } => {
                    if self.local_press == Some(button) {
                        self.local_press = None;
                        let pressed = self.windows.get_mut(&wid).and_then(|w| w.lights_pressed.take());
                        if let Some((b, true)) = pressed {
                            // Traffic lights act on the Linux window; on the Mac they would
                            // minimise/fullscreen the window out of the stream.
                            match b {
                                0 => self.close(wid, qh),
                                1 => {
                                    if let Some(Role::Toplevel(t)) = self.windows.get(&wid).map(|w| &w.role) {
                                        t.set_minimized();
                                    }
                                }
                                _ => self.toggle_maximize(wid),
                            }
                        }
                        self.lights_pointer(wid, px, py, qh);
                        continue;
                    }
                    if button == BTN_LEFT {
                        self.left_down = None;
                        self.mac_drag = None;
                        self.move_ready = false;
                        self.end_selection();
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
                        // Fractional notches as they come (high-resolution wheels); the Mac glides.
                        let (nx, ny) = (v120(&horizontal), v120(&vertical));
                        if nx != 0.0 || ny != 0.0 {
                            self.send(
                                W::new(t::SCROLL).u32(wid).f32(x).f32(y).f32(-nx as f32).f32(-ny as f32).u8(0).u8(0).finish(),
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
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.kb_serial = serial;
        if let Some(wid) = self.wid_for(surface) {
            // The Mac's clipboard, waiting for a Mac window to have the keyboard (GNOME).
            if self.data_control.is_none() && self.cp_offers.is_some() && self.cp_source.is_none() {
                self.offer_clipboard(qh);
            }
            log::debug!("keyboard focus → {wid}: focusing it on the Mac");
            self.kb_focus = Some(wid);
            // Remembered past this focus for the Global menu, which takes the focus to open
            // (not the Mac's display: it's no one app's).
            if let Some(info) = self.windows.get(&wid).map(|w| &w.info).filter(|i| i.kind != 3) {
                crate::menus::focused(info.pid, &info.app_name);
            }
            self.send(W::new(t::FOCUS).u32(wid).finish());
            self.update_lights(wid, qh);
        }
        self.sync_inhibitor(qh);
        self.schedule_menu_panel();
    }

    fn leave(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {
        let was = self.kb_focus.take();
        log::debug!("keyboard focus left {was:?}");
        if let Some(wid) = was {
            self.update_lights(wid, qh);
        }
        self.sync_inhibitor(qh);
        self.schedule_menu_panel();
        self.meta_down = 0;
        self.shift_down = 0;
        self.mods = Modifiers::default();
        self.swallowed.clear();
        // Whatever is still held (Cmd and Opt, when Cmd+Opt+U has just opened the Global menu,
        // which takes the focus) is let go on the Mac: it gets no release for keys let go elsewhere.
        self.send(W::new(t::RELEASE_ALL).finish());
    }

    fn press_key(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, e: KeyEvent) {
        match e.raw_code {
            KEY_LEFTMETA | KEY_RIGHTMETA => self.meta_down += 1,
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.shift_down += 1,
            _ => {}
        }
        if is_global_menu_key(&self.mods, e.keysym) && self.app_window_focused() {
            // Cmd+Opt+U: the Global menu, for the app of this window (menus::focused). The desktop
            // never sees the key while a Mac window has focus (they pass every shortcut through
            // to the Mac), so it's caught here, on every desktop alike.
            open_global_menu();
            self.swallowed.push(e.raw_code);
            return;
        }
        if is_open_sweeflow_key(&self.mods, e.keysym) {
            // Cmd+Opt+O: Sweeflow's own window. Caught here for the same reason (on Hyprland its
            // binding gets the key first, see hyprland.rs; elsewhere this is the way while a
            // Mac window has focus).
            run_own("open", "Sweeflow");
            self.swallowed.push(e.raw_code);
            return;
        }
        if let Some(toggle) = stream_toggle_key(&self.mods, e.keysym) {
            // Cmd+Opt+Y / Cmd+Opt+Z, for the same reason. Off this thread: it may wait on the
            // tray's state.
            std::thread::spawn(move || crate::tray::panel_do(toggle, ""));
            self.swallowed.push(e.raw_code);
            return;
        }
        let kde = crate::keys::desktop().contains("KDE");
        if kde && self.inhibiting() && e.raw_code == KEY_TAB && self.meta_down > 0 {
            // Cmd+Tab switches between *all* desktop windows, like it does everywhere else.
            switch_windows(self.shift_down > 0);
            self.swallowed.push(e.raw_code);
            return;
        }
        if kde && self.inhibiting() && e.raw_code == KEY_SPACE && self.meta_down > 0 && self.shift_down == 0 {
            open_search();
            self.swallowed.push(e.raw_code);
            return;
        }
        // Keys for windows and the desktop stay the desktop's (deskkeys): run, or handed back.
        let m = crate::deskkeys::Mods { logo: self.meta_down > 0, alt: self.mods.alt, ctrl: self.mods.ctrl, shift: self.shift_down > 0 };
        if self.inhibiting() && crate::deskkeys::is_desktop_key(e.raw_code, m) {
            let handled = if kde {
                crate::deskkeys::plasma_action(e.raw_code, m).map(|(c, a)| crate::deskkeys::plasma_invoke(&c, &a)).is_some()
            } else if crate::keys::desktop().contains("GNOME") && crate::deskkeys::gnome_uses(e.raw_code, m) {
                // GNOME runs none for an app: let through from the next press (deskkeys).
                self.let_go_now(qh);
                true
            } else {
                false
            };
            if handled {
                log::debug!("key {} with {m:?}: the desktop's", e.raw_code);
                self.swallowed.push(e.raw_code);
                return;
            }
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

    fn release_key(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, e: KeyEvent) {
        match e.raw_code {
            KEY_LEFTMETA | KEY_RIGHTMETA => self.meta_down = self.meta_down.saturating_sub(1),
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.shift_down = self.shift_down.saturating_sub(1),
            _ => {}
        }
        if self.let_go.is_some() {
            self.sync_inhibitor(qh);
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
        mods: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.mods = mods;
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
        if self.clip_device.is_none() {
            if let Some(m) = &self.data_control {
                self.clip_device = Some(m.get_data_device(&seat, qh, ()));
            }
        }
        if self.dnd_device.is_none() {
            if let Some(ddm) = &self.ddm {
                self.dnd_device = Some(ddm.get_data_device(qh, &seat));
            }
        }
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

// --- drag and drop ---------------------------------------------------------------------

impl DataDeviceHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice, x: f64, y: f64, surface: &wl_surface::WlSurface) {
        let Some(offer) = self.dnd_device.as_ref().and_then(|d| d.data().drag_offer()) else { return };
        // Files, images, links or text dragged onto a Mac window (not our own drag coming back).
        let mimes = offer.with_mime_types(|m| m.to_vec());
        let accept = crate::clipboard::drop_mime(&mimes);
        log::info!("drag entered {:?} (surface {:?}, ours: {}) with {mimes:?}", self.wid_for(surface), surface.id(), self.drag_source.is_some());
        match self.wid_for(surface) {
            // A drag out of a Mac window landing on another one: the Mac drops its own files.
            Some(wid) if self.drag_source.is_some() => {
                offer.accept_mime_type(offer.serial, Some("text/uri-list".into()));
                offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
                self.dnd_target = Some((wid, x, y));
            }
            Some(wid) if accept.is_some() => {
                log::info!("drag from Linux entered Mac window {wid} (will drop {accept:?})");
                offer.accept_mime_type(offer.serial, accept);
                offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
                self.dnd_target = Some((wid, x, y));
            }
            other => {
                if other.is_some() {
                    log::info!("drag from Linux refused: nothing usable in {mimes:?}");
                }
                offer.accept_mime_type(offer.serial, None);
                self.dnd_target = None;
            }
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {
        log::info!("drag left {:?}", self.dnd_target.map(|t| t.0));
        self.dnd_target = None;
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice, x: f64, y: f64) {
        if let Some((wid, _, _)) = self.dnd_target {
            self.dnd_target = Some((wid, x, y));
        }
    }

    /// The clipboard as a focused Wayland client sees it (GNOME, no ext-data-control): what
    /// another app copied goes to the Mac.
    fn selection(&mut self, conn: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {
        if self.data_control.is_some() || !crate::tray::allows_clipboard() {
            return;
        }
        let Some(offer) = self.dnd_device.as_ref().and_then(|d| d.data().selection_offer()) else { return };
        if self.clip_ignore > 0 {
            self.clip_ignore -= 1;
            return;
        }
        let mimes = offer.with_mime_types(|m| m.to_vec());
        let Some(mime) = crate::clipboard::pick_mime(&mimes) else { return };
        let Ok(mut pipe) = offer.receive(mime.clone()) else { return };
        let _ = conn.flush();
        let net = self.net.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut data = Vec::new();
            if pipe.by_ref().take(crate::clipboard::MAX_BYTES as u64).read_to_end(&mut data).is_err() {
                return;
            }
            let items = crate::clipboard::items_from(&mime, data);
            if !items.is_empty() {
                log::debug!("clipboard to the Mac: {} item(s) ({mime})", items.len());
                let _ = net.send(NetCmd::Clipboard(items));
            }
        });
    }

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {
        let target = self.dnd_target.take();
        let offer = self.dnd_device.as_ref().and_then(|d| d.data().drag_offer());
        log::info!("drop performed on {:?} (offer: {}, ours: {})", target.map(|t| t.0), offer.is_some(), self.drag_source.is_some());
        let (Some((wid, x, y)), Some(offer)) = (target, offer) else {
            return;
        };
        if !crate::tray::allows_drag() {
            log::info!("drop on Mac window {wid} ignored: drag and drop is switched off");
            offer.destroy();
            return;
        }
        if self.drag_source.is_some() {
            log::info!("Mac drag dropped on Mac window {wid}");
            self.send(W::new(t::DRAG_DROP_LOCAL).u32(wid).f32(x as f32).f32(y as f32).finish());
            offer.finish();
            offer.destroy();
            return;
        }
        // Ask for every representation we might use now (the offer is finished after reading),
        // then decide in the thread which one to send.
        let mimes = offer.with_mime_types(|m| m.to_vec());
        let wanted = crate::clipboard::drop_reads(&mimes);
        let pipes: Vec<(String, _)> =
            wanted.into_iter().filter_map(|m| offer.receive(m.clone()).ok().map(|p| (m, p))).collect();
        if pipes.is_empty() {
            return;
        }
        let net = self.net.clone();
        let done = self.internal.clone();
        let _ = self.conn.flush();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut data = Vec::new();
            for (mime, mut pipe) in pipes {
                let mut buf = Vec::new();
                let _ = pipe.by_ref().take(crate::clipboard::MAX_BYTES as u64).read_to_end(&mut buf);
                data.push((mime, buf));
            }
            let _ = done.send(Internal::DropRead);
            let items = crate::clipboard::drop_items(data);
            if items.is_empty() {
                log::info!("nothing usable in the drop on Mac window {wid} (offered {mimes:?})");
                return;
            }
            log::info!("dropping {:?} on Mac window {wid}", items.iter().map(|i| (&i.kind, &i.name)).collect::<Vec<_>>());
            let mut w = W::new(t::DROP_FILES).u32(wid).f32(x as f32).f32(y as f32).u16(items.len() as u16);
            for it in &items {
                w = w.str(&it.kind).str(&it.name).bytes(&it.data);
            }
            let _ = net.send(NetCmd::ClipStream(w.finish()));
        });
    }
}

impl DataOfferHandler for App {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl DataSourceHandler for App {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource, mime: Option<String>) {
        log::info!("drag out: target accepts {mime:?}");
    }

    /// A Linux app wants the files of a drag out of a Mac window: fetch them from the Mac.
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &wl_data_source::WlDataSource, mime: String, mut fd: WritePipe) {
        // A paste of the Mac's clipboard (GNOME's ordinary clipboard).
        if self.cp_source.as_ref().is_some_and(|s| s.inner() == source) {
            if let Some(data) = self.cp_offers.as_ref().and_then(|o| o.get(&mime)).cloned() {
                std::thread::spawn(move || {
                    use std::io::Write;
                    let _ = fd.write_all(&data);
                });
            }
            return;
        }
        log::info!("drag out: target asks for {mime}");
        if let Some(uris) = self.drag_uris.clone() {
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = fd.write_all(&uris);
            });
            return;
        }
        if self.drag_waiting.is_empty() {
            let _ = self.net.send(NetCmd::ClipStream(W::new(t::DRAG_FETCH).finish()));
        }
        self.drag_waiting.push(fd);
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &wl_data_source::WlDataSource) {
        // Another app copied something: the Mac's clipboard isn't the selection any more.
        if self.cp_source.as_ref().is_some_and(|s| s.inner() == source) {
            self.cp_source = None;
            self.cp_offers = None;
            return;
        }
        log::info!("drag out: cancelled (no target took it)");
        self.drag_source = None;
        self.drag_icon = None;
        self.drag_waiting.clear();
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {
        log::info!("drag out: dropped");
    }

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {
        self.drag_source = None;
        self.drag_icon = None;
    }

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource, a: DndAction) {
        log::info!("drag out: action {a:?}");
    }
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
wayland_client::delegate_noop!(App: ignore XdgWmDialogV1);
wayland_client::delegate_noop!(App: ignore XdgDialogV1);
wayland_client::delegate_noop!(App: ignore XdgToplevelIconManagerV1);
wayland_client::delegate_noop!(App: ignore XdgToplevelIconV1);
wayland_client::delegate_noop!(App: ignore WpCursorShapeManagerV1);
wayland_client::delegate_noop!(App: ignore WpCursorShapeDeviceV1);

wayland_client::delegate_noop!(App: ignore WpPresentation);
wayland_client::delegate_noop!(App: ignore OrgKdeKwinAppmenuManager);
wayland_client::delegate_noop!(App: ignore WpColorManagerV1);
wayland_client::delegate_noop!(App: ignore WpImageDescriptionCreatorParamsV1);
wayland_client::delegate_noop!(App: ignore WpColorManagementSurfaceV1);

impl Dispatch<WpImageDescriptionV1, ()> for App {
    fn event(app: &mut Self, _: &WpImageDescriptionV1, event: wp_image_description_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wp_image_description_v1::Event::Ready { .. } | wp_image_description_v1::Event::Ready2 { .. } => {
                app.srgb_ready = true;
                log::info!("sRGB image description ready");
            }
            wp_image_description_v1::Event::Failed { cause, msg } => log::warn!("sRGB image description failed: {cause:?} {msg}"),
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(App: ignore OrgKdeKwinAppmenu);
wayland_client::delegate_noop!(App: ignore XdgActivationV1);

/// A token asked for ourselves (the launch came without one): used on the window as it arrives.
impl Dispatch<XdgActivationTokenV1, wl_surface::WlSurface> for App {
    fn event(state: &mut Self, token: &XdgActivationTokenV1, e: xdg_activation_token_v1::Event, surface: &wl_surface::WlSurface, _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_activation_token_v1::Event::Done { token: t } = e {
            if let Some(a) = &state.activation {
                a.activate(t, surface);
            }
            token.destroy();
        }
    }
}

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

/// MIME types offered by a clipboard offer.
#[derive(Default)]
pub struct OfferMimes(std::sync::Mutex<Vec<String>>);
/// Our clipboard contents: MIME type → bytes.
pub struct ClipData(HashMap<String, Arc<Vec<u8>>>);

wayland_client::delegate_noop!(App: ignore ExtDataControlManagerV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for App {
    fn event(app: &mut Self, dev: &ExtDataControlDeviceV1, e: ext_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            ext_data_control_device_v1::Event::Selection { id } => app.on_selection(id),
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(o) = id {
                    o.destroy();
                }
            }
            ext_data_control_device_v1::Event::Finished => {
                dev.destroy();
                app.clip_device = None;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferMimes::default()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, OfferMimes> for App {
    fn event(_: &mut Self, _: &ExtDataControlOfferV1, e: ext_data_control_offer_v1::Event, d: &OfferMimes, _: &Connection, _: &QueueHandle<Self>) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = e {
            d.0.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ClipData> for App {
    fn event(app: &mut Self, src: &ExtDataControlSourceV1, e: ext_data_control_source_v1::Event, d: &ClipData, _: &Connection, _: &QueueHandle<Self>) {
        match e {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                if let Some(bytes) = d.0.get(&mime_type).cloned() {
                    // A paste into a slow reader must not stall the UI.
                    std::thread::spawn(move || {
                        use std::io::Write;
                        let _ = std::fs::File::from(fd).write_all(&bytes);
                    });
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                src.destroy();
                if app.clip_source.as_ref() == Some(src) {
                    app.clip_source = None;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for App {
    fn event(_: &mut Self, _: &ZwpKeyboardShortcutsInhibitorV1, e: zwp_keyboard_shortcuts_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        log::debug!("shortcut inhibitor: {e:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_menu_key_is_cmd_opt_u_alone() {
        let mods = |logo, alt, ctrl, shift| Modifiers { logo, alt, ctrl, shift, ..Default::default() };
        assert!(is_global_menu_key(&mods(true, true, false, false), Keysym::u));
        // Caps lock on.
        assert!(is_global_menu_key(&Modifiers { caps_lock: true, ..mods(true, true, false, false) }, Keysym::U));
        assert!(!is_global_menu_key(&mods(true, false, false, false), Keysym::u));
        assert!(!is_global_menu_key(&mods(false, true, false, false), Keysym::u));
        assert!(!is_global_menu_key(&mods(true, true, true, false), Keysym::u));
        assert!(!is_global_menu_key(&mods(true, true, false, true), Keysym::U));
        assert!(!is_global_menu_key(&mods(true, true, false, false), Keysym::m));
    }

    #[test]
    fn a_picture_is_drawn_1_to_1_and_cut_off_at_the_window() {
        // The Mac lags a shrink (or the app goes no smaller): the window shows the part that
        // fits, and the cut edges are straight.
        assert_eq!(shown_part((2048, 1324), 2.0, (781, 468), 1.0), ((1562.0, 936.0), (781, 468), (true, true)));
        // Cut off one way only: the other edge is still the window's own.
        assert_eq!(shown_part((2048, 936), 2.0, (781, 468), 1.0), ((1562.0, 936.0), (781, 468), (true, false)));
        // A picture smaller than the window is shown whole, at its size, with its own corners.
        assert_eq!(shown_part((1148, 936), 2.0, (1148, 1000), 1.0), ((1148.0, 936.0), (574, 468), (false, false)));
        // A point or two over (Terminal's character cells) is still the window's edge.
        assert_eq!(shown_part((1566, 940), 2.0, (781, 468), 1.0), ((1562.0, 936.0), (781, 468), (false, false)));
        // At two logical px per point the window is half as many points.
        assert_eq!(shown_part((1562, 936), 2.0, (1000, 600), 2.0), ((1000.0, 600.0), (1000, 600), (true, true)));
    }
}

