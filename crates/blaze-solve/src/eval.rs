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
//! the global cache; else a frame is pushed. A fact the world does not have yet is pulled: the
//! step that met it is rolled back (its bindings, rows and frame state: [`Solver::pull`]), the
//! [`FactSource`] fills it, and the step runs again in the same frame. Only a source that cannot
//! fill ([`crate::facts::Stopping`]) stops the whole evaluation (`Stop::Need`): then every
//! snapshot is rolled back and the caller fills the fact and asks the root again.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{Alloc, Vec};

use crate::assembly::{AStep, Added, Cand, Clause, Cx, Done, EnvId, Goal, Langs, Merger, Rule as CandRule, Source, Step, Via};
use crate::canon::{self, Canon, Decode, RespGoal, RespId, Responses};
use crate::error::{Limit, Malformed, SolveError, Stop};
use crate::facts::{Ask, Facts, Question, Read, Source as FactSource, Stopping, TraitFlags};
use crate::share::{self, Audit, GoalHit, GoalKey, NewGoal, Taken, Refuse, ShareError, ShareOpts, ShareRefused, SharedAns, SharedGoal};
use crate::fold::{self, Inst, Scratch};
use crate::readlog::{ReadLog, SegLog};
use crate::search::{Ans, FIXPOINT_STEP_LIMIT, Heads, Inputs, MAX_DEPTH, PathKind, Prov};
use crate::term::{Certainty, Def, Flags, GoalSource, Lang, MarkerTrait, Polarity, Pred, Ty, TyKind, TypingMode, VarKind};
use crate::trait_goals;
use crate::tys::Tys;
use crate::witness::{self, Answered, Choice, Derivations, Emit, Fate, NoDerivation, Refusal, Trace, TraceAsk, TracedRoot};
use crate::why_maybe::{Bail, Cause, LimitHit, MaybeLog, Shallow, Verdict, WhyMaybe};
use crate::why_no::{Failed, NoLog, Reach, Reached, WhyNo};

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
    /// `for<F in fields(container)> F: Trait<args...>` matched against a param-env assumption
    /// at index `Cand::n`.
    ForFieldsEnv,
    /// `for<F in fields(container)> F: Trait<args...>` proved structurally over the fields of
    /// `Cand::a` (normalized container).
    ForFieldsStructural,
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
    ForFields,
    Other,
}

fn kind_of(p: Pred) -> Kind {
    match p {
        Pred::Trait { .. } => Kind::Trait,
        Pred::Projection { .. } | Pred::NormalizesTo { .. } => Kind::Alias,
        Pred::WellFormed(_) => Kind::WellFormed,
        Pred::ForFields { .. } => Kind::ForFields,
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
    /// What the goal is of the goal its rule ran for (why_no.rs).
    reach: Reach,
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

/// What a root's look into the request's table gave (`Solver::share_root`).
enum Shared<'a> {
    /// The answer, taken: the root is done.
    Hit(Ans),
    /// A hit the audit solves again (share.rs `ShareOpts::audit`).
    Audit(Audit<'a>),
    Miss,
}

/// The solver: its tables live as long as the allocator, across goals (the global cache, the
/// interned inputs and envs). A solver serves one term arena. `E` is the derivation sink
/// (witness.rs, rule 78): `NoDerivation` by default.
pub struct Solver<'a, E = NoDerivation> {
    /// The choice each framed `Yes` goal's merge made (witness.rs).
    emit: E,
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
    merge_dones: Vec<Done, Alloc<'a>>,
    goals: Vec<Pending, Alloc<'a>>,
    origs: Vec<Ty, Alloc<'a>>,
    /// Every frame's read list, a set per frame (readlog.rs).
    reads: ReadLog<'a>,
    /// Every frame's nested goals (input ids), a set per frame.
    nested_ids: SegLog<'a>,
    added: Vec<Added, Alloc<'a>>,
    deferred: Vec<(Ty, Ty), Alloc<'a>>,
    resp_goals: Vec<RespGoal, Alloc<'a>>,
    langs: Langs,
    universe: u32,
    limit: u32,
    /// The ground fast path's reads, by (term, marker trait) (solve.md (c)).
    marker_reads: Map<(u32, u8), (u32, u32), FixedState, Alloc<'a>>,
    marker_pool: Vec<u32, Alloc<'a>>,
    /// A measurement, off unless asked for ([`Solver::keep_root_keys`]): a 64-bit digest of
    /// each ground root goal's canonical input with its environment's canonical words in place
    /// of the env id, so goals of different bodies (different solvers) compare.
    root_keys: Option<Vec<u64, Alloc<'a>>>,
    /// A measurement, off unless asked for ([`Solver::trace_on`]): every candidate, merge and
    /// root evaluation of the goals of one trait (witness.rs, "the trait-goal trace").
    trace: Option<Trace<'a>>,
    /// The rows of the one [`Solver::why_no`] running, `None` otherwise (why_no.rs): on
    /// demand for one goal reported unmet, never a mode.
    why: NoLog<'a>,
    /// Whether [`Solver::why_no`] or [`Solver::why_maybe`] is running: every goal is framed.
    why_on: bool,
    /// The rows of the one [`Solver::why_maybe`] running (why_maybe.rs), empty otherwise.
    maybe: MaybeLog<'a>,
    /// Whether [`Solver::why_maybe`] is running: every hook of its rows tests this one flag.
    maybe_on: bool,
    /// The plain-goal memo: a goal whose shallow-resolved top-level terms hold no bindable
    /// inference variable, keyed by its resolved handles (mode, env id, bound count, resolved
    /// predicate), to its interned canonical input. Such a goal's canonical words are a function
    /// of those handles alone (`goal_input` reads `tys` only to resolve variables), so a hit
    /// skips the canonical walk. Only goals whose canonical form has no variable are kept; at
    /// most [`MAX_PLAIN`] entries.
    plain: Map<PlainKey, u32, FixedState, Alloc<'a>>,
    /// Whether the root being solved shares its goals (its source hands a table:
    /// `facts::Source::goals`): `finish` then keeps each final goal in `share_new`.
    sharing: bool,
    /// The final goals of the root being solved, put when it ends (share.rs `NewGoal`).
    share_new: Vec<NewGoal, Alloc<'a>>,
    /// The shared goals this solver took, in order (share.rs `GoalHit`): the one read each.
    share_hits: Vec<GoalHit, Alloc<'a>>,
    /// Scratch of the sharing: a key's words, an entry copied out, read ids, reads, a walk.
    share_key: Vec<u32, Alloc<'a>>,
    share_got: Taken<'a>,
    share_ids: Vec<u32, Alloc<'a>>,
    share_reads: Vec<Read, Alloc<'a>>,
    share_work: Vec<(Ty, u32), Alloc<'a>>,
    /// The step count ([`Solver::steps_now`]) when the root being solved started.
    steps_at: u64,
}

/// The most solver steps (frames pushed, cycle heads rerun, candidates run) one root goal may
/// take (rule 18): a count, so a goal stops at the same step whoever asks it, never a time.
/// The depth limit and the fixpoint's rerun limit give their goals an overflow ANSWER (a
/// result like any other, shared as one); this cap is a typed failure naming the root goal
/// (`SolveError::Steps`), for a search wide rather than deep (one typo made rustc's solver run
/// for hours).
pub const MAX_STEPS: u64 = 1 << 22;

/// The plain-goal memo's cap (rule 18): past it goals are canonicalized each time.
pub const MAX_PLAIN: usize = 1 << 16;

/// What the solver did, counted: plain integers, bumped on the paths they name and read by the
/// caller after a body ([`Solver::stats`]). They live in the scratch (`fold::Scratch::stats`),
/// the one table every rule and walk is handed, so a walk deep in a rule counts without a
/// second channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Root goals asked ([`Solver::evaluate`] calls).
    pub evaluations: u64,
    /// Distinct canonical goals the solver has interned (frames pushed for each, cache hits
    /// after): filled by [`Solver::stats`].
    pub distinct_goals: u64,
    /// Goals proven by a frame (each a stack entry that assembled and ran candidates).
    pub frames: u64,
    /// Goals answered from the task cache.
    pub cache_hits: u64,
    pub fast_path: u64,
    pub candidates: u64,
    pub reruns: u64,
    /// Impl candidates assembled, after the fingerprint prefilter (prefilter.rs): each is one
    /// impl header imported with fresh arguments (`trait_goals::step_impl`).
    pub impl_candidates: u64,
    /// Term nodes made in the arena by importing a portable answer (`fold::import_at`).
    pub terms_imported: u64,
    /// ADT fields instantiated for a structural goal (`builtin::adt_fields`).
    pub adt_fields: u64,
    /// Read lists sorted, and the reads those sorts held (none since the read log, readlog.rs:
    /// kept so the fixtures say so).
    pub read_sorts: u64,
    pub reads_sorted: u64,
    /// Read ids a finished frame's list folded into its parent's (`SegLog::close`): the read
    /// log's work per frame, where the sort was.
    pub reads_folded: u64,
    /// Auto-trait summaries made, one per (ADT, trait) (auto_summary.rs), and the fields their
    /// walks read.
    pub auto_summaries: u64,
    pub summary_fields: u64,
    /// Impl headers imported whose arguments then failed to relate with the goal's
    /// (`trait_goals::step_impl`): the imports a deeper reject would have saved.
    pub impl_header_mismatch: u64,
    /// Impl candidates refused by the header pre-match before any import
    /// (prefilter.rs `header_may_match`).
    pub impl_prematch_rejected: u64,
    /// Negative impls refused for a positive goal whose header names the goal's type, counted
    /// only under [`Solver::why_no`] (`trait_goals::step_impl`).
    pub impl_polarity_refused: u64,
    /// Goals written in canonical form (`canon::goal_input`: each root and each nested goal
    /// started), and the words written.
    pub canonicalized: u64,
    pub canon_words: u64,
    /// Goals whose input was found by their plain key, not written in canonical form
    /// ([`Solver::plain_key`]).
    pub canon_skipped: u64,
    /// Canonical inputs decoded into the arena (`canon::instantiate_input`, one per frame).
    pub decoded: u64,
    /// Root goals that reached the search with no inference variable, no parameter and no
    /// placeholder (counted only while root keys are kept: [`Solver::keep_root_keys`]).
    pub ground_roots: u64,
    /// Root evaluations stopped by a fact not yet filled (`Stop::Need`: the caller fills it and
    /// asks the root again), and the frames on the stack each such stop (or any other) threw
    /// away unfinished.
    pub need_stops: u64,
    pub frames_abandoned: u64,
    /// Facts the source filled inside an evaluation ([`Solver::pull`]): each ran one step of
    /// one frame again, where a need stop ran the whole root again.
    pub pulls: u64,
    /// Frames finished with a provisional answer (inside a cycle, not in the global cache).
    pub provisional: u64,
    /// THE SHARED GOALS (share.rs): root goals answered from the request's table; entries this
    /// solver put there; puts of a goal already there; hits the audit solved again.
    pub shared_hits: u64,
    pub shared_puts: u64,
    pub shared_present: u64,
    pub shared_audited: u64,
    /// Goals not shared, in all and by reason (share.rs `Refuse`).
    pub shared_refused: u64,
    pub shared_refused_vars: u64,
    pub shared_refused_local: u64,
    pub shared_refused_const_holds: u64,
    pub shared_refused_overflow: u64,
    pub shared_refused_scope: u64,
    pub shared_refused_full: u64,
}

/// `add` and `since` over every field of [`Stats`], named once: a field left out of the list
/// fails `since`'s struct literal.
macro_rules! stats_fields {
    ($($f:ident),* $(,)?) => {
        impl Stats {
            /// Adds `o` into `self`.
            pub fn add(&mut self, o: &Stats) {
                $(self.$f += o.$f;)*
            }

            /// `self - earlier`: what happened between two readings.
            pub fn since(&self, earlier: &Stats) -> Stats {
                Stats { $($f: self.$f - earlier.$f,)* }
            }
        }
    };
}

stats_fields!(
    evaluations,
    distinct_goals,
    frames,
    cache_hits,
    fast_path,
    candidates,
    reruns,
    impl_candidates,
    terms_imported,
    adt_fields,
    read_sorts,
    reads_sorted,
    reads_folded,
    auto_summaries,
    summary_fields,
    impl_header_mismatch,
    impl_prematch_rejected,
    impl_polarity_refused,
    canonicalized,
    canon_words,
    canon_skipped,
    decoded,
    ground_roots,
    need_stops,
    frames_abandoned,
    pulls,
    provisional,
    shared_hits,
    shared_puts,
    shared_present,
    shared_audited,
    shared_refused,
    shared_refused_vars,
    shared_refused_local,
    shared_refused_const_holds,
    shared_refused_overflow,
    shared_refused_scope,
    shared_refused_full,
);

/// Which of the solver's shortcuts are off. Each answers as the steps it shortcuts do; the
/// tests run with each off and compare (tests/cost.rs, rule 78).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Off {
    /// The auto-trait summaries (auto_summary.rs): every structural step a goal of its own.
    pub summaries: bool,
    /// The impl header pre-match (prefilter.rs `header_may_match`): every surviving header
    /// imported and related.
    pub prematch: bool,
    /// The plain-goal memo ([`Solver::plain_key`]): every goal written in canonical form.
    pub memo: bool,
}

/// The most facts one step may pull before it is refused (rule 18): each fill answers a
/// question the step had not had answered, so a world never comes near it.
pub const MAX_PULLS: u32 = 1 << 16;

/// What one pulled step may change, taken before it runs ([`Solver::pull`]).
struct StepMark<S> {
    snap: S,
    frame: Frame,
    cands: usize,
    dones: usize,
    goals: usize,
    added: usize,
    deferred: usize,
    origs: usize,
    reads: usize,
    reads_lo: u32,
    universe: u32,
    resps: (u32, u32),
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

impl<'a> Solver<'a, NoDerivation> {
    pub fn new_in(alloc: Alloc<'a>) -> Solver<'a> {
        Solver::with_emit_in(alloc, NoDerivation)
    }
}

impl<'a> Solver<'a, Derivations<'a>> {
    /// The choice recorded for `goal` (witness.rs): the candidate the merge chose when the
    /// goal was framed and answered `Yes`. `None`: never framed in this solver (or not `Yes`).
    /// The goal is put in canonical form as the solver does, so a goal with an unconstrained
    /// variable (a `NormalizesTo`'s term) finds the frame the solver pushed for it.
    pub fn choice<T: Tys>(&mut self, tys: &T, mode: TypingMode, goal: Goal) -> Result<Option<Choice>, Stop> {
        let input = self.input_of(tys, mode, goal)?;
        self.canon.clear();
        Ok(self.emit.choice(input))
    }

    pub fn derivations(&self) -> &Derivations<'a> {
        &self.emit
    }
}

/// Key of the plain-goal memo (`Solver::plain`): a goal whose shallow-resolved top-level terms
/// hold no bindable inference variable. Storing shallow-resolved term handles for the common
/// predicate shapes avoids re-canonicalizing nested goals whose arguments are already-bound
/// inference variables or freshly interned argument lists.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PlainKey {
    mode: TypingMode,
    env: EnvId,
    bound: u32,
    pred: PlainPred,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum PlainPred {
    Trait1(Def, Polarity, Ty),
    Trait2(Def, Polarity, Ty, Ty),
    Trait3(Def, Polarity, Ty, Ty, Ty),
    WellFormed(Ty),
    NormalizesTo(Ty, Ty),
    Projection(Ty, Ty),
    Other(Pred),
}

fn plain_key<T: Tys>(tys: &T, mode: TypingMode, goal: Goal) -> Result<Option<PlainKey>, Stop> {
    let infer_flags = if T::REGIONS_BIND { Flags::HAS_INFER | Flags::HAS_RE_INFER } else { Flags::HAS_INFER };
    let resolve_plain = |t: Ty| -> Option<Ty> {
        let r = tys.shallow_resolve(t);
        (!tys.flags(r).has(infer_flags)).then_some(r)
    };
    let pred = match goal.pred {
        Pred::Trait { trait_, args, polarity } if args.len == 1 => {
            let Some(a0) = resolve_plain(tys.list_get(args, 0)) else { return Ok(None) };
            PlainPred::Trait1(trait_, polarity, a0)
        }
        Pred::Trait { trait_, args, polarity } if args.len == 2 => {
            let Some(a0) = resolve_plain(tys.list_get(args, 0)) else { return Ok(None) };
            let Some(a1) = resolve_plain(tys.list_get(args, 1)) else { return Ok(None) };
            PlainPred::Trait2(trait_, polarity, a0, a1)
        }
        Pred::Trait { trait_, args, polarity } if args.len == 3 => {
            let Some(a0) = resolve_plain(tys.list_get(args, 0)) else { return Ok(None) };
            let Some(a1) = resolve_plain(tys.list_get(args, 1)) else { return Ok(None) };
            let Some(a2) = resolve_plain(tys.list_get(args, 2)) else { return Ok(None) };
            PlainPred::Trait3(trait_, polarity, a0, a1, a2)
        }
        Pred::WellFormed(t) => {
            let Some(r) = resolve_plain(t) else { return Ok(None) };
            PlainPred::WellFormed(r)
        }
        Pred::NormalizesTo { alias, term } => {
            let Some(a) = resolve_plain(alias) else { return Ok(None) };
            let Some(t) = resolve_plain(term) else { return Ok(None) };
            PlainPred::NormalizesTo(a, t)
        }
        Pred::Projection { alias, term } => {
            let Some(a) = resolve_plain(alias) else { return Ok(None) };
            let Some(t) = resolve_plain(term) else { return Ok(None) };
            PlainPred::Projection(a, t)
        }
        other => {
            let mut ok = true;
            pred_terms(tys, other, &mut |t| {
                ok &= !tys.flags(t).has(infer_flags);
                Ok(())
            })?;
            if !ok {
                return Ok(None);
            }
            PlainPred::Other(other)
        }
    };
    Ok(Some(PlainKey { mode, env: goal.env, bound: goal.bound, pred }))
}

impl<'a, E: Emit> Solver<'a, E> {
    /// A solver recording into `emit`. A recording solver turns the auto-trait summaries off
    /// (every structural step is then a goal of its own, with its own choice) and skips the
    /// ground fast path and the request's shared goals (each would answer without a frame,
    /// so without a choice); each shortcut answers as the steps it shortcuts do (tests/cost.rs),
    /// so the answers are the same (crates/blaze-cert/src/emit_tests.rs
    /// `answers_equal_with_and_without_derivations`).
    pub fn with_emit_in(alloc: Alloc<'a>, emit: E) -> Solver<'a, E> {
        let mut s = Solver {
            emit,
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
            merge_dones: Vec::new_in(alloc),
            goals: Vec::new_in(alloc),
            origs: Vec::new_in(alloc),
            reads: ReadLog::new_in(alloc),
            nested_ids: SegLog::new_in(alloc),
            added: Vec::new_in(alloc),
            deferred: Vec::new_in(alloc),
            resp_goals: Vec::new_in(alloc),
            langs: Langs::new(),
            universe: 0,
            limit: RECURSION_LIMIT,
            marker_reads: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            marker_pool: Vec::new_in(alloc),
            root_keys: None,
            trace: None,
            why: NoLog::new_in(alloc),
            why_on: false,
            maybe: MaybeLog::new_in(alloc),
            maybe_on: false,
            plain: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            sharing: false,
            share_new: Vec::new_in(alloc),
            share_hits: Vec::new_in(alloc),
            share_key: Vec::new_in(alloc),
            share_got: Taken::new_in(alloc),
            share_ids: Vec::new_in(alloc),
            share_reads: Vec::new_in(alloc),
            share_work: Vec::new_in(alloc),
            steps_at: 0,
        };
        if E::ON {
            s.scratch.off.summaries = true;
        }
        s
    }

    /// The shared goals this solver took so far, in order (share.rs `GoalHit`): the one read
    /// each hit names, for the body's result.
    pub fn goal_hits(&self) -> &[GoalHit] {
        &self.share_hits
    }

    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Turns shortcuts off: the tests' other strategy.
    #[cfg(test)]
    pub(crate) fn turn_off(&mut self, off: Off) {
        self.scratch.off = off;
    }

    /// Keeps a digest of each ground root goal from now on (a measurement: how many goals
    /// of one body another body asks too). Off by default: it hashes every root goal's words.
    pub fn keep_root_keys(&mut self) {
        if self.root_keys.is_none() {
            self.root_keys = Some(Vec::new_in(self.alloc));
        }
    }

    /// The digests kept since [`Solver::keep_root_keys`], one per ground root goal that
    /// reached the search, in order.
    pub fn root_keys(&self) -> &[u64] {
        self.root_keys.as_deref().unwrap_or(&[])
    }

    /// Traces the goals `ask` names from now on (witness.rs, "the trait-goal trace"): every
    /// candidate a frame for one of them runs, with what became of it, each merge, and each
    /// root evaluation. A measurement, off by default; asking again starts a new trace.
    pub fn trace_on(&mut self, ask: TraceAsk) {
        self.trace = Some(Trace::new_in(ask, self.alloc));
    }

    /// The trace kept since [`Solver::trace_on`], ending it; `None` when none was asked.
    pub fn take_trace(&mut self) -> Option<Trace<'a>> {
        self.trace.take()
    }

    /// What this solver did so far, counted ([`Stats`]).
    pub fn stats(&self) -> Stats {
        Stats { distinct_goals: self.inputs.len() as u64, ..self.scratch.stats }
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
        self.env_from(tys, &mut Stopping(facts), clauses, reads)
    }

    /// [`Solver::env`] over a source: a missing fact is filled and the elaboration (terms
    /// interned only, no bindings) runs again, its reads taken back first.
    pub fn env_from<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        clauses: &[Clause],
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<EnvId, Stop> {
        let mark = reads.len();
        for _ in 0..MAX_PULLS {
            match self.env_once(tys, src.facts(), clauses, reads) {
                Err(Stop::Need(q)) if S::PULLS => {
                    reads.truncate(mark);
                    if !src.fill(tys, q) {
                        return Err(Stop::Need(q));
                    }
                    self.scratch.stats.pulls += 1;
                }
                r => return r,
            }
        }
        Err(Stop::Error(SolveError::Limit(Limit::Pulls)))
    }

    fn env_once<T: Tys, F: Facts>(
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
            let v = crate::assembly::SmallTys::from_list(tys, args, self.alloc);
            for &sp in def.supertraits {
                let p = import_pred(tys, &mut self.scratch, def.portable, sp, &v)?;
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
        self.evaluate_from(tys, &mut Stopping(facts), mode, goal, reads)
    }

    /// [`Solver::evaluate`] over a source that may fill a missing fact where it is met
    /// ([`FactSource`]): `Stop::Need` only when the source does not fill it.
    pub fn evaluate_from<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        mode: TypingMode,
        goal: Goal,
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<Answer, Stop> {
        // The trace's root row (witness.rs): one test of `None` when off.
        let traced = match &self.trace {
            Some(t) => t.ask().matches(&*tys, goal.pred),
            None => None,
        };
        let Some(before) = traced else { return self.evaluate_once(tys, src, mode, goal, reads) };
        let at = self.scratch.stats;
        let r = self.evaluate_once(tys, src, mode, goal, reads);
        let now = self.scratch.stats;
        let how = if now.fast_path > at.fast_path {
            Answered::FastPath
        } else if now.frames > at.frames {
            Answered::Framed
        } else if now.shared_hits > at.shared_hits {
            Answered::Shared
        } else {
            Answered::Cached
        };
        let after = match goal.pred {
            Pred::Trait { args, .. } => witness::kinds_of(&*tys, args),
            _ => before,
        };
        let answer = r.as_ref().ok().copied();
        if let Some(t) = &mut self.trace {
            t.root(TracedRoot { before, after, answer, how });
        }
        r
    }

    /// Why `goal` has no solution (why_no.rs): evaluated again, framing every goal under it,
    /// and walked from the goal to the first goal under it that answers No by itself. Asked
    /// only for a goal the caller reports unmet, never as a mode; what the evaluation binds is
    /// rolled back (the caller's variables are as before). Every fact read is appended to
    /// `reads`, as for [`Solver::evaluate_from`]. A goal that holds gives [`Leaf::NotNo`] with
    /// the root alone.
    ///
    /// [`Leaf::NotNo`]: crate::why_no::Leaf::NotNo
    pub fn why_no<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        mode: TypingMode,
        goal: Goal,
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<WhyNo<'a>, Stop> {
        let snap = tys.snapshot();
        let off = self.scratch.off;
        // Every ADT's fields a goal of their own (no summary), so a field is a step.
        self.scratch.off.summaries = true;
        self.scratch.why_no = true;
        self.why.clear();
        self.why_on = true;
        let r = self.evaluate_once(tys, src, mode, goal, reads);
        self.why_on = false;
        self.scratch.why_no = false;
        self.scratch.off = off;
        tys.rollback_to(snap);
        let answer = r?;
        let root = self.input_of(&*tys, mode, goal)?;
        Ok(self.why.walk(root, answer != Answer::No, self.alloc))
    }

    /// Why `goal` is still ambiguous (why_maybe.rs): evaluated again as [`Solver::why_no`]
    /// evaluates (every goal under it framed), and walked from the goal to the first goal under
    /// it that is ambiguous by itself. Asked only for a goal the caller reports undecided,
    /// never as a mode; what the evaluation binds is rolled back. Every fact read is appended
    /// to `reads`. A goal that holds or fails gives [`Leaf::NotMaybe`] with the root alone.
    ///
    /// [`Leaf::NotMaybe`]: crate::why_maybe::Leaf::NotMaybe
    pub fn why_maybe<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        mode: TypingMode,
        goal: Goal,
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<WhyMaybe<'a>, Stop> {
        let snap = tys.snapshot();
        let off = self.scratch.off;
        self.scratch.off.summaries = true;
        self.why.clear();
        self.maybe.clear();
        self.why_on = true;
        self.maybe_on = true;
        let r = self.evaluate_once(tys, src, mode, goal, reads);
        self.why_on = false;
        self.maybe_on = false;
        self.scratch.off = off;
        tys.rollback_to(snap);
        let answer = r?;
        let root = self.input_of(&*tys, mode, goal)?;
        Ok(self.maybe.walk(root, matches!(answer, Answer::Maybe(_)), self.alloc))
    }

    /// `why_maybe`'s row of the frame at `top`, whose merge answered `ans` (`bail`: the
    /// candidates it could not choose between): the verdict, and the frame's candidate causes
    /// popped.
    fn maybe_merged(&mut self, top: usize, ans: Ans, bail: Option<Bail>) {
        let f = self.frames[top];
        let verdict = match ans {
            Some(r) if self.resps.certainty(r) != Certainty::Yes => match bail {
                Some(b) => Verdict::Bail(b),
                None => {
                    // The candidate whose response the merge took: one not `Yes` and equal to
                    // the merged response (`with_via` aside), else the first not `Yes`.
                    let lo = f.dones_lo as usize;
                    let mut taken: Option<u32> = None;
                    let mut first: Option<u32> = None;
                    for (i, d) in self.dones[lo..].iter().enumerate() {
                        if self.resps.certainty(d.resp) == Certainty::Yes {
                            continue;
                        }
                        let at = (lo + i) as u32;
                        first = first.or(Some(at));
                        if taken.is_none() && self.resps.equal(d.resp, r) {
                            taken = Some(at);
                        }
                    }
                    match taken.or(first).and_then(|at| self.maybe.cause_of(at)) {
                        Some((cand, cause)) => Verdict::Cand { cand, cause },
                        None => Verdict::None,
                    }
                }
            },
            _ => Verdict::None,
        };
        self.maybe.pop_cands(f.dones_lo);
        self.maybe.verdict(f.input, verdict);
    }

    /// Why the rule of the candidate of the frame at `top` answered with a shallow certainty
    /// other than `Yes` (why_maybe.rs `Shallow`), read from the frame's goal as the rule saw it:
    /// an opaque type with no hidden type under the auto-trait rule; a projection's alias, or
    /// any other goal's first term, holding an inference variable; a projection otherwise; else
    /// the rule's own.
    fn maybe_shallow<T: Tys>(&mut self, tys: &mut T, top: usize) -> Result<Shallow, Stop> {
        let f = self.frames[top];
        if f.cand.rule == CandRule::Structural(crate::structural::Rule::Auto)
            && let Pred::Trait { args, .. } = f.goal.pred
            && args.len > 0
            && let TyKind::Alias(_, crate::term::AliasKind::Opaque, opaque, _) = tys.kind(tys.shallow_resolve(tys.list_get(args, 0)))
        {
            return Ok(Shallow::Hidden { opaque });
        }
        let mut terms: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        pred_terms(&*tys, f.goal.pred, &mut |t| {
            terms.push(t);
            Ok(())
        })?;
        let alias = match f.goal.pred {
            Pred::Projection { alias, .. } | Pred::NormalizesTo { alias, .. } => match tys.kind(tys.shallow_resolve(alias)) {
                TyKind::Alias(_, _, d, _) => Some(d),
                _ => None,
            },
            _ => None,
        };
        // A projection's term is the variable the goal binds: only its alias is read.
        let read = if alias.is_some() { 1 } else { terms.len() };
        for (i, &t) in terms.iter().take(read).enumerate() {
            let t = fold::resolve(tys, &mut self.scratch, t)?;
            if tys.flags(t).has(Flags::HAS_INFER) {
                return Ok(Shallow::Unresolved { term: i as u32 });
            }
        }
        Ok(match alias {
            Some(alias) => Shallow::Normalize { alias },
            None => Shallow::Rule,
        })
    }

    /// `why_maybe`'s cause of the candidate of the frame at `top`, whose nested goals settled
    /// with `c` after its rule's shallow certainty `shallow`: `None` when its response is
    /// `Yes`.
    fn maybe_cause<T: Tys>(&mut self, tys: &mut T, top: usize, shallow: Certainty, c: Certainty) -> Result<Option<Cause>, Stop> {
        let f = self.frames[top];
        let note = self.maybe.note_of(top);
        if shallow != Certainty::Yes {
            return Ok(Some(Cause::Shallow(note.shallow)));
        }
        if c == Certainty::Yes || matches!(f.goal.pred, Pred::NormalizesTo { .. }) {
            return Ok(None);
        }
        if note.rounds {
            return Ok(Some(Cause::Rounds));
        }
        if c == Certainty::OverflowLimit
            && let Some(hit) = note.limit
        {
            return Ok(Some(Cause::Limit(hit)));
        }
        // The round compacted the pending goals to `nested.lo..`: the first is the one followed.
        let Some(g) = self.goals.get(f.nested.lo as usize).copied() else { return Ok(Some(Cause::Unknown)) };
        let child = self.input_of(&*tys, f.mode, Goal { env: f.goal.env, pred: g.pred, bound: g.bound })?;
        let above = match f.goal.pred {
            Pred::Trait { args, .. } if args.len > 0 => Some(tys.kind(tys.shallow_resolve(tys.list_get(args, 0)))),
            Pred::ForFields { container, .. } => Some(tys.kind(tys.shallow_resolve(container))),
            _ => None,
        };
        let reached = Reached::of(Some(Choice::of(&f.cand)), g.source, g.reach, above);
        Ok(Some(Cause::Nested { child, reached }))
    }

    /// The goal of a step of a [`WhyNo`] (its canonical input), decoded into `tys`: a region or
    /// type the canonical form erased is a fresh variable (call it in a probe of the caller's
    /// and read the terms before rolling back). `None`: no such input in this solver.
    pub fn why_goal<T: Tys>(&mut self, tys: &mut T, input: u32) -> Result<Option<(TypingMode, Goal)>, Stop> {
        if input as usize >= self.inputs.len() {
            return Ok(None);
        }
        let w = self.inputs.words_of(input);
        Ok(Some(canon::instantiate_input(tys, &mut self.decode, w)?))
    }

    fn evaluate_once<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        mode: TypingMode,
        goal: Goal,
        reads: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<Answer, Stop> {
        self.scratch.stats.evaluations += 1;
        // A recording solver frames every goal (`with_emit_in`), as `why_no` does.
        if !E::ON
            && !self.why_on
            && let Some(a) = self.fast_path(tys, mode, goal, reads)
        {
            self.scratch.stats.fast_path += 1;
            return Ok(a);
        }
        let reads_lo = self.reads.seg.len();
        self.reads.lo = reads_lo as u32;
        let r = self.evaluate_root(tys, src, mode, goal);
        match r {
            Ok((ans, orig_lo)) => {
                let answer = self.apply_root(tys, ans, orig_lo);
                self.origs.truncate(orig_lo as usize);
                let answer = answer?;
                self.remember_marker(tys, mode, goal, answer, reads_lo);
                self.reads.export(reads_lo, reads);
                self.reads.seg.truncate(reads_lo);
                self.resps.truncate_scratch((0, 0));
                Ok(answer)
            }
            Err(e) => {
                self.reads.seg.truncate(reads_lo);
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
        for &id in &self.marker_pool[s as usize..(s + l) as usize] {
            reads.push(self.reads.read(id));
        }
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
        let rs = self.reads.seg.from(reads_lo);
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

    /// The goal's interned canonical input; `canon.orig` holds its original values after. A
    /// goal with no inference variable is found by its own identity in the plain memo when it
    /// was seen before (no canonical walk: `canon_skipped`); any other goal is canonicalized
    /// (`canonicalized`). `self.words` holds the canonical words only after a walk: read them
    /// back by `Inputs::words_of`.
    fn input_of<T: Tys>(&mut self, tys: &T, mode: TypingMode, goal: Goal) -> Result<u32, Stop> {
        let key = if !self.scratch.off.memo { plain_key(tys, mode, goal)? } else { None };
        if let Some(k) = key
            && let Some(&input) = self.plain.get(&k)
        {
            self.canon.clear();
            self.scratch.stats.canon_skipped += 1;
            return Ok(input);
        }
        canon::goal_input(tys, &mut self.canon, &mut self.words, mode, goal)?;
        self.scratch.stats.canonicalized += 1;
        self.scratch.stats.canon_words += self.words.len() as u64;
        let input = self.inputs.intern(&self.words)?;
        if let Some(k) = key
            && self.canon.orig.is_empty()
            && self.plain.len() < MAX_PLAIN
        {
            let _ = self.plain.insert(k, input);
        }
        Ok(input)
    }

    /// Runs the root goal to its answer; the root's original values are left on `origs` from
    /// the returned index.
    fn evaluate_root<T: Tys, S: FactSource<T>>(&mut self, tys: &mut T, src: &mut S, mode: TypingMode, goal: Goal) -> Result<(Ans, u32), Stop> {
        let input = self.input_of(&*tys, mode, goal)?;
        if self.root_keys.is_some() && self.canon.orig.is_empty() {
            let mut ground = true;
            pred_terms(&*tys, goal.pred, &mut |t| {
                ground &= tys.flags(t).is_global_ground();
                Ok(())
            })?;
            if ground {
                self.scratch.stats.ground_roots += 1;
                let env = match self.env_word_rows.get(goal.env.0 as usize) {
                    Some(&(s, l)) => crate::search::hash_words(&self.env_words[s as usize..(s + l) as usize]),
                    None => 0,
                };
                let words = self.inputs.words_of(input);
                let key = crate::search::hash_words(&words[2..]) ^ env.rotate_left(17) ^ u64::from(words[0]);
                if let Some(keys) = self.root_keys.as_mut() {
                    keys.push(key);
                }
            }
        }
        let orig_lo = self.origs.len() as u32;
        self.origs.extend_from_slice(&self.canon.orig);
        // `why_no` frames the goal again, so each goal under it is seen (why_no.rs).
        if !self.why_on
            && let Some(e) = self.inputs.lookup(input, self.limit, true)
        {
            self.scratch.stats.cache_hits += 1;
            let lo = self.reads.lo;
            self.reads.seg.extend(lo, self.inputs.reads_of(&e))?;
            return Ok((e.ans, orig_lo));
        }
        // THE REQUEST'S SHARED GOALS (share.rs): a goal another body solved is one read here.
        let root_lo = self.reads.lo as usize;
        let mut audit: Option<Audit<'a>> = None;
        // A recording solver takes no shared answer (it has no choice behind it: witness.rs);
        // it still puts its own final goals (`share_after`) for the bodies that do.
        if !E::ON
            && !self.why_on
            && let Some(opts) = src.goals().map(|g| g.opts())
        {
            match self.share_root(tys, src, input, goal, opts) {
                Ok(Shared::Hit(ans)) => return Ok((ans, orig_lo)),
                Ok(Shared::Audit(a)) => audit = Some(a),
                Ok(Shared::Miss) => {}
                Err(e) => {
                    self.origs.truncate(orig_lo as usize);
                    return Err(e);
                }
            }
        }
        self.sharing = src.goals().is_some();
        self.share_new.clear();
        self.steps_at = self.steps_now();
        let mut snaps: Vec<T::Snapshot, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut probes: Vec<Option<T::Snapshot>, Alloc<'a>> = Vec::new_in(self.alloc);
        self.push_frame(tys, &mut snaps, &mut probes, input, mode, self.limit, PathKind::Inductive);
        let r = self.drive(tys, src, &mut snaps, &mut probes);
        self.sharing = false;
        let r = match r {
            Ok(ans) => match self.share_after(&*tys, &*src, ans, root_lo, goal, audit) {
                Ok(()) => Ok(ans),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        self.share_new.clear();
        match r {
            Ok(ans) => Ok((ans, orig_lo)),
            Err(e) => {
                // Unwind: the root's base snapshot undoes every frame's and probe's bindings.
                if let Some(&s) = snaps.first() {
                    tys.rollback_to(s);
                }
                if matches!(e, Stop::Need(_)) {
                    self.scratch.stats.need_stops += 1;
                }
                self.scratch.stats.frames_abandoned += self.frames.len() as u64;
                for f in self.frames.iter() {
                    self.inputs.on_stack[f.input as usize] = u32::MAX;
                }
                self.frames.clear();
                self.cands.clear();
                self.dones.clear();
                self.goals.clear();
                self.nested_ids.truncate(0);
                self.added.clear();
                self.deferred.clear();
                self.inputs.clear_provisional();
                Err(e)
            }
        }
    }

    /// The solver's steps so far (the count [`MAX_STEPS`] bounds per root).
    fn steps_now(&self) -> u64 {
        let s = &self.scratch.stats;
        s.frames + s.reruns + s.candidates
    }

    /// Why a goal may not be shared, or `None` (share.rs, "WHAT IS SHARED"): `vars` is the
    /// number of canonical variables of its input, `pred` its predicate in the arena.
    fn share_refusal<T: Tys>(&mut self, tys: &T, pred: Pred, vars: u32) -> Result<Option<Refuse>, SolveError> {
        if vars != 0 {
            return Ok(Some(Refuse::Vars));
        }
        let mut local = false;
        let work = &mut self.share_work;
        pred_terms(tys, pred, &mut |t| {
            if local {
                return Ok(());
            }
            if tys.flags(t).has(Flags::HAS_INFER | Flags::HAS_PLACEHOLDER | Flags::HAS_ERROR | Flags::HAS_CANONICAL) {
                local = true;
                return Ok(());
            }
            fold::visit(tys, work, t, |_, k| {
                if matches!(k, TyKind::Closure(..) | TyKind::Coroutine(..)) {
                    local = true;
                }
                !local
            })
        })?;
        Ok(if local { Some(Refuse::Local) } else { None })
    }

    fn count_refused(&mut self, why: Refuse) {
        let s = &mut self.scratch.stats;
        s.shared_refused += 1;
        match why {
            Refuse::Vars => s.shared_refused_vars += 1,
            Refuse::Local => s.shared_refused_local += 1,
            Refuse::ConstHolds => s.shared_refused_const_holds += 1,
            Refuse::Overflow => s.shared_refused_overflow += 1,
            Refuse::Scope => s.shared_refused_scope += 1,
            Refuse::Full => s.shared_refused_full += 1,
        }
    }

    /// The shared key's words of interned input `input` (share.rs, "THE KEY"): the code id,
    /// the recursion limit, the mode, the bound count, the environment's canonical words (its
    /// length first) in place of this solver's env id, then the predicate's words.
    fn key_words(&self, input: u32, out: &mut Vec<u32, Alloc<'a>>) -> Result<(), SolveError> {
        let w = self.inputs.words_of(input);
        let [mode, env, bound, pred @ ..] = w else {
            return Err(SolveError::Malformed(Malformed::Canonical { at: 0 }));
        };
        let env_words = match self.env_word_rows.get(*env as usize) {
            Some(&(s, l)) => &self.env_words[s as usize..(s + l) as usize],
            None => &[],
        };
        if env_words.len() + pred.len() + 5 > share::MOST_KEY_WORDS {
            return Err(SolveError::Limit(Limit::CanonicalSize));
        }
        out.clear();
        out.push(share::SOLVER_CODE_ID);
        out.push(self.limit);
        out.push(*mode);
        out.push(*bound);
        out.push(env_words.len() as u32);
        out.extend_from_slice(env_words);
        out.extend_from_slice(pred);
        Ok(())
    }

    /// The root goal `input` looked up in the request's table (share.rs). A hit's read list is
    /// checked against this body's facts first (`share_replay`, "THE SCOPE").
    fn share_root<T: Tys, S: FactSource<T>>(&mut self, tys: &mut T, src: &mut S, input: u32, goal: Goal, opts: ShareOpts) -> Result<Shared<'a>, Stop> {
        let vars = self.canon.orig.len() as u32;
        if self.share_refusal(&*tys, goal.pred, vars)?.is_some() {
            // Counted once, where the goal would be put (`share_after`).
            return Ok(Shared::Miss);
        }
        let mut kw = core::mem::replace(&mut self.share_key, Vec::new_in(self.alloc));
        let mut got = core::mem::replace(&mut self.share_got, Taken::new_in(self.alloc));
        let r = self.share_root_with(tys, src, input, opts, &mut kw, &mut got);
        self.share_key = kw;
        self.share_got = got;
        r
    }

    fn share_root_with<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        input: u32,
        opts: ShareOpts,
        kw: &mut Vec<u32, Alloc<'a>>,
        got: &mut Taken<'a>,
    ) -> Result<Shared<'a>, Stop> {
        self.key_words(input, kw)?;
        let digest = share::key_digest(kw);
        got.clear();
        let found = match src.goals() {
            Some(table) => table.get(GoalKey { digest, words: &kw[..] }, &mut |g| got.fill(g)),
            None => return Ok(Shared::Miss),
        };
        let found = match found {
            Ok(found) => found,
            Err(ShareRefused::Collision { digest }) => return Err(Stop::Error(SolveError::Share(ShareError::Collision { digest }))),
            Err(ShareRefused::Full { .. }) => false,
        };
        if !found {
            return Ok(Shared::Miss);
        }
        // THE SCOPE: the entry is taken only if every fact it read answers the same digest in
        // this body's facts (filled where they lack one, as solving the goal here would).
        if !opts.one_read_only && !self.share_replay(tys, src, &got.reads)? {
            self.count_refused(Refuse::Scope);
            return Ok(Shared::Miss);
        }
        let plant = opts.plant_hit_without_read;
        self.scratch.stats.shared_hits += 1;
        self.share_hits.push(GoalHit { key: digest, reads_digest: got.reads_digest });
        if opts.audit {
            // THE AUDIT: the hit is solved again by the caller (`evaluate_root`), the fresh
            // solve's reads are the ones recorded, and `share_after` compares.
            let hit_reads = if plant || opts.one_read_only { share::reads_digest(&[]) } else { got.reads_digest };
            let mut words: Vec<u32, Alloc<'a>> = Vec::new_in(self.alloc);
            words.extend_from_slice(&got.words);
            return Ok(Shared::Audit(Audit { key: digest, ans: got.ans, words, reads_digest: got.reads_digest, hit_reads }));
        }
        let ans = match got.ans {
            Some((c, via)) => Some(self.resps.push_cached(&got.words, c, via)?),
            None => None,
        };
        let mut ids = core::mem::replace(&mut self.share_ids, Vec::new_in(self.alloc));
        ids.clear();
        if !opts.one_read_only && !plant {
            for &r in got.reads.iter() {
                ids.push(self.reads.intern(r)?);
            }
            // SOUND WITH TODAY'S ENGINE (plan row A4, this piece): the hit ALSO appends the
            // goal's own reads to the body's list, exactly as solving the goal in the body
            // would, so a body is invalidated by the same facts as before. The one goal read
            // (`share_hits`) is recorded beside them; `ShareOpts::one_read_only` (tests, until
            // the engine keeps goals as records with their own rows) records it alone.
            let lo = self.reads.lo;
            self.reads.seg.extend(lo, &ids)?;
        }
        if !opts.one_read_only {
            // Into this solver's own cache, applicable only with an empty stack (its nested
            // goals are not known here): the next root ask of it is this solver's hit.
            self.inputs.insert(input, self.limit, got.overflow, ans, got.required_depth, &[], true, &ids)?;
        }
        self.share_ids = ids;
        Ok(Shared::Hit(ans))
    }

    /// Asks every read of a shared entry again of this body's facts: `true` when each answers
    /// the digest the entry holds. A fact the facts lack is filled by the source (and noted in
    /// the body's own list by it, as solving would); one it does not fill stops the root
    /// (`Stop::Need`), and the caller fills it and asks again.
    fn share_replay<T: Tys, S: FactSource<T>>(&mut self, tys: &mut T, src: &mut S, reads: &[Read]) -> Result<bool, Stop> {
        for r in reads {
            let mut filled = 0u32;
            loop {
                let got = Ask::new(src.facts()).replay(r.question);
                match got {
                    Ok(d) if d == r.digest => break,
                    Ok(_) => return Ok(false),
                    Err(Stop::Need(q)) if S::PULLS && filled < MAX_PULLS => {
                        if !src.fill(tys, q) {
                            return Err(Stop::Need(q));
                        }
                        filled += 1;
                        self.scratch.stats.pulls += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(true)
    }

    /// After a root finished: each final goal of its solve put in the request's table, and the
    /// audit's comparison when the root was a hit solved again.
    fn share_after<T: Tys, S: FactSource<T>>(&mut self, tys: &T, src: &S, root_ans: Ans, root_lo: usize, goal: Goal, audit: Option<Audit<'a>>) -> Result<(), Stop> {
        let Some(table) = src.goals() else { return Ok(()) };
        let news = core::mem::replace(&mut self.share_new, Vec::new_in(self.alloc));
        let mut kw = core::mem::replace(&mut self.share_key, Vec::new_in(self.alloc));
        let mut rs = core::mem::replace(&mut self.share_reads, Vec::new_in(self.alloc));
        let r = self.share_after_with(tys, table, &news, root_ans, root_lo, goal, audit, &mut kw, &mut rs);
        self.share_new = news;
        self.share_key = kw;
        self.share_reads = rs;
        r
    }

    fn share_after_with<T: Tys>(
        &mut self,
        tys: &T,
        table: &dyn share::GoalShare,
        news: &[NewGoal],
        root_ans: Ans,
        root_lo: usize,
        goal: Goal,
        audit: Option<Audit<'a>>,
        kw: &mut Vec<u32, Alloc<'a>>,
        rs: &mut Vec<Read, Alloc<'a>>,
    ) -> Result<(), Stop> {
        if let Some(a) = audit {
            self.scratch.stats.shared_audited += 1;
            let same_ans = match (root_ans, a.ans) {
                (None, None) => true,
                (Some(id), Some((c, via))) => self.resps.certainty(id) == c && self.resps.via(id) == via && self.resps.words_of(id) == &a.words[..],
                _ => false,
            };
            rs.clear();
            for &id in self.reads.seg.from(root_lo) {
                rs.push(self.reads.read(id));
            }
            rs.sort_unstable();
            rs.dedup();
            let fresh = share::reads_digest(rs);
            let what = if !same_ans {
                Some(share::AuditMiss::Answer)
            } else if fresh != a.reads_digest {
                Some(share::AuditMiss::EntryReads)
            } else if !table.opts().one_read_only && a.hit_reads != fresh {
                Some(share::AuditMiss::HitReads)
            } else {
                None
            };
            if let Some(what) = what {
                return Err(Stop::Error(SolveError::Share(ShareError::Audit { goal: bad_pred_of(tys, goal.pred), key: a.key, what })));
            }
        }
        for n in news {
            if let Some(why) = n.why {
                self.count_refused(why);
                continue;
            }
            rs.clear();
            let mut const_holds = false;
            for &id in self.inputs.reads_of(&n.entry) {
                let read = self.reads.read(id);
                const_holds |= matches!(read.question, Question::ConstHolds(..));
                rs.push(read);
            }
            if const_holds {
                self.count_refused(Refuse::ConstHolds);
                continue;
            }
            rs.sort_unstable();
            rs.dedup();
            let reads_digest = share::reads_digest(rs);
            self.key_words(n.input, kw)?;
            let key = GoalKey { digest: share::key_digest(&kw[..]), words: &kw[..] };
            let ans = n.entry.ans.map(|id| SharedAns { certainty: self.resps.certainty(id), via: self.resps.via(id), words: self.resps.words_of(id) });
            let put = table.put(key, SharedGoal { ans, required_depth: n.entry.required_depth, overflow: n.overflow, reads: &rs[..], reads_digest });
            match put {
                Ok(true) => self.scratch.stats.shared_puts += 1,
                Ok(false) => self.scratch.stats.shared_present += 1,
                Err(ShareRefused::Full { .. }) => self.count_refused(Refuse::Full),
                Err(ShareRefused::Collision { digest }) => return Err(Stop::Error(SolveError::Share(ShareError::Collision { digest }))),
            }
        }
        Ok(())
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
        self.scratch.stats.frames += 1;
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
            reads_lo: self.reads.seg.len() as u32,
            nested_lo: self.nested_ids.len() as u32,
            scratch_mark: self.resps.scratch_mark(),
        });
        self.reads.lo = self.reads.seg.len() as u32;
    }

    /// The main loop: runs the top frame until the root frame finishes.
    fn drive<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
    ) -> Result<Ans, Stop> {
        loop {
            if self.steps_now().saturating_sub(self.steps_at) > MAX_STEPS {
                let goal = self.frames.first().map(|f| bad_pred_of(&*tys, f.goal.pred));
                // The root goal alone does not say which rule re-entered (rule 19): the goal on
                // top of the stack when the bound was hit, and the stack's depth, do.
                let top = self.frames.last().map(|f| bad_pred_of(&*tys, f.goal.pred));
                let depth = u32::try_from(self.frames.len()).unwrap_or(u32::MAX);
                return Err(Stop::Error(SolveError::Steps { goal, top, depth, most: MAX_STEPS }));
            }
            let top = self.frames.len() - 1;
            match self.drive_one(tys, src, snaps, probes, top) {
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
                Err(Stop::Error(SolveError::Malformed(Malformed::Param { index, args, head, at, goal: None }))) => {
                    let goal = self.frames.get(top).map(|f| bad_pred_of(tys, f.goal.pred));
                    return Err(Stop::Error(SolveError::Malformed(Malformed::Param { index, args, head, at, goal })));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One phase of the top frame: the answer when the root finished.
    fn drive_one<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
        top: usize,
    ) -> Result<Option<Ans>, Stop> {
        let phase = self.frames[top].phase;
        match phase {
            Phase::Start => {
                self.pull(tys, src, top, |s, tys, facts| s.start(tys, facts, top))?;
                if let Some(t) = &mut self.trace {
                    let f = &self.frames[top];
                    if let (Some(args), Pred::Trait { trait_, .. }) = (t.ask().matches(&*tys, f.goal.pred), f.goal.pred) {
                        t.admit(f.input, trait_, args);
                    }
                }
            }
            Phase::Assemble(k) => self.assemble(tys, src, top, k)?,
            Phase::NextCand => self.next_cand(tys, src, probes, top)?,
            Phase::Step(k) => self.step(tys, src, probes, top, k)?,
            Phase::Nested => {
                self.nested(tys, snaps, probes, top)?;
            }
            Phase::Merge => {
                let ans = self.merge(tys, src, top)?;
                return self.finish(tys, snaps, probes, top, ans);
            }
        }
        Ok(None)
    }

    /// Runs one step of frame `top` that may ask the world (`run`), pulling each fact it lacks
    /// (the module, [`FactSource`]): on `Need(q)` everything the step changed is put back (the
    /// arena's bindings by a snapshot of the step, the frame's row, the rows it appended:
    /// candidates, done candidates, pending and added goals, deferred pairs, the original
    /// values, the read log's open segment, the universe, the response scratch), the source
    /// fills `q`, and the step runs again. Every step `run` stands for only appends to those
    /// rows and opens no snapshot it leaves open on success, so the rollback is exact and the
    /// second run sees what the first did. A source that cannot fill gets the old stop.
    fn pull<T: Tys, S: FactSource<T>, R>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        top: usize,
        mut run: impl FnMut(&mut Self, &mut T, &S::Facts) -> Result<R, Stop>,
    ) -> Result<R, Stop> {
        if !S::PULLS {
            return run(self, tys, src.facts());
        }
        for _ in 0..MAX_PULLS {
            let mark = StepMark {
                snap: tys.snapshot(),
                frame: self.frames[top],
                cands: self.cands.len(),
                dones: self.dones.len(),
                goals: self.goals.len(),
                added: self.added.len(),
                deferred: self.deferred.len(),
                origs: self.origs.len(),
                reads: self.reads.seg.len(),
                reads_lo: self.reads.lo,
                universe: self.universe,
                resps: self.resps.scratch_mark(),
            };
            match run(self, tys, src.facts()) {
                Ok(v) => {
                    tys.commit(mark.snap);
                    return Ok(v);
                }
                // Another stop ends the root, whose unwind rolls back past this snapshot; the
                // frame stays as the step left it, so the error names the goal it was at.
                Err(Stop::Error(e)) => return Err(Stop::Error(e)),
                Err(Stop::Need(q)) => {
                    tys.rollback_to(mark.snap);
                    self.frames[top] = mark.frame;
                    self.cands.truncate(mark.cands);
                    self.dones.truncate(mark.dones);
                    self.goals.truncate(mark.goals);
                    self.added.truncate(mark.added);
                    self.deferred.truncate(mark.deferred);
                    self.origs.truncate(mark.origs);
                    self.reads.seg.truncate(mark.reads);
                    self.reads.lo = mark.reads_lo;
                    self.universe = mark.universe;
                    self.resps.truncate_scratch(mark.resps);
                    if !src.fill(tys, q) {
                        return Err(Stop::Need(q));
                    }
                    self.scratch.stats.pulls += 1;
                }
            }
        }
        Err(Stop::Error(SolveError::Limit(Limit::Pulls)))
    }

    fn cx<'c, T: Tys, F: Facts>(&'c mut self, tys: &'c mut T, facts: &'c F, top: usize) -> Cx<'c, 'a, T, F> {
        let f = self.frames[top];
        self.reads.lo = f.reads_lo;
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
        if self.why_on {
            self.why.framed(input);
        }
        let (mode, goal) = {
            let w = self.inputs.words_of(input);
            canon::instantiate_input(tys, &mut self.decode, w)?
        };
        self.scratch.stats.decoded += 1;
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
            Pred::Trait { trait_, .. } | Pred::ForFields { trait_, .. } => {
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
            self.goals.push(Pending { pred: a.pred, bound: a.bound, source: a.source, via_only: a.via_only, done: false, reach: a.reach });
        }
        self.added.clear();
    }

    fn assemble<T: Tys, S: FactSource<T>>(&mut self, tys: &mut T, src: &mut S, top: usize, stage: u8) -> Result<(), Stop> {
        let f = self.frames[top];
        // `out` holds the candidates of earlier stages: a stage may keep a row there for a
        // later one (alias.rs's stash) and remove it. The rows stay in `cands` until the stage
        // answered, so a pulled stage starts again from them.
        let (r, out, st) = self.pull(tys, src, top, |s, tys, facts| {
            let mut out: Vec<Cand, Alloc<'a>> = Vec::new_in(s.alloc);
            out.extend_from_slice(&s.cands[f.cands_lo as usize..]);
            let mut st = f.tstate;
            let mut cx = s.cx(tys, facts, top);
            let r = match f.kind {
                Kind::Trait => trait_goals::assemble(&mut cx, stage, &mut st, &mut out)?,
                Kind::Alias => crate::alias::assemble(&mut cx, stage, &mut out)?,
                Kind::WellFormed => {
                    let Pred::WellFormed(t) = f.goal.pred else { return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred()))) };
                    crate::structural::assemble_wf(&mut cx, t, &mut out)?;
                    AStep::Done
                }
                Kind::ForFields => assemble_for_fields(&mut cx, stage, &mut st, &mut out)?,
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
            };
            Ok((r, out, st))
        })?;
        self.cands.truncate(f.cands_lo as usize);
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

    fn next_cand<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        probes: &mut [Option<T::Snapshot>],
        top: usize,
    ) -> Result<(), Stop> {
        let f = self.frames[top];
        if (f.cand_at as usize) < self.cands.len() {
            self.scratch.stats.candidates += 1;
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
                let st = f.tstate;
                let out = self.pull(tys, src, top, |s, tys, facts| {
                    let mut out: Vec<Cand, Alloc<'a>> = Vec::new_in(s.alloc);
                    let mut cx = s.cx(tys, facts, top);
                    trait_goals::assemble_impls(&mut cx, &st, &mut out)?;
                    Ok(out)
                })?;
                self.scratch.stats.impl_candidates += out.iter().filter(|c| matches!(c.source, Source::Impl(_))).count() as u64;
                self.added.clear();
                self.cands.extend_from_slice(&out);
                return Ok(());
            }
        }
        self.frames[top].phase = Phase::Merge;
        Ok(())
    }

    fn step<T: Tys, S: FactSource<T>>(
        &mut self,
        tys: &mut T,
        src: &mut S,
        probes: &mut [Option<T::Snapshot>],
        top: usize,
        stage: u8,
    ) -> Result<(), Stop> {
        // The trace's two counters before the step (witness.rs `Refusal`), only when tracing
        // or under `why_no`; and the negative impls `why_no` counts.
        let at = (self.trace.is_some() || self.why_on).then_some((self.scratch.stats.impl_prematch_rejected, self.scratch.stats.impl_header_mismatch));
        let polarity_at = self.scratch.stats.impl_polarity_refused;
        let (r, cand) = self.pull(tys, src, top, |s, tys, facts| {
            let mut cand = s.frames[top].cand;
            let mut cx = s.cx(tys, facts, top);
            let r = match cand.rule {
                CandRule::Certain(c) => Step::Done(c),
                CandRule::Trait(r) => trait_goals::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Builtin(r) => crate::builtin::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Structural(r) => crate::structural::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Alias(r) => crate::alias::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Coherence(r) => crate::coherence::step(&mut cx, &mut cand, r, stage)?,
                CandRule::Other(r) => step_other(&mut cx, &cand, r)?,
            };
            Ok((r, cand))
        })?;
        self.take_added();
        if let Some((prematch, mismatch)) = at
            && matches!(r, Step::No)
        {
            let now = self.scratch.stats;
            let why = if now.impl_prematch_rejected > prematch {
                Refusal::Prematch
            } else if now.impl_header_mismatch > mismatch {
                Refusal::Header
            } else {
                Refusal::Rule
            };
            self.trace_cand(top, cand, Fate::Refused(why));
            let negative = now.impl_polarity_refused > polarity_at;
            let input = self.frames[top].input;
            if self.why_on {
                self.why.refused(input, Choice::of(&cand), why, negative);
            }
        }
        self.frames[top].cand = cand;
        // Under `why_maybe`: why the rule's own certainty is not `Yes`, as the rule saw the
        // goal; the round limit is noted afresh for each nested loop that follows.
        if self.maybe_on {
            let shallow = match r {
                Step::Done(c) if c != Certainty::Yes => Some(self.maybe_shallow(tys, top)?),
                _ => None,
            };
            if let Some(n) = self.maybe.note(top) {
                n.rounds = false;
                n.limit = None;
                if let Some(s) = shallow {
                    n.shallow = s;
                }
            }
        }
        let fr = &mut self.frames[top];
        let lo = fr.probe_lo;
        match r {
            Step::No => self.end_probe(tys, probes, top, None),
            Step::Then(k) => Self::begin_nested(fr, lo, After::Step(k)),
            Step::Done(c) => Self::begin_nested(fr, lo, After::Make(c)),
        }
        Ok(())
    }

    /// Candidate `cand` of frame `top` met `fate`: a trace row when the frame's goal is traced.
    fn trace_cand(&mut self, top: usize, cand: Cand, fate: Fate) {
        let input = self.frames[top].input;
        if let Some(t) = &mut self.trace {
            t.cand(input, Choice::of(&cand), fate);
        }
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
    fn nested<T: Tys>(
        &mut self,
        tys: &mut T,
        snaps: &mut Vec<T::Snapshot, Alloc<'a>>,
        probes: &mut Vec<Option<T::Snapshot>, Alloc<'a>>,
        top: usize,
    ) -> Result<bool, Stop> {
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
                            self.nested_failed(tys, probes, top)?;
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
                if self.maybe_on
                    && let Some(note) = self.maybe.note(top)
                {
                    note.rounds = true;
                }
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
    fn nested_failed<T: Tys>(&mut self, tys: &mut T, probes: &mut [Option<T::Snapshot>], top: usize) -> Result<(), Stop> {
        let f = self.frames[top];
        // Under `why_no`: the goal that failed (`receive` leaves `nested.i` at it), by its
        // canonical input, and how the candidate (none: the frame itself) reached it.
        if self.why_on
            && let Some(g) = self.goals.get(f.nested.i as usize).copied()
        {
            let child = self.input_of(&*tys, f.mode, Goal { env: f.goal.env, pred: g.pred, bound: g.bound })?;
            let cand = match f.after {
                After::Assemble(_) => None,
                After::Step(_) | After::Make(_) => Some(Choice::of(&f.cand)),
            };
            let above = match f.goal.pred {
                Pred::Trait { args, .. } if args.len > 0 => Some(tys.kind(tys.shallow_resolve(tys.list_get(args, 0)))),
                Pred::ForFields { container, .. } => Some(tys.kind(tys.shallow_resolve(container))),
                _ => None,
            };
            let reached = Reached::of(cand, g.source, g.reach, above);
            self.why.failed(f.input, Failed { child, reached });
        }
        match f.after {
            After::Assemble(_) => {
                self.cands.truncate(f.cands_lo as usize);
                self.goals.truncate(f.goals_lo as usize);
                self.frames[top].fgoals_hi = f.goals_lo;
                self.frames[top].phase = Phase::Merge;
            }
            After::Step(_) | After::Make(_) => {
                if self.trace.is_some() {
                    // `receive` leaves `nested.i` at the goal that failed.
                    let (trait_, self_kind) = match self.goals.get(f.nested.i as usize).map(|g| g.pred) {
                        Some(Pred::Trait { trait_, args, .. }) => (Some(trait_), witness::kinds_of(&*tys, args)[0]),
                        _ => (None, None),
                    };
                    self.trace_cand(top, f.cand, Fate::NestedNo { trait_, self_kind });
                }
                self.end_probe(tys, probes, top, None)
            }
        }
        Ok(())
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
                // Under `why_maybe`: why this candidate is not certain, kept by the index its
                // response takes in `dones` (`end_probe` pushes it next).
                let r = self.make_response(tys, top, shallow, c)?;
                if self.maybe_on
                    && let Some(cause) = self.maybe_cause(tys, top, shallow, c)?
                {
                    self.maybe.cand_done(self.dones.len() as u32, Choice::of(&f.cand), cause);
                }
                if self.trace.is_some() {
                    let fate = Fate::Held { certainty: self.resps.certainty(r), constrains: !self.resps.is_identity(r) };
                    self.trace_cand(top, f.cand, fate);
                }
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
            self.goals.push(Pending { pred: g.pred, bound: g.bound, source: g.source, via_only: false, done: false, reach: Reach::Nth(u32::MAX) });
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
        let input = self.input_of(&*tys, f.mode, goal)?;
        let orig_lo = self.origs.len() as u32;
        self.origs.extend_from_slice(&self.canon.orig);
        self.frames[top].call_orig_lo = orig_lo;
        self.frames[top].call_via_only = g.via_only;
        let vars = self.canon.orig.len() as u32;
        let step = f.to_nested;
        // A cycle (`lookup_cycle` / `cycle_on_stack`).
        let h = self.inputs.on_stack[input as usize];
        if h != u32::MAX {
            let h = h as usize;
            let path = self.path_from(h, step);
            self.frames[h].used |= path.bit();
            self.frames[top].heads.insert(h as u32);
            // The head met is a nested goal of every goal between (`CycleOnStack(head)`): an
            // entry holding it does not apply while it is on the stack again.
            self.nested_ids.push(f.nested_lo, self.frames[h].input)?;
            let prov = self.frames[h].provisional;
            let ans = match prov {
                Some(a) => a,
                None => self.initial(vars, path)?,
            };
            return Ok(Got::Now(ans));
        }
        // The provisional cache.
        let mut hit = None;
        if !self.inputs.prov.is_empty() {
            let mut at = self.inputs.prov_head(input);
            while at != u32::MAX {
                let p = self.inputs.prov[at as usize];
                if (p.highest as usize) < self.frames.len() && self.path_from(p.highest as usize, step) == p.path {
                    hit = Some(p);
                    break;
                }
                at = self.inputs.prov_next(at);
            }
        }
        if let Some(p) = hit {
            // rustc `lookup_provisional_cache` -> `update_parent_goal`: the entry's overflow
            // is the parent's too.
            self.frames[top].overflow |= p.overflow;
            self.frames[top].heads.union(&p.heads);
            self.frames[p.highest as usize].used |= p.path.bit();
            // An answer that depends on a cycle (`ProvisionalCacheHit`): kept as a nested goal.
            self.nested_ids.push(f.nested_lo, input)?;
            return Ok(Got::Now(p.ans));
        }
        // The global cache.
        let avail = f.depth.saturating_sub(1);
        // `why_no` frames every goal under its root (why_no.rs): no global cache.
        if !self.why_on
            && let Some(e) = self.inputs.lookup(input, avail, false)
        {
            self.scratch.stats.cache_hits += 1;
            self.reads.seg.extend(f.reads_lo, self.inputs.reads_of(&e))?;
            let (big, depends) = match self.inputs.nested_of(&e) {
                Some(n) => {
                    self.nested_ids.extend(f.nested_lo, n)?;
                    (false, !n.is_empty())
                }
                None => (true, true),
            };
            // WHICH NESTED GOALS (`finish`): the entry's goal itself only when its answer met
            // a cycle or overflow.
            if depends || e.overflow {
                self.nested_ids.push(f.nested_lo, input)?;
            }
            let fr = &mut self.frames[top];
            fr.big |= big;
            fr.required_depth = fr.required_depth.max(e.required_depth + 1);
            // rustc search_graph/mod.rs `lookup_global_cache` -> `update_parent_goal`:
            // `parent.encountered_overflow |= encountered_overflow`. An overflow entry holds at
            // its own depth only (search.rs `lookup`); a parent built on it that is not marked
            // would be filed as a success, reused at any deeper limit with the child's
            // overflow inside (tests/overflow_cache.rs).
            fr.overflow |= e.overflow;
            return Ok(Got::Now(e.ans));
        }
        // Overflow (`stack_overflow_result`).
        if avail == 0 {
            if self.maybe_on {
                let depth = u32::try_from(self.frames.len()).unwrap_or(u32::MAX);
                let hit = LimitHit::Depth { depth, limit: self.limit };
                self.maybe.verdict(input, Verdict::Limit(hit));
                if let Some(n) = self.maybe.note(top) {
                    n.limit = Some(hit);
                }
            }
            self.frames[top].overflow = true;
            return Ok(Got::Now(Some(self.resps.no_constraints(vars, Certainty::OverflowLimit, Via::None)?)));
        }
        self.push_frame(tys, snaps, probes, input, f.mode, avail, step);
        Ok(Got::Pushed)
    }

    fn merge<T: Tys, S: FactSource<T>>(&mut self, tys: &mut T, src: &mut S, top: usize) -> Result<Ans, Stop> {
        let f = self.frames[top];
        self.merge_dones.clear();
        self.merge_dones.extend_from_slice(&self.dones[f.dones_lo as usize..]);
        let mut m = Merger { resps: &mut self.resps, vars: f.vars, bail: None };
        // Specialization between two impl candidates, a fact of the impls read like any other
        // (the frame's read list keeps it). Pull missing pairs in place so `filter_specialized_impls`
        // does not restart from `(0, 0)` on every miss (avoiding O(N^4) rescans).
        self.reads.lo = f.reads_lo;
        let reads = &mut self.reads;
        let dones = &mut self.merge_dones;
        let mut pulls = 0u32;
        let mut specializes = |a: Def, b: Def| -> Result<bool, Stop> {
            loop {
                match Ask::new(src.facts()).specializes(reads, a, b) {
                    Ok(v) => return Ok(v),
                    Err(Stop::Need(q)) if S::PULLS => {
                        if pulls >= MAX_PULLS {
                            return Err(Stop::Error(SolveError::Limit(Limit::Pulls)));
                        }
                        if !src.fill(tys, q) {
                            return Err(Stop::Need(q));
                        }
                        pulls += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        let merged = match f.kind {
            Kind::Trait => match trait_goals::merge(&mut m, f.mode, &f.tstate, dones, &mut specializes) {
                Ok(Some((r, via))) => Ok(Some(m.with_via(r, via)?)),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            },
            Kind::Alias => crate::alias::merge(&mut m, f.mode, f.goal.pred, dones, &mut specializes),
            Kind::WellFormed | Kind::ForFields | Kind::Other => m.merge_responses(dones).map_err(Stop::Error),
        };
        self.scratch.stats.pulls += u64::from(pulls);
        let ans = merged?;
        let bail = m.bail;
        if self.maybe_on {
            self.maybe_merged(top, ans, bail);
        }
        if let Some(t) = &mut self.trace {
            let verdict = ans.map(|r| (self.resps.certainty(r), !self.resps.is_identity(r)));
            t.merge(f.input, self.merge_dones.len() as u32, verdict);
        }
        if E::ON
            && let Some(r) = ans
        {
            self.note_choice(f.input, r)?;
        }
        Ok(ans)
    }

    /// Records the frame's choice (witness.rs): of the candidates the merge kept (`merge_dones`,
    /// as it filtered them), those answering `Yes` with the merged response's words; the least
    /// by `witness::least`. Nothing is recorded for an answer other than `Yes`.
    fn note_choice(&mut self, input: u32, r: RespId) -> Result<(), Stop> {
        if self.resps.certainty(r) != Certainty::Yes {
            return Ok(());
        }
        let resps = &self.resps;
        let best = crate::witness::least(
            self.merge_dones
                .iter()
                .filter(|d| resps.certainty(d.resp) == Certainty::Yes && resps.equal(d.resp, r))
                .map(|d| Choice::of(&d.cand)),
        );
        if let Some(c) = best {
            self.emit.record(input, c)?;
        }
        Ok(())
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
                    self.scratch.stats.reruns += 1;
                    self.inputs.drop_provisional(top as u32);
                    self.rerun(tys, snaps, probes, top, ans);
                    return Ok(None);
                }
                // `fixpoint_overflow_result`.
                if self.maybe_on {
                    self.maybe.verdict(f.input, Verdict::Limit(LimitHit::Fixpoint { reruns: FIXPOINT_STEP_LIMIT }));
                }
                let r = self.resps_no_constraints(f.vars, OVERFLOW)?;
                ans = Some(self.resps.keep(r)?);
            }
        }
        self.inputs.drop_provisional(top as u32);
        // Whether the goal may be shared (share.rs, "WHAT IS SHARED"): asked of its decoded
        // goal while its terms are still in the arena, before the pop rolls them back.
        let share_why = if self.sharing { Some(self.share_refusal(&*tys, f.goal.pred, f.vars)?) } else { None };
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
        // The frame's reads and nested goals are sets already (readlog.rs): the entry takes them
        // as they are, and they fold into the parent's below.
        let rl = f.reads_lo as usize;
        let nl = f.nested_lo as usize;
        let mut heads = f.heads;
        heads.remove(top as u32);
        if heads.is_empty() {
            let (inputs, nested, reads) = (&mut self.inputs, self.nested_ids.from(nl), self.reads.seg.from(rl));
            let entry = inputs.insert(f.input, f.depth, f.overflow, ans, f.required_depth, nested, f.big, reads)?;
            // A FINAL result (no cycle head below it): put when the root ends. A cycle's
            // members other than its head finish provisional (the branch below) and are never
            // put; the head is the goal whose frame is lowest on the stack, so every worker
            // that enters the cycle at one goal puts that goal's answer, a function of it
            // alone (share.rs). A nested goal's overflow holds only at its depth: not put.
            if let Some(why) = share_why {
                let why = if why.is_none() && f.overflow && top != 0 { Some(Refuse::Overflow) } else { why };
                self.share_new.push(NewGoal { input: f.input, entry, overflow: f.overflow, why });
            }
        } else if let Some(highest) = heads.highest() {
            self.scratch.stats.provisional += 1;
            let path = {
                let mut p = f.from_parent;
                for g in self.frames[highest as usize + 1..].iter() {
                    p = p.extend(g.from_parent);
                }
                p
            };
            self.inputs.push_provisional(Prov { input: f.input, heads, highest, path, ans, overflow: f.overflow });
        }
        if top == 0 {
            // The root's reads stay for `evaluate` to hand out; its nested goals are done with.
            self.nested_ids.truncate(nl);
            return Ok(Some(ans));
        }
        let p = top - 1;
        let pf = self.frames[p];
        self.scratch.stats.reads_folded += (self.reads.seg.len() - rl) as u64;
        self.reads.seg.close(pf.reads_lo, f.reads_lo);
        self.reads.lo = pf.reads_lo;
        // WHICH NESTED GOALS (`update_parent_goal`: "once we've got goals which encountered
        // overflow or a cycle, we track all goals whose behavior may depend changed if they're
        // moved onto the stack"): this goal is kept in its parent's nested goals only when its
        // own are not empty (it met a cycle head, a provisional answer, or such an entry) or it
        // overflowed. A cycle-free goal's answer is the same with any goal on the stack: were a
        // goal of its subtree on the stack and asking it, that goal's own evaluation would have
        // met it as a cycle (evaluation is a function of the goal), so its subtree is not
        // cycle-free. Before, every goal below was kept: an entry with more than `MAX_NESTED`
        // applied only to an empty stack, and so did every entry above it, so a shared subtree
        // under two parents was solved again under each, 2^depth times (tempo
        // crates/consensus/src/follow/resolver/mod.rs `get_block`, `SolveError::Steps` at
        // depth 30; tests/cost.rs `cost_shared_wide_subtree_is_solved_once`).
        let depends = self.nested_ids.len() > nl || f.overflow || f.big;
        self.nested_ids.close(pf.nested_lo, f.nested_lo);
        if depends {
            self.nested_ids.push(pf.nested_lo, f.input)?;
        }
        {
            let pf = &mut self.frames[p];
            pf.required_depth = pf.required_depth.max(f.required_depth + 1);
            pf.overflow |= f.overflow;
            pf.big |= f.big;
            pf.heads.union(&heads);
        }
        if !self.receive(tys, p, ans)? {
            self.nested_failed(tys, probes, p)?;
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
        self.reads.seg.truncate(f.reads_lo as usize);
        self.reads.lo = f.reads_lo;
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

/// Calls `f` on every term of `p`.
fn pred_terms<T: Tys>(tys: &T, p: Pred, f: &mut impl FnMut(Ty) -> Result<(), SolveError>) -> Result<(), SolveError> {
    let list = |l: crate::term::List, f: &mut dyn FnMut(Ty) -> Result<(), SolveError>| -> Result<(), SolveError> {
        for i in 0..l.len {
            f(tys.list_get(l, i))?;
        }
        Ok(())
    };
    match p {
        Pred::Trait { args, .. } | Pred::HostEffect { args, .. } | Pred::ConstHolds { args, .. } => list(args, f),
        Pred::ForFields { container, args_after_self, .. } => {
            f(container)?;
            list(args_after_self, f)
        }
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
) -> Result<Pred, SolveError> {
    let at = core::panic::Location::caller();
    let inst = Inst { args, bound: &[] };
    Ok(match pred {
        Pred::Trait { trait_, args: a, polarity } => {
            Pred::Trait { trait_, args: fold::import_list_at(tys, s, p, a, inst, at)?, polarity }
        }
        Pred::HostEffect { trait_, args: a, constness } => {
            Pred::HostEffect { trait_, args: fold::import_list_at(tys, s, p, a, inst, at)?, constness }
        }
        Pred::ConstHolds { cond, args: a } => {
            Pred::ConstHolds { cond, args: fold::import_list_at(tys, s, p, a, inst, at)? }
        }
        Pred::ForFields { container, trait_, args_after_self } => Pred::ForFields {
            container: fold::import_at(tys, s, p, container, inst, at)?,
            trait_,
            args_after_self: fold::import_list_at(tys, s, p, args_after_self, inst, at)?,
        },
        Pred::Projection { alias: a, term: b }
        | Pred::TypeOutlives { ty: a, region: b }
        | Pred::RegionOutlives { a, b } => {
            let mut out = [Ty::NONE; 2];
            fold::import_slice(tys, s, p, &[a, b], inst, &mut out, at)?;
            match pred {
                Pred::Projection { .. } => Pred::Projection { alias: out[0], term: out[1] },
                Pred::TypeOutlives { .. } => Pred::TypeOutlives { ty: out[0], region: out[1] },
                Pred::RegionOutlives { .. } => Pred::RegionOutlives { a: out[0], b: out[1] },
                _ => unreachable!(),
            }
        }
        Pred::WellFormed(t) => Pred::WellFormed(fold::import_at(tys, s, p, t, inst, at)?),
        Pred::ConstEvaluatable(t) => Pred::ConstEvaluatable(fold::import_at(tys, s, p, t, inst, at)?),
        Pred::DynCompatible(d) => Pred::DynCompatible(d),
        _ => return Err(SolveError::Malformed(crate::error::bad_pred())),
    })
}

/// The steps of [`Rule`].
fn step_other<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand, rule: Rule) -> Result<Step, Stop> {
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
            // An undecided verdict stops the goal (builtin.rs `dyn_compatible`), never No.
            if crate::builtin::dyn_compatible(cx, d)? { Step::Done(Certainty::Yes) } else { Step::No }
        }
        Rule::ConstHolds => {
            let Pred::ConstHolds { cond, args } = cx.goal.pred else { return Err(bad()) };
            const_holds(cx, cond, args)?
        }
        Rule::ForFieldsEnv => step_for_fields_env(cx, cand.n)?,
        Rule::ForFieldsStructural => step_for_fields_structural(cx, cand.a)?,
        Rule::HostEffect => {
            let Pred::HostEffect { trait_, args, .. } = cx.goal.pred else { return Err(bad()) };
            cx.add(GoalSource::Misc, Pred::Trait { trait_, args, polarity: Polarity::Positive });
            Step::Done(Certainty::Yes)
        }
    })
}

/// Assembles candidates for `Pred::ForFields { container, trait_, args_after_self }`:
/// stage 0 normalizes a non-rigid alias container via `Projection`; stage 1 emits one
/// `ParamEnv` candidate per matching `ForFields` assumption plus one structural candidate.
fn assemble_for_fields<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    stage: u8,
    st: &mut trait_goals::State,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<AStep, Stop> {
    let Pred::ForFields { container, trait_, .. } = cx.goal.pred else {
        return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred())));
    };
    if stage == 0 {
        let container = cx.resolve(container);
        st.self_ty = container;
        if crate::alias::needs_normalizing(cx, container)? {
            let v = cx.fresh_ty()?;
            cx.add(GoalSource::NormalizeGoal, Pred::Projection { alias: container, term: v });
            st.self_ty = v;
            return Ok(AStep::Then(1));
        }
    }
    let container = cx.resolve(st.self_ty);
    // Step A: ParamEnv assumptions for the same trait.
    for (i, c) in (0u32..).zip(cx.env.iter()) {
        if let Pred::ForFields { trait_: env_tr, .. } = c.pred
            && env_tr == trait_
        {
            let mut cand = Cand::new(Source::ParamEnv { global: false }, CandRule::Other(Rule::ForFieldsEnv));
            cand.d = trait_;
            cand.n = i;
            out.push(cand);
        }
    }
    // Steps B, C, D: structural decomposition of `container`.
    let mut cand = Cand::new(Source::Builtin(crate::assembly::BuiltinSource::Misc), CandRule::Other(Rule::ForFieldsStructural));
    cand.a = container;
    cand.d = trait_;
    out.push(cand);
    Ok(AStep::Done)
}

/// Step A: match `Pred::ForFields` against param-env clause `idx`.
fn step_for_fields_env<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, idx: u32) -> Result<Step, Stop> {
    let bad = || Stop::Error(SolveError::Malformed(crate::error::bad_pred()));
    let Pred::ForFields { container, trait_, args_after_self } = cx.goal.pred else { return Err(bad()) };
    let Some(&clause) = usize::try_from(idx).ok().and_then(|i| cx.env.get(i)) else { return Err(bad()) };
    let Pred::ForFields { container: env_c, trait_: env_tr, args_after_self: env_args } = clause.pred else {
        return Err(bad());
    };
    if env_tr != trait_ {
        return Ok(Step::No);
    }
    let (env_c, env_args) = if clause.bound > 0 {
        let with = cx.fresh_args(clause.bound)?;
        let env_c = cx.instantiate_bound(env_c, with)?;
        let mut f = |cx: &mut Cx<'_, '_, T, F>, t: Ty| cx.instantiate_bound(t, with);
        let env_args = cx.map_list(env_args, &mut f)?;
        (env_c, env_args)
    } else {
        (env_c, env_args)
    };
    if !cx.eq(container, env_c)? || !cx.eq_lists(args_after_self, env_args)? {
        return Ok(Step::No);
    }
    Ok(Step::Done(Certainty::Yes))
}

/// Steps B, C, D: structurally prove `Pred::ForFields` over a concrete product type
/// (struct ADT, structural record, or tuple), stall on `TyKind::Infer`, and reject all
/// other types (including primitives, enums, unions, and unassumed generic params).
fn step_for_fields_structural<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, container: Ty) -> Result<Step, Stop> {
    let Pred::ForFields { trait_, args_after_self, .. } = cx.goal.pred else {
        return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred())));
    };
    let container = cx.resolve(container);
    match cx.tys.kind(container) {
        TyKind::Infer(_) => Ok(Step::Done(Certainty::Ambiguous)),
        TyKind::Error => Ok(Step::Done(Certainty::Yes)),
        TyKind::Adt(adt_def, adt_args) => {
            let adt = cx.adt(adt_def)?;
            if !adt.flags.has(crate::facts::AdtFlags::STRUCT) {
                return Ok(Step::No);
            }
            let mut fields: Vec<Ty, Alloc<'_>> = Vec::new_in(cx.alloc);
            crate::builtin::adt_fields(cx, adt_def, adt_args, &mut fields)?;
            add_for_fields_goals(cx, trait_, args_after_self, &fields, true)?;
            Ok(Step::Done(Certainty::Yes))
        }
        TyKind::Record(_, fields_list) => {
            let fields = cx.list_small(fields_list);
            add_for_fields_goals(cx, trait_, args_after_self, &fields, false)?;
            Ok(Step::Done(Certainty::Yes))
        }
        TyKind::Tuple(elems_list) => {
            let elems = cx.list_small(elems_list);
            add_for_fields_goals(cx, trait_, args_after_self, &elems, false)?;
            Ok(Step::Done(Certainty::Yes))
        }
        _ => Ok(Step::No),
    }
}

/// Adds `F_i: trait_<args_after_self...>` as an `ImplWhereBound` nested goal for every field `F_i`.
fn add_for_fields_goals<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    trait_: Def,
    args_after_self: crate::term::List,
    fields: &[Ty],
    is_adt: bool,
) -> Result<(), Stop> {
    let rest = cx.list_small(args_after_self);
    let mut args = crate::assembly::SmallTys::new_with_len(1 + rest.len(), cx.alloc);
    args[1..].copy_from_slice(&rest);
    for (i, &field_ty) in (0u32..).zip(fields.iter()) {
        args[0] = field_ty;
        let list = cx.mk_list(&args)?;
        let pred = Pred::Trait { trait_, args: list, polarity: Polarity::Positive };
        let reach = if is_adt { Reach::Field(i) } else { Reach::Element(i) };
        cx.add_from(GoalSource::ImplWhereBound, pred, reach);
    }
    Ok(())
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
    let mut v = cx.list_small(args);
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
    let two = |out: &mut BadPred, kind: PredKind, a: Ty, b: Ty| {
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
        Pred::ForFields { container, trait_, args_after_self } => {
            out.kind = PredKind::ForFields;
            out.def = trait_;
            out.n_args = args_after_self.len.saturating_add(1);
            out.args[0] = Some(arg_shape(tys, container));
            for (i, slot) in (0u32..).zip(out.args[1..].iter_mut()) {
                if i >= args_after_self.len {
                    break;
                }
                *slot = Some(arg_shape(tys, tys.list_get(args_after_self, i)));
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
