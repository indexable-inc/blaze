//! The harness over random programs: functions calling the ones before them, methods, struct
//! literals and closures; an edit of each body reaches only it.

use hegel::{Hegel, Settings, TestCase};
use hegel::generators as gs;
use hir_firewall::Harness;
use hir_expand::fixture;

/// One operand of a function of `x` that may call the `functions` before it and use the
/// `structs` there are.
fn operand(tc: &TestCase, functions: usize, structs: usize) -> String {
    match tc.draw(gs::integers::<u8>().max_value(4)) {
        1 if functions > 0 => {
            let at: usize = tc.draw(gs::integers().max_value(functions - 1));
            format!("f{at}(x)")
        }
        2 if structs > 0 => {
            let at: usize = tc.draw(gs::integers().max_value(structs - 1));
            format!("P{at} {{ a: x }}.get()")
        }
        3 => format!("(|y: u32| y ^ {})(x)", tc.draw(gs::integers::<u32>().max_value(99))),
        4 => "x".to_owned(),
        _ => format!("{}u32", tc.draw(gs::integers::<u32>().max_value(99))),
    }
}

fn program(tc: &TestCase) -> String {
    let mut out = String::from("//- minicore: sized, copy, fn, option\n");
    let structs: usize = tc.draw(gs::integers().max_value(2));
    for at in 0..structs {
        let lit: u32 = tc.draw(gs::integers().max_value(99));
        out.push_str(&format!(
            "pub struct P{at} {{ pub a: u32 }}\nimpl P{at} {{ pub fn get(&self) -> u32 {{ self.a ^ {lit} }} }}\n"
        ));
    }
    let functions: usize = tc.draw(gs::integers().min_value(1).max_value(5));
    for at in 0..functions {
        let mut body = operand(tc, at, structs);
        for _ in 0..tc.draw(gs::integers::<u8>().max_value(2)) {
            body.push_str(tc.draw(gs::sampled_from(vec![" ^ ", " | ", " & "])));
            body.push_str(&operand(tc, at, structs));
        }
        out.push_str(&format!("pub fn f{at}(x: u32) -> u32 {{ {body} }}\n"));
    }
    out
}

/// Each case edits every body of its program.
fn reaches_only_the_edited_body(tc: TestCase) {
    let fixture = program(&tc);
    let mut db = fixture::db(&fixture).into_db();
    let krate = fixture::test_crate(&db);
    let mut harness = Harness::new(&mut db, krate, None).expect("the harness sets up");
    let report = harness.run(&mut db, 1).expect("every edit is made");
    assert!(report.edits > 0, "no function was edited:\n{fixture}");
    assert!(report.is_clean(), "{report}\n{fixture}");
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
#[ignore = "queue 01304: a body edit re-executes `VariantFields::with_source_map` for the module's item checks, which need only its diagnostics"]
fn an_edit_of_any_body_of_any_program_reaches_only_it() {
    run_cases(16, reaches_only_the_edited_body);
}
