//! A projection whose trait's members are not known is a named gap, never an answer.
//!
//! Rust's rule (the Reference, "Associated Items", "Associated Types"): "The optional trait
//! bounds must be fulfilled by the implementations of the type alias", so a rigid projection
//! `<T as Tr>::A` meets every bound written on `A`, and no other (rustc's
//! `assemble_alias_bound_candidates`); an impl that does not define `A` takes the trait's
//! default, and with none the program is E0046 at the impl (`consider_impl_candidate`'s error
//! response). Both answers need `Tr`'s members: an `A: Clone` the facts never read cannot be
//! told from an `A` with no bounds. tempo a0700, crates/node/src/executed_state.rs:123: E0277
//! "is not a future" on `<BasicEngineValidatorBuilder<..> as EngineValidatorBuilder<N>>::{the
//! `impl Future` of `build`}`, printed "a trait whose members are not filed".
//!
//! The cause (alias.rs `bound_count` / `bound_pred`, structural.rs, `step_impl`): a member
//! missing from its trait's facts was `malformed()` (a stop naming nothing) at the bounds, and
//! THE ERROR TYPE at an impl without the item (an unknown answered as the program's E0046,
//! which silences every later use). Now each is `Unsupported::MemberNotFiled { trait_, item }`.
//!
//! Each case has its twin: the same goal with the member filed answers (yes by its bound; No
//! where its bounds are known and none matches, rustc's E0277).

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::assembly::{Clause, Goal};
use crate::error::{SolveError, Stop, Unsupported};
use crate::eval::{Answer, Solver};
use crate::facts::Read;
use crate::term::{Def, TypingMode};
use crate::testing::parse::{CheckA, DeclA, parse_file};
use crate::testing::run::PRELUDE;
use crate::testing::tys::TestTys;
use crate::testing::world::{Arena, Scope, World, def_of};

/// `Tr::A` has a written bound; `Bare::A` has none.
const TRAITS: &str = "trait Tr { type A: Clone; }\ntrait Bare { type A; }\n";

/// The answer of the one goal of `case` (its `params`/`env` lines, then `goal`), in a world
/// whose `forget` members (trait, item) are dropped from their traits' facts.
fn solve(case: &str, forget: &[(&str, &str)]) -> Result<Answer, Stop> {
    let text = format!("{TRAITS}case c = member_not_filed.rs\n{case}");
    let r = (|| -> Result<Result<Answer, Stop>, String> {
        let prelude = parse_file(PRELUDE).map_err(|e| format!("the prelude: {e}"))?;
        let f = parse_file(&text).map_err(|e| format!("the fixture: {e}"))?;
        let mut decls: Vec<(u32, &DeclA)> = Vec::new();
        decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
        decls.extend(f.decls.iter().map(|(l, d)| (*l, d)));
        for c in &f.cases {
            decls.extend(c.decls.iter().map(|(l, d)| (*l, d)));
        }
        let mut world = World::build(&decls).map_err(|e| format!("the world: {e}"))?;
        for &(trait_, item) in forget {
            let dropped = world.forget_member(def_of("trait", trait_), def_of("assoc", &format!("{trait_}::{item}")));
            if !dropped {
                return Err(format!("`{trait_}::{item}` is no member of `{trait_}` in the fixture"));
            }
        }
        let alloc = blaze_own::testing::heap().alloc();
        let [case] = &f.cases[..] else { return Err(format!("one case: {text}")) };
        let [g] = &case.goals[..] else { return Err(format!("one goal: {text}")) };
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
        let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        let env = solver.env(&mut tys, &world, &env_cl, &mut reads).map_err(|s| format!("the env: {s:?}"))?;
        let [c]: [Clause; 1] = cl.try_into().map_err(|_| format!("line {}: a goal is one predicate", g.line))?;
        Ok(solver.evaluate(&mut tys, &world, TypingMode::Analysis, Goal { env, pred: c.pred, bound: c.bound }, &mut reads))
    })();
    r.unwrap_or_else(|e| panic!("{e}"))
}

fn not_filed(trait_: &str, item: &str) -> Stop {
    let (trait_, item): (Def, Def) = (def_of("trait", trait_), def_of("assoc", &format!("{trait_}::{item}")));
    Stop::Error(SolveError::Unsupported(Unsupported::MemberNotFiled { trait_, item }))
}

/// `<T as Tr>::A: Clone` under `T: Tr`: rigid (the env proves the trait), so it holds by `A`'s
/// written bound. With `Tr`'s members not filed: the gap naming `Tr` and `A`, never No.
/// TWIN: `<T as Bare>::A: Clone`, every bound of `Bare::A` known and none `Clone`: No (rustc
/// E0277). CONTROL (rule 23): `Tr` filed, the same goal holds, so the gap is the missing
/// member's and not the goal's shape.
#[test]
fn a_rigid_projection_of_an_unfiled_member_is_a_named_gap() {
    let goal = "params T\nenv T: Tr\ngoal <T as Tr>::A: Clone => yes\n";
    assert_eq!(solve(goal, &[("Tr", "A")]), Err(not_filed("Tr", "A")), "`Tr::A`'s bounds are not known: the gap naming them");
    assert_eq!(solve(goal, &[]), Ok(Answer::Yes), "CONTROL: `Tr` filed, `A: Clone` by its written bound");
    let twin = "params T\nenv T: Bare\ngoal <T as Bare>::A: Clone => no\n";
    assert_eq!(solve(twin, &[]), Ok(Answer::No), "TWIN: every bound of `Bare::A` known, none `Clone`: No (E0277)");
}

/// An impl that does not define `A`, under a trait whose members are not filed: whether `Tr`
/// has a default for `A` is not known, so neither is `<u8 as Tr>::A` (never the error type,
/// the program's E0046, which would pass every use silently). TWIN (the control, rule 23): the
/// same impl with `Bare` filed and no default is the program's E0046, reported at the impl: the
/// projection is the error type, an answer (Yes, `?0` the error type), not a stop.
#[test]
fn an_impl_without_an_unfiled_member_is_a_named_gap() {
    let goal = "impl Bare for u8 { }\ngoal <u8 as Bare>::A == ?0 => yes\n";
    assert_eq!(solve(goal, &[("Bare", "A")]), Err(not_filed("Bare", "A")), "`Bare`'s members not known: the gap naming `A`");
    assert_eq!(solve(goal, &[]), Ok(Answer::Yes), "TWIN: `Bare` filed, no default: E0046's error type, an answer");
}
