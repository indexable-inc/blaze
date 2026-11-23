//! Multi-Pose Corrective Draping (Pose-Space Deformation / PSD) for `blaze-cad-cloth`.
//!
//! Why PSD eliminates the classic LBS garment pinching seen in extreme gameplay poses:
//! - Standard Linear Blend Skinning (LBS) rigidly transforms cloth vertices by bone weights, so
//!   lifting an arm across the chest (`RifleAim`) or bending a knee deeply (`Crouch`) stretches and
//!   pinches the deltoid/armpit/knee fabric instead of letting it slide over the body.
//! - [`compute_psd_corrective_deltas`] poses the 57-joint MetaHuman skeleton into canonical
//!   gameplay poses (`RifleAim`, `Crouch`), runs fast physical cloth relaxation + body collision
//!   in the posed frame, and exact-inverse-skins the relaxed positions back to bind pose to produce
//!   standard glTF 2.0 morph target deltas (`targets: [{"POSITION": ...}]`) blended live in ASTRA's
//!   `skin.slang`!

#![blaze::sans_io]

use blaze_math::{Vec3, cos, sin, sqrt};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::collider::MeshCollider;
use crate::detail::GarmentMesh;
use crate::gltf_io::LoadedGlb;
use crate::skin::{PosedMannequin, SkinnedWeights};

/// Canonical gameplay articulation poses used to generate pose-space corrective morph targets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CanonicalPose {
    /// Two-handed Pulse Rifle hold: left arm lifted & crossed forward to support forend, right arm
    /// raised to trigger grip, elbows bent ~50°.
    RifleAim,
    /// Deep athletic crouch: thighs pitched forward ~48°, knees flexed ~58°.
    Crouch,
}

impl CanonicalPose {
    pub const fn gltf_name(self) -> &'static str {
        match self {
            Self::RifleAim => "rifle_aim",
            Self::Crouch => "crouch",
        }
    }
}

/// Summary telemetry for one computed PSD morph target.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PsdReport {
    pub pose: CanonicalPose,
    pub active_vertices: u32,
    pub max_delta_mm: f64,
    pub initial_penetrations: u32,
    pub remaining_penetrations: u32,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), AllocFailure> {
    v.try_push(x).map_err(|e| e.why)
}

type Mat4 = [f64; 16];

fn mat4_mul(a: Mat4, b: Mat4) -> Mat4 {
    let mut out = [0.0f64; 16];
    for col in 0..4 {
        for row in 0..4 {
            out[col * 4 + row] = a[0 * 4 + row] * b[col * 4 + 0]
                + a[1 * 4 + row] * b[col * 4 + 1]
                + a[2 * 4 + row] * b[col * 4 + 2]
                + a[3 * 4 + row] * b[col * 4 + 3];
        }
    }
    out
}

fn mat4_from_trs(t: [f64; 3], q: [f64; 4], s: [f64; 3]) -> Mat4 {
    let [x, y, z, w] = q;
    let xx = x * x; let yy = y * y; let zz = z * z;
    let xy = x * y; let xz = x * z; let yz = y * z;
    let wx = w * x; let wy = w * y; let wz = w * z;
    [
        (1.0 - 2.0 * (yy + zz)) * s[0], (2.0 * (xy + wz)) * s[0],       (2.0 * (xz - wy)) * s[0],       0.0,
        (2.0 * (xy - wz)) * s[1],       (1.0 - 2.0 * (xx + zz)) * s[1], (2.0 * (yz + wx)) * s[1],       0.0,
        (2.0 * (xz + wy)) * s[2],       (2.0 * (yz - wx)) * s[2],       (1.0 - 2.0 * (xx + yy)) * s[2], 0.0,
        t[0],                           t[1],                           t[2],                           1.0,
    ]
}

fn pivot_rot(pivot: Vec3, axis: Vec3, angle: f64) -> Mat4 {
    let ax = axis.normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0));
    let half = 0.5 * angle;
    let s = sin(half);
    let q = [ax.x * s, ax.y * s, ax.z * s, cos(half)];
    let r = mat4_from_trs([0.0; 3], q, [1.0; 3]);
    let t_pos = mat4_from_trs([pivot.x, pivot.y, pivot.z], [0.0, 0.0, 0.0, 1.0], [1.0; 3]);
    let t_neg = mat4_from_trs([-pivot.x, -pivot.y, -pivot.z], [0.0, 0.0, 0.0, 1.0], [1.0; 3]);
    mat4_mul(t_pos, mat4_mul(r, t_neg))
}

fn blended_mat4(jv: [u16; 4], wv: [f32; 4], skin: &[Mat4]) -> Mat4 {
    let mut m = [0.0f64; 16];
    for slot in 0..4 {
        let w = wv[slot] as f64;
        if w <= 0.0 {
            continue;
        }
        let ji = (jv[slot] as usize).min(skin.len().saturating_sub(1));
        for k in 0..16 {
            m[k] += skin[ji][k] * w;
        }
    }
    m
}

#[inline]
fn mul_point(m: Mat4, p: Vec3) -> Vec3 {
    Vec3::new(
        m[0] * p.x + m[4] * p.y + m[8] * p.z + m[12],
        m[1] * p.x + m[5] * p.y + m[9] * p.z + m[13],
        m[2] * p.x + m[6] * p.y + m[10] * p.z + m[14],
    )
}

fn invert_affine_point(m: Mat4, p: Vec3) -> Vec3 {
    let a00 = m[0]; let a01 = m[4]; let a02 = m[8];
    let a10 = m[1]; let a11 = m[5]; let a12 = m[9];
    let a20 = m[2]; let a21 = m[6]; let a22 = m[10];
    let det = a00 * (a11 * a22 - a12 * a21) - a01 * (a10 * a22 - a12 * a20) + a02 * (a10 * a21 - a11 * a20);
    if det.abs() <= 1e-12 {
        return p;
    }
    let inv = 1.0 / det;
    let rx = p.x - m[12];
    let ry = p.y - m[13];
    let rz = p.z - m[14];
    Vec3::new(
        ((a11 * a22 - a12 * a21) * rx + (a02 * a21 - a01 * a22) * ry + (a01 * a12 - a02 * a11) * rz) * inv,
        ((a12 * a20 - a10 * a22) * rx + (a00 * a22 - a02 * a20) * ry + (a02 * a10 - a00 * a12) * rz) * inv,
        ((a10 * a21 - a11 * a20) * rx + (a01 * a20 - a00 * a21) * ry + (a00 * a11 - a01 * a10) * rz) * inv,
    )
}

/// Pose `body_glb` into a [`CanonicalPose`] (`RifleAim` or `Crouch`) using exact world-space
/// pivot rotations (matching `spacer.glb` glTF Y-up, `-Z` front, `-X` left).
pub fn pose_body_canonical<'a>(
    body_glb: &LoadedGlb<'_>,
    pose: CanonicalPose,
    alloc: Alloc<'a>,
) -> Result<PosedMannequin<'a>, AllocFailure> {
    let n_joints = body_glb.joints.len();
    let mut world = Vec::new_in(alloc);
    let mut computed = Vec::new_in(alloc);
    for _ in 0..n_joints {
        push(&mut world, [0.0; 16])?;
        push(&mut computed, false)?;
    }

    let mut remaining = n_joints;
    let mut guard = 0usize;
    while remaining > 0 && guard <= n_joints {
        guard += 1;
        for i in 0..n_joints {
            if computed[i] {
                continue;
            }
            if let Some(p) = body_glb.joints[i].parent {
                if !computed[p as usize] {
                    continue;
                }
            }
            let local = mat4_from_trs(body_glb.joints[i].translation, body_glb.joints[i].rotation, body_glb.joints[i].scale);
            world[i] = match body_glb.joints[i].parent {
                Some(p) => mat4_mul(world[p as usize], local),
                None => local,
            };
            computed[i] = true;
            remaining -= 1;
        }
    }

    let is_descendant = |mut ji: usize, anc: usize| -> bool {
        let mut steps = 0usize;
        while steps <= n_joints {
            if ji == anc {
                return true;
            }
            match body_glb.joints[ji].parent {
                Some(p) => ji = p as usize,
                None => return false,
            }
            steps += 1;
        }
        false
    };
    let find_j = |target: &str| -> Option<usize> {
        body_glb.joints.iter().position(|j| core::str::from_utf8(&j.name).ok() == Some(target))
    };

    let apply_joint_rot = |world: &mut [Mat4], jname: &str, axis: Vec3, angle: f64| {
        if let Some(ji) = find_j(jname) {
            let piv = Vec3::new(world[ji][12], world[ji][13], world[ji][14]);
            let m = pivot_rot(piv, axis, angle);
            for i in 0..n_joints {
                if is_descendant(i, ji) {
                    world[i] = mat4_mul(m, world[i]);
                }
            }
        }
    };

    match pose {
        CanonicalPose::RifleAim => {
            // Subtle tactical torso blading (~10 deg right) + cheek-weld neck/head tilt (+9 deg right,
            // +4 deg chin tuck) matching ASTRA's real two-handed Pulse Rifle stance (`human.rs`)!
            apply_joint_rot(&mut world, "spine_03", Vec3::new(0.0, 1.0, 0.0), -0.17);
            apply_joint_rot(&mut world, "neck_01", Vec3::new(0.0, 0.0, 1.0), -0.09);
            apply_joint_rot(&mut world, "neck_01", Vec3::new(1.0, 0.0, 0.0), -0.04);
            apply_joint_rot(&mut world, "head", Vec3::new(0.0, 0.0, 1.0), -0.08);
            apply_joint_rot(&mut world, "head", Vec3::new(1.0, 0.0, 0.0), -0.035);
            // Natural clavicle protraction (forward toward -Z) & slight elevation matching ASTRA's
            // real two-handed Pulse Rifle stance (`solve_arm` in `astra-scene/src/human.rs`).
            apply_joint_rot(&mut world, "clavicle_l", Vec3::new(0.0, 1.0, 0.0), -0.26);
            apply_joint_rot(&mut world, "clavicle_l", Vec3::new(0.0, 0.0, 1.0), -0.08);
            apply_joint_rot(&mut world, "clavicle_r", Vec3::new(0.0, 1.0, 0.0), 0.22);
            apply_joint_rot(&mut world, "clavicle_r", Vec3::new(0.0, 0.0, 1.0), 0.07);
            // Left arm (-X): drop ~42 deg, swing forward/inward across chest to support forend,
            // bend elbow strongly inward with slight upward cup!
            apply_joint_rot(&mut world, "upperarm_l", Vec3::new(0.0, 0.0, 1.0), 0.74);
            apply_joint_rot(&mut world, "upperarm_l", Vec3::new(0.0, 1.0, 0.0), -0.96);
            apply_joint_rot(&mut world, "lowerarm_l", Vec3::new(0.0, 1.0, 0.0), -0.88);
            apply_joint_rot(&mut world, "lowerarm_l", Vec3::new(1.0, 0.0, 0.0), 0.18);
            // Right arm (+X): drop ~52 deg, swing forward to pistol grip, bend elbow ~57 deg!
            apply_joint_rot(&mut world, "upperarm_r", Vec3::new(0.0, 0.0, 1.0), -0.90);
            apply_joint_rot(&mut world, "upperarm_r", Vec3::new(0.0, 1.0, 0.0), 0.72);
            apply_joint_rot(&mut world, "lowerarm_r", Vec3::new(0.0, 1.0, 0.0), 1.00);
        }
        CanonicalPose::Crouch => {
            // Flex thighs forward toward -Z (+X rotation in glTF Y-up) and bend calves backward!
            apply_joint_rot(&mut world, "thigh_l", Vec3::new(1.0, 0.0, 0.0), 0.84);
            apply_joint_rot(&mut world, "calf_l", Vec3::new(1.0, 0.0, 0.0), -1.02);
            apply_joint_rot(&mut world, "thigh_r", Vec3::new(1.0, 0.0, 0.0), 0.84);
            apply_joint_rot(&mut world, "calf_r", Vec3::new(1.0, 0.0, 0.0), -1.02);
        }
    }

    let mut skin_mats = Vec::new_in(alloc);
    let mut joint_world_pos = Vec::new_in(alloc);
    for i in 0..n_joints {
        let mut ibm = [0.0f64; 16];
        for k in 0..16 {
            ibm[k] = body_glb.joints[i].inverse_bind[k] as f64;
        }
        push(&mut skin_mats, mat4_mul(world[i], ibm))?;
        push(&mut joint_world_pos, Vec3::new(world[i][12], world[i][13], world[i][14]))?;
    }

    let mut positions = Vec::new_in(alloc);
    for i in 0..body_glb.positions.len() {
        let m = blended_mat4(body_glb.joints_0[i], body_glb.weights_0[i], &skin_mats);
        push(&mut positions, mul_point(m, body_glb.positions[i]))?;
    }

    Ok(PosedMannequin { positions, skin_mats, joint_world_pos })
}

/// Multi-step crease-aware body collision escape: resolves standard surface penetrations on step 0,
/// and when trapped in a concave fold (armpit between upper arm & ribcage, bent knee between thigh
/// & calf, or groin crease where `prev_n · n < 0.25`), escapes along the outward bisector `(prev_n + n)`
/// instead of ping-ponging between opposing walls!
fn project_out_of_body(col: &MeshCollider<'_>, mut p: Vec3, target_clear: f64, max_total_push: f64) -> (Vec3, f64) {
    let mut q = col.query(p);
    if q.signed_dist >= target_clear {
        return (p, q.signed_dist);
    }
    let mut prev_n = q.normal;
    let mut total_pushed = 0.0;
    for step in 0..4u32 {
        let need = target_clear - q.signed_dist;
        if need <= 1e-5 || total_pushed >= max_total_push {
            break;
        }
        let rem = max_total_push - total_pushed;
        let dir = if step == 0 {
            q.normal
        } else if prev_n.dot(q.normal) < 0.25 {
            let bis = prev_n + q.normal;
            let bl = bis.length();
            if bl > 0.10 {
                bis * (1.0 / bl)
            } else {
                // Nearly opposite crease walls: escape radially outward & slightly forward/down.
                Vec3::new(p.x, -0.20, p.z - 0.12).normalized().unwrap_or(q.normal)
            }
        } else {
            q.normal
        };
        let step_d = (need * if step == 0 { 1.0 } else { 1.30 }).min(rem);
        p = p + dir * step_d;
        total_pushed += step_d;
        prev_n = q.normal;
        q = col.query(p);
    }
    (p, q.signed_dist)
}

/// Compute exact bind-pose corrective morph target position deltas (`Vec<Vec3>`) for `pose`
/// (`RifleAim` or `Crouch`) by skinning `bind_garment` into `pose`, relaxing soft-cloth vertices
/// against the posed body+head collider with rest-edge preservation & Laplacian smoothing, and
/// exact-inverse-skinning back into T-pose bind!
pub fn compute_psd_corrective_deltas<'a>(
    bind_garment: &GarmentMesh<'_>,
    weights: &SkinnedWeights<'_>,
    body_glb: &LoadedGlb<'_>,
    head_glb: &LoadedGlb<'_>,
    pose: CanonicalPose,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<(Vec<Vec3, Alloc<'a>>, PsdReport), AllocFailure> {
    let posed = pose_body_canonical(body_glb, pose, scratch)?;
    let n_verts = bind_garment.positions.len();

    // Build COMPLETE posed body + POSED head/neck collider (skinning `head_glb` with the exact
    // same `posed.skin_mats` so cheek-weld neck tilt & head turn are physically collided!).
    let mut col_pos = Vec::new_in(scratch);
    let mut col_tris = Vec::new_in(scratch);
    for &p in posed.positions.iter() {
        push(&mut col_pos, p)?;
    }
    for &t in body_glb.triangles.iter() {
        push(&mut col_tris, t)?;
    }
    let hbase = col_pos.len() as u32;
    let has_head_skin =
        head_glb.joints_0.len() == head_glb.positions.len() && head_glb.weights_0.len() == head_glb.positions.len();
    for (i, &p) in head_glb.positions.iter().enumerate() {
        let hp = if has_head_skin {
            let m = blended_mat4(head_glb.joints_0[i], head_glb.weights_0[i], &posed.skin_mats);
            mul_point(m, p)
        } else {
            p
        };
        push(&mut col_pos, hp)?;
    }
    for &[a, b, c] in head_glb.triangles.iter() {
        push(&mut col_tris, [hbase + a, hbase + b, hbase + c])?;
    }
    let posed_col = MeshCollider::build(&col_pos, &col_tris, true, scratch)?;

    // 1. Classify canonical welded roots (`weld_rep`):
    //    - `is_cloth_root`: belongs to soft-cloth shell (`tri_layer <= 2`) with `rigid_anchor.is_none()`.
    //    - `is_patch_root`: stitched surface patch (`tri_layer == 3`, e.g., yoke/epaulette/pockets)
    //      with `rigid_anchor.is_none()`; follows its host soft cloth smoothly after XPBD relaxation!
    let mut in_cloth_layer = Vec::new_in(scratch);
    let mut in_patch_layer = Vec::new_in(scratch);
    for _ in 0..n_verts {
        push(&mut in_cloth_layer, false)?;
        push(&mut in_patch_layer, false)?;
    }
    for (ti, &[a, b, c]) in bind_garment.triangles.iter().enumerate() {
        let ra = bind_garment.weld_of(a) as usize;
        let rb = bind_garment.weld_of(b) as usize;
        let rc = bind_garment.weld_of(c) as usize;
        if bind_garment.tri_layer[ti] <= 2 {
            in_cloth_layer[ra] = true;
            in_cloth_layer[rb] = true;
            in_cloth_layer[rc] = true;
        } else {
            in_patch_layer[ra] = true;
            in_patch_layer[rb] = true;
            in_patch_layer[rc] = true;
        }
    }

    // Skin garment into target pose via exact LBS on canonical welded roots (`weld_rep`)
    // so PSD morph deltas stay bit-for-bit identical across every UV-split sewn seam!
    let mut posed_pos = Vec::new_in(scratch);
    let mut prev_pos = Vec::new_in(scratch);
    let mut lbs_pos = Vec::new_in(scratch);
    let mut vert_mats = Vec::new_in(scratch);
    let mut is_cloth_root = Vec::new_in(scratch);
    let mut is_patch_root = Vec::new_in(scratch);
    for i in 0..n_verts {
        let r = bind_garment.weld_of(i as u32) as usize;
        let m = blended_mat4(weights.joints_0[r], weights.weights_0[r], &posed.skin_mats);
        let p = mul_point(m, bind_garment.positions[r]);
        let free = r == i && bind_garment.rigid_anchor[r].is_none();
        push(&mut posed_pos, p)?;
        push(&mut prev_pos, p)?;
        push(&mut lbs_pos, p)?;
        push(&mut vert_mats, m)?;
        push(&mut is_cloth_root, free && in_cloth_layer[r])?;
        push(&mut is_patch_root, free && !in_cloth_layer[r] && in_patch_layer[r])?;
    }

    // 2. Build unique 1-ring stretch edges AND 2-ring opposite-vertex bending edges across
    //    CANONICAL WELDED ROOTS (`tri_layer <= 2`) for seam-watertight sliding and smooth diagonal
    //    buckling folds!
    #[derive(Clone, Copy)]
    struct EdgeRest {
        a: u32,
        b: u32,
        rest_len: f64,
    }
    let mut tri_edges = Vec::new_in(scratch);
    for (ti, &[a, b, c]) in bind_garment.triangles.iter().enumerate() {
        if bind_garment.tri_layer[ti] > 2 {
            continue;
        }
        let ra = bind_garment.weld_of(a);
        let rb = bind_garment.weld_of(b);
        let rc = bind_garment.weld_of(c);
        for (u, v, opp) in [(ra, rb, rc), (rb, rc, ra), (rc, ra, rb)] {
            if u != v {
                let (lo, hi) = if u < v { (u, v) } else { (v, u) };
                push(&mut tri_edges, (lo, hi, opp))?;
            }
        }
    }
    tri_edges.sort_unstable();

    let mut edges = Vec::new_in(scratch);
    let mut bend_edges = Vec::new_in(scratch);
    let mut idx = 0usize;
    while idx < tri_edges.len() {
        let (lo, hi, opp0) = tri_edges[idx];
        let mut end = idx + 1;
        while end < tri_edges.len() && tri_edges[end].0 == lo && tri_edges[end].1 == hi {
            end += 1;
        }
        let rl = (bind_garment.positions[hi as usize] - bind_garment.positions[lo as usize]).length();
        if rl > 1e-5 {
            push(&mut edges, EdgeRest { a: lo, b: hi, rest_len: rl })?;
        }
        if end - idx == 2 {
            let opp1 = tri_edges[idx + 1].2;
            if opp0 != opp1 {
                let bl = (bind_garment.positions[opp1 as usize] - bind_garment.positions[opp0 as usize]).length();
                if bl > 1e-5 {
                    push(&mut bend_edges, EdgeRest { a: opp0, b: opp1, rest_len: bl })?;
                }
            }
        }
        idx = end;
    }

    // 3. Measure initial LBS body penetrations and compute smooth pose-activity mask (`motion_zone`)
    //    so subtle gravity & relaxation localize cleanly to the articulated region (upper body in
    //    `RifleAim`, legs/hips in `Crouch`) while distant static garment zones stay bit-exact zero!
    let min_clear = 0.0055;
    let push_target = min_clear + 0.0012;
    let tether_free = min_clear + 0.0040;
    let max_delta = 0.055;

    let mut initial_pen = 0u32;
    let mut motion_zone = Vec::new_in(scratch);
    let mut signed_dist = Vec::new_in(scratch);
    let mut sum_pos = Vec::new_in(scratch);
    let mut cnt = Vec::new_in(scratch);
    for i in 0..n_verts {
        let mut act = 0.0f64;
        let mut sd = 1.0f64;
        if is_cloth_root[i] {
            let q = posed_col.query(posed_pos[i]);
            sd = q.signed_dist;
            if sd < min_clear {
                initial_pen += 1;
                act = 1.0;
            }
            let moved = (lbs_pos[i] - bind_garment.positions[i]).length();
            if moved > 0.003 {
                act = act.max(((moved - 0.003) / 0.025).clamp(0.0, 1.0));
            }
        }
        push(&mut motion_zone, act)?;
        push(&mut signed_dist, sd)?;
        push(&mut sum_pos, Vec3::ZERO)?;
        push(&mut cnt, 0u32)?;
    }
    // Diffuse `motion_zone` 10 edge-hops so torso/armpit/hip transition cloth smoothly participates!
    for _ in 0..10u32 {
        for e in edges.iter() {
            let ia = e.a as usize;
            let ib = e.b as usize;
            let ma = motion_zone[ia];
            let mb = motion_zone[ib];
            if ma * 0.85 > mb {
                motion_zone[ib] = ma * 0.85;
            }
            if mb * 0.85 > ma {
                motion_zone[ia] = mb * 0.85;
            }
        }
    }

    // 4. True warm-started XPBD cloth relaxation (36 iterations) over canonical welded roots!
    //    - Subtle gravity (`-9.81` scaled gently) settles loose slack naturally into gravity folds.
    //    - Anisotropic stretch (`k = 0.65`) vs compression (`k = 0.20`) lets compressed armpit/elbow/
    //      knee fabric buckle into organic diagonal tension folds instead of pinching!
    //    - Opposite-vertex bending springs (`k = 0.10`) keep buckled folds rolling smoothly.
    //    - Zero tether pull-back inside `min_clear + 4 mm` so body-contact vertices stay cleanly out!
    //    - Selective smoothing ONLY on non-penetrating vertices (`signed_dist > min_clear + 2 mm`).
    //    - End-of-iteration multi-step body collision projection resolves sharp armpit/knee creases!
    let g_accel = Vec3::new(0.0, -9.81, 0.0);
    let dt = 0.0045;
    let g_step_base = g_accel * (0.14 * dt * dt);

    for iter in 0..36u32 {
        // 4a. Warm-started XPBD prediction + subtle gravity inside active pose zone.
        for i in 0..n_verts {
            if !is_cloth_root[i] {
                continue;
            }
            let mz = motion_zone[i];
            if mz <= 0.01 {
                continue;
            }
            let v = (posed_pos[i] - prev_pos[i]) * 0.65;
            prev_pos[i] = posed_pos[i];
            let free_weight = if signed_dist[i] < min_clear + 0.002 { 0.15 } else { 1.0 };
            posed_pos[i] = posed_pos[i] + v * (0.42 * free_weight) + g_step_base * (mz * free_weight);
        }

        // 4b. Anisotropic XPBD stretch / compression edge projection (`k_stretch = 0.65`, `k_comp = 0.20`).
        //     Body-contact vertices carry higher effective mass (`inv_m = 0.20`) so free neighbor
        //     cloth slides smoothly around armpits/knees instead of pulling contact verts back in!
        for e in edges.iter() {
            let ia = e.a as usize;
            let ib = e.b as usize;
            let diff = posed_pos[ib] - posed_pos[ia];
            let d2 = diff.dot(diff);
            if d2 <= 1e-12 {
                continue;
            }
            let d = sqrt(d2);
            let err = d - e.rest_len;
            let k = if err > 0.0 { 0.65 } else { 0.20 };
            let wa = if signed_dist[ia] < min_clear + 0.002 { 0.20 } else { 1.0 };
            let wb = if signed_dist[ib] < min_clear + 0.002 { 0.20 } else { 1.0 };
            let w_sum = wa + wb;
            let scale = (err * k) / (d * w_sum);
            posed_pos[ia] = posed_pos[ia] + diff * (scale * wa);
            posed_pos[ib] = posed_pos[ib] - diff * (scale * wb);
        }

        // 4c. Dihedral / 2-ring opposite-vertex bending resistance so compressed folds form smooth
        //     diagonal waves instead of sharp pinch creases.
        for b in bend_edges.iter() {
            let ia = b.a as usize;
            let ib = b.b as usize;
            let diff = posed_pos[ib] - posed_pos[ia];
            let d2 = diff.dot(diff);
            if d2 <= 1e-12 {
                continue;
            }
            let d = sqrt(d2);
            let err = d - b.rest_len;
            if err < 0.0 {
                let wa = if signed_dist[ia] < min_clear + 0.002 { 0.20 } else { 1.0 };
                let wb = if signed_dist[ib] < min_clear + 0.002 { 0.20 } else { 1.0 };
                let scale = (err * 0.10) / (d * (wa + wb));
                posed_pos[ia] = posed_pos[ia] + diff * (scale * wa);
                posed_pos[ib] = posed_pos[ib] - diff * (scale * wb);
            }
        }

        // 4d. Zero tether pull-back inside `min_clear + 4 mm`; smoothly ramp tether outside!
        let tether_max = if iter < 24 { 0.045 } else { 0.075 };
        for i in 0..n_verts {
            if !is_cloth_root[i] {
                continue;
            }
            let sd = signed_dist[i];
            if sd > tether_free {
                let ramp = ((sd - tether_free) / 0.018).clamp(0.0, 1.0);
                let t = tether_max * ramp;
                posed_pos[i] = posed_pos[i] * (1.0 - t) + lbs_pos[i] * t;
            }
        }

        // 4e. Gentle Laplacian delta smoothing ONLY on non-penetrating vertices (`sd > min_clear + 2 mm`)!
        for i in 0..n_verts {
            sum_pos[i] = Vec3::ZERO;
            cnt[i] = 0;
        }
        for e in edges.iter() {
            let ia = e.a as usize;
            let ib = e.b as usize;
            let da = posed_pos[ia] - lbs_pos[ia];
            let db = posed_pos[ib] - lbs_pos[ib];
            sum_pos[ia] = sum_pos[ia] + db;
            cnt[ia] += 1;
            sum_pos[ib] = sum_pos[ib] + da;
            cnt[ib] += 1;
        }
        let smooth_alpha = if iter < 28 { 0.18 } else { 0.12 };
        for i in 0..n_verts {
            if is_cloth_root[i] && cnt[i] > 0 && signed_dist[i] > min_clear + 0.002 {
                let d_cur = posed_pos[i] - lbs_pos[i];
                let d_avg = sum_pos[i] * (1.0 / (cnt[i] as f64));
                posed_pos[i] = lbs_pos[i] + d_cur * (1.0 - smooth_alpha) + d_avg * smooth_alpha;
            }
        }

        // 4f. End-of-iteration XPBD body collision projection with crease-bisector escape!
        for i in 0..n_verts {
            if !is_cloth_root[i] {
                continue;
            }
            let (p_out, sd) = project_out_of_body(&posed_col, posed_pos[i], push_target, 0.036);
            let dp = p_out - posed_pos[i];
            if dp.dot(dp) > 1e-12 {
                // Skin contact friction: damp residual velocity on contact!
                prev_pos[i] = prev_pos[i] * 0.30 + p_out * 0.70;
                posed_pos[i] = p_out;
            }
            signed_dist[i] = sd;
        }
    }

    // 5. Final 4-pass crease-aware collision polish + overstretch safety guard on non-contact vertices!
    for pass in 0..4u32 {
        for i in 0..n_verts {
            if !is_cloth_root[i] {
                continue;
            }
            let (p_out, sd) = project_out_of_body(&posed_col, posed_pos[i], push_target, 0.052);
            posed_pos[i] = p_out;
            signed_dist[i] = sd;
        }
        if pass < 2 {
            for e in edges.iter() {
                let ia = e.a as usize;
                let ib = e.b as usize;
                let diff = posed_pos[ib] - posed_pos[ia];
                let d2 = diff.dot(diff);
                if d2 <= 1e-12 {
                    continue;
                }
                let d = sqrt(d2);
                let err = d - e.rest_len;
                if err > 0.006 {
                    let wa = if signed_dist[ia] > min_clear + 0.002 { 1.0 } else { 0.0 };
                    let wb = if signed_dist[ib] > min_clear + 0.002 { 1.0 } else { 0.0 };
                    let w_sum = wa + wb;
                    if w_sum > 0.0 {
                        let scale = (err * 0.35) / (d * w_sum);
                        posed_pos[ia] = posed_pos[ia] + diff * (scale * wa);
                        posed_pos[ib] = posed_pos[ib] - diff * (scale * wb);
                    }
                }
            }
        }
    }

    // 6. Compute exact inverse-LBS bind-pose deltas on canonical cloth roots (clamped to `max_delta`).
    let mut rem_pen = 0u32;
    let mut root_deltas = Vec::new_in(scratch);
    let mut active_verts = 0u32;
    let mut max_delta_m = 0.0f64;

    for i in 0..n_verts {
        if !is_cloth_root[i] {
            push(&mut root_deltas, Vec3::ZERO)?;
            continue;
        }
        let p_bind_relaxed = invert_affine_point(vert_mats[i], posed_pos[i]);
        let mut d = p_bind_relaxed - bind_garment.positions[i];
        let dl = d.length();
        if dl > max_delta {
            d = d * (max_delta / dl);
        }
        let final_d = if dl > 0.00015 { d } else { Vec3::ZERO };
        let final_posed = mul_point(vert_mats[i], bind_garment.positions[i] + final_d);
        let q = posed_col.query(final_posed);
        if q.signed_dist < min_clear {
            rem_pen += 1;
        }
        if dl > 0.00015 {
            active_verts += 1;
            max_delta_m = max_delta_m.max(final_d.length());
        }
        push(&mut root_deltas, final_d)?;
    }

    // 7. Transfer smooth host-cloth PSD deltas onto stitched surface-patch vertices (`tri_layer == 3`,
    //    `rigid_anchor.is_none()`, e.g., `back_yoke`, `epaulette_l/r`, `sleeve_util_pkt_l`, `elbow_l/r`,
    //    `knee_patch`) via a deterministic sorted spatial grid so patches ride cleanly with the
    //    relaxed host fabric with zero floating corners or cloth poke-through!
    let inv_cell = 1.0 / 0.032;
    let cell_key = |p: Vec3| -> (i32, i32, i32) {
        (
            blaze_math::floor(p.x * inv_cell) as i32,
            blaze_math::floor(p.y * inv_cell) as i32,
            blaze_math::floor(p.z * inv_cell) as i32,
        )
    };
    let mut cloth_grid = Vec::new_in(scratch);
    for i in 0..n_verts {
        if is_cloth_root[i] {
            push(&mut cloth_grid, (cell_key(bind_garment.positions[i]), i as u32))?;
        }
    }
    cloth_grid.sort_unstable();

    for i in 0..n_verts {
        if !is_patch_root[i] {
            continue;
        }
        let bp = bind_garment.positions[i];
        let (cx, cy, cz) = cell_key(bp);
        let mut w_sum = 0.0f64;
        let mut d_acc = Vec3::ZERO;
        let max_r2 = 0.036 * 0.036;
        let sigma2 = 0.008 * 0.008;
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let key = (cx + dx, cy + dy, cz + dz);
                    let start = cloth_grid.partition_point(|&(k, _)| k < key);
                    let mut k_idx = start;
                    while k_idx < cloth_grid.len() && cloth_grid[k_idx].0 == key {
                        let ci = cloth_grid[k_idx].1 as usize;
                        let diff = bind_garment.positions[ci] - bp;
                        let d2 = diff.dot(diff);
                        if d2 <= max_r2 {
                            let w = 1.0 / (d2 + sigma2);
                            w_sum += w;
                            d_acc = d_acc + root_deltas[ci] * w;
                        }
                        k_idx += 1;
                    }
                }
            }
        }
        let mut d = if w_sum > 1e-12 { d_acc * (1.0 / w_sum) } else { Vec3::ZERO };
        let mut p_posed = mul_point(vert_mats[i], bp + d);
        let q = posed_col.query(p_posed);
        if q.signed_dist < min_clear {
            let push_d = (min_clear - q.signed_dist).min(0.036);
            p_posed = p_posed + q.normal * push_d;
            d = invert_affine_point(vert_mats[i], p_posed) - bp;
        }
        let dl = d.length();
        if dl > max_delta {
            d = d * (max_delta / dl);
        }
        if dl > 0.0005 {
            root_deltas[i] = d;
            active_verts += 1;
            max_delta_m = max_delta_m.max(d.length());
        }
    }

    // Sync deltas bit-for-bit from canonical welded roots to every UV-split seam twin!
    let mut deltas = Vec::new_in(alloc);
    for i in 0..n_verts {
        let r = bind_garment.weld_of(i as u32) as usize;
        push(&mut deltas, root_deltas[r])?;
    }

    Ok((
        deltas,
        PsdReport {
            pose,
            active_vertices: active_verts,
            max_delta_mm: max_delta_m * 1000.0,
            initial_penetrations: initial_pen,
            remaining_penetrations: rem_pen,
        },
    ))
}

/// Hard-fail multi-pose collar & neck clearance verification gate: skins BOTH `head_glb` (the
/// MetaHuman neck & head) AND `bind_garment + psd_deltas` into `pose` (e.g. `RifleAim` with
/// cheek-weld neck roll & chin tuck) and proves every upper-neckline/collar vertex (`y > 1.46`)
/// clears the posed neck skin by at least `min_clear_m` (metres).
pub fn verify_posed_collar_clearance(
    bind_garment: &GarmentMesh<'_>,
    weights: &SkinnedWeights<'_>,
    psd_deltas: &[Vec3],
    body_glb: &LoadedGlb<'_>,
    head_glb: &LoadedGlb<'_>,
    pose: CanonicalPose,
    min_clear_m: f64,
    scratch: Alloc<'_>,
) -> Result<f64, crate::skin::ClearanceFinding> {
    let posed = pose_body_canonical(body_glb, pose, scratch).map_err(|_| crate::skin::ClearanceFinding {
        worst_vertex: 0,
        position: Vec3::ZERO,
        min_clearance: blaze_units::Length::m(-1.0),
        violating_vertices: 1,
    })?;
    let mut head_pos = Vec::new_in(scratch);
    let has_head_skin =
        head_glb.joints_0.len() == head_glb.positions.len() && head_glb.weights_0.len() == head_glb.positions.len();
    for (i, &p) in head_glb.positions.iter().enumerate() {
        let hp = if has_head_skin {
            let m = blended_mat4(head_glb.joints_0[i], head_glb.weights_0[i], &posed.skin_mats);
            mul_point(m, p)
        } else {
            p
        };
        let _ = push(&mut head_pos, hp);
    }
    let head_col = MeshCollider::build(&head_pos, &head_glb.triangles, false, scratch).map_err(|_| {
        crate::skin::ClearanceFinding {
            worst_vertex: 0,
            position: Vec3::ZERO,
            min_clearance: blaze_units::Length::m(-1.0),
            violating_vertices: 1,
        }
    })?;

    let mut worst_d = f64::INFINITY;
    let mut worst_v = 0u32;
    let mut worst_p = Vec3::ZERO;
    let mut violating = 0u32;

    for (ti, &[a, b, c]) in bind_garment.triangles.iter().enumerate() {
        if bind_garment.tri_layer[ti] > 2 {
            continue;
        }
        for v in [a, b, c] {
            let vi = v as usize;
            let bp = bind_garment.positions[vi];
            if bp.y < 1.465 || bp.x.abs() > 0.14 {
                continue;
            }
            let d = if vi < psd_deltas.len() { psd_deltas[vi] } else { Vec3::ZERO };
            let m = blended_mat4(weights.joints_0[vi], weights.weights_0[vi], &posed.skin_mats);
            let p_posed = mul_point(m, bp + d);
            let q = head_col.query(p_posed);
            if q.signed_dist < min_clear_m {
                violating += 1;
            }
            if q.signed_dist < worst_d {
                worst_d = q.signed_dist;
                worst_v = v;
                worst_p = p_posed;
            }
        }
    }

    if violating > 0 {
        Err(crate::skin::ClearanceFinding {
            worst_vertex: worst_v,
            position: worst_p,
            min_clearance: blaze_units::Length::m(worst_d),
            violating_vertices: violating,
        })
    } else {
        Ok(if worst_d.is_finite() { worst_d } else { min_clear_m })
    }
}
