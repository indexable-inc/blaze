//! Kip's Metal pass (copied from den-native's src/mascot/gpu.rs): each Kip's picture rendered off screen at its own
//! device pixels (4x MSAA with depth, both memoryless on Apple GPUs, resolved into a half-float texture), then set into
//! the frame by a composite quad over its contact shadow (shaders/mascot.slang). harness-native-kip says what to draw.
//!
//! A picture renders again only when its uniforms change (its pose, its look, its size), so a still Kip costs one quad a
//! frame and nothing when nothing else draws. The renders go in a command buffer of their own on the frame's queue,
//! committed just before the frame's, so the frame samples them finished (Metal orders tracked textures on one queue).
use crate::gpu::PIXEL_FORMAT;
use crate::pack::Frame;
use harness_native_kip::MascotQuad;
use harness_native_kip::model::Vertex;
use bytemuck::{Pod, Zeroable};
use dispatch2::DispatchData;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use std::collections::HashMap;
use std::ptr::NonNull;

mod bindings {
    include!(concat!(env!("OUT_DIR"), "/mascot_bindings.rs"));
}

static METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mascot.metallib"));

/// Samples a pixel of the picture takes.
const SAMPLES: usize = 4;
/// Frames a mascot may go undrawn before its picture is let go.
const FORGET: u64 = 300;

type Dev = Retained<ProtocolObject<dyn MTLDevice>>;
type Tex = Retained<ProtocolObject<dyn MTLTexture>>;
type Buf = Retained<ProtocolObject<dyn MTLBuffer>>;
type Pso = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

/// `MascotQuad` in shaders/mascot.slang.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
struct QuadParams {
    rect: [f32; 4],
    shape: [f32; 4],
    color: [f32; 4],
}

struct Picture {
    tex: Tex,
    msaa: Tex,
    depth: Tex,
    /// What it holds, once rendered.
    shown: Option<harness_native_kip::Uniforms>,
    seen: u64,
}

/// Every Kip's picture, its pipelines and the meshes they draw.
pub struct Mascots {
    dev: Dev,
    mesh: Pso,
    composite: Pso,
    depth: Retained<ProtocolObject<dyn MTLDepthStencilState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    memoryless: bool,
    meshes: HashMap<u64, (Buf, usize)>,
    pictures: HashMap<u64, Picture>,
    serial: u64,
}

fn slot(entry: &str, param: &str) -> usize {
    bindings::BINDINGS.iter().find(|b| b.0 == entry && b.1 == param).unwrap_or_else(|| panic!("no binding {entry}.{param}")).3 as usize
}

fn bytes<T: Pod>(v: &T) -> (NonNull<std::ffi::c_void>, usize) {
    let b = bytemuck::bytes_of(v);
    (NonNull::new(b.as_ptr().cast_mut().cast()).expect("live bytes"), b.len())
}

impl Mascots {
    pub fn new(dev: &Dev) -> Self {
        let data = DispatchData::from_bytes(METALLIB);
        let lib = dev.newLibraryWithData_error(&data).expect("mascot metallib");
        let f = |n: &str| lib.newFunctionWithName(&NSString::from_str(n)).unwrap_or_else(|| panic!("shader entry {n} missing from mascot.metallib"));

        let desc = MTLRenderPipelineDescriptor::new();
        desc.setVertexFunction(Some(&f("mascot_vs")));
        desc.setFragmentFunction(Some(&f("mascot_fs")));
        desc.setRasterSampleCount(SAMPLES);
        desc.setDepthAttachmentPixelFormat(MTLPixelFormat::Depth32Float);
        // SAFETY: attachment 0 always exists.
        unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(PIXEL_FORMAT);
        let mesh = dev.newRenderPipelineStateWithDescriptor_error(&desc).unwrap_or_else(|e| panic!("pipeline mascot_vs/mascot_fs: {e}"));

        let desc = MTLRenderPipelineDescriptor::new();
        desc.setVertexFunction(Some(&f("mascot_quad_vs")));
        desc.setFragmentFunction(Some(&f("mascot_quad_fs")));
        // SAFETY: attachment 0 always exists.
        let att = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
        att.setPixelFormat(PIXEL_FORMAT);
        att.setBlendingEnabled(true);
        att.setSourceRGBBlendFactor(MTLBlendFactor::One);
        att.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        att.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        att.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        let composite = dev.newRenderPipelineStateWithDescriptor_error(&desc).unwrap_or_else(|e| panic!("pipeline mascot_quad_vs/mascot_quad_fs: {e}"));

        let dd = MTLDepthStencilDescriptor::new();
        dd.setDepthCompareFunction(MTLCompareFunction::Less);
        dd.setDepthWriteEnabled(true);
        let depth = dev.newDepthStencilStateWithDescriptor(&dd).expect("mascot depth state");

        let sd = MTLSamplerDescriptor::new();
        sd.setMinFilter(MTLSamplerMinMagFilter::Linear);
        sd.setMagFilter(MTLSamplerMinMagFilter::Linear);
        sd.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
        sd.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
        let sampler = dev.newSamplerStateWithDescriptor(&sd).expect("mascot sampler");
        let memoryless = dev.supportsFamily(MTLGPUFamily::Apple1);
        Self { dev: dev.clone(), mesh, composite, depth, sampler, memoryless, meshes: HashMap::new(), pictures: HashMap::new(), serial: 0 }
    }

    fn texture(&self, format: MTLPixelFormat, (w, h): (usize, usize), samples: usize) -> Option<Tex> {
        // SAFETY: a plain 2D descriptor of a supported format and a positive size.
        let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(format, w.max(1), h.max(1), false) };
        if samples > 1 {
            d.setTextureType(MTLTextureType::Type2DMultisample);
            // SAFETY: 4 samples, which every Metal GPU supports for these formats.
            unsafe { d.setSampleCount(samples) };
            d.setUsage(MTLTextureUsage::RenderTarget);
            d.setStorageMode(if self.memoryless { MTLStorageMode::Memoryless } else { MTLStorageMode::Private });
        } else {
            d.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            d.setStorageMode(MTLStorageMode::Private);
        }
        self.dev.newTextureWithDescriptor(&d)
    }

    fn mesh_of(&mut self, q: &MascotQuad) -> Option<(Buf, usize)> {
        if let Some(m) = self.meshes.get(&q.mesh_key) {
            return Some(m.clone());
        }
        let bytes: &[u8] = bytemuck::cast_slice::<Vertex, u8>(&q.mesh);
        // SAFETY: the bytes are live for the call and Metal copies them into the new buffer.
        let buf = unsafe { self.dev.newBufferWithBytes_length_options(NonNull::new(bytes.as_ptr().cast_mut().cast())?, bytes.len().max(16), MTLResourceOptions::StorageModeShared) }?;
        self.meshes.insert(q.mesh_key, (buf.clone(), q.mesh.len()));
        Some((buf, q.mesh.len()))
    }

    /// Before a frame that draws `quads`: render each picture whose uniforms changed, in a command buffer of its own on
    /// `queue` committed now (before the frame's).
    pub fn prepare(&mut self, queue: &ProtocolObject<dyn MTLCommandQueue>, quads: &[MascotQuad]) {
        self.serial += 1;
        let serial = self.serial;
        let mut cmd = None;
        for q in quads {
            let size = (q.pixels[0] as usize, q.pixels[1] as usize);
            let fits = self.pictures.get(&q.id).is_some_and(|p| (p.tex.width(), p.tex.height()) == size);
            if !fits {
                let (Some(tex), Some(msaa), Some(depth)) = (self.texture(PIXEL_FORMAT, size, 1), self.texture(PIXEL_FORMAT, size, SAMPLES), self.texture(MTLPixelFormat::Depth32Float, size, SAMPLES))
                else {
                    continue;
                };
                self.pictures.insert(q.id, Picture { tex, msaa, depth, shown: None, seen: serial });
            }
            let Some(p) = self.pictures.get_mut(&q.id) else { continue };
            p.seen = serial;
            if p.shown == Some(q.uniforms) {
                continue;
            }
            let Some((verts, count)) = self.mesh_of(q) else { continue };
            let Some(p) = self.pictures.get_mut(&q.id) else { continue };
            if cmd.is_none() {
                cmd = queue.commandBuffer();
            }
            let Some(cb) = &cmd else { return };
            let pass = MTLRenderPassDescriptor::new();
            // SAFETY: attachment 0 always exists.
            let ca = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            ca.setTexture(Some(&p.msaa));
            ca.setResolveTexture(Some(&p.tex));
            ca.setLoadAction(MTLLoadAction::Clear);
            ca.setClearColor(MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 0.0 });
            ca.setStoreAction(MTLStoreAction::MultisampleResolve);
            let da = pass.depthAttachment();
            da.setTexture(Some(&p.depth));
            da.setLoadAction(MTLLoadAction::Clear);
            da.setClearDepth(1.0);
            da.setStoreAction(MTLStoreAction::DontCare);
            let Some(enc) = cb.renderCommandEncoderWithDescriptor(&pass) else { continue };
            enc.setRenderPipelineState(&self.mesh);
            enc.setDepthStencilState(Some(&self.depth));
            let (ptr, len) = bytes(&q.uniforms);
            // SAFETY: live bytes (under 4 KB), copied by setBytes; the mesh buffer is live and holds `count` vertices.
            unsafe {
                enc.setVertexBytes_length_atIndex(ptr, len, slot("mascot_vs", "mf"));
                enc.setFragmentBytes_length_atIndex(ptr, len, slot("mascot_fs", "mf"));
                enc.setVertexBuffer_offset_atIndex(Some(&verts), 0, slot("mascot_vs", "verts"));
                enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, count);
            }
            enc.endEncoding();
            p.shown = Some(q.uniforms);
        }
        self.pictures.retain(|_, p| serial - p.seen < FORGET);
        if let Some(cb) = cmd {
            cb.commit();
        }
    }

    /// Draw `quads` into the pass `enc` is encoding: each mascot's shadow, then its picture.
    pub fn draw(&self, enc: &ProtocolObject<dyn MTLRenderCommandEncoder>, frame: &Frame, quads: &[MascotQuad]) {
        let mut bound = false;
        for q in quads {
            let Some(p) = self.pictures.get(&q.id).filter(|p| p.shown.is_some()) else { continue };
            if !bound {
                enc.setRenderPipelineState(&self.composite);
                let (ptr, len) = bytes(frame);
                // SAFETY: live bytes, copied by setBytes; a live sampler at a slot the shader declares.
                unsafe {
                    enc.setVertexBytes_length_atIndex(ptr, len, slot("mascot_quad_vs", "frame"));
                    enc.setFragmentSamplerState_atIndex(Some(&self.sampler), slot("mascot_quad_fs", "smp"));
                }
                bound = true;
            }
            let shadow = QuadParams { rect: q.shadow, shape: [q.alpha, 1.0, 0.0, 0.0], color: q.shadow_color };
            let picture = QuadParams { rect: q.rect, shape: [q.alpha, 0.0, 0.0, 0.0], color: [0.0; 4] };
            for params in [shadow, picture] {
                let (ptr, len) = bytes(&params);
                // SAFETY: live bytes, copied by setBytes; a live texture at a slot the shader declares; one quad.
                unsafe {
                    enc.setVertexBytes_length_atIndex(ptr, len, slot("mascot_quad_vs", "quad"));
                    enc.setFragmentBytes_length_atIndex(ptr, len, slot("mascot_quad_fs", "quad"));
                    enc.setFragmentTexture_atIndex(Some(&p.tex), slot("mascot_quad_fs", "src"));
                    enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
                }
            }
        }
    }
}
