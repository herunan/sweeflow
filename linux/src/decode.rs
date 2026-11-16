//! HEVC decoding on the GPU via FFmpeg + VA-API. Decoded surfaces are exported as DRM PRIME
//! dmabufs and handed to the compositor without ever touching the CPU: as they are (YUV) where
//! the compositor shows those right, turned into RGB on the GPU first where it doesn't (see
//! `rgb_output`).

use std::{ffi::CString, os::fd::RawFd, ptr};

use anyhow::{anyhow, bail, Result};
use ffmpeg_next::ffi::*;

// libavutil/hwcontext_drm.h (not covered by ffmpeg-sys bindings).
const AV_DRM_MAX_PLANES: usize = 4;
#[repr(C)]
struct AVDRMObjectDescriptor {
    fd: libc_int,
    size: usize,
    format_modifier: u64,
}
#[repr(C)]
struct AVDRMPlaneDescriptor {
    object_index: libc_int,
    offset: isize,
    pitch: isize,
}
#[repr(C)]
struct AVDRMLayerDescriptor {
    format: u32,
    nb_planes: libc_int,
    planes: [AVDRMPlaneDescriptor; AV_DRM_MAX_PLANES],
}
#[repr(C)]
struct AVDRMFrameDescriptor {
    nb_objects: libc_int,
    objects: [AVDRMObjectDescriptor; AV_DRM_MAX_PLANES],
    nb_layers: libc_int,
    layers: [AVDRMLayerDescriptor; AV_DRM_MAX_PLANES],
}
#[allow(non_camel_case_types)]
type libc_int = std::os::raw::c_int;

pub const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const DRM_FORMAT_P010: u32 = u32::from_le_bytes(*b"P010");
const DRM_FORMAT_R16: u32 = u32::from_le_bytes(*b"R16 ");

const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// RGBA in memory (a CPU picture for the X11 frontend).
const DRM_FORMAT_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");

/// Whether pictures go to the compositor as RGB instead of the decoder's own YUV. Hyprland takes
/// YUV buffers (NV12 and P010 alike) and then draws only their brightness plane, as red: there
/// the GPU converts each picture first. `SWEEFLOW_VIDEO=rgb` or `=yuv` overrides the choice.
pub fn rgb_output() -> bool {
    static RGB: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| match std::env::var("SWEEFLOW_VIDEO").as_deref() {
        Ok("rgb") => true,
        Ok("yuv") => false,
        _ => crate::hyprland::running(),
    });
    *RGB
}

pub struct Plane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

/// A decoded frame living in GPU memory. Keeps the VA surface alive until dropped, so it must
/// outlive the wl_buffer created from it.
pub struct GpuFrame {
    frame: *mut AVFrame,
    /// The decoded VA-API surface itself (another reference): read back for a still (`grey`).
    source: *mut AVFrame,
    pub width: u32,
    pub height: u32,
    /// The part of the frame (from its top-left) that is the window: the Mac keeps its encoder
    /// a size up while a window shrinks (no new encoder and keyframe per resize) and pads.
    pub content: (u32, u32),
    pub format: u32,
    pub planes: Vec<Plane>,
    pub capture_us: u64,
    /// When decoding finished (for the latency figures).
    pub decoded_at: std::time::Instant,
    /// The Mac saw a dark title bar in this frame (picks the inactive traffic-light colour).
    pub dark_titlebar: bool,
    /// Pixels per point (from the Mac); 0 = unknown.
    pub px_per_pt: u8,
    /// Full-range (0–255) YUV rather than limited (16–235), as signalled in the stream.
    pub full_range: bool,
    /// Already RGB (see `rgb_output`): no YUV matrix or range to tell the compositor about.
    pub rgb: bool,
    /// Decoded on the CPU (no HEVC decoding on the GPU): the picture in memory (width × height ×
    /// 4), RGBA for the X11 frontend's textures and BGRA (wl_shm's XRGB8888) for Wayland's
    /// shared memory, and no planes in GPU memory.
    pub cpu_rgba: Option<Vec<u8>>,
}

// The AVFrame is exclusively owned; FFmpeg frames are safe to move between threads.
unsafe impl Send for GpuFrame {}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        unsafe {
            av_frame_free(&mut self.frame);
            av_frame_free(&mut self.source);
        }
    }
}

impl GpuFrame {
    /// The picture in black and white, as opaque BGRA (width × height): read back from the GPU
    /// (VA-API copes with its own tiling). For a paused window's still; not for every frame.
    pub fn grey(&self) -> Option<Vec<u8>> {
        if let Some(rgba) = &self.cpu_rgba {
            return Some(
                rgba.chunks_exact(4)
                    .flat_map(|p| {
                        // BGRA (XRGB8888) or RGBA (XBGR8888).
                        let (r, b) = if self.format == DRM_FORMAT_XRGB8888 { (p[2], p[0]) } else { (p[0], p[2]) };
                        let v = ((r as u32 * 54 + p[1] as u32 * 183 + b as u32 * 19) >> 8) as u8;
                        [v, v, v, 255]
                    })
                    .collect(),
            );
        }
        unsafe {
            let mut sw = av_frame_alloc();
            if sw.is_null() || av_hwframe_transfer_data(sw, self.source, 0) < 0 {
                av_frame_free(&mut sw);
                return None;
            }
            let ten_bit = (*sw).format == AVPixelFormat::AV_PIX_FMT_P010LE as i32;
            let (w, h) = (self.width as usize, (self.height as usize).min((*sw).height as usize));
            let (luma, stride) = ((*sw).data[0], (*sw).linesize[0] as isize);
            let mut out = vec![0u8; self.width as usize * self.height as usize * 4];
            for y in 0..h {
                let row = luma.offset(stride * y as isize);
                for x in 0..w.min((*sw).width as usize) {
                    // P010 keeps its 10 bits at the top of each 16-bit sample.
                    let v = if ten_bit { *row.add(x * 2 + 1) as i32 } else { *row.add(x) as i32 };
                    let v = if self.full_range { v } else { ((v - 16) * 255 / 219).clamp(0, 255) } as u8;
                    out[(y * w + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
                }
            }
            av_frame_free(&mut sw);
            Some(out)
        }
    }
}

/// Turns decoded pictures into RGB ones on the GPU (VA-API's video processing, through FFmpeg's
/// `scale_vaapi`), for compositors that show YUV buffers wrong.
struct ToRgb {
    graph: *mut AVFilterGraph,
    src: *mut AVFilterContext,
    sink: *mut AVFilterContext,
    out: *mut AVFrame,
    /// What it was built for: the decoder's pool of surfaces and their size. A new stream size
    /// brings a new pool, and a new converter.
    made_for: (*const u8, i32, i32),
}

impl ToRgb {
    unsafe fn new(frame: *const AVFrame) -> Result<Self> {
        unsafe {
            let mut last = anyhow!("no RGB format to convert to");
            // Opaque first; with alpha where the driver only has that (it is shown as opaque).
            // Full range: the stream's is the video one (16–235), and left at that the picture
            // comes out washed (black as dark grey).
            for format in [c"format=bgr0:out_range=full", c"format=bgra:out_range=full"] {
                match Self::build(frame, format) {
                    Ok(made) => return Ok(made),
                    Err(e) => last = e,
                }
            }
            Err(last)
        }
    }

    unsafe fn build(frame: *const AVFrame, format: &std::ffi::CStr) -> Result<Self> {
        unsafe {
            let (buffer, scale, buffersink) =
                (avfilter_get_by_name(c"buffer".as_ptr()), avfilter_get_by_name(c"scale_vaapi".as_ptr()), avfilter_get_by_name(c"buffersink".as_ptr()));
            if buffer.is_null() || scale.is_null() || buffersink.is_null() {
                bail!("this FFmpeg has no scale_vaapi filter");
            }
            let mut graph = avfilter_graph_alloc();
            let fail = |graph: &mut *mut AVFilterGraph, what: &str, r: i32| {
                avfilter_graph_free(graph);
                anyhow!("RGB conversion: {what} failed ({r})")
            };
            let src = avfilter_graph_alloc_filter(graph, buffer, c"in".as_ptr());
            let par = av_buffersrc_parameters_alloc();
            (*par).format = AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            (*par).width = (*frame).width;
            (*par).height = (*frame).height;
            (*par).time_base = AVRational { num: 1, den: 1_000_000 };
            (*par).sample_aspect_ratio = AVRational { num: 1, den: 1 };
            (*par).hw_frames_ctx = (*frame).hw_frames_ctx;
            #[cfg(avfilter_colour)]
            {
                (*par).color_space = (*frame).colorspace;
                (*par).color_range = (*frame).color_range;
            }
            let r = av_buffersrc_parameters_set(src, par);
            av_free(par.cast());
            if r < 0 {
                return Err(fail(&mut graph, "the source's parameters", r));
            }
            let r = avfilter_init_str(src, ptr::null());
            if r < 0 {
                return Err(fail(&mut graph, "the source", r));
            }
            let (mut convert, mut sink) = (ptr::null_mut(), ptr::null_mut());
            let r = avfilter_graph_create_filter(&mut convert, scale, c"rgb".as_ptr(), format.as_ptr(), ptr::null_mut(), graph);
            if r < 0 {
                return Err(fail(&mut graph, "scale_vaapi", r));
            }
            // Pictures the compositor still holds, on top of the filter's own few.
            (*convert).extra_hw_frames = 8;
            let r = avfilter_graph_create_filter(&mut sink, buffersink, c"out".as_ptr(), ptr::null(), ptr::null_mut(), graph);
            if r < 0 {
                return Err(fail(&mut graph, "the sink", r));
            }
            let r = match avfilter_link(src, 0, convert, 0) {
                0 => avfilter_link(convert, 0, sink, 0),
                r => r,
            };
            if r < 0 {
                return Err(fail(&mut graph, "linking", r));
            }
            let r = avfilter_graph_config(graph, ptr::null_mut());
            if r < 0 {
                return Err(fail(&mut graph, &format!("setting up {}", format.to_string_lossy()), r));
            }
            log::info!("pictures are converted to RGB on the GPU ({})", format.to_string_lossy());
            Ok(ToRgb { graph, src, sink, out: av_frame_alloc(), made_for: Self::key(frame) })
        }
    }

    unsafe fn key(frame: *const AVFrame) -> (*const u8, i32, i32) {
        unsafe {
            let pool = (*frame).hw_frames_ctx;
            (if pool.is_null() { ptr::null() } else { (*pool).data.cast_const() }, (*frame).width, (*frame).height)
        }
    }

    /// The RGB picture of a decoded one (a VA-API surface too); the caller unrefs it.
    unsafe fn convert(&mut self, frame: *mut AVFrame) -> Result<*mut AVFrame> {
        unsafe {
            let r = av_buffersrc_add_frame_flags(self.src, frame, AV_BUFFERSRC_FLAG_KEEP_REF as i32);
            if r < 0 {
                bail!("RGB conversion: picture not taken ({r})");
            }
            av_frame_unref(self.out);
            let r = av_buffersink_get_frame(self.sink, self.out);
            if r < 0 {
                bail!("RGB conversion: no picture back ({r})");
            }
            Ok(self.out)
        }
    }
}

impl Drop for ToRgb {
    fn drop(&mut self) {
        unsafe {
            av_frame_free(&mut self.out);
            avfilter_graph_free(&mut self.graph);
        }
    }
}

pub struct Decoder {
    ctx: *mut AVCodecContext,
    hw_device: *mut AVBufferRef,
    pkt: *mut AVPacket,
    frame: *mut AVFrame,
    /// The first frame's colour description has been logged.
    logged: bool,
    /// Set up with the first picture, where pictures go out as RGB.
    to_rgb: Option<ToRgb>,
    /// Decoding on the CPU (no VA-API device, or one without HEVC): pictures go out in memory,
    /// converted by this.
    sws: Option<*mut SwsContext>,
    /// CPU pictures may be handed on: also when VA-API opened but FFmpeg decodes on the CPU
    /// after all (a driver without HEVC decoding).
    software_ok: bool,
    /// CPU pictures go out as BGRA (Wayland) rather than RGBA (X11).
    cpu_bgra: bool,
}

unsafe impl Send for Decoder {}

unsafe extern "C" fn pick_vaapi(_: *mut AVCodecContext, mut fmts: *const AVPixelFormat) -> AVPixelFormat {
    // FFmpeg lists the hardware formats first and its own (CPU) format last.
    let mut last = AVPixelFormat::AV_PIX_FMT_NONE;
    unsafe {
        while *fmts != AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmts == AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *fmts;
            }
            last = *fmts;
            fmts = fmts.add(1);
        }
    }
    // No VA-API for this video (the driver can't decode HEVC): the CPU (see `software_ok`).
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| log::warn!("the decoder offers no VA-API output for this video: the CPU's"));
    last
}

/// The bytes per row swscale is given for a CPU picture `w` pixels wide: its SIMD converters write
/// whole blocks of pixels (16 for yuv420p → RGBA), so rows padded to 64 bytes.
fn padded_row(w: i32) -> usize {
    (w as usize * 4 + 63) & !63
}

impl Decoder {
    /// `render_node` is the DRM render node of the GPU the compositor scans out from.
    pub fn new(render_node: &str) -> Result<Self> {
        unsafe {
            let codec = avcodec_find_decoder(AVCodecID::AV_CODEC_ID_HEVC);
            if codec.is_null() {
                bail!("no HEVC decoder in FFmpeg");
            }
            let mut hw_device = ptr::null_mut();
            let dev = CString::new(render_node)?;
            let r = av_hwdevice_ctx_create(&mut hw_device, AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, dev.as_ptr(),
                                           ptr::null_mut(), 0);
            if r < 0 {
                bail!("VA-API device {render_node} unavailable ({r})");
            }
            let ctx = avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = av_buffer_ref(hw_device);
            (*ctx).get_format = Some(pick_vaapi);
            (*ctx).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).flags2 |= AV_CODEC_FLAG2_FAST as i32;
            (*ctx).thread_count = 1;
            // Frames we hold for the compositor, on top of the decoder's reference frames.
            (*ctx).extra_hw_frames = 6;
            let r = avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                bail!("avcodec_open2 failed ({r})");
            }
            Ok(Decoder { ctx, hw_device, pkt: av_packet_alloc(), frame: av_frame_alloc(), logged: false, to_rgb: None, sws: None, software_ok: false, cpu_bgra: false })
        }
    }

    /// VA-API if the GPU can, otherwise FFmpeg's own decoder on the CPU: a GPU without HEVC
    /// decoding (a Mesa built without the patented codecs, NVIDIA's driver, a virtual machine's).
    /// `cpu_bgra`: CPU pictures as BGRA (Wayland's shared memory) rather than RGBA (X11).
    pub fn new_or_software(render_node: &str, cpu_bgra: bool) -> Result<Self> {
        let mut d = match Self::new(render_node) {
            Ok(d) => d,
            Err(e) => {
                static SAID: std::sync::Once = std::sync::Once::new();
                SAID.call_once(|| log::warn!("{e:#}: decoding on the CPU instead"));
                Self::software()?
            }
        };
        d.software_ok = true;
        d.cpu_bgra = cpu_bgra;
        Ok(d)
    }

    fn software() -> Result<Self> {
        unsafe {
            let codec = avcodec_find_decoder(AVCodecID::AV_CODEC_ID_HEVC);
            if codec.is_null() {
                bail!("no HEVC decoder in FFmpeg");
            }
            let ctx = avcodec_alloc_context3(codec);
            (*ctx).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).flags2 |= AV_CODEC_FLAG2_FAST as i32;
            // Slices in parallel, frames not (that would add a frame of delay per thread).
            (*ctx).thread_count = 0;
            (*ctx).thread_type = 2; // FF_THREAD_SLICE
            let r = avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                bail!("avcodec_open2 failed ({r})");
            }
            Ok(Decoder {
                ctx,
                hw_device: ptr::null_mut(),
                pkt: av_packet_alloc(),
                frame: av_frame_alloc(),
                logged: false,
                to_rgb: None,
                sws: Some(ptr::null_mut()),
                software_ok: true,
                cpu_bgra: false,
            })
        }
    }

    /// A CPU-decoded picture as RGBA.
    unsafe fn export_cpu(&mut self, capture_us: u64) -> Result<GpuFrame> {
        unsafe {
            let f = &*self.frame;
            let (w, h) = (f.width, f.height);
            let ctx = sws_getCachedContext(
                self.sws.unwrap_or(ptr::null_mut()),
                w, h, std::mem::transmute::<i32, AVPixelFormat>(f.format),
                w, h, if self.cpu_bgra { AVPixelFormat::AV_PIX_FMT_BGRA } else { AVPixelFormat::AV_PIX_FMT_RGBA },
                2 /* SWS_BILINEAR (a constant or an enum, by FFmpeg version) */, ptr::null_mut(), ptr::null_mut(), ptr::null(),
            );
            if ctx.is_null() {
                bail!("no colour converter for this picture");
            }
            self.sws = Some(ctx);
            // BT.709, in the range the stream signals.
            let full = f.color_range == AVColorRange::AVCOL_RANGE_JPEG;
            let coeffs = sws_getCoefficients(1 /* SWS_CS_ITU709 */);
            sws_setColorspaceDetails(ctx, coeffs, full as i32, sws_getCoefficients(5 /* SWS_CS_DEFAULT */), 1, 0, 1 << 16, 1 << 16);
            // swscale's SIMD converters write past the end of a row whose width isn't a multiple
            // of their block: into a tight buffer, the last row's tail went past its end (16
            // bytes for 780 px), damaging the heap, a crash on every start wherever "light"
            // pictures (8-bit 4:2:0, 1×, so any width) were decoded on the CPU. So padded rows
            // and two spare rows, then the rows packed tight in place.
            let row = w as usize * 4;
            let padded = padded_row(w);
            let mut rgba = vec![0u8; padded * (h as usize + 2)];
            let dst = [rgba.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
            let stride = [padded as i32, 0, 0, 0];
            sws_scale(ctx, f.data.as_ptr() as *const *const u8, f.linesize.as_ptr(), 0, h, dst.as_ptr(), stride.as_ptr());
            if padded != row {
                for y in 1..h as usize {
                    rgba.copy_within(y * padded..y * padded + row, y * row);
                }
            }
            rgba.truncate(row * h as usize);
            if !self.logged {
                self.logged = true;
                log::info!("decoding on the CPU: {}x{} format {} range {:?}", w, h, f.format, f.color_range);
            }
            Ok(GpuFrame {
                decoded_at: std::time::Instant::now(),
                source: ptr::null_mut(),
                width: w as u32,
                height: h as u32,
                content: (w as u32, h as u32),
                frame: ptr::null_mut(),
                format: if self.cpu_bgra { DRM_FORMAT_XRGB8888 } else { DRM_FORMAT_XBGR8888 },
                planes: Vec::new(),
                capture_us,
                dark_titlebar: true,
                px_per_pt: 0,
                full_range: true,
                rgb: true,
                cpu_rgba: Some(rgba),
            })
        }
    }

    /// Decodes one access unit (Annex B). Returns the frame if one is ready.
    pub fn decode(&mut self, data: &[u8], capture_us: u64) -> Result<Option<GpuFrame>> {
        unsafe {
            av_packet_unref(self.pkt);
            if av_new_packet(self.pkt, data.len() as i32) < 0 {
                bail!("av_new_packet");
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let r = avcodec_send_packet(self.ctx, self.pkt);
            if r < 0 {
                bail!("avcodec_send_packet ({r})");
            }
            let mut out = None;
            loop {
                let r = avcodec_receive_frame(self.ctx, self.frame);
                if r == AVERROR(EAGAIN) || r == AVERROR_EOF {
                    break;
                }
                if r < 0 {
                    bail!("avcodec_receive_frame ({r})");
                }
                // Only the newest frame matters (there should only ever be one per packet).
                // VA-API that can't decode HEVC: FFmpeg decodes on the CPU after all.
                let on_cpu = (*self.frame).format != AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
                if on_cpu && self.software_ok && self.sws.is_none() {
                    log::warn!("VA-API can't decode this video here: decoding on the CPU");
                    self.sws = Some(ptr::null_mut());
                }
                let exported = if self.sws.is_some() {
                    self.export_cpu(capture_us)
                } else if rgb_output() {
                    self.export_rgb(capture_us)
                } else {
                    self.export(self.frame, capture_us)
                };
                av_frame_unref(self.frame);
                out = Some(exported?);
            }
            Ok(out)
        }
    }

    /// The decoded picture converted to RGB, then exported like any other.
    unsafe fn export_rgb(&mut self, capture_us: u64) -> Result<GpuFrame> {
        unsafe {
            if self.to_rgb.as_ref().is_none_or(|c| c.made_for != ToRgb::key(self.frame)) {
                self.to_rgb = None;
                self.to_rgb = Some(ToRgb::new(self.frame)?);
            }
            let Some(converter) = self.to_rgb.as_mut() else { bail!("no RGB converter") };
            let rgb = converter.convert(self.frame)?;
            let exported = self.export(rgb, capture_us);
            av_frame_unref(rgb);
            exported
        }
    }

    /// `shown` is the picture to hand over: the decoded one, or its RGB conversion.
    unsafe fn export(&mut self, shown: *mut AVFrame, capture_us: u64) -> Result<GpuFrame> {
        unsafe {
            let mut drm = av_frame_alloc();
            (*drm).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let r = av_hwframe_map(drm, shown, AV_HWFRAME_MAP_READ as i32);
            if r < 0 {
                av_frame_free(&mut drm);
                return Err(anyhow!("av_hwframe_map to DRM PRIME failed ({r})"));
            }
            let desc = &*((*drm).data[0] as *const AVDRMFrameDescriptor);
            // VA-API exports NV12 either as one layer with two planes or as two single-plane
            // layers (R8 + GR88). Either way the planes in order form an NV12 buffer.
            let mut planes = Vec::new();
            for l in &desc.layers[..desc.nb_layers as usize] {
                for p in &l.planes[..l.nb_planes as usize] {
                    let obj = &desc.objects[p.object_index as usize];
                    planes.push(Plane {
                        fd: obj.fd,
                        offset: p.offset as u32,
                        stride: p.pitch as u32,
                        modifier: obj.format_modifier,
                    });
                }
            }
            // Two single-plane layers: R8 + GR88 is NV12, R16 + GR1616 is P010 (10-bit video).
            let format = if desc.nb_layers == 1 && desc.layers[0].format == DRM_FORMAT_ARGB8888 {
                // A converted picture has no transparency to show.
                DRM_FORMAT_XRGB8888
            } else if desc.nb_layers == 1 {
                desc.layers[0].format
            } else if desc.layers[0].format == DRM_FORMAT_R16 {
                DRM_FORMAT_P010
            } else {
                DRM_FORMAT_NV12
            };
            if !self.logged {
                self.logged = true;
                let f = &*self.frame;
                log::info!(
                    "decoded format: {:?} range {:?} matrix {:?} transfer {:?} primaries {:?}, DRM {}",
                    desc.layers[..desc.nb_layers as usize].iter().map(|l| String::from_utf8_lossy(&l.format.to_le_bytes()).into_owned()).collect::<Vec<_>>(),
                    f.color_range, f.colorspace, f.color_trc, f.color_primaries,
                    String::from_utf8_lossy(&format.to_le_bytes())
                );
            }
            Ok(GpuFrame {
                decoded_at: std::time::Instant::now(),
                source: av_frame_clone(self.frame),
                width: (*self.frame).width as u32,
                height: (*self.frame).height as u32,
                content: ((*self.frame).width as u32, (*self.frame).height as u32),
                frame: drm,
                format,
                planes,
                capture_us,
                dark_titlebar: true,
                px_per_pt: 0,
                full_range: (*self.frame).color_range == AVColorRange::AVCOL_RANGE_JPEG,
                rgb: shown != self.frame,
                cpu_rgba: None,
            })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.pkt);
            av_frame_free(&mut self.frame);
            avcodec_free_context(&mut self.ctx);
            if !self.hw_device.is_null() {
                av_buffer_unref(&mut self.hw_device);
            }
            if let Some(ctx) = self.sws.filter(|c| !c.is_null()) {
                sws_freeContext(ctx);
            }
        }
    }
}

/// A made-up picture in GPU memory, as the decoder gives them (a VA-API surface exported as a
/// dmabuf), for `sweeflow x11-selftest`, which draws it with no Mac. `yuv(x, y)` gives each
/// pixel's (Y, Cb, Cr) as 8-bit code values (shifted up for 10-bit); chroma is taken at even
/// pixels. `ten_bit`: P010 rather than NV12.
pub fn synthetic_frame(
    render_node: &str,
    (width, height): (u32, u32),
    ten_bit: bool,
    full_range: bool,
    yuv: impl Fn(u32, u32) -> (u8, u8, u8),
) -> Result<GpuFrame> {
    unsafe {
        let mut device = ptr::null_mut();
        let dev = CString::new(render_node)?;
        let r = av_hwdevice_ctx_create(&mut device, AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, dev.as_ptr(), ptr::null_mut(), 0);
        if r < 0 {
            bail!("VA-API device {render_node} unavailable ({r})");
        }
        let sw_format = if ten_bit { AVPixelFormat::AV_PIX_FMT_P010LE } else { AVPixelFormat::AV_PIX_FMT_NV12 };
        let mut pool = av_hwframe_ctx_alloc(device);
        av_buffer_unref(&mut device);
        if pool.is_null() {
            bail!("av_hwframe_ctx_alloc");
        }
        let fc = (*pool).data as *mut AVHWFramesContext;
        (*fc).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*fc).sw_format = sw_format;
        (*fc).width = width as i32;
        (*fc).height = height as i32;
        (*fc).initial_pool_size = 2;
        let r = av_hwframe_ctx_init(pool);
        if r < 0 {
            av_buffer_unref(&mut pool);
            bail!("av_hwframe_ctx_init ({r})");
        }
        let mut hw = av_frame_alloc();
        let r = av_hwframe_get_buffer(pool, hw, 0);
        av_buffer_unref(&mut pool);
        if r < 0 {
            av_frame_free(&mut hw);
            bail!("av_hwframe_get_buffer ({r})");
        }
        let mut sw = av_frame_alloc();
        (*sw).format = sw_format as i32;
        (*sw).width = width as i32;
        (*sw).height = height as i32;
        if av_frame_get_buffer(sw, 0) < 0 {
            av_frame_free(&mut sw);
            av_frame_free(&mut hw);
            bail!("av_frame_get_buffer");
        }
        let (luma, luma_stride) = ((*sw).data[0], (*sw).linesize[0] as usize);
        let (chroma, chroma_stride) = ((*sw).data[1], (*sw).linesize[1] as usize);
        let put = |row: *mut u8, i: usize, v: u8| {
            if ten_bit {
                // P010: the 10-bit value in the top bits of a little-endian 16-bit sample.
                let s = ((v as u16) << 2) << 6;
                *row.add(i * 2) = s as u8;
                *row.add(i * 2 + 1) = (s >> 8) as u8;
            } else {
                *row.add(i) = v;
            }
        };
        for y in 0..height {
            let row = luma.add(y as usize * luma_stride);
            for x in 0..width {
                put(row, x as usize, yuv(x, y).0);
            }
        }
        for y in 0..height.div_ceil(2) {
            let row = chroma.add(y as usize * chroma_stride);
            for x in 0..width.div_ceil(2) {
                let (_, cb, cr) = yuv(x * 2, y * 2);
                put(row, x as usize * 2, cb);
                put(row, x as usize * 2 + 1, cr);
            }
        }
        let r = av_hwframe_transfer_data(hw, sw, 0);
        av_frame_free(&mut sw);
        if r < 0 {
            av_frame_free(&mut hw);
            bail!("upload to the GPU ({r})");
        }
        (*hw).color_range = if full_range { AVColorRange::AVCOL_RANGE_JPEG } else { AVColorRange::AVCOL_RANGE_MPEG };
        let mut drm = av_frame_alloc();
        (*drm).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        let r = av_hwframe_map(drm, hw, AV_HWFRAME_MAP_READ as i32);
        if r < 0 {
            av_frame_free(&mut drm);
            av_frame_free(&mut hw);
            bail!("av_hwframe_map to DRM PRIME failed ({r})");
        }
        // As Decoder::export reads them.
        let desc = &*((*drm).data[0] as *const AVDRMFrameDescriptor);
        let mut planes = Vec::new();
        for l in &desc.layers[..desc.nb_layers as usize] {
            for p in &l.planes[..l.nb_planes as usize] {
                let obj = &desc.objects[p.object_index as usize];
                planes.push(Plane { fd: obj.fd, offset: p.offset as u32, stride: p.pitch as u32, modifier: obj.format_modifier });
            }
        }
        let format = if desc.nb_layers == 1 {
            desc.layers[0].format
        } else if desc.layers[0].format == DRM_FORMAT_R16 {
            DRM_FORMAT_P010
        } else {
            DRM_FORMAT_NV12
        };
        Ok(GpuFrame {
            decoded_at: std::time::Instant::now(),
            source: hw,
            width,
            height,
            content: (width, height),
            frame: drm,
            format,
            planes,
            capture_us: 0,
            dark_titlebar: true,
            px_per_pt: 0,
            full_range,
            rgb: false,
            cpu_rgba: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A yuv420p picture `w`×`h` of one grey, in a decoder's frame.
    unsafe fn grey_frame(d: &mut Decoder, w: i32, h: i32) {
        unsafe {
            let f = &mut *d.frame;
            f.width = w;
            f.height = h;
            f.format = AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            f.color_range = AVColorRange::AVCOL_RANGE_JPEG;
            assert!(av_frame_get_buffer(d.frame, 0) >= 0);
            for (plane, rows, v) in [(0, h, 200u8), (1, (h + 1) / 2, 128), (2, (h + 1) / 2, 128)] {
                ptr::write_bytes(f.data[plane], v, (f.linesize[plane] * rows) as usize);
            }
        }
    }

    /// How far past `stride` × `h` swscale writes, converting a `w`×`h` yuv420p picture.
    unsafe fn overrun(w: i32, h: i32, out: AVPixelFormat, full: bool, stride: usize) -> usize {
        unsafe {
            let mut d = Decoder::software().unwrap();
            grey_frame(&mut d, w, h);
            let f = &*d.frame;
            let ctx = sws_getContext(w, h, AVPixelFormat::AV_PIX_FMT_YUV420P, w, h, out, 2,
                                     ptr::null_mut(), ptr::null_mut(), ptr::null());
            sws_setColorspaceDetails(ctx, sws_getCoefficients(1), full as i32, sws_getCoefficients(5), 1, 0, 1 << 16, 1 << 16);
            let end = stride * h as usize;
            let mut buf = vec![0xA5u8; end + 4096];
            let dst = [buf.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
            let stride = [stride as i32, 0, 0, 0];
            sws_scale(ctx, f.data.as_ptr() as *const *const u8, f.linesize.as_ptr(), 0, h, dst.as_ptr(), stride.as_ptr());
            sws_freeContext(ctx);
            buf[end..].iter().rposition(|&b| b != 0xA5).map_or(0, |i| i + 1)
        }
    }

    /// Padded rows take everything swscale writes (a tight 780 px row doesn't: 16 bytes over,
    /// the "light" crash on Mint and Ubuntu).
    #[test]
    fn padded_rows_hold_what_swscale_writes() {
        let sizes = (1..=80).map(|w| (w, 6)).chain((760..=800).map(|w| (w, 40))).chain([(780, 870), (1001, 701), (1431, 858)]);
        for (w, h) in sizes {
            for out in [AVPixelFormat::AV_PIX_FMT_RGBA, AVPixelFormat::AV_PIX_FMT_BGRA] {
                for full in [false, true] {
                    assert_eq!(unsafe { overrun(w, h, out, full, padded_row(w)) }, 0, "{w}x{h} {out:?}");
                }
            }
        }
        eprintln!("tight 780x870: {} bytes past the end", unsafe { overrun(780, 870, AVPixelFormat::AV_PIX_FMT_BGRA, false, 780 * 4) });
    }

    #[test]
    fn cpu_pictures_are_tight_and_whole() {
        for (w, h) in [(1001, 7), (1366, 9), (803, 5), (1600, 960), (1431, 858), (2, 2)] {
            unsafe {
                let mut d = Decoder::software().unwrap();
                grey_frame(&mut d, w, h);
                let out = d.export_cpu(0).unwrap();
                let px = out.cpu_rgba.as_ref().unwrap();
                assert_eq!(px.len(), w as usize * h as usize * 4, "{w}x{h}");
                // Every pixel the same grey, the last row's last one included.
                let first = [px[0], px[1], px[2], px[3]];
                assert!(first[0] > 150 && first[0] == first[1] && first[1] == first[2], "{w}x{h}: {first:?}");
                assert!(px.chunks_exact(4).all(|p| p == first), "{w}x{h}: rows not packed");
            }
        }
    }
}
