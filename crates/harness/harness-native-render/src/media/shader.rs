//! Shaders: each one's pipeline, built from the MSL the kernel's slangc made (Metal compiles it on its own threads), and each
//! element's pictures, rendered off screen on Media's own command queue into RGBA16Float textures (the window's format, so
//! values past 1 stay past SDR white), as premultiplied colour the prelude made (no blending: every texel is written).
//!
//! The guard. A frame samples only a picture whose render has finished, so a slow shader never holds up the window's pass,
//! and an element has at most one render in flight (the next waits for it), so a runaway shader cannot queue up GPU work.
//! The first render is at 1/4 resolution and steps up to full while a step costs under the budget; a rolling mean over
//! [`BUDGET_MS`] halves the resolution, down to 1/8; still over there, or one render over [`PAUSE_MS`], pauses the element
//! (`resume` runs it again from 1/4). A render unfinished after [`HANG`], or a command buffer that ends in error, fails the
//! shader (its pipeline, so every element showing it stops): never sent again until its source changes.
//!
//! What an infinite loop does: Apple GPUs do not preempt a fragment shader mid-draw, so the loop runs until the kernel's GPU
//! watchdog kills the command buffer (a few seconds; Metal reports MTLCommandBufferError Timeout, "Caused GPU Timeout Error
//! (kIOGPUCommandBufferCallbackErrorTimeout)"). Until then other queues' work, the window's pass included, can be held up
//! behind it on the same GPU; once the watchdog fires the GPU recovers. Repeated faults from one process can get its
//! submissions ignored for a while (kIOGPUCommandBufferCallbackErrorSubmissionsIgnored), which is why a failed shader is
//! never sent again. HANG (2 s) fails the shader in the UI before the watchdog does; it cannot cancel work already on the
//! GPU, so the flight is kept (and its picture never shown) until Metal ends it.
//!
//! Completion is polled: each prepare reads the in-flight command buffer's `status` (and its GPU start and end times once
//! completed), so no completion block crosses threads.
use std::cell::Cell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSError, NSString};
use objc2_metal::*;

use super::video::{Frame, Videos};
use super::{FORGET_SECS, Input, ShaderSpec, Status};
use crate::gpu::{Dev, PIXEL_FORMAT, Tex};

/// The prelude's vertex and fragment entries (the kernel's binding layout). One place, so a rename is one edit.
pub const ENTRIES: (&str, &str) = ("den_vs", "den_fs");
/// The fixed binding layout of the fragment entry: the ShaderFrame buffer and the sampler; texture i is slot i.
const FRAME_BUFFER: usize = 0;
const SAMPLER: usize = 0;
/// GPU milliseconds a render may cost on average.
pub const BUDGET_MS: f64 = 6.0;
/// Renders the average is over, and how many it needs before it may halve the resolution (one slow first render, say a
/// cold cache, is not a verdict).
pub const ROLLING: usize = 8;
const MIN_SAMPLES: usize = 3;
/// One render slower than this pauses the element at once.
pub const PAUSE_MS: f64 = 100.0;
/// A render still unfinished this long after it was sent fails its shader.
pub const HANG: Duration = Duration::from_secs(2);
/// A pipeline still building this long after it was asked for fails its shader.
pub const BUILD_LIMIT: Duration = Duration::from_secs(30);
/// Pictures an element keeps: the one shown, ones the window's frames in flight still sample, the one being rendered.
pub const PICTURES: usize = 4;
/// Resolution levels: a picture's side is the box's in pixels shifted right by the level. The first render's, and the
/// lowest the guard goes.
const START_LEVEL: u32 = 2;
const MAX_LEVEL: u32 = 3;
/// The most pixels a side of a picture has.
const MAX_SIDE: u32 = 2048;
/// Fragment data up to this many bytes goes by setBytes (Metal's limit for it); more goes in a buffer of its own.
const INLINE_BYTES: usize = 4096;
/// The fewest floats of a ShaderFrame: size, pointer and one slot (the prelude declares at least one).
const MIN_FLOATS: usize = 12;
/// How often a photo input's file is looked at again for a change.
const PHOTO_RECHECK: Duration = Duration::from_secs(1);

type Pso = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
type Built = Arc<Mutex<Option<Result<Pso, String>>>>;
type Cmd = Retained<ProtocolObject<dyn MTLCommandBuffer>>;

enum Pipe {
    Building { built: Built, since: Instant },
    Ready(Pso),
    Failed(String),
}

struct PipeSlot {
    pipe: Pipe,
    seen: Instant,
}

struct Picture {
    tex: Tex,
    /// The resolution level it was rendered at.
    level: u32,
    /// The window frame that last drew it (Media's serial).
    sampled: Cell<u64>,
}

struct Flight {
    cmd: Cmd,
    picture: usize,
    since: Instant,
    level: u32,
    /// The full-resolution data and the input versions it rendered.
    data: Vec<f32>,
    inputs: Vec<u64>,
    /// The video frames it samples, alive until it finishes (their pixel buffers are not recycled under it).
    _frames: Vec<Rc<Frame>>,
}

struct Element {
    msl_key: u64,
    pictures: Vec<Picture>,
    /// The picture shown (the latest finished render) and what it read.
    front: Option<usize>,
    shown: Vec<f32>,
    shown_inputs: Vec<u64>,
    /// Finished renders so far: what a shader that samples this one compares.
    version: u64,
    flight: Option<Flight>,
    level: u32,
    /// Still stepping up from the first render's resolution.
    progressive: bool,
    /// GPU milliseconds of the last renders at this level.
    times: Vec<f64>,
    /// Ready, Paused (the guard) or Failed (an input it cannot bind; GPU failures are the pipeline's).
    status: Status,
    /// The inputs a Failed status is about: a change of inputs tries again.
    failed_inputs: Option<Vec<Input>>,
    /// Visible and its picture is not what it would render now (a render pending or in flight).
    wants: bool,
    /// Its picture is at its current level and of this frame's data and inputs (its own picture, feedback, aside).
    fresh: bool,
    visible: bool,
    seen: Instant,
}

struct Photo {
    tex: Result<Tex, String>,
    mtime: Option<SystemTime>,
    checked: Instant,
    version: u64,
}

/// What a texture slot binds this render.
struct Bound {
    tex: Tex,
    version: u64,
    frame: Option<Rc<Frame>>,
}

pub(crate) struct Shaders {
    dev: Dev,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    /// 1x1 transparent: a missing input.
    blank: Tex,
    /// By msl_key.
    pipes: HashMap<u64, PipeSlot>,
    /// By element key.
    elements: HashMap<u64, Element>,
    /// By path.
    photos: HashMap<String, Photo>,
}

fn describe(e: &NSError) -> String {
    e.localizedDescription().to_string()
}

fn paused(ms: f64) -> Status {
    Status::Paused(format!("this shader took {ms:.0} ms of GPU a frame (the budget is {BUDGET_MS:.0} ms) and was paused"))
}

/// An RGBA8 texture of `w` by `h` texels from tightly packed `px`.
fn rgba8(dev: &Dev, w: usize, h: usize, px: &[u8]) -> Result<Tex, String> {
    // SAFETY: a plain 2D descriptor of a supported format and a positive size.
    let d = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::RGBA8Unorm,
            w.max(1),
            h.max(1),
            false,
        )
    };
    d.setUsage(MTLTextureUsage::ShaderRead);
    let t = dev.newTextureWithDescriptor(&d).ok_or_else(|| format!("Metal made no {w}x{h} texture"))?;
    if px.len() != w * h * 4 {
        return Err(format!("{} bytes for a {w}x{h} RGBA picture", px.len()));
    }
    let region = MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w, height: h, depth: 1 } };
    // SAFETY: px holds w*h tightly packed RGBA texels and stays alive for the call; the region is the whole texture, which
    // no GPU work uses yet.
    unsafe {
        t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            region,
            0,
            NonNull::new(px.as_ptr().cast_mut().cast()).ok_or("a picture with no pixels")?,
            w * 4,
        );
    }
    Ok(t)
}

/// A picture to render into: `w` by `h`, PIXEL_FORMAT, private to the GPU.
fn target(dev: &Dev, w: u32, h: u32) -> Option<Tex> {
    // SAFETY: a plain 2D descriptor of a supported format and a positive size.
    let d = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            PIXEL_FORMAT,
            w.max(1) as usize,
            h.max(1) as usize,
            false,
        )
    };
    d.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
    d.setStorageMode(MTLStorageMode::Private);
    dev.newTextureWithDescriptor(&d)
}

/// The pipeline of a kernel shader: the prelude's entries, writing premultiplied colour unblended.
fn pipeline(dev: &Dev, lib: &ProtocolObject<dyn MTLLibrary>) -> Result<Pso, String> {
    let f = |n: &str| {
        lib.newFunctionWithName(&NSString::from_str(n))
            .ok_or_else(|| format!("the compiled shader has no entry {n} (the prelude's)"))
    };
    let (vertex, fragment) = (f(ENTRIES.0)?, f(ENTRIES.1)?);
    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(Some(&vertex));
    desc.setFragmentFunction(Some(&fragment));
    // SAFETY: attachment 0 always exists.
    let att = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
    att.setPixelFormat(PIXEL_FORMAT);
    att.setBlendingEnabled(false);
    dev.newRenderPipelineStateWithDescriptor_error(&desc)
        .map_err(|e| format!("Metal could not make the shader's pipeline: {}", describe(&e)))
}

/// `pixels` at resolution `level`, each side at least 1 and at most [`MAX_SIDE`].
fn scaled_size(pixels: [u32; 2], level: u32) -> [u32; 2] {
    let side = |p: u32| ((p.clamp(1, MAX_SIDE) as f32) / (1u32 << level) as f32).round().clamp(1.0, MAX_SIDE as f32) as u32;
    [side(pixels[0]), side(pixels[1])]
}

/// The ShaderFrame for a picture `size` texels of a box `pixels` big: size.xy the picture's, the scale (pixels a point)
/// and the pointer (when over it) scaled alike, padded to [`MIN_FLOATS`].
fn scaled_data(data: &[f32], pixels: [u32; 2], size: [u32; 2]) -> Vec<f32> {
    let mut d = data.to_vec();
    if d.len() < MIN_FLOATS {
        d.resize(MIN_FLOATS, 0.0);
    }
    let fx = size[0] as f32 / pixels[0].max(1) as f32;
    let fy = size[1] as f32 / pixels[1].max(1) as f32;
    d[0] = size[0] as f32;
    d[1] = size[1] as f32;
    d[2] *= fx;
    // (-1, -1): not over it, whatever the scale.
    if !(d[4] < 0.0 && d[5] < 0.0) {
        d[4] *= fx;
        d[5] *= fy;
    }
    d
}

impl Element {
    fn new(msl_key: u64, now: Instant) -> Self {
        Self {
            msl_key,
            pictures: Vec::new(),
            front: None,
            shown: Vec::new(),
            shown_inputs: Vec::new(),
            version: 0,
            flight: None,
            level: START_LEVEL,
            progressive: true,
            times: Vec::new(),
            status: Status::Ready,
            failed_inputs: None,
            wants: false,
            fresh: false,
            visible: false,
            seen: now,
        }
    }

    /// The guard, after a render of `ms` GPU milliseconds at the current level.
    fn judge(&mut self, ms: f64) {
        if ms > PAUSE_MS {
            self.status = paused(ms);
            return;
        }
        if self.progressive && self.level > 0 {
            // A level up costs about four times the pixels.
            if ms * 4.0 <= BUDGET_MS {
                self.level -= 1;
                self.times.clear();
            } else {
                self.progressive = false;
            }
            return;
        }
        self.progressive = false;
        self.times.push(ms);
        if self.times.len() > ROLLING {
            self.times.remove(0);
        }
        if self.times.len() < MIN_SAMPLES {
            return;
        }
        let mean = self.times.iter().sum::<f64>() / self.times.len() as f64;
        if mean > BUDGET_MS {
            if self.level < MAX_LEVEL {
                self.level += 1;
                self.times.clear();
            } else {
                self.status = paused(mean);
            }
        }
    }
}

impl Shaders {
    pub(crate) fn new(dev: &Dev) -> Self {
        let queue = dev.newCommandQueue().expect("Media's command queue");
        let sd = MTLSamplerDescriptor::new();
        sd.setMinFilter(MTLSamplerMinMagFilter::Linear);
        sd.setMagFilter(MTLSamplerMinMagFilter::Linear);
        sd.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
        sd.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
        let sampler = dev.newSamplerStateWithDescriptor(&sd).expect("Media's sampler");
        let blank = rgba8(dev, 1, 1, &[0; 4]).expect("Media's blank texture");
        Self {
            dev: dev.clone(),
            queue,
            sampler,
            blank,
            pipes: HashMap::new(),
            elements: HashMap::new(),
            photos: HashMap::new(),
        }
    }

    /// Ask Metal for the pipeline of `msl` (compiled on Metal's threads; the handler leaves it, or why not, in `built`).
    fn build(&self, msl: &str) -> Pipe {
        let built: Built = Arc::default();
        let (out, dev) = (built.clone(), self.dev.clone());
        let handler = RcBlock::new(move |lib: *mut ProtocolObject<dyn MTLLibrary>, err: *mut NSError| {
            // SAFETY: Metal passes a live library or null, and a live error or null, for the handler's duration; retaining
            // keeps them.
            let (lib, err) = unsafe { (Retained::retain(lib), Retained::retain(err)) };
            let made = match lib {
                Some(lib) => pipeline(&dev, &lib),
                None => Err(format!(
                    "Metal could not build the shader: {}",
                    err.as_deref().map_or_else(|| "it gave no reason".to_owned(), describe)
                )),
            };
            *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(made);
        });
        let opts = MTLCompileOptions::new();
        // SAFETY: the source and options are live for the call, and Metal copies the block, which outlives the call.
        unsafe {
            self.dev.newLibraryWithSource_options_completionHandler(
                &NSString::from_str(msl),
                Some(&opts),
                RcBlock::as_ptr(&handler),
            );
        }
        Pipe::Building { built, since: Instant::now() }
    }

    /// Take in pipelines Metal finished building; fail those building too long.
    fn harvest_pipes(&mut self) {
        for slot in self.pipes.values_mut() {
            let Pipe::Building { built, since } = &slot.pipe else { continue };
            let made = built.lock().unwrap_or_else(PoisonError::into_inner).take();
            slot.pipe = match made {
                Some(Ok(pso)) => Pipe::Ready(pso),
                Some(Err(why)) => Pipe::Failed(why),
                None if since.elapsed() > BUILD_LIMIT => {
                    Pipe::Failed(format!("Metal took more than {} s to build the shader", BUILD_LIMIT.as_secs()))
                }
                None => continue,
            };
        }
    }

    /// The photo at `path` as a texture and its version (a change of the file makes a new one), decoded once.
    fn photo(&mut self, path: &str) -> (Result<Tex, String>, u64) {
        let mtime = || std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if let Some(p) = self.photos.get_mut(path) {
            if p.checked.elapsed() < PHOTO_RECHECK {
                return (p.tex.clone(), p.version);
            }
            p.checked = Instant::now();
            let now = mtime();
            if now == p.mtime {
                return (p.tex.clone(), p.version);
            }
        }
        let stamp = mtime();
        // Straight RGBA, sRGB-encoded bytes (photo.rs un-premultiplies CoreGraphics's output), sampled as RGBA8Unorm so the
        // shader sees the values as the UI's colours are: encoded, not linearised.
        let tex = crate::photo::decode(std::path::Path::new(path), None, false)
            .and_then(|ph| rgba8(&self.dev, ph.w as usize, ph.h as usize, &ph.rgba).map_err(|e| format!("{path}: {e}")));
        let version = self.photos.get(path).map_or(1, |p| p.version + 1);
        self.photos.insert(path.to_owned(), Photo { tex: tex.clone(), mtime: stamp, checked: Instant::now(), version });
        (tex, version)
    }

    /// What `spec`'s texture slots bind now, or why one cannot be.
    fn inputs(&mut self, spec: &ShaderSpec, videos: &Videos) -> Result<Vec<Bound>, String> {
        let mut out = Vec::with_capacity(spec.textures.len());
        for input in &spec.textures {
            let b = match input {
                Input::None => Bound { tex: self.blank.clone(), version: 0, frame: None },
                Input::Photo(path) => {
                    let (tex, version) = self.photo(path);
                    Bound { tex: tex?, version, frame: None }
                }
                Input::Surface(k) => {
                    if let Some(e) = self.elements.get(k) {
                        // A shader's latest finished picture (its own: the previous frame); premultiplied, as stored.
                        let tex = e.front.map_or_else(|| self.blank.clone(), |i| e.pictures[i].tex.clone());
                        Bound { tex, version: e.version, frame: None }
                    } else if let Some((f, version)) = videos.frame(*k) {
                        Bound { tex: f.tex.clone(), version, frame: Some(f) }
                    } else {
                        // Not drawn yet (a video with no frame, one not asked for this frame). A video's frames count from 1.
                        Bound { tex: self.blank.clone(), version: 0, frame: None }
                    }
                }
            };
            out.push(b);
        }
        Ok(out)
    }

    pub(crate) fn prepare(&mut self, specs: &[&ShaderSpec], videos: &Videos, now: Instant, done: u64) {
        self.harvest_pipes();
        for spec in specs {
            if !self.pipes.contains_key(&spec.msl_key) {
                let pipe = self.build(&spec.msl);
                self.pipes.insert(spec.msl_key, PipeSlot { pipe, seen: now });
            }
            if let Some(p) = self.pipes.get_mut(&spec.msl_key) {
                p.seen = now;
            }
        }
        for e in self.elements.values_mut() {
            e.visible = false;
        }
        for spec in specs {
            self.step(spec, videos, now, done);
        }
        // Elements unasked for: polled until their flight ends (it holds video frames), then dropped. Ready pipelines no
        // element uses go too; a failed one is kept so a shader that faulted the GPU is never sent again.
        for e in self.elements.values_mut() {
            if now.duration_since(e.seen).as_secs_f64() >= FORGET_SECS
                && e.flight.as_ref().is_some_and(|f| f.cmd.status().0 >= MTLCommandBufferStatus::Completed.0)
            {
                e.flight = None;
            }
        }
        self.elements.retain(|_, e| e.flight.is_some() || now.duration_since(e.seen).as_secs_f64() < FORGET_SECS);
        let used: std::collections::HashSet<u64> = self.elements.values().map(|e| e.msl_key).collect();
        self.pipes.retain(|k, p| {
            matches!(p.pipe, Pipe::Failed(_)) || used.contains(k) || now.duration_since(p.seen).as_secs_f64() < FORGET_SECS
        });
    }

    /// One element this frame: take in its finished render (or fail it), judge it, send the next when what it reads changed.
    fn step(&mut self, spec: &ShaderSpec, videos: &Videos, now: Instant, done: u64) {
        let bound = self.inputs(spec, videos);
        let el = self.elements.entry(spec.key).or_insert_with(|| Element::new(spec.msl_key, now));
        if el.msl_key != spec.msl_key {
            // A new shader in the same element: start over (a render of the old one still in flight keeps its own
            // resources through its command buffer).
            *el = Element::new(spec.msl_key, now);
        }
        el.seen = now;
        el.visible = spec.visible;
        el.wants = false;
        el.fresh = false;
        let Some(pslot) = self.pipes.get_mut(&spec.msl_key) else { return };
        // The render in flight: finished, failed, or hung.
        if let Some(f) = &el.flight {
            let st = f.cmd.status();
            if st == MTLCommandBufferStatus::Completed {
                let f = el.flight.take().expect("a flight");
                if matches!(pslot.pipe, Pipe::Ready(_)) {
                    let ms = (f.cmd.GPUEndTime() - f.cmd.GPUStartTime()) * 1000.0;
                    el.pictures[f.picture].level = f.level;
                    el.front = Some(f.picture);
                    el.version += 1;
                    el.shown = f.data;
                    el.shown_inputs = f.inputs;
                    if f.level == el.level && el.status == Status::Ready {
                        el.judge(ms.max(0.0));
                    }
                }
            } else if st == MTLCommandBufferStatus::Error {
                let f = el.flight.take().expect("a flight");
                let why = f.cmd.error().map_or_else(|| "it faulted".to_owned(), |e| describe(&e));
                if !matches!(pslot.pipe, Pipe::Failed(_)) {
                    pslot.pipe = Pipe::Failed(format!("the GPU stopped the shader: {why}"));
                }
            } else if f.since.elapsed() > HANG && !matches!(pslot.pipe, Pipe::Failed(_)) {
                pslot.pipe = Pipe::Failed(format!("ran for more than {} s on the GPU", HANG.as_secs()));
            }
        }
        if spec.resume && matches!(el.status, Status::Paused(_)) {
            el.status = Status::Ready;
            el.level = START_LEVEL;
            el.progressive = true;
            el.times.clear();
        }
        // An input it could not bind fails it until its inputs change.
        if el.failed_inputs.as_ref().is_some_and(|f| *f != spec.textures) {
            el.failed_inputs = None;
            el.status = Status::Ready;
        }
        let bound = match bound {
            Ok(b) => b,
            Err(why) => {
                el.status = Status::Failed(why);
                el.failed_inputs = Some(spec.textures.clone());
                return;
            }
        };
        let Pipe::Ready(pso) = &pslot.pipe else { return };
        if el.status != Status::Ready || !spec.visible {
            return;
        }
        let versions: Vec<u64> = bound.iter().map(|b| b.version).collect();
        let front_level = el.front.map(|i| el.pictures[i].level);
        // Its own picture (feedback) changes with every render, and a playing video's with every frame, so freshness asks
        // only that a video's picture showed some frame of it.
        let moving = |i: usize| match spec.textures[i] {
            Input::Surface(k) => k == spec.key || videos.has(k),
            _ => false,
        };
        let inputs_fresh = el.shown_inputs.len() == versions.len()
            && (0..versions.len()).all(|i| el.shown_inputs[i] == versions[i] || (moving(i) && el.shown_inputs[i] > 0));
        el.fresh = front_level == Some(el.level) && el.shown == spec.data && inputs_fresh;
        let need = !el.fresh || el.shown_inputs != versions;
        if !need {
            return;
        }
        el.wants = true;
        if el.flight.is_some() {
            return;
        }
        let size = scaled_size(spec.pixels, el.level);
        // A picture to write: not the one shown, and no window frame still in flight samples it.
        let free = |p: &Picture| p.sampled.get() <= done;
        let fits = |p: &Picture| (p.tex.width() as u32, p.tex.height() as u32) == (size[0], size[1]);
        let pick = (0..el.pictures.len()).find(|&i| Some(i) != el.front && free(&el.pictures[i]) && fits(&el.pictures[i]));
        let pick = match pick {
            Some(i) => i,
            None => {
                let stale = (0..el.pictures.len()).find(|&i| Some(i) != el.front && free(&el.pictures[i]));
                if stale.is_none() && el.pictures.len() >= PICTURES {
                    // Every picture is shown or still sampled: wait a frame.
                    return;
                }
                let Some(tex) = target(&self.dev, size[0], size[1]) else {
                    el.status = Status::Failed(format!("Metal made no {}x{} picture for the shader", size[0], size[1]));
                    return;
                };
                let p = Picture { tex, level: el.level, sampled: Cell::new(0) };
                match stale {
                    Some(i) => {
                        el.pictures[i] = p;
                        i
                    }
                    None => {
                        el.pictures.push(p);
                        el.pictures.len() - 1
                    }
                }
            }
        };
        let data = scaled_data(&spec.data, spec.pixels, size);
        match render(&self.queue, &self.dev, pso, &self.sampler, &el.pictures[pick].tex, &data, &bound) {
            Ok(cmd) => {
                el.flight = Some(Flight {
                    cmd,
                    picture: pick,
                    since: Instant::now(),
                    level: el.level,
                    data: spec.data.clone(),
                    inputs: versions,
                    _frames: bound.into_iter().filter_map(|b| b.frame).collect(),
                });
            }
            Err(why) => el.status = Status::Failed(why),
        }
    }

    /// `key`'s latest finished picture, marked drawn by window frame `serial`.
    pub(crate) fn front(&self, key: u64, serial: u64) -> Option<&Tex> {
        let e = self.elements.get(&key)?;
        let p = &e.pictures[e.front?];
        p.sampled.set(serial);
        Some(&p.tex)
    }

    pub(crate) fn status(&self, key: u64) -> Option<Status> {
        let e = self.elements.get(&key)?;
        Some(match self.pipes.get(&e.msl_key).map(|p| &p.pipe) {
            None | Some(Pipe::Building { .. }) => Status::Building,
            Some(Pipe::Failed(why)) => Status::Failed(why.clone()),
            Some(Pipe::Ready(_)) => e.status.clone(),
        })
    }

    pub(crate) fn moving(&self) -> bool {
        self.pipes.values().any(|p| matches!(p.pipe, Pipe::Building { .. }))
            || self.elements.values().any(|e| e.flight.is_some() || (e.visible && e.wants))
    }

    /// No pipeline is building, and every visible shader is paused or failed or shows a picture of this frame's data at
    /// the resolution the guard settled on (the progressive start done). A feedback shader settles though it keeps
    /// rendering.
    pub(crate) fn settled(&self) -> bool {
        !self.pipes.values().any(|p| matches!(p.pipe, Pipe::Building { .. }))
            && self.elements.values().filter(|e| e.visible).all(|e| {
                let failed = matches!(self.pipes.get(&e.msl_key).map(|p| &p.pipe), Some(Pipe::Failed(_)));
                failed || e.status != Status::Ready || (e.fresh && !e.progressive)
            })
    }
}

/// Encode and commit one render of `pso` into `tex`: `data` the ShaderFrame at buffer 0, the sampler at 0, `inputs` at
/// textures 0... The command buffer, to poll.
fn render(
    queue: &ProtocolObject<dyn MTLCommandQueue>,
    dev: &Dev,
    pso: &Pso,
    sampler: &ProtocolObject<dyn MTLSamplerState>,
    tex: &Tex,
    data: &[f32],
    inputs: &[Bound],
) -> Result<Cmd, String> {
    let cmd = queue.commandBuffer().ok_or("Metal gave no command buffer for the shader")?;
    let pass = MTLRenderPassDescriptor::new();
    // SAFETY: attachment 0 always exists.
    let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
    att.setTexture(Some(tex));
    // The full-screen triangle writes every texel.
    att.setLoadAction(MTLLoadAction::DontCare);
    att.setStoreAction(MTLStoreAction::Store);
    let enc = cmd.renderCommandEncoderWithDescriptor(&pass).ok_or("Metal gave no render encoder for the shader")?;
    enc.setRenderPipelineState(pso);
    let bytes: &[u8] = bytemuck::cast_slice(data);
    let ptr = NonNull::new(bytes.as_ptr().cast_mut().cast()).ok_or("a shader frame with no data")?;
    // SAFETY: live bytes copied by setBytes (or into a new buffer), bound at the prelude's fixed slots; textures and the
    // sampler are live (the command buffer retains them until it completes) at the slots the wire's order gives; one
    // full-screen triangle.
    unsafe {
        if bytes.len() <= INLINE_BYTES {
            enc.setFragmentBytes_length_atIndex(ptr, bytes.len(), FRAME_BUFFER);
        } else {
            let buf = dev
                .newBufferWithBytes_length_options(ptr, bytes.len(), MTLResourceOptions::StorageModeShared)
                .ok_or_else(|| format!("Metal made no {} byte buffer for the shader's data", bytes.len()))?;
            enc.setFragmentBuffer_offset_atIndex(Some(&buf), 0, FRAME_BUFFER);
        }
        enc.setFragmentSamplerState_atIndex(Some(sampler), SAMPLER);
        for (i, b) in inputs.iter().enumerate() {
            enc.setFragmentTexture_atIndex(Some(&b.tex), i);
        }
        enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
    }
    enc.endEncoding();
    cmd.commit();
    Ok(cmd)
}
