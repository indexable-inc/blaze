//! A video's living picture: full-frame shaders under the UI. A scene whose root carries a [`Backdrop`] gets, each
//! frame, a procedural image drawn by a Metal compute kernel from the component's numbers (which `Cue`s animate and a
//! scene's data constants set, so the picture is data like everything else), with the UI (captions, figures) laid
//! over it, then film grain and a vignette. Two kernels, compiled by Metal at start-up from the source below, as
//! `yuv.rs`'s are: `backdrop` draws the picture into a half-float texture, `finish` lays the UI over it and grades it.
//!
//! The shots (`Backdrop.shot`), each a few dozen lines of the kernel, all made of points of light on near-black with
//! one accent:
//!
//! | shot | picture | numbers |
//! | --- | --- | --- |
//! | 0 | drift: three depths of slow points | |
//! | 1 | race: comets on lanes with motion-blurred tails, a flash at each finish, the losers still travelling | `a`, `b`, `c`: each lane's seconds (0: no lane); the first is the accent |
//! | 2 | graph: a field of nodes; a ring leaves one node and lights only it and its few readers | `a`: when the ring leaves (shot seconds) |
//! | 3 | core: a warm core with rings of points in orbit | |
//! | 4 | swarm: thousands of sparks, of which the share `a` live; the rest go dark as `a` falls | `a`: the live share |
//! | 5 | beam: time as a bar of light, `a` of the track long, over the ghost of its full length | `a`: the lit share |
//! | 6 | gather: every point streams to the middle, a warm glow growing there (the mark forms over it) | `a`: how far gathered |
//!
//! Colours are computed in linear light and written sRGB-encoded (extended), which is what the renderer's target and
//! the colour pass hold. Motion blur is analytic (a comet's tail is its path over the shutter), so any frame can still
//! be drawn alone.
use std::ptr::NonNull;

use harness_native_flecs::prelude::*;
use harness_native_render::gpu::{Dev, PIXEL_FORMAT, Tex};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;

/// The picture under a video's UI, on the node that carries the `Video`: which `shot` the kernel draws, when it began
/// (`at`, seconds on the frame clock: the kernel's own clock is the time since), and six numbers a shot reads (module
/// docs). Every member can be cued.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Backdrop {
    pub shot: f32,
    pub at: f32,
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

const KERNEL: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Params { uint w; uint h; float time; float shot; float t; float a; float b; float c; float d; float e; float f; uint frame; };

static float hash1(float2 p) { p = fract(p * float2(123.34f, 456.21f)); p += dot(p, p + 45.32f); return fract(p.x * p.y); }
static float2 hash2(float2 p) { float n = hash1(p); return float2(n, hash1(p + n * 17.13f)); }

constant float3 ACCENT = float3(1.0f, 0.30f, 0.03f);   // linear light: the film's orange
constant float3 COOL = float3(0.42f, 0.50f, 0.66f);
constant float3 GROUND = float3(0.0030f, 0.0034f, 0.0045f);

// A layer of points: one a cell of a grid `scale` cells to the frame's short side, each wandering a little; `live`
// says how bright a cell's point is (0 to 1) from its hash. Returns the light at `p`.
static float points(float2 p, float scale, float time, float size, float keep) {
    float2 q = p * scale;
    float2 cell = floor(q);
    float sum = 0.0f;
    for (int j = -1; j <= 1; j++) for (int i = -1; i <= 1; i++) {
        float2 c = cell + float2(i, j);
        float2 h = hash2(c);
        if (h.x > keep) continue;
        float2 at = c + 0.5f + 0.38f * float2(sin(time * (0.3f + h.x) + 6.283f * h.y), cos(time * (0.25f + h.y) + 6.283f * h.x));
        float d = length(q - at);
        float r = size * (0.6f + 0.8f * h.y);
        sum += exp(-d * d / (r * r)) * (0.35f + 0.65f * fract(h.x * 91.7f));
    }
    return sum;
}

// Three depths of drifting points: the film's air.
static float3 drift(float2 p, float time, float amount) {
    float3 c = float3(0.0f);
    c += COOL * 0.10f * points(p + float2(time * 0.010f, 0.0f), 9.0f, time, 0.050f, 0.55f);
    c += COOL * 0.16f * points(p + float2(time * 0.018f, 3.1f), 16.0f, time, 0.060f, 0.45f);
    c += COOL * 0.22f * points(p + float2(time * 0.030f, 7.7f), 30.0f, time, 0.075f, 0.30f);
    return c * amount;
}

// The distance from `p` to the segment a-b.
static float seg(float2 p, float2 a, float2 b) {
    float2 pa = p - a, ba = b - a;
    return length(pa - ba * clamp(dot(pa, ba) / dot(ba, ba), 0.0f, 1.0f));
}

// One comet of a race: on the lane at `y`, `took` seconds to cross from x0 to x1 (in `p`'s units); `t` since the start.
static float3 comet(float2 p, float y, float x0, float x1, float took, float t, float3 tint) {
    float k = clamp(t / took, 0.0f, 1.0f);
    float head = mix(x0, x1, k);
    float speed = (x1 - x0) / took;
    float3 c = float3(0.0f);
    // The lane, faint, and the finish.
    c += tint * 0.020f * exp(-pow((p.y - y) / 0.0022f, 2.0f)) * step(x0, p.x) * step(p.x, x1);
    // The tail: where the head has been, fading back over a length its speed sets (the shutter's blur and then some).
    float back = head - p.x;
    float len = clamp(0.10f + 0.22f * speed, 0.10f, 0.7f) * (k < 1.0f ? 1.0f : exp(-(t - took) * 5.0f));
    if (back > 0.0f && p.x > x0) {
        float along = clamp(1.0f - back / len, 0.0f, 1.0f);
        float wide = 0.004f + 0.010f * along;
        c += tint * 1.6f * along * along * exp(-pow((p.y - y) / wide, 2.0f));
    }
    // The head: a hot core and its bloom.
    float d = length(p - float2(head, y));
    // Landed, its bloom settles, so three finished heads stay three points and not one glare.
    float rest = k < 1.0f ? 1.0f : 0.25f + 0.75f * exp(-(t - took) * 4.0f);
    c += (tint + 0.6f) * 2.2f * exp(-d * d / 0.00016f) + rest * (tint * 0.55f * exp(-d * d / 0.0045f) + tint * 0.035f * exp(-d * d / 0.03f));
    // The finish: a flash and a ring leaving it.
    if (t > took) {
        float since = t - took;
        float2 end = float2(x1, y);
        float r = length(p - end);
        c += (tint + 0.8f) * 1.5f * exp(-since * 9.0f) * exp(-r * r / 0.006f);
        c += tint * 0.9f * exp(-since * 3.5f) * exp(-pow((r - since * 0.55f) / 0.006f, 2.0f));
    }
    return c;
}

kernel void backdrop(texture2d<half, access::write> out [[texture(0)]],
                     constant Params& u [[buffer(0)]],
                     uint2 g [[thread_position_in_grid]]) {
    if (g.x >= u.w || g.y >= u.h) return;
    float2 res = float2(u.w, u.h);
    float side = min(res.x, res.y);
    // Centred, the short side 1 across; `ext` is the frame's half extent in those units.
    float2 p = (float2(g) + 0.5f - 0.5f * res) / side;
    float2 ext = 0.5f * res / side;
    float t = u.t;
    int shot = int(u.shot + 0.5f);
    float3 c = GROUND;

    if (shot == 1) {
        // Race. Lanes a tenth of the height apart, centred; the track from 8% to 92% of the width.
        c += drift(p + float2(t * 0.12f, 0.0f), u.time, 0.55f);
        float took[3] = { u.a, u.b, u.c };
        int n = (u.c > 0.0f) ? 3 : 2;
        float x0 = -0.84f * ext.x, x1 = 0.84f * ext.x;
        float gap = 0.2f * ext.y;
        // The finish line.
        c += COOL * 0.05f * exp(-pow((p.x - x1) / 0.0015f, 2.0f)) * smoothstep(gap * 1.6f, gap * 0.9f, abs(p.y));
        for (int i = 0; i < n; i++) {
            float y = (float(i) - 0.5f * float(n - 1)) * gap;
            c += comet(p, y, x0, x1, max(took[i], 0.001f), t, i == 0 ? ACCENT : COOL);
        }
    } else if (shot == 2) {
        // Graph: calm nodes; a ring from the edited node lights it and its readers, and their edges.
        c += drift(p, u.time, 0.5f);
        c += COOL * 0.30f * points(p + float2(0.0f, 11.3f), 22.0f, u.time * 0.4f, 0.085f, 0.8f);
        float since = t - u.a;
        float2 src = float2(-0.10f, 0.02f);
        const float2 readers[5] = { float2(0.16f, -0.13f), float2(0.24f, 0.07f), float2(0.02f, 0.21f), float2(-0.27f, 0.12f), float2(-0.20f, -0.17f) };
        float ring = since * 0.55f;
        if (since > 0.0f) {
            float r = length(p - src);
            // The ring itself: thin, quickly gone. Everything it crosses stays dark.
            c += ACCENT * 0.5f * exp(-since * 1.6f) * exp(-pow((r - ring) / 0.006f, 2.0f));
            c += (ACCENT + 0.7f) * 2.4f * exp(-r * r / 0.00035f) + ACCENT * 0.5f * exp(-r * r / 0.012f);
            for (int i = 0; i < 5; i++) {
                float reach = length(readers[i] - src);
                float lit = smoothstep(reach, reach + 0.05f, ring);
                float2 tip = mix(src, readers[i], clamp(ring / reach, 0.0f, 1.0f));
                c += ACCENT * 0.35f * exp(-pow(seg(p, src, tip) / 0.0018f, 2.0f));
                float d = length(p - readers[i]);
                c += lit * ((ACCENT + 0.5f) * 1.6f * exp(-d * d / 0.00022f) + ACCENT * 0.35f * exp(-d * d / 0.006f));
            }
        } else {
            float r = length(p - src);
            c += COOL * 0.9f * exp(-r * r / 0.0003f);
        }
    } else if (shot == 3) {
        // Core: the daemon, warm, with everything in orbit round it.
        c += drift(p, u.time, 0.35f);
        float r = length(p);
        float breathe = 0.9f + 0.1f * sin(t * 2.2f);
        c += (ACCENT + 0.6f) * 1.8f * exp(-r * r / (0.0022f * breathe)) + ACCENT * 0.6f * exp(-r * r / 0.03f) + ACCENT * 0.10f * exp(-r * r / 0.25f);
        float ang = atan2(p.y, p.x);
        for (int k = 0; k < 4; k++) {
            float rad = 0.13f + 0.075f * float(k);
            float turn = t * (0.9f - 0.17f * float(k)) * (k % 2 == 0 ? 1.0f : -1.0f);
            float count = 9.0f + 6.0f * float(k);
            float a = (ang + turn) / 6.2832f * count;
            float id = floor(a);
            float h = hash1(float2(id, float(k) * 7.0f));
            float2 at = rad * (1.0f + 0.05f * (h - 0.5f)) * float2(cos((id + 0.5f) / count * 6.2832f - turn), sin((id + 0.5f) / count * 6.2832f - turn));
            float d = length(p - at);
            float3 tint = h < 0.25f ? ACCENT : COOL;
            c += tint * (0.5f + 0.9f * h) * exp(-d * d / 0.00007f) + tint * 0.08f * exp(-d * d / 0.002f);
            c += COOL * 0.012f * exp(-pow((r - rad) / 0.0012f, 2.0f));
        }
    } else if (shot == 4) {
        // Swarm: a spark a cell; the share `a` of them live. One whose hash is over `a` has gone dark; one near the
        // edge of going is hot. The field draws in as it thins.
        float a = clamp(u.a, 0.0f, 1.0f);
        float2 q = p * (1.0f + 0.25f * (1.0f - a)) * 46.0f;
        // The middle is kept dim: the count stands there.
        float room = 1.0f - 0.82f * exp(-(p.x * p.x / 0.16f + p.y * p.y / 0.012f));
        float2 cell = floor(q);
        for (int j = -1; j <= 1; j++) for (int i = -1; i <= 1; i++) {
            float2 cc = cell + float2(i, j);
            float2 h = hash2(cc);
            float2 at = cc + 0.5f + 0.36f * float2(sin(u.time * (0.6f + h.x) + 6.283f * h.y), cos(u.time * (0.5f + h.y) + 6.283f * h.x));
            float d = length(q - at);
            float alive = smoothstep(a + 0.02f, a, h.x);
            float dying = exp(-pow((h.x - a) / 0.03f, 2.0f)) * step(a, 0.985f);
            float kept = step(h.x, u.b);
            float3 tint = mix(COOL * 0.55f, ACCENT, max(kept, 0.25f));
            c += room * tint * alive * (0.55f + 0.6f * kept) * exp(-d * d / 0.035f);
            c += room * (ACCENT + 0.6f) * dying * 1.2f * exp(-d * d / 0.06f);
            c += room * ACCENT * kept * alive * 0.22f * exp(-d * d / 0.22f);
        }
    } else if (shot == 5) {
        // Beam: a time as a bar of light. Its full length stays as a ghost; the lit part is `a` of it.
        c += drift(p, u.time, 0.45f);
        float x0 = -0.80f * ext.x, x1 = 0.80f * ext.x;
        float a = clamp(u.a, 0.0f, 1.0f);
        float end = mix(x0, x1, a);
        float in_x = step(x0, p.x) * step(p.x, x1);
        c += COOL * 0.055f * exp(-pow(p.y / 0.010f, 2.0f)) * in_x * (0.6f + 0.4f * step(0.5f, fract((p.x - x0) * 40.0f)));
        float lit = step(x0, p.x) * smoothstep(end + 0.002f, end - 0.002f, p.x);
        c += (ACCENT + 0.35f) * 1.5f * lit * exp(-pow(p.y / 0.007f, 2.0f)) + ACCENT * 0.35f * lit * exp(-pow(p.y / 0.035f, 2.0f));
        float d = length(p - float2(end, 0.0f));
        c += (ACCENT + 0.7f) * 2.0f * exp(-d * d / 0.0003f) + ACCENT * 0.5f * exp(-d * d / 0.008f);
    } else if (shot == 6) {
        // Gather: the points stream inward; a warm glow grows where the mark forms.
        float a = clamp(u.a, 0.0f, 1.0f);
        float r = length(p);
        float ang = atan2(p.y, p.x);
        float2 polar = float2(ang / 6.2832f * 40.0f, log(r + 0.02f) * 9.0f + t * 1.6f);
        c += mix(COOL, ACCENT, 0.5f) * 0.5f * (1.0f - a) * points(polar, 1.0f, u.time, 0.16f, 0.5f) * smoothstep(0.02f, 0.25f, r);
        c += drift(p, u.time, 0.35f);
        c += ACCENT * (0.10f + 0.22f * a) * exp(-r * r / 0.10f);
    } else {
        c += drift(p, u.time, 1.0f);
    }
    // Each shot opens over a few frames, so a cut reads as light arriving.
    c = mix(GROUND, c, 0.35f + 0.65f * smoothstep(0.0f, 0.18f, t));
    // To the target's encoding: sRGB, extended.
    float3 e = pow(max(c, 0.0f), float3(1.0f / 2.2f));
    out.write(half4(half3(e), 1.0h), g);
}

// The UI (premultiplied, sRGB-encoded) over the picture, then a vignette on the picture and grain on it all.
kernel void finish(texture2d<half, access::read> picture [[texture(0)]],
                   texture2d<half, access::read> ui [[texture(1)]],
                   texture2d<half, access::write> out [[texture(2)]],
                   constant Params& u [[buffer(0)]],
                   uint2 g [[thread_position_in_grid]]) {
    if (g.x >= u.w || g.y >= u.h) return;
    float2 uv = (float2(g) + 0.5f) / float2(u.w, u.h);
    float3 bg = float3(picture.read(g).rgb);
    float2 v = uv - 0.5f;
    bg *= 1.0f - 0.55f * smoothstep(0.25f, 0.95f, dot(v, v) * 2.4f);
    float4 over = float4(ui.read(g));
    float3 c = over.rgb + bg * (1.0f - clamp(over.a, 0.0f, 1.0f));
    // Grain: one hash a pixel a frame, a little stronger in the shadows, so flat dark never bands in the encoder.
    float n = hash1(float2(g) + float2(float(u.frame % 977u) * 13.7f, float(u.frame % 631u) * 7.3f)) - 0.5f;
    c += n * (0.030f - 0.012f * clamp(dot(c, float3(0.333f)), 0.0f, 1.0f));
    out.write(half4(half3(max(c, 0.0f)), 1.0h), g);
}
"#;

#[repr(C)]
struct Params {
    w: u32,
    h: u32,
    time: f32,
    shot: f32,
    t: f32,
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
    frame: u32,
}

/// The two passes and their textures, made once for a video's size.
pub struct Pass {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    backdrop: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    finish: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    picture: Tex,
    out: Tex,
    w: u32,
    h: u32,
}

impl Pass {
    pub fn new(dev: &Dev, w: u32, h: u32) -> Result<Self, String> {
        let lib = dev
            .newLibraryWithSource_options_error(&NSString::from_str(KERNEL), None)
            .map_err(|e| format!("backdrop: the kernels do not compile: {e}"))?;
        let pipeline = |name: &str| -> Result<_, String> {
            let f = lib
                .newFunctionWithName(&NSString::from_str(name))
                .ok_or_else(|| format!("backdrop: the kernels have no `{name}` function"))?;
            dev.newComputePipelineStateWithFunction_error(&f).map_err(|e| format!("backdrop: pipeline {name}: {e}"))
        };
        let texture = || -> Result<Tex, String> {
            // SAFETY: a plain 2D descriptor of the renderer's own target format and a positive size.
            let d = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(PIXEL_FORMAT, w as usize, h as usize, false)
            };
            d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
            dev.newTextureWithDescriptor(&d).ok_or_else(|| format!("backdrop: no {w}x{h} texture"))
        };
        Ok(Self {
            queue: dev.newCommandQueue().ok_or("backdrop: no Metal command queue")?,
            backdrop: pipeline("backdrop")?,
            finish: pipeline("finish")?,
            picture: texture()?,
            out: texture()?,
            w,
            h,
        })
    }

    /// Draw frame `frame` (at `time` seconds): the picture `b` asks for, with `ui` (the renderer's finished target,
    /// drawn over a clear ground) laid over it. Returns the finished frame, which the next call draws over.
    pub fn draw(&self, b: &Backdrop, time: f32, frame: u32, ui: &Tex) -> Result<&Tex, String> {
        if (ui.width(), ui.height()) != (self.w as usize, self.h as usize) {
            return Err(format!("backdrop: the UI is {}x{}, the pass {}x{}", ui.width(), ui.height(), self.w, self.h));
        }
        let p = Params { w: self.w, h: self.h, time, shot: b.shot, t: time - b.at, a: b.a, b: b.b, c: b.c, d: b.d, e: b.e, f: b.f, frame };
        let cmd = self.queue.commandBuffer().ok_or("backdrop: no command buffer")?;
        for (pso, textures) in [(&self.backdrop, vec![&self.picture]), (&self.finish, vec![&self.picture, ui, &self.out])] {
            let enc = cmd.computeCommandEncoder().ok_or("backdrop: no compute encoder")?;
            enc.setComputePipelineState(pso);
            // SAFETY: indices match each kernel's bindings; Params is repr(C) and matches the kernels' struct (copied by
            // setBytes); the textures are owned by self or borrowed for this call, all outliving the command buffer (we
            // wait on it below), and are the size the kernels are told.
            unsafe {
                for (i, t) in textures.iter().enumerate() {
                    enc.setTexture_atIndex(Some(t), i);
                }
                enc.setBytes_length_atIndex(NonNull::from(&p).cast(), std::mem::size_of::<Params>(), 0);
            }
            let tg = pso.threadExecutionWidth();
            enc.dispatchThreads_threadsPerThreadgroup(
                MTLSize { width: self.w as usize, height: self.h as usize, depth: 1 },
                MTLSize { width: tg, height: (pso.maxTotalThreadsPerThreadgroup() / tg).max(1), depth: 1 },
            );
            enc.endEncoding();
        }
        cmd.commit();
        cmd.waitUntilCompleted();
        if let Some(e) = cmd.error() {
            return Err(format!("backdrop: the pass failed: {e}"));
        }
        Ok(&self.out)
    }
}
