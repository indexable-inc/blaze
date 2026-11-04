//! The building blocks over fixtures: the intrinsic table's classes, the identity spelling
//! ([`identity`]), and the differential programs. Which instances of one body are one is
//! compile identity's, tested in `hir-ty-identity` (`src/tests/compile.rs`).
//!
//! The programs in `programs/` are those cases as whole programs, with what the pinned
//! nightly's rustc prints for each, for a differential run with sharing on
//! (`harness/v2/share.py`).

mod db;
mod identity;

use hir_def::{
    FunctionId, ModuleDefId, TypeAliasId,
    nameres::crate_def_map,
    signatures::{FunctionSignature, TypeAliasSignature},
};
use hir_expand::fixture;
use hir_ty::intrinsic::Intrinsic;

use crate::{Class, classify};
use db::TestDB;

fn declarations(db: &TestDB) -> Vec<ModuleDefId> {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .collect()
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    let found: Vec<FunctionId> = declarations(db)
        .into_iter()
        .filter_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} functions named `{name}`", found.len()),
    }
}

#[track_caller]
fn alias(db: &TestDB, name: &str) -> TypeAliasId {
    let found: Vec<TypeAliasId> = declarations(db)
        .into_iter()
        .filter_map(|def| match def {
            ModuleDefId::TypeAliasId(it)
                if TypeAliasSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} type aliases named `{name}`", found.len()),
    }
}

#[test]
fn reflection_intrinsics_read_identity() {
    for index in 0..Intrinsic::COUNT as u16 {
        let intrinsic = Intrinsic::from_index(index).expect("an intrinsic at each index");
        let name = intrinsic.to_string();
        let class = classify(intrinsic);
        // `type_id_eq(a: TypeId, b: TypeId)` has no generic argument: it compares two values
        // whose identity the `type_id` calls that made them read.
        if (name.starts_with("type_id") && name != "type_id_eq") || name.starts_with("type_name") {
            assert_eq!(class, Class::Identity, "`{name}` tells types apart");
        }
        // `blaze_int_cast` is `as` between integers: its types' class and valid range.
        if intrinsic.is_blaze() && intrinsic != Intrinsic::BlazeIntCast {
            assert_eq!(class, Class::Identity, "`{name}` reads names and attributes");
        }
        if name.starts_with("atomic_") || name.starts_with("simd_") {
            assert_eq!(class, Class::Layout, "`{name}` reads only layouts");
        }
    }
}

#[test]
fn intrinsic_classes() {
    for (intrinsic, class) in [
        (Intrinsic::TypeId, Class::Identity),
        (Intrinsic::TypeName, Class::Identity),
        (Intrinsic::AssertInhabited, Class::Identity),
        (Intrinsic::AssertZeroValid, Class::Identity),
        (Intrinsic::ConstEvalSelect, Class::Identity),
        (Intrinsic::SizeOf, Class::Layout),
        (Intrinsic::AlignOf, Class::Layout),
        (Intrinsic::NeedsDrop, Class::Layout),
        (Intrinsic::Transmute, Class::Layout),
        (Intrinsic::BlazeIntCast, Class::Layout),
        (Intrinsic::DiscriminantValue, Class::Layout),
        (Intrinsic::VariantCount, Class::Layout),
        (Intrinsic::CallerLocation, Class::Layout),
        (Intrinsic::Autodiff, Class::PerInstance),
        (Intrinsic::Offload, Class::PerInstance),
    ] {
        assert_eq!(classify(intrinsic), class, "{intrinsic}");
    }
}

/// Every differential program has what rustc prints beside it.
#[test]
fn every_program_has_its_expected_output() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("programs");
    let mut programs = 0;
    for entry in std::fs::read_dir(&dir).expect("the programs directory") {
        let path = entry.expect("an entry").path();
        if path.extension().is_some_and(|it| it == "rs") {
            programs += 1;
            let expected = path.with_extension("stdout");
            assert!(expected.exists(), "{} has no {}", path.display(), expected.display());
        }
    }
    assert!(programs >= 9, "{programs} programs");
}
