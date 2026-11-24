//! Fulfillment (solve.md (e)): a set of obligations driven to a fixpoint, the role of the
//! reference's `FulfillmentCtxt` (rustc_trait_selection solve/fulfill.rs, not vendored) over
//! [`Solver::evaluate`]. An obligation left ambiguous is *stalled* on the inference variables
//! its goal mentions (the reference's `GoalStalledOn`); it is evaluated again only once one of
//! them is bound.
//!
//! Two strategies, one seam (rule 78), tested equal: [`Wake`] keeps a wake list per variable
//! and reads the arena's binding log from a cursor, so a pass costs what changed; [`Rescan`]
//! evaluates every pending obligation on every pass, the reference's loop.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{Alloc, Vec};

use crate::assembly::Goal;
use crate::error::{SolveError, Stop};
use crate::eval::{Answer, Solver};
use crate::facts::{Facts, Read};
use crate::term::{Certainty, Pred, Ty, TyKind, TypingMode};
use crate::tys::Tys;

/// An obligation's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    Holds,
    Fails,
    /// Ambiguous with overflow: not retried on bindings (`Certainty::Overflow*`).
    Overflow,
}

/// How a pass picks the obligations it evaluates.
pub trait Strategy {
    const WAKE: bool;
}

/// Wake lists.
pub struct Wake;
/// Every pending obligation, every pass.
pub struct Rescan;

impl Strategy for Wake {
    const WAKE: bool = true;
}

impl Strategy for Rescan {
    const WAKE: bool = false;
}

const NONE: u32 = u32::MAX;

/// The most passes `select` makes (rule 18): each pass that does not end the loop binds a
/// variable, so a world never comes near it.
pub const MAX_PASSES: u32 = 1 << 20;

/// Obligations in columns.
pub struct Fulfill<'a, S: Strategy> {
    alloc: Alloc<'a>,
    pub goal: Vec<Goal, Alloc<'a>>,
    pub state: Vec<State, Alloc<'a>>,
    /// Bumped each time the obligation is evaluated: a wake entry of an older generation is
    /// stale.
    stamp: Vec<u32, Alloc<'a>>,
    ready: Vec<u32, Alloc<'a>>,
    batch: Vec<u32, Alloc<'a>>,
    queued: Vec<bool, Alloc<'a>>,
    /// Variable index to the first wake entry; entries chain by `wait_next`.
    wait_head: Map<u32, u32, FixedState, Alloc<'a>>,
    wait_ob: Vec<(u32, u32), Alloc<'a>>,
    wait_next: Vec<u32, Alloc<'a>>,
    cursor: u32,
    work: Vec<(Ty, u32), Alloc<'a>>,
    vars: Vec<u32, Alloc<'a>>,
    /// Goal evaluations made (for the cost example).
    pub evaluations: u64,
    _s: core::marker::PhantomData<S>,
}

impl<'a, S: Strategy> Fulfill<'a, S> {
    pub fn new_in(alloc: Alloc<'a>) -> Fulfill<'a, S> {
        Fulfill {
            alloc,
            goal: Vec::new_in(alloc),
            state: Vec::new_in(alloc),
            stamp: Vec::new_in(alloc),
            ready: Vec::new_in(alloc),
            batch: Vec::new_in(alloc),
            queued: Vec::new_in(alloc),
            wait_head: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            wait_ob: Vec::new_in(alloc),
            wait_next: Vec::new_in(alloc),
            cursor: 0,
            work: Vec::new_in(alloc),
            vars: Vec::new_in(alloc),
            evaluations: 0,
            _s: core::marker::PhantomData,
        }
    }

    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Adds an obligation; it is evaluated on the next `select`.
    pub fn register(&mut self, g: Goal) -> Result<u32, SolveError> {
        let id = crate::error::row(self.goal.len())?;
        self.goal.push(g);
        self.state.push(State::Pending);
        self.stamp.push(0);
        self.queued.push(true);
        self.ready.push(id);
        Ok(id)
    }

    pub fn pending(&self) -> usize {
        self.state.iter().filter(|&&s| s == State::Pending).count()
    }

    /// The unresolved variables of `g`, into `self.vars`.
    fn stalled_on<T: Tys>(&mut self, tys: &T, g: Goal) -> Result<(), SolveError> {
        self.vars.clear();
        let each = |t: Ty, vars: &mut Vec<u32, Alloc<'a>>, work: &mut Vec<(Ty, u32), Alloc<'a>>| {
            fold_vars(tys, work, t, vars)
        };
        match g.pred {
            Pred::Trait { args, .. } | Pred::HostEffect { args, .. } | Pred::ConstHolds { args, .. } => {
                for i in 0..args.len {
                    each(tys.list_get(args, i), &mut self.vars, &mut self.work)?;
                }
            }
            Pred::ForFields { container, args_after_self, .. } => {
                each(container, &mut self.vars, &mut self.work)?;
                for i in 0..args_after_self.len {
                    each(tys.list_get(args_after_self, i), &mut self.vars, &mut self.work)?;
                }
            }
            Pred::Projection { alias: a, term: b }
            | Pred::NormalizesTo { alias: a, term: b }
            | Pred::TypeOutlives { ty: a, region: b }
            | Pred::RegionOutlives { a, b }
            | Pred::Subtype { a, b }
            | Pred::Coerce { a, b }
            | Pred::ConstArgHasType { ct: a, ty: b }
            | Pred::Eq { a, b } => {
                each(a, &mut self.vars, &mut self.work)?;
                each(b, &mut self.vars, &mut self.work)?;
            }
            Pred::WellFormed(t) | Pred::ConstEvaluatable(t) => each(t, &mut self.vars, &mut self.work)?,
            Pred::DynCompatible(_) => {}
        }
        Ok(())
    }

    fn wait(&mut self, ob: u32) -> Result<(), SolveError> {
        let stamp = self.stamp[ob as usize];
        for k in 0..self.vars.len() {
            let v = self.vars[k];
            let e = crate::error::row(self.wait_ob.len())?;
            let prev = self.wait_head.insert(v, e).unwrap_or(NONE);
            self.wait_ob.push((ob, stamp));
            self.wait_next.push(prev);
        }
        Ok(())
    }

    /// Queues the obligations waiting on variables bound since the cursor.
    fn wake<T: Tys>(&mut self, tys: &T) {
        let n = tys.bindings_len();
        if n < self.cursor {
            // The log was rolled back under the cursor: every pending obligation may have
            // changed.
            for i in 0..self.state.len() {
                if self.state[i] == State::Pending && !self.queued[i] {
                    self.queued[i] = true;
                    self.ready.push(i as u32);
                }
            }
            self.cursor = n;
            return;
        }
        for i in self.cursor..n {
            let v = tys.binding(i);
            let Some(mut e) = self.wait_head.remove(&v) else { continue };
            while e != NONE {
                let (ob, stamp) = self.wait_ob[e as usize];
                if self.stamp[ob as usize] == stamp && self.state[ob as usize] == State::Pending && !self.queued[ob as usize] {
                    self.queued[ob as usize] = true;
                    self.ready.push(ob);
                }
                e = self.wait_next[e as usize];
            }
        }
        self.cursor = n;
    }

    /// Evaluates obligations until no pass binds a variable (`select_where_possible`). Returns
    /// how many hold now in total.
    pub fn select<T: Tys, F: Facts>(
        &mut self,
        solver: &mut Solver<'a>,
        tys: &mut T,
        facts: &F,
        mode: TypingMode,
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<(), Stop> {
        self.select_from(solver, tys, &mut crate::facts::Stopping(facts), mode, reads)
    }

    /// [`Fulfill::select`] over a source that fills a missing fact where a goal meets it
    /// (`Solver::evaluate_from`): no pass stops for a fact, so no obligation is registered
    /// again for one.
    pub fn select_from<T: Tys, Src: crate::facts::Source<T>>(
        &mut self,
        solver: &mut Solver<'a>,
        tys: &mut T,
        src: &mut Src,
        mode: TypingMode,
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<(), Stop> {
        for _ in 0..MAX_PASSES {
            if S::WAKE {
                self.wake(&*tys);
            } else {
                self.ready.clear();
                for i in 0..self.state.len() {
                    if self.state[i] == State::Pending {
                        self.ready.push(i as u32);
                    }
                }
            }
            if self.ready.is_empty() {
                return Ok(());
            }
            let before = tys.bindings_len();
            self.batch.clear();
            core::mem::swap(&mut self.batch, &mut self.ready);
            for i in 0..self.batch.len() {
                let ob = self.batch[i];
                self.queued[ob as usize] = false;
                let g = self.goal[ob as usize];
                self.evaluations += 1;
                self.stamp[ob as usize] += 1;
                let a = solver.evaluate_from(tys, src, mode, g, reads)?;
                self.state[ob as usize] = match a {
                    Answer::Yes => State::Holds,
                    Answer::No => State::Fails,
                    Answer::Maybe(c) if c == Certainty::Overflow || c == Certainty::OverflowLimit => State::Overflow,
                    Answer::Maybe(_) => {
                        if S::WAKE {
                            self.stalled_on(&*tys, g)?;
                            self.wait(ob)?;
                        }
                        State::Pending
                    }
                };
            }
            self.batch.clear();
            if !S::WAKE && tys.bindings_len() == before {
                return Ok(());
            }
        }
        Err(Stop::Error(SolveError::Limit(crate::error::Limit::Rounds)))
    }
}

fn fold_vars<T: Tys>(tys: &T, work: &mut Vec<(Ty, u32), Alloc<'_>>, t: Ty, out: &mut Vec<u32, Alloc<'_>>) -> Result<(), SolveError> {
    crate::fold::visit(tys, work, t, |_, k| {
        if let TyKind::Infer(v) = k {
            if !out.contains(&v.index) {
                out.push(v.index);
            }
            return false;
        }
        true
    })
}
