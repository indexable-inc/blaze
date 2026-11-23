//! The crate's properties, each with an oracle and the planted bugs it must catch (rule 23: a
//! control removes the code's correctness and watches the test fail).
//!
//! | test | oracle | planted |
//! |---|---|---|
//! | `intervals_contain_points` | every instruction's value at a point of a box lies in that instruction's interval over the box (corners included) | `IntervalNotOutward` |
//! | `simplified_equals_full` | in its box, a simplified tape gives the full tape's bits | `SimplifyWrongSide` |
//! | `fields_keep_their_level` | an exact field steps onto its surface along its gradient; a blend stays on its side of the sharp boolean; every field is 1-Lipschitz | `EllipsoidExact`, `SmoothUnionWrongSign` |
//! | `gradients_match_differences` | forward-mode gradient equals central differences where they are smooth | `GradAbsSignIgnored` |
//! | `tape_matches_reference` | the tape equals a recursive evaluator written from the node table | `PlacementShiftDropped` |
//! | `lanes_match_scalar` | four and eight lanes give the scalar interpreter's bits | `VectorDivReciprocal` |
//! | `digests_cover_every_parameter` | one changed parameter changes the root digest; history does not | `DigestDropsParameter` |
//! | `spellings_share_a_digest` | commuted operands, scale there and back, grow twice: one node | `OperandsUnordered` |
//! | `rotation_keeps_geometry` | rotating a part changes its placement, not its geometry's digest | `FrameKeptInGeometry` |
//! | `one_change_rekeys_its_path` | changing one of `2^k` spheres makes exactly `k + 2` nodes | `NoHashConsing` |
//! | `region_keys_follow_the_change` | a far change keeps a cell's key, a change in the cell changes it | `TapeDigestSkipsConsts` |
//! | `far_parts_drop_out_of_cells` | at most 2% of (surface cell, far part) pairs keep the part | `SimplifyKeepsBothSides` |
//! | `tapes_round_trip` | decode of encode re-encodes to the same bytes, digest and values | `EncodeConstsAsF32` |

use core::fmt;
use std::vec;
use std::vec::Vec;

use blaze_math::{Aabb3, Interval, Vec3};
use blaze_own::Alloc;
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape as Draw, check, controls};
use blaze_units::{Angle, Length};

use crate::eval::{eval_box_each, eval_point_each};
use crate::{
    Axis, BlendOpts, Boolean, BoxOpts, CadError, CadPlant, CapsuleOpts, CompileOpts, ConeOpts, CylinderOpts, DetailOpts, DistanceOpts,
    DistanceStopOpts, Edge, EdgeSelect, EllipsoidOpts, EvalError, Gap, Graph, GraphOpts, GridOpts, GuideRail, GuidedLoftOpts,
    GuidedStation, GyroidOpts, HalfSpaceOpts, Lanes, Level, Limits, LoftContinuity, Node, NodeId, NodeKind, OffsetOpts, Placement, Points,
    Profile, ProfileOpts, RadiusProfile, RadiusStop, RailPoint, RayOpts, ScaleOpts, Scratch, Shape, ShellOpts, Solid, SphereOpts,
    SweepCrossSection, SweepFrame, SweepFramesOpts, SweepPolylineOpts, TaperOpts, Tape, TorusOpts, TwistOpts, VariableBlendOpts,
    VariableOffsetOpts, compile, distance_between, distance_between_stopping, eval_box, eval_grad, eval_grads, eval_grads_with, eval_point,
    eval_points, eval_points_with, ray, simplify_with,
};
use crate::level::{Bound, Exact, Inside, Outside};

// ---- what a test finds ------------------------------------------------------------------------

/// An invariant seen broken.
#[derive(Debug)]
enum CadBroken {
    /// A point's value of instruction `instr` lies outside that instruction's box interval.
    Escapes { instr: usize, op: &'static str, value: f64, lo: f64, hi: f64 },
    /// A simplified tape differs from the full tape inside its box.
    SimplifiedDiffers { full: f64, simplified: f64 },
    /// A field said to be exact (there) does not step onto its surface.
    NotExact { level: &'static str, f: f64, landed: f64 },
    /// A blended union rose above the sharp union, or a blended intersection fell below.
    BlendCrossesSharp { blend: f64, sharp: f64 },
    /// A field changed faster than distance.
    NotLipschitz { slope: f64 },
    /// The forward-mode gradient is off from central differences.
    GradientOff { analytic: [f64; 3], numeric: [f64; 3] },
    /// The tape is off from the reference evaluator.
    ReferenceDiffers { tape: f64, reference: f64 },
    /// A lane form gave other bits than the scalar interpreter.
    LanesDiffer { lanes: &'static str, scalar: f64, vector: f64 },
    /// A changed parameter kept the root digest.
    DigestBlind { what: &'static str },
    /// The same model built after other nodes got another digest.
    DigestUnstable,
    /// Two spellings of one solid made two nodes.
    SpellingsDiffer { what: &'static str },
    /// Rotating a part changed its geometry's digest.
    GeometryRekeyed,
    /// One changed leaf made other than `k + 2` nodes.
    RekeyedTooMuch { expected: usize, made: usize },
    /// A region key ignored a change in its region, or followed one outside it.
    RegionKey { what: &'static str },
    /// Far parts stayed in the simplified tapes of surface cells.
    FarKept { kept: u32, far: u32 },
    /// A tape did not survive encode and decode.
    RoundTrip { what: &'static str },
    /// A SIMD gradient lane differed from scalar `eval_grad`.
    GradLanesDiffer { lanes: &'static str, component: &'static str, scalar: f64, vector: f64 },
}

impl fmt::Display for CadBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

type R = Result<(), Broken<CadBroken>>;

fn harness(e: impl fmt::Display) -> Broken<CadBroken> {
    Broken::Harness(std::format!("{e}"))
}

fn broken(b: CadBroken) -> R {
    Err(Broken::Oracle(b))
}

/// A test's plant as the crate's plant: each test names the variants it controls.
macro_rules! into_cad {
    ($($name:ident { $($v:ident),* })*) => {$(
        impl Planted<CadPlant> for $name {
            fn has(self, p: CadPlant) -> bool {
                match self {
                    $name::None => false,
                    $( $name::$v => p == CadPlant::$v, )*
                }
            }
        }
    )*};
}

// ---- models ------------------------------------------------------------------------------------

fn nm(n: i64) -> Length {
    Length::nm(n)
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn graph<P: Planted<CadPlant>>(plant: P) -> Graph<'static, P> {
    Graph::planted(GraphOpts { alloc: alloc(), .. }, plant)
}

/// A primitive's parameters in nanometres, in the order of `Shape::key`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Prim {
    Sphere(i64),
    /// Full sizes, edge radius.
    Box([i64; 3], i64),
    /// Radius, full height.
    Cylinder(i64, i64),
    /// Radius, length between the cap centres.
    Capsule(i64, i64),
    /// Bottom radius, top radius, full height.
    Cone(i64, i64, i64),
    Torus(i64, i64),
    Ellipsoid([i64; 3]),
}

impl Prim {
    fn arity(self) -> usize {
        match self {
            Prim::Sphere(_) => 1,
            Prim::Cylinder(..) | Prim::Capsule(..) | Prim::Torus(..) => 2,
            Prim::Cone(..) | Prim::Ellipsoid(_) => 3,
            Prim::Box(..) => 4,
        }
    }

    /// Parameter `k` (of [`Prim::arity`]) made 2 micrometres larger: still valid, and a whole
    /// micrometre on every half-size the graph keys.
    fn bump(self, k: usize) -> Prim {
        let d = 2_000;
        match self {
            Prim::Sphere(r) => Prim::Sphere(r + d),
            Prim::Box(mut s, round) => {
                if k < 3 {
                    s[k] += d;
                    Prim::Box(s, round)
                } else {
                    Prim::Box(s, round + d)
                }
            }
            Prim::Cylinder(r, h) => if k == 0 { Prim::Cylinder(r + d, h) } else { Prim::Cylinder(r, h + d) },
            Prim::Capsule(r, l) => if k == 0 { Prim::Capsule(r + d, l) } else { Prim::Capsule(r, l + d) },
            Prim::Cone(a, b, h) => match k {
                0 => Prim::Cone(a + d, b, h),
                1 => Prim::Cone(a, b + d, h),
                _ => Prim::Cone(a, b, h + d),
            },
            Prim::Torus(a, b) => if k == 0 { Prim::Torus(a + d, b) } else { Prim::Torus(a, b + d) },
            Prim::Ellipsoid(mut r) => {
                r[k] += d;
                Prim::Ellipsoid(r)
            }
        }
    }
}

/// What a leaf's exact primitive is wrapped in before it is placed.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Wrap {
    Plain,
    Grow(i64),
    Erode(i64),
    Shell(i64),
    Detail(i64, i64),
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Leaf {
    prim: Prim,
    wrap: Wrap,
    axis: Axis,
    /// Micro-arcseconds about `axis`.
    turn: i64,
    /// Nanometres.
    at: [i64; 3],
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Join {
    Sharp(Boolean),
    Round(Boolean, i64),
    Chamfer(Boolean, i64),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Over {
    /// Millionths.
    Scale(i64),
    /// Micro-arcseconds per metre.
    Twist(i64),
    /// Millionths at the top.
    Taper(i64),
}

/// A model in postfix: leaves push, joins pop two, overs map the top.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Step {
    Leaf(Leaf),
    Join(Join),
    Over(Over),
}

/// Which nodes a model may use.
#[derive(Clone, Copy)]
struct Lang {
    /// Twist and taper (bounds with a guard: not Lipschitz across it, not smooth).
    deform: bool,
    /// Ellipsoids (bounds).
    ellipsoid: bool,
    /// Detail reliefs.
    detail: bool,
}

const ALL: Lang = Lang { deform: true, ellipsoid: true, detail: true };

/// A length drawn in whole micrometres, as nanometres.
fn um(t: &Draw, lo: i64, hi: i64, label: &'static str) -> i64 {
    t.int_in(lo..=hi, label) * 1_000
}

fn leaf_spec(t: &Draw, lang: Lang) -> Leaf {
    let top = if lang.ellipsoid { 6u32 } else { 5 };
    let prim = match t.int_in(0u32..=top, "primitive") {
        0 => Prim::Sphere(um(t, 1_000, 12_000, "radius")),
        1 => {
            let s = [um(t, 2_000, 24_000, "size x"), um(t, 2_000, 24_000, "size y"), um(t, 2_000, 24_000, "size z")];
            let most = (s[0].min(s[1]).min(s[2]) / 2 - 2_000) / 1_000;
            let round = if t.coin("rounded") { um(t, 0, most, "round") } else { 0 };
            Prim::Box(s, round)
        }
        2 => Prim::Cylinder(um(t, 1_000, 10_000, "radius"), um(t, 2_000, 24_000, "height")),
        3 => Prim::Capsule(um(t, 1_000, 8_000, "radius"), um(t, 0, 20_000, "length")),
        4 => Prim::Cone(um(t, 1_000, 10_000, "bottom"), um(t, 0, 10_000, "top"), um(t, 2_000, 24_000, "height")),
        5 => {
            let small = um(t, 1_000, 4_000, "minor");
            Prim::Torus(small + um(t, 1_000, 10_000, "major beyond minor"), small)
        }
        _ => Prim::Ellipsoid([um(t, 2_000, 12_000, "radius x"), um(t, 2_000, 12_000, "radius y"), um(t, 2_000, 12_000, "radius z")]),
    };
    let wrap = if matches!(prim, Prim::Ellipsoid(_)) {
        Wrap::Plain
    } else {
        match t.int_in(0u32..=6, "wrap") {
            0 => Wrap::Grow(um(t, 100, 3_000, "grow")),
            1 => Wrap::Erode(um(t, 100, 1_000, "erode")),
            2 => Wrap::Shell(um(t, 200, 2_000, "shell")),
            3 if lang.detail => Wrap::Detail(um(t, 50, 500, "amplitude"), um(t, 1_000, 5_000, "period")),
            _ => Wrap::Plain,
        }
    };
    let axis = *t.pick(&[Axis::X, Axis::Y, Axis::Z], "axis").unwrap_or(&Axis::Z);
    let half_turn = Angle::<blaze_units::Plain>::PER_TURN / 2;
    let turn = if t.coin("turned") { t.int_in(-half_turn..=half_turn, "turn") } else { 0 };
    let at = [um(t, -15_000, 15_000, "at x"), um(t, -15_000, 15_000, "at y"), um(t, -15_000, 15_000, "at z")];
    Leaf { prim, wrap, axis, turn, at }
}

fn join_spec(t: &Draw) -> Join {
    let op = *t.pick(&[Boolean::Union, Boolean::Intersect, Boolean::Difference], "boolean").unwrap_or(&Boolean::Union);
    match t.int_in(0u32..=3, "edge") {
        0 => Join::Round(op, um(t, 500, 4_000, "blend radius")),
        1 => Join::Chamfer(op, um(t, 500, 4_000, "chamfer")),
        _ => Join::Sharp(op),
    }
}

fn over_spec(t: &Draw, lang: Lang) -> Over {
    let pick = if lang.deform { t.int_in(0u32..=2, "over") } else { 0 };
    match pick {
        0 => {
            let ppm = t.int_in(300_000i64..=3_000_000, "scale ppm");
            Over::Scale(if ppm == 1_000_000 { 1_300_000 } else { ppm })
        }
        1 => {
            let turns = Angle::<blaze_units::Plain>::PER_TURN * 5;
            Over::Twist(t.int_in(-turns..=turns, "twist per metre"))
        }
        _ => Over::Taper(t.int_in(300_000i64..=2_000_000, "taper ppm")),
    }
}

fn tree(t: &Draw, lang: Lang, depth: u32, out: &mut Vec<Step>) {
    if depth == 0 || t.one_in(3, "leaf here") {
        out.push(Step::Leaf(leaf_spec(t, lang)));
    } else {
        tree(t, lang, depth - 1, out);
        tree(t, lang, depth - 1, out);
        out.push(Step::Join(join_spec(t)));
    }
    if depth > 0 && t.one_in(4, "over here") {
        out.push(Step::Over(over_spec(t, lang)));
    }
}

fn spec(t: &Draw, lang: Lang) -> Vec<Step> {
    let mut v = Vec::new();
    tree(t, lang, 3, &mut v);
    v
}

fn leaf<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, l: &Leaf) -> Result<Solid<Bound>, crate::CadError> {
    let s: Solid<Bound> = match l.prim {
        Prim::Ellipsoid(r) => g.ellipsoid(EllipsoidOpts { radii: r.map(nm), .. })?,
        p => {
            let e: Solid<Exact> = match p {
                Prim::Sphere(r) => g.sphere(SphereOpts { radius: nm(r), .. })?,
                Prim::Box(s, round) => g.cuboid(BoxOpts { size: s.map(nm), round: nm(round), .. })?,
                Prim::Cylinder(r, h) => g.cylinder(CylinderOpts { radius: nm(r), height: nm(h), .. })?,
                Prim::Capsule(r, len) => g.capsule(CapsuleOpts { radius: nm(r), length: nm(len), .. })?,
                Prim::Cone(a, b, h) => g.cone(ConeOpts { bottom: nm(a), top: nm(b), height: nm(h), .. })?,
                Prim::Torus(a, b) => g.torus(TorusOpts { major: nm(a), minor: nm(b), .. })?,
                Prim::Ellipsoid(_) => unreachable!("handled above"),
            };
            match l.wrap {
                Wrap::Plain => e.weaken::<Bound>(),
                Wrap::Grow(r) => g.grow(e, OffsetOpts { by: nm(r), .. })?.weaken::<Bound>(),
                Wrap::Erode(r) => g.erode(e, OffsetOpts { by: nm(r), .. })?.weaken::<Bound>(),
                Wrap::Shell(th) => g.shell(e, ShellOpts { thickness: nm(th), .. })?.weaken::<Bound>(),
                Wrap::Detail(a, p) => g.detail(e, DetailOpts { amplitude: nm(a), period: nm(p), .. })?,
            }
        }
    };
    let s = g.place(s, Placement::rotation(l.axis, Angle::microarcsec(l.turn)))?;
    g.place(s, Placement::shift(nm(l.at[0]), nm(l.at[1]), nm(l.at[2])))
}

fn join<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, j: Join, a: Solid<Bound>, b: Solid<Bound>) -> Result<Solid<Bound>, crate::CadError> {
    let blend = |r: i64| BlendOpts { radius: nm(r), .. };
    match j {
        Join::Sharp(Boolean::Union) => g.union(a, b),
        Join::Sharp(Boolean::Intersect) => g.intersect(a, b),
        Join::Sharp(Boolean::Difference) => g.difference(a, b),
        Join::Round(Boolean::Union, r) => g.round_union(a, b, blend(r)),
        Join::Round(Boolean::Intersect, r) => g.round_intersect(a, b, blend(r)),
        Join::Round(Boolean::Difference, r) => g.round_difference(a, b, blend(r)),
        Join::Chamfer(Boolean::Union, r) => g.chamfer_union(a, b, blend(r)),
        Join::Chamfer(Boolean::Intersect, r) => g.chamfer_intersect(a, b, blend(r)),
        Join::Chamfer(Boolean::Difference, r) => g.chamfer_difference(a, b, blend(r)),
    }
}

fn over<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, o: Over, a: Solid<Bound>) -> Result<Solid<Bound>, crate::CadError> {
    match o {
        Over::Scale(ppm) => g.scale(a, ScaleOpts { factor: ppm as f64 / 1e6, .. }),
        Over::Twist(rate) => g.twist(a, TwistOpts { per_metre: Angle::microarcsec(rate), .. }),
        Over::Taper(ppm) => g.taper(a, TaperOpts { top_scale: ppm as f64 / 1e6, .. }),
    }
}

fn build<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, spec: &[Step]) -> Result<Solid<Bound>, crate::CadError> {
    let mut stack: Vec<Solid<Bound>> = Vec::new();
    for s in spec {
        match *s {
            Step::Leaf(l) => stack.push(leaf(g, &l)?),
            Step::Join(j) => {
                let b = stack.pop().expect("a join has two operands (postfix)");
                let a = stack.pop().expect("a join has two operands (postfix)");
                stack.push(join(g, j, a, b)?);
            }
            Step::Over(o) => {
                let a = stack.pop().expect("an over has an operand (postfix)");
                stack.push(over(g, o, a)?);
            }
        }
    }
    Ok(stack.pop().expect("a model has a root"))
}

/// The model, or the case discarded when the generator drew one the graph refuses (a deformed
/// empty intersection): the refusal is the graph's documented answer, not a finding.
fn built<P: Planted<CadPlant>>(t: &Draw, g: &mut Graph<'_, P>, spec: &[Step]) -> NodeId {
    match build(g, spec) {
        Ok(s) => s.id(),
        Err(_) => t.discard(),
    }
}

fn tape_of<'a, P: Planted<CadPlant>>(g: &Graph<'_, P>, root: NodeId, a: Alloc<'a>) -> Result<Tape<'a>, Broken<CadBroken>> {
    compile(g, root, CompileOpts { alloc: a, .. }).map_err(harness)
}

// ---- points and boxes --------------------------------------------------------------------------

const REACH: f64 = 0.04;

fn point(t: &Draw) -> Vec3 {
    Vec3::new(t.f64_in(-REACH..=REACH, "x"), t.f64_in(-REACH..=REACH, "y"), t.f64_in(-REACH..=REACH, "z"))
}

fn cell(t: &Draw) -> Aabb3 {
    let c = point(t);
    let h = 10f64.powf(t.f64_in(-5.0..=-1.7, "log10 half"));
    Aabb3::new(Vec3::new(c.x - h, c.y - h, c.z - h), Vec3::new(c.x + h, c.y + h, c.z + h))
}

/// The eight corners (where an inward end shows) and eight points drawn inside.
fn points_in(t: &Draw, b: &Aabb3) -> Vec<Vec3> {
    let (lo, hi) = (b.lo, b.hi);
    let mut v = Vec::new();
    for k in 0..8 {
        v.push(Vec3::new(if k & 1 == 0 { lo.x } else { hi.x }, if k & 2 == 0 { lo.y } else { hi.y }, if k & 4 == 0 { lo.z } else { hi.z }));
    }
    let inside = |a: f64, b: f64, u: f64| (a + (b - a) * u).max(a).min(b);
    for _ in 0..8 {
        v.push(Vec3::new(
            inside(lo.x, hi.x, t.unit_f64("u")),
            inside(lo.y, hi.y, t.unit_f64("v")),
            inside(lo.z, hi.z, t.unit_f64("w")),
        ));
    }
    v
}

fn same_bits(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

// ---- 1. intervals contain points --------------------------------------------------------------

fn containment(t: &Draw, plant: impl Planted<CadPlant>) -> R {
    let mut g = graph(NoPlant);
    let root = built(t, &mut g, &spec(t, ALL));
    let tape = tape_of(&g, root, alloc())?;
    let b = cell(t);
    let mut s = Scratch::new(alloc());
    let mut boxes: Vec<Interval> = vec![Interval::ENTIRE; tape.prog.len()];
    let _ = eval_box_each(&tape, &b, &mut s, plant, &mut |j, iv| boxes[j] = iv);
    for p in points_in(t, &b) {
        let mut escaped = None;
        eval_point_each(&tape, p, &mut s, &mut |j, v| {
            if escaped.is_none() && !v.is_nan() && !boxes[j].contains(v) {
                escaped = Some((j, v));
            }
        });
        if let Some((j, value)) = escaped {
            let iv = boxes[j];
            return broken(CadBroken::Escapes { instr: j, op: tape.prog[j].op.name(), value, lo: iv.lo(), hi: iv.hi() });
        }
    }
    Ok(())
}

blaze_sim::plants! {
    enum ContainPlant breaks CadBroken {
        None,
        IntervalNotOutward => CadBroken::Escapes { .. },
    }
}

#[test]
fn intervals_contain_points() {
    check(Config::cases(600), |t| containment(t, NoPlant));
}

#[test]
fn intervals_contain_points_controls() {
    controls(Config::cases(600), |t, plant: ContainPlant| containment(t, plant));
}

// ---- 2. simplified == full ---------------------------------------------------------------------

fn simplified(t: &Draw, plant: impl Planted<CadPlant>) -> R {
    let mut g = graph(NoPlant);
    let root = built(t, &mut g, &spec(t, ALL));
    let tape = tape_of(&g, root, alloc())?;
    let b = cell(t);
    let mut s = Scratch::new(alloc());
    let (_, choices) = eval_box(&tape, &b, &mut s);
    let small = simplify_with(&tape, choices, alloc(), plant).map_err(harness)?;
    for p in points_in(t, &b) {
        let full = eval_point(&tape, p, &mut s);
        let simplified = eval_point(&small, p, &mut s);
        if !same_bits(full, simplified) {
            return broken(CadBroken::SimplifiedDiffers { full, simplified });
        }
    }
    Ok(())
}

blaze_sim::plants! {
    enum SimplifyPlant breaks CadBroken {
        None,
        SimplifyWrongSide => CadBroken::SimplifiedDiffers { .. },
    }
}

#[test]
fn simplified_equals_full() {
    check(Config::cases(600), |t| simplified(t, NoPlant));
}

#[test]
fn simplified_equals_full_controls() {
    controls(Config::cases(600), |t, plant: SimplifyPlant| simplified(t, plant));
}

// ---- 3. fields keep their level ----------------------------------------------------------------

fn levels<P: Planted<CadPlant>>(t: &Draw, plant: P) -> R {
    let lang = Lang { deform: false, ellipsoid: true, detail: true };
    let mut g = graph(plant);
    let a = leaf_spec(t, lang);
    let model = if t.coin("one leaf") {
        vec![Step::Leaf(a)]
    } else {
        vec![Step::Leaf(a), Step::Leaf(leaf_spec(t, lang)), Step::Join(join_spec(t))]
    };
    let root = built(t, &mut g, &model);
    let tape = tape_of(&g, root, alloc())?;
    let mut s = Scratch::new(alloc());
    let near = Vec3::new(a.at[0] as f64 / 1e9, a.at[1] as f64 / 1e9, a.at[2] as f64 / 1e9);
    let mut pts = Vec::new();
    for _ in 0..48 {
        pts.push(if t.coin("near the first leaf") { near + point(t) * 0.5 } else { point(t) });
    }
    // A blend stays on its side of the sharp boolean (level table: "below the min or above
    // the max"), checked before anything a wrong sign would also break.
    if let Node::Boolean { op, edge, a: na, b: nb } = *g.node(root)
        && edge != Edge::Sharp
    {
        let (ta, tb) = (tape_of(&g, na, alloc())?, tape_of(&g, nb, alloc())?);
        for p in &pts {
            let f = eval_point(&tape, *p, &mut s);
            let fa = eval_point(&ta, *p, &mut s);
            let fb = eval_point(&tb, *p, &mut s);
            let fb = if op == Boolean::Difference { -fb } else { fb };
            if op == Boolean::Union {
                if f > fa.min(fb) {
                    return broken(CadBroken::BlendCrossesSharp { blend: f, sharp: fa.min(fb) });
                }
            } else if f < fa.max(fb) {
                return broken(CadBroken::BlendCrossesSharp { blend: f, sharp: fa.max(fb) });
            }
        }
    }
    // Where the level says the field is the distance, a step of `f` along the gradient lands
    // on the surface.
    let level = g.level(root);
    for p in &pts {
        let (f, grad) = eval_grad(&tape, *p, &mut s);
        let n = grad.length();
        if !f.is_finite() || !n.is_finite() || n < 0.5 {
            continue;
        }
        let exact_here = match level {
            Level::Exact => true,
            Level::Outside => f > 0.0,
            Level::Inside => f < 0.0,
            Level::Bound => false,
        };
        if exact_here {
            let q = *p - grad * (f / n);
            let landed = eval_point(&tape, q, &mut s);
            if landed.abs() > 1e-10 + 1e-6 * f.abs() {
                return broken(CadBroken::NotExact { level: level.name(), f, landed });
            }
        }
    }
    // Every level is 1-Lipschitz for these nodes (no deformation guard).
    for p in &pts {
        let len = 10f64.powf(t.f64_in(-5.0..=-2.0, "log10 step"));
        let d = Vec3::new(t.f64_in(-1.0..=1.0, "dx"), t.f64_in(-1.0..=1.0, "dy"), t.f64_in(-1.0..=1.0, "dz"));
        let Some(d) = d.normalized() else { continue };
        let q = *p + d * len;
        let (fp, fq) = (eval_point(&tape, *p, &mut s), eval_point(&tape, q, &mut s));
        let dist = (q - *p).length();
        if (fp - fq).abs() > dist * (1.0 + 1e-9) + 1e-15 {
            return broken(CadBroken::NotLipschitz { slope: (fp - fq).abs() / dist });
        }
    }
    Ok(())
}

blaze_sim::plants! {
    enum LevelPlant breaks CadBroken {
        None,
        EllipsoidExact => CadBroken::NotExact { .. },
        SmoothUnionWrongSign => CadBroken::BlendCrossesSharp { .. },
    }
}

#[test]
fn fields_keep_their_level() {
    check(Config::cases(800), |t| levels(t, NoPlant));
}

#[test]
fn fields_keep_their_level_controls() {
    controls(Config::cases(800), |t, plant: LevelPlant| levels(t, plant));
}

// ---- 4. gradients ------------------------------------------------------------------------------

fn gradients(t: &Draw, plant: impl Planted<CadPlant>) -> R {
    let mut g = graph(NoPlant);
    let root = built(t, &mut g, &spec(t, Lang { deform: false, ellipsoid: true, detail: true }));
    let tape = tape_of(&g, root, alloc())?;
    let mut s = Scratch::new(alloc());
    let axes = [Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 0.0, 1.0)];
    for _ in 0..16 {
        let p = point(t);
        let (f, grad) = crate::eval_grad_with(&tape, p, &mut s, plant);
        if !f.is_finite() {
            continue;
        }
        let mut diff = |h: f64| axes.map(|e| (eval_point(&tape, p + e * h, &mut s) - eval_point(&tape, p - e * h, &mut s)) / (2.0 * h));
        let (n1, n2) = (diff(1e-6), diff(2.5e-7));
        // Smooth here: the two step sizes agree (a kink within a step makes them differ).
        if (0..3).any(|i| !n1[i].is_finite() || (n1[i] - n2[i]).abs() > 1e-3) {
            continue;
        }
        let analytic = grad.to_array();
        if (0..3).any(|i| (analytic[i] - n2[i]).abs() > 1e-2) {
            return broken(CadBroken::GradientOff { analytic, numeric: n2 });
        }
    }
    Ok(())
}

blaze_sim::plants! {
    enum GradPlant breaks CadBroken {
        None,
        GradAbsSignIgnored => CadBroken::GradientOff { .. },
    }
}

#[test]
fn gradients_match_differences() {
    check(Config::cases(500), |t| gradients(t, NoPlant));
}

#[test]
fn gradients_match_differences_controls() {
    controls(Config::cases(500), |t, plant: GradPlant| gradients(t, plant));
}

// ---- 5. the tape against a reference evaluator ---------------------------------------------------

fn mm(v: i64) -> f64 {
    v as f64 / 1e9
}

/// World point to a frame's local point, by the transpose (the frame is rigid) and the exact
/// shift: an independent route from `local_rows`, which snaps the inverse's shift.
fn local(pl: &Placement, p: Vec3) -> Vec3 {
    pl.linear().transpose().apply(p - pl.shift_m())
}

fn shape_ref(s: &Shape, p: Vec3) -> f64 {
    let len2 = |a: f64, b: f64| (a * a + b * b).sqrt();
    match *s {
        Shape::HalfSpace => p.z,
        Shape::Sphere { r } => p.length() - mm(r),
        Shape::Box { half, round } => {
            let q = [p.x.abs() - mm(half[0] - round), p.y.abs() - mm(half[1] - round), p.z.abs() - mm(half[2] - round)];
            let out = Vec3::new(q[0].max(0.0), q[1].max(0.0), q[2].max(0.0)).length();
            out + q[0].max(q[1]).max(q[2]).min(0.0) - mm(round)
        }
        Shape::Cylinder { r, half_h } => {
            let (dx, dz) = (len2(p.x, p.y) - mm(r), p.z.abs() - mm(half_h));
            dx.max(dz).min(0.0) + len2(dx.max(0.0), dz.max(0.0))
        }
        Shape::Capsule { r, half_h } => {
            let h = mm(half_h);
            Vec3::new(p.x, p.y, p.z - p.z.clamp(-h, h)).length() - mm(r)
        }
        Shape::Cone { r0, r1, half_h } => {
            // Inigo Quilez's capped cone, radius `rb` at `-h`, `rt` at `+h`.
            let (rb, rt, h) = (mm(r0), mm(r1), mm(half_h));
            let q = len2(p.x, p.y);
            let ca = (q - q.min(if p.z < 0.0 { rb } else { rt }), p.z.abs() - h);
            let k2 = (rt - rb, 2.0 * h);
            let tt = (((rt - q) * k2.0 + (h - p.z) * k2.1) / (k2.0 * k2.0 + k2.1 * k2.1)).clamp(0.0, 1.0);
            let cb = (q - rt + k2.0 * tt, p.z - h + k2.1 * tt);
            let d = len2(ca.0, ca.1).min(len2(cb.0, cb.1));
            if cb.0 < 0.0 && ca.1 < 0.0 { -d } else { d }
        }
        Shape::Torus { big, small } => len2(len2(p.x, p.y) - mm(big), p.z) - mm(small),
        Shape::Ellipsoid { radii } => {
            let r = radii.map(mm);
            (Vec3::new(p.x / r[0], p.y / r[1], p.z / r[2]).length() - 1.0) * r[0].min(r[1]).min(r[2])
        }
        Shape::Superellipsoid { .. } => f64::NAN,
        Shape::Gyroid { period, thickness } => {
            let k = 2.0 * core::f64::consts::PI / mm(period);
            let g = (k * p.x).sin() * (k * p.y).cos() + (k * p.y).sin() * (k * p.z).cos() + (k * p.z).sin() * (k * p.x).cos();
            g.abs() / (k * 3f64.sqrt()) - 0.5 * mm(thickness)
        }
    }
}

/// The node table's formulas, recursively: `None` for a node this evaluator leaves out.
fn reference<P: Planted<CadPlant>>(g: &Graph<'_, P>, id: NodeId, p: Vec3) -> Option<f64> {
    Some(match *g.node(id) {
        Node::Empty => f64::INFINITY,
        Node::Primitive { shape, frame } => shape_ref(&shape, local(&frame, p)),
        Node::Placed { inner, placement } => reference(g, inner, local(&placement, p))?,
        Node::Scale { inner, factor_pp12 } => {
            let f = factor_pp12 as f64 / 1e12;
            f * reference(g, inner, p * (1.0 / f))?
        }
        Node::MirrorFold { inner, axis } => {
            let mut q = p.to_array();
            q[axis.index()] = q[axis.index()].abs();
            reference(g, inner, Vec3::from_array(q))?
        }
        Node::Boolean { op, edge, a, b } => {
            let fa = reference(g, a, p)?;
            let fb = reference(g, b, p)?;
            let fb = if op == Boolean::Difference { -fb } else { fb };
            let union = op == Boolean::Union;
            let sharp = if union { fa.min(fb) } else { fa.max(fb) };
            match edge {
                Edge::Sharp => sharp,
                Edge::Round { r } => {
                    let r = mm(r);
                    let h = (r - (fa - fb).abs()).max(0.0) / r;
                    let bump = h * h * r * 0.25;
                    if union { sharp - bump } else { sharp + bump }
                }
                Edge::Chamfer { r } => {
                    let r = mm(r);
                    if union { sharp.min((fa + fb - r) * 0.5) } else { sharp.max((fa + fb + r) * 0.5) }
                }
            }
        }
        Node::Grow { inner, r } => reference(g, inner, p)? - mm(r),
        Node::Erode { inner, r } => reference(g, inner, p)? + mm(r),
        Node::Shell { inner, t } => {
            let v = reference(g, inner, p)?;
            v.max(-v - mm(t))
        }
        Node::Detail { inner, amp, period } => {
            let (a, k) = (mm(amp), 2.0 * core::f64::consts::PI / mm(period));
            let relief = a * (k * p.x).sin() * (k * p.y).sin() * (k * p.z).sin();
            (reference(g, inner, p)? + relief) / (1.0 + a * k * 3f64.sqrt())
        }
        _ => return None,
    })
}

fn against_reference<P: Planted<CadPlant>>(t: &Draw, plant: P) -> R {
    let mut g = graph(plant);
    let root = built(t, &mut g, &spec(t, Lang { deform: false, ellipsoid: true, detail: true }));
    let tape = tape_of(&g, root, alloc())?;
    let mut s = Scratch::new(alloc());
    for _ in 0..16 {
        let p = point(t);
        let Some(want) = reference(&g, root, p) else {
            return Err(Broken::Harness(std::format!("the reference evaluator has no formula for a node of {root:?}")));
        };
        let got = eval_point(&tape, p, &mut s);
        // Rounding apart, the only difference is the inverse's shift snapped to the
        // nanometre: under 1e-9 m a level of placement.
        let close = (got - want).abs() <= 1e-8 || (got.is_infinite() && got == want);
        if !close {
            return broken(CadBroken::ReferenceDiffers { tape: got, reference: want });
        }
    }
    Ok(())
}

blaze_sim::plants! {
    enum RefPlant breaks CadBroken {
        None,
        PlacementShiftDropped => CadBroken::ReferenceDiffers { .. },
    }
}

#[test]
fn tape_matches_reference() {
    check(Config::cases(500), |t| against_reference(t, NoPlant));
}

#[test]
fn tape_matches_reference_controls() {
    controls(Config::cases(500), |t, plant: RefPlant| against_reference(t, plant));
}

// ---- 6. lanes == scalar --------------------------------------------------------------------------

fn lanes(t: &Draw, plant: impl Planted<CadPlant>) -> R {
    let mut g = graph(NoPlant);
    let mut model = spec(t, ALL);
    // A division in every tape: a scale by a factor whose reciprocal is inexact.
    model.push(Step::Over(Over::Scale(t.int_in(1_100_000i64..=1_900_000, "scale ppm"))));
    let root = built(t, &mut g, &model);
    let tape = tape_of(&g, root, alloc())?;
    let mut s = Scratch::new(alloc());
    let n = t.int_in(1usize..=21, "points");
    let (mut xs, mut ys, mut zs) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..n {
        let p = point(t);
        xs.push(p.x);
        ys.push(p.y);
        zs.push(p.z);
    }
    let pts = Points { x: &xs, y: &ys, z: &zs };
    for (lanes, name) in [(Lanes::Four, "four"), (Lanes::Eight, "eight"), (Lanes::One, "one")] {
        let mut out = vec![0.0; n];
        eval_points_with(&tape, pts, &mut out, lanes, &mut s, plant).map_err(harness)?;
        for i in 0..n {
            let scalar = eval_point(&tape, Vec3::new(xs[i], ys[i], zs[i]), &mut s);
            if !same_bits(scalar, out[i]) {
                return broken(CadBroken::LanesDiffer { lanes: name, scalar, vector: out[i] });
            }
        }
    }
    Ok(())
}

blaze_sim::plants! {
    enum LanePlant breaks CadBroken {
        None,
        VectorDivReciprocal => CadBroken::LanesDiffer { .. },
    }
}

#[test]
fn lanes_match_scalar() {
    check(Config::cases(400), |t| lanes(t, NoPlant));
}

#[test]
fn lanes_match_scalar_controls() {
    controls(Config::cases(400), |t, plant: LanePlant| lanes(t, plant));
}

// ---- 7. digests cover every parameter --------------------------------------------------------------

fn digests<P: Planted<CadPlant>>(t: &Draw, plant: P) -> R {
    let model = spec(t, ALL);
    let mut g1 = graph(plant);
    let r1 = built(t, &mut g1, &model);
    // History: the same model after other nodes.
    let mut g2 = graph(plant);
    let junk = spec(t, ALL);
    let _ = build(&mut g2, &junk);
    let r2 = built(t, &mut g2, &model);
    if g1.digest(r1) != g2.digest(r2) {
        return broken(CadBroken::DigestUnstable);
    }
    // One parameter of one leaf.
    let leaves: Vec<usize> = (0..model.len()).filter(|i| matches!(model[*i], Step::Leaf(_))).collect();
    let at = leaves[t.index(leaves.len(), "leaf")];
    let Step::Leaf(mut l) = model[at] else { unreachable!("chosen among leaves") };
    if t.coin("a primitive's parameter") {
        let k = t.index(l.prim.arity(), "parameter");
        l.prim = l.prim.bump(k);
    } else {
        let k = t.index(3, "axis of the shift");
        l.at[k] += 1_000;
    }
    let mut changed = model.clone();
    changed[at] = Step::Leaf(l);
    let mut g3 = graph(plant);
    let r3 = built(t, &mut g3, &changed);
    if g3.digest(r3) == g1.digest(r1) {
        return broken(CadBroken::DigestBlind { what: "one leaf's parameter" });
    }
    Ok(())
}

blaze_sim::plants! {
    enum DigestPlant breaks CadBroken {
        None,
        DigestDropsParameter => CadBroken::DigestBlind { .. },
    }
}

#[test]
fn digests_cover_every_parameter() {
    check(Config::cases(500), |t| digests(t, NoPlant));
}

#[test]
fn digests_cover_every_parameter_controls() {
    controls(Config::cases(500), |t, plant: DigestPlant| digests(t, plant));
}

// ---- 8. spellings share a digest -------------------------------------------------------------------

fn spellings<P: Planted<CadPlant>>(t: &Draw, plant: P) -> R {
    let lang = Lang { deform: false, ellipsoid: true, detail: true };
    let mut g = graph(plant);
    let (la, lb) = (leaf_spec(t, lang), leaf_spec(t, lang));
    let (Ok(a), Ok(b)) = (leaf(&mut g, &la), leaf(&mut g, &lb)) else { t.discard() };
    let same = |x: Result<Solid<Bound>, crate::CadError>, y: Result<Solid<Bound>, crate::CadError>, what| -> R {
        match (x, y) {
            (Ok(x), Ok(y)) if x.id() == y.id() => Ok(()),
            (Ok(_), Ok(_)) => broken(CadBroken::SpellingsDiffer { what }),
            (Err(e), _) | (_, Err(e)) => Err(harness(e)),
        }
    };
    let r = BlendOpts { radius: nm(um(t, 500, 4_000, "blend")), .. };
    let x = g.union(a, b);
    same(x, g.union(b, a), "union of a, b and of b, a")?;
    let x = g.intersect(a, b);
    same(x, g.intersect(b, a), "intersection of a, b and of b, a")?;
    let x = g.round_union(a, b, r);
    same(x, g.round_union(b, a, r), "rounded union of a, b and of b, a")?;
    let x = g.chamfer_intersect(a, b, r);
    same(x, g.chamfer_intersect(b, a, r), "chamfered intersection of a, b and of b, a")?;
    let e = g.empty().map_err(harness)?;
    same(g.union(a, e), Ok(a), "a union with nothing and a")?;
    let f = *t.pick(&[2.0, 4.0], "factor").unwrap_or(&2.0);
    let there = g.scale(a, ScaleOpts { factor: f, .. }).map_err(harness)?;
    same(g.scale(there, ScaleOpts { factor: 1.0 / f, .. }), Ok(a), "a scaled there and back and a")?;
    // Growing twice is growing once by the sum (an exact leaf).
    let mut le = la;
    le.prim = if matches!(le.prim, Prim::Ellipsoid(_)) { Prim::Sphere(4_000_000) } else { le.prim };
    le.wrap = Wrap::Plain;
    let exact = match le.prim {
        Prim::Sphere(r) => g.sphere(SphereOpts { radius: nm(r), .. }),
        Prim::Box(s, round) => g.cuboid(BoxOpts { size: s.map(nm), round: nm(round), .. }),
        Prim::Cylinder(r, h) => g.cylinder(CylinderOpts { radius: nm(r), height: nm(h), .. }),
        Prim::Capsule(r, len) => g.capsule(CapsuleOpts { radius: nm(r), length: nm(len), .. }),
        Prim::Cone(c0, c1, h) => g.cone(ConeOpts { bottom: nm(c0), top: nm(c1), height: nm(h), .. }),
        Prim::Torus(c0, c1) => g.torus(TorusOpts { major: nm(c0), minor: nm(c1), .. }),
        Prim::Ellipsoid(_) => unreachable!("replaced by a sphere above"),
    }
    .map_err(harness)?;
    let exact = g.place(exact, Placement::shift(nm(le.at[0]), nm(le.at[1]), nm(le.at[2]))).map_err(harness)?;
    let (r1, r2) = (um(t, 100, 2_000, "first grow"), um(t, 100, 2_000, "second grow"));
    let once = g.grow(exact, OffsetOpts { by: nm(r1), .. }).map_err(harness)?;
    let twice = g.grow(once, OffsetOpts { by: nm(r2), .. }).map(|s| s.weaken::<Bound>());
    let sum = g.grow(exact, OffsetOpts { by: nm(r1 + r2), .. }).map(|s| s.weaken::<Bound>());
    same(twice, sum, "grown twice and grown by the sum")
}

blaze_sim::plants! {
    enum SpellPlant breaks CadBroken {
        None,
        OperandsUnordered => CadBroken::SpellingsDiffer { .. },
    }
}

#[test]
fn spellings_share_a_digest() {
    check(Config::cases(400), |t| spellings(t, NoPlant));
}

#[test]
fn spellings_share_a_digest_controls() {
    controls(Config::cases(400), |t, plant: SpellPlant| spellings(t, plant));
}

// ---- 9. rotation keeps the geometry digest ----------------------------------------------------------

fn rotation<P: Planted<CadPlant>>(t: &Draw, plant: P) -> R {
    let mut g = graph(plant);
    let mut l = leaf_spec(t, Lang { deform: false, ellipsoid: true, detail: false });
    l.wrap = Wrap::Plain;
    if l.at == [0; 3] {
        l.at[0] = 1_000_000;
    }
    let Ok(part) = leaf(&mut g, &l) else { t.discard() };
    let axis = *t.pick(&[Axis::X, Axis::Y, Axis::Z], "rotation axis").unwrap_or(&Axis::Z);
    let turn = t.int_in(1i64..=Angle::<blaze_units::Plain>::PER_TURN - 1, "rotation");
    let turned = g.place(part, Placement::rotation(axis, Angle::microarcsec(turn))).map_err(harness)?;
    let (p0, geo0) = g.split_placement(part).map_err(harness)?;
    let (p1, geo1) = g.split_placement(turned).map_err(harness)?;
    if g.digest(geo0.id()) != g.digest(geo1.id()) {
        return broken(CadBroken::GeometryRekeyed);
    }
    if p0 == p1 && turned.id() != part.id() {
        return Err(Broken::Harness(std::format!("a rotation by {turn} micro-arcseconds kept the placement")));
    }
    Ok(())
}

blaze_sim::plants! {
    enum FramePlant breaks CadBroken {
        None,
        FrameKeptInGeometry => CadBroken::GeometryRekeyed,
    }
}

#[test]
fn rotation_keeps_geometry() {
    check(Config::cases(300), |t| rotation(t, NoPlant));
}

#[test]
fn rotation_keeps_geometry_controls() {
    controls(Config::cases(300), |t, plant: FramePlant| rotation(t, plant));
}

// ---- 10. one change rekeys exactly its path -----------------------------------------------------------

fn row_of_spheres<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, radii: &[i64], step: i64) -> Result<Solid<Bound>, crate::CadError> {
    let mut layer = Vec::new();
    for (i, r) in radii.iter().enumerate() {
        let s = g.sphere(SphereOpts { radius: nm(*r), .. })?;
        let x = (i as i64 + 1) * step;
        layer.push(g.place(s, Placement::shift(nm(x), Length::ZERO, Length::ZERO))?.weaken::<Bound>());
    }
    while layer.len() > 1 {
        let mut next = Vec::new();
        for pair in layer.chunks(2) {
            next.push(if pair.len() == 2 { g.union(pair[0], pair[1])? } else { pair[0] });
        }
        layer = next;
    }
    Ok(layer[0])
}

fn rekeys<P: Planted<CadPlant>>(t: &Draw, plant: P) -> R {
    let k = t.int_in(1u32..=6, "depth");
    let n = 1usize << k;
    let r0 = um(t, 2_000, 4_000, "radius");
    let mut radii = vec![r0; n];
    let mut g = graph(plant);
    row_of_spheres(&mut g, &radii, 10_000_000).map_err(harness)?;
    let before = g.len();
    let j = t.index(n, "the changed sphere");
    radii[j] = r0 + um(t, 1, 999, "change");
    row_of_spheres(&mut g, &radii, 10_000_000).map_err(harness)?;
    // A new unplaced sphere, its placed form, and the `k` unions above it.
    let expected = k as usize + 2;
    let made = g.len() - before;
    if made != expected {
        return broken(CadBroken::RekeyedTooMuch { expected, made });
    }
    Ok(())
}

blaze_sim::plants! {
    enum ConsPlant breaks CadBroken {
        None,
        NoHashConsing => CadBroken::RekeyedTooMuch { .. },
    }
}

#[test]
fn one_change_rekeys_its_path() {
    check(Config::cases(200), |t| rekeys(t, NoPlant));
}

#[test]
fn one_change_rekeys_its_path_controls() {
    controls(Config::cases(200), |t, plant: ConsPlant| rekeys(t, plant));
}

// ---- 11. region keys follow the change -------------------------------------------------------------

/// The key of the cell around a sphere of radius `ra` at `x = -60 mm`, in a model that also
/// holds `other` near `x = +60 mm`.
fn region_key<P: Planted<CadPlant>>(plant: P, ra: i64, other: &Leaf) -> Result<crate::Digest, Broken<CadBroken>> {
    let mut g = graph(plant);
    let a = g.sphere(SphereOpts { radius: nm(ra), .. }).map_err(harness)?;
    let a = g.place(a, Placement::shift(nm(-60_000_000), Length::ZERO, Length::ZERO)).map_err(harness)?;
    let b = leaf(&mut g, other).map_err(harness)?;
    let u = g.union(a.weaken::<Bound>(), b).map_err(harness)?;
    let tape = tape_of(&g, u.id(), alloc())?;
    let cell = Aabb3::new(Vec3::new(-0.062, -0.002, -0.002), Vec3::new(-0.058, 0.002, 0.002));
    let mut s = Scratch::new(alloc());
    let (_, choices) = eval_box(&tape, &cell, &mut s);
    let small = simplify_with(&tape, choices, alloc(), plant).map_err(harness)?;
    Ok(small.digest())
}

fn region_keys<P: Planted<CadPlant>>(t: &Draw, plant: P) -> R {
    let ra = um(t, 2_000, 8_000, "radius in the cell");
    let mut other = leaf_spec(t, Lang { deform: false, ellipsoid: true, detail: true });
    other.at = [60_000_000, 0, 0];
    let base = region_key(plant, ra, &other)?;
    let mut far = other;
    far.prim = far.prim.bump(t.index(far.prim.arity(), "far parameter"));
    if region_key(plant, ra, &far)? != base {
        return broken(CadBroken::RegionKey { what: "a change outside the cell changed its key" });
    }
    let near = ra + um(t, 1, 999, "change in the cell");
    if region_key(plant, near, &other)? == base {
        return broken(CadBroken::RegionKey { what: "a change inside the cell kept its key" });
    }
    Ok(())
}

blaze_sim::plants! {
    enum RegionPlant breaks CadBroken {
        None,
        TapeDigestSkipsConsts => CadBroken::RegionKey { .. },
    }
}

#[test]
fn region_keys_follow_the_change() {
    check(Config::cases(300), |t| region_keys(t, NoPlant));
}

#[test]
fn region_keys_follow_the_change_controls() {
    controls(Config::cases(300), |t, plant: RegionPlant| region_keys(t, plant));
}

// ---- 12. far parts drop out of cells ------------------------------------------------------------------

fn far_parts<P: Planted<CadPlant>>(t: &Draw, plant: P) -> R {
    let n = t.int_in(4usize..=16, "spheres");
    let step = um(t, 10_000, 16_000, "spacing");
    let r0 = um(t, 2_000, 4_000, "radius");
    // Distinct radii, a micrometre apart: each sphere's radius constant names it in a tape.
    let radii: Vec<i64> = (0..n).map(|i| r0 + i as i64 * 1_000).collect();
    let mut g = graph(plant);
    let root = row_of_spheres(&mut g, &radii, step).map_err(harness)?;
    let tape = tape_of(&g, root.id(), alloc())?;
    let rmax = mm(*radii.last().unwrap_or(&r0));
    let side = mm(r0) / 2.0;
    let lo = Vec3::new(mm(step) - rmax - side, -rmax - side, -rmax - side);
    let hi = Vec3::new(mm(step) * n as f64 + rmax + side, rmax + side, rmax + side);
    let count = |a: f64, b: f64| ((b - a) / side).ceil() as usize;
    let (nx, ny, nz) = (count(lo.x, hi.x), count(lo.y, hi.y), count(lo.z, hi.z));
    let mut s = Scratch::new(alloc());
    let (mut far, mut kept) = (0u32, 0u32);
    for i in 0..nx {
        for j in 0..ny {
            for k in 0..nz {
                let c0 = Vec3::new(lo.x + i as f64 * side, lo.y + j as f64 * side, lo.z + k as f64 * side);
                let cell = Aabb3::new(c0, c0 + Vec3::splat(side));
                let (iv, choices) = eval_box(&tape, &cell, &mut s);
                if !(iv.lo() <= 0.0 && iv.hi() >= 0.0) {
                    continue;
                }
                let small = simplify_with(&tape, choices, alloc(), plant).map_err(harness)?;
                let centre = cell.center();
                for (q, r) in radii.iter().enumerate() {
                    let sx = mm(step) * (q as f64 + 1.0);
                    let rr = mm(*r);
                    let gap = |c: f64, s: f64| ((c - s).abs() - side / 2.0 - rr).max(0.0);
                    let d = Vec3::new(gap(centre.x, sx), gap(centre.y, 0.0), gap(centre.z, 0.0)).length();
                    if d > 2.0 * side {
                        far += 1;
                        if small.holds_const(rr) {
                            kept += 1;
                        }
                    }
                }
            }
        }
    }
    if far > 0 && f64::from(kept) > 0.02 * f64::from(far) {
        return broken(CadBroken::FarKept { kept, far });
    }
    Ok(())
}

blaze_sim::plants! {
    enum FarPlant breaks CadBroken {
        None,
        SimplifyKeepsBothSides => CadBroken::FarKept { .. },
    }
}

#[test]
fn far_parts_drop_out_of_cells() {
    check(Config::cases(12), |t| far_parts(t, NoPlant));
}

#[test]
fn far_parts_drop_out_of_cells_controls() {
    controls(Config::cases(12), |t, plant: FarPlant| far_parts(t, plant));
}

// ---- 13. tapes round trip -------------------------------------------------------------------------------

fn round_trip(t: &Draw, plant: impl Planted<CadPlant>) -> R {
    let mut g = graph(NoPlant);
    let root = built(t, &mut g, &spec(t, ALL));
    let tape = tape_of(&g, root, alloc())?;
    let mut bytes = blaze_own::Vec::new_in(alloc());
    tape.encode_with(&mut bytes, plant);
    let Ok(back) = Tape::decode(&bytes, alloc(), Limits::SHIPPED) else {
        return broken(CadBroken::RoundTrip { what: "decode refused what encode wrote" });
    };
    if back.digest() != tape.digest() {
        return broken(CadBroken::RoundTrip { what: "the digest" });
    }
    let mut again = blaze_own::Vec::new_in(alloc());
    back.encode(&mut again);
    if again[..] != bytes[..] {
        return broken(CadBroken::RoundTrip { what: "the bytes" });
    }
    let mut s = Scratch::new(alloc());
    for _ in 0..8 {
        let p = point(t);
        if !same_bits(eval_point(&tape, p, &mut s), eval_point(&back, p, &mut s)) {
            return broken(CadBroken::RoundTrip { what: "the values" });
        }
    }
    Ok(())
}

blaze_sim::plants! {
    enum TripPlant breaks CadBroken {
        None,
        EncodeConstsAsF32 => CadBroken::RoundTrip { .. },
    }
}

#[test]
fn tapes_round_trip() {
    check(Config::cases(400), |t| round_trip(t, NoPlant));
}

#[test]
fn tapes_round_trip_controls() {
    controls(Config::cases(400), |t, plant: TripPlant| round_trip(t, plant));
}

into_cad! {
    ContainPlant { IntervalNotOutward }
    SimplifyPlant { SimplifyWrongSide }
    LevelPlant { EllipsoidExact, SmoothUnionWrongSign }
    GradPlant { GradAbsSignIgnored }
    RefPlant { PlacementShiftDropped }
    LanePlant { VectorDivReciprocal }
    GradLanePlant { VectorDivReciprocal }
    DigestPlant { DigestDropsParameter }
    SpellPlant { OperandsUnordered }
    FramePlant { FrameKeptInGeometry }
    ConsPlant { NoHashConsing }
    RegionPlant { TapeDigestSkipsConsts }
    FarPlant { SimplifyKeepsBothSides }
    TripPlant { EncodeConstsAsF32 }
}

// ---- 14. SIMD forward-mode AD lanes == scalar ---------------------------------------------------------

fn grad_lanes(t: &Draw, plant: impl Planted<CadPlant>) -> R {
    let mut g = graph(NoPlant);
    let mut model = spec(t, ALL);
    model.push(Step::Over(Over::Scale(t.int_in(1_100_000i64..=1_900_000, "scale ppm"))));
    let root = built(t, &mut g, &model);
    let tape = tape_of(&g, root, alloc())?;
    let mut s = Scratch::new(alloc());
    let n = t.int_in(1usize..=21, "points");
    let (mut xs, mut ys, mut zs) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..n {
        let p = point(t);
        xs.push(p.x);
        ys.push(p.y);
        zs.push(p.z);
    }
    let pts = Points { x: &xs, y: &ys, z: &zs };
    for (lanes, name) in [(Lanes::Four, "four"), (Lanes::Eight, "eight"), (Lanes::One, "one")] {
        let mut vals = vec![0.0; n];
        let mut grads = vec![Vec3::ZERO; n];
        eval_grads_with(&tape, pts, &mut vals, &mut grads, lanes, &mut s, plant).map_err(harness)?;
        for i in 0..n {
            let (sv, sg) = eval_grad(&tape, Vec3::new(xs[i], ys[i], zs[i]), &mut s);
            if !same_bits(sv, vals[i]) {
                return broken(CadBroken::GradLanesDiffer { lanes: name, component: "val", scalar: sv, vector: vals[i] });
            }
            for (comp, a, b) in [("gx", sg.x, grads[i].x), ("gy", sg.y, grads[i].y), ("gz", sg.z, grads[i].z)] {
                if !same_bits(a, b) {
                    return broken(CadBroken::GradLanesDiffer { lanes: name, component: comp, scalar: a, vector: b });
                }
            }
        }
    }
    Ok(())
}

blaze_sim::plants! {
    enum GradLanePlant breaks CadBroken {
        None,
        VectorDivReciprocal => CadBroken::GradLanesDiffer { .. },
    }
}

#[test]
fn grad_lanes_match_scalar() {
    check(Config::cases(400), |t| grad_lanes(t, NoPlant));
}

#[test]
fn grad_lanes_match_scalar_controls() {
    controls(Config::cases(400), |t, plant: GradLanePlant| grad_lanes(t, plant));
}

#[test]
fn eval_grads_rejects_mismatched_lengths() {
    let mut g = graph(NoPlant);
    let sp = g.sphere(SphereOpts { radius: nm(5_000_000), .. }).unwrap();
    let tape = compile(&g, sp.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let mut s = Scratch::new(alloc());
    let pts = Points { x: &[0.0, 1.0], y: &[0.0, 1.0], z: &[0.0, 1.0] };
    let mut vals = [0.0; 2];
    let mut grads = [Vec3::ZERO; 1];
    let err = eval_grads(&tape, pts, &mut vals, &mut grads, Lanes::Four, &mut s).unwrap_err();
    assert_eq!(err, EvalError::GradLengths { x: 2, y: 2, z: 2, vals: 2, grads: 1 });
}

// ---- 15. zero-overhead scratch reuse ------------------------------------------------------------------

#[test]
fn scratch_reuse_matches_fresh_across_varied_tapes() {
    let mut g = graph(NoPlant);
    let small_s = g.sphere(SphereOpts { radius: nm(5_000_000), .. }).unwrap();
    let mut big_parts = Vec::new();
    for i in 0..16 {
        let s = g.cuboid(BoxOpts { size: [nm(4_000_000); 3], round: nm(500_000), .. }).unwrap();
        let placed = g.place(s, Placement::shift(nm(i * 6_000_000), Length::ZERO, Length::ZERO)).unwrap();
        big_parts.push(placed);
    }
    let big_s = g.union_all(&big_parts).unwrap();
    let t_small = compile(&g, small_s.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let t_big = compile(&g, big_s.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    assert!(t_big.registers() > t_small.registers());

    let mut reused = Scratch::new(alloc());
    let xs = [-0.01, 0.0, 0.004, 0.012, 0.025, 0.05, 0.07, 0.09, 0.11];
    let ys = [0.001, -0.002, 0.003, 0.0, -0.001, 0.002, -0.003, 0.004, 0.0];
    let zs = [0.0, 0.001, -0.001, 0.002, -0.002, 0.0, 0.001, -0.001, 0.002];
    let pts = Points { x: &xs, y: &ys, z: &zs };
    let b = Aabb3::new(Vec3::new(-0.02, -0.02, -0.02), Vec3::new(0.05, 0.02, 0.02));

    for tape in [&t_big, &t_small, &t_big, &t_small] {
        let mut fresh = Scratch::new(alloc());
        for i in 0..xs.len() {
            let p = Vec3::new(xs[i], ys[i], zs[i]);
            assert!(same_bits(eval_point(tape, p, &mut reused), eval_point(tape, p, &mut fresh)));
            let (v1, g1) = eval_grad(tape, p, &mut reused);
            let (v2, g2) = eval_grad(tape, p, &mut fresh);
            assert!(same_bits(v1, v2) && same_bits(g1.x, g2.x) && same_bits(g1.y, g2.y) && same_bits(g1.z, g2.z));
        }
        for lanes in [Lanes::One, Lanes::Four, Lanes::Eight] {
            let (mut o1, mut o2) = (vec![0.0; xs.len()], vec![0.0; xs.len()]);
            eval_points(tape, pts, &mut o1, lanes, &mut reused).unwrap();
            eval_points(tape, pts, &mut o2, lanes, &mut fresh).unwrap();
            for (a, b) in o1.iter().zip(o2.iter()) {
                assert!(same_bits(*a, *b));
            }
            let (mut v1, mut v2) = (vec![0.0; xs.len()], vec![0.0; xs.len()]);
            let (mut g1, mut g2) = (vec![Vec3::ZERO; xs.len()], vec![Vec3::ZERO; xs.len()]);
            eval_grads(tape, pts, &mut v1, &mut g1, lanes, &mut reused).unwrap();
            eval_grads(tape, pts, &mut v2, &mut g2, lanes, &mut fresh).unwrap();
            for i in 0..xs.len() {
                assert!(same_bits(v1[i], v2[i]) && same_bits(g1[i].x, g2[i].x) && same_bits(g1[i].y, g2[i].y) && same_bits(g1[i].z, g2[i].z));
            }
        }
        let (iv1, ch1) = eval_box(tape, &b, &mut reused);
        let (iv2, ch2) = eval_box(tape, &b, &mut fresh);
        assert!(same_bits(iv1.lo(), iv2.lo()) && same_bits(iv1.hi(), iv2.hi()));
        assert_eq!(ch1.as_slice(), ch2.as_slice());
    }
}

// ---- 16. balanced CSG combinators ---------------------------------------------------------------------

fn graph_depth<P: Planted<CadPlant>>(g: &Graph<'_, P>, id: NodeId) -> usize {
    let (kids, n) = g.node(id).children();
    let mut best = 0;
    for &k in &kids[..n] {
        best = best.max(graph_depth(g, k));
    }
    1 + best
}

#[test]
fn balanced_csg_combinators_and_sweep_polyline() {
    let mut g = graph(NoPlant);
    // Empty slice rejected with typed error.
    let empty_ex: &[Solid<Exact>] = &[];
    assert!(matches!(
        g.union_all(empty_ex),
        Err(CadError::Argument { kind: NodeKind::Union, arg: "solids", value: crate::Value::Count(0), .. })
    ));
    assert!(matches!(
        g.intersect_all(empty_ex),
        Err(CadError::Argument { kind: NodeKind::Intersect, arg: "solids", value: crate::Value::Count(0), .. })
    ));
    assert!(matches!(
        g.round_union_all(empty_ex, BlendOpts { radius: nm(1_000_000), .. }),
        Err(CadError::Argument { kind: NodeKind::SmoothBoolean, arg: "solids", .. })
    ));

    // Level preservation & balanced tree depth.
    let mut spheres: Vec<Solid<Exact>> = Vec::new();
    for i in 0..16 {
        let sp = g.sphere(SphereOpts { radius: nm(4_000_000), .. }).unwrap();
        spheres.push(g.place(sp, Placement::shift(nm(i * 6_000_000), Length::ZERO, Length::ZERO)).unwrap());
    }
    let u_all: Solid<Outside> = g.union_all(&spheres).unwrap();
    assert_eq!(g.level(u_all.id()), Level::Outside);
    assert_eq!(graph_depth(&g, u_all.id()), 5); // 16 leaves -> 4 union levels + 1 leaf

    let i_all: Solid<Inside> = g.intersect_all(&spheres).unwrap();
    assert_eq!(g.level(i_all.id()), Level::Inside);
    assert_eq!(graph_depth(&g, i_all.id()), 5);

    // Changing one leaf in `union_all` rekeys only `k + 2` nodes.
    let before = g.len();
    let changed = g.sphere(SphereOpts { radius: nm(4_500_000), .. }).unwrap();
    spheres[7] = g.place(changed, Placement::shift(nm(7 * 6_000_000), Length::ZERO, Length::ZERO)).unwrap();
    let _u_all2 = g.union_all(&spheres).unwrap();
    assert_eq!(g.len() - before, 4 + 2);

    // Blended / chamfered balanced combinators.
    let ru: Solid<Bound> = g.round_union_all(&spheres[..4], BlendOpts { radius: nm(1_000_000), .. }).unwrap();
    let ci: Solid<Bound> = g.chamfer_intersect_all(&spheres[..4], BlendOpts { radius: nm(1_000_000), .. }).unwrap();
    assert_eq!(g.level(ru.id()), Level::Bound);
    assert_eq!(g.level(ci.id()), Level::Bound);

    // `sweep_polyline` builds a balanced binary union tree instead of a left-deep chain.
    let prof = g.profile(ProfileOpts { profile: Profile::Circle { radius: nm(1_000_000) }, .. }).unwrap();
    let mut pts = Vec::new();
    for i in 0..=16 {
        pts.push([nm(i * 10_000_000), nm((i % 2) * 2_000_000), Length::ZERO]);
    }
    let swept = g.sweep_polyline(prof, SweepPolylineOpts { points: &pts, .. }).unwrap();
    // 16 segments -> balanced union depth 4 + per-segment mitre cuts (depth <= 5) => total <= 10 (vs >= 17 for left-deep).
    let d = graph_depth(&g, swept.id());
    assert!(d <= 10, "expected balanced polyline sweep depth <= 10, got {d}");
    let tape = compile(&g, swept.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let mut sc = Scratch::new(alloc());
    // Midpoint of first segment must be inside the swept tube.
    assert!(eval_point(&tape, Vec3::new(0.005, 0.001, 0.0), &mut sc) < 0.0);
}

// ---- 17. TPMS gyroid primitive ------------------------------------------------------------------------

fn gyroid_prop(t: &Draw) -> R {
    let period = um(t, 2_000, 20_000, "period");
    let thickness = um(t, 200, 3_000, "thickness");
    let mut g = graph(NoPlant);
    let gyr = g.gyroid(GyroidOpts { period: nm(period), thickness: nm(thickness), .. }).map_err(harness)?;
    let tape = tape_of(&g, gyr.id(), alloc())?;
    let mut sc = Scratch::new(alloc());

    // Reference agreement + 1-Lipschitz + forward-mode AD gradient vs finite differences.
    let axes = [Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 0.0, 1.0)];
    let (mut xs, mut ys, mut zs) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..10 {
        let p = point(t);
        xs.push(p.x);
        ys.push(p.y);
        zs.push(p.z);
        let got = eval_point(&tape, p, &mut sc);
        let want = reference(&g, gyr.id(), p).expect("gyroid in reference");
        if (got - want).abs() > 1e-12 {
            return broken(CadBroken::ReferenceDiffers { tape: got, reference: want });
        }
        // 1-Lipschitz check.
        let step = 10f64.powf(t.f64_in(-5.0..=-2.5, "log10 step"));
        let dir = Vec3::new(t.f64_in(-1.0..=1.0, "dx"), t.f64_in(-1.0..=1.0, "dy"), t.f64_in(-1.0..=1.0, "dz"));
        if let Some(d) = dir.normalized() {
            let q = p + d * step;
            let fq = eval_point(&tape, q, &mut sc);
            let dist = (q - p).length();
            if (got - fq).abs() > dist * (1.0 + 1e-9) + 1e-15 {
                return broken(CadBroken::NotLipschitz { slope: (got - fq).abs() / dist });
            }
        }
        // Gradient check where smooth (forward and backward differences agree, so symmetric
        // V-kinks of `|g| = 0` are not mistaken for smooth points with zero slope).
        let (_, grad) = eval_grad(&tape, p, &mut sc);
        let h = 1e-6;
        let fwd = axes.map(|e| (eval_point(&tape, p + e * h, &mut sc) - got) / h);
        let bwd = axes.map(|e| (got - eval_point(&tape, p - e * h, &mut sc)) / h);
        if (0..3).all(|i| fwd[i].is_finite() && bwd[i].is_finite() && (fwd[i] - bwd[i]).abs() <= 1e-3) {
            let numeric = [0.5 * (fwd[0] + bwd[0]), 0.5 * (fwd[1] + bwd[1]), 0.5 * (fwd[2] + bwd[2])];
            let analytic = grad.to_array();
            if (0..3).any(|i| (analytic[i] - numeric[i]).abs() > 1e-2) {
                return broken(CadBroken::GradientOff { analytic, numeric });
            }
        }
    }

    // SIMD 4/8-lane grad bit-equality on gyroid.
    let pts = Points { x: &xs, y: &ys, z: &zs };
    for (lanes, name) in [(Lanes::Four, "four"), (Lanes::Eight, "eight")] {
        let mut vals = vec![0.0; xs.len()];
        let mut grads = vec![Vec3::ZERO; xs.len()];
        eval_grads(&tape, pts, &mut vals, &mut grads, lanes, &mut sc).map_err(harness)?;
        for i in 0..xs.len() {
            let (sv, sg) = eval_grad(&tape, Vec3::new(xs[i], ys[i], zs[i]), &mut sc);
            if !same_bits(sv, vals[i]) {
                return broken(CadBroken::GradLanesDiffer { lanes: name, component: "val", scalar: sv, vector: vals[i] });
            }
            if !same_bits(sg.x, grads[i].x) || !same_bits(sg.y, grads[i].y) || !same_bits(sg.z, grads[i].z) {
                return broken(CadBroken::GradLanesDiffer { lanes: name, component: "g", scalar: sg.x, vector: grads[i].x });
            }
        }
    }

    // Interval containment over random box.
    let b = cell(t);
    let (iv, _) = eval_box(&tape, &b, &mut sc);
    for p in points_in(t, &b) {
        let v = eval_point(&tape, p, &mut sc);
        if !iv.contains(v) {
            return broken(CadBroken::Escapes { instr: 0, op: "gyroid", value: v, lo: iv.lo(), hi: iv.hi() });
        }
    }

    // BCFT encode / decode round-trip.
    let mut bytes = blaze_own::Vec::new_in(alloc());
    tape.encode(&mut bytes);
    let Ok(back) = Tape::decode(&bytes, alloc(), Limits::SHIPPED) else {
        return broken(CadBroken::RoundTrip { what: "gyroid decode" });
    };
    if back.digest() != tape.digest() {
        return broken(CadBroken::RoundTrip { what: "gyroid digest" });
    }
    Ok(())
}

#[test]
fn gyroid_properties() {
    check(Config::cases(200), gyroid_prop);
}

#[test]
fn gyroid_bounded_intersection_and_validation() {
    let mut g = graph(NoPlant);
    assert!(matches!(
        g.gyroid(GyroidOpts { period: Length::ZERO, thickness: nm(1_000_000), .. }),
        Err(CadError::Argument { kind: NodeKind::Gyroid, arg: "period", .. })
    ));
    assert!(matches!(
        g.gyroid(GyroidOpts { period: nm(10_000_000), thickness: Length::ZERO, .. }),
        Err(CadError::Argument { kind: NodeKind::Gyroid, arg: "thickness", .. })
    ));
    let gyr = g.gyroid(GyroidOpts { period: nm(10_000_000), thickness: nm(1_000_000), .. }).unwrap();
    let cube = g.cuboid(BoxOpts { size: [nm(20_000_000); 3], .. }).unwrap();
    let lattice = g.intersect(gyr, cube.weaken::<Bound>()).unwrap();
    assert_eq!(g.aabb(lattice.id()), g.aabb(cube.id()));
    let tape = compile(&g, lattice.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let mut sc = Scratch::new(alloc());
    // Origin lies on g(0,0,0) = 0, so inside the gyroid sheet (f = -thickness/2).
    let v0 = eval_point(&tape, Vec3::ZERO, &mut sc);
    assert!((v0 - (-0.0005)).abs() < 1e-12);
    // Ray from outside hits the bounded gyroid lattice.
    let hit = ray(&tape, RayOpts { origin: Vec3::new(-0.05, 0.0, 0.0), dir: Vec3::new(1.0, 0.0, 0.0), max_t: 0.2, .. }, &mut sc).unwrap();
    assert!(hit.is_some());
}

// ---- 18. fast inspection: ray AABB clip & distance_between early-exit ---------------------------------

#[test]
fn ray_aabb_clipping_skips_misses_and_jumps_to_entry() {
    let mut g = graph(NoPlant);
    let sp = g.sphere(SphereOpts { radius: nm(10_000_000), .. }).unwrap();
    let t_sp = compile(&g, sp.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let mut sc = Scratch::new(alloc());

    // Ray missing AABB returns None with zero steps allowed!
    let miss = ray(
        &t_sp,
        RayOpts { origin: Vec3::new(-1.0, 0.5, 0.0), dir: Vec3::new(1.0, 0.0, 0.0), max_t: 10.0, most_steps: 0, .. },
        &mut sc,
    )
    .unwrap();
    assert!(miss.is_none());

    // Ray starting 1000 m away hits exact sphere surface in <= 4 steps after jumping to t_enter.
    let far_hit = ray(
        &t_sp,
        RayOpts { origin: Vec3::new(-1000.0, 0.0, 0.0), dir: Vec3::new(1.0, 0.0, 0.0), max_t: 2000.0, most_steps: 4, .. },
        &mut sc,
    )
    .unwrap()
    .expect("far ray hits sphere");
    assert!((far_hit.t - 999.99).abs() < 1e-9, "got t = {}", far_hit.t);
    let n = far_hit.normal.expect("sphere normal");
    assert!((n.x - (-1.0)).abs() < 1e-9 && n.y.abs() < 1e-9 && n.z.abs() < 1e-9);

    // Ray hitting a box face dead-on (surface coincides with AABB face).
    let bx = g.cuboid(BoxOpts { size: [nm(20_000_000); 3], .. }).unwrap();
    let t_bx = compile(&g, bx.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let face_hit = ray(
        &t_bx,
        RayOpts { origin: Vec3::new(-5.0, 0.0, 0.0), dir: Vec3::new(1.0, 0.0, 0.0), max_t: 20.0, .. },
        &mut sc,
    )
    .unwrap()
    .expect("ray hits box face");
    assert!((face_hit.t - 4.99).abs() < 1e-9, "got t = {}", face_hit.t);

    // Ray starting inside returns t = 0.
    let inside_hit = ray(
        &t_sp,
        RayOpts { origin: Vec3::ZERO, dir: Vec3::new(1.0, 0.0, 0.0), max_t: 1.0, most_steps: 0, .. },
        &mut sc,
    )
    .unwrap()
    .expect("inside hit");
    assert_eq!(inside_hit.t, 0.0);

    // Empty solid misses immediately.
    let empty = g.empty().unwrap();
    let t_empty = compile(&g, empty.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    assert!(ray(&t_empty, RayOpts { origin: Vec3::ZERO, dir: Vec3::new(1.0, 0.0, 0.0), max_t: 10.0, most_steps: 0, .. }, &mut sc).unwrap().is_none());

    // Partially unbounded HalfSpace (z <= 0).
    let hs = g.half_space(HalfSpaceOpts { .. }).unwrap();
    let t_hs = compile(&g, hs.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let hs_hit = ray(
        &t_hs,
        RayOpts { origin: Vec3::new(0.0, 0.0, 2.0), dir: Vec3::new(0.0, 0.0, -1.0), max_t: 10.0, .. },
        &mut sc,
    )
    .unwrap()
    .expect("hits half-space");
    assert!((hs_hit.t - 2.0).abs() < 1e-9);
    // Ray above z = 0 pointing +z misses immediately.
    let hs_miss = ray(
        &t_hs,
        RayOpts { origin: Vec3::new(0.0, 0.0, 2.0), dir: Vec3::new(0.0, 0.0, 1.0), max_t: 10.0, most_steps: 0, .. },
        &mut sc,
    )
    .unwrap();
    assert!(hs_miss.is_none());
}

#[test]
fn distance_between_aabb_lb_and_early_stops() {
    let mut g = graph(NoPlant);
    let s1 = g.sphere(SphereOpts { radius: nm(10_000_000), .. }).unwrap();
    let s2 = g.place(s1, Placement::shift(nm(100_000_000), Length::ZERO, Length::ZERO)).unwrap();
    let t1 = compile(&g, s1.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let t2 = compile(&g, s2.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let mut sc = Scratch::new(alloc());

    // True gap between spheres of r=0.01m at x=0 and x=0.10m is 0.08m.
    // AABB gap is also 0.08m, so `stop_above: Some(0.05)` settles with ZERO boxes!
    let early_above = distance_between_stopping(
        &t1,
        &t2,
        DistanceStopOpts { tolerance: 1e-6, most_boxes: 0, stop_above: Some(0.05), .. },
        &mut sc,
    )
    .unwrap();
    match early_above {
        Gap::Settled(iv) => assert!(iv.lo() >= 0.05, "expected lb >= 0.05, got {:?}", iv),
        other => panic!("expected early Settled above, got {other:?}"),
    }

    // `stop_below: Some(0.12)` settles early once upper bound < 0.12m (at root centre ub = 0.09m).
    let early_below = distance_between_stopping(
        &t1,
        &t2,
        DistanceStopOpts { tolerance: 1e-9, most_boxes: 1, stop_below: Some(0.12), .. },
        &mut sc,
    )
    .unwrap();
    match early_below {
        Gap::Settled(iv) => assert!(iv.hi() < 0.12 && iv.lo() <= 0.08 + 1e-12, "got {:?}", iv),
        other => panic!("expected early Settled below, got {other:?}"),
    }

    // Standard `distance_between` settles accurately around 0.08m.
    let exact_gap = distance_between(&t1, &t2, DistanceOpts { tolerance: 1e-4, .. }, &mut sc).unwrap();
    match exact_gap {
        Gap::Settled(iv) => assert!(iv.lo() <= 0.08 + 1e-9 && iv.hi() >= 0.08 - 1e-9 && iv.hi() - iv.lo() <= 1e-4, "got {:?}", iv),
        other => panic!("expected Settled, got {other:?}"),
    }

    // Empty solid returns Gap::Empty.
    let empty = g.empty().unwrap();
    let t_empty = compile(&g, empty.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    assert_eq!(distance_between(&t_empty, &t2, DistanceOpts { tolerance: 1e-4, .. }, &mut sc).unwrap(), Gap::Empty);
}

// ---- 19. Grid3 coarse min-max block summary -----------------------------------------------------------

#[test]
fn grid3_coarse_summary_sound_and_round_trips() {
    // Build a non-multiple-of-4 grid (29 x 25 x 33) sampling a sphere SDF so both 4^3 and 16^3
    // block summaries exercise partial tail blocks and full blocks.
    let dims = [29u32, 25u32, 33u32];
    let step_nm = 1_000_000i64; // 1 mm
    let step = step_nm as f64 / 1e9;
    let origin_nm = [-14_000_000i64, -12_000_000i64, -16_000_000i64];
    let origin = origin_nm.map(|v| v as f64 / 1e9);
    let r = 0.009;
    let mut values = Vec::with_capacity((dims[0] * dims[1] * dims[2]) as usize);
    for k in 0..dims[2] {
        for j in 0..dims[1] {
            for i in 0..dims[0] {
                let p = Vec3::new(origin[0] + i as f64 * step, origin[1] + j as f64 * step, origin[2] + k as f64 * step);
                values.push(p.length() - r);
            }
        }
    }
    let mut g = graph(NoPlant);
    let sp = g.sphere(SphereOpts { radius: nm(9_000_000), .. }).unwrap();
    let rd = g.redistance(sp).unwrap();
    g.attach_grid(
        rd,
        GridOpts {
            origin: origin_nm.map(nm),
            step: nm(step_nm),
            dims,
            values: &values,
            band: nm(2_000_000),
            ..
        },
    )
    .unwrap();
    let tape = compile(&g, rd.id(), CompileOpts { alloc: alloc(), .. }).unwrap();

    // Round-trip through BCFT encode/decode.
    let mut bytes = blaze_own::Vec::new_in(alloc());
    tape.encode(&mut bytes);
    let decoded = Tape::decode(&bytes, alloc(), Limits::SHIPPED).unwrap();
    assert_eq!(decoded.digest(), tape.digest());

    let mut sc = Scratch::new(alloc());
    // Query boxes spanning: (a) tiny <= 64 voxels, (b) medium 4^3 block level, (c) full grid 16^3 superblock level, (d) outside grid.
    let boxes = [
        Aabb3::new(Vec3::new(-0.0015, -0.0015, -0.0015), Vec3::new(0.0015, 0.0015, 0.0015)),
        Aabb3::new(Vec3::new(-0.008, -0.007, -0.006), Vec3::new(0.008, 0.007, 0.006)),
        Aabb3::new(Vec3::new(-0.015, -0.013, -0.017), Vec3::new(0.015, 0.013, 0.017)),
        Aabb3::new(Vec3::new(0.02, 0.02, 0.02), Vec3::new(0.03, 0.03, 0.03)),
    ];
    for b in &boxes {
        let (iv, _) = eval_box(&tape, b, &mut sc);
        let (iv_dec, _) = eval_box(&decoded, b, &mut sc);
        assert!(same_bits(iv.lo(), iv_dec.lo()) && same_bits(iv.hi(), iv_dec.hi()));
        // Check all 8 corners + centre + intermediate samples lie inside the coarse interval.
        for c in b.corners().into_iter().chain(core::iter::once(b.center())) {
            let v = eval_point(&tape, c, &mut sc);
            assert!(iv.contains(v), "point {c:?} value {v} escaped grid_box interval {iv:?}");
        }
    }

    // Verify SIMD 4/8-lane `eval_grads` matches scalar `eval_grad` on `Grid3` bit-for-bit!
    let xs = [-0.01, -0.003, 0.0, 0.004, 0.009, 0.013, 0.02, -0.02, 0.001];
    let ys = [0.002, -0.005, 0.0, 0.003, -0.004, 0.011, 0.0, 0.02, -0.002];
    let zs = [-0.001, 0.004, 0.0, -0.006, 0.002, -0.012, 0.015, -0.018, 0.003];
    let pts = Points { x: &xs, y: &ys, z: &zs };
    for lanes in [Lanes::One, Lanes::Four, Lanes::Eight] {
        let mut vals = vec![0.0; xs.len()];
        let mut grads = vec![Vec3::ZERO; xs.len()];
        eval_grads(&tape, pts, &mut vals, &mut grads, lanes, &mut sc).unwrap();
        for i in 0..xs.len() {
            let (sv, sg) = eval_grad(&tape, Vec3::new(xs[i], ys[i], zs[i]), &mut sc);
            assert!(same_bits(sv, vals[i]));
            assert!(same_bits(sg.x, grads[i].x) && same_bits(sg.y, grads[i].y) && same_bits(sg.z, grads[i].z));
        }
    }
}

// ---- 20. rail-guided lofts, variable blends/offsets, and frame sweeps ---------------------------------

fn check_box_contains_samples(tape: &Tape<'_>, b: &Aabb3, sc: &mut Scratch<'_>) {
    let (iv, _) = eval_box(tape, b, sc);
    let c = b.center();
    for p in b.corners().into_iter().chain(core::iter::once(c)) {
        let v = eval_point(tape, p, sc);
        assert!(iv.contains(v), "point {p:?} value {v} escaped box interval {iv:?} for {b:?}");
    }
    // Also probe quarter-points along diagonals.
    for corner in b.corners() {
        let q = c + (corner - c) * 0.5;
        let v = eval_point(tape, q, sc);
        assert!(iv.contains(v), "interior point {q:?} value {v} escaped box interval {iv:?} for {b:?}");
    }
}

fn check_grad_matches_fd(tape: &Tape<'_>, p: Vec3, sc: &mut Scratch<'_>) {
    let (v, grad) = eval_grad(tape, p, sc);
    assert!(v.is_finite(), "non-finite value at {p:?}");
    let axes = [Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 0.0, 1.0)];
    let h = 1e-6;
    let fwd = axes.map(|e| (eval_point(tape, p + e * h, sc) - v) / h);
    let bwd = axes.map(|e| (v - eval_point(tape, p - e * h, sc)) / h);
    if (0..3).all(|i| fwd[i].is_finite() && bwd[i].is_finite() && (fwd[i] - bwd[i]).abs() <= 1e-3) {
        let num = [0.5 * (fwd[0] + bwd[0]), 0.5 * (fwd[1] + bwd[1]), 0.5 * (fwd[2] + bwd[2])];
        let ana = grad.to_array();
        for i in 0..3 {
            assert!(
                (ana[i] - num[i]).abs() < 1e-2,
                "gradient mismatch at {p:?} axis {i}: analytic={} numeric={}",
                ana[i],
                num[i]
            );
        }
    }
}

#[test]
fn guided_loft_rails_continuity_and_invariants() {
    let mut g = graph(NoPlant);
    let p_circ = g.profile(ProfileOpts { profile: Profile::Circle { radius: Length::mm(10.0) }, .. }).unwrap();
    let p_rrect = g
        .profile(ProfileOpts {
            profile: Profile::RoundedRect { width: Length::mm(20.0), height: Length::mm(20.0), radius: Length::mm(5.0) },
            ..
        })
        .unwrap();

    // Natural C2 side rail: exact mid-span value at z = 25 mm is 19.0625 mm (0.0190625 m).
    let side_pts = [
        RailPoint { z: Length::mm(0.0), value: Length::mm(12.0), tangent: None },
        RailPoint { z: Length::mm(50.0), value: Length::mm(22.0), tangent: None },
        RailPoint { z: Length::mm(100.0), value: Length::mm(10.0), tangent: None },
    ];
    // Clamped-end C2 ridge rail (zero slope at z = 0 and z = 100 mm).
    let ridge_pts = [
        RailPoint { z: Length::mm(0.0), value: Length::mm(8.0), tangent: Some(0.0) },
        RailPoint { z: Length::mm(50.0), value: Length::mm(14.0), tangent: None },
        RailPoint { z: Length::mm(100.0), value: Length::mm(6.0), tangent: Some(0.0) },
    ];
    let stations = [
        GuidedStation { z: Length::mm(0.0), profile: p_circ, .. },
        GuidedStation { z: Length::mm(50.0), profile: p_rrect, .. },
        GuidedStation { z: Length::mm(100.0), profile: p_circ, .. },
    ];
    let loft = g
        .guided_loft(GuidedLoftOpts {
            stations: &stations,
            side_rail: Some(GuideRail { points: &side_pts }),
            ridge_rail: Some(GuideRail { points: &ridge_pts }),
            continuity: LoftContinuity::CurvatureC2,
            ..
        })
        .unwrap();
    assert_eq!(g.level(loft.id()), Level::Bound);

    let tape = compile(&g, loft.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let mut sc = Scratch::new(alloc());

    // Exact widths at stations & mid-span z = 25 mm.
    for &(z, x_half) in &[(0.0, 0.012), (0.025, 0.0190625), (0.050, 0.022), (0.100, 0.010)] {
        let on = eval_point(&tape, Vec3::new(x_half, 0.0, z), &mut sc);
        assert!(on.abs() < 1e-9, "expected surface at x={x_half}, z={z}, got {on}");
        let z_in = z.clamp(0.002, 0.098);
        if (z - z_in).abs() < 1e-9 {
            assert!(eval_point(&tape, Vec3::new(x_half - 1e-4, 0.0, z), &mut sc) < 0.0);
            assert!(eval_point(&tape, Vec3::new(x_half + 1e-4, 0.0, z), &mut sc) > 0.0);
        }
    }
    // Exact ridge half-heights at stations.
    for &(z, y_half) in &[(0.0, 0.008), (0.050, 0.014), (0.100, 0.006)] {
        let on = eval_point(&tape, Vec3::new(0.0, y_half, z), &mut sc);
        assert!(on.abs() < 1e-9, "expected ridge surface at y={y_half}, z={z}, got {on}");
    }

    // Smooth C1/C2 normal continuity across interior station boundary z = 50 mm.
    let eps = 1e-6;
    for &(x, y) in &[(0.022, 0.0), (0.0, 0.014), (0.015, 0.010)] {
        let (_, g_lo) = eval_grad(&tape, Vec3::new(x, y, 0.050 - eps), &mut sc);
        let (_, g_hi) = eval_grad(&tape, Vec3::new(x, y, 0.050 + eps), &mut sc);
        let n_lo = g_lo.normalized().expect("non-zero gradient below station");
        let n_hi = g_hi.normalized().expect("non-zero gradient above station");
        let jump = (n_hi - n_lo).length();
        assert!(jump < 1e-4, "normal discontinuity at ({x}, {y}, 0.05): jump={jump}");
    }

    // Interval soundness across surface, station boundary, cap, and guard boundary.
    let test_boxes = [
        Aabb3::new(Vec3::new(0.015, -0.005, 0.020), Vec3::new(0.025, 0.005, 0.030)),
        Aabb3::new(Vec3::new(-0.025, -0.016, 0.045), Vec3::new(0.025, 0.016, 0.055)),
        Aabb3::new(Vec3::new(-0.015, -0.010, -0.005), Vec3::new(0.015, 0.010, 0.005)),
        Aabb3::new(Vec3::new(0.018, -0.004, 0.040), Vec3::new(0.030, 0.004, 0.060)),
        Aabb3::new(Vec3::new(0.040, 0.040, 0.040), Vec3::new(0.060, 0.060, 0.060)),
    ];
    for b in &test_boxes {
        check_box_contains_samples(&tape, b, &mut sc);
    }

    // SIMD lane parity (`eval_points` and `eval_grads` match scalar bit-for-bit).
    let sample_pts = [
        Vec3::new(0.0, 0.0, 0.025),
        Vec3::new(0.019, 0.002, 0.025),
        Vec3::new(0.022, 0.0, 0.050),
        Vec3::new(-0.010, 0.011, 0.065),
        Vec3::new(0.005, -0.007, 0.085),
        Vec3::new(0.015, 0.008, 0.015),
        Vec3::new(0.035, 0.0, 0.050),
        Vec3::new(0.0, 0.0, 0.105),
        Vec3::new(-0.014, -0.005, 0.035),
    ];
    let xs: Vec<f64> = sample_pts.iter().map(|p| p.x).collect();
    let ys: Vec<f64> = sample_pts.iter().map(|p| p.y).collect();
    let zs: Vec<f64> = sample_pts.iter().map(|p| p.z).collect();
    let pts = Points { x: &xs, y: &ys, z: &zs };
    for lanes in [Lanes::One, Lanes::Four, Lanes::Eight] {
        let mut out = vec![0.0; sample_pts.len()];
        eval_points(&tape, pts, &mut out, lanes, &mut sc).unwrap();
        let mut vals = vec![0.0; sample_pts.len()];
        let mut grads = vec![Vec3::ZERO; sample_pts.len()];
        eval_grads(&tape, pts, &mut vals, &mut grads, lanes, &mut sc).unwrap();
        for (i, &p) in sample_pts.iter().enumerate() {
            let sv = eval_point(&tape, p, &mut sc);
            assert!(same_bits(sv, out[i]));
            let (gv, sg) = eval_grad(&tape, p, &mut sc);
            assert!(same_bits(gv, vals[i]));
            assert!(same_bits(sg.x, grads[i].x) && same_bits(sg.y, grads[i].y) && same_bits(sg.z, grads[i].z));
        }
    }

    // Gradient parity vs finite differences & 1-Lipschitz check inside guard.
    for &p in &sample_pts[..6] {
        check_grad_matches_fd(&tape, p, &mut sc);
        let q = p + Vec3::new(2e-4, -1.5e-4, 2.5e-4);
        let (fp, fq) = (eval_point(&tape, p, &mut sc), eval_point(&tape, q, &mut sc));
        assert!((fq - fp).abs() <= (q - p).length() * (1.0 + 1e-6) + 1e-12);
    }

    // Digest sensitivity & deduplication: changing one rail control point changes digest;
    // rebuilding identical loft reuses the exact same NodeId.
    let loft_dup = g
        .guided_loft(GuidedLoftOpts {
            stations: &stations,
            side_rail: Some(GuideRail { points: &side_pts }),
            ridge_rail: Some(GuideRail { points: &ridge_pts }),
            continuity: LoftContinuity::CurvatureC2,
            ..
        })
        .unwrap();
    assert_eq!(loft_dup.id(), loft.id());

    let mut side_bumped = side_pts;
    side_bumped[1].value = Length::mm(22.5);
    let loft_changed = g
        .guided_loft(GuidedLoftOpts {
            stations: &stations,
            side_rail: Some(GuideRail { points: &side_bumped }),
            ridge_rail: Some(GuideRail { points: &ridge_pts }),
            continuity: LoftContinuity::CurvatureC2,
            ..
        })
        .unwrap();
    assert_ne!(g.digest(loft_changed.id()), g.digest(loft.id()));

    // Center rails, twist, and cap_round compile and stay interval-sound.
    let cx_pts = [
        RailPoint { z: Length::mm(0.0), value: Length::mm(0.0), tangent: None },
        RailPoint { z: Length::mm(100.0), value: Length::mm(8.0), tangent: None },
    ];
    let twisted_stations = [
        GuidedStation { z: Length::mm(0.0), profile: p_rrect, scale: [1.0, 0.8], twist: Angle::ZERO, .. },
        GuidedStation { z: Length::mm(100.0), profile: p_rrect, scale: [1.3, 1.1], twist: Angle::degrees(30.0), .. },
    ];
    let twisted = g
        .guided_loft(GuidedLoftOpts {
            stations: &twisted_stations,
            center_x_rail: Some(GuideRail { points: &cx_pts }),
            continuity: LoftContinuity::SmoothC1,
            cap_round: Length::mm(3.0),
            ..
        })
        .unwrap();
    let t_tw = compile(&g, twisted.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    // Centerline at z = 50 mm is shifted to x = 4 mm (inside the solid).
    assert!(eval_point(&t_tw, Vec3::new(0.004, 0.0, 0.050), &mut sc) < 0.0);
    check_box_contains_samples(&t_tw, &Aabb3::new(Vec3::new(-0.015, -0.015, -0.002), Vec3::new(0.025, 0.015, 0.020)), &mut sc);

    // Validation errors.
    assert!(matches!(
        g.guided_loft(GuidedLoftOpts { stations: &stations[..1], .. }),
        Err(CadError::Argument { kind: NodeKind::GuidedLoft, arg: "stations", .. })
    ));
    let bad_rail = [
        RailPoint { z: Length::mm(0.0), value: Length::mm(0.0), tangent: None },
        RailPoint { z: Length::mm(100.0), value: Length::mm(10.0), tangent: None },
    ];
    assert!(matches!(
        g.guided_loft(GuidedLoftOpts { stations: &stations, side_rail: Some(GuideRail { points: &bad_rail }), .. }),
        Err(CadError::Argument { kind: NodeKind::GuidedLoft, arg: "side_rail", .. })
    ));
}

#[test]
fn variable_blends_and_offsets_selective_and_sound() {
    let mut g = graph(NoPlant);
    let box_a = g.cuboid(BoxOpts { size: [Length::mm(40.0), Length::mm(10.0), Length::mm(40.0)], .. }).unwrap();
    let box_b = g.cuboid(BoxOpts { size: [Length::mm(10.0), Length::mm(40.0), Length::mm(40.0)], .. }).unwrap();
    let sharp = g.union(box_a, box_b).unwrap();

    // 1. Variable round union with Linear profile along (+5, +5, z) selected by Capsule.
    let v_round = g
        .variable_round_union(
            box_a,
            box_b,
            VariableBlendOpts {
                profile: RadiusProfile::Linear {
                    from: [Length::mm(5.0), Length::mm(5.0), Length::mm(-20.0)],
                    to: [Length::mm(5.0), Length::mm(5.0), Length::mm(20.0)],
                    r0: Length::mm(2.0),
                    r1: Length::mm(6.0),
                },
                select: EdgeSelect::Capsule {
                    a: [Length::mm(5.0), Length::mm(5.0), Length::mm(-20.0)],
                    b: [Length::mm(5.0), Length::mm(5.0), Length::mm(20.0)],
                    radius: Length::mm(4.0),
                    fade: Length::mm(3.0),
                },
                ..
            },
        )
        .unwrap();

    let t_sharp = compile(&g, sharp.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let t_round = compile(&g, v_round.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let mut sc = Scratch::new(alloc());

    // Selected edge (+5, +5, z) is filleted with tapered radius: r(-15mm)=2.5mm, r(+15mm)=5.5mm.
    let f_sel_lo = eval_point(&t_round, Vec3::new(0.005, 0.005, -0.015), &mut sc);
    let f_sel_hi = eval_point(&t_round, Vec3::new(0.005, 0.005, 0.015), &mut sc);
    assert!(f_sel_lo < 0.0 && f_sel_hi < 0.0, "selected edge must be filleted: lo={f_sel_lo}, hi={f_sel_hi}");
    let ratio = f_sel_hi / f_sel_lo;
    assert!((ratio - (5.5 / 2.5)).abs() < 1e-6, "expected tapered fillet ratio 2.2, got {ratio}");

    // Unselected opposite edge (-5, -5, z) stays sharp (zero fillet, exact sharp zero-set).
    let f_unsel_edge = eval_point(&t_round, Vec3::new(-0.005, -0.005, 0.0), &mut sc);
    assert!(f_unsel_edge.abs() < 1e-12, "unselected edge must stay sharp (0.0), got {f_unsel_edge}");
    let p_unsel_out = Vec3::new(-0.0055, -0.0055, 0.0);
    let f_unsel_out = eval_point(&t_round, p_unsel_out, &mut sc);
    let f_sharp_out = eval_point(&t_sharp, p_unsel_out, &mut sc);
    assert!(f_unsel_out > 0.0 && f_sharp_out > 0.0);
    // Right outside the selected edge at the same diagonal offset (0.5mm), the fillet pulls it inside (< 0).
    assert!(eval_point(&t_round, Vec3::new(0.0055, 0.0055, 0.015), &mut sc) < 0.0);

    // 2. Variable round difference with Curve profile and EdgeSelect::Box.
    let block = g.cuboid(BoxOpts { size: [Length::mm(30.0), Length::mm(30.0), Length::mm(30.0)], .. }).unwrap();
    let slot = g.cuboid(BoxOpts { size: [Length::mm(12.0), Length::mm(12.0), Length::mm(40.0)], .. }).unwrap();
    let slot = g.place(slot, Placement::shift(Length::mm(0.0), Length::mm(12.0), Length::mm(0.0))).unwrap();
    let stops = [
        RadiusStop { at: Length::mm(-15.0), radius: Length::mm(1.0) },
        RadiusStop { at: Length::mm(0.0), radius: Length::mm(4.0) },
        RadiusStop { at: Length::mm(15.0), radius: Length::mm(1.0) },
    ];
    let v_diff = g
        .variable_round_difference(
            block,
            slot,
            VariableBlendOpts {
                profile: RadiusProfile::Curve { origin: [Length::ZERO; 3], axis: [0.0, 0.0, 1.0], stops: &stops },
                select: EdgeSelect::Box {
                    lo: [Length::mm(-10.0), Length::mm(4.0), Length::mm(-16.0)],
                    hi: [Length::mm(10.0), Length::mm(16.0), Length::mm(16.0)],
                    fade: Length::mm(2.0),
                },
                ..
            },
        )
        .unwrap();
    let t_diff = compile(&g, v_diff.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    // Pocket lip edge where block (+Y=15mm) meets slot (+X=6mm): more rounded at z = 0 (r = 4mm) than z = 14mm (r ≈ 1mm).
    let d_mid = eval_point(&t_diff, Vec3::new(0.006, 0.015, 0.0), &mut sc);
    let d_end = eval_point(&t_diff, Vec3::new(0.006, 0.015, 0.014), &mut sc);
    assert!(d_mid > d_end && d_end > 0.0, "expected larger difference round at z=0: mid={d_mid} end={d_end}");

    // 3. Variable chamfer union with EdgeSelect::Ball.
    let v_chamf = g
        .variable_chamfer_union(
            box_a,
            box_b,
            VariableBlendOpts {
                profile: RadiusProfile::Constant(Length::mm(4.0)),
                select: EdgeSelect::Ball {
                    center: [Length::mm(5.0), Length::mm(5.0), Length::ZERO],
                    radius: Length::mm(4.0),
                    fade: Length::mm(2.0),
                },
                ..
            },
        )
        .unwrap();
    let t_chamf = compile(&g, v_chamf.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    assert!(eval_point(&t_chamf, Vec3::new(0.005, 0.005, 0.0), &mut sc) < 0.0);
    assert!(eval_point(&t_chamf, Vec3::new(-0.005, -0.005, 0.0), &mut sc).abs() < 1e-12);

    // 4. Variable grow & variable erode with EdgeSelect::Box.
    let cube = g.cuboid(BoxOpts { size: [Length::mm(20.0); 3], .. }).unwrap();
    let sel_pos_x = EdgeSelect::Box {
        lo: [Length::mm(6.0), Length::mm(-12.0), Length::mm(-12.0)],
        hi: [Length::mm(14.0), Length::mm(12.0), Length::mm(12.0)],
        fade: Length::mm(2.0),
    };
    let lin_z = RadiusProfile::Linear {
        from: [Length::ZERO, Length::ZERO, Length::mm(-10.0)],
        to: [Length::ZERO, Length::ZERO, Length::mm(10.0)],
        r0: Length::mm(1.0),
        r1: Length::mm(3.0),
    };
    let v_grow = g.variable_grow(cube, VariableOffsetOpts { profile: lin_z, select: sel_pos_x, .. }).unwrap();
    let v_erode = g.variable_erode(cube, VariableOffsetOpts { profile: lin_z, select: sel_pos_x, .. }).unwrap();
    let t_grow = compile(&g, v_grow.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let t_erode = compile(&g, v_erode.id(), CompileOpts { alloc: alloc(), .. }).unwrap();

    // Selected +X face moves outward for grow and inward for erode; unselected -X face stays at x = -10 mm.
    assert!(eval_point(&t_grow, Vec3::new(0.010, 0.0, 0.0), &mut sc) < 0.0);
    assert!(eval_point(&t_grow, Vec3::new(0.012, 0.0, 0.0), &mut sc).abs() < 1e-9);
    assert!(eval_point(&t_grow, Vec3::new(-0.010, 0.0, 0.0), &mut sc).abs() < 1e-12);

    assert!(eval_point(&t_erode, Vec3::new(0.010, 0.0, 0.0), &mut sc) > 0.0);
    assert!(eval_point(&t_erode, Vec3::new(0.008, 0.0, 0.0), &mut sc).abs() < 1e-9);
    assert!(eval_point(&t_erode, Vec3::new(-0.010, 0.0, 0.0), &mut sc).abs() < 1e-12);

    // Interval soundness, 1-Lipschitz, and gradient parity across all variable tapes.
    let probe_boxes = [
        Aabb3::new(Vec3::new(0.002, 0.002, -0.012), Vec3::new(0.012, 0.012, 0.012)),
        Aabb3::new(Vec3::new(-0.012, -0.012, -0.008), Vec3::new(-0.002, -0.002, 0.008)),
        Aabb3::new(Vec3::new(0.003, -0.005, -0.005), Vec3::new(0.014, 0.005, 0.005)),
    ];
    let probe_pts = [
        Vec3::new(0.006, 0.006, 0.005),
        Vec3::new(0.009, 0.007, -0.008),
        Vec3::new(-0.007, -0.008, 0.002),
        Vec3::new(0.009, 0.002, 0.004),
    ];
    for tape in [&t_round, &t_diff, &t_chamf, &t_grow, &t_erode] {
        for b in &probe_boxes {
            check_box_contains_samples(tape, b, &mut sc);
        }
        for &p in &probe_pts {
            check_grad_matches_fd(tape, p, &mut sc);
            let q = p + Vec3::new(3e-4, -2e-4, 2.5e-4);
            let (fp, fq) = (eval_point(tape, p, &mut sc), eval_point(tape, q, &mut sc));
            assert!((fq - fp).abs() <= (q - p).length() * (1.0 + 1e-6) + 1e-12);
        }
    }

    // Validation rejects fade below 1 micrometre.
    assert!(matches!(
        g.variable_round_union(
            box_a,
            box_b,
            VariableBlendOpts {
                profile: RadiusProfile::Constant(Length::mm(2.0)),
                select: EdgeSelect::Ball { center: [Length::ZERO; 3], radius: Length::mm(5.0), fade: Length::nm(500) },
                ..
            },
        ),
        Err(CadError::Argument { kind: NodeKind::VariableSmoothBoolean, arg: "fade", .. })
    ));
}

#[test]
fn sweep_frames_tapered_channel_and_circular_bead() {
    let mut g = graph(NoPlant);
    let r = 0.050; // 50 mm radius quarter-circle helical arc
    let n = 9usize;
    let mut frames = Vec::with_capacity(n);
    for k in 0..n {
        let u = (k as f64) / ((n - 1) as f64);
        let th = core::f64::consts::FRAC_PI_2 * u;
        let (s, c) = blaze_math::sin_cos(th);
        let z = 0.010 * u;
        frames.push(SweepFrame {
            point: [Length::mm(r * c * 1e3), Length::mm(r * s * 1e3), Length::mm(z * 1e3)],
            tangent: [-r * s * core::f64::consts::FRAC_PI_2, r * c * core::f64::consts::FRAC_PI_2, 0.010],
            normal: [c, s, 0.0],
            width: Length::mm(4.0 + 6.0 * u), // 4 mm -> 10 mm
            depth: Length::mm(2.0 + 4.0 * u), // 2 mm -> 6 mm
        });
    }

    let channel = g
        .sweep_frames(SweepFramesOpts { frames: &frames, section: SweepCrossSection::Channel, closed: false, .. })
        .unwrap();
    let t_chan = compile(&g, channel.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    let mut sc = Scratch::new(alloc());

    // Endpoints of an open sweep sit on the end caps (`f ≈ 0`); every interior frame and segment
    // midpoint is strictly inside the swept channel.
    for i in 0..n {
        let p = Vec3::new(frames[i].point[0].as_m(), frames[i].point[1].as_m(), frames[i].point[2].as_m());
        let fp = eval_point(&t_chan, p, &mut sc);
        if i == 0 || i + 1 == n {
            assert!(fp.abs() < 1e-6, "frame {i} endpoint should sit on end cap: fp={fp}");
        } else {
            assert!(fp < 0.0, "frame {i} point not inside swept channel: fp={fp}");
        }
        if i + 1 < n {
            let q = Vec3::new(frames[i + 1].point[0].as_m(), frames[i + 1].point[1].as_m(), frames[i + 1].point[2].as_m());
            let mid = (p + q) * 0.5;
            assert!(eval_point(&t_chan, mid, &mut sc) < 0.0, "segment {i} midpoint not inside swept channel");
        }
    }

    // Verify tapered depth (along normal) and width (along binormal) near start vs near end.
    let mut seg_check = |idx: usize, depth_in: f64, depth_out: f64, width_in: f64, width_out: f64| {
        let a = Vec3::new(frames[idx].point[0].as_m(), frames[idx].point[1].as_m(), frames[idx].point[2].as_m());
        let b = Vec3::new(frames[idx + 1].point[0].as_m(), frames[idx + 1].point[1].as_m(), frames[idx + 1].point[2].as_m());
        let mid = (a + b) * 0.5;
        let z_ax = (b - a).normalized().unwrap();
        let n_avg = (Vec3::from_array(frames[idx].normal) + Vec3::from_array(frames[idx + 1].normal)).normalized().unwrap();
        let y_ax = (n_avg - z_ax * n_avg.dot(z_ax)).normalized().unwrap();
        let x_ax = y_ax.cross(z_ax).normalized().unwrap();
        assert!(eval_point(&t_chan, mid + y_ax * depth_in, &mut sc) < 0.0);
        assert!(eval_point(&t_chan, mid + y_ax * depth_out, &mut sc) > 0.0);
        assert!(eval_point(&t_chan, mid + x_ax * width_in, &mut sc) < 0.0);
        assert!(eval_point(&t_chan, mid + x_ax * width_out, &mut sc) > 0.0);
    };
    // Segment 0: width ≈ 4.375mm (half ≈ 2.19mm), depth ≈ 2.25mm (half ≈ 1.125mm).
    seg_check(0, 0.0005, 0.0020, 0.0015, 0.0035);
    // Segment 7: width ≈ 9.625mm (half ≈ 4.81mm), depth ≈ 5.75mm (half ≈ 2.875mm).
    seg_check(7, 0.0020, 0.0040, 0.0035, 0.0060);

    // Interval soundness over boxes along the swept channel.
    check_box_contains_samples(&t_chan, &Aabb3::new(Vec3::new(0.044, -0.005, -0.004), Vec3::new(0.056, 0.015, 0.006)), &mut sc);
    check_box_contains_samples(&t_chan, &Aabb3::new(Vec3::new(-0.005, 0.044, 0.004), Vec3::new(0.015, 0.056, 0.016)), &mut sc);

    // Closed circular bead with smooth joins around a 6-frame ring.
    let mut ring = Vec::with_capacity(6);
    for k in 0..6 {
        let th = 2.0 * core::f64::consts::PI * (k as f64) / 6.0;
        let (s, c) = blaze_math::sin_cos(th);
        let d = if k % 2 == 0 { 4.0 } else { 6.0 };
        ring.push(SweepFrame {
            point: [Length::mm(30.0 * c), Length::mm(30.0 * s), Length::ZERO],
            tangent: [-s, c, 0.0],
            normal: [0.0, 0.0, 1.0],
            width: Length::mm(d),
            depth: Length::mm(d),
        });
    }
    let bead = g
        .sweep_frames(SweepFramesOpts {
            frames: &ring,
            section: SweepCrossSection::Circular,
            closed: true,
            smooth_join: Length::mm(0.5),
            ..
        })
        .unwrap();
    let t_bead = compile(&g, bead.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
    for i in 0..6 {
        let j = (i + 1) % 6;
        let a = Vec3::new(ring[i].point[0].as_m(), ring[i].point[1].as_m(), 0.0);
        let b = Vec3::new(ring[j].point[0].as_m(), ring[j].point[1].as_m(), 0.0);
        assert!(eval_point(&t_bead, a, &mut sc) < 0.0);
        assert!(eval_point(&t_bead, (a + b) * 0.5, &mut sc) < 0.0);
    }
    check_box_contains_samples(&t_bead, &Aabb3::new(Vec3::new(0.020, -0.010, -0.006), Vec3::new(0.036, 0.010, 0.006)), &mut sc);

    // Exercise HalfRound, VGroove, and Rectangular cross sections.
    for sec in [
        SweepCrossSection::HalfRound,
        SweepCrossSection::VGroove,
        SweepCrossSection::Rectangular { round: Length::mm(0.4) },
    ] {
        let s = g.sweep_frames(SweepFramesOpts { frames: &frames[..3], section: sec, .. }).unwrap();
        let tp = compile(&g, s.id(), CompileOpts { alloc: alloc(), .. }).unwrap();
        let mid = Vec3::new(
            0.5 * (frames[0].point[0].as_m() + frames[1].point[0].as_m()),
            0.5 * (frames[0].point[1].as_m() + frames[1].point[1].as_m()),
            0.5 * (frames[0].point[2].as_m() + frames[1].point[2].as_m()),
        );
        assert!(eval_point(&tp, mid, &mut sc) < 0.0);
    }

    // Validation checks.
    assert!(matches!(
        g.sweep_frames(SweepFramesOpts { frames: &frames[..1], .. }),
        Err(CadError::Argument { kind: NodeKind::SweepFrames, arg: "frames", .. })
    ));
    assert!(matches!(
        g.sweep_frames(SweepFramesOpts { frames: &frames[..2], closed: true, .. }),
        Err(CadError::Argument { kind: NodeKind::SweepFrames, arg: "frames", .. })
    ));
}
