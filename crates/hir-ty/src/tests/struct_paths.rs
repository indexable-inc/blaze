//! A struct literal's or pattern's path that resolves to something it can't be is an error
//! naming what it found, as rustc's resolver (E0574, E0532), `check_struct_path` (E0071),
//! `check_pat_tuple_struct` (E0164) and `check_pat_path` (E0533) give it, never an error type
//! with no diagnostic (queue 01168).

use hir_def::{
    AssocItemId, DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{
    GenericArgsProhibitedReason, InferBodyId, InferenceDiagnostic, InferenceResult,
    PathLoweringDiagnostic, StructPathDef, StructPathFound,
};

const FIXTURE: &str = r#"
//- minicore: sized
enum E { A { x: u8 }, B(u8) }
struct S { x: u8 }
type Alias = E;
type Int = u8;
trait Tr { fn f(); }
struct U;
impl S {
    fn new() -> S { S { x: 0 } }
    const C: u8 = 0;
    fn self_in_a_path_pattern(self) { match self { Self => {} } }
    fn self_as_a_value() { let _ = Self; }
    fn self_new_in_a_tuple_pattern() { match 0u8 { Self::new(_) => {} _ => {} } }
}
impl U {
    fn unit_self_in_a_path_pattern(self) { match self { Self => {} } }
}
impl E {
    fn enum_self_as_a_value() { let _ = Self; }
}
static ST: u8 = 0;
fn f() {}
mod m {}
fn enum_literal() { let _ = E {}; }
fn alias_to_enum() { let _ = Alias {}; }
fn alias_to_builtin() { let _ = Int {}; }
fn builtin_literal() { let _ = u8 {}; }
fn trait_literal() { let _ = Tr {}; }
fn module_literal() { let _ = m {}; }
fn no_such_variant() { let _ = E::C {}; }
fn after_a_variant() { let _ = E::A::X {}; }
fn associated_type_of_a_struct() { let _ = S::X {}; }
fn record_struct_in_a_tuple_pattern(s: S) { let S(_) = s; }
fn function_in_a_tuple_pattern() { match 0u8 { f(_) => {} _ => {} } }
fn static_in_a_path_pattern() { match 0u8 { crate::ST => {} _ => {} } }
fn assoc_fn_in_a_tuple_pattern() { match 0u8 { S::new(_) => {} _ => {} } }
fn assoc_const_in_a_tuple_pattern() { match 0u8 { S::C(_) => {} _ => {} } }
fn anchored_assoc_fn_in_a_tuple_pattern() { match 0u8 { <S>::new(_) => {} _ => {} } }
fn trait_fn_in_a_tuple_pattern() { match 0u8 { Tr::f(_) => {} _ => {} } }
fn missing_item_in_a_tuple_pattern() { match 0u8 { S::nope(_) => {} _ => {} } }
fn assoc_fn_in_a_path_pattern() { match 0u8 { S::new => {} _ => {} } }
fn generics_on_an_aliased_variant() { let _ = Alias::A::<u8> { x: 0 }; }
fn generics_on_an_aliased_variant_value() { let _ = Alias::B::<u8>(0); }
fn generics_on_an_aliased_variant_pattern(e: E) { match e { Alias::B::<u8>(_) => {} _ => {} } }
struct P { x: u8 }
enum Q { V(u8), W { x: u8 } }
trait HasA { type A; }
trait HasF { fn f(); }
impl HasF for P { fn f() {} }
fn assoc_struct_through_a_param<T: HasA<A = P>>(p: P) {
    let _ = T::A { x: 0 };
    let T::A { x: _ } = p;
}
fn assoc_variant_through_a_param<T: HasA<A = Q>>(q: Q) {
    let _ = T::A::W { x: 0 };
    match q { T::A::V(_) => {} _ => {} }
}
fn unnormalized_assoc_through_a_param<T: HasA>() { let _ = T::A {}; }
fn assoc_fn_through_a_param_in_a_tuple_pattern<T: HasA<A = P>>() { match 0u8 { T::A::f(_) => {} _ => {} } }
fn fine(e: E) {
    let _ = S { x: 0 };
    let _ = E::A { x: 0 };
    match e { E::B(_) => {} _ => {} }
}
"#;

/// The inference diagnostics of the fixture's function `name`, free or in an impl.
fn diagnostics(name: &str) -> Vec<InferenceDiagnostic> {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let mut functions = Vec::new();
        for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
            let scope = module.scope(&db);
            functions.extend(scope.declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            }));
            for it in scope.impls() {
                functions.extend(it.impl_items(&db).items.iter().filter_map(|&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                }));
            }
        }
        let function = functions
            .into_iter()
            .find(|&it| FunctionSignature::of(&db, it).name.as_str() == name)
            .unwrap_or_else(|| panic!("the fixture's `{name}`"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)));
        infer.diagnostics().to_vec()
    })
}

/// What each `NotAStructPath` of the fixture's function `name` found, with whether it has a
/// type.
fn found(name: &str) -> Vec<(StructPathFound, bool)> {
    diagnostics(name)
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::NotAStructPath { found, ty, .. } => Some((*found, ty.is_some())),
            _ => None,
        })
        .collect()
}

#[track_caller]
fn check(name: &str, expected: StructPathFound) {
    let with_ty = expected == StructPathFound::Ty;
    assert_eq!(found(name), [(expected, with_ty)], "`{name}`");
}

#[test]
fn an_enum_as_a_struct_literal_is_e0574() {
    check("enum_literal", StructPathFound::Def(StructPathDef::Enum));
}

#[test]
fn an_alias_to_an_enum_is_e0071() {
    check("alias_to_enum", StructPathFound::Ty);
}

#[test]
fn an_alias_to_a_builtin_is_e0071() {
    check("alias_to_builtin", StructPathFound::Ty);
}

#[test]
fn a_builtin_a_trait_and_a_module_are_e0574() {
    check("builtin_literal", StructPathFound::Def(StructPathDef::BuiltinType));
    check("trait_literal", StructPathFound::Def(StructPathDef::Trait));
    check("module_literal", StructPathFound::Def(StructPathDef::Module));
}

#[test]
fn a_missing_variant_is_e0599() {
    check("no_such_variant", StructPathFound::NoVariant { segment: 1 });
}

#[test]
fn a_segment_after_a_variant_is_e0433() {
    check("after_a_variant", StructPathFound::AfterVariant { segment: 2 });
}

#[test]
fn an_associated_type_of_a_struct_is_e0223() {
    check("associated_type_of_a_struct", StructPathFound::AssocType { segment: 1 });
}

#[test]
fn a_record_struct_or_function_as_a_tuple_struct_pattern_is_e0532() {
    check("record_struct_in_a_tuple_pattern", StructPathFound::Def(StructPathDef::Struct));
    check("function_in_a_tuple_pattern", StructPathFound::Def(StructPathDef::Function));
}

#[test]
fn a_static_as_a_path_pattern_is_e0532() {
    check("static_in_a_path_pattern", StructPathFound::Def(StructPathDef::Static));
}

#[test]
fn an_associated_item_after_a_type_in_a_tuple_struct_pattern_is_e0164() {
    check("assoc_fn_in_a_tuple_pattern", StructPathFound::TypeRelative(StructPathDef::AssocFn));
    check("assoc_const_in_a_tuple_pattern", StructPathFound::TypeRelative(StructPathDef::AssocConst));
    check("anchored_assoc_fn_in_a_tuple_pattern", StructPathFound::TypeRelative(StructPathDef::AssocFn));
    // Before, `new` was lowered as an associated type of `Self` (E0223).
    let found = diagnostics("self_new_in_a_tuple_pattern");
    assert!(
        matches!(
            &found[..],
            [InferenceDiagnostic::NotAStructPath {
                found: StructPathFound::TypeRelative(StructPathDef::AssocFn),
                ..
            }]
        ),
        "{found:?}"
    );
}

#[test]
fn a_trait_function_in_a_tuple_struct_pattern_is_e0532() {
    check("trait_fn_in_a_tuple_pattern", StructPathFound::Def(StructPathDef::AssocFn));
}

#[test]
fn a_missing_item_after_a_type_in_a_tuple_struct_pattern_is_e0599() {
    let found = diagnostics("missing_item_in_a_tuple_pattern");
    assert!(
        matches!(&found[..], [InferenceDiagnostic::UnresolvedAssocItem { .. }]),
        "{found:?}"
    );
}

#[test]
fn an_associated_function_as_a_path_pattern_is_e0533() {
    check("assoc_fn_in_a_path_pattern", StructPathFound::TypeRelative(StructPathDef::AssocFn));
}

#[test]
fn a_self_that_is_not_a_unit_struct_as_a_path_pattern_is_e0533() {
    check("self_in_a_path_pattern", StructPathFound::TypeRelative(StructPathDef::SelfCtor));
}

#[test]
fn a_unit_struct_self_as_a_path_pattern_has_none() {
    let found = diagnostics("unit_self_in_a_path_pattern");
    assert!(found.is_empty(), "rustc accepts a unit struct's `Self` pattern: {found:?}");
}

#[test]
fn a_self_without_a_constructor_as_a_value_is_an_error() {
    for name in ["self_as_a_value", "enum_self_as_a_value"] {
        let found = diagnostics(name);
        assert!(
            matches!(&found[..], [InferenceDiagnostic::SelfCtorNotTupleOrUnit { .. }]),
            "`{name}`: {found:?}"
        );
    }
}

#[test]
fn generic_arguments_on_a_variant_through_an_alias_are_e0109() {
    let found = diagnostics("generics_on_an_aliased_variant");
    assert!(
        matches!(
            &found[..],
            [InferenceDiagnostic::PathDiagnostic {
                diag: PathLoweringDiagnostic::GenericArgsProhibited {
                    segments,
                    reason: GenericArgsProhibitedReason::TypeRelativeVariant,
                },
                ..
            }] if segments[..] == [1]
        ),
        "{found:?}"
    );
}

#[test]
fn generic_arguments_on_a_variant_through_an_alias_in_a_value_or_tuple_pattern_are_e0109() {
    for name in ["generics_on_an_aliased_variant_value", "generics_on_an_aliased_variant_pattern"] {
        let found = diagnostics(name);
        assert!(
            matches!(
                &found[..],
                [InferenceDiagnostic::PathDiagnostic {
                    diag: PathLoweringDiagnostic::GenericArgsProhibited {
                        segments,
                        reason: GenericArgsProhibitedReason::TypeRelativeVariant,
                    },
                    ..
                }] if segments[..] == [1]
            ),
            "`{name}`: {found:?}"
        );
    }
}

#[test]
fn an_associated_type_of_a_type_parameter_resolves_through_its_bounds() {
    for name in ["assoc_struct_through_a_param", "assoc_variant_through_a_param"] {
        let found = diagnostics(name);
        assert!(found.is_empty(), "`{name}`: rustc accepts these: {found:?}");
    }
}

#[test]
fn an_associated_type_of_a_type_parameter_that_does_not_normalize_is_e0071() {
    check("unnormalized_assoc_through_a_param", StructPathFound::Ty);
}

#[test]
fn an_associated_function_through_a_type_parameter_in_a_tuple_struct_pattern_is_e0164() {
    check(
        "assoc_fn_through_a_param_in_a_tuple_pattern",
        StructPathFound::TypeRelative(StructPathDef::AssocFn),
    );
}

#[test]
fn struct_literals_and_patterns_that_resolve_have_none() {
    let found = found("fine");
    assert!(found.is_empty(), "{found:?}");
}
