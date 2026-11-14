//! The harness over random programs: functions calling the ones before them, methods, struct
//! literals and closures in crate `a`, and crate `b` calling them; an edit of each body of `a`
//! recomputes nothing of `b`.

use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};
use hir_expand::fixture;
use hir_firewall::Harness;

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

/// Crate `a` of random structs and functions, and crate `b` that calls each of `a`'s
/// functions and methods.
fn program(tc: &TestCase) -> String {
    let mut out = String::from("//- minicore: sized, copy, fn, option\n//- /a.rs crate:a\n");
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
    out.push_str("//- /b.rs crate:b deps:a\npub fn g(x: u32) -> u32 { 0");
    for at in 0..functions {
        out.push_str(&format!(" ^ a::f{at}(x)"));
    }
    for at in 0..structs {
        out.push_str(&format!(" ^ a::P{at} {{ a: x }}.get()"));
    }
    out.push_str(" }\n");
    out
}

/// The crate of the fixture named `name`.
fn crate_named(db: &base_db::Db, name: &str) -> base_db::Crate {
    base_db::all_crates(db)
        .iter()
        .copied()
        .find(|krate| {
            krate
                .extra_data(db)
                .display_name
                .as_ref()
                .is_some_and(|it| it.crate_name().as_str() == name)
        })
        .unwrap_or_else(|| panic!("the fixture has no crate `{name}`"))
}

/// Each case edits every body of `a`, and nothing of `b` runs again.
fn reaches_no_dependent(tc: &TestCase) {
    let fixture = program(tc);
    let mut db = fixture::db(&fixture).into_db();
    let krate = crate_named(&db, "a");
    let mut harness = Harness::new(&mut db, krate, None).expect("the harness sets up");
    let report = harness.run(&mut db, 1).expect("every edit is made");
    assert!(report.edits > 0, "no function was edited:\n{fixture}");
    assert!(report.is_clean(), "{report}\n{fixture}");
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, mut property: impl FnMut(&TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(move |tc: TestCase| property(&tc))
        .settings(Settings::new().test_cases(cases).database(None))
        .run();
}

/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn an_edit_of_any_body_of_any_program_recomputes_nothing_of_a_dependent() {
    run_cases(16, reaches_no_dependent);
}
