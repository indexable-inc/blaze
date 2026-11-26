//! Context-aware smart-material layering driven by analytic SDF surface masks (`COLOR_0` or
//! baked mask maps): edge wear on convex bevels, grime/rust in concave crevices, directional
//! dust/weathering on upward-facing surfaces, and world-space macro roughness/albedo variation.

#![blaze::sans_io]

use blaze_math::Vec3;

use crate::brdf::SurfacePbr;
use crate::texture::fbm3;

/// Per-point geometric intelligence decoded from `COLOR_0` (`[u8; 4]`) or a baked SDF mask map.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SurfaceMasks {
    /// Ambient occlusion in `[0, 1]` (`1.0` = fully open, `0.0` = occluded contact/corner).
    pub ao: f64,
    /// Mean curvature in `[0, 1]` (`0.5` = flat, `> 0.5` = convex bevel/edge, `< 0.5` = concave crease).
    pub curvature: f64,
    /// Short-range cavity in `[0, 1]` (`1.0` = exposed, `0.0` = tight recessed seam/pocket).
    pub cavity: f64,
    /// Normalized local height (or optical thickness) in `[0, 1]`.
    pub height: f64,
}

impl SurfaceMasks {
    pub const NEUTRAL: SurfaceMasks = SurfaceMasks { ao: 1.0, curvature: 0.5, cavity: 1.0, height: 0.5 };

    /// Decode from normalized `[u8; 4]` `COLOR_0`.
    #[inline]
    pub fn from_rgba8(c: [u8; 4]) -> SurfaceMasks {
        SurfaceMasks {
            ao: f64::from(c[0]) / 255.0,
            curvature: f64::from(c[1]) / 255.0,
            cavity: f64::from(c[2]) / 255.0,
            height: f64::from(c[3]) / 255.0,
        }
    }
}

/// Configurable smart-material layering profile (analogous to a Substance Smart Material stack,
/// driven directly by exact SDF surface masks and world-space 3D noise).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SmartMaterial {
    /// Strength of convex-edge wear / chipped paint (`0.0..=1.0`).
    pub edge_wear: f64,
    /// Linear albedo revealed on worn convex edges (e.g. bright bare titanium or scuffed wood).
    pub edge_colour: [f64; 3],
    /// Metallic value on worn edges (e.g. `0.95` for bare steel under paint, `0.0` for wood).
    pub edge_metallic: f64,
    /// Roughness on worn/burnished edges (e.g. `0.26`).
    pub edge_roughness: f64,
    /// Strength of concave-crevice grime / dirt / oxide accumulation (`0.0..=1.0`).
    pub grime: f64,
    /// Linear albedo of crevice grime.
    pub grime_colour: [f64; 3],
    /// Roughness of crevice grime (typically matte `0.88..0.95`).
    pub grime_roughness: f64,
    /// Strength of upward-facing dust / moss / weathering (`0.0..=1.0`).
    pub dust: f64,
    /// Linear albedo of settled dust / moss.
    pub dust_colour: [f64; 3],
    /// World-space low-frequency fBm breakup of albedo & roughness (`0.0..=1.0`), eliminating
    /// uniform CGI surfaces across large walls and hulls.
    pub macro_variation: f64,
    /// Spatial scale of the macro variation noise in metres (e.g. `1.5`).
    pub macro_scale_m: f64,
    /// Strength of baked SDF AO darkening applied to `surface.ao` (`0.0..=1.0`).
    pub ao_strength: f64,
}

impl SmartMaterial {
    pub const NONE: SmartMaterial = SmartMaterial {
        edge_wear: 0.0,
        edge_colour: [0.7, 0.72, 0.75],
        edge_metallic: 0.9,
        edge_roughness: 0.28,
        grime: 0.0,
        grime_colour: [0.06, 0.05, 0.04],
        grime_roughness: 0.92,
        dust: 0.0,
        dust_colour: [0.35, 0.33, 0.29],
        macro_variation: 0.0,
        macro_scale_m: 1.2,
        ao_strength: 1.0,
    };

    /// Painted aerospace/naval steel: chipped paint exposing bright bare metal along convex bevels,
    /// dark oil/soot pooling in concave panel seams, and subtle hull plate weathering.
    pub const PAINTED_METAL: SmartMaterial = SmartMaterial {
        edge_wear: 0.72,
        edge_colour: [0.72, 0.74, 0.78],
        edge_metallic: 0.96,
        edge_roughness: 0.24,
        grime: 0.65,
        grime_colour: [0.04, 0.04, 0.045],
        grime_roughness: 0.92,
        dust: 0.16,
        dust_colour: [0.28, 0.27, 0.25],
        macro_variation: 0.32,
        macro_scale_m: 1.4,
        ao_strength: 0.95,
    };

    /// Weathered architectural timber: lighter worn edges, deep dark stain in joints/crevices,
    /// slight moss/dust on upward-facing sills, and plank-scale roughness breakup.
    pub const WEATHERED_TIMBER: SmartMaterial = SmartMaterial {
        edge_wear: 0.45,
        edge_colour: [0.48, 0.34, 0.20],
        edge_metallic: 0.0,
        edge_roughness: 0.56,
        grime: 0.55,
        grime_colour: [0.05, 0.035, 0.02],
        grime_roughness: 0.94,
        dust: 0.14,
        dust_colour: [0.22, 0.24, 0.18],
        macro_variation: 0.35,
        macro_scale_m: 1.8,
        ao_strength: 0.95,
    };

    /// Natural stone / slate: pale chipped aris edges, dark damp mortar crevices, subtle moss/dust
    /// on upper ledges, and broad geological tonal variation.
    pub const WEATHERED_STONE: SmartMaterial = SmartMaterial {
        edge_wear: 0.40,
        edge_colour: [0.48, 0.47, 0.45],
        edge_metallic: 0.0,
        edge_roughness: 0.72,
        grime: 0.60,
        grime_colour: [0.06, 0.065, 0.055],
        grime_roughness: 0.95,
        dust: 0.22,
        dust_colour: [0.18, 0.23, 0.14],
        macro_variation: 0.42,
        macro_scale_m: 1.6,
        ao_strength: 0.95,
    };
}

/// Modulate `surface` using `masks`, world position `world_pos`, world normal `world_normal`,
/// world `up`, and a [`SmartMaterial`] stack (with optional per-instance `instance_wear` scalar
/// matching Flecs `cad.MaterialWear`).
pub fn apply_smart_material(
    mut surface: SurfacePbr,
    masks: SurfaceMasks,
    world_pos: Vec3,
    world_normal: Vec3,
    up: Vec3,
    smart: &SmartMaterial,
    instance_wear: f64,
) -> SurfacePbr {
    let scale = smart.macro_scale_m.max(0.05);
    let macro_n = fbm3(world_pos * (1.0 / scale), 3);
    let detail_n = fbm3(world_pos * (4.5 / scale) + Vec3::new(17.3, -9.1, 4.7), 3);

    // 1. Macro per-region albedo & roughness breakup (eliminates uniform CGI sheen).
    if smart.macro_variation > 0.0 {
        let mv = smart.macro_variation.clamp(0.0, 1.0);
        let tint = 1.0 + (macro_n - 0.5) * 0.36 * mv;
        surface.base_colour[0] = (surface.base_colour[0] * tint).clamp(0.0, 1.0);
        surface.base_colour[1] = (surface.base_colour[1] * tint).clamp(0.0, 1.0);
        surface.base_colour[2] = (surface.base_colour[2] * tint).clamp(0.0, 1.0);
        surface.roughness = (surface.roughness + (macro_n - 0.5) * 0.28 * mv).clamp(0.04, 1.0);
    }

    // 2. Apply baked SDF ambient occlusion.
    let ao_mix = 1.0 - smart.ao_strength.clamp(0.0, 1.0) * (1.0 - masks.ao.clamp(0.0, 1.0));
    surface.ao = (surface.ao * ao_mix).clamp(0.0, 1.0);

    // 3. Convex-edge wear (curvature > 0.54 modulated by high-frequency chipping noise).
    let wear_gain = (smart.edge_wear * instance_wear).clamp(0.0, 1.5);
    if wear_gain > 1e-4 {
        let convex = ((masks.curvature - 0.53) / 0.28).clamp(0.0, 1.0);
        let chip = ((convex * (0.55 + 0.90 * detail_n) - 0.15) * 1.45).clamp(0.0, 1.0);
        let w = (chip * wear_gain).clamp(0.0, 1.0);
        for c in 0..3 {
            surface.base_colour[c] += (smart.edge_colour[c] - surface.base_colour[c]) * w;
        }
        surface.metallic += (smart.edge_metallic - surface.metallic) * w;
        surface.roughness += (smart.edge_roughness - surface.roughness) * w;
        // Worn paint strips clearcoat too.
        surface.clearcoat *= 1.0 - 0.85 * w;
    }

    // 4. Concave-crevice & cavity grime / oil / rust pooling.
    let grime_gain = (smart.grime * instance_wear).clamp(0.0, 1.5);
    if grime_gain > 1e-4 {
        let concave = ((0.48 - masks.curvature) / 0.28).clamp(0.0, 1.0);
        let cav_inv = (1.0 - masks.cavity).clamp(0.0, 1.0);
        let ao_inv = (1.0 - masks.ao).clamp(0.0, 1.0);
        let pool = (concave * 0.55 + cav_inv * 0.45 + ao_inv * 0.35).clamp(0.0, 1.0);
        let g = (pool * (0.65 + 0.55 * detail_n) * grime_gain).clamp(0.0, 0.92);
        for c in 0..3 {
            surface.base_colour[c] += (smart.grime_colour[c] - surface.base_colour[c]) * g;
        }
        surface.metallic *= 1.0 - 0.9 * g;
        surface.roughness += (smart.grime_roughness - surface.roughness) * g;
        surface.clearcoat *= 1.0 - g;
    }

    // 5. Upward-facing dust / moss / weathering.
    let dust_gain = (smart.dust * instance_wear).clamp(0.0, 1.5);
    if dust_gain > 1e-4 {
        let up_dot = world_normal.dot(up).clamp(0.0, 1.0);
        let top = ((up_dot - 0.55) / 0.45).clamp(0.0, 1.0);
        let d = (top * top * (0.45 + 0.80 * macro_n) * dust_gain).clamp(0.0, 0.85);
        for c in 0..3 {
            surface.base_colour[c] += (smart.dust_colour[c] - surface.base_colour[c]) * d;
        }
        surface.metallic *= 1.0 - 0.85 * d;
        surface.roughness = (surface.roughness + (0.94 - surface.roughness) * d).clamp(0.04, 1.0);
    }

    surface
}
