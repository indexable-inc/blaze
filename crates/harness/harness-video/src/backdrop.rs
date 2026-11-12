//! A video's living picture: full-frame shaders under the UI. A scene whose root carries a [`Backdrop`] gets, each
//! frame, a procedural image drawn by a Metal compute kernel from the component's numbers (which `Cue`s animate and a
//! scene's data constants set, so the picture is data like everything else), with the UI (captions, figures) laid
//! over it, then film grain and a vignette. Two kernels, compiled by Metal at start-up from the source below, as
//! `yuv.rs`'s are: `backdrop` draws the picture into a half-float texture, `finish` lays the UI over it and grades it.
//!
//! The world is one sea of points the camera flies through without stopping; each beat's idea is a landmark of light
//! in it that the camera nears, holds on and flies through at the end of the beat (`f`: the beat's seconds). The
//! landmarks (`Backdrop.shot`): 0 the name's sky, the galaxy far off; 1 cage (a hexagon nothing leaves); 2 check; 3
//! agents streaming to a core; 4 rings that only narrow; 5 disk (at `a` it tears, time runs back, it tears again);
//! 6 proof tree; 7 prism; 8 mirror (two builds read against each other); 9 ripples (`a`, `b`, `c`: each ring's seconds
//! to the shore); 10 the sea at speed; 11 the galaxy (`a`: how far in, `b`: when one star is edited); 12 comb; 13
//! stair; 14 store; 15 cores; 16 the end, the sea streaming in; 17 the interrupt, a field of the accent.
//!
//! Every light is a Gaussian of a distance to a point, a segment or a curve, so it reaches zero on its own: nothing is
//! cut by a `step` at the end of a track or the edge of a shape (the fault the second film had: a beam's glow ended on
//! a straight line at its track's start). `no_light_ends_at_a_line` (main.rs's tests) draws every shot and fails on a
//! long straight run of sharp change.
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
constant float TAU = 6.2831853f;

// Every light here is a Gaussian of a distance: it reaches zero on its own, never at an edge of anything drawn
// (rule: a falloff ends inside its geometry; the test `no_light_ends_at_a_line` holds every shot to it).
static float glow(float d, float w) { return exp(-d * d / (w * w)); }
static float seg(float2 p, float2 a, float2 b) {
    float2 pa = p - a, ba = b - a;
    return length(pa - ba * clamp(dot(pa, ba) / max(dot(ba, ba), 1e-9f), 0.0f, 1.0f));
}
static float hexagon(float2 p, float r) { p = abs(p); return max(dot(p, float2(0.8660254f, 0.5f)), p.y) - r; }
static float2 turn(float2 p, float a) { float c = cos(a), s = sin(a); return float2(c * p.x - s * p.y, s * p.x + c * p.y); }

// A layer of points: one a cell of a unit grid, wandering a little; `keep` is the share of cells that have one.
static float points(float2 q, float time, float size, float keep) {
    float2 cell = floor(q);
    float sum = 0.0f;
    for (int j = -1; j <= 1; j++) for (int i = -1; i <= 1; i++) {
        float2 c = cell + float2(i, j);
        float2 h = hash2(c);
        if (h.x > keep) continue;
        float2 at = c + 0.5f + 0.38f * float2(sin(time * (0.3f + h.x) + TAU * h.y), cos(time * (0.25f + h.y) + TAU * h.x));
        float r = size * (0.6f + 0.8f * h.y);
        sum += glow(length(q - at), r) * (0.35f + 0.65f * fract(h.x * 91.7f));
    }
    return sum;
}

// The world: a sea of points the camera flies through. Six depths, each coming nearer (its grid opening out from the
// middle) and fading in far and out near, so the flight never ends and never pops.
static float3 sea(float2 p, float time, float speed) {
    float3 c = float3(0.0f);
    for (int l = 0; l < 6; l++) {
        float z = fract(float(l) / 6.0f + time * speed);
        float scale = mix(34.0f, 2.5f, z * z);
        float fade = smoothstep(0.0f, 0.25f, z) * smoothstep(1.0f, 0.7f, z);
        c += COOL * 0.30f * fade * points(p * scale + float(l) * 19.7f, time * 0.3f, 0.07f, 0.42f);
    }
    return c;
}

// A landmark's size as the camera nears it, holds, and flies through it at the end of its `len` seconds; and how
// much of it is there (it comes out of the dark and is gone as it passes).
static float nearing(float t, float len) { return 0.55f + 0.45f * smoothstep(0.0f, min(0.5f, len * 0.4f), t) + 0.12f * t / len + 5.0f * exp((t - len) * 9.0f / clamp(len * 0.5f, 0.25f, 1.0f)); }
static float there(float t, float len) { return smoothstep(-0.02f, 0.05f, t) * smoothstep(len, len - min(0.22f, len * 0.3f), t); }

kernel void backdrop(texture2d<half, access::write> out [[texture(0)]],
                     constant Params& u [[buffer(0)]],
                     uint2 g [[thread_position_in_grid]]) {
    if (g.x >= u.w || g.y >= u.h) return;
    float2 res = float2(u.w, u.h);
    float side = min(res.x, res.y);
    // Centred, the short side 1 across, y down.
    float2 p = (float2(g) + 0.5f - 0.5f * res) / side;
    float t = u.t;
    int shot = int(u.shot + 0.5f);
    float len = max(u.f, 0.5f);
    float3 c = GROUND;
    // The landmark's own space: nearer as the beat goes, through it at the end.
    float2 q = p / nearing(t, len);
    float on = there(t, len);
    float3 m = float3(0.0f);

    if (shot == 17) {
        // The interrupt: the palette turned over. A field of the accent, a little hotter in the middle.
        c = ACCENT * (0.80f + 0.25f * glow(length(p), 0.7f));
    } else if (shot == 11) {
        // The galaxy: every function a star. `a` is how far in the camera is (1: all of it), toward one star; at `b`
        // that star is edited: it flares, and only its four readers light after it.
        float zoom = max(u.a, 1.0f);
        float2 tc = float2(5.0f, -3.0f);
        float2 star = (tc + 0.5f + 0.85f * (hash2(tc) - 0.5f)) / 60.0f;
        float2 w = star * (1.0f - 1.0f / zoom) + turn(p, 0.25f) / zoom;
        float r = length(w);
        c += (float3(1.0f, 0.8f, 0.6f) * 0.9f * glow(r, 0.045f) + ACCENT * 0.07f * glow(r, 0.22f)) / zoom;
        float2 cell = floor(w * 60.0f);
        float since = t - u.b;
        for (int j = -1; j <= 1; j++) for (int i = -1; i <= 1; i++) {
            float2 cc = cell + float2(i, j);
            float2 h = hash2(cc);
            float2 at = (cc + 0.5f + 0.85f * (h - 0.5f)) / 60.0f;
            float ar = length(at), ang = atan2(at.y, at.x);
            // Two arms winding out, thick with stars; thin between them and at the rim.
            float arm = 0.5f + 0.5f * cos(2.0f * ang - 7.0f * log(ar + 0.04f));
            float dens = 0.12f + glow(ar, 0.34f) * (0.25f + 0.75f * arm * arm);
            float d = length(w - at) * zoom;
            float2 rel = cc - tc;
            float reader = (abs(rel.x) + abs(rel.y) == 1.0f) ? 1.0f : 0.0f;
            float self = (rel.x == 0.0f && rel.y == 0.0f) ? 1.0f : 0.0f;
            float lit = since > 0.0f ? self + reader * smoothstep(0.25f, 0.6f, since) : 0.0f;
            float3 tint = mix(COOL * (0.6f + 0.8f * h.y), ACCENT + 0.5f, lit);
            float big = 0.0035f * (0.7f + 0.9f * h.x) * (1.0f + 0.4f * log(zoom)) * (1.0f + 1.2f * lit);
            c += tint * (dens + lit) * (1.1f * glow(d, big) + 0.3f * lit * glow(d, big * 2.2f));
            // The edges from the edited star to its readers, drawn as the edit reaches them.
            if (since > 0.0f && reader > 0.0f) {
                float2 tip = mix(star, at, smoothstep(0.1f, 0.5f, since));
                c += ACCENT * 0.5f * glow(seg(w, star, tip) * zoom, 0.0025f);
            }
        }
        if (since > 0.0f) c += ACCENT * 0.6f * exp(-since * 2.0f) * glow(length(w - star) * zoom - since * 0.35f, 0.006f);
    } else {
        c += sea(p, u.time, shot == 10 ? 0.55f : 0.06f) * (shot == 0 ? 1.0f : 0.6f);
    }

    if (shot == 0) {
        // The name's sky: far off, small, the galaxy the film is flying to.
        // `a` is how near it has come (1: far).
        float2 w = (p - float2(0.0f, -0.02f)) / max(u.a, 1.0f);
        float r = length(w), ang = atan2(w.y, w.x);
        c += (ACCENT * 0.5f + 0.25f) * glow(r, 0.012f) + ACCENT * 0.22f * glow(r, 0.06f) * (0.5f + 0.5f * cos(2.0f * ang - 9.0f * log(r + 0.01f) + u.time * 0.3f));
    } else if (shot == 1) {
        // The cage: a build script inside a hexagon; what it sends out stops at the wall.
        float d = hexagon(q, 0.25f);
        m += ACCENT * (1.2f * glow(d, 0.005f) + 0.30f * glow(d, 0.035f));
        float2 it = 0.13f * float2(sin(t * 9.0f), cos(t * 7.3f));
        m += (COOL + 0.4f) * (1.5f * glow(length(q - it), 0.012f) + 0.3f * glow(length(q - it), 0.05f));
        for (int i = 0; i < 6; i++) {
            float ang = TAU * (float(i) + 0.5f) / 6.0f;
            float2 dir = float2(cos(ang), sin(ang));
            float k = fract(t * 1.7f + float(i) * 0.31f);
            float2 head = it + (dir * 0.215f - it) * k;
            m += COOL * 0.9f * glow(seg(q, it + (head - it) * 0.7f, head), 0.004f) * (1.0f - k);
            m += (ACCENT + 0.6f) * 1.4f * glow(length(q - dir * 0.215f), 0.02f) * glow(k - 1.0f, 0.12f);
        }
    } else if (shot == 2) {
        // The check: a proof's mark, drawn in two strokes inside its ring.
        float k = clamp(t * 4.0f, 0.0f, 2.0f);
        float2 a = float2(-0.15f, 0.01f), b = float2(-0.04f, 0.12f), e = float2(0.17f, -0.13f);
        float d = seg(q, a, mix(a, b, min(k, 1.0f)));
        if (k > 1.0f) d = min(d, seg(q, b, mix(b, e, k - 1.0f)));
        m += (ACCENT + 0.3f) * 1.3f * glow(d, 0.016f) + ACCENT * 0.35f * glow(d, 0.07f);
        m += COOL * 0.5f * glow(length(q) - 0.30f, 0.004f);
    } else if (shot == 3) {
        // The agents: many, streaming in to one warm core that answers each.
        float r = length(q);
        m += (ACCENT + 0.6f) * 1.6f * glow(r, 0.035f) + ACCENT * 0.5f * glow(r, 0.13f);
        for (int i = 0; i < 44; i++) {
            float2 h = hash2(float2(float(i), 3.0f));
            float ang = TAU * h.x;
            float k = fract(t * (0.35f + 0.3f * h.y) + h.y);
            float2 dir = float2(cos(ang), sin(ang));
            float2 head = dir * mix(0.62f, 0.05f, k);
            m += (COOL + 0.2f) * (0.9f * glow(seg(q, head, head + dir * 0.035f), 0.0035f) + 0.8f * glow(length(q - head), 0.006f)) * smoothstep(0.0f, 0.1f, k);
        }
    } else if (shot == 4) {
        // The capabilities: each ring is handed down from the one outside it and spans less: it only narrows.
        for (int k = 0; k < 5; k++) {
            float rad = 0.34f * pow(0.74f, float(k));
            float span = 3.0f * pow(0.72f, float(k));
            float ang = atan2(q.y, q.x) + 1.57f - 0.35f * float(k) + 0.25f * sin(t * 0.8f);
            ang = atan2(sin(ang), cos(ang));
            float in_span = smoothstep(span, span - 0.25f, abs(ang));
            float shown = smoothstep(float(k) * 0.42f, float(k) * 0.42f + 0.3f, t);
            float3 tint = mix(COOL, ACCENT, float(k) / 4.0f);
            m += tint * shown * in_span * (1.3f * glow(length(q) - rad, 0.005f) + 0.25f * glow(length(q) - rad, 0.03f));
        }
    } else if (shot == 5) {
        // The disk: spinning under the same code; at `a` it tears; then time runs back and it tears again, exactly.
        float stop = u.a + 0.8f;
        float tau = t < stop ? t : (t < stop + 0.5f ? stop * (1.0f - (t - stop) / 0.5f) : t - stop - 0.5f);
        float tear = glow(tau - u.a, 0.22f);
        float2 w = q;
        w.x += 0.07f * tear * sin(w.y * 41.0f + tau * 31.0f) * sin(w.y * 9.0f + 1.3f);
        float r = length(w), ang = atan2(w.y, w.x) + tau * 5.0f;
        float tracks = 0.0f;
        for (int k = 0; k < 6; k++) tracks += glow(r - (0.08f + 0.045f * float(k)), 0.0035f) * (0.55f + 0.45f * sin(ang * float(3 + k) + float(k)));
        m += mix(COOL + 0.15f, ACCENT + 0.3f, tear) * 0.9f * tracks;
        m += COOL * 0.8f * glow(r, 0.02f) + COOL * 0.5f * glow(r - 0.33f, 0.004f);
        // The head: an arm in from the right.
        m += (COOL + 0.3f) * 0.8f * glow(seg(q, float2(0.42f, 0.16f), float2(0.19f, 0.03f)), 0.006f);
    } else if (shot == 6) {
        // The proof: a tree that grows level by level from its root, every node lighting as it is closed.
        for (int l = 0; l < 6; l++) {
            float n = pow(2.0f, float(l));
            float y = -0.30f + 0.12f * float(l);
            float grown = smoothstep(float(l) * 0.62f, float(l) * 0.62f + 0.3f, t);
            float i = clamp(floor((q.x / 0.9f + 0.5f) * n), 0.0f, n - 1.0f);
            for (float j = max(i - 1.0f, 0.0f); j <= min(i + 1.0f, n - 1.0f); j += 1.0f) {
                float2 at = float2(((j + 0.5f) / n - 0.5f) * 0.9f, y);
                float2 up = float2(((floor(j / 2.0f) + 0.5f) / (n / 2.0f) - 0.5f) * 0.9f, y - 0.12f);
                if (l > 0) m += COOL * 0.45f * grown * glow(seg(q, up, mix(up, at, grown)), 0.0022f);
                float closed = smoothstep(float(l) * 0.62f + 0.25f, float(l) * 0.62f + 0.5f, t);
                m += mix(COOL, ACCENT + 0.4f, closed) * grown * (1.2f * glow(length(q - at), 0.009f) + 0.2f * glow(length(q - at), 0.03f));
            }
        }
    } else if (shot == 7) {
        // The prism: one beam in, each effect out as its own.
        float2 a = float2(0.0f, -0.17f), b = float2(-0.15f, 0.10f), e = float2(0.15f, 0.10f);
        float d = min(seg(q, a, b), min(seg(q, b, e), seg(q, e, a)));
        m += (COOL + 0.3f) * (1.0f * glow(d, 0.004f) + 0.2f * glow(d, 0.03f));
        float k = clamp(t * 1.6f, 0.0f, 1.0f);
        m += float3(1.0f) * 1.1f * glow(seg(q, float2(-0.62f, 0.06f), mix(float2(-0.62f, 0.06f), float2(-0.07f, 0.0f), k)), 0.006f);
        for (int i = 0; i < 4; i++) {
            float go = clamp(t * 1.6f - 1.0f - 0.25f * float(i), 0.0f, 1.0f);
            float2 from = float2(0.07f, 0.0f);
            float2 to = from + float2(0.6f, -0.18f + 0.12f * float(i)) * go;
            float3 tint = mix(ACCENT + 0.2f, COOL + 0.2f, float(i) / 3.0f);
            m += tint * step(0.001f, go) * (1.1f * glow(seg(q, from, to), 0.005f) + 0.2f * glow(seg(q, from, to), 0.025f));
        }
    } else if (shot == 8) {
        // The mirror: two builds, bit for bit; a line reads down both and finds nothing to say.
        float2 w = float2(abs(q.x) - 0.22f, q.y);
        float inside = glow(max(abs(w.x) - 0.13f, 0.0f), 0.02f) * glow(max(abs(w.y) - 0.22f, 0.0f), 0.02f);
        float2 cell = floor(w * 44.0f);
        float bit = step(0.5f, hash1(cell));
        float dot_ = glow(length(fract(w * 44.0f) - 0.5f), 0.26f);
        float scan = -0.24f + 0.48f * fract(t * 0.45f);
        float read = smoothstep(scan + 0.01f, scan - 0.05f, w.y);
        m += inside * dot_ * (0.15f + 0.85f * bit) * mix(COOL, ACCENT + 0.2f, read);
        m += (ACCENT + 0.5f) * 0.8f * inside * glow(w.y - scan, 0.006f);
    } else if (shot == 9) {
        // The ripples: three starts at once; each ring grows at its own speed to the same shore (`a`, `b`, `c`: its
        // seconds). The first there is the fastest.
        float took[3] = { u.a, u.b, u.c };
        float r = length(q);
        m += COOL * 0.35f * glow(r - 0.36f, 0.003f);
        for (int i = 0; i < 3; i++) {
            float k = clamp(t / max(took[i], 0.001f), 0.0f, 1.0f);
            float3 tint = i == 0 ? ACCENT + 0.2f : COOL * (1.2f - 0.3f * float(i));
            m += tint * (1.2f * glow(r - 0.36f * k, 0.005f) + 0.25f * glow(r - 0.36f * k, 0.03f));
            if (t > took[i]) m += tint * 1.2f * exp(-(t - took[i]) * 5.0f) * glow(r - 0.36f, 0.03f);
        }
        m += float3(1.0f) * 0.9f * glow(r, 0.012f);
    } else if (shot == 12) {
        // Soon, the hub: a comb of cells, each lit from the one beside it, out from the middle.
        float2 w = q * 9.0f;
        float2 a = float2(w.x - w.y * 0.57735f, w.y * 1.1547f);
        float2 id = floor(a + 0.5f);
        float2 f = a - id;
        float2 centre = float2(id.x + id.y * 0.5f, id.y * 0.8660254f);
        float d = hexagon(float2(f.x + f.y * 0.5f, f.y * 0.8660254f), 0.40f);
        float reach = smoothstep(0.0f, 0.6f, t * 1.1f - length(centre) * 0.9f);
        m += mix(COOL * 0.5f, ACCENT + 0.2f, reach) * glow(length(q), 0.36f) * (0.9f * glow(d, 0.035f) + 0.5f * reach * glow(d + 0.4f, 0.25f));
    } else if (shot == 13) {
        // Soon, first call: a stair that is only built where the walker steps.
        float walker = t * 3.2f;
        for (int i = 0; i < 7; i++) {
            float2 a = float2(-0.36f + 0.105f * float(i), 0.22f - 0.07f * float(i));
            float built = smoothstep(float(i) - 0.3f, float(i) + 0.2f, walker);
            float d = min(seg(q, a, a + float2(0.105f, 0.0f)), seg(q, a + float2(0.105f, 0.0f), a + float2(0.105f, -0.07f)));
            m += mix(COOL * 0.25f, ACCENT + 0.2f, built) * (0.9f * glow(d, 0.004f) + 0.15f * built * glow(d, 0.03f));
        }
        float i = min(floor(walker), 6.0f), hop = fract(min(walker, 6.99f));
        float2 on_step = float2(-0.36f + 0.105f * (i + 0.5f + hop), 0.22f - 0.07f * (i + hop) - 0.03f - 0.05f * sin(hop * 3.1416f));
        m += float3(1.0f) * (1.4f * glow(length(q - on_step), 0.012f) + 0.3f * glow(length(q - on_step), 0.05f));
    } else if (shot == 14) {
        // Soon, the shared store: one store in the middle, every machine fed from it.
        m += (ACCENT + 0.5f) * (1.4f * glow(length(q), 0.03f) + 0.4f * glow(length(q), 0.1f));
        for (int i = 0; i < 9; i++) {
            float ang = TAU * float(i) / 9.0f + 0.2f;
            float2 at = 0.32f * float2(cos(ang), sin(ang));
            m += COOL * 0.3f * glow(seg(q, float2(0.0f), at), 0.002f);
            m += (COOL + 0.3f) * (1.0f * glow(length(q - at), 0.014f) + 0.2f * glow(length(q - at), 0.05f));
            float k = fract(t * 1.3f + float(i) * 0.37f);
            m += (ACCENT + 0.3f) * 1.2f * glow(length(q - at * k), 0.008f);
        }
    } else if (shot == 15) {
        // Soon, the cores: a field of them, work washing across.
        float2 w = q * 22.0f;
        float2 id = floor(w);
        float tile = glow(max(abs(fract(w.x) - 0.5f) - 0.2f, 0.0f), 0.09f) * glow(max(abs(fract(w.y) - 0.5f) - 0.2f, 0.0f), 0.09f);
        float wave = glow(fract((id.x + id.y) * 0.045f - t * 0.55f + hash1(id) * 0.12f) - 0.5f, 0.16f);
        m += tile * glow(length(q), 0.42f) * mix(COOL * 0.35f, ACCENT + 0.3f, wave);
    } else if (shot == 16) {
        // The end: the sea streams in to where the mark forms.
        float a = clamp(u.a, 0.0f, 1.0f);
        float r = length(p);
        float2 polar = float2(atan2(p.y, p.x) / TAU * 40.0f, log(r + 0.02f) * 9.0f + t * 1.6f);
        c += mix(COOL, ACCENT, 0.5f) * 0.5f * (1.0f - a) * points(polar, u.time, 0.16f, 0.5f) * smoothstep(0.02f, 0.25f, r);
        c += ACCENT * (0.10f + 0.22f * a) * glow(r, 0.32f);
    }
    c += m * on;
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
    bg *= 1.0f - 0.5f * (1.0f - exp(-dot(v, v) * 3.2f));
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

    /// The picture alone, with no UI, grain or vignette: what `b` draws at `time`. For the test that no light ends at
    /// a line.
    #[cfg(test)]
    pub fn picture(&self, b: &Backdrop, time: f32) -> Result<&Tex, String> {
        let p = Params { w: self.w, h: self.h, time, shot: b.shot, t: time - b.at, a: b.a, b: b.b, c: b.c, d: b.d, e: b.e, f: b.f, frame: 0 };
        let cmd = self.queue.commandBuffer().ok_or("backdrop: no command buffer")?;
        let enc = cmd.computeCommandEncoder().ok_or("backdrop: no compute encoder")?;
        enc.setComputePipelineState(&self.backdrop);
        // SAFETY: as in `draw`: the kernel's one texture and its Params, both outliving the command buffer.
        unsafe {
            enc.setTexture_atIndex(Some(&self.picture), 0);
            enc.setBytes_length_atIndex(NonNull::from(&p).cast(), std::mem::size_of::<Params>(), 0);
        }
        let tg = self.backdrop.threadExecutionWidth();
        enc.dispatchThreads_threadsPerThreadgroup(
            MTLSize { width: self.w as usize, height: self.h as usize, depth: 1 },
            MTLSize { width: tg, height: (self.backdrop.maxTotalThreadsPerThreadgroup() / tg).max(1), depth: 1 },
        );
        enc.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        if let Some(e) = cmd.error() {
            return Err(format!("backdrop: the pass failed: {e}"));
        }
        Ok(&self.picture)
    }
}
