//! Multi-motion articulated Pose-Sweep Checker (`pose_sweep_check`).
//!
//! Runs a skinned garment through realistic gameplay motions (`Sitting`, `Reaching`, `Walking`,
//! `Crouching`, `DeepSquat`, `RifleAim`) frame-by-frame using exact world-space pivot rotations
//! matching `psd.rs` (`glTF Y-up, -Z front, -X left`) and flags the **exact first frame** where:
//! 1. **Body penetration** occurs (`SweepFailureKind::BodyPenetration`),
//! 2. **An inner layer or rim lining breaks out through an outer shell**
//!    (`SweepFailureKind::LayerBreakthrough`), or
//! 3. **A surface-mounted pocket or flap pulls away from its host pattern panel** beyond its
//!    bind-pose rest anchor distance (`SweepFailureKind::PocketPullAway`).

#![blaze::sans_io]

use blaze_math::{Vec3, cos, sin};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::Length;

use crate::collider::MeshCollider;
use crate::detail::{GarmentMesh, PanelKind};
use crate::gltf_io::LoadedGlb;
use crate::skin::{PosedMannequin, SkinnedWeights};

/// Standard default motion set swept by [`PoseSweepOpts`].
pub const DEFAULT_SWEEP_MOTIONS: &[SweepMotion] = &[
    SweepMotion::Sitting,
    SweepMotion::Reaching,
    SweepMotion::Walking,
    SweepMotion::Crouching,
];

/// Articulated motion trajectory swept during [`pose_sweep_check`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SweepMotion {
    Sitting,
    Reaching,
    Walking,
    Crouching,
    DeepSquat,
    RifleAim,
}

impl SweepMotion {
    /// Human-readable canonical identifier for this motion sweep.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Sitting => "sitting",
            Self::Reaching => "reaching",
            Self::Walking => "walking",
            Self::Crouching => "crouching",
            Self::DeepSquat => "deep_squat",
            Self::RifleAim => "rifle_aim",
        }
    }
}

/// One world-space pivot rotation applied to a named skeleton joint and all of its descendants.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct JointRot {
    pub joint: &'static str,
    pub axis: [f64; 3],
    pub angle_rad: f64,
}

/// Planted-bug control enum proving [`pose_sweep_check`] catches every defect class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PoseSweepPlant {
    None,
    IgnoreBodyPenetration,
    IgnoreLayerBreakthrough,
    IgnorePocketPullAway,
}

impl Plants for PoseSweepPlant {}

/// Options controlling [`pose_sweep_check`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct PoseSweepOpts<'a, 's, 'm> {
    pub motions: &'m [SweepMotion] = DEFAULT_SWEEP_MOTIONS,
    pub frames_per_motion: u32 = 8,
    pub min_body_clearance: Length = Length::ZERO,
    pub max_lining_breakthrough: Length = Length::nm(500_000),
    pub max_pocket_pull_away: Length = Length::nm(2_500_000),
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// Exact physical failure mode detected on a swept frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SweepFailureKind {
    BodyPenetration {
        depth: Length,
        vertex: u32,
        panel_id: u16,
    },
    LayerBreakthrough {
        inner_layer: u8,
        outer_layer: u8,
        breakthrough: Length,
        inner_tri: u32,
        outer_tri: u32,
        inner_panel: u16,
        outer_panel: u16,
    },
    PocketPullAway {
        gap: Length,
        rest_gap: Length,
        pocket_vertex: u32,
        pocket_panel: u16,
        host_panel: u16,
    },
}

/// First failing frame hit reported by [`pose_sweep_check`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PoseSweepHit {
    pub motion: SweepMotion,
    pub frame: u32,
    pub phase: f64,
    pub position: Vec3,
    pub kind: SweepFailureKind,
}

/// Per-motion sweep summary telemetry.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MotionSweepSummary {
    pub motion: SweepMotion,
    pub frames_checked: u32,
    pub first_failure: Option<PoseSweepHit>,
    pub worst_body_clearance_mm: f64,
    pub worst_breakthrough_mm: f64,
    pub worst_pocket_pull_away_mm: f64,
    pub passed: bool,
}

/// Aggregate report returned by [`pose_sweep_check`].
pub struct PoseSweepReport<'a> {
    pub summaries: Vec<MotionSweepSummary, Alloc<'a>>,
    pub first_failure: Option<PoseSweepHit>,
    pub passed: bool,
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
    let xx = x * x;
    let yy = y * y;
    let zz = z * z;
    let xy = x * y;
    let xz = x * z;
    let yz = y * z;
    let wx = w * x;
    let wy = w * y;
    let wz = w * z;
    [
        (1.0 - 2.0 * (yy + zz)) * s[0],
        (2.0 * (xy + wz)) * s[0],
        (2.0 * (xz - wy)) * s[0],
        0.0,
        (2.0 * (xy - wz)) * s[1],
        (1.0 - 2.0 * (xx + zz)) * s[1],
        (2.0 * (yz + wx)) * s[1],
        0.0,
        (2.0 * (xz + wy)) * s[2],
        (2.0 * (yz - wx)) * s[2],
        (1.0 - 2.0 * (xx + yy)) * s[2],
        0.0,
        t[0],
        t[1],
        t[2],
        1.0,
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

#[inline]
fn lbs_point(p: Vec3, jv: [u16; 4], wv: [f32; 4], skin: &[Mat4]) -> Vec3 {
    let mut acc = Vec3::ZERO;
    for slot in 0..4 {
        let w = wv[slot] as f64;
        if w <= 0.0 {
            continue;
        }
        let ji = (jv[slot] as usize).min(skin.len().saturating_sub(1));
        let m = skin[ji];
        let x = m[0] * p.x + m[4] * p.y + m[8] * p.z + m[12];
        let y = m[1] * p.x + m[5] * p.y + m[9] * p.z + m[13];
        let z = m[2] * p.x + m[6] * p.y + m[10] * p.z + m[14];
        acc = acc + Vec3::new(x, y, z) * w;
    }
    acc
}

/// Pose `body_glb` using an arbitrary ordered slice of world-space pivot [`JointRot`]s.
pub fn pose_body_with_rots<'a>(
    body_glb: &LoadedGlb<'_>,
    rots: &[JointRot],
    alloc: Alloc<'a>,
) -> Result<PosedMannequin<'a>, AllocFailure> {
    let n_joints = body_glb.joints.len();
    let mut world = Vec::new_in(alloc);
    let mut computed = Vec::new_in(alloc);
    for _ in 0..n_joints {
        push(&mut world, [0.0; 16])?;
        push(&mut computed, false)?;
    }

    // 1. Compute bind-pose world matrices parent-before-child (bounded loop, rule 18).
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
            let local = mat4_from_trs(
                body_glb.joints[i].translation,
                body_glb.joints[i].rotation,
                body_glb.joints[i].scale,
            );
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

    // 2. Apply each requested world-space pivot rotation to the target joint and its subtree.
    for rot in rots.iter() {
        if rot.angle_rad.abs() <= 1e-12 {
            continue;
        }
        let ji_opt = body_glb
            .joints
            .iter()
            .position(|j| core::str::from_utf8(&j.name).ok() == Some(rot.joint));
        if let Some(ji) = ji_opt {
            let piv = Vec3::new(world[ji][12], world[ji][13], world[ji][14]);
            let ax = Vec3::new(rot.axis[0], rot.axis[1], rot.axis[2]);
            let m = pivot_rot(piv, ax, rot.angle_rad);
            for i in 0..n_joints {
                if is_descendant(i, ji) {
                    world[i] = mat4_mul(m, world[i]);
                }
            }
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
    let has_skin =
        !skin_mats.is_empty() && body_glb.joints_0.len() == body_glb.positions.len() && body_glb.weights_0.len() == body_glb.positions.len();
    for i in 0..body_glb.positions.len() {
        let p = if has_skin {
            lbs_point(body_glb.positions[i], body_glb.joints_0[i], body_glb.weights_0[i], &skin_mats)
        } else {
            body_glb.positions[i]
        };
        push(&mut positions, p)?;
    }

    Ok(PosedMannequin {
        positions,
        skin_mats,
        joint_world_pos,
    })
}

/// Smoothly pose the 57-joint skeleton at `phase in [0.0, 1.0]` for any [`SweepMotion`] using
/// exact world-space pivot rotations matching `psd.rs` (`glTF Y-up, -Z front, -X left`).
pub fn pose_body_at<'a>(
    body_glb: &LoadedGlb<'_>,
    motion: SweepMotion,
    phase: f64,
    alloc: Alloc<'a>,
) -> Result<PosedMannequin<'a>, AllocFailure> {
    const DEG: f64 = core::f64::consts::PI / 180.0;
    let t = phase.clamp(0.0, 1.0);

    match motion {
        SweepMotion::Sitting => {
            let rots = [
                JointRot { joint: "spine_01", axis: [1.0, 0.0, 0.0], angle_rad: -6.0 * DEG * t },
                JointRot { joint: "thigh_l", axis: [1.0, 0.0, 0.0], angle_rad: 92.0 * DEG * t },
                JointRot { joint: "calf_l", axis: [1.0, 0.0, 0.0], angle_rad: -90.0 * DEG * t },
                JointRot { joint: "thigh_r", axis: [1.0, 0.0, 0.0], angle_rad: 92.0 * DEG * t },
                JointRot { joint: "calf_r", axis: [1.0, 0.0, 0.0], angle_rad: -90.0 * DEG * t },
                JointRot { joint: "upperarm_l", axis: [0.0, 0.0, 1.0], angle_rad: 50.0 * DEG * t },
                JointRot { joint: "upperarm_r", axis: [0.0, 0.0, 1.0], angle_rad: -50.0 * DEG * t },
            ];
            pose_body_with_rots(body_glb, &rots, alloc)
        }
        SweepMotion::Reaching => {
            let rots = [
                JointRot { joint: "spine_02", axis: [1.0, 0.0, 0.0], angle_rad: 8.0 * DEG * t },
                JointRot { joint: "clavicle_l", axis: [0.0, 1.0, 0.0], angle_rad: -18.0 * DEG * t },
                JointRot { joint: "clavicle_l", axis: [0.0, 0.0, 1.0], angle_rad: -16.0 * DEG * t },
                JointRot { joint: "clavicle_r", axis: [0.0, 1.0, 0.0], angle_rad: 18.0 * DEG * t },
                JointRot { joint: "clavicle_r", axis: [0.0, 0.0, 1.0], angle_rad: 16.0 * DEG * t },
                JointRot { joint: "upperarm_l", axis: [0.0, 1.0, 0.0], angle_rad: -45.0 * DEG * t },
                JointRot { joint: "upperarm_l", axis: [0.0, 0.0, 1.0], angle_rad: -90.0 * DEG * t },
                JointRot { joint: "lowerarm_l", axis: [0.0, 1.0, 0.0], angle_rad: -10.0 * DEG * t },
                JointRot { joint: "upperarm_r", axis: [0.0, 1.0, 0.0], angle_rad: 45.0 * DEG * t },
                JointRot { joint: "upperarm_r", axis: [0.0, 0.0, 1.0], angle_rad: 90.0 * DEG * t },
                JointRot { joint: "lowerarm_r", axis: [0.0, 1.0, 0.0], angle_rad: 10.0 * DEG * t },
            ];
            pose_body_with_rots(body_glb, &rots, alloc)
        }
        SweepMotion::Walking => {
            let s = sin(core::f64::consts::TAU * t);
            let s_pos = s.max(0.0);
            let s_neg = (-s).max(0.0);
            let rots = [
                JointRot { joint: "pelvis", axis: [0.0, 1.0, 0.0], angle_rad: 6.0 * DEG * s },
                JointRot { joint: "spine_02", axis: [0.0, 1.0, 0.0], angle_rad: -10.0 * DEG * s },
                JointRot { joint: "thigh_l", axis: [1.0, 0.0, 0.0], angle_rad: 34.0 * DEG * s },
                JointRot { joint: "calf_l", axis: [1.0, 0.0, 0.0], angle_rad: -(42.0 * s_neg + 10.0 * s_pos) * DEG },
                JointRot { joint: "thigh_r", axis: [1.0, 0.0, 0.0], angle_rad: -34.0 * DEG * s },
                JointRot { joint: "calf_r", axis: [1.0, 0.0, 0.0], angle_rad: -(42.0 * s_pos + 10.0 * s_neg) * DEG },
                JointRot { joint: "upperarm_l", axis: [0.0, 0.0, 1.0], angle_rad: 58.0 * DEG },
                JointRot { joint: "upperarm_l", axis: [1.0, 0.0, 0.0], angle_rad: -24.0 * DEG * s },
                JointRot { joint: "lowerarm_l", axis: [1.0, 0.0, 0.0], angle_rad: (16.0 * s_neg + 6.0) * DEG },
                JointRot { joint: "upperarm_r", axis: [0.0, 0.0, 1.0], angle_rad: -58.0 * DEG },
                JointRot { joint: "upperarm_r", axis: [1.0, 0.0, 0.0], angle_rad: 24.0 * DEG * s },
                JointRot { joint: "lowerarm_r", axis: [1.0, 0.0, 0.0], angle_rad: (16.0 * s_pos + 6.0) * DEG },
            ];
            pose_body_with_rots(body_glb, &rots, alloc)
        }
        SweepMotion::Crouching => {
            let rots = [
                JointRot { joint: "spine_01", axis: [1.0, 0.0, 0.0], angle_rad: -10.0 * DEG * t },
                JointRot { joint: "spine_02", axis: [1.0, 0.0, 0.0], angle_rad: -8.0 * DEG * t },
                JointRot { joint: "thigh_l", axis: [1.0, 0.0, 0.0], angle_rad: 68.0 * DEG * t },
                JointRot { joint: "calf_l", axis: [1.0, 0.0, 0.0], angle_rad: -82.0 * DEG * t },
                JointRot { joint: "thigh_r", axis: [1.0, 0.0, 0.0], angle_rad: 68.0 * DEG * t },
                JointRot { joint: "calf_r", axis: [1.0, 0.0, 0.0], angle_rad: -82.0 * DEG * t },
                JointRot { joint: "foot_l", axis: [1.0, 0.0, 0.0], angle_rad: 14.0 * DEG * t },
                JointRot { joint: "foot_r", axis: [1.0, 0.0, 0.0], angle_rad: 14.0 * DEG * t },
            ];
            pose_body_with_rots(body_glb, &rots, alloc)
        }
        SweepMotion::DeepSquat => {
            let rots = [
                JointRot { joint: "spine_01", axis: [1.0, 0.0, 0.0], angle_rad: -14.0 * DEG * t },
                JointRot { joint: "spine_02", axis: [1.0, 0.0, 0.0], angle_rad: -12.0 * DEG * t },
                JointRot { joint: "thigh_l", axis: [1.0, 0.0, 0.0], angle_rad: 108.0 * DEG * t },
                JointRot { joint: "calf_l", axis: [1.0, 0.0, 0.0], angle_rad: -125.0 * DEG * t },
                JointRot { joint: "thigh_r", axis: [1.0, 0.0, 0.0], angle_rad: 108.0 * DEG * t },
                JointRot { joint: "calf_r", axis: [1.0, 0.0, 0.0], angle_rad: -125.0 * DEG * t },
                JointRot { joint: "foot_l", axis: [1.0, 0.0, 0.0], angle_rad: 24.0 * DEG * t },
                JointRot { joint: "foot_r", axis: [1.0, 0.0, 0.0], angle_rad: 24.0 * DEG * t },
            ];
            pose_body_with_rots(body_glb, &rots, alloc)
        }
        SweepMotion::RifleAim => {
            // Exact match to `CanonicalPose::RifleAim` in `psd.rs`, smoothly interpolated by `t`.
            let rots = [
                JointRot { joint: "spine_03", axis: [0.0, 1.0, 0.0], angle_rad: -0.17 * t },
                JointRot { joint: "neck_01", axis: [0.0, 0.0, 1.0], angle_rad: -0.09 * t },
                JointRot { joint: "neck_01", axis: [1.0, 0.0, 0.0], angle_rad: -0.04 * t },
                JointRot { joint: "head", axis: [0.0, 0.0, 1.0], angle_rad: -0.08 * t },
                JointRot { joint: "head", axis: [1.0, 0.0, 0.0], angle_rad: -0.035 * t },
                JointRot { joint: "clavicle_l", axis: [0.0, 1.0, 0.0], angle_rad: -0.26 * t },
                JointRot { joint: "clavicle_l", axis: [0.0, 0.0, 1.0], angle_rad: -0.08 * t },
                JointRot { joint: "clavicle_r", axis: [0.0, 1.0, 0.0], angle_rad: 0.22 * t },
                JointRot { joint: "clavicle_r", axis: [0.0, 0.0, 1.0], angle_rad: 0.07 * t },
                JointRot { joint: "upperarm_l", axis: [0.0, 0.0, 1.0], angle_rad: 0.74 * t },
                JointRot { joint: "upperarm_l", axis: [0.0, 1.0, 0.0], angle_rad: -0.96 * t },
                JointRot { joint: "lowerarm_l", axis: [0.0, 1.0, 0.0], angle_rad: -0.88 * t },
                JointRot { joint: "lowerarm_l", axis: [1.0, 0.0, 0.0], angle_rad: 0.18 * t },
                JointRot { joint: "upperarm_r", axis: [0.0, 0.0, 1.0], angle_rad: -0.90 * t },
                JointRot { joint: "upperarm_r", axis: [0.0, 1.0, 0.0], angle_rad: 0.72 * t },
                JointRot { joint: "lowerarm_r", axis: [0.0, 1.0, 0.0], angle_rad: 1.00 * t },
            ];
            pose_body_with_rots(body_glb, &rots, alloc)
        }
    }
}

/// Sweep a garment through all requested articulated motions and flag the exact first failing
/// frame for body penetration, lining/layer breakthrough, or pocket pull-away.
pub fn pose_sweep_check<'a>(
    garment: &GarmentMesh<'_>,
    weights: &SkinnedWeights<'_>,
    body_glb: &LoadedGlb<'_>,
    morph_deltas: &[(&[Vec3], f64)],
    opts: PoseSweepOpts<'a, '_, '_>,
) -> Result<PoseSweepReport<'a>, AllocFailure> {
    pose_sweep_check_planted(garment, weights, body_glb, morph_deltas, opts, NoPlant)
}

/// Planted-bug seam entry point for [`pose_sweep_check`].
pub fn pose_sweep_check_planted<'a>(
    garment: &GarmentMesh<'_>,
    weights: &SkinnedWeights<'_>,
    body_glb: &LoadedGlb<'_>,
    morph_deltas: &[(&[Vec3], f64)],
    opts: PoseSweepOpts<'a, '_, '_>,
    plant: impl Planted<PoseSweepPlant>,
) -> Result<PoseSweepReport<'a>, AllocFailure> {
    let n_verts = garment.positions.len();
    let n_tris = garment.triangles.len();
    let scratch = opts.scratch;

    // 1. Precompute per-vertex representative panel_id & active flag.
    let mut vert_panel = Vec::new_in(scratch);
    let mut vert_active = Vec::new_in(scratch);
    for _ in 0..n_verts {
        push(&mut vert_panel, 0u16)?;
        push(&mut vert_active, false)?;
    }
    for (ti, &[a, b, c]) in garment.triangles.iter().enumerate() {
        let pid = garment.tri_panel.get(ti).copied().unwrap_or(0);
        for v in [a as usize, b as usize, c as usize] {
            if v < n_verts {
                if !vert_active[v] {
                    vert_panel[v] = pid;
                    vert_active[v] = true;
                }
            }
        }
    }

    // 2. Precompute rest pocket-to-host-surface anchor distance for every vertex belonging to a
    //    `PanelKind::SurfacePatch` panel against its host CAD pattern piece in bind pose!
    #[derive(Clone, Copy)]
    struct PocketAnchor {
        vertex: u32,
        pocket_panel: u16,
        host_panel: u16,
        rest_dist_m: f64,
    }
    let mut host_ids = Vec::new_in(scratch);
    let mut patch_host_pairs = Vec::new_in(scratch);
    for pm in garment.panels.iter() {
        if pm.kind != PanelKind::SurfacePatch {
            continue;
        }
        let Some(host_name) = pm.host_panel else {
            continue;
        };
        let host_id = garment
            .panels
            .iter()
            .find(|h| h.name == host_name && h.kind == PanelKind::PatternPiece)
            .or_else(|| garment.panels.iter().find(|h| h.name == host_name))
            .map(|h| h.id);
        let Some(hid) = host_id else {
            continue;
        };
        push(&mut patch_host_pairs, (pm.id, hid))?;
        if !host_ids.iter().any(|&x| x == hid) {
            push(&mut host_ids, hid)?;
        }
    }

    let mut pocket_anchors = Vec::new_in(scratch);
    if !patch_host_pairs.is_empty() && garment.tri_panel.len() == n_tris {
        let mut host_tris_bind = Vec::new_in(scratch);
        let mut vert_seen_patch = Vec::new_in(scratch);
        for _ in 0..n_verts {
            push(&mut vert_seen_patch, u16::MAX)?;
        }
        for &hid in host_ids.iter() {
            host_tris_bind.clear();
            for (ti, &tri) in garment.triangles.iter().enumerate() {
                if garment.tri_panel[ti] == hid {
                    push(&mut host_tris_bind, tri)?;
                }
            }
            if host_tris_bind.is_empty() {
                continue;
            }
            let bind_host_col = MeshCollider::build(&garment.positions, &host_tris_bind, false, scratch)?;
            for &(patch_pid, pair_hid) in patch_host_pairs.iter() {
                if pair_hid != hid {
                    continue;
                }
                for (ti, &[a, b, c]) in garment.triangles.iter().enumerate() {
                    if garment.tri_panel[ti] != patch_pid {
                        continue;
                    }
                    for v in [a, b, c] {
                        let vi = v as usize;
                        if vi < n_verts && vert_seen_patch[vi] != patch_pid {
                            vert_seen_patch[vi] = patch_pid;
                            let p = garment.positions[vi];
                            let q = bind_host_col.query(p);
                            let rest_dist_m = (p - q.closest).length();
                            push(
                                &mut pocket_anchors,
                                PocketAnchor {
                                    vertex: v,
                                    pocket_panel: patch_pid,
                                    host_panel: hid,
                                    rest_dist_m,
                                },
                            )?;
                        }
                    }
                }
            }
        }
    }

    // 3. Pre-classify triangles by panel kind so per-frame breakthrough checks are fast & exact.
    let mut tri_kind = Vec::new_in(scratch);
    for ti in 0..n_tris {
        let pid = garment.tri_panel.get(ti).copied().unwrap_or(0);
        let kind = garment
            .panels
            .get(pid as usize)
            .map(|p| p.kind)
            .unwrap_or(PanelKind::PatternPiece);
        push(&mut tri_kind, kind)?;
    }

    let check_body = !plant.has(PoseSweepPlant::IgnoreBodyPenetration);
    let check_breakthrough = !plant.has(PoseSweepPlant::IgnoreLayerBreakthrough);
    let check_pocket = !plant.has(PoseSweepPlant::IgnorePocketPullAway);

    let min_body_clear_m = opts.min_body_clearance.as_m();
    let max_breakthrough_m = opts.max_lining_breakthrough.as_m();
    let max_pocket_pull_m = opts.max_pocket_pull_away.as_m();
    let has_garment_skin = weights.joints_0.len() == n_verts && weights.weights_0.len() == n_verts;

    let mut summaries = Vec::new_in(opts.alloc);
    let mut overall_first_failure: Option<PoseSweepHit> = None;

    let mut posed_garment = Vec::new_in(scratch);
    for _ in 0..n_verts {
        push(&mut posed_garment, Vec3::ZERO)?;
    }
    let mut outer_tris = Vec::new_in(scratch);
    let mut outer_orig_idx = Vec::new_in(scratch);
    let mut host_tris_posed = Vec::new_in(scratch);

    for &motion in opts.motions.iter() {
        let mut motion_first_failure: Option<PoseSweepHit> = None;
        let mut worst_body_clearance_mm = f64::INFINITY;
        let mut worst_breakthrough_mm = 0.0f64;
        let mut worst_pocket_pull_away_mm = 0.0f64;

        for frame in 1..=opts.frames_per_motion {
            let phase = (frame as f64) / (opts.frames_per_motion as f64);
            let posed_body = pose_body_at(body_glb, motion, phase, scratch)?;

            // Skin every garment vertex with exact LBS (+ phase-scaled morph_deltas).
            let can_skin = has_garment_skin && !posed_body.skin_mats.is_empty();
            for i in 0..n_verts {
                let mut p_bind = garment.positions[i];
                for &(deltas, w) in morph_deltas.iter() {
                    if i < deltas.len() {
                        p_bind = p_bind + deltas[i] * (w * phase);
                    }
                }
                posed_garment[i] = if can_skin {
                    lbs_point(p_bind, weights.joints_0[i], weights.weights_0[i], &posed_body.skin_mats)
                } else {
                    p_bind
                };
            }

            let mut frame_failure: Option<PoseSweepHit> = None;

            // --- Check A: Body Penetration ---
            if check_body && !body_glb.triangles.is_empty() && !posed_body.positions.is_empty() {
                let body_col = MeshCollider::build(&posed_body.positions, &body_glb.triangles, false, scratch)?;
                let mut worst_pen_sd = f64::INFINITY;
                let mut worst_pen_hit: Option<PoseSweepHit> = None;
                for vi in 0..n_verts {
                    if !vert_active[vi] {
                        continue;
                    }
                    let p = posed_garment[vi];
                    let q = body_col.query(p);
                    let sd_mm = q.signed_dist * 1000.0;
                    if sd_mm < worst_body_clearance_mm {
                        worst_body_clearance_mm = sd_mm;
                    }
                    if q.signed_dist < min_body_clear_m && q.signed_dist < worst_pen_sd {
                        worst_pen_sd = q.signed_dist;
                        let depth_m = (min_body_clear_m - q.signed_dist).max(-q.signed_dist);
                        worst_pen_hit = Some(PoseSweepHit {
                            motion,
                            frame,
                            phase,
                            position: p,
                            kind: SweepFailureKind::BodyPenetration {
                                depth: Length::m(depth_m),
                                vertex: vi as u32,
                                panel_id: vert_panel[vi],
                            },
                        });
                    }
                }
                if frame_failure.is_none() {
                    frame_failure = worst_pen_hit;
                }
            }

            // --- Check B: Lining & Inner-Layer Breakthrough through Outer Shell ---
            if check_breakthrough && n_tris > 0 {
                let mut worst_bt_m = 0.0f64;
                let mut worst_bt_hit: Option<PoseSweepHit> = None;
                for outer_layer in 0u8..=2u8 {
                    outer_tris.clear();
                    outer_orig_idx.clear();
                    for ti in 0..n_tris {
                        if garment.tri_layer[ti] == outer_layer && tri_kind[ti] == PanelKind::PatternPiece {
                            push(&mut outer_tris, garment.triangles[ti])?;
                            push(&mut outer_orig_idx, ti as u32)?;
                        }
                    }
                    if outer_tris.is_empty() {
                        continue;
                    }

                    // Check if any inner-layer PatternPiece (layer < outer_layer) or RimLining
                    // (layer == outer_layer) exists before building the outer BVH.
                    let mut has_inner = false;
                    for ti in 0..n_tris {
                        let l = garment.tri_layer[ti];
                        let k = tri_kind[ti];
                        if (k == PanelKind::PatternPiece && l < outer_layer)
                            || (k == PanelKind::RimLining && l == outer_layer)
                        {
                            has_inner = true;
                            break;
                        }
                    }
                    if !has_inner {
                        continue;
                    }

                    let outer_col = MeshCollider::build(&posed_garment, &outer_tris, false, scratch)?;
                    let mut sample = [(0u32, Vec3::ZERO, [0.0f64; 3], 0.0f64); 1];

                    for inner_ti in 0..n_tris {
                        let inner_layer = garment.tri_layer[inner_ti];
                        let inner_kind = tri_kind[inner_ti];
                        let is_candidate = (inner_kind == PanelKind::PatternPiece && inner_layer < outer_layer)
                            || (inner_kind == PanelKind::RimLining && inner_layer == outer_layer);
                        if !is_candidate {
                            continue;
                        }
                        let [ia, ib, ic] = garment.triangles[inner_ti];
                        let pa = posed_garment[ia as usize];
                        let pb = posed_garment[ib as usize];
                        let pc = posed_garment[ic as usize];
                        let centroid = (pa + pb + pc) * (1.0 / 3.0);

                        for probe in [centroid, pa, pb, pc] {
                            let found = outer_col.nearest_surface_samples(probe, &mut sample);
                            if found == 0 {
                                continue;
                            }
                            let (local_outer_ti, cp, _bary, _d2) = sample[0];
                            let [oa, ob, oc] = outer_tris[local_outer_ti as usize];
                            let opa = posed_garment[oa as usize];
                            let opb = posed_garment[ob as usize];
                            let opc = posed_garment[oc as usize];
                            let Some(n_out) = (opb - opa).cross(opc - opa).normalized() else {
                                continue;
                            };
                            let diff = probe - cp;
                            let dn = diff.dot(n_out);
                            if dn <= 0.0 || dn >= 0.060 {
                                continue;
                            }
                            // Lateral projection guard: reject points past open hems/collars where
                            // closest point clamps to a boundary edge with large tangential offset.
                            let tang = diff - n_out * dn;
                            let dt = tang.length();
                            let max_dt = 0.004_f64.max(dn * 0.45);
                            if dt > max_dt {
                                continue;
                            }
                            let bt_mm = dn * 1000.0;
                            if bt_mm > worst_breakthrough_mm {
                                worst_breakthrough_mm = bt_mm;
                            }
                            if dn > max_breakthrough_m && dn > worst_bt_m {
                                worst_bt_m = dn;
                                let orig_outer_ti = outer_orig_idx[local_outer_ti as usize];
                                let inner_panel = garment.tri_panel.get(inner_ti).copied().unwrap_or(0);
                                let outer_panel = garment.tri_panel.get(orig_outer_ti as usize).copied().unwrap_or(0);
                                worst_bt_hit = Some(PoseSweepHit {
                                    motion,
                                    frame,
                                    phase,
                                    position: probe,
                                    kind: SweepFailureKind::LayerBreakthrough {
                                        inner_layer,
                                        outer_layer,
                                        breakthrough: Length::m(dn),
                                        inner_tri: inner_ti as u32,
                                        outer_tri: orig_outer_ti,
                                        inner_panel,
                                        outer_panel,
                                    },
                                });
                            }
                        }
                    }
                }
                if frame_failure.is_none() {
                    frame_failure = worst_bt_hit;
                }
            }

            // --- Check C: Pocket / Surface-Patch Pull-Away from Host Panel ---
            if check_pocket && !pocket_anchors.is_empty() {
                let mut worst_pull_m = 0.0f64;
                let mut worst_pull_hit: Option<PoseSweepHit> = None;
                for &hid in host_ids.iter() {
                    host_tris_posed.clear();
                    for (ti, &tri) in garment.triangles.iter().enumerate() {
                        if garment.tri_panel[ti] == hid {
                            push(&mut host_tris_posed, tri)?;
                        }
                    }
                    if host_tris_posed.is_empty() {
                        continue;
                    }
                    let posed_host_col = MeshCollider::build(&posed_garment, &host_tris_posed, false, scratch)?;
                    for anchor in pocket_anchors.iter() {
                        if anchor.host_panel != hid {
                            continue;
                        }
                        let p = posed_garment[anchor.vertex as usize];
                        let q = posed_host_col.query(p);
                        let posed_dist_m = (p - q.closest).length();
                        let pull_away_m = posed_dist_m - anchor.rest_dist_m;
                        let pull_mm = pull_away_m * 1000.0;
                        if pull_mm > worst_pocket_pull_away_mm {
                            worst_pocket_pull_away_mm = pull_mm;
                        }
                        if pull_away_m > max_pocket_pull_m && pull_away_m > worst_pull_m {
                            worst_pull_m = pull_away_m;
                            worst_pull_hit = Some(PoseSweepHit {
                                motion,
                                frame,
                                phase,
                                position: p,
                                kind: SweepFailureKind::PocketPullAway {
                                    gap: Length::m(posed_dist_m),
                                    rest_gap: Length::m(anchor.rest_dist_m),
                                    pocket_vertex: anchor.vertex,
                                    pocket_panel: anchor.pocket_panel,
                                    host_panel: anchor.host_panel,
                                },
                            });
                        }
                    }
                }
                if frame_failure.is_none() {
                    frame_failure = worst_pull_hit;
                }
            }

            if motion_first_failure.is_none() {
                motion_first_failure = frame_failure;
            }
        }

        if !worst_body_clearance_mm.is_finite() {
            worst_body_clearance_mm = 0.0;
        }
        if overall_first_failure.is_none() {
            overall_first_failure = motion_first_failure;
        }
        let passed = motion_first_failure.is_none();
        push(
            &mut summaries,
            MotionSweepSummary {
                motion,
                frames_checked: opts.frames_per_motion,
                first_failure: motion_first_failure,
                worst_body_clearance_mm,
                worst_breakthrough_mm,
                worst_pocket_pull_away_mm,
                passed,
            },
        )?;
    }

    let passed = overall_first_failure.is_none();
    Ok(PoseSweepReport {
        summaries,
        first_failure: overall_first_failure,
        passed,
    })
}
