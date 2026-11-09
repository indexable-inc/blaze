//! The sealed-trait relaxation on small crate graphs: `up`, a dependency, and `main`, the crate
//! that implements `up`'s traits (with `l` between them where a test needs a third crate). Each
//! test names the property it pins; the refusals fail with their check removed
//! (docs/v2/plan/design/sealed-orphan.md, "What is built").

use base_db::{Crate, Db, all_crates};
use hir_def::{HasModule as _, nameres::crate_def_map, signatures::TraitSignature};
use hir_expand::fixture;
use hir_ty::{
    check_orphan_rules,
    db::HirDatabase as _,
    display::{DisplayTarget, HirDisplay as _},
};
use hir_ty_mir as _;

use crate::{Rule, check_impl, check_trait};

/// What the checks say about `main`'s impls and traits.
#[derive(Debug, Default)]
struct Report {
    /// Each refusal's rule and message, sorted.
    refused: Vec<(Rule, String)>,
    /// The impls the orphan rules refuse (E0117/E0210), as `Trait for Self`, sorted.
    orphans: Vec<String>,
}

impl Report {
    fn rules(&self) -> Vec<Rule> {
        self.refused.iter().map(|(rule, _)| *rule).collect()
    }
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("the fixture's crate `{name}`"))
}

/// `up`'s text and `main`'s, as one fixture over a minimal `core`.
fn check(up: &str, main: &str) -> Report {
    let text = format!(
        "//- minicore: sized, copy, clone\n//- /up.rs crate:up deps:core\n{up}\n//- /main.rs crate:main deps:up,core\n{main}\n"
    );
    check_crate(&text, "main")
}

/// `up`, `l` (which depends on `up`) and `main` (which depends on both), as one fixture: what
/// the checks say about `l`'s items and about `main`'s.
fn check3(up: &str, l: &str, main: &str) -> (Report, Report) {
    let text = format!(
        "//- minicore: sized, copy, clone\n//- /up.rs crate:up deps:core\n{up}\n//- /l.rs crate:l deps:up,core\n{l}\n//- /main.rs crate:main deps:l,up,core\n{main}\n"
    );
    (check_crate(&text, "l"), check_crate(&text, "main"))
}

/// What the checks say about the items of the fixture `text`'s crate `name`.
fn check_crate(text: &str, name: &str) -> Report {
    let db: Db = fixture::db(text).into_db();
    blaze_query::db::attach(&db, || {
        let main = crate_named(&db, name);
        let target = DisplayTarget::from_crate(&db, main);
        let mut report = Report::default();
        let def_map = crate_def_map(&db, main);
        for (_, module) in def_map.modules(&db) {
            let scope = module.scope(&db);
            for impl_ in scope.impls() {
                assert_eq!(impl_.krate(&db), main);
                for error in check_impl(&db, impl_) {
                    assert!(error.message.contains("rule: "), "{}", error.message);
                    report.refused.push((error.rule, error.message));
                }
                if !check_orphan_rules(&db, impl_) {
                    let trait_ = db.impl_trait(impl_).unwrap().skip_binder().def_id.0;
                    let self_ty = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
                    report.orphans.push(format!(
                        "{} for {}",
                        TraitSignature::of(&db, trait_).name.as_str(),
                        self_ty.display(&db, target)
                    ));
                }
            }
            for def in scope.declarations() {
                if let hir_def::ModuleDefId::TraitId(trait_) = def {
                    for error in check_trait(&db, trait_) {
                        report.refused.push((error.rule, error.message));
                    }
                }
            }
        }
        report.refused.sort_by(|a, b| a.1.cmp(&b.1));
        report.orphans.sort();
        report
    })
}

/// `up`'s trait with an `impl<T: Show + ?Sized> Show for &T`, as std's `Display` has.
const UP_SHOW: &str = r#"
pub trait Show {}
impl<T: Show + ?Sized> Show for &T {}
"#;

/// A crate may implement another crate's trait for every type of a closed sealed trait of its
/// own, even beside an upstream blanket over `&T`: coherence knows no other crate implements
/// the sealed trait for `&Downstream` (`rustc_type_ir` patch `0013-sealed-traits`).
#[test]
fn closed_sealed_blanket_is_allowed() {
    let report = check(
        UP_SHOW,
        r#"
#[blaze::sealed]
trait Sealed {}
struct A;
struct B<X>(X);
impl Sealed for A {}
impl<X> Sealed for B<X> {}
impl<T: Sealed> up::Show for T {}
"#,
    );
    assert_eq!(report.refused, vec![], "{report:#?}");
    assert_eq!(report.orphans, Vec::<String>::new(), "{report:#?}");
}

/// A sealed blanket that overlaps another impl of its trait is refused, naming both impls.
#[test]
fn overlapping_sealed_blanket_is_refused() {
    let report = check(
        UP_SHOW,
        r#"
#[blaze::sealed]
trait Sealed {}
struct A;
impl Sealed for A {}
impl up::Show for A {}
impl<T: Sealed> up::Show for T {}
"#,
    );
    assert_eq!(report.rules(), vec![Rule::Overlap], "{report:#?}");
    let message = &report.refused[0].1;
    assert!(message.contains("for T") && message.contains("for A"), "{message}");
}

/// steffahn's upstream negative reasoning: `up` relies on `Foo: !Tr` to give `Foo` its own
/// `Other` impl beside `impl<T: Tr> Other for T`. A sealed trait implemented for `up::Foo` is
/// not closed, so the blanket that would give `Foo: Tr` is refused and stays an orphan.
#[test]
fn upstream_negative_reasoning_is_refused() {
    let report = check(
        r#"
pub trait Tr {}
pub trait Other {}
impl<T: Tr> Other for T {}
pub struct Foo;
impl Other for Foo {}
"#,
        r#"
#[blaze::sealed]
trait Sealed {}
impl Sealed for up::Foo {}
impl<T: Sealed> up::Tr for T {}
"#,
    );
    assert_eq!(report.rules(), vec![Rule::SealedNotClosed], "{report:#?}");
    assert!(report.refused[0].1.contains("impl Sealed for Foo"), "{report:#?}");
    assert_eq!(report.orphans, vec!["Tr for T".to_owned()], "{report:#?}");
}

/// Only the sealed trait's crate may implement it, or name it as a supertrait (whose `dyn`
/// would implement it); both are refused in another crate.
#[test]
fn sealed_trait_is_refused_outside_its_crate() {
    let report = check(
        r#"
#[blaze::sealed]
pub trait Sealed {}
"#,
        r#"
struct M;
impl up::Sealed for M {}
trait Sub: up::Sealed {}
"#,
    );
    assert_eq!(
        report.rules(),
        vec![Rule::SealedImplElsewhere, Rule::SealedSupertraitElsewhere],
        "{report:#?}"
    );
}

/// Without `#[blaze::sealed]` the orphan rules are rustc's: the same blanket is an orphan
/// (E0210) and nothing of this crate's checks runs.
#[test]
fn without_sealed_the_blanket_is_an_orphan() {
    let report = check(
        UP_SHOW,
        r#"
trait Plain {}
struct A;
impl Plain for A {}
impl<T: Plain> up::Show for T {}
"#,
    );
    assert_eq!(report.refused, vec![], "{report:#?}");
    assert_eq!(report.orphans, vec!["Show for T".to_owned()], "{report:#?}");
}

/// A sealed blanket whose sealed parameter may be unsized is refused (`dyn` types implement
/// their supertraits without an impl), and so is one of a trait the solver builds in (`Copy`).
#[test]
fn unsized_self_and_builtin_traits_are_refused() {
    let report = check(
        UP_SHOW,
        r#"
#[blaze::sealed]
trait Sealed {}
struct A;
impl Sealed for A {}
impl<T: Sealed + ?Sized> up::Show for T {}
impl<T: Sealed> Clone for T { fn clone(&self) -> Self { loop {} } }
impl<T: Sealed> Copy for T {}
"#,
    );
    let mut rules = report.rules();
    rules.sort_by_key(|rule| rule.name());
    assert_eq!(rules, vec![Rule::RelaxedTraitNotAllowed, Rule::RelaxedUnsized], "{report:#?}");
}

/// `up`'s trait with an associated type, and a `#[fundamental]` box of its own (as `Box` is).
const UP_CONV: &str = r#"
#![feature(fundamental)]
pub trait Conv { type Out; }
#[fundamental]
pub struct Wrap<T: ?Sized>(pub T);
"#;

/// The reviewer's program: `main`'s `trait DSub: l::LSub` with `LSub: Sealed` makes
/// `dyn DSub: Sealed` hold through the object candidate, so `l`'s blanket over `Wrap<T>` would
/// cover `Wrap<dyn DSub>`, for which `main` writes its own `Conv` impl: two values of
/// `<Wrap<dyn DSub> as Conv>::Out`. Refused twice: `main`'s trait has a sealed trait of another
/// crate among its supertraits (at any depth, `where Self:` bounds included), and `l`'s blanket
/// leaves its sealed parameter unsized, which would let a `dyn` type in.
#[test]
fn sealed_supertrait_at_any_depth_is_refused() {
    let (l, main) = check3(
        UP_CONV,
        r#"
#[blaze::sealed]
pub trait Sealed {}
pub trait LSub: Sealed {}
pub struct A;
impl Sealed for A {}
impl<T: Sealed + ?Sized> up::Conv for up::Wrap<T> { type Out = u64; }
"#,
        r#"
trait DSub: l::LSub {}
trait DWhere where Self: l::LSub {}
impl up::Conv for up::Wrap<dyn DSub> { type Out = &'static str; }
"#,
    );
    assert_eq!(l.rules(), vec![Rule::RelaxedUnsized], "{l:#?}");
    assert_eq!(
        main.rules(),
        vec![Rule::SealedSupertraitElsewhere, Rule::SealedSupertraitElsewhere],
        "{main:#?}"
    );
    let messages: Vec<&str> = main.refused.iter().map(|(_, message)| message.as_str()).collect();
    assert!(
        messages.iter().any(|it| it.contains("`DSub`")
            && it.contains("`Sealed`")
            && it.contains("DSub: LSub: Sealed")),
        "{messages:#?}"
    );
    assert!(
        messages.iter().any(|it| it.contains("`DWhere`") && it.contains("DWhere: LSub: Sealed")),
        "{messages:#?}"
    );
}

/// With the sealed parameter `Sized`, the same blanket over `Wrap<T>` is allowed: no `dyn`
/// type is among the types it implements for.
#[test]
fn sized_sealed_blanket_under_a_fundamental_type_is_allowed() {
    let (l, _) = check3(
        UP_CONV,
        r#"
#[blaze::sealed]
pub trait Sealed {}
pub trait LSub: Sealed {}
pub struct A;
impl Sealed for A {}
impl<T: Sealed> up::Conv for up::Wrap<T> { type Out = u64; }
"#,
        "",
    );
    assert_eq!(l.refused, vec![], "{l:#?}");
    assert_eq!(l.orphans, Vec::<String>::new(), "{l:#?}");
}

/// `impl<T: Sealed, U> F<U> for T` stands for `impl<U> F<U> for A`, which a downstream crate
/// could overlap with `impl F<Downstream> for A` (legal there by the orphan rules), and blaze
/// has no E0119 across crates: refused while the other parameter is uncovered, allowed once it
/// is covered (`Vec`-like, not `#[fundamental]`) or sealed too.
#[test]
fn uncovered_param_beside_the_sealed_one_is_refused() {
    let report = check(
        r#"
pub trait F<X> {}
pub trait G<X> {}
pub trait H<X> {}
pub trait K<X> {}
pub struct Cover<X>(X);
"#,
        r#"
#[blaze::sealed]
trait Sealed {}
#[blaze::sealed]
trait Other {}
struct A;
struct B;
impl Sealed for A {}
impl Other for B {}
impl<T: Sealed, U> up::F<U> for T {}
impl<T: Sealed, U> up::G<&'static U> for T {}
impl<T: Sealed, U> up::H<up::Cover<U>> for T {}
impl<T: Sealed, U: Other> up::K<U> for T {}
"#,
    );
    let rules = report.rules();
    assert_eq!(
        rules,
        vec![Rule::RelaxedUncoveredParam, Rule::RelaxedUncoveredParam],
        "{report:#?}"
    );
    assert!(
        report
            .refused
            .iter()
            .all(|(_, it)| it.contains("`U`") || it.contains("`&U`") || it.contains("&'static U")),
        "{report:#?}"
    );
}

/// Of two sealed bounds on the parameter, one closed and one not, the closed one decides,
/// whatever their order: both impls are allowed (the orphan check and the coherence check pick
/// the same bound).
#[test]
fn sealed_bound_choice_does_not_depend_on_order() {
    let report = check(
        r#"
pub trait Show {}
pub trait Other {}
pub struct Foo;
"#,
        r#"
#[blaze::sealed]
trait Closed {}
#[blaze::sealed]
trait Open {}
struct A;
impl Closed for A {}
impl Open for A {}
impl Open for up::Foo {}
impl<T: Open + Closed> up::Show for T {}
impl<T: Closed + Open> up::Other for T {}
"#,
    );
    assert_eq!(report.refused, vec![], "{report:#?}");
    assert_eq!(report.orphans, Vec::<String>::new(), "{report:#?}");
}

/// A relaxed impl that overlaps a reservation impl (`#[rustc_reservation_impl]`, which the
/// solver's tables leave out) is refused, as rustc refuses the hand-written expansion (E0119).
#[test]
fn overlap_with_a_reservation_impl_is_refused() {
    let report = check(
        r#"
#![feature(rustc_attrs)]
pub trait Conv<X> {}
#[rustc_reservation_impl = "a future impl"]
impl<T> Conv<u8> for T {}
"#,
        r#"
#[blaze::sealed]
trait Sealed {}
struct A;
impl Sealed for A {}
impl<T: Sealed> up::Conv<u8> for T {}
"#,
    );
    assert_eq!(report.rules(), vec![Rule::Overlap], "{report:#?}");
}

/// `ConstParamTy_` is checked one type at a time by rustc (structural fields only), so a
/// blanket over a sealed trait may not implement it.
#[test]
fn const_param_ty_is_not_relaxable() {
    let text = r#"
//- minicore: sized, copy, clone, const_param_ty
//- /up.rs crate:up deps:core
//- /main.rs crate:main deps:up,core
#![feature(adt_const_params)]
#[blaze::sealed]
trait Sealed {}
struct A;
impl Sealed for A {}
impl<T: Sealed> core::marker::ConstParamTy_ for T {}
"#;
    let report = check_crate(text, "main");
    assert!(report.rules().contains(&Rule::RelaxedTraitNotAllowed), "{report:#?}");
}

/// The round-2 reviewer's program: `l`'s relaxed impl leans on `T: Sealed`, and its other
/// parameter `U: Other + ?Sized` is sealed too, so rule 5 lets it stand uncovered under
/// `Wrap`. `main`'s `trait DO: l::LO` with `LO: Other` makes `dyn DO: Other` hold through the
/// object candidate, so the expansion would cover `A: F<Wrap<dyn DO>>`, for which `main` writes
/// its own impl: two values of `<A as F<Wrap<dyn DO>>>::Out`. Every parameter with a sealed
/// bound must be `Sized`, not only the one the relaxation leans on: `l`'s impl is refused,
/// naming `U` and `Other`, while the same impl with `U` sized is allowed. (`main`'s supertrait
/// is refused on its own as well.)
#[test]
fn every_sealed_parameter_of_a_relaxed_impl_must_be_sized() {
    let (l, main) = check3(
        r#"
#![feature(fundamental)]
pub trait F<X> { type Out; }
pub trait G<X> { type Out; }
#[fundamental]
pub struct Wrap<T: ?Sized>(pub T);
"#,
        r#"
#[blaze::sealed]
pub trait Sealed {}
#[blaze::sealed]
pub trait Other {}
pub trait LO: Other {}
pub struct A;
pub struct B;
impl Sealed for A {}
impl Other for B {}
impl<T: Sealed, U: Other + ?Sized> up::F<up::Wrap<U>> for T { type Out = u64; }
impl<T: Sealed, U: Other> up::G<up::Wrap<U>> for T { type Out = u64; }
"#,
        r#"
pub trait DO: l::LO {}
impl up::F<up::Wrap<dyn DO>> for l::A { type Out = &'static str; }
"#,
    );
    assert_eq!(l.rules(), vec![Rule::RelaxedUnsized], "{l:#?}");
    let message = &l.refused[0].1;
    assert!(message.contains("for T") && message.contains("`U: Other`"), "{message}");
    assert_eq!(l.orphans, Vec::<String>::new(), "{l:#?}");
    assert_eq!(main.rules(), vec![Rule::SealedSupertraitElsewhere], "{main:#?}");
    assert!(main.refused[0].1.contains("DO: LO: Other"), "{main:#?}");
}

/// `up`'s `#[fundamental]` pair: rustc's orphan rules look through every argument of a
/// fundamental type but `Box`'s (all but its first), so a downstream crate may write
/// `impl F for Pair<l::A, Downstream>`.
const UP_PAIR: &str = r#"
#![feature(fundamental)]
#[fundamental]
pub struct Pair<X, Y>(pub X, pub Y);
pub trait F {}
pub trait G {}
pub trait H {}
"#;

/// Rule 5 and the closure walk a `#[fundamental]` type as rustc does, every argument and not
/// only the first. `impl<T: Sealed, U> F for Pair<T, U>` stands for `impl<U> F for Pair<A, U>`,
/// which a downstream `impl F for Pair<A, Downstream>` overlaps: refused, naming `U`; with the
/// second argument covered (`Pair<T, A>`) it is allowed. A sealed trait implemented for
/// `LPair<A, Q>`, a fundamental type of the crate with a parameter in its second argument, is
/// not closed: a blanket over it is refused, naming that impl, and stays an orphan.
#[test]
fn every_argument_of_a_fundamental_type_is_walked() {
    let report = check(
        UP_PAIR,
        r#"
#![feature(fundamental)]
#[blaze::sealed]
trait Sealed {}
struct A;
impl Sealed for A {}
impl<T: Sealed, U> up::F for up::Pair<T, U> {}
impl<T: Sealed> up::H for up::Pair<T, A> {}

#[fundamental]
struct LPair<X, Y>(X, Y);
#[blaze::sealed]
trait Open {}
impl<Q> Open for LPair<A, Q> {}
impl<T: Open> up::G for T {}
"#,
    );
    let mut rules = report.rules();
    rules.sort_by_key(|rule| rule.name());
    assert_eq!(rules, vec![Rule::RelaxedUncoveredParam, Rule::SealedNotClosed], "{report:#?}");
    let uncovered = report
        .refused
        .iter()
        .find(|(rule, _)| *rule == Rule::RelaxedUncoveredParam)
        .map(|(_, message)| message.as_str())
        .unwrap();
    assert!(uncovered.contains("Pair<T, U>") && uncovered.contains("holds `U`"), "{uncovered}");
    let open = report
        .refused
        .iter()
        .find(|(rule, _)| *rule == Rule::SealedNotClosed)
        .map(|(_, message)| message.as_str())
        .unwrap();
    assert!(open.contains("LPair<A, Q>") && open.contains("`#[fundamental]`"), "{open}");
    assert_eq!(report.orphans, vec!["G for T".to_owned()], "{report:#?}");
}
