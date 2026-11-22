//! The case runner: builds each case's world, solves its goals from one starting state, and
//! compares every answer (and every expected variable value) with the case's expectation.

use std::collections::{BTreeMap, HashMap};
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use blaze_own::Alloc;

use super::parse::{CaseA, CaseError, CheckA, DeclA, Expect, FileA, Parsed, parse_file};
use super::tys::TestTys;
use super::world::{Arena, Scope, World};
use crate::assembly::{Clause, Goal};
use crate::coherence::{self, Orphan, Overlap};
use crate::error::Stop;
use crate::eval::{Answer, Solver};
use crate::facts::{Ask, Read};
use crate::fold::{self, Scratch};
use crate::term::{Certainty, TypingMode};
use crate::tys::Tys;

/// The prelude every file is read on top of.
pub const PRELUDE: &str = include_str!("../tests/cases/prelude.solve");

/// The case files, by name.
pub const FILES: [(&str, &str); 8] = [
    ("core.solve", include_str!("../tests/cases/core.solve")),
    ("builtin.solve", include_str!("../tests/cases/builtin.solve")),
    ("auto.solve", include_str!("../tests/cases/auto.solve")),
    ("alias.solve", include_str!("../tests/cases/alias.solve")),
    ("coherence.solve", include_str!("../tests/cases/coherence.solve")),
    ("dyn_auto.solve", include_str!("../tests/cases/dyn_auto.solve")),
    ("shadow.solve", include_str!("../tests/cases/shadow.solve")),
    ("env_bounds.solve", include_str!("../tests/cases/env_bounds.solve")),
];

/// What a run found.
#[derive(Default, Debug)]
pub struct Report {
    pub cases: u32,
    pub goals: u32,
    pub passed: u32,
    /// One line per failed goal: file, line, case, rule, what was wanted and what came.
    pub failures: Vec<String>,
    /// Cases per reference rule (the rule's file and function).
    pub by_rule: BTreeMap<String, u32>,
}

/// The rule a case names, as `file::function`.
fn rule_key(rule: &str) -> String {
    let r = rule.split(" (").next().unwrap_or(rule).trim();
    let r = r.strip_prefix("rustc_trait_selection ").unwrap_or(r);
    r.replace(' ', "::")
}

/// Runs every case of `text` whose name `filter` accepts.
pub fn run_text(file: &str, prelude: &FileA, text: &str, filter: &dyn Fn(&str) -> bool, report: &mut Report) -> Parsed<()> {
    run_text_with(file, prelude, text, filter, &TestTys::new, report)
}

/// `run_text` over the term arena `new_tys` makes, one per case: the same cases through
/// another `Tys` (blaze-typeck runs them over its `Terms`, blaze-ty's arena).
pub fn run_text_with<T: Tys>(
    file: &str,
    prelude: &FileA,
    text: &str,
    filter: &dyn Fn(&str) -> bool,
    new_tys: &dyn Fn() -> T,
    report: &mut Report,
) -> Parsed<()> {
    let f = parse_file(text)?;
    for case in &f.cases {
        if !filter(&case.name) {
            continue;
        }
        report.cases += 1;
        *report.by_rule.entry(rule_key(&case.rule)).or_default() += 1;
        let mut decls: Vec<(u32, &DeclA)> = Vec::new();
        decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
        decls.extend(f.decls.iter().map(|(l, d)| (*l, d)));
        decls.extend(case.decls.iter().map(|(l, d)| (*l, d)));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(case, &decls, &mut new_tys())));
        match outcome {
            Ok(Ok(results)) => {
                for (line, ok, want, got) in results {
                    report.goals += 1;
                    if ok {
                        report.passed += 1;
                    } else {
                        report.failures.push(format!("{file}:{line}: case {} ({}): want {want}, got {got}", case.name, case.rule));
                    }
                }
            }
            Ok(Err(e)) => {
                report.goals += case.goals.len() as u32;
                report.failures.push(format!("{file}:{}: case {} ({}): {e}", e.line, case.name, case.rule));
            }
            Err(p) => {
                report.goals += case.goals.len() as u32;
                let why = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                report.failures.push(format!("{file}:{}: case {} ({}): panicked: {why}", case.line, case.name, case.rule));
            }
        }
    }
    Ok(())
}

/// Runs every case file.
pub fn run_all(filter: &dyn Fn(&str) -> bool) -> Parsed<Report> {
    run_all_with(filter, &TestTys::new)
}

/// `run_all` over the term arena `new_tys` makes (see `run_text_with`).
pub fn run_all_with<T: Tys>(filter: &dyn Fn(&str) -> bool, new_tys: &dyn Fn() -> T) -> Parsed<Report> {
    let prelude = parse_file(PRELUDE)?;
    let mut report = Report::default();
    for (name, text) in FILES {
        run_text_with(name, &prelude, text, filter, new_tys, &mut report).map_err(|e| CaseError { line: e.line, what: format!("{name}: {}", e.what) })?;
    }
    Ok(report)
}

type GoalResult = (u32, bool, String, String);

fn run_case<T: Tys>(case: &CaseA, decls: &[(u32, &DeclA)], tys: &mut T) -> Parsed<Vec<GoalResult>> {
    let world = World::build(decls)?;
    let alloc = blaze_own::testing::heap().alloc();
    let mut scratch = Scratch::new_in(alloc);
    let mut vars: HashMap<String, crate::term::Ty> = HashMap::new();
    let mut solver = Solver::new_in(alloc);
    let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
    let sc = Scope { params: &case.params, bound: &[] };
    let mut clauses: Vec<Clause> = Vec::new();
    {
        let mut a = Arena { tys: &mut *tys, vars: &mut vars };
        for p in &case.env {
            world.names.pred(&mut a, sc, case.line, p, &mut clauses)?;
        }
        // `env T: ?Sized` takes `T`'s implicit `Sized` away, as on a generic parameter.
        let unsized_: Vec<bool> = case
            .params
            .iter()
            .map(|n| {
                case.env.iter().any(|p| matches!(p, crate::testing::parse::PredA::Bound(crate::testing::parse::TyA::Path(w, a), b) if b.maybe && a.is_empty() && w == n))
            })
            .collect();
        world.names.case_sized(&mut a, case.line, &unsized_, &mut clauses)?;
    }
    let stop = |line: u32, s: Stop| CaseError { line, what: format!("the solver stopped: {s:?}") };
    let env = solver.env(tys, &world, &clauses, &mut reads).map_err(|s| stop(case.line, s))?;
    // A case with no `mode` line is a body's check: analysis.
    let mode = case.mode.unwrap_or(TypingMode::Analysis);
    let mut out = Vec::new();
    for g in &case.goals {
        let line = g.line;
        match &g.check {
            CheckA::Goal(p) => {
                let mut cl = Vec::new();
                {
                    let mut a = Arena { tys: &mut *tys, vars: &mut vars };
                    world.names.pred(&mut a, sc, line, p, &mut cl)?;
                }
                let [c]: [Clause; 1] = cl.try_into().map_err(|_| CaseError { line, what: "a goal is one predicate".to_string() })?;
                let snap = tys.snapshot();
                let r = solver.evaluate(tys, &world, mode, Goal { env, pred: c.pred, bound: c.bound }, &mut reads);
                let res = match r {
                    Err(s) => {
                        let undecided = matches!(
                            s,
                            Stop::Error(crate::error::SolveError::Unsupported(crate::error::Unsupported::DynUndecided(_)))
                        );
                        (line, undecided && g.expect == Expect::DynUndecided, format!("{:?}", g.expect), format!("stop {s:?}"))
                    }
                    Ok(a) => {
                        let got = match a {
                            Answer::Yes => "yes".to_string(),
                            Answer::No => "no".to_string(),
                            Answer::Maybe(c) if c.is_overflow() => "overflow".to_string(),
                            Answer::Maybe(_) => "maybe".to_string(),
                        };
                        match &g.expect {
                            Expect::Yes(binds) => {
                                let mut ok = a == Answer::Yes;
                                let mut detail = String::new();
                                if ok {
                                    for (name, want) in binds {
                                        let Some(&v) = vars.get(name) else {
                                            return Err(CaseError { line, what: format!("?{name} is not in the goal") });
                                        };
                                        let have = fold::resolve(tys, &mut scratch, v).map_err(|e| CaseError { line, what: format!("{e}") })?;
                                        let want_t = {
                                            let mut a = Arena { tys: &mut *tys, vars: &mut vars };
                                            world.names.ty(&mut a, sc, line, want)?
                                        };
                                        let want_t = fold::resolve(tys, &mut scratch, want_t).map_err(|e| CaseError { line, what: format!("{e}") })?;
                                        if have != want_t {
                                            ok = false;
                                            detail.push_str(&format!(" ?{name} = {:?}", show(tys, have)));
                                        }
                                    }
                                }
                                (line, ok, format!("{:?}", g.expect), format!("{got}{detail}"))
                            }
                            Expect::No => (line, a == Answer::No, "no".to_string(), got),
                            Expect::Maybe => (line, matches!(a, Answer::Maybe(Certainty::Ambiguous)), "maybe".to_string(), got),
                            Expect::Overflow => (line, matches!(a, Answer::Maybe(c) if c.is_overflow()), "overflow".to_string(), got),
                            Expect::DynUndecided => (line, false, "dyn_undecided".to_string(), got),
                            other => return Err(CaseError { line, what: format!("a goal cannot expect {other:?}") }),
                        }
                    }
                };
                tys.rollback_to(snap);
                out.push(res);
            }
            CheckA::Overlap(a, b) => {
                let da = *world.names.impls.get(a).ok_or_else(|| CaseError { line, what: format!("no impl @{a}") })?;
                let db = *world.names.impls.get(b).ok_or_else(|| CaseError { line, what: format!("no impl @{b}") })?;
                let r = coherence::overlap(&mut solver, tys, &world, &mut reads, da, db);
                let want = match g.expect {
                    Expect::Overlap => Overlap::Overlap,
                    Expect::Disjoint => Overlap::Disjoint,
                    Expect::MayOverlap => Overlap::MayOverlap,
                    _ => return Err(CaseError { line, what: "overlap expects overlap, disjoint or may_overlap".to_string() }),
                };
                match r {
                    Ok(o) => out.push((line, o == want, format!("{want:?}"), format!("{o:?}"))),
                    Err(s) => out.push((line, false, format!("{want:?}"), format!("stop {s:?}"))),
                }
            }
            CheckA::Orphan(a) => {
                let da = *world.names.impls.get(a).ok_or_else(|| CaseError { line, what: format!("no impl @{a}") })?;
                let r = coherence::orphan_check(tys, &Ask::new(&world), &mut reads, da);
                let want = match g.expect {
                    Expect::Ok => Orphan::Ok,
                    Expect::NotLocal => Orphan::NotLocal,
                    Expect::Uncovered(i) => Orphan::UncoveredParam(i),
                    _ => return Err(CaseError { line, what: "orphan expects ok, not_local or uncovered(i)".to_string() }),
                };
                match r {
                    Ok(o) => out.push((line, o == want, format!("{want:?}"), format!("{o:?}"))),
                    Err(s) => out.push((line, false, format!("{want:?}"), format!("stop {s:?}"))),
                }
            }
        }
    }
    Ok(out)
}

/// A term's kind, one level, for a failure line.
fn show<T: Tys>(tys: &T, t: crate::term::Ty) -> crate::term::TyKind {
    tys.kind(t)
}
