//! The step bound of a proof: a walk bounded by a measure of the walk itself (rule 18), set by
//! the caller of the pure evaluator ([`with_steps`]) and taken by the walk ([`step`]).
//!
//! The bound is the task's, never a query's: it is a thread-local carried with the task across
//! its fiber waits (`blaze_graph::sched::carry`, registered once by the first bound), so a job
//! pumped onto the thread while the task waits sees no bound, and a step it gives is no one's.
//! Running out unwinds to the caller as a typed [`OutOfSteps`]; blaze-memo never memoizes an
//! unwind, so no record depends on the bound: a later proof with more steps runs what this one
//! left.

use std::{
    alloc::Global,
    cell::RefCell,
    fmt,
    num::NonZeroUsize,
    sync::{
        Once,
        atomic::{AtomicUsize, Ordering},
    },
};

use blaze_core::lock::Lock;
use blaze_hash::{HashMap, HashSet};

/// The bound of the walk this task is in: how many distinct steps it may take, and those it
/// took. Global heap: one per bound, freed as the bound ends.
struct Budget {
    given: u64,
    seen: HashSet<u128, Global>,
}

thread_local! {
    /// The bound of the task on this thread, if it is in one.
    static BOUND: RefCell<Option<Budget>> = const { RefCell::new(None) };
}

/// The bounds of tasks parked at a wait, by the word their task carries: moved here as the task
/// switches off its thread and back onto the thread it resumes on. Touched only at a switch of
/// a bounded task, never on a step.
static PARKED: Lock<Option<HashMap<usize, Budget, Global>>> = Lock::new("hir_ty_pure.steps.parked", None);

/// The next word naming a parked bound; never zero.
static NEXT: AtomicUsize = AtomicUsize::new(1);

fn take() -> Option<NonZeroUsize> {
    let budget = BOUND.with(|it| it.borrow_mut().take())?;
    let word = NonZeroUsize::new(NEXT.fetch_add(1, Ordering::Relaxed))
        .expect("the parked bounds' words do not wrap in a process's life");
    PARKED.lock().get_or_insert_with(|| HashMap::new_in(Global)).insert(word.get(), budget);
    Some(word)
}

fn put(word: NonZeroUsize) {
    let budget = PARKED
        .lock()
        .as_mut()
        .and_then(|it| it.remove(&word.get()))
        .unwrap_or_else(|| panic!("the parked step bound {word} was put back twice or never taken"));
    BOUND.with(|it| *it.borrow_mut() = Some(budget));
}

fn end(word: NonZeroUsize) {
    if let Some(parked) = PARKED.lock().as_mut() {
        parked.remove(&word.get());
    }
}

/// Registers the bound to be carried with each job across its waits. Called by the first bound.
fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| blaze_graph::sched::carry(blaze_graph::sched::Carried { take, put, end }));
}

/// Why [`with_steps`]'s walk stopped: it had taken `steps` distinct steps, all it was given,
/// and was taking another, `at`, as the walk named it ([`step`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OutOfSteps {
    pub steps: u64,
    pub at: String,
}

impl fmt::Display for OutOfSteps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let OutOfSteps { steps, at } = self;
        write!(f, "took {steps} distinct steps, all it was given, and was taking another: {at}")
    }
}

impl std::error::Error for OutOfSteps {}

/// Runs `f`, a walk that may take at most `steps` distinct steps on this task, each a key it
/// gives [`step`]. A step is taken once however often it is given, whether the summary it
/// stands for was run or found in the memo, so a walk that gives a step for each thing it
/// reaches takes the same steps on a cold memo and a warm one: its count, and whether it ran
/// out, depend only on what it walks. Out of steps, the step being given unwinds, as a panic
/// there does (blaze-memo memoizes nothing of it), and `f` gives way with [`OutOfSteps`].
/// Ended, it gives what `f` gave and the steps it took. Nested, the inner bound is at most what
/// the outer has left, and its steps are the outer's too.
///
/// Only this task is bounded: a job pumped onto its thread while it waits, or one it hands to
/// the pool, gives its steps to no one's bound.
pub fn with_steps<R>(steps: u64, f: impl FnOnce() -> R) -> Result<(R, u64), OutOfSteps> {
    register();
    let outer = BOUND.with(|it| {
        let mut bound = it.borrow_mut();
        let given = match &*bound {
            Some(outer) => steps.min(outer.given.saturating_sub(outer.seen.len() as u64)),
            None => steps,
        };
        bound.replace(Budget { given, seen: HashSet::new_in(Global) })
    });
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let taken = BOUND.with(|it| {
        let mut bound = it.borrow_mut();
        let ours = std::mem::replace(&mut *bound, outer);
        let taken = ours.as_ref().map_or(0, |it| it.seen.len() as u64);
        if let (Some(outer), Some(ours)) = (&mut *bound, ours) {
            outer.seen.extend(ours.seen);
        }
        taken
    });
    match ran {
        Ok(value) => Ok((value, taken)),
        Err(payload) => match payload.downcast::<OutOfSteps>() {
            Ok(out) => Err(*out),
            Err(payload) => std::panic::resume_unwind(payload),
        },
    }
}

/// The step `key` of the walk [`with_steps`] bounds on this task; nothing outside one, where
/// `key` is never made. A key already taken costs nothing. With none left, unwinds with
/// [`OutOfSteps`], `at` saying what the step was: `at` runs only then, with the bound set
/// aside, so a `Debug` that asks a query or gives a step can't run out again there, and a panic
/// in it leaves the step unnamed.
#[inline]
pub fn step(key: impl FnOnce() -> u128, at: impl FnOnce() -> String) {
    if !BOUND.with(|it| it.borrow().is_some()) {
        return;
    }
    // Made outside the borrow: a key that asks a query gives no step under it.
    let key = key();
    let out = BOUND.with(|it| {
        let mut bound = it.borrow_mut();
        let budget = bound.as_mut()?;
        if budget.seen.contains(&key) {
            return None;
        }
        if budget.seen.len() as u64 >= budget.given {
            return Some((budget.given, key));
        }
        budget.seen.insert(key);
        None
    });
    if let Some((steps, key)) = out {
        out_of_steps(steps, key, at);
    }
}

#[cold]
#[inline(never)]
fn out_of_steps(steps: u64, key: u128, at: impl FnOnce() -> String) -> ! {
    let bound = BOUND.with(|it| it.borrow_mut().take());
    let named = std::panic::catch_unwind(std::panic::AssertUnwindSafe(at));
    BOUND.with(|it| *it.borrow_mut() = bound);
    let at = named.unwrap_or_else(|_| format!("the step {key:#x} (its name panicked)"));
    std::panic::resume_unwind(Box::new_in(OutOfSteps { steps, at }, Global))
}
