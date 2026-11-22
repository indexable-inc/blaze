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
    Axis, BoxOpts, CompileOpts, CylinderOpts, Detail, Graph, GraphOpts, Inside, Placement, RepeatOpts, Scratch, Solid, SphereOpts,
    compile, eval_point,
};
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
