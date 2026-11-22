//! Why a goal is still ambiguous: a typed chain from the goal asked to the first goal under it
//! that is ambiguous by itself (`Solver::why_maybe`), the sibling of why_no.rs.
//!
//! It is on demand, never a mode: the caller asks it only for a goal it reports as undecided
//! (E0283, E0282, a stalled goal decided ambiguous), and the solver evaluates that one goal
//! again as `why_no` does (every goal under it framed: no fast path, no global cache, no shared
//! answer, no ADT summary). Per framed input it keeps what the frame's merge ended at
//! ([`Verdict`]): two or more candidates it could not choose between, a limit, or the one
//! candidate whose response the merge took, with why that candidate is not certain
//! ([`Cause`]): its rule said so by itself, or a nested goal of it stayed undecided. The walk
//! goes from the root's input along the nested goals to the first input whose verdict is
//! primitive: the leaf ([`Leaf`]).
//!
//! The Reference is the spec of each step, as for `why_no` ("Implementations": an impl holds
//! where its where-clauses hold; "Auto traits": a type is `Send` when its constituents are). A
//! goal is ambiguous by itself when the language cannot decide it yet: a type in it is still an
//! inference variable, two impls or where-clauses may each hold, a recursion limit was met, or
//! a structure the goal looks into is not known (an opaque type's hidden type not defined, an
//! async block's interior or a closure's captures still undecided). Nothing here is a string
//! (rule 76): the printing is the caller's (front2 check.rs `why_words`).
//!
//! Every table is bounded (rule 18): past [`MAX_WHY_ROWS`] rows nothing more is kept and a walk
//! that needs a row not kept ends at [`Leaf::NotKept`], named, never a guess.

use blaze_own::{Alloc, Vec};

use crate::assembly::Cand;
use crate::term::Def;
use crate::why_no::{MAX_WHY_DEPTH, MAX_WHY_ROWS, Reached, WhyStep, fold_tuples};
use crate::witness::Choice;

/// Which limit an evaluation met.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LimitHit {
    /// The goal stood `depth` frames deep, at the solver's recursion limit `limit`
    /// (`stack_overflow_result`; rustc's E0275).
    Depth { depth: u32, limit: u32 },
    /// A cycle head's result did not settle in `reruns` reruns (`fixpoint_overflow_result`).
    Fixpoint { reruns: u32 },
    /// A candidate's nested goals kept changing for `rounds` rounds
    /// (`evaluate_added_goals`'s step limit).
    Rounds { rounds: u32 },
}

/// A structure the leaf goal looks into that is not known yet: the record that would open it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unopened {
    /// The hidden type of the opaque type `opaque` is not defined in this world yet (the
    /// defining body's record has not given it).
    Hidden { opaque: Def },
    /// The interior of the async block's future `coroutine` (its captures and what it holds
    /// across an `.await`) is still undecided.
    Interior { coroutine: Def },
    /// The captures of the closure `closure` are still undecided.
    Captures { closure: Def },
}

/// Why the leaf goal is ambiguous by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaf {
    /// Term `term` of the leaf goal (its terms in order: a trait goal's arguments, `Self`
    /// first; a projection's alias, then its term) is, or holds, an inference variable, and
    /// the rule of `cand` cannot decide the goal before it is known.
    Unresolved { cand: Choice, term: u32 },
    /// Two or more candidates may hold (`count`, the first two by key) and the merge cannot
    /// choose (`bail_with_ambiguity`).
    Candidates { first: Choice, second: Choice, count: u32 },
    /// A limit was met.
    Limit(LimitHit),
    /// The goal looks into a structure not known yet.
    Unopened(Unopened),
    /// The projection of the associated item `alias` did not normalize: `cand`'s rule stalled
    /// with no variable in the alias to name.
    Normalize { alias: Def, cand: Choice },
    /// The rule of `cand` answered ambiguous by itself with no variable in the goal (an
    /// unevaluated constant, a closure kind not known yet).
    Rule { cand: Choice },
    /// The walk met a goal already on its path: an inductive cycle the search answered
    /// ambiguous.
    Cycle,
    /// The goal is not ambiguous under `why_maybe`'s evaluation (it holds or fails): no chain.
    NotMaybe,
    /// A row the walk needs was not kept ([`MAX_WHY_ROWS`]), or the chain is deeper than
    /// [`MAX_WHY_DEPTH`], or the merge's choice has no cause kept: the chain stops here, said.
    NotKept,
}

/// The chain from the goal asked (`steps[0]`, reached [`Reached::Root`]) to the leaf (the last
/// step), and why the leaf is ambiguous.
pub struct WhyMaybe<'a> {
    pub steps: Vec<WhyStep, Alloc<'a>>,
    pub leaf: Leaf,
}

/// Why a candidate's own rule (its shallow certainty) was not `Yes`, read when the rule said
/// so (eval.rs `maybe_shallow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shallow {
    Unresolved { term: u32 },
    Hidden { opaque: Def },
    Normalize { alias: Def },
    Rule,
}

/// Why one candidate's response was not `Yes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// Its rule said so by itself.
    Shallow(Shallow),
    /// Its first nested goal still pending when the nested goals settled: that goal's input
    /// and how the candidate reached it.
    Nested { child: u32, reached: Reached },
    /// Its nested goals hit the round limit.
    Rounds,
    /// One of its nested goals was refused a frame by the depth limit, and the overflow
    /// decides its certainty (`Certainty::and`: overflow dominates ambiguity): the chain ends
    /// at this frame, not at whichever nested goal happened to be asked first at the limit.
    Limit(LimitHit),
    /// Its nested goals settled not `Yes` with no goal left pending (never by construction):
    /// said, never guessed.
    Unknown,
}

/// The candidates a merge could not choose between (`bail_with_ambiguity`'s).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bail {
    pub first: Choice,
    pub second: Choice,
    pub count: u32,
}

impl Bail {
    /// The bail over `cands` (in the merge's order); `None` under two.
    pub fn of<'c>(mut cands: impl Iterator<Item = &'c Cand>) -> Option<Bail> {
        let first = Choice::of(cands.next()?);
        let second = Choice::of(cands.next()?);
        let rest = cands.count();
        Some(Bail { first, second, count: u32::try_from(rest).unwrap_or(u32::MAX).saturating_add(2) })
    }
}

/// What the merge of one framed input ended at, in its last framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Bail(Bail),
    Cand { cand: Choice, cause: Cause },
    Limit(LimitHit),
    /// Not ambiguous (yes or no), or ambiguous with no candidate's cause kept.
    None,
}

#[derive(Clone, Copy, Debug)]
struct Row {
    input: u32,
    verdict: Verdict,
}

/// What `step` noted of the candidate of the frame at one stack index, read when the
/// candidate's nested goals settle (`nested_done`): no frame above it can be at its index.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Note {
    pub shallow: Shallow,
    pub rounds: bool,
    /// A nested goal of this candidate was refused a frame by the depth limit (eval.rs
    /// `call`'s `stack_overflow_result`): this frame is where the limit was met. Not carried
    /// to the parent, unlike the frame's `overflow`.
    pub limit: Option<LimitHit>,
}

/// The rows of one `why_maybe` evaluation.
pub(crate) struct MaybeLog<'a> {
    rows: Vec<Row, Alloc<'a>>,
    /// The cause of each candidate response of the frames on the stack, by the response's
    /// index in the solver's `dones`; popped at its frame's merge.
    cands: Vec<(u32, Choice, Cause), Alloc<'a>>,
    /// Per stack index, the candidate's note ([`Note`]).
    notes: Vec<Note, Alloc<'a>>,
}

impl<'a> MaybeLog<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> MaybeLog<'a> {
        MaybeLog { rows: Vec::new_in(alloc), cands: Vec::new_in(alloc), notes: Vec::new_in(alloc) }
    }

    /// Empty, for the next `why_maybe`.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.cands.clear();
        self.notes.clear();
    }

    /// The note of the frame at stack index `top`, made if new.
    pub fn note(&mut self, top: usize) -> Option<&mut Note> {
        if top >= MAX_WHY_DEPTH * 4 {
            return None;
        }
        while self.notes.len() <= top {
            self.notes.push(Note { shallow: Shallow::Rule, rounds: false, limit: None });
        }
        self.notes.get_mut(top)
    }

    /// The note of the frame at `top` as it stands (`Rule`, no rounds, if never noted).
    pub fn note_of(&self, top: usize) -> Note {
        self.notes.get(top).copied().unwrap_or(Note { shallow: Shallow::Rule, rounds: false, limit: None })
    }

    /// The candidate response at `dones` index `done` is not `Yes`, for `cause`.
    pub fn cand_done(&mut self, done: u32, cand: Choice, cause: Cause) {
        if self.cands.len() >= MAX_WHY_ROWS {
            return;
        }
        self.cands.push((done, cand, cause));
    }

    /// The cause kept for the response at `dones` index `done` (the latest: an older entry at
    /// the same index is a popped frame's).
    pub fn cause_of(&self, done: u32) -> Option<(Choice, Cause)> {
        self.cands.iter().rev().find(|(d, _, _)| *d == done).map(|&(_, c, k)| (c, k))
    }

    /// The frame whose responses start at `dones_lo` merged: its entries go.
    pub fn pop_cands(&mut self, dones_lo: u32) {
        while self.cands.last().is_some_and(|(d, _, _)| *d >= dones_lo) {
            let _ = self.cands.pop();
        }
    }

    /// `input`'s verdict, its last framing's.
    pub fn verdict(&mut self, input: u32, verdict: Verdict) {
        match self.rows.iter_mut().find(|r| r.input == input) {
            Some(r) => r.verdict = verdict,
            None => {
                if self.rows.len() < MAX_WHY_ROWS {
                    self.rows.push(Row { input, verdict });
                }
            }
        }
    }

    fn row(&self, input: u32) -> Option<&Row> {
        self.rows.iter().find(|r| r.input == input)
    }

    /// The chain from `root` (see the module doc). `maybe`: the root's answer was ambiguous.
    pub fn walk(&self, root: u32, maybe: bool, alloc: Alloc<'a>) -> WhyMaybe<'a> {
        let mut steps: Vec<WhyStep, Alloc<'a>> = Vec::new_in(alloc);
        steps.push(WhyStep { input: root, reached: Reached::Root });
        if !maybe {
            return WhyMaybe { steps, leaf: Leaf::NotMaybe };
        }
        let mut input = root;
        let leaf = loop {
            if steps.len() >= MAX_WHY_DEPTH {
                break Leaf::NotKept;
            }
            let Some(row) = self.row(input) else { break Leaf::NotKept };
            match row.verdict {
                Verdict::Bail(b) => break Leaf::Candidates { first: b.first, second: b.second, count: b.count },
                Verdict::Limit(l) => break Leaf::Limit(l),
                Verdict::None => break Leaf::NotKept,
                Verdict::Cand { cand, cause } => match cause {
                    Cause::Shallow(Shallow::Unresolved { term }) => break Leaf::Unresolved { cand, term },
                    Cause::Shallow(Shallow::Hidden { opaque }) => break Leaf::Unopened(Unopened::Hidden { opaque }),
                    Cause::Shallow(Shallow::Normalize { alias }) => break Leaf::Normalize { alias, cand },
                    Cause::Shallow(Shallow::Rule) => break Leaf::Rule { cand },
                    Cause::Rounds => break Leaf::Limit(LimitHit::Rounds { rounds: crate::search::FIXPOINT_STEP_LIMIT }),
                    Cause::Limit(l) => break Leaf::Limit(l),
                    Cause::Unknown => break Leaf::NotKept,
                    Cause::Nested { child, reached } => {
                        if steps.iter().any(|s| s.input == child) {
                            break Leaf::Cycle;
                        }
                        steps.push(WhyStep { input: child, reached });
                        input = child;
                    }
                },
            }
        };
        // A goal on a closure's captures, a future's interior or an opaque's hidden type whose
        // own `Self` is still a variable: that structure is what is not known yet, said by it.
        let leaf = match (leaf, steps.last().map(|s| s.reached)) {
            (Leaf::Unresolved { term: 0, .. }, Some(Reached::Interior { coroutine })) => Leaf::Unopened(Unopened::Interior { coroutine }),
            (Leaf::Unresolved { term: 0, .. }, Some(Reached::Captures { closure })) => Leaf::Unopened(Unopened::Captures { closure }),
            (Leaf::Unresolved { term: 0, .. }, Some(Reached::Hidden { opaque })) => Leaf::Unopened(Unopened::Hidden { opaque }),
            (leaf, _) => leaf,
        };
        fold_tuples(&mut steps);
        WhyMaybe { steps, leaf }
    }
}
