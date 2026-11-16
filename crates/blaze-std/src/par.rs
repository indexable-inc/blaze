//! Data parallelism over disjoint data (std.md, "Concurrency: what to reach for"): one slice,
//! or several columns of one table (docs/v2/plan/design/bottom.md, "Layout: columns"), cut
//! into chunks that tasks of a [`Scope`] work on at once.
//!
//! ```text
//! let scope = cx.scope();
//! let grain = Grain::new(NonZeroUsize::new(4096).unwrap(), TASK_FRAME);
//! par::for_each(cx, &scope, (&mut xs[..], &ys[..]), grain, |(xs, ys)| for (x, y) in xs.iter_mut().zip(ys) { *x += *y })?;
//! par::map_into(cx, &scope, &names, &mut lens, grain, |name| name.len())?;
//! let total = par::reduce(cx, &scope, &lens[..], grain, || 0, |sum, lens| sum + lens.iter().sum::<usize>(), |a, b| a + b)?;
//! scope.join(cx)?;
//! ```
//!
//! - **Disjoint by construction.** Data is cut with `split_at`/`split_at_mut` ([`Split`]), so
//!   each chunk is its own `&mut` range: user code holds no lock and no atomic, and the borrow
//!   checker proves no two chunks overlap. Columns are cut at the same index; columns of
//!   different lengths are refused ([`ParError::Ragged`]) before anything runs.
//! - **Deterministic.** Where the data is cut depends only on its length and the grain, never
//!   on the schedule, and [`reduce`] joins neighbours left to right by position, so its result
//!   is the same in every run and every layout, for a `join` that is not associative or
//!   commutative too (a float sum gives one answer). [`map_into`] writes item `i`'s result at
//!   `out[i]`.
//! - **The grain is the caller's** ([`Grain`]: rule "a cost the caller should decide is an
//!   argument"): the fewest items a chunk is worth, and the frame each chunk's task gets. A
//!   slice of `n` items makes `ceil(n / items)` chunks and one fewer tasks; the calling task
//!   works on one itself.
//! - **Nothing outlives the call.** Chunks run as tasks of the caller's `scope`, each joined
//!   before the call returns; a chunk that panics fails its task and the call
//!   ([`ParError::Task`]), and cancels the scope.
//! - **May overlap.** Each chunk is correct whether it runs beside the others or after them,
//!   so each is a [`Scope::may_overlap`] task: with no worker free it runs inline, and a scope
//!   narrowed to one worker gives the same result as one with many.
//! - **Placement.** Each chunk is a new task, placed by [`Scope::may_overlap`] on a worker of its own,
//!   and once started it stays there (crate::task, "A started task never changes its thread").
//!   Under `Inline` the chunks run one after another on the calling thread, in an order the
//!   simulation draws, which is how the tests show the result does not depend on it.
//! - **Bounds for later layouts.** The closures are `Sync` and the chunks and results `Send`
//!   now, though `Inline` runs everything on one thread, so code written today runs unchanged
//!   on a pool.

use core::{convert::Infallible, fmt, num::NonZeroUsize};

use crate::{
    Plant, Stopped,
    cx::Cx,
    task::{Scope, TaskError},
};

/// How finely to cut: the fewest items a chunk is worth, and the most bytes each chunk's
/// task's frame hands out ([`Scope::may_overlap`]'s `scratch`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grain {
    pub items: NonZeroUsize,
    pub scratch: usize,
}

impl Grain {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(items: NonZeroUsize, scratch: usize) -> Grain {
        Grain { items, scratch }
    }
}

/// Data that cuts into disjoint parts: a slice, shared or unique, or a tuple of them (columns
/// of one table, cut at the same index).
pub trait Split: Sized {
    /// Its length: every column's, or the first two that differ.
    fn lengths(&self) -> Result<usize, (usize, usize)>;
    /// The two parts before and after `at`, which is at most its length.
    fn split(self, at: usize) -> (Self, Self);
}

impl<T> Split for &[T] {
    fn lengths(&self) -> Result<usize, (usize, usize)> {
        Ok(self.len())
    }
    fn split(self, at: usize) -> (Self, Self) {
        self.split_at(at)
    }
}

impl<T> Split for &mut [T] {
    fn lengths(&self) -> Result<usize, (usize, usize)> {
        Ok(self.len())
    }
    fn split(self, at: usize) -> (Self, Self) {
        self.split_at_mut(at)
    }
}

/// Both the same length.
fn agree(a: usize, b: usize) -> Result<usize, (usize, usize)> {
    match a == b {
        true => Ok(a),
        false => Err((a, b)),
    }
}

impl<A: Split, B: Split> Split for (A, B) {
    fn lengths(&self) -> Result<usize, (usize, usize)> {
        agree(self.0.lengths()?, self.1.lengths()?)
    }
    fn split(self, at: usize) -> (Self, Self) {
        let ((a0, a1), (b0, b1)) = (self.0.split(at), self.1.split(at));
        ((a0, b0), (a1, b1))
    }
}

impl<A: Split, B: Split, C: Split> Split for (A, B, C) {
    fn lengths(&self) -> Result<usize, (usize, usize)> {
        agree(agree(self.0.lengths()?, self.1.lengths()?)?, self.2.lengths()?)
    }
    fn split(self, at: usize) -> (Self, Self) {
        let ((a0, a1), (b0, b1), (c0, c1)) = (self.0.split(at), self.1.split(at), self.2.split(at));
        ((a0, b0, c0), (a1, b1, c1))
    }
}

impl<A: Split, B: Split, C: Split, D: Split> Split for (A, B, C, D) {
    fn lengths(&self) -> Result<usize, (usize, usize)> {
        agree(agree(agree(self.0.lengths()?, self.1.lengths()?)?, self.2.lengths()?)?, self.3.lengths()?)
    }
    fn split(self, at: usize) -> (Self, Self) {
        let ((a0, a1), (b0, b1), (c0, c1), (d0, d1)) =
            (self.0.split(at), self.1.split(at), self.2.split(at), self.3.split(at));
        ((a0, b0, c0, d0), (a1, b1, c1, d1))
    }
}

/// Why a parallel call gave no result.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum ParError {
    /// A chunk's task failed (it panicked, or got no frame), which cancelled the scope.
    Task(TaskError),
    /// The calling task's scope was cancelled before a chunk began.
    Stopped(Stopped),
    /// Two columns have these lengths.
    Ragged { first: usize, other: usize },
}

impl fmt::Display for ParError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParError::Task(failed) => write!(f, "bstd::par: a chunk failed: {failed}"),
            ParError::Stopped(stopped) => write!(f, "bstd::par: stopped before a chunk began: {stopped}"),
            ParError::Ragged { first, other } => {
                write!(f, "bstd::par: columns of {first} and {other} items cannot be cut at the same places")
            }
        }
    }
}

impl core::error::Error for ParError {}

/// `work` on every chunk of `data`, the chunks at once.
pub fn for_each<D: Split + Send>(
    cx: &Cx,
    scope: &Scope<'_>,
    data: D,
    grain: Grain,
    work: impl Fn(D) + Sync,
) -> Result<(), ParError> {
    reduce(cx, scope, data, grain, || (), |(), chunk| work(chunk), |(), ()| ())
}

/// `out[i] = make(&input[i])` for every `i`, the chunks at once. `input` and `out` are the
/// same length ([`ParError::Ragged`] otherwise).
pub fn map_into<T: Sync, O: Send>(
    cx: &Cx,
    scope: &Scope<'_>,
    input: &[T],
    out: &mut [O],
    grain: Grain,
    make: impl Fn(&T) -> O + Sync,
) -> Result<(), ParError> {
    for_each(cx, scope, (input, out), grain, |(input, out)| {
        for (from, to) in input.iter().zip(out) {
            *to = make(from);
        }
    })
}

/// Every chunk folded from `identity()` by `fold`, then neighbours joined by `join`, left to
/// right by position: the same result whatever ran first (see the module's doc).
pub fn reduce<D: Split + Send, R: Send>(
    cx: &Cx,
    scope: &Scope<'_>,
    data: D,
    grain: Grain,
    identity: impl Fn() -> R + Sync,
    fold: impl Fn(R, D) -> R + Sync,
    join: impl Fn(R, R) -> R + Sync,
) -> Result<R, ParError> {
    let len = data.lengths().map_err(|(first, other)| ParError::Ragged { first, other })?;
    let ops = Ops { grain, identity: &identity, fold: &fold, join: &join };
    cut(cx, scope, data, len, &ops)
}

/// What every chunk is done with.
struct Ops<'f, I, F, J> {
    grain: Grain,
    identity: &'f I,
    fold: &'f F,
    join: &'f J,
}

/// `data`, of `len` items, folded: one chunk here, or cut in two at a chunk boundary chosen by
/// `len` and the grain alone, the left half a task of `scope`, the halves joined in order.
fn cut<D, R, I, F, J>(cx: &Cx, scope: &Scope<'_>, data: D, len: usize, ops: &Ops<'_, I, F, J>) -> Result<R, ParError>
where
    D: Split + Send,
    R: Send,
    I: Fn() -> R + Sync,
    F: Fn(R, D) -> R + Sync,
    J: Fn(R, R) -> R + Sync,
{
    let items = ops.grain.items.get();
    let chunks = len.div_ceil(items);
    if chunks <= 1 {
        cx.check().map_err(ParError::Stopped)?;
        return Ok((ops.fold)((ops.identity)(), data));
    }
    let at = (chunks / 2) * items;
    let (left, right) = data.split(at);
    // Which half runs first: under `Inline` one after the other, so the simulation draws it, and
    // a native world takes the left; a pool runs them at once.
    let right_first = cx.io().with_sim(|sim| sim.rng().coin()).unwrap_or(false);
    let mut right = Some(right);
    let mut ran_right = None;
    if right_first && let Some(right) = right.take() {
        ran_right = Some(cut(cx, scope, right, len - at, ops));
    }
    let task = scope.may_overlap("bstd.par", ops.grain.scratch, |cx| Ok::<_, Infallible>(cut(cx, scope, left, at, ops)));
    if let Some(right) = right.take() {
        ran_right = Some(cut(cx, scope, right, len - at, ops));
    }
    let left = match task.join(cx) {
        Ok(left) => left,
        Err(failed) => Err(ParError::Task(failed)),
    };
    let right = ran_right.expect("bstd::par: the right half ran, before or after the left");
    let (left, right) = (left?, right?);
    Ok(match (cx.plant(), right_first) {
        // The planted bug: joined in the order they ran.
        (Plant::ParJoinsInRunOrder, true) => (ops.join)(right, left),
        _ => (ops.join)(left, right),
    })
}
