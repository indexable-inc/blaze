//! The Metal renderer: pipelines from the Slang metallib, instance rings, one pass.
//!
//! Copied from den-native's gpu.rs (github.com/andrewgazelka/den) and stripped to what harness draws: panels, glyphs and
//! lines in paint order over a clear colour, plus den's outline glyphs (Slug) for the brand marks (icons.rs), whose curves
//! and bands are one static pair of buffers. den's backdrop blur chain, Apple glass, art cache, live textures, image atlas,
//! shader elements, mascots and 3D scenes are gone. Per-frame CPU-written buffers are rings of [`FRAMES`]
//! slots gated by a semaphore; the tiny uniforms go by setBytes.
pub use crate::draw::*;
use objc2_quartz_core::CAMetalDrawable as _;
use crate::pack::{GlyphInst, LineInst, PanelInst, SpriteInst};
use bytemuck::Pod;
use dispatch2::{DispatchData, DispatchRetained, DispatchSemaphore, DispatchTime};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use std::ffi::c_void;
use std::ptr::NonNull;

/// The one pixel format of the layer and every pipeline's colour attachment. Half floats: sRGB-encoded colour.
pub const PIXEL_FORMAT: MTLPixelFormat = MTLPixelFormat::RGBA16Float;
pub const FRAMES: usize = 3;

static METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/harness.metallib"));
include!(concat!(env!("OUT_DIR"), "/harness_bindings.rs"));

/// Slot of an entry point's parameter, from the table build.rs read out of the generated MSL.
fn slot(entry: &str, param: &str) -> usize {
    BINDINGS.iter().find(|b| b.0 == entry && b.1 == param).unwrap_or_else(|| panic!("no binding {entry}.{param}")).3 as usize
}

pub type Dev = Retained<ProtocolObject<dyn MTLDevice>>;
pub type Tex = Retained<ProtocolObject<dyn MTLTexture>>;
type Buf = Retained<ProtocolObject<dyn MTLBuffer>>;
type Pso = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

pub enum Dest {
    Drawable(Retained<ProtocolObject<dyn objc2_quartz_core::CAMetalDrawable>>),
    Texture(Tex),
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
    sprite: Pso,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    pub atlas: Tex,
    /// The brand marks' Slug curves and bands (icons.rs), uploaded once.
    curves: Buf,
    bands: Buf,
    rings: [Option<Ring>; 4],
    slot: usize,
    sem: DispatchRetained<DispatchSemaphore>,
}

fn s(x: &str) -> Retained<NSString> {
    NSString::from_str(x)
}

fn pipeline(dev: &Dev, lib: &Retained<ProtocolObject<dyn MTLLibrary>>, vs: &str, fs: &str) -> Pso {
    let desc = MTLRenderPipelineDescriptor::new();
    let f = |n: &str| lib.newFunctionWithName(&s(n)).unwrap_or_else(|| panic!("shader entry {n} missing from the metallib"));
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
    dev.newRenderPipelineStateWithDescriptor_error(&desc).unwrap_or_else(|e| panic!("pipeline {vs}/{fs}: {e}"))
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
        let ad = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::RGBA8Unorm, ATLAS_SIZE as usize, ATLAS_SIZE as usize, false) };
        ad.setUsage(MTLTextureUsage::ShaderRead);
        let atlas = dev.newTextureWithDescriptor(&ad).expect("atlas");
        let marks = crate::icons::marks();
        let fixed = |bytes: &[u8]| {
            let b = dev.newBufferWithLength_options(bytes.len().max(256), MTLResourceOptions::StorageModeShared).expect("outline buffer");
            // SAFETY: the buffer holds at least bytes.len() bytes of CPU-visible memory no GPU work uses yet.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), b.contents().as_ptr().cast::<u8>(), bytes.len()) };
            b
        };
        let curves = fixed(bytemuck::cast_slice(&marks.curves));
        let bands = fixed(bytemuck::cast_slice(&marks.bands));
        Self {
            panel: pipeline(&dev, &lib, "panel_vs", "panel_fs"),
            glyph: pipeline(&dev, &lib, "glyph_vs", "glyph_fs"),
            line: pipeline(&dev, &lib, "line_vs", "line_fs"),
            sprite: pipeline(&dev, &lib, "sprite_vs", "sprite_fs"),
            dev,
            queue,
            sampler,
            atlas,
            curves,
            bands,
            rings: [None, None, None, None],
            slot: 0,
            sem: DispatchSemaphore::new(FRAMES as isize),
        }
    }

    pub fn device(&self) -> &Dev {
        &self.dev
    }

    /// Upload glyph pixels (premultiplied RGBA) into the atlas (new texels only, so frames in flight never see them
    /// change).
    pub fn upload_atlas(&self, x: u32, y: u32, w: u32, h: u32, bytes: &[u8]) {
        let region = MTLRegion { origin: MTLOrigin { x: x as usize, y: y as usize, z: 0 }, size: MTLSize { width: w as usize, height: h as usize, depth: 1 } };
        debug_assert_eq!(bytes.len(), (w * h * 4) as usize);
        // SAFETY: bytes holds w*h tightly packed RGBA texels and stays alive for the call; the region lies inside the atlas.
        unsafe { self.atlas.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, NonNull::new(bytes.as_ptr().cast_mut().cast::<c_void>()).unwrap(), w as usize * 4) };
    }

    /// An offscreen destination (for screenshots), readable by [`Self::read`].
    pub fn offscreen(&self, w: usize, h: usize) -> Tex {
        // SAFETY: a plain 2D descriptor of a supported format and a positive size.
        let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(PIXEL_FORMAT, w.max(1), h.max(1), false) };
        d.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        d.setStorageMode(MTLStorageMode::Shared);
        self.dev.newTextureWithDescriptor(&d).expect("target texture")
    }

    fn ring<T: Pod>(&mut self, which: usize, slot: usize, items: &[T]) -> Buf {
        let need = std::mem::size_of_val(items).max(256);
        if self.rings[which].as_ref().is_none_or(|r| r.cap < need) {
            let cap = need.next_power_of_two();
            let bufs = (0..FRAMES).map(|_| self.dev.newBufferWithLength_options(cap, MTLResourceOptions::StorageModeShared).expect("ring buffer")).collect();
            self.rings[which] = Some(Ring { bufs, cap });
        }
        let buf = self.rings[which].as_ref().unwrap().bufs[slot].clone();
        // SAFETY: the buffer holds at least `need` bytes of CPU-visible memory, and the semaphore guarantees the GPU is done with this slot.
        unsafe { std::ptr::copy_nonoverlapping(bytemuck::cast_slice::<T, u8>(items).as_ptr(), buf.contents().as_ptr().cast::<u8>(), std::mem::size_of_val(items)) };
        buf
    }

    pub fn render(&mut self, draw: &Draw, dest: Dest) {
        self.sem.wait(DispatchTime::FOREVER);
        let ring_slot = self.slot;
        self.slot = (self.slot + 1) % FRAMES;
        let pb = self.ring(0, ring_slot, &draw.panels);
        let gb = self.ring(1, ring_slot, &draw.glyphs);
        let lb = self.ring(2, ring_slot, &draw.lines);
        let sb = self.ring(3, ring_slot, &draw.sprites);
        let Some(cmd) = self.queue.commandBuffer() else {
            self.sem.signal();
            return;
        };
        let (target, drawable) = match &dest {
            Dest::Drawable(d) => (d.texture(), Some(d.clone())),
            Dest::Texture(t) => (t.clone(), None),
        };
        let rp = MTLRenderPassDescriptor::new();
        // SAFETY: attachment 0 always exists.
        let ca = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
        ca.setTexture(Some(&target));
        ca.setStoreAction(MTLStoreAction::Store);
        ca.setLoadAction(MTLLoadAction::Clear);
        let c = draw.clear;
        let a = f64::from(c[3]);
        // Premultiplied, as every pipeline's blend and the layer's compositing take colour.
        ca.setClearColor(MTLClearColor { red: f64::from(c[0]) * a, green: f64::from(c[1]) * a, blue: f64::from(c[2]) * a, alpha: a });
        if let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&rp) {
            let fb = bytemuck::bytes_of(&draw.frame);
            for sg in &draw.segs {
                if sg.count == 0 {
                    continue;
                }
                let (pso, vs, fs, buf, stride) = match sg.kind {
                    SegKind::Panel => (&self.panel, "panel_vs", "panel_fs", &pb, std::mem::size_of::<PanelInst>()),
                    SegKind::Glyph => (&self.glyph, "glyph_vs", "glyph_fs", &gb, std::mem::size_of::<GlyphInst>()),
                    SegKind::Line => (&self.line, "line_vs", "line_fs", &lb, std::mem::size_of::<LineInst>()),
                    SegKind::Sprite => (&self.sprite, "sprite_vs", "sprite_fs", &sb, std::mem::size_of::<SpriteInst>()),
                };
                enc.setRenderPipelineState(pso);
                // SAFETY: the pointers are valid for the lengths given; setBytes copies them. Buffers, textures and the
                // sampler are live and bound to slots the shader declares; the draw reads `count` instances from the
                // bound offset, inside the ring buffer.
                unsafe {
                    enc.setVertexBytes_length_atIndex(NonNull::new(fb.as_ptr().cast_mut().cast()).unwrap(), fb.len(), slot(vs, "frame"));
                    if sg.kind != SegKind::Sprite {
                        enc.setFragmentBytes_length_atIndex(NonNull::new(fb.as_ptr().cast_mut().cast()).unwrap(), fb.len(), slot(fs, "frame"));
                    }
                    enc.setVertexBuffer_offset_atIndex(Some(buf), sg.first as usize * stride, slot(vs, "insts"));
                    match sg.kind {
                        SegKind::Panel | SegKind::Line | SegKind::Sprite => enc.setFragmentBuffer_offset_atIndex(Some(buf), sg.first as usize * stride, slot(fs, "insts")),
                        SegKind::Glyph => {
                            enc.setFragmentTexture_atIndex(Some(&self.atlas), slot(fs, "atlas"));
                            enc.setFragmentSamplerState_atIndex(Some(&self.sampler), slot(fs, "smp"));
                            enc.setFragmentBuffer_offset_atIndex(Some(&self.curves), 0, slot(fs, "curves"));
                            enc.setFragmentBuffer_offset_atIndex(Some(&self.bands), 0, slot(fs, "bands"));
                        }
                    }
                    enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::TriangleStrip, 0, 4, sg.count as usize);
                }
            }
            enc.endEncoding();
        }
        if let Some(d) = &drawable {
            cmd.presentDrawable(ProtocolObject::from_ref(&**d));
        }
        let sem = self.sem.clone();
        let done = RcBlock::new(move |_cb: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
            sem.signal();
        });
        // SAFETY: Metal copies the block.
        unsafe { cmd.addCompletedHandler(RcBlock::as_ptr(&done)) };
        cmd.commit();
        if drawable.is_none() {
            cmd.waitUntilCompleted();
        }
    }

    /// Read an offscreen texture back as tightly packed RGBA8 bytes, extended range clipped to SDR.
    pub fn read(&self, t: &Tex) -> Vec<u8> {
        let (w, h) = (t.width(), t.height());
        let mut v = vec![0u16; w * h * 4];
        let region = MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w, height: h, depth: 1 } };
        // SAFETY: the buffer is w*h*8 bytes, one RGBA16Float texel per 8 bytes, and the texture is CPU readable (shared).
        unsafe { t.getBytes_bytesPerRow_fromRegion_mipmapLevel(NonNull::new(v.as_mut_ptr().cast()).unwrap(), w * 8, region, 0) };
        v.into_iter().map(|h| (half_to_f32(h).clamp(0.0, 1.0) * 255.0).round() as u8).collect()
    }
}

/// An IEEE half float's bits as an f32.
fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let frac = f32::from(h & 0x3ff);
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        31 => if frac == 0.0 { sign * f32::INFINITY } else { f32::NAN },
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}
