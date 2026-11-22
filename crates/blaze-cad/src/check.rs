//! Design checks: [`min_wall`], [`no_interference`], [`fits_within`]. Each reads `Detail::Shape`
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
//!   SAMPLING CELLS) that the search does not clear. A cell is CLEARED when the field at its
//!   centre is farther from zero than its half diagonal (`|f| <= distance` at every level, so no
//!   boundary point is in the cell: one evaluation), or, above and below the sampling size, when
//!   an interval over it excludes zero. The point test is what clears the cells an interval
//!   keeps for no surface: a linear repeat's cells across the plane between two copies, where
//!   the copy index is an interval and so the copy's field is taken over a whole step.
//!
//!   A SAMPLE is a surface point found in its cell's reach: Newton steps from the centre (at
//!   most eight, each kept within the cell's diameter of it) to `|f| <= tolerance`; when they do
//!   not settle, a SIGN BRACKET: a step, or a corner of the cell, of the other sign than the
//!   centre, narrowed by bisection on the sign to the tolerance (its inside end is the sample).
//!   Newton alone does not settle on a field that is only a bound outside the solid: on
//!   `max(a, -b)` (a ring cut by a taller cylinder) a point in the hole between the two caps'
//!   planes is sent to one plane by the term `a` and back to the other by `-b`, for ever (the
//!   frames' 138564 unsettled places; the planted `NoBracket`). A sampling cell with no bracket
//!   is split, down to a sixteenth of its side (each part cleared or sampled as above); one still
//!   without a bracket at that size is unsettled.
//!
//!   THE RAY from a sample along the inward normal: inside, the field is the distance to the
//!   boundary (`AtLeastInside`), so the ball of radius `-f` about the ray's middle is solid, and
//!   when it reaches both ends (a tolerance to spare) the ray stays in for the whole wall: one
//!   evaluation, the common case on a thick face. Otherwise the ray is marched by `|f|` (no step
//!   crosses the surface), at least `wall / 1024` a step, and its exit placed to the tolerance by
//!   bisection on the sign. A FINDING is a sample whose chord is under `wall`: its point and the
//!   chord. A PASS is every sample's chord at least `wall`: a thin patch at least a sampling
//!   cell across is found; one smaller, or a gap in the material under `wall / 1024` across the
//!   ray, can be missed. A sample whose bracket or ray spent `most_steps_per_point` is reported,
//!   as [`Unsettled::Open`]: the only way a place is unsettled is that its surface point or its
//!   ray's exit could not be bracketed within the budget.
//!
//!   COST. Sampling cells looked at: about `k A / s^2` for a surface of area `A` and sampling
//!   size `s = wall / samples_per_wall` (`k` about 2 to 4: the cells an interval or the centre
//!   test leaves near a surface), plus, where an interval is loose, the cells of that region
//!   cleared at one evaluation each. Evaluations per sample: one to clear; about three on a thick
//!   face (the centre's value with its gradient, one Newton step, the ball); a bracket adds at
//!   most 8 (the corners) and `log2(4 h / tolerance)` (`h` the half diagonal: 12 at a 6 mm wall
//!   and the defaults); a marched ray `log2(T / least)` to cross a plate of thickness `T` and
//!   `log2(least / tolerance)` to place its exit, and at most `wall / least` where it runs along a
//!   face within `least` of it. [`Checked::samples`] and [`Checked::points`] report both.
//! - [`no_interference`]: `distance_between` settles the gap to the tolerance; a gap whose upper
//!   end is under the clearance is a finding, one whose lower end is at or over it a pass, and
//!   anything else (the clearance inside the settled interval, or the budget spent first) is
//!   [`Verdict::Unknown`]: NEVER A PASS.
//! - [`fits_within`]: the part without the envelope; a point of it deeper than the tolerance (in
//!   the field, which is a distance for exact operands) is an overhang, a box search whose every
//!   cell's interval stays above minus the tolerance is a pass.
//!
//! # Records
//!
//! A verdict is a record under its [`CheckKey`]: the check, the digests of the shape tapes it
//! read, its arguments in nanometres and [`CHECK_SCHEMA`]. Moving a part changes no key of a
//! check of that part alone (its tape is unplaced); editing a detail node changes none (the
//! tapes are `Detail::Shape`). A finding's position is in the frame of the tape it was found in:
//! the part's own for [`min_wall`], the assembly's for the others.

#![blaze::sans_io]

use blaze_cad_field::{
    AtLeastInside, Bound, CadPlant, CompileOpts, Detail, Digest, DistanceOpts, Gap, Graph, InspectError, Level, Scratch, Solid, Tape,
    compile, distance_between, eval_box, eval_grad, eval_point,
};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, Vec};
use blaze_text::{Put, Show, Writes};
use blaze_units::Length;

use crate::{FitPlant, InterferencePlant, ModelError, Part, WallPlant};

/// The checks' record layout; bumped when a verdict's meaning or a key's fields change, or when
/// the same key can get another verdict (2: the wall is the inward chord; 3: [`min_wall`]'s
/// samples are bracketed, so places once unsettled now settle).
pub const CHECK_SCHEMA: u32 = 3;

/// Which check a key is of.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CheckKind {
    MinWall,
    NoInterference,
    FitsWithin,
}

/// The key a verdict is a record under (see the module).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CheckKey {
    pub check: CheckKind,
    /// The shape tapes' digests (sorted, for the symmetric [`no_interference`]; the second is
    /// zero for a check of one tape).
    pub tapes: [Digest; 2],
    /// The arguments, nanometres or counts, in the check's order (see each check).
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
    /// [`min_wall`]'s sampling cells looked at (cleared or sampled); zero for the other checks.
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

// ---- printing for people (and language models) -------------------------------------------
//
// A finding prints as one sentence, millimetres throughout (a point's three coordinates in one
// unit), by integer arithmetic from the nanometres: never rounded through a float to be shown.

/// `nm` in millimetres to `places` decimals (at most 6), halves away from zero, trailing zeros
/// dropped; no minus sign on a value that prints as zero.
fn put_mm<P: Put + ?Sized>(out: &mut P, nm: i64, places: u32) {
    let places = places.min(6);
    let unit = 10u128.pow(6 - places);
    let scale = 10u128.pow(places);
    let q = (i128::from(nm).unsigned_abs() + unit / 2) / unit;
    let (whole, mut frac) = (q / scale, q % scale);
    if nm < 0 && q != 0 {
        out.put_byte(b'-');
    }
    out.int(whole);
    if frac != 0 {
        let mut digits = places as usize;
        while frac % 10 == 0 {
            frac /= 10;
            digits -= 1;
        }
        out.put_byte(b'.');
        out.int_zeros(frac, digits);
    }
}

/// `(x, y, z) mm` to the micrometre.
fn put_point<P: Put + ?Sized>(out: &mut P, at: &[Length; 3]) {
    out.put_byte(b'(');
    for (k, c) in at.iter().enumerate() {
        if k > 0 {
            out.put_str(", ");
        }
        put_mm(out, c.as_nm(), 3);
    }
    out.put_str(") mm");
}

/// "thin wall of about 4.2 mm at (295.898, 290.039, -0.586) mm"
impl Show for ThinWall {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str("thin wall of about ");
        put_mm(out, self.thickness.as_nm(), 1);
        out.put_str(" mm at ");
        put_point(out, &self.at);
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

/// "sticks out of the envelope by about 3.5 mm at (1, 2, 3) mm"
impl Show for Overhang {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str("sticks out of the envelope by about ");
        put_mm(out, self.depth.as_nm(), 1);
        out.put_str(" mm at ");
        put_point(out, &self.at);
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
                put_point(out, near);
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

/// Two parts nearer than the clearance: the gap lies in `gap` (zero when they meet).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Interference {
    pub gap: [Length; 2],
    pub clearance: Length,
}

/// A point of the part outside the envelope: `depth` is the difference's field there.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Overhang {
    pub at: [Length; 3],
    pub depth: Length,
}

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

    /// The interval of the field over a cell: `(lo, hi)`.
    fn cell(&mut self, c: &Aabb3) -> (f64, f64) {
        let (i, _) = eval_box(self.t, c, self.sc);
        (i.lo(), i.hi())
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
    let mut pr = Probe { t: &s_tape, sc: &mut sc, points: 0, used: 0, most: o.most_steps_per_point };
    let mut stack: Vec<Aabb3, Alloc<'_>> = Vec::new_in(o.scratch);
    push(&mut stack, root)?;
    let mut boxes: u64 = 0;
    let mut samples: u64 = 0;
    let mut open: u64 = 0;
    let mut near: Option<Vec3> = None;
    while let Some(c) = stack.pop() {
        boxes += 1;
        if boxes > o.most_boxes {
            let verdict = Verdict::Unknown(Unsettled::Budget { boxes });
            return Ok(Checked { verdict, key, boxes, samples, points: pr.points });
        }
        let side = c.longest_side();
        let sampling = side <= leaf;
        if sampling {
            samples += 1;
        }
        // `|f| <= distance` at every level: a centre farther from zero than the half diagonal
        // has no boundary point in its cell.
        if pr.point(c.center()).abs() > (c.hi - c.lo).length() / 2.0 {
            continue;
        }
        // A sampling cell straight from the search (side over `leaf / 2`) skips the interval:
        // over a cell that small it excludes what the centre test does. Larger cells and the
        // parts of a split one take it.
        if !sampling || side <= leaf / 2.0 {
            let (lo, hi) = pr.cell(&c);
            if lo > 0.0 || hi < 0.0 {
                continue;
            }
        }
        if !sampling {
            let (kids, n) = split(&c);
            for k in &kids[..n] {
                push(&mut stack, *k)?;
            }
            continue;
        }
        match wall_at(&mut pr, &c, wall, tol, plant) {
            Sample::Thick => {}
            Sample::Thin { at, thickness } => {
                let found = ThinWall { at: lengths(at), thickness: Length::m(thickness), wall: o.wall };
                return Ok(Checked { verdict: Verdict::Found(found), key, boxes, samples, points: pr.points });
            }
            Sample::NoBracket if side > floor => {
                let (kids, n) = split(&c);
                for k in &kids[..n] {
                    push(&mut stack, *k)?;
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
    Ok(Checked { verdict, key, boxes, samples, points: pr.points })
}

/// What [`no_interference`] takes.
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
    /// Boxes `distance_between` may take.
    pub most_boxes: u32 = 1 << 20,
}

/// Whether the solids of `a` and `b` (shape tapes in one frame, see [`crate::placed_tape`]) are
/// at least the clearance apart. The tape whose field measures outside its solid (level at least
/// `Outside`) is taken as the measured one, so either order works when one of them does.
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
    let gap = distance_between(a, b, DistanceOpts { tolerance: o.tolerance.as_m(), most_boxes: o.most_boxes }, &mut sc)
        .map_err(ModelError::Inspect)?;
    let c = o.clearance.as_m();
    let (verdict, boxes) = match gap {
        Gap::Empty => return Err(ModelError::EmptySolid { function: "no_interference", arg: "a" }),
        Gap::Settled(iv) => (judge(iv.lo(), iv.hi(), c, o.clearance, Unsettled::Straddles { lo: Length::m(iv.lo()), hi: Length::m(iv.hi()) }), 0),
        Gap::BudgetSpent { so_far, boxes } => (judge(so_far.lo(), so_far.hi(), c, o.clearance, Unsettled::Budget { boxes }), boxes),
    };
    let verdict = match verdict {
        Verdict::Unknown(_) if plant.has(InterferencePlant::UnknownPasses) => Verdict::Pass,
        v => v,
    };
    Ok(Checked { verdict, key, boxes, samples: 0, points: 0 })
}

fn judge(lo: f64, hi: f64, c: f64, clearance: Length, open: Unsettled) -> Verdict<Interference> {
    if hi < c {
        Verdict::Found(Interference { gap: [Length::m(lo), Length::m(hi)], clearance })
    } else if lo >= c {
        Verdict::Pass
    } else {
        Verdict::Unknown(open)
    }
}

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
    let mut stack: Vec<Aabb3, Alloc<'_>> = Vec::new_in(o.scratch);
    push(&mut stack, root)?;
    let mut boxes: u64 = 0;
    let mut points: u64 = 0;
    let mut open: u64 = 0;
    let mut near: Option<Vec3> = None;
    while let Some(c) = stack.pop() {
        boxes += 1;
        if boxes > o.most_boxes {
            return Ok(Checked { verdict: Verdict::Unknown(Unsettled::Budget { boxes }), key, boxes, samples: 0, points });
        }
        let (f, _) = eval_box(&t, &c, &mut sc);
        if f.lo() > -tol {
            continue;
        }
        let centre = c.center();
        points += 1;
        let v = eval_point(&t, centre, &mut sc);
        if v < -tol {
            let found = Overhang { at: lengths(centre), depth: Length::m(-v) };
            return Ok(Checked { verdict: Verdict::Found(found), key, boxes, samples: 0, points });
        }
        if c.longest_side() <= tol {
            open += 1;
            near.get_or_insert(centre);
            continue;
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
