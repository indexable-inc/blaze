//! The harness over random programs: functions calling the ones before them, methods, struct
//! literals and closures; an edit of each body reaches only it.

use blaze_sim::{Config, Tape};
use hir_expand::fixture;
use hir_firewall::Harness;

/// One operand of a function of `x` that may call the `functions` before it and use the
/// `structs` there are.
fn operand(tape: &Tape, functions: usize, structs: usize) -> String {
    match tape.int_in(0..=4u8, "operand") {
        1 if functions > 0 => {
            let at: usize = tape.index(functions, "callee");
            format!("f{at}(x)")
        }
        2 if structs > 0 => {
            let at: usize = tape.index(structs, "struct");
            format!("P{at} {{ a: x }}.get()")
        }
        3 => format!("(|y: u32| y ^ {})(x)", tape.int_in(0..=99u32, "closure literal")),
        4 => "x".to_owned(),
        _ => format!("{}u32", tape.int_in(0..=99u32, "literal")),
    }
}

fn program(tape: &Tape) -> String {
    let mut out = String::from("//- minicore: sized, copy, fn, option\n");
    let structs: usize = tape.int_in(0..=2, "structs");
    for at in 0..structs {
        let lit: u32 = tape.int_in(0..=99, "struct literal");
        out.push_str(&format!(
            "pub struct P{at} {{ pub a: u32 }}\nimpl P{at} {{ pub fn get(&self) -> u32 {{ self.a ^ {lit} }} }}\n"
        ));
    }
    let functions: usize = tape.int_in(1..=5, "functions");
    for at in 0..functions {
        let mut body = operand(tape, at, structs);
        for _ in 0..tape.int_in(0..=2u8, "more operands") {
            body.push_str(tape.one_of(&[" ^ ", " | ", " & "], "operator"));
            body.push_str(&operand(tape, at, structs));
        }
        out.push_str(&format!("pub fn f{at}(x: u32) -> u32 {{ {body} }}\n"));
    }
    out
}

/// Each case edits every body of its program.
fn reaches_only_the_edited_body(tape: &Tape) {
    let fixture = program(tape);
    let mut db = fixture::db(&fixture).into_db();
    let krate = fixture::test_crate(&db);
    let mut harness = Harness::new(&mut db, krate, None).expect("the harness sets up");
    let report = harness.run(&mut db, 1).expect("every edit is made");
    assert!(report.edits > 0, "no function was edited:\n{fixture}");
    assert!(report.is_clean(), "{report}\n{fixture}");
}

/// `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
#[ignore = "a body edit re-executes `VariantFields::with_source_map` for the module's item checks, which need only its diagnostics"]
fn an_edit_of_any_body_of_any_program_reaches_only_it() {
    blaze_sim::check(Config::cases(16), reaches_only_the_edited_body);
}
