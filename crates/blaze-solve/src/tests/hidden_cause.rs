//! `Ask::first_hidden_unknown` (facts.rs): an undecided answer that stalled on an opaque type
//! is named by the opaque whose HIDDEN type was asked (`Question::OpaqueHidden`), never by an
//! opaque read only for its bounds (`Question::Opaque`). The Reference, "Impl trait", "Auto
//! trait leakage": an auto trait of an opaque holds by its written bound, or else by its hidden
//! type; a written `+ Send` decides `Send` without the hidden type, so an opaque with one is no
//! cause of a stall (tempo c0255wave87: seven bodies named a required trait fn's `+ Send`
//! future, hiding their real cause).

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::assembly::{Clause, Goal};
use crate::eval::{Answer, Solver};
use crate::facts::{Ask, Question, Read};
use crate::term::{Def, TypingMode};
use crate::testing::parse::{CheckA, DeclA, parse_file};
use crate::testing::run::PRELUDE;
use crate::testing::tys::TestTys;
use crate::testing::world::{Arena, Scope, World, def_of};

/// Two opaques, neither with a hidden type: `Bounded` says `Send`, `Unknown` does not.
const OPAQUES: &str = "opaque Bounded: Send\nopaque Unknown: Clone\n";

/// The answer of the one goal of `goal`, its read list, and the stall's named opaque.
fn solve(goal: &str) -> (Answer, Vec<Read>, Option<Def>) {
    let text = format!("case c = hidden_cause.rs\n{OPAQUES}goal {goal}\n");
    let r = (|| -> Result<(Answer, Vec<Read>, Option<Def>), String> {
        let prelude = parse_file(PRELUDE).map_err(|e| format!("the prelude: {e}"))?;
        let f = parse_file(&text).map_err(|e| format!("the fixture: {e}"))?;
        let mut decls: Vec<(u32, &DeclA)> = Vec::new();
        decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
        decls.extend(f.decls.iter().map(|(l, d)| (*l, d)));
        for c in &f.cases {
            decls.extend(c.decls.iter().map(|(l, d)| (*l, d)));
        }
        let world = World::build(&decls).map_err(|e| format!("the world: {e}"))?;
        let alloc = blaze_own::testing::heap().alloc();
        let [case] = &f.cases[..] else { return Err(format!("one case: {text}")) };
        let [g] = &case.goals[..] else { return Err(format!("one goal: {text}")) };
        let CheckA::Goal(p) = &g.check else { return Err(format!("line {}: not a goal", g.line)) };
        let mut tys = TestTys::new();
        let mut vars = std::collections::HashMap::new();
        let mut solver = Solver::new_in(alloc);
        let sc = Scope { params: &case.params, bound: &[] };
        let mut cl: Vec<Clause> = Vec::new();
        {
            let mut a = Arena { tys: &mut tys, vars: &mut vars };
            world.names.pred(&mut a, sc, g.line, p, &mut cl).map_err(|e| format!("line {}: {e}", g.line))?;
        }
        let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        let env = solver.env(&mut tys, &world, &[], &mut reads).map_err(|s| format!("the env: {s:?}"))?;
        let [c]: [Clause; 1] = cl.try_into().map_err(|_| format!("line {}: a goal is one predicate", g.line))?;
        let answer = solver
            .evaluate(&mut tys, &world, TypingMode::Analysis, Goal { env, pred: c.pred, bound: c.bound }, &mut reads)
            .map_err(|s| format!("line {}: the solve stopped: {s:?}", g.line))?;
        let named = Ask::new(&world).first_hidden_unknown(&reads, |_| false);
        Ok((answer, reads.iter().copied().collect(), named))
    })();
    r.unwrap_or_else(|e| panic!("{e}"))
}

/// `(Bounded, Unknown): Send`: `Bounded` holds by its bound (read first, for its bounds),
/// `Unknown` stalls on its hidden type: the stall is `Unknown`'s. TWIN `Bounded: Send` alone
/// holds, and although its read list holds `Bounded` (no hidden type) nothing is named.
#[test]
fn a_stall_is_named_by_the_opaque_whose_hidden_type_was_asked() {
    let bounded = def_of("opaque", "Bounded");
    let unknown = def_of("opaque", "Unknown");
    let (answer, reads, named) = solve("(opaque#Bounded, opaque#Unknown): Send => maybe");
    assert!(matches!(answer, Answer::Maybe(_)), "`Unknown`'s hidden type is not known: {answer:?}");
    assert!(reads.iter().any(|r| r.question == Question::OpaqueHidden(unknown)), "`Unknown` read for its hidden type: {reads:?}");
    assert_eq!(named, Some(unknown), "the stall is `Unknown`'s, never `Bounded`'s: {reads:?}");

    let (answer, reads, named) = solve("opaque#Bounded: Send => yes");
    assert!(matches!(answer, Answer::Yes), "TWIN: `+ Send` written on it holds: {answer:?}");
    assert!(reads.iter().any(|r| r.question == Question::Opaque(bounded)), "TWIN: read for its bounds: {reads:?}");
    assert!(!reads.iter().any(|r| r.question == Question::OpaqueHidden(bounded)), "TWIN: its hidden type not asked: {reads:?}");
    assert_eq!(named, None, "TWIN: an opaque read for its bounds alone names no stall: {reads:?}");
}
