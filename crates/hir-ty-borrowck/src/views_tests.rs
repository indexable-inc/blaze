//! `#![blaze::views]` (`crate::views`): a call of a method with a view borrows only those
//! fields of the receiver, so calls whose views are disjoint do not conflict; a body uses
//! `self` only through its view; a private method's view is inferred, a `pub` one's is what it
//! writes; a trait method's is refused; a crate without the attribute is checked as Rust is.

use base_db::{Db, all_crates};
use hir_def::{
    AssocItemId, FunctionId, ModuleDefId,
    nameres::{assoc::TraitItems, crate_def_map},
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{BorrowckDiagnostic, Mode, borrowck_in_mode};

const VIEWS: &str = r#"
//- minicore: sized, copy
#![blaze::views]
fn keep(_: &u32) {}
struct Cache { map: u32, lru: u32 }
impl Cache {
    fn old_keys(&self.{lru}) -> &u32 { &self.lru }
    fn evict(&mut self.{map}, k: &u32) { self.map = *k; }
    pub fn trim(&mut self) { let k = self.old_keys(); self.evict(k); keep(k); }

    fn map_and_lru(&mut self.{map, lru}) -> &mut u32 { &mut self.map }
    fn lru_mut(&mut self.{lru}) -> &mut u32 { &mut self.lru }
    pub fn overlap(&mut self) { let a = self.map_and_lru(); let b = self.lru_mut(); *a = *b; }

    fn outside(&self.{lru}) -> u32 { self.map }

    fn peek_lru(&self) -> &u32 { &self.lru }
    fn set_map(&mut self) { self.map = 1; }
    pub fn inferred(&mut self) { let k = self.peek_lru(); self.set_map(); keep(k); }
    pub fn pub_peek(&self) -> &u32 { &self.lru }
    pub fn not_inferred(&mut self) { let k = self.pub_peek(); self.set_map(); keep(k); }
}
trait Tr {
    fn get(&self.{lru}) -> u32;
}
"#;

const NO_VIEWS: &str = r#"
//- minicore: sized, copy
fn keep(_: &u32) {}
struct Cache { map: u32, lru: u32 }
impl Cache {
    fn old_keys(&self) -> &u32 { &self.lru }
    fn evict(&mut self, k: &u32) { self.map = *k; }
    pub fn trim(&mut self) { let k = self.old_keys(); self.evict(k); keep(k); }
}
struct Written { lru: u32 }
impl Written {
    fn written(&self.{lru}) -> &u32 { &self.lru }
}
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            let free = scope.declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            });
            let methods = scope.impls().flat_map(|it| {
                it.impl_items(db).items.iter().filter_map(|&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                })
            });
            for it in free.chain(methods) {
                if FunctionSignature::of(db, it).name.as_str() == name {
                    return it;
                }
            }
            for def in scope.declarations() {
                if let ModuleDefId::TraitId(it) = def {
                    let (items, _) = TraitItems::query_with_diagnostics(db, it);
                    for &(_, item) in items.items.iter() {
                        if let AssocItemId::FunctionId(it) = item
                            && FunctionSignature::of(db, it).name.as_str() == name
                        {
                            return it;
                        }
                    }
                }
            }
        }
    }
    panic!("no function `{name}`")
}

/// The errors of `name` in `fixture`, in each mode, checked to be the same in both.
fn errors(fixture: &str, name: &str) -> Vec<BorrowckDiagnostic> {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, name);
        let [nll, polonius] = [Mode::Nll, Mode::Polonius].map(|mode| {
            let result = borrowck_in_mode(&db, function.into(), mode);
            assert_eq!(result.skipped, None, "`{name}` in {mode:?}");
            assert_eq!(result.not_checked, None, "`{name}` in {mode:?}");
            // alloc: test
            result.diagnostics.into_iter().filter(BorrowckDiagnostic::is_error).collect::<Vec<_>>()
        });
        assert_eq!(nll, polonius, "`{name}`: NLL and Polonius differ");
        nll
    })
}

fn codes(errors: &[BorrowckDiagnostic]) -> Vec<&'static str> {
    errors.iter().map(|it| it.code).collect() // alloc: test
}

/// The spec's example: `old_keys` borrows `lru`, `evict` borrows `map` mutably, so `trim` may
/// evict while it holds what `old_keys` gave.
#[test]
fn disjoint_views_do_not_conflict() {
    assert_eq!(codes(&errors(VIEWS, "trim")), Vec::<&str>::new());
    assert_eq!(codes(&errors(VIEWS, "old_keys")), Vec::<&str>::new());
    assert_eq!(codes(&errors(VIEWS, "evict")), Vec::<&str>::new());
}

/// Two mutable views that share `lru` conflict on it, as two `&mut` of `self.lru` do.
#[test]
fn overlapping_mutable_views_conflict() {
    let found = errors(VIEWS, "overlap");
    assert_eq!(codes(&found), ["E0499"], "{found:?}");
    assert!(found[0].message.contains("lru"), "{found:?}");
    assert!(!found[0].message.contains("map"), "only the shared field conflicts: {found:?}");
}

#[test]
fn a_body_uses_only_its_view() {
    let found = errors(VIEWS, "outside");
    assert_eq!(codes(&found), [crate::views::CODE], "{found:?}");
    assert!(found[0].message.contains("field `map`"), "{found:?}");
    assert!(found[0].message.contains("&self.{lru}"), "{found:?}");
}

/// A private method has the view its body uses; a `pub` one of the same shape borrows the whole
/// receiver, so the same caller is refused.
#[test]
fn a_private_view_is_inferred_and_a_pub_one_is_written() {
    assert_eq!(codes(&errors(VIEWS, "inferred")), Vec::<&str>::new());
    assert_eq!(codes(&errors(VIEWS, "not_inferred")), ["E0502"]);
}

#[test]
fn a_view_on_a_trait_method_is_refused() {
    let found = errors(VIEWS, "get");
    assert_eq!(codes(&found), [crate::views::CODE], "{found:?}");
    assert!(found[0].message.contains("trait"), "{found:?}");
    assert!(found[0].message.contains("`get`"), "{found:?}");
}

/// Without `#![blaze::views]` the receiver is borrowed whole, as rustc borrows it, and a view
/// written is refused, naming the attribute.
#[test]
fn a_crate_without_the_attribute_is_checked_as_rust() {
    assert_eq!(codes(&errors(NO_VIEWS, "trim")), ["E0502"]);
    let found = errors(NO_VIEWS, "written");
    assert_eq!(codes(&found), [crate::views::CODE], "{found:?}");
    assert!(found[0].message.contains("#![blaze::views]"), "{found:?}");
}
