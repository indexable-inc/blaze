//! Each case is a program the pinned rustc (nightly-2026-10-27, edition 2024) was run on, with
//! the errors it reports of the impls: code and where.

use base_db::all_crates;
use hir_def::{AssocItemId, GenericDefId, ModuleDefId, nameres::crate_def_map};
use hir_expand::fixture;

use crate::{Site, wfcheck};

const PRELUDE: &str = "
pub trait Plain {}
impl<T> Plain for T {}
pub static B: u8 = 0;
pub struct Wrap<T>(pub T);
pub struct W<'a>(pub &'a u8);
";

/// The errors of every item of `source`: code, where (`assoc type`, `opaque`, `output`, or
/// the site as debug-printed) and message.
fn errors(source: &str) -> Vec<(&'static str, String, String)> {
    let fx = fixture::db(&format!(
        "//- minicore: sized, copy, clone\n//- /main.rs edition:2024\n{PRELUDE}{source}"
    ));
    let db = fx.into_db();
    let mut out = Vec::new();
    blaze_query::db::attach(&db, || {
        for &krate in all_crates(&db).iter() {
            let def_map = crate_def_map(&db, krate);
            for (_, module) in def_map.modules(&db) {
                let scope = module.scope(&db);
                let mut defs: Vec<GenericDefId> = Vec::new();
                for def in scope.declarations() {
                    match def {
                        ModuleDefId::FunctionId(it) => defs.push(it.into()),
                        ModuleDefId::TraitId(it) => defs.push(it.into()),
                        ModuleDefId::AdtId(it) => defs.push(it.into()),
                        _ => (),
                    }
                }
                for imp in scope.impls() {
                    defs.push(imp.into());
                    for &(_, item) in imp.impl_items(&db).items.iter() {
                        if let AssocItemId::FunctionId(it) = item {
                            defs.push(it.into());
                        }
                    }
                }
                for def in defs {
                    for found in &wfcheck(&db, def).diagnostics {
                        let site = match found.site {
                            Site::AssocType(_) => "assoc type".to_owned(),
                            Site::Opaque(..) => "opaque".to_owned(),
                            Site::Output(_) => "output".to_owned(),
                            other => format!("{other:?}"),
                        };
                        out.push((found.code, site, found.message.clone()));
                    }
                }
            }
        }
    });
    out
}

/// Runs every case and reports each that differs from rustc at once.
fn check(cases: &[(&str, &str, &[(&str, &str)])]) {
    let mut wrong = Vec::new();
    for &(name, source, expected) in cases {
        let found = errors(source);
        let codes: Vec<(&str, &str)> =
            found.iter().map(|(code, site, _)| (*code, site.as_str())).collect();
        if codes != expected {
            wrong.push(format!("{name}: expected {expected:?}, found {found:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} cases differ from rustc:\n{}",
        wrong.len(),
        cases.len(),
        wrong.join("\n")
    );
}

#[test]
fn associated_types_satisfy_the_trait_s_bounds() {
    check(&[
        // wasmborrow review S2 (/tmp/wasmborrow-rv/rs/wf.rs): rustc E0477.
        (
            "borrowed value of a 'static type",
            "pub trait Ts { type A: 'static; }
             impl<'a> Ts for W<'a> { type A = &'a u8; }",
            &[("E0477", "assoc type")],
        ),
        (
            "parameter value of a 'static type",
            "pub trait Ts { type A: 'static; }
             impl<T> Ts for Wrap<T> { type A = T; }",
            &[("E0310", "assoc type")],
        ),
        (
            "bounded parameter",
            "pub trait Ts { type A: 'static; }
             impl<T: 'static> Ts for Wrap<T> { type A = T; }",
            &[],
        ),
        (
            "trait bound",
            "pub struct S;
             pub trait C { type A: Copy; }
             impl C for u8 { type A = S; }",
            &[("E0277", "assoc type")],
        ),
        (
            "trait bound that holds",
            "pub trait C { type A: Copy; }
             impl C for u8 { type A = u8; }",
            &[],
        ),
        (
            "bound on a trait parameter",
            "pub trait Out<'b> { type A: 'b; }
             impl<'a, 'b> Out<'b> for W<'a> { type A = &'a u8; }",
            &[("E0477", "assoc type")],
        ),
        (
            "bound the header implies",
            "pub trait Out<'b> { type A: 'b; }
             impl<'a, 'b> Out<'b> for &'b W<'a> { type A = &'a u8; }",
            &[],
        ),
        (
            "generic associated type",
            "pub trait G { type A<'x>: 'x; }
             impl G for u8 { type A<'x> = &'x u8; }",
            &[],
        ),
        (
            "bound naming another item",
            "pub trait It { type Item; }
             impl It for u8 { type Item = &'static u8; }
             pub trait Two { type I: It<Item = Self::E>; type E; }
             impl Two for u8 { type I = u8; type E = &'static u8; }",
            &[],
        ),
        // wasmtime 50.0.1's `AsContext for &T` and `for &mut T` (store/context.rs:180, 190) and
        // `AsContext for RootScope<T>` with `T: AsContextMut` (gc/disabled/rooting.rs:139):
        // `T::Data`, rigid once normalized, outlives `'static` by the trait's own bound.
        (
            "forwarded associated type with a declared bound",
            "pub trait Ac { type Data: 'static; }
             pub trait AcMut: Ac {}
             impl<T: Ac> Ac for &'_ T { type Data = T::Data; }
             impl<T: Ac> Ac for &'_ mut T { type Data = T::Data; }
             impl<T: AcMut> Ac for Wrap<T> { type Data = T::Data; }",
            &[],
        ),
        (
            "forwarded associated type bounded by a where clause",
            "pub trait Un { type Data; }
             pub trait Out<'b> { type A: 'b; }
             impl<'b, T: Un> Out<'b> for Wrap<T> where T::Data: 'b { type A = T::Data; }",
            &[],
        ),
        (
            "forwarded associated type with no bound",
            "pub trait Un { type Data; }
             pub trait Ts { type A: 'static; }
             impl<T: Un> Ts for Wrap<T> { type A = T::Data; }",
            &[("E0310", "assoc type")],
        ),
        (
            "projection bound that fails",
            "pub trait It { type Item; }
             impl It for u8 { type Item = &'static u8; }
             pub trait Two { type I: It<Item = u16>; }
             impl Two for u8 { type I = u8; }",
            &[("E0271", "assoc type")],
        ),
    ]);
}

#[test]
fn rpitit_types_satisfy_the_trait_s_bounds() {
    check(&[
        // wasmborrow review S1 (/tmp/wasmborrow-rv/rs/ub.rs): rustc E0477 on the impl only;
        // blaze accepted it and the program read a dead stack slot.
        (
            "ub.rs",
            "pub trait Get { fn get(&self) -> u8; }
             impl Get for &u8 { fn get(&self) -> u8 { **self } }
             pub trait MkS { fn mks<'a>(&self, x: &'a u8) -> impl Get + 'static; }
             impl MkS for u8 { fn mks<'a>(&self, x: &'a u8) -> impl Get { x } }",
            &[("E0477", "opaque")],
        ),
        (
            "o1: declared bound too short",
            "pub trait M { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'static; }
             impl M for u8 { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'a { x } }",
            &[("E0477", "opaque")],
        ),
        (
            "o2: captures every lifetime in scope",
            "pub trait M { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'static; }
             impl M for u8 { fn m<'a>(&self, x: &'a u8) -> impl Plain { &B } }",
            &[("E0477", "opaque")],
        ),
        (
            "o3: captures nothing",
            "pub trait M { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'static; }
             impl M for u8 { fn m<'a>(&self, x: &'a u8) -> impl Plain + use<> { &B } }",
            &[],
        ),
        (
            "o4: concrete 'static type",
            "pub trait M { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'static; }
             impl M for u8 { fn m<'a>(&self, x: &'a u8) -> &'static u8 { &B } }",
            &[],
        ),
        (
            "o5: concrete borrowed type",
            "pub trait M { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'static; }
             impl M for u8 { fn m<'a>(&self, x: &'a u8) -> &'a u8 { x } }",
            &[("E0477", "output")],
        ),
        (
            "o6: the same bound",
            "pub trait M { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'a; }
             impl M for u8 { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'a { x } }",
            &[],
        ),
        (
            "o7: captures the elided lifetime of &self",
            "pub trait M { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'a; }
             impl M for u8 { fn m<'a>(&self, x: &'a u8) -> impl Plain { x } }",
            &[("E0477", "opaque")],
        ),
        (
            "o8: captures a 'static parameter",
            "pub trait M { fn m(&self) -> impl Plain + 'static; }
             impl<T: Clone + 'static> M for Wrap<T> {
                 fn m(&self) -> impl Plain + use<T> { self.0.clone() }
             }",
            &[],
        ),
        (
            "o9: captures a parameter",
            "pub trait M { fn m(&self) -> impl Plain + 'static; }
             impl<T: Clone> M for Wrap<T> { fn m(&self) -> impl Plain + use<T> { self.0.clone() } }",
            &[("E0310", "opaque")],
        ),
        (
            "o10: a parameter",
            "pub trait M { fn m(&self) -> impl Plain + 'static; }
             impl<T: Clone> M for Wrap<T> { fn m(&self) -> T { self.0.clone() } }",
            &[("E0310", "output")],
        ),
        (
            "o11: captures the trait's lifetime",
            "pub trait M<'b> { fn m<'a>(&self, x: &'a u8) -> impl Plain + 'b; }
             impl<'b> M<'b> for &'b u8 { fn m<'a>(&self, x: &'a u8) -> impl Plain + use<'b> { *self } }",
            &[],
        ),
        (
            "o12: nested impl Trait",
            "pub trait It { type Item; }
             pub struct Once<T>(pub T);
             impl<T> It for Once<T> { type Item = T; }
             pub trait M { fn m<'a>(&self, x: &'a u8) -> impl It<Item = impl Plain + 'static>; }
             impl M for u8 { fn m<'a>(&self, x: &'a u8) -> impl It<Item = &'a u8> { Once(x) } }",
            &[("E0477", "output")],
        ),
        (
            "o13: nested impl Trait that holds",
            "pub trait It { type Item; }
             pub struct Once<T>(pub T);
             impl<T> It for Once<T> { type Item = T; }
             pub trait M { fn m<'a>(&self, x: &'a u8) -> impl It<Item = impl Plain + 'static>; }
             impl M for u8 {
                 fn m<'a>(&self, x: &'a u8) -> impl It<Item = &'static u8> + use<> { Once(&B) }
             }",
            &[],
        ),
        (
            "parameter behind &self",
            "pub trait MkS { fn mks(&self) -> impl Plain + 'static; }
             impl<T: Clone> MkS for Wrap<T> { fn mks(&self) -> impl Plain { self.0.clone() } }",
            &[("E0477", "opaque")],
        ),
    ]);
}

/// Callers that rely on the bounds, and the review's control cases, have no error
/// (/tmp/wasmborrow-rv/rs/wf3.rs and ctl.rs).
#[test]
fn users_of_the_bounds_are_clean() {
    check(&[(
        "wf3.rs",
        "pub fn take_static(_: impl Plain + 'static) {}
         pub trait Ts { type A: 'static; }
         pub fn gen_static<T: Ts>(x: T::A) { take_static(x) }
         pub trait Mk { fn mk<'a>(&self, x: &'a u8) -> impl Plain + 'static; }
         pub fn gen_rpitit<M: Mk>(m: &M, c: &u8) { take_static(m.mk(c)) }
         pub trait G { type A<'x>: 'x; }
         pub trait Tw<'a, 'b> { type A: 'b; }
         impl<'a, 'b> Tw<'a, 'b> for &'b u8 { type A = &'b u8; }",
        &[],
    )]);
}
