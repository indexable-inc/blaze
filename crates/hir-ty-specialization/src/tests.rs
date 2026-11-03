//! The specialization rules on small crates: which impls each mode refuses, and by which rule.

use base_db::Db;
use hir_def::{nameres::crate_def_map, signatures::TraitSignature};
use hir_expand::fixture;
use hir_ty::db::HirDatabase as _;
use hir_ty_mir as _;

use crate::{Rule, check_impl};

/// The impls of a crate, one specializing impl per trait: what each mode's rules say about
/// them is keyed by the trait's name.
const IMPLS: &str = r#"
#[rustc_unsafe_specialization_marker]
trait Marker {}
#[blaze::specialization_trait]
trait Always {}
trait Plain {}

trait A { fn f(); }
impl<T> A for T { default fn f() {} }
impl A for &'static u8 { fn f() {} }

trait B { fn f(); }
impl<T> B for T { default fn f() {} }
impl<U> B for (U, U) { fn f() {} }

trait C { fn f(); }
impl<T> C for T { default fn f() {} }
impl<T: Plain> C for T { fn f() {} }

trait D { fn f(); }
impl<T> D for T { default fn f() {} }
impl<T: Always> D for T { fn f() {} }

impl<X: Plain> Always for X {}

trait E { fn f(); }
impl<T> E for T { default fn f() {} }
impl<T> E for Option<T> {}

trait M { fn f(); }
impl<T> M for T { default fn f() {} }
impl<T: Marker> M for T { fn f() {} }

trait G { type Out; fn f(); }
impl<T> G for T { default type Out = u8; default fn f() {} }
"#;

/// Each refused impl's trait and rule, sorted.
fn refused(head: &str) -> Vec<(String, Rule)> {
    let text = format!("//- minicore: sized, option\n{head}\n{IMPLS}");
    let fx = fixture::db(&text);
    let _ = fx.only_file();
    let db: Db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let krate = fixture::test_crate(&db);
        let def_map = crate_def_map(&db, krate);
        let mut out = Vec::new();
        for (_, module) in def_map.modules(&db) {
            for impl_ in module.scope(&db).impls() {
                for error in check_impl(&db, impl_) {
                    let trait_ = db.impl_trait(impl_).unwrap().skip_binder().def_id.0;
                    let name = TraitSignature::of(&db, trait_).name.as_str().to_owned();
                    assert!(error.message.contains("rule: "), "{}", error.message);
                    out.push((name, error.rule));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    })
}

fn named(it: &[(&str, Rule)]) -> Vec<(String, Rule)> {
    it.iter().map(|&(name, rule)| (name.to_owned(), rule)).collect()
}

/// blaze's subset refuses what rustc's `min_specialization` does, and also a bound on an
/// unsafe marker trait and a `default type`.
#[test]
fn blaze_specialization_is_min_specialization_without_unsafe_markers() {
    assert_eq!(
        refused("#![blaze::specialization]"),
        named(&[
            ("A", Rule::StaticLifetime),
            ("Always", Rule::SpecializesOnTrait),
            ("B", Rule::RepeatedParameter),
            ("C", Rule::SpecializesOnTrait),
            ("E", Rule::EmptySpecialization),
            ("G", Rule::DefaultItem),
            ("M", Rule::UnsafeMarker),
        ]),
    );
}

/// rustc's rules accept the unsafe marker bound and leave `default type` to the feature gate.
#[test]
fn min_specialization_allows_unsafe_markers() {
    assert_eq!(
        refused("#![feature(min_specialization)]"),
        named(&[
            ("A", Rule::StaticLifetime),
            ("Always", Rule::SpecializesOnTrait),
            ("B", Rule::RepeatedParameter),
            ("C", Rule::SpecializesOnTrait),
            ("E", Rule::EmptySpecialization),
        ]),
    );
}

/// Without the extension or a feature nothing specializes and nothing is checked, but for the
/// impls of a `#[blaze::specialization_trait]`, which its author opted into.
#[test]
fn without_the_extension_only_blaze_trait_impls_are_checked() {
    assert_eq!(refused(""), named(&[("Always", Rule::SpecializesOnTrait)]));
}

/// E0520 under every mode that specializes: an item overriding one its parent leaves final is
/// refused, naming the item and both impls; `default` on the item or the parent impl allows it.
#[test]
fn overriding_a_final_item_is_refused_in_every_mode() {
    const FINAL: &str = r#"
trait F { fn f(); fn g(); }
impl<T> F for T { fn f() {} default fn g() {} }
impl F for u8 { fn f() {} fn g() {} }
"#;
    for head in [
        "#![blaze::specialization]",
        "#![feature(min_specialization)]",
        "#![feature(specialization)]",
    ] {
        let text = format!("//- minicore: sized\n{head}\n{FINAL}");
        let fx = fixture::db(&text);
        let _ = fx.only_file();
        let db: Db = fx.into_db();
        blaze_query::db::attach(&db, || {
            let krate = fixture::test_crate(&db);
            let def_map = crate_def_map(&db, krate);
            let mut found = Vec::new();
            for (_, module) in def_map.modules(&db) {
                for impl_ in module.scope(&db).impls() {
                    found.extend(
                        check_impl(&db, impl_)
                            .into_iter()
                            .filter(|it| it.rule == Rule::NotMarkedDefault),
                    );
                }
            }
            assert_eq!(found.len(), 1, "{head}: {found:?}");
            let error = &found[0];
            assert_eq!(error.rule.code(), "E0520");
            assert!(error.message.starts_with("`f` specializes an item"), "{}", error.message);
            assert!(
                error.message.contains("impl F for u8") && error.message.contains("impl F for T"),
                "{head}: {}",
                error.message
            );
        });
    }
}
