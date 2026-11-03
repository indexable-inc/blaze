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
        assert_eq!(codes("held_generic"), ["E0502"], "`touch` borrows `t` mutably while `it` borrows it");
        assert_eq!(codes("dropped_generic"), Vec::<&str>::new());
    });
}
