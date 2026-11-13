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

/// Whether pictures go to the compositor as RGB instead of the decoder's own YUV. Hyprland takes
/// YUV buffers (NV12 and P010 alike) and then draws only their brightness plane, as red: there
/// the GPU converts each picture first. `SWEEFLOW_VIDEO=rgb` or `=yuv` overrides the choice.
pub fn rgb_output() -> bool {
    static RGB: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| match std::env::var("SWEEFLOW_VIDEO").as_deref() {
        Ok("rgb") => true,
        Ok("yuv") => false,
        _ => std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some(),
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
            (*par).color_space = (*frame).colorspace;
            (*par).color_range = (*frame).color_range;
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
}

unsafe impl Send for Decoder {}

unsafe extern "C" fn pick_vaapi(_: *mut AVCodecContext, mut fmts: *const AVPixelFormat) -> AVPixelFormat {
    unsafe {
        while *fmts != AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmts == AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *fmts;
            }
            fmts = fmts.add(1);
        }
    }
    log::error!("decoder did not offer VAAPI output");
    AVPixelFormat::AV_PIX_FMT_NONE
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
            Ok(Decoder { ctx, hw_device, pkt: av_packet_alloc(), frame: av_frame_alloc(), logged: false, to_rgb: None })
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
                let exported = if rgb_output() { self.export_rgb(capture_us) } else { self.export(self.frame, capture_us) };
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
            av_buffer_unref(&mut self.hw_device);
        }
    }
}
