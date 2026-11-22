//! The library's promises, each an oracle over drawn models with the planted bug its control
//! must catch:
//!
//! - PLACEMENT IS NOT GEOMETRY: two parts made from one solid placed two ways have equal tape
//!   digests, and the assembly is, point for point, the placed solid. Planted `PlacementKept`
//!   (the placement left in the solid), caught as `KeyMoved`.
//! - ROUNDED EDGES ARE WHERE THEY SHOULD BE: a cube rounded by `r` has its face where the cube's
//!   was and its corner `r` from the eroded cube's corner, within two grid steps. Planted
//!   `GrowShort`, caught as `RoundWrong`.
//! - MASS: a meshed sphere's volume within its stated bound of `4/3 pi R^3`, its mass the density
//!   times it, its centre at the origin, its inertia `2/5 m R^2` on the diagonal. Planted
//!   `InertiaUnscaled`, caught as `MassWrong`.
//! - MIN WALL: a plate thinner than the wall (0.4 to 0.8 of it) is found, with a thickness within
//!   a tolerance of the plate's; one thicker (1.2 to 2 times) passes, with or without a hole
//!   through it; a solid cube passes at any wall a millimetre or more under its side (its 90-degree
//!   edges and corners are not thin). Planted `TangentBall` (thin where a ball of the wall's
//!   diameter does not fit, which flags every convex edge), caught as `WallWrong { thin: false }`.
//!   And in every case a ring frame (a thick annulus of thickness `T` cut by a taller cylinder),
//!   alone or repeated along its axis: found at a wall over `T`, a pass under it, never
//!   unsettled, and at most `POINTS_PER_SAMPLE` field evaluations a sample on any part. Planted
//!   `NoBracket` (Newton's steps alone, the bracketing removed), caught as `WallOpen`.
//! - NO INTERFERENCE: two spheres of known gap: a pass only when the gap is at least the clearance,
//!   a finding only when it is under it, unknown only on a starved budget or a gap at the
//!   clearance. Planted `UnknownPasses`, caught as `InterferenceWrong`.
//! - FITS WITHIN: a box in a box: a pass only when it is inside with room, a finding only when it
//!   sticks out. Planted `Reversed`, caught as `FitWrong { inside: true }`.

use core::f64::consts::PI;
use core::fmt;
use std::{format, string::String};

use blaze_cad_field::{
    Axis, BoxOpts, CompileOpts, ConeOpts, CylinderOpts, Detail, Graph, GraphOpts, GyroidOpts, Inside, Name, Placement, RepeatOpts,
    Scratch, Solid, SphereOpts, compile, eval_point,
};
use blaze_cad_picture::{PictureOpts, Projection, View};
use blaze_math::Vec3;
use blaze_own::Alloc;
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};
use blaze_units::{Angle, Density, Length};

use crate::*;

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn harness(e: impl fmt::Debug) -> Broken<CadBroken> {
    Broken::Harness(format!("{e:?}"))
}

#[derive(Clone, PartialEq, Debug)]
enum CadBroken {
    /// Two placements of one solid gave parts of different digests.
    KeyMoved { a: String, b: String },
    /// The assembly differs from the placed solid at a point.
    PlacedWrong { at: [f64; 3], got: f64, want: f64 },
    /// A rounded cube's field is off at a probe.
    RoundWrong { probe: &'static str, got: f64, want: f64, bound: f64 },
    /// A mass property is off.
    MassWrong { what: &'static str, got: f64, want: f64, bound: f64 },
    /// A wall check's verdict disagrees with the plate's thickness.
    WallWrong { thin: bool, verdict: String },
    /// A wall check left places unsettled on a part where every place settles.
    WallOpen { verdict: String },
    /// A wall check made more field evaluations than its bound for its samples.
    WallCost { points: u64, samples: u64, bound: u64 },
    /// An interference verdict disagrees with the true gap.
    InterferenceWrong { gap: f64, clearance: f64, verdict: String },
    /// A fit verdict disagrees with the boxes.
    FitWrong { inside: bool, verdict: String },
    /// An overhang DFM verdict disagrees with the geometry.
    OverhangWrong { should_fail: bool, verdict: String },
    /// A draft-angle DFM verdict disagrees with the geometry.
    DraftWrong { should_fail: bool, verdict: String },
}

impl fmt::Display for CadBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    impl PartPlant breaks CadBroken {
        None,
        PlacementKept => CadBroken::KeyMoved { .. },
    }
}

blaze_sim::plants! {
    impl RoundPlant breaks CadBroken {
        None,
        GrowShort => CadBroken::RoundWrong { .. },
    }
}

blaze_sim::plants! {
    impl MassPlant breaks CadBroken {
        None,
        InertiaUnscaled => CadBroken::MassWrong { .. },
    }
}

blaze_sim::plants! {
    impl WallPlant breaks CadBroken {
        None,
        TangentBall => CadBroken::WallWrong { thin: false, .. },
        NoBracket => CadBroken::WallOpen { .. },
    }
}

blaze_sim::plants! {
    impl InterferencePlant breaks CadBroken {
        None,
        UnknownPasses => CadBroken::InterferenceWrong { .. },
    }
}

blaze_sim::plants! {
    impl FitPlant breaks CadBroken {
        None,
        Reversed => CadBroken::FitWrong { inside: true, .. },
    }
}

blaze_sim::plants! {
    impl OverhangPlant breaks CadBroken {
        None,
        FlippedBuildDir => CadBroken::OverhangWrong { .. },
    }
}

blaze_sim::plants! {
    impl DraftPlant breaks CadBroken {
        None,
        IgnoreUndercutSign => CadBroken::DraftWrong { should_fail: true, .. },
    }
}

fn mm(x: i64) -> Length {
    Length::mm(x as f64)
}

/// A shift of whole millimetres after a quarter turn about `z`.
fn placement(tape: &Tape) -> Placement {
    let turns = tape.int_in(0..=3i64, "quarter turns") as f64;
    let mut at = [0i64; 3];
    for c in &mut at {
        *c = tape.int_in(-100..=100i64, "shift mm");
    }
    Placement::shift(mm(at[0]), mm(at[1]), mm(at[2])).then_after(&Placement::rotation(Axis::Z, Angle::degrees(90.0 * turns)))
}

fn part_case(tape: &Tape, plant: impl Planted<PartPlant> + Copy) -> Result<(), Broken<CadBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let size = tape.int_in(5..=40i64, "size mm");
    let blob = if tape.coin("sphere") {
        g.sphere(SphereOpts { radius: mm(size), .. })
    } else {
        g.cuboid(BoxOpts { size: [mm(size), mm(size + 3), mm(size + 7)], .. })
    }
    .map_err(harness)?;
    let p1 = placement(tape);
    // A second placement that differs from the first by at least a millimetre.
    let p2 = Placement::shift(mm(tape.int_in(1..=50i64, "moved mm")), Length::ZERO, Length::ZERO).then_after(&p1);
    let s1 = g.place(blob, p1).map_err(harness)?;
    let s2 = g.place(blob, p2).map_err(harness)?;
    let a = Part::new_planted(&mut g, PartOpts { solid: s1, name: "a" }, plant).map_err(harness)?;
    let b = Part::new_planted(&mut g, PartOpts { solid: s2, name: "b" }, plant).map_err(harness)?;
    let da = part_tape(&g, &a, Detail::Full, heap()).map_err(harness)?.digest();
    let db = part_tape(&g, &b, Detail::Full, heap()).map_err(harness)?.digest();
    if da != db {
        return Err(Broken::Oracle(CadBroken::KeyMoved { a: format!("{da:02x?}"), b: format!("{db:02x?}") }));
    }
    let whole = assemble(&mut g, &[a.weaken()]).map_err(harness)?;
    let tw = compile(&g, whole.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    let ts = compile(&g, s1.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    let mut sc = Scratch::new(heap());
    for _ in 0..16 {
        let mut at = [0.0f64; 3];
        for c in &mut at {
            *c = tape.int_in(-200..=200i64, "probe mm") as f64 / 1000.0;
        }
        let p = Vec3::new(at[0], at[1], at[2]);
        let (got, want) = (eval_point(&tw, p, &mut sc), eval_point(&ts, p, &mut sc));
        if !((got - want).abs() <= 1e-9) {
            return Err(Broken::Oracle(CadBroken::PlacedWrong { at, got, want }));
        }
    }
    Ok(())
}

#[test]
fn placement_is_not_geometry() {
    check(Config::cases(80), |tape| part_case(tape, NoPlant));
}

#[test]
fn part_plants_are_caught() {
    controls(Config::cases(40), |tape, plant: PartPlant| part_case(tape, plant));
}

fn round_case(tape: &Tape, plant: impl Planted<RoundPlant> + Copy) -> Result<(), Broken<CadBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    // Small enough for a debug run in a second a case. The bound is two grid steps: a face's
    // distance is exact at the grid (its foot is a crossing) and a corner's is off by under a
    // step (the nearest crossing is within a step of the corner); the plant moves both by
    // `r / 2`, at least two and a half steps.
    let side = tape.int_in(20..=26i64, "side mm") as f64;
    let r = tape.int_in(5..=7i64, "radius mm") as f64;
    let step = 1.0;
    let cube = g.cuboid(BoxOpts { size: [Length::mm(side); 3], .. }).map_err(harness)?;
    let o = RoundEdgesOpts { radius: Length::mm(r), step: Length::mm(step), scratch: heap(), .. };
    let rounded = round_edges_planted(&mut g, cube, o, plant).map_err(harness)?;
    let t = compile(&g, rounded.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    let mut sc = Scratch::new(heap());
    let bound = 2.0 * step / 1000.0;
    let h = side / 2.0;
    let sign = |tape: &Tape, label| if tape.coin(label) { 1.0 } else { -1.0 };

    // A face centre: on the surface.
    let axis = tape.index(3, "face axis");
    let mut f = [0.0f64; 3];
    f[axis] = sign(tape, "face side") * h / 1000.0;
    let got = eval_point(&t, Vec3::new(f[0], f[1], f[2]), &mut sc);
    if !(got.abs() <= bound) {
        return Err(Broken::Oracle(CadBroken::RoundWrong { probe: "face centre", got, want: 0.0, bound }));
    }

    // Along a corner's diagonal, two millimetres past the rounded surface.
    let s = [sign(tape, "corner x"), sign(tape, "corner y"), sign(tape, "corner z")];
    let out = (r + 2.0) / 3.0f64.sqrt();
    let c: [f64; 3] = core::array::from_fn(|i| s[i] * (h - r + out) / 1000.0);
    let got = eval_point(&t, Vec3::new(c[0], c[1], c[2]), &mut sc);
    let want = 0.002;
    if !((got - want).abs() <= bound) {
        return Err(Broken::Oracle(CadBroken::RoundWrong { probe: "corner diagonal", got, want, bound }));
    }
    Ok(())
}

#[test]
fn rounded_edges_are_placed_right() {
    check(Config::cases(6), |tape| round_case(tape, NoPlant));
}

#[test]
fn round_plants_are_caught() {
    controls(Config::cases(6), |tape, plant: RoundPlant| round_case(tape, plant));
}

fn mass_case(tape: &Tape, plant: impl Planted<MassPlant> + Copy) -> Result<(), Broken<CadBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let r_mm = tape.int_in(20..=40i64, "radius mm") as f64;
    let rho = tape.int_in(500..=8000i64, "density kg/m3") as f64;
    let ball = g.sphere(SphereOpts { radius: Length::mm(r_mm), .. }).map_err(harness)?;
    let part = Part::new(&mut g, PartOpts { solid: ball, name: "ball" }).map_err(harness)?;
    // A fortieth of the radius (half a millimetre to one): a debug run in a second a case; every
    // bound below is in tolerances.
    let tol = r_mm / 40.0;
    let meshed = mesh_part(&g, &part, MeshPartOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(tol) }, None).map_err(harness)?;
    let m = mass_properties_planted(&meshed.verified(), Density::kg_per_m3(rho), plant);

    let r = r_mm / 1000.0;
    let v_true = 4.0 / 3.0 * PI * r * r * r;
    // Every vertex within two tolerances of the sphere, so every point of the mesh within that
    // and a chord's sag (under a tolerance at these sizes) of it.
    let v_bound = 4.0 * PI * r * r * (3.0 * tol / 1000.0);
    let v = m.volume.as_m3();
    if !((v - v_true).abs() <= v_bound) {
        return Err(Broken::Oracle(CadBroken::MassWrong { what: "volume", got: v, want: v_true, bound: v_bound }));
    }
    let mass = m.mass.as_kg();
    if !((mass - rho * v).abs() <= 1e-9 * rho * v) {
        return Err(Broken::Oracle(CadBroken::MassWrong { what: "mass", got: mass, want: rho * v, bound: 1e-9 * rho * v }));
    }
    for (i, c) in m.centre.iter().enumerate() {
        let got = c.as_m();
        let bound = 2.0 * tol / 1000.0;
        if !(got.abs() <= bound) {
            let what = ["centre x", "centre y", "centre z"][i];
            return Err(Broken::Oracle(CadBroken::MassWrong { what, got, want: 0.0, bound }));
        }
    }
    let i_true = 0.4 * rho * v_true * r * r;
    for i in 0..3 {
        let got = m.inertia[i][i];
        if !((got - i_true).abs() <= 0.2 * i_true) {
            return Err(Broken::Oracle(CadBroken::MassWrong { what: "inertia diagonal", got, want: i_true, bound: 0.2 * i_true }));
        }
        for j in 0..3 {
            if j != i && !(m.inertia[i][j].abs() <= 0.05 * i_true) {
                let got = m.inertia[i][j];
                return Err(Broken::Oracle(CadBroken::MassWrong { what: "inertia off the diagonal", got, want: 0.0, bound: 0.05 * i_true }));
            }
        }
    }
    Ok(())
}

#[test]
fn mass_of_a_sphere() {
    check(Config::cases(6), |tape| mass_case(tape, NoPlant));
}

#[test]
fn mass_plants_are_caught() {
    controls(Config::cases(6), |tape, plant: MassPlant| mass_case(tape, plant));
}

/// The most field evaluations a sample may average (module documentation of `check`, COST: about
/// three on a thick face, a bracket's corners and bisection, a ray marched along an edge's face
/// now and then); the planted `NoBracket` spends the whole step budget (4096) on each place it
/// leaves open.
const POINTS_PER_SAMPLE: u64 = 64;

fn wall_case(tape: &Tape, plant: impl Planted<WallPlant> + Copy) -> Result<(), Broken<CadBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    // Sizes keep a debug run to about a second a case: the surface is sampled every half wall.
    let first: (Solid<Inside>, f64, bool, f64) = if tape.coin("cube") {
        // A solid cube: no wall under its side, edges and corners included.
        let side = tape.int_in(10..=30i64, "cube side mm") as f64;
        let w = tape.int_in(3..=(side as i64 - 1), "wall mm") as f64;
        let cube = g.cuboid(BoxOpts { size: [Length::mm(side); 3], .. }).map_err(harness)?;
        (cube.weaken::<Inside>(), w, false, side)
    } else {
        let w = tape.int_in(2..=6i64, "wall mm") as f64;
        let thin = tape.coin("thin");
        let tenths = if thin { tape.int_in(4..=8i64, "thickness / wall, tenths") } else { tape.int_in(12..=20i64, "thickness / wall, tenths") };
        let t = w * tenths as f64 / 10.0;
        let plate = g.cuboid(BoxOpts { size: [Length::mm(30.0), Length::mm(30.0), Length::mm(t)], .. }).map_err(harness)?;
        let solid: Solid<Inside> = if tape.coin("hole") {
            let hole = g.cylinder(CylinderOpts { radius: Length::mm(6.0), height: Length::mm(t + 10.0), .. }).map_err(harness)?;
            g.difference(plate, hole).map_err(harness)?
        } else {
            plate.weaken::<Inside>()
        };
        (solid, w, thin, t)
    };
    // A ring frame (a thick annulus of radial thickness `t`, axial `2t`), cut as the hull's
    // frames are: by an inner cylinder taller than the ring by a margin under a sampling cell,
    // so a sampling cell in the hole can straddle both caps' planes, where Newton's steps on the
    // difference's bound go back and forth (the frames' unsettled places); one copy, or two
    // along its axis (the repeat's cells across the plane between them are loose).
    let w = tape.int_in(2..=4i64, "ring wall mm") as f64;
    let thin = tape.coin("ring thin");
    let tenths = if thin { tape.int_in(4..=8i64, "ring thickness / wall, tenths") } else { tape.int_in(12..=20i64, "ring thickness / wall, tenths") };
    let t = w * tenths as f64 / 10.0;
    let r = tape.int_in(4..=8i64, "ring inner radius mm") as f64;
    let margin = w / 2.0 * tape.int_in(1..=4i64, "cutter margin / sampling cell, tenths") as f64 / 10.0;
    let outer = g.cylinder(CylinderOpts { radius: Length::mm(r + t), height: Length::mm(2.0 * t), .. }).map_err(harness)?;
    let inner = g.cylinder(CylinderOpts { radius: Length::mm(r), height: Length::mm(2.0 * t + 2.0 * margin), .. }).map_err(harness)?;
    let ring = g.difference(outer, inner).map_err(harness)?;
    let copies = tape.int_in(1..=2u32, "ring copies");
    let gap = w * tape.int_in(1..=3i64, "ring gap / wall") as f64;
    let step = Length::mm(2.0 * t + gap);
    let rings: Solid<Inside> = g.linear_repeat(ring, RepeatOpts { axis: Axis::Z, step, count: copies, .. }).map_err(harness)?;

    for (solid, w, thin, least) in [first, (rings, w, thin, t)] {
        let placed = g.place(solid, placement(tape)).map_err(harness)?;
        let part = Part::new(&mut g, PartOpts { solid: placed, name: "wall" }).map_err(harness)?;
        let got = min_wall_planted(&g, &part, MinWallOpts { wall: Length::mm(w), scratch: heap(), .. }, plant).map_err(harness)?;
        match (thin, &got.verdict) {
            // A found wall is at least the thinnest the part has, and under the wall.
            (true, Verdict::Found(f)) if f.thickness.as_m() * 1000.0 >= least - 0.01 && f.thickness.as_m() * 1000.0 < w => {}
            (false, Verdict::Pass) => {}
            (_, v @ Verdict::Unknown(_)) => return Err(Broken::Oracle(CadBroken::WallOpen { verdict: format!("{v:?}") })),
            (_, v) => return Err(Broken::Oracle(CadBroken::WallWrong { thin, verdict: format!("{v:?}") })),
        }
        let bound = POINTS_PER_SAMPLE * got.samples.max(1);
        if got.points > bound {
            return Err(Broken::Oracle(CadBroken::WallCost { points: got.points, samples: got.samples, bound }));
        }
    }
    Ok(())
}

#[test]
fn thin_walls_are_found_and_thick_ones_pass() {
    check(Config::cases(16), |tape| wall_case(tape, NoPlant));
}

#[test]
fn wall_plants_are_caught() {
    controls(Config::cases(16), |tape, plant: WallPlant| wall_case(tape, plant));
}

fn interference_case(tape: &Tape, plant: impl Planted<InterferencePlant> + Copy) -> Result<(), Broken<CadBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let r1 = tape.int_in(5..=30i64, "radius a mm");
    let r2 = tape.int_in(5..=30i64, "radius b mm");
    let d = tape.int_in(0..=100i64, "centres apart mm");
    let c = tape.int_in(1..=20i64, "clearance mm") as f64;
    let axis = tape.index(3, "axis");
    let starved = tape.coin("starved budget");
    // Starved: a handful of boxes, so the gap is often still open (`Unknown`) with the parts
    // nearer than the clearance, the case the planted `UnknownPasses` turns into a false pass
    // (from 8 boxes up two spheres' gap settled in every one of 120 cases: the control missed).
    let most_boxes = if starved { tape.int_in(1..=12u32, "boxes") } else { 1 << 20 };
    let a = g.sphere(SphereOpts { radius: mm(r1), .. }).map_err(harness)?;
    let b = g.sphere(SphereOpts { radius: mm(r2), .. }).map_err(harness)?;
    let mut at = [Length::ZERO; 3];
    at[axis] = mm(d);
    let b = g.place(b, Placement::shift(at[0], at[1], at[2])).map_err(harness)?;
    let shape = CompileOpts { alloc: heap(), detail: Detail::Shape, .. };
    let ta = compile(&g, a.id(), shape).map_err(harness)?;
    let tb = compile(&g, b.id(), shape).map_err(harness)?;
    let o = InterferenceOpts { clearance: Length::mm(c), scratch: heap(), most_boxes, .. };
    let got = no_interference_planted(&ta, &tb, o, plant).map_err(harness)?;
    let gap = ((d - r1 - r2) as f64).max(0.0);
    let margin = 0.05;
    let right = match got.verdict {
        Verdict::Pass => gap >= c - margin,
        Verdict::Found(_) => gap <= c + margin,
        Verdict::Unknown(_) => starved || (gap - c).abs() <= margin,
    };
    if right {
        Ok(())
    } else {
        Err(Broken::Oracle(CadBroken::InterferenceWrong { gap, clearance: c, verdict: format!("{:?}", got.verdict) }))
    }
}

#[test]
fn interference_is_never_a_false_pass() {
    check(Config::cases(80), |tape| interference_case(tape, NoPlant));
}

#[test]
fn interference_plants_are_caught() {
    controls(Config::cases(120), |tape, plant: InterferencePlant| interference_case(tape, plant));
}

fn fit_case(tape: &Tape, plant: impl Planted<FitPlant> + Copy) -> Result<(), Broken<CadBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let mut size = [0i64; 3];
    let mut at = [0i64; 3];
    for i in 0..3 {
        size[i] = tape.int_in(10..=40i64, "size mm");
        at[i] = tape.int_in(-60..=60i64, "shift mm");
    }
    // Room on the tightest side: the envelope is a 100 mm cube about the origin.
    let room = (0..3).map(|i| 50.0 - (at[i].abs() as f64 + size[i] as f64 / 2.0)).fold(f64::INFINITY, f64::min);
    let (inside, outside) = (room >= 2.0, room <= -2.0);
    let part = g.cuboid(BoxOpts { size: [mm(size[0]), mm(size[1]), mm(size[2])], .. }).map_err(harness)?;
    let part = g.place(part, Placement::shift(mm(at[0]), mm(at[1]), mm(at[2]))).map_err(harness)?;
    let envelope = g.cuboid(BoxOpts { size: [Length::mm(100.0); 3], .. }).map_err(harness)?;
    let got = fits_within_planted(&mut g, part.weaken(), envelope.weaken(), FitOpts { scratch: heap(), .. }, plant).map_err(harness)?;
    let right = match got.verdict {
        Verdict::Pass => !outside,
        Verdict::Found(_) => !inside,
        Verdict::Unknown(_) => !inside && !outside,
    };
    if right { Ok(()) } else { Err(Broken::Oracle(CadBroken::FitWrong { inside, verdict: format!("{:?}", got.verdict) })) }
}

#[test]
fn fits_within_agrees_with_the_boxes() {
    check(Config::cases(60), |tape| fit_case(tape, NoPlant));
}

#[test]
fn fit_plants_are_caught() {
    controls(Config::cases(60), |tape, plant: FitPlant| fit_case(tape, plant));
}

/// The model builder refuses a second part of one name, and a moved part keeps its tape.
#[test]
fn model_names_are_unique_and_moves_keep_keys() {
    let mut model = Model::new(ModelOpts { alloc: heap(), .. });
    let ball = model.graph().sphere(SphereOpts { radius: Length::mm(10.0), .. }).expect("a sphere");
    let (id, part) = model.add(PartOpts { solid: ball, name: "ball" }).expect("a part");
    let again = model.add(PartOpts { solid: ball, name: "ball" });
    assert!(matches!(again, Err(ModelError::DuplicateName { .. })), "{again:?}");
    let before = part_tape(model.graph_ref(), &part, Detail::Full, heap()).expect("a tape").digest();
    model.move_part(id, Placement::shift(Length::mm(5.0), Length::ZERO, Length::ZERO)).expect("a move");
    let moved = *model.part(id).expect("the part");
    let after = part_tape(model.graph_ref(), &moved, Detail::Full, heap()).expect("a tape").digest();
    assert_eq!(before, after);
}

// ---- SOTA E2E export tests: GPU-ready GLB, Binary STL, 2D Vector SVG ---------------------

fn glb_json_chunk(glb: &[u8]) -> &str {
    assert!(glb.len() >= 20, "glb too short");
    assert_eq!(&glb[0..4], b"glTF");
    assert_eq!(u32::from_le_bytes(glb[4..8].try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize, glb.len());
    let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
    assert_eq!(&glb[16..20], b"JSON");
    core::str::from_utf8(&glb[20..20 + json_len]).expect("valid utf8 gltf json")
}

#[test]
fn export_glb_gpu_optimized_lods_colliders_materials_and_hierarchy() {
    let mut model = Model::new(ModelOpts { alloc: heap(), .. });
    let base_solid = model.graph().cuboid(BoxOpts { size: mm3(40.0, 40.0, 20.0), .. }).expect("base");
    let cap_solid = model.graph().sphere(SphereOpts { radius: Length::mm(18.0), .. }).expect("cap");
    let cap_solid = model
        .graph()
        .place(cap_solid, Placement::shift(Length::ZERO, Length::ZERO, Length::mm(22.0)))
        .expect("placed cap");

    let (base_id, _) = model.add(PartOpts { solid: base_solid, name: "base" }).expect("add base");
    let (cap_id, _) = model.add(PartOpts { solid: cap_solid, name: "cap" }).expect("add cap");
    model.set_material(cap_id, MaterialRole(1)).expect("set material");
    model.set_parent(cap_id, Some(base_id)).expect("set parent");

    // Forward parent reference must be rejected to keep the scene hierarchy a forest.
    assert!(matches!(model.set_parent(base_id, Some(cap_id)), Err(ModelError::Argument { .. })));

    const COPPER: Material<'static> = Material {
        name: "copper",
        base_colour: [0.85, 0.45, 0.25, 1.0],
        metallic: 1.0,
        roughness: 0.3,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    };
    let materials = [STEEL, COPPER];
    let steel = Density::kg_per_m3(7850.0);

    let opts = ExportOpts {
        alloc: heap(),
        scratch: heap(),
        tolerance: Length::mm(1.0),
        density: Some(steel),
        materials: &materials,
        optimize_gpu: true,
        vertex_cache_size: 32,
        lods: Some(ExportLodOpts { ratio: 0.5, fewest_triangles: 12, .. }),
        colliders: Some(ColliderMode::ConvexHull),
        ..
    };

    let out1 = export_glb(model.graph_ref(), model.parts(), opts, None).expect("export glb");
    let out2 = export_glb(model.graph_ref(), model.parts(), opts, None).expect("export glb deterministic");
    assert_eq!(out1.glb.as_slice(), out2.glb.as_slice(), "GLB export must be byte-identical");
    assert_eq!(out1.counts.len(), 2);
    assert!(out1.triangles() > 0);

    let json = glb_json_chunk(&out1.glb);
    assert!(json.contains("\"steel\""), "missing steel material: {json}");
    assert!(json.contains("\"copper\""), "missing copper material: {json}");
    assert!(json.contains("\"children\":[1]"), "missing parent->child hierarchy: {json}");
    assert!(json.contains("\"material\":1"), "missing role 1 material binding: {json}");
    assert!(json.contains("\"lod\":["), "missing LOD chain metadata: {json}");
    assert!(json.contains("\"collider\":{\"kind\":\"hulls\""), "missing convex hull collider: {json}");
    assert!(json.contains("\"mass\":"), "missing mass facts: {json}");

    // Also verify convex decomposition collider mode on a concave part (L-shape).
    let mut m2 = Model::new(ModelOpts { alloc: heap(), .. });
    let arm_a = m2.graph().cuboid(BoxOpts { size: mm3(40.0, 12.0, 12.0), .. }).expect("arm a");
    let arm_b = m2.graph().cuboid(BoxOpts { size: mm3(12.0, 40.0, 12.0), .. }).expect("arm b");
    let arm_b = m2
        .graph()
        .place(arm_b, Placement::shift(Length::mm(-14.0), Length::mm(14.0), Length::ZERO))
        .expect("placed arm b");
    let bracket = m2.graph().union(arm_a, arm_b).expect("L bracket");
    m2.add(PartOpts { solid: bracket, name: "bracket" }).expect("add bracket");

    let decomp_out = export_glb(
        m2.graph_ref(),
        m2.parts(),
        ExportOpts {
            alloc: heap(),
            scratch: heap(),
            tolerance: Length::mm(1.5),
            colliders: Some(ColliderMode::Decompose { concavity: 0.05, rays: 16 }),
            ..
        },
        None,
    )
    .expect("export decomposed collider glb");
    let decomp_json = glb_json_chunk(&decomp_out.glb);
    assert!(decomp_json.contains("\"collider\":{\"kind\":\"hulls\""), "missing decomposed collider: {decomp_json}");

    // Verify Tipsify vertex-cache optimization directly reduces or keeps low ACMR on a meshed sphere.
    let cap_part = *model.part(cap_id).expect("cap part");
    let meshed = mesh_part(model.graph_ref(), &cap_part, MeshPartOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(0.8) }, None)
        .expect("mesh sphere");
    let raw_acmr = blaze_cad_mesh::acmr(&meshed.mesh.view(), blaze_cad_mesh::AcmrOpts { cache_size: 32, scratch: heap() }).expect("raw acmr");
    let cached = blaze_cad_mesh::optimize_vertex_cache(
        &meshed.mesh.view(),
        blaze_cad_mesh::CacheOpts { cache_size: 32, alloc: heap(), scratch: heap() },
    )
    .expect("tipsify");
    let fetched = blaze_cad_mesh::optimize_vertex_fetch(&cached.view(), blaze_cad_mesh::FetchOpts { alloc: heap(), scratch: heap() })
        .expect("fetch");
    let opt_acmr = blaze_cad_mesh::acmr(&fetched.view(), blaze_cad_mesh::AcmrOpts { cache_size: 32, scratch: heap() }).expect("opt acmr");
    assert!(opt_acmr <= raw_acmr + 1e-6, "GPU vertex-cache optimization regressed ACMR: raw={raw_acmr}, opt={opt_acmr}");
    assert!(opt_acmr < 1.1, "Tipsify ACMR should be near optimal (< 1.1), got {opt_acmr}");
}

#[test]
fn export_stl_binary_valid_units_and_deterministic() {
    let mut model = Model::new(ModelOpts { alloc: heap(), .. });
    let ball = model.graph().sphere(SphereOpts { radius: Length::mm(20.0), .. }).expect("sphere");
    let (_, part) = model.add(PartOpts { solid: ball, name: "ball" }).expect("part");

    let mm_out1 = export_stl(
        model.graph_ref(),
        &part,
        ExportStlOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.0), unit: StlUnit::Millimetres, .. },
        None,
    )
    .expect("stl mm");
    let mm_out2 = export_stl(
        model.graph_ref(),
        &part,
        ExportStlOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.0), unit: StlUnit::Millimetres, .. },
        None,
    )
    .expect("stl mm deterministic");
    assert_eq!(mm_out1.stl.as_slice(), mm_out2.stl.as_slice(), "STL export must be byte-identical");

    let bytes = &mm_out1.stl;
    assert!(bytes.starts_with(b"blaze-cad binary stl (mm): ball"), "unexpected header");
    assert!(!bytes.starts_with(b"solid "), "binary STL header must never begin with 'solid '");
    let n_tris = u32::from_le_bytes(bytes[80..84].try_into().unwrap());
    assert_eq!(n_tris, mm_out1.count.written);
    assert!(n_tris >= 8);
    assert_eq!(bytes.len(), 84 + (n_tris as usize) * 50);

    // Check first vertex magnitude is ~20 mm in Millimetres mode, ~0.02 m in Metres mode.
    let vx_mm = f32::from_le_bytes(bytes[96..100].try_into().unwrap());
    let vy_mm = f32::from_le_bytes(bytes[100..104].try_into().unwrap());
    let vz_mm = f32::from_le_bytes(bytes[104..108].try_into().unwrap());
    let r_mm = (vx_mm * vx_mm + vy_mm * vy_mm + vz_mm * vz_mm).sqrt();
    assert!((r_mm - 20.0).abs() <= 1.5, "expected ~20 mm radius in mm STL, got {r_mm}");

    let m_out = export_stl(
        model.graph_ref(),
        &part,
        ExportStlOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.0), unit: StlUnit::Metres, title: "custom", .. },
        None,
    )
    .expect("stl m");
    assert!(m_out.stl.starts_with(b"blaze-cad binary stl (m): custom"));
    let vx_m = f32::from_le_bytes(m_out.stl[96..100].try_into().unwrap());
    let vy_m = f32::from_le_bytes(m_out.stl[100..104].try_into().unwrap());
    let vz_m = f32::from_le_bytes(m_out.stl[104..108].try_into().unwrap());
    let r_m = (vx_m * vx_m + vy_m * vy_m + vz_m * vz_m).sqrt();
    assert!((r_m - 0.02).abs() <= 0.0015, "expected ~0.02 m radius in m STL, got {r_m}");
}

#[test]
fn export_section_svg_box_with_hole_produces_two_contours() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let plate = g.cuboid(BoxOpts { size: mm3(60.0, 40.0, 20.0), .. }).expect("plate");
    let hole = g.cylinder(CylinderOpts { radius: Length::mm(10.0), height: Length::mm(40.0), .. }).expect("hole");
    let solid = g.difference(plate, hole).expect("plate with hole");

    let svg1 = export_section_svg(
        &g,
        solid,
        SectionSvgOpts { alloc: heap(), scratch: heap(), step: Length::mm(1.0), title: "plate section", .. },
    )
    .expect("section svg");
    let svg2 = export_section_svg(
        &g,
        solid,
        SectionSvgOpts { alloc: heap(), scratch: heap(), step: Length::mm(1.0), title: "plate section", .. },
    )
    .expect("section svg deterministic");
    assert_eq!(svg1.as_slice(), svg2.as_slice(), "SVG section export must be byte-identical");

    let text = core::str::from_utf8(&svg1).expect("utf8 svg");
    assert!(text.starts_with("<?xml version=\"1.0\""), "missing xml header");
    assert!(text.contains("<title>plate section</title>"), "missing title");
    assert!(text.contains("fill-rule=\"evenodd\""), "missing evenodd fill rule");
    assert!(text.contains("id=\"dimensions\""), "missing dimensions");
    assert!(text.contains("id=\"scale-bar\""), "missing scale bar");

    // Extract the `silhouette` path's `d="..."` attribute and verify it has 2 closed loops
    // (outer 60×40 rectangle + inner r=10 circular hole).
    let sil_start = text.find("id=\"silhouette\" d=\"").expect("silhouette path") + "id=\"silhouette\" d=\"".len();
    let sil_end = text[sil_start..].find('"').expect("end of d attr");
    let d_attr = &text[sil_start..sil_start + sil_end];
    let z_count = d_attr.matches('Z').count();
    assert_eq!(z_count, 2, "expected 2 closed contours (box + hole), got {z_count} in d={d_attr}");
}

// ---- Early-exit no_interference & no_interference_assembly -------------------------------

#[test]
fn no_interference_early_exit_terminates_in_tiny_box_budget() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    // Exact/Outside multi-primitive parts (union preserves Outside distance).
    let body_a = g.cuboid(BoxOpts { size: mm3(40.0, 40.0, 10.0), round: Length::mm(3.0), .. }).expect("body a");
    let boss_a = g.cylinder(CylinderOpts { radius: Length::mm(10.0), height: Length::mm(24.0), .. }).expect("boss a");
    let part_a = g.union(body_a, boss_a).expect("part a");

    // 1) Far apart (gap = 60 mm >> clearance = 5 mm): AABB lower bound >= clearance exits in 0 boxes!
    let far_b = g.place(part_a, Placement::shift(Length::mm(100.0), Length::ZERO, Length::ZERO)).expect("far b");
    let shape = CompileOpts { alloc: heap(), detail: Detail::Shape, .. };
    let ta = compile(&g, part_a.id(), shape).expect("ta");
    let tb_far = compile(&g, far_b.id(), shape).expect("tb_far");
    let c_far = no_interference(
        &ta,
        &tb_far,
        InterferenceOpts { clearance: Length::mm(5.0), scratch: heap(), most_boxes: 4, .. },
    )
    .expect("far check");
    assert!(c_far.verdict.passed(), "far parts should pass in <= 4 boxes, got {:?}", c_far.verdict);

    // 2) Overlapping (gap = 0 < clearance = 5 mm): early-exit stop_below settles immediately!
    let hit_b = g.place(part_a, Placement::shift(Length::mm(10.0), Length::ZERO, Length::ZERO)).expect("hit b");
    let tb_hit = compile(&g, hit_b.id(), shape).expect("tb_hit");
    let c_hit = no_interference(
        &ta,
        &tb_hit,
        InterferenceOpts { clearance: Length::mm(5.0), scratch: heap(), most_boxes: 4, .. },
    )
    .expect("hit check");
    assert!(matches!(c_hit.verdict, Verdict::Found(_)), "overlapping parts should be Found in <= 4 boxes, got {:?}", c_hit.verdict);
}

#[test]
fn no_interference_assembly_prunes_broad_phase_and_finds_witness_pair() {
    // 1) 3×3 grid of well-separated cubes: all 36 pairs pruned in O(1) by AABB gap >= clearance.
    let mut m_ok = Model::new(ModelOpts { alloc: heap(), .. });
    let cube = m_ok.graph().cuboid(BoxOpts { size: mm3(20.0, 20.0, 20.0), .. }).expect("cube");
    let names = ["g0", "g1", "g2", "g3", "g4", "g5", "g6", "g7", "g8"];
    for (idx, name) in names.iter().enumerate() {
        let gx = (idx % 3) as f64 * 50.0;
        let gy = (idx / 3) as f64 * 50.0;
        let s = m_ok.graph().place(cube, Placement::shift(Length::mm(gx), Length::mm(gy), Length::ZERO)).expect("place");
        m_ok.add(PartOpts { solid: s, name }).expect("add");
    }
    let res_ok = m_ok
        .no_interference(InterferenceOpts { clearance: Length::mm(5.0), scratch: heap(), .. })
        .expect("assembly ok");
    assert!(res_ok.verdict.passed(), "expected pass, got {:?}", res_ok.verdict);
    assert_eq!(res_ok.boxes, 0, "broad-phase AABB pruning should evaluate 0 boxes");

    // 2) 5 parts where pair (1, 3) is only 2 mm apart (< 5 mm clearance).
    let mut m_bad = Model::new(ModelOpts { alloc: heap(), .. });
    let box20 = m_bad.graph().cuboid(BoxOpts { size: mm3(20.0, 20.0, 20.0), .. }).expect("box20");
    let coords = [0.0, 100.0, 200.0, 122.0, 300.0];
    let pnames = ["p0", "p1", "p2", "p3", "p4"];
    for (x, name) in coords.iter().zip(pnames.iter()) {
        let s = m_bad.graph().place(box20, Placement::shift(Length::mm(*x), Length::ZERO, Length::ZERO)).expect("place");
        m_bad.add(PartOpts { solid: s, name }).expect("add");
    }
    let res_bad = m_bad
        .no_interference(InterferenceOpts { clearance: Length::mm(5.0), scratch: heap(), .. })
        .expect("assembly bad");
    match res_bad.verdict {
        Verdict::Found(f) => {
            assert_eq!(f.pair, [1, 3], "expected witness pair (1, 3), got {:?}", f.pair);
            assert_eq!(f.names, [Name::new("p1"), Name::new("p3")]);
            assert!(f.gap[1] < Length::mm(5.0));
            let msg = format!("{}", Shown(&res_bad.verdict));
            assert!(msg.contains("p1 and p3"), "unexpected Show message: {msg}");
        }
        other => panic!("expected Verdict::Found for pair (1, 3), got {other:?}"),
    }
}

// ---- SOTA DFM checks: overhang_check & draft_angle_check ---------------------------------

#[test]
fn overhang_check_cube_passes_and_t_overhang_fails() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    // 1) Solid cube resting on build plate: passes 45° overhang check.
    let cube = g.cuboid(BoxOpts { size: mm3(20.0, 20.0, 20.0), .. }).expect("cube");
    let cube_part = Part::new(&mut g, PartOpts { solid: cube, name: "cube" }).expect("cube part");
    let res_cube = overhang_check(&g, &cube_part, OverhangOpts { scratch: heap(), .. }).expect("overhang cube");
    assert!(res_cube.verdict.passed(), "cube must pass overhang check, got {:?}", res_cube.verdict);

    // 2) T-overhang: vertical stem (10×10×22 at z ∈ [0, 22]) unioned with wide top bar
    // (40×10×10 at z ∈ [20, 30]) — overlapped by 2 mm so the joint is solid with no internal seam.
    let stem = g.cuboid(BoxOpts { size: mm3(10.0, 10.0, 22.0), .. }).expect("stem");
    let stem = g.place(stem, Placement::shift(Length::ZERO, Length::ZERO, Length::mm(11.0))).expect("placed stem");
    let bar = g.cuboid(BoxOpts { size: mm3(40.0, 10.0, 10.0), .. }).expect("bar");
    let bar = g.place(bar, Placement::shift(Length::ZERO, Length::ZERO, Length::mm(25.0))).expect("placed bar");
    let tee = g.union(stem, bar).expect("T-shape");
    let tee_part = Part::new(&mut g, PartOpts { solid: tee, name: "tee" }).expect("tee part");

    let res_tee = overhang_check(&g, &tee_part, OverhangOpts { scratch: heap(), .. }).expect("overhang tee");
    match res_tee.verdict {
        Verdict::Found(f) => {
            assert!(f.angle.as_degrees() >= 85.0, "expected ~90 deg overhang, got {}", f.angle.as_degrees());
            assert!((f.at[2].as_mm() - 20.0).abs() <= 0.5, "expected witness at z ≈ 20 mm, got {:?}", f.at);
            let msg = format!("{}", Shown(&res_tee.verdict));
            assert!(msg.contains("overhang of 90 deg"), "unexpected Show output: {msg}");
        }
        other => panic!("T-overhang must fail overhang_check, got {other:?}"),
    }

    // Translating the part preserves its overhang CheckKey.
    let shifted_tee = tee_part.moved_to(Placement::shift(Length::mm(50.0), Length::mm(-30.0), Length::mm(12.0)));
    let res_shifted = overhang_check(&g, &shifted_tee, OverhangOpts { scratch: heap(), .. }).expect("shifted tee");
    assert_eq!(res_tee.key, res_shifted.key, "translation must keep overhang CheckKey");

    // 3) Flipping the T-shape 180° around X puts the wide bar flat on the build plate: passes!
    let flipped_tee = tee_part.moved_to(Placement::rotation(Axis::X, Angle::degrees(180.0)));
    let res_flipped = overhang_check(&g, &flipped_tee, OverhangOpts { scratch: heap(), .. }).expect("flipped tee");
    assert!(res_flipped.verdict.passed(), "flipped T-shape must pass overhang check, got {:?}", res_flipped.verdict);
}

fn overhang_case(tape: &Tape, plant: impl Planted<OverhangPlant> + Copy) -> Result<(), Broken<CadBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let has_overhang = tape.coin("T-overhang");
    let solid: Solid<Bound> = if has_overhang {
        let stem_w = tape.int_in(8..=12i64, "stem width mm") as f64;
        let bar_w = stem_w + tape.int_in(14..=24i64, "overhang extra mm") as f64;
        let stem_h = tape.int_in(12..=20i64, "stem height mm") as f64;
        let bar_h = tape.int_in(8..=12i64, "bar height mm") as f64;
        let stem = g.cuboid(BoxOpts { size: mm3(stem_w, stem_w, stem_h + 2.0), .. }).map_err(harness)?;
        let stem = g.place(stem, Placement::shift(Length::ZERO, Length::ZERO, Length::mm((stem_h + 2.0) / 2.0))).map_err(harness)?;
        let bar = g.cuboid(BoxOpts { size: mm3(bar_w, stem_w, bar_h), .. }).map_err(harness)?;
        let bar = g
            .place(bar, Placement::shift(Length::ZERO, Length::ZERO, Length::mm(stem_h + bar_h / 2.0)))
            .map_err(harness)?;
        g.union(stem, bar).map_err(harness)?.weaken()
    } else {
        let side = tape.int_in(14..=26i64, "cube side mm") as f64;
        let h = tape.int_in(14..=26i64, "cube height mm") as f64;
        g.cuboid(BoxOpts { size: mm3(side, side, h), .. }).map_err(harness)?.weaken()
    };
    let part = Part::new(&mut g, PartOpts { solid, name: "p" }).map_err(harness)?;
    let got = overhang_check_planted(
        &g,
        &part,
        OverhangOpts { max_angle: Angle::degrees(45.0), sample_size: Length::mm(3.0), scratch: heap(), .. },
        plant,
    )
    .map_err(harness)?;
    match (has_overhang, &got.verdict) {
        (true, Verdict::Found(f)) if f.angle.as_degrees() > 45.0 => Ok(()),
        (false, Verdict::Pass) => Ok(()),
        (_, v) => Err(Broken::Oracle(CadBroken::OverhangWrong { should_fail: has_overhang, verdict: format!("{v:?}") })),
    }
}

#[test]
fn overhang_check_agrees_with_geometry() {
    check(Config::cases(20), |tape| overhang_case(tape, NoPlant));
}

#[test]
fn overhang_plants_are_caught() {
    controls(Config::cases(20), |tape, plant: OverhangPlant| overhang_case(tape, plant));
}

#[test]
fn draft_angle_check_cone_passes_box_and_undercut_fail() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });

    // 1) Tapered frustum (bottom 20 mm, top 15 mm, height 30 mm -> ~9.46° draft): passes at 2°, fails at 15°.
    let frustum = g
        .cone(ConeOpts { bottom: Length::mm(20.0), top: Length::mm(15.0), height: Length::mm(30.0), .. })
        .expect("frustum");
    let frustum_part = Part::new(&mut g, PartOpts { solid: frustum, name: "frustum" }).expect("frustum part");
    let pass_2 = draft_angle_check(
        &g,
        &frustum_part,
        DraftOpts { min_draft: Angle::degrees(2.0), scratch: heap(), .. },
    )
    .expect("draft pass 2 deg");
    assert!(pass_2.verdict.passed(), "9.46 deg frustum must pass 2 deg draft check, got {:?}", pass_2.verdict);

    let fail_15 = draft_angle_check(
        &g,
        &frustum_part,
        DraftOpts { min_draft: Angle::degrees(15.0), scratch: heap(), .. },
    )
    .expect("draft fail 15 deg");
    match fail_15.verdict {
        Verdict::Found(f) => {
            assert!(!f.undercut, "9.46 deg frustum is insufficient draft, not undercut");
            assert!((f.angle.as_degrees() - 9.4623).abs() < 0.5, "unexpected angle {}", f.angle.as_degrees());
        }
        other => panic!("expected insufficient draft at 15 deg, got {other:?}"),
    }

    // 2) Vertical box (0° draft on side walls): fails at 2°.
    let box_s = g.cuboid(BoxOpts { size: mm3(20.0, 20.0, 20.0), .. }).expect("box");
    let box_part = Part::new(&mut g, PartOpts { solid: box_s, name: "box" }).expect("box part");
    let fail_box = draft_angle_check(&g, &box_part, DraftOpts { min_draft: Angle::degrees(2.0), scratch: heap(), .. })
        .expect("draft box");
    match fail_box.verdict {
        Verdict::Found(f) => {
            assert!(!f.undercut, "vertical wall is 0 deg draft, not undercut");
            assert!(f.angle.as_degrees().abs() < 0.5, "expected ~0 deg draft, got {}", f.angle.as_degrees());
            let msg = format!("{}", Shown(&fail_box.verdict));
            assert!(msg.contains("insufficient draft"), "unexpected Show output: {msg}");
        }
        other => panic!("vertical box must fail draft check, got {other:?}"),
    }

    // 3) Inverted cone (bottom 15 mm, top 20 mm, height 30 mm -> ~-9.46° undercut): fails with undercut = true!
    let undercut_cone = g
        .cone(ConeOpts { bottom: Length::mm(15.0), top: Length::mm(20.0), height: Length::mm(30.0), .. })
        .expect("inverted cone");
    let uc_part = Part::new(&mut g, PartOpts { solid: undercut_cone, name: "inverted" }).expect("inverted part");
    let fail_uc = draft_angle_check(&g, &uc_part, DraftOpts { min_draft: Angle::degrees(2.0), scratch: heap(), .. })
        .expect("draft undercut");
    match fail_uc.verdict {
        Verdict::Found(f) => {
            assert!(f.undercut, "inverted cone must be flagged as undercut");
            assert!(f.angle.as_degrees() < -8.0, "expected negative undercut angle, got {}", f.angle.as_degrees());
            let msg = format!("{}", Shown(&fail_uc.verdict));
            assert!(msg.contains("undercut of"), "unexpected Show output: {msg}");
        }
        other => panic!("inverted cone must fail draft check as undercut, got {other:?}"),
    }
}

fn draft_case(tape: &Tape, plant: impl Planted<DraftPlant> + Copy) -> Result<(), Broken<CadBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let kind = tape.index(3, "shape kind");
    let h = tape.int_in(18..=30i64, "height mm") as f64;
    let r_big = tape.int_in(16..=22i64, "big radius mm") as f64;
    let delta = tape.int_in(4..=7i64, "taper delta mm") as f64;
    let r_small = r_big - delta;
    let (solid, should_fail): (Solid<Bound>, bool) = match kind {
        0 => {
            // Properly drafted frustum (taper > 7° >> min_draft = 2°): passes.
            let c = g
                .cone(ConeOpts { bottom: Length::mm(r_big), top: Length::mm(r_small), height: Length::mm(h), .. })
                .map_err(harness)?;
            (c.weaken(), false)
        }
        1 => {
            // Inverted frustum (undercut < -7°): fails (caught by IgnoreUndercutSign plant!).
            let c = g
                .cone(ConeOpts { bottom: Length::mm(r_small), top: Length::mm(r_big), height: Length::mm(h), .. })
                .map_err(harness)?;
            (c.weaken(), true)
        }
        _ => {
            // Vertical cuboid (0° draft): fails.
            let b = g.cuboid(BoxOpts { size: mm3(r_big, r_big, h), .. }).map_err(harness)?;
            (b.weaken(), true)
        }
    };
    let part = Part::new(&mut g, PartOpts { solid, name: "molded" }).map_err(harness)?;
    let got = draft_angle_check_planted(
        &g,
        &part,
        DraftOpts { min_draft: Angle::degrees(2.0), sample_size: Length::mm(3.0), scratch: heap(), .. },
        plant,
    )
    .map_err(harness)?;
    match (should_fail, &got.verdict) {
        (true, Verdict::Found(_)) => Ok(()),
        (false, Verdict::Pass) => Ok(()),
        (_, v) => Err(Broken::Oracle(CadBroken::DraftWrong { should_fail, verdict: format!("{v:?}") })),
    }
}

#[test]
fn draft_angle_check_agrees_with_geometry() {
    check(Config::cases(24), |tape| draft_case(tape, NoPlant));
}

#[test]
fn draft_plants_are_caught() {
    controls(Config::cases(24), |tape, plant: DraftPlant| draft_case(tape, plant));
}

// ---- Kernel & Studio picture ergonomics --------------------------------------------------

#[test]
fn gyroid_booleans_and_picture_studio() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let lattice = gyroid(&mut g, GyroidOpts { period: Length::mm(10.0), thickness: Length::mm(2.0), .. }).expect("gyroid");
    let cube = g.cuboid(BoxOpts { size: mm3(20.0, 20.0, 20.0), .. }).expect("cube").weaken::<Bound>();
    let gyroid_cube = intersect_all(&mut g, &[lattice, cube]).expect("gyroid cube");

    let s1 = g.sphere(SphereOpts { radius: Length::mm(8.0), .. }).expect("s1");
    let s2 = g.place(s1, Placement::shift(Length::mm(14.0), Length::ZERO, Length::ZERO)).expect("s2");
    let s3 = g.place(s1, Placement::shift(Length::mm(-14.0), Length::ZERO, Length::ZERO)).expect("s3");
    let beads = union_all(&mut g, &[s1, s2, s3]).expect("union_all");

    let scene = union_all(&mut g, &[gyroid_cube, beads.weaken()]).expect("combined");
    let view = View {
        eye: mm3(45.0, -50.0, 35.0),
        target: mm3(0.0, 0.0, 0.0),
        projection: Projection::Perspective { fov: Angle::degrees(35.0) },
        width: 48,
        height: 48,
        ..
    };

    let classic = picture(&g, scene, view, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("classic picture");
    let studio1 = picture_studio(&g, scene, view, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("studio picture");
    let studio2 = picture_studio(&g, scene, view, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("studio picture 2");

    assert!(classic.stats.hits > 50, "expected solid hits in classic render, got {}", classic.stats.hits);
    assert!(studio1.stats.hits > 50, "expected solid hits in studio render, got {}", studio1.stats.hits);
    assert_eq!(studio1.pixels.as_slice(), studio2.pixels.as_slice(), "picture_studio must be deterministic");
    assert_ne!(
        classic.pixels.as_slice(),
        studio1.pixels.as_slice(),
        "Studio shading + 2x2 supersampling should differ from classic Lambertian"
    );
}

#[test]
fn decimate_mesh_reduces_triangles_within_hausdorff_bound() {
    let mut model = Model::new(ModelOpts { alloc: heap(), .. });
    let s = model.graph().sphere(SphereOpts { radius: Length::mm(25.0), .. }).expect("sphere");
    let (_, part) = model.add(PartOpts { solid: s, name: "ball", .. }).expect("add");
    let meshed = mesh_part(
        model.graph_ref(),
        &part,
        MeshPartOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(0.3) },
        None,
    )
    .expect("mesh");
    let orig_tris = meshed.mesh.view().triangle_count();
    let max_err = Length::mm(1.0);
    let dec = decimate_mesh(
        &meshed.verified(),
        DecimatePartOpts {
            target: Target::Error(max_err),
            alloc: heap(),
            scratch: heap(),
            ..
        },
    )
    .expect("decimate");
    let dec_tris = dec.mesh.view().triangle_count();
    assert!(dec.verified().is_some(), "decimated mesh keeps Verified proof");
    assert!(dec_tris < orig_tris, "expected fewer triangles: {dec_tris} vs {orig_tris}");
    assert!(dec.error_bound <= max_err, "error_bound {:?} exceeded {:?}", dec.error_bound, max_err);
}
