//! CC0 PBR texture pack descriptors and deterministic procedural CC0-format texture synthesis
//! (seamless tileable albedo, tangent-space normal, and glTF-standard packed ORM maps).
//!
//! Works seamlessly with external CC0 PBR packs (AmbientCG, Poly Haven) loaded as RGBA8 images
//! as well as built-in zero-dependency synthesized packs for `#![no_std]` builds and tests.

#![blaze::sans_io]

use blaze_math::{Vec3, cos, sin, sqrt};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::texture::{OwnedTexture, TextureView, fbm3, hash2};
use crate::tonemap::{linear_to_srgb, quantize_u8};

/// Borrowed CC0 PBR texture pack (compatible with AmbientCG / Poly Haven / glTF 2.0 ORM).
#[derive(Clone, Copy, Default, Debug)]
pub struct Cc0PackView<'a> {
    /// sRGB RGBA8 base colour map.
    pub albedo: Option<TextureView<'a>>,
    /// Linear tangent-space RGB normal map (`[128, 128, 255]` = flat).
    pub normal: Option<TextureView<'a>>,
    /// glTF 2.0 packed ORM map: `R = AO`, `G = Roughness`, `B = Metallic`.
    pub orm: Option<TextureView<'a>>,
    /// Normal map bump strength (default `1.0`).
    pub normal_scale: f64,
    /// Whether to apply stochastic hex-tiling to eliminate periodic repetition on large spans.
    pub hex_tile: bool,
    /// UV repeat scale (`1.0` = use mesh metric UVs verbatim).
    pub uv_scale: [f64; 2],
}

impl<'a> Cc0PackView<'a> {
    pub const NONE: Cc0PackView<'static> = Cc0PackView {
        albedo: None,
        normal: None,
        orm: None,
        normal_scale: 1.0,
        hex_tile: false,
        uv_scale: [1.0, 1.0],
    };
}

/// Owned deterministic tileable PBR texture pack (ready to embed into `.glb` via `ImageData::Rgba8`
/// or pass directly to `blaze-cad-picture` via [`SynthesizedPack::view`]).
#[derive(Debug)]
pub struct SynthesizedPack<'a> {
    pub albedo: OwnedTexture<'a>,
    pub normal: OwnedTexture<'a>,
    /// glTF 2.0 packed ORM: `R = AO`, `G = Roughness`, `B = Metallic`, `A = 255`.
    pub orm: OwnedTexture<'a>,
}

impl<'a> SynthesizedPack<'a> {
    pub fn view(&self, normal_scale: f64, hex_tile: bool) -> Cc0PackView<'_> {
        Cc0PackView {
            albedo: Some(self.albedo.view()),
            normal: Some(self.normal.view()),
            orm: Some(self.orm.view()),
            normal_scale,
            hex_tile,
            uv_scale: [1.0, 1.0],
        }
    }
}

/// Built-in procedural CC0-style seamless tileable material generators.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ProceduralKind {
    /// Horizontal milled timber planks with flowing organic wood grain, knots, bevel plank seams,
    /// and realistic per-plank colour & roughness variation.
    TimberPlanks {
        /// Base linear RGB wood colour (e.g. `[0.34, 0.19, 0.09]`).
        base_rgb: [f64; 3],
        /// Number of planks across one vertical tile (`4..=12`).
        planks: u32,
    },
    /// Continuous solid milled hardwood (oak, walnut, cedar timber posts, beams, window mullions,
    /// and paneled doors) with elongated natural wood grain and zero clapboard seams.
    MilledWood {
        /// Base linear RGB wood colour (e.g. `[0.16, 0.085, 0.042]` for stained oak).
        base_rgb: [f64; 3],
        /// Base surface roughness (`0.38..=0.62` for oiled/satin architectural timber).
        roughness: f64,
    },
    /// Overlapping weathered slate roof shingles with beveled courses, cleft stone micro-relief,
    /// and slight mineral colour variation.
    SlateShingles {
        base_rgb: [f64; 3],
        rows: u32,
        cols: u32,
    },
    /// Coursed architectural ashlar stone blocks with recessed mortar joints and chiseled texture.
    AshlarStone {
        base_rgb: [f64; 3],
        rows: u32,
        cols: u32,
    },
    /// Aerospace/naval painted hull plating with recessed panel seams, corner rivets, subtle
    /// orange-peel micro-texture, and oil streaks.
    HullPlating {
        base_rgb: [f64; 3],
        metallic: f64,
        roughness: f64,
        panels: u32,
    },
    /// Directional brushed/machined titanium or steel with fine anisotropic streaks.
    BrushedMetal {
        base_rgb: [f64; 3],
        roughness: f64,
    },
    /// Diagonal 2×2 twill aerospace carbon-fiber composite weave.
    CarbonWeave {
        base_rgb: [f64; 3],
        roughness: f64,
    },
    /// Hand-forged matte architectural wrought iron or blackened steel with micro-hammering.
    WroughtIron {
        base_rgb: [f64; 3],
        roughness: f64,
    },
    /// Architectural aged copper or standing-seam zinc with subtle verdigris patina in crevices.
    AgedCopper {
        base_rgb: [f64; 3],
        /// Patina blend strength in `0.0..=1.0`.
        patina: f64,
    },
    /// Deep furrowed alpine conifer/spruce bark with warm inner fissures and mossy ridges.
    TreeBark {
        base_rgb: [f64; 3],
    },
    /// Layered alpine spruce/pine needle clusters with organic colour variation and deep shade.
    PineFoliage {
        base_rgb: [f64; 3],
    },
    /// Fractured natural alpine granite / basalt rock with mineral speckles, dark crevices,
    /// and organic moss/lichen patches (zero masonry grid — ideal for boulders & flagstones).
    WeatheredRock {
        base_rgb: [f64; 3],
        /// Moss & lichen coverage strength in `0.0..=1.0`.
        moss: f64,
    },
}

/// Synthesize a seamless tileable `resolution × resolution` PBR pack (`albedo`, `normal`, `orm`)
/// deterministically in `alloc`.
pub fn synthesize_cc0_pack<'a>(
    kind: ProceduralKind,
    resolution: u32,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<SynthesizedPack<'a>, AllocFailure> {
    let res = resolution.max(16) as usize;
    let n = res * res;

    let mut height: Vec<f64, Alloc<'_>> = Vec::try_with_capacity_in(n, scratch)?;
    let mut albedo_px: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n * 4, alloc)?;
    let mut orm_px: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n * 4, alloc)?;
    let mut normal_px: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n * 4, alloc)?;

    let inv_res = 1.0 / (res as f64);

    for py in 0..res {
        let v = (py as f64 + 0.5) * inv_res;
        for px in 0..res {
            let u = (px as f64 + 0.5) * inv_res;
            let s = eval_procedural_sample(kind, u, v);
            height.try_push(s.height).map_err(|r| r.why)?;

            albedo_px.try_push(quantize_u8(linear_to_srgb(s.albedo[0]))).map_err(|r| r.why)?;
            albedo_px.try_push(quantize_u8(linear_to_srgb(s.albedo[1]))).map_err(|r| r.why)?;
            albedo_px.try_push(quantize_u8(linear_to_srgb(s.albedo[2]))).map_err(|r| r.why)?;
            albedo_px.try_push(255).map_err(|r| r.why)?;

            orm_px.try_push(quantize_u8(s.ao)).map_err(|r| r.why)?;
            orm_px.try_push(quantize_u8(s.roughness)).map_err(|r| r.why)?;
            orm_px.try_push(quantize_u8(s.metallic)).map_err(|r| r.why)?;
            orm_px.try_push(255).map_err(|r| r.why)?;
        }
    }

    // Derive seamless periodic tangent-space normal map via central differences of `height`.
    let bump_gain = match kind {
        ProceduralKind::TimberPlanks { .. } => 0.85,
        ProceduralKind::MilledWood { .. } => 0.45,
        ProceduralKind::SlateShingles { .. } => 1.45,
        ProceduralKind::AshlarStone { .. } => 1.55,
        ProceduralKind::HullPlating { .. } => 1.20,
        ProceduralKind::BrushedMetal { .. } => 0.20,
        ProceduralKind::CarbonWeave { .. } => 0.75,
        ProceduralKind::WroughtIron { .. } => 0.85,
        ProceduralKind::AgedCopper { .. } => 0.40,
        ProceduralKind::TreeBark { .. } => 1.95,
        ProceduralKind::PineFoliage { .. } => 1.35,
        ProceduralKind::WeatheredRock { .. } => 1.85,
    };
    for py in 0..res {
        let ym = (py + res - 1) % res;
        let yp = (py + 1) % res;
        for px in 0..res {
            let xm = (px + res - 1) % res;
            let xp = (px + 1) % res;
            let dh_dx = (height[py * res + xp] - height[py * res + xm]) * 0.5 * (res as f64) * 0.06 * bump_gain;
            let dh_dy = (height[yp * res + px] - height[ym * res + px]) * 0.5 * (res as f64) * 0.06 * bump_gain;
            // OpenGL / glTF tangent-space normal (+Y up, image top-to-bottom -> flip dh_dy).
            let nx = -dh_dx;
            let ny = dh_dy;
            let nz = 1.0;
            let inv = 1.0 / sqrt(nx * nx + ny * ny + nz * nz);
            normal_px.try_push(quantize_u8(nx * inv * 0.5 + 0.5)).map_err(|r| r.why)?;
            normal_px.try_push(quantize_u8(ny * inv * 0.5 + 0.5)).map_err(|r| r.why)?;
            normal_px.try_push(quantize_u8(nz * inv * 0.5 + 0.5)).map_err(|r| r.why)?;
            normal_px.try_push(255).map_err(|r| r.why)?;
        }
    }

    Ok(SynthesizedPack {
        albedo: OwnedTexture { width: res as u32, height: res as u32, pixels: albedo_px },
        normal: OwnedTexture { width: res as u32, height: res as u32, pixels: normal_px },
        orm: OwnedTexture { width: res as u32, height: res as u32, pixels: orm_px },
    })
}

struct Sample {
    albedo: [f64; 3],
    roughness: f64,
    metallic: f64,
    ao: f64,
    height: f64,
}

/// Seamless tileable 2D noise mapped onto a 4D torus-projected 3D loop so edges wrap with zero seam.
#[inline]
fn tile_fbm(u: f64, v: f64, freq_u: f64, freq_v: f64, octaves: u8) -> f64 {
    let au = u * core::f64::consts::TAU;
    let av = v * core::f64::consts::TAU;
    let p = Vec3::new(
        cos(au) * freq_u * 0.5 + sin(av) * freq_v * 0.17,
        sin(au) * freq_u * 0.5 + cos(av) * freq_v * 0.5,
        sin(av) * freq_v * 0.5 - cos(au) * freq_u * 0.17,
    );
    fbm3(p, octaves)
}

fn eval_procedural_sample(kind: ProceduralKind, u: f64, v: f64) -> Sample {
    match kind {
        ProceduralKind::TimberPlanks { base_rgb, planks } => {
            let np = planks.max(2) as f64;
            let row_f = v * np;
            let row = core::f64::math::floor(row_f) as i64;
            let rv = row_f - (row as f64);
            let stagger = hash2(row, 11, 0x1234);
            let col_f = (u + stagger).rem_euclid(1.0);
            let col = core::f64::math::floor(col_f) as i64;
            let cu = col_f - (col as f64);

            // Real architectural Western Red Cedar shiplap/clapboard: crisp horizontal shadow-line
            // lap groove + ultra-subtle tight end-grain hairline butt joint (1 per board span).
            let seam_v = (rv.min(1.0 - rv) / 0.036).clamp(0.0, 1.0);
            let seam_u = 0.72 + 0.28 * (cu.min(1.0 - cu) / 0.0045).clamp(0.0, 1.0);
            let seam = (seam_v * seam_u).clamp(0.0, 1.0);
            let lap_slope = rv; // subtle clapboard taper toward bottom drip edge
            let bevel = (rv.min(1.0 - rv) / 0.09).clamp(0.0, 1.0);

            // Kiln-dried cedar: elongated natural wood fibers along U + rich organic cathedrals.
            let plank_seed = hash2(row, col, 0x5678);
            let warp = tile_fbm(u, v, 1.4, 4.2, 3);
            let rings = sin((rv * 11.0 + warp * 2.8 + plank_seed * 9.0) * core::f64::consts::PI) * 0.5 + 0.5;
            let fine = tile_fbm(u, v, 1.6, 32.0, 3);
            let streak = tile_fbm(u, v, 0.9, 18.0, 2);
            let grain = 0.45 * rings + 0.35 * fine + 0.20 * streak;

            // Natural board-to-board honey / warm-chestnut tonal variation.
            let h_tone = hash2(row, 19, 0x9abc);
            let h_warm = hash2(row + 7, 5, 0x4242);
            let plank_tone = 0.76 + 0.44 * h_tone;
            let tone = plank_tone * (0.78 + 0.38 * grain) * (0.30 + 0.70 * seam);
            let albedo = [
                (base_rgb[0] * tone * (0.93 + 0.16 * h_warm)).clamp(0.01, 0.95),
                (base_rgb[1] * tone * (0.90 + 0.14 * h_warm)).clamp(0.01, 0.95),
                (base_rgb[2] * tone * (0.85 + 0.14 * (1.0 - h_warm))).clamp(0.01, 0.95),
            ];
            let roughness = (0.48 + 0.26 * (1.0 - grain) + 0.12 * (1.0 - h_tone) + 0.18 * (1.0 - seam)).clamp(0.26, 0.96);
            let ao = (0.24 + 0.76 * seam) * (0.88 + 0.12 * grain);
            let height = 0.14 * seam_v + 0.06 * lap_slope + 0.03 * bevel + 0.020 * grain;
            Sample { albedo, roughness, metallic: 0.0, ao, height }
        }
        ProceduralKind::MilledWood { base_rgb, roughness } => {
            // Continuous flowing solid hardwood grain (ideal for structural posts, beams, mullions & doors).
            let warp = tile_fbm(u, v, 1.2, 4.5, 3);
            let rings = sin((v * 14.0 + warp * 3.0) * core::f64::consts::PI) * 0.5 + 0.5;
            let fibers = tile_fbm(u, v, 1.6, 30.0, 3);
            let pores = tile_fbm(u, v, 4.0, 48.0, 2);
            let grain = 0.50 * rings + 0.35 * fibers + 0.15 * pores;
            let tone = 0.76 + 0.44 * grain;
            let albedo = [
                (base_rgb[0] * tone).clamp(0.01, 0.95),
                (base_rgb[1] * tone * 0.97).clamp(0.01, 0.95),
                (base_rgb[2] * tone * 0.92).clamp(0.01, 0.95),
            ];
            let r_out = (roughness + (1.0 - grain) * 0.22).clamp(0.22, 0.92);
            let ao = 0.86 + 0.14 * grain;
            let height = 0.028 * grain;
            Sample { albedo, roughness: r_out, metallic: 0.0, ao, height }
        }
        ProceduralKind::SlateShingles { base_rgb, rows, cols } => {
            let nr = rows.max(2) as f64;
            let nc = cols.max(2) as f64;
            // Subtle organic hand-riven drip-edge wave so courses read like authentic split Welsh slate!
            let edge_wave = (tile_fbm(u, v, 10.0, 5.0, 3) - 0.5) * 0.065;
            let rf = (v * nr + edge_wave).rem_euclid(nr);
            let row = core::f64::math::floor(rf) as i64;
            let rv = rf - (row as f64);
            let offset = if row.rem_euclid(2) == 0 { 0.0 } else { 0.5 };
            let cf = (u * nc + offset).rem_euclid(nc);
            let col = core::f64::math::floor(cf) as i64;
            let cu = cf - (col as f64);

            let edge_u = (cu.min(1.0 - cu) / 0.036).clamp(0.0, 1.0);
            let edge_v = (rv.min(1.0 - rv) / 0.062).clamp(0.0, 1.0);
            let drip_shadow = (rv / 0.12).clamp(0.0, 1.0);
            let lap = edge_u * edge_v;
            let cleft = tile_fbm(u, v, 6.0, 14.0, 4);
            let lichen = ((tile_fbm(u, v, 9.0, 9.0, 3) - 0.60) / 0.26).clamp(0.0, 1.0) * (1.0 - rv * 0.7);
            let chip = hash2(row, col, 0x3141);
            let tint = hash2(row + 13, col + 5, 0x7711);
            let tone = (0.64 + 0.62 * chip) * (0.76 + 0.36 * cleft) * (0.26 + 0.74 * lap) * (0.62 + 0.38 * drip_shadow);
            let mut albedo = [
                (base_rgb[0] * tone * (0.88 + 0.22 * tint)).clamp(0.01, 0.9),
                (base_rgb[1] * tone * (0.92 + 0.14 * chip)).clamp(0.01, 0.9),
                (base_rgb[2] * tone * (0.98 + 0.18 * (1.0 - tint))).clamp(0.01, 0.9),
            ];
            // Very subtle alpine moss/lichen whisper along shaded shingle laps.
            let moss_w = lichen * 0.20;
            albedo[0] = albedo[0] * (1.0 - moss_w) + 0.10 * moss_w;
            albedo[1] = albedo[1] * (1.0 - moss_w) + 0.13 * moss_w;
            albedo[2] = albedo[2] * (1.0 - moss_w) + 0.07 * moss_w;
            let roughness = (0.36 + 0.30 * (1.0 - cleft) + 0.16 * (1.0 - chip) + 0.22 * (1.0 - lap) + 0.12 * moss_w).clamp(0.20, 0.96);
            let ao = (0.22 + 0.78 * lap) * (0.70 + 0.30 * drip_shadow) * (0.84 + 0.16 * cleft);
            let height = 0.36 * (1.0 - rv) * edge_u + 0.055 * cleft + 0.12 * lap;
            Sample { albedo, roughness, metallic: 0.04, ao, height }
        }
        ProceduralKind::AshlarStone { base_rgb, rows, cols } => {
            let nr = rows.max(2) as f64;
            let nc = cols.max(2) as f64;
            let rf = v * nr;
            let row = core::f64::math::floor(rf) as i64;
            let rv = rf - (row as f64);
            let offset = if row.rem_euclid(2) == 0 { 0.0 } else { 0.47 };
            let cf = (u * nc + offset).rem_euclid(nc);
            let col = core::f64::math::floor(cf) as i64;
            let cu = cf - (col as f64);

            let jitter = (tile_fbm(u, v, 8.0, 8.0, 3) - 0.5) * 0.025;
            let m_u = ((cu.min(1.0 - cu) + jitter) / 0.048).clamp(0.0, 1.0);
            let m_v = ((rv.min(1.0 - rv) + jitter) / 0.068).clamp(0.0, 1.0);
            let block = m_u.min(m_v);

            let rock = tile_fbm(u, v, 10.0, 10.0, 4);
            let speckle = tile_fbm(u, v, 28.0, 28.0, 2);
            let block_tone = 0.78 + 0.40 * hash2(row, col, 0x2718);
            let block_warm = hash2(row + 9, col + 2, 0x6161);
            let mortar_rgb = [0.24, 0.23, 0.21];
            let stone_tone = block_tone * (0.80 + 0.28 * rock) * (0.92 + 0.16 * speckle);
            let albedo = [
                (mortar_rgb[0] + (base_rgb[0] * stone_tone * (0.94 + 0.14 * block_warm) - mortar_rgb[0]) * block).clamp(0.01, 0.92),
                (mortar_rgb[1] + (base_rgb[1] * stone_tone * (0.95 + 0.10 * block_warm) - mortar_rgb[1]) * block).clamp(0.01, 0.92),
                (mortar_rgb[2] + (base_rgb[2] * stone_tone * (0.92 + 0.14 * (1.0 - block_warm)) - mortar_rgb[2]) * block).clamp(0.01, 0.92),
            ];
            let roughness = (0.92 - 0.18 * rock * block - 0.08 * speckle * block).clamp(0.52, 0.98);
            let ao = (0.34 + 0.66 * block) * (0.86 + 0.14 * rock);
            let height = 0.26 * block + 0.075 * rock * block + 0.018 * speckle * block;
            Sample { albedo, roughness, metallic: 0.0, ao, height }
        }
        ProceduralKind::HullPlating { base_rgb, metallic, roughness, panels } => {
            let np = panels.max(1) as f64;
            let pf_u = u * np;
            let pf_v = v * np;
            let pi_u = core::f64::math::floor(pf_u) as i64;
            let pi_v = core::f64::math::floor(pf_v) as i64;
            let lu = pf_u - (pi_u as f64);
            let lv = pf_v - (pi_v as f64);

            // Crisp CNC recessed panel seam.
            let du = lu.min(1.0 - lu);
            let dv = lv.min(1.0 - lv);
            let seam = (du.min(dv) / 0.022).clamp(0.0, 1.0);

            // Recessed fasteners near the 4 corners of each panel!
            let rx = (lu - 0.08).abs().min((lu - 0.92).abs());
            let ry = (lv - 0.08).abs().min((lv - 0.92).abs());
            let rdist = sqrt(rx * rx + ry * ry);
            let rivet = (1.0 - (rdist / 0.024).clamp(0.0, 1.0)).powi(2);

            let peel = tile_fbm(u, v, 14.0, 14.0, 3);
            let panel_var = 0.90 + 0.20 * hash2(pi_u, pi_v, 0xbeef);
            let tone = panel_var * (0.92 + 0.16 * peel) * (0.45 + 0.55 * seam);
            let albedo = [
                (base_rgb[0] * tone).clamp(0.01, 0.95),
                (base_rgb[1] * tone).clamp(0.01, 0.95),
                (base_rgb[2] * tone).clamp(0.01, 0.95),
            ];
            let r_out = (roughness + (peel - 0.5) * 0.14 + 0.25 * (1.0 - seam)).clamp(0.08, 0.95);
            let ao = (0.45 + 0.55 * seam) * (1.0 - 0.28 * rivet);
            // Smooth satin aerospace sheet metal: crisp CNC seams & recessed rivets without lumpy peel!
            let height = 0.22 * seam - 0.10 * rivet + 0.004 * peel;
            Sample { albedo, roughness: r_out, metallic, ao, height }
        }
        ProceduralKind::BrushedMetal { base_rgb, roughness } => {
            let streaks = tile_fbm(u, v, 1.5, 28.0, 3);
            let micro = tile_fbm(u, v, 12.0, 12.0, 2);
            let tone = 0.92 + 0.16 * streaks;
            let albedo = [
                (base_rgb[0] * tone).clamp(0.02, 0.98),
                (base_rgb[1] * tone).clamp(0.02, 0.98),
                (base_rgb[2] * tone).clamp(0.02, 0.98),
            ];
            let r_out = (roughness + (streaks - 0.5) * 0.14 + (micro - 0.5) * 0.04).clamp(0.06, 0.85);
            Sample { albedo, roughness: r_out, metallic: 0.95, ao: 0.94 + 0.06 * streaks, height: 0.025 * streaks }
        }
        ProceduralKind::CarbonWeave { base_rgb, roughness } => {
            let freq = 24.0;
            let fu = u * freq;
            let fv = v * freq;
            let iu = core::f64::math::floor(fu) as i64;
            let iv = core::f64::math::floor(fv) as i64;
            let lu = fu - (iu as f64);
            let lv = fv - (iv as f64);
            // 2×2 diagonal twill weave: alternating warp/weft ribbons.
            let diag = (iu + iv).rem_euclid(4);
            let warp = diag < 2;
            let profile = if warp {
                sin(lu * core::f64::consts::PI) * (0.7 + 0.3 * sin(lv * core::f64::consts::PI))
            } else {
                sin(lv * core::f64::consts::PI) * (0.7 + 0.3 * sin(lu * core::f64::consts::PI))
            };
            let sheen_var = if warp { 1.18 } else { 0.82 };
            let tone = (0.55 + 0.65 * profile) * sheen_var;
            let albedo = [
                (base_rgb[0] * tone).clamp(0.015, 0.85),
                (base_rgb[1] * tone).clamp(0.015, 0.85),
                (base_rgb[2] * tone).clamp(0.015, 0.85),
            ];
            let r_out = (roughness + (1.0 - profile) * 0.22).clamp(0.14, 0.78);
            let ao = 0.65 + 0.35 * profile;
            Sample { albedo, roughness: r_out, metallic: 0.22, ao, height: 0.075 * profile }
        }
        ProceduralKind::WroughtIron { base_rgb, roughness } => {
            let hammer = tile_fbm(u, v, 8.0, 8.0, 4);
            let scale = tile_fbm(u, v, 24.0, 24.0, 3);
            let tone = 0.72 + 0.45 * hammer * (0.85 + 0.3 * scale);
            let albedo = [
                (base_rgb[0] * tone).clamp(0.015, 0.8),
                (base_rgb[1] * tone).clamp(0.015, 0.8),
                (base_rgb[2] * tone * 1.04).clamp(0.015, 0.8),
            ];
            let r_out = (roughness + (0.5 - hammer) * 0.22 + (0.5 - scale) * 0.12).clamp(0.24, 0.90);
            Sample { albedo, roughness: r_out, metallic: 0.88, ao: 0.78 + 0.22 * hammer, height: 0.09 * hammer + 0.03 * scale }
        }
        ProceduralKind::AgedCopper { base_rgb, patina } => {
            let blotch = tile_fbm(u, v, 5.0, 5.0, 4);
            let fine = tile_fbm(u, v, 18.0, 18.0, 3);
            let ox = ((blotch * 0.75 + fine * 0.25 - 0.42) * 2.4).clamp(0.0, 1.0) * patina.clamp(0.0, 1.0);
            let pat_col = [0.14, 0.36, 0.28];
            let metal_tone = 0.84 + 0.28 * fine;
            let albedo = [
                (base_rgb[0] * metal_tone * (1.0 - ox) + pat_col[0] * ox).clamp(0.02, 0.95),
                (base_rgb[1] * metal_tone * (1.0 - ox) + pat_col[1] * ox).clamp(0.02, 0.95),
                (base_rgb[2] * metal_tone * (1.0 - ox) + pat_col[2] * ox).clamp(0.02, 0.95),
            ];
            let metallic = (0.94 * (1.0 - ox * 0.85)).clamp(0.05, 0.98);
            let roughness = (0.28 + 0.56 * ox + (0.5 - fine) * 0.12).clamp(0.16, 0.92);
            Sample { albedo, roughness, metallic, ao: 0.86 + 0.14 * (1.0 - ox), height: 0.04 * fine + 0.03 * ox }
        }
        ProceduralKind::TreeBark { base_rgb } => {
            // Deep vertical conifer bark ridges and warm reddish inner fissures.
            let warp = tile_fbm(u, v, 3.0, 2.0, 3);
            let furrows = sin((u * 14.0 + warp * 2.6) * core::f64::consts::TAU) * 0.5 + 0.5;
            let plates = tile_fbm(u, v, 14.0, 4.5, 4);
            let ridge = (furrows * 0.60 + plates * 0.40).clamp(0.0, 1.0);
            let fissure = (ridge / 0.45).clamp(0.0, 1.0);
            let tone = 0.35 + 0.85 * ridge;
            let albedo = [
                (base_rgb[0] * tone * (1.08 - 0.12 * fissure)).clamp(0.01, 0.85),
                (base_rgb[1] * tone).clamp(0.01, 0.85),
                (base_rgb[2] * tone * 0.90).clamp(0.01, 0.85),
            ];
            Sample { albedo, roughness: (0.94 - 0.12 * ridge).clamp(0.70, 0.98), metallic: 0.0, ao: 0.25 + 0.75 * fissure, height: 0.26 * ridge }
        }
        ProceduralKind::PineFoliage { base_rgb } => {
            let clusters = tile_fbm(u, v, 9.0, 9.0, 4);
            let needles = tile_fbm(u, v, 32.0, 16.0, 3);
            let tip = ((clusters * 0.6 + needles * 0.4 - 0.35) * 1.6).clamp(0.0, 1.0);
            let tone = 0.42 + 0.85 * tip;
            let albedo = [
                (base_rgb[0] * tone * (0.90 + 0.22 * needles)).clamp(0.01, 0.75),
                (base_rgb[1] * tone * (0.95 + 0.18 * tip)).clamp(0.01, 0.80),
                (base_rgb[2] * tone * 0.88).clamp(0.01, 0.70),
            ];
            Sample { albedo, roughness: (0.76 - 0.18 * tip).clamp(0.42, 0.92), metallic: 0.0, ao: 0.32 + 0.68 * tip, height: 0.18 * clusters + 0.08 * needles }
        }
        ProceduralKind::WeatheredRock { base_rgb, moss } => {
            // Multi-scale ridged fracture granite with fine mica speckle & organic alpine moss.
            let strata = tile_fbm(u, v, 4.0, 6.5, 4);
            let ridge = 1.0 - (tile_fbm(u, v, 7.5, 7.5, 4) * 2.0 - 1.0).abs();
            let crack = ((ridge - 0.42) / 0.48).clamp(0.0, 1.0);
            let grain = tile_fbm(u, v, 24.0, 24.0, 3);
            let moss_n = ((tile_fbm(u, v, 5.5, 5.5, 3) - 0.48) * 2.6).clamp(0.0, 1.0) * moss.clamp(0.0, 1.0);
            let tone = (0.48 + 0.68 * strata) * (0.55 + 0.45 * crack) * (0.86 + 0.28 * grain);
            let moss_rgb = [0.075, 0.125, 0.052];
            let albedo = [
                (base_rgb[0] * tone * (1.0 - moss_n) + moss_rgb[0] * moss_n).clamp(0.015, 0.88),
                (base_rgb[1] * tone * (1.0 - moss_n) + moss_rgb[1] * moss_n).clamp(0.015, 0.88),
                (base_rgb[2] * tone * (1.0 - moss_n) + moss_rgb[2] * moss_n).clamp(0.015, 0.88),
            ];
            let roughness = (0.90 - 0.18 * grain + 0.08 * moss_n).clamp(0.58, 0.98);
            let ao = (0.28 + 0.72 * crack) * (0.80 + 0.20 * strata);
            let height = 0.22 * strata + 0.14 * crack + 0.03 * grain;
            Sample { albedo, roughness, metallic: 0.0, ao, height }
        }
    }
}
