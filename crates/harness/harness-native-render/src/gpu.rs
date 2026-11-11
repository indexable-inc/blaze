//! The Metal renderer: pipelines from the Slang metallib, instance rings, one pass per target a frame draws (the window,
//! and each windowed overlay's window: [`Pass`]), all in one command buffer.
//!
//! Copied from den-native's gpu.rs (github.com/andrewgazelka/den) and stripped to what harness draws: panels, glyphs and
//! lines in paint order over a clear colour, plus den's outline glyphs (Slug) for the brand marks (icons.rs), whose curves
//! and bands are one static pair of buffers. den's backdrop blur chain, Apple glass, art cache, live textures, image atlas,
//! shader elements and 3D scenes are gone; den's mascot, Kip, is back (kip.rs). Per-frame CPU-written buffers are rings of [`FRAMES`]
//! slots gated by a semaphore; the tiny uniforms go by setBytes.
pub use crate::draw::*;
use crate::media::Media;
use crate::pack::{GlyphInst, ImageInst, LineInst, PanelInst, PathInst, SpriteInst, SurfaceInst};
use block2::RcBlock;
use bytemuck::Pod;
use dispatch2::{DispatchData, DispatchRetained, DispatchSemaphore, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_quartz_core::CAMetalDrawable as _;
use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;

/// The one pixel format of the layer and every pipeline's colour attachment. Half floats: sRGB-encoded colour.
pub const PIXEL_FORMAT: MTLPixelFormat = MTLPixelFormat::RGBA16Float;
pub const FRAMES: usize = 3;

static METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/harness.metallib"));
include!(concat!(env!("OUT_DIR"), "/harness_bindings.rs"));

/// Slot of an entry point's parameter, from the table build.rs read out of the generated MSL.
fn slot(entry: &str, param: &str) -> usize {
    BINDINGS
        .iter()
        .find(|b| b.0 == entry && b.1 == param)
        .unwrap_or_else(|| panic!("no binding {entry}.{param}"))
        .3 as usize
}

pub type Dev = Retained<ProtocolObject<dyn MTLDevice>>;
pub type Tex = Retained<ProtocolObject<dyn MTLTexture>>;
type Buf = Retained<ProtocolObject<dyn MTLBuffer>>;
type Pso = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

pub enum Dest {
    Drawable(Retained<ProtocolObject<dyn objc2_quartz_core::CAMetalDrawable>>),
    Texture(Tex),
}

/// The instance rings one pass uses (panels, glyphs, lines, sprites, images, paths, edges, surfaces).
const RINGS: usize = 8;

/// One target a frame draws: its draw list, where it goes, and whether the target is cleared first (`over`: drawn over
/// what an earlier pass of the same frame left in it, as a shot lays a windowed overlay's picture over the window's).
/// A drawable whose layer presents with its transaction (`presentsWithTransaction`: a windowed overlay's, so its picture
/// and its window's new frame land together) is presented once the frame is scheduled; any other with the frame.
pub struct Pass<'a> {
    pub draw: &'a Draw,
    pub dest: Dest,
    pub over: bool,
}

struct Ring {
    bufs: Vec<Buf>,
    cap: usize,
}

pub struct Renderer {
    dev: Dev,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    panel: Pso,
    glyph: Pso,
    line: Pso,
    path: Pso,
    sprite: Pso,
    image: Pso,
    surface: Pso,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    /// Kip's pictures and pipelines (kip.rs, harness-native-shaders' `mascot`).
    kips: crate::kip::Mascots,
    pub atlas: Tex,
    /// The brand marks' Slug curves and bands (icons.rs), uploaded once.
    curves: Buf,
    bands: Buf,
    /// [`RINGS`] a pass, pass by pass (a frame drawing a windowed overlay has more than one).
    rings: Vec<Option<Ring>>,
    slot: usize,
    sem: DispatchRetained<DispatchSemaphore>,
    /// The last finished command buffer's GPU time (GPUEndTime - GPUStartTime), milliseconds as f32 bits: two reads in
    /// the completion handler the frame already has. F3's overlay (src/app/stats.rs) reads it.
    gpu_ms: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Shaders' and videos' live pictures (media/), shared with the host, which reads their status and states.
    media: Rc<RefCell<Media>>,
}

fn s(x: &str) -> Retained<NSString> {
    NSString::from_str(x)
}

fn pipeline(dev: &Dev, lib: &Retained<ProtocolObject<dyn MTLLibrary>>, vs: &str, fs: &str) -> Pso {
    let desc = MTLRenderPipelineDescriptor::new();
    let f = |n: &str| {
        lib.newFunctionWithName(&s(n))
            .unwrap_or_else(|| panic!("shader entry {n} missing from the metallib"))
    };
    desc.setVertexFunction(Some(&f(vs)));
    desc.setFragmentFunction(Some(&f(fs)));
    // SAFETY: attachment 0 always exists.
    let att = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
    att.setPixelFormat(PIXEL_FORMAT);
    // Premultiplied alpha: every fragment shader returns colour already multiplied by its coverage.
    att.setBlendingEnabled(true);
    att.setSourceRGBBlendFactor(MTLBlendFactor::One);
    att.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    att.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    att.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    dev.newRenderPipelineStateWithDescriptor_error(&desc)
        .unwrap_or_else(|e| panic!("pipeline {vs}/{fs}: {e}"))
}

impl Renderer {
    pub fn new(dev: Dev) -> Self {
        let queue = dev.newCommandQueue().expect("command queue");
        let data = DispatchData::from_bytes(METALLIB);
        let lib = dev.newLibraryWithData_error(&data).expect("metallib");
        let sd = MTLSamplerDescriptor::new();
        sd.setMinFilter(MTLSamplerMinMagFilter::Linear);
        sd.setMagFilter(MTLSamplerMinMagFilter::Linear);
        sd.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
        sd.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
        let sampler = dev.newSamplerStateWithDescriptor(&sd).expect("sampler");
        // SAFETY: a plain 2D descriptor of a supported format and a positive size.
        let ad = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                ATLAS_SIZE as usize,
                ATLAS_SIZE as usize,
                false,
            )
        };
        ad.setUsage(MTLTextureUsage::ShaderRead);
        let atlas = dev.newTextureWithDescriptor(&ad).expect("atlas");
        let marks = crate::icons::marks();
        let fixed = |bytes: &[u8]| {
            let b = dev
                .newBufferWithLength_options(
                    bytes.len().max(256),
                    MTLResourceOptions::StorageModeShared,
                )
                .expect("outline buffer");
            // SAFETY: the buffer holds at least bytes.len() bytes of CPU-visible memory no GPU work uses yet.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    b.contents().as_ptr().cast::<u8>(),
                    bytes.len(),
                )
            };
            b
        };
        let curves = fixed(bytemuck::cast_slice(&marks.curves));
        let bands = fixed(bytemuck::cast_slice(&marks.bands));
        let media = Rc::new(RefCell::new(Media::new(&dev)));
        Self {
            panel: pipeline(&dev, &lib, "panel_vs", "panel_fs"),
            glyph: pipeline(&dev, &lib, "glyph_vs", "glyph_fs"),
            line: pipeline(&dev, &lib, "line_vs", "line_fs"),
            path: pipeline(&dev, &lib, "path_vs", "path_fs"),
            sprite: pipeline(&dev, &lib, "sprite_vs", "sprite_fs"),
            image: pipeline(&dev, &lib, "image_vs", "image_fs"),
            kips: crate::kip::Mascots::new(&dev),
            surface: pipeline(&dev, &lib, "surface_vs", "surface_fs"),
            dev,
            queue,
            sampler,
            atlas,
            curves,
            bands,
            rings: Vec::new(), // alloc: the renderer's rings, a pass's eight made once
            slot: 0,
            sem: DispatchSemaphore::new(FRAMES as isize),
            gpu_ms: std::sync::Arc::default(),
            media,
        }
    }

    /// The live pictures Surface nodes show: the host reads shader status and video state from it (and, in the offscreen
    /// shot, pumps and settles it). Borrowed mutably only inside [`Self::render`].
    pub fn media(&self) -> Rc<RefCell<Media>> {
        self.media.clone()
    }

    /// Where the last frame's GPU time lands (milliseconds as f32 bits).
    pub fn gpu_clock(&self) -> std::sync::Arc<std::sync::atomic::AtomicU32> {
        self.gpu_ms.clone()
    }

    pub fn device(&self) -> &Dev {
        &self.dev
    }

    /// Upload glyph pixels (premultiplied RGBA) into the atlas (new texels only, so frames in flight never see them
    /// change).
    pub fn upload_atlas(&self, x: u32, y: u32, w: u32, h: u32, bytes: &[u8]) {
        let region = MTLRegion {
            origin: MTLOrigin { x: x as usize, y: y as usize, z: 0 },
            size: MTLSize { width: w as usize, height: h as usize, depth: 1 },
        };
        debug_assert_eq!(bytes.len(), (w * h * 4) as usize);
        // SAFETY: bytes holds w*h tightly packed RGBA texels and stays alive for the call; the region lies inside the atlas.
        unsafe {
            self.atlas.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(bytes.as_ptr().cast_mut().cast::<c_void>()).unwrap(),
                w as usize * 4,
            )
        };
    }

    /// A picture's texture: `w` by `h` texels of `bytes` each: straight RGBA8 (4), or an HDR picture's RGBA half floats
    /// (8, `ImageInst::texel_bytes`).
    fn picture(&self, w: usize, h: usize, bytes: usize, px: &[u8]) -> Tex {
        let format = if bytes == 8 { MTLPixelFormat::RGBA16Float } else { MTLPixelFormat::RGBA8Unorm };
        // SAFETY: a plain 2D descriptor of a supported format and a positive size.
        let d = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format,
                w.max(1),
                h.max(1),
                false,
            )
        };
        d.setUsage(MTLTextureUsage::ShaderRead);
        let t = self.dev.newTextureWithDescriptor(&d).expect("picture texture");
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize { width: w, height: h, depth: 1 },
        };
        assert_eq!(px.len(), w * h * bytes, "picture texels");
        // SAFETY: px holds w*h tightly packed texels of `bytes` each, the texture's format, and stays alive for the call;
        // the region is the whole texture.
        unsafe {
            t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(px.as_ptr().cast_mut().cast::<c_void>()).unwrap(),
                w * bytes,
            )
        };
        t
    }

    /// An offscreen destination (for screenshots), readable by [`Self::read`].
    pub fn offscreen(&self, w: usize, h: usize) -> Tex {
        // SAFETY: a plain 2D descriptor of a supported format and a positive size.
        let d = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                PIXEL_FORMAT,
                w.max(1),
                h.max(1),
                false,
            )
        };
        d.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        d.setStorageMode(MTLStorageMode::Shared);
        self.dev.newTextureWithDescriptor(&d).expect("target texture")
    }

    fn ring<T: Pod>(&mut self, which: usize, slot: usize, items: &[T]) -> Buf {
        if self.rings.len() <= which {
            self.rings.resize_with(which + 1, || None);
        }
        let need = std::mem::size_of_val(items).max(256);
        if self.rings[which].as_ref().is_none_or(|r| r.cap < need) {
            let cap = need.next_power_of_two();
            let bufs = (0..FRAMES)
                .map(|_| {
                    self.dev
                        .newBufferWithLength_options(cap, MTLResourceOptions::StorageModeShared)
                        .expect("ring buffer")
                })
                .collect();
            self.rings[which] = Some(Ring { bufs, cap });
        }
        let buf = self.rings[which].as_ref().unwrap().bufs[slot].clone();
        // SAFETY: the buffer holds at least `need` bytes of CPU-visible memory, and the semaphore guarantees the GPU is done with this slot.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytemuck::cast_slice::<T, u8>(items).as_ptr(),
                buf.contents().as_ptr().cast::<u8>(),
                std::mem::size_of_val(items),
            )
        };
        buf
    }

    /// Draw one target: [`Self::render_passes`] with one pass.
    pub fn render(&mut self, draw: &Draw, dest: Dest) {
        self.render_passes(&[Pass { draw, dest, over: false }]);
    }

    /// Draw a frame's targets in order (the window, then each windowed overlay's own window), in one command buffer and
    /// one ring slot, so a frame that shows an overlay waits on the GPU no more than one that does not. Media and Kips are
    /// prepared once, for what every pass shows: preparing them a pass at a time would stop what only another pass shows.
    pub fn render_passes(&mut self, passes: &[Pass<'_>]) {
        self.sem.wait(DispatchTime::FOREVER);
        let ring_slot = self.slot;
        self.slot = (self.slot + 1) % FRAMES;
        // Media steps first (renders sent, frames pulled), then each Surface samples its latest finished picture.
        let media_rc = self.media.clone();
        let mut media = media_rc.borrow_mut();
        match passes {
            [one] => media.prepare(&one.draw.media),
            _ => {
                let all: Vec<_> = passes.iter().flat_map(|p| p.draw.media.iter().cloned()).collect(); // alloc: a frame's media specs, while an overlay's window is up
                media.prepare(&all);
            }
        }
        let (frames_done, serial) = media.frame_token();
        // Kips whose pose changed render their pictures first, in a command buffer committed before the frame's.
        match passes {
            [one] => self.kips.prepare(&self.queue, &one.draw.kips),
            _ => {
                let all: Vec<_> = passes.iter().flat_map(|p| p.draw.kips.iter().cloned()).collect(); // alloc: a frame's Kips, while an overlay's window is up
                self.kips.prepare(&self.queue, &all);
            }
        }
        let Some(cmd) = self.queue.commandBuffer() else {
            // Nothing of this frame reaches the GPU: what it marked sampled is free.
            frames_done.fetch_max(serial, std::sync::atomic::Ordering::Release);
            self.sem.signal();
            return;
        };
        // Drawables presented once the frame is scheduled, with their layer's transaction; whether any target is a drawable.
        let mut with_transaction = Vec::new(); // alloc: an overlay window's drawable or two a frame
        let mut on_screen = false;
        for (pass_no, pass) in passes.iter().enumerate() {
            let draw = pass.draw;
            let base = pass_no * RINGS;
            let pb = self.ring(base, ring_slot, &draw.panels);
            let gb = self.ring(base + 1, ring_slot, &draw.glyphs);
            let lb = self.ring(base + 2, ring_slot, &draw.lines);
            let sb = self.ring(base + 3, ring_slot, &draw.sprites);
            let ib = self.ring(base + 4, ring_slot, &draw.images);
            let ab = self.ring(base + 5, ring_slot, &draw.paths);
            let eb = self.ring(base + 6, ring_slot, &draw.edges);
            let surfaces: Vec<SurfaceInst> = draw
                .surfaces
                .iter()
                .zip(&draw.surface_keys)
                .map(|(inst, key)| {
                    let mut inst = *inst;
                    if let Some((_, size)) = media.texture(*key) {
                        inst.uv = cover_uv(inst.rect, inst.shape[3] != 0.0, size);
                        // How its texels encode light (an HDR video's PQ or HLG), so the pass brings it into the frame's range.
                        inst.hdr = media.encoding(*key);
                    }
                    inst
                })
                .collect();
            let fb_surf = self.ring(base + 7, ring_slot, &surfaces);
            // A texture per picture, filled now: small, and alive until the command buffer that reads it is done with it.
            let texs: Vec<Tex> = draw
                .images
                .iter()
                .zip(&draw.texels)
                .map(|(m, px)| self.picture(m.size[0] as usize, m.size[1] as usize, m.texel_bytes(), px))
                .collect();
            let (target, drawable) = match &pass.dest {
                Dest::Drawable(d) => (d.texture(), Some(d.clone())),
                Dest::Texture(t) => (t.clone(), None),
            };
            let rp = MTLRenderPassDescriptor::new();
            // SAFETY: attachment 0 always exists.
            let ca = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
            ca.setTexture(Some(&target));
            ca.setStoreAction(MTLStoreAction::Store);
            ca.setLoadAction(if pass.over { MTLLoadAction::Load } else { MTLLoadAction::Clear });
            let c = draw.clear;
            let a = f64::from(c[3]);
            // Premultiplied, as every pipeline's blend and the layer's compositing take colour.
            ca.setClearColor(MTLClearColor {
                red: f64::from(c[0]) * a,
                green: f64::from(c[1]) * a,
                blue: f64::from(c[2]) * a,
                alpha: a,
            });
            if let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&rp) {
                let fb = bytemuck::bytes_of(&draw.frame);
                for sg in &draw.segs {
                    if sg.count == 0 {
                        continue;
                    }
                    if sg.kind == SegKind::Kip {
                        let (lo, hi) = (sg.first as usize, (sg.first + sg.count) as usize);
                        self.kips.draw(&enc, &draw.frame, &draw.kips[lo..hi]);
                        continue;
                    }
                    let (pso, vs, fs, buf, stride) = match sg.kind {
                        SegKind::Kip => continue,
                        SegKind::Panel => {
                            (&self.panel, "panel_vs", "panel_fs", &pb, std::mem::size_of::<PanelInst>())
                        }
                        SegKind::Glyph => {
                            (&self.glyph, "glyph_vs", "glyph_fs", &gb, std::mem::size_of::<GlyphInst>())
                        }
                        SegKind::Line => {
                            (&self.line, "line_vs", "line_fs", &lb, std::mem::size_of::<LineInst>())
                        }
                        SegKind::Path => {
                            (&self.path, "path_vs", "path_fs", &ab, std::mem::size_of::<PathInst>())
                        }
                        SegKind::Sprite => (
                            &self.sprite,
                            "sprite_vs",
                            "sprite_fs",
                            &sb,
                            std::mem::size_of::<SpriteInst>(),
                        ),
                        SegKind::Image => {
                            (&self.image, "image_vs", "image_fs", &ib, std::mem::size_of::<ImageInst>())
                        }
                        SegKind::Surface => (
                            &self.surface,
                            "surface_vs",
                            "surface_fs",
                            &fb_surf,
                            std::mem::size_of::<SurfaceInst>(),
                        ),
                    };
                    // A Surface with nothing finished to show yet (a shader building or on its first render, a video before
                    // its first frame) draws nothing.
                    let live = match sg.kind {
                        SegKind::Surface => match media.texture(draw.surface_keys[sg.first as usize]) {
                            Some((t, _)) => Some(&**t),
                            None => continue,
                        },
                        _ => None,
                    };
                    enc.setRenderPipelineState(pso);
                    // SAFETY: the pointers are valid for the lengths given; setBytes copies them. Buffers, textures and the
                    // sampler are live and bound to slots the shader declares; the draw reads `count` instances from the
                    // bound offset, inside the ring buffer.
                    unsafe {
                        enc.setVertexBytes_length_atIndex(
                            NonNull::new(fb.as_ptr().cast_mut().cast()).unwrap(),
                            fb.len(),
                            slot(vs, "frame"),
                        );
                        if sg.kind != SegKind::Sprite {
                            enc.setFragmentBytes_length_atIndex(
                                NonNull::new(fb.as_ptr().cast_mut().cast()).unwrap(),
                                fb.len(),
                                slot(fs, "frame"),
                            );
                        }
                        enc.setVertexBuffer_offset_atIndex(
                            Some(buf),
                            sg.first as usize * stride,
                            slot(vs, "insts"),
                        );
                        match sg.kind {
                            // Drawn by kip.rs above, never here.
                            SegKind::Kip => {}
                            SegKind::Panel | SegKind::Line | SegKind::Sprite => enc
                                .setFragmentBuffer_offset_atIndex(
                                    Some(buf),
                                    sg.first as usize * stride,
                                    slot(fs, "insts"),
                                ),
                            SegKind::Path => {
                                enc.setFragmentBuffer_offset_atIndex(
                                    Some(buf),
                                    sg.first as usize * stride,
                                    slot(fs, "insts"),
                                );
                                enc.setFragmentBuffer_offset_atIndex(Some(&eb), 0, slot(fs, "edges"));
                            }
                            SegKind::Image => {
                                enc.setFragmentBuffer_offset_atIndex(
                                    Some(buf),
                                    sg.first as usize * stride,
                                    slot(fs, "insts"),
                                );
                                enc.setFragmentTexture_atIndex(
                                    Some(&texs[sg.first as usize]),
                                    slot(fs, "tex"),
                                );
                            }
                            SegKind::Surface => {
                                enc.setFragmentBuffer_offset_atIndex(
                                    Some(buf),
                                    sg.first as usize * stride,
                                    slot(fs, "insts"),
                                );
                                enc.setFragmentTexture_atIndex(live, slot(fs, "tex"));
                                enc.setFragmentSamplerState_atIndex(Some(&self.sampler), slot(fs, "smp"));
                            }
                            SegKind::Glyph => {
                                enc.setFragmentTexture_atIndex(Some(&self.atlas), slot(fs, "atlas"));
                                enc.setFragmentSamplerState_atIndex(
                                    Some(&self.sampler),
                                    slot(fs, "smp"),
                                );
                                enc.setFragmentBuffer_offset_atIndex(
                                    Some(&self.curves),
                                    0,
                                    slot(fs, "curves"),
                                );
                                enc.setFragmentBuffer_offset_atIndex(
                                    Some(&self.bands),
                                    0,
                                    slot(fs, "bands"),
                                );
                            }
                        }
                        enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                            MTLPrimitiveType::TriangleStrip,
                            0,
                            4,
                            sg.count as usize,
                        );
                    }
                }
                enc.endEncoding();
            }
            // A pass drawn over an earlier one's drawable (a phone's overlays over its view) is presented by that one.
            if let Some(d) = drawable.filter(|_| !pass.over) {
                on_screen = true;
                if d.layer().presentsWithTransaction() {
                    with_transaction.push(d);
                } else {
                    cmd.presentDrawable(ProtocolObject::from_ref(&*d));
                }
            }
        }
        drop(media);
        let (sem, gpu_ms) = (self.sem.clone(), self.gpu_ms.clone());
        let done = RcBlock::new(move |cb: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
            // SAFETY: Metal hands the completed command buffer, alive for the handler.
            let cb = unsafe { cb.as_ref() };
            let ms = ((cb.GPUEndTime() - cb.GPUStartTime()) * 1000.0) as f32;
            gpu_ms.store(ms.max(0.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
            // Media may now write over or let go what this frame sampled.
            frames_done.fetch_max(serial, std::sync::atomic::Ordering::Release);
            sem.signal();
        });
        // SAFETY: Metal copies the block.
        unsafe { cmd.addCompletedHandler(RcBlock::as_ptr(&done)) };
        cmd.commit();
        // A layer that presents with its transaction shows its new picture in the same Core Animation commit as its window's
        // new frame (Apple's CAMetalLayer docs: wait until scheduled, then present the drawable itself).
        if !with_transaction.is_empty() {
            cmd.waitUntilScheduled();
            for d in &with_transaction {
                d.present();
            }
        }
        if !on_screen {
            cmd.waitUntilCompleted();
        }
    }

    /// Read an offscreen texture back as tightly packed RGBA8 bytes, extended range clipped to SDR.
    pub fn read(&self, t: &Tex) -> Vec<u8> {
        self.read_float(t).into_iter().map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8).collect()
    }

    /// Read an offscreen texture back as it is: RGBA floats, sRGB-encoded and extended (past 1.0 is brighter than SDR
    /// white, up to the frame's headroom), premultiplied.
    pub fn read_float(&self, t: &Tex) -> Vec<f32> {
        let (w, h) = (t.width(), t.height());
        let mut v = vec![0u16; w * h * 4];
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize { width: w, height: h, depth: 1 },
        };
        // SAFETY: the buffer is w*h*8 bytes, one RGBA16Float texel per 8 bytes, and the texture is CPU readable (shared).
        unsafe {
            t.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(v.as_mut_ptr().cast()).unwrap(),
                w * 8,
                region,
                0,
            )
        };
        v.into_iter().map(half_to_f32).collect()
    }
}

/// The part of a `size` texture a Surface shows over `rect` (points): with `cover`, the middle of it at the box's aspect
/// (cropped, never stretched); without, all of it.
fn cover_uv(rect: [f32; 4], cover: bool, size: [u32; 2]) -> [f32; 4] {
    let (bw, bh) = (rect[2].max(1e-3), rect[3].max(1e-3));
    let (tw, th) = (size[0].max(1) as f32, size[1].max(1) as f32);
    if !cover {
        return [0.0, 0.0, 1.0, 1.0];
    }
    let (box_aspect, tex_aspect) = (bw / bh, tw / th);
    if tex_aspect > box_aspect {
        // Wider than the box: crop left and right.
        let k = box_aspect / tex_aspect;
        [0.5 - k / 2.0, 0.0, 0.5 + k / 2.0, 1.0]
    } else {
        // Taller: crop top and bottom.
        let k = tex_aspect / box_aspect;
        [0.0, 0.5 - k / 2.0, 1.0, 0.5 + k / 2.0]
    }
}

/// An IEEE half float's bits as an f32.
pub(crate) fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let frac = f32::from(h & 0x3ff);
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        31 => {
            if frac == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}
