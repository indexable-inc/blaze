//! The solver's cost per goal, counted on fixtures (`eval::Stats`): every goal of a fixture is
//! asked of ONE solver, as one body's goals are, and the counters say how much work the answers
//! took. The fixtures are the shapes tempo's check spends its time on (the busy thread's profile
//! of 2026-11-17: structural auto-trait goals walking ADT fields, terms imported, read lists
//! sorted per goal):
//!
//! - `nested`: six levels of generic structs of eight fields each, asked `Send`, `Sized` and
//!   `Clone` at 100 different outer types, and `Send` of a tuple holding one fixed inner type.
//! - `chain`: the goals a method chain `xs.iter().map(f).filter(p).collect::<Vec<_>>()` raises,
//!   for 20 chains, over a world with 30 other `Iterator` impls.
//! - `deep`: 40 impls whose headers differ below the fingerprint's depth.
//!
//! Each fixture's answers are all `yes`; the counters are bounded so a change that brings the
//! cost back fails here with the counters in its message.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::assembly::{Clause, Goal};
use crate::eval::{Answer, Off, Solver, Stats};
use crate::facts::Read;
use crate::term::TypingMode;
use crate::testing::parse::{CheckA, DeclA, parse_file};
use crate::testing::run::PRELUDE;
use crate::testing::tys::TestTys;
use crate::testing::world::{Arena, Scope, World};
use crate::tys::Tys;

/// What one fixture run gave: the counters, each goal's answer, and the reads in the order the
/// solver reported them, with where each goal's start (`starts[i]`).
pub(crate) struct Run {
    pub stats: Stats,
    pub answers: Vec<Answer>,
    pub reads: Vec<Read>,
    pub starts: Vec<usize>,
}

impl Run {
    /// Goal `i`'s reads as a sorted set.
    fn set(&self, i: usize) -> Vec<Read> {
        let hi = self.starts.get(i + 1).copied().unwrap_or(self.reads.len());
        let mut v: Vec<Read> = self.reads[self.starts[i]..hi].to_vec();
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// Solves every goal of the one case in `text` (decls first) with one solver, or with
/// `fresh`, each goal with its own solver and arena (no cache, no fast path).
pub(crate) fn run(text: &str) -> Result<Run, String> {
    run_with(text, false, ON, Feed::Whole)
}

/// How the solver gets its facts in a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    /// The whole world, nothing ever missing.
    Whole,
    /// [`Shy`] (each question missing at its first ask) with the stop: the evaluation stops
    /// with `Need`, the caller fills it and asks the root again (`Stopping`).
    ShyStop,
    /// [`Shy`] with the pull: the solver fills it where it is met and redoes that step.
    ShyPull,
}

/// The world answering `Missing` to each question until it is filled: the stop path's worst
/// case, every fact a body reads not yet in its cache.
struct Shy<'w> {
    world: &'w World,
    given: std::collections::HashSet<crate::facts::Question>,
}

impl Shy<'_> {
    fn gate(&self, q: crate::facts::Question) -> crate::facts::Fetch<()> {
        if self.given.contains(&q) { Ok(()) } else { Err(crate::facts::Missing) }
    }
}

impl crate::facts::Facts for Shy<'_> {
    fn trait_def(&self, seal: crate::facts::Seal, t: crate::term::Def) -> crate::facts::Fetch<crate::facts::TraitDef<'_>> {
        self.gate(crate::facts::Question::TraitDef(t))?;
        self.world.trait_def(seal, t)
    }
    fn impls(&self, seal: crate::facts::Seal, t: crate::term::Def, shape: crate::term::Shape) -> crate::facts::Fetch<crate::facts::ImplSet<'_>> {
        self.gate(crate::facts::Question::Impls(t, shape))?;
        self.world.impls(seal, t, shape)
    }
    fn impl_header(&self, seal: crate::facts::Seal, i: crate::term::Def) -> crate::facts::Fetch<crate::facts::ImplHeader<'_>> {
        self.gate(crate::facts::Question::ImplHeader(i))?;
        self.world.impl_header(seal, i)
    }
    fn adt(&self, seal: crate::facts::Seal, d: crate::term::Def) -> crate::facts::Fetch<crate::facts::AdtDef<'_>> {
        self.gate(crate::facts::Question::Adt(d))?;
        self.world.adt(seal, d)
    }
    fn fn_sig(&self, seal: crate::facts::Seal, d: crate::term::Def) -> crate::facts::Fetch<crate::facts::FnSigDef<'_>> {
        self.gate(crate::facts::Question::FnSig(d))?;
        self.world.fn_sig(seal, d)
    }
    fn assoc(&self, seal: crate::facts::Seal, d: crate::term::Def) -> crate::facts::Fetch<crate::facts::AssocDef> {
        self.gate(crate::facts::Question::Assoc(d))?;
        self.world.assoc(seal, d)
    }
    fn opaque(&self, seal: crate::facts::Seal, d: crate::term::Def) -> crate::facts::Fetch<crate::facts::OpaqueDef<'_>> {
        // One fact, two kinds of question (`Question::OpaqueHidden`): either fill gives it.
        self.gate(crate::facts::Question::Opaque(d)).or_else(|_| self.gate(crate::facts::Question::OpaqueHidden(d)))?;
        self.world.opaque(seal, d)
    }
    fn lang(&self, seal: crate::facts::Seal, l: crate::term::Lang) -> crate::facts::Fetch<Option<crate::term::Def>> {
        self.gate(crate::facts::Question::Lang(l))?;
        self.world.lang(seal, l)
    }
    fn is_local(&self, seal: crate::facts::Seal, d: crate::term::Def) -> crate::facts::Fetch<bool> {
        self.gate(crate::facts::Question::IsLocal(d))?;
        self.world.is_local(seal, d)
    }
    /// Its question names the caller's terms, not the arguments: answered at once.
    fn const_holds(&self, seal: crate::facts::Seal, cond: crate::term::Def, args: &[crate::term::Ty]) -> crate::facts::Fetch<crate::facts::ConstHolds> {
        self.world.const_holds(seal, cond, args)
    }
    fn specializes(&self, seal: crate::facts::Seal, a: crate::term::Def, b: crate::term::Def) -> crate::facts::Fetch<bool> {
        self.gate(crate::facts::Question::Specializes(a, b))?;
        self.world.specializes(seal, a, b)
    }
}

/// The pull over [`Shy`]: a fill gives the question; a second fill of one question is a step
/// that did not see its fill, refused (`false`) so the run fails with the `Need`.
struct ShyPull<'s, 'w>(&'s mut Shy<'w>);

impl<'w, T: Tys> crate::facts::Source<T> for ShyPull<'_, 'w> {
    type Facts = Shy<'w>;
    const PULLS: bool = true;
    fn facts(&self) -> &Shy<'w> {
        &*self.0
    }
    fn fill(&mut self, _tys: &mut T, q: crate::facts::Question) -> bool {
        self.0.given.insert(q)
    }
}

/// The most `Need` rounds the stop path's loop takes for one ask (a fixture reads far fewer).
const MAX_ROUNDS: usize = 1 << 14;

/// `Solver::evaluate` under `feed`: with `ShyStop` the caller's loop, filling each `Need` and
/// asking the root again, as typeck's `InferCtx::evaluate` did.
fn eval_fed(
    feed: Feed,
    solver: &mut Solver<'static>,
    tys: &mut TestTys,
    shy: &mut Shy<'_>,
    goal: Goal,
    reads: &mut blaze_own::Vec<Read, Alloc<'static>>,
) -> Result<Answer, crate::error::Stop> {
    let mode = TypingMode::Analysis;
    match feed {
        Feed::Whole => solver.evaluate(tys, shy.world, mode, goal, reads),
        Feed::ShyPull => solver.evaluate_from(tys, &mut ShyPull(shy), mode, goal, reads),
        Feed::ShyStop => {
            for _ in 0..MAX_ROUNDS {
                match solver.evaluate(tys, &*shy, mode, goal, reads) {
                    Err(crate::error::Stop::Need(q)) if shy.given.insert(q) => {}
                    r => return r,
                }
            }
            Err(crate::error::Stop::Error(crate::error::SolveError::Limit(crate::error::Limit::Rounds)))
        }
    }
}

/// `Solver::env` under `feed` (see [`eval_fed`]).
fn env_fed(
    feed: Feed,
    solver: &mut Solver<'static>,
    tys: &mut TestTys,
    shy: &mut Shy<'_>,
    clauses: &[Clause],
    reads: &mut blaze_own::Vec<Read, Alloc<'static>>,
) -> Result<crate::assembly::EnvId, crate::error::Stop> {
    match feed {
        Feed::Whole => solver.env(tys, shy.world, clauses, reads),
        Feed::ShyPull => solver.env_from(tys, &mut ShyPull(shy), clauses, reads),
        Feed::ShyStop => {
            for _ in 0..MAX_ROUNDS {
                reads.clear();
                match solver.env(tys, &*shy, clauses, reads) {
                    Err(crate::error::Stop::Need(q)) if shy.given.insert(q) => {}
                    r => return r,
                }
            }
            Err(crate::error::Stop::Error(crate::error::SolveError::Limit(crate::error::Limit::Rounds)))
        }
    }
}

/// Every shortcut on.
const ON: Off = Off { summaries: false, prematch: false, memo: false };
/// Every shortcut off: each step as the rules take it.
const PLAIN: Off = Off { summaries: true, prematch: true, memo: true };

/// `off`: the shortcuts turned off (eval.rs `Off`).
fn run_with(text: &str, fresh: bool, off: Off, feed: Feed) -> Result<Run, String> {
    let prelude = parse_file(PRELUDE).map_err(|e| format!("the prelude: {e}"))?;
    let f = parse_file(text).map_err(|e| format!("the fixture: {e}"))?;
    let [case] = f.cases.as_slice() else { return Err(format!("{} cases, want one", f.cases.len())) };
    let mut decls: Vec<(u32, &DeclA)> = Vec::new();
    decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
    decls.extend(f.decls.iter().map(|(l, d)| (*l, d)));
    decls.extend(case.decls.iter().map(|(l, d)| (*l, d)));
    let world = World::build(&decls).map_err(|e| format!("the world: {e}"))?;
    let mut shy = Shy { world: &world, given: std::collections::HashSet::new() };
    let alloc = blaze_own::testing::heap().alloc();
    let mut tys = TestTys::new();
    let mut vars = std::collections::HashMap::new();
    let mut solver = Solver::new_in(alloc);
    solver.turn_off(off);
    let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
    let sc = Scope { params: &case.params, bound: &[] };
    // The case's `env` clauses and its parameters' implicit `Sized`, as run.rs builds them
    // (no `?Sized` here).
    let clauses = |tys: &mut TestTys, vars: &mut std::collections::HashMap<String, crate::term::Ty>| -> Result<Vec<Clause>, String> {
        let mut out: Vec<Clause> = Vec::new();
        let mut a = Arena { tys, vars };
        for p in &case.env {
            world.names.pred(&mut a, sc, case.line, p, &mut out).map_err(|e| format!("the env: {e}"))?;
        }
        let sized: Vec<bool> = case.params.iter().map(|_| false).collect();
        world.names.case_sized(&mut a, case.line, &sized, &mut out).map_err(|e| format!("the env: {e}"))?;
        Ok(out)
    };
    let cl0 = clauses(&mut tys, &mut vars)?;
    let mut env = env_fed(feed, &mut solver, &mut tys, &mut shy, &cl0, &mut reads).map_err(|s| format!("the env: {s:?}"))?;
    reads.clear();
    let mut answers = Vec::new();
    let mut starts = Vec::new();
    for g in &case.goals {
        let CheckA::Goal(p) = &g.check else { return Err(format!("line {}: not a goal", g.line)) };
        if fresh {
            tys = TestTys::new();
            vars = std::collections::HashMap::new();
            solver = Solver::new_in(alloc);
            solver.turn_off(off);
            let mut scratch: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
            let cl = clauses(&mut tys, &mut vars)?;
            env = env_fed(feed, &mut solver, &mut tys, &mut shy, &cl, &mut scratch).map_err(|s| format!("the env: {s:?}"))?;
        }
        starts.push(reads.len());
        let mut cl: Vec<Clause> = Vec::new();
        {
            let mut a = Arena { tys: &mut tys, vars: &mut vars };
            world.names.pred(&mut a, sc, g.line, p, &mut cl).map_err(|e| format!("line {}: {e}", g.line))?;
        }
        let [c]: [Clause; 1] = cl.try_into().map_err(|_| format!("line {}: a goal is one predicate", g.line))?;
        let snap = tys.snapshot();
        let a = eval_fed(feed, &mut solver, &mut tys, &mut shy, Goal { env, pred: c.pred, bound: c.bound }, &mut reads)
            .map_err(|s| format!("line {}: the solver stopped: {s:?}", g.line))?;
        tys.rollback_to(snap);
        answers.push(a);
    }
    Ok(Run { stats: solver.stats(), answers, reads: reads.iter().copied().collect(), starts })
}

/// The two claims, per goal:
///
/// - with the solver's caches (one solver for all goals: the task cache, the ground fast path,
///   the read log's folding, the summaries' table) a goal's answer and read set equal those
///   of a fresh solver: what a goal answers and is valid by does not depend on what was asked
///   before it (rule 25);
/// - with the shortcuts and without (the auto-trait summaries, every step a goal of its own;
///   the header pre-match, every header imported) the answers are equal (rule 78), and so
///   are the read sets of every `yes`; a summary walks every field, so for a goal that fails
///   its set may hold more than the goal-by-goal walk read before its first failing field (a
///   larger read list is valid by more facts, never fewer: rule 21).
fn same_reads(name: &str, text: &str) {
    let get = |fresh: bool, off: Off| match run_with(text, fresh, off, Feed::Whole) {
        Ok(r) => r,
        Err(e) => panic!("{name} (fresh {fresh}, off {off:?}): {e}"),
    };
    let (shared, fresh, plain) = (get(false, ON), get(true, ON), get(true, PLAIN));
    assert_eq!(shared.answers, fresh.answers, "{name}: answers differ with the cache");
    assert_eq!(fresh.answers, plain.answers, "{name}: answers differ with the summaries");
    for i in 0..shared.answers.len() {
        let (a, b, c) = (shared.set(i), fresh.set(i), plain.set(i));
        assert!(a == b, "{name}: goal {i} reads {} with the cache and {} without: {a:?} vs {b:?}", a.len(), b.len());
        if fresh.answers[i] == Answer::Yes {
            assert!(b == c, "{name}: goal {i} reads {} with the summaries and {} without: {b:?} vs {c:?}", b.len(), c.len());
        } else {
            let missing: Vec<&Read> = c.iter().filter(|r| !b.contains(r)).collect();
            assert!(missing.is_empty(), "{name}: goal {i} ({:?}) misses reads with the summaries: {missing:?}", fresh.answers[i]);
        }
    }
}

#[test]
fn cost_reads_equal_without_cache_nested() {
    same_reads("nested", &nested_text());
}

#[test]
fn cost_reads_equal_without_cache_chain() {
    same_reads("chain", &chain_text());
}

/// Goals where a summary must NOT see through a type, mixed answers: an inner ADT with a
/// negative impl, one whose explicit impl's where-clause fails, a raw pointer, PhantomData of
/// a `!Send`, a self-recursive ADT, a local auto trait with a negative impl two levels down,
/// a where-clause of the trait in scope. Answers and reads must equal those with no summary.
fn controls_text() -> String {
    let mut s = String::new();
    s.push_str("struct L<T>(Option<Box<L<T>>>, T)\n");
    s.push_str("struct W1<T>(Option<T>, (u8, T), [T; 2])\n");
    s.push_str("struct W2<T>(W1<T>, Box<W1<T>>, &'static W1<T>)\n");
    s.push_str("struct C1<T>(Cell<T>, u8)\nstruct C2<T>(Option<C1<T>>)\n");
    s.push_str("struct P1<T>(*const T)\nstruct Ph<T>(PhantomData<T>, u8)\n");
    s.push_str("auto trait Mark\nstruct In(u8)\nimpl !Mark for In\nstruct O1<T>(T, In)\nstruct O2<T>(Option<O1<T>>, T)\n");
    s.push_str("case controls = fixture (cost.rs)\nparams T\nenv T: Send\n");
    for g in [
        "W2<u8>: Send",
        "W2<Rc<u8>>: Send",
        "W2<Rc<u8>>: Sync",
        "W2<String>: Sync",
        "W2<L<u8>>: Send",
        "W2<L<Rc<u8>>>: Send",
        "C2<u8>: Send",
        "C2<u8>: Sync",
        "C2<Rc<u8>>: Send",
        "P1<u8>: Send",
        "P1<Rc<u8>>: Send",
        "Ph<Rc<u8>>: Send",
        "Ph<u8>: Sync",
        "O2<u8>: Mark",
        "O2<u16>: Send",
        "W2<T>: Send",
        "W2<T>: Sync",
    ] {
        s.push_str(&format!("goal {g} => yes\n"));
    }
    s
}

#[test]
fn cost_summary_controls_equal_without_summary() {
    same_reads("controls", &controls_text());
}

/// 40 impls of one trait whose headers differ only at depth four (`Box<Box<Box<Kj>>>`), below
/// what the fingerprint sees (an argument's head and its first children's): every header
/// survives the prefilter, and before the pre-match each was imported and related. With
/// `controls`, goals the pre-match must let through: an inference variable at the depth the
/// impls differ (ambiguous), an impl parameter there, and a repeated impl parameter whose two
/// uses disagree (the relation refuses it, not the pre-match).
fn deep_text(controls: bool) -> String {
    let mut s = String::new();
    s.push_str("trait Tr\n");
    for j in 0..40 {
        s.push_str(&format!("struct D{j}(u8)\nimpl Tr for Box<Box<Box<D{j}>>>\n"));
    }
    s.push_str("trait Tw<A>\nimpl<T> Tw<T> for Box<Box<Box<T>>>\nimpl<T> Tw<u8> for (Box<T>, Box<T>)\n");
    s.push_str("case deep = fixture (cost.rs)\n");
    for j in 0..40 {
        s.push_str(&format!("goal Box<Box<Box<D{j}>>>: Tr => yes\n"));
    }
    if controls {
        s.push_str("goal Box<Box<Box<?X>>>: Tr => yes\n");
        s.push_str("goal Box<Box<Box<u16>>>: Tr => yes\n");
        s.push_str("goal Box<Box<Box<D3>>>: Tw<D3> => yes\n");
        s.push_str("goal Box<Box<Box<D3>>>: Tw<D4> => yes\n");
        s.push_str("goal (Box<D1>, Box<D1>): Tw<u8> => yes\n");
        s.push_str("goal (Box<D1>, Box<D2>): Tw<u8> => yes\n");
    }
    s
}

#[test]
fn cost_prematch_controls_equal_without_prematch() {
    same_reads("deep", &deep_text(true));
}

/// The read log's segments against a plain model: a push into the open segment keeps it a set,
/// a close folds a finished segment into its parent's as a set union, a truncate drops a
/// suffix. Driven by a seeded generator over a few ids, so ids repeat across segments.
#[test]
fn cost_seglog_matches_model() {
    use crate::readlog::SegLog;
    let alloc = blaze_own::testing::heap().alloc();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = |n: u64| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % n
    };
    for _ in 0..200 {
        let mut log = SegLog::new_in(alloc);
        // The model: the log as a plain list, and the open segments' starts.
        let mut model: Vec<u32> = Vec::new();
        let mut opens: Vec<usize> = Vec::new();
        opens.push(0);
        for _ in 0..300 {
            let lo = *opens.last().unwrap_or(&0);
            match next(10) {
                0..=5 => {
                    let id = next(12) as u32;
                    if let Err(e) = log.push(lo as u32, id) {
                        panic!("push: {e:?}");
                    }
                    if !model[lo..].contains(&id) {
                        model.push(id);
                    }
                }
                6 | 7 => opens.push(model.len()),
                8 if opens.len() > 1 => {
                    let child = opens.pop().unwrap_or(0);
                    let parent = *opens.last().unwrap_or(&0);
                    log.close(parent as u32, child as u32);
                    let tail: Vec<u32> = model.split_off(child);
                    for id in tail {
                        if !model[parent..].contains(&id) {
                            model.push(id);
                        }
                    }
                }
                _ => {
                    // A rerun: the open segment starts over.
                    log.truncate(lo);
                    model.truncate(lo);
                }
            }
            assert_eq!(log.from(0), model.as_slice(), "the log left the model");
        }
    }
}

/// Six levels of generic structs, eight fields each; `K0..K99` the outer types.
pub(crate) fn nested_text() -> String {
    let mut s = String::new();
    s.push_str("struct N0<T>(T, u8, u16, u32, u64, String, Vec<T>, Option<T>)\n");
    for i in 1..6 {
        let n = format!("N{}<T>", i - 1);
        s.push_str(&format!(
            "struct N{i}<T>({n}, Option<{n}>, Box<{n}>, Vec<{n}>, ({n}, u8), (u16, {n}), Option<Box<{n}>>, T)\n"
        ));
    }
    for i in 0..6 {
        s.push_str(&format!("impl<T: Clone> Clone for N{i}<T>\n"));
    }
    for k in 0..100 {
        s.push_str(&format!("struct K{k}(u8)\nimpl Clone for K{k}\n"));
    }
    s.push_str("case nested = fixture (cost.rs)\n");
    for k in 0..100 {
        s.push_str(&format!("goal N5<K{k}>: Send => yes\n"));
        s.push_str(&format!("goal (N5<u8>, K{k}): Send => yes\n"));
        s.push_str(&format!("goal N5<K{k}>: Sized => yes\n"));
        s.push_str(&format!("goal N5<K{k}>: Clone => yes\n"));
    }
    s
}

/// The goals of 20 method chains `xs.iter().map(f).filter(p).collect::<Vec<_>>()`.
pub(crate) fn chain_text() -> String {
    let mut s = String::new();
    s.push_str("struct Iter<T>(T)\nstruct Map<I, F>(I, F)\nstruct Filter<I, P>(I, P)\n");
    s.push_str("impl<T> Iterator for Iter<T> { type Item = T; }\n");
    s.push_str("impl<I: Iterator, F: FnMut<(<I as Iterator>::Item,)>> Iterator for Map<I, F> { type Item = <F as FnOnce<(<I as Iterator>::Item,)>>::Output; }\n");
    s.push_str("impl<I: Iterator, P: FnMut<(<I as Iterator>::Item,)>> Iterator for Filter<I, P> { type Item = <I as Iterator>::Item; }\n");
    s.push_str("trait FromIterator<A>\nimpl<T> FromIterator<T> for Vec<T>\n");
    for j in 0..30 {
        s.push_str(&format!("struct F{j}<T>(T)\nimpl<T> Iterator for F{j}<T> {{ type Item = T; }}\n"));
    }
    s.push_str("case chain = fixture (cost.rs)\n");
    let elems = ["u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "String", "char"];
    for c in 0..20 {
        let e = elems[c % elems.len()];
        let e2 = elems[(c + 3) % elems.len()];
        let f = format!("closure#{}<fnmut, fn(({e},)) -> {e2}, ()>", 2 * c);
        let p = format!("closure#{}<fnmut, fn(({e2},)) -> u8, ()>", 2 * c + 1);
        let m = format!("Map<Iter<{e}>, {f}>");
        let fl = format!("Filter<{m}, {p}>");
        s.push_str(&format!("goal Iter<{e}>: Iterator => yes\n"));
        s.push_str(&format!("goal {m}: Iterator => yes\n"));
        s.push_str(&format!("goal <{m} as Iterator>::Item == ?A{c} => yes\n"));
        s.push_str(&format!("goal {fl}: Iterator => yes\n"));
        s.push_str(&format!("goal <{fl} as Iterator>::Item == ?B{c} => yes\n"));
        s.push_str(&format!("goal Vec<{e2}>: FromIterator<{e2}> => yes\n"));
    }
    s
}

fn show(name: &str, r: &Run) -> String {
    let s = r.stats;
    format!(
        "{name}: goals {} evaluations {} distinct {} frames {} cache_hits {} fast_path {} candidates {} impl_candidates {} terms_imported {} adt_fields {} read_sorts {} reads_sorted {} reads_folded {} auto_summaries {} summary_fields {} impl_header_mismatch {} impl_prematch_rejected {} reads_out {}",
        r.answers.len(),
        s.evaluations,
        s.distinct_goals,
        s.frames,
        s.cache_hits,
        s.fast_path,
        s.candidates,
        s.impl_candidates,
        s.terms_imported,
        s.adt_fields,
        s.read_sorts,
        s.reads_sorted,
        s.reads_folded,
        s.auto_summaries,
        s.summary_fields,
        s.impl_header_mismatch,
        s.impl_prematch_rejected,
        r.reads.len()
    )
}

fn all_yes(name: &str, r: &Run) {
    for (i, a) in r.answers.iter().enumerate() {
        assert_eq!(*a, Answer::Yes, "{name}: goal {i} answers {a:?} ({})", show(name, r));
    }
}

/// The counters of both fixtures, in one line each (the cost report the lane quotes).
#[test]
fn cost_fixture_counters() {
    let n = match run(&nested_text()) {
        Ok(r) => r,
        Err(e) => panic!("nested: {e}"),
    };
    let c = match run(&chain_text()) {
        Ok(r) => r,
        Err(e) => panic!("chain: {e}"),
    };
    let d = match run(&deep_text(false)) {
        Ok(r) => r,
        Err(e) => panic!("deep: {e}"),
    };
    let d0 = match run_with(&deep_text(false), false, Off { summaries: false, prematch: true, memo: false }, Feed::Whole) {
        Ok(r) => r,
        Err(e) => panic!("deep without the pre-match: {e}"),
    };
    all_yes("nested", &n);
    all_yes("chain", &c);
    all_yes("deep", &d);
    let line = format!(
        "COUNTERS\n{}\n{}\n{}\n{}",
        show("nested", &n),
        show("chain", &c),
        show("deep (no pre-match)", &d0),
        show("deep", &d)
    );
    // Ceilings, lowered by each step that lowers a counter (a rise fails here). Before the
    // lane (green bb4733274b): nested frames 4444, terms_imported 16154, adt_fields 7171,
    // read_sorts 4444 holding 195843 reads; chain frames 361, terms_imported 1083, read_sorts
    // 361 holding 12493. The read log (step 1): no sort, 82579 and 4453 ids folded. The
    // auto-trait summaries (step 3): nested frames 703, terms_imported 700, adt_fields 0,
    // 110 summaries (one per ADT and trait: N0..N5, K0..K99 and the prelude's).
    let ceil = |name: &str, got: u64, max: u64| assert!(got <= max, "{name}: {got} over its ceiling {max}\n{line}");
    ceil("nested frames", n.stats.frames, 703);
    ceil("nested terms_imported", n.stats.terms_imported, 700);
    ceil("nested adt_fields", n.stats.adt_fields, 0);
    ceil("nested reads_sorted", n.stats.reads_sorted, 0);
    ceil("nested reads_folded", n.stats.reads_folded, 1433);
    ceil("nested auto_summaries", n.stats.auto_summaries, 110);
    ceil("chain frames", c.stats.frames, 361);
    ceil("chain terms_imported", c.stats.terms_imported, 1083);
    ceil("chain reads_sorted", c.stats.reads_sorted, 0);
    ceil("chain reads_folded", c.stats.reads_folded, 4453);
    // Each distinct goal is proven by one frame: a second frame for one goal is the cache
    // failing.
    ceil("nested frames over distinct goals", n.stats.frames, n.stats.distinct_goals);
    ceil("chain frames over distinct goals", c.stats.frames, c.stats.distinct_goals);
    // The header pre-match (step 4): without it, deep imports 6400 terms and relates 1560
    // headers that fail; with it, 160 terms, and the 1560 are refused in place.
    ceil("deep (no pre-match) impl_header_mismatch", d0.stats.impl_header_mismatch, 1560);
    ceil("deep terms_imported", d.stats.terms_imported, 160);
    ceil("deep impl_header_mismatch", d.stats.impl_header_mismatch, 0);
    assert_eq!(d.stats.impl_prematch_rejected, 1560, "deep: the pre-match refused other than the 1560 failing headers\n{line}");
}

/// The pull against the stop (rule 78), over the world that lacks every fact at its first ask
/// ([`Shy`]): each goal's answer and read set equal the whole world's under both; the stop
/// path did stop (the control: without the shy world nothing is pulled or stopped for), and
/// the pull ran every frame once, as many as the whole world's run, none abandoned.
fn pull_equals_stop(name: &str, text: &str) {
    let get = |feed: Feed| match run_with(text, false, ON, feed) {
        Ok(r) => r,
        Err(e) => panic!("{name} ({feed:?}): {e}"),
    };
    let (whole, stop, pull) = (get(Feed::Whole), get(Feed::ShyStop), get(Feed::ShyPull));
    let line = format!(
        "{name}: frames whole {} stop {} pull {}; need_stops stop {} pull {}; abandoned stop {} pull {}; pulls {}",
        whole.stats.frames,
        stop.stats.frames,
        pull.stats.frames,
        stop.stats.need_stops,
        pull.stats.need_stops,
        stop.stats.frames_abandoned,
        pull.stats.frames_abandoned,
        pull.stats.pulls
    );
    std::println!("PULL {line}");
    assert_eq!(whole.answers, stop.answers, "{line}: answers differ with the stop");
    assert_eq!(whole.answers, pull.answers, "{line}: answers differ with the pull");
    for i in 0..whole.answers.len() {
        let (a, b, c) = (whole.set(i), stop.set(i), pull.set(i));
        assert!(a == b, "{line}: goal {i} reads {a:?} with the whole world and {b:?} with the stop");
        assert!(a == c, "{line}: goal {i} reads {a:?} with the whole world and {c:?} with the pull");
    }
    assert_eq!((whole.stats.need_stops, whole.stats.pulls), (0, 0), "{line}: the whole world stopped or pulled");
    assert!(stop.stats.need_stops > 0, "{line}: the shy world never stopped the stop path");
    assert!(pull.stats.pulls > 0, "{line}: the shy world never pulled");
    assert_eq!((pull.stats.need_stops, pull.stats.frames_abandoned), (0, 0), "{line}: the pull stopped");
    assert_eq!(pull.stats.frames, whole.stats.frames, "{line}: the pull ran a frame twice");
}

/// The plain-goal memo against the canonical walk (eval.rs `Solver::input_of`, rule 78): equal
/// answers and read sets, each goal asked either walked or skipped (the two sum to the walks
/// without the memo), and a repeated plain goal costs no walk.
fn memo_equals_walk(name: &str, text: &str) {
    const NO_MEMO: Off = Off { summaries: false, prematch: false, memo: true };
    for feed in [Feed::Whole, Feed::ShyPull] {
        let get = |off: Off| match run_with(text, false, off, feed) {
            Ok(r) => r,
            Err(e) => panic!("{name} (off {off:?}): {e}"),
        };
        let (memo, walk) = (get(ON), get(NO_MEMO));
        let line = format!(
            "{name}: walked {} skipped {} with the memo, walked {} without",
            memo.stats.canonicalized, memo.stats.canon_skipped, walk.stats.canonicalized
        );
        std::println!("MEMO {line}");
        assert_eq!(memo.answers, walk.answers, "{line}: answers differ with the memo");
        for i in 0..memo.answers.len() {
            let (a, b) = (memo.set(i), walk.set(i));
            assert!(a == b, "{line}: goal {i} reads {a:?} with the memo and {b:?} without");
        }
        assert_eq!(walk.stats.canon_skipped, 0, "{line}: the memo ran while off");
        assert_eq!(memo.stats.canonicalized + memo.stats.canon_skipped, walk.stats.canonicalized, "{line}: goals asked differ");
        assert!(memo.stats.canon_skipped > 0, "{line}: no goal was found by its plain key");
        assert_eq!(memo.stats.frames, walk.stats.frames, "{line}: frames differ with the memo");
    }
}

#[test]
fn memo_equals_walk_nested() {
    memo_equals_walk("nested", &nested_text());
}

#[test]
fn memo_equals_walk_chain() {
    memo_equals_walk("chain", &chain_text());
}

#[test]
fn memo_equals_walk_deep() {
    memo_equals_walk("deep", &deep_text(true));
}

#[test]
fn memo_equals_walk_controls() {
    memo_equals_walk("controls", &controls_text());
}

#[test]
fn pull_equals_stop_nested() {
    pull_equals_stop("nested", &nested_text());
}

#[test]
fn pull_equals_stop_chain() {
    pull_equals_stop("chain", &chain_text());
}

#[test]
fn pull_equals_stop_deep() {
    pull_equals_stop("deep", &deep_text(true));
}

#[test]
fn pull_equals_stop_controls() {
    pull_equals_stop("controls", &controls_text());
}

/// Tempo's `get_block` (crates/consensus/src/follow/resolver/mod.rs, run a0612: `SolveError::
/// Steps` for `{coroutine}: Send`, the top goal `Send` of an alloy_consensus ADT at depth 30),
/// reduced. The Reference ("Auto traits"): an ADT is `Send` when its fields are; a cycle
/// through an ADT's own fields holds (coinduction). A wide subtree (`Wide`: 68 groups of 64
/// leaves, more than `MAX_NESTED` goals below it) under a ladder of 20 levels where each level
/// names the next level's two ADTs, so each ADT has two parents. The env names `Send` (`T:
/// Send`), so no auto-trait summary sees through a field: each field is a goal, as in tempo's
/// trait impl fns, whose env names `Send` through `Send + Sync` supertraits.
///
/// Before (eval.rs `finish` kept every goal below as a nested goal): `Wide`'s entry had more
/// than `MAX_NESTED`, so it and every entry above it applied only to an empty stack, and each
/// ladder ADT was solved again under each parent: 2^20 solves of `Wide`, `Steps`. Now a
/// cycle-free goal keeps no nested goals and its entry applies anywhere: each distinct goal is
/// solved once. TWINS, decided: `R` (recursive through `Option<Box<R>>`, over the ladder) is
/// `Send`; `Rn`, the same with an `Rc<u8>` field, is NOT `Send` (a real `!Send` field: a
/// decided no, never a limit).
fn wide_text() -> String {
    let mut s = String::new();
    for k in 0..68 * 64 {
        s.push_str(&format!("struct K{k}(u8)\n"));
    }
    for g in 0..68 {
        let fields: Vec<String> = (0..64).map(|i| format!("K{}", g * 64 + i)).collect();
        s.push_str(&format!("struct G{g}({})\n", fields.join(", ")));
    }
    let groups: Vec<String> = (0..68).map(|g| format!("G{g}")).collect();
    s.push_str(&format!("struct Wide({})\n", groups.join(", ")));
    s.push_str("struct A20(Wide, u8)\nstruct B20(Wide, u16)\n");
    for k in (0..20).rev() {
        s.push_str(&format!("struct A{k}(A{n}, B{n})\nstruct B{k}(B{n}, A{n})\n", n = k + 1));
    }
    s.push_str("struct R(Option<Box<R>>, A0)\n");
    s.push_str("struct Rn(Option<Box<Rn>>, A0, Rc<u8>)\n");
    s.push_str("case wide = fixture (cost.rs)\nparams T\nenv T: Send\n");
    s.push_str("goal A0: Send => yes\n");
    s.push_str("goal R: Send => yes\n");
    s.push_str("goal Rn: Send => no\n");
    s
}

/// On failure panics naming the counters (or the solver's stop: before, `Steps`).
#[test]
fn cost_shared_wide_subtree_is_solved_once() {
    let text = wide_text();
    let r = match run(&text) {
        Ok(r) => r,
        Err(e) => panic!("wide: {e}"),
    };
    let line = show("wide", &r);
    std::println!("WIDE {line}");
    assert_eq!(r.answers, [Answer::Yes, Answer::Yes, Answer::No], "wide: `A0` and `R` are Send, `Rn` (an `Rc<u8>` field) is not\n{line}");
    // Each distinct goal is solved once, but `Rn`'s cycle reruns once (its first answer `no`
    // differs from the coinductive start `yes`): its members are solved again, a handful.
    assert!(r.stats.frames <= r.stats.distinct_goals + 8, "wide: a goal was solved more than once\n{line}");
    // The same answers and read sets with a fresh solver per goal and with every shortcut off.
    same_reads("wide", &text);
}
