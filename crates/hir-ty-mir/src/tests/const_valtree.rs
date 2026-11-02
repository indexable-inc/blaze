//! The value-tree error of a const argument, asked once per anonymous constant: rustc's
//! "maximum number of nodes exceeded" for a value of more than `VALTREE_MAX_NODES` leaves.
//! Here, not in hir-ty, because the value is evaluated by MIR, which this crate builds.

use base_db::Db;
use hir_def::{
    FunctionId, GenericDefId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;
use hir_ty::{
    const_valtree::{ValTreeError, anon_const_valtree_error},
    db::signature_anon_consts_and_diagnostics,
};

fn function_named(db: &Db, name: &str) -> FunctionId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}` in the fixture"))
}

/// The value-tree error of each anonymous constant in `name`'s signature.
fn errors_in_signature(db: &Db, name: &str) -> Vec<Option<ValTreeError>> {
    let def = GenericDefId::FunctionId(function_named(db, name));
    signature_anon_consts_and_diagnostics(db, def)
        .into_iter()
        .flat_map(|(anon_consts, _)| anon_consts.iter())
        .map(|&anon| anon_const_valtree_error(db, anon).clone())
        .collect()
}

#[test]
fn a_const_argument_of_more_nodes_than_a_value_tree_holds_is_an_overflow() {
    let db = fixture::db(
        r#"
//- minicore: sized
#![feature(adt_const_params)]
struct Big<const N: [u8; 100001]>;
struct Small<const M: u8>;
fn big(_: Big<{ [0u8; 100001] }>) {}
fn small(_: Small<{ 1 + 2 }>) {}
fn generic<T>(_: Small<{ 1 + 3 }>, _: T) {}
"#,
    )
    .into_db();
    hir_ty::attach_db(&db, || {
        assert_eq!(errors_in_signature(&db, "big"), [Some(ValTreeError::NodesOverflow)]);
        assert_eq!(errors_in_signature(&db, "small"), [None]);
        assert_eq!(
            errors_in_signature(&db, "generic"),
            [None],
            "an argument in a generic item waits for a use with concrete arguments"
        );
    });
}
