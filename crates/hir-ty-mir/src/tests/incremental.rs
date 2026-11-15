use std::alloc::Global;

use expect_test::Expect;
use hir_def::{DefWithBodyId, ModuleDefId};
use hir_expand::fixture;

use crate::{
    InferenceResult,
    method_resolution::TraitImpls,
    test_db::{TestDB, TestDBExt as _, runs_of, write_file},
};

use super::visit_module;

/// The kinds `required` counts, by their generated names: a body's inference and a crate's
/// trait impls.
const INFER: &str = crate::kinds::infer_body_query::KIND_NAME;
const TRAIT_IMPLS_FOR_CRATE: &str = crate::kinds::TraitImpls_for_crate::KIND_NAME;

// False by design on blaze-memo (left failing; port lane 3, 2026-11-14): this claims an edit of
// the file runs only the queries that read the changed text (`infer_body_query` 0 times). An
// edit of a file now rebuilds its crate's arena, so every query of that crate runs again; reuse
// across edits comes from records (the body store), not from the memo. The first snapshot (the
// cold run's list) is only to re-bless.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn typing_whitespace_inside_a_function_should_not_invalidate_types() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    $01 + 1
}",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    // A file's id is the same across a commit; the interned `EditionedFileId` is not.
    let file = pos.file_id.file_id(&db);
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[(INFER, 1)],
        &expect_test::expect![[r#"
            [
                "source_root_crates",
                "infer_body_query",
                "FunctionSignature::kept",
                "image_of",
                "signature_syntax",
                "file_outline",
                "AttrFlags::query",
                "file_attr_flags",
                "Body::of",
                "file_body_syntax",
                "trait_environment_query",
                "crate_recursion_limit",
                "UnstableFeatures::query",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "elided_lifetimes",
                "ScopeInput::legacy_macro",
                "ScopeInput::name",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> i32 {
    1
    +
    1
}";

    write_file(&mut db, file, new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[(INFER, 0)],
        &expect_test::expect![[r#"
            [
                "file_outline",
                "signature_syntax",
                "file_attr_flags",
                "file_body_syntax",
                "Body::of",
            ]
        "#]],
    );
}

// False by design on blaze-memo (left failing; port lane 3, 2026-11-14): this claims an edit of
// one body infers only that body again (`infer_body_query` once). An edit of a file now rebuilds
// its crate's arena, so every body of the crate is inferred again; reuse across edits comes from
// records (the body store), not from the memo. The first snapshot is only to re-bless.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn typing_inside_a_function_should_not_invalidate_types_in_another() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> f32 {
    1.0 + 2.0
}
fn bar() -> i32 {
    $01 + 1
}
fn baz() -> i32 {
    1 + 1
}",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    // A file's id is the same across a commit; the interned `EditionedFileId` is not.
    let file = pos.file_id.file_id(&db);
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[(INFER, 3)],
        &expect_test::expect![[r#"
            [
                "source_root_crates",
                "infer_body_query",
                "FunctionSignature::kept",
                "image_of",
                "signature_syntax",
                "file_outline",
                "AttrFlags::query",
                "file_attr_flags",
                "Body::of",
                "file_body_syntax",
                "trait_environment_query",
                "crate_recursion_limit",
                "UnstableFeatures::query",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "elided_lifetimes",
                "ScopeInput::legacy_macro",
                "ScopeInput::name",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
                "infer_body_query",
                "FunctionSignature::kept",
                "signature_syntax",
                "AttrFlags::query",
                "Body::of",
                "trait_environment_query",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "elided_lifetimes",
                "ScopeInput::legacy_macro",
                "ScopeInput::name",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
                "infer_body_query",
                "FunctionSignature::kept",
                "signature_syntax",
                "AttrFlags::query",
                "Body::of",
                "trait_environment_query",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "elided_lifetimes",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> f32 {
    1.0 + 2.0
}
fn bar() -> i32 {
    53
}
fn baz() -> i32 {
    1 + 1
}
";

    write_file(&mut db, file, new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[(INFER, 1)],
        &expect_test::expect![[r#"
            [
                "file_outline",
                "signature_syntax",
                "file_attr_flags",
                "file_body_syntax",
                "signature_syntax",
                "Body::of",
                "infer_body_query",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
                "signature_syntax",
            ]
        "#]],
    );
}

// False by design on blaze-memo (left failing; port lane 3, 2026-11-14): this claims adding a
// struct leaves the crate's trait impls (`TraitImpls::for_crate` 0 times). An edit of a file now
// rebuilds its crate's arena, so every query of that crate runs again; reuse across edits comes
// from records, not from the memo. The first snapshot is only to re-bless.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn adding_struct_invalidates_infer() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}
$0",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    // A file's id is the same across a commit; the interned `EditionedFileId` is not.
    let file = pos.file_id.file_id(&db);
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[(TRAIT_IMPLS_FOR_CRATE, 1)],
        &expect_test::expect![[r#"
            [
                "source_root_crates",
                "TraitImpls::for_crate",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}

pub struct NewStruct {
    field: i32,
}
";

    write_file(&mut db, file, new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[(TRAIT_IMPLS_FOR_CRATE, 0)],
        &expect_test::expect![[r#"
            []
        "#]],
    );
}

// False by design on blaze-memo (left failing; port lane 3, 2026-11-14): this claims adding an
// enum leaves the crate's trait impls (`TraitImpls::for_crate` 0 times). An edit of a file now
// rebuilds its crate's arena, so every query of that crate runs again; reuse across edits comes
// from records, not from the memo. The first snapshot is only to re-bless.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn adding_enum_query_log() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}
$0",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    // A file's id is the same across a commit; the interned `EditionedFileId` is not.
    let file = pos.file_id.file_id(&db);
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[(TRAIT_IMPLS_FOR_CRATE, 1)],
        &expect_test::expect![[r#"
            [
                "source_root_crates",
                "TraitImpls::for_crate",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}

pub enum SomeEnum {
    A,
    B
}
";

    write_file(&mut db, file, new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[(TRAIT_IMPLS_FOR_CRATE, 0)],
        &expect_test::expect![[r#"
            []
        "#]],
    );
}

// False by design on blaze-memo (left failing; port lane 3, 2026-11-14): this claims adding a
// `use` leaves the crate's trait impls (`TraitImpls::for_crate` 0 times). An edit of a file now
// rebuilds its crate's arena, so every query of that crate runs again; reuse across edits comes
// from records, not from the memo. The first snapshot is only to re-bless.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn adding_use_query_log() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}
$0",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    // A file's id is the same across a commit; the interned `EditionedFileId` is not.
    let file = pos.file_id.file_id(&db);
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[(TRAIT_IMPLS_FOR_CRATE, 1)],
        &expect_test::expect![[r#"
            [
                "source_root_crates",
                "TraitImpls::for_crate",
            ]
        "#]],
    );

    let new_text = "
use std::collections::HashMap;

fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}
";

    write_file(&mut db, file, new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[(TRAIT_IMPLS_FOR_CRATE, 0)],
        &expect_test::expect![[r#"
            []
        "#]],
    );
}

// False by design on blaze-memo (left failing; port lane 3, 2026-11-14): this claims adding an
// inherent impl leaves the crate's trait impls (`TraitImpls::for_crate` 0 times). An edit of a
// file now rebuilds its crate's arena, so every query of that crate runs again; reuse across
// edits comes from records, not from the memo. The first snapshot is only to re-bless.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn adding_impl_query_log() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}

pub struct SomeStruct {
    field: i32,
}
$0",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    // A file's id is the same across a commit; the interned `EditionedFileId` is not.
    let file = pos.file_id.file_id(&db);
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[(TRAIT_IMPLS_FOR_CRATE, 1)],
        &expect_test::expect![[r#"
            [
                "source_root_crates",
                "TraitImpls::for_crate",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}

pub struct SomeStruct {
    field: i32,
}

impl SomeStruct {
    pub fn new(value: i32) -> Self {
        Self { field: value }
    }
}
";

    write_file(&mut db, file, new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[(TRAIT_IMPLS_FOR_CRATE, 0)],
        &expect_test::expect![[r#"
            []
        "#]],
    );
}

// FIXME(next-solver): does this test make sense with fast path?
// False by design on blaze-memo (left failing; port lane 3, 2026-11-14): the second snapshot
// claims adding a struct reruns only the queries that read the changed text (one
// `infer_body_query`, no signatures). An edit of a file now rebuilds its crate's arena, so every
// query of that crate runs again; reuse across edits comes from records, not from the memo. The
// first snapshot (the cold run's list) is only to re-bless.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn add_struct_invalidates_trait_solve() {
    let fx = fixture::db(
        "
//- /main.rs crate:main
struct SomeStruct;

trait Trait<T> {
    fn method(&self) -> T;
}
impl Trait<u32> for SomeStruct {}

fn main() {
    let s = SomeStruct;
    s.method();
    s.$0
}",
    );
    let file_id = fx.only_file();
    let mut db = fx.into_db();
    // A file's id is the same across a commit; the interned `EditionedFileId` is not.
    let file = file_id.file_id(&db);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let crate_def_map = module.def_map(&db);
            let mut defs: Vec<DefWithBodyId> = vec![];
            visit_module(&db, crate_def_map, module, &mut |it| {
                let def = match it {
                    ModuleDefId::FunctionId(it) => it.into(),
                    ModuleDefId::EnumVariantId(it) => it.into(),
                    ModuleDefId::ConstId(it) => it.into(),
                    ModuleDefId::StaticId(it) => it.into(),
                    _ => return,
                };
                defs.push(def);
            });

            for def in defs {
                let _inference_result = InferenceResult::of(&db, def);
            }
        },
        // FIXME: What does this test check for now? trait_solve_shim is no longer a query
        &[],
        &expect_test::expect![[r#"
            [
                "source_root_crates",
                "Body::of",
                "file_body_syntax",
                "AttrFlags::query",
                "ScopeInput::legacy_macro",
                "ScopeInput::name",
                "infer_body_query",
                "FunctionSignature::kept",
                "signature_syntax",
                "AttrFlags::query",
                "Body::of",
                "trait_environment_query",
                "generic_predicates_of",
                "generic_predicates_of",
                "explicit_implied_clauses_of",
                "fn_sig_for_fn",
                "elided_lifetimes",
                "trait_type_aliases",
                "body_upvars_mentioned",
                "infer_body_query",
                "FunctionSignature::kept",
                "signature_syntax",
                "trait_environment_query",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "elided_lifetimes",
                "ExprScopes::body_expr_scopes",
                "generic_predicates_of",
                "module_items_named",
                "module_trait_set",
                "ScopeInput::traits",
                "in_crate_trait_value_items",
                "candidate_slice",
                "trait_value_item",
                "InherentImpls::in_crate_for_self_ty",
                "inherent_slice",
                "TraitImpls::in_crate_and_deps_for_trait_and_self_ty",
                "loaded_crates",
                "include_macro_invoc",
                "crates_that_name",
                "TraitImpls::for_crate_and_trait",
                "trait_slice",
                "int_gates",
                "generic_predicates_of",
                "impl_super_outlives",
                "TraitImpls::blanket_in_crate_and_deps",
                "body_upvars_mentioned",
            ]
        "#]],
    );

    let new_text = "
//- /main.rs crate:main
struct AnotherStruct;

struct SomeStruct;

trait Trait<T> {
    fn method(&self) -> T;
}
impl Trait<u32> for SomeStruct {}

fn main() {
    let s = SomeStruct;
    s.method();
    s.$0
}";

    write_file(&mut db, file, new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file);
            let crate_def_map = module.def_map(&db);
            let mut defs: Vec<DefWithBodyId> = vec![];

            visit_module(&db, crate_def_map, module, &mut |it| {
                let def = match it {
                    ModuleDefId::FunctionId(it) => it.into(),
                    ModuleDefId::EnumVariantId(it) => it.into(),
                    ModuleDefId::ConstId(it) => it.into(),
                    ModuleDefId::StaticId(it) => it.into(),
                    _ => return,
                };
                defs.push(def);
            });

            for def in defs {
                let _inference_result = InferenceResult::of(&db, def);
            }
        },
        &[],
        &expect_test::expect![[r#"
            [
                "file_body_syntax",
                "Body::of",
                "AttrFlags::query",
                "ScopeInput::legacy_macro",
                "ScopeInput::name",
                "signature_syntax",
                "AttrFlags::query",
                "signature_syntax",
                "infer_body_query",
                "ExprScopes::body_expr_scopes",
                "ScopeInput::traits",
                "include_macro_invoc",
                "loaded_crates",
                "body_upvars_mentioned",
            ]
        "#]],
    );
}

fn execute_assert_events(
    db: &TestDB,
    f: impl FnOnce(),
    required: &[(&str, usize)],
    expect: &Expect,
) {
    blaze_memo::attach(db, || {
        let runs = runs_of(db, f);
        // Each run by its query's own name, the last segment of its kind's (a method of a
        // tracked impl is `Type_method`), in the order they ran.
        let executed: Vec<&str, Global> = {
            let mut names = Vec::new_in(Global);
            names.extend(
                runs.iter().map(|it| it.query.rsplit_once("::").map_or(it.query, |(_, name)| name)),
            );
            names
        };
        expect.assert_debug_eq(&executed);
        for &(kind, count) in required {
            let n = runs.iter().filter(|it| it.query == kind).count();
            assert_eq!(
                n, count,
                "Expected {kind} to be executed {count} times, but only got {n}:\n \
             Run log: {runs:#?}",
            );
        }
    });
}
