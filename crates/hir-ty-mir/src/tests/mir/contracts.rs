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

/// The function `name` of the fixture's last file: why it does not lower, or nothing.
fn refusal(fixture_text: &str, name: &str) -> Option<String> {
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
        db.mir_body(func.into()).err().map(|e| format!("{e:?}"))
    })
}

/// A crate with the atom whose `main` runs in the MIR evaluator: nothing when it returns, the
/// message when a clause aborts. The fixture's `panic_nounwind` panics with what it is given,
/// so the evaluator reports the abort as a panic.
fn run(source: &str) -> Option<String> {
    use crate::mir_pretty::MirEvalErrorPretty as _;
    let _tracing = setup_tracing();
    let text = format!(
        "//- minicore: sized, copy, fmt, panic, option, result\n//- /main.rs \
         cfg:blaze::contract_checks\n#![feature(blaze_verify, lang_items)]\n#[lang = \
         \"panic_nounwind\"]\nfn panic_nounwind(msg: &'static str) -> ! {{ panic!(\"{{}}\", msg) \
         }}\n{source}"
    );
    let fx = fixture::db(&text);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let file_id = *file_ids.last().unwrap();
        match super::eval::eval_main_at(&db, file_id, crate::mir::MirOptLevel::Off) {
            Ok(_) => None,
            Err(e) => Some(
                e.is_panic().unwrap_or_else(|| panic!("not a broken clause: {e:?}")).to_owned(),
            ),
        }
    })
}

/// What `Key`-shaped fixtures share: a value that is not `Copy` in a `Result`.
const KEYED: &str = r#"
struct Key(u32);
fn key(n: u32) -> (r: Result<Key, u32>)
    ensures match &r {
        Ok(k) => k.0 < 10,
        Err(e) => *e == n,
    }
{
    if n > 100 { return Err(n); }
    Ok(Key(n))
}
"#;

/// What an `ensures` with a named return binds in its patterns (`Ok(k) =>`, `Err(e) =>`) is
/// owned by the clause group, not the body: each gets a local where the clause binds it, at
/// every way out. Without that the body did not lower at all (`InaccessibleLocal`).
#[test]
fn an_ensures_declares_what_its_patterns_bind() {
    let source = format!("#![feature(blaze_verify, lang_items)]\n#[lang = \"panic_nounwind\"]\nfn panic_nounwind(_: &'static str) -> ! {{ loop {{}} }}\n{KEYED}");
    let fixture_text = format!(
        "//- minicore: sized, copy, option, result\n//- /main.rs cfg:blaze::contract_checks\n{source}"
    );
    assert_eq!(refusal(&fixture_text, "key"), None);
    let on = lowered(&fixture_text, "key", |body, _| aborts(body));
    assert_eq!(on, 2, "the `ensures` is tested at the `return` and at the body's value");
    // The cause, removed: without the atom the clause is ghost and binds nothing.
    let ghost = format!("//- minicore: sized, copy, option, result\n//- /main.rs\n{source}");
    assert_eq!(lowered(&ghost, "key", |body, _| aborts(body)), 0);
}

/// The clause's bindings hold what the return slot holds: a clause that holds lets the value
/// out by either way, and one that does not aborts naming the clause.
#[test]
fn an_ensures_that_matches_the_result_runs() {
    let main = |n: u32| format!("{KEYED}\nfn main() {{ let _ = key({n}); }}");
    assert_eq!(run(&main(3)), None, "`Ok(Key(3))`: `k.0 < 10` holds");
    assert_eq!(run(&main(200)), None, "`Err(200)` by `return`: `*e == n` holds");
    let broken = run(&main(50)).expect("`Ok(Key(50))` breaks `k.0 < 10`");
    assert!(broken.contains("broke its contract: ensures"), "{broken}");
    assert!(broken.contains("key"), "{broken}");
}

/// The three clauses of crates/blaze-std that first hit this (`Budget::take`, `Dir::quota`,
/// `Key::new`), as free functions: each lowers with its checks.
#[test]
fn the_clauses_of_bstd_that_bind_lower() {
    let source = r#"
#![feature(blaze_verify, lang_items)]
#[lang = "panic_nounwind"]
fn panic_nounwind(_: &'static str) -> ! { loop {} }
struct Over { asked: u64, used: u64, limit: u64 }
fn take(used: &u64, bytes: u64) -> (r: Result<(), Over>)
    ensures match &r {
        Ok(()) => true,
        Err(over) => over.asked == bytes && over.used <= over.limit,
    }
{
    if *used > bytes { return Err(Over { asked: bytes, used: *used, limit: bytes }); }
    Ok(())
}
struct Dir { left: Option<u64> }
impl Dir {
    fn quota_left(&self) -> Option<u64> { self.left }
}
fn quota(bytes: u64) -> (r: Dir)
    ensures match r.quota_left() {
        Some(left) => left <= bytes,
        None => false,
    }
{
    Dir { left: Some(bytes) }
}
struct Key { len: usize }
impl Key {
    fn len(&self) -> usize { self.len }
}
struct KeyError;
fn new(len: usize) -> (r: Result<Key, KeyError>)
    ensures match &r {
        Ok(key) => !(key.len() == 0) && key.len() <= 512,
        Err(_) => true,
    }
{
    if len == 0 { return Err(KeyError); }
    Ok(Key { len })
}
"#;
    let fixture_text = format!(
        "//- minicore: sized, copy, option, result\n//- /main.rs cfg:blaze::contract_checks\n{source}"
    );
    for (name, ways_out) in [("take", 2), ("quota", 1), ("new", 2)] {
        assert_eq!(refusal(&fixture_text, name), None, "`{name}`");
        let on = lowered(&fixture_text, name, |body, _| aborts(body));
        assert_eq!(on, ways_out, "`{name}`: one check at each way out");
    }
}

/// An `ensures` runs before the caller gets the value, so it reads it: a pattern that binds a
/// part that is not `Copy` by value, or a call that takes the value, would move out of the
/// return slot and drop what the caller is given. Refused naming the function and what is
/// moved; the same clause by reference, and a by-value binding of a `Copy` part, lower.
#[test]
fn an_ensures_that_moves_out_of_the_result_is_refused() {
    let with = |clause: &str| {
        format!(
            "//- minicore: sized, copy, option, result\n//- /main.rs \
             cfg:blaze::contract_checks\n#![feature(blaze_verify, lang_items)]\n#[lang = \
             \"panic_nounwind\"]\nfn panic_nounwind(_: &'static str) -> ! {{ loop {{}} }}\nstruct \
             Key(u32);\nfn small(k: Key) -> bool {{ k.0 < 10 }}\nfn key(n: u32) -> (r: \
             Result<Key, u32>)\n    ensures {clause}\n{{\n    Ok(Key(n))\n}}\n"
        )
    };
    for moving in ["match r { Ok(k) => small(k), Err(_) => true }", "r.is_ok_owned()"] {
        let fixture_text = with(moving).replace(
            "struct Key(u32);",
            "struct Key(u32);\ntrait Owned { fn is_ok_owned(self) -> bool; }\nimpl Owned for \
             Result<Key, u32> { fn is_ok_owned(self) -> bool { true } }",
        );
        let why = refusal(&fixture_text, "key").unwrap_or_else(|| panic!("`{moving}` lowered"));
        assert!(why.contains("key") && why.contains("moves out of `r`"), "{moving}: {why}");
    }
    for reading in [
        "match &r { Ok(k) => k.0 < 10, Err(_) => true }",
        "match r { Ok(ref k) => k.0 < 10, Err(_) => true }",
        "match r { Ok(_) => true, Err(e) => e == n }",
    ] {
        assert_eq!(refusal(&with(reading), "key"), None, "`{reading}`");
    }
}
