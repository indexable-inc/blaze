//! Deterministic 2D texture sampling (bilinear, stochastic hex-tiling anti-repetition, and
//! world-space triplanar mapping), tangent-space normal decoding & RNM blending, and
//! deterministic 3D value noise / fBm.

#![blaze::sans_io]

use blaze_math::{Vec3, cos, sin, sqrt};
use blaze_own::{Alloc, Vec};

/// Borrowed RGBA8 image view (row-major, top-to-bottom, 4 bytes per pixel).
#[derive(Clone, Copy, Debug)]
pub struct TextureView<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

/// Owned RGBA8 image buffer in a named allocator.
#[derive(Debug)]
pub struct OwnedTexture<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8, Alloc<'a>>,
}

impl<'a> OwnedTexture<'a> {
    #[inline]
    pub fn view(&self) -> TextureView<'_> {
        TextureView { width: self.width, height: self.height, pixels: &self.pixels }
    }
}

/// Wrapping bilinear sample of an RGBA8 texture in normalized `[0, 1]` channels.
pub fn sample_bilinear(tex: &TextureView<'_>, uv: [f64; 2]) -> [f64; 4] {
    if tex.width == 0 || tex.height == 0 || tex.pixels.len() < (tex.width as usize) * (tex.height as usize) * 4 {
        return [1.0, 1.0, 1.0, 1.0];
    }
    let w = tex.width as usize;
    let h = tex.height as usize;
    let u = uv[0].rem_euclid(1.0);
    let v = (1.0 - uv[1]).rem_euclid(1.0);

    let fx = u * (w as f64) - 0.5;
    let fy = v * (h as f64) - 0.5;
    let x0 = core::f64::math::floor(fx) as isize;
    let y0 = core::f64::math::floor(fy) as isize;
    let tx = fx - (x0 as f64);
    let ty = fy - (y0 as f64);

    let ix0 = x0.rem_euclid(w as isize) as usize;
    let iy0 = y0.rem_euclid(h as isize) as usize;
    let ix1 = (ix0 + 1) % w;
    let iy1 = (iy0 + 1) % h;

    let p00 = load_px(tex.pixels, iy0 * w + ix0);
    let p10 = load_px(tex.pixels, iy0 * w + ix1);
    let p01 = load_px(tex.pixels, iy1 * w + ix0);
    let p11 = load_px(tex.pixels, iy1 * w + ix1);

    let mut out = [0.0; 4];
    for c in 0..4 {
        let top = p00[c] + (p10[c] - p00[c]) * tx;
        let bot = p01[c] + (p11[c] - p01[c]) * tx;
        out[c] = top + (bot - top) * ty;
    }
    out
}

/// Stochastic hex-tile sampling (Deliot & Heitz 2018): eliminates visible periodic tiling on
/// arbitrarily large architectural/ship surfaces by randomly offsetting and rotating 3 overlapping
/// triangular lattice tiles per shading point with contrast-preserving weights.
pub fn sample_hex_tile(tex: &TextureView<'_>, uv: [f64; 2], scale: f64) -> [f64; 4] {
    // Transform UV into equilateral triangular grid coordinates.
    let s = scale.max(1e-4);
    let p = [uv[0] * s * 3.464101615, uv[1] * s * 3.464101615];
    // Skew matrix: [1, -1/sqrt(3); 0, 2/sqrt(3)]
    let gx = p[0] - p[1] * 0.577350269;
    let gy = p[1] * 1.154700538;

    let vx = core::f64::math::floor(gx);
    let vy = core::f64::math::floor(gy);
    let fx = gx - vx;
    let fy = gy - vy;

    let (v1, v2, v3, w1, w2, w3) = if fx + fy < 1.0 {
        ([vx, vy], [vx + 1.0, vy], [vx, vy + 1.0], 1.0 - fx - fy, fx, fy)
    } else {
        ([vx + 1.0, vy + 1.0], [vx + 1.0, vy], [vx, vy + 1.0], fx + fy - 1.0, 1.0 - fy, 1.0 - fx)
    };

    // Sharp contrast-preserving blend weights (avoids washed-out ghosting at tile seams).
    let p1 = w1 * w1 * w1;
    let p2 = w2 * w2 * w2;
    let p3 = w3 * w3 * w3;
    let inv_sum = 1.0 / (p1 + p2 + p3).max(1e-9);
    let bw1 = p1 * inv_sum;
    let bw2 = p2 * inv_sum;
    let bw3 = p3 * inv_sum;

    let c1 = sample_bilinear(tex, hex_cell_uv(uv, v1));
    let c2 = sample_bilinear(tex, hex_cell_uv(uv, v2));
    let c3 = sample_bilinear(tex, hex_cell_uv(uv, v3));

    [
        c1[0] * bw1 + c2[0] * bw2 + c3[0] * bw3,
        c1[1] * bw1 + c2[1] * bw2 + c3[1] * bw3,
        c1[2] * bw1 + c2[2] * bw2 + c3[2] * bw3,
        c1[3] * bw1 + c2[3] * bw2 + c3[3] * bw3,
    ]
}

fn hex_cell_uv(uv: [f64; 2], cell: [f64; 2]) -> [f64; 2] {
    let h1 = hash2(cell[0] as i64, cell[1] as i64, 0x9e37_79b9);
    let h2 = hash2(cell[0] as i64, cell[1] as i64, 0x85eb_ca6b);
    let h3 = hash2(cell[0] as i64, cell[1] as i64, 0xc2b2_ae35);
    // Rotate by a subtle deterministic angle (`±20°` keeps wood/brushed grain aligned while
    // breaking grid periodicity; or full 360° for isotropic stone/concrete).
    let ang = (h3 - 0.5) * 0.35;
    let c = cos(ang);
    let s = sin(ang);
    [uv[0] * c - uv[1] * s + h1, uv[0] * s + uv[1] * c + h2]
}

/// World-space triplanar texture sampling (zero UV seams on arbitrary CAD geometry).
pub fn sample_triplanar(tex: &TextureView<'_>, pos: Vec3, normal: Vec3, tile_size_m: f64, hex_tile: bool) -> [f64; 4] {
    let inv = 1.0 / tile_size_m.max(1e-4);
    let nx = normal.x.abs();
    let ny = normal.y.abs();
    let nz = normal.z.abs();
    let wx = nx * nx * nx * nx;
    let wy = ny * ny * ny * ny;
    let wz = nz * nz * nz * nz;
    let inv_w = 1.0 / (wx + wy + wz).max(1e-9);
    let (wx, wy, wz) = (wx * inv_w, wy * inv_w, wz * inv_w);

    let uv_x = [pos.y * inv, pos.z * inv];
    let uv_y = [pos.x * inv, pos.z * inv];
    let uv_z = [pos.x * inv, pos.y * inv];

    let sx = if hex_tile { sample_hex_tile(tex, uv_x, 0.5) } else { sample_bilinear(tex, uv_x) };
    let sy = if hex_tile { sample_hex_tile(tex, uv_y, 0.5) } else { sample_bilinear(tex, uv_y) };
    let sz = if hex_tile { sample_hex_tile(tex, uv_z, 0.5) } else { sample_bilinear(tex, uv_z) };

    [
        sx[0] * wx + sy[0] * wy + sz[0] * wz,
        sx[1] * wx + sy[1] * wy + sz[1] * wz,
        sx[2] * wx + sy[2] * wy + sz[2] * wz,
        sx[3] * wx + sy[3] * wy + sz[3] * wz,
    ]
}

/// Decode an RGBA tangent-space normal sample (`[0, 1]` channels) with normal scale `strength`.
#[inline]
pub fn decode_tangent_normal(rgba: [f64; 4], strength: f64) -> [f64; 3] {
    let x = (rgba[0] * 2.0 - 1.0) * strength;
    let y = (rgba[1] * 2.0 - 1.0) * strength;
    let z = (rgba[2] * 2.0 - 1.0).max(1e-4);
    let inv = 1.0 / sqrt(x * x + y * y + z * z).max(1e-9);
    [x * inv, y * inv, z * inv]
}

/// Reoriented Normal Mapping (Barré-Brisebois & Hill 2012): combines a base tangent-space
/// normal (e.g. baked SDF bevel map) with a detail tangent-space normal (e.g. CC0 micro-grain)
/// without flattening either!
#[inline]
pub fn rnm_blend(base: [f64; 3], detail: [f64; 3]) -> [f64; 3] {
    let t = [base[0], base[1], base[2] + 1.0];
    let u = [-detail[0], -detail[1], detail[2]];
    let d = t[0] * u[0] + t[1] * u[1] + t[2] * u[2];
    let inv_tz = 1.0 / t[2].max(1e-6);
    let r = [t[0] * d * inv_tz - u[0], t[1] * d * inv_tz - u[1], t[2] * d * inv_tz - u[2]];
    let inv = 1.0 / sqrt(r[0] * r[0] + r[1] * r[1] + r[2] * r[2]).max(1e-9);
    [r[0] * inv, r[1] * inv, r[2] * inv]
}

/// Perturb world normal `n` by tangent-space normal `tn` using orthonormal `(t, b, n)`.
#[inline]
pub fn perturb_normal(n: Vec3, t: Vec3, b: Vec3, tn: [f64; 3]) -> Vec3 {
    (t * tn[0] + b * tn[1] + n * tn[2]).normalized().unwrap_or(n)
}

/// Deterministic 3D smooth value noise in `[0, 1]` (zero RNG, identical on all targets).
pub fn noise3(p: Vec3) -> f64 {
    let ix = core::f64::math::floor(p.x) as i64;
    let iy = core::f64::math::floor(p.y) as i64;
    let iz = core::f64::math::floor(p.z) as i64;
    let fx = p.x - (ix as f64);
    let fy = p.y - (iy as f64);
    let fz = p.z - (iz as f64);
    let ux = fx * fx * (3.0 - 2.0 * fx);
    let uy = fy * fy * (3.0 - 2.0 * fy);
    let uz = fz * fz * (3.0 - 2.0 * fz);

    let c000 = hash3(ix, iy, iz);
    let c100 = hash3(ix + 1, iy, iz);
    let c010 = hash3(ix, iy + 1, iz);
    let c110 = hash3(ix + 1, iy + 1, iz);
    let c001 = hash3(ix, iy, iz + 1);
    let c101 = hash3(ix + 1, iy, iz + 1);
    let c011 = hash3(ix, iy + 1, iz + 1);
    let c111 = hash3(ix + 1, iy + 1, iz + 1);

    let x00 = c000 + (c100 - c000) * ux;
    let x10 = c010 + (c110 - c010) * ux;
    let x01 = c001 + (c101 - c001) * ux;
    let x11 = c011 + (c111 - c011) * ux;
    let y0 = x00 + (x10 - x00) * uy;
    let y1 = x01 + (x11 - x01) * uy;
    y0 + (y1 - y0) * uz
}

/// Multi-octave 3D fractional Brownian motion in `[0, 1]`.
pub fn fbm3(p: Vec3, octaves: u8) -> f64 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut freq = 1.0;
    let mut norm = 0.0;
    for _ in 0..octaves.min(8) {
        sum += amp * noise3(p * freq);
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    if norm > 0.0 { sum / norm } else { 0.5 }
}

/// Deterministic 2D hash in `[0, 1)`.
#[inline]
pub fn hash2(x: i64, y: i64, seed: u64) -> f64 {
    let mut h = (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (y as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)
        ^ seed;
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    ((h >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
}

/// Deterministic 3D hash in `[0, 1)`.
#[inline]
pub fn hash3(x: i64, y: i64, z: i64) -> f64 {
    let mut h = (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (y as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)
        ^ (z as u64).wrapping_mul(0x94d0_49bb_1331_11eb);
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    ((h >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
}

#[inline]
fn load_px(pixels: &[u8], idx: usize) -> [f64; 4] {
    let off = idx * 4;
    [
        f64::from(pixels[off]) / 255.0,
        f64::from(pixels[off + 1]) / 255.0,
        f64::from(pixels[off + 2]) / 255.0,
        f64::from(pixels[off + 3]) / 255.0,
    ]
}
