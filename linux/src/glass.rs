//! Real translucency for Mac windows with glass (sidebars, toolbars, Liquid Glass): the video
//! has no alpha, so the Mac sends each window's alpha separately (a mask, only when it changes)
//! and we put the two back together on the GPU — decoded NV12 + mask → premultiplied RGBA —
//! into dmabufs the compositor shows with KDE's blur behind them, like macOS does with its own
//! desktop. Windows without glass keep the zero-copy path.

use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use anyhow::{anyhow, bail, Context as _, Result};
use glow::HasContext;
use khronos_egl as egl;

use crate::decode::GpuFrame;

const DRM_FORMAT_R8: u32 = u32::from_le_bytes(*b"R8  ");
const DRM_FORMAT_GR88: u32 = u32::from_le_bytes(*b"GR88");

// EGL extension constants.
const EGL_PLATFORM_DEVICE_EXT: egl::Enum = 0x313F;
const EGL_DRM_RENDER_NODE_FILE_EXT: egl::Int = 0x3377;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const DRM_FORMAT_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const GBM_BO_USE_RENDERING: u32 = 1 << 2;

// libgbm, for render targets with an explicit memory layout the compositor can import (a
// texture's own export has an implicit layout, which KWin can't use).
#[repr(C)]
struct GbmDevice(u8);
#[repr(C)]
struct GbmBo(u8);
#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut GbmDevice;
    fn gbm_bo_create_with_modifiers2(dev: *mut GbmDevice, w: u32, h: u32, format: u32, modifiers: *const u64, count: u32, flags: u32) -> *mut GbmBo;
    fn gbm_bo_get_fd_for_plane(bo: *mut GbmBo, plane: i32) -> i32;
    fn gbm_bo_get_stride_for_plane(bo: *mut GbmBo, plane: i32) -> u32;
    fn gbm_bo_get_offset(bo: *mut GbmBo, plane: i32) -> u32;
    fn gbm_bo_get_modifier(bo: *mut GbmBo) -> u64;
    fn gbm_bo_get_plane_count(bo: *mut GbmBo) -> i32;
    fn gbm_bo_destroy(bo: *mut GbmBo);
}

type EglImage = *mut std::ffi::c_void;
type QueryDevicesExt = unsafe extern "system" fn(egl::Int, *mut *mut std::ffi::c_void, *mut egl::Int) -> egl::Boolean;
type QueryDeviceStringExt = unsafe extern "system" fn(*mut std::ffi::c_void, egl::Int) -> *const std::ffi::c_char;
type ImageTargetTexture = unsafe extern "system" fn(u32, EglImage);

pub struct Glass {
    egl: egl::Instance<egl::Static>,
    display: egl::Display,
    context: egl::Context,
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    loc_size: Option<glow::UniformLocation>,
    loc_full: Option<glow::UniformLocation>,
    image_target: ImageTargetTexture,
    gbm: *mut GbmDevice,
    _node: std::fs::File,
}

/// A window's alpha mask on the GPU.
pub struct Mask {
    tex: glow::Texture,
    pub width: u32,
    pub height: u32,
}

/// A render target the compositor can show: an RGBA texture exported as a dmabuf.
pub struct Output {
    tex: glow::Texture,
    fbo: glow::Framebuffer,
    image: egl::Image,
    bo: *mut GbmBo,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<(OwnedFd, u32, u32)>, // fd, offset, stride
}

const VERTEX: &str = r#"#version 300 es
void main() {
    // One triangle covering the target.
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}"#;

const FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform sampler2D y_tex;
uniform sampler2D uv_tex;
uniform sampler2D a_tex;
uniform vec2 size;
uniform float full_range;
out vec4 color;
void main() {
    // Row 0 of every buffer is the image's top; the target's row 0 is fragment y = 0.
    vec2 uv = gl_FragCoord.xy / size;
    float y = texture(y_tex, uv).r;
    vec2 c = texture(uv_tex, uv).rg; // GR88: Cb in r, Cr in g
    if (full_range > 0.5) {
        c -= 0.5;
    } else {
        y = (y * 255.0 - 16.0) / 219.0;
        c = (c * 255.0 - 128.0) / 224.0;
    }
    // BT.709
    vec3 rgb = vec3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);
    float a = texture(a_tex, uv).r;
    color = vec4(clamp(rgb, 0.0, 1.0) * a, a);
}"#;

impl Glass {
    /// A GL context on the GPU behind `render_node` (the one decoding and compositing).
    pub fn new(render_node: &str) -> Result<Glass> {
        let egl = egl::Instance::new(egl::Static);
        let display = unsafe { open_device(&egl, render_node)? };
        egl.initialize(display).context("eglInitialize")?;
        let exts = egl.query_string(Some(display), egl::EXTENSIONS)?.to_string_lossy().into_owned();
        for need in ["EGL_EXT_image_dma_buf_import", "EGL_EXT_image_dma_buf_import_modifiers", "EGL_KHR_surfaceless_context"] {
            if !exts.contains(need) {
                bail!("{need} missing");
            }
        }
        egl.bind_api(egl::OPENGL_ES_API)?;
        let config = egl
            .choose_first_config(display, &[egl::RENDERABLE_TYPE, egl::OPENGL_ES3_BIT, egl::SURFACE_TYPE, 0, egl::NONE])?
            .ok_or_else(|| anyhow!("no GLES3 config"))?;
        let context = egl.create_context(display, config, None, &[egl::CONTEXT_MAJOR_VERSION, 3, egl::NONE])?;
        egl.make_current(display, None, None, Some(context))?;

        let proc = |name: &str| egl.get_proc_address(name).map(|f| f as *const std::ffi::c_void);
        let gl = unsafe { glow::Context::from_loader_function(|s| proc(s).unwrap_or(std::ptr::null())) };
        let image_target: ImageTargetTexture =
            unsafe { std::mem::transmute(proc("glEGLImageTargetTexture2DOES").ok_or_else(|| anyhow!("glEGLImageTargetTexture2DOES"))?) };
        let node = std::fs::OpenOptions::new().read(true).write(true).open(render_node).context("open render node")?;
        let gbm = unsafe { gbm_create_device(std::os::fd::AsRawFd::as_raw_fd(&node)) };
        if gbm.is_null() {
            bail!("gbm_create_device failed");
        }

        let (program, vao, loc_size, loc_full) = unsafe {
            let program = gl.create_program().map_err(|e| anyhow!(e))?;
            for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
                let sh = gl.create_shader(kind).map_err(|e| anyhow!(e))?;
                gl.shader_source(sh, src);
                gl.compile_shader(sh);
                if !gl.get_shader_compile_status(sh) {
                    bail!("shader: {}", gl.get_shader_info_log(sh));
                }
                gl.attach_shader(program, sh);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                bail!("program: {}", gl.get_program_info_log(program));
            }
            gl.use_program(Some(program));
            for (i, name) in ["y_tex", "uv_tex", "a_tex"].iter().enumerate() {
                gl.uniform_1_i32(gl.get_uniform_location(program, name).as_ref(), i as i32);
            }
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            (program, vao, gl.get_uniform_location(program, "size"), gl.get_uniform_location(program, "full_range"))
        };
        log::info!("glass compositing on {render_node}");
        Ok(Glass { egl, display, context, gl, program, vao, loc_size, loc_full, image_target, gbm, _node: node })
    }

    fn current(&self) {
        let _ = self.egl.make_current(self.display, None, None, Some(self.context));
    }

    pub fn upload_mask(&self, mask: &mut Option<Mask>, width: u32, height: u32, alpha: &[u8]) -> Result<()> {
        self.current();
        let gl = &self.gl;
        unsafe {
            if mask.as_ref().is_some_and(|m| m.width != width || m.height != height) {
                if let Some(m) = mask.take() {
                    gl.delete_texture(m.tex);
                }
            }
            if mask.is_none() {
                let tex = gl.create_texture().map_err(|e| anyhow!(e))?;
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::R8, width as i32, height as i32);
                set_sampling(gl);
                *mask = Some(Mask { tex, width, height });
            }
            let m = mask.as_ref().unwrap();
            gl.bind_texture(glow::TEXTURE_2D, Some(m.tex));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, width as i32, height as i32, glow::RED, glow::UNSIGNED_BYTE,
                                glow::PixelUnpackData::Slice(Some(alpha)));
        }
        Ok(())
    }

    pub fn drop_mask(&self, mask: Mask) {
        self.current();
        unsafe { self.gl.delete_texture(mask.tex) };
    }

    /// A render target: a linear RGBA buffer (a layout every compositor can import) that we
    /// draw into through a texture.
    pub fn new_output(&self, width: u32, height: u32) -> Result<Output> {
        self.current();
        let gl = &self.gl;
        unsafe {
            let mods = [DRM_FORMAT_MOD_LINEAR];
            let bo = gbm_bo_create_with_modifiers2(self.gbm, width, height, DRM_FORMAT_ABGR8888, mods.as_ptr(), 1, GBM_BO_USE_RENDERING);
            if bo.is_null() {
                bail!("gbm_bo_create failed");
            }
            let n = gbm_bo_get_plane_count(bo).max(1);
            let mut planes = Vec::new();
            for p in 0..n {
                let fd = gbm_bo_get_fd_for_plane(bo, p);
                if fd < 0 {
                    gbm_bo_destroy(bo);
                    bail!("gbm_bo_get_fd failed");
                }
                planes.push((OwnedFd::from_raw_fd(fd), gbm_bo_get_offset(bo, p), gbm_bo_get_stride_for_plane(bo, p)));
            }
            let modifier = gbm_bo_get_modifier(bo);
            let (fd, offset, stride) = (std::os::fd::AsRawFd::as_raw_fd(&planes[0].0), planes[0].1, planes[0].2);
            let image = match self.import(fd, offset, stride, modifier, DRM_FORMAT_ABGR8888, width, height) {
                Ok(i) => i,
                Err(e) => {
                    gbm_bo_destroy(bo);
                    return Err(e);
                }
            };
            let tex = gl.create_texture().map_err(|e| anyhow!(e))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            (self.image_target)(glow::TEXTURE_2D, image.as_ptr());
            set_sampling(gl);
            let fbo = gl.create_framebuffer().map_err(|e| anyhow!(e))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(tex);
                let _ = self.egl.destroy_image(self.display, image);
                gbm_bo_destroy(bo);
                bail!("glass render target incomplete");
            }
            Ok(Output { tex, fbo, image, bo, width, height, fourcc: DRM_FORMAT_ABGR8888, modifier, planes })
        }
    }

    /// A linear NV12 buffer (for `glass-test`): fds, offsets and strides of its two planes.
    pub fn new_nv12(&self, width: u32, height: u32) -> Result<Vec<(OwnedFd, u32, u32)>> {
        unsafe {
            let mods = [DRM_FORMAT_MOD_LINEAR];
            let bo = gbm_bo_create_with_modifiers2(self.gbm, width, height, u32::from_le_bytes(*b"NV12"), mods.as_ptr(), 1, 0);
            if bo.is_null() {
                bail!("gbm NV12 allocation failed");
            }
            let planes = (0..gbm_bo_get_plane_count(bo).max(1))
                .map(|p| (OwnedFd::from_raw_fd(gbm_bo_get_fd_for_plane(bo, p)), gbm_bo_get_offset(bo, p), gbm_bo_get_stride_for_plane(bo, p)))
                .collect();
            Ok(planes) // the bo itself is leaked: test only
        }
    }

    /// Fills `out` with one premultiplied colour (for `glass-test`).
    pub fn clear(&self, out: &Output, rgba: [f32; 4]) -> Result<()> {
        self.current();
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(out.fbo));
            self.gl.viewport(0, 0, out.width as i32, out.height as i32);
            self.gl.clear_color(rgba[0], rgba[1], rgba[2], rgba[3]);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.finish();
        }
        Ok(())
    }

    pub fn drop_output(&self, out: Output) {
        self.current();
        unsafe {
            self.gl.delete_framebuffer(out.fbo);
            self.gl.delete_texture(out.tex);
        }
        let _ = self.egl.destroy_image(self.display, out.image);
        unsafe { gbm_bo_destroy(out.bo) };
    }

    /// Draws `frame` with `mask` as its alpha into `out` (premultiplied), finished on return.
    pub fn render(&self, frame: &GpuFrame, mask: &Mask, out: &Output) -> Result<()> {
        if frame.planes.len() < 2 {
            bail!("expected NV12 in two planes, got {}", frame.planes.len());
        }
        self.current();
        let gl = &self.gl;
        let (w, h) = (frame.width, frame.height);
        let y_img = self.import(frame.planes[0].fd, frame.planes[0].offset, frame.planes[0].stride, frame.planes[0].modifier,
                                DRM_FORMAT_R8, w, h)?;
        let uv_img = match self.import(frame.planes[1].fd, frame.planes[1].offset, frame.planes[1].stride, frame.planes[1].modifier,
                                       DRM_FORMAT_GR88, w.div_ceil(2), h.div_ceil(2)) {
            Ok(i) => i,
            Err(e) => {
                let _ = self.egl.destroy_image(self.display, y_img);
                return Err(e);
            }
        };
        unsafe {
            let y_tex = gl.create_texture().map_err(|e| anyhow!(e))?;
            let uv_tex = gl.create_texture().map_err(|e| anyhow!(e))?;
            for (unit, tex, img) in [(0, y_tex, y_img), (1, uv_tex, uv_img)] {
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                (self.image_target)(glow::TEXTURE_2D, img.as_ptr());
                set_sampling(gl);
            }
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(glow::TEXTURE_2D, Some(mask.tex));

            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(out.fbo));
            gl.viewport(0, 0, out.width as i32, out.height as i32);
            gl.use_program(Some(self.program));
            gl.uniform_2_f32(self.loc_size.as_ref(), out.width as f32, out.height as f32);
            gl.uniform_1_f32(self.loc_full.as_ref(), if frame.full_range { 1.0 } else { 0.0 });
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            // The compositor reads the dmabuf next; make sure it's all there.
            gl.finish();

            gl.delete_texture(y_tex);
            gl.delete_texture(uv_tex);
            gl.active_texture(glow::TEXTURE0);
        }
        let _ = self.egl.destroy_image(self.display, y_img);
        let _ = self.egl.destroy_image(self.display, uv_img);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn import(&self, fd: RawFd, offset: u32, stride: u32, modifier: u64, fourcc: u32, w: u32, h: u32) -> Result<egl::Image> {
        let mut attrs: Vec<egl::Attrib> = vec![
            egl::WIDTH as egl::Attrib, w as egl::Attrib,
            egl::HEIGHT as egl::Attrib, h as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT, fourcc as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT, fd as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT, offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT, stride as egl::Attrib,
        ];
        if modifier != DRM_FORMAT_MOD_INVALID {
            attrs.extend([
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (modifier & 0xffff_ffff) as egl::Attrib,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (modifier >> 32) as egl::Attrib,
            ]);
        }
        attrs.push(egl::ATTRIB_NONE);
        self.egl
            .create_image(self.display, unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) }, EGL_LINUX_DMA_BUF_EXT,
                          unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) }, &attrs)
            .context("eglCreateImage (dmabuf)")
    }
}

unsafe fn set_sampling(gl: &glow::Context) {
    unsafe {
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    }
}

/// The EGL display of the GPU whose render node is `node`.
unsafe fn open_device(egl: &egl::Instance<egl::Static>, node: &str) -> Result<egl::Display> {
    unsafe {
        let query_devices: QueryDevicesExt = std::mem::transmute(
            egl.get_proc_address("eglQueryDevicesEXT").ok_or_else(|| anyhow!("eglQueryDevicesEXT"))?,
        );
        let query_string: QueryDeviceStringExt = std::mem::transmute(
            egl.get_proc_address("eglQueryDeviceStringEXT").ok_or_else(|| anyhow!("eglQueryDeviceStringEXT"))?,
        );
        let mut devices = [std::ptr::null_mut(); 16];
        let mut n = 0;
        if query_devices(16, devices.as_mut_ptr(), &mut n) == 0 {
            bail!("eglQueryDevicesEXT failed");
        }
        for &dev in &devices[..n as usize] {
            let s = query_string(dev, EGL_DRM_RENDER_NODE_FILE_EXT);
            if !s.is_null() && std::ffi::CStr::from_ptr(s).to_string_lossy() == node {
                return egl
                    .get_platform_display(EGL_PLATFORM_DEVICE_EXT, dev, &[egl::ATTRIB_NONE])
                    .context("eglGetPlatformDisplay (device)");
            }
        }
        bail!("no EGL device for {node}")
    }
}
