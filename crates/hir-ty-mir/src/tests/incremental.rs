use expect_test::Expect;
use hir_def::{DefWithBodyId, ModuleDefId};
use blaze_query::db::{Database as _, EventKind};
use hir_expand::fixture;

use crate::{InferenceResult, method_resolution::TraitImpls, test_db::{TestDB, TestDBExt as _}};

use super::visit_module;

#[test]
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
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[("infer_body_query", 1)],
        expect_test::expect![[r#"
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
                "lang_items",
                "crate_lang_items",
                "frozen_lang_items",
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

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[("infer_body_query", 0)],
        expect_test::expect![[r#"
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

#[test]
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
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[("infer_body_query", 3)],
        expect_test::expect![[r#"
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
                "lang_items",
                "crate_lang_items",
                "frozen_lang_items",
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

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[("infer_body_query", 1)],
        expect_test::expect![[r#"
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

#[test]
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
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 1)],
        expect_test::expect![[r#"
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

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 0), ("trait_bodies", 0)],
        expect_test::expect![[r#"
            []
        "#]],
    );
}

#[test]
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
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 1)],
        expect_test::expect![[r#"
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

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 0), ("trait_bodies", 0)],
        expect_test::expect![[r#"
            []
        "#]],
    );
}

#[test]
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
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 1)],
        expect_test::expect![[r#"
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

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 0), ("trait_bodies", 0)],
        expect_test::expect![[r#"
            []
        "#]],
    );
}

#[test]
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
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 1)],
        expect_test::expect![[r#"
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

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 0), ("trait_bodies", 0)],
        expect_test::expect![[r#"
            []
        "#]],
    );
}

// FIXME(next-solver): does this test make sense with fast path?
#[test]
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

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file_id.file_id(&db));
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
        &[("trait_solve_shim", 0)],
        expect_test::expect![[r#"
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

    db.set_file_text(file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file_id.file_id(&db));
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
        &[("trait_solve_shim", 0)],
        expect_test::expect![[r#"
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
    expect: Expect,
) {
    crate::attach_db(db, || {
        let events = db.log(f);
        let executed: Vec<String> = events
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::WillExecute { database_key } => {
                    Some(db.storage().ingredient_name(database_key.ingredient_index()).to_owned())
                }
                _ => None,
            })
            .collect();
        expect.assert_debug_eq(&executed);
        for (event, count) in required {
            let n = executed.iter().filter(|it| it.contains(event)).count();
            assert_eq!(
                n,
                *count,
                "Expected {event} to be executed {count} times, but only got {n}:\n \
             Executed: {executed:#?}\n \
             Event log: {events:#?}",
                events = events
                    .iter()
                    .filter(|event| !matches!(event.kind, EventKind::WillCheckCancellation))
                    .map(|event| { format!("{:?}", event.kind) })
                    .collect::<Vec<_>>(),
            );
        }
    });
}
