//! What a crate keeps by name, found by lowering only the bodies that can declare it: from
//! its record when it is served, by the text of its bodies when it is not. Each test against
//! the reference model, the walk of every body of the crate, with its control.

#[path = "../../tests/db.rs"]
mod db;

use std::sync::Arc;

use base_db::{Crate, Env};
use blaze_core::lock::Lock;
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_def::{
    AssocItemId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, ModuleDefId, StaticId,
    deps::{ImageBuf, ImageStore},
    expr_store::ExpressionStore,
    nameres::{DefMap, crate_def_map, demand::bodies_declaring_roots, frozen::Mode},
    signatures::{FunctionSignature, StaticFlags, StaticSignature},
};
use hir_expand::fixture;
use std::alloc::Global;

use super::{frozen, keeps_function, keeps_static, kept, over_bodies};
use db::TestDB;

/// A dependency that keeps a `#[no_mangle]` function at module level (`top`), one in an impl
/// (`in_impl`), one in a block of a function's body (`in_block`, in `f`) and a `#[used]`
/// static in a nested block of another's (`NESTED`, in `g`), with bodies that keep nothing
/// (`h`, `i`, `S::plain`), and a crate that depends on it.
const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
pub fn main() { lib::h(); }

//- /lib.rs crate:lib new_source_root:library
#[no_mangle]
pub extern "C" fn top() {}
pub struct S;
impl S {
    #[no_mangle]
    pub extern "C" fn in_impl() {}
    pub fn plain() -> u32 { 2 }
}
pub fn f() {
    #[no_mangle]
    extern "C" fn in_block() {}
}
pub fn g() {
    {
        {
            #[used]
            static NESTED: u8 = 0;
        }
    }
}
pub fn h() {}
pub fn i() -> u32 { 1 + 1 }
"#;

/// The key every image of the tests is made with.
const BUILD: ContentId = ContentId([7; 32]);

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
struct Memory {
    values: Lock<HashMap<ContentId, Arc<Aligned>, Global>>,
}

impl Default for Memory {
    fn default() -> Memory {
        Memory { values: Lock::new("test.codegen.kept.memory", HashMap::new_in(Global)) }
    }
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(self.values.lock().get(key).map(|it| it.clone() as ImageBuf))
    }

    fn reads_now(&self, _: &Env, _: &[u8]) -> Result<ContentId, String> {
        Err("the fixture runs no proc macro, so no expansion read is asked".to_owned())
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        let mut values = self.values.lock();
        match values.get(&key) {
            Some(kept) if kept[..] != value[..] => Err(format!("{key:?} kept with other bytes")),
            Some(_) => Ok(()),
            None => {
                values.insert(key, Arc::new(Aligned::copy_from(&value)));
                Ok(())
            }
        }
    }
}

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// What is kept, by name, sorted: what two databases' answers compare by.
fn names(db: &TestDB, functions: &[FunctionId], statics: &[StaticId]) -> Vec<String> {
    let functions =
        functions.iter().map(|&it| FunctionSignature::of(db, it).name.as_str().to_owned());
    let statics = statics.iter().map(|&it| StaticSignature::of(db, it).name.as_str().to_owned());
    let mut names: Vec<String> = functions.chain(statics).collect();
    names.sort();
    names
}

/// The reference model: what `krate` keeps, found by the walk of every body of it, as rustc's
/// collector finds it (and as `kept` found it before it lowered only some): each module of
/// the crate's def map and of every block of every body, the bodies declared in blocks
/// included. By name ([`names`]).
fn reference(db: &TestDB, krate: Crate) -> Vec<String> {
    let (mut functions, mut statics) = (Vec::new(), Vec::new());
    let mut bodies = Vec::new();
    let mut maps: Vec<&DefMap> = vec![crate_def_map(db, krate)];
    while let Some(def_map) = maps.pop() {
        for (_, data) in def_map.modules(db) {
            let scope = data.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(func) => {
                        if FunctionSignature::of(db, func).has_body() {
                            bodies.push(DefWithBodyId::FunctionId(func));
                        }
                        if keeps_function(db, func) {
                            functions.push(func);
                        }
                    }
                    ModuleDefId::StaticId(value) => {
                        if !StaticSignature::of(db, value).flags.contains(StaticFlags::EXTERN) {
                            bodies.push(DefWithBodyId::StaticId(value));
                        }
                        if keeps_static(db, value) {
                            statics.push(value);
                        }
                    }
                    ModuleDefId::ConstId(it) => bodies.push(DefWithBodyId::ConstId(it)),
                    _ => {}
                }
            }
            for it in scope.unnamed_consts() {
                bodies.push(DefWithBodyId::ConstId(it));
            }
            for imp in scope.impls() {
                for &(_, item) in imp.impl_items(db).items.iter() {
                    match item {
                        AssocItemId::FunctionId(func) => {
                            if keeps_function(db, func) {
                                functions.push(func);
                            }
                            if FunctionSignature::of(db, func).has_body() {
                                bodies.push(DefWithBodyId::FunctionId(func));
                            }
                        }
                        AssocItemId::ConstId(it) => bodies.push(DefWithBodyId::ConstId(it)),
                        AssocItemId::TypeAliasId(_) => {}
                    }
                }
            }
        }
        for owner in bodies.drain(..) {
            let store = ExpressionStore::of(db, ExpressionStoreOwnerId::Body(owner));
            maps.extend(store.blocks(db).map(|(_, map)| map));
        }
    }
    names(db, &functions, &statics)
}

/// How many bodies `ran` lowered, by the name the run log gives the lowering.
fn lowered(ran: &[String]) -> usize {
    ran.iter().filter(|it| it.contains("Body::of")).count()
}

/// Whether `ran` lowered at most the `owners` bodies it may: a body another query lowered
/// first is read, not lowered again, and the run log has no row of it. The impls maintainer
/// does that when the fixture sets its crate graph: it reads the bodies of every module whose
/// bodies may hold an impl (`method_resolution::delta`'s body parts), which are the bodies with
/// items in their blocks, `f`'s and `g`'s here. So the bound is the check: a body none of them
/// may declare (`h`'s, `i`'s, `S::plain`'s) is lowered by nobody before, and `kept` lowering it
/// is a row more than `owners`.
fn lowered_at_most(ran: &[String], owners: usize) -> bool {
    lowered(ran) <= owners
}

/// A database of [`FIXTURE`] that reads `lib` from its image, its def map pages and its
/// kept-roots record, all written into `memory` by a database that resolved it; with the
/// record's bytes. `record` makes the record from the one written (the planted bug of a
/// control); `None` keeps it.
// dyn: a test's planted record, one a call
fn served(record: Option<&dyn Fn(&TestDB, Crate) -> Vec<u8>>) -> (TestDB, Vec<u8>) {
    let memory = Memory::default();
    let mut first = fixture::db(FIXTURE).into_db();
    let crates: Vec<Crate> = base_db::all_crates(&first).iter().copied().collect();
    let served = hir_def::deps::serve(&mut first, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new(), "premise: a cold store");
    let stored = hir_def::deps::store(&first, &served, &memory, hir_def::deps::Which::Sealed).unwrap();
    let lib = crate_named(&first, "lib");
    let filed = stored.filed[&lib];
    let refused = frozen::store(&first, &[(lib, filed)], &memory).unwrap();
    assert_eq!(refused, Vec::new(), "`lib`'s kept roots do not encode");
    let key = hir_def::deps::kept_key(filed);
    if let Some(record) = record {
        let planted = record(&first, lib);
        memory.values.lock().insert(key, Arc::new(Aligned::copy_from(&planted)));
    }
    let bytes = memory.values.lock()[&key].to_vec();

    let mut db = fixture::db(FIXTURE).into_db();
    let crates: Vec<Crate> = base_db::all_crates(&db).iter().copied().collect();
    let served = hir_def::deps::serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(served.hits, [crate_named(&db, "lib")], "premise: `lib` is served");
    let given = frozen::serve(&mut db, &served, &memory, Mode::Serve).unwrap();
    assert_eq!(given, [crate_named(&db, "lib")], "premise: `lib`'s record is given");
    (db, bytes)
}

/// The invariant: what a served crate keeps, read from its record, is what the walk of every
/// body of it keeps (the reference model), and finding it lowers the bodies the record names
/// alone (`f`'s and `g`'s, whose blocks declare `in_block` and `NESTED`): no other body of
/// the crate. The record written again from the crate as served is the bytes it was read
/// from, byte for byte (rule 26).
#[test]
fn kept_of_a_served_crate_equals_the_walk_and_lowers_only_its_owners() {
    let (db, bytes) = served(None);
    let lib = crate_named(&db, "lib");
    let mut found = None;
    let ran = db.log_executed(|| found = Some(kept(&db, lib)));
    let found = found.unwrap();
    assert_eq!(found.owners.len(), 2, "`f` and `g` declare a kept item: {:?}", found.owners);
    assert!(lowered_at_most(&ran, found.owners.len()), "lowered beyond the record's owners: {ran:?}");
    let fresh = fixture::db(FIXTURE).into_db();
    let expected = reference(&fresh, crate_named(&fresh, "lib"));
    assert_eq!(expected, ["NESTED", "in_block", "in_impl", "top"], "premise: the model");
    assert_eq!(names(&db, &found.functions, &found.statics), expected);
    assert_eq!(frozen::encode(&db, lib).unwrap(), bytes, "the record read back is other bytes");
}

/// The control: a record that names no owner (as one written without the walk of the bodies
/// would be) is caught by the same comparison: the items declared in blocks are missing.
#[test]
fn the_oracle_catches_a_record_without_its_owners() {
    let without_owners = |db: &TestDB, krate: Crate| {
        let (functions, statics) = super::at_module_level(db, krate);
        frozen::encode_lists(db, krate, &functions, &statics, &[]).unwrap()
    };
    let (db, _) = served(Some(&without_owners));
    let lib = crate_named(&db, "lib");
    let found = kept(&db, lib);
    assert!(found.owners.is_empty(), "premise: the planted record names no owner");
    let fresh = fixture::db(FIXTURE).into_db();
    let expected = reference(&fresh, crate_named(&fresh, "lib"));
    assert_ne!(names(&db, &found.functions, &found.statics), expected, "the oracle missed it");
}

/// The invariant: what a crate that is not served keeps is what the walk of every body of it
/// keeps, and finding it lowers only the bodies whose text can say a root
/// (`bodies_declaring_roots`): `f`'s and `g`'s, not `h`'s, `i`'s or `S::plain`'s. The
/// control: the cause removed, the same lists over those bodies less `f` (a filter that
/// skipped the body declaring `in_block`) are caught by the comparison.
#[test]
fn an_unserved_crate_lowers_only_bodies_that_can_say_a_root() {
    let db = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&db, "lib");
    let mut found = None;
    let ran = db.log_executed(|| found = Some(kept(&db, lib)));
    let found = found.unwrap();
    let saying = bodies_declaring_roots(&db, lib);
    assert!(lowered_at_most(&ran, saying.len()), "lowered other bodies than those that say a root: {ran:?}");
    assert_eq!(saying.len(), 2, "premise: `f` and `g` alone can say one: {saying:?}");
    let fresh = fixture::db(FIXTURE).into_db();
    let expected = reference(&fresh, crate_named(&fresh, "lib"));
    assert_eq!(names(&db, &found.functions, &found.statics), expected);

    // The control: a filter that skipped `f`.
    let f = saying.iter().copied().find(|it| match it {
        DefWithBodyId::FunctionId(it) => FunctionSignature::of(&db, *it).name.as_str() == "f",
        _ => false,
    });
    let skipped: Vec<DefWithBodyId> = saying.iter().copied().filter(|it| Some(*it) != f).collect();
    assert_eq!(skipped.len(), 1, "premise: `f` is one of the bodies");
    let planted = over_bodies(&db, lib, &skipped);
    assert_ne!(names(&db, &planted.functions, &planted.statics), expected, "the oracle missed it");
}

/// The invariant: a record of another layout than [`frozen::MAGIC`]'s is refused by name: the
/// query that reads it stops with [`hir_def::nameres::frozen::Unreadable`], naming the crate,
/// the record and the layout; nothing is found from source in its place.
#[test]
fn a_record_of_another_layout_is_refused_by_name() {
    let other_layout = |db: &TestDB, krate: Crate| {
        let mut bytes = frozen::encode(db, krate).unwrap();
        bytes[..8].copy_from_slice(b"bzkept00");
        bytes
    };
    let (db, _) = served(Some(&other_layout));
    let lib = crate_named(&db, "lib");
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| _ = kept(&db, lib)));
    let Err(payload) = caught else { panic!("a record of another layout was read") };
    let Some(stop) = hir_def::nameres::frozen::Unreadable::of_payload(&*payload) else {
        panic!("the refusal unwound with another payload than `Unreadable`")
    };
    assert_eq!((stop.krate, stop.kind, stop.name.as_deref()), (lib, "kept roots", Some("lib")));
    assert!(stop.why.contains("another layout") && stop.why.contains("bzkept00"), "{}", stop.why);
}
