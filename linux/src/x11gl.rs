//! EGL and OpenGL ES drawing for the X11 frontend (x11ui.rs).
//!
//! On Wayland the compositor shows the decoder's dmabufs itself; on X11 the client draws them.
//! Each decoded picture (NV12 or P010 planes in GPU memory, see decode.rs) is imported into EGL
//! (EGL_EXT_image_dma_buf_import), one image per plane (R8 + GR88, or R16 + GR1616), and drawn
//! into the window with a shader that turns BT.709 YUV into RGB in the picture's own range. A
//! driver that can't import single planes gets the whole picture as one external image (it
//! converts the colours itself, told the matrix and range). Nothing is copied through the CPU.
//! Lossless popup pictures, corner tiles, traffic lights and paused stills are plain textures.
//!
//! libEGL and libX11 (EGL's own X connection; x11ui's is x11rb's) are loaded at run time.

use std::ffi::{c_void, CStr, CString};

use anyhow::{anyhow, bail, Context as _, Result};
use glow::HasContext;

use crate::decode::{GpuFrame, DRM_FORMAT_NV12, DRM_FORMAT_P010};

type EGLDisplay = *mut c_void;
type EGLConfig = *mut c_void;
type EGLContext = *mut c_void;
type EGLSurface = *mut c_void;
type EGLImage = *mut c_void;
type EGLint = i32;
type EGLBoolean = u32;
type EGLenum = u32;

const EGL_NONE: EGLint = 0x3038;
const EGL_ALPHA_SIZE: EGLint = 0x3021;
const EGL_BLUE_SIZE: EGLint = 0x3022;
const EGL_GREEN_SIZE: EGLint = 0x3023;
const EGL_RED_SIZE: EGLint = 0x3024;
const EGL_NATIVE_VISUAL_ID: EGLint = 0x302E;
const EGL_SURFACE_TYPE: EGLint = 0x3033;
const EGL_WINDOW_BIT: EGLint = 0x0004;
const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
const EGL_OPENGL_ES_API: EGLenum = 0x30A0;
const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
const EGL_EXTENSIONS: EGLint = 0x3055;
const EGL_VENDOR: EGLint = 0x3053;
const EGL_PLATFORM_X11_KHR: EGLenum = 0x31D5;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_HEIGHT: EGLint = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: EGLenum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
const EGL_DMA_BUF_PLANE1_FD_EXT: EGLint = 0x3275;
const EGL_DMA_BUF_PLANE1_OFFSET_EXT: EGLint = 0x3276;
const EGL_DMA_BUF_PLANE1_PITCH_EXT: EGLint = 0x3277;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;
const EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT: EGLint = 0x3445;
const EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT: EGLint = 0x3446;
const EGL_YUV_COLOR_SPACE_HINT_EXT: EGLint = 0x327B;
const EGL_SAMPLE_RANGE_HINT_EXT: EGLint = 0x327C;
const EGL_ITU_REC709_EXT: EGLint = 0x327F;
const EGL_YUV_FULL_RANGE_EXT: EGLint = 0x3282;
const EGL_YUV_NARROW_RANGE_EXT: EGLint = 0x3283;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

const fn fourcc(c: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*c)
}
const DRM_FORMAT_R8: u32 = fourcc(b"R8  ");
const DRM_FORMAT_GR88: u32 = fourcc(b"GR88");
const DRM_FORMAT_R16: u32 = fourcc(b"R16 ");
const DRM_FORMAT_GR1616: u32 = fourcc(b"GR32");

/// The EGL calls used, from libEGL.so.1.
struct Egl {
    _lib: libloading::Library,
    get_proc_address: unsafe extern "C" fn(*const i8) -> *mut c_void,
    get_display: unsafe extern "C" fn(*mut c_void) -> EGLDisplay,
    initialize: unsafe extern "C" fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean,
    terminate: unsafe extern "C" fn(EGLDisplay) -> EGLBoolean,
    query_string: unsafe extern "C" fn(EGLDisplay, EGLint) -> *const i8,
    bind_api: unsafe extern "C" fn(EGLenum) -> EGLBoolean,
    choose_config: unsafe extern "C" fn(EGLDisplay, *const EGLint, *mut EGLConfig, EGLint, *mut EGLint) -> EGLBoolean,
    get_config_attrib: unsafe extern "C" fn(EGLDisplay, EGLConfig, EGLint, *mut EGLint) -> EGLBoolean,
    create_context: unsafe extern "C" fn(EGLDisplay, EGLConfig, EGLContext, *const EGLint) -> EGLContext,
    destroy_context: unsafe extern "C" fn(EGLDisplay, EGLContext) -> EGLBoolean,
    create_window_surface: unsafe extern "C" fn(EGLDisplay, EGLConfig, std::os::raw::c_ulong, *const EGLint) -> EGLSurface,
    destroy_surface: unsafe extern "C" fn(EGLDisplay, EGLSurface) -> EGLBoolean,
    make_current: unsafe extern "C" fn(EGLDisplay, EGLSurface, EGLSurface, EGLContext) -> EGLBoolean,
    swap_buffers: unsafe extern "C" fn(EGLDisplay, EGLSurface) -> EGLBoolean,
    swap_interval: unsafe extern "C" fn(EGLDisplay, EGLint) -> EGLBoolean,
    get_error: unsafe extern "C" fn() -> EGLint,
}

impl Egl {
    fn load() -> Result<Egl> {
        // SAFETY: loading the system's EGL library and looking up its standard entry points,
        // with the signatures of the EGL 1.4 specification.
        unsafe {
            let lib = libloading::Library::new("libEGL.so.1").context("libEGL.so.1 (Mesa's EGL) isn't installed")?;
            macro_rules! f {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes()).with_context(|| format!("libEGL has no {}", $name))?
                };
            }
            Ok(Egl {
                get_proc_address: f!("eglGetProcAddress"),
                get_display: f!("eglGetDisplay"),
                initialize: f!("eglInitialize"),
                terminate: f!("eglTerminate"),
                query_string: f!("eglQueryString"),
                bind_api: f!("eglBindAPI"),
                choose_config: f!("eglChooseConfig"),
                get_config_attrib: f!("eglGetConfigAttrib"),
                create_context: f!("eglCreateContext"),
                destroy_context: f!("eglDestroyContext"),
                create_window_surface: f!("eglCreateWindowSurface"),
                destroy_surface: f!("eglDestroySurface"),
                make_current: f!("eglMakeCurrent"),
                swap_buffers: f!("eglSwapBuffers"),
                swap_interval: f!("eglSwapInterval"),
                get_error: f!("eglGetError"),
                _lib: lib,
            })
        }
    }

    fn proc(&self, name: &str) -> *mut c_void {
        let c = CString::new(name).unwrap();
        // SAFETY: a NUL-terminated name.
        unsafe { (self.get_proc_address)(c.as_ptr()) }
    }
}

/// EGL's own connection to the X server (Xlib), from libX11.so.6.
struct Xlib {
    _lib: libloading::Library,
    display: *mut c_void,
    close: unsafe extern "C" fn(*mut c_void) -> i32,
}

impl Xlib {
    fn open() -> Result<Xlib> {
        // SAFETY: XOpenDisplay / XCloseDisplay with their Xlib signatures.
        unsafe {
            let lib = libloading::Library::new("libX11.so.6").context("libX11.so.6 isn't installed")?;
            let open: unsafe extern "C" fn(*const i8) -> *mut c_void = *lib.get(b"XOpenDisplay\0")?;
            let close: unsafe extern "C" fn(*mut c_void) -> i32 = *lib.get(b"XCloseDisplay\0")?;
            let display = open(std::ptr::null());
            if display.is_null() {
                bail!("Xlib couldn't open the display");
            }
            Ok(Xlib { _lib: lib, display, close })
        }
    }
}

impl Drop for Xlib {
    fn drop(&mut self) {
        // SAFETY: the display we opened.
        unsafe { (self.close)(self.display) };
    }
}

/// A window to draw into.
pub struct Target {
    surface: EGLSurface,
    /// Its swap interval has been set (once it was first current).
    set_up: bool,
}

/// A picture of the video, ready to draw.
pub struct Video {
    planes: VideoPlanes,
    /// YUV → RGB for the plane shader (column-major).
    matrix: [f32; 16],
    pub size: (u32, u32),
}

enum VideoPlanes {
    /// Luma and chroma as textures of their own.
    Split(glow::NativeTexture, glow::NativeTexture),
    /// The whole picture, converted by the driver.
    External(glow::NativeTexture),
    /// Already RGB (decode::rgb_output).
    Rgb(glow::NativeTexture),
}

/// A picture from memory (premultiplied BGRA, as the Mac sends them).
pub struct Tex {
    tex: glow::NativeTexture,
    pub size: (u32, u32),
}

/// How a picture goes onto what's there: replacing it (alpha and all: the window is see-through
/// where the picture is), or over it (premultiplied).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    Replace,
    Over,
}

/// A rectangle of the window (x, y, w, h in window pixels, from its top-left) showing part of a
/// picture (u0, v0, u1, v1, from its top-left, 0..1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub dst: [f32; 4],
    pub src: [f32; 4],
}

struct Prog {
    prog: glow::NativeProgram,
    dst: Option<glow::NativeUniformLocation>,
    src: Option<glow::NativeUniformLocation>,
    extra: Vec<Option<glow::NativeUniformLocation>>,
}

pub struct Gl {
    egl: Egl,
    _xlib: Xlib,
    display: EGLDisplay,
    config: EGLConfig,
    context: EGLContext,
    gl: glow::Context,
    image_target: unsafe extern "C" fn(u32, EGLImage),
    create_image: unsafe extern "C" fn(EGLDisplay, EGLContext, EGLenum, *mut c_void, *const EGLint) -> EGLImage,
    destroy_image: unsafe extern "C" fn(EGLDisplay, EGLImage) -> EGLBoolean,
    modifiers: bool,
    /// The X visual of the windows to draw into, and whether it has alpha (rounded corners and
    /// see-through parts show the desktop behind).
    pub visual: u32,
    pub alpha: bool,
    progs: Option<Progs>,
    quad: Option<glow::NativeBuffer>,
    /// The driver turned single planes away once: whole pictures from then on.
    split_failed: bool,
    current: EGLSurface,
    /// Window size of the frame being drawn.
    size: (u32, u32),
    logged_import: bool,
}

struct Progs {
    yuv: Prog,
    external: Option<Prog>,
    tex: Prog,
    solid: Prog,
}

const VERTEX: &str = "
attribute vec2 a_pos;
uniform vec4 u_dst;
uniform vec4 u_src;
varying vec2 v_uv;
void main() {
    gl_Position = vec4(mix(u_dst.xy, u_dst.zw, a_pos), 0.0, 1.0);
    v_uv = mix(u_src.xy, u_src.zw, a_pos);
}";

const PRECISION: &str = "
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
";

const YUV: &str = "
uniform sampler2D u_y;
uniform sampler2D u_uv;
uniform mat4 u_m;
varying vec2 v_uv;
void main() {
    vec4 yuv = vec4(texture2D(u_y, v_uv).r, texture2D(u_uv, v_uv).rg, 1.0);
    gl_FragColor = vec4(clamp((u_m * yuv).rgb, 0.0, 1.0), 1.0);
}";

const EXTERNAL: &str = "
uniform samplerExternalOES u_t;
varying vec2 v_uv;
void main() {
    gl_FragColor = vec4(texture2D(u_t, v_uv).rgb, 1.0);
}";

/// u_k.x: the texture is BGRA in memory (uploaded as RGBA); u_k.y: opaque.
const TEX: &str = "
uniform sampler2D u_t;
uniform vec2 u_k;
varying vec2 v_uv;
void main() {
    vec4 c = texture2D(u_t, v_uv);
    c = mix(c, c.bgra, u_k.x);
    gl_FragColor = mix(c, vec4(c.rgb, 1.0), u_k.y);
}";

const SOLID: &str = "
uniform vec4 u_c;
varying vec2 v_uv;
void main() {
    gl_FragColor = u_c;
}";

impl Gl {
    /// `argb_visuals`: the screen's 32-bit TrueColor visuals; a config drawing into one of them
    /// is taken if there is one (see-through corners), else any.
    pub fn new(argb_visuals: &[u32]) -> Result<Gl> {
        let egl = Egl::load()?;
        let xlib = Xlib::open()?;
        // SAFETY: EGL calls with valid arguments; pointers live through each call.
        unsafe {
            let client_ext = cstr((egl.query_string)(std::ptr::null_mut(), EGL_EXTENSIONS));
            let display = if client_ext.contains("EGL_EXT_platform_x11") || client_ext.contains("EGL_KHR_platform_x11") {
                let gpd = egl.proc("eglGetPlatformDisplayEXT");
                if gpd.is_null() {
                    (egl.get_display)(xlib.display)
                } else {
                    let gpd: unsafe extern "C" fn(EGLenum, *mut c_void, *const EGLint) -> EGLDisplay = std::mem::transmute(gpd);
                    gpd(EGL_PLATFORM_X11_KHR, xlib.display, std::ptr::null())
                }
            } else {
                (egl.get_display)(xlib.display)
            };
            if display.is_null() {
                bail!("no EGL display for X11");
            }
            let (mut major, mut minor) = (0, 0);
            if (egl.initialize)(display, &mut major, &mut minor) == 0 {
                bail!("eglInitialize failed ({:#x})", (egl.get_error)());
            }
            let ext = cstr((egl.query_string)(display, EGL_EXTENSIONS));
            log::info!("EGL {major}.{minor} ({}) on X11", cstr((egl.query_string)(display, EGL_VENDOR)));
            if !ext.contains("EGL_EXT_image_dma_buf_import") {
                bail!("EGL can't import dmabufs (EGL_EXT_image_dma_buf_import): the video can't be shown");
            }
            let modifiers = ext.contains("EGL_EXT_image_dma_buf_import_modifiers");
            if (egl.bind_api)(EGL_OPENGL_ES_API) == 0 {
                bail!("no OpenGL ES in EGL");
            }
            let attrs = [
                EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE,
            ];
            let mut configs = vec![std::ptr::null_mut(); 128];
            let mut n = 0;
            if (egl.choose_config)(display, attrs.as_ptr(), configs.as_mut_ptr(), configs.len() as i32, &mut n) == 0 || n == 0 {
                bail!("no EGL config for 8-bit RGBA windows");
            }
            configs.truncate(n as usize);
            let visual_of = |c: EGLConfig| {
                let mut v = 0;
                (egl.get_config_attrib)(display, c, EGL_NATIVE_VISUAL_ID, &mut v);
                v as u32
            };
            let with_alpha = configs.iter().copied().find(|&c| argb_visuals.contains(&visual_of(c)));
            let (config, alpha) = match with_alpha {
                Some(c) => (c, true),
                None => {
                    log::warn!("no EGL config for a 32-bit visual: Mac windows get square, opaque corners");
                    (configs.iter().copied().find(|&c| visual_of(c) != 0).unwrap_or(configs[0]), false)
                }
            };
            let visual = visual_of(config);
            let ctx_attrs = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
            let context = (egl.create_context)(display, config, std::ptr::null_mut(), ctx_attrs.as_ptr());
            if context.is_null() {
                bail!("eglCreateContext failed ({:#x})", (egl.get_error)());
            }
            let need = |name: &str| {
                let p = egl.proc(name);
                if p.is_null() { Err(anyhow!("EGL/GL has no {name}")) } else { Ok(p) }
            };
            let create_image = std::mem::transmute::<*mut c_void, _>(need("eglCreateImageKHR")?);
            let destroy_image = std::mem::transmute::<*mut c_void, _>(need("eglDestroyImageKHR")?);
            let image_target = std::mem::transmute::<*mut c_void, _>(need("glEGLImageTargetTexture2DOES")?);
            // Current with no window yet (EGL_KHR_surfaceless_context), for GL to be set up.
            if !ext.contains("EGL_KHR_surfaceless_context") || (egl.make_current)(display, std::ptr::null_mut(), std::ptr::null_mut(), context) == 0 {
                bail!("EGL can't make its context current without a window (EGL_KHR_surfaceless_context)");
            }
            let gl = glow::Context::from_loader_function(|name| egl.proc(name) as *const c_void);
            Ok(Gl {
                display,
                config,
                context,
                gl,
                image_target,
                create_image,
                destroy_image,
                modifiers,
                visual,
                alpha,
                progs: None,
                quad: None,
                split_failed: false,
                current: std::ptr::null_mut(),
                size: (1, 1),
                logged_import: false,
                egl,
                _xlib: xlib,
            })
        }
    }

    /// A window (created with `self.visual`) to draw into.
    pub fn target(&self, window: u32) -> Result<Target> {
        // SAFETY: the X window exists (created by the caller with our visual).
        let surface = unsafe { (self.egl.create_window_surface)(self.display, self.config, window as std::os::raw::c_ulong, std::ptr::null()) };
        if surface.is_null() {
            // SAFETY: plain EGL call.
            bail!("eglCreateWindowSurface failed ({:#x})", unsafe { (self.egl.get_error)() });
        }
        Ok(Target { surface, set_up: false })
    }

    pub fn drop_target(&mut self, t: Target) {
        // SAFETY: a surface of ours; it's no longer current once released.
        unsafe {
            if self.current == t.surface {
                (self.egl.make_current)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
                self.current = std::ptr::null_mut();
            }
            (self.egl.destroy_surface)(self.display, t.surface);
        }
    }

    /// Makes some window's surface current (GL objects need a current context).
    fn any_current(&self) -> bool {
        !self.current.is_null()
    }

    /// Starts a frame in `t`, `size` window pixels, cleared to `clear` (premultiplied RGBA).
    pub fn begin(&mut self, t: &mut Target, size: (u32, u32), clear: [f32; 4]) -> Result<()> {
        // SAFETY: our display, surface and context.
        unsafe {
            if (self.egl.make_current)(self.display, t.surface, t.surface, self.context) == 0 {
                bail!("eglMakeCurrent failed ({:#x})", (self.egl.get_error)());
            }
            self.current = t.surface;
            if !t.set_up {
                // Never wait for the screen: one thread draws every window.
                (self.egl.swap_interval)(self.display, 0);
                t.set_up = true;
            }
        }
        self.ensure_programs()?;
        self.size = (size.0.max(1), size.1.max(1));
        // SAFETY: GL calls on the current context.
        unsafe {
            self.gl.viewport(0, 0, self.size.0 as i32, self.size.1 as i32);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.clear_color(clear[0], clear[1], clear[2], clear[3]);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
        Ok(())
    }

    /// Shows the frame drawn since `begin`.
    pub fn end(&mut self, t: &Target) {
        // SAFETY: our display and surface.
        unsafe {
            if (self.egl.swap_buffers)(self.display, t.surface) == 0 {
                log::warn!("eglSwapBuffers failed ({:#x})", (self.egl.get_error)());
            }
        }
    }

    /// The frame drawn so far (RGBA, top row first), for the self-test.
    pub fn read_back(&self) -> Vec<u8> {
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        let mut px = vec![0u8; w * h * 4];
        // SAFETY: the buffer holds w × h RGBA pixels.
        unsafe {
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(0, 0, w as i32, h as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
        }
        let mut flipped = Vec::with_capacity(px.len());
        for row in px.chunks_exact(w * 4).rev() {
            flipped.extend_from_slice(row);
        }
        flipped
    }

    fn ensure_programs(&mut self) -> Result<()> {
        if self.progs.is_some() {
            return Ok(());
        }
        let external = self.gl.supported_extensions().contains("GL_OES_EGL_image_external");
        let progs = Progs {
            yuv: self.program(YUV, "", &["u_y", "u_uv", "u_m"])?,
            external: if external {
                Some(self.program(EXTERNAL, "#extension GL_OES_EGL_image_external : require\n", &["u_t"])?)
            } else {
                None
            },
            tex: self.program(TEX, "", &["u_t", "u_k"])?,
            solid: self.program(SOLID, "", &["u_c"])?,
        };
        // SAFETY: GL calls on the current context.
        unsafe {
            let quad = self.gl.create_buffer().map_err(|e| anyhow!("{e}"))?;
            self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(quad));
            let v: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
            let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
            self.gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
            self.quad = Some(quad);
            log::info!("OpenGL ES: {} ({})", self.gl.get_parameter_string(glow::VERSION), self.gl.get_parameter_string(glow::RENDERER));
        }
        self.progs = Some(progs);
        Ok(())
    }

    fn program(&self, fragment: &str, extension: &str, uniforms: &[&str]) -> Result<Prog> {
        // SAFETY: GL calls on the current context.
        unsafe {
            let gl = &self.gl;
            let prog = gl.create_program().map_err(|e| anyhow!("{e}"))?;
            for (kind, src) in [(glow::VERTEX_SHADER, VERTEX.to_string()), (glow::FRAGMENT_SHADER, format!("{extension}{PRECISION}{fragment}"))] {
                let sh = gl.create_shader(kind).map_err(|e| anyhow!("{e}"))?;
                gl.shader_source(sh, &src);
                gl.compile_shader(sh);
                if !gl.get_shader_compile_status(sh) {
                    bail!("shader: {}", gl.get_shader_info_log(sh));
                }
                gl.attach_shader(prog, sh);
                gl.delete_shader(sh);
            }
            gl.bind_attrib_location(prog, 0, "a_pos");
            gl.link_program(prog);
            if !gl.get_program_link_status(prog) {
                bail!("shader program: {}", gl.get_program_info_log(prog));
            }
            Ok(Prog {
                dst: gl.get_uniform_location(prog, "u_dst"),
                src: gl.get_uniform_location(prog, "u_src"),
                extra: uniforms.iter().map(|u| gl.get_uniform_location(prog, u)).collect(),
                prog,
            })
        }
    }

    /// One dmabuf image: `planes` (fd, offset, pitch, modifier), one or two.
    fn image(&self, (w, h): (u32, u32), format: u32, planes: &[(i32, u32, u32, u64)], hints: Option<bool>) -> Option<EGLImage> {
        let mut a = vec![EGL_WIDTH, w as i32, EGL_HEIGHT, h as i32, EGL_LINUX_DRM_FOURCC_EXT, format as i32];
        let keys = [
            (EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT),
            (EGL_DMA_BUF_PLANE1_FD_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT),
        ];
        for (&(fd, offset, pitch, modifier), k) in planes.iter().zip(keys) {
            a.extend([k.0, fd, k.1, offset as i32, k.2, pitch as i32]);
            if self.modifiers && modifier != DRM_FORMAT_MOD_INVALID {
                a.extend([k.3, modifier as u32 as i32, k.4, (modifier >> 32) as u32 as i32]);
            }
        }
        if let Some(full) = hints {
            a.extend([EGL_YUV_COLOR_SPACE_HINT_EXT, EGL_ITU_REC709_EXT, EGL_SAMPLE_RANGE_HINT_EXT, if full { EGL_YUV_FULL_RANGE_EXT } else { EGL_YUV_NARROW_RANGE_EXT }]);
        }
        a.push(EGL_NONE);
        // SAFETY: a NONE-terminated attribute list; EGL dups the fds it needs.
        let img = unsafe { (self.create_image)(self.display, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), a.as_ptr()) };
        if img.is_null() {
            // SAFETY: plain EGL call.
            log::debug!("dmabuf import of {} failed ({:#x})", fourcc_name(format), unsafe { (self.egl.get_error)() });
            None
        } else {
            Some(img)
        }
    }

    /// A texture holding an EGL image (which can go once it's bound).
    fn texture_of(&self, target: u32, img: EGLImage) -> Result<glow::NativeTexture> {
        // SAFETY: GL calls on the current context; the image is ours.
        unsafe {
            let tex = self.gl.create_texture().map_err(|e| anyhow!("{e}"))?;
            self.gl.bind_texture(target, Some(tex));
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                self.gl.tex_parameter_i32(target, p, v as i32);
            }
            (self.image_target)(target, img);
            (self.destroy_image)(self.display, img);
            Ok(tex)
        }
    }

    /// The decoder's picture, imported for drawing. Needs a current window (call after `begin`).
    pub fn import(&mut self, f: &GpuFrame) -> Result<Video> {
        if !self.any_current() {
            bail!("no window to draw in yet");
        }
        if let Some(rgba) = &f.cpu_rgba {
            // Decoded on the CPU: a plain texture of it.
            let t = self.upload(f.width, f.height, rgba)?;
            return Ok(Video { planes: VideoPlanes::Rgb(t.tex), matrix: yuv_to_rgb(8, true), size: (f.width, f.height) });
        }
        let planes: Vec<(i32, u32, u32, u64)> = f.planes.iter().map(|p| (p.fd, p.offset, p.stride, p.modifier)).collect();
        let size = (f.width, f.height);
        let half = (f.width.div_ceil(2), f.height.div_ceil(2));
        let ten_bit = f.format == DRM_FORMAT_P010;
        let matrix = yuv_to_rgb(if ten_bit { 10 } else { 8 }, f.full_range);
        if f.rgb || !(f.format == DRM_FORMAT_NV12 || ten_bit) {
            let img = self.image(size, f.format, &planes[..1], None).ok_or_else(|| anyhow!("couldn't import a {} picture", fourcc_name(f.format)))?;
            return Ok(Video { planes: VideoPlanes::Rgb(self.texture_of(glow::TEXTURE_2D, img)?), matrix, size });
        }
        if planes.len() < 2 {
            bail!("a {} picture with {} plane(s)", fourcc_name(f.format), planes.len());
        }
        if !self.split_failed {
            let (yf, uvf) = if ten_bit { (DRM_FORMAT_R16, DRM_FORMAT_GR1616) } else { (DRM_FORMAT_R8, DRM_FORMAT_GR88) };
            let y = self.image(size, yf, &planes[..1], None);
            let uv = y.and_then(|_| self.image(half, uvf, &planes[1..2], None));
            match (y, uv) {
                (Some(y), Some(uv)) => {
                    if !self.logged_import {
                        self.logged_import = true;
                        log::info!("video drawn from its planes ({} + {}, modifier {:#x}), BT.709 {} range", fourcc_name(yf), fourcc_name(uvf), planes[0].3, if f.full_range { "full" } else { "limited" });
                    }
                    let y = self.texture_of(glow::TEXTURE_2D, y)?;
                    let uv = self.texture_of(glow::TEXTURE_2D, uv)?;
                    return Ok(Video { planes: VideoPlanes::Split(y, uv), matrix, size });
                }
                (y, _) => {
                    if let Some(y) = y {
                        // SAFETY: our image.
                        unsafe { (self.destroy_image)(self.display, y) };
                    }
                    log::warn!("the driver won't import the video's planes one by one: whole pictures instead");
                    self.split_failed = true;
                }
            }
        }
        if self.progs.as_ref().is_some_and(|p| p.external.is_none()) {
            bail!("no way to draw {} pictures here (no GL_OES_EGL_image_external)", fourcc_name(f.format));
        }
        let img = self.image(size, f.format, &planes[..2], Some(f.full_range)).ok_or_else(|| anyhow!("couldn't import a {} picture", fourcc_name(f.format)))?;
        if !self.logged_import {
            self.logged_import = true;
            log::info!("video drawn as whole {} pictures (the driver's colour conversion)", fourcc_name(f.format));
        }
        Ok(Video { planes: VideoPlanes::External(self.texture_of(GL_TEXTURE_EXTERNAL_OES, img)?), matrix, size })
    }

    pub fn drop_video(&mut self, v: Video) {
        // SAFETY: textures of ours.
        unsafe {
            match v.planes {
                VideoPlanes::Split(a, b) => {
                    self.gl.delete_texture(a);
                    self.gl.delete_texture(b);
                }
                VideoPlanes::External(t) | VideoPlanes::Rgb(t) => self.gl.delete_texture(t),
            }
        }
    }

    /// A texture from premultiplied BGRA (`w` × `h`).
    pub fn upload(&mut self, w: u32, h: u32, bgra: &[u8]) -> Result<Tex> {
        if !self.any_current() {
            bail!("no window to draw in yet");
        }
        // SAFETY: GL calls on the current context; the slice holds w × h pixels.
        unsafe {
            let tex = self.gl.create_texture().map_err(|e| anyhow!("{e}"))?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
            }
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            let n = (w * h * 4) as usize;
            self.gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA as i32, w as i32, h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&bgra[..n.min(bgra.len())])));
            Ok(Tex { tex, size: (w, h) })
        }
    }

    pub fn drop_tex(&mut self, t: Tex) {
        // SAFETY: a texture of ours.
        unsafe { self.gl.delete_texture(t.tex) };
    }

    fn set_blend(&self, blend: Blend) {
        // SAFETY: GL calls on the current context.
        unsafe {
            match blend {
                Blend::Replace => self.gl.disable(glow::BLEND),
                Blend::Over => {
                    self.gl.enable(glow::BLEND);
                    self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                }
            }
        }
    }

    fn draw(&self, p: &Prog, q: Quad) {
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        let [x, y, dw, dh] = q.dst;
        // Window pixels (y down) → clip space (y up).
        let dst = [x / w * 2.0 - 1.0, 1.0 - y / h * 2.0, (x + dw) / w * 2.0 - 1.0, 1.0 - (y + dh) / h * 2.0];
        // SAFETY: GL calls on the current context; the program and buffer are ours.
        unsafe {
            self.gl.uniform_4_f32(p.dst.as_ref(), dst[0], dst[1], dst[2], dst[3]);
            self.gl.uniform_4_f32(p.src.as_ref(), q.src[0], q.src[1], q.src[2], q.src[3]);
            self.gl.bind_buffer(glow::ARRAY_BUFFER, self.quad);
            self.gl.enable_vertex_attrib_array(0);
            self.gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    pub fn draw_video(&self, v: &Video, q: Quad) {
        let Some(progs) = &self.progs else { return };
        self.set_blend(Blend::Replace);
        // SAFETY: GL calls on the current context; textures and programs are ours.
        unsafe {
            match &v.planes {
                VideoPlanes::Split(y, uv) => {
                    let p = &progs.yuv;
                    self.gl.use_program(Some(p.prog));
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(*y));
                    self.gl.active_texture(glow::TEXTURE1);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(*uv));
                    self.gl.uniform_1_i32(p.extra[0].as_ref(), 0);
                    self.gl.uniform_1_i32(p.extra[1].as_ref(), 1);
                    self.gl.uniform_matrix_4_f32_slice(p.extra[2].as_ref(), false, &v.matrix);
                    self.gl.active_texture(glow::TEXTURE0);
                    self.draw(p, q);
                }
                VideoPlanes::External(t) => {
                    let Some(p) = &progs.external else { return };
                    self.gl.use_program(Some(p.prog));
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(*t));
                    self.gl.uniform_1_i32(p.extra[0].as_ref(), 0);
                    self.draw(p, q);
                }
                VideoPlanes::Rgb(t) => self.draw_texture(*t, q, false, true),
            }
        }
    }

    pub fn draw_tex(&self, t: &Tex, q: Quad, blend: Blend) {
        self.set_blend(blend);
        self.draw_texture(t.tex, q, true, false);
    }

    fn draw_texture(&self, tex: glow::NativeTexture, q: Quad, bgra: bool, opaque: bool) {
        let Some(progs) = &self.progs else { return };
        let p = &progs.tex;
        // SAFETY: GL calls on the current context.
        unsafe {
            self.gl.use_program(Some(p.prog));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.uniform_1_i32(p.extra[0].as_ref(), 0);
            self.gl.uniform_2_f32(p.extra[1].as_ref(), bgra as u8 as f32, opaque as u8 as f32);
        }
        self.draw(p, q);
    }

    /// A rectangle of one colour (premultiplied RGBA), over what's there.
    pub fn draw_solid(&self, dst: [f32; 4], c: [f32; 4]) {
        let Some(progs) = &self.progs else { return };
        self.set_blend(Blend::Over);
        let p = &progs.solid;
        // SAFETY: GL calls on the current context.
        unsafe {
            self.gl.use_program(Some(p.prog));
            self.gl.uniform_4_f32(p.extra[0].as_ref(), c[0], c[1], c[2], c[3]);
        }
        self.draw(p, Quad { dst, src: [0.0, 0.0, 1.0, 1.0] });
    }
}

impl Drop for Gl {
    fn drop(&mut self) {
        // SAFETY: our display and context.
        unsafe {
            (self.egl.make_current)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            (self.egl.destroy_context)(self.display, self.context);
            (self.egl.terminate)(self.display);
        }
    }
}

fn cstr(p: *const i8) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: EGL returns NUL-terminated strings.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn fourcc_name(f: u32) -> String {
    String::from_utf8_lossy(&f.to_le_bytes()).trim_end().to_string()
}

/// BT.709 Y'CbCr → R'G'B' for the plane shader: a column-major 4×4 matrix taking
/// (Y, Cb, Cr, 1) as the textures give them (normalised: 8-bit samples /255; P010's 16-bit
/// samples /65535, its 10 bits at the top) to R'G'B' in 0..1. `full`: full range (0..2^bits-1)
/// rather than limited (16..235 and 16..240, scaled for 10-bit).
pub fn yuv_to_rgb(bits: u32, full: bool) -> [f32; 16] {
    let (kr, kb) = (0.2126f64, 0.0722f64);
    let kg = 1.0 - kr - kb;
    // Normalised sample → code value.
    let scale = if bits == 8 { 255.0 } else { 65535.0 / (1u32 << (16 - bits)) as f64 };
    let k = (1u32 << (bits - 8)) as f64;
    let max = ((1u32 << bits) - 1) as f64;
    // Y = ay·n + by; C = ac·n + bc.
    let (ay, by, ac, bc) = if full {
        (scale / max, 0.0, scale / max, -((1u32 << (bits - 1)) as f64) / max)
    } else {
        (scale / (219.0 * k), -16.0 / 219.0, scale / (224.0 * k), -128.0 / 224.0)
    };
    let cr_r = 2.0 * (1.0 - kr);
    let cb_g = -2.0 * kb * (1.0 - kb) / kg;
    let cr_g = -2.0 * kr * (1.0 - kr) / kg;
    let cb_b = 2.0 * (1.0 - kb);
    let m = [
        // Column 0: Y.
        ay, ay, ay, 0.0,
        // Column 1: Cb.
        0.0, cb_g * ac, cb_b * ac, 0.0,
        // Column 2: Cr.
        cr_r * ac, cr_g * ac, 0.0, 0.0,
        // Column 3: offsets.
        by + cr_r * bc, by + (cb_g + cr_g) * bc, by + cb_b * bc, 1.0,
    ];
    m.map(|v| v as f32)
}

/// What the plane shader makes of one pixel (for the tests).
#[cfg(test)]
pub fn convert(m: &[f32; 16], y: f32, cb: f32, cr: f32) -> [f32; 3] {
    let v = [y, cb, cr, 1.0];
    std::array::from_fn(|row| (0..4).map(|col| m[col * 4 + row] * v[col]).sum::<f32>().clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01)
    }

    #[test]
    fn limited_range_8_bit() {
        let m = yuv_to_rgb(8, false);
        let n = |c: f32| c / 255.0;
        assert!(close(convert(&m, n(16.0), n(128.0), n(128.0)), [0.0; 3]), "black");
        assert!(close(convert(&m, n(235.0), n(128.0), n(128.0)), [1.0; 3]), "white");
        // BT.709 red, green and blue.
        assert!(close(convert(&m, n(63.0), n(102.0), n(240.0)), [1.0, 0.0, 0.0]));
        assert!(close(convert(&m, n(173.0), n(42.0), n(26.0)), [0.0, 1.0, 0.0]));
        assert!(close(convert(&m, n(32.0), n(240.0), n(118.0)), [0.0, 0.0, 1.0]));
    }

    #[test]
    fn full_range_8_bit() {
        let m = yuv_to_rgb(8, true);
        let n = |c: f32| c / 255.0;
        assert!(close(convert(&m, 0.0, n(128.0), n(128.0)), [0.0; 3]));
        assert!(close(convert(&m, 1.0, n(128.0), n(128.0)), [1.0; 3]));
        let grey = convert(&m, n(128.0), n(128.0), n(128.0));
        assert!(close(grey, [128.0 / 255.0; 3]), "{grey:?}");
    }

    #[test]
    fn p010_samples() {
        // 10-bit code values sit in the top bits of 16-bit samples.
        let m = yuv_to_rgb(10, false);
        let n = |c10: f32| c10 * 64.0 / 65535.0;
        assert!(close(convert(&m, n(64.0), n(512.0), n(512.0)), [0.0; 3]), "black");
        assert!(close(convert(&m, n(940.0), n(512.0), n(512.0)), [1.0; 3]), "white");
        assert!(close(convert(&m, n(250.0), n(409.0), n(960.0)), [1.0, 0.0, 0.0]), "{:?}", convert(&m, n(250.0), n(409.0), n(960.0)));
        let m = yuv_to_rgb(10, true);
        assert!(close(convert(&m, n(1023.0), n(512.0), n(512.0)), [1.0; 3]));
    }
}
