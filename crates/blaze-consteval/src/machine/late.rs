//! What a body's code keeps of a stop met while compiling it, and whether that code is
//! complete.
//!
//! A stop met while compiling becomes an operation that raises it when reached (`Op::Late`).
//! It is one of two kinds, and the type says which ([`Late`]):
//!
//! - FINAL: the program's own fault, or a form the evaluator does not handle. It is the same on
//!   any retry, so the code holding it is complete and kept for every later evaluation.
//! - PENDING ([`Wait`]): it names something a later step can supply (a record, a body, an ADT's
//!   row or the impls that answer a projection, not made yet: the "need" the caller is told
//!   about, or the refusal the front end reads to make it). The code holding it is NOT
//!   complete, and its `lates` list is the list of what it waited for ([`waited`]).
//!
//! Incomplete code is compiled again (that body only, in place: callers' links stay valid) by
//! the first later evaluation that asks for it, before that evaluation runs it; within one
//! evaluation it is compiled at most once (`Code::compiled_in` against `Codes::evaluation`),
//! since the world does not change while an evaluation runs. So nothing outside the evaluator
//! ever throws compiled code away, and code compiled against complete answers is never
//! compiled again.
//!
//! The bound (rule 18): one compile of a body per evaluation, by construction; and every
//! recompile is charged to the evaluation's step bound (`Limits::steps`), one step per
//! operation compiled, so a world that never answers costs each evaluation a bounded amount and
//! the refusal (`Refusal::StepLimit`, `at` naming the recompiled body) says which body.

use core::alloc::Allocator;

use blaze_typeck::{ItemKey, Name, Range};

use super::code::Codes;
use super::{Machine, Need, Plant, R, Stop};
use crate::error::{At, Refusal};
use crate::layout::Layout;
use crate::ConstWorld;

/// What a reached `Op::Late` stops with.
#[derive(Debug)]
pub(super) enum Late {
    /// The same on any retry: the code holding it stays complete.
    Final(Refusal<'static>),
    /// Names what a later step can supply: the code holding it is not complete.
    Pending(Wait),
}

/// What a body's code waited for when it was compiled: each is something the world may hold
/// at a later evaluation that it did not hold then.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Wait {
    /// A record, a body or a discriminant not made yet: the caller is told and makes it.
    Need(Need),
    /// The world has no body, ADT definition or impl for an associated item yet
    /// ([`Refusal::Missing`]): the front end makes the row or the impl headers and asks again.
    Missing { what: &'static str, item: ItemKey },
    /// A projection `<_ as trait_>::name` in a type of the body no impl answered yet
    /// ([`Refusal::Projection`]).
    Projection { trait_: ItemKey, name: Name },
    /// A field's type holds a projection no impl answered yet ([`Refusal::FieldGround`] with
    /// `projection: Some`).
    FieldProjection { adt: ItemKey, field: u32, name: Name, projection: (ItemKey, Name) },
    /// A callee's kind needs a fact the world does not hold yet ([`Refusal::FnFact`]): the
    /// front end makes it (an impl's fn's signature, a trait's members) and asks again.
    FnFact { item: ItemKey, fact: crate::Fact },
}

impl Wait {
    /// The pending part of a refusal; `None`: the refusal is final. Every variant is named
    /// (no wildcard): a new refusal must be decided here.
    pub(crate) fn of(r: &Refusal<'_>) -> Option<Wait> {
        match r {
            Refusal::Missing { what, item } => Some(Wait::Missing { what: *what, item: *item }),
            Refusal::Projection { trait_, name } => Some(Wait::Projection { trait_: *trait_, name: *name }),
            Refusal::FieldGround { adt, field, name, projection: Some(p) } => {
                Some(Wait::FieldProjection { adt: *adt, field: *field, name: *name, projection: *p })
            }
            // An opaque type or another grounding refusal: not a projection a later impl answers.
            Refusal::FieldGround { projection: None, .. } => None,
            // A part `needs_drop` cannot judge: final, no later answer changes the type.
            Refusal::NeedsDrop { .. } => None,
            Refusal::FnFact { item, fact } => Some(Wait::FnFact { item: *item, fact: *fact }),
            Refusal::Overflow { .. }
            | Refusal::DivByZero
            | Refusal::RemByZero
            | Refusal::ShiftTooFar { .. }
            | Refusal::IndexOutOfBounds { .. }
            | Refusal::Uninit { .. }
            | Refusal::OutOfBounds { .. }
            | Refusal::Dangling
            | Refusal::PointerAsInt
            | Refusal::IntAsPointer
            | Refusal::PartialPointer { .. }
            | Refusal::PointerCompare
            | Refusal::WriteToImmutable
            | Refusal::MutableStatic { .. }
            | Refusal::NotConstFn { .. }
            | Refusal::UnknownIntrinsic { .. }
            | Refusal::InvalidBool { .. }
            | Refusal::InvalidChar { .. }
            | Refusal::InvalidTag { .. }
            | Refusal::TransmuteSize { .. }
            | Refusal::Unreachable
            | Refusal::NonzeroOnZero { .. }
            | Refusal::Panic { .. }
            | Refusal::DiscriminantOverflow { .. }
            | Refusal::StepLimit { .. }
            | Refusal::MemoryLimit { .. }
            | Refusal::CallDepth { .. }
            | Refusal::Cycle { .. }
            | Refusal::Unsupported { .. }
            | Refusal::Malformed { .. }
            | Refusal::NoSize { .. }
            | Refusal::Literal(_) => None,
        }
    }

    /// The stop a run raises when it reaches this wait: the same need or refusal as when it
    /// was met, so the caller makes what is named and asks again.
    fn stop<'b>(&self) -> Stop<'b> {
        match *self {
            Wait::Need(n) => Stop::Need(n),
            Wait::Missing { what, item } => Stop::Refused(Refusal::Missing { what, item }),
            Wait::Projection { trait_, name } => Stop::Refused(Refusal::Projection { trait_, name }),
            Wait::FieldProjection { adt, field, name, projection } => {
                Stop::Refused(Refusal::FieldGround { adt, field, name, projection: Some(projection) })
            }
            Wait::FnFact { item, fact } => Stop::Refused(Refusal::FnFact { item, fact }),
        }
    }
}

impl Late {
    /// What a stop met while compiling becomes.
    pub(super) fn of(s: Stop<'_>) -> Late {
        match s {
            Stop::Need(n) => Late::Pending(Wait::Need(n)),
            Stop::Refused(r) => match Wait::of(&r) {
                Some(w) => Late::Pending(w),
                None => Late::Final(r.detach()),
            },
        }
    }

    /// Whether code holding this is not complete. `Plant::KeepPendingCode` is the old
    /// behaviour, for the tests' control: only a need made code incomplete, and a pending
    /// refusal was kept as if final.
    pub(super) fn makes_incomplete(&self, plant: Plant) -> bool {
        match self {
            Late::Final(_) => false,
            Late::Pending(Wait::Need(_)) => true,
            Late::Pending(_) => plant != Plant::KeepPendingCode,
        }
    }

    /// The stop a run raises when it reaches this.
    pub(super) fn stop<'b>(&self) -> Stop<'b> {
        match self {
            Late::Final(r) => Stop::Refused(r.attach()),
            Late::Pending(w) => w.stop(),
        }
    }
}

/// What a body's code waited for when it was compiled: empty for complete code.
pub(super) fn waited(lates: &[Late]) -> impl Iterator<Item = Wait> + '_ {
    lates.iter().filter_map(|l| match l {
        Late::Pending(w) => Some(*w),
        Late::Final(_) => None,
    })
}

impl<A: Allocator + Clone> Codes<A> {
    /// How many times the body of `item` was compiled, over every instance of it in the table:
    /// the tests' counter of recompiles.
    pub(crate) fn compiles(&self, item: ItemKey) -> u32 {
        self.table.list.iter().filter(|c| c.item == item).map(|c| c.compiles).sum()
    }

    /// What the code of `item` (every instance of it) waited for when last compiled.
    pub(crate) fn waits(&self, item: ItemKey) -> usize {
        self.table.list.iter().filter(|c| c.item == item).map(|c| waited(&c.lates).count()).sum()
    }
}

impl<'w, 'l, 'g, 't, 'wk, 'a, W: ConstWorld, L: Layout, K: Allocator + Clone> Machine<'w, 'l, 'g, 't, 'wk, 'a, W, L, K> {
    /// Charges a recompile of `item`'s body (`ops` operations) to the evaluation's step bound;
    /// past it, the refusal names the body recompiled.
    pub(super) fn charge_recompile(&mut self, item: ItemKey, ops: usize) -> R<'a, ()> {
        self.steps = self.steps.saturating_add(ops as u64);
        if self.steps > self.limits.steps {
            self.at = Some(At { body: item, range: Range::default() });
            return Err(Stop::Refused(Refusal::StepLimit { limit: self.limits.steps }));
        }
        Ok(())
    }
}
