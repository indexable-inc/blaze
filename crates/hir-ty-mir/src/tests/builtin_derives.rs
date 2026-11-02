use expect_test::{Expect, expect};
use hir_def::nameres::crate_def_map;
use itertools::Itertools;
use stdx::format_to;
use hir_expand::fixture;

use crate::{builtin_derive::impl_trait, next_solver::DbInterner};

fn check_trait_refs(#[rust_analyzer::rust_fixture] ra_fixture: &str, expectation: Expect) {
    let db = fixture::db(ra_fixture).into_db();
    let def_map = crate_def_map(&db, fixture::test_crate(&db));

    let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
    crate::attach_db(&db, || {
        let mut trait_refs = Vec::new();
        for (_, module) in def_map.modules(&db) {
            for derive in module.scope(&db).builtin_derive_impls() {
                let trait_ref = impl_trait(interner, derive).skip_binder();
                trait_refs.push(format!("{trait_ref:?}"));
            }
        }

        expectation.assert_eq(&trait_refs.join("\n"));
    });
}

fn check_predicates(#[rust_analyzer::rust_fixture] ra_fixture: &str, expectation: Expect) {
    let db = fixture::db(ra_fixture).into_db();
    let def_map = crate_def_map(&db, fixture::test_crate(&db));

    crate::attach_db(&db, || {
        let mut predicates = String::new();
        for (_, module) in def_map.modules(&db) {
            for derive in module.scope(&db).builtin_derive_impls() {
                let preds =
                    crate::builtin_derive::predicates(&db, derive).all_predicates().skip_binder();
                format_to!(
                    predicates,
                    "{}\n\n",
                    preds.format_with("\n", |pred, formatter| formatter(&format_args!("{pred:?}"))),
                );
            }
        }

        expectation.assert_eq(&predicates);
    });
}

#[test]
fn simple_macros_trait_ref() {
    check_trait_refs(
        r#"
//- minicore: derive, clone, copy, eq, ord, hash, fmt

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Simple;

trait Trait {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct WithGenerics<'a, T: Trait, const N: usize>(&'a [T; N]);
        "#,
        expect![[r#"
                Simple: Debug
                Simple: Clone
                Simple: Copy
                Simple: PartialEq<[Simple]>
                Simple: Eq
                Simple: PartialOrd<[Simple]>
                Simple: Ord
                Simple: Hash
                WithGenerics<#0, #1, #2>: Debug
                WithGenerics<#0, #1, #2>: Clone
                WithGenerics<#0, #1, #2>: Copy
                WithGenerics<#0, #1, #2>: PartialEq<[WithGenerics<#0, #1, #2>]>
                WithGenerics<#0, #1, #2>: Eq
                WithGenerics<#0, #1, #2>: PartialOrd<[WithGenerics<#0, #1, #2>]>
                WithGenerics<#0, #1, #2>: Ord
                WithGenerics<#0, #1, #2>: Hash"#]],
    );
}

#[test]
fn coerce_pointee_trait_ref() {
    check_trait_refs(
        r#"
//- minicore: derive, coerce_pointee
use core::marker::CoercePointee;

#[derive(CoercePointee)]
struct Simple<T: ?Sized>(*const T);

#[derive(CoercePointee)]
struct MultiGenericParams<'a, T, #[pointee] U: ?Sized, const N: usize>(*const U);
        "#,
        expect![[r#"
                Simple<#0>: CoerceUnsized<[Simple<#1>]>
                Simple<#0>: DispatchFromDyn<[Simple<#1>]>
                MultiGenericParams<#0, #1, #2, #3>: CoerceUnsized<[MultiGenericParams<#0, #1, #4, #3>]>
                MultiGenericParams<#0, #1, #2, #3>: DispatchFromDyn<[MultiGenericParams<#0, #1, #4, #3>]>"#]],
    );
}

#[test]
fn reborrow_trait_ref() {
    check_trait_refs(
        r#"
//- minicore: reborrow
use core::marker::Reborrow;

#[derive(Reborrow)]
struct Marker<'a, T>(&'a mut T);
        "#,
        expect![[r#"
                Marker<#0, #1>: Reborrow"#]],
    );
}

#[test]
fn simple_macros_predicates() {
    check_predicates(
        r#"
//- minicore: derive, clone, copy, eq, ord, hash, fmt

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Simple;

trait Trait {
    type Assoc;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct WithGenerics<'a, T: Trait, const N: usize>(&'a [T; N], T::Assoc);
        "#,
        expect![[r#"
















            Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Debug, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. }): Debug, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Clone, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. }): Clone, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Copy, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. }): Copy, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: PartialEq<[#1]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. }): PartialEq<[Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. })]>, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Eq, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. }): Eq, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: PartialOrd<[#1]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. }): PartialOrd<[Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. })]>, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Ord, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. }): Ord, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Hash, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(Alias(No, Alias { kind: Projection { def_id: TypeAliasId("Assoc") }, args: [#1], .. }): Hash, polarity:Positive), bound_vars: [] })

        "#]],
    );
}

#[test]
fn reborrow_predicates() {
    check_predicates(
        r#"
//- minicore: reborrow
use core::marker::Reborrow;

trait Trait {}

#[derive(Reborrow)]
struct Marker<'a, T: Trait, const N: usize>(&'a mut [T; N]);
        "#,
        expect![[r#"
                Clause(Binder { value: TraitClause(#1: Trait, polarity:Positive), bound_vars: [] })
                Clause(Binder { value: ConstArgHasType(#2, usize), bound_vars: [] })
                Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })

            "#]],
    );
}

#[test]
fn coerce_pointee_predicates() {
    check_predicates(
        r#"
//- minicore: derive, coerce_pointee
use core::marker::CoercePointee;

#[derive(CoercePointee)]
struct Simple<T: ?Sized>(*const T);

trait Trait<T> {}

#[derive(CoercePointee)]
struct MultiGenericParams<'a, T, #[pointee] U: ?Sized, const N: usize>(*const U)
where
    T: Trait<U>,
    U: Trait<U>;
        "#,
        expect![[r#"
            Clause(Binder { value: TraitClause(#0: MetaSized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: MetaSized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#0: Unsize<[#1]>, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#0: MetaSized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: MetaSized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#0: Unsize<[#1]>, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait<[#2]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#2: Trait<[#2]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#3, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#2: MetaSized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Trait<[#4]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#4: Trait<[#4]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#4: MetaSized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#2: Unsize<[#4]>, polarity:Positive), bound_vars: [] })

            Clause(Binder { value: TraitClause(#1: Trait<[#2]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#2: Trait<[#2]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: ConstArgHasType(#3, usize), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Sized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#2: MetaSized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#1: Trait<[#4]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#4: Trait<[#4]>, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#4: MetaSized, polarity:Positive), bound_vars: [] })
            Clause(Binder { value: TraitClause(#2: Unsize<[#4]>, polarity:Positive), bound_vars: [] })

        "#]],
    );
}
