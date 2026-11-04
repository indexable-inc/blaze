//! A field binder `where for<F: Field<T>> F::Type: Show` (`#![feature(blaze_comptime)]`,
//! docs/v2/plan/design/comptime.md, "The bounds, checked once"), as the trait solver proves it:
//! of a concrete container, once per field in declaration order, an unmet one naming the field;
//! of a generic one, from the caller's own binder over it, never expanded.

use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId,
    nameres::crate_def_map,
    signatures::{FunctionSignature, VariantFields},
};
use hir_expand::fixture;

use crate::{
    InferBodyId, InferenceDiagnostic, InferenceResult,
    comptime::ReflectedMember,
    solver_errors::{SolverDiagnostic, SolverDiagnosticKind},
};

/// A `core` with what the solver reads of `library/core/src/{field,reflect}.rs`.
const REFLECT: &str = r#"
#[lang = "field_representing_type"]
/// Zero-sized, as core's (`library/core/src/field.rs`).
pub struct FieldRepresentingType<T: ?Sized, const VARIANT: u32, const FIELD: u32> {
    _phantom: core::marker::PhantomData<fn(T) -> T>,
}
#[lang = "field"]
pub unsafe trait FieldOf: Copy {
    #[lang = "field_base"]
    type Base: ?Sized;
    #[lang = "field_type"]
    type Type: ?Sized;
}
#[lang = "blaze_struct"]
pub unsafe trait Struct {}
#[lang = "blaze_field"]
pub unsafe trait Field<B: ?Sized>: FieldOf {}
#[lang = "blaze_variant"]
pub unsafe trait Variant<B>: Copy {}
#[lang = "blaze_variants"]
pub struct Variants<T: ?Sized>(*const T);

pub trait Show {}
impl Show for u8 {}
pub struct Shown;
impl Show for Shown {}
pub struct NotShown;

fn need<T>() where for<F: Field<T>> F::Type: Show {}
"#;

const BODIES: &str = r#"
struct Holds { pub a: u8, pub b: Shown }
struct Fails { pub a: u8, pub b: NotShown, pub c: NotShown }

fn holds() { need::<Holds>(); }
fn fails() { need::<Fails>(); }
fn generic<T>() where for<F: Field<T>> F::Type: Show { need::<T>(); }
"#;

fn function(db: &base_db::Db, name: &str) -> FunctionId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the fixture's `{name}`"))
}

fn diagnostics<'db>(db: &'db base_db::Db, name: &str) -> &'db [InferenceDiagnostic] {
    let func = function(db, name);
    InferenceResult::of(db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)))
        .diagnostics()
}

fn with_db(check: impl FnOnce(&base_db::Db)) {
    let fixture = format!(
        "//- minicore: sized, copy, clone, phantom_data\n#![feature(blaze_comptime, lang_items)]\n{REFLECT}{BODIES}"
    );
    let db = fixture::db(&fixture).into_db();
    blaze_query::db::attach(&db, || check(&db));
}

#[test]
fn a_field_binder_holds_when_every_field_meets_it() {
    with_db(|db| {
        let diagnostics = diagnostics(db, "holds");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    });
}

#[test]
fn an_unmet_field_binder_names_the_first_field_that_fails() {
    with_db(|db| {
        let diagnostics = diagnostics(db, "fails");
        let unmet: Vec<_> = diagnostics
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::SolverDiagnostic(SolverDiagnostic {
                    kind:
                        SolverDiagnosticKind::ReflectedMemberUnmet { member, member_ty, inner, .. },
                    ..
                }) => Some((*member, member_ty.is_some(), inner)),
                _ => None,
            })
            .collect();
        let [(ReflectedMember::Field(field), true, inner)] = unmet[..] else {
            panic!("one error naming a field, not {diagnostics:?}");
        };
        let name =
            VariantFields::of(db, field.parent).fields()[field.local_id].name.as_str().to_owned();
        assert_eq!(name, "b", "`b` is the first field that is not `Show`; `c` is not reported");
        assert!(
            matches!(**inner, SolverDiagnosticKind::TraitUnimplemented { .. }),
            "the reason is `NotShown: Show`, not {inner:?}"
        );
    });
}

#[test]
fn a_field_binder_over_a_generic_container_is_met_by_the_callers_own() {
    with_db(|db| {
        let diagnostics = diagnostics(db, "generic");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    });
}
