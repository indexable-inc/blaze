//! One fixture crate of many bodies in many files, one body edited at a time: the time to
//! ask for the whole crate again after an edit, against its bound
//! (docs/v2/plan/design/item-hash.md). An edit's pass can do no less than a pass
//! that changed nothing (every entry a hit) plus running the edited body's queries,
//! which cost a cold pass's share per body; an edit is that twice, once for the undo.

use std::{fmt::Write as _, time::Instant};

use hir_expand::fixture;
use hir_firewall::Harness;

const MODULES: usize = 10;
const PER_MODULE: usize = 20;
/// Every `STRIDE`th function is edited.
const STRIDE: usize = 7;

fn fixture() -> String {
    let mut text = String::from("//- minicore: sized, copy\n//- /lib.rs\n");
    for m in 0..MODULES {
        writeln!(text, "pub mod m{m};").unwrap();
    }
    for m in 0..MODULES {
        writeln!(text, "//- /m{m}.rs").unwrap();
        writeln!(text, "pub struct S{m} {{ pub v: u32, pub w: u32 }}").unwrap();
        writeln!(text, "impl S{m} {{ pub fn get(&self) -> u32 {{ self.v + self.w }} }}").unwrap();
        for f in 0..PER_MODULE {
            let call = if f == 0 { "0".to_owned() } else { format!("f{}(s)", f - 1) };
            writeln!(
                text,
                "pub fn f{f}(s: &S{m}) -> u32 {{\n    let mut total = s.get();\n    let mut i = 0;\n    while i < {f} {{ total += i; i += 1; }}\n    if total > 100 {{ total - {call} }} else {{ total + {call} }}\n}}"
            )
            .unwrap();
        }
    }
    text
}

fn main() {
    let mut db = fixture::db(&fixture()).into_db();
    let krate = fixture::test_crate(&db);

    let started = Instant::now();
    let mut harness = Harness::new(&mut db, krate, None).expect("the harness sets up");
    let setup = started.elapsed();

    // Nothing changed: a commit that moves nothing makes no snapshot, so this is the same one.
    let started = Instant::now();
    let executed = harness.ask_all(&db).expect("a pass");
    let warm = started.elapsed();
    if executed != 0 {
        println!(
            "  asked again with nothing changed, {executed} queries ran again: they are volatile"
        );
    }

    let bodies = harness.body_count() as u32;
    // `setup` is a cold pass and a warm one.
    let per_body = setup.saturating_sub(warm) / bodies.max(1);
    let started = Instant::now();
    let report = harness.run(&mut db, STRIDE).expect("every edit is made");
    let edits = started.elapsed();
    let per_edit = edits / (report.edits as u32).max(1);
    let bound = 2 * (warm + per_body);

    let ratio = per_edit.as_secs_f64() / bound.as_secs_f64();
    let verdict =
        if ratio <= 1.1 { "within 10% of the bound" } else { "OVER the bound by more than 10%" };
    println!(
        "firewall edit: {bodies} bodies, {} edits; cold body {per_body:?}, warm pass {warm:?}; \
         per edit {per_edit:?} against a bound of {bound:?} ({ratio:.2}x): {verdict}",
        report.edits
    );
    println!(
        "  per edit: {} queries ran, {} of them the crate's own; {} served from records",
        report.executed / report.edits.max(1),
        report.reran_own / report.edits.max(1),
        report.served / report.edits.max(1) as u64
    );
    if !report.is_clean() {
        println!("  the edits leaked: {report}");
    }
}
