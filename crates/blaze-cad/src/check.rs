//! Design and DFM checks: [`min_wall`], [`no_interference`], [`no_interference_assembly`],
//! [`fits_within`], [`overhang_check`], and [`draft_angle_check`]. Each reads `Detail::Shape`
//! tapes (a detail relief changes no verdict), returns a three-way [`Verdict`] (a check that ran
//! out of budget or could not settle says so: it never reports a pass it did not prove), and
//! carries the [`CheckKey`] its verdict is a record under.
//!
//! # What each proves
//!
//! - [`min_wall`]: THE WALL AT A SURFACE POINT `p` is the solid's extent through `p` along the
//!   inward normal: the distance from `p` along `-∇f/|∇f|` to where the ray first leaves the
//!   solid, when it leaves through a face within 60 degrees of facing `p`'s (the wall's far
//!   side). The wall is THIN at `p` when that is under `wall`. A face's wall is the plate's
//!   thickness; at a sharp convex edge or corner the normal is one face's or a direction between
//!   them, and the ray either crosses the part (a chord at least the thinner plate meeting
//!   there) or runs along the other face and leaves through it, square to the ray, or past a
//!   convex face curving away from it: no far side, no wall. So a 90-degree edge of a thick part
//!   is never thin, which a ball-fits definition gets wrong (the planted `TangentBall`), and a
//!   knife edge sharper than 60 degrees is.
//!
//!   SAMPLING. The surface is sampled on the cells of side `wall / samples_per_wall` (the
//!   SAMPLING CELLS) that the search does not clear. During octree descent, decided tape choices
//!   are simplified hierarchically (`simplify`), and child cell centres are pre-screened in
//!   8-wide SIMD (`eval_points(..., Lanes::Eight, ...)`). A cell is CLEARED when the field at its
//!   centre is farther from zero than its half diagonal (`|f| <= distance` at every level, so no
//!   boundary point is in the cell: one evaluation), or, above and below the sampling size, when
//!   an interval over it excludes zero.
//! - [`no_interference`]: `distance_between_stopping` settles the gap with early-exit bounds
//!   `stop_below = clearance` and `stop_above = clearance`, so clearly separated or colliding
//!   parts terminate in O(1) boxes; a gap whose upper end is under the clearance is a finding,
//!   one whose lower end is at or over it a pass, and anything else (the clearance inside the
//!   settled interval, or the budget spent first) is [`Verdict::Unknown`]: NEVER A PASS.
//! - [`no_interference_assembly`]: all-pairs interference check over `&[Part]` with O(1) AABB
//!   broad-phase pruning (`aabb.gap(&other_aabb) >= clearance`) and deterministic `(i, j)` pair
//!   ordering.
//! - [`fits_within`]: the part without the envelope, accelerated by hierarchical tape
//!   simplification; a point of it deeper than the tolerance is an overhang, a box search whose
//!   every cell's interval stays above minus the tolerance is a pass.
//! - [`overhang_check`]: 3D-printing DFM check — samples surface cells via the verified octree
//!   and surface projection, computes outward unit normals via `eval_grad`, and flags downward-
//!   facing faces exceeding `max_angle` from vertical (`dot(n, build_dir) < -sin(max_angle)`),
//!   excluding build-plate contact (`dot(p, build_dir) <= z_min + plate_tolerance`).
//! - [`draft_angle_check`]: injection-molding / casting DFM check — verifies side walls relative
//!   to mold `pull_dir`, reporting insufficient draft or undercut locations (`dot(n, pull_dir) <
//!   sin(min_draft)`) excluding parting-plane base contact (`dot(p, pull_dir) <= z_min +
//!   parting_tolerance`).
//!
//! # Records
//!
//! A verdict is a record under its [`CheckKey`]: the check, the digests of the shape tapes it
//! read, its arguments in nanometres / micro-arcseconds and [`CHECK_SCHEMA`]. Moving a part
//! changes no key of a check of that part alone (its tape is unplaced); editing a detail node
//! changes none (the tapes are `Detail::Shape`).

#![blaze::sans_io]

use blaze_cad_field::{
    AtLeastInside, Bound, CadPlant, Choices, CompileOpts, Detail, Digest, DistanceStopOpts, Ex, Gap, Graph, InspectError, Lanes,
    Level, Name, Points, Scratch, Solid, Tape, compile, distance_between_stopping, eval_box, eval_grad, eval_point, eval_points,
    simplify,
};
use blaze_math::{Aabb3, Mat3, Vec3};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, Vec};
use blaze_text::{Put, Show, Writes};
use blaze_units::{Angle, Length};

use crate::{DraftPlant, FitPlant, InterferencePlant, ModelError, OverhangPlant, Part, WallPlant, placed_tape};

/// The checks' record layout; bumped when a verdict's meaning or a key's fields change, or when
/// the same key can get another verdict (2: the wall is the inward chord; 3: [`min_wall`]'s
/// samples are bracketed, so places once unsettled now settle).
pub const CHECK_SCHEMA: u32 = 3;

/// Minimum tape instruction count worth running `simplify` on during octree descent.
const MIN_SIMPLIFY_INSTRS: usize = 16;

/// Micro-arcseconds in one degree (`3600 * 10^6`).
pub(crate) const UAS_PER_DEGREE: i64 = 3_600_000_000;

/// Which check a key is of.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CheckKind {
    MinWall,
    NoInterference,
    FitsWithin,
    NoInterferenceAssembly,
    Overhang,
    DraftAngle,
}

/// The key a verdict is a record under (see the module).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CheckKey {
    pub check: CheckKind,
    /// The shape tapes' digests (sorted, for the symmetric [`no_interference`]; the second is
    /// zero for a check of one tape; folded deterministically for an N-part assembly).
    pub tapes: [Digest; 2],
    /// The arguments, nanometres, micro-arcseconds, or counts, in the check's order.
    pub args: [i64; 3],
    pub schema: u32,
}

/// Why a check did not settle.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Unsettled {
    /// The box budget was spent after `boxes`.
    Budget { boxes: u64 },
    /// `cells` cells at the finest size were neither proved clear nor held a witness; `near` is
    /// the first one's centre.
    Open { cells: u64, near: [Length; 3] },
    /// The gap is settled to `[lo, hi]` and the clearance lies inside it.
    Straddles { lo: Length, hi: Length },
}

/// A check's answer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Verdict<F> {
    /// Proved to hold.
    Pass,
    /// Proved broken, with the witness.
    Found(F),
    /// Neither proved: never to be read as a pass.
    Unknown(Unsettled),
}

impl<F> Verdict<F> {
    /// Whether the check is proved to hold.
    pub fn passed(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

/// A check's answer with its key and its cost.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Checked<F> {
    pub verdict: Verdict<F>,
    pub key: CheckKey,
    /// Cells the search looked at (the inspector's own are not counted).
    pub boxes: u64,
    /// Surface sampling cells looked at (cleared or sampled); zero for checks without surface
    /// sampling.
    pub samples: u64,
    /// Field evaluations at points the search made (a value with its gradient counts once): the
    /// cost model's unit (see the module).
    pub points: u64,
}

/// A thin place: `at` is on the part's surface, and the part's extent through it along the
/// inward normal is `thickness`, under `wall`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ThinWall {
    pub at: [Length; 3],
    pub thickness: Length,
    pub wall: Length,
}

/// Two parts nearer than the clearance: the gap lies in `gap` (zero when they meet).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Interference {
    pub gap: [Length; 2],
    pub clearance: Length,
}

/// A pair of parts in an assembly nearer than the clearance.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AssemblyInterference {
    /// Indices `(first, second)` into the input `parts` slice (`first < second`).
    pub pair: [u32; 2],
    /// Names of the two parts.
    pub names: [Name; 2],
    /// The gap lies in `gap` (zero when they meet).
    pub gap: [Length; 2],
    pub clearance: Length,
}

/// A point of the part outside the envelope: `depth` is the difference's field there.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Overhang {
    pub at: [Length; 3],
    pub depth: Length,
}

/// A downward-facing surface point whose 3D-printing overhang angle from vertical exceeds
/// `max_angle`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OverhangFace {
    /// Point on the part's surface (in the assembly / world frame).
    pub at: [Length; 3],
    /// Outward unit normal at `at` (in the assembly / world frame).
    pub normal: [f64; 3],
    /// Measured overhang angle from vertical (`0°` = vertical wall, `90°` = flat downward face).
    pub angle: Angle,
    /// Maximum overhang angle allowed by the check.
    pub max_angle: Angle,
}

/// A surface point with insufficient draft angle or an undercut relative to `pull_dir`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DraftFace {
    /// Point on the part's surface (in the assembly / world frame).
    pub at: [Length; 3],
    /// Outward unit normal at `at` (in the assembly / world frame).
    pub normal: [f64; 3],
    /// Measured draft angle (`asin(n · pull_dir)`): positive = tapered, zero = vertical wall,
    /// negative = undercut.
    pub angle: Angle,
    /// Minimum draft angle required by the check.
    pub min_draft: Angle,
    /// True when the face slopes inward (`n · pull_dir < 0`, an undercut).
    pub undercut: bool,
}

// ---- printing for people (and language models) -------------------------------------------
//
// A finding prints as one sentence, millimetres and degrees throughout, by integer arithmetic
// from the nanometres / micro-arcseconds: never rounded through a float to be shown.

/// Write `q / scale` with up to `places` decimals, dropping trailing zeros and omitting `-` on zero.
fn put_fixed<P: Put + ?Sized>(out: &mut P, negative: bool, q: u128, scale: u128, places: u8) {
    let (whole, mut frac) = (q / scale, q % scale);
    if negative && q != 0 {
        out.put_byte(b'-');
    }
    out.int(whole);
    if frac != 0 {
        let mut digits = usize::from(places);
        while frac % 10 == 0 {
            frac /= 10;
            digits -= 1;
        }
        out.put_byte(b'.');
        out.int_zeros(frac, digits);
    }
}

/// `nm` in millimetres to `places` decimals (at most 6), halves away from zero, trailing zeros
/// dropped; no minus sign on a value that prints as zero.
pub(crate) fn put_mm<P: Put + ?Sized>(out: &mut P, nm: i64, places: u8) {
    let places = places.min(6);
    let unit = 10u128.pow(u32::from(6 - places));
    let scale = 10u128.pow(u32::from(places));
    let q = (i128::from(nm).unsigned_abs() + unit / 2) / unit;
    put_fixed(out, nm < 0, q, scale, places);
}

/// `uas` (micro-arcseconds) in degrees to `places` decimals (at most 6), halves away from zero,
/// trailing zeros dropped; no minus sign on a value that prints as zero.
pub(crate) fn put_deg<P: Put + ?Sized>(out: &mut P, uas: i64, places: u8) {
    const PER_DEG: u128 = 3_600_000_000;
    let places = places.min(6);
    let scale = 10u128.pow(u32::from(places));
    let mag = i128::from(uas).unsigned_abs();
    let q = (mag * scale + PER_DEG / 2) / PER_DEG;
    put_fixed(out, uas < 0, q, scale, places);
}

/// `(x, y, z) mm` to `places` decimals.
pub(crate) fn put_point<P: Put + ?Sized>(out: &mut P, at: &[Length; 3], places: u8) {
    out.put_byte(b'(');
    for (k, c) in at.iter().enumerate() {
        if k > 0 {
            out.put_str(", ");
        }
        put_mm(out, c.as_nm(), places);
    }
    out.put_str(") mm");
}

/// "thin wall of about 4.2 mm at (295.898, 290.039, -0.586) mm"
impl Show for ThinWall {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str("thin wall of about ");
        put_mm(out, self.thickness.as_nm(), 1);
        out.put_str(" mm at ");
        put_point(out, &self.at, 3);
    }
}

/// "gap of 1.2 to 1.21 mm, under the 5 mm clearance", or "parts meet, under the 5 mm clearance"
impl Show for Interference {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        if self.gap[1].as_nm() <= 0 {
            out.put_str("parts meet");
        } else {
            out.put_str("gap of ");
            put_mm(out, self.gap[0].as_nm(), 3);
            out.put_str(" to ");
            put_mm(out, self.gap[1].as_nm(), 3);
            out.put_str(" mm");
        }
        out.put_str(", under the ");
        put_mm(out, self.clearance.as_nm(), 3);
        out.put_str(" mm clearance");
    }
}

/// "cover and deck: gap of 1.2 to 1.21 mm, under the 5 mm clearance"
impl Show for AssemblyInterference {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str(self.names[0].as_str());
        out.put_str(" and ");
        out.put_str(self.names[1].as_str());
        out.put_str(": ");
        Interference { gap: self.gap, clearance: self.clearance }.show(out);
    }
}

/// "sticks out of the envelope by about 3.5 mm at (1, 2, 3) mm"
impl Show for Overhang {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str("sticks out of the envelope by about ");
        put_mm(out, self.depth.as_nm(), 1);
        out.put_str(" mm at ");
        put_point(out, &self.at, 3);
    }
}

/// "overhang of 90 deg (limit 45 deg) at (15, 0, 20) mm"
impl Show for OverhangFace {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str("overhang of ");
        put_deg(out, self.angle.as_microarcsec(), 1);
        out.put_str(" deg (limit ");
        put_deg(out, self.max_angle.as_microarcsec(), 1);
        out.put_str(" deg) at ");
        put_point(out, &self.at, 3);
    }
}

/// "undercut of -9.5 deg (need at least 2 deg draft) at (17.5, 0, 0) mm" or
/// "insufficient draft of 0 deg (need at least 2 deg) at (10, 0, 0) mm"
impl Show for DraftFace {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        if self.undercut {
            out.put_str("undercut of ");
            put_deg(out, self.angle.as_microarcsec(), 1);
            out.put_str(" deg (need at least ");
            put_deg(out, self.min_draft.as_microarcsec(), 1);
            out.put_str(" deg draft) at ");
        } else {
            out.put_str("insufficient draft of ");
            put_deg(out, self.angle.as_microarcsec(), 1);
            out.put_str(" deg (need at least ");
            put_deg(out, self.min_draft.as_microarcsec(), 1);
            out.put_str(" deg) at ");
        }
        put_point(out, &self.at, 3);
    }
}

impl Show for Unsettled {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            Unsettled::Budget { boxes } => {
                out.put_str("not settled: the budget ran out after ");
                out.int(*boxes);
                out.put_str(" boxes");
            }
            Unsettled::Open { cells, near } => {
                out.put_str("not settled: ");
                out.int(*cells);
                out.put_str(" places, the first near ");
                put_point(out, near, 3);
            }
            Unsettled::Straddles { lo, hi } => {
                out.put_str("not settled: the gap is ");
                put_mm(out, lo.as_nm(), 3);
                out.put_str(" to ");
                put_mm(out, hi.as_nm(), 3);
                out.put_str(" mm, and the clearance lies inside");
            }
        }
    }
}

/// "pass", the finding's sentence, or why it is not settled.
impl<F: Show> Show for Verdict<F> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            Verdict::Pass => out.put_str("pass"),
            Verdict::Found(f) => f.show(out),
            Verdict::Unknown(u) => u.show(out),
        }
    }
}

// ---- hierarchical tape simplification & SIMD helpers -------------------------------------

/// A simplified tape valid everywhere inside `region`, active while `stack.len() > stack_base`.
struct TapeFrame<'s> {
    tape: Tape<'s>,
    region: Aabb3,
    stack_base: usize,
}

#[inline]
fn contains_box(outer: &Aabb3, inner: &Aabb3) -> bool {
    outer.lo.x <= inner.lo.x
        && outer.lo.y <= inner.lo.y
        && outer.lo.z <= inner.lo.z
        && inner.hi.x <= outer.hi.x
        && inner.hi.y <= outer.hi.y
        && inner.hi.z <= outer.hi.z
}

#[inline]
fn active_tape<'a, 't>(root: &'a Tape<'t>, frames: &'a [TapeFrame<'t>]) -> &'a Tape<'t> {
    frames.last().map_or(root, |f| &f.tape)
}

/// Deepest active simplified tape whose valid `region` completely encloses `query` (or `root`).
#[inline]
fn tape_for_box<'a, 't>(root: &'a Tape<'t>, frames: &'a [TapeFrame<'t>], query: &Aabb3) -> &'a Tape<'t> {
    for f in frames.iter().rev() {
        if contains_box(&f.region, query) {
            return &f.tape;
        }
    }
    root
}

/// Simplify `cur` using `choices` decided over a region, returning `Some(sub)` only when it
/// strictly shrinks the tape.
fn try_simplify<'s>(cur: &Tape<'_>, choices: Choices<'_>, scratch: Alloc<'s>) -> Result<Option<Tape<'s>>, ModelError> {
    if cur.len() < MIN_SIMPLIFY_INSTRS || choices.decided() == 0 {
        return Ok(None);
    }
    let sub = simplify(cur, choices, scratch)?;
    if sub.len() < cur.len() { Ok(Some(sub)) } else { Ok(None) }
}

/// Evaluate the centres of `kids` (`1..=8`) in one SIMD batch (`Lanes::Eight` / `Lanes::Four` /
/// `Lanes::One`), writing results into `out[..kids.len()]`.
fn batch_eval_centers(t: &Tape<'_>, kids: &[Aabb3], out: &mut [f64; 8], sc: &mut Scratch<'_>) -> Result<(), ModelError> {
    let n = kids.len();
    let mut xs = [0.0f64; 8];
    let mut ys = [0.0f64; 8];
    let mut zs = [0.0f64; 8];
    for i in 0..n {
        let c = kids[i].center();
        xs[i] = c.x;
        ys[i] = c.y;
        zs[i] = c.z;
    }
    let lanes = if n == 8 {
        Lanes::Eight
    } else if n >= 4 {
        Lanes::Four
    } else {
        Lanes::One
    };
    eval_points(t, Points { x: &xs[..n], y: &ys[..n], z: &zs[..n] }, &mut out[..n], lanes, sc)?;
    Ok(())
}

// ---- min_wall ----------------------------------------------------------------------------

/// What [`min_wall`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct MinWallOpts<'s> {
    /// The thinnest wall allowed.
    pub wall: Length,
    /// Where the tape and the box stack live.
    pub scratch: Alloc<'s>,
    /// Surface samples across one wall: the surface is sampled on cells of side
    /// `wall / samples_per_wall`, and a thin patch at least that across is found.
    pub samples_per_wall: u32 = 2,
    /// How near the surface a sample is placed, and how exactly a ray's exit is placed.
    pub tolerance: Length = Length::nm(1_000),
    /// Cells the search may look at (rule 18).
    pub most_boxes: u64 = 1 << 24,
    /// Field evaluations one sample's bracket and ray may take; a sample that spends them is
    /// unsettled.
    pub most_steps_per_point: u32 = 1 << 12,
}

/// Whether every wall of `part` is at least `wall` thick (see the module for the definition).
pub fn min_wall<K: AtLeastInside, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    o: MinWallOpts<'_>,
) -> Result<Checked<ThinWall>, ModelError> {
    min_wall_planted(g, part, o, NoPlant)
}

/// Newton steps a sample takes before it looks for a sign bracket.
const NEWTON_STEPS: u32 = 8;
/// Halvings of a sampling cell's side a cell with no bracket is split through before it is left
/// unsettled.
const SPLIT_LEVELS: u32 = 4;
/// The ray's least step is the wall over this: a gap in the material narrower across the ray
/// can be missed.
const RAY_STEPS: f64 = 1024.0;

/// The field at points, counted (the cost model's unit), with one sample's budget.
struct Probe<'a, 't, 's> {
    t: &'a Tape<'t>,
    sc: &'a mut Scratch<'s>,
    /// Every evaluation at a point the search made.
    points: u64,
    /// This sample's evaluations, and the most it may make.
    used: u32,
    most: u32,
}

impl Probe<'_, '_, '_> {
    /// The field at `p`, outside any sample's budget (a cell's centre test).
    fn point(&mut self, p: Vec3) -> f64 {
        self.points += 1;
        eval_point(self.t, p, self.sc)
    }

    /// The field at `p` within the sample's budget; `None` once it is spent.
    fn f(&mut self, p: Vec3) -> Option<f64> {
        if self.used >= self.most {
            return None;
        }
        self.used += 1;
        Some(self.point(p))
    }

    /// The field and its gradient at `p` within the sample's budget.
    fn grad(&mut self, p: Vec3) -> Option<(f64, Vec3)> {
        if self.used >= self.most {
            return None;
        }
        self.used += 1;
        self.points += 1;
        Some(eval_grad(self.t, p, self.sc))
    }
}

/// A sample: a point within the tolerance of the surface, with the field's gradient there.
struct OnSurface {
    p: Vec3,
    g: Vec3,
}

/// What looking for a sample in a cell gave.
enum Projected {
    At(OnSurface),
    /// Newton's steps did not settle, and the centre, the steps and the corners are all of one
    /// sign.
    NoBracket,
    /// The budget was spent.
    Spent,
}

/// What one sampling cell gave.
enum Sample {
    Thick,
    Thin { at: Vec3, thickness: f64 },
    /// No sign bracket in the cell (see [`Projected::NoBracket`]): split it.
    NoBracket,
    /// The budget was spent, or the field's gradient is zero or not finite at the sample.
    Open,
}

/// The surface crossing between `a` (field `va`) and `b` (of the other sign), narrowed by
/// bisection on the sign to the tolerance: its inside end, with the gradient there.
fn narrow(pr: &mut Probe<'_, '_, '_>, a: Vec3, va: f64, b: Vec3, tol: f64) -> Projected {
    let (mut inn, mut out) = if va < 0.0 { (a, b) } else { (b, a) };
    while (out - inn).length() > tol {
        let mid = inn + (out - inn) * 0.5;
        let Some(vm) = pr.f(mid) else { return Projected::Spent };
        if vm < 0.0 {
            inn = mid;
        } else {
            out = mid;
        }
    }
    let Some((_, g)) = pr.grad(inn) else { return Projected::Spent };
    Projected::At(OnSurface { p: inn, g })
}

/// A sample in `cell` (module documentation, SAMPLING). `newton_only` is the planted
/// `NoBracket`.
fn project(pr: &mut Probe<'_, '_, '_>, cell: &Aabb3, tol: f64, newton_only: bool) -> Projected {
    let c = cell.center();
    let reach = (cell.hi - cell.lo).length();
    let Some((v0, g0)) = pr.grad(c) else { return Projected::Spent };
    let (mut p, mut v, mut g) = (c, v0, g0);
    if newton_only {
        // PLANTED: Newton's steps alone, until the budget is spent.
        loop {
            if v.abs() <= tol {
                return Projected::At(OnSurface { p, g });
            }
            let gg = g.dot(g);
            if !(gg > 0.0 && gg.is_finite() && v.is_finite()) {
                return Projected::Spent;
            }
            p = p - g * (v / gg);
            let Some((vq, gq)) = pr.grad(p) else { return Projected::Spent };
            (v, g) = (vq, gq);
        }
    }
    for _ in 0..NEWTON_STEPS {
        if v.abs() <= tol {
            return Projected::At(OnSurface { p, g });
        }
        let gg = g.dot(g);
        if !(gg > 0.0 && gg.is_finite() && v.is_finite()) {
            break;
        }
        let q = p - g * (v / gg);
        // A step out of the cell's reach (or not finite) finds no sample of this cell.
        if !((q - c).length() <= reach) {
            break;
        }
        let Some((vq, gq)) = pr.grad(q) else { return Projected::Spent };
        if !vq.is_finite() {
            break;
        }
        if (vq < 0.0) != (v < 0.0) {
            return narrow(pr, p, v, q, tol);
        }
        (p, v, g) = (q, vq, gq);
    }
    if v.abs() <= tol {
        return Projected::At(OnSurface { p, g });
    }
    if !v0.is_finite() {
        return Projected::NoBracket;
    }
    for k in cell.corners() {
        let Some(vk) = pr.f(k) else { return Projected::Spent };
        if vk.is_finite() && (vk < 0.0) != (v0 < 0.0) {
            return narrow(pr, c, v0, k, tol);
        }
    }
    Projected::NoBracket
}

/// The wall at a sample of `cell` (module documentation): its chord along the inward normal,
/// against `wall` (metres).
fn wall_at(pr: &mut Probe<'_, '_, '_>, cell: &Aabb3, wall: f64, tol: f64, plant: impl Planted<WallPlant>) -> Sample {
    pr.used = 0;
    let s = match project(pr, cell, tol, plant.has(WallPlant::NoBracket)) {
        Projected::At(s) => s,
        Projected::NoBracket => return Sample::NoBracket,
        Projected::Spent => return Sample::Open,
    };
    let p = s.p;
    let gl = s.g.length();
    if !(gl > 0.0 && gl.is_finite()) {
        return Sample::Open;
    }
    let inward = s.g * (-1.0 / gl);
    if plant.has(WallPlant::TangentBall) {
        // PLANTED: thin where a ball of diameter `wall` tangent at `p` does not fit.
        let r = wall / 2.0;
        let Some(f) = pr.f(p + inward * r) else { return Sample::Open };
        let depth = -f;
        return if depth < r - tol { Sample::Thin { at: p, thickness: r + depth } } else { Sample::Thick };
    }
    // Start two tolerances in: past the surface the sample is within a tolerance of.
    let x0 = 2.0 * tol / gl.min(1.0);
    if x0 >= wall {
        return Sample::Thick;
    }
    // ONE BALL: inside, `-f` is the distance to the boundary, so the ball of that radius about
    // the ray's middle is solid; reaching both ends with a tolerance to spare, the ray stays in.
    let middle = (x0 + wall) / 2.0;
    let Some(fm) = pr.f(p + inward * middle) else { return Sample::Open };
    if -fm >= (wall - x0) / 2.0 + tol {
        return Sample::Thick;
    }
    // March by `|f|` (no step crosses the surface), at least `least`, until the ray leaves.
    let least = tol.max(wall / RAY_STEPS);
    let mut inside = 0.0f64;
    let mut x = x0;
    let left = loop {
        if x >= wall {
            return Sample::Thick;
        }
        let Some(f) = pr.f(p + inward * x) else { return Sample::Open };
        if f >= 0.0 {
            break f;
        }
        inside = x;
        x += (-f).max(least);
    };
    // Left between `inside` and `x`: on the surface already, or placed by bisection on the sign.
    let (mut a, mut b) = (inside, x);
    if left > tol {
        while b - a > tol {
            let mid = a + (b - a) / 2.0;
            let Some(f) = pr.f(p + inward * mid) else { return Sample::Open };
            if f >= 0.0 {
                b = mid;
            } else {
                a = mid;
            }
        }
    }
    if b >= wall {
        return Sample::Thick;
    }
    // A wall's far side faces the near one: the ray leaves through a face within 60 degrees of
    // facing it. One that leaves through a face it runs along (from a sharp edge, or past a
    // convex face curving away) measured no wall.
    let Some((_, gq)) = pr.grad(p + inward * b) else { return Sample::Open };
    let gql = gq.length();
    if !(gql > 0.0 && gql.is_finite()) {
        return Sample::Open;
    }
    if gq.dot(inward) >= 0.5 * gql { Sample::Thin { at: p, thickness: b } } else { Sample::Thick }
}

/// [`min_wall`] with a planted bug, for the tests' controls.
pub fn min_wall_planted<K: AtLeastInside, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    o: MinWallOpts<'_>,
    plant: impl Planted<WallPlant>,
) -> Result<Checked<ThinWall>, ModelError> {
    if o.wall <= Length::ZERO {
        return Err(ModelError::Argument { function: "min_wall", arg: "wall", accepted: "a length above zero" });
    }
    if o.samples_per_wall == 0 {
        return Err(ModelError::Argument { function: "min_wall", arg: "samples_per_wall", accepted: "at least one" });
    }
    if o.tolerance < Length::ZERO {
        return Err(ModelError::Argument { function: "min_wall", arg: "tolerance", accepted: "a length at least zero" });
    }
    if o.most_steps_per_point == 0 {
        return Err(ModelError::Argument { function: "min_wall", arg: "most_steps_per_point", accepted: "at least one" });
    }
    let s_tape = compile(g, part.solid.id(), CompileOpts { alloc: o.scratch, detail: Detail::Shape, .. }).map_err(ModelError::Compile)?;
    let key = CheckKey {
        check: CheckKind::MinWall,
        tapes: [s_tape.digest(), [0; 32]],
        args: [o.wall.as_nm(), i64::from(o.samples_per_wall), o.tolerance.as_nm()],
        schema: CHECK_SCHEMA,
    };
    let root = s_tape.aabb();
    if !finite(&root) {
        return Err(ModelError::Inspect(InspectError::Unbounded { what: "the part's box" }));
    }

    let wall = o.wall.as_m();
    let leaf = wall / f64::from(o.samples_per_wall);
    let floor = leaf / f64::from(1u32 << SPLIT_LEVELS);
    let tol = o.tolerance.as_m().max(1e-9);
    let mut sc = Scratch::new(o.scratch);
    let mut frames: Vec<TapeFrame<'_>, Alloc<'_>> = Vec::new_in(o.scratch);
    let mut stack: Vec<(Aabb3, bool), Alloc<'_>> = Vec::new_in(o.scratch);
    stack.try_push((root, false)).map_err(|_| ModelError::Alloc { what: "a check's box stack" })?;
    let mut boxes: u64 = 0;
    let mut samples: u64 = 0;
    let mut points: u64 = 0;
    let mut open: u64 = 0;
    let mut near: Option<Vec3> = None;
    let mut vals = [0.0f64; 8];

    loop {
        while frames.last().is_some_and(|f| stack.len() <= f.stack_base) {
            frames.pop();
        }
        let Some((c, center_tested)) = stack.pop() else { break };
        boxes += 1;
        if boxes > o.most_boxes {
            let verdict = Verdict::Unknown(Unsettled::Budget { boxes });
            return Ok(Checked { verdict, key, boxes, samples, points });
        }
        let side = c.longest_side();
        let sampling = side <= leaf;
        if sampling {
            samples += 1;
        }
        let cur_tape = active_tape(&s_tape, &frames);
        // `|f| <= distance` at every level: a centre farther from zero than the half diagonal
        // has no boundary point in its cell.
        if !center_tested {
            points += 1;
            if eval_point(cur_tape, c.center(), &mut sc).abs() > (c.hi - c.lo).length() / 2.0 {
                continue;
            }
        }
        // A sampling cell straight from the search (side over `leaf / 2`) skips the interval:
        // over a cell that small it excludes what the centre test does. Larger cells and the
        // parts of a split one take it.
        if !sampling || side <= leaf / 2.0 {
            let (iv, choices) = eval_box(cur_tape, &c, &mut sc);
            if iv.lo() > 0.0 || iv.hi() < 0.0 {
                continue;
            }
            if !sampling {
                let sub = if side > leaf * 2.0 { try_simplify(cur_tape, choices, o.scratch)? } else { None };
                if let Some(tape) = sub {
                    frames
                        .try_push(TapeFrame { tape, region: c, stack_base: stack.len() })
                        .map_err(|_| ModelError::Alloc { what: "a check's tape stack" })?;
                }
                let child_tape = active_tape(&s_tape, &frames);
                let (kids, n) = split(&c);
                if boxes + (n as u64) <= o.most_boxes {
                    batch_eval_centers(child_tape, &kids[..n], &mut vals, &mut sc)?;
                    points += n as u64;
                    for i in 0..n {
                        let k = kids[i];
                        let k_sampling = k.longest_side() <= leaf;
                        if vals[i].abs() > (k.hi - k.lo).length() / 2.0 {
                            boxes += 1;
                            if k_sampling {
                                samples += 1;
                            }
                        } else {
                            stack.try_push((k, true)).map_err(|_| ModelError::Alloc { what: "a check's box stack" })?;
                        }
                    }
                } else {
                    for k in &kids[..n] {
                        stack.try_push((*k, false)).map_err(|_| ModelError::Alloc { what: "a check's box stack" })?;
                    }
                }
                continue;
            }
        }
        let reach = (c.hi - c.lo).length();
        let ray_tape = if plant.has(WallPlant::NoBracket) {
            &s_tape
        } else {
            let query = Aabb3::around(c.center(), Vec3::splat(reach + wall));
            tape_for_box(&s_tape, &frames, &query)
        };
        let mut pr = Probe { t: ray_tape, sc: &mut sc, points, used: 0, most: o.most_steps_per_point };
        let res = wall_at(&mut pr, &c, wall, tol, plant);
        points = pr.points;
        match res {
            Sample::Thick => {}
            Sample::Thin { at, thickness } => {
                let found = ThinWall { at: lengths(at), thickness: Length::m(thickness), wall: o.wall };
                return Ok(Checked { verdict: Verdict::Found(found), key, boxes, samples, points });
            }
            Sample::NoBracket if side > floor => {
                let (kids, n) = split(&c);
                for k in &kids[..n] {
                    stack.try_push((*k, false)).map_err(|_| ModelError::Alloc { what: "a check's box stack" })?;
                }
            }
            Sample::NoBracket | Sample::Open => {
                open += 1;
                near.get_or_insert(c.center());
            }
        }
    }
    let verdict = match near {
        Some(p) => Verdict::Unknown(Unsettled::Open { cells: open, near: lengths(p) }),
        None => Verdict::Pass,
    };
    Ok(Checked { verdict, key, boxes, samples, points })
}

// ---- no_interference & no_interference_assembly ------------------------------------------

/// What [`no_interference`] and [`no_interference_assembly`] take.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct InterferenceOpts<'s> {
    /// The least gap allowed; above zero (two parts that only touch have gap zero, as two that
    /// overlap do, so a clearance of zero could never fail).
    pub clearance: Length,
    /// Where the search's heap lives.
    pub scratch: Alloc<'s>,
    /// How narrow the gap is settled.
    pub tolerance: Length = Length::nm(10_000),
    /// Boxes `distance_between_stopping` may take per pair.
    pub most_boxes: u32 = 1 << 20,
}

/// Whether the solids of `a` and `b` (shape tapes in one frame, see [`crate::placed_tape`]) are
/// at least the clearance apart. Uses early-exit bounds (`stop_below` / `stop_above`) so clearly
/// separated or colliding parts settle in O(1) boxes. The tape whose field measures outside its
/// solid (level at least `Outside`) is taken as the measured one, so either order works when one
/// of them does.
pub fn no_interference(a: &Tape<'_>, b: &Tape<'_>, o: InterferenceOpts<'_>) -> Result<Checked<Interference>, ModelError> {
    no_interference_planted(a, b, o, NoPlant)
}

/// [`no_interference`] with a planted bug, for the tests' controls.
pub fn no_interference_planted(
    a: &Tape<'_>,
    b: &Tape<'_>,
    o: InterferenceOpts<'_>,
    plant: impl Planted<InterferencePlant>,
) -> Result<Checked<Interference>, ModelError> {
    if o.clearance <= Length::ZERO {
        return Err(ModelError::Argument { function: "no_interference", arg: "clearance", accepted: "a length above zero" });
    }
    if o.tolerance < Length::ZERO {
        return Err(ModelError::Argument { function: "no_interference", arg: "tolerance", accepted: "a length at least zero" });
    }
    let (a, b) = if !b.level().at_least(Level::Outside) && a.level().at_least(Level::Outside) { (b, a) } else { (a, b) };
    let (da, db) = (a.digest(), b.digest());
    let key = CheckKey {
        check: CheckKind::NoInterference,
        tapes: if da <= db { [da, db] } else { [db, da] },
        args: [o.clearance.as_nm(), o.tolerance.as_nm(), 0],
        schema: CHECK_SCHEMA,
    };
    let mut sc = Scratch::new(o.scratch);
    let c = o.clearance.as_m();
    let gap = distance_between_stopping(
        a,
        b,
        DistanceStopOpts { tolerance: o.tolerance.as_m(), most_boxes: o.most_boxes, stop_below: Some(c), stop_above: Some(c) },
        &mut sc,
    )
    .map_err(ModelError::Inspect)?;
    let (verdict, boxes) = match gap {
        Gap::Empty => return Err(ModelError::EmptySolid { function: "no_interference", arg: "a" }),
        Gap::Settled(iv) => (
            judge(iv.lo(), iv.hi(), c, o.clearance, || Unsettled::Straddles { lo: Length::m(iv.lo()), hi: Length::m(iv.hi()) }),
            0,
        ),
        Gap::BudgetSpent { so_far, boxes } => (judge(so_far.lo(), so_far.hi(), c, o.clearance, || Unsettled::Budget { boxes }), boxes),
    };
    let verdict = match verdict {
        Verdict::Unknown(_) if plant.has(InterferencePlant::UnknownPasses) => Verdict::Pass,
        v => v,
    };
    Ok(Checked { verdict, key, boxes, samples: 0, points: 0 })
}

fn judge(lo: f64, hi: f64, c: f64, clearance: Length, open: impl FnOnce() -> Unsettled) -> Verdict<Interference> {
    if hi < c {
        Verdict::Found(Interference { gap: [Length::m(lo), Length::m(hi)], clearance })
    } else if lo >= c {
        Verdict::Pass
    } else {
        Verdict::Unknown(open())
    }
}

/// Deterministic order-independent key fold for an assembly of `n` placed tape digests.
fn assembly_tapes_key(digests: &mut [Digest]) -> [Digest; 2] {
    digests.sort_unstable();
    match digests {
        [] => [[0; 32], [0; 32]],
        [a] => [*a, [0; 32]],
        [a, b] => [*a, *b],
        many => {
            let mut acc0 = [0u64; 4];
            let mut acc1 = [0x9e37_79b9_7f4a_7c15u64; 4];
            for d in many.iter() {
                for lane in 0..4 {
                    let word = u64::from_le_bytes([
                        d[lane * 8],
                        d[lane * 8 + 1],
                        d[lane * 8 + 2],
                        d[lane * 8 + 3],
                        d[lane * 8 + 4],
                        d[lane * 8 + 5],
                        d[lane * 8 + 6],
                        d[lane * 8 + 7],
                    ]);
                    acc0[lane] = acc0[lane].rotate_left(13).wrapping_mul(0xbf58_476d_1ce4_e5b9) ^ word;
                    acc1[lane] = acc1[lane].rotate_left(29).wrapping_add(word).wrapping_mul(0x94d0_49bb_1331_11eb);
                }
            }
            let pack = |w: [u64; 4]| -> Digest {
                let mut out = [0u8; 32];
                for lane in 0..4 {
                    out[lane * 8..lane * 8 + 8].copy_from_slice(&w[lane].to_le_bytes());
                }
                out
            };
            [pack(acc0), pack(acc1)]
        }
    }
}

/// All-pairs interference check over `parts` in their assembly placements, with O(1) AABB
/// broad-phase pruning (`aabb.gap(&other_aabb) >= clearance` skips separated pairs immediately)
/// and deterministic lexicographical pair ordering `(0, 1), (0, 2), ..., (n - 2, n - 1)`.
/// Returns a three-way [`Verdict<AssemblyInterference>`]: definite collision is reported first,
/// an unsettled pair never becomes a pass.
pub fn no_interference_assembly<P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    parts: &[Part],
    o: InterferenceOpts<'_>,
) -> Result<Checked<AssemblyInterference>, ModelError> {
    if o.clearance <= Length::ZERO {
        return Err(ModelError::Argument {
            function: "no_interference_assembly",
            arg: "clearance",
            accepted: "a length above zero",
        });
    }
    if o.tolerance < Length::ZERO {
        return Err(ModelError::Argument {
            function: "no_interference_assembly",
            arg: "tolerance",
            accepted: "a length at least zero",
        });
    }
    let s = o.scratch;
    let n = parts.len();
    let mut tapes: Vec<Tape<'_>, Alloc<'_>> = Vec::new_in(s);
    tapes.try_reserve(n).map_err(|_| ModelError::Alloc { what: "assembly interference tapes" })?;
    let mut aabbs: Vec<Aabb3, Alloc<'_>> = Vec::new_in(s);
    aabbs.try_reserve(n).map_err(|_| ModelError::Alloc { what: "assembly interference boxes" })?;
    let mut digests: Vec<Digest, Alloc<'_>> = Vec::new_in(s);
    digests.try_reserve(n).map_err(|_| ModelError::Alloc { what: "assembly interference digests" })?;

    for part in parts {
        let t = placed_tape(g, part, Detail::Shape, s)?;
        aabbs.try_push(t.aabb()).map_err(|_| ModelError::Alloc { what: "assembly interference boxes" })?;
        digests.try_push(t.digest()).map_err(|_| ModelError::Alloc { what: "assembly interference digests" })?;
        tapes.try_push(t).map_err(|_| ModelError::Alloc { what: "assembly interference tapes" })?;
    }

    let key = CheckKey {
        check: CheckKind::NoInterferenceAssembly,
        tapes: assembly_tapes_key(&mut digests),
        args: [o.clearance.as_nm(), o.tolerance.as_nm(), n as i64],
        schema: CHECK_SCHEMA,
    };

    let c = o.clearance.as_m();
    let mut total_boxes: u64 = 0;
    let mut first_unknown: Option<Unsettled> = None;

    for i in 0..n {
        for j in (i + 1)..n {
            // O(1) AABB broad-phase pruning.
            let (ai, aj) = (&aabbs[i], &aabbs[j]);
            if !ai.is_empty() && !aj.is_empty() && finite(ai) && finite(aj) && ai.gap(aj) >= c {
                continue;
            }
            let checked = no_interference(&tapes[i], &tapes[j], o)?;
            total_boxes = total_boxes.saturating_add(checked.boxes);
            match checked.verdict {
                Verdict::Pass => {}
                Verdict::Found(f) => {
                    let found = AssemblyInterference {
                        pair: [i as u32, j as u32],
                        names: [parts[i].name, parts[j].name],
                        gap: f.gap,
                        clearance: o.clearance,
                    };
                    return Ok(Checked { verdict: Verdict::Found(found), key, boxes: total_boxes, samples: 0, points: 0 });
                }
                Verdict::Unknown(u) => {
                    first_unknown.get_or_insert(u);
                }
            }
        }
    }

    let verdict = match first_unknown {
        Some(u) => Verdict::Unknown(u),
        None => Verdict::Pass,
    };
    Ok(Checked { verdict, key, boxes: total_boxes, samples: 0, points: 0 })
}

// ---- fits_within -------------------------------------------------------------------------

/// What [`fits_within`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct FitOpts<'s> {
    /// Where the tape and the box stack live.
    pub scratch: Alloc<'s>,
    /// How far out a point may be and still fit (also the finest cell). A face of the part flush
    /// with one of the envelope costs about its area over the tolerance squared in boxes: give
    /// the fit you need, not less.
    pub tolerance: Length = Length::nm(100_000),
    /// Box evaluations of the search (rule 18).
    pub most_boxes: u64 = 1 << 22,
}

/// Whether `part` lies within `envelope` (both in one frame) to the tolerance.
pub fn fits_within<P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    part: Solid<Bound>,
    envelope: Solid<Bound>,
    o: FitOpts<'_>,
) -> Result<Checked<Overhang>, ModelError> {
    fits_within_planted(g, part, envelope, o, NoPlant)
}

/// [`fits_within`] with a planted bug, for the tests' controls.
pub fn fits_within_planted<P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    part: Solid<Bound>,
    envelope: Solid<Bound>,
    o: FitOpts<'_>,
    plant: impl Planted<FitPlant>,
) -> Result<Checked<Overhang>, ModelError> {
    if o.tolerance <= Length::ZERO {
        return Err(ModelError::Argument { function: "fits_within", arg: "tolerance", accepted: "a length above zero" });
    }
    let out = if plant.has(FitPlant::Reversed) { g.difference(envelope, part) } else { g.difference(part, envelope) }
        .map_err(ModelError::Cad)?;
    let t = compile(g, out.id(), CompileOpts { alloc: o.scratch, detail: Detail::Shape, .. }).map_err(ModelError::Compile)?;
    let key = CheckKey {
        check: CheckKind::FitsWithin,
        tapes: [t.digest(), [0; 32]],
        args: [o.tolerance.as_nm(), 0, 0],
        schema: CHECK_SCHEMA,
    };
    let root = t.aabb();
    if root.is_empty() {
        return Ok(Checked { verdict: Verdict::Pass, key, boxes: 0, samples: 0, points: 0 });
    }
    if !finite(&root) {
        return Err(ModelError::Inspect(InspectError::Unbounded { what: "the part's box" }));
    }
    let tol = o.tolerance.as_m();
    let mut sc = Scratch::new(o.scratch);
    let mut frames: Vec<TapeFrame<'_>, Alloc<'_>> = Vec::new_in(o.scratch);
    let mut stack: Vec<Aabb3, Alloc<'_>> = Vec::new_in(o.scratch);
    push(&mut stack, root)?;
    let mut boxes: u64 = 0;
    let mut points: u64 = 0;
    let mut open: u64 = 0;
    let mut near: Option<Vec3> = None;

    loop {
        while frames.last().is_some_and(|f| stack.len() <= f.stack_base) {
            frames.pop();
        }
        let Some(c) = stack.pop() else { break };
        boxes += 1;
        if boxes > o.most_boxes {
            return Ok(Checked { verdict: Verdict::Unknown(Unsettled::Budget { boxes }), key, boxes, samples: 0, points });
        }
        let cur_tape = active_tape(&t, &frames);
        let side = c.longest_side();
        let (f, choices) = eval_box(cur_tape, &c, &mut sc);
        if f.lo() > -tol {
            continue;
        }
        let sub = if side > tol * 2.0 { try_simplify(cur_tape, choices, o.scratch)? } else { None };
        let eval_t = sub.as_ref().unwrap_or(cur_tape);
        let centre = c.center();
        points += 1;
        let v = eval_point(eval_t, centre, &mut sc);
        if v < -tol {
            let found = Overhang { at: lengths(centre), depth: Length::m(-v) };
            return Ok(Checked { verdict: Verdict::Found(found), key, boxes, samples: 0, points });
        }
        if side <= tol {
            open += 1;
            near.get_or_insert(centre);
            continue;
        }
        if let Some(tape) = sub {
            frames
                .try_push(TapeFrame { tape, region: c, stack_base: stack.len() })
                .map_err(|_| ModelError::Alloc { what: "a check's tape stack" })?;
        }
        let (kids, n) = split(&c);
        for k in &kids[..n] {
            push(&mut stack, *k)?;
        }
    }
    let verdict = match near {
        Some(p) => Verdict::Unknown(Unsettled::Open { cells: open, near: lengths(p) }),
        None => Verdict::Pass,
    };
    Ok(Checked { verdict, key, boxes, samples: 0, points })
}

// ---- SOTA DFM checks: overhang_check & draft_angle_check ---------------------------------

#[inline]
fn box_min_dot(b: &Aabb3, u: Vec3) -> f64 {
    (if u.x >= 0.0 { b.lo.x } else { b.hi.x }) * u.x
        + (if u.y >= 0.0 { b.lo.y } else { b.hi.y }) * u.y
        + (if u.z >= 0.0 { b.lo.z } else { b.hi.z }) * u.z
}

#[inline]
fn box_max_dot(b: &Aabb3, u: Vec3) -> f64 {
    (if u.x >= 0.0 { b.hi.x } else { b.lo.x }) * u.x
        + (if u.y >= 0.0 { b.hi.y } else { b.lo.y }) * u.y
        + (if u.z >= 0.0 { b.hi.z } else { b.lo.z }) * u.z
}

#[derive(Clone, Copy)]
struct MinNode {
    lb: f64,
    side: f64,
    b: Aabb3,
}

impl MinNode {
    /// Order by smallest `lb`, breaking ties by smallest `side` (depth-first straight to a flat
    /// bottom face so finding `z_min` takes `O(log(H / tol))` pops instead of scanning the face).
    #[inline]
    fn better_than(&self, other: &Self) -> bool {
        self.lb < other.lb || (self.lb == other.lb && self.side < other.side)
    }
}

fn min_heap_push(h: &mut Vec<MinNode, Alloc<'_>>, node: MinNode) -> Result<(), ModelError> {
    h.try_push(node).map_err(|_| ModelError::Alloc { what: "a check's extent heap" })?;
    let mut i = h.len() - 1;
    while i > 0 {
        let p = (i - 1) / 2;
        if !h[i].better_than(&h[p]) {
            break;
        }
        h.swap(p, i);
        i = p;
    }
    Ok(())
}

fn min_heap_pop(h: &mut Vec<MinNode, Alloc<'_>>) -> Option<MinNode> {
    let n = h.len();
    if n == 0 {
        return None;
    }
    h.swap(0, n - 1);
    let top = h.pop();
    let n = h.len();
    let mut i = 0;
    loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < n && h[l].better_than(&h[m]) {
            m = l;
        }
        if r < n && h[r].better_than(&h[m]) {
            m = r;
        }
        if m == i {
            break;
        }
        h.swap(i, m);
        i = m;
    }
    top
}

/// Result of locating the solid's minimum extent `min_{f(p) <= 0} dot(p, u)`.
enum MinExtent {
    Found(f64),
    Empty,
    Budget,
}

/// Find the solid's minimum projection along unit vector `u` to within `tol` metres.
fn min_along(
    t: &Tape<'_>,
    root: Aabb3,
    u: Vec3,
    tol: f64,
    most_boxes: u64,
    boxes: &mut u64,
    points: &mut u64,
    sc: &mut Scratch<'_>,
    scratch: Alloc<'_>,
) -> Result<MinExtent, ModelError> {
    let mut heap: Vec<MinNode, Alloc<'_>> = Vec::new_in(scratch);
    min_heap_push(&mut heap, MinNode { lb: box_min_dot(&root, u), side: root.longest_side(), b: root })?;
    let mut ub = f64::INFINITY;
    while let Some(node) = min_heap_pop(&mut heap) {
        if ub.is_finite() && ub - node.lb <= tol {
            return Ok(MinExtent::Found(node.lb));
        }
        if node.lb >= ub {
            break;
        }
        *boxes += 1;
        if *boxes > most_boxes {
            return Ok(MinExtent::Budget);
        }
        let c = node.b.center();
        *points += 1;
        if eval_point(t, c, sc) <= 0.0 {
            ub = ub.min(c.dot(u));
            if ub - node.lb <= tol {
                return Ok(MinExtent::Found(node.lb));
            }
        }
        if (ub.is_finite() && node.side <= tol * 0.25) || node.side <= 1e-9 {
            continue;
        }
        let (kids, n) = split(&node.b);
        for k in &kids[..n] {
            let klb = box_min_dot(k, u);
            if klb >= ub {
                continue;
            }
            let (iv, _) = eval_box(t, k, sc);
            if iv.lo() > 0.0 {
                continue;
            }
            min_heap_push(&mut heap, MinNode { lb: klb, side: k.longest_side(), b: *k })?;
        }
    }
    if ub.is_finite() { Ok(MinExtent::Found(ub)) } else { Ok(MinExtent::Empty) }
}

/// Deterministic key word packing a unit direction and a tolerance (nm) into one `i64`.
fn dir_tol_key(u: Vec3, tol_nm: i64) -> i64 {
    let q = |x: f64| core::f64::math::round(x * 1_000_000.0) as i64;
    let mut h = (tol_nm as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for c in [q(u.x), q(u.y), q(u.z)] {
        h = h.rotate_left(17).wrapping_mul(0xbf58_476d_1ce4_e5b9) ^ (c as u64);
    }
    h as i64
}

/// Recover a verified outward unit normal at surface sample `s`. Uses the AD gradient `s.g` when
/// non-zero (verifying its outward sign at distance `eps` to guard against `O(1e-16)` sign flips
/// on primitives like `Cone` that compute `±sqrt(dx^2 + dy^2)`), and falls back to central
/// differences at `eps` if AD vanished right on an exact zero of `sqrt`.
fn surface_normal(pr: &mut Probe<'_, '_, '_>, s: &OnSurface, eps: f64) -> Option<Vec3> {
    let gl = s.g.length();
    if gl > 0.0 && gl.is_finite() {
        let mut n = s.g * (1.0 / gl);
        if let (Some(fp), Some(fm)) = (pr.f(s.p + n * eps), pr.f(s.p - n * eps))
            && fp < 0.0
            && fm > 0.0
        {
            n = -n;
        }
        return Some(n);
    }
    let fx = pr.f(s.p + Vec3::new(eps, 0.0, 0.0))? - pr.f(s.p - Vec3::new(eps, 0.0, 0.0))?;
    let fy = pr.f(s.p + Vec3::new(0.0, eps, 0.0))? - pr.f(s.p - Vec3::new(0.0, eps, 0.0))?;
    let fz = pr.f(s.p + Vec3::new(0.0, 0.0, eps))? - pr.f(s.p - Vec3::new(0.0, 0.0, eps))?;
    let fd = Vec3::new(fx, fy, fz);
    let fl = fd.length();
    if fl > 0.0 && fl.is_finite() { Some(fd * (1.0 / fl)) } else { None }
}

fn normalize_dir(dir: [f64; 3], function: &'static str, arg: &'static str) -> Result<Vec3, ModelError> {
    if !dir.iter().all(|v| v.is_finite()) {
        return Err(ModelError::Argument { function, arg, accepted: "a finite non-zero direction" });
    }
    let v = Vec3::new(dir[0], dir[1], dir[2]);
    v.normalized().ok_or(ModelError::Argument { function, arg, accepted: "a finite non-zero direction" })
}

/// Shared configuration for the verified surface-normal DFM checks ([`overhang_check_planted`]
/// and [`draft_angle_check_planted`]).
struct SurfaceCheck<'s> {
    function: &'static str,
    check: CheckKind,
    angle: Angle,
    dir: [f64; 3],
    dir_arg: &'static str,
    flip_eval_dir: bool,
    sample_size: Length,
    base_tolerance: Length,
    base_tol_arg: &'static str,
    tolerance: Length,
    most_boxes: u64,
    most_steps_per_point: u32,
    scratch: Alloc<'s>,
}

/// Deferred local-to-world transform for a surface violation witness.
struct SurfaceHit {
    lin: Mat3,
    shift: Vec3,
    p_local: Vec3,
    n_local: Vec3,
}

impl SurfaceHit {
    fn world(self) -> ([Length; 3], [f64; 3]) {
        let at_world = self.lin.apply(self.p_local) + self.shift;
        let n_world = self.lin.apply(self.n_local).normalized().unwrap_or(self.n_local);
        (lengths(at_world), n_world.to_array())
    }
}

/// Validated setup, key derivation, verified octree, hierarchical tape simplification, surface
/// projection, and outward normal walk shared by [`overhang_check_planted`] and
/// [`draft_angle_check_planted`]. Excludes cells and samples within `base_tolerance` above `z_min`
/// along `local_dir`, invoking `check` on `(n_local · eval_dir, hit)` to return the first
/// violating witness face.
fn check_surface_normals<K: Ex, P: Planted<CadPlant>, W, F>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    cfg: SurfaceCheck<'_>,
    mut check: F,
) -> Result<Checked<W>, ModelError>
where
    F: FnMut(f64, SurfaceHit) -> Option<W>,
{
    if cfg.sample_size <= Length::ZERO {
        return Err(ModelError::Argument { function: cfg.function, arg: "sample_size", accepted: "a length above zero" });
    }
    if cfg.base_tolerance < Length::ZERO {
        return Err(ModelError::Argument { function: cfg.function, arg: cfg.base_tol_arg, accepted: "a length at least zero" });
    }
    if cfg.tolerance < Length::ZERO {
        return Err(ModelError::Argument { function: cfg.function, arg: "tolerance", accepted: "a length at least zero" });
    }
    if cfg.most_steps_per_point == 0 {
        return Err(ModelError::Argument { function: cfg.function, arg: "most_steps_per_point", accepted: "at least one" });
    }
    let world_dir = normalize_dir(cfg.dir, cfg.function, cfg.dir_arg)?;
    let lin = part.placement.linear();
    let shift = part.placement.shift_m();
    let local_dir = lin.transpose().apply(world_dir).normalized().unwrap_or(world_dir);

    let s_tape = compile(g, part.solid.id(), CompileOpts { alloc: cfg.scratch, detail: Detail::Shape, .. })
        .map_err(ModelError::Compile)?;
    let key = CheckKey {
        check: cfg.check,
        tapes: [s_tape.digest(), [0; 32]],
        args: [cfg.angle.as_microarcsec(), cfg.sample_size.as_nm(), dir_tol_key(local_dir, cfg.base_tolerance.as_nm())],
        schema: CHECK_SCHEMA,
    };
    let eval_dir = if cfg.flip_eval_dir { -local_dir } else { local_dir };

    let root = s_tape.aabb();
    if root.is_empty() {
        return Ok(Checked { verdict: Verdict::Pass, key, boxes: 0, samples: 0, points: 0 });
    }
    if !finite(&root) {
        return Err(ModelError::Inspect(InspectError::Unbounded { what: "the part's box" }));
    }

    let leaf = cfg.sample_size.as_m();
    let floor = leaf / f64::from(1u32 << SPLIT_LEVELS);
    let tol = cfg.tolerance.as_m().max(1e-9);
    let base_tol = cfg.base_tolerance.as_m();
    let z_tol = (base_tol * 0.25).max(tol);

    let mut sc = Scratch::new(cfg.scratch);
    let mut boxes: u64 = 0;
    let mut samples: u64 = 0;
    let mut points: u64 = 0;

    let z_min = match min_along(&s_tape, root, local_dir, z_tol, cfg.most_boxes, &mut boxes, &mut points, &mut sc, cfg.scratch)? {
        MinExtent::Found(z) => z,
        MinExtent::Empty => return Ok(Checked { verdict: Verdict::Pass, key, boxes, samples, points }),
        MinExtent::Budget => {
            return Ok(Checked { verdict: Verdict::Unknown(Unsettled::Budget { boxes }), key, boxes, samples, points });
        }
    };
    let base_cut = z_min + base_tol;

    let mut frames: Vec<TapeFrame<'_>, Alloc<'_>> = Vec::new_in(cfg.scratch);
    let mut stack: Vec<(Aabb3, bool), Alloc<'_>> = Vec::new_in(cfg.scratch);
    stack.try_push((root, false)).map_err(|_| ModelError::Alloc { what: "a check's box stack" })?;
    let mut open: u64 = 0;
    let mut near: Option<Vec3> = None;
    let mut vals = [0.0f64; 8];

    loop {
        while frames.last().is_some_and(|f| stack.len() <= f.stack_base) {
            frames.pop();
        }
        let Some((c, center_tested)) = stack.pop() else { break };
        boxes += 1;
        if boxes > cfg.most_boxes {
            return Ok(Checked { verdict: Verdict::Unknown(Unsettled::Budget { boxes }), key, boxes, samples, points });
        }
        let side = c.longest_side();
        let sampling = side <= leaf;
        if sampling {
            samples += 1;
        }
        let reach = (c.hi - c.lo).length();
        // Prune cells wholly inside the base contact band along `local_dir` (even when a planted
        // bug flips `eval_dir`, base exclusion stays along `local_dir` so the bottom remains
        // excluded).
        if box_max_dot(&c, local_dir) + reach <= base_cut {
            continue;
        }
        let cur_tape = active_tape(&s_tape, &frames);
        if !center_tested {
            points += 1;
            if eval_point(cur_tape, c.center(), &mut sc).abs() > reach / 2.0 {
                continue;
            }
        }
        if !sampling || side <= leaf / 2.0 {
            let (iv, choices) = eval_box(cur_tape, &c, &mut sc);
            if iv.lo() > 0.0 || iv.hi() < 0.0 {
                continue;
            }
            if !sampling {
                let sub = if side > leaf * 2.0 { try_simplify(cur_tape, choices, cfg.scratch)? } else { None };
                if let Some(tape) = sub {
                    frames
                        .try_push(TapeFrame { tape, region: c, stack_base: stack.len() })
                        .map_err(|_| ModelError::Alloc { what: "a check's tape stack" })?;
                }
                let child_tape = active_tape(&s_tape, &frames);
                let (kids, n) = split(&c);
                if boxes + (n as u64) <= cfg.most_boxes {
                    batch_eval_centers(child_tape, &kids[..n], &mut vals, &mut sc)?;
                    points += n as u64;
                    for i in 0..n {
                        let k = kids[i];
                        let k_sampling = k.longest_side() <= leaf;
                        if vals[i].abs() > (k.hi - k.lo).length() / 2.0 {
                            boxes += 1;
                            if k_sampling {
                                samples += 1;
                            }
                        } else {
                            stack.try_push((k, true)).map_err(|_| ModelError::Alloc { what: "a check's box stack" })?;
                        }
                    }
                } else {
                    for k in &kids[..n] {
                        stack.try_push((*k, false)).map_err(|_| ModelError::Alloc { what: "a check's box stack" })?;
                    }
                }
                continue;
            }
        }
        let eps = (4.0 * tol).min(leaf * 0.05);
        let proj_tape = tape_for_box(&s_tape, &frames, &Aabb3::around(c.center(), Vec3::splat(reach + eps)));
        let mut pr = Probe { t: proj_tape, sc: &mut sc, points, used: 0, most: cfg.most_steps_per_point };
        let res = project(&mut pr, &c, tol, false);
        match res {
            Projected::At(s) => {
                if s.p.dot(local_dir) <= base_cut {
                    points = pr.points;
                    continue;
                }
                let Some(n_local) = surface_normal(&mut pr, &s, eps) else {
                    points = pr.points;
                    open += 1;
                    near.get_or_insert(lin.apply(c.center()) + shift);
                    continue;
                };
                points = pr.points;
                if let Some(found) = check(n_local.dot(eval_dir), SurfaceHit { lin, shift, p_local: s.p, n_local }) {
                    return Ok(Checked { verdict: Verdict::Found(found), key, boxes, samples, points });
                }
            }
            Projected::NoBracket if side > floor => {
                points = pr.points;
                let (kids, n) = split(&c);
                for k in &kids[..n] {
                    stack.try_push((*k, false)).map_err(|_| ModelError::Alloc { what: "a check's box stack" })?;
                }
            }
            Projected::NoBracket | Projected::Spent => {
                points = pr.points;
                open += 1;
                near.get_or_insert(lin.apply(c.center()) + shift);
            }
        }
    }

    let verdict = match near {
        Some(p) => Verdict::Unknown(Unsettled::Open { cells: open, near: lengths(p) }),
        None => Verdict::Pass,
    };
    Ok(Checked { verdict, key, boxes, samples, points })
}

/// What [`overhang_check`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct OverhangOpts<'s> {
    /// Maximum allowed overhang angle from vertical, strictly between `0°` and `90°` (default `45°`).
    pub max_angle: Angle = Angle::microarcsec(45 * UAS_PER_DEGREE),
    /// Where the tape and the box stack live.
    pub scratch: Alloc<'s>,
    /// Upward build direction in the assembly / world frame (need not be unit length, must be
    /// non-zero; defaults to `+Z`).
    pub build_dir: [f64; 3] = [0.0, 0.0, 1.0],
    /// Surface sampling cell side length: overhang patches at least this across are found
    /// (default `2 mm`).
    pub sample_size: Length = Length::nm(2_000_000),
    /// Build-plate contact exclusion height above the part's lowest point along `build_dir`
    /// (default `0.5 mm`).
    pub plate_tolerance: Length = Length::nm(500_000),
    /// How near the surface a sample is placed.
    pub tolerance: Length = Length::nm(1_000),
    /// Cells the search may look at (rule 18).
    pub most_boxes: u64 = 1 << 24,
    /// Field evaluations one sample's projection may take; a sample that spends them is unsettled.
    pub most_steps_per_point: u32 = 1 << 12,
}

/// 3D-printing DFM check: verifies that no downward-facing surface of `part` (in its placement,
/// excluding build-plate contact `dot(p, build_dir) <= z_min + plate_tolerance`) exceeds
/// `max_angle` from vertical.
pub fn overhang_check<K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    o: OverhangOpts<'_>,
) -> Result<Checked<OverhangFace>, ModelError> {
    overhang_check_planted(g, part, o, NoPlant)
}

/// [`overhang_check`] with a planted bug, for the tests' controls.
pub fn overhang_check_planted<K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    o: OverhangOpts<'_>,
    plant: impl Planted<OverhangPlant>,
) -> Result<Checked<OverhangFace>, ModelError> {
    let right_angle = Angle::microarcsec(90 * UAS_PER_DEGREE);
    if o.max_angle <= Angle::ZERO || o.max_angle >= right_angle {
        return Err(ModelError::Argument {
            function: "overhang_check",
            arg: "max_angle",
            accepted: "an angle strictly between 0 and 90 degrees",
        });
    }
    let sin_max = blaze_math::sin(o.max_angle.as_radians());
    check_surface_normals(
        g,
        part,
        SurfaceCheck {
            function: "overhang_check",
            check: CheckKind::Overhang,
            angle: o.max_angle,
            dir: o.build_dir,
            dir_arg: "build_dir",
            flip_eval_dir: plant.has(OverhangPlant::FlippedBuildDir),
            sample_size: o.sample_size,
            base_tolerance: o.plate_tolerance,
            base_tol_arg: "plate_tolerance",
            tolerance: o.tolerance,
            most_boxes: o.most_boxes,
            most_steps_per_point: o.most_steps_per_point,
            scratch: o.scratch,
        },
        |d, hit| {
            if d < -sin_max - 1e-9 {
                let (at, normal) = hit.world();
                let angle = Angle::radians(blaze_math::asin((-d).clamp(0.0, 1.0)));
                Some(OverhangFace { at, normal, angle, max_angle: o.max_angle })
            } else {
                None
            }
        },
    )
}

/// What [`draft_angle_check`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DraftOpts<'s> {
    /// Minimum required draft angle in `[0°, 90°)` (default `2°`).
    pub min_draft: Angle = Angle::microarcsec(2 * UAS_PER_DEGREE),
    /// Where the tape and the box stack live.
    pub scratch: Alloc<'s>,
    /// Mold pull direction in the assembly / world frame (need not be unit length, must be
    /// non-zero; defaults to `+Z`).
    pub pull_dir: [f64; 3] = [0.0, 0.0, 1.0],
    /// Surface sampling cell side length: insufficient-draft / undercut patches at least this
    /// across are found (default `2 mm`).
    pub sample_size: Length = Length::nm(2_000_000),
    /// Parting-plane base exclusion height above the part's lowest point along `pull_dir`
    /// (default `0.5 mm`).
    pub parting_tolerance: Length = Length::nm(500_000),
    /// How near the surface a sample is placed.
    pub tolerance: Length = Length::nm(1_000),
    /// Cells the search may look at (rule 18).
    pub most_boxes: u64 = 1 << 24,
    /// Field evaluations one sample's projection may take; a sample that spends them is unsettled.
    pub most_steps_per_point: u32 = 1 << 12,
}

/// Injection-molding / casting DFM check: verifies that every non-parting-plane surface of `part`
/// (in its placement, excluding base contact `dot(p, pull_dir) <= z_min + parting_tolerance`) has
/// draft angle at least `min_draft` relative to `pull_dir` (`dot(n, pull_dir) >= sin(min_draft)`),
/// flagging insufficient draft and undercuts (`dot(n, pull_dir) < 0`).
pub fn draft_angle_check<K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    o: DraftOpts<'_>,
) -> Result<Checked<DraftFace>, ModelError> {
    draft_angle_check_planted(g, part, o, NoPlant)
}

/// [`draft_angle_check`] with a planted bug, for the tests' controls.
pub fn draft_angle_check_planted<K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    o: DraftOpts<'_>,
    plant: impl Planted<DraftPlant>,
) -> Result<Checked<DraftFace>, ModelError> {
    let right_angle = Angle::microarcsec(90 * UAS_PER_DEGREE);
    if o.min_draft < Angle::ZERO || o.min_draft >= right_angle {
        return Err(ModelError::Argument {
            function: "draft_angle_check",
            arg: "min_draft",
            accepted: "an angle from 0 up to (not including) 90 degrees",
        });
    }
    let sin_min = blaze_math::sin(o.min_draft.as_radians());
    let ignore_sign = plant.has(DraftPlant::IgnoreUndercutSign);
    check_surface_normals(
        g,
        part,
        SurfaceCheck {
            function: "draft_angle_check",
            check: CheckKind::DraftAngle,
            angle: o.min_draft,
            dir: o.pull_dir,
            dir_arg: "pull_dir",
            flip_eval_dir: false,
            sample_size: o.sample_size,
            base_tolerance: o.parting_tolerance,
            base_tol_arg: "parting_tolerance",
            tolerance: o.tolerance,
            most_boxes: o.most_boxes,
            most_steps_per_point: o.most_steps_per_point,
            scratch: o.scratch,
        },
        |d_raw, hit| {
            let d = if ignore_sign { d_raw.abs() } else { d_raw };
            if d < sin_min - 1e-9 {
                let (at, normal) = hit.world();
                let angle = Angle::radians(blaze_math::asin(d_raw.clamp(-1.0, 1.0)));
                Some(DraftFace { at, normal, angle, min_draft: o.min_draft, undercut: d_raw < -1e-9 })
            } else {
                None
            }
        },
    )
}

// ---- common helpers ----------------------------------------------------------------------

fn finite(b: &Aabb3) -> bool {
    let (lo, hi) = (b.lo, b.hi);
    lo.x.is_finite() && lo.y.is_finite() && lo.z.is_finite() && hi.x.is_finite() && hi.y.is_finite() && hi.z.is_finite()
}

fn lengths(p: Vec3) -> [Length; 3] {
    [Length::m(p.x), Length::m(p.y), Length::m(p.z)]
}

fn push(stack: &mut Vec<Aabb3, Alloc<'_>>, b: Aabb3) -> Result<(), ModelError> {
    stack.try_push(b).map_err(|_| ModelError::Alloc { what: "a check's box stack" })
}

/// The cell cut at the middle of each axis at least half as long as its longest (so cells stay
/// near cubes however flat the start): two, four or eight children, in a fixed order.
pub(crate) fn split(b: &Aabb3) -> ([Aabb3; 8], usize) {
    let size = b.hi - b.lo;
    let longest = b.longest_side();
    let cut = [size.x * 2.0 >= longest, size.y * 2.0 >= longest, size.z * 2.0 >= longest];
    let mid = b.center();
    let mut out = [*b; 8];
    let mut n = 0;
    for i in 0..8usize {
        let bit = [i & 1 != 0, i & 2 != 0, i & 4 != 0];
        if (bit[0] && !cut[0]) || (bit[1] && !cut[1]) || (bit[2] && !cut[2]) {
            continue;
        }
        let (mut lo, mut hi) = (b.lo, b.hi);
        if cut[0] {
            if bit[0] { lo.x = mid.x } else { hi.x = mid.x }
        }
        if cut[1] {
            if bit[1] { lo.y = mid.y } else { hi.y = mid.y }
        }
        if cut[2] {
            if bit[2] { lo.z = mid.z } else { hi.z = mid.z }
        }
        out[n] = Aabb3::new(lo, hi);
        n += 1;
    }
    (out, n)
}
