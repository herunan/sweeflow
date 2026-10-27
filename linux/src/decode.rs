//! HEVC decoding on the GPU via FFmpeg + VA-API. Decoded surfaces are exported as DRM PRIME
//! dmabufs and handed to the compositor without ever touching the CPU.

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
    pub width: u32,
    pub height: u32,
    /// The part of the frame (from its top-left) that is the window: the Mac keeps its encoder
    /// a size up while a window shrinks (no new encoder and keyframe per resize) and pads.
    pub content: (u32, u32),
    pub format: u32,
    pub planes: Vec<Plane>,
    pub capture_us: u64,
    /// The Mac saw a dark title bar in this frame (picks the inactive traffic-light colour).
    pub dark_titlebar: bool,
    /// Pixels per point (from the Mac); 0 = unknown.
    pub px_per_pt: u8,
    /// Full-range (0–255) YUV rather than limited (16–235), as signalled in the stream.
    pub full_range: bool,
}

// The AVFrame is exclusively owned; FFmpeg frames are safe to move between threads.
unsafe impl Send for GpuFrame {}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        unsafe { av_frame_free(&mut self.frame) };
    }
}

pub struct Decoder {
    ctx: *mut AVCodecContext,
    hw_device: *mut AVBufferRef,
    pkt: *mut AVPacket,
    frame: *mut AVFrame,
    /// The first frame's colour description has been logged.
    logged: bool,
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
            Ok(Decoder { ctx, hw_device, pkt: av_packet_alloc(), frame: av_frame_alloc(), logged: false })
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
                out = Some(self.export(capture_us)?);
                av_frame_unref(self.frame);
            }
            Ok(out)
        }
    }

    unsafe fn export(&mut self, capture_us: u64) -> Result<GpuFrame> {
        unsafe {
            let mut drm = av_frame_alloc();
            (*drm).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let r = av_hwframe_map(drm, self.frame, AV_HWFRAME_MAP_READ as i32);
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
            let format = if desc.nb_layers == 1 {
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
