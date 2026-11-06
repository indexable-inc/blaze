//! Live pictures for the generative UI's `Shader` and `Video` elements and the embedded browser's views (browser.rs):
//! what a `Surface` node shows. One [`Media`] per
//! `Renderer`, shared with the app as `Rc<RefCell<Media>>` (`Renderer::media`). Each frame the app lists what it wants as
//! [`Spec`]s in `Draw::media`; `Renderer::render` hands them to [`Media::prepare`] before it encodes, then binds
//! [`Media::texture`] for each Surface it draws.
//!
//! Shaders (shader.rs) render off screen on Media's own command queue, and a frame samples only a picture whose render has
//! finished, so a slow or hung shader never holds up the window's pass. Videos (video.rs) are AVPlayers whose frames come
//! through CoreVideo's Metal texture cache with no copy.
//!
//! Threading: everything here runs on the thread that calls `prepare`, the main thread in the app and in the offscreen
//! shot. AVPlayer and AVPlayerItem are main-thread-only in objc2 (`MainThreadOnly`), so a video asked for off the main
//! thread is refused in words; shaders work on any one thread.
pub mod browser;
pub mod shader;
pub mod video;

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::gpu::{Dev, Tex};
use objc2_metal::MTLTexture as _;

/// Seconds a shader or video may go without a Spec before it is dropped (its pictures freed, its player paused and released).
pub const FORGET_SECS: f64 = 2.0;
/// How often [`Media::settle`] steps while it waits for renders and first frames.
const SETTLE_STEP: Duration = Duration::from_millis(4);
/// The most run-loop sources one [`Media::pump`] handles before it returns.
const PUMP_MAX: usize = 64;

/// What one frame asks of Media for one element.
#[derive(Clone, Debug)]
pub enum Spec {
    Shader(ShaderSpec),
    Video(VideoSpec),
    Browser(BrowserSpec),
}

/// A harness browser view something on screen shows this frame (its Surface key is [`browser::key`]): keeps its ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserSpec {
    pub view: u64,
}

#[derive(Clone, Debug)]
pub struct ShaderSpec {
    /// The element entity.
    pub key: u64,
    /// slangc's MSL for the wrapped source (entries [`shader::ENTRIES`]).
    pub msl: Rc<str>,
    /// Parsed from the wire's hex key: the pipeline cache key.
    pub msl_key: u64,
    /// Its box in device pixels (each side 1..=2048).
    pub pixels: [u32; 2],
    /// The ShaderFrame floats at FULL resolution (size.xy, size.z scale, pointer.xy in full-resolution pixels); Media scales
    /// them when it renders at a fraction.
    pub data: Vec<f32>,
    /// Slot order.
    pub textures: Vec<Input>,
    /// Painted this frame.
    pub visible: bool,
    /// The person pressed Run on a paused shader this frame.
    pub resume: bool,
}

/// What a shader's texture slot binds.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// A 1x1 transparent texture.
    None,
    /// A PNG or JPEG at this absolute path, decoded once (straight RGBA, sRGB-encoded, as the UI's colours are).
    Photo(String),
    /// A video's current frame or a shader's latest finished picture (its own key: its previous frame).
    Surface(u64),
}

#[derive(Clone, Debug)]
pub struct VideoSpec {
    /// The element entity (or a derived key for a texture-only player).
    pub key: u64,
    /// An absolute path or an http(s) URL.
    pub src: String,
    pub looped: bool,
    pub muted: bool,
    /// Whether it should be playing now. Applied on a change only (an edge), so the app's echo never fights the player;
    /// a play after it ended starts it again from the beginning.
    pub play: bool,
    /// Seconds, asked this frame.
    pub seek: Option<f64>,
    /// Pull frames only when true; the player keeps its clock either way.
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Building,
    Ready,
    Failed(String),
    Paused(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoState {
    /// AVFoundation can play it (the item is ready).
    pub ready: bool,
    /// Seconds into it.
    pub time: f64,
    /// Seconds long; 0 until known (and for a live stream).
    pub duration: f64,
    pub playing: bool,
    /// Reached the end and stopped (never while looping); stays true until play or seek.
    pub ended: bool,
    /// Natural pixels, once known.
    pub size: Option<(u32, u32)>,
    /// Why it cannot play (missing file, not a video, network), naming the src.
    pub error: Option<String>,
}

pub struct Media {
    shaders: shader::Shaders,
    videos: video::Videos,
    browsers: browser::Browsers,
    /// This frame's number (one per `prepare`), and the last frame whose window pass has finished on the GPU: a picture
    /// or video frame last drawn at a serial at or below `done` may be written over or let go.
    serial: u64,
    done: Arc<AtomicU64>,
}

impl Media {
    #[must_use]
    pub fn new(dev: &Dev) -> Self {
        Self {
            shaders: shader::Shaders::new(dev),
            videos: video::Videos::new(dev),
            browsers: browser::Browsers::new(dev),
            serial: 0,
            done: Arc::default(),
        }
    }

    /// Called by Renderer::render with draw.media before encoding: build pipelines, take in finished renders, step players
    /// (pull a new frame for visible ones), send shader renders whose inputs changed. Specs absent for [`FORGET_SECS`] are
    /// dropped (players paused and released).
    pub fn prepare(&mut self, specs: &[Spec]) {
        self.serial += 1;
        let now = Instant::now();
        let done = self.done.load(Ordering::Acquire);
        let shaders: Vec<&ShaderSpec> = specs
            .iter()
            .filter_map(|s| match s {
                Spec::Shader(s) => Some(s),
                Spec::Video(_) | Spec::Browser(_) => None,
            })
            .collect();
        let videos: Vec<&VideoSpec> = specs
            .iter()
            .filter_map(|s| match s {
                Spec::Video(v) => Some(v),
                Spec::Shader(_) | Spec::Browser(_) => None,
            })
            .collect();
        // A video a visible shader samples pulls frames even when its own element is out of sight (a texture-only player).
        let sampled: HashSet<u64> = shaders
            .iter()
            .filter(|s| s.visible)
            .flat_map(|s| s.textures.iter())
            .filter_map(|i| match i {
                Input::Surface(k) => Some(*k),
                _ => None,
            })
            .collect();
        self.videos.prepare(&videos, &sampled, now, done);
        self.shaders.prepare(&shaders, &self.videos, now, done);
        let browsers: Vec<&BrowserSpec> = specs
            .iter()
            .filter_map(|s| match s {
                Spec::Browser(b) => Some(b),
                Spec::Shader(_) | Spec::Video(_) => None,
            })
            .collect();
        self.browsers.prepare(&browsers, now, done);
    }

    /// A browser view's frames now go to `surfaces` (`AppDown::Ring`'s ports, in slot order). The error names the view
    /// and what was wrong with the ring.
    pub fn browser_ring(
        &mut self,
        view: u64,
        generation: u32,
        w: u32,
        h: u32,
        surfaces: Vec<harness_mach::iosurface::Surface>,
    ) -> Result<(), String> {
        self.browsers.ring(view, generation, w, h, surfaces)
    }

    /// A browser view's newest picture is in `slot` (`AppDown::Frame`); a stale generation is dropped.
    pub fn browser_frame(&mut self, view: u64, generation: u32, slot: u8, seq: u64) -> Result<(), String> {
        self.browsers.frame(view, generation, slot, seq)
    }

    /// The host closed a browser view: let its ring go.
    pub fn browser_forget(&mut self, view: u64) {
        self.browsers.forget(view);
    }

    /// Where Media reports each browser view's latch (the app sends `AppUp::Latched`).
    pub fn set_browser_latch(&mut self, sink: browser::LatchSink) {
        self.browsers.set_latch(sink);
    }

    /// Whether a browser view has a picture to show yet.
    #[must_use]
    pub fn browser_showing(&self, view: u64) -> bool {
        self.browsers.showing(view)
    }

    /// What a Surface samples now for `key` (a finished frame only) and its size in texels. Marks it drawn by this frame,
    /// so it is not written over or let go until this frame's pass has finished.
    #[must_use]
    pub fn texture(&self, key: u64) -> Option<(&Tex, [u32; 2])> {
        let t = self
            .shaders
            .front(key, self.serial)
            .or_else(|| self.videos.front(key, self.serial))
            .or_else(|| self.browsers.front(key, self.serial))?;
        Some((t, [t.width() as u32, t.height() as u32]))
    }

    /// How what [`Media::texture`] gives for `key` encodes light, as `SurfaceInst::hdr`: an HDR video frame's PQ or HLG
    /// signal, its peak and primaries; zeros for SDR (a shader's output and SDR video are sRGB-encoded, drawn as they are).
    #[must_use]
    pub fn encoding(&self, key: u64) -> [f32; 4] {
        if self.shaders.front(key, self.serial).is_some() {
            return [0.0; 4];
        }
        self.videos.encoding(key).unwrap_or([0.0; 4])
    }

    /// A shader's state (None: Media has never seen `key`'s spec, or forgot it).
    #[must_use]
    pub fn status(&self, key: u64) -> Option<Status> {
        self.shaders.status(key)
    }

    /// A video's state (None: Media has never seen `key`'s spec, or forgot it).
    #[must_use]
    pub fn video(&self, key: u64) -> Option<VideoState> {
        self.videos.state(key)
    }

    /// A render or a frame is in flight or pending (a pipeline building, a render on the GPU, one waiting for the last to
    /// finish, the progressive start not yet at full resolution, a visible video playing or waiting for its first frame):
    /// draw again next frame.
    #[must_use]
    pub fn moving(&self) -> bool {
        self.shaders.moving() || self.videos.moving() || self.browsers.moving()
    }

    /// Let the main run loop handle what is waiting on it (AVFoundation posts its players' status, time and first frames
    /// through the main queue), without blocking: at most [`PUMP_MAX`] sources, none waited for. The window's run loop
    /// does this on its own; the offscreen shot, which runs no run loop, calls it once a frame. Call on the main thread.
    pub fn pump() {
        use objc2_core_foundation::{CFRunLoop, CFRunLoopRunResult, kCFRunLoopDefaultMode};
        for _ in 0..PUMP_MAX {
            // SAFETY: reading a CoreFoundation constant, alive for the process.
            let mode = unsafe { kCFRunLoopDefaultMode };
            if CFRunLoop::run_in_mode(mode, 0.0, true) != CFRunLoopRunResult::HandledSource {
                break;
            }
        }
    }

    /// For a frame drawn once (the offscreen shot): pump and prepare `specs` until every visible shader has a picture at
    /// full resolution for this data and every visible video has its first frame (or an error), or `limit` passes. True
    /// when it settled. A shader that renders forever (feedback, a paused or failed one) settles as soon as nothing is in
    /// flight that would change what it shows.
    pub fn settle(&mut self, specs: &[Spec], limit: Duration) -> bool {
        let start = Instant::now();
        loop {
            Self::pump();
            self.prepare(specs);
            if self.shaders.settled() && self.videos.settled() {
                return true;
            }
            if start.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(SETTLE_STEP);
        }
    }

    /// Where the renderer reports finished frames, and this frame's serial (crate::gpu's completion handler stores it).
    pub(crate) fn frame_token(&self) -> (Arc<AtomicU64>, u64) {
        (self.done.clone(), self.serial)
    }
}
