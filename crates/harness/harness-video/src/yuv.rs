//! Frames to Y'CbCr on the GPU: a Metal compute pass takes the renderer's extended-range target (sRGB-encoded half
//! floats, 1.0 is SDR white, above it brighter) to the two planes of the encoder's own pixel buffer (4:2:0 biplanar, video
//! range, IOSurface-backed and mapped into Metal by CoreVideo, see `encode.rs`). No byte of a frame crosses the CPU. One
//! kernel per colour a preset can ask for:
//!
//! - `p010`, HDR10: BT.2020 primaries, the SMPTE ST 2084 (PQ) curve, 10-bit Y'CbCr (BT.2020 non-constant luminance),
//!   codes MSB-aligned in 16 bits (`x420`: an `R16Unorm` luma plane, an `RG16Unorm` CbCr plane at half size). SDR white
//!   is placed at 203 nits (ITU-R BT.2408's reference white for graphics in HDR), so the UI reads as it does on an SDR
//!   screen and only what the renderer draws above 1.0 (the mark's fire, white-hot letters) rises past it, up to the
//!   1000-nit mastering peak, where it clips.
//! - `nv12`, SDR: the frame's own R'G'B' clipped at SDR white, 8-bit Y'CbCr by the BT.709 matrix (`420v`: `R8Unorm`
//!   and `RG8Unorm`). The values keep the sRGB curve they were drawn with and the stream is tagged BT.709, as web video
//!   is: players and X show a BT.709 stream on an sRGB screen without converting between the two curves, so the film's
//!   colours are the app's.
//!
//! Both site chroma left (between the two rows, on the left luma column), the default for H.264 and what HDR10 wants.
use std::ptr::NonNull;

use harness_native_render::gpu::{Dev, Tex};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;

use crate::preset::Colour;

/// Nits of SDR reference white (BT.2408).
pub const WHITE_NITS: f32 = 203.0;
/// The mastering display's peak, nits; brighter is clipped here.
pub const PEAK_NITS: f32 = 1000.0;

const KERNEL: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Params { uint w; uint h; float white; float peak; };

static float srgb_linear(float v) {
    float a = fabs(v);
    float l = a <= 0.04045f ? a / 12.92f : pow((a + 0.055f) / 1.055f, 2.4f);
    return copysign(l, v);
}

// ST 2084 inverse EOTF: nits to signal.
static float pq(float nits) {
    float y = pow(clamp(nits / 10000.0f, 0.0f, 1.0f), 0.1593017578125f);
    return pow((0.8359375f + 18.8515625f * y) / (1.0f + 18.6875f * y), 78.84375f);
}

// One texel to PQ R'G'B' over BT.2020; `nits` gets its brightest channel.
static float3 hdr(texture2d<half, access::read> t, int x, int y, constant Params& p, thread float& nits) {
    uint2 at = uint2(clamp(x, 0, int(p.w) - 1), clamp(y, 0, int(p.h) - 1));
    float3 s = float3(t.read(at).rgb);
    float3 l = float3(srgb_linear(s.r), srgb_linear(s.g), srgb_linear(s.b));
    // BT.709 to BT.2020 primaries, linear light (ITU-R BT.2087).
    float3 r = float3(dot(float3(0.627404f, 0.329283f, 0.043313f), l),
                      dot(float3(0.069097f, 0.919540f, 0.011362f), l),
                      dot(float3(0.016391f, 0.088013f, 0.895595f), l));
    float3 n = min(max(r, 0.0f) * p.white, p.peak);
    nits = max(nits, max(n.r, max(n.g, n.b)));
    return float3(pq(n.r), pq(n.g), pq(n.b));
}

static float luma(float3 c) { return dot(float3(0.2627f, 0.6780f, 0.0593f), c); }
// A 10-bit code, MSB-aligned in 16 bits, as the unorm value that stores those exact bits.
static float code(float v) { return float(ushort(clamp(rint(v), 4.0f, 1019.0f)) << 6) / 65535.0f; }

// One texel as it was drawn, clipped to SDR; `top` gets its brightest channel before the clip, in nits at SDR white.
static float3 sdr(texture2d<half, access::read> t, int x, int y, constant Params& p, thread float& nits) {
    uint2 at = uint2(clamp(x, 0, int(p.w) - 1), clamp(y, 0, int(p.h) - 1));
    float3 s = float3(t.read(at).rgb);
    nits = max(nits, min(max(s.r, max(s.g, s.b)), 1.0f) * p.white);
    return clamp(s, 0.0f, 1.0f);
}

static float luma709(float3 c) { return dot(float3(0.2126f, 0.7152f, 0.0722f), c); }
// An 8-bit code as the unorm value that stores it.
static float code8(float v, float lo, float hi) { return clamp(rint(v), lo, hi) / 255.0f; }

// As p010 below, for SDR: 8-bit BT.709, video range (luma 16 to 235, chroma 16 to 240).
kernel void nv12(texture2d<half, access::read> src [[texture(0)]],
                 texture2d<float, access::write> luma_plane [[texture(1)]],
                 texture2d<float, access::write> chroma_plane [[texture(2)]],
                 device atomic_uint* top [[buffer(0)]],
                 constant Params& p [[buffer(1)]],
                 uint2 g [[thread_position_in_grid]]) {
    if (g.x >= p.w / 2 || g.y >= p.h / 2) return;
    int x = int(g.x) * 2, y = int(g.y) * 2;
    float nits = 0.0f, spare = 0.0f;
    float3 a0 = sdr(src, x, y, p, nits), b0 = sdr(src, x + 1, y, p, nits);
    float3 a1 = sdr(src, x, y + 1, p, nits), b1 = sdr(src, x + 1, y + 1, p, nits);
    float3 l0 = sdr(src, x - 1, y, p, spare), l1 = sdr(src, x - 1, y + 1, p, spare);
    uint2 at = uint2(x, y);
    luma_plane.write(float4(code8(16.0f + 219.0f * luma709(a0), 16.0f, 235.0f)), at);
    luma_plane.write(float4(code8(16.0f + 219.0f * luma709(b0), 16.0f, 235.0f)), at + uint2(1, 0));
    luma_plane.write(float4(code8(16.0f + 219.0f * luma709(a1), 16.0f, 235.0f)), at + uint2(0, 1));
    luma_plane.write(float4(code8(16.0f + 219.0f * luma709(b1), 16.0f, 235.0f)), at + uint2(1, 1));
    float3 c = ((l0 + 2.0f * a0 + b0) + (l1 + 2.0f * a1 + b1)) / 8.0f;
    float yc = luma709(c);
    float cb = code8(128.0f + 224.0f * (c.b - yc) / 1.8556f, 16.0f, 240.0f);
    float cr = code8(128.0f + 224.0f * (c.r - yc) / 1.5748f, 16.0f, 240.0f);
    chroma_plane.write(float4(cb, cr, 0.0f, 0.0f), g);
    float m = simd_max(nits);
    if (simd_is_first()) atomic_fetch_max_explicit(top, as_type<uint>(m), memory_order_relaxed);
}

// One thread per 2x2 block: four luma samples and one chroma pair, filtered [1 2 1] across (left sited) and averaged
// down the two rows.
kernel void p010(texture2d<half, access::read> src [[texture(0)]],
                 texture2d<float, access::write> luma_plane [[texture(1)]],
                 texture2d<float, access::write> chroma_plane [[texture(2)]],
                 device atomic_uint* top [[buffer(0)]],
                 constant Params& p [[buffer(1)]],
                 uint2 g [[thread_position_in_grid]]) {
    if (g.x >= p.w / 2 || g.y >= p.h / 2) return;
    int x = int(g.x) * 2, y = int(g.y) * 2;
    float nits = 0.0f, spare = 0.0f;
    float3 a0 = hdr(src, x, y, p, nits), b0 = hdr(src, x + 1, y, p, nits);
    float3 a1 = hdr(src, x, y + 1, p, nits), b1 = hdr(src, x + 1, y + 1, p, nits);
    float3 l0 = hdr(src, x - 1, y, p, spare), l1 = hdr(src, x - 1, y + 1, p, spare);
    uint2 at = uint2(x, y);
    luma_plane.write(float4(code(64.0f + 876.0f * luma(a0))), at);
    luma_plane.write(float4(code(64.0f + 876.0f * luma(b0))), at + uint2(1, 0));
    luma_plane.write(float4(code(64.0f + 876.0f * luma(a1))), at + uint2(0, 1));
    luma_plane.write(float4(code(64.0f + 876.0f * luma(b1))), at + uint2(1, 1));
    float3 c = ((l0 + 2.0f * a0 + b0) + (l1 + 2.0f * a1 + b1)) / 8.0f;
    float yc = luma(c);
    float cb = code(512.0f + 896.0f * (c.b - yc) / 1.8814f), cr = code(512.0f + 896.0f * (c.r - yc) / 1.4746f);
    chroma_plane.write(float4(cb, cr, 0.0f, 0.0f), g);
    // Non-negative floats order as their bits do: one atomic per SIMD group for the frame's brightest pixel.
    float m = simd_max(nits);
    if (simd_is_first()) atomic_fetch_max_explicit(top, as_type<uint>(m), memory_order_relaxed);
}
"#;

#[repr(C)]
struct Params {
    w: u32,
    h: u32,
    white: f32,
    peak: f32,
}

/// The compute pass for one colour and its peak buffer, made once for a video's size.
pub struct Yuv {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pso: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    top: Retained<ProtocolObject<dyn MTLBuffer>>,
    w: u32,
    h: u32,
}

impl Yuv {
    pub fn new(dev: &Dev, w: u32, h: u32, colour: Colour) -> Result<Self, String> {
        let entry = match colour {
            Colour::Hdr10 => "p010",
            Colour::Bt709 => "nv12",
        };
        let lib = dev
            .newLibraryWithSource_options_error(&NSString::from_str(KERNEL), None)
            .map_err(|e| format!("yuv: the kernels do not compile: {e}"))?;
        let f = lib
            .newFunctionWithName(&NSString::from_str(entry))
            .ok_or_else(|| format!("yuv: the kernels have no `{entry}` function"))?;
        let pso = dev
            .newComputePipelineStateWithFunction_error(&f)
            .map_err(|e| format!("yuv: pipeline: {e}"))?;
        let queue = dev.newCommandQueue().ok_or("yuv: no Metal command queue")?;
        let top = dev
            .newBufferWithLength_options(4, MTLResourceOptions::StorageModeShared)
            .ok_or("yuv: no peak buffer")?;
        // SAFETY: a 4-byte shared buffer, written before any GPU use.
        unsafe { top.contents().cast::<u32>().write(0) };
        Ok(Self { queue, pso, top, w, h })
    }

    /// Convert one rendered frame (the renderer has finished with `src`) into a pixel buffer's planes, `luma` (`w x h`)
    /// and `chroma` (`w/2 x h/2`), 16 bits a sample for HDR10 and 8 for SDR, and wait for it, so `src` can be drawn over next.
    pub fn convert(&self, src: &Tex, luma: &Tex, chroma: &Tex) -> Result<(), String> {
        let sizes = [(luma.width(), luma.height()), (chroma.width(), chroma.height())];
        let (w, h) = (self.w as usize, self.h as usize);
        if sizes != [(w, h), (w / 2, h / 2)] {
            return Err(format!("yuv: planes {sizes:?} for a {w}x{h} frame (want luma {w}x{h}, chroma {}x{})", w / 2, h / 2));
        }
        let cmd = self.queue.commandBuffer().ok_or("yuv: no command buffer")?;
        let enc = cmd.computeCommandEncoder().ok_or("yuv: no compute encoder")?;
        let p = Params { w: self.w, h: self.h, white: WHITE_NITS, peak: PEAK_NITS };
        enc.setComputePipelineState(&self.pso);
        // SAFETY: indices match the kernel's bindings; Params is repr(C) and matches its struct; the textures are borrowed
        // and the peak buffer owned by self, all outliving the command buffer (we wait on it below); the planes were
        // checked to be the sizes the kernel writes.
        unsafe {
            enc.setTexture_atIndex(Some(src), 0);
            enc.setTexture_atIndex(Some(luma), 1);
            enc.setTexture_atIndex(Some(chroma), 2);
            enc.setBuffer_offset_atIndex(Some(&self.top), 0, 0);
            enc.setBytes_length_atIndex(
                NonNull::from(&p).cast(),
                std::mem::size_of::<Params>(),
                1,
            );
        }
        let tg = self.pso.threadExecutionWidth();
        enc.dispatchThreads_threadsPerThreadgroup(
            MTLSize { width: (self.w / 2) as usize, height: (self.h / 2) as usize, depth: 1 },
            MTLSize { width: tg, height: (self.pso.maxTotalThreadsPerThreadgroup() / tg).max(1), depth: 1 },
        );
        enc.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        if let Some(e) = cmd.error() {
            return Err(format!("yuv: the pass failed: {e}"));
        }
        Ok(())
    }

    /// The brightest pixel of every frame so far, nits (after the clip).
    pub fn peak_nits(&self) -> f32 {
        // SAFETY: the 4-byte shared buffer, not being written (no pass in flight).
        f32::from_bits(unsafe { self.top.contents().cast::<u32>().read() })
    }
}
