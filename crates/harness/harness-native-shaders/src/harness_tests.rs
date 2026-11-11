//! harness.rs on the CPU: the pure helpers (the SDFs, the HDR curves, Slug's root code, solver and
//! coverage, Ghostty's whole-pixel sprites) and the entry points that use no intrinsic. Every expected
//! value is a property or a constant worked out from the Slang source, never a printed run.
use crate::harness::*;
use gpu::prelude::*;
use hegel::generators as gs;

fn f(lo: f32, hi: f32) -> gs::FloatGenerator<f32> {
    gs::floats::<f32>().min_value(lo).max_value(hi).allow_nan(false).allow_infinity(false)
}

fn i(lo: i32, hi: i32) -> gs::IntegerGenerator<i32> {
    gs::integers::<i32>().min_value(lo).max_value(hi)
}

fn close(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol * b.abs().max(1.0)
}

fn frame() -> Frame {
    Frame { view: vec2(200.0, 100.0), scale: 2.0, ..Frame::default() }
}

/// to_ndc maps the view's top-left corner to (-1, 1) and its bottom-right to (1, -1) (y down to y up).
#[test]
fn to_ndc_maps_the_view_corners() {
    let view = vec2(200.0, 100.0);
    assert_eq!(to_ndc(vec2(0.0, 0.0), view), vec2(-1.0, 1.0));
    assert_eq!(to_ndc(view, view), vec2(1.0, -1.0));
    assert_eq!(to_ndc(view * 0.5, view), vec2(0.0, 0.0));
}

/// The squircle's distance along an axis, past the corner region, is how far the point is from the
/// side (q.y < 0 there, so qp = (k + r, 0) and the corner term is k + r); at the centre it is minus
/// the smaller half size; and at n = 2 the corner is a circle of radius r about half - r.
#[hegel::test]
fn sd_squircle_is_a_distance_on_its_axes_and_round_at_n_2(tc: hegel::TestCase) {
    let half = vec2(tc.draw(f(1.0, 100.0)), tc.draw(f(1.0, 100.0)));
    let r = tc.draw(f(0.0, 1.0)) * half.x.min(half.y) * 0.99;
    let n = tc.draw(f(2.0, 8.0));
    let k = tc.draw(f(-r, 50.0));
    let d = sd_squircle(vec2(half.x + k, 0.0), half, r, n);
    assert!(close(d, k, 1e-3), "on the x axis {k} past the side: {d}");
    let d = sd_squircle(vec2(0.0, -(half.y + k)), half, r, n);
    assert!(close(d, k, 1e-3), "on the y axis {k} past the side: {d}");
    let c = sd_squircle(vec2(0.0, 0.0), half, r, n);
    assert!(close(c, -half.x.min(half.y), 1e-5), "centre: {c}");
    let a = tc.draw(f(0.0, core::f32::consts::FRAC_PI_2));
    let p = half - r + (r + k) * vec2(a.cos(), a.sin());
    let d = sd_squircle(p, half, r, 2.0);
    assert!((d - k).abs() <= 1e-3 * (r + k).max(1.0), "n = 2 corner at {a} rad, {k} out: {d}");
}

/// sd_seg is the distance to the nearest point of the segment; sd_stub is the same, and with no
/// length (a == b, where sd_seg divides by 0) the distance to the point.
#[hegel::test]
fn sd_seg_and_sd_stub_are_the_distance_to_the_segment(tc: hegel::TestCase) {
    let v = |tc: &hegel::TestCase| vec2(tc.draw(f(-100.0, 100.0)), tc.draw(f(-100.0, 100.0)));
    let (p, a, b) = (v(&tc), v(&tc), v(&tc));
    tc.assume(length(b - a) > 1e-2);
    let d = sd_seg(p, a, b);
    assert!(close(sd_stub(p, a, b), d, 1e-5));
    assert!(d <= length(p - a).min(length(p - b)) + 1e-3, "{d} is past an end");
    // The nearest point is on the segment: some t in 0..1 is that far.
    let t = saturate(dot(p - a, b - a) / dot(b - a, b - a));
    assert!(close(d, length(p - lerp(a, b, t)), 1e-4));
    assert_eq!(sd_stub(p, a, a), length(p - a));
}

/// sd_quad on the unit square: inside, minus the distance to the nearest side; outside, the distance
/// to the square. Either winding gives the same sign (the crossing test counts parity).
#[hegel::test]
fn sd_quad_is_signed_by_inside(tc: hegel::TestCase) {
    let p = vec2(tc.draw(f(-2.0, 3.0)), tc.draw(f(-2.0, 3.0)));
    let (v0, v1, v2, v3) = (vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0));
    let out = (p - 1.0).max(-p).max(Vec2::splat(0.0));
    let expect = if out.x > 0.0 || out.y > 0.0 { length(out) } else { -p.x.min(1.0 - p.x).min(p.y.min(1.0 - p.y)) };
    tc.assume((expect).abs() > 1e-4);
    assert!(close(sd_quad(p, v0, v1, v2, v3), expect, 1e-4), "ccw at {p:?}");
    assert!(close(sd_quad(p, v0, v3, v2, v1), expect, 1e-4), "cw at {p:?}");
}

/// The powerline triangle is a quad with v3 = v0: that edge has no length and is skipped. At (0.5, 1)
/// in a 2 by 2 cell the left side (0.5 away) is nearer than the slanted ones (0.75 / sqrt(1.25)).
#[test]
fn sd_quad_skips_a_degenerate_edge() {
    let d = sd_quad(vec2(0.5, 1.0), vec2(0.0, 0.0), vec2(2.0, 1.0), vec2(0.0, 2.0), vec2(0.0, 0.0));
    assert!((d + 0.5).abs() < 1e-5, "{d}");
    let d = sd_quad(vec2(-1.0, 1.0), vec2(0.0, 0.0), vec2(2.0, 1.0), vec2(0.0, 2.0), vec2(0.0, 0.0));
    assert!((d - 1.0).abs() < 1e-5, "{d}");
}

/// The caret is solid for `hold` seconds; after that each cycle is shown at its first quarter and
/// hidden at its third (the fades are under a quarter of the cycle wide), always between 0 and 1.
#[hegel::test]
fn caret_blink_holds_then_blinks(tc: hegel::TestCase) {
    let hold = tc.draw(f(0.0, 2.0));
    let period = tc.draw(f(0.2, 2.0));
    let fade = tc.draw(f(0.01, 0.2));
    let t = tc.draw(f(0.0, 30.0));
    let b = caret_blink(t, hold, period, fade);
    assert!((0.0..=1.0).contains(&b));
    assert_eq!(caret_blink(hold * tc.draw(f(0.0, 0.999)), hold, period, fade), 1.0);
    let k = tc.draw(i(0, 10)) as f32;
    assert_eq!(caret_blink(hold + period * (k + 0.25), hold, period, fade), 1.0);
    assert_eq!(caret_blink(hold + period * (k + 0.75), hold, period, fade), 0.0);
}

/// pick is the dark colour past 0.5, the light one otherwise.
#[test]
fn pick_picks_by_appearance() {
    let (d, l) = (Vec4::splat(1.0), Vec4::splat(0.0));
    assert_eq!((pick(1.0, d, l), pick(0.0, d, l), pick(0.5, d, l)), (d, l, l));
}

/// in_band: a pixel's coverage of the band, half at an edge (0 * scale + 0.5), whole well inside,
/// none well outside; a foot of 0 is none; a fade ramps the edge by smoothstep, half at its middle.
#[hegel::test]
fn in_band_cuts_at_its_edges(tc: hegel::TestCase) {
    // A foot of 0 or less is none, so the band sits below 0 to have one.
    let top = tc.draw(f(0.0, 100.0));
    let foot = top + tc.draw(f(10.0, 100.0));
    let scale = tc.draw(f(1.0, 3.0));
    let hard = vec4(top, foot, 0.0, 0.0);
    assert_eq!(in_band(top, hard, hard.zw(), scale), 0.5);
    assert_eq!(in_band(foot, hard, hard.zw(), scale), 0.5);
    assert_eq!(in_band((top + foot) * 0.5, hard, hard.zw(), scale), 1.0);
    assert_eq!(in_band(top - 5.0, hard, hard.zw(), scale), 0.0);
    assert_eq!(in_band(foot + 5.0, hard, hard.zw(), scale), 0.0);
    let open = vec4(top, 0.0, 0.0, 0.0);
    assert_eq!(in_band(top + 1e4, open, open.zw(), scale), 1.0);
    let fade = vec4(top, foot, 8.0, 0.0);
    assert!((in_band(top + 4.0, fade, fade.zw(), scale) - 0.5).abs() < 1e-5);
}

/// unit2 is length 1, or 0 for a vector under 1e-5 long.
#[hegel::test]
fn unit2_is_unit_or_zero(tc: hegel::TestCase) {
    let v = vec2(tc.draw(f(-100.0, 100.0)), tc.draw(f(-100.0, 100.0)));
    let u = unit2(v);
    if length(v) > 1e-5 {
        assert!(close(length(u), 1.0, 1e-5));
    } else {
        assert_eq!(u, vec2(0.0, 0.0));
    }
    assert_eq!(unit2(vec2(0.0, 0.0)), vec2(0.0, 0.0));
}

/// sRGB's extended curves invert each other, are odd (extended sRGB carries negatives), and meet the
/// pinned points: 0, 1, and an encoded 0.5 at 0.214 linear.
#[hegel::test]
fn srgb_ext_curves_invert_each_other(tc: hegel::TestCase) {
    let x = tc.draw(f(-8.0, 8.0));
    let v = vec3(x, -x, x * 0.5);
    let back = srgb_encode_ext(srgb_decode_ext(v));
    assert!(close(back.x, v.x, 1e-5) && close(back.y, v.y, 1e-5) && close(back.z, v.z, 1e-5), "{v:?} -> {back:?}");
    assert_eq!(srgb_decode_ext(-v), -srgb_decode_ext(v));
    let pins = srgb_decode_ext(vec3(0.0, 1.0, 0.5));
    assert_eq!(pins.x, 0.0);
    assert!((pins.y - 1.0).abs() < 1e-6 && (pins.z - 0.214_041).abs() < 1e-5, "{pins:?}");
}

/// The frame's headroom is textfx.x, at least 1, and 1 for a reading that isn't finite.
#[test]
fn frame_headroom_is_at_least_one() {
    let h = |x: f32| frame_headroom(&Frame { textfx: vec2(x, 0.0), ..Frame::default() });
    assert_eq!((h(4.0), h(0.5), h(f32::NAN), h(f32::INFINITY)), (4.0, 1.0, 1.0, 1.0));
    assert_eq!((hdr_knee(1.0), hdr_knee(4.0)), (0.75, 1.0));
}

/// tone_map: 0 for nothing, negative or NaN; the identity up to the knee; never past the headroom;
/// never decreasing; a known peak at or under the headroom only clipped; a known peak over it lands
/// on it.
#[hegel::test]
fn tone_map_rolls_off_into_the_headroom(tc: hegel::TestCase) {
    let h = tc.draw(f(1.0, 16.0));
    let peak = if tc.draw(gs::booleans()) { 0.0 } else { tc.draw(f(0.1, 64.0)) };
    let x = tc.draw(f(0.0, 100.0));
    let dx = tc.draw(f(0.0, 10.0));
    let y = tone_map(x, peak, h);
    assert!(y <= h && y >= 0.0, "{x} -> {y} past {h}");
    assert!(tone_map(x + dx, peak, h) + 1e-5 >= y, "decreased past {x}");
    if x <= hdr_knee(h) {
        assert_eq!(y, x);
    }
    if peak > 0.0 && peak <= h {
        assert_eq!(y, x.min(h));
    }
    if peak > h {
        assert!(close(tone_map(peak, peak, h), h, 1e-4), "peak {peak} lands at {}", tone_map(peak, peak, h));
    }
    assert_eq!((tone_map(-x, peak, h), tone_map(f32::NAN, peak, h)), (0.0, 0.0));
    let rgb = vec3(x, x * 0.5, x * 0.25);
    let m = tone_map_rgb(rgb, peak, h);
    if x > 0.0 {
        assert!(close(m.x, y, 1e-5) && close(m.y, y * 0.5, 1e-5) && close(m.z, y * 0.25, 1e-5), "hue moved: {m:?}");
    }
    assert_eq!(tone_map_rgb(vec3(0.0, -1.0, 0.0), peak, h), vec3(0.0, 0.0, 0.0));
}

/// PQ: 0 is black, 1 is 10000 nits (p = 1, so (1 - C1) / (C2 - C3) = 0.1640625 / 0.1640625 = 1), as
/// a multiple of the 203-nit white; never decreasing between.
#[hegel::test]
fn pq_linear_spans_black_to_10000_nits(tc: hegel::TestCase) {
    let top = pq_linear(Vec3::splat(1.0));
    assert!(close(top.x, 10000.0 / HDR_WHITE_NITS, 1e-4), "{top:?}");
    assert_eq!(pq_linear(Vec3::splat(0.0)), Vec3::splat(0.0));
    let e = tc.draw(f(0.0, 1.0));
    let de = tc.draw(f(0.0, 0.1));
    let (lo, hi) = (pq_linear(Vec3::splat(e)).x, pq_linear(Vec3::splat(e + de)).x);
    assert!(hi >= lo * (1.0 - 1e-6), "{e} gives {lo}, {} gives {hi}", e + de);
}

/// HLG: e^2 / 3 up to 0.5, which is 1/12 there and where the exponential half meets it
/// ((exp((0.5 - C) / A) + B) / 12 = 1/12); 1 at a signal of 1. Through the OOTF a signal of 1 on every
/// channel (luma weights summing to 1) is the display's 1000 nits over 203.
#[test]
fn hlg_meets_its_pinned_points() {
    let s = hlg_scene(vec3(0.5, 0.5 + 1e-6, 1.0));
    assert!((s.x - 1.0 / 12.0).abs() < 1e-6 && (s.y - 1.0 / 12.0).abs() < 1e-5, "{s:?}");
    assert!((s.z - 1.0).abs() < 1e-4, "{s:?}");
    assert_eq!(hlg_scene(vec3(-1.0, 0.0, 0.25)), vec3(0.0, 0.0, 0.0625 / 3.0));
    let l = hlg_linear(Vec3::splat(1.0));
    assert!(close(l.x, HDR_HLG_PEAK_NITS / HDR_WHITE_NITS, 1e-4), "{l:?}");
}

/// BT.2020 to BT.709 keeps white (each row sums to 1) and is linear.
#[test]
fn bt2020_to_709_keeps_white() {
    let w = bt2020_to_709(Vec3::splat(1.0));
    assert!((w.x - 1.0).abs() < 1e-5 && (w.y - 1.0).abs() < 1e-5 && (w.z - 1.0).abs() < 1e-5, "{w:?}");
    assert_eq!(bt2020_to_709(Vec3::splat(0.0)), Vec3::splat(0.0));
}

/// Slug's root code is the sign pattern of the curve's three coordinates (bit 31 of each, so -0
/// counts as below): a curve that ends where it began, on one side, crosses twice if its middle is on
/// the other side and never if not; one that goes from above to below crosses once, its first root;
/// from below to above once, its second.
#[hegel::test]
fn slug_root_code_counts_the_crossings(tc: hegel::TestCase) {
    let y = |tc: &hegel::TestCase| {
        let v = tc.draw(f(0.0, 10.0));
        if tc.draw(gs::booleans()) { -v - f32::MIN_POSITIVE } else { v }
    };
    let (y1, y2, y3) = (y(&tc), y(&tc), y(&tc));
    let below = |v: f32| v.is_sign_negative();
    let expect = match (below(y1), below(y2), below(y3)) {
        (a, b, c) if a == c => {
            if a != b { 0x0101 } else { 0 }
        }
        (false, _, true) => 0x0001,
        _ => 0x0100,
    };
    assert_eq!(slug_root_code(y1, y2, y3), expect, "({y1}, {y2}, {y3})");
    assert_eq!(slug_root_code(-0.0, 1.0, 1.0), 0x0100);
}

/// For a straight curve (the middle point halfway), the polynomial is linear and both roots are where
/// the line crosses y = 0; the vertical solver is the horizontal one with x and y swapped, bit for bit.
#[hegel::test]
fn slug_solvers_find_the_crossing(tc: hegel::TestCase) {
    let p1 = vec2(tc.draw(f(-10.0, 10.0)), tc.draw(f(0.1, 10.0)));
    let p3 = vec2(tc.draw(f(-10.0, 10.0)), -tc.draw(f(0.1, 10.0)));
    let p2 = (p1 + p3) * 0.5;
    let r = slug_solve_horiz(vec4(p1.x, p1.y, p2.x, p2.y), p3);
    let x = p1.x + (p3.x - p1.x) * p1.y / (p1.y - p3.y);
    assert!((r.x - x).abs() < 1e-3 && r.x == r.y, "{r:?}, crossing at {x}");
    let q = vec2(tc.draw(f(-10.0, 10.0)), tc.draw(f(-10.0, 10.0)));
    let p12 = vec4(p1.x, p1.y, q.x, q.y);
    // By bits, so a NaN made the same way on both is the same too.
    let bits = |r: Vec2| (r.x.to_bits(), r.y.to_bits());
    assert_eq!(bits(slug_solve_vert(vec4(p12.y, p12.x, p12.w, p12.z), vec2(p3.y, p3.x))), bits(slug_solve_horiz(p12, p3)));
    // (-1, 1), (0, -1), (1, 1): y(t) = (2t - 1)^2, a double root at t = 1/2, x = 0.
    assert_eq!(slug_solve_horiz(vec4(-1.0, 1.0, 0.0, -1.0), vec2(1.0, 1.0)), vec2(0.0, 0.0));
}

/// The unit square in em space as Slug's buffers hold it: four straight edges counter-clockwise (ink
/// on the left), one band each way; each list sorted by its far coordinate, greatest first.
fn square() -> ([Vec4; 8], [u32; 12]) {
    let edge = |a: Vec2, b: Vec2| {
        let m = (a + b) * 0.5;
        [vec4(a.x, a.y, m.x, m.y), vec4(b.x, b.y, 0.0, 0.0)]
    };
    let (bottom, right, top, left) = (
        edge(vec2(0.0, 0.0), vec2(1.0, 0.0)),
        edge(vec2(1.0, 0.0), vec2(1.0, 1.0)),
        edge(vec2(1.0, 1.0), vec2(0.0, 1.0)),
        edge(vec2(0.0, 1.0), vec2(0.0, 0.0)),
    );
    let curves = [bottom[0], bottom[1], right[0], right[1], top[0], top[1], left[0], left[1]];
    // From 0: the horizontal band's (count, offset), the vertical one's, then the lists, offsets from 0.
    // Horizontal rays stop at a curve wholly left: by greatest x (right, bottom, top, left). Vertical
    // rays by greatest y (top, right, left, bottom).
    let bands = [4, 4, 4, 8, 2, 0, 4, 6, 4, 2, 6, 0];
    (curves, bands)
}

/// Inside the square by more than a pixel the coverage is 1; outside by more than one, 0; on its right
/// edge half (the horizontal ray's root at 0 gives saturate(0.5), weighted fully; worked through in the
/// Slang by hand), whatever the sample.
#[hegel::test]
fn slug_render_covers_the_square(tc: hegel::TestCase) {
    let (curves, bands) = square();
    let ppem = vec2(64.0, 64.0);
    let band_tf = vec4(0.0, 0.0, 0.0, 0.0);
    let gd = uvec4(0, 0, 0, 0);
    let p = vec2(tc.draw(f(-1.0, 2.0)), tc.draw(f(-1.0, 2.0)));
    let margin = 2.0 / 64.0;
    let inside = p.x.min(1.0 - p.x).min(p.y.min(1.0 - p.y));
    let cov = slug_render(&curves, &bands, p, band_tf, gd, ppem, 0.0);
    assert!((0.0..=1.0).contains(&cov));
    if inside > margin {
        assert_eq!(cov, 1.0, "inside at {p:?}");
    } else if inside < -margin {
        assert_eq!(cov, 0.0, "outside at {p:?}");
    }
    assert_eq!(slug_render(&curves, &bands, vec2(1.0, 0.5), band_tf, gd, ppem, 0.0), 0.5);
}

/// sp_box fills the half-open box between its corners, in either order.
#[test]
fn sp_box_is_half_open_either_way() {
    for (x, y, on) in [(0, 0, true), (2, 2, true), (3, 0, false), (0, 3, false), (-1, 1, false)] {
        assert_eq!(sp_box(ivec2(x, y), 0, 0, 3, 3), on, "({x}, {y})");
        assert_eq!(sp_box(ivec2(x, y), 3, 3, 0, 0), on, "({x}, {y}) reversed");
    }
}

/// Ghostty's fractions: 0 starts at 0 and 1 ends at the size; a block ending at f and the next
/// starting at f leave no gap (round(f n) + round((1 - f) n) is n, or n + 1 at a half).
#[hegel::test]
fn fractions_leave_no_gap(tc: hegel::TestCase) {
    let n = tc.draw(i(0, 400));
    let fr = tc.draw(gs::sampled_from(vec![0.0f32, 1.0 / 3.0, 0.5, 2.0 / 3.0, 1.0]));
    assert_eq!((frac_lo(0.0, n), frac_hi(1.0, n)), (0, n));
    let (lo, hi) = (frac_lo(fr, n), frac_hi(fr, n));
    assert!(lo <= hi && hi <= lo + 1, "{fr} of {n}: next starts at {lo}, this ends at {hi}");
}

fn count(w: i32, h: i32, on: impl Fn(IVec2) -> bool) -> i32 {
    let mut c = 0;
    for y in 0..h {
        for x in 0..w {
            c += i32::from(on(ivec2(x, y)));
        }
    }
    c
}

/// A light line across (left and right arms light) is every pixel of the t rows from (H - t) / 2; a
/// light cross is that and the t columns from (W - t) / 2.
#[hegel::test]
fn sp_lines_draws_whole_pixel_light_lines(tc: hegel::TestCase) {
    let (w, h, t) = (tc.draw(i(4, 40)), tc.draw(i(4, 40)), tc.draw(i(1, 3)));
    let across = (1 << 2) | (1 << 6);
    let top = (h - t).max(0) / 2;
    let left = (w - t).max(0) / 2;
    for y in 0..h {
        for x in 0..w {
            let row = y >= top && y < top + t;
            let col = x >= left && x < left + t;
            assert_eq!(sp_lines(ivec2(x, y), w, h, t, across), row, "across at ({x}, {y})");
            assert_eq!(sp_lines(ivec2(x, y), w, h, t, across | 1 | (1 << 4)), row || col, "cross at ({x}, {y})");
        }
    }
}

/// A horizontal dash row of n: n runs on its middle row, with W - n * gap pixels between them, the
/// runs differing by at most one (spare pixels widen the first).
#[hegel::test]
fn sp_dash_makes_n_dashes(tc: hegel::TestCase) {
    let (w, h, t) = (tc.draw(i(8, 60)), tc.draw(i(4, 40)), tc.draw(i(1, 3)));
    let n = tc.draw(i(2, 4));
    let b = (tc.draw(i(0, 2)) as u32) << 2;
    let want = match b >> 2 {
        0 => t.max(4),
        1 => t,
        _ => 2 * t,
    };
    let gap = want.min(w / (2 * n));
    let y = (h - t).max(0) / 2;
    let mut runs = Vec::new();
    let mut prev = false;
    for x in 0..w {
        let on = sp_dash(ivec2(x, y), w, h, t, n as u32, b);
        if on && !prev {
            runs.push(0);
        }
        if on {
            *runs.last_mut().unwrap() += 1;
        }
        prev = on;
    }
    assert_eq!(runs.len() as i32, n, "{runs:?}");
    assert_eq!(runs.iter().sum::<i32>(), w - n * gap);
    assert!(runs.iter().max().unwrap() - runs.iter().min().unwrap() <= 1, "{runs:?}");
}

/// Braille: no bits, no pixels; each dot a square of its own, apart from the others, so all eight
/// cover what the eight alone do together.
#[hegel::test]
fn sp_braille_dots_are_disjoint_squares(tc: hegel::TestCase) {
    let (w, h) = (tc.draw(i(4, 40)), tc.draw(i(8, 60)));
    assert_eq!(count(w, h, |p| sp_braille(p, w, h, 0)), 0);
    let each: Vec<i32> = (0..8).map(|b| count(w, h, |p| sp_braille(p, w, h, 1 << b))).collect();
    let side = (each[0] as f32).sqrt() as i32;
    assert!(each.iter().all(|&c| c == side * side), "{each:?}");
    assert_eq!(count(w, h, |p| sp_braille(p, w, h, 0xFF)), each.iter().sum::<i32>());
}

/// Shades are a / 255 everywhere; a block is round(side * b / 8) whole pixels flush to its side.
#[hegel::test]
fn sprite_cov_shades_and_blocks(tc: hegel::TestCase) {
    let (w, h) = (tc.draw(i(1, 40)), tc.draw(i(1, 60)));
    let a = tc.draw(i(0, 3)) as u32;
    let b = tc.draw(i(0, 8)) as u32;
    let s = |kind, a, b| SpriteInst { rect: ivec4(0, 0, w, h), shape: uvec4(kind, a, b, 1), ..SpriteInst::default() };
    assert_eq!(sprite_cov(s(SPRITE_SHADE, 51, 0), ivec2(0, 0)), 0.2);
    let along = |n: i32| ((n as f32) * (b as f32 / 8.0)).round() as i32;
    let expect = if a < 2 { along(w) * h } else { w * along(h) };
    let got = count(w, h, |p| sprite_cov(s(SPRITE_BLOCK, a, b), p) == 1.0);
    assert_eq!(got, expect, "block side {a} share {b}/8");
    let first = if a == 1 { ivec2(w - 1, 0) } else if a == 3 { ivec2(0, h - 1) } else { ivec2(0, 0) };
    assert_eq!(sprite_cov(s(SPRITE_BLOCK, a, b), first) == 1.0, b > 0 && expect > 0, "flush to side {a}");
}

/// panel_vs's quad is the rect padded on every side by the shadow and feather's reach, centred on it;
/// its local coordinate is from the rect's middle; the instance rides along flat.
#[test]
fn panel_vs_pads_its_rect() {
    let fr = frame();
    let p = PanelInst { rect: vec4(10.0, 20.0, 40.0, 30.0), fx: vec4(2.0, 0.0, 0.0, 0.0), shape: vec4(0.0, 0.0, 0.0, 1.0), ..PanelInst::default() };
    // pad = 2 * 2 + 1 + |0| + 3 = 8
    let tl = panel_vs(VertexIndex(0), InstanceIndex(0), &fr, &[p]);
    let br = panel_vs(VertexIndex(3), InstanceIndex(0), &fr, &[p]);
    assert_eq!(tl.local, vec2(-28.0, -23.0));
    assert_eq!(br.local, vec2(28.0, 23.0));
    assert_eq!(tl.pos.0, to_ndc(vec2(2.0, 12.0), fr.view).extend(0.0).extend(1.0));
    assert_eq!(*br.inst, 0);
}

/// A plain panel's middle is its fill, premultiplied, times its opacity; far outside it, nothing.
#[test]
fn panel_fs_fills_a_plain_plate() {
    let fr = frame();
    let p = PanelInst {
        rect: vec4(10.0, 20.0, 40.0, 30.0),
        fill_l: vec4(1.0, 0.5, 0.25, 0.8),
        shape: vec4(4.0, 2.0, 0.0, 0.0),
        fx2: vec4(0.0, 0.5, 0.0, 0.0),
        ..PanelInst::default()
    };
    let at = |local| panel_fs(PanelV { pos: Position::default(), local, inst: Flat(0) }, &fr, &[p]);
    let a = 0.8 * 0.5;
    assert_eq!(at(vec2(0.0, 0.0)), vec4(1.0 * 0.8 * 0.5, 0.5 * 0.8 * 0.5, 0.25 * 0.8 * 0.5, a));
    assert_eq!(at(vec2(25.0, 0.0)), Vec4::splat(0.0));
}

/// line_vs's box holds the capsule: along the segment from reach before a to reach past b, reach to
/// each side, reach = half width + two device pixels.
#[test]
fn line_vs_boxes_the_capsule() {
    let fr = frame();
    let l = LineInst { seg: vec4(10.0, 10.0, 30.0, 10.0), shape: vec4(2.0, 0.0, 0.0, 1.0), ..LineInst::default() };
    let corners: Vec<Vec2> = (0..4).map(|v| line_vs(VertexIndex(v), InstanceIndex(0), &fr, &[l]).pt).collect();
    assert_eq!(corners, [vec2(7.0, 7.0), vec2(33.0, 7.0), vec2(7.0, 13.0), vec2(33.0, 13.0)]);
}

/// line_fs: on the segment the whole colour (premultiplied, times opacity), past its half width
/// nothing, and a pixel beyond the join's bisector belongs to the neighbour.
#[test]
fn line_fs_covers_the_stroke_and_splits_its_joins() {
    let fr = frame();
    let l = LineInst {
        seg: vec4(10.0, 10.0, 30.0, 10.0),
        around: vec4(10.0, 30.0, 0.0, 0.0),
        col_l: vec4(1.0, 1.0, 1.0, 1.0),
        shape: vec4(2.0, 1.0, 0.0, 0.5),
        ..LineInst::default()
    };
    let at = |pt| line_fs(LineV { pos: Position::default(), pt, inst: Flat(0) }, &fr, &[l]);
    assert_eq!(at(vec2(20.0, 10.0)), Vec4::splat(0.5));
    assert_eq!(at(vec2(20.0, 14.0)), Vec4::splat(0.0));
    // The previous segment comes up from (10, 30): the bisector at a is the diagonal toward (1, -1), so
    // (9, 11), on the previous segment's side, is its pixel.
    assert_eq!(at(vec2(9.0, 11.0)), Vec4::splat(0.0));
}

/// path_fs: a square's middle is inside under either rule (winding 1), its outside is not.
#[test]
fn path_fs_fills_by_winding() {
    let fr = frame();
    let edges = [vec4(0.0, 0.0, 10.0, 0.0), vec4(10.0, 0.0, 10.0, 10.0), vec4(10.0, 10.0, 0.0, 10.0), vec4(0.0, 10.0, 0.0, 0.0)];
    for rule in [0.0, 1.0] {
        let p = PathInst { col_l: Vec4::splat(1.0), shape: vec4(0.0, 4.0, rule, 1.0), ..PathInst::default() };
        let at = |pt| path_fs(LineV { pos: Position::default(), pt, inst: Flat(0) }, &fr, &[p], &edges);
        assert_eq!(at(vec2(5.0, 5.0)), Vec4::splat(1.0), "rule {rule}");
        assert_eq!(at(vec2(15.0, 5.0)), Vec4::splat(0.0), "rule {rule}");
    }
}

/// image_vs, surface_vs, path_vs and sprite_vs put their first corner at the rect's top left and the
/// last at its bottom right (path_vs two device pixels out; sprite_vs from device pixels to points).
#[test]
fn quads_cover_their_rects() {
    let fr = frame();
    let rect = vec4(10.0, 20.0, 40.0, 30.0);
    let img = ImageInst { rect, ..ImageInst::default() };
    let (a, b) = (image_vs(VertexIndex(0), InstanceIndex(0), &fr, &[img]), image_vs(VertexIndex(3), InstanceIndex(0), &fr, &[img]));
    assert_eq!((a.x, a.y, b.x, b.y, a.uv, b.uv), (10.0, 20.0, 50.0, 50.0, vec2(0.0, 0.0), vec2(1.0, 1.0)));
    let sf = SurfaceInst { rect, uv: vec4(0.25, 0.5, 0.75, 1.0), ..SurfaceInst::default() };
    let (a, b) = (surface_vs(VertexIndex(0), InstanceIndex(0), &fr, &[sf]), surface_vs(VertexIndex(3), InstanceIndex(0), &fr, &[sf]));
    assert_eq!((a.local, b.local, a.uv, b.uv), (vec2(-20.0, -15.0), vec2(20.0, 15.0), vec2(0.25, 0.5), vec2(0.75, 1.0)));
    let pa = PathInst { rect, ..PathInst::default() };
    assert_eq!(path_vs(VertexIndex(0), InstanceIndex(0), &fr, &[pa]).pt, vec2(9.0, 19.0));
    assert_eq!(path_vs(VertexIndex(3), InstanceIndex(0), &fr, &[pa]).pt, vec2(51.0, 51.0));
    let sp = SpriteInst { rect: ivec4(20, 40, 16, 32), ..SpriteInst::default() };
    let s = sprite_vs(VertexIndex(3), InstanceIndex(0), &fr, &[sp]);
    assert_eq!(s.pos.0, to_ndc(vec2(18.0, 36.0), fr.view).extend(0.0).extend(1.0));
}

/// A bitmap glyph is invisible before its birth and, long after, its colour's alpha times its opacity
/// with no rise left: the spring's step response 1 - (1 + x) e^-x goes 0 to 1.
#[test]
fn glyph_vs_settles_its_spring() {
    let g = GlyphInst {
        rect: vec4(10.0, 20.0, 8.0, 16.0),
        uv: vec4(0.0, 0.0, 0.5, 0.5),
        col_l: vec4(1.0, 1.0, 1.0, 0.8),
        anim: vec4(1.0, 20.0, 5.0, 0.5),
        ..GlyphInst::default()
    };
    let at = |time| glyph_vs(VertexIndex(0), InstanceIndex(0), &Frame { time, ..frame() }, &[g]);
    assert_eq!(at(0.5).color.w, 0.0);
    let late = at(100.0);
    assert!((late.color.w - 0.4).abs() < 1e-6 && late.y == 20.0, "{:?} at {}", late.color, late.y);
    assert_eq!(glyph_vs(VertexIndex(3), InstanceIndex(0), &Frame { time: 100.0, ..frame() }, &[g]).uv, vec2(0.5, 0.5));
}

/// An outline glyph's quad is dilated half a device pixel out on every side: with m3 = (0, 0, 0, 1),
/// s = 1 and t = 0, so d = n / |(m0.x n̂.x dim.x, m1.y n̂.y dim.y)| = n / (2 * 30 * 2) ems = 1/120 em
/// each way, 0.25 points at 30 points to the em and 2 pixels to the point. The em coordinates move
/// with it, and the clip position is the moved point's.
#[test]
fn glyph_vs_dilates_an_outline_half_a_pixel() {
    let fr = Frame { live: 2.0, time: 100.0, ..frame() };
    let g = GlyphInst {
        rect: vec4(10.0, 20.0, 30.0, 30.0),
        uv: vec4(0.0, 1.0, 1.0, 0.0),
        col_l: Vec4::splat(1.0),
        anim: vec4(0.0, 20.0, 0.0, 1.0),
        kind: vec4(0.0, 0.0, 0.0, 1.0),
        ..GlyphInst::default()
    };
    let tl = glyph_vs(VertexIndex(0), InstanceIndex(0), &fr, &[g]);
    let br = glyph_vs(VertexIndex(3), InstanceIndex(0), &fr, &[g]);
    let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
    assert!(near(tl.x, 9.75) && near(tl.y, 19.75), "top left at ({}, {})", tl.x, tl.y);
    assert!(near(br.x, 40.25) && near(br.y, 50.25), "bottom right at ({}, {})", br.x, br.y);
    assert!(near(tl.uv.x, -1.0 / 120.0) && near(tl.uv.y, 1.0 + 1.0 / 120.0), "{:?}", tl.uv);
    let ndc = to_ndc(vec2(tl.x, tl.y), fr.view);
    assert!(near(tl.pos.0.x, ndc.x) && near(tl.pos.0.y, ndc.y), "{:?} vs {ndc:?}", tl.pos.0);
}

/// The binding table: per entry, per table, in parameter order, from 0.
#[test]
fn harness_slots() {
    use gpu::{Slot, Table};
    let b = |index| Slot { table: Table::Buffer, index };
    let t = |index| Slot { table: Table::Texture, index };
    let s = |index| Slot { table: Table::Sampler, index };
    assert_eq!((panel_vs::frame, panel_vs::insts, panel_fs::frame, panel_fs::insts), (b(0), b(1), b(0), b(1)));
    assert_eq!((path_fs::frame, path_fs::insts, path_fs::edges), (b(0), b(1), b(2)));
    assert_eq!((image_fs::tex, image_fs::smp, surface_fs::tex, surface_fs::smp), (t(0), s(0), t(0), s(0)));
    assert_eq!(sprite_fs::insts, b(0));
    assert_eq!((glyph_fs::frame, glyph_fs::atlas, glyph_fs::smp, glyph_fs::curves, glyph_fs::bands), (b(0), t(0), s(0), b(1), b(2)));
}
