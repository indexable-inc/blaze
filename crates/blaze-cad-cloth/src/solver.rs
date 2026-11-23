//! Deterministic pure-Rust XPBD + Anisotropic Continuum Membrane + Bridson/Bergou Isometric
//! Dihedral Bending cloth physics engine.
//!
//! Two-stage garment tailoring pipeline:
//! 1. **Stage 1 (Sewing & Topological Welding)**: Gently pulls matched 2D seam vertex pairs
//!    together under reduced gravity while maintaining membrane/bending/body-contact stability,
//!    records `max_pre_weld_gap`, and then **topologically welds** every sewn seam via deterministic
//!    union-find (rebuilding cross-seam bending hinges so fabric drapes smoothly across seams!).
//! 2. **Stage 2 (Physical Gravity Drape & Settle)**: Full `-9.81 m/s²` gravity with sub-stepped
//!    XPBD anisotropic continuum membrane (`E_uu`, `E_vv`, `E_uv`), exact isometric dihedral
//!    bending, elastic gathering, unified body + boots + Blaze-CAD solid + inner-layer contact
//!    with Coulomb friction, and deterministic spatial-hash cloth self-collision.

#![blaze::sans_io]

use blaze_cad_field::Scratch;
use blaze_math::{Vec3, atan2, sqrt};
use blaze_own::{Alloc, Vec};
use blaze_units::Length;

use crate::collider::UnifiedCollider;
use crate::ipc::{IpcOpts, project_ipc_barriers};
use crate::mesh2d::{ClothMesh, Mesh2DError, rebuild_bending_hinges, recompute_normals};
use crate::pattern::PatternSet;
use crate::remesh::{RemeshOpts, adaptive_refine_cloth};

/// Simulation parameters for [`simulate_garment`].
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct SolverOpts {
    /// Time step in seconds per outer frame (`1/60` s default).
    pub frame_dt: f64 = 1.0 / 60.0,
    /// Number of XPBD sub-steps per frame (`4` gives `dt = 1/240` s).
    pub substeps: u32 = 4,
    /// Number of Gauss-Seidel constraint projection sweeps per sub-step.
    pub iterations: u32 = 4,
    /// Number of sewing frames (Stage 1) before topological seam welding.
    pub sewing_frames: u32 = 45,
    /// Number of physical gravity settle frames (Stage 2) after welding.
    pub settle_frames: u32 = 65,
    /// Gravity acceleration along `Y` (`-9.81 m/s²` in glTF Y-up).
    pub gravity_y: f64 = -9.81,
    /// Gravity multiplier during Stage 1 sewing (low gravity keeps panels aligned while closing).
    pub sewing_gravity_scale: f64 = 0.20,
    /// Per-substep velocity damping (`0.985..=0.996`) for smooth static equilibrium.
    pub velocity_damping: f64 = 0.988,
    /// Extra air clearance margin added on top of `fabric.thickness * 0.5` against colliders.
    pub contact_margin: Length = Length::nm(1_800_000),
    /// Self-collision minimum separation distance between non-adjacent cloth vertices.
    pub self_collision_radius: Length = Length::nm(3_500_000),
    /// Enable deterministic spatial-hash self-collision during Stage 2 settle.
    pub self_collision: bool = true,
    /// Enable smooth $C^2$ Codimensional IPC (VT + EE) barriers & CCD step clamping in Stage 2.
    pub use_ipc: bool = true,
    /// C-IPC barrier activation distance $\hat{d}$ (default `2.8 mm`).
    pub ipc_d_hat: Length = Length::nm(2_800_000),
    /// Enable mid-settle ARCSim curvature/compression adaptive refinement & fold-ridge edge flips.
    pub adaptive_remesh: bool = false,
    /// Maximum interior edges split during mid-settle adaptive refinement (default `220`).
    pub remesh_max_splits: u32 = 220,
    /// Enable Chebyshev acceleration over inner XPBD constraint iterations (faster convergence to
    /// inextensible woven canvas/twill buckling!).
    pub chebyshev_omega: f64 = 0.92,
    /// Hard warp/weft tensile strain limit (`1.018` = max 1.8% stretch before buckling).
    pub max_tensile_strain: f64 = 1.018,
    /// Maximum displacement per seam-pull projection (metres) to avoid violent overshoot.
    pub max_seam_pull: Length = Length::nm(18_000_000),
    /// Velocity threshold (`m/s`) for early static equilibrium convergence in Stage 2.
    pub settle_vel_eps: f64 = 0.008,
}

/// Telemetry returned by [`simulate_garment`] for automated engineering checks.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SimReport {
    /// Maximum distance across any sewn vertex pair immediately prior to topological welding.
    pub max_pre_weld_gap: Length,
    /// Number of welded seam vertex pairs.
    pub welded_pairs: u32,
    /// Number of Stage 2 settle frames actually executed (may stop early at static equilibrium).
    pub settle_frames_run: u32,
    /// Maximum vertex speed at end of settle (`m/s`).
    pub final_max_speed: f64,
    /// Minimum signed clearance against the collider after final projection (metres).
    pub min_clearance_m: f64,
    /// Cumulative C-IPC Vertex-Triangle contacts resolved during Stage 2.
    pub ipc_vt_contacts: u32,
    /// Cumulative C-IPC Edge-Edge contacts resolved during Stage 2.
    pub ipc_ee_contacts: u32,
    /// Interior edges flipped to align with 3D fold ridges during adaptive remeshing.
    pub remesh_edges_flipped: u32,
    /// Interior edges adaptively split at high curvature / buckling compression during remeshing.
    pub remesh_edges_split: u32,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), Mesh2DError> {
    v.try_push(x).map_err(|e| Mesh2DError::Alloc(e.why))
}

/// Finding returned by [`seam_alignment_check`] when a seam is crossed/reversed in 3D pre-sewing
/// placement (`reversed` flag should be flipped!).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SeamAlignmentFinding {
    pub seam: &'static str,
    pub mean_gap: Length,
    pub reversed_mean_gap: Length,
}

/// Deterministic check verifying that every seam in `mesh` is uncrossed in its initial 3D wrapped
/// placement (i.e. endpoint pairing is closer in its authored direction than reversed).
pub fn seam_alignment_check(mesh: &ClothMesh<'_>, patterns: &PatternSet<'_>) -> Result<(), SeamAlignmentFinding> {
    for (si, seam) in patterns.seams.iter().enumerate() {
        let mut count = 0usize;
        let mut sum_cur = 0.0f64;
        // Collect pair range for this seam (pairs are stored contiguously in seam order).
        let mut first = usize::MAX;
        let mut last = 0usize;
        for (pi, pair) in mesh.seam_pairs.iter().enumerate() {
            if pair.seam_idx as usize == si {
                if first == usize::MAX {
                    first = pi;
                }
                last = pi + 1;
                sum_cur += (mesh.init_pos[pair.a as usize] - mesh.init_pos[pair.b as usize]).length();
                count += 1;
            }
        }
        if count >= 2 {
            let mut sum_rev = 0.0f64;
            for k in 0..count {
                let pa = mesh.seam_pairs[first + k].a as usize;
                let pb = mesh.seam_pairs[last - 1 - k].b as usize;
                sum_rev += (mesh.init_pos[pa] - mesh.init_pos[pb]).length();
            }
            let mean_cur = sum_cur / (count as f64);
            let mean_rev = sum_rev / (count as f64);
            if mean_rev + 0.015 < mean_cur {
                return Err(SeamAlignmentFinding {
                    seam: seam.name,
                    mean_gap: Length::m(mean_cur),
                    reversed_mean_gap: Length::m(mean_rev),
                });
            }
        }
    }
    Ok(())
}

/// Run Stage 1 (sewing + topological weld) and Stage 2 (full-gravity physical drape + self-collision)
/// on `mesh` against `collider`. Bit-for-bit deterministic.
pub fn simulate_garment<'a>(
    mesh: &mut ClothMesh<'a>,
    patterns: &PatternSet<'_>,
    collider: &UnifiedCollider<'_, '_>,
    opts: SolverOpts,
    scratch: Alloc<'_>,
    cad_scratch: &mut Scratch<'_>,
) -> Result<SimReport, Mesh2DError> {
    let n = mesh.pos.len();
    let mut prev_pos = Vec::new_in(scratch);
    let mut vel = Vec::new_in(scratch);
    for i in 0..n {
        push(&mut prev_pos, mesh.pos[i])?;
        push(&mut vel, Vec3::ZERO)?;
    }

    let dt = opts.frame_dt / (opts.substeps.max(1) as f64);
    let margin_m = opts.contact_margin.as_m();

    // Ensure initial wrapped positions start strictly outside the body/boots collider.
    project_collider_initial(mesh, patterns, collider, margin_m, cad_scratch);
    for i in 0..n {
        prev_pos[i] = mesh.pos[i];
    }

    // ==================== STAGE 1: PROGRESSIVE SEWING ====================
    for frame in 0..opts.sewing_frames {
        if !mesh.internal_stitches.is_empty() {
            recompute_normals(mesh);
        }
        let sew_alpha = ((frame + 1) as f64 / (opts.sewing_frames.max(1) as f64)).clamp(0.0, 1.0);
        for _ in 0..opts.substeps.max(1) {
            integrate_predict(mesh, &mut prev_pos, &mut vel, dt, opts.gravity_y * opts.sewing_gravity_scale);
            for _ in 0..opts.iterations.max(1) {
                project_pins(mesh, true);
                project_seams(mesh, sew_alpha, opts.max_seam_pull.as_m());
                project_internal_stitches(mesh);
                project_elastics(mesh);
                project_continuum_membrane(mesh, patterns, dt, 0.75, opts.max_tensile_strain);
                project_dihedral_bending(mesh, dt, 0.50);
            }
            project_collider(mesh, patterns, collider, &prev_pos, margin_m, true, cad_scratch);
            update_velocities(mesh, &prev_pos, &mut vel, dt, 0.92);
        }
    }

    // Measure seam closure before topological welding.
    let mut max_gap = 0.0f64;
    for pair in mesh.seam_pairs.iter() {
        if pair.weld {
            let ra = mesh.weld_rep[pair.a as usize] as usize;
            let rb = mesh.weld_rep[pair.b as usize] as usize;
            let d = (mesh.pos[ra] - mesh.pos[rb]).length();
            if d > max_gap {
                max_gap = d;
            }
        }
    }

    // Topologically weld sewn seams via union-find and rebuild cross-seam bending hinges.
    let welded_pairs = weld_seams_topologically(mesh, &mut vel, scratch)?;
    rebuild_bending_hinges(mesh, patterns)?;

    // Release sewing-only pins, damp velocities smoothly, and seed subtle deterministic
    // Euler-buckling asymmetry (±0.9 mm) on free vertices so compressed vertical/horizontal fabric
    // breaks symmetry cleanly into genuine organic 3D gravity drapery waves!
    for v in vel.iter_mut() {
        *v = *v * 0.20;
    }
    recompute_normals(mesh);
    for i in 0..n {
        if mesh.weld_rep[i] as usize == i && mesh.inv_mass[i] > 0.0 && !mesh.is_open_border[i] {
            let uv = mesh.rest_uv_m[i];
            let wave = blaze_math::sin(uv.x * 29.0 + uv.y * 17.0) * blaze_math::cos(uv.y * 23.0 - uv.x * 13.0);
            mesh.pos[i] = mesh.pos[i] + mesh.normals[i] * (0.0009 * wave);
            prev_pos[i] = mesh.pos[i];
        }
    }

    // Build adjacency / Chebyshev history buffers for Stage 2 settle.
    let self_r = opts.self_collision_radius.as_m();
    let mut hash_cells = Vec::new_in(scratch);
    let mut cheb_prev = Vec::new_in(scratch);
    let mut cheb_cur = Vec::new_in(scratch);
    for i in 0..n {
        push(&mut cheb_prev, mesh.pos[i])?;
        push(&mut cheb_cur, mesh.pos[i])?;
    }

    // ==================== STAGE 2: PHYSICAL GRAVITY DRAPE ====================
    let mut n = mesh.pos.len();
    let mut settle_frames_run = 0u32;
    let mut final_max_speed = 0.0f64;
    let mut ipc_vt_contacts = 0u32;
    let mut ipc_ee_contacts = 0u32;
    let mut remesh_edges_flipped = 0u32;
    let mut remesh_edges_split = 0u32;
    let rho2 = (opts.chebyshev_omega.clamp(0.0, 0.96)) * (opts.chebyshev_omega.clamp(0.0, 0.96));
    let substeps = opts.substeps.max(1);
    for frame in 0..opts.settle_frames {
        settle_frames_run = frame + 1;
        if !mesh.internal_stitches.is_empty() {
            recompute_normals(mesh);
        }

        // Mid-settle ARCSim curvature/compression adaptive refinement + fold-ridge edge flips
        // once natural gravity folds have formed.
        if opts.adaptive_remesh && frame == 18 {
            sync_welded_positions(mesh);
            let rep = adaptive_refine_cloth(
                mesh,
                patterns,
                RemeshOpts { max_splits: opts.remesh_max_splits, .. },
                scratch,
            )?;
            remesh_edges_flipped += rep.edges_flipped;
            remesh_edges_split += rep.edges_split;
            let new_n = mesh.pos.len();
            if new_n > n {
                for i in n..new_n {
                    push(&mut prev_pos, mesh.pos[i])?;
                    push(&mut vel, Vec3::ZERO)?;
                    push(&mut cheb_prev, mesh.pos[i])?;
                    push(&mut cheb_cur, mesh.pos[i])?;
                }
                n = new_n;
            }
        }

        // Ramp gravity smoothly over first 10 settle frames to avoid shock bounce.
        let grav_ramp = ((frame + 1) as f64 / 10.0).min(1.0);
        let gy = opts.gravity_y * grav_ramp;
        // Smoothly taper Chebyshev acceleration down to 0 over the final 25 frames and apply
        // stronger final-phase velocity damping so seams & folds settle to true static equilibrium!
        let frames_left = opts.settle_frames.saturating_sub(frame);
        let cheb_fade = ((frames_left as f64) / 25.0).clamp(0.0, 1.0);
        let eff_rho2 = rho2 * cheb_fade * cheb_fade;
        let in_final_polish = frames_left <= 10;
        let eff_damp = if in_final_polish { opts.velocity_damping.min(0.955) } else { opts.velocity_damping };
        for sub in 0..substeps {
            integrate_predict(mesh, &mut prev_pos, &mut vel, dt, gy);
            let iters = opts.iterations.max(1);
            let mut omega = 1.0f64;
            for iter in 0..iters {
                if eff_rho2 > 0.01 {
                    for i in 0..n {
                        cheb_cur[i] = mesh.pos[i];
                    }
                }
                project_pins(mesh, false);
                project_internal_stitches(mesh);
                project_elastics(mesh);
                project_continuum_membrane(mesh, patterns, dt, 1.0, opts.max_tensile_strain);
                project_dihedral_bending(mesh, dt, 1.0);
                // Deterministic Chebyshev acceleration (Wang 2015): boosts global inextensibility
                // across woven canvas/twill without destabilizing Gauss-Seidel!
                if eff_rho2 > 0.01 {
                    if iter == 0 {
                        omega = 1.0;
                    } else if iter == 1 {
                        omega = 2.0 / (2.0 - eff_rho2);
                    } else {
                        omega = 4.0 / (4.0 - eff_rho2 * omega);
                    }
                    let w_safe = omega.clamp(1.0, 1.45);
                    if iter >= 2 && w_safe > 1.001 {
                        for i in 0..n {
                            if mesh.weld_rep[i] as usize == i && mesh.inv_mass[i] > 0.0 {
                                let delta = mesh.pos[i] - cheb_prev[i];
                                if delta.dot(delta) < 0.008 * 0.008 {
                                    mesh.pos[i] = cheb_prev[i] + delta * w_safe;
                                }
                            }
                        }
                    }
                    for i in 0..n {
                        cheb_prev[i] = cheb_cur[i];
                    }
                }
            }
            if opts.self_collision && self_r > 1e-5 {
                project_self_collision(mesh, self_r, &mut hash_cells)?;
            }
            if opts.use_ipc && !in_final_polish && sub + 1 == substeps && frame % 2 == 0 {
                let stats = project_ipc_barriers(
                    mesh,
                    &prev_pos,
                    IpcOpts { d_hat: opts.ipc_d_hat, .. },
                    scratch,
                )?;
                ipc_vt_contacts = ipc_vt_contacts.saturating_add(stats.vt_contacts);
                ipc_ee_contacts = ipc_ee_contacts.saturating_add(stats.ee_contacts);
            }
            project_collider(mesh, patterns, collider, &prev_pos, margin_m, false, cad_scratch);
            update_velocities(mesh, &prev_pos, &mut vel, dt, eff_damp);
        }

        // Check equilibrium speed.
        let mut max_spd = 0.0f64;
        for i in 0..n {
            if mesh.weld_rep[i] as usize == i && mesh.inv_mass[i] > 0.0 {
                let s = vel[i].length();
                if s > max_spd {
                    max_spd = s;
                }
            }
        }
        final_max_speed = max_spd;
        if frame >= 24 && max_spd < opts.settle_vel_eps {
            break;
        }
    }

    // Sync welded slave vertices to their representative positions and measure clearance.
    sync_welded_positions(mesh);
    let mut min_clearance = f64::INFINITY;
    for i in 0..n {
        if mesh.weld_rep[i] as usize == i {
            let q = collider.query(mesh.pos[i], cad_scratch);
            if q.signed_dist < min_clearance {
                min_clearance = q.signed_dist;
            }
        }
    }
    recompute_normals(mesh);

    Ok(SimReport {
        max_pre_weld_gap: Length::m(max_gap),
        welded_pairs,
        settle_frames_run,
        final_max_speed,
        min_clearance_m: if min_clearance.is_finite() { min_clearance } else { 0.0 },
        ipc_vt_contacts,
        ipc_ee_contacts,
        remesh_edges_flipped,
        remesh_edges_split,
    })
}

fn project_collider_initial(
    mesh: &mut ClothMesh<'_>,
    patterns: &PatternSet<'_>,
    collider: &UnifiedCollider<'_, '_>,
    margin_m: f64,
    cad_scratch: &mut Scratch<'_>,
) {
    for i in 0..mesh.pos.len() {
        let fab = patterns.fabrics.get(mesh.vert_fabric[i] as usize);
        let thick = fab.map(|f| f.thickness.as_m() * 0.5).unwrap_or(0.001) + margin_m;
        let q = collider.query(mesh.pos[i], cad_scratch);
        if q.signed_dist < thick {
            mesh.pos[i] = q.closest + q.normal * thick;
            mesh.init_pos[i] = mesh.pos[i];
        }
    }
    // Keep pin targets strictly outside the body as well.
    for pin in mesh.pins.iter_mut() {
        pin.target = mesh.init_pos[pin.vertex as usize];
    }
}

fn integrate_predict(
    mesh: &mut ClothMesh<'_>,
    prev_pos: &mut [Vec3],
    vel: &mut [Vec3],
    dt: f64,
    gravity_y: f64,
) {
    let g = Vec3::new(0.0, gravity_y, 0.0);
    for i in 0..mesh.pos.len() {
        if mesh.weld_rep[i] as usize != i {
            continue;
        }
        prev_pos[i] = mesh.pos[i];
        if mesh.inv_mass[i] <= 0.0 {
            continue;
        }
        let v = vel[i] + g * dt;
        vel[i] = v;
        mesh.pos[i] = mesh.pos[i] + v * dt;
    }
}

fn update_velocities(
    mesh: &ClothMesh<'_>,
    prev_pos: &[Vec3],
    vel: &mut [Vec3],
    dt: f64,
    damping: f64,
) {
    let inv_dt = 1.0 / dt.max(1e-9);
    for i in 0..mesh.pos.len() {
        if mesh.weld_rep[i] as usize != i || mesh.inv_mass[i] <= 0.0 {
            vel[i] = Vec3::ZERO;
            continue;
        }
        let mut v = (mesh.pos[i] - prev_pos[i]) * (inv_dt * damping);
        // Clamp extreme velocity spikes to guarantee zero spiking / triangle inversion.
        let spd = v.length();
        if spd > 1.8 {
            v = v * (1.8 / spd);
        }
        vel[i] = v;
    }
}

fn project_pins(mesh: &mut ClothMesh<'_>, in_sewing: bool) {
    for pin in mesh.pins.iter() {
        if pin.sewing_only && !in_sewing {
            continue;
        }
        let r = mesh.weld_rep[pin.vertex as usize] as usize;
        if pin.stiffness >= 1.0 {
            mesh.pos[r] = pin.target;
        } else if mesh.inv_mass[r] > 0.0 {
            let s = pin.stiffness.clamp(0.0, 1.0);
            mesh.pos[r] = mesh.pos[r] * (1.0 - s) + pin.target * s;
        }
    }
}

fn project_seams(mesh: &mut ClothMesh<'_>, sew_alpha: f64, max_pull: f64) {
    // Progressive pull stiffness ramps from 0.15 to 0.85 over Stage 1.
    let strength = 0.15 + 0.70 * sew_alpha;
    for pair in mesh.seam_pairs.iter() {
        let ra = mesh.weld_rep[pair.a as usize] as usize;
        let rb = mesh.weld_rep[pair.b as usize] as usize;
        if ra == rb {
            continue;
        }
        let wa = mesh.inv_mass[ra];
        let wb = mesh.inv_mass[rb];
        let wsum = wa + wb;
        if wsum <= 1e-12 {
            continue;
        }
        let diff = mesh.pos[rb] - mesh.pos[ra];
        let dist = diff.length();
        if dist <= 1e-7 {
            continue;
        }
        let pull = (dist * strength).min(max_pull);
        let dir = diff * (pull / dist);
        mesh.pos[ra] = mesh.pos[ra] + dir * (wa / wsum);
        mesh.pos[rb] = mesh.pos[rb] - dir * (wb / wsum);
    }
}

/// Anchor internal-seam sub-panel edge vertices (`sub_vert`) cleanly onto their host interior
/// surface vertex (`host_vert + host_normal * offset`), transferring real sub-panel gravity
/// weight back onto the host panel while allowing hanging flaps & folded collar leaves to hinge!
fn project_internal_stitches(mesh: &mut ClothMesh<'_>) {
    for st in mesh.internal_stitches.iter() {
        let rh = mesh.weld_rep[st.host_vert as usize] as usize;
        let rs = mesh.weld_rep[st.sub_vert as usize] as usize;
        if rh == rs {
            continue;
        }
        let target = mesh.pos[rh] + mesh.normals[rh] * st.normal_offset_m;
        let wh = mesh.inv_mass[rh];
        let ws = mesh.inv_mass[rs];
        let wsum = wh * 0.25 + ws;
        if wsum <= 1e-12 {
            continue;
        }
        let diff = target - mesh.pos[rs];
        mesh.pos[rs] = mesh.pos[rs] + diff * (ws / wsum);
        mesh.pos[rh] = mesh.pos[rh] - diff * (wh * 0.25 / wsum);
    }
}

fn project_elastics(mesh: &mut ClothMesh<'_>) {
    for el in mesh.elastics.iter() {
        let ra = mesh.weld_rep[el.a as usize] as usize;
        let rb = mesh.weld_rep[el.b as usize] as usize;
        if ra == rb {
            continue;
        }
        let wa = mesh.inv_mass[ra];
        let wb = mesh.inv_mass[rb];
        let wsum = wa + wb;
        if wsum <= 1e-12 {
            continue;
        }
        let diff = mesh.pos[rb] - mesh.pos[ra];
        let len = diff.length();
        if len <= 1e-9 {
            continue;
        }
        let err = len - el.rest_len;
        let corr = diff * (err * 0.65 / len);
        mesh.pos[ra] = mesh.pos[ra] + corr * (wa / wsum);
        mesh.pos[rb] = mesh.pos[rb] - corr * (wb / wsum);
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn apply_bounded_tri_step(
    pa: &mut Vec3,
    pb: &mut Vec3,
    pc: &mut Vec3,
    ga: Vec3,
    gb: Vec3,
    gc: Vec3,
    wa: f64,
    wb: f64,
    wc: f64,
    lam: f64,
    max_disp_m: f64,
) {
    let da = ga * (wa * lam);
    let db = gb * (wb * lam);
    let dc = gc * (wc * lam);
    let max_d = da.length().max(db.length()).max(dc.length());
    let scale = if max_d > max_disp_m { max_disp_m / max_d } else { 1.0 };
    *pa = *pa + da * scale;
    *pb = *pb + db * scale;
    *pc = *pc + dc * scale;
}

/// Anisotropic continuum triangle membrane projection directly from flat 2D pattern `Dm^{-1}`
/// with bounded displacement stability across ARCSim-refined small triangles!
fn project_continuum_membrane(
    mesh: &mut ClothMesh<'_>,
    patterns: &PatternSet<'_>,
    dt: f64,
    stiffness_scale: f64,
    max_tensile_strain: f64,
) {
    let dt2 = dt * dt;
    for ti in 0..mesh.triangles.len() {
        let [ia, ib, ic] = mesh.triangles[ti];
        let ra = mesh.weld_rep[ia as usize] as usize;
        let rb = mesh.weld_rep[ib as usize] as usize;
        let rc = mesh.weld_rep[ic as usize] as usize;
        if ra == rb || rb == rc || rc == ra {
            continue;
        }
        let area = mesh.tri_rest_area[ti];
        if area <= 1e-10 {
            continue;
        }
        let wa = mesh.inv_mass[ra];
        let wb = mesh.inv_mass[rb];
        let wc = mesh.inv_mass[rc];
        if wa + wb + wc <= 1e-12 {
            continue;
        }
        let [d00, d01, d10, d11] = mesh.tri_dm_inv[ti];
        let fab = patterns.fabrics.get(mesh.tri_fabric[ti] as usize);
        let k_weft = fab.map(|f| f.weft_stiffness).unwrap_or(800.0) * stiffness_scale;
        let k_warp = fab.map(|f| f.warp_stiffness).unwrap_or(900.0) * stiffness_scale;
        let k_shear = fab.map(|f| f.shear_stiffness).unwrap_or(160.0) * stiffness_scale;

        let mut pa = mesh.pos[ra];
        let mut pb = mesh.pos[rb];
        let mut pc = mesh.pos[rc];

        // 1. Weft stretch constraint: C_u = |f_u| - 1 (stiffens 3x past max_tensile_strain without exploding!)
        {
            let e1 = pb - pa;
            let e2 = pc - pa;
            let fu = e1 * d00 + e2 * d10;
            let len = fu.length();
            if len > 1e-9 {
                let c = len - 1.0;
                let dir = fu * (1.0 / len);
                let gb = dir * d00;
                let gc = dir * d10;
                let ga = -(gb + gc);
                let k_eff = if len > max_tensile_strain { k_weft * 3.0 } else { k_weft };
                let alpha = 1.0 / (k_eff * area * dt2).max(1e-9);
                let denom = wa * ga.dot(ga) + wb * gb.dot(gb) + wc * gc.dot(gc) + alpha;
                if denom > 1e-12 {
                    let lam = -c / denom;
                    apply_bounded_tri_step(&mut pa, &mut pb, &mut pc, ga, gb, gc, wa, wb, wc, lam, 0.0028);
                }
            }
        }

        // 2. Warp stretch constraint: C_v = |f_v| - 1
        {
            let e1 = pb - pa;
            let e2 = pc - pa;
            let fv = e1 * d01 + e2 * d11;
            let len = fv.length();
            if len > 1e-9 {
                let c = len - 1.0;
                let dir = fv * (1.0 / len);
                let gb = dir * d01;
                let gc = dir * d11;
                let ga = -(gb + gc);
                let k_eff = if len > max_tensile_strain { k_warp * 3.0 } else { k_warp };
                let alpha = 1.0 / (k_eff * area * dt2).max(1e-9);
                let denom = wa * ga.dot(ga) + wb * gb.dot(gb) + wc * gc.dot(gc) + alpha;
                if denom > 1e-12 {
                    let lam = -c / denom;
                    apply_bounded_tri_step(&mut pa, &mut pb, &mut pc, ga, gb, gc, wa, wb, wc, lam, 0.0028);
                }
            }
        }

        // 3. In-plane bias shear constraint: C_uv = f_u · f_v
        {
            let e1 = pb - pa;
            let e2 = pc - pa;
            let fu = e1 * d00 + e2 * d10;
            let fv = e1 * d01 + e2 * d11;
            let c = fu.dot(fv);
            let gb = fv * d00 + fu * d01;
            let gc = fv * d10 + fu * d11;
            let ga = -(gb + gc);
            let alpha = 1.0 / (k_shear * area * dt2).max(1e-9);
            let denom = wa * ga.dot(ga) + wb * gb.dot(gb) + wc * gc.dot(gc) + alpha;
            if denom > 1e-12 {
                let lam = -c / denom;
                apply_bounded_tri_step(&mut pa, &mut pb, &mut pc, ga, gb, gc, wa, wb, wc, lam, 0.0024);
            }
        }

        mesh.pos[ra] = pa;
        mesh.pos[rb] = pb;
        mesh.pos[rc] = pc;
    }
}

/// Exact Bridson / Bergou / XPBD isometric dihedral bending projection.
fn project_dihedral_bending(mesh: &mut ClothMesh<'_>, dt: f64, stiffness_scale: f64) {
    let dt2 = dt * dt;
    for hi in 0..mesh.hinges.len() {
        let h = mesh.hinges[hi];
        let r0 = mesh.weld_rep[h.v0 as usize] as usize;
        let r1 = mesh.weld_rep[h.v1 as usize] as usize;
        let r2 = mesh.weld_rep[h.v2 as usize] as usize;
        let r3 = mesh.weld_rep[h.v3 as usize] as usize;

        let w0 = mesh.inv_mass[r0];
        let w1 = mesh.inv_mass[r1];
        let w2 = mesh.inv_mass[r2];
        let w3 = mesh.inv_mass[r3];
        if w0 + w1 + w2 + w3 <= 1e-12 {
            continue;
        }

        let p0 = mesh.pos[r0];
        let p1 = mesh.pos[r1];
        let p2 = mesh.pos[r2];
        let p3 = mesh.pos[r3];

        let e = p1 - p0;
        let elen2 = e.dot(e);
        if elen2 <= 1e-12 {
            continue;
        }
        let elen = sqrt(elen2);
        let e_hat = e * (1.0 / elen);

        let n1 = e.cross(p2 - p0);
        let n2 = (p3 - p0).cross(e);
        let n1_len2 = n1.dot(n1);
        let n2_len2 = n2.dot(n2);
        if n1_len2 <= 1e-14 || n2_len2 <= 1e-14 {
            continue;
        }
        let n1_hat = n1 * (1.0 / sqrt(n1_len2));
        let n2_hat = n2 * (1.0 / sqrt(n2_len2));

        let sin_t = n1_hat.cross(n2_hat).dot(e_hat);
        let cos_t = n1_hat.dot(n2_hat).clamp(-1.0, 1.0);
        let theta = atan2(sin_t, cos_t);
        let c = theta - h.rest_angle;
        if c.abs() < 1e-7 {
            continue;
        }

        // Exact analytical gradients (translation & rotation invariant: g0 + g1 + g2 + g3 == 0).
        // Scaled by discrete-shells edge factor `|e| / h_avg` (Bergou / Grinspun 2003) so real SI
        // flexural rigidity `k_b ~ 0.0002..0.001 N·m` produces authentic physical fabric waves!
        let g2 = n1 * (-elen / n1_len2);
        let g3 = n2 * (-elen / n2_len2);
        let inv_e2 = 1.0 / elen2;
        let c0_2 = (p2 - p1).dot(e) * inv_e2;
        let c1_2 = -(p2 - p0).dot(e) * inv_e2;
        let c0_3 = (p3 - p1).dot(e) * inv_e2;
        let c1_3 = -(p3 - p0).dot(e) * inv_e2;
        let g0 = g2 * c0_2 + g3 * c0_3;
        let g1 = g2 * c1_2 + g3 * c1_3;

        let h_avg = 0.5 * (sqrt(n1_len2) + sqrt(n2_len2)) / elen;
        let geom_factor = (elen / h_avg.max(1e-4)).clamp(0.25, 4.0);
        let k_eff = (h.stiffness * geom_factor * stiffness_scale).max(1e-8);
        let alpha = 1.0 / (k_eff * dt2).max(1e-12);
        let denom = w0 * g0.dot(g0) + w1 * g1.dot(g1) + w2 * g2.dot(g2) + w3 * g3.dot(g3) + alpha;
        if denom <= 1e-12 {
            continue;
        }
        let lam = -c / denom;
        let d0 = g0 * (w0 * lam);
        let d1 = g1 * (w1 * lam);
        let d2 = g2 * (w2 * lam);
        let d3 = g3 * (w3 * lam);
        let max_d = d0.length().max(d1.length()).max(d2.length()).max(d3.length());
        let scale = if max_d > 0.0020 { 0.0020 / max_d } else { 1.0 };
        mesh.pos[r0] = p0 + d0 * scale;
        mesh.pos[r1] = p1 + d1 * scale;
        mesh.pos[r2] = p2 + d2 * scale;
        mesh.pos[r3] = p3 + d3 * scale;
    }
}

fn project_collider(
    mesh: &mut ClothMesh<'_>,
    patterns: &PatternSet<'_>,
    collider: &UnifiedCollider<'_, '_>,
    prev_pos: &[Vec3],
    margin_m: f64,
    sewing: bool,
    cad_scratch: &mut Scratch<'_>,
) {
    for i in 0..mesh.pos.len() {
        if mesh.weld_rep[i] as usize != i || mesh.inv_mass[i] <= 0.0 {
            continue;
        }
        let fab = patterns.fabrics.get(mesh.vert_fabric[i] as usize);
        let thick = fab.map(|f| f.thickness.as_m() * 0.5).unwrap_or(0.001) + margin_m;
        let mu = if sewing { 0.20 } else { fab.map(|f| f.friction).unwrap_or(0.55) };
        let q = collider.query(mesh.pos[i], cad_scratch);
        if q.signed_dist < thick {
            let pen = thick - q.signed_dist;
            let p_proj = q.closest + q.normal * thick;
            // Apply tangential Coulomb friction relative to previous substep position.
            let disp = p_proj - prev_pos[i];
            let normal_disp = q.normal * disp.dot(q.normal);
            let tang = disp - normal_disp;
            let tang_len = tang.length();
            let p_final = if tang_len > 1e-9 {
                let max_fric = mu * pen;
                if tang_len <= max_fric {
                    p_proj - tang
                } else {
                    p_proj - tang * (max_fric / tang_len)
                }
            } else {
                p_proj
            };
            mesh.pos[i] = p_final;
        }
    }
}

/// Deterministic spatial-hash cloth self-collision separating non-adjacent vertices within `radius`.
fn project_self_collision(
    mesh: &mut ClothMesh<'_>,
    radius: f64,
    cells: &mut Vec<(i64, u32), Alloc<'_>>,
) -> Result<(), Mesh2DError> {
    cells.clear();
    let inv_cell = 1.0 / (radius * 1.25).max(1e-4);
    for i in 0..mesh.pos.len() {
        if mesh.weld_rep[i] as usize != i {
            continue;
        }
        let p = mesh.pos[i];
        let cx = (p.x * inv_cell).floor() as i64;
        let cy = (p.y * inv_cell).floor() as i64;
        let cz = (p.z * inv_cell).floor() as i64;
        let key = pack_cell(cx, cy, cz);
        push(cells, (key, i as u32))?;
    }
    cells.sort_unstable();

    let r2 = radius * radius;
    let mut start = 0usize;
    while start < cells.len() {
        let key = cells[start].0;
        let mut end = start + 1;
        while end < cells.len() && cells[end].0 == key {
            end += 1;
        }
        // Check pairs within the same spatial cell (deterministic index order).
        let cap = (end - start).min(24);
        for a_i in 0..cap {
            let va = cells[start + a_i].1 as usize;
            let wa = mesh.inv_mass[va];
            let pa_uv = mesh.rest_uv_m[va];
            let panel_a = mesh.vert_panel[va];
            for b_i in (a_i + 1)..cap {
                let vb = cells[start + b_i].1 as usize;
                let wb = mesh.inv_mass[vb];
                if wa + wb <= 1e-12 {
                    continue;
                }
                // Skip vertices that are neighbours in 2D rest space on the same panel.
                if panel_a == mesh.vert_panel[vb] {
                    let duv = mesh.rest_uv_m[vb] - pa_uv;
                    if duv.dot(duv) < (radius * 2.8) * (radius * 2.8) {
                        continue;
                    }
                }
                let diff = mesh.pos[vb] - mesh.pos[va];
                let d2 = diff.dot(diff);
                if d2 < r2 && d2 > 1e-12 {
                    let d = sqrt(d2);
                    let push_dist = (radius - d) * 0.45;
                    let dir = diff * (push_dist / d);
                    let wsum = wa + wb;
                    mesh.pos[va] = mesh.pos[va] - dir * (wa / wsum);
                    mesh.pos[vb] = mesh.pos[vb] + dir * (wb / wsum);
                }
            }
        }
        start = end;
    }
    Ok(())
}

#[inline]
fn pack_cell(cx: i64, cy: i64, cz: i64) -> i64 {
    let bx = (cx + 524_288) & 0xFFFFF;
    let by = (cy + 524_288) & 0xFFFFF;
    let bz = (cz + 524_288) & 0xFFFFF;
    (bx << 40) | (by << 20) | bz
}

/// Topologically weld every `SeamPair { weld: true }` via deterministic union-find (lower vertex
/// index wins). Sums masses and averages positions/velocities so seams become 100% watertight
/// while keeping per-panel UV/seam attributes intact!
fn weld_seams_topologically(
    mesh: &mut ClothMesh<'_>,
    vel: &mut [Vec3],
    scratch: Alloc<'_>,
) -> Result<u32, Mesh2DError> {
    let n = mesh.pos.len();
    let mut parent = Vec::new_in(scratch);
    for i in 0..n {
        push(&mut parent, i as u32)?;
    }

    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let p = parent[x as usize];
            parent[x as usize] = parent[p as usize];
            x = p;
        }
        x
    }

    let mut welded_pairs = 0u32;
    for pair in mesh.seam_pairs.iter() {
        if !pair.weld {
            continue;
        }
        let ra = find(&mut parent, pair.a);
        let rb = find(&mut parent, pair.b);
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            parent[hi as usize] = lo;
            welded_pairs += 1;
        }
    }

    // Accumulate mass-weighted positions and velocities onto each root.
    let mut mass_sum = Vec::new_in(scratch);
    let mut pos_sum = Vec::new_in(scratch);
    let mut vel_sum = Vec::new_in(scratch);
    let mut count = Vec::new_in(scratch);
    let mut pinned = Vec::new_in(scratch);
    for _ in 0..n {
        push(&mut mass_sum, 0.0f64)?;
        push(&mut pos_sum, Vec3::ZERO)?;
        push(&mut vel_sum, Vec3::ZERO)?;
        push(&mut count, 0u32)?;
        push(&mut pinned, false)?;
    }

    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        mesh.weld_rep[i] = r as u32;
        if mesh.inv_mass[i] <= 0.0 {
            pinned[r] = true;
            pos_sum[r] = pos_sum[r] + mesh.pos[i];
            count[r] += 1;
        } else {
            let m = 1.0 / mesh.inv_mass[i];
            mass_sum[r] += m;
            pos_sum[r] = pos_sum[r] + mesh.pos[i];
            vel_sum[r] = vel_sum[r] + vel[i];
            count[r] += 1;
        }
    }

    for i in 0..n {
        if mesh.weld_rep[i] as usize == i && count[i] > 0 {
            let inv_c = 1.0 / (count[i] as f64);
            mesh.pos[i] = pos_sum[i] * inv_c;
            vel[i] = vel_sum[i] * inv_c;
            mesh.inv_mass[i] = if pinned[i] || mass_sum[i] <= 1e-12 { 0.0 } else { 1.0 / mass_sum[i] };
        }
    }
    sync_welded_positions(mesh);
    Ok(welded_pairs)
}

pub fn sync_welded_positions(mesh: &mut ClothMesh<'_>) {
    for i in 0..mesh.pos.len() {
        let r = mesh.weld_rep[i] as usize;
        mesh.pos[i] = mesh.pos[r];
    }
}
