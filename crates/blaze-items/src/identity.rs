//! blaze-id's two ways between an id and its key, for the one database
//! (docs/v2/plan/design/identity.md, point 4): [`blaze_id::key_of`] of an [`ItemId`] is its
//! [`key::item_key`], and [`blaze_id::resolve`] is a read of the crate's key index
//! ([`key::resolve`]) after the key is checked to be of the crate asked for.
//!
//! Only the ids this crate defines are served here, [`ItemId`] and [`Resolved`]: an impl of
//! blaze-id's traits for `base_db::Db` and a hir-def id (`FunctionId`, `ModuleId`, ..) can
//! only be written in hir-def or base-db, which is where the key computation moves next.

use base_db::Db;
use blaze_id::{CrateItemKey, ItemKey, KeyOf, NoItem, Resolve};

use crate::{
    ItemId,
    key::{self, Resolved},
};

impl KeyOf<ItemId> for Db {
    /// [`key::item_key`]: hashed with its module's, impl's or trait's items in one batch,
    /// kept for the snapshot.
    fn key_of(&self, id: ItemId) -> ItemKey {
        key::item_key(self, id)
    }
}

impl Resolve<Resolved> for Db {
    /// What `key` names in `krate`, of any kind: an item, a module, a variant, a closure, an
    /// opaque type, a constructor or an anonymous constant.
    ///
    /// A key carries its crate's first 8 bytes, and two crates of one build never share them
    /// (`key::check_crate_keys`), so the key's crate is `krate` when those bytes agree and
    /// `krate` is a crate of this database; [`key::resolve`] then reads that crate's index.
    fn resolve(&self, krate: CrateItemKey, key: ItemKey) -> Result<Resolved, NoItem> {
        let none = NoItem { krate, key };
        if !key.is_of(krate) || key::resolve_crate(self, &krate.0).is_none() {
            return Err(none);
        }
        key::resolve(self, key).ok_or(none)
    }
}

impl Resolve<ItemId> for Db {
    /// The item `key` names in `krate`; a key of a module, a variant or anything else that is
    /// not an [`ItemId`] names no item here.
    fn resolve(&self, krate: CrateItemKey, key: ItemKey) -> Result<ItemId, NoItem> {
        match blaze_id::resolve::<Resolved, Db>(self, krate, key)? {
            Resolved::Item(it) => Ok(it),
            _ => Err(NoItem { krate, key }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_id::{CrateItemKey, ItemKey};
    use blaze_memo::Id as _;
    use blaze_sim::{Broke, Config, Tape};
    use hir_expand::fixture;

    use crate::{
        ItemId, key,
        tests::{TestDB, items},
    };

    /// The fixture's modules, each declared by `pub mod <name>;` in the crate's root.
    const MODULES: [&str; 4] = ["a", "b", "c", "d"];

    /// What each module holds: an item of every kind a module's, a trait's and an impl's
    /// batch of keys hashes.
    const BODY: &str = "pub struct S;\npub enum E { V }\npub fn f() {}\npub const C: u32 = 1;\n\
                        pub static X: u32 = 2;\npub type A = S;\n\
                        pub trait T { fn m(&self); const K: u32; }\n\
                        impl T for S { fn m(&self) {} const K: u32 = 3; }\n\
                        impl S { pub fn new() -> S { S } }\n";

    /// The fixture with the modules declared in `order`. Name resolution collects them in
    /// declaration order, so the order moves where each item's id lands in its arena, and
    /// nothing a key is made of: a module's key is its parent's and its name.
    fn fixture_text(order: &[&str]) -> String {
        let mut text = String::from("//- /lib.rs crate:lib\n");
        for name in order {
            text.push_str(&format!("pub mod {name};\n"));
        }
        for name in MODULES {
            text.push_str(&format!("//- /{name}.rs\n{BODY}"));
        }
        text
    }

    /// Each item of the fixture's crate: its path, its id and its key, sorted by path then key.
    /// Fails, naming the item, where resolving the key does not give the id back.
    fn keyed(db: &TestDB) -> Result<Vec<(String, ItemId, ItemKey), Global>, Broke> {
        let krate = CrateItemKey(key::crate_key(db, fixture::test_crate(db)));
        let ids = items(db);
        let mut out = Vec::with_capacity_in(ids.len(), Global);
        for id in ids {
            let key = blaze_id::key_of(db, id);
            let back = blaze_id::resolve::<ItemId, TestDB>(db, krate, key);
            if back != Ok(id) {
                let path = key::path(db, id);
                return Err(Broke::Oracle(format!("`{path}` ({id:?}) has key {key:x}, which resolves to {back:?}")));
            }
            out.push((key::path(db, id), id, key));
        }
        out.sort_unstable_by(|a, b| (&a.0, a.2).cmp(&(&b.0, b.2)));
        Ok(out)
    }

    /// The module order a case declares: a permutation of [`MODULES`], drawn swap by swap.
    fn order(tape: &Tape) -> [&'static str; 4] {
        let mut order = MODULES;
        for at in (1..order.len()).rev() {
            let bound = u64::try_from(at + 1).expect("4 fits a u64");
            let with = usize::try_from(tape.below(bound, "swap")).expect("below 4 fits a usize");
            order.swap(at, with);
        }
        order
    }

    /// The local index of each function's id, by its path: where interning put it.
    fn function_slots(rows: &[(String, ItemId, ItemKey)]) -> Vec<(&str, u32), Global> {
        let mut out = Vec::new_in(Global);
        for (path, id, _) in rows {
            if let ItemId::Function(it) = id {
                out.push((path.as_str(), it.gid().local()));
            }
        }
        out
    }

    const PERMUTED: &str = "a function's id has another slot than in the declaration order";

    /// The invariant: every item of a crate resolves from its key back to its id, and two
    /// databases of one crate whose items were interned in another order give each item the
    /// same key. The reach point shows the order did move the ids.
    #[test]
    fn a_key_resolves_to_its_item_and_is_the_same_in_any_interning_order() {
        let first = fixture::db(&fixture_text(&MODULES));
        let first = keyed(first.db()).unwrap_or_else(|broke| panic!("in declaration order: {broke}"));
        blaze_sim::check(Config::cases(24).must_reach(&[PERMUTED]), |tape| {
            let order = order(tape);
            let fixture = fixture::db(&fixture_text(&order));
            let rows = keyed(fixture.db())?;
            if function_slots(&rows) != function_slots(&first) {
                tape.reached(PERMUTED);
            }
            let keys = |rows: &[(String, ItemId, ItemKey)]| {
                let mut out = Vec::with_capacity_in(rows.len(), Global);
                out.extend(rows.iter().map(|(path, _, key)| (path.clone(), *key)));
                out
            };
            let (now, then) = (keys(&rows), keys(&first));
            if now != then {
                return Err(Broke::Oracle(format!(
                    "the modules declared as {order:?} give the keys {now:?}, in declaration order {then:?}"
                )));
            }
            Ok(())
        });
    }

    /// A key of another crate names nothing in this one, though the key is of a real item.
    #[test]
    fn a_key_of_another_crate_resolves_to_no_item() {
        let fixture = fixture::db(&fixture_text(&MODULES));
        let db = fixture.db();
        let id = items(db)[0];
        let key = blaze_id::key_of(db, id);
        let mut other = key::crate_key(db, fixture::test_crate(db));
        other.0[0] ^= 1;
        let other = CrateItemKey(other);
        assert_eq!(
            blaze_id::resolve::<ItemId, TestDB>(db, other, key),
            Err(blaze_id::NoItem { krate: other, key })
        );
    }
}
