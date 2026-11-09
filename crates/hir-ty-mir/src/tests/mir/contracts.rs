//! blaze_verify's native contracts checked at run time (docs/v2/plan/design/contracts.md,
//! part 1): what the crate cfg atom `blaze::contract_checks` changes in MIR, and what it
//! leaves alone.

use hir_expand::fixture;

use crate::{
    db::HirDatabase,
    mir::{MirBody, TerminatorKind, print_mir_body},
    setup_tracing,
    test_db::TestDBExt as _,
};

/// The fixture `source` as one crate, with the atom when `checks`.
fn crate_of(source: &str, checks: bool) -> String {
    let cfg = if checks { " cfg:blaze::contract_checks" } else { "" };
    format!("//- minicore: sized, copy\n//- /main.rs{cfg}\n{source}")
}

/// The function `name` of the fixture's last file, lowered, given to `check`: a panic when it
/// does not lower.
fn lowered<T>(fixture_text: &str, name: &str, check: impl FnOnce(&MirBody<'_>, String) -> T) -> T {
    let _tracing = setup_tracing();
    let fx = fixture::db(fixture_text);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let file_id = *file_ids.last().unwrap();
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let func = scope
            .declarations()
            .find_map(|x| match x {
                hir_def::ModuleDefId::FunctionId(it)
                    if hir_def::signatures::FunctionSignature::of(&db, it).name.as_str()
                        == name =>
                {
                    Some(it)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the fixture's `{name}`"));
        match db.mir_body(func.into()) {
            Ok(body) => {
                let printed = print_mir_body(&db, &body, name);
                check(&body, printed)
            }
            Err(e) => panic!("`{name}` does not lower: {e:?}"),
        }
    })
}

/// The calls of `body` that never return: here, the aborts of broken clauses.
fn aborts(body: &MirBody<'_>) -> usize {
    body.basic_blocks
        .iter()
        .filter(|(_, it)| {
            matches!(
                it.terminator.as_ref().map(|t| &t.kind),
                Some(TerminatorKind::Call { target: None, .. })
            )
        })
        .count()
}

/// Opt-in (rule 4): a program without `blaze_verify` and without clauses lowers to the same MIR
/// whether its profile checks contracts or not.
#[test]
fn contract_checks_leave_a_body_without_clauses_byte_identical() {
    let source = r#"
fn f(n: u32, flag: bool) -> u32 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        if flag { total += i; } else { return total; }
        i += 1;
    }
    match n { 0 => 1, _ => total }
}
"#;
    let off = lowered(&crate_of(source, false), "f", |_, printed| printed);
    let on = lowered(&crate_of(source, true), "f", |_, printed| printed);
    assert_eq!(off, on, "the atom changed the MIR of a body without clauses");
}

/// With the atom, a `requires` is tested at the head of the body and an `ensures` at every way
/// out (the `return` and the body's value), each aborting where it fails; without it the
/// clauses stay ghost. The fixture declares `panic_nounwind` itself, so it enables
/// `lang_items`, as rustc asks of any `#[lang]` (E0658) and `crate_lang_items` does.
#[test]
fn requires_and_ensures_abort_only_with_contract_checks() {
    let source = r#"
#![feature(blaze_verify, lang_items)]
#[lang = "panic_nounwind"]
fn panic_nounwind(_: &'static str) -> ! { loop {} }
fn add_one(n: u32) -> (r: u32)
    requires n < 10
    ensures r == n + 1
{
    if n == 0 { return 1; }
    n + 1
}
"#;
    let off = lowered(&crate_of(source, false), "add_one", |body, _| aborts(body));
    assert_eq!(off, 0, "clauses ran without `contract-checks`");
    let on = lowered(&crate_of(source, true), "add_one", |body, _| aborts(body));
    assert_eq!(on, 3, "one `requires` check and an `ensures` check at each of two ways out");
}
