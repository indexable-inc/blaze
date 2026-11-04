//! A trait method's `impl Trait` captures the method's lifetimes (`hir_ty::rpitit`): what an
//! impl gives for it borrows `self` for as long as the caller holds it, whether the caller
//! names the impl (and sees its type) or is generic (and sees the trait's item).

use base_db::Db;
use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

const FIXTURE: &str = r#"
//- minicore: sized
struct Iter<'a>(&'a u32);
impl<'a> Iter<'a> { fn get(&self) -> u32 { *self.0 } }
trait Tr { fn iter(&self) -> impl Sized; }
struct S(u32);
impl Tr for S { fn iter(&self) -> Iter<'_> { Iter(&self.0) } }
fn touch<T>(_: &mut T) {}
fn held(mut s: S) -> u32 { let it = s.iter(); s.0 = 1; it.get() }
fn dropped(mut s: S) -> u32 { let n = s.iter().get(); s.0 = 1; n }
fn held_generic<T: Tr>(mut t: T) { let it = t.iter(); touch(&mut t); let _keep = it; }
fn dropped_generic<T: Tr>(mut t: T) { let _ = t.iter(); touch(&mut t); }
"#;

fn function(db: &Db, name: &str) -> DefWithBodyId {
    for (_, module) in crate_def_map(db, fixture::test_crate(db)).modules(db) {
        for def in module.scope(db).declarations() {
            if let ModuleDefId::FunctionId(it) = def
                && FunctionSignature::of(db, it).name.as_str() == name
            {
                return it.into();
            }
        }
    }
    panic!("no function `{name}`")
}

#[test]
fn the_value_of_a_trait_methods_impl_trait_borrows_self_while_held() {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let codes = |name: &str| -> Vec<&'static str> {
            let result = crate::borrowck(&db, function(&db, name));
            assert_eq!(result.skipped, None, "`{name}`");
            assert_eq!(result.not_checked, None, "`{name}`");
            // alloc: test
            result.diagnostics.iter().filter(|it| it.is_error()).map(|it| it.code).collect()
        };
        assert_eq!(codes("held"), ["E0506"], "`held` assigns to `s.0` while `it` borrows it");
        assert_eq!(codes("dropped"), Vec::<&str>::new());
        assert_eq!(
            codes("held_generic"),
            ["E0502"],
            "`touch` borrows `t` mutably while `it` borrows it"
        );
        assert_eq!(codes("dropped_generic"), Vec::<&str>::new());
    });
}

/// Called on an impl's type, a trait method's `impl Trait` is the impl method's own opaque type
/// (what `<u8 as Mk>::{impl Trait#0}<'a>` normalizes to), which captures the borrow passed for
/// `'a`: the call's value outlives `'static` only if that borrow does, and holds it while it
/// lives. Each `bad_*` is rejected by rustc with the code given; each `ok_*` accepted.
const CONCRETE: &str = r#"
//- minicore: sized
//- /main.rs edition:2024
pub trait Plain {}
impl<T> Plain for T {}
pub fn take_static(_: impl Plain + 'static) {}
pub static B: u8 = 0;

pub trait Mk { fn mk<'a>(&self, x: &'a u8) -> impl Plain + 'a; }
impl Mk for u8 { fn mk<'a>(&self, x: &'a u8) -> impl Plain + 'a { x } }
pub fn bad_param(c: &u8) { take_static(0u8.mk(c)) }
pub fn bad_local() { let x = 0u8; take_static(0u8.mk(&x)) }
pub fn bad_held() -> u8 { let mut x = 0u8; let it = 0u8.mk(&x); x = 1; let _keep = it; x }
pub fn ok_static() { take_static(0u8.mk(&B)) }

pub trait Mk2 { fn mk2(&self) -> impl Plain + '_; }
impl Mk2 for u8 { fn mk2(&self) -> impl Plain + '_ { self } }
pub fn bad_self(s: &u8) { take_static(s.mk2()) }

pub trait Mk3 { fn mk3<'a: 'a>(&self, x: &'a u8) -> impl Plain + 'a; }
impl Mk3 for u8 { fn mk3<'a: 'a>(&self, x: &'a u8) -> impl Plain + 'a { x } }
pub fn bad_early(c: &u8) { take_static(0u8.mk3(c)) }

pub struct Gen<T>(T);
pub trait MkG { fn mkg<'a>(&self, x: &'a u8) -> impl Plain + 'a; }
impl<T> MkG for Gen<T> { fn mkg<'a>(&self, x: &'a u8) -> impl Plain + 'a { x } }
pub fn bad_generic_impl(g: &Gen<u8>, c: &u8) { take_static(g.mkg(c)) }

pub trait Mk4 { fn mk4<'a>(&self, x: &'a u8) -> impl Plain; }
impl Mk4 for u8 { fn mk4<'a>(&self, x: &'a u8) -> impl Plain { *x } }
pub fn bad_unbounded(c: &u8) { take_static(0u8.mk4(c)) }

pub trait MkSt { fn mkst<'a>(&self, x: &'a u8) -> impl Plain + 'static; }
impl MkSt for u8 { fn mkst<'a>(&self, x: &'a u8) -> impl Plain + 'static + use<'a> { *x } }
pub fn ok_bounded(c: &u8) { take_static(0u8.mkst(c)) }
"#;

#[test]
fn a_trait_methods_impl_trait_called_on_an_impl_type_borrows_what_is_passed() {
    let db = fixture::db(CONCRETE).into_db();
    blaze_query::db::attach(&db, || {
        let expect = |name: &str, want: &[&str]| {
            for mode in [crate::Mode::Nll, crate::Mode::Polonius] {
                let result = crate::borrowck_in_mode(&db, function(&db, name), mode);
                assert_eq!(result.skipped, None, "`{name}` {mode:?}");
                assert_eq!(result.not_checked, None, "`{name}` {mode:?}");
                // alloc: test
                let codes: Vec<&str> = result
                    .diagnostics
                    .iter()
                    .filter(|it| it.is_error())
                    .map(|it| it.code)
                    .collect();
                assert_eq!(codes, want, "`{name}` {mode:?}");
            }
        };
        expect("bad_param", &["E0521"]);
        expect("bad_local", &["E0597"]);
        expect("bad_held", &["E0506"]);
        expect("bad_self", &["E0521"]);
        expect("bad_early", &["E0521"]);
        expect("bad_generic_impl", &["E0521"]);
        expect("bad_unbounded", &["E0521"]);
        expect("ok_static", &[]);
        expect("ok_bounded", &[]);
    });
}

const USE_FIXTURE: &str = r#"
//- minicore: sized
trait Tr { fn f(&self) -> impl Sized + use<Self>; }
fn touch<T>(_: &mut T) {}
fn uncaptured<T: Tr>(mut t: T) { let it = t.f(); touch(&mut t); let _keep = it; }
"#;

/// rustc's `opt_alias_variances` of an RPITIT projection is its opaque type's: a lifetime its
/// `use<..>` leaves out is bivariant, so the item need not keep it live. `use<Self>` leaves out
/// the borrow of `self`, so `t` may be borrowed again while `it` is held (stable since 1.87).
#[test]
fn a_lifetime_a_trait_methods_impl_trait_does_not_capture_need_not_be_live() {
    let db = fixture::db(USE_FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let result = crate::borrowck(&db, function(&db, "uncaptured"));
        assert_eq!(result.skipped, None);
        assert_eq!(result.not_checked, None);
        let errors: Vec<&str> = // alloc: test
            result.diagnostics.iter().filter(|it| it.is_error()).map(|it| it.code).collect();
        assert_eq!(errors, Vec::<&str>::new());
    });
}

const DEFAULT_FIXTURE: &str = r#"
//- minicore: sized, copy
trait D {
    fn returns_self(&self) -> impl Sized { self }
    fn returns_a_local(&self) -> impl Sized { let x = 1u32; &x }
    fn returns_a_byte(&self) -> impl Sized + Copy { 7u8 }
    fn without_self() -> impl Sized { 7u8 }
}
"#;

/// A trait method's default body returns its own `impl Trait` (the item's value, by the
/// `Proj == Opaque` clause of its environment), and is borrow checked as a function returning
/// that opaque type: `self` may be returned, a local's borrow may not. The pinned nightly says
/// E0597 (`x` does not live long enough) for `returns_a_local`, in edition 2021 and 2024.
#[test]
fn a_trait_methods_default_body_is_borrow_checked_as_returning_its_impl_trait() {
    let db = fixture::db(DEFAULT_FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let codes = |name: &str| -> Vec<&'static str> {
            let result = crate::borrowck(&db, trait_method(&db, name));
            assert_eq!(result.skipped, None, "`{name}`");
            assert_eq!(result.not_checked, None, "`{name}`");
            // alloc: test
            result.diagnostics.iter().filter(|it| it.is_error()).map(|it| it.code).collect()
        };
        assert_eq!(codes("returns_self"), Vec::<&str>::new());
        assert_eq!(codes("returns_a_local"), ["E0597"]);
        assert_eq!(codes("returns_a_byte"), Vec::<&str>::new());
        assert_eq!(codes("without_self"), Vec::<&str>::new());
    });
}

fn trait_method(db: &Db, name: &str) -> DefWithBodyId {
    for (_, module) in crate_def_map(db, fixture::test_crate(db)).modules(db) {
        for def in module.scope(db).declarations() {
            if let ModuleDefId::TraitId(tr) = def
                && let Some(it) =
                    tr.trait_items(db).method_by_name(&hir_expand::name::Name::new_root(name))
            {
                return it.into();
            }
        }
    }
    panic!("no trait method `{name}`")
}
