//! The prover's invariant against the MIR evaluator
//! (docs/v2/plan/design/proved-unreachable.md, "Proof and tests").
//!
//! The invariant: a site `reach` discharges is not reached, or its condition holds, in every
//! run of the body's MIR. Held here as: a body all of whose panic sites are discharged does
//! not panic when the evaluator runs it. Hegel draws the body (guards over variants, bounds,
//! lengths and linear arithmetic around one site), the arguments (the ends of `usize` among
//! them) and whether the crate keeps overflow checks; the control plants an unsound model
//! entry, which the oracle catches.

use base_db::Db;
use hegel::{TestCase, generators as gs};
use hir_def::{DefWithBodyId, FunctionId, signatures::FunctionSignature};
use hir_expand::fixture;
use hir_ty::{InferBodyId, ParamEnvAndCrate, db::HirDatabase as _, next_solver::GenericArgs};
use hir_ty_mir::mir::{TerminatorKind, interpret_mir};

use super::{fixture::crate_named, fns::fn_named};
use crate::reach::{self, models::PLANT_FIRST_ALWAYS_SOME};

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, reach\n";

/// A body to prove and to run: `probe`'s guards, innermost last, around its site, and the
/// arguments `run` calls it with.
#[derive(Clone, Debug)]
struct Program {
    guards: Vec<String>,
    site: String,
    slice: Vec<u32>,
    a: usize,
    b: usize,
    option: Option<u32>,
    divisor: u32,
    overflow_checks: bool,
}

impl Program {
    fn source(&self) -> String {
        let mut body = self.site.clone(); // clone: the text grows around it
        for guard in self.guards.iter().rev() {
            body = format!("if {guard} {{ {body} }} else {{ 0 }}"); // alloc: a test's text
        }
        let elements: Vec<String> = self.slice.iter().map(u32::to_string).collect();
        let option = match self.option {
            Some(value) => format!("Some({value})"), // alloc: a test's text
            None => "None".to_owned(),               // alloc: a test's text
        };
        let meta = if self.overflow_checks { " cfg:overflow_checks" } else { "" };
        // alloc: a test's fixture
        format!(
            "{MINICORE}//- /lib.rs crate:kernel{meta}\n\
             pub fn probe(v: &[u32], a: usize, b: usize, o: Option<u32>, d: u32) -> u32 {{\n    {body}\n}}\n\
             pub fn run() -> u32 {{\n    let v: [u32; {}] = [{}];\n    probe(&v, {}, {}, {option}, {})\n}}\n",
            self.slice.len(),
            elements.join(", "),
            self.a,
            self.b,
            self.divisor,
        )
    }
}

/// What the prover and the evaluator said of one program.
#[derive(Debug, PartialEq, Eq)]
struct Verdict {
    /// Every panic site of `probe` is discharged.
    proved: bool,
    /// The evaluator's run of `run` panicked.
    panicked: bool,
}

/// Whether every site of `probe` that can panic is discharged: each `Assert`, and each call
/// but those of the functions the generator uses that cannot panic.
fn proved(db: &Db, probe: FunctionId) -> bool {
    let def = DefWithBodyId::FunctionId(probe);
    let reach = reach::of_body(db, def);
    if reach.refused.is_some() {
        return false;
    }
    let body = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)).expect("`probe` lowers");
    body.basic_blocks.iter().all(|(id, block)| {
        match block.terminator.as_ref().map(|it| &it.kind) {
            Some(TerminatorKind::Assert { .. }) => reach.discharged_block(id),
            Some(TerminatorKind::Call { func, .. }) => {
                let quiet = func.fn_item().is_some_and(|(callee, _)| {
                    let name = FunctionSignature::of(db, callee).name.symbol().as_str();
                    matches!(name, "len" | "is_empty" | "is_some" | "is_none" | "first")
                });
                quiet || reach.discharged_block(id)
            }
            _ => true,
        }
    })
}

/// The oracle: the verdict of `program`, or what breaks the invariant.
fn check(program: &Program) -> Result<Verdict, String> {
    let source = program.source();
    let db = fixture::db(&source).into_db();
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        let (probe, run) = (fn_named(&db, krate, "probe"), fn_named(&db, krate, "run"));
        let proved = proved(&db, probe);
        let env = ParamEnvAndCrate { param_env: db.trait_environment(run.into()), krate }.store();
        let body = db
            .monomorphized_runtime_mir_body(run.into(), GenericArgs::empty().store(), env)
            .map_err(|error| format!("`run` does not lower: {error:?}\n{source}"))?; // alloc: a test's message
        let ran = interpret_mir(&db, body, None).and_then(|(value, _)| value);
        let panicked = match ran {
            Ok(_) => false,
            // alloc: a test's message
            Err(error) => match format!("{error:?}") {
                said if said.contains("Panic") => true,
                said => return Err(format!("the evaluator could not run it: {said}\n{source}")),
            },
        };
        if proved && panicked {
            // alloc: a test's message
            return Err(format!(
                "every panic site of `probe` is discharged, and the run panics:\n{source}"
            ));
        }
        Ok(Verdict { proved, panicked })
    })
}

fn below(tc: &TestCase, n: u32) -> u32 {
    tc.draw(gs::integers::<u32>().max_value(n - 1))
}

fn pick<'a>(tc: &TestCase, of: &[&'a str]) -> &'a str {
    of[below(tc, of.len() as u32) as usize]
}

/// A linear expression over `a`, `b` and the length.
fn linear(tc: &TestCase) -> String {
    let base = pick(tc, &["a", "b", "v.len()", "0", "1", "2"]);
    match below(tc, 6) {
        0 => format!("{base} + {}", pick(tc, &["1", "2", "b"])), // alloc: a test's text
        1 => format!("{base} - {}", pick(tc, &["1", "2", "a"])), // alloc: a test's text
        2 => format!("{base} / 2"),                             // alloc: a test's text
        3 => format!("a + (b - a) / 2"),                        // alloc: a test's text
        _ => base.to_owned(),                                   // alloc: a test's text
    }
}

fn guard(tc: &TestCase) -> String {
    match below(tc, 8) {
        0 => "!v.is_empty()".to_owned(), // alloc: a test's text
        1 => "o.is_some()".to_owned(),   // alloc: a test's text
        2 => "d != 0".to_owned(),        // alloc: a test's text
        3 => "a < b && b <= v.len()".to_owned(), // alloc: a test's text
        _ => {
            let relation = pick(tc, &["<", "<=", ">", ">=", "!=", "=="]);
            format!("{} {relation} {}", linear(tc), linear(tc)) // alloc: a test's text
        }
    }
}

fn site(tc: &TestCase) -> String {
    match below(tc, 7) {
        0 => "o.unwrap()".to_owned(),         // alloc: a test's text
        1 => "100 / d".to_owned(),            // alloc: a test's text
        2 => "*v.first().unwrap()".to_owned(), // alloc: a test's text
        3 => format!("v[{}] ^ v[{}]", linear(tc), linear(tc)), // alloc: a test's text
        _ => format!("v[{}]", linear(tc)),    // alloc: a test's text
    }
}

fn drawn(tc: &TestCase) -> Program {
    let ends = [0, 1, 2, 3, 4, 5, usize::MAX - 1, usize::MAX];
    Program {
        guards: (0..below(tc, 4)).map(|_| guard(tc)).collect(),
        site: site(tc),
        slice: (0..below(tc, 5)).collect(),
        a: ends[below(tc, ends.len() as u32) as usize],
        b: ends[below(tc, ends.len() as u32) as usize],
        option: (below(tc, 2) == 0).then_some(7),
        divisor: below(tc, 3),
        overflow_checks: below(tc, 2) == 0,
    }
}

/// The invariant: a body all of whose panic sites `reach` discharges does not panic when the
/// evaluator runs it. Random guards (variants, bounds, lengths, linear arithmetic, `&&`)
/// around a random site, random arguments, both overflow profiles.
#[hegel::test(test_cases = 150)]
fn a_body_whose_sites_are_discharged_does_not_panic(tc: TestCase) {
    let program = drawn(&tc);
    if let Err(what) = check(&program) {
        panic!("{what}");
    }
}

fn program(guards: &[&str], site: &str, slice: &[u32], a: usize, b: usize) -> Program {
    Program {
        guards: guards.iter().map(|it| (*it).to_owned()).collect(), // alloc: a test's text
        site: site.to_owned(),                                      // alloc: a test's text
        slice: slice.to_vec(),
        a,
        b,
        option: None,
        divisor: 0,
        overflow_checks: false,
    }
}

/// The oracle sees both sides: a guarded site is proved and runs; the same site unguarded is
/// not proved, and the evaluator's run of it panics, so a `proved` that were always true
/// would be caught.
#[test]
fn the_oracle_tells_a_guarded_site_from_a_bare_one() {
    let guarded = program(&["a < v.len()"], "v[a]", &[1, 2], 5, 0);
    assert_eq!(check(&guarded), Ok(Verdict { proved: true, panicked: false }));
    let bare = program(&[], "v[a]", &[1, 2], 5, 0);
    assert_eq!(check(&bare), Ok(Verdict { proved: false, panicked: true }));
    let last = program(&["!v.is_empty()"], "v[v.len() - 1]", &[1, 2], 0, 0);
    assert_eq!(check(&last), Ok(Verdict { proved: true, panicked: false }));
    // Without overflow checks `len - 1` wraps on an empty slice: not proved, and it panics.
    let wraps = program(&[], "v[v.len() - 1]", &[], 0, 0);
    assert_eq!(check(&wraps), Ok(Verdict { proved: false, panicked: true }));
    let middle = program(&["a < b && b <= v.len()"], "v[a + (b - a) / 2]", &[1, 2, 3], 0, 3);
    assert_eq!(check(&middle), Ok(Verdict { proved: true, panicked: false }));
}

/// A loop's invariant is held to the evaluator too: a binary search over `0..len` is proved
/// and runs to its end for a key above every element; the same loop started one past the
/// length is not proved, and that run panics.
#[test]
fn the_oracle_runs_a_binary_search_loop() {
    let search = |hi: &str| {
        // alloc: a test's text
        let site = format!(
            "{{ let mut lo = 0; let mut hi = {hi}; while lo < hi {{ let mid = lo + (hi - lo) / 2; \
             if v[mid] < d {{ lo = mid + 1; }} else {{ hi = mid; }} }} lo as u32 }}"
        );
        Program { divisor: 100, ..program(&[], &site, &[1, 2, 3, 4, 5], 0, 0) }
    };
    for overflow_checks in [false, true] {
        let inside = Program { overflow_checks, ..search("v.len()") };
        assert_eq!(check(&inside), Ok(Verdict { proved: true, panicked: false }));
        let past = Program { overflow_checks, ..search("v.len() + 1") };
        assert_eq!(check(&past), Ok(Verdict { proved: false, panicked: true }));
    }
}

/// The control: with a model entry planted that says `first` is always `Some`, the prover
/// discharges `v.first().unwrap()` on a slice that may be empty, and the oracle refuses it.
#[test]
fn the_oracle_catches_a_model_that_says_first_is_always_some() {
    let empty = program(&[], "*v.first().unwrap()", &[], 0, 0);
    assert_eq!(check(&empty), Ok(Verdict { proved: false, panicked: true }));
    PLANT_FIRST_ALWAYS_SOME.set(true);
    let caught = check(&empty);
    PLANT_FIRST_ALWAYS_SOME.set(false);
    let Err(what) = caught else { panic!("the planted model was not caught: {caught:?}") };
    assert!(what.contains("is discharged, and the run panics"), "{what}");
}
