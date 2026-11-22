//! A global cache hit on an overflow entry marks its parent as overflowed.
//!
//! Rust's rule, as the pinned nightly's next solver states it (search_graph/mod.rs): an answer
//! reached by overflow holds only at the depth it was computed at (`GlobalCache::get`, the
//! `with_overflow` entries keyed by `available_depth`), and a goal that used one "encountered
//! overflow" itself (`lookup_global_cache` -> `update_parent_goal`: `parent.encountered_overflow
//! |= encountered_overflow`), so its own entry is again an overflow entry at its depth.
//!
//! The cause (eval.rs `start_goal`, the global cache branch): the hit took the entry's answer and
//! required depth but not its overflow, so the parent was filed as a success, applicable at
//! every deeper limit: an overflow answer (ambiguous) reused where the goal holds.
//!
//! The goal: `Wrap<Nest<Nest<Nest<Nest<u8>>>>>: Deep` holds (four `Nest` impls and `Wrap`'s over
//! `u8`'s), but needs more depth than a limit of 3 gives.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::assembly::{Clause, Goal};
use crate::eval::{Answer, Solver};
use crate::facts::Read;
use crate::term::TypingMode;
use crate::testing::parse::{CheckA, DeclA, parse_file};
use crate::testing::run::PRELUDE;
use crate::testing::tys::TestTys;
use crate::testing::world::{Arena, Scope, World};

const FIXTURE: &str = "trait Deep
struct Nest<T>(T)
struct Wrap<T>(T)
impl Deep for u8
impl<T> Deep for Nest<T> where T: Deep
impl<T> Deep for Wrap<T> where T: Deep
case child = overflow_cache.rs
goal Nest<Nest<Nest<Nest<u8>>>>: Deep => overflow
case parent = overflow_cache.rs
goal Wrap<Nest<Nest<Nest<Nest<u8>>>>>: Deep => yes
";

/// Each `(case, limit)` of `asks` in turn on ONE solver (its global cache kept across them),
/// with its answer.
fn ask_in_order(asks: &[(&str, u32)]) -> Vec<Answer> {
    let r = (|| -> Result<Vec<Answer>, String> {
        let prelude = parse_file(PRELUDE).map_err(|e| format!("the prelude: {e}"))?;
        let f = parse_file(FIXTURE).map_err(|e| format!("the fixture: {e}"))?;
        let mut decls: Vec<(u32, &DeclA)> = Vec::new();
        decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
        decls.extend(f.decls.iter().map(|(l, d)| (*l, d)));
        for c in &f.cases {
            decls.extend(c.decls.iter().map(|(l, d)| (*l, d)));
        }
        let world = World::build(&decls).map_err(|e| format!("the world: {e}"))?;
        let alloc = blaze_own::testing::heap().alloc();
        let mut tys = TestTys::new();
        let mut vars = std::collections::HashMap::new();
        let mut solver = Solver::new_in(alloc);
        let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        let env = solver.env(&mut tys, &world, &[], &mut reads).map_err(|s| format!("the env: {s:?}"))?;
        let mut out = Vec::new();
        for &(name, limit) in asks {
            let case = f.cases.iter().find(|c| c.name == name).ok_or_else(|| format!("no case `{name}`"))?;
            let [g] = &case.goals[..] else { return Err(format!("`{name}`: one goal")) };
            let CheckA::Goal(p) = &g.check else { return Err(format!("line {}: not a goal", g.line)) };
            let sc = Scope { params: &case.params, bound: &[] };
            let mut cl: Vec<Clause> = Vec::new();
            {
                let mut a = Arena { tys: &mut tys, vars: &mut vars };
                world.names.pred(&mut a, sc, g.line, p, &mut cl).map_err(|e| format!("line {}: {e}", g.line))?;
            }
            let [c]: [Clause; 1] = cl.try_into().map_err(|_| format!("line {}: a goal is one predicate", g.line))?;
            solver.set_recursion_limit(limit);
            let a = solver
                .evaluate(&mut tys, &world, TypingMode::Analysis, Goal { env, pred: c.pred, bound: c.bound }, &mut reads)
                .map_err(|s| format!("`{name}` at limit {limit}: the solve stopped: {s:?}"))?;
            out.push(a);
        }
        Ok(out)
    })();
    r.unwrap_or_else(|e| panic!("{e}"))
}

fn overflowed(a: Answer) -> bool {
    matches!(a, Answer::Maybe(c) if c.is_overflow())
}

/// `child` at limit 2 overflows and is filed as an overflow entry at depth 2; `parent` at
/// limit 3 meets it at exactly that depth (a cache hit) and overflows too. At limit 64 the
/// parent holds: answered from its own cache entry, it must not be the overflow of the
/// shallower run. CONTROL (rule 23): on a fresh solver, the parent at 64 holds and at 3
/// overflows, so the answer depends on the depth and the first assertion is the cache's.
#[test]
fn a_parent_of_an_overflow_cache_hit_is_not_reused_deeper() {
    let [child, shallow, deep] = ask_in_order(&[("child", 2), ("parent", 3), ("parent", 64)])[..] else { panic!("three answers") };
    assert!(overflowed(child), "the child overflows at limit 2: {child:?}");
    assert!(overflowed(shallow), "the parent at limit 3 meets the child's overflow: {shallow:?}");
    assert_eq!(deep, Answer::Yes, "at limit 64 the parent holds; never the limit-3 overflow from the cache");

    let [fresh_deep] = ask_in_order(&[("parent", 64)])[..] else { panic!("one answer") };
    assert_eq!(fresh_deep, Answer::Yes, "CONTROL: a fresh solver at limit 64: the parent holds");
    let [fresh_shallow] = ask_in_order(&[("parent", 3)])[..] else { panic!("one answer") };
    assert!(overflowed(fresh_shallow), "CONTROL: a fresh solver at limit 3: the parent overflows: {fresh_shallow:?}");
}
