//! Constants of dependencies served from the store against the same constants evaluated, through
//! `const_eval` (`hir_ty::consteval::deps`).

use std::sync::Arc;

use base_db::{ContentId, Crate, Env, all_crates};
use blaze_hash::HashMap;
use hir_def::{
    ConstId, ModuleDefId,
    deps::{Held, ImageBuf, ImageStore, Mode},
    nameres::{crate_def_map, frozen},
};
use hir_expand::fixture;
use parking_lot::Mutex;

use crate::{
    consteval::{
        ConstEvalError,
        deps::{Drained, drain, set},
    },
    db::{GeneralConstId, HirDatabase},
    mir::MirEvalError,
    next_solver::GenericArgs,
    test_db::TestDB,
};

const BASE: ContentId = ContentId([5; 32]);

const FIXTURE: &str = r#"
//- minicore: coerce_unsized, slice
//- /main.rs crate:main deps:lib new_source_root:local
const MINE: u8 = lib::ONE + 1;
//- /lib.rs crate:lib new_source_root:library
const fn twice(n: u16) -> u16 { n * 2 }
pub const ONE: u8 = 1;
pub const FLAG: bool = ONE == 1;
pub const LETTER: char = 'z';
pub const WIDE: i128 = -(1 << 100);
pub const HALF: f64 = 0.5;
pub const ROW: [u16; 3] = [twice(1), twice(2), twice(3)];
pub const PAIR: (u8, [u16; 2], ()) = (ONE, [7, 8], ());
pub const NAME: &str = "kept";
pub const BYTES: &[u8] = b"abc";
pub const ARRAY_REF: &[u32; 2] = &[11, 12];
"#;

const KEPT: [&str; 10] =
    ["ONE", "FLAG", "LETTER", "WIDE", "HALF", "ROW", "PAIR", "NAME", "BYTES", "ARRAY_REF"];

/// The bytes a store keeps, as it gave them.
struct Bytes(Vec<u8>);

impl Held for Bytes {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A store in memory; [`Memory::replace`] stands for one whose bytes changed under their key.
#[derive(Default)]
struct Memory {
    values: Mutex<HashMap<ContentId, Arc<Bytes>>>,
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(self.values.lock().get(key).map(|it| Arc::clone(it) as ImageBuf))
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        let mut values = self.values.lock();
        match values.get(&key) {
            Some(kept) if kept.0 != value => Err(format!("{key:?} is kept with other bytes")),
            Some(_) => Ok(()),
            None => {
                values.insert(key, Arc::new(Bytes(value)));
                Ok(())
            }
        }
    }

    fn reads_now(&self, _: &Env, _: &[u8]) -> Result<ContentId, String> {
        Err("these fixtures expand no proc macro".to_owned())
    }
}

impl Memory {
    fn replace(&self, change: impl Fn(&mut Vec<u8>)) {
        for value in self.values.lock().values_mut() {
            let mut bytes = value.0.clone();
            change(&mut bytes);
            *value = Arc::new(Bytes(bytes));
        }
    }

    fn len(&self) -> usize {
        self.values.lock().len()
    }
}

/// Makes the kept value of `ONE` 9: a `u8` (type 11), a memory of one byte, and an empty map
/// end the value, which no other constant of [`FIXTURE`] does.
fn one_is_nine(bytes: &mut Vec<u8>) {
    if bytes.ends_with(&[11, 1, 0, 0, 0, 1, 0]) {
        let at = bytes.len() - 2;
        bytes[at] = 9;
    }
}

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

fn const_named(db: &TestDB, krate: Crate, name: &str) -> ConstId {
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            if let ModuleDefId::ConstId(it) = def
                && GeneralConstId::ConstId(it).name(db) == name
            {
                return it;
            }
        }
    }
    panic!("no constant {name}")
}

/// A database of [`FIXTURE`] keeping `lib`'s constants in `store` under `base`.
fn database(store: &Arc<Memory>, base: Option<ContentId>, mode: Mode) -> (TestDB, Crate) {
    let mut db = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&db, "lib");
    let crates = HashMap::from_iter([(lib, base)]);
    // dyn: hir-ty's `set` takes the driver's store, a boundary (`consteval::deps::Shared::store`)
    set(&mut db, Arc::clone(store) as Arc<dyn ImageStore + Send + Sync>, crates, mode);
    (db, lib)
}

/// What each of `names` evaluates to in `db`, or is served as, written out.
fn values(db: &TestDB, krate: Crate, names: &[&str]) -> Vec<String> {
    let mut values = Vec::with_capacity(names.len());
    for name in names {
        let value = db
            .const_eval(const_named(db, krate, name), GenericArgs::empty(), None)
            .unwrap_or_else(|_| panic!("`{name}` does not evaluate"));
        values.push(format!("{value:?}"));
    }
    values
}

/// `lib`'s constants evaluated in a session that checks `lib`, kept under [`BASE`].
fn evaluated(store: &Arc<Memory>) -> Vec<String> {
    let (db, lib) = database(store, None, Mode::Serve);
    let evaluated = values(&db, lib, &KEPT);
    let bases = HashMap::from_iter([(lib, BASE)]);
    assert_eq!(drain(&db, &bases), Ok(Drained { written: KEPT.len(), unkeyed: 0 }));
    assert_eq!(store.len(), KEPT.len());
    evaluated
}

#[test]
fn a_served_constant_equals_the_evaluated_one() {
    let store = Arc::new(Memory::default());
    let evaluated = evaluated(&store);
    let (db, lib) = database(&store, Some(BASE), Mode::Serve);
    assert_eq!(values(&db, lib, &KEPT), evaluated);
    assert_eq!(drain(&db, &HashMap::default()), Ok(Drained::default()), "every one was served");
    let main = crate_named(&db, "main");
    let mine = db.const_eval(const_named(&db, main, "MINE"), GenericArgs::empty(), None).unwrap();
    assert_eq!(mine.memory[..], [2], "a member's constant reads a served one");

    store.replace(one_is_nine);
    let (db, lib) = database(&store, Some(BASE), Mode::Serve);
    let one = db.const_eval(const_named(&db, lib, "ONE"), GenericArgs::empty(), None).unwrap();
    assert_eq!(one.memory[..], [9], "served from the store, not evaluated");
}

#[test]
fn check_mode_records_a_difference() {
    let store = Arc::new(Memory::default());
    evaluated(&store);
    _ = frozen::differences();
    let (db, lib) = database(&store, Some(BASE), Mode::Check);
    assert_eq!(values(&db, lib, &KEPT).len(), KEPT.len());
    let of_constants = |found: Vec<String>| -> Vec<String> {
        found.into_iter().filter(|it| it.contains("constant values")).collect()
    };
    assert_eq!(of_constants(frozen::differences()), Vec::<String>::new(), "evaluated alike");

    store.replace(one_is_nine);
    let (db, lib) = database(&store, Some(BASE), Mode::Check);
    let one = db.const_eval(const_named(&db, lib, "ONE"), GenericArgs::empty(), None).unwrap();
    assert_eq!(one.memory[..], [1], "check mode evaluates");
    let found = of_constants(frozen::differences());
    assert!(found.len() == 1 && found[0].contains("`ONE`"), "{found:?}");
}

#[test]
fn an_unreadable_kept_value_is_an_error() {
    let store = Arc::new(Memory::default());
    evaluated(&store);
    store.replace(|bytes| bytes.push(0));
    let (db, lib) = database(&store, Some(BASE), Mode::Serve);
    let one = db.const_eval(const_named(&db, lib, "ONE"), GenericArgs::empty(), None);
    let Err(ConstEvalError::MirEvalError(MirEvalError::InternalError(said))) = one else {
        panic!("a damaged kept value is served or evaluated: {one:?}")
    };
    assert!(said.contains("`ONE`") && said.contains("bytes after the value"), "{said}");
}
