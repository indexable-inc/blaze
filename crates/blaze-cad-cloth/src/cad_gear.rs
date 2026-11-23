//! Direct `blaze-cad-field` hard-surface tactical gear CAD solids for cloth co-simulation
//! (`UnifiedCollider::cad_tapes`) and verified manifold meshing directly into [`GarmentMesh`].
//!
//! Every hardware piece is authored as an exact implicit CSG solid in `blaze-cad-field` (with its
//! back face resting at local `z = 0` and front detail protruding along `+z`, centered in `x, y`):
//! 1. **Tactical Cobra Belt Buckle** ([`build_cobra_buckle_tape`]): bevelled outer frame with
//!    webbing slots, proud central lock bar with recessed groove, and dual quick-release wings.
//! 2. **Spacefarer Chest Comms / Telemetry Beacon** ([`build_chest_beacon_tape`]): bevelled
//!    rectangular housing, raised circular sensor bezel with recessed lens cavity, tactile status
//!    bar, and recessed top antenna nub.
//! 3. **Collar Magnetic Clasp** ([`build_collar_clasp_tape`]): sleek chamfered low-profile clasp
//!    bar with twin end ridges, central parting channel, and flush alignment dimples.
//!
//! [`append_cad_solid_to_garment`] meshes any compiled [`Tape`] via `blaze-cad-mesh::mesh`,
//! transforms it into the draped cloth tangent frame `(tu, tv, nrm)` at `origin`, tags every
//! vertex with `rigid_anchor = Some(anchor)` and `tri_layer = 3`, and appends directly to the
//! garment mesh with zero stretching during animation.

#![blaze::sans_io]

use blaze_cad_field::{
    Axis, BlendOpts, BoxOpts, CadError, CompileError, CompileOpts, CylinderOpts, Graph, GraphOpts, Placement, Tape,
    compile,
};
use blaze_cad_mesh::{MeshError, MeshOpts, auto_grid, mesh as contour_mesh};
use blaze_math::{Vec2, Vec3};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::{Angle, Length};

use crate::detail::{GarmentMesh, PanelKind};

/// Default `[width, height, thickness]` for the Tactical Cobra Belt Buckle (`54 × 42 × 10 mm`).
pub const COBRA_BUCKLE_DEFAULT: [Length; 3] = [
    Length::nm(54_000_000),
    Length::nm(42_000_000),
    Length::nm(10_000_000),
];

/// Default `[width, height, thickness]` for the Spacefarer Chest Comms / Telemetry Beacon
/// (`44 × 64 × 12 mm`).
pub const CHEST_BEACON_DEFAULT: [Length; 3] = [
    Length::nm(44_000_000),
    Length::nm(64_000_000),
    Length::nm(12_000_000),
];

/// Default `[width, height, thickness]` for the Collar Magnetic Clasp (`38 × 14 × 6 mm`).
pub const COLLAR_CLASP_DEFAULT: [Length; 3] = [
    Length::nm(38_000_000),
    Length::nm(14_000_000),
    Length::nm(6_000_000),
];

/// Errors produced when building, compiling, or meshing hard-surface CAD gear solids.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CadGearError {
    Cad(CadError),
    Compile(CompileError),
    Mesh(MeshError),
    Alloc(AllocFailure),
}

impl From<CadError> for CadGearError {
    fn from(e: CadError) -> Self {
        Self::Cad(e)
    }
}

impl From<CompileError> for CadGearError {
    fn from(e: CompileError) -> Self {
        Self::Compile(e)
    }
}

impl From<MeshError> for CadGearError {
    fn from(e: MeshError) -> Self {
        Self::Mesh(e)
    }
}

impl From<AllocFailure> for CadGearError {
    fn from(e: AllocFailure) -> Self {
        Self::Alloc(e)
    }
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), CadGearError> {
    v.try_push(x).map_err(|e| CadGearError::Alloc(e.why))
}

#[inline]
fn scaled_pos(base: Length, frac: f64) -> Length {
    let nm = ((base.as_nm() as f64) * frac) as i64;
    Length::nm(nm.max(2_000))
}

#[inline]
fn scaled_signed(base: Length, frac: f64) -> Length {
    Length::nm(((base.as_nm() as f64) * frac) as i64)
}

#[inline]
fn clamped_box_round(size: [Length; 3], frac: f64) -> Length {
    let min_half = (size[0].as_nm() / 2)
        .min(size[1].as_nm() / 2)
        .min(size[2].as_nm() / 2);
    let desired = ((min_half as f64) * frac) as i64;
    Length::nm(desired.clamp(0, (min_half - 1_000).max(0)))
}

/// Build a parametric Tactical Cobra Belt Buckle compiled to a [`Tape`] in local coordinates
/// (back face at `z = 0`, front proud at `z = +thickness`, centered in `x, y`).
pub fn build_cobra_buckle_tape<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Tape<'a>, CadGearError> {
    build_cobra_buckle_tape_at(width, height, thickness, Placement::IDENTITY, alloc, scratch)
}

/// Build a parametric Tactical Cobra Belt Buckle placed at `placement` (e.g. for direct world-space
/// cloth co-simulation in `UnifiedCollider::cad_tapes`).
pub fn build_cobra_buckle_tape_at<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    placement: Placement,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Tape<'a>, CadGearError> {
    let mut g = Graph::new(GraphOpts { alloc: scratch, .. });

    // 1. Bevelled aluminum/titanium outer buckle frame.
    let frame_size = [width, height, thickness];
    let frame_raw = g.cuboid(BoxOpts {
        size: frame_size,
        round: clamped_box_round(frame_size, 0.35),
        name: Some("cobra_outer_frame"),
    })?;
    let half_t = scaled_pos(thickness, 0.50);
    let frame = g.place(frame_raw, Placement::shift(Length::ZERO, Length::ZERO, half_t))?;

    // 2. Left & right belt-webbing through-slots cut out of the outer frame.
    let slot_size = [scaled_pos(width, 0.17), scaled_pos(height, 0.72), scaled_pos(thickness, 1.50)];
    let slot_raw = g.cuboid(BoxOpts {
        size: slot_size,
        round: clamped_box_round(slot_size, 0.35),
        name: Some("cobra_webbing_slot"),
    })?;
    let slot_x = scaled_pos(width, 0.31);
    let slot_l = g.place(slot_raw, Placement::shift(-slot_x, Length::ZERO, half_t))?;
    let slot_r = g.place(slot_raw, Placement::shift(slot_x, Length::ZERO, half_t))?;
    let frame_cut_l = g.difference(frame, slot_l)?;
    let frame_slotted = g.difference(frame_cut_l, slot_r)?;

    // 3. Proud central lock bar with subtle recessed release channel.
    let bar_size = [scaled_pos(width, 0.28), scaled_pos(height, 0.86), scaled_pos(thickness, 1.16)];
    let bar_raw = g.cuboid(BoxOpts {
        size: bar_size,
        round: clamped_box_round(bar_size, 0.32),
        name: Some("cobra_lock_bar"),
    })?;
    let bar = g.place(bar_raw, Placement::shift(Length::ZERO, Length::ZERO, scaled_pos(thickness, 0.56)))?;
    let body = g.chamfer_union(
        frame_slotted,
        bar,
        BlendOpts { radius: scaled_pos(thickness, 0.10), name: Some("cobra_frame_bar") },
    )?;

    let groove_size = [scaled_pos(width, 0.06), scaled_pos(height, 0.56), scaled_pos(thickness, 0.34)];
    let groove_raw = g.cuboid(BoxOpts {
        size: groove_size,
        round: clamped_box_round(groove_size, 0.25),
        name: Some("cobra_lock_groove"),
    })?;
    let groove = g.place(groove_raw, Placement::shift(Length::ZERO, Length::ZERO, scaled_pos(thickness, 1.12)))?;
    let body_grooved = g.difference(body, groove)?;

    // 4. Dual quick-release side wings protruding cleanly at top (+y) and bottom (-y).
    let wing_size = [scaled_pos(width, 0.24), scaled_pos(height, 0.22), scaled_pos(thickness, 0.68)];
    let wing_raw = g.cuboid(BoxOpts {
        size: wing_size,
        round: clamped_box_round(wing_size, 0.40),
        name: Some("cobra_release_wing"),
    })?;
    let wing_x = scaled_signed(width, 0.10);
    let wing_y = scaled_pos(height, 0.52);
    let wing_z = scaled_pos(thickness, 0.52);
    let wing_top = g.place(wing_raw, Placement::shift(wing_x, wing_y, wing_z))?;
    let wing_bot = g.place(wing_raw, Placement::shift(wing_x, -wing_y, wing_z))?;

    let blend_r = scaled_pos(thickness, 0.12);
    let with_top = g.round_union(body_grooved, wing_top, BlendOpts { radius: blend_r, name: None })?;
    let full = g.round_union(with_top, wing_bot, BlendOpts { radius: blend_r, name: Some("cobra_buckle") })?;
    let placed = g.place(full, placement)?;

    Ok(compile(&g, placed.id(), CompileOpts { alloc, .. })?)
}

/// Build a parametric Spacefarer Chest Comms / Telemetry Beacon compiled to a [`Tape`] in local
/// coordinates (back face at `z = 0`, front proud at `z = +thickness`, centered in `x, y`).
pub fn build_chest_beacon_tape<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Tape<'a>, CadGearError> {
    build_chest_beacon_tape_at(width, height, thickness, Placement::IDENTITY, alloc, scratch)
}

/// Build a parametric Spacefarer Chest Comms / Telemetry Beacon placed at `placement`.
pub fn build_chest_beacon_tape_at<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    placement: Placement,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Tape<'a>, CadGearError> {
    let mut g = Graph::new(GraphOpts { alloc: scratch, .. });

    // 1. Bevelled rectangular comms housing.
    let body_size = [width, height, thickness];
    let housing_raw = g.cuboid(BoxOpts {
        size: body_size,
        round: clamped_box_round(body_size, 0.34),
        name: Some("beacon_housing"),
    })?;
    let half_t = scaled_pos(thickness, 0.50);
    let housing = g.place(housing_raw, Placement::shift(Length::ZERO, Length::ZERO, half_t))?;

    // 2. Raised circular sensor bezel on upper front face with recessed lens cavity.
    let min_xy = if width.as_nm() < height.as_nm() { width } else { height };
    let bezel_r = scaled_pos(min_xy, 0.27);
    let bezel_h = scaled_pos(thickness, 0.38);
    let bezel_raw = g.cylinder(CylinderOpts {
        radius: bezel_r,
        height: bezel_h,
        name: Some("beacon_sensor_bezel"),
    })?;
    let sensor_y = scaled_signed(height, 0.16);
    let bezel = g.place(bezel_raw, Placement::shift(Length::ZERO, sensor_y, thickness))?;
    let with_bezel = g.chamfer_union(
        housing,
        bezel,
        BlendOpts { radius: scaled_pos(thickness, 0.10), name: None },
    )?;

    let lens_raw = g.cylinder(CylinderOpts {
        radius: scaled_pos(min_xy, 0.18),
        height: scaled_pos(thickness, 0.36),
        name: Some("beacon_lens_recess"),
    })?;
    let lens_cut = g.place(lens_raw, Placement::shift(Length::ZERO, sensor_y, scaled_pos(thickness, 1.12)))?;
    let with_lens = g.difference(with_bezel, lens_cut)?;

    // 3. Lower tactile status / push-to-talk telemetry bar.
    let ptt_size = [scaled_pos(width, 0.62), scaled_pos(height, 0.15), scaled_pos(thickness, 0.26)];
    let ptt_raw = g.cuboid(BoxOpts {
        size: ptt_size,
        round: clamped_box_round(ptt_size, 0.35),
        name: Some("beacon_ptt_bar"),
    })?;
    let ptt = g.place(
        ptt_raw,
        Placement::shift(Length::ZERO, scaled_signed(height, -0.25), scaled_pos(thickness, 0.98)),
    )?;
    let with_ptt = g.chamfer_union(
        with_lens,
        ptt,
        BlendOpts { radius: scaled_pos(thickness, 0.08), name: None },
    )?;

    // 4. Recessed antenna pocket at top edge + coaxial antenna nub protruding along +Y.
    let ant_x = scaled_signed(width, 0.24);
    let ant_z = scaled_pos(thickness, 0.50);
    let rot_y = Placement::rotation(Axis::X, Angle::degrees(-90.0));

    let recess_raw = g.cylinder(CylinderOpts {
        radius: scaled_pos(thickness, 0.34),
        height: scaled_pos(height, 0.12),
        name: Some("beacon_antenna_recess"),
    })?;
    let recess_p = Placement::shift(ant_x, scaled_signed(height, 0.49), ant_z).then_after(&rot_y);
    let recess = g.place(recess_raw, recess_p)?;
    let with_recess = g.difference(with_ptt, recess)?;

    let nub_raw = g.cylinder(CylinderOpts {
        radius: scaled_pos(thickness, 0.20),
        height: scaled_pos(height, 0.26),
        name: Some("beacon_antenna_nub"),
    })?;
    let nub_p = Placement::shift(ant_x, scaled_signed(height, 0.54), ant_z).then_after(&rot_y);
    let nub = g.place(nub_raw, nub_p)?;
    let full = g.round_union(
        with_recess,
        nub,
        BlendOpts { radius: scaled_pos(thickness, 0.08), name: Some("chest_beacon") },
    )?;
    let placed = g.place(full, placement)?;

    Ok(compile(&g, placed.id(), CompileOpts { alloc, .. })?)
}

/// Build a parametric Collar Magnetic Clasp compiled to a [`Tape`] in local coordinates
/// (back face at `z = 0`, front proud at `z = +thickness`, centered in `x, y`).
pub fn build_collar_clasp_tape<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Tape<'a>, CadGearError> {
    build_collar_clasp_tape_at(width, height, thickness, Placement::IDENTITY, alloc, scratch)
}

/// Build a parametric Collar Magnetic Clasp placed at `placement`.
pub fn build_collar_clasp_tape_at<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    placement: Placement,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Tape<'a>, CadGearError> {
    let mut g = Graph::new(GraphOpts { alloc: scratch, .. });

    // 1. Sleek low-profile chamfered clasp bar.
    let bar_size = [width, height, thickness];
    let bar_raw = g.cuboid(BoxOpts {
        size: bar_size,
        round: clamped_box_round(bar_size, 0.36),
        name: Some("collar_clasp_bar"),
    })?;
    let half_t = scaled_pos(thickness, 0.50);
    let bar = g.place(bar_raw, Placement::shift(Length::ZERO, Length::ZERO, half_t))?;

    // 2. Twin end-cap grip ridges joined with crisp chamfers.
    let cap_size = [scaled_pos(width, 0.24), scaled_pos(height, 0.88), scaled_pos(thickness, 0.42)];
    let cap_raw = g.cuboid(BoxOpts {
        size: cap_size,
        round: clamped_box_round(cap_size, 0.35),
        name: Some("collar_clasp_ridge"),
    })?;
    let cap_x = scaled_pos(width, 0.34);
    let cap_z = scaled_pos(thickness, 0.92);
    let cap_l = g.place(cap_raw, Placement::shift(-cap_x, Length::ZERO, cap_z))?;
    let cap_r = g.place(cap_raw, Placement::shift(cap_x, Length::ZERO, cap_z))?;

    let chamfer_r = scaled_pos(thickness, 0.14);
    let with_l = g.chamfer_union(bar, cap_l, BlendOpts { radius: chamfer_r, name: None })?;
    let with_caps = g.chamfer_union(with_l, cap_r, BlendOpts { radius: chamfer_r, name: None })?;

    // 3. Central magnetic parting channel + twin flush alignment dimples.
    let seam_size = [scaled_pos(width, 0.05), scaled_pos(height, 0.80), scaled_pos(thickness, 0.36)];
    let seam_raw = g.cuboid(BoxOpts {
        size: seam_size,
        round: clamped_box_round(seam_size, 0.25),
        name: Some("collar_clasp_parting"),
    })?;
    let seam_cut = g.place(seam_raw, Placement::shift(Length::ZERO, Length::ZERO, scaled_pos(thickness, 1.02)))?;
    let parted = g.difference(with_caps, seam_cut)?;

    let dimple_raw = g.cylinder(CylinderOpts {
        radius: scaled_pos(height, 0.18),
        height: scaled_pos(thickness, 0.32),
        name: Some("collar_clasp_dimple"),
    })?;
    let dimp_x = scaled_pos(width, 0.18);
    let dimp_z = scaled_pos(thickness, 1.02);
    let dimp_l = g.place(dimple_raw, Placement::shift(-dimp_x, Length::ZERO, dimp_z))?;
    let dimp_r = g.place(dimple_raw, Placement::shift(dimp_x, Length::ZERO, dimp_z))?;
    let cut_l = g.difference(parted, dimp_l)?;
    let full = g.difference(cut_l, dimp_r)?;
    let placed = g.place(full, placement)?;

    Ok(compile(&g, placed.id(), CompileOpts { alloc, .. })?)
}

/// Build a **two-tone** Spacefarer Chest Comms / Telemetry Beacon as paired `(housing_dark, accent_metal)`
/// [`Tape`] solids: matte dark polymer bevelled housing with lens recess + brushed titanium sensor
/// bezel ring, lower PTT telemetry bar, and coaxial top antenna nub!
pub fn build_chest_beacon_dual_tapes<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<(Tape<'a>, Tape<'a>), CadGearError> {
    let min_xy = if width.as_nm() < height.as_nm() { width } else { height };
    let half_t = scaled_pos(thickness, 0.50);
    let sensor_y = scaled_signed(height, 0.16);
    let ant_x = scaled_signed(width, 0.24);
    let ant_z = half_t;
    let rot_y = Placement::rotation(Axis::X, Angle::degrees(-90.0));

    // 1. Matte dark polymer bevelled housing with optical lens cavity & top antenna pocket.
    let housing_tape = {
        let mut g = Graph::new(GraphOpts { alloc: scratch, .. });
        let body_size = [width, height, thickness];
        let housing_raw = g.cuboid(BoxOpts {
            size: body_size,
            round: clamped_box_round(body_size, 0.34),
            name: Some("beacon_housing_dark"),
        })?;
        let housing = g.place(housing_raw, Placement::shift(Length::ZERO, Length::ZERO, half_t))?;

        let lens_raw = g.cylinder(CylinderOpts {
            radius: scaled_pos(min_xy, 0.17),
            height: scaled_pos(thickness, 0.45),
            name: Some("beacon_lens_cavity"),
        })?;
        let lens_cut = g.place(lens_raw, Placement::shift(Length::ZERO, sensor_y, thickness))?;
        let with_lens = g.difference(housing, lens_cut)?;

        let recess_raw = g.cylinder(CylinderOpts {
            radius: scaled_pos(thickness, 0.34),
            height: scaled_pos(height, 0.12),
            name: Some("beacon_antenna_pocket"),
        })?;
        let recess_p = Placement::shift(ant_x, scaled_signed(height, 0.49), ant_z).then_after(&rot_y);
        let recess = g.place(recess_raw, recess_p)?;
        let final_body = g.difference(with_lens, recess)?;
        compile(&g, final_body.id(), CompileOpts { alloc, .. })?
    };

    // 2. Brushed titanium accents: hollow circular sensor bezel ring + PTT status bar + coaxial antenna.
    let accent_tape = {
        let mut g = Graph::new(GraphOpts { alloc: scratch, .. });
        let bezel_outer = g.cylinder(CylinderOpts {
            radius: scaled_pos(min_xy, 0.28),
            height: scaled_pos(thickness, 0.34),
            name: Some("beacon_bezel_outer"),
        })?;
        let bezel_inner = g.cylinder(CylinderOpts {
            radius: scaled_pos(min_xy, 0.17),
            height: scaled_pos(thickness, 0.50),
            name: Some("beacon_bezel_inner"),
        })?;
        let bezel_ring_raw = g.difference(bezel_outer, bezel_inner)?;
        let bezel_ring = g.place(
            bezel_ring_raw,
            Placement::shift(Length::ZERO, sensor_y, scaled_pos(thickness, 1.08)),
        )?;

        let ptt_size = [scaled_pos(width, 0.64), scaled_pos(height, 0.14), scaled_pos(thickness, 0.28)];
        let ptt_raw = g.cuboid(BoxOpts {
            size: ptt_size,
            round: clamped_box_round(ptt_size, 0.35),
            name: Some("beacon_ptt_titanium"),
        })?;
        let ptt = g.place(
            ptt_raw,
            Placement::shift(Length::ZERO, scaled_signed(height, -0.25), scaled_pos(thickness, 1.02)),
        )?;

        let nub_raw = g.cylinder(CylinderOpts {
            radius: scaled_pos(thickness, 0.20),
            height: scaled_pos(height, 0.28),
            name: Some("beacon_antenna_titanium"),
        })?;
        let nub_p = Placement::shift(ant_x, scaled_signed(height, 0.55), ant_z).then_after(&rot_y);
        let nub = g.place(nub_raw, nub_p)?;

        let u1 = g.union(bezel_ring, ptt)?;
        let u2 = g.union(u1, nub)?;
        compile(&g, u2.id(), CompileOpts { alloc, .. })?
    };

    Ok((housing_tape, accent_tape))
}

/// Build a **two-tone** Collar Magnetic Clasp as paired `(bar_dark, ridges_metal)` [`Tape`] solids:
/// sleek matte dark anodized clasp bar + brushed titanium twin end-cap grip ridges!
pub fn build_collar_clasp_dual_tapes<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<(Tape<'a>, Tape<'a>), CadGearError> {
    let half_t = scaled_pos(thickness, 0.50);

    let bar_tape = {
        let mut g = Graph::new(GraphOpts { alloc: scratch, .. });
        let bar_size = [width, height, thickness];
        let bar_raw = g.cuboid(BoxOpts {
            size: bar_size,
            round: clamped_box_round(bar_size, 0.36),
            name: Some("collar_clasp_bar_dark"),
        })?;
        let bar = g.place(bar_raw, Placement::shift(Length::ZERO, Length::ZERO, half_t))?;
        let seam_size = [scaled_pos(width, 0.055), scaled_pos(height, 0.84), scaled_pos(thickness, 0.38)];
        let seam_raw = g.cuboid(BoxOpts {
            size: seam_size,
            round: clamped_box_round(seam_size, 0.25),
            name: Some("collar_clasp_parting"),
        })?;
        let seam_cut = g.place(seam_raw, Placement::shift(Length::ZERO, Length::ZERO, scaled_pos(thickness, 1.00)))?;
        let parted = g.difference(bar, seam_cut)?;
        compile(&g, parted.id(), CompileOpts { alloc, .. })?
    };

    let ridges_tape = {
        let mut g = Graph::new(GraphOpts { alloc: scratch, .. });
        let cap_size = [scaled_pos(width, 0.23), scaled_pos(height, 0.84), scaled_pos(thickness, 0.40)];
        let cap_raw = g.cuboid(BoxOpts {
            size: cap_size,
            round: clamped_box_round(cap_size, 0.35),
            name: Some("collar_clasp_ridge_metal"),
        })?;
        let cap_x = scaled_pos(width, 0.34);
        let cap_z = scaled_pos(thickness, 0.96);
        let cap_l = g.place(cap_raw, Placement::shift(-cap_x, Length::ZERO, cap_z))?;
        let cap_r = g.place(cap_raw, Placement::shift(cap_x, Length::ZERO, cap_z))?;
        let both = g.union(cap_l, cap_r)?;
        compile(&g, both.id(), CompileOpts { alloc, .. })?
    };

    Ok((bar_tape, ridges_tape))
}

/// Build a **two-tone** Tactical Cobra Belt Buckle as paired `(frame_dark, lockbar_metal)` [`Tape`]
/// solids: matte dark anodized slotted frame + brushed titanium central lock bar & release wings!
pub fn build_cobra_buckle_dual_tapes<'a>(
    width: Length,
    height: Length,
    thickness: Length,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<(Tape<'a>, Tape<'a>), CadGearError> {
    let half_t = scaled_pos(thickness, 0.50);

    let frame_tape = {
        let mut g = Graph::new(GraphOpts { alloc: scratch, .. });
        let frame_size = [width, height, thickness];
        let frame_raw = g.cuboid(BoxOpts {
            size: frame_size,
            round: clamped_box_round(frame_size, 0.35),
            name: Some("cobra_frame_dark"),
        })?;
        let frame = g.place(frame_raw, Placement::shift(Length::ZERO, Length::ZERO, half_t))?;
        let slot_size = [scaled_pos(width, 0.17), scaled_pos(height, 0.72), scaled_pos(thickness, 1.50)];
        let slot_raw = g.cuboid(BoxOpts {
            size: slot_size,
            round: clamped_box_round(slot_size, 0.35),
            name: Some("cobra_slot"),
        })?;
        let slot_x = scaled_pos(width, 0.31);
        let slot_l = g.place(slot_raw, Placement::shift(-slot_x, Length::ZERO, half_t))?;
        let slot_r = g.place(slot_raw, Placement::shift(slot_x, Length::ZERO, half_t))?;
        let cut_l = g.difference(frame, slot_l)?;
        let slotted = g.difference(cut_l, slot_r)?;
        compile(&g, slotted.id(), CompileOpts { alloc, .. })?
    };

    let lock_tape = {
        let mut g = Graph::new(GraphOpts { alloc: scratch, .. });
        let bar_size = [scaled_pos(width, 0.28), scaled_pos(height, 0.86), scaled_pos(thickness, 1.16)];
        let bar_raw = g.cuboid(BoxOpts {
            size: bar_size,
            round: clamped_box_round(bar_size, 0.32),
            name: Some("cobra_lock_metal"),
        })?;
        let bar = g.place(bar_raw, Placement::shift(Length::ZERO, Length::ZERO, scaled_pos(thickness, 0.58)))?;
        let wing_size = [scaled_pos(width, 0.24), scaled_pos(height, 0.22), scaled_pos(thickness, 0.68)];
        let wing_raw = g.cuboid(BoxOpts {
            size: wing_size,
            round: clamped_box_round(wing_size, 0.40),
            name: Some("cobra_wing_metal"),
        })?;
        let wing_x = scaled_signed(width, 0.10);
        let wing_y = scaled_pos(height, 0.52);
        let wing_z = scaled_pos(thickness, 0.54);
        let wing_top = g.place(wing_raw, Placement::shift(wing_x, wing_y, wing_z))?;
        let wing_bot = g.place(wing_raw, Placement::shift(wing_x, -wing_y, wing_z))?;
        let u1 = g.union(bar, wing_top)?;
        let u2 = g.union(u1, wing_bot)?;
        compile(&g, u2.id(), CompileOpts { alloc, .. })?
    };

    Ok((frame_tape, lock_tape))
}

/// Mesh `tape` via `blaze-cad-mesh::mesh` at `tolerance`, transform vertices and normals into the
/// draped cloth tangent frame `(tu, tv, nrm)` at `origin`, tag every vertex with
/// `rigid_anchor = Some(anchor)` and `tri_layer = 3`, and append directly into `garment`.
///
/// Returns the number of triangles appended.
#[allow(clippy::too_many_arguments)]
pub fn append_cad_solid_to_garment(
    tape: &Tape<'_>,
    fabric_idx: u16,
    origin: Vec3,
    tu: Vec3,
    tv: Vec3,
    nrm: Vec3,
    anchor: Vec3,
    tolerance: Length,
    garment: &mut GarmentMesh<'_>,
    scratch: Alloc<'_>,
) -> Result<u32, CadGearError> {
    let min_feat = Length::nm((tolerance.as_nm().saturating_mul(4)).max(1_000_000));
    let grid = auto_grid(&tape.aabb(), tolerance, Some(min_feat));
    let meshed = contour_mesh(
        tape,
        MeshOpts {
            alloc: scratch,
            scratch,
            tolerance,
            piece: grid.piece,
            feature: grid.feature,
            ..
        },
    )?;

    let view = meshed.mesh.view();
    let src_pos = view.positions();
    let src_nrm = view.normals();
    let src_tris = view.triangles();
    if src_pos.is_empty() || src_tris.is_empty() {
        return Ok(0);
    }

    let tu_n = tu.normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
    let tv_n = tv.normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0));
    let nrm_n = nrm.normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
    let flip_winding = tu_n.cross(tv_n).dot(nrm_n) < 0.0;

    // Local XY bounds for deterministic distortion-free planar UV mapping.
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in src_pos.iter() {
        let x = f64::from(p[0]);
        let y = f64::from(p[1]);
        if x < min_x { min_x = x; }
        if x > max_x { max_x = x; }
        if y < min_y { min_y = y; }
        if y > max_y { max_y = y; }
    }
    let span_x = (max_x - min_x).max(1e-6);
    let span_y = (max_y - min_y).max(1e-6);

    let cad_panel_id = garment.register_panel("cad_hardware", PanelKind::Hardware, 3, fabric_idx, None)?;

    let base_v = garment.positions.len() as u32;
    for (p, n) in src_pos.iter().zip(src_nrm.iter()) {
        let lx = f64::from(p[0]);
        let ly = f64::from(p[1]);
        let lz = f64::from(p[2]);
        let world_p = origin + tu_n * lx + tv_n * ly + nrm_n * lz;
        let world_n = (tu_n * f64::from(n[0]) + tv_n * f64::from(n[1]) + nrm_n * f64::from(n[2]))
            .normalized()
            .unwrap_or(nrm_n);
        let uv = Vec2::new((lx - min_x) / span_x, (ly - min_y) / span_y);

        let vi = garment.positions.len() as u32;
        push(&mut garment.positions, world_p)?;
        push(&mut garment.normals, world_n)?;
        push(&mut garment.uvs, uv)?;
        push(&mut garment.rest_pos_m, Vec2::new(lx, ly))?;
        push(&mut garment.colors, [70, 235, 0, 0])?;
        push(&mut garment.rigid_anchor, Some(anchor))?;
        push(&mut garment.weld_rep, vi)?;
    }

    for &[a, b, c] in src_tris.iter() {
        let tri = if flip_winding {
            [base_v + a, base_v + c, base_v + b]
        } else {
            [base_v + a, base_v + b, base_v + c]
        };
        push(&mut garment.triangles, tri)?;
        push(&mut garment.tri_fabric, fabric_idx)?;
        push(&mut garment.tri_layer, 3)?;
        push(&mut garment.tri_panel, cad_panel_id)?;
    }

    Ok(src_tris.len() as u32)
}
