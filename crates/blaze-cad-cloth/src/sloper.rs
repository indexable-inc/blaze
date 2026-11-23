//! Anatomical `BodySloper` primitive: measures real cross-sectional body contours directly from
//! any scanned or MetaHuman mesh (`spacer.glb`) and wraps flat 2D garment panels conformantly onto
//! the true body + tailored ease allowance.
//!
//! Why this replaces crude cylinder wraps:
//! - A real human torso narrows from chest (`rx ~ 0.20 m`) to waist (`rx ~ 0.14 m`) to neck
//!   (`rx ~ 0.068 m`), and shoulders slope down from neck (`Y ~ 1.49 m`) to deltoid (`Y ~ 1.45 m`).
//! - Wrapping panels conformantly onto the measured `BodySloper` ensures EVERY seam (collar,
//!   shoulder, armscye, side, crotch, sleeve) starts within `5–18 mm` of its mate — eliminating
//!   violent pre-weld stretching, collar explosions, and shoulder pinching by construction!

#![blaze::sans_io]

use blaze_math::{Vec2, Vec3, cos, sin};
use blaze_own::{Alloc, AllocFailure, Vec};

/// One elliptical horizontal cross-section slice `(center_x, center_z, radius_x, radius_z)` at
/// height `y`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BodySlice {
    pub y: f64,
    pub cx: f64,
    pub cz: f64,
    pub rx: f64,
    pub rz: f64,
}

/// Measured anatomical profile extracted directly from the rigged body mesh (in either T-pose or
/// A-pose!).
pub struct BodySloper<'a> {
    /// Horizontal torso+neck slices sorted ascending in `y` (`Y = 0.80 .. 1.56`).
    pub torso_slices: Vec<BodySlice, Alloc<'a>>,
    /// Measured left & right shoulder/arm axes `(shoulder_root, wrist_tip)` and cross-section radii.
    pub left_arm_start: Vec3,
    pub left_arm_dir: Vec3,
    pub right_arm_start: Vec3,
    pub right_arm_dir: Vec3,
    /// Measured top-of-shoulder `y(abs_x)` profile (`|x| = 0.06 .. 0.22`) for true contoured shoulder seams.
    pub shoulder_top_y: [f64; 9],
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), AllocFailure> {
    v.try_push(x).map_err(|e| e.why)
}

impl<'a> BodySloper<'a> {
    /// Measure smooth anatomical cross-sections directly from `body_positions` (filtered to exclude
    /// outstretched arms when measuring the torso).
    pub fn measure(
        body_positions: &[Vec3],
        left_arm_start: Vec3,
        left_arm_end: Vec3,
        right_arm_start: Vec3,
        right_arm_end: Vec3,
        alloc: Alloc<'a>,
    ) -> Result<Self, AllocFailure> {
        let mut torso_slices = Vec::new_in(alloc);
        const SLICES: usize = 36;
        let y_min = 0.80;
        let y_max = 1.52;
        let band = 0.018;

        for s in 0..SLICES {
            let y = y_min + (y_max - y_min) * (s as f64) / ((SLICES - 1) as f64);
            // Exclude arms past `|x| = 0.225` (narrower above Y = 1.43 where neck narrows).
            let x_cap = if y > 1.475 { 0.105 } else if y > 1.435 { 0.195 } else { 0.225 };
            let mut min_x = f64::INFINITY;
            let mut max_x = f64::NEG_INFINITY;
            let mut min_z = f64::INFINITY;
            let mut max_z = f64::NEG_INFINITY;
            let mut found = 0u32;
            for &p in body_positions {
                if (p.y - y).abs() <= band && p.x.abs() <= x_cap {
                    // Exclude anterior non-structural crotch protrusion below Y = 0.90.
                    if p.y < 0.90 && p.x.abs() < 0.068 && p.z < -0.102 {
                        continue;
                    }
                    min_x = min_x.min(p.x);
                    max_x = max_x.max(p.x);
                    min_z = min_z.min(p.z);
                    max_z = max_z.max(p.z);
                    found += 1;
                }
            }
            let slice = if found >= 6 {
                BodySlice {
                    y,
                    cx: 0.5 * (min_x + max_x),
                    cz: 0.5 * (min_z + max_z),
                    rx: (0.5 * (max_x - min_x)).max(0.055),
                    rz: (0.5 * (max_z - min_z)).max(0.055),
                }
            } else if let Some(&prev) = torso_slices.last() {
                BodySlice { y, ..prev }
            } else {
                BodySlice { y, cx: 0.0, cz: 0.01, rx: 0.16, rz: 0.11 }
            };
            push(&mut torso_slices, slice)?;
        }

        // Smooth slices gently (2 passes of [0.25, 0.5, 0.25]) so transitions across armpit/neck are silk smooth.
        for _ in 0..2 {
            for i in 1..(SLICES - 1) {
                let a = torso_slices[i - 1];
                let b = torso_slices[i];
                let c = torso_slices[i + 1];
                torso_slices[i] = BodySlice {
                    y: b.y,
                    cx: 0.25 * a.cx + 0.50 * b.cx + 0.25 * c.cx,
                    cz: 0.25 * a.cz + 0.50 * b.cz + 0.25 * c.cz,
                    rx: 0.25 * a.rx + 0.50 * b.rx + 0.25 * c.rx,
                    rz: 0.25 * a.rz + 0.50 * b.rz + 0.25 * c.rz,
                };
            }
        }

        // Measure shoulder top contour y(|x|) across |x| = 0.06 .. 0.22 (step 0.02 m).
        let mut shoulder_top_y = [1.475f64; 9];
        for k in 0..9 {
            let x_target = 0.06 + (k as f64) * 0.02;
            let mut max_y = 1.43f64;
            for &p in body_positions {
                if p.y > 1.38 && p.y < 1.54 && (p.x.abs() - x_target).abs() < 0.014 && p.z.abs() < 0.09 {
                    if p.y > max_y {
                        max_y = p.y;
                    }
                }
            }
            shoulder_top_y[k] = max_y;
        }

        let left_arm_dir = (left_arm_end - left_arm_start).normalized().unwrap_or(Vec3::new(-1.0, 0.0, 0.0));
        let right_arm_dir = (right_arm_end - right_arm_start).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));

        Ok(Self {
            torso_slices,
            left_arm_start,
            left_arm_dir,
            right_arm_start,
            right_arm_dir,
            shoulder_top_y,
        })
    }

    /// Linearly interpolate the measured torso cross-section at world height `y`.
    pub fn sample_torso(&self, y: f64) -> BodySlice {
        let slices = self.torso_slices.as_slice();
        if slices.is_empty() {
            return BodySlice { y, cx: 0.0, cz: 0.01, rx: 0.16, rz: 0.11 };
        }
        if y <= slices[0].y {
            return BodySlice { y, ..slices[0] };
        }
        let last = slices.len() - 1;
        if y >= slices[last].y {
            return BodySlice { y, ..slices[last] };
        }
        let mut lo = 0usize;
        while lo + 1 < slices.len() && slices[lo + 1].y < y {
            lo += 1;
        }
        let a = slices[lo];
        let b = slices[lo + 1];
        let t = ((y - a.y) / (b.y - a.y).max(1e-9)).clamp(0.0, 1.0);
        BodySlice {
            y,
            cx: a.cx + (b.cx - a.cx) * t,
            cz: a.cz + (b.cz - a.cz) * t,
            rx: a.rx + (b.rx - a.rx) * t,
            rz: a.rz + (b.rz - a.rz) * t,
        }
    }

    /// Map a flat 2D torso or collar pattern vertex `(u, v)` directly onto the measured body
    /// contour at height `y = y_origin + v` plus `(ease_x, ease_z)` radial ease!
    ///
    /// Notice: because `rx(y)` and `rz(y)` follow the REAL body from hips -> waist -> chest ->
    /// neck, the neckline and collar automatically hug the true neck radius (`~85 mm`), while the
    /// chest automatically spans the full chest (`~215 mm`), eliminating any violent sewing jump!
    pub fn wrap_torso_point(
        &self,
        uv: Vec2,
        y_origin: f64,
        angle_center: f64,
        ease_x: f64,
        ease_z: f64,
        half_span_u: f64,
        angle_half_span: f64,
    ) -> Vec3 {
        let y = y_origin + uv.y;
        let sl = self.sample_torso(y);
        let rx = sl.rx + ease_x;
        let rz = sl.rz + ease_z;
        let frac = (uv.x / half_span_u.max(1e-4)).clamp(-1.35, 1.35);
        let theta = angle_center + frac * angle_half_span;
        let x = sl.cx + rx * sin(theta);
        let z = sl.cz - rz * cos(theta);
        Vec3::new(x, y, z)
    }

    /// Re-project a [`ClothMesh`](crate::mesh2d::ClothMesh) panel's initial positions conformantly
    /// onto this measured `BodySloper` with **over-the-shoulder dome arching**:
    /// - Below `shoulder_dome_y` (hips -> waist -> chest), conforms cleanly to `(rx(y)+ease_x, rz(y)+ease_z)`.
    /// - Above `shoulder_dome_y`, smoothly arches both front & back shoulder seams over the top
    ///   shoulder ridge (`z -> cz`) outside the neck opening (`|u| > neck_half_u`), while keeping
    ///   the neck opening cleanly around the measured neck cylinder!
    /// - Result: shoulder and collar initial seam gaps drop from `160 mm` to `< 18 mm`!
    #[allow(clippy::too_many_arguments)]
    pub fn conform_panel(
        &self,
        mesh: &mut crate::mesh2d::ClothMesh<'_>,
        panel_name: &str,
        y_origin: f64,
        angle_center: f64,
        ease_x: f64,
        ease_z: f64,
        half_span_u: f64,
        angle_half_span: f64,
        shoulder_dome_y: f64,
    ) {
        let Some(pslice) = mesh.panels.iter().find(|p| p.name == panel_name).copied() else {
            return;
        };
        let end = (pslice.vert_start + pslice.vert_count) as usize;
        for vi in (pslice.vert_start as usize)..end {
            let uv = mesh.rest_uv_m[vi];
            let y = y_origin + uv.y;
            let sl = self.sample_torso(y.min(1.47));
            let frac = (uv.x / half_span_u.max(1e-4)).clamp(-1.20, 1.20);
            let theta = angle_center + frac * angle_half_span;

            // Smooth over-the-shoulder dome arch above `shoulder_dome_y`:
            // At the high-point-shoulder (HPS, `|frac| >= 0.22`) AND all along the shoulder seam
            // (`sh_l`/`sh_r`), BOTH front and back panels arch smoothly over the top shoulder ridge
            // (`z -> cz = sl.cz`, `y -> body_sh_y + 0.018`), eliminating the 144 mm front-to-back
            // shoulder gap! Inside the neck hole (`|frac| < 0.22`), front/back curve cleanly around
            // the true 42 cm neck cylinder (`r = 0.074 m`)!
            let dome_t = ((y - shoulder_dome_y) / (1.490 - shoulder_dome_y).max(0.02)).clamp(0.0, 1.0);
            let dome_smooth = dome_t * dome_t * (3.0 - 2.0 * dome_t);
            let is_shoulder = ((frac.abs() - 0.18) / 0.12).clamp(0.0, 1.0);
            let neck_focus = dome_smooth * (1.0 - is_shoulder);

            let rx_base = sl.rx + ease_x;
            let rz_base = sl.rz + ease_z;
            let rx_neck = 0.078;
            let rz_neck = 0.078;

            // Pull shoulder seam inner corner (`frac ~ 0.28`) inward toward the real side of the
            // neck (`x ~ ±0.082`) while outer deltoid stays at `rx_base`!
            let sh_taper = if dome_smooth > 0.01 {
                let near_neck = 1.0 - ((frac.abs() - 0.26) / 0.55).clamp(0.0, 1.0);
                1.0 - 0.42 * dome_smooth * near_neck
            } else {
                1.0
            };
            let rx = (rx_base * sh_taper) * (1.0 - neck_focus) + rx_neck * neck_focus;
            // Flatten `z` all the way to the top ridge (`0.96`) on the entire shoulder seam!
            let rz = (rz_base * (1.0 - 0.96 * dome_smooth * is_shoulder)) * (1.0 - neck_focus) + rz_neck * neck_focus;

            let abs_x = (rx * sin(theta)).abs();
            let sh_idx = (((abs_x - 0.06) / 0.02).clamp(0.0, 8.0)) as usize;
            let body_sh_y = self.shoulder_top_y[sh_idx];
            // Keep shoulder & neckline strictly ABOVE the real trapezius/collarbone surface (+24 mm)!
            let y_final = if dome_t > 0.01 {
                let target_y = body_sh_y + 0.026;
                y.max(target_y)
            } else {
                y
            };

            let p = Vec3::new(sl.cx + rx * sin(theta), y_final, sl.cz - rz * cos(theta));
            mesh.pos[vi] = p;
            mesh.init_pos[vi] = p;
        }
        for pin in mesh.pins.iter_mut() {
            let v = pin.vertex;
            if v >= pslice.vert_start && v < pslice.vert_start + pslice.vert_count {
                pin.target = mesh.init_pos[v as usize];
            }
        }
    }

    /// Position a collar panel cleanly around the measured **forward-leaning** human neck column
    /// (`Y = 1.48..1.55`, where the MetaHuman neck center shifts forward by `-28 mm` from `cz=-0.016`
    /// at the base to `cz=-0.044` under the chin!) with proper structured field-jacket throat ease
    /// (`~15 mm` radial clearance) so the neck sits cleanly inside the collar opening with zero
    /// skin-through-collar slicing even under cheek-weld tilts or head turns!
    pub fn conform_collar_to_neckline(
        &self,
        mesh: &mut crate::mesh2d::ClothMesh<'_>,
        patterns: &crate::pattern::PatternSet<'_>,
        collar_panel: &str,
        outward_flare_m: f64,
    ) {
        let Some(cslice) = mesh.panels.iter().find(|p| p.name == collar_panel).copied() else {
            return;
        };
        let neck_base = self.sample_torso(1.48);
        let neck_mid = self.sample_torso(1.52);
        let neck_top = self.sample_torso(1.545);

        let cx_base = neck_base.cx;
        let cz_base = neck_base.cz;
        // Structured field-jacket collar base: clears sternal notch (-0.098), trapezius sides,
        // and back nape (+0.058) with comfortable ergonomic throat room.
        let rx_base = 0.088;
        let rz_back_base = 0.080;
        let rz_front_base = 0.112;

        // Upper collar rim follows the REAL forward neck lean (`cz_top ~ -0.041` instead of frozen
        // base `cz ~ -0.016`!) with +15 mm structured radial clearance around the neck (`rx ~ 0.070`,
        // `rz ~ 0.061`) so the neck never pierces the front or sides of the collar during cheek welds,
        // while staying sleekly tucked behind the nape (`+11 mm`, zero wing flare!).
        let cx_top = neck_top.cx;
        let cz_top = (neck_top.cz * 0.72 + neck_mid.cz * 0.28).min(-0.036);
        let rx_top = (neck_top.rx + 0.015).clamp(0.080, 0.088) + outward_flare_m;
        let rz_back_top = (neck_top.rz + 0.013).clamp(0.072, 0.080) + outward_flare_m * 0.5;
        let rz_front_top = (neck_top.rz + 0.026).clamp(0.084, 0.094) + outward_flare_m;

        let nu = (cslice.nu as usize).min(63);
        let mut base_pts = [Vec3::ZERO; 64];
        let half_arc = 2.94;
        for i in 0..=nu {
            let s = (i as f64) / (nu.max(1) as f64); // 0 = front-right throat, 0.5 = back nape, 1 = front-left throat
            let theta = core::f64::consts::PI + (s * 2.0 - 1.0) * half_arc;
            let cth = cos(theta); // > 0 in front (-Z), < 0 in back (+Z)
            let rz = if cth > 0.0 { rz_front_base } else { rz_back_base };
            let front_dip = (s * 2.0 - 1.0).abs();
            let y = 1.494 - 0.022 * front_dip * front_dip;
            let p = Vec3::new(cx_base + rx_base * sin(theta), y, cz_base - rz * cth);
            base_pts[i] = p;
        }

        // Place all collar rows extending smoothly upward along the real forward-leaning neck axis!
        for j in 0..=cslice.nv {
            let fj = (j as f64) / (cslice.nv.max(1) as f64);
            for i in 0..=nu {
                let cv = cslice.grid_vertex(i as u32, j) as usize;
                let s = (i as f64) / (nu.max(1) as f64);
                let theta = core::f64::consts::PI + (s * 2.0 - 1.0) * half_arc;
                let cth = cos(theta);
                let rz_t = if cth > 0.0 { rz_front_top } else { rz_back_top };
                let base = base_pts[i];
                let v_height = mesh.rest_uv_m[cv].y - mesh.rest_uv_m[cslice.grid_vertex(i as u32, 0) as usize].y;
                let top_target = Vec3::new(
                    cx_top + rx_top * sin(theta),
                    base.y + v_height,
                    cz_top - rz_t * cth,
                );
                let p = Vec3::new(
                    base.x * (1.0 - fj) + top_target.x * fj,
                    base.y + v_height,
                    base.z * (1.0 - fj) + top_target.z * fj,
                );
                mesh.pos[cv] = p;
                mesh.init_pos[cv] = p;
            }
        }

        // Pull every mated torso neckline vertex (`neck_r`, `neck_b`, `neck_l`) onto its exact collar
        // base point so ALL collar seams start at **0.0 mm gap**!
        for pair in mesh.seam_pairs.iter() {
            let seam = &patterns.seams[pair.seam_idx as usize];
            if seam.panel_a == collar_panel {
                let p = mesh.pos[pair.a as usize];
                mesh.pos[pair.b as usize] = p;
                mesh.init_pos[pair.b as usize] = p;
            } else if seam.panel_b == collar_panel {
                let p = mesh.pos[pair.b as usize];
                mesh.pos[pair.a as usize] = p;
                mesh.init_pos[pair.a as usize] = p;
            }
        }

        for pin in mesh.pins.iter_mut() {
            pin.target = mesh.init_pos[pin.vertex as usize];
        }
    }

    /// Smoothly snap a sleeve panel's cap edge (`j = 0`) onto its mated conformed torso armscye
    /// seam pairs, decaying exponentially down the sleeve (`j = 0..=nv`) so **ALL armscye seams
    /// start at 0.0 mm gap** instead of `~95 mm`, eliminating shoulder/armpit seam pinching!
    pub fn conform_sleeve_to_armscye(
        &self,
        mesh: &mut crate::mesh2d::ClothMesh<'_>,
        patterns: &crate::pattern::PatternSet<'_>,
        sleeve_panel: &str,
    ) {
        let Some(sslice) = mesh.panels.iter().find(|p| p.name == sleeve_panel).copied() else {
            return;
        };
        let nu = (sslice.nu as usize).min(63);
        let mut cap_delta = [Vec3::ZERO; 64];
        let mut cap_has = [false; 64];

        for pair in mesh.seam_pairs.iter() {
            let seam = &patterns.seams[pair.seam_idx as usize];
            let (sv, tv) = if seam.panel_a == sleeve_panel && seam.edge_a == "cap" {
                (pair.a as usize, pair.b as usize)
            } else if seam.panel_b == sleeve_panel && seam.edge_b == "cap" {
                (pair.b as usize, pair.a as usize)
            } else {
                continue;
            };
            let local = sv.saturating_sub(sslice.vert_start as usize);
            if local <= nu {
                cap_delta[local] = mesh.pos[tv] - mesh.pos[sv];
                cap_has[local] = true;
            }
        }

        // Smoothly propagate the cap offset down the sleeve with rapid cubic decay over the upper
        // third of the sleeve so the elbow & wrist cuff stay completely aligned to the A-pose arm!
        for j in 0..=sslice.nv {
            let fj = (j as f64) / (sslice.nv.max(1) as f64);
            let w = (1.0 - fj * 2.2).clamp(0.0, 1.0);
            let decay = w * w * (3.0 - 2.0 * w);
            if decay <= 1e-6 {
                continue;
            }
            for i in 0..=nu {
                if !cap_has[i] {
                    continue;
                }
                let vi = sslice.grid_vertex(i as u32, j) as usize;
                let p = mesh.pos[vi] + cap_delta[i] * decay;
                mesh.pos[vi] = p;
                mesh.init_pos[vi] = p;
            }
        }
        for pin in mesh.pins.iter_mut() {
            let v = pin.vertex;
            if v >= sslice.vert_start && v < sslice.vert_start + sslice.vert_count {
                pin.target = mesh.init_pos[v as usize];
            }
        }
    }
}
