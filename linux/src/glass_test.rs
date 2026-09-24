//! `sweeflow glass-test`: shows glass buffers (as built for glass windows) in a window with
//! KDE's blur behind it, for a few seconds. Meant for a throwaway compositor
//! (`kwin_wayland --virtual`), to check the compositor takes them before trying it for real.

use anyhow::{anyhow, Context, Result};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_region, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_client::protocol::{wl_subcompositor, wl_subsurface};
use wayland_protocols::wp::{
    color_representation::v1::client::{
        wp_color_representation_manager_v1 as crm, wp_color_representation_surface_v1 as crs,
    },
    viewporter::client::{wp_viewport, wp_viewporter},
};
use wayland_protocols::{
    wp::linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1},
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use wayland_protocols_plasma::blur::client::{org_kde_kwin_blur, org_kde_kwin_blur_manager};

#[derive(Default)]
struct State {
    configured: bool,
    released: usize,
}

pub fn run(render_node: &str, clip: Option<&str>) -> Result<()> {
    let conn = Connection::connect_to_env().context("wayland")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ())?;
    let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = globals.bind(&qh, 3..=4, ())?;
    let blur_mgr: Option<org_kde_kwin_blur_manager::OrgKdeKwinBlurManager> = globals.bind(&qh, 1..=1, ()).ok();
    let subc: wl_subcompositor::WlSubcompositor = globals.bind(&qh, 1..=1, ())?;
    let vpr: wp_viewporter::WpViewporter = globals.bind(&qh, 1..=1, ())?;
    let color_mgr: Option<crm::WpColorRepresentationManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    println!("color representation: {}", color_mgr.is_some());

    let surface = compositor.create_surface(&qh, ());
    let xs = wm.get_xdg_surface(&surface, &qh, ());
    let top = xs.get_toplevel(&qh, ());
    top.set_title("sweeflow glass test".into());
    surface.commit();
    let mut state = State::default();
    while !state.configured {
        queue.blocking_dispatch(&mut state)?;
    }
    if let Some(m) = &blur_mgr {
        let b = m.create(&surface, &qh, ());
        b.set_region(None);
        b.commit();
    }

    // As a Mac window: video on subsurfaces (a middle strip and a top strip sharing the
    // buffer, with viewports), tagged BT.709 YUV; then the tags go and glass buffers take over.
    let make_sub = || {
        let s2 = compositor.create_surface(&qh, ());
        let sub = subc.get_subsurface(&s2, &surface, &qh, ());
        sub.set_desync();
        let vp = vpr.get_viewport(&s2, &qh, ());
        (s2, sub, vp)
    };
    let (mid, _msub, mvp) = make_sub();
    let (topstrip, tsub, tvp) = make_sub();
    tsub.set_position(20, 0);
    let mut tags: Vec<crs::WpColorRepresentationSurfaceV1> = color_mgr
        .iter()
        .flat_map(|m| [m.get_surface(&mid, &qh, ()), m.get_surface(&topstrip, &qh, ())])
        .collect();
    for t in &tags {
        t.set_coefficients_and_range(crs::Coefficients::Bt709, crs::Range::Limited);
    }
    surface.commit();

    let gpu = crate::glass::Glass::new(render_node)?;
    // Real decoded frames (VA-API, as in the client) from an Annex B clip with access unit
    // delimiters.
    let clip = std::fs::read(clip.ok_or_else(|| anyhow!("usage: sweeflow glass-test CLIP.hevc"))?)?;
    let mut decoder = crate::decode::Decoder::new(render_node)?;
    let aud = [0u8, 0, 0, 1, 0x46, 0x01];
    let starts: Vec<usize> = (0..clip.len().saturating_sub(6)).filter(|&i| clip[i..i + 6] == aud).collect();
    let mut frames = Vec::new();
    for (k, &st) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(clip.len());
        if let Some(f) = decoder.decode(&clip[st..end], 0)? {
            frames.push(f);
        }
    }
    println!("decoded {} frames", frames.len());
    let f0 = frames.first().ok_or_else(|| anyhow!("no frames decoded"))?;
    let (w, h) = (f0.width, f0.height);
    let nv12_buffer = {
        let params = dmabuf.create_params(&qh, ());
        for (i, p) in f0.planes.iter().enumerate() {
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(p.fd) };
            params.add(fd, i as u32, p.offset, p.stride, (p.modifier >> 32) as u32, p.modifier as u32);
        }
        params.create_immed(w as i32, h as i32, f0.format, zwp_linux_buffer_params_v1::Flags::empty(), &qh, ())
    };
    // A mask like a glass sidebar: left third translucent, the rest opaque.
    let mut mask = None;
    let plane: Vec<u8> = (0..h).flat_map(|_| (0..w).map(move |x| if x < w / 3 { 150 } else { 255 })).collect();
    gpu.upload_mask(&mut mask, w, h, &plane)?;
    let mask = mask.unwrap();
    let mut bufs = Vec::new();
    for _ in 0..3 {
        let out = gpu.new_output(w, h)?;
        let params = dmabuf.create_params(&qh, ());
        for (i, (fd, offset, stride)) in out.planes.iter().enumerate() {
            use std::os::fd::AsFd;
            params.add(fd.as_fd(), i as u32, *offset, *stride, (out.modifier >> 32) as u32, out.modifier as u32);
        }
        let buffer = params.create_immed(w as i32, h as i32, out.fourcc, zwp_linux_buffer_params_v1::Flags::empty(), &qh, ());
        bufs.push((out, buffer));
    }
    let show = |buffer: &wl_buffer::WlBuffer| {
        let (w, h) = (w, h);
        mvp.set_source(0.0, 40.0, w as f64, (h - 40) as f64);
        mvp.set_destination(w as i32, (h - 40) as i32);
        tvp.set_source(20.0, 0.0, (w - 40) as f64, 40.0);
        tvp.set_destination((w - 40) as i32, 40);
        for s2 in [&topstrip, &mid] {
            s2.attach(Some(buffer), 0, 0);
            s2.damage_buffer(0, 0, w as i32, h as i32);
            s2.commit();
        }
    };
    for _ in 0..60 {
        show(&nv12_buffer);
        queue.roundtrip(&mut state).map_err(|e| anyhow!("compositor went away (video): {e}"))?;
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    println!("video phase ok");
    for t in tags.drain(..) {
        t.destroy();
    }
    for frame in 0..180 {
        let (out, buffer) = &bufs[frame % 3];
        gpu.render(&frames[frame % frames.len()], &mask, out)?;
        show(buffer);
        queue.roundtrip(&mut state).map_err(|e| anyhow!("compositor went away (glass): {e}"))?;
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    println!("ok: 180 frames shown, {} buffer releases", state.released);
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, e: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(s: &mut Self, xs: &xdg_surface::XdgSurface, e: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = e {
            xs.ack_configure(serial);
            s.configured = true;
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(s: &mut Self, _: &wl_buffer::WlBuffer, e: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = e {
            s.released += 1;
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(State: ignore wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(State: ignore wp_viewporter::WpViewporter);
wayland_client::delegate_noop!(State: ignore wp_viewport::WpViewport);
wayland_client::delegate_noop!(State: ignore crm::WpColorRepresentationManagerV1);
wayland_client::delegate_noop!(State: ignore crs::WpColorRepresentationSurfaceV1);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(State: ignore xdg_toplevel::XdgToplevel);
wayland_client::delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
wayland_client::delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
wayland_client::delegate_noop!(State: ignore org_kde_kwin_blur_manager::OrgKdeKwinBlurManager);
wayland_client::delegate_noop!(State: ignore org_kde_kwin_blur::OrgKdeKwinBlur);
