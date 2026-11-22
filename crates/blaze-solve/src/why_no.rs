//! Why a goal has no solution: a typed chain from the goal asked to the first goal under it
//! that answers No by itself (`Solver::why_no`).
//!
//! It is on demand, never a mode: the caller asks it only for a goal it reports as unmet, and
//! the solver evaluates that one goal again, framing every goal under it (no fast path, no
//! global cache, no shared answer, no ADT summary, so each step is a frame whose candidates
//! are seen), and keeps per framed input the FIRST of its candidates whose nested goal had no
//! solution, with the goal that failed and how the candidate reached it ([`Reach`]). The walk
//! then goes from the root's input along those rows to the first input with none: the leaf,
//! classified by what its candidates met ([`Leaf`]).
//!
//! The Reference is the spec of each step: "Auto traits": a struct, enum, union or tuple is
//! `Send` if all its fields are, `&T` is `Send` if `T: Sync`, a closure is `Send` if all the
//! variables it captures are; "Implementations": an impl holds where its where-clauses hold.
//! A step is the goal and how it was reached; nothing here is a string (rule 76): the
//! printing is the caller's (front2 check.rs `diag_about`).
//!
//! Every table is bounded (rule 18): past [`MAX_WHY_ROWS`] rows nothing more is kept and a walk
//! that needs a row not kept ends at [`Leaf::NotKept`], named, never a guess.

use blaze_own::{Alloc, Vec};

use crate::assembly::{BuiltinSource, Source};
use crate::term::{Def, GoalSource, TyKind};
use crate::witness::{Choice, Refusal};

/// The most framed inputs one `why_no` keeps a row for.
pub const MAX_WHY_ROWS: usize = 1 << 12;
/// The most steps one chain walks (deeper than any real nesting of where-clauses and fields).
pub const MAX_WHY_DEPTH: usize = 256;

/// How a candidate's rule added one nested goal: what the goal is of the goal above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reach {
    /// Where-clause `n` of the impl the candidate is (`trait_goals::step_impl`), in the impl's
    /// order of where-clauses.
    Clause(u32),
    /// Field `n` of the ADT that is the goal's self type (an auto trait's constituent).
    Field(u32),
    /// Type argument `n` of the ADT that is the goal's self type: an auto trait's summary asks
    /// it in place of the fields that hold it (auto_summary.rs).
    AdtArg(u32),
    /// The pointee of a reference or raw pointer, the element of an array, slice or pattern
    /// type, `[u8]` of `str`.
    Pointee,
    /// Element `n` of a tuple.
    Element(u32),
    /// The tuple of a closure's captures (its upvars).
    Captures,
    /// The interior of an async block's future: the tuple of its captures and what it holds
    /// across an `.await`.
    Interior,
    /// `PhantomData<T>`'s `T`.
    Phantom,
    /// An opaque type's hidden type.
    Hidden,
    /// Added by a rule that names no position (the `n`th goal it added), or by a nested
    /// goal's response (`n` = `u32::MAX`).
    Nth(u32),
}

/// What a step's goal is of the goal above it, said for the reader: the candidate's source and
/// the rule's [`Reach`] read together with the self type of the goal above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reached {
    /// The goal asked.
    Root,
    /// Where-clause `clause` of impl `impl_`.
    ImplClause { impl_: Def, clause: u32 },
    /// Field `field` of the ADT `adt`.
    Field { adt: Def, field: u32 },
    /// Type argument `arg` of the ADT `adt` (its fields hold it).
    AdtArg { adt: Def, arg: u32 },
    /// The target of a reference.
    RefTarget,
    /// The pointee of a raw pointer.
    PtrTarget,
    /// The element of an array, slice or pattern type, or `[u8]` of `str`.
    Element,
    /// Element `index` of a tuple.
    TupleElement { index: u32 },
    /// Capture `capture` of the closure `closure` (in the closure's capture order).
    Capture { closure: Def, capture: u32 },
    /// Slot `slot` of the interior of the async block's future `coroutine` (its captures,
    /// then what it holds across an `.await`, in the witness's order).
    CoroutineSlot { coroutine: Def, slot: u32 },
    /// The captures of `closure` as one tuple (the next step names the capture).
    Captures { closure: Def },
    /// The interior of `coroutine` as one tuple (the next step names the slot).
    Interior { coroutine: Def },
    /// `T` of `PhantomData<T>`.
    Phantom,
    /// The hidden type of the opaque type `opaque`.
    Hidden { opaque: Def },
    /// The `nth` goal a candidate from `source` added for its own reason (`goal_source`): a
    /// where-clause of the environment or an alias's bound, a builtin's requirement; `nth` is
    /// `u32::MAX` for one handed back by a nested goal's response.
    Other { source: Option<Source>, goal_source: GoalSource, nth: u32 },
}

impl Reached {
    /// How a goal added by `cand` (`None`: by the frame before its candidates) with `reach`
    /// stands to the goal above it, whose self type is of kind `above` (one level, resolved).
    pub fn of(cand: Option<Choice>, goal_source: GoalSource, reach: Reach, above: Option<TyKind>) -> Reached {
        match (cand.map(|c| c.source), reach, above) {
            (Some(Source::Impl(impl_)), Reach::Clause(clause), _) => Reached::ImplClause { impl_, clause },
            (_, Reach::Field(field), Some(TyKind::Adt(adt, _))) => Reached::Field { adt, field },
            (_, Reach::AdtArg(arg), Some(TyKind::Adt(adt, _))) => Reached::AdtArg { adt, arg },
            (_, Reach::Pointee, Some(TyKind::Ref(..))) => Reached::RefTarget,
            (_, Reach::Pointee, Some(TyKind::RawPtr(..))) => Reached::PtrTarget,
            (_, Reach::Pointee, _) => Reached::Element,
            (_, Reach::Element(index), _) => Reached::TupleElement { index },
            (_, Reach::Captures, Some(TyKind::Closure(closure, _))) => Reached::Captures { closure },
            (_, Reach::Interior, Some(TyKind::Coroutine(coroutine, _))) => Reached::Interior { coroutine },
            (_, Reach::Phantom, _) => Reached::Phantom,
            (_, Reach::Hidden, Some(TyKind::Alias(_, _, opaque, _))) => Reached::Hidden { opaque },
            (source, Reach::Nth(nth), _) => Reached::Other { source, goal_source, nth },
            // A reach whose goal above is not of the kind the rule reads it from (never by
            // construction): said as the rule's own order, not guessed.
            (source, _, _) => Reached::Other { source, goal_source, nth: u32::MAX },
        }
    }
}

/// Why the leaf goal answers No by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaf {
    /// No candidate at all: no impl names the type, no where-clause or bound says it, no
    /// builtin rule applies.
    NoImpl,
    /// A negative impl names the type (`impl !Send for *const T`): the candidates that were
    /// refused include this impl, refused by its polarity.
    NegativeImpl { impl_: Def },
    /// Every candidate was refused by its rule: `first` the first, `why` how, `count` how many.
    /// An impl refused by its header is an impl for other types; a builtin refused is the
    /// type's own No (a raw pointer is not `Copy`-like here, a coroutine never `Unpin`).
    Refused { first: Choice, why: Refusal, count: u32 },
    /// A builtin rule (no impl) refused the goal and no other candidate came.
    BuiltinNo { source: BuiltinSource },
    /// The walk met a goal already on its path: a cycle the search answered No.
    Cycle,
    /// The goal holds (or is ambiguous) under `why_no`'s evaluation: no chain.
    NotNo,
    /// A row the walk needs was not kept ([`MAX_WHY_ROWS`]), or the chain is deeper than
    /// [`MAX_WHY_DEPTH`]: the chain stops here, said.
    NotKept,
}

/// One step: the goal (its canonical input in this solver: `Solver::why_goal` gives it back
/// as terms) and how it was reached from the step before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WhyStep {
    pub input: u32,
    pub reached: Reached,
}

/// The chain from the goal asked (`steps[0]`, reached [`Reached::Root`]) to the leaf (the last
/// step), and why the leaf has no solution.
pub struct WhyNo<'a> {
    pub steps: Vec<WhyStep, Alloc<'a>>,
    pub leaf: Leaf,
}

/// The first failed nested goal of one framed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Failed {
    pub child: u32,
    pub reached: Reached,
}

/// What `why_no`'s evaluation kept of one framed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub input: u32,
    pub failed: Option<Failed>,
    pub refused: u32,
    pub first_refused: Option<(Choice, Refusal)>,
    pub negative: Option<Def>,
}

/// The rows of one `why_no` evaluation.
pub(crate) struct NoLog<'a> {
    rows: Vec<Row, Alloc<'a>>,
}

impl<'a> NoLog<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> NoLog<'a> {
        NoLog { rows: Vec::new_in(alloc) }
    }

    /// Empty, for the next `why_no`.
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    fn row_mut(&mut self, input: u32) -> Option<&mut Row> {
        let at = match self.rows.iter().position(|r| r.input == input) {
            Some(at) => at,
            None => {
                if self.rows.len() >= MAX_WHY_ROWS {
                    return None;
                }
                self.rows.push(Row { input, failed: None, refused: 0, first_refused: None, negative: None });
                self.rows.len() - 1
            }
        };
        self.rows.get_mut(at)
    }

    /// A frame for `input` began: its row exists even if no candidate comes (a leaf of
    /// [`Leaf::NoImpl`]).
    pub fn framed(&mut self, input: u32) {
        let _ = self.row_mut(input);
    }

    /// Candidate `c` of `input` was refused by its rule (`negative`: an impl refused by its
    /// polarity, a negative impl naming the type).
    pub fn refused(&mut self, input: u32, c: Choice, why: Refusal, negative: bool) {
        let Some(row) = self.row_mut(input) else { return };
        row.refused += 1;
        if row.first_refused.is_none() {
            row.first_refused = Some((c, why));
        }
        if negative
            && row.negative.is_none()
            && let Source::Impl(d) = c.source
        {
            row.negative = Some(d);
        }
    }

    /// A nested goal (input `child`) of `input`'s candidate `cand` had no solution; the first
    /// such is kept (a cycle head's rerun meets the same first).
    pub fn failed(&mut self, input: u32, f: Failed) {
        let Some(row) = self.row_mut(input) else { return };
        if row.failed.is_none() {
            row.failed = Some(f);
        }
    }

    fn row(&self, input: u32) -> Option<&Row> {
        self.rows.iter().find(|r| r.input == input)
    }

    /// The chain from `root` (see the module doc). `holds`: the root's answer was not No.
    pub fn walk(&self, root: u32, holds: bool, alloc: Alloc<'a>) -> WhyNo<'a> {
        let mut steps: Vec<WhyStep, Alloc<'a>> = Vec::new_in(alloc);
        steps.push(WhyStep { input: root, reached: Reached::Root });
        if holds {
            return WhyNo { steps, leaf: Leaf::NotNo };
        }
        let mut input = root;
        let leaf = loop {
            if steps.len() >= MAX_WHY_DEPTH {
                break Leaf::NotKept;
            }
            // No row: the goal was never framed here (a provisional answer inside a cycle) or
            // the table was full: said, never taken for "no impl".
            let Some(row) = self.row(input) else { break Leaf::NotKept };
            match row.failed {
                Some(f) => {
                    if steps.iter().any(|s| s.input == f.child) {
                        break Leaf::Cycle;
                    }
                    steps.push(WhyStep { input: f.child, reached: f.reached });
                    input = f.child;
                }
                None => {
                    break match (row.negative, row.first_refused) {
                        (Some(impl_), _) => Leaf::NegativeImpl { impl_ },
                        (None, Some((first, why))) => match first.source {
                            Source::Builtin(source) if row.refused == 1 => Leaf::BuiltinNo { source },
                            _ => Leaf::Refused { first, why, count: row.refused },
                        },
                        (None, None) => Leaf::NoImpl,
                    };
                }
            }
        };
        fold_tuples(&mut steps);
        WhyNo { steps, leaf }
    }
}

/// A closure's captures and a future's interior are each one tuple goal under the goal of the
/// closure or future: the step to the tuple and the step to its element are one step, the
/// capture or slot ([`Reached::Capture`], [`Reached::CoroutineSlot`]).
pub(crate) fn fold_tuples(steps: &mut Vec<WhyStep, Alloc<'_>>) {
    let mut i = 0;
    while i + 1 < steps.len() {
        let folded = match (steps[i].reached, steps[i + 1].reached) {
            (Reached::Captures { closure }, Reached::TupleElement { index }) => Some(Reached::Capture { closure, capture: index }),
            (Reached::Interior { coroutine }, Reached::TupleElement { index }) => Some(Reached::CoroutineSlot { coroutine, slot: index }),
            _ => None,
        };
        if let Some(reached) = folded {
            steps[i + 1].reached = reached;
            let _ = steps.remove_shift(i);
        }
        i += 1;
    }
}
