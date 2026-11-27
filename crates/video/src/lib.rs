//! Linen's video: mpv (libmpv) rendering into an OpenGL texture that the UI
//! shows like any image. Everything stays on the GPU: decoding (VA-API) and
//! drawing. The UI asks for a frame only when mpv says one is ready, so a
//! paused video costs nothing.

use std::ffi::{CString, c_void};
use std::num::NonZeroU32;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use glow::HasContext;
use libmpv2::Mpv;
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType, mpv_render_update};

/// mpv asks for OpenGL functions whenever it needs them (also later, e.g. when
/// hardware decoding starts), so it gets the system's lookup directly rather than a
/// callback that's only valid during one frame. Slint uses EGL on Wayland, WGL on Windows.
#[cfg(windows)]
fn egl_proc(name: &str) -> *mut c_void {
    type GetProc = unsafe extern "system" fn(*const std::os::raw::c_char) -> *mut c_void;
    static GL: OnceLock<Option<(libloading::Library, GetProc)>> = OnceLock::new();
    let gl = GL.get_or_init(|| unsafe {
        let lib = libloading::Library::new("opengl32.dll").ok()?;
        let f: GetProc = *lib.get::<GetProc>(b"wglGetProcAddress\0").ok()?;
        Some((lib, f))
    });
    let (Some((lib, f)), Ok(c)) = (gl, CString::new(name)) else { return std::ptr::null_mut() };
    // wglGetProcAddress only knows newer functions (and may answer 1, 2, 3 or -1 for
    // "no"); OpenGL 1.1 ones come straight from opengl32.dll
    let p = unsafe { f(c.as_ptr()) };
    if !matches!(p as isize, -1 | 0..=3) {
        return p;
    }
    unsafe { lib.get::<*mut c_void>(c.as_bytes_with_nul()).map(|s| *s).unwrap_or(std::ptr::null_mut()) }
}

#[cfg(not(windows))]
fn egl_proc(name: &str) -> *mut c_void {
    type GetProc = unsafe extern "C" fn(*const std::os::raw::c_char) -> *mut c_void;
    static EGL: OnceLock<Option<(libloading::Library, GetProc)>> = OnceLock::new();
    let egl = EGL.get_or_init(|| unsafe {
        let lib = libloading::Library::new("libEGL.so.1").ok()?;
        let f: GetProc = *lib.get::<GetProc>(b"eglGetProcAddress\0").ok()?;
        Some((lib, f))
    });
    let (Some((_, f)), Ok(c)) = (egl, CString::new(name)) else { return std::ptr::null_mut() };
    unsafe { f(c.as_ptr()) }
}

fn mpv_get_proc(_: &(), name: &str) -> *mut c_void {
    egl_proc(name)
}

/// Settings for a fresh player.
pub struct Options {
    /// Path to yt-dlp (mpv uses it to open YouTube links).
    pub ytdl_path: String,
    /// yt-dlp format choice: H.264 first, because older GPUs (like Haswell)
    /// can only decode that in hardware.
    pub ytdl_format: String,
    /// Extra yt-dlp options, e.g. "js-runtimes=node:/usr/bin/node".
    pub ytdl_raw: Option<String>,
    /// The screen's refresh rate. Rendering inside the app's window, mpv can't
    /// see it, and without it mpv drops "late" frames (half of a 60 fps video).
    pub display_fps: f64,
}

/// Everything about OpenGL's state that the UI's renderer relies on and mpv
/// may change. Saved before mpv touches the context, restored right after:
/// missed pieces showed up as blank images and empty boxes for icons.
struct Saved {
    fbo: Option<glow::Framebuffer>,
    viewport: [i32; 4],
    program: Option<glow::Program>,
    vao: Option<glow::VertexArray>,
    array_buf: Option<glow::Buffer>,
    unpack_buf: Option<glow::Buffer>,
    active: u32,
    textures: [Option<glow::Texture>; 4],
    caps: [(u32, bool); 5],
    blend_src: (i32, i32),
    blend_dst: (i32, i32),
    unpack: [(u32, i32); 4],
    pack_align: i32,
}

impl Saved {
    unsafe fn take(gl: &glow::Context) -> Self {
        unsafe {
            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            let active = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            let mut textures = [None; 4];
            for (i, t) in textures.iter_mut().enumerate() {
                gl.active_texture(glow::TEXTURE0 + i as u32);
                *t = gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            }
            gl.active_texture(active);
            let caps = [glow::BLEND, glow::SCISSOR_TEST, glow::STENCIL_TEST, glow::DEPTH_TEST, glow::CULL_FACE]
                .map(|c| (c, gl.is_enabled(c)));
            let unpack = [glow::UNPACK_ALIGNMENT, glow::UNPACK_ROW_LENGTH, glow::UNPACK_SKIP_ROWS, glow::UNPACK_SKIP_PIXELS]
                .map(|p| (p, gl.get_parameter_i32(p)));
            Saved {
                fbo: gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING),
                viewport,
                program: gl.get_parameter_program(glow::CURRENT_PROGRAM),
                vao: gl.get_parameter_vertex_array(glow::VERTEX_ARRAY_BINDING),
                array_buf: gl.get_parameter_buffer(glow::ARRAY_BUFFER_BINDING),
                unpack_buf: gl.get_parameter_buffer(glow::PIXEL_UNPACK_BUFFER_BINDING),
                active,
                textures,
                caps,
                blend_src: (gl.get_parameter_i32(glow::BLEND_SRC_RGB), gl.get_parameter_i32(glow::BLEND_SRC_ALPHA)),
                blend_dst: (gl.get_parameter_i32(glow::BLEND_DST_RGB), gl.get_parameter_i32(glow::BLEND_DST_ALPHA)),
                unpack,
                pack_align: gl.get_parameter_i32(glow::PACK_ALIGNMENT),
            }
        }
    }

    unsafe fn restore(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, self.fbo);
            let v = self.viewport;
            gl.viewport(v[0], v[1], v[2], v[3]);
            gl.use_program(self.program);
            gl.bind_vertex_array(self.vao);
            gl.bind_buffer(glow::ARRAY_BUFFER, self.array_buf);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, self.unpack_buf);
            for (i, t) in self.textures.iter().enumerate() {
                gl.active_texture(glow::TEXTURE0 + i as u32);
                gl.bind_texture(glow::TEXTURE_2D, *t);
            }
            gl.active_texture(self.active);
            for (c, on) in self.caps {
                if on { gl.enable(c) } else { gl.disable(c) }
            }
            gl.blend_func_separate(self.blend_src.0 as u32, self.blend_dst.0 as u32, self.blend_src.1 as u32, self.blend_dst.1 as u32);
            for (p, v) in self.unpack {
                gl.pixel_store_i32(p, v);
            }
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, self.pack_align);
        }
    }
}

pub struct Gl {
    gl: glow::Context,
    render: RenderContext<'static>,
    target: Option<(glow::Texture, glow::Framebuffer, u32, u32)>,
}

pub struct Video {
    mpv: &'static Mpv,
    gl: Option<Gl>,
}

impl Video {
    pub fn new(opts: &Options) -> Result<Self> {
        let mpv = Mpv::with_initializer(|init| {
            // predictable: ignore the user's mpv.conf and scripts
            init.set_option("config", "no")?;
            init.set_option("load-scripts", "no")?;
            init.set_option("vo", "libmpv")?;
            // cheap scaling: mpv's high-quality scalers halved the frame rate on older
            // GPUs (Haswell); at window sizes the difference isn't visible
            init.set_option("profile", "fast")?;
            // YouTube often refuses (403) a stream link for its first few seconds:
            // retry instead of failing
            init.set_option("stream-lavf-o", "reconnect=1,reconnect_streamed=1,reconnect_on_http_error=4xx,reconnect_delay_max=12")?;
            // GPU decoding with zero-copy into our OpenGL texture; falls back to software
            init.set_option("hwdec", "auto-safe")?;
            init.set_option("ytdl", "yes")?;
            init.set_option("script-opts", format!("ytdl_hook-ytdl_path={}", opts.ytdl_path).as_str())?;
            init.set_option("ytdl-format", opts.ytdl_format.as_str())?;
            if let Some(raw) = &opts.ytdl_raw {
                init.set_option("ytdl-raw-options", raw.as_str())?;
            }
            init.set_option("keep-open", "yes")?;
            init.set_option("idle", "yes")?;
            init.set_option("osc", "no")?;
            init.set_option("input-default-bindings", "no")?;
            init.set_option("terminal", "no")?;
            // a bit of buffer so YouTube stalls don't stutter
            init.set_option("cache", "yes")?;
            init.set_option("demuxer-max-bytes", "150MiB")?;
            // mpv can't see the screen's refresh rate from inside our window; tell it
            // (it uses it to decide when a frame is late). Sync stays on audio: then
            // a 30 fps video needs 30 redraws a second, not one per screen refresh.
            init.set_option("display-fps-override", format!("{:.3}", opts.display_fps).as_str())?;
            // developer aid: LINEN_MPV="opt=value,opt=value" for experiments
            if let Ok(extra) = std::env::var("LINEN_MPV") {
                for kv in extra.split(',') {
                    if let Some((k, v)) = kv.split_once('=') {
                        init.set_option(k.trim(), v.trim())?;
                    }
                }
            }
            Ok(())
        })
        .map_err(|e| anyhow!("mpv didn't start: {e}"))?;
        // one player for the whole program: lives forever, so the render context
        // (which borrows it) can too
        let mpv: &'static Mpv = Box::leak(Box::new(mpv));
        Ok(Self { mpv, gl: None })
    }

    /// The player, for commands and properties (thread-safe).
    pub fn mpv(&self) -> &'static Mpv {
        self.mpv
    }

    pub fn ready(&self) -> bool {
        self.gl.is_some()
    }

    /// Hook mpv up to the window's OpenGL context. Call from the UI's rendering
    /// callback (the context must be current). `on_frame` runs on mpv's thread
    /// whenever a new frame is ready, and should ask the window to redraw.
    ///
    /// # Safety
    /// The window's OpenGL context must be current, and `wayland_display`
    /// (if any) must be the window's live `wl_display`.
    pub unsafe fn init_gl(&mut self, wayland_display: Option<*const c_void>, on_frame: impl Fn() + Send + 'static) -> Result<()> {
        let gl = unsafe { glow::Context::from_loader_function(|s| egl_proc(s) as *const c_void) };
        let mut params: Vec<RenderParam<()>> = vec![
            RenderParam::ApiType(RenderParamApiType::OpenGl),
            RenderParam::InitParams(OpenGLInitParams { get_proc_address: mpv_get_proc, ctx: () }),
        ];
        // lets VA-API frames go straight into OpenGL without a copy through the CPU
        if let Some(d) = wayland_display {
            params.push(RenderParam::WaylandDisplay(d));
        }
        let saved = unsafe { Saved::take(&gl) };
        let render = self.mpv.create_render_context(params);
        unsafe { saved.restore(&gl) };
        let mut render = render.map_err(|e| anyhow!("mpv render: {e}"))?;
        render.set_update_callback(on_frame);
        self.gl = Some(Gl { gl, render, target: None });
        Ok(())
    }

    /// Draw the newest frame (if any) at `w` x `h` physical pixels into our
    /// texture. Returns the texture and whether it's new (re-sized), so the UI
    /// only rebuilds its image then.
    ///
    /// # Safety
    /// Same OpenGL context as `init_gl`, current.
    pub unsafe fn render(&mut self, w: u32, h: u32) -> Option<(NonZeroU32, u32, u32, bool)> {
        let g = self.gl.as_mut()?;
        if w == 0 || h == 0 {
            return None;
        }
        let gl = &g.gl;
        let mut fresh = false;
        unsafe {
            let saved = Saved::take(gl);
            if g.target.is_none_or(|(_, _, tw, th)| tw != w || th != h) {
                if let Some((t, f, _, _)) = g.target.take() {
                    gl.delete_framebuffer(f);
                    gl.delete_texture(t);
                }
                let Ok(tex) = gl.create_texture() else {
                    saved.restore(gl);
                    return None;
                };
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    w as i32,
                    h as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                let Ok(fbo) = gl.create_framebuffer() else {
                    saved.restore(gl);
                    return None;
                };
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
                g.target = Some((tex, fbo, w, h));
                fresh = true;
            }
            let Some((tex, fbo, _, _)) = g.target else {
                saved.restore(gl);
                return None;
            };
            let wants = g.render.update().unwrap_or(0) & mpv_render_update::Frame != 0;
            if wants || fresh {
                let id = fbo.0.get() as i32;
                if let Err(e) = g.render.render::<()>(id, w as i32, h as i32, false) {
                    log::warn!("mpv render failed: {e}");
                }
            }
            saved.restore(gl);
            Some((tex.0, w, h, fresh))
        }
    }

    /// Tell mpv a frame went to the screen (keeps its timing smooth).
    pub fn report_swap(&self) {
        if let Some(g) = &self.gl {
            g.render.report_swap();
        }
    }
}

/// What the player tells the UI.
#[derive(Debug, Clone)]
pub enum Event {
    Title(String),
    Duration(f64),
    Paused(bool),
    /// Waiting for data (buffering).
    Buffering(bool),
    /// A file/stream started loading.
    Loading,
    /// It's playing (first frame out).
    Started,
    /// Reached the end, or failed (`Some(reason)`).
    Ended(Option<String>),
}

/// Watch mpv's events on a thread of their own.
pub fn watch(mpv: &'static Mpv, send: impl Fn(Event) + Send + 'static) {
    use libmpv2::events::{Event as E, PropertyData};
    use libmpv2::Format;
    let _ = mpv.disable_deprecated_events();
    let _ = mpv.observe_property("media-title", Format::String, 1);
    let _ = mpv.observe_property("duration", Format::Double, 2);
    let _ = mpv.observe_property("pause", Format::Flag, 3);
    let _ = mpv.observe_property("paused-for-cache", Format::Flag, 4);
    std::thread::Builder::new()
        .name("linen-mpv".into())
        .spawn(move || {
            loop {
                let Some(ev) = mpv.wait_event(60.0) else { continue };
                match ev {
                    Ok(E::Shutdown) => break,
                    Ok(E::StartFile) => send(Event::Loading),
                    Ok(E::PlaybackRestart) => send(Event::Started),
                    Ok(E::EndFile(r)) => send(Event::Ended(if r == libmpv2::mpv_end_file_reason::Error {
                        Some("couldn't play this video".into())
                    } else {
                        None
                    })),
                    Ok(E::PropertyChange { name, change, .. }) => match (name, change) {
                        ("media-title", PropertyData::Str(s)) => send(Event::Title(s.to_string())),
                        ("duration", PropertyData::Double(d)) => send(Event::Duration(d)),
                        ("pause", PropertyData::Flag(p)) => send(Event::Paused(p)),
                        ("paused-for-cache", PropertyData::Flag(b)) => send(Event::Buffering(b)),
                        _ => {}
                    },
                    Ok(_) => {}
                    // a file that fails to load arrives as an error, not an end-file event
                    Err(e) => {
                        log::warn!("mpv: {e}");
                        send(Event::Ended(Some("couldn't play this video".into())));
                    }
                }
            }
        })
        .expect("spawn mpv event thread");
}

/// Small helper for the UI: run an mpv command, logging failures.
pub fn cmd(mpv: &Mpv, name: &str, args: &[&str]) {
    if let Err(e) = mpv.command(name, args) {
        log::warn!("mpv {name} {args:?}: {e}");
    }
}

pub fn check_wayland(display: Option<*const c_void>) -> Result<()> {
    display.map(|_| ()).context("no Wayland display (hardware decoding will copy through the CPU)")
}
