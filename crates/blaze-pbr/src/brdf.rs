//! Cook-Torrance GGX microfacet BRDF with height-correlated Smith masking-shadowing,
//! multi-scattering energy compensation (Turquin/Kulla-Conty), anisotropic GGX, clearcoat,
//! Charlie microfiber sheen, and physical dielectric IOR / Fresnel.

#![blaze::sans_io]

use blaze_math::{Vec3, cos, pow, sin, sqrt};

const PI: f64 = core::f64::consts::PI;
const INV_PI: f64 = core::f64::consts::FRAC_1_PI;

/// Full physical surface parameters at a shading point (linear RGB).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SurfacePbr {
    /// Linear base reflectance (diffuse albedo for dielectrics, F0 tint for metals).
    pub base_colour: [f64; 3],
    /// `0.0` = dielectric, `1.0` = conductor/metal.
    pub metallic: f64,
    /// Perceptual roughness in `[0.04, 1.0]`.
    pub roughness: f64,
    /// Ambient occlusion factor in `[0.0, 1.0]`.
    pub ao: f64,
    /// Linear HDR self-emission (`emissive_colour * emissive_strength`).
    pub emissive: [f64; 3],
    /// Clearcoat layer weight in `[0.0, 1.0]`.
    pub clearcoat: f64,
    /// Clearcoat perceptual roughness in `[0.04, 1.0]`.
    pub clearcoat_roughness: f64,
    /// Specular anisotropy strength in `[-1.0, 1.0]` (`0.0` = isotropic).
    pub anisotropy: f64,
    /// Microfiber sheen colour (linear RGB).
    pub sheen_colour: [f64; 3],
    /// Microfiber sheen roughness in `[0.04, 1.0]`.
    pub sheen_roughness: f64,
    /// Physical dielectric transmission in `[0.0, 1.0]` (glass/canopy/water).
    pub transmission: f64,
    /// Index of refraction (`>= 1.0`, default `1.5`).
    pub ior: f64,
}

impl SurfacePbr {
    pub const DEFAULT: SurfacePbr = SurfacePbr {
        base_colour: [0.5, 0.5, 0.5],
        metallic: 0.0,
        roughness: 0.5,
        ao: 1.0,
        emissive: [0.0; 3],
        clearcoat: 0.0,
        clearcoat_roughness: 0.1,
        anisotropy: 0.0,
        sheen_colour: [0.0; 3],
        sheen_roughness: 0.5,
        transmission: 0.0,
        ior: 1.5,
    };

    /// Base specular reflectance at normal incidence (`F0`), blending dielectric IOR with
    /// metallic tint.
    #[inline]
    pub fn f0(&self) -> [f64; 3] {
        let d0 = dielectric_f0(self.ior);
        let m = self.metallic.clamp(0.0, 1.0);
        [
            d0 + (self.base_colour[0] - d0) * m,
            d0 + (self.base_colour[1] - d0) * m,
            d0 + (self.base_colour[2] - d0) * m,
        ]
    }
}

/// Dielectric Fresnel reflectance at normal incidence from index of refraction (`ior >= 1.0`).
#[inline]
pub fn dielectric_f0(ior: f64) -> f64 {
    let ior = ior.max(1.0);
    let r = (ior - 1.0) / (ior + 1.0);
    r * r
}

/// Schlick Fresnel approximation for RGB `f0`.
#[inline]
pub fn fresnel_schlick(cos_theta: f64, f0: [f64; 3]) -> [f64; 3] {
    let x = (1.0 - cos_theta.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let x2 = x * x;
    let x5 = x2 * x2 * x;
    [
        f0[0] + (1.0 - f0[0]) * x5,
        f0[1] + (1.0 - f0[1]) * x5,
        f0[2] + (1.0 - f0[2]) * x5,
    ]
}

/// Roughness-attenuated Schlick Fresnel for IBL environment reflections (Sebastien Lagarde).
#[inline]
pub fn fresnel_schlick_roughness(cos_theta: f64, f0: [f64; 3], roughness: f64) -> [f64; 3] {
    let x = (1.0 - cos_theta.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let x2 = x * x;
    let x5 = x2 * x2 * x;
    let max_r = (1.0 - roughness).clamp(0.0, 1.0);
    [
        f0[0] + (max_r.max(f0[0]) - f0[0]) * x5,
        f0[1] + (max_r.max(f0[1]) - f0[1]) * x5,
        f0[2] + (max_r.max(f0[2]) - f0[2]) * x5,
    ]
}

/// Isotropic Trowbridge-Reitz GGX normal distribution function `D(h)`.
#[inline]
pub fn ggx_d(ndoth: f64, alpha: f64) -> f64 {
    let a2 = alpha * alpha;
    let denom = ndoth * ndoth * (a2 - 1.0) + 1.0;
    a2 / (PI * denom * denom).max(1e-12)
}

/// Anisotropic Trowbridge-Reitz GGX NDF (Burley 2012) for brushed/machined surfaces.
#[inline]
pub fn ggx_d_aniso(ndoth: f64, tdoth: f64, bdoth: f64, ax: f64, ay: f64) -> f64 {
    let tx = tdoth / ax.max(1e-4);
    let by = bdoth / ay.max(1e-4);
    let d = tx * tx + by * by + ndoth * ndoth;
    1.0 / (PI * ax * ay * d * d).max(1e-12)
}

/// Height-correlated Smith GGX visibility term `V = G2 / (4 * NdotV * NdotL)` (Heitz 2014).
#[inline]
pub fn smith_v_ggx(ndotv: f64, ndotl: f64, alpha: f64) -> f64 {
    let a2 = alpha * alpha;
    let ggx_v = ndotl * sqrt(ndotv * ndotv * (1.0 - a2) + a2);
    let ggx_l = ndotv * sqrt(ndotl * ndotl * (1.0 - a2) + a2);
    0.5 / (ggx_v + ggx_l).max(1e-8)
}

/// Charlie sheen NDF (Estevez & Kulla 2017, Sony Imageworks) for cloth/velvet/dust.
#[inline]
pub fn charlie_d(ndoth: f64, roughness: f64) -> f64 {
    let inv_r = 1.0 / roughness.clamp(0.04, 1.0);
    let cos2 = (ndoth * ndoth).clamp(0.0, 1.0);
    let sin2 = (1.0 - cos2).max(1e-6);
    (2.0 + inv_r) * pow(sin2, inv_r * 0.5) / (2.0 * PI)
}

/// Specular ambient occlusion from diffuse AO and viewing angle (Lagarde & de Rousiers 2014),
/// preventing glossy metals in crevices from glowing unnaturally.
#[inline]
pub fn specular_ao(ndotv: f64, ao: f64, roughness: f64) -> f64 {
    let exp = (1.0 - roughness).clamp(0.0, 1.0) * 1.5 + 0.5;
    (pow(ndotv + ao, exp) - 1.0 + ao).clamp(0.0, 1.0)
}

/// Evaluate multi-scattering GGX energy boost factor `1 + F0 * (1/E - 1)` (Turquin 2019),
/// preventing rough metals from losing energy compared to real-world measurements.
#[inline]
pub fn multi_scatter_boost(f0: [f64; 3], roughness: f64, ndotv: f64) -> [f64; 3] {
    // Fast analytic fit to directional albedo E(mu, r) of single-scattering GGX.
    let r = roughness.clamp(0.04, 1.0);
    let e = 1.0 - r * (0.28 + 0.36 * (1.0 - ndotv.clamp(0.0, 1.0)));
    let inv_e_minus_1 = (1.0 / e.max(0.2) - 1.0).max(0.0);
    [
        1.0 + f0[0] * inv_e_minus_1,
        1.0 + f0[1] * inv_e_minus_1,
        1.0 + f0[2] * inv_e_minus_1,
    ]
}

/// Evaluate direct lighting `f(v, l) * (n · l)` for a single light direction `l` (unit vector
/// pointing toward the light) and view direction `v` (unit vector pointing toward the camera).
pub fn eval_direct_brdf(surface: &SurfacePbr, n: Vec3, t: Vec3, b: Vec3, v: Vec3, l: Vec3) -> [f64; 3] {
    let ndotl = n.dot(l);
    let ndotv = n.dot(v).max(1e-4);
    if ndotl <= 0.0 {
        return [0.0; 3];
    }
    let Some(h) = (v + l).normalized() else {
        return [0.0; 3];
    };
    let ndoth = n.dot(h).clamp(0.0, 1.0);
    let vdoth = v.dot(h).clamp(0.0, 1.0);

    let rough = surface.roughness.clamp(0.04, 1.0);
    let alpha = rough * rough;
    let f0 = surface.f0();
    let f = fresnel_schlick(vdoth, f0);
    let ms = multi_scatter_boost(f0, rough, ndotv);

    // Primary specular GGX lobe (anisotropic when `|anisotropy| > 1e-3`).
    let d = if surface.anisotropy.abs() > 1e-3 {
        let aniso = surface.anisotropy.clamp(-0.95, 0.95);
        let ax = (alpha * (1.0 + aniso)).max(0.002);
        let ay = (alpha * (1.0 - aniso)).max(0.002);
        ggx_d_aniso(ndoth, t.dot(h), b.dot(h), ax, ay)
    } else {
        ggx_d(ndoth, alpha)
    };
    let vis = smith_v_ggx(ndotv, ndotl, alpha);
    let spec = [
        d * vis * f[0] * ms[0],
        d * vis * f[1] * ms[1],
        d * vis * f[2] * ms[2],
    ];

    // Energy-conserving Burley-inspired diffuse lobe (attenuated by metallic & transmission).
    let kd_scale = (1.0 - surface.metallic.clamp(0.0, 1.0)) * (1.0 - surface.transmission.clamp(0.0, 1.0));
    let fd90 = 0.5 + 2.0 * rough * vdoth * vdoth;
    let fl = 1.0 + (fd90 - 1.0) * pow(1.0 - ndotl, 5.0);
    let fv = 1.0 + (fd90 - 1.0) * pow(1.0 - ndotv, 5.0);
    let burley = fl * fv * INV_PI;
    let diff = [
        surface.base_colour[0] * (1.0 - f[0]) * kd_scale * burley,
        surface.base_colour[1] * (1.0 - f[1]) * kd_scale * burley,
        surface.base_colour[2] * (1.0 - f[2]) * kd_scale * burley,
    ];

    // Optional sheen lobe (cloth, velvet, dusty matte surfaces).
    let mut sheen = [0.0; 3];
    let sheen_max = surface.sheen_colour[0].max(surface.sheen_colour[1]).max(surface.sheen_colour[2]);
    if sheen_max > 1e-4 {
        let sd = charlie_d(ndoth, surface.sheen_roughness);
        let sv = 1.0 / (4.0 * (ndotl + ndotv - ndotl * ndotv)).max(1e-4);
        let s_term = sd * sv;
        sheen = [surface.sheen_colour[0] * s_term, surface.sheen_colour[1] * s_term, surface.sheen_colour[2] * s_term];
    }

    // Optional clearcoat GGX topcoat (attenuates base layer by `1 - Fc * clearcoat`).
    let cc = surface.clearcoat.clamp(0.0, 1.0);
    let (base_atten, cc_spec) = if cc > 1e-4 {
        let cc_r = surface.clearcoat_roughness.clamp(0.04, 1.0);
        let cc_a = cc_r * cc_r;
        let fc = fresnel_schlick(vdoth, [0.04; 3])[0] * cc;
        let cc_d = ggx_d(ndoth, cc_a);
        let cc_v = smith_v_ggx(ndotv, ndotl, cc_a);
        (1.0 - fc, cc_d * cc_v * fc)
    } else {
        (1.0, 0.0)
    };

    [
        ((diff[0] + spec[0] + sheen[0]) * base_atten + cc_spec) * ndotl,
        ((diff[1] + spec[1] + sheen[1]) * base_atten + cc_spec) * ndotl,
        ((diff[2] + spec[2] + sheen[2]) * base_atten + cc_spec) * ndotl,
    ]
}

/// Sample a GGX microfacet reflection direction using Eric Heitz's (2018) *Sampling the GGX
/// Distribution of Visible Normals* (VNDF), supporting both isotropic and anisotropic surfaces.
///
/// Given camera-pointing view vector `v`, orthonormal surface frame `(t, b, n)`, perceptual
/// roughness `roughness`, anisotropy `anisotropy ∈ [-0.95, 0.95]`, and quasi-random uniform
/// coordinates `(u1, u2) ∈ [0, 1)^2`, returns a unit reflected ray direction `l` in world space
/// (producing physically accurate rough contact-hardening reflections).
pub fn sample_ggx_vndf(
    v: Vec3,
    n: Vec3,
    t: Vec3,
    b: Vec3,
    roughness: f64,
    anisotropy: f64,
    u1: f64,
    u2: f64,
) -> Vec3 {
    let rough = roughness.clamp(0.03, 1.0);
    let alpha = rough * rough;
    let (ax, ay) = if anisotropy.abs() > 1e-3 {
        let an = anisotropy.clamp(-0.95, 0.95);
        ((alpha * (1.0 + an)).max(0.001), (alpha * (1.0 - an)).max(0.001))
    } else {
        (alpha.max(0.001), alpha.max(0.001))
    };

    // Transform view vector `v` into local tangent frame.
    let vx = v.dot(t);
    let vy = v.dot(b);
    let vz = v.dot(n).max(1e-4);

    // 1. Stretch view vector into hemisphere configuration.
    let v_h = Vec3::new(ax * vx, ay * vy, vz).normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));

    // 2. Orthonormal basis around `v_h`.
    let len_sq = v_h.x * v_h.x + v_h.y * v_h.y;
    let t1 = if len_sq > 1e-12 {
        let inv = 1.0 / sqrt(len_sq);
        Vec3::new(-v_h.y * inv, v_h.x * inv, 0.0)
    } else {
        Vec3::new(1.0, 0.0, 0.0)
    };
    let t2 = v_h.cross(t1);

    // 3. Parameterize projected area disk uniformly.
    let r = sqrt(u1.clamp(1e-6, 0.999999));
    let phi = 2.0 * PI * u2;
    let p1 = r * cos(phi);
    let mut p2 = r * sin(phi);
    let s = 0.5 * (1.0 + v_h.z);
    p2 = (1.0 - s) * sqrt((1.0 - p1 * p1).max(0.0)) + s * p2;

    // 4. Reproject onto stretched hemisphere & unstretch to microfacet normal `h`.
    let n_h = t1 * p1 + t2 * p2 + v_h * sqrt((1.0 - p1 * p1 - p2 * p2).max(0.0));
    let h_local = Vec3::new(ax * n_h.x, ay * n_h.y, n_h.z.max(1e-4))
        .normalized()
        .unwrap_or(Vec3::new(0.0, 0.0, 1.0));

    // 5. Transform `h` back to world space and reflect `-v` across `h`.
    let h_world = (t * h_local.x + b * h_local.y + n * h_local.z).normalized().unwrap_or(n);
    let vdoth = v.dot(h_world).max(1e-4);
    (h_world * (2.0 * vdoth) - v).normalized().unwrap_or(n)
}
