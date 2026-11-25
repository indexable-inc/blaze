//! Procedural HDRI environments and deterministic roughness-filtered IBL (diffuse irradiance +
//! specular GGX environment reflections + physical dielectric transmission).

#![blaze::sans_io]

use blaze_math::{Vec3, pow, sqrt};

use crate::brdf::{SurfacePbr, fresnel_schlick_roughness, multi_scatter_boost, specular_ao};
use crate::texture::fbm3;

/// Procedural HDRI environment map presets (Z-up world convention by default; caller supplies
/// world-up vector so both Y-up and Z-up engines work identically).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnvironmentPreset {
    /// Clean automotive/product studio with bright overhead softbox, warm key panel, cool rim
    /// strip, and dark reflective studio sweep floor.
    StudioSoftbox,
    /// Warm golden-hour outdoor sun (`~22°` elevation) with Rayleigh blue zenith, warm horizon
    /// scatter, and earthy ground bounce. Ideal for cabins, architecture, and outdoor ships.
    GoldenHour,
    /// Soft neutral overcast sky dome for unbiased CC0 PBR material inspection.
    OvercastSky,
    /// Sci-fi industrial hangar with bright ceiling strip lights, cool steel reflections, and
    /// subtle warm amber work lamps. Ideal for ships, mechs, and mechanical hardware.
    HangarBay,
}

/// Primary directional light extracted from an [`EnvironmentPreset`] for sharp/soft shadow rays.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DominantLight {
    /// Unit direction pointing toward the light.
    pub direction: Vec3,
    /// Linear HDR radiance colour of the light.
    pub colour: [f64; 3],
    /// Angular radius in radians (used for soft penumbra shadow sampling).
    pub angular_radius: f64,
}

/// Return the primary shadow-casting light matching `preset` for a world whose up-axis is `up`.
pub fn dominant_light(preset: EnvironmentPreset, up: Vec3) -> DominantLight {
    let (right, forward) = basis_from_up(up);
    let make_dir = |r: f64, f: f64, u: f64| (right * r + forward * f + up * u).normalized().unwrap_or(up);
    match preset {
        EnvironmentPreset::StudioSoftbox => DominantLight {
            direction: make_dir(-0.52, -0.56, 0.76),
            colour: [3.8, 3.82, 3.88],
            angular_radius: 0.11,
        },
        EnvironmentPreset::GoldenHour => DominantLight {
            direction: make_dir(-0.78, -0.52, 0.42),
            colour: [5.6, 3.8, 2.15],
            angular_radius: 0.042,
        },
        EnvironmentPreset::OvercastSky => DominantLight {
            direction: make_dir(-0.35, -0.40, 0.85),
            colour: [1.8, 1.85, 1.92],
            angular_radius: 0.35,
        },
        EnvironmentPreset::HangarBay => DominantLight {
            direction: make_dir(-0.42, -0.45, 0.82),
            colour: [4.2, 4.5, 4.9],
            angular_radius: 0.14,
        },
    }
}

/// Sample the procedural HDRI radiance (linear RGB, unbounded HDR) in unit direction `dir`.
pub fn sample_environment(preset: EnvironmentPreset, dir: Vec3, up: Vec3) -> [f64; 3] {
    let (right, forward) = basis_from_up(up);
    let u = dir.dot(up).clamp(-1.0, 1.0);
    let r = dir.dot(right);
    let f = dir.dot(forward);

    match preset {
        EnvironmentPreset::StudioSoftbox => {
            // High-contrast neutral commercial photography studio (velvety dark-slate cyclorama
            // backdrop + elevated pure-white strip softboxes for dramatic metal/glass contrast!).
            let horizon = pow(1.0 - u.abs(), 5.0);
            let mut col = if u >= 0.0 {
                let base = 0.042 + 0.045 * u + 0.068 * horizon;
                [base * 0.97, base * 0.99, base * 1.04]
            } else {
                let g = (-u).clamp(0.0, 1.0);
                let base = 0.058 * (1.0 - 0.55 * g);
                [base * 0.97, base * 0.99, base * 1.04]
            };
            // Primary overhead strip softbox (creates crisp specular roll-off along curved barrels!).
            if u > 0.54 && r.abs() < 0.65 && f.abs() < 0.34 {
                let edge = smoothstep(0.54, 0.64, u) * smoothstep(0.65, 0.50, r.abs()) * smoothstep(0.34, 0.20, f.abs());
                col[0] += 10.2 * edge;
                col[1] += 10.3 * edge;
                col[2] += 10.6 * edge;
            }
            // Crisp neutral-white elevated front-left key softbox (high enough to reflect in glass, never in horizon!).
            let key_dir = (right * -0.46 - forward * 0.58 + up * 0.68).normalized().unwrap_or(up);
            let k = smoothstep(0.87, 0.96, dir.dot(key_dir));
            col[0] += 8.2 * k;
            col[1] += 8.1 * k;
            col[2] += 8.0 * k;
            // Cool-white sharp front-right rim kicker strip (sculpts silhouette & optical glass reflections!).
            let rim_dir = (right * 0.72 - forward * 0.38 + up * 0.58).normalized().unwrap_or(right);
            let rm = smoothstep(0.86, 0.96, dir.dot(rim_dir));
            col[0] += 4.6 * rm;
            col[1] += 5.1 * rm;
            col[2] += 6.0 * rm;
            col
        }
        EnvironmentPreset::GoldenHour => {
            let sun_dir = (right * -0.78 - forward * 0.52 + up * 0.42).normalized().unwrap_or(up);
            let sun_cos = dir.dot(sun_dir).clamp(-1.0, 1.0);
            if u >= 0.0 {
                // Physical Rayleigh blue zenith -> warm peach/amber golden-hour horizon.
                let t = pow(u, 0.52);
                let zenith = [0.14, 0.34, 0.84];
                let mid = [0.46, 0.58, 0.86];
                let horizon = [1.45, 0.84, 0.44];
                let mut col = if t < 0.35 {
                    let k = t / 0.35;
                    [
                        horizon[0] + (mid[0] - horizon[0]) * k,
                        horizon[1] + (mid[1] - horizon[1]) * k,
                        horizon[2] + (mid[2] - horizon[2]) * k,
                    ]
                } else {
                    let k = (t - 0.35) / 0.65;
                    [
                        mid[0] + (zenith[0] - mid[0]) * k,
                        mid[1] + (zenith[1] - mid[1]) * k,
                        mid[2] + (zenith[2] - mid[2]) * k,
                    ]
                };

                // Soft high-altitude wispy golden-hour clouds projected onto a dome plane.
                if u > 0.04 {
                    let inv_h = 1.0 / (u + 0.14);
                    let cp = Vec3::new(r * inv_h * 0.55 + 1.3, f * inv_h * 0.55 - 0.7, 0.35);
                    let c1 = fbm3(cp, 4);
                    let c2 = fbm3(Vec3::new(cp.x * 2.1 + 3.7, cp.y * 2.1 - 1.9, 1.2), 3);
                    let cloud = smoothstep(0.47, 0.78, c1 * 0.72 + c2 * 0.28) * smoothstep(0.04, 0.22, u) * smoothstep(0.95, 0.45, u);
                    if cloud > 1e-4 {
                        let sun_rim = pow(sun_cos.max(0.0), 4.0);
                        let cloud_col = [
                            0.72 + 0.95 * sun_rim,
                            0.58 + 0.62 * sun_rim,
                            0.54 + 0.28 * sun_rim,
                        ];
                        for i in 0..3 {
                            col[i] = col[i] * (1.0 - cloud * 0.62) + cloud_col[i] * (cloud * 0.62);
                        }
                    }
                }

                // Mie forward scattering halo around the sun + crisp HDR solar disc.
                let halo = pow(sun_cos.max(0.0), 7.0) * 1.35;
                let disc = smoothstep(0.994, 0.9992, sun_cos) * 14.0;
                col[0] += (halo + disc) * 1.28;
                col[1] += (halo + disc) * 0.86;
                col[2] += (halo + disc) * 0.44;

                // Distant atmospheric alpine mountain range & forested foothills near horizon (`u in [0, 0.18]`).
                if u < 0.19 {
                    let az_p = Vec3::new(r * 2.8, f * 2.8, 0.6);
                    let peak_detail = fbm3(Vec3::new(r * 9.5 + 1.1, f * 9.5 - 2.7, 2.3), 3);
                    let ridge_far = 0.038 + 0.122 * pow(fbm3(az_p, 4), 1.32) + 0.016 * peak_detail;
                    let tree_jagged = fbm3(Vec3::new(r * 26.0 + 3.1, f * 26.0 - 1.4, 3.2), 3);
                    let ridge_near = 0.016 + 0.056 * pow(fbm3(Vec3::new(r * 5.2 + 4.1, f * 5.2 - 2.3, 1.8), 4), 1.35) + 0.012 * tree_jagged;
                    let haze_col = [0.78 + 0.36 * sun_cos.max(0.0), 0.56 + 0.20 * sun_cos.max(0.0), 0.52];
                    if u < ridge_far {
                        let edge = smoothstep(ridge_far, ridge_far - 0.005, u);
                        // Sun-kissed snowfields along upper alpine ridges!
                        let snow = smoothstep(ridge_far - 0.024, ridge_far - 0.004, u) * smoothstep(0.38, 0.70, peak_detail);
                        let sun_face = 0.35 + 0.65 * sun_cos.max(0.0);
                        let m_far = [
                            0.14 + 0.34 * haze_col[0] + 0.58 * snow * sun_face,
                            0.17 + 0.30 * haze_col[1] + 0.44 * snow * sun_face,
                            0.27 + 0.29 * haze_col[2] + 0.36 * snow * sun_face,
                        ];
                        for i in 0..3 {
                            col[i] = col[i] + (m_far[i] - col[i]) * edge * 0.85;
                        }
                    }
                    if u < ridge_near {
                        let edge = smoothstep(ridge_near, ridge_near - 0.004, u);
                        // Deep evergreen alpine forest foothills with soft golden valley mist near base.
                        let mist = smoothstep(0.036, 0.0, u);
                        let canopy_var = 0.72 + 0.52 * tree_jagged;
                        let m_near = [
                            0.055 * canopy_var + 0.22 * haze_col[0] * (0.28 + 0.72 * mist),
                            0.088 * canopy_var + 0.19 * haze_col[1] * (0.28 + 0.72 * mist),
                            0.082 * canopy_var + 0.15 * haze_col[2] * (0.28 + 0.72 * mist),
                        ];
                        for i in 0..3 {
                            col[i] = col[i] + (m_near[i] - col[i]) * edge * 0.94;
                        }
                    }
                }
                col
            } else {
                // Warm sunlit earthy ground bounce.
                let g = (-u).clamp(0.0, 1.0);
                let base = [0.24 - 0.12 * g, 0.21 - 0.10 * g, 0.14 - 0.07 * g];
                let rim = pow(sun_cos.max(0.0), 3.0) * 0.18;
                [base[0] + rim * 1.2, base[1] + rim * 0.85, base[2] + rim * 0.5]
            }
        }
        EnvironmentPreset::OvercastSky => {
            if u >= 0.0 {
                let t = pow(u, 0.6);
                let h = [0.68, 0.72, 0.78];
                let z = [0.95, 1.00, 1.08];
                [h[0] + (z[0] - h[0]) * t, h[1] + (z[1] - h[1]) * t, h[2] + (z[2] - h[2]) * t]
            } else {
                let g = (-u).clamp(0.0, 1.0);
                [0.24 - 0.10 * g, 0.24 - 0.10 * g, 0.25 - 0.10 * g]
            }
        }
        EnvironmentPreset::HangarBay => {
            // Architectural aerospace hangar dome: structural steel wall ribs, high clerestory
            // daylight strip, twin overhead industrial LED light banks, and warm bay work lights.
            let mut col = if u >= 0.0 {
                let rib = 0.72 + 0.28 * blaze_math::cos(blaze_math::atan2(f, r) * 16.0).abs();
                let band = 0.80 + 0.20 * smoothstep(0.0, 0.35, u);
                [0.055 * rib * band, 0.072 * rib * band, 0.102 * rib * band]
            } else {
                let g = (-u).clamp(0.0, 1.0);
                [0.065 * (1.0 - 0.5 * g), 0.075 * (1.0 - 0.5 * g), 0.092 * (1.0 - 0.45 * g)]
            };
            // Distant sunlit hangar-door horizon bay strip (reaches u=0 cleanly so floor meets wall seamlessly!).
            if u >= 0.0 && u < 0.24 {
                let door_mask = smoothstep(0.24, 0.14, u);
                let pillar = smoothstep(0.12, 0.24, blaze_math::cos(blaze_math::atan2(f, r) * 8.0).abs());
                let bay = door_mask * (0.30 + 0.70 * pillar);
                col[0] += 0.38 * bay;
                col[1] += 0.44 * bay;
                col[2] += 0.54 * bay;
            }
            // High architectural clerestory window band around upper hangar wall (`u in [0.28, 0.42]`).
            if u > 0.28 && u < 0.42 {
                let cw = smoothstep(0.28, 0.31, u) * smoothstep(0.42, 0.39, u);
                let mullion = smoothstep(0.18, 0.32, blaze_math::sin(blaze_math::atan2(f, r) * 12.0).abs());
                let w = cw * mullion;
                col[0] += 1.35 * w;
                col[1] += 1.55 * w;
                col[2] += 1.85 * w;
            }
            // Dual razor-sharp cool-white industrial overhead LED light banks (gleam on fuselage!).
            if u > 0.48 {
                let strip1 = smoothstep(0.20, 0.09, (r - 0.36).abs()) * smoothstep(0.72, 0.54, f.abs());
                let strip2 = smoothstep(0.20, 0.09, (r + 0.36).abs()) * smoothstep(0.72, 0.54, f.abs());
                let truss = 0.25 + 0.75 * smoothstep(0.12, 0.25, blaze_math::sin(f * 18.0).abs());
                let s = (strip1 + strip2) * smoothstep(0.48, 0.62, u) * truss;
                col[0] += 8.6 * s;
                col[1] += 9.4 * s;
                col[2] += 10.6 * s;
            }
            // Warm sodium hangar work-lamp accent on one flank (orange-teal aerospace color contrast!).
            let lamp_dir = (right * 0.85 - forward * 0.32 + up * 0.22).normalized().unwrap_or(right);
            let l = smoothstep(0.84, 0.96, dir.dot(lamp_dir));
            col[0] += 6.2 * l;
            col[1] += 3.0 * l;
            col[2] += 0.85 * l;
            col
        }
    }
}

/// Deterministic split-sum-style IBL evaluation (diffuse irradiance + roughness-filtered GGX
/// specular reflection + clearcoat reflection + dielectric transmission/refraction).
pub fn eval_ibl(
    preset: EnvironmentPreset,
    surface: &SurfacePbr,
    n: Vec3,
    t: Vec3,
    b: Vec3,
    v: Vec3,
    up: Vec3,
    intensity: f64,
) -> [f64; 3] {
    eval_ibl_with_irradiance(preset, surface, n, t, b, v, up, intensity, None)
}

/// Like [`eval_ibl`], except an optional pre-integrated diffuse irradiance (e.g. from ray-traced
/// 1-bounce hemisphere GI with sky visibility and emissive bounce) replaces the unoccluded
/// environment diffuse irradiance while keeping roughness-filtered GGX specular, clearcoat,
/// sheen, and dielectric transmission.
#[allow(clippy::too_many_arguments)]
pub fn eval_ibl_with_irradiance(
    preset: EnvironmentPreset,
    surface: &SurfacePbr,
    n: Vec3,
    t: Vec3,
    b: Vec3,
    v: Vec3,
    up: Vec3,
    intensity: f64,
    override_irradiance: Option<[f64; 3]>,
) -> [f64; 3] {
    let ndotv = n.dot(v).clamp(1e-4, 1.0);
    let rough = surface.roughness.clamp(0.04, 1.0);
    let f0 = surface.f0();
    let f = fresnel_schlick_roughness(ndotv, f0, rough);
    let ms = multi_scatter_boost(f0, rough, ndotv);
    let ao = surface.ao.clamp(0.0, 1.0);
    let spec_ao = specular_ao(ndotv, ao, rough);

    // 1. Diffuse irradiance: either caller-supplied GI (already includes sky visibility!) or
    //    deterministic 5-direction environment quadrature modulated by `ao`.
    let (irr, diff_ao) = match override_irradiance {
        Some(gi) => (gi, 1.0),
        None => (sample_irradiance(preset, n, t, b, up), ao),
    };
    let kd_scale = (1.0 - surface.metallic.clamp(0.0, 1.0)) * (1.0 - surface.transmission.clamp(0.0, 1.0));
    let diff = [
        irr[0] * surface.base_colour[0] * (1.0 - f[0]) * kd_scale * diff_ao,
        irr[1] * surface.base_colour[1] * (1.0 - f[1]) * kd_scale * diff_ao,
        irr[2] * surface.base_colour[2] * (1.0 - f[2]) * kd_scale * diff_ao,
    ];

    // 2. Roughness-filtered GGX specular reflection along reflected vector `r`.
    let r_dir = (n * (2.0 * ndotv) - v).normalized().unwrap_or(n);
    let prefiltered = sample_prefiltered_env(preset, r_dir, t, b, rough, surface.anisotropy, up);
    // Analytic split-sum BRDF LUT approximation (Karis 2013 / Lazarov).
    let (brdf_scale, brdf_bias) = env_brdf_approx(rough, ndotv);
    let spec = [
        prefiltered[0] * (f[0] * brdf_scale + brdf_bias) * ms[0] * spec_ao,
        prefiltered[1] * (f[1] * brdf_scale + brdf_bias) * ms[1] * spec_ao,
        prefiltered[2] * (f[2] * brdf_scale + brdf_bias) * ms[2] * spec_ao,
    ];

    // 3. Physical dielectric transmission (Snell refraction into environment for glass/canopies).
    let mut trans = [0.0; 3];
    let tr = surface.transmission.clamp(0.0, 1.0);
    if tr > 1e-4 {
        let eta = 1.0 / surface.ior.max(1.0);
        let k = (1.0 - eta * eta * (1.0 - ndotv * ndotv)).max(0.0);
        let refract_dir = ((v * -eta) + n * (eta * ndotv - sqrt(k))).normalized().unwrap_or(v * -1.0);
        let env_t = sample_prefiltered_env(preset, refract_dir, t, b, rough * 0.5, 0.0, up);
        trans = [
            env_t[0] * surface.base_colour[0] * (1.0 - f[0]) * tr,
            env_t[1] * surface.base_colour[1] * (1.0 - f[1]) * tr,
            env_t[2] * surface.base_colour[2] * (1.0 - f[2]) * tr,
        ];
    }

    // 4. Optional clearcoat reflection lobe.
    let cc = surface.clearcoat.clamp(0.0, 1.0);
    let (base_atten, cc_col) = if cc > 1e-4 {
        let cc_r = surface.clearcoat_roughness.clamp(0.04, 1.0);
        let fc = fresnel_schlick_roughness(ndotv, [0.04; 3], cc_r)[0] * cc;
        let cc_env = sample_prefiltered_env(preset, r_dir, t, b, cc_r, 0.0, up);
        let (cs, cb) = env_brdf_approx(cc_r, ndotv);
        let c_ao = specular_ao(ndotv, ao, cc_r);
        let w = (fc * cs + cb * cc) * c_ao;
        (1.0 - fc, [cc_env[0] * w, cc_env[1] * w, cc_env[2] * w])
    } else {
        (1.0, [0.0; 3])
    };

    // 5. Optional grazing sheen ambient lift.
    let sheen_max = surface.sheen_colour[0].max(surface.sheen_colour[1]).max(surface.sheen_colour[2]);
    let sheen = if sheen_max > 1e-4 {
        let grazing = pow(1.0 - ndotv, 3.0) * (0.25 + 0.75 * surface.sheen_roughness);
        [
            irr[0] * surface.sheen_colour[0] * grazing * ao,
            irr[1] * surface.sheen_colour[1] * grazing * ao,
            irr[2] * surface.sheen_colour[2] * grazing * ao,
        ]
    } else {
        [0.0; 3]
    };

    [
        ((diff[0] + spec[0] + trans[0] + sheen[0]) * base_atten + cc_col[0]) * intensity,
        ((diff[1] + spec[1] + trans[1] + sheen[1]) * base_atten + cc_col[1]) * intensity,
        ((diff[2] + spec[2] + trans[2] + sheen[2]) * base_atten + cc_col[2]) * intensity,
    ]
}

fn sample_irradiance(preset: EnvironmentPreset, n: Vec3, t: Vec3, b: Vec3, up: Vec3) -> [f64; 3] {
    let dirs = [
        (n, 0.36),
        ((n * 0.70710678 + t * 0.70710678).normalized().unwrap_or(n), 0.16),
        ((n * 0.70710678 - t * 0.70710678).normalized().unwrap_or(n), 0.16),
        ((n * 0.70710678 + b * 0.70710678).normalized().unwrap_or(n), 0.16),
        ((n * 0.70710678 - b * 0.70710678).normalized().unwrap_or(n), 0.16),
    ];
    let mut sum = [0.0; 3];
    for (d, w) in dirs {
        let s = sample_environment(preset, d, up);
        sum[0] += s[0] * w;
        sum[1] += s[1] * w;
        sum[2] += s[2] * w;
    }
    sum
}

/// Deterministic 5-tap GGX-cone prefiltered environment lookup (sharp at roughness=0, smoothly
/// widened into hemisphere blur as roughness -> 1, stretched along tangent when anisotropic).
fn sample_prefiltered_env(
    preset: EnvironmentPreset,
    r_dir: Vec3,
    t: Vec3,
    b: Vec3,
    roughness: f64,
    anisotropy: f64,
    up: Vec3,
) -> [f64; 3] {
    let cone = roughness * roughness * 0.85;
    if cone < 0.015 {
        return sample_environment(preset, r_dir, up);
    }
    let helper = if r_dir.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
    let u_ax = if anisotropy.abs() > 1e-3 {
        (t - r_dir * r_dir.dot(t)).normalized().unwrap_or_else(|| helper.cross(r_dir).normalized().unwrap_or(t))
    } else {
        helper.cross(r_dir).normalized().unwrap_or(t)
    };
    let v_ax = r_dir.cross(u_ax).normalized().unwrap_or(b);
    let sx = cone * (1.0 + anisotropy.clamp(-0.85, 0.85));
    let sy = cone * (1.0 - anisotropy.clamp(-0.85, 0.85));

    let taps = [
        (r_dir, 0.32),
        ((r_dir + u_ax * sx).normalized().unwrap_or(r_dir), 0.17),
        ((r_dir - u_ax * sx).normalized().unwrap_or(r_dir), 0.17),
        ((r_dir + v_ax * sy).normalized().unwrap_or(r_dir), 0.17),
        ((r_dir - v_ax * sy).normalized().unwrap_or(r_dir), 0.17),
    ];
    let mut sum = [0.0; 3];
    for (d, w) in taps {
        let s = sample_environment(preset, d, up);
        sum[0] += s[0] * w;
        sum[1] += s[1] * w;
        sum[2] += s[2] * w;
    }
    sum
}

/// Analytic approximation to the GGX split-sum BRDF integration LUT (Lazarov / Unreal Mobile).
#[inline]
fn env_brdf_approx(roughness: f64, ndotv: f64) -> (f64, f64) {
    let c0 = [-1.0, -0.0275, -0.572, 0.022];
    let c1 = [1.0, 0.0425, 1.04, -0.04];
    let r = [
        roughness * c0[0] + c1[0],
        roughness * c0[1] + c1[1],
        roughness * c0[2] + c1[2],
        roughness * c0[3] + c1[3],
    ];
    let a004 = (r[0] * r[0]).min(pow(2.0, -9.28 * ndotv)) * r[0] + r[1];
    let scale = (-1.04 * a004 + r[2]).clamp(0.0, 1.0);
    let bias = (1.04 * a004 + r[3]).clamp(0.0, 1.0);
    (scale, bias)
}

#[inline]
fn basis_from_up(up: Vec3) -> (Vec3, Vec3) {
    let up = up.normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
    let ref_v = if up.z.abs() > 0.9 { Vec3::new(0.0, 1.0, 0.0) } else { Vec3::new(0.0, 0.0, 1.0) };
    let right = ref_v.cross(up).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
    let forward = up.cross(right).normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0));
    (right, forward)
}

#[inline]
fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
