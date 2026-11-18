//! The goals shared by a request's bodies (share.rs): every goal of a fixture is asked by its
//! own BODY (a fresh solver, a fresh arena, its case's environment), all bodies over one table,
//! and the answers and read sets are compared with the same bodies over no table.

use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::sync::Mutex;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::assembly::{Clause, Goal, Via};
use crate::eval::{Answer, Solver, Stats};
use crate::facts::{Question, Read, Source};
use crate::share::{AuditMiss, GoalKey, GoalShare, ShareError, ShareOpts, ShareRefused, SharedAns, SharedGoal};
use crate::term::{Certainty, TypingMode};
use crate::testing::parse::{CheckA, DeclA, parse_file};
use crate::testing::run::PRELUDE;
use crate::testing::tys::TestTys;
use crate::testing::world::{Arena, Scope, World};
use crate::tys::Tys;
use crate::{SolveError, Stop};

struct Row {
    words: Vec<u32>,
    ans: Option<(Certainty, Via, Vec<u32>)>,
    required_depth: u32,
    overflow: bool,
    reads: Vec<Read>,
    reads_digest: u64,
}

/// The test's table: a map under a lock (the ladder's is lock-free; this one is the oracle's).
struct Table {
    opts: ShareOpts,
    rows: Mutex<BTreeMap<u128, Row>>,
}

impl Table {
    fn new(opts: ShareOpts) -> Table {
        Table { opts, rows: Mutex::new(BTreeMap::new()) }
    }

    fn len(&self) -> usize {
        self.rows.lock().map(|r| r.len()).unwrap_or(usize::MAX)
    }
}

impl GoalShare for Table {
    fn opts(&self) -> ShareOpts {
        self.opts
    }

    fn get(&self, key: GoalKey<'_>, see: &mut dyn FnMut(SharedGoal<'_>)) -> Result<bool, ShareRefused> {
        let rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        let Some(r) = rows.get(&key.digest) else { return Ok(false) };
        if r.words != key.words {
            return Err(ShareRefused::Collision { digest: key.digest });
        }
        let ans = r.ans.as_ref().map(|(c, v, w)| SharedAns { certainty: *c, via: *v, words: w });
        see(SharedGoal { ans, required_depth: r.required_depth, overflow: r.overflow, reads: &r.reads, reads_digest: r.reads_digest });
        Ok(true)
    }

    fn put(&self, key: GoalKey<'_>, g: SharedGoal<'_>) -> Result<bool, ShareRefused> {
        let mut rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = rows.get(&key.digest) {
            return if r.words == key.words { Ok(false) } else { Err(ShareRefused::Collision { digest: key.digest }) };
        }
        let row = Row {
            words: key.words.to_vec(),
            ans: g.ans.map(|a| (a.certainty, a.via, a.words.to_vec())),
            required_depth: g.required_depth,
            overflow: g.overflow,
            reads: g.reads.to_vec(),
            reads_digest: g.reads_digest,
        };
        rows.insert(key.digest, row);
        Ok(true)
    }
}

/// A body's source: the world's facts, and the request's table when there is one.
struct Body<'f> {
    world: &'f World,
    table: Option<&'f Table>,
}

impl<T: Tys> Source<T> for Body<'_> {
    type Facts = World;
    const PULLS: bool = false;
    fn facts(&self) -> &World {
        self.world
    }
    fn fill(&mut self, _tys: &mut T, _q: Question) -> bool {
        false
    }
    fn goals(&self) -> Option<&dyn GoalShare> {
        self.table.map(|t| t as &dyn GoalShare)
    }
}

/// One body's result.
#[derive(Debug)]
struct Out {
    line: u32,
    answer: Answer,
    reads: Vec<Read>,
    stats: Stats,
    hits: usize,
}

fn world_of(text: &str) -> Result<(World, crate::testing::parse::FileA), String> {
    let prelude = parse_file(PRELUDE).map_err(|e| format!("the prelude: {e}"))?;
    let f = parse_file(text).map_err(|e| format!("the fixture: {e}"))?;
    let mut decls: Vec<(u32, &DeclA)> = Vec::new();
    decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
    decls.extend(f.decls.iter().map(|(l, d)| (*l, d)));
    for c in &f.cases {
        decls.extend(c.decls.iter().map(|(l, d)| (*l, d)));
    }
    let world = World::build(&decls).map_err(|e| format!("the world: {e}"))?;
    Ok((world, f))
}

/// Every goal of every case of `text`, each its own body, in `order` (indices into the goals
/// as written), over `table`.
fn bodies(world: &World, f: &crate::testing::parse::FileA, table: Option<&Table>, reversed: bool) -> Result<Vec<Out>, String> {
    let mut asks = Vec::new();
    for case in &f.cases {
        for g in &case.goals {
            asks.push((case, g));
        }
    }
    if reversed {
        asks.reverse();
    }
    let alloc = blaze_own::testing::heap().alloc();
    let mut outs = Vec::new();
    for (case, g) in asks {
        let CheckA::Goal(p) = &g.check else { return Err(format!("line {}: not a goal", g.line)) };
        let mut tys = TestTys::new();
        let mut vars = std::collections::HashMap::new();
        let mut solver = Solver::new_in(alloc);
        let sc = Scope { params: &case.params, bound: &[] };
        let mut env_cl: Vec<Clause> = Vec::new();
        let mut cl: Vec<Clause> = Vec::new();
        {
            let mut a = Arena { tys: &mut tys, vars: &mut vars };
            for e in &case.env {
                world.names.pred(&mut a, sc, case.line, e, &mut env_cl).map_err(|e| format!("the env: {e}"))?;
            }
            let sized: Vec<bool> = case.params.iter().map(|_| false).collect();
            world.names.case_sized(&mut a, case.line, &sized, &mut env_cl).map_err(|e| format!("the env: {e}"))?;
            world.names.pred(&mut a, sc, g.line, p, &mut cl).map_err(|e| format!("line {}: {e}", g.line))?;
        }
        let mut scratch: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        let env = solver.env(&mut tys, world, &env_cl, &mut scratch).map_err(|s| format!("the env: {s:?}"))?;
        let [c]: [Clause; 1] = cl.try_into().map_err(|_| format!("line {}: a goal is one predicate", g.line))?;
        let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        let mut src = Body { world, table };
        let answer = solver
            .evaluate_from(&mut tys, &mut src, TypingMode::Analysis, Goal { env, pred: c.pred, bound: c.bound }, &mut reads)
            .map_err(|s| format!("line {}: the solver stopped: {s:?}", g.line))?;
        let mut rs: Vec<Read> = reads.iter().copied().collect();
        rs.sort_unstable();
        rs.dedup();
        outs.push(Out { line: g.line, answer, reads: rs, stats: solver.stats(), hits: solver.goal_hits().len() });
    }
    outs.sort_by_key(|o| o.line);
    Ok(outs)
}

fn run(text: &str, table: Option<&Table>, reversed: bool) -> Vec<Out> {
    let (world, f) = world_of(text).unwrap_or_else(|e| panic!("{e}"));
    bodies(&world, &f, table, reversed).unwrap_or_else(|e| panic!("{e}"))
}

fn sum(outs: &[Out], f: impl Fn(&Stats) -> u64) -> u64 {
    outs.iter().map(|o| f(&o.stats)).sum()
}

/// Answers and read sets of `got` equal those of the bodies over no table.
fn assert_same(name: &str, plain: &[Out], got: &[Out]) {
    assert_eq!(plain.len(), got.len(), "{name}: bodies");
    for (p, g) in plain.iter().zip(got) {
        assert_eq!(p.answer, g.answer, "{name}: line {}: the answer", p.line);
        assert_eq!(p.reads, g.reads, "{name}: line {}: the read set", p.line);
    }
}

const TWO_BODIES: &str = "case two = shared (share.rs)\ngoal Vec<u8>: Clone => yes\ngoal Vec<u8>: Clone => yes\n";

/// (a) Two bodies asking `Vec<u8>: Clone`: solved once, hit once, answers and read sets those
/// of the bodies alone.
#[test]
fn share_two_bodies_solve_once() {
    let plain = run(TWO_BODIES, None, false);
    let table = Table::new(ShareOpts::default());
    let got = run(TWO_BODIES, Some(&table), false);
    assert_same("two", &plain, &got);
    assert_eq!(sum(&got, |s| s.shared_hits), 1, "{got:?}");
    assert_eq!(got[0].hits + got[1].hits, 1, "{got:?}");
    assert!(sum(&got, |s| s.shared_puts) >= 1, "{got:?}");
    assert_eq!(got[1].stats.frames, 0, "the second body solved nothing: {:?}", got[1].stats);
    assert!(got[0].stats.frames > 0, "{:?}", got[0].stats);
    assert_eq!(table.len() as u64, sum(&got, |s| s.shared_puts));
    assert_eq!(sum(&got, |s| s.shared_refused_scope), 0);
}

const ENVS: &str = "case with = shared (share.rs)\nparams T\nenv T: Clone\ngoal Vec<T>: Clone => yes\n\
case without = shared (share.rs)\nparams T\ngoal Vec<T>: Clone => no\n";

/// (b) A goal under the where-clause `T: Clone` is not the goal without it: no hit either way
/// round, and each answer is its own.
#[test]
fn share_env_is_in_the_key() {
    for reversed in [false, true] {
        let plain = run(ENVS, None, reversed);
        let table = Table::new(ShareOpts::default());
        let got = run(ENVS, Some(&table), reversed);
        assert_same("envs", &plain, &got);
        assert_eq!(sum(&got, |s| s.shared_hits), 0, "reversed {reversed}: {got:?}");
        assert_ne!(got[0].answer, got[1].answer, "{got:?}");
    }
}

const CYCLE: &str = "struct List(Option<Box<List>>, u8)\ncase cycle = shared (share.rs)\n\
goal List: Send => yes\ngoal Option<Box<List>>: Send => yes\ngoal Box<List>: Send => yes\ngoal List: Send => yes\n";

/// (c) A cycle `List: Send` through `Option<Box<List>>`: only goals solved as a cycle's head
/// are put (each body's root), so the members entered first by another body are solved there,
/// and every body's answer and read set equal those of the body alone, in either order.
#[test]
fn share_cycle_puts_only_its_head() {
    for reversed in [false, true] {
        let plain = run(CYCLE, None, reversed);
        let table = Table::new(ShareOpts::default());
        let got = run(CYCLE, Some(&table), reversed);
        assert_same("cycle", &plain, &got);
        // The second `List: Send` is a hit whichever order; the cycle's other members are hits
        // only where an earlier body solved them as its own root.
        let hits = sum(&got, |s| s.shared_hits);
        assert!(hits >= 1, "reversed {reversed}: {got:?}");
        let audit = Table::new(ShareOpts { audit: true, ..ShareOpts::default() });
        let audited = run(CYCLE, Some(&audit), reversed);
        assert_same("cycle audited", &plain, &audited);
        assert_eq!(sum(&audited, |s| s.shared_audited), hits);
    }
}

/// The audit solves every hit again and compares: it passes on the cost fixture's goals, each
/// its own body, and the answers and read sets are those of the bodies alone.
#[test]
fn share_audit_passes_on_nested() {
    // The fixture's 400 goals are 400 DISTINCT roots, and the table is looked up only for a
    // root goal: one pass over them can hit nothing. So the goals are asked by two rounds of
    // bodies over one table, the first forward (it puts), the second reversed (it takes).
    let text = super::cost::nested_text();
    let plain = run(&text, None, false);
    let n = plain.len() as u64;
    for audit in [false, true] {
        let table = Table::new(ShareOpts { audit, ..ShareOpts::default() });
        let first = run(&text, Some(&table), false);
        let second = run(&text, Some(&table), true);
        let said = format!("audit {audit}: first {}; second {}", counters(&first), counters(&second));
        assert_same("nested first", &plain, &first);
        assert_same("nested second", &plain, &second);
        // Every root the second round does not answer on its ground fast path is a hit.
        assert_eq!(sum(&second, |s| s.shared_hits) + sum(&second, |s| s.fast_path), n, "{said}");
        assert!(sum(&second, |s| s.shared_hits) > 0, "nothing shared: {said}");
        if audit {
            assert_eq!(sum(&second, |s| s.shared_audited), sum(&second, |s| s.shared_hits), "{said}");
        } else {
            assert!(sum(&second, |s| s.frames) < sum(&plain, |s| s.frames), "sharing solved no less: {said}");
        }
    }
}

/// Every sharing counter of `outs`, summed: what a failure says.
fn counters(outs: &[Out]) -> String {
    format!(
        "hits {} puts {} present {} audited {} fast_path {} frames {} refused {} (vars {} local {} const_holds {} overflow {} scope {} full {})",
        sum(outs, |s| s.shared_hits),
        sum(outs, |s| s.shared_puts),
        sum(outs, |s| s.shared_present),
        sum(outs, |s| s.shared_audited),
        sum(outs, |s| s.fast_path),
        sum(outs, |s| s.frames),
        sum(outs, |s| s.shared_refused),
        sum(outs, |s| s.shared_refused_vars),
        sum(outs, |s| s.shared_refused_local),
        sum(outs, |s| s.shared_refused_const_holds),
        sum(outs, |s| s.shared_refused_overflow),
        sum(outs, |s| s.shared_refused_scope),
        sum(outs, |s| s.shared_refused_full),
    )
}

/// (e) THE PLANTED BUG, a hit without its read: the audit names it, and without the audit the
/// read sets differ from the bodies alone.
#[test]
fn share_plant_hit_without_read_is_caught() {
    let plant = ShareOpts { plant_hit_without_read: true, ..ShareOpts::default() };
    let (world, f) = world_of(TWO_BODIES).unwrap_or_else(|e| panic!("{e}"));
    let audit = Table::new(ShareOpts { audit: true, ..plant });
    match bodies(&world, &f, Some(&audit), false) {
        Err(e) => assert!(e.contains(&format!("{:?}", AuditMiss::HitReads)), "{e}"),
        Ok(o) => panic!("the audit missed the planted hit: {o:?}"),
    }
    let plain = run(TWO_BODIES, None, false);
    let quiet = Table::new(plant);
    let got = run(TWO_BODIES, Some(&quiet), false);
    assert_eq!(got[1].answer, plain[1].answer);
    assert!(got[1].reads.is_empty() && !plain[1].reads.is_empty(), "the plant recorded reads: {got:?}");
}

/// THE SCOPE: an entry put in a world with `impl Clone for K` is not taken by a body whose
/// facts lack the impl (its read of `Clone`'s impls answers another digest).
#[test]
fn share_scope_is_checked_by_reads() {
    let with = "struct K(u8)\nimpl Clone for K\ncase k = shared (share.rs)\ngoal Vec<K>: Clone => yes\n";
    let without = "struct K(u8)\ncase k = shared (share.rs)\ngoal Vec<K>: Clone => no\n";
    let table = Table::new(ShareOpts::default());
    let a = run(with, Some(&table), false);
    let b = run(without, Some(&table), false);
    let b_plain = run(without, None, false);
    assert_same("scope", &b_plain, &b);
    assert_ne!(a[0].answer, b[0].answer);
    assert_eq!(sum(&b, |s| s.shared_hits), 0, "{b:?}");
    assert_eq!(sum(&b, |s| s.shared_refused_scope), 1, "{b:?}");
}

/// A collision (two goals' words under one digest) is a typed refusal, never an answer.
#[test]
fn share_collision_is_refused() {
    let table = Table::new(ShareOpts::default());
    let (world, f) = world_of(TWO_BODIES).unwrap_or_else(|e| panic!("{e}"));
    let first = bodies(&world, &f, Some(&table), false).unwrap_or_else(|e| panic!("{e}"));
    assert!(!first.is_empty());
    {
        let mut rows = table.rows.lock().unwrap_or_else(|p| p.into_inner());
        for r in rows.values_mut() {
            r.words.push(7);
        }
    }
    let e = bodies(&world, &f, Some(&table), false).err().unwrap_or_default();
    let want = format!("{:?}", Stop::Error(SolveError::Share(ShareError::Collision { digest: 0 })));
    let want = &want[..want.find("digest").unwrap_or(want.len())];
    assert!(e.contains(want), "{e}");
}
