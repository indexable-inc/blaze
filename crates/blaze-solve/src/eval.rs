//! The engine (solve.md (a), (b)): goals evaluated on one explicit stack of frames, the
//! reference's `EvalCtxt` (rustc_next_trait_solver solve/eval_ctxt/mod.rs) and search graph
//! (rustc_type_ir search_graph/mod.rs) as one loop with no recursion on a goal's depth.
//!
//! A frame proves one canonical goal. It instantiates its input with fresh variables
//! (`instantiate_canonical`), enters a binder with placeholders (`enter_forall`), runs its goal
//! kind's assembly (staged: goals it adds are evaluated at the frame's level), then each
//! candidate in a probe (a snapshot rolled back after): the rule's stages, the nested goals
//! (`try_evaluate_added_goals`, at most `FIXPOINT_STEP_LIMIT` rounds), the response
//! (`evaluate_added_goals_and_make_canonical_response`). Trait goals assemble their impls and
//! objects after the first candidates are evaluated. The goal kind's merge picks the answer;
//! the search graph decides whether it is final (a cycle head reruns until its result is a
//! fixpoint), cached globally, or provisional on the heads it depends on.
//!
//! Evaluating a nested goal canonicalizes it and looks it up: on the stack (a cycle: the head's
//! provisional result, or the initial one for the path's kind), in the provisional cache, in
//! the global cache; else a frame is pushed. A fact the world does not have yet stops the whole
//! evaluation (`Stop::Need`): every snapshot is rolled back and the caller retries the root.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{Alloc, Vec};

use crate::assembly::{AStep, Added, Cand, Clause, Cx, Done, EnvId, Goal, Langs, Merger, Rule as CandRule, Source, Step, Via};
use crate::canon::{self, Canon, Decode, RespGoal, RespId, Responses};
use crate::error::{Limit, Malformed, SolveError, Stop};
use crate::facts::{Ask, Facts, Read, TraitFlags};
use crate::fold::{self, Inst, Scratch};
use crate::search::{Ans, FIXPOINT_STEP_LIMIT, Heads, Inputs, MAX_DEPTH, PathKind, Prov};
use crate::term::{Certainty, Def, Flags, GoalSource, Lang, MarkerTrait, Polarity, Pred, Ty, TyKind, TypingMode, VarKind};
use crate::trait_goals;
use crate::tys::Tys;

/// A root goal's answer, its response applied to the caller's variables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    No,
    Yes,
    Maybe(Certainty),
}

/// The rules of goal kinds with one way to prove them (solve/mod.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// `compute_type_outlives_goal`, `compute_region_outlives_goal`: regions are erased, so
    /// every outlives goal holds (solve.md, "Not ported").
    Outlives,
    /// `compute_subtype_goal`, `compute_coerce_goal`: two unresolved type variables are
    /// ambiguous; else the sides are related (subtyping is equality without regions).
    Relate,
    /// `compute_const_evaluatable_goal`: an unresolved const is ambiguous; a value holds.
    ConstEvaluatable,
    /// `compute_dyn_compatible_goal`.
    DynCompatible,
    /// `compute_const_arg_has_type_goal`: const arguments are well typed by construction here.
    ConstArgHasType,
    /// `compute_host_effect_goal`, as its trait goal (const traits are not ported).
    HostEffect,
    /// `where const { COND }` (`Pred::ConstHolds`): see [`const_holds`].
    ConstHolds,
    /// Two terms equal (an equation goal of the tests and of coherence).
    Eq,
}

/// The recursion limit (the reference's default `recursion_limit` of 128).
pub const RECURSION_LIMIT: u32 = 128;

/// The most clauses an elaborated param env holds (rule 18).
pub const MAX_ENV: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    Assemble(u8),
    NextCand,
    Step(u8),
    Nested,
    Merge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
    Assemble(u8),
    Step(u8),
    Make(Certainty),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Trait,
    Alias,
    WellFormed,
    Other,
}

fn kind_of(p: Pred) -> Kind {
    match p {
        Pred::Trait { .. } => Kind::Trait,
        Pred::Projection { .. } | Pred::NormalizesTo { .. } => Kind::Alias,
        Pred::WellFormed(_) => Kind::WellFormed,
        _ => Kind::Other,
    }
}

/// A nested goal waiting in a frame or a probe.
#[derive(Clone, Copy, Debug)]
struct Pending {
    pred: Pred,
    bound: u32,
    source: GoalSource,
    via_only: bool,
    done: bool,
}

#[derive(Clone, Copy, Debug)]
struct Nested {
    lo: u32,
    i: u32,
    round: u32,
    changed: bool,
    cert: Certainty,
}

/// A stack entry: one goal being proven (`StackEntry` of the search graph with its
/// `EvalCtxt`).
#[derive(Clone, Copy, Debug)]
struct Frame {
    input: u32,
    mode: TypingMode,
    goal: Goal,
    kind: Kind,
    vars_lo: u32,
    vars: u32,
    /// The path kind of a step from this goal to a nested one (`step_kind_for_source`).
    to_nested: PathKind,
    from_parent: PathKind,
    phase: Phase,
    after: After,
    nested: Nested,
    tstate: trait_goals::State,
    impls_done: bool,
    cands_lo: u32,
    cand_at: u32,
    cand: Cand,
    dones_lo: u32,
    goals_lo: u32,
    fgoals_hi: u32,
    probe_lo: u32,
    via: Via,
    /// The goal being evaluated by the frame above, and where its original values are.
    call_orig_lo: u32,
    call_via_only: bool,
    // The search graph.
    depth: u32,
    required_depth: u32,
    heads: Heads,
    used: u8,
    provisional: Option<Ans>,
    iterations: u32,
    overflow: bool,
    big: bool,
    reads_lo: u32,
    nested_lo: u32,
    scratch_mark: (u32, u32),
}

enum Got {
    Now(Ans),
    Pushed,
}

/// The solver: its tables live as long as the allocator, across goals (the global cache, the
/// interned inputs and envs). A solver serves one term arena.
pub struct Solver<'a> {
    alloc: Alloc<'a>,
    pub(crate) inputs: Inputs<'a>,
    pub(crate) resps: Responses<'a>,
    env_clauses: Vec<Clause, Alloc<'a>>,
    env_rows: Vec<(u32, u32), Alloc<'a>>,
    env_map: Map<u64, u32, FixedState, Alloc<'a>>,
    env_next: Vec<u32, Alloc<'a>>,
    env_words: Vec<u32, Alloc<'a>>,
    env_word_rows: Vec<(u32, u32), Alloc<'a>>,
    canon: Canon<'a>,
    decode: Decode<'a>,
    scratch: Scratch<'a>,
    words: Vec<u32, Alloc<'a>>,
    tmp: Vec<u32, Alloc<'a>>,
    frames: Vec<Frame, Alloc<'a>>,
    cands: Vec<Cand, Alloc<'a>>,
    dones: Vec<Done, Alloc<'a>>,
    goals: Vec<Pending, Alloc<'a>>,
    origs: Vec<Ty, Alloc<'a>>,
    reads: Vec<Read, Alloc<'a>>,
    nested_ids: Vec<u32, Alloc<'a>>,
    added: Vec<Added, Alloc<'a>>,
    deferred: Vec<(Ty, Ty), Alloc<'a>>,
    resp_goals: Vec<RespGoal, Alloc<'a>>,
    langs: Langs,
    universe: u32,
    limit: u32,
    /// The ground fast path's reads, by (term, marker trait) (solve.md (c)).
    marker_reads: Map<(u32, u8), (u32, u32), FixedState, Alloc<'a>>,
    marker_pool: Vec<Read, Alloc<'a>>,
    /// Counters for the cost example and the tests.
    pub stats: Stats,
}

/// What the solver did, counted.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub frames: u64,
    pub cache_hits: u64,
    pub fast_path: u64,
    pub candidates: u64,
    pub reruns: u64,
    /// Impl candidates assembled, after the fingerprint prefilter (prefilter.rs): each is one
    /// impl header imported with fresh arguments (`trait_goals::step_impl`).
    pub impl_candidates: u64,
}

fn env_slice<'e>(clauses: &'e [Clause], rows: &[(u32, u32)], env: EnvId) -> &'e [Clause] {
    match rows.get(env.0 as usize) {
        Some(&(s, l)) => &clauses[s as usize..(s + l) as usize],
        None => &[],
    }
}

/// The certainty of an overflow (`Certainty::overflow(false)`).
const OVERFLOW: Certainty = Certainty::Overflow;

#[cfg(test)]
fn coinductive_as_inductive() -> bool {
    crate::tests::plant::on(crate::tests::plant::Plant::CoinductiveAsInductive)
}

#[cfg(not(test))]
fn coinductive_as_inductive() -> bool {
    false
}

impl<'a> Solver<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Solver<'a> {
        Solver {
            alloc,
            inputs: Inputs::new_in(alloc),
            resps: Responses::new_in(alloc),
            env_clauses: Vec::new_in(alloc),
            env_rows: Vec::new_in(alloc),
            env_map: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            env_next: Vec::new_in(alloc),
            env_words: Vec::new_in(alloc),
            env_word_rows: Vec::new_in(alloc),
            canon: Canon::new_in(alloc),
            decode: Decode::new_in(alloc),
            scratch: Scratch::new_in(alloc),
            words: Vec::new_in(alloc),
            tmp: Vec::new_in(alloc),
            frames: Vec::new_in(alloc),
            cands: Vec::new_in(alloc),
            dones: Vec::new_in(alloc),
            goals: Vec::new_in(alloc),
            origs: Vec::new_in(alloc),
            reads: Vec::new_in(alloc),
            nested_ids: Vec::new_in(alloc),
            added: Vec::new_in(alloc),
            deferred: Vec::new_in(alloc),
            resp_goals: Vec::new_in(alloc),
            langs: Langs::new(),
            universe: 0,
            limit: RECURSION_LIMIT,
            marker_reads: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            marker_pool: Vec::new_in(alloc),
            stats: Stats::default(),
        }
    }

    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Sets the recursion limit (clamped to the stack's bound, `MAX_DEPTH`).
    pub fn set_recursion_limit(&mut self, n: u32) {
        self.limit = n.clamp(1, MAX_DEPTH);
    }

    /// The clauses of `env`.
    pub fn env_clauses(&self, env: EnvId) -> &[Clause] {
        env_slice(&self.env_clauses, &self.env_rows, env)
    }

    /// Interns a param env: `clauses` and their supertraits, elaborated transitively
    /// (rustc_type_ir elaborate.rs, as the reference's param envs are), each once.
    pub fn env<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        facts: &F,
        clauses: &[Clause],
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<EnvId, Stop> {
        let ask = Ask::new(facts);
        let mut all: Vec<Clause, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut work: Vec<(Ty, u32), Alloc<'a>> = Vec::new_in(self.alloc);
        for &c in clauses {
            let mut infer = false;
            let mut chk = |t: Ty| -> Result<(), SolveError> {
                fold::visit(&*tys, &mut work, t, |_, k| {
                    if matches!(k, TyKind::Infer(_)) {
                        infer = true;
                    }
                    true
                })
            };
            pred_terms(tys, c.pred, &mut chk)?;
            if infer {
                // A param env is the item's written clauses: an inference variable in one is the
                // caller's bug. The refusal names that clause as its goal.
                let goal = Some(bad_pred_of(&*tys, c.pred));
                return Err(Stop::Error(SolveError::Malformed(crate::error::Malformed::Pred { at: core::panic::Location::caller(), goal, asked_by: None })));
            }
            if !all.contains(&c) {
                all.push(c);
            }
        }
        let mut i = 0;
        while i < all.len() {
            let c = all[i];
            i += 1;
            let Pred::Trait { trait_, args, polarity: Polarity::Positive } = c.pred else { continue };
            let def = ask.trait_def(reads, trait_)?;
            let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(args.len as usize, self.alloc);
            for k in 0..args.len {
                v.push(tys.list_get(args, k));
            }
            for &sp in def.supertraits {
                let p = import_pred(tys, &mut self.scratch, def.portable, sp, &v, self.alloc)?;
                let nc = Clause { pred: p, bound: c.bound };
                if !all.contains(&nc) {
                    if all.len() >= MAX_ENV {
                        return Err(Stop::Error(SolveError::Limit(Limit::Table)));
                    }
                    all.push(nc);
                }
            }
        }
        // The key: the clauses' canonical words (rule 21), in the given order.
        self.words.clear();
        for c in all.iter() {
            self.words.push(c.bound);
            self.canon.clear();
            self.canon.pred(&*tys, &mut self.words, c.pred)?;
        }
        let h = crate::search::hash_words(&self.words);
        let mut at = self.env_map.get(&h).copied().unwrap_or(u32::MAX);
        while at != u32::MAX {
            let (s, l) = self.env_word_rows[at as usize];
            if self.env_words[s as usize..(s + l) as usize] == self.words[..] {
                return Ok(EnvId(at));
            }
            at = self.env_next[at as usize];
        }
        let id = crate::error::row(self.env_rows.len())?;
        let s = crate::error::row(self.env_clauses.len())?;
        self.env_clauses.extend_from_slice(&all);
        self.env_rows.push((s, all.len() as u32));
        let ws = crate::error::row(self.env_words.len())?;
        self.env_words.extend_from_slice(&self.words);
        self.env_word_rows.push((ws, self.words.len() as u32));
        let prev = self.env_map.insert(h, id).unwrap_or(u32::MAX);
        self.env_next.push(prev);
        Ok(EnvId(id))
    }

    /// Evaluates `goal` and applies its response to the caller's variables
    /// (`evaluate_root_goal`). Every fact read is appended to `reads`.
    pub fn evaluate<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        facts: &F,
        mode: TypingMode,
        goal: Goal,
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<Answer, Stop> {
        if let Some(a) = self.fast_path(tys, mode, goal, reads) {
            self.stats.fast_path += 1;
            return Ok(a);
        }
        let reads_lo = self.reads.len();
        let r = self.evaluate_root(tys, facts, mode, goal);
        match r {
            Ok((ans, orig_lo)) => {
                let answer = self.apply_root(tys, ans, orig_lo);
                self.origs.truncate(orig_lo as usize);
                let answer = answer?;
                self.remember_marker(tys, mode, goal, answer, reads_lo);
                reads.extend_from_slice(&self.reads[reads_lo..]);
                self.reads.truncate(reads_lo);
                self.resps.truncate_scratch((0, 0));
                Ok(answer)
            }
            Err(e) => {
                self.reads.truncate(reads_lo);
                self.origs.clear();
                self.resps.truncate_scratch((0, 0));
                Err(e)
            }
        }
    }

    /// The marker trait a goal asks about, if the fast path may answer it.
    fn marker_of<T: Tys>(&self, tys: &T, mode: TypingMode, goal: Goal) -> Option<(Ty, MarkerTrait)> {
        if mode != TypingMode::Analysis || goal.bound != 0 {
            return None;
        }
        let Pred::Trait { trait_, args, polarity: Polarity::Positive } = goal.pred else { return None };
        if args.len != 1 {
            return None;
        }
        let lang = |l: Lang| self.langs.state[l as usize] == Some(Some(trait_));
        let m = if lang(Lang::Sized) {
            MarkerTrait::Sized
        } else if lang(Lang::Copy) {
            MarkerTrait::Copy
        } else if lang(Lang::Send) {
            MarkerTrait::Send
        } else if lang(Lang::Sync) {
            MarkerTrait::Sync
        } else {
            return None;
        };
        let t = tys.shallow_resolve(tys.list_get(args, 0));
        if !tys.flags(t).is_global_ground() {
            return None;
        }
        let env = self.env_clauses(goal.env);
        if env.iter().any(|c| matches!(c.pred, Pred::Trait { trait_: d, .. } if d == trait_)) {
            return None;
        }
        Some((t, m))
    }

    /// The ground fast path (solve.md (c)): a global ground type's answer for a marker trait,
    /// kept in its `markers` byte, with the reads it was valid by.
    fn fast_path<T: Tys>(&mut self, tys: &T, mode: TypingMode, goal: Goal, reads: &mut Vec<Read, Alloc<'a>>) -> Option<Answer> {
        let (t, m) = self.marker_of(tys, mode, goal)?;
        let holds = tys.markers(t).get(m)?;
        let &(s, l) = self.marker_reads.get(&(t.0, m as u8))?;
        reads.extend_from_slice(&self.marker_pool[s as usize..(s + l) as usize]);
        Some(if holds { Answer::Yes } else { Answer::No })
    }

    fn remember_marker<T: Tys>(&mut self, tys: &mut T, mode: TypingMode, goal: Goal, answer: Answer, reads_lo: usize) {
        let holds = match answer {
            Answer::Yes => true,
            Answer::No => false,
            Answer::Maybe(_) => return,
        };
        let Some((t, m)) = self.marker_of(tys, mode, goal) else { return };
        let s = self.marker_pool.len() as u32;
        let rs = &self.reads[reads_lo..];
        self.marker_pool.extend_from_slice(rs);
        self.marker_reads.insert((t.0, m as u8), (s, rs.len() as u32));
        let k = tys.markers(t).with(m, holds);
        tys.set_markers(t, k);
    }

    fn apply_root<T: Tys>(&mut self, tys: &mut T, ans: Ans, orig_lo: u32) -> Result<Answer, Stop> {
        let Some(r) = ans else { return Ok(Answer::No) };
        self.resp_goals.clear();
        let ok = self.resps.apply(tys, &mut self.decode, &mut self.deferred, r, &self.origs[orig_lo as usize..], &mut self.resp_goals)?;
        if !ok {
            return Err(Stop::Error(SolveError::Malformed(Malformed::Canonical { at: 0 })));
        }
        let c = self.resps.certainty(r);
        Ok(if c == Certainty::Yes && self.resp_goals.is_empty() {
            Answer::Yes
        } else if c == Certainty::Yes {
            Answer::Maybe(Certainty::Ambiguous)
        } else {
            Answer::Maybe(c)
        })
    }

    /// Runs the root goal to its answer; the root's original values are left on `origs` from
    /// the returned index.
    fn evaluate_root<T: Tys, F: Facts>(&mut self, tys: &mut T, facts: &F, mode: TypingMode, goal: Goal) -> Result<(Ans, u32), Stop> {
        canon::goal_input(&*tys, &mut self.canon, &mut self.words, mode, goal)?;
        let orig_lo = self.origs.len() as u32;
        self.origs.extend_from_slice(&self.canon.orig);
        let input = self.inputs.intern(&self.words)?;
        if let Some(e) = self.inputs.lookup(input, self.limit, true) {
            self.stats.cache_hits += 1;
            let rs = self.inputs.reads_of(&e);
            self.reads.extend_from_slice(rs);
            return Ok((e.ans, orig_lo));
        }
        let mut snaps: Vec<T::Snapshot, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut probes: Vec<Option<T::Snapshot>, Alloc<'a>> = Vec::new_in(self.alloc);
        self.push_frame(tys, &mut snaps, &mut probes, input, mode, self.limit, PathKind::Inductive);
        let r = self.drive(tys, facts, &mut snaps, &mut probes);
        match r {
            Ok(ans) => Ok((ans, orig_lo)),
            Err(e) => {
                // Unwind: the root's base snapshot undoes every frame's and probe's bindings.
                if let Some(&s) = snaps.first() {
                    tys.rollback_to(s);
                }
                for f in self.frames.iter() {
                    self.inputs.on_stack[f.input as usize] = u32::MAX;
                }
                self.frames.clear();
                self.cands.clear();
                self.dones.clear();
                self.goals.clear();
                self.nested_ids.clear();
                self.added.clear();
                self.deferred.clear();
                self.inputs.prov.clear();
                Err(e)
            }
        }
    }

    fn push_frame<T: Tys>(
        &mut self,
        tys: &mut T,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
        input: u32,
        mode: TypingMode,
        depth: u32,
        from_parent: PathKind,
    ) {
        self.stats.frames += 1;
        let at = self.frames.len() as u32;
        self.inputs.on_stack[input as usize] = at;
        snaps.push(tys.snapshot());
        probes.push(None);
        let goals_lo = self.goals.len() as u32;
        self.frames.push(Frame {
            input,
            mode,
            goal: Goal { env: EnvId(0), pred: Pred::DynCompatible(Def(0)), bound: 0 },
            kind: Kind::Other,
            vars_lo: self.origs.len() as u32,
            vars: 0,
            to_nested: PathKind::ForcedAmbiguity,
            from_parent,
            phase: Phase::Start,
            after: After::Make(Certainty::Yes),
            nested: Nested { lo: goals_lo, i: goals_lo, round: 0, changed: false, cert: Certainty::Yes },
            tstate: trait_goals::State::NEW,
            impls_done: false,
            cands_lo: self.cands.len() as u32,
            cand_at: self.cands.len() as u32,
            cand: Cand::new(Source::Builtin(crate::assembly::BuiltinSource::Misc), CandRule::Certain(Certainty::Yes)),
            dones_lo: self.dones.len() as u32,
            goals_lo,
            fgoals_hi: goals_lo,
            probe_lo: goals_lo,
            via: Via::None,
            call_orig_lo: 0,
            call_via_only: false,
            depth,
            required_depth: 0,
            heads: Heads::default(),
            used: 0,
            provisional: None,
            iterations: 0,
            overflow: false,
            big: false,
            reads_lo: self.reads.len() as u32,
            nested_lo: self.nested_ids.len() as u32,
            scratch_mark: self.resps.scratch_mark(),
        });
    }

    /// The main loop: runs the top frame until the root frame finishes.
    fn drive<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        facts: &F,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
    ) -> Result<Ans, Stop> {
        loop {
            let top = self.frames.len() - 1;
            match self.drive_one(tys, facts, snaps, probes, top) {
                Ok(Some(done)) => return Ok(done),
                Ok(None) => {}
                Err(Stop::Error(SolveError::Malformed(Malformed::Pred { at, goal: None, .. }))) => {
                    // The goal being solved names the refusal (the frame may have finished and
                    // been popped by `finish`: then no goal is known here), and the goal that
                    // nested it names who asked.
                    let goal = self.frames.get(top).map(|f| bad_pred_of(tys, f.goal.pred));
                    let asked_by = top.checked_sub(1).and_then(|p| self.frames.get(p)).map(|f| bad_pred_of(tys, f.goal.pred));
                    return Err(Stop::Error(SolveError::Malformed(Malformed::Pred { at, goal, asked_by })));
                }
                Err(Stop::Error(SolveError::Malformed(Malformed::Sort { term, met, at, goal: None }))) => {
                    let goal = self.frames.get(top).map(|f| bad_pred_of(tys, f.goal.pred));
                    return Err(Stop::Error(SolveError::Malformed(Malformed::Sort { term, met, at, goal })));
                }
                Err(Stop::Error(SolveError::Malformed(Malformed::Param { index, args, at, goal: None }))) => {
                    let goal = self.frames.get(top).map(|f| bad_pred_of(tys, f.goal.pred));
                    return Err(Stop::Error(SolveError::Malformed(Malformed::Param { index, args, at, goal })));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One phase of the top frame: the answer when the root finished.
    fn drive_one<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        facts: &F,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
        top: usize,
    ) -> Result<Option<Ans>, Stop> {
        let phase = self.frames[top].phase;
        match phase {
            Phase::Start => self.start(tys, facts, top)?,
            Phase::Assemble(k) => self.assemble(tys, facts, top, k)?,
            Phase::NextCand => self.next_cand(tys, facts, probes, top)?,
            Phase::Step(k) => self.step(tys, facts, probes, top, k)?,
            Phase::Nested => {
                self.nested(tys, facts, snaps, probes, top)?;
            }
            Phase::Merge => {
                let ans = self.merge(facts, top)?;
                return self.finish(tys, snaps, probes, top, ans);
            }
        }
        Ok(None)
    }

    fn cx<'c, T: Tys, F: Facts>(&'c mut self, tys: &'c mut T, facts: &'c F, top: usize) -> Cx<'c, 'a, T, F> {
        let f = self.frames[top];
        Cx {
            tys,
            ask: Ask::new(facts),
            reads: &mut self.reads,
            added: &mut self.added,
            deferred: &mut self.deferred,
            scratch: &mut self.scratch,
            langs: &mut self.langs,
            universe: &mut self.universe,
            via: f.via,
            goal: f.goal,
            env: env_slice(&self.env_clauses, &self.env_rows, f.goal.env),
            mode: f.mode,
            alloc: self.alloc,
        }
    }

    /// Instantiates the frame's input (`instantiate_canonical`), enters its binder
    /// (`enter_forall`), and computes the path kind of its nested steps.
    fn start<T: Tys, F: Facts>(&mut self, tys: &mut T, facts: &F, top: usize) -> Result<(), Stop> {
        let input = self.frames[top].input;
        let (mode, goal) = {
            let w = self.inputs.words_of(input);
            canon::instantiate_input(tys, &mut self.decode, w)?
        };
        let vars_lo = self.origs.len() as u32;
        for &s in self.decode.slots.iter() {
            if s == Ty::NONE {
                return Err(Stop::Error(SolveError::Malformed(Malformed::Canonical { at: 0 })));
            }
        }
        self.origs.extend_from_slice(&self.decode.slots);
        let vars = self.decode.slots.len() as u32;
        {
            let f = &mut self.frames[top];
            f.mode = mode;
            f.goal = goal;
            f.vars_lo = vars_lo;
            f.vars = vars;
            f.kind = kind_of(goal.pred);
        }
        let mut cx = self.cx(tys, facts, top);
        let goal = if goal.bound > 0 {
            let with = cx.placeholders(goal.bound)?;
            let pred = cx.map_pred(goal.pred, |cx, t| cx.instantiate_bound(t, with))?;
            Goal { env: goal.env, pred, bound: 0 }
        } else {
            goal
        };
        // `step_kind_for_source` through `CurrentGoalKind`: a coinductive trait's nested goals
        // are coinductive steps, a projection's unknown, any other goal's forced ambiguity.
        let to_nested = match goal.pred {
            Pred::Trait { trait_, .. } => {
                let d = cx.trait_def(trait_)?;
                if d.flags.has(TraitFlags::AUTO) || d.flags.has(TraitFlags::COINDUCTIVE) {
                    PathKind::Coinductive
                } else {
                    PathKind::ForcedAmbiguity
                }
            }
            Pred::Projection { .. } | Pred::NormalizesTo { .. } => PathKind::Unknown,
            _ => PathKind::ForcedAmbiguity,
        };
        let f = &mut self.frames[top];
        f.goal = goal;
        f.to_nested = to_nested;
        f.phase = Phase::Assemble(0);
        Ok(())
    }

    /// Moves the goals a rule added into the pending list.
    fn take_added(&mut self) {
        for a in self.added.iter() {
            self.goals.push(Pending { pred: a.pred, bound: a.bound, source: a.source, via_only: a.via_only, done: false });
        }
        self.added.clear();
    }

    fn assemble<T: Tys, F: Facts>(&mut self, tys: &mut T, facts: &F, top: usize, stage: u8) -> Result<(), Stop> {
        let f = self.frames[top];
        // `out` holds the candidates of earlier stages: a stage may keep a row there for a
        // later one (alias.rs's stash) and remove it.
        let mut out: Vec<Cand, Alloc<'a>> = Vec::new_in(self.alloc);
        out.extend_from_slice(&self.cands[f.cands_lo as usize..]);
        self.cands.truncate(f.cands_lo as usize);
        let mut st = f.tstate;
        let r = {
            let mut cx = self.cx(tys, facts, top);
            match f.kind {
                Kind::Trait => trait_goals::assemble(&mut cx, stage, &mut st, &mut out)?,
                Kind::Alias => crate::alias::assemble(&mut cx, stage, &mut out)?,
                Kind::WellFormed => {
                    let Pred::WellFormed(t) = f.goal.pred else { return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred()))) };
                    crate::structural::assemble_wf(&mut cx, t, &mut out)?;
                    AStep::Done
                }
                Kind::Other => {
                    let rule = match f.goal.pred {
                        Pred::TypeOutlives { .. } | Pred::RegionOutlives { .. } => Rule::Outlives,
                        Pred::Subtype { .. } | Pred::Coerce { .. } => Rule::Relate,
                        Pred::ConstEvaluatable(_) => Rule::ConstEvaluatable,
                        Pred::DynCompatible(_) => Rule::DynCompatible,
                        Pred::ConstArgHasType { .. } => Rule::ConstArgHasType,
                        Pred::HostEffect { .. } => Rule::HostEffect,
                        Pred::ConstHolds { .. } => Rule::ConstHolds,
                        _ => Rule::Eq,
                    };
                    out.push(Cand::new(Source::Builtin(crate::assembly::BuiltinSource::Misc), CandRule::Other(rule)));
                    AStep::Done
                }
            }
        };
        self.cands.extend_from_slice(&out);
        self.take_added();
        let fr = &mut self.frames[top];
        fr.tstate = st;
        match r {
            AStep::Done => {
                fr.fgoals_hi = self.goals.len() as u32;
                fr.cand_at = fr.cands_lo;
                fr.phase = Phase::NextCand;
            }
            AStep::Then(k) => {
                let lo = fr.goals_lo;
                Self::begin_nested(fr, lo, After::Assemble(k));
            }
            AStep::No => {
                self.cands.truncate(fr.cands_lo as usize);
                fr.phase = Phase::Merge;
            }
        }
        Ok(())
    }

    fn begin_nested(f: &mut Frame, lo: u32, after: After) {
        f.nested = Nested { lo, i: lo, round: 0, changed: false, cert: Certainty::Yes };
        f.after = after;
        f.phase = Phase::Nested;
    }

    fn next_cand<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        facts: &F,
        probes: &mut [Option<T::Snapshot>],
        top: usize,
    ) -> Result<(), Stop> {
        let f = self.frames[top];
        if (f.cand_at as usize) < self.cands.len() {
            self.stats.candidates += 1;
            probes[top] = Some(tys.snapshot());
            let lo = self.goals.len() as u32;
            for i in f.goals_lo..f.fgoals_hi {
                let g = self.goals[i as usize];
                if !g.done {
                    self.goals.push(g);
                }
            }
            let fr = &mut self.frames[top];
            fr.probe_lo = lo;
            fr.via = Via::None;
            fr.cand = self.cands[f.cand_at as usize];
            fr.phase = Phase::Step(0);
            return Ok(());
        }
        if f.kind == Kind::Trait && !f.impls_done {
            self.frames[top].impls_done = true;
            let done = &self.dones[f.dones_lo as usize..];
            if trait_goals::wants_impls(f.mode, &f.tstate, &self.resps, done) {
                let mut out: Vec<Cand, Alloc<'a>> = Vec::new_in(self.alloc);
                let st = f.tstate;
                {
                    let mut cx = self.cx(tys, facts, top);
                    trait_goals::assemble_impls(&mut cx, &st, &mut out)?;
                }
                self.stats.impl_candidates += out.iter().filter(|c| matches!(c.source, Source::Impl(_))).count() as u64;
                self.added.clear();
                self.cands.extend_from_slice(&out);
                return Ok(());
            }
        }
        self.frames[top].phase = Phase::Merge;
        Ok(())
    }

    fn step<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        facts: &F,
        probes: &mut [Option<T::Snapshot>],
        top: usize,
        stage: u8,
    ) -> Result<(), Stop> {
        let mut cand = self.frames[top].cand;
        let r = {
            let mut cx = self.cx(tys, facts, top);
            match cand.rule {
                CandRule::Certain(c) => Step::Done(c),
                CandRule::Trait(r) => trait_goals::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Builtin(r) => crate::builtin::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Structural(r) => crate::structural::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Alias(r) => crate::alias::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Coherence(r) => crate::coherence::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Other(r) => step_other(&mut cx, r)?,
            }
        };
        self.take_added();
        let fr = &mut self.frames[top];
        fr.cand = cand;
        let lo = fr.probe_lo;
        match r {
            Step::No => self.end_probe(tys, probes, top, None),
            Step::Then(k) => Self::begin_nested(fr, lo, After::Step(k)),
            Step::Done(c) => Self::begin_nested(fr, lo, After::Make(c)),
        }
        Ok(())
    }

    fn end_probe<T: Tys>(&mut self, tys: &mut T, probes: &mut [Option<T::Snapshot>], top: usize, resp: Option<RespId>) {
        if let Some(s) = probes[top].take() {
            tys.rollback_to(s);
        }
        let fr = &mut self.frames[top];
        self.goals.truncate(fr.fgoals_hi as usize);
        if let Some(r) = resp {
            self.dones.push(Done { cand: fr.cand, resp: r });
        }
        fr.cand_at += 1;
        fr.phase = Phase::NextCand;
    }

    /// One step of the nested-goal loop (`evaluate_added_goals_step`); `true` when a frame was
    /// pushed or finished (the loop continues with the new top).
    fn nested<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        facts: &F,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
        top: usize,
    ) -> Result<bool, Stop> {
        let _ = facts;
        loop {
            let n = self.frames[top].nested;
            if (n.i as usize) < self.goals.len() {
                let g = self.goals[n.i as usize];
                if g.done {
                    self.frames[top].nested.i += 1;
                    continue;
                }
                match self.start_goal(tys, snaps, probes, top, g)? {
                    Got::Pushed => return Ok(true),
                    Got::Now(ans) => {
                        if !self.receive(tys, top, ans)? {
                            self.nested_failed(tys, probes, top);
                            return Ok(false);
                        }
                    }
                }
                continue;
            }
            // The round is over: drop the goals that hold.
            let mut w = n.lo as usize;
            for r in n.lo as usize..self.goals.len() {
                if !self.goals[r].done {
                    self.goals[w] = self.goals[r];
                    w += 1;
                }
            }
            self.goals.truncate(w);
            let c = if w == n.lo as usize {
                Some(Certainty::Yes)
            } else if !n.changed {
                Some(n.cert)
            } else if n.round + 1 >= FIXPOINT_STEP_LIMIT {
                Some(OVERFLOW)
            } else {
                None
            };
            match c {
                Some(c) => {
                    self.nested_done(tys, probes, top, c)?;
                    return Ok(false);
                }
                None => {
                    let fr = &mut self.frames[top];
                    fr.nested = Nested { lo: n.lo, i: n.lo, round: n.round + 1, changed: false, cert: Certainty::Yes };
                }
            }
        }
    }

    /// A nested goal has no solution.
    fn nested_failed<T: Tys>(&mut self, tys: &mut T, probes: &mut [Option<T::Snapshot>], top: usize) {
        let f = self.frames[top];
        match f.after {
            After::Assemble(_) => {
                self.cands.truncate(f.cands_lo as usize);
                self.goals.truncate(f.goals_lo as usize);
                self.frames[top].fgoals_hi = f.goals_lo;
                self.frames[top].phase = Phase::Merge;
            }
            After::Step(_) | After::Make(_) => self.end_probe(tys, probes, top, None),
        }
    }

    /// The nested goals settled with certainty `c`.
    fn nested_done<T: Tys>(&mut self, tys: &mut T, probes: &mut [Option<T::Snapshot>], top: usize, c: Certainty) -> Result<(), Stop> {
        let f = self.frames[top];
        match f.after {
            After::Assemble(k) => {
                self.frames[top].fgoals_hi = self.goals.len() as u32;
                self.frames[top].phase = Phase::Assemble(k);
            }
            After::Step(k) => self.frames[top].phase = Phase::Step(k),
            After::Make(shallow) => {
                let r = self.make_response(tys, top, shallow, c)?;
                self.end_probe(tys, probes, top, Some(r));
            }
        }
        Ok(())
    }

    /// `evaluate_added_goals_and_make_canonical_response`: a `NormalizesTo` goal proven with
    /// shallow certainty `Yes` hands its pending goals back in the response.
    fn make_response<T: Tys>(&mut self, tys: &mut T, top: usize, shallow: Certainty, goals_c: Certainty) -> Result<RespId, Stop> {
        let f = self.frames[top];
        self.resp_goals.clear();
        let normalizes = matches!(f.goal.pred, Pred::NormalizesTo { .. });
        let certainty = if normalizes && shallow == Certainty::Yes {
            for g in self.goals[f.probe_lo as usize..].iter() {
                if !g.done && !g.via_only {
                    self.resp_goals.push(RespGoal { source: g.source, bound: g.bound, pred: g.pred });
                }
            }
            Certainty::Yes
        } else {
            shallow.and(goals_c)
        };
        if certainty.is_overflow() {
            self.resp_goals.clear();
            return Ok(self.resps.no_constraints(f.vars, certainty, Via::None)?);
        }
        let vals = &self.origs[f.vars_lo as usize..(f.vars_lo + f.vars) as usize];
        Ok(self.resps.make(&*tys, &mut self.canon, &mut self.tmp, vals, &self.resp_goals, certainty, Via::None)?)
    }

    /// Takes a nested goal's answer: applies its response (`instantiate_and_apply_query_response`)
    /// and keeps the goal pending unless it holds. `false`: no solution.
    fn receive<T: Tys>(&mut self, tys: &mut T, top: usize, ans: Ans) -> Result<bool, Stop> {
        let f = self.frames[top];
        let i = f.nested.i as usize;
        let orig_lo = f.call_orig_lo as usize;
        let Some(r) = ans else {
            self.origs.truncate(orig_lo);
            // A goal evaluated for how it is proven fails its probe of the trait goal, not ours.
            if self.goals[i].via_only {
                self.goals[i].done = true;
                self.frames[top].via = Via::Failed;
                self.frames[top].nested.i += 1;
                return Ok(true);
            }
            return Ok(false);
        };
        if self.goals[i].via_only {
            self.frames[top].via = self.resps.via(r);
            self.goals[i].done = true;
            self.origs.truncate(orig_lo);
            self.frames[top].nested.i += 1;
            return Ok(true);
        }
        self.resp_goals.clear();
        let ok = self.resps.apply(tys, &mut self.decode, &mut self.deferred, r, &self.origs[orig_lo..], &mut self.resp_goals)?;
        self.origs.truncate(orig_lo);
        if !ok {
            return Err(Stop::Error(SolveError::Malformed(Malformed::Canonical { at: 0 })));
        }
        let identity = self.resps.is_identity(r);
        let c = self.resps.certainty(r);
        for g in self.resp_goals.iter() {
            self.goals.push(Pending { pred: g.pred, bound: g.bound, source: g.source, via_only: false, done: false });
        }
        let fr = &mut self.frames[top];
        if !identity {
            fr.nested.changed = true;
        }
        if c == Certainty::Yes {
            self.goals[i].done = true;
        } else {
            fr.nested.cert = fr.nested.cert.and(c);
        }
        fr.nested.i += 1;
        Ok(true)
    }

    /// `initial_provisional_result`.
    fn initial(&mut self, vars: u32, path: PathKind) -> Result<Ans, SolveError> {
        let path = if path == PathKind::Coinductive && coinductive_as_inductive() { PathKind::Inductive } else { path };
        Ok(match path {
            PathKind::Coinductive => Some(self.resps.no_constraints(vars, Certainty::Yes, Via::None)?),
            PathKind::Unknown | PathKind::ForcedAmbiguity => Some(self.resps.no_constraints(vars, OVERFLOW, Via::None)?),
            // Unreachable in the reference (no step is inductive); the planted bug's answer.
            PathKind::Inductive => None,
        })
    }

    /// The path kind from the stack entry `head` to a new goal under the top.
    fn path_from(&self, head: usize, step: PathKind) -> PathKind {
        let mut p = step;
        for f in self.frames[head + 1..].iter() {
            p = p.extend(f.from_parent);
        }
        p
    }

    /// Starts evaluating the pending goal `g` of the top frame: an answer now (a cycle, a
    /// cache hit, overflow), or a pushed frame.
    fn start_goal<T: Tys>(
        &mut self,
        tys: &mut T,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
        top: usize,
        g: Pending,
    ) -> Result<Got, Stop> {
        let f = self.frames[top];
        let goal = Goal { env: f.goal.env, pred: g.pred, bound: g.bound };
        canon::goal_input(&*tys, &mut self.canon, &mut self.words, f.mode, goal)?;
        let orig_lo = self.origs.len() as u32;
        self.origs.extend_from_slice(&self.canon.orig);
        self.frames[top].call_orig_lo = orig_lo;
        self.frames[top].call_via_only = g.via_only;
        let vars = self.canon.orig.len() as u32;
        let input = self.inputs.intern(&self.words)?;
        let step = f.to_nested;
        // A cycle (`lookup_cycle` / `cycle_on_stack`).
        let h = self.inputs.on_stack[input as usize];
        if h != u32::MAX {
            let h = h as usize;
            let path = self.path_from(h, step);
            self.frames[h].used |= path.bit();
            self.frames[top].heads.insert(h as u32);
            let prov = self.frames[h].provisional;
            let ans = match prov {
                Some(a) => a,
                None => self.initial(vars, path)?,
            };
            return Ok(Got::Now(ans));
        }
        // The provisional cache.
        let mut hit = None;
        for p in self.inputs.prov.iter() {
            if p.input == input && (p.highest as usize) < self.frames.len() && self.path_from(p.highest as usize, step) == p.path {
                hit = Some(*p);
                break;
            }
        }
        if let Some(p) = hit {
            self.frames[top].heads.union(&p.heads);
            self.frames[p.highest as usize].used |= p.path.bit();
            return Ok(Got::Now(p.ans));
        }
        // The global cache.
        let avail = f.depth.saturating_sub(1);
        if let Some(e) = self.inputs.lookup(input, avail, false) {
            self.stats.cache_hits += 1;
            let rs = self.inputs.reads_of(&e);
            self.reads.extend_from_slice(rs);
            let big = match self.inputs.nested_of(&e) {
                Some(n) => {
                    let n: &[u32] = n;
                    self.nested_ids.extend_from_slice(n);
                    false
                }
                None => true,
            };
            self.nested_ids.push(input);
            let fr = &mut self.frames[top];
            fr.big |= big;
            fr.required_depth = fr.required_depth.max(e.required_depth + 1);
            return Ok(Got::Now(e.ans));
        }
        // Overflow (`stack_overflow_result`).
        if avail == 0 {
            self.frames[top].overflow = true;
            return Ok(Got::Now(Some(self.resps.no_constraints(vars, Certainty::OverflowLimit, Via::None)?)));
        }
        self.push_frame(tys, snaps, probes, input, f.mode, avail, step);
        Ok(Got::Pushed)
    }

    fn merge<F: Facts>(&mut self, facts: &F, top: usize) -> Result<Ans, Stop> {
        let f = self.frames[top];
        let mut dones: Vec<Done, Alloc<'a>> = Vec::new_in(self.alloc);
        dones.extend_from_slice(&self.dones[f.dones_lo as usize..]);
        let mut m = Merger { resps: &mut self.resps, vars: f.vars };
        // Specialization between two impl candidates, a fact of the impls read like any other
        // (the frame's read list keeps it).
        let ask = Ask::new(facts);
        let reads = &mut self.reads;
        let mut specializes = |a: Def, b: Def| ask.specializes(reads, a, b);
        Ok(match f.kind {
            Kind::Trait => match trait_goals::merge(&mut m, f.mode, &f.tstate, &mut dones, &mut specializes)? {
                Some((r, via)) => Some(m.with_via(r, via)?),
                None => None,
            },
            Kind::Alias => crate::alias::merge(&mut m, f.mode, f.goal.pred, &mut dones, &mut specializes)?,
            Kind::WellFormed | Kind::Other => m.merge_responses(&dones)?,
        })
    }

    fn ans_eq(&self, a: Ans, b: Ans) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => self.resps.equal(x, y),
            _ => false,
        }
    }

    /// The search graph's pop (`SearchGraph::with_new_goal`'s tail): a cycle head reruns until
    /// its result is a fixpoint; the result is cached globally, or provisionally while it
    /// depends on heads still on the stack. `Some`: the root finished.
    fn finish<T: Tys>(
        &mut self,
        tys: &mut T,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
        top: usize,
        ans: Ans,
    ) -> Result<Option<Ans>, Stop> {
        let mut ans = match ans {
            Some(r) => Some(self.resps.keep(r)?),
            None => None,
        };
        let f = self.frames[top];
        if f.used != 0 {
            let mut reached = true;
            for k in PathKind::ALL {
                if f.used & k.bit() == 0 {
                    continue;
                }
                let prev = match f.provisional {
                    Some(p) => p,
                    None => {
                        let i = self.initial(f.vars, k)?;
                        match i {
                            Some(r) => Some(self.resps.keep(r)?),
                            None => None,
                        }
                    }
                };
                if !self.ans_eq(prev, ans) {
                    reached = false;
                }
            }
            if !reached {
                if f.iterations + 1 < FIXPOINT_STEP_LIMIT {
                    self.stats.reruns += 1;
                    self.inputs.drop_provisional(top as u32);
                    self.rerun(tys, snaps, probes, top, ans);
                    return Ok(None);
                }
                // `fixpoint_overflow_result`.
                let r = self.resps_no_constraints(f.vars, OVERFLOW)?;
                ans = Some(self.resps.keep(r)?);
            }
        }
        self.inputs.drop_provisional(top as u32);
        // Pop.
        if let Some(s) = probes[top].take() {
            tys.rollback_to(s);
        }
        let base = snaps[top];
        tys.rollback_to(base);
        snaps.pop();
        probes.pop();
        let f = self.frames[top];
        self.frames.pop();
        self.inputs.on_stack[f.input as usize] = u32::MAX;
        self.cands.truncate(f.cands_lo as usize);
        self.dones.truncate(f.dones_lo as usize);
        self.goals.truncate(f.goals_lo as usize);
        self.origs.truncate(f.vars_lo as usize);
        self.resps.truncate_scratch(f.scratch_mark);
        let rl = f.reads_lo as usize;
        sort_dedup(&mut self.reads, rl);
        let nl = f.nested_lo as usize;
        sort_dedup(&mut self.nested_ids, nl);
        let mut heads = f.heads;
        heads.remove(top as u32);
        if heads.is_empty() {
            let (inputs, nested, reads) = (&mut self.inputs, &self.nested_ids[nl..], &self.reads[rl..]);
            inputs.insert(f.input, f.depth, f.overflow, ans, f.required_depth, nested, f.big, reads)?;
        } else if let Some(highest) = heads.highest() {
            let path = {
                let mut p = f.from_parent;
                for g in self.frames[highest as usize + 1..].iter() {
                    p = p.extend(g.from_parent);
                }
                p
            };
            self.inputs.prov.push(Prov { input: f.input, heads, highest, path, ans });
        }
        if top == 0 {
            return Ok(Some(ans));
        }
        let p = top - 1;
        self.nested_ids.push(f.input);
        {
            let pf = &mut self.frames[p];
            pf.required_depth = pf.required_depth.max(f.required_depth + 1);
            pf.overflow |= f.overflow;
            pf.big |= f.big;
            pf.heads.union(&heads);
        }
        if !self.receive(tys, p, ans)? {
            self.nested_failed(tys, probes, p);
        }
        Ok(None)
    }

    fn resps_no_constraints(&mut self, vars: u32, c: Certainty) -> Result<RespId, SolveError> {
        self.resps.no_constraints(vars, c, Via::None)
    }

    /// Reruns a cycle head with its last result as the provisional one.
    fn rerun<T: Tys>(
        &mut self,
        tys: &mut T,
        snaps: &mut [T::Snapshot],
        probes: &mut [Option<T::Snapshot>],
        top: usize,
        ans: Ans,
    ) {
        if let Some(s) = probes[top].take() {
            tys.rollback_to(s);
        }
        tys.rollback_to(snaps[top]);
        snaps[top] = tys.snapshot();
        let f = self.frames[top];
        self.cands.truncate(f.cands_lo as usize);
        self.dones.truncate(f.dones_lo as usize);
        self.goals.truncate(f.goals_lo as usize);
        self.origs.truncate(f.vars_lo as usize);
        self.reads.truncate(f.reads_lo as usize);
        self.nested_ids.truncate(f.nested_lo as usize);
        let fr = &mut self.frames[top];
        fr.provisional = Some(ans);
        fr.iterations += 1;
        fr.used = 0;
        fr.heads = Heads::default();
        fr.required_depth = 0;
        fr.overflow = false;
        fr.big = false;
        fr.impls_done = false;
        fr.tstate = trait_goals::State::NEW;
        fr.fgoals_hi = fr.goals_lo;
        fr.phase = Phase::Start;
    }
}

fn sort_dedup<X: Ord + Copy>(v: &mut Vec<X, Alloc<'_>>, lo: usize) {
    v[lo..].sort_unstable();
    let mut w = lo;
    for r in lo..v.len() {
        if w == lo || v[w - 1] != v[r] {
            v[w] = v[r];
            w += 1;
        }
    }
    v.truncate(w);
}

/// Calls `f` on every term of `p`.
fn pred_terms<T: Tys>(tys: &T, p: Pred, f: &mut impl FnMut(Ty) -> Result<(), SolveError>) -> Result<(), SolveError> {
    let mut list = |l: crate::term::List, f: &mut dyn FnMut(Ty) -> Result<(), SolveError>| -> Result<(), SolveError> {
        for i in 0..l.len {
            f(tys.list_get(l, i))?;
        }
        Ok(())
    };
    match p {
        Pred::Trait { args, .. } | Pred::HostEffect { args, .. } | Pred::ConstHolds { args, .. } => list(args, f),
        Pred::Projection { alias: a, term: b }
        | Pred::NormalizesTo { alias: a, term: b }
        | Pred::TypeOutlives { ty: a, region: b }
        | Pred::RegionOutlives { a, b }
        | Pred::Subtype { a, b }
        | Pred::Coerce { a, b }
        | Pred::ConstArgHasType { ct: a, ty: b }
        | Pred::Eq { a, b } => {
            f(a)?;
            f(b)
        }
        Pred::WellFormed(t) | Pred::ConstEvaluatable(t) => f(t),
        Pred::DynCompatible(_) => Ok(()),
    }
}

/// A portable predicate of a trait's definition, `Param(i)` replaced by `args[i]`. A parameter
/// past `args` is `Malformed::Param` naming the caller's line.
#[track_caller]
fn import_pred<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    p: crate::facts::Portable<'_>,
    pred: Pred,
    args: &[Ty],
    alloc: Alloc<'_>,
) -> Result<Pred, SolveError> {
    let at = core::panic::Location::caller();
    let inst = |tys: &mut T, s: &mut Scratch<'_>, t: Ty| fold::import_at(tys, s, p, t, Inst { args, bound: &[] }, at);
    let list = |tys: &mut T, s: &mut Scratch<'_>, l: crate::term::List| -> Result<crate::term::List, SolveError> {
        let mut v: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(l.len as usize, alloc);
        for i in 0..l.len {
            let Some(&x) = p.lists.get((l.start + i) as usize) else {
                return Err(SolveError::Malformed(Malformed::PortableIndex { node: l.start + i, len: p.lists.len() as u32 }));
            };
            v.push(fold::import_at(tys, s, p, x, Inst { args, bound: &[] }, at)?);
        }
        tys.mk_list(&v)
    };
    Ok(match pred {
        Pred::Trait { trait_, args: a, polarity } => Pred::Trait { trait_, args: list(tys, s, a)?, polarity },
        Pred::HostEffect { trait_, args: a, constness } => Pred::HostEffect { trait_, args: list(tys, s, a)?, constness },
        Pred::ConstHolds { cond, args: a } => Pred::ConstHolds { cond, args: list(tys, s, a)? },
        Pred::Projection { alias, term } => Pred::Projection { alias: inst(tys, s, alias)?, term: inst(tys, s, term)? },
        Pred::TypeOutlives { ty, region } => Pred::TypeOutlives { ty: inst(tys, s, ty)?, region: inst(tys, s, region)? },
        Pred::RegionOutlives { a, b } => Pred::RegionOutlives { a: inst(tys, s, a)?, b: inst(tys, s, b)? },
        Pred::WellFormed(t) => Pred::WellFormed(inst(tys, s, t)?),
        Pred::ConstEvaluatable(t) => Pred::ConstEvaluatable(inst(tys, s, t)?),
        Pred::DynCompatible(d) => Pred::DynCompatible(d),
        _ => return Err(SolveError::Malformed(crate::error::bad_pred())),
    })
}

/// The steps of [`Rule`].
fn step_other<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, rule: Rule) -> Result<Step, Stop> {
    let bad = || Stop::Error(SolveError::Malformed(crate::error::bad_pred()));
    let is_var = |k: TyKind| matches!(k, TyKind::Infer(v) if v.kind == VarKind::Ty);
    Ok(match rule {
        Rule::Outlives | Rule::ConstArgHasType => Step::Done(Certainty::Yes),
        Rule::Relate => {
            let (a, b) = match cx.goal.pred {
                Pred::Subtype { a, b } | Pred::Coerce { a, b } => (a, b),
                _ => return Err(bad()),
            };
            if is_var(cx.kind(a)) && is_var(cx.kind(b)) {
                Step::Done(Certainty::Ambiguous)
            } else if cx.eq(a, b)? {
                Step::Done(Certainty::Yes)
            } else {
                Step::No
            }
        }
        Rule::Eq => {
            let Pred::Eq { a, b } = cx.goal.pred else { return Err(bad()) };
            if cx.eq(a, b)? { Step::Done(Certainty::Yes) } else { Step::No }
        }
        Rule::ConstEvaluatable => {
            let Pred::ConstEvaluatable(t) = cx.goal.pred else { return Err(bad()) };
            match cx.kind(t) {
                TyKind::Infer(_) => Step::Done(Certainty::Ambiguous),
                // An unevaluated constant: const evaluation is not ported.
                TyKind::Alias(..) => Step::Done(Certainty::Ambiguous),
                _ => Step::Done(Certainty::Yes),
            }
        }
        Rule::DynCompatible => {
            let Pred::DynCompatible(d) = cx.goal.pred else { return Err(bad()) };
            if cx.trait_def(d)?.flags.has(TraitFlags::DYN_COMPATIBLE) { Step::Done(Certainty::Yes) } else { Step::No }
        }
        Rule::ConstHolds => {
            let Pred::ConstHolds { cond, args } = cx.goal.pred else { return Err(bad()) };
            const_holds(cx, cond, args)?
        }
        Rule::HostEffect => {
            let Pred::HostEffect { trait_, args, .. } = cx.goal.pred else { return Err(bad()) };
            cx.add(GoalSource::Misc, Pred::Trait { trait_, args, polarity: Polarity::Positive });
            Step::Done(Certainty::Yes)
        }
    })
}

/// `where const { COND }` at `args` (extensions.md, `where const`). The clause is a nested goal
/// of its impl, so `false` fails that impl's goal; it never sends selection elsewhere.
/// - Coherence: ambiguous, so a condition never proves two impls disjoint.
/// - An argument still an inference variable (or a non-rigid alias): ambiguous, a stall.
/// - An argument over a generic parameter, a placeholder or an opaque type: undecided for a
///   generic type, so the clause fails (never true, never stalled forever).
/// - Every argument ground: the world's answer; `Missing` is this goal's `Need`.
fn const_holds<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cond: Def, args: crate::term::List) -> Result<Step, Stop> {
    if cx.mode == TypingMode::Coherence {
        return Ok(Step::Done(Certainty::Ambiguous));
    }
    let mut v = cx.list(args);
    let mut flags = 0u16;
    for t in v.iter_mut() {
        *t = cx.resolve_deep(*t)?;
        flags |= cx.tys.flags(*t).0;
    }
    let f = Flags(flags);
    if f.has(Flags::HAS_INFER | Flags::HAS_NON_RIGID_ALIAS) {
        return Ok(Step::Done(Certainty::Ambiguous));
    }
    // A type error is already a finding; the clause adds none of its own.
    if f.has(Flags::HAS_ERROR) {
        return Ok(Step::Done(Certainty::Yes));
    }
    if f.has(Flags::HAS_PARAM | Flags::HAS_PLACEHOLDER | Flags::HAS_BOUND | Flags::HAS_ALIAS | Flags::HAS_OPAQUE) {
        // The item's own clause, assumed in its param env, meets the same condition at the
        // same (hash-consed) arguments; nothing else decides it for a generic type.
        let assumed = cx.env.iter().any(|c| match c.pred {
            Pred::ConstHolds { cond: d, args: a } if d == cond && a.len == args.len => {
                (0..a.len).all(|i| cx.tys.list_get(a, i) == v[i as usize])
            }
            _ => false,
        });
        return Ok(if assumed { Step::Done(Certainty::Yes) } else { Step::No });
    }
    let list = cx.mk_list(&v)?;
    Ok(match cx.const_holds(cond, list, &v)? {
        crate::facts::ConstHolds::True => Step::Done(Certainty::Yes),
        crate::facts::ConstHolds::False | crate::facts::ConstHolds::Error => Step::No,
    })
}

#[allow(dead_code)]
fn unused(_: Flags) {}

/// The sort and shape of one argument of a refused goal.
fn arg_shape<T: Tys>(tys: &T, t: Ty) -> crate::error::ArgShape {
    use crate::error::ArgShape;
    let k = tys.kind(tys.shallow_resolve(t));
    match k {
        TyKind::ReStatic | TyKind::ReParam(_) | TyKind::ReErased => ArgShape::Region,
        TyKind::Infer(v) if v.kind == VarKind::Region => ArgShape::Region,
        TyKind::Infer(v) if v.kind == VarKind::Const => ArgShape::Const,
        TyKind::ConstValue(..) | TyKind::ConstParam(_) => ArgShape::Const,
        TyKind::Bound(..) | TyKind::Canonical(_) => ArgShape::Bound,
        TyKind::Alias(_, _, d, _) => ArgShape::Alias(d),
        _ => match crate::term::shape_of_kind(k) {
            Some(s) => ArgShape::Ty(s),
            None => ArgShape::Var,
        },
    }
}

/// A goal's predicate summarised for `Malformed::Pred`: kind, item, first argument shapes.
fn bad_pred_of<T: Tys>(tys: &T, p: Pred) -> crate::error::BadPred {
    use crate::error::{BadPred, PredKind};
    let mut out = BadPred { kind: PredKind::Other, def: Def(0), args: [None; 4], n_args: 0 };
    let mut two = |out: &mut BadPred, kind: PredKind, a: Ty, b: Ty| {
        out.kind = kind;
        out.args[0] = Some(arg_shape(tys, a));
        out.args[1] = Some(arg_shape(tys, b));
        out.n_args = 2;
    };
    match p {
        Pred::Trait { trait_, args, .. } | Pred::HostEffect { trait_, args, .. } | Pred::ConstHolds { cond: trait_, args } => {
            out.kind = match p {
                Pred::Trait { .. } => PredKind::Trait,
                Pred::HostEffect { .. } => PredKind::HostEffect,
                _ => PredKind::ConstHolds,
            };
            out.def = trait_;
            out.n_args = args.len;
            for i in 0..args.len.min(4) {
                out.args[i as usize] = Some(arg_shape(tys, tys.list_get(args, i)));
            }
        }
        Pred::Projection { alias, term } => {
            two(&mut out, PredKind::Projection, alias, term);
            if let TyKind::Alias(_, _, d, _) = tys.kind(alias) {
                out.def = d;
            }
        }
        Pred::NormalizesTo { alias, term } => {
            two(&mut out, PredKind::NormalizesTo, alias, term);
            if let TyKind::Alias(_, _, d, _) = tys.kind(alias) {
                out.def = d;
            }
        }
        Pred::TypeOutlives { ty, region } => two(&mut out, PredKind::TypeOutlives, ty, region),
        Pred::RegionOutlives { a, b } => two(&mut out, PredKind::RegionOutlives, a, b),
        Pred::Subtype { a, b } => two(&mut out, PredKind::Subtype, a, b),
        Pred::Coerce { a, b } => two(&mut out, PredKind::Coerce, a, b),
        Pred::ConstArgHasType { ct, ty } => two(&mut out, PredKind::ConstArgHasType, ct, ty),
        Pred::Eq { a, b } => two(&mut out, PredKind::Other, a, b),
        Pred::WellFormed(t) | Pred::ConstEvaluatable(t) => {
            out.kind = if matches!(p, Pred::WellFormed(_)) { PredKind::WellFormed } else { PredKind::ConstEvaluatable };
            out.args[0] = Some(arg_shape(tys, t));
            out.n_args = 1;
        }
        Pred::DynCompatible(d) => {
            out.kind = PredKind::DynCompatible;
            out.def = d;
        }
    }
    out
}
