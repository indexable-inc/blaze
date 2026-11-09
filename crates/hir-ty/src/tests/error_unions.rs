//! blaze's error unions (`#![blaze::error_unions]`): `A | B` is core's error union of its
//! members in one canonical order, so `A | B`, `B | A`, `A | A | B` and `A | (B | A)` are one
//! type; a member that is not a known named type is refused naming why, and a crate without
//! the attribute is refused.

use base_db::{Crate, Db, all_crates};
use hir_def::{AdtId, ModuleDefId, VariantId, nameres::crate_def_map, signatures::StructSignature};
use hir_expand::fixture;
use rustc_type_ir::{TyKind, inherent::IntoKind as _};

use crate::{ErrorUnionRefusal, TyLoweringDiagnostic, db::HirDatabase, next_solver::Ty};

const FIXTURE: &str = r#"
//- minicore: sized, error_union
//- /main.rs crate:main
#![blaze::error_unions]
struct A;
struct B;
struct V<T> {
    ab: A | B,
    ba: B | A,
    aab: A | A | B,
    nested: A | (B | A),
    one: A | A,
    param: A | T,
    imp: A | impl Sized,
    reference: A | &'static B,
}
//- /other.rs crate:other
struct A;
struct B;
struct V {
    ab: A | B,
}
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// The field types of `krate`'s struct `V`, in order, and the diagnostics of lowering them.
fn fields(krate: &str, check: impl FnOnce(&Db, &[Ty<'_>], &[TyLoweringDiagnostic])) {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let krate = crate_named(&db, krate);
        let v = crate_def_map(&db, krate)
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::AdtId(AdtId::StructId(it))
                    if StructSignature::of(&db, it).name.as_str() == "V" =>
                {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `V`");
        let lowered = db.field_types_with_diagnostics(VariantId::StructId(v));
        let tys = lowered
            .value
            .as_slice()
            .iter()
            .map(|it| it.ty().instantiate_identity().skip_norm_wip())
            .collect::<Vec<_>>();
        check(&db, &tys, lowered.diagnostics());
    })
}

fn refusals(diagnostics: &[TyLoweringDiagnostic]) -> Vec<ErrorUnionRefusal> {
    diagnostics
        .iter()
        .filter_map(|it| match it {
            TyLoweringDiagnostic::ErrorUnionRefused { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

#[test]
fn a_union_is_one_type_whatever_the_order_and_repeats() {
    fields("main", |_, tys, _| {
        let [ab, ba, aab, nested, one, ..] = tys else { panic!("{tys:?}") };
        assert_eq!(ab, ba);
        assert_eq!(ab, aab);
        assert_eq!(ab, nested, "a member that is a union gives its members");
        match ab.kind() {
            TyKind::Adt(_, args) => assert!(
                args.types().any(|it| matches!(it.kind(), TyKind::Tuple(members) if members.len() == 2)),
                "{ab:?}",
            ),
            other => panic!("the error union struct, not {other:?}"),
        }
        assert!(matches!(one.kind(), TyKind::Adt(..)), "`A | A` is `A`: {one:?}");
        assert_ne!(ab, one);
    });
}

#[test]
fn a_member_that_is_not_a_known_named_type_is_refused_naming_why() {
    fields("main", |_, _, diagnostics| {
        assert_eq!(
            refusals(diagnostics),
            [ErrorUnionRefusal::TypeParam, ErrorUnionRefusal::NotNamed, ErrorUnionRefusal::NotNamed],
            "{diagnostics:?}",
        );
    });
}

/// `other` shares `main`'s source root, so its file parses with the union; hir-ty refuses it,
/// as the crate lacks the attribute.
#[test]
fn a_crate_without_the_attribute_is_refused() {
    fields("other", |_, _, diagnostics| {
        assert_eq!(refusals(diagnostics), [ErrorUnionRefusal::NotEnabled], "{diagnostics:?}");
    });
}
