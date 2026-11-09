//! blaze's error unions (`#![blaze::error_unions]`): `A | B` is core's error union of its
//! members in one canonical order, so `A | B`, `B | A`, `A | A | B` and `A | (B | A)` are one
//! type; a member that is not a known named type is refused naming why, and a crate without
//! the attribute is refused. A member, or a union of some of the members, widens to a union:
//! by coercion where a union is expected, and by `?` through core's `From`; anything else is
//! refused. A union is `Debug`, `Display`, `Error`, `Clone` and `Hash` when its members are.

use base_db::{Crate, Db, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, ModuleDefId, VariantId,
    expr_store::Body,
    lang_item::lang_items,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature, TraitSignature},
};
use hir_expand::fixture;
use rustc_type_ir::{TyKind, inherent::IntoKind as _};

use crate::{
    Adjust, ErrorUnionRefusal, InferBodyId, InferenceDiagnostic, InferenceResult,
    TyLoweringDiagnostic,
    db::HirDatabase,
    next_solver::{DbInterner, Ty},
    solver_errors::SolverDiagnosticKind,
};

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

/// `other` lacks the attribute, so its file parses as Rust's though it shares `main`'s source
/// root (a file is read under its own crate's extensions): its `A | B` is the syntax error it
/// is in Rust, and no union of it reaches hir-ty. `main`'s file, read under `main`'s, has none.
#[test]
fn a_crate_without_the_attribute_parses_no_union() {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let (main, other) = (crate_named(&db, "main"), crate_named(&db, "other"));
        assert!(main.extensions(&db).error_unions());
        assert!(other.extensions(&db).is_none());
        let (main, other) = (main.root_file_id(&db), other.root_file_id(&db));
        assert!(main.extensions(&db).error_unions() && other.extensions(&db).is_none());
        let errors = main.parse(&db).errors();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!other.parse(&db).errors().is_empty(), "`A | B` is no type in `other`");
    })
}

/// Bodies that widen into unions, in a crate with the attribute (`main`), and one without it
/// (`plain`) that calls `main`'s functions taking a union.
const BODIES: &str = r#"
//- minicore: sized, error_union, result, try, from, fmt, error, clone, hash
//- /main.rs crate:main
#![blaze::error_unions]
use core::{error::Error, fmt};

pub struct A;
pub struct B;
pub struct C;
macro_rules! error {
    ($($t:ident)*) => {$(
        impl fmt::Debug for $t {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result { loop {} }
        }
        impl fmt::Display for $t {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result { loop {} }
        }
        impl Error for $t {}
        impl Clone for $t {
            fn clone(&self) -> Self { loop {} }
        }
    )*};
}
error!(A B C);

pub type AB = A | B;
pub type ABC = A | B | C;

pub fn take(_: AB) {}
fn needs_error<E: Error>(_: &E) {}
fn needs_display<E: fmt::Display>(_: &E) {}
fn needs_clone<E: Clone>(_: &E) {}

fn in_return(a: A) -> AB { a }
fn as_argument(b: B) { take(b) }
fn in_typed_let(b: B) { let _x: A | B = b; }
fn in_err(a: A) -> Result<(), AB> { Err(a) }
fn subset_union(ab: AB) -> ABC { ab }
fn same_union(ab: B | A) -> AB { ab }

fn try_single(r: Result<(), A>) -> Result<(), AB> { r?; Ok(()) }
fn try_subset(r: Result<(), AB>) -> Result<(), ABC> { r?; Ok(()) }
fn try_same(r: Result<(), AB>) -> Result<(), B | A> { r?; Ok(()) }
fn try_not_a_member(r: Result<(), C>) -> Result<(), AB> { r?; Ok(()) }
fn try_not_a_subset(r: Result<(), ABC>) -> Result<(), AB> { r?; Ok(()) }

fn delegated(e: AB) {
    needs_error(&e);
    needs_display(&e);
    needs_clone(&e);
}
//- /plain.rs crate:plain deps:main
fn not_gated() {
    main::take(main::A);
}
"#;

/// The function `name` of `krate`.
fn function(db: &Db, krate: &str, name: &str) -> FunctionId {
    crate_def_map(db, crate_named(db, krate))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the fixture's `{krate}::{name}`"))
}

/// Infers `krate`'s function `name` and passes its result and how many of its expressions
/// were widened into a union by coercion.
fn inferred(krate: &str, name: &str, check: impl FnOnce(&Db, &InferenceResult<'_>, usize)) {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let func = function(&db, krate, name);
        let result =
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)));
        let widened = Body::of(&db, DefWithBodyId::FunctionId(func))
            .exprs()
            .filter(|(id, _)| {
                result
                    .expr_adjustment(*id)
                    .is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::ErrorUnion)))
            })
            .count();
        check(&db, result, widened);
    })
}

fn assert_clean(name: &str, result: &InferenceResult<'_>) {
    assert!(result.diagnostics().is_empty(), "{name}: {:?}", result.diagnostics());
    assert!(!result.has_type_mismatches(), "{name} has a type mismatch");
}

#[test]
fn a_member_coerces_to_the_union_wherever_a_union_is_expected() {
    for name in ["in_return", "as_argument", "in_typed_let", "in_err", "subset_union"] {
        inferred("main", name, |_, result, widened| {
            assert_clean(name, result);
            assert_eq!(widened, 1, "{name} widens once");
        });
    }
}

/// `B | A` is `A | B`: they unify, no widening.
#[test]
fn the_same_union_is_not_widened() {
    inferred("main", "same_union", |_, result, widened| {
        assert_clean("same_union", result);
        assert_eq!(widened, 0);
    });
}

/// `plain` lacks the attribute: `main::take`'s union parameter takes no `A`.
#[test]
fn a_crate_without_the_attribute_does_not_coerce() {
    inferred("plain", "not_gated", |_, result, widened| {
        assert_eq!(widened, 0);
        assert!(result.has_type_mismatches(), "{:?}", result.diagnostics());
    });
}

#[test]
fn question_mark_widens_a_member_or_a_subset() {
    for name in ["try_single", "try_subset", "try_same"] {
        inferred("main", name, |_, result, _| assert_clean(name, result));
    }
}

/// `?` from a type that is not a member, or a union that is not a subset, is refused: the
/// bound that does not hold is the union's `From` of it, or under it `UnionFrom`, which the
/// diagnostic names the missing members of.
#[test]
fn question_mark_refuses_what_is_not_a_subset() {
    for name in ["try_not_a_member", "try_not_a_subset"] {
        inferred("main", name, |db, result, _| {
            let interner = DbInterner::new_no_crate(db);
            let union_from =
                lang_items(db, crate_named(db, "main")).ErrorUnionFrom.expect("the lang item");
            let from_or_union_from = result.diagnostics().iter().any(|it| match it {
                InferenceDiagnostic::SolverDiagnostic(it) => match &it.kind {
                    SolverDiagnosticKind::TraitUnimplemented {
                        trait_predicate,
                        parent_trait_predicates,
                        ..
                    } => std::iter::once(trait_predicate)
                        .chain(parent_trait_predicates)
                        .any(|it| {
                            let id = it.get(interner).def_id().0;
                            id == union_from || TraitSignature::of(db, id).name.as_str() == "From"
                        }),
                    _ => false,
                },
                _ => false,
            });
            assert!(from_or_union_from, "{name}: {:?}", result.diagnostics());
        });
    }
}

/// core's `Debug`, `Display`, `Error` and `Clone` of a union hold when its members' do.
#[test]
fn a_union_is_an_error_when_its_members_are() {
    inferred("main", "delegated", |_, result, _| assert_clean("delegated", result));
}
