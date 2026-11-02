//! `lock_across_query` over [`super`]'s fixtures, with a `blaze_query_macros` whose `tracked`,
//! `input` and `interned` are given the test proc macro `identity` (so what each makes is the
//! item it is on, defined in the attribute's expansion), `base_db`'s `Db`, and a `parking_lot`
//! whose guards are `lock_api`'s.

use base_db::all_crates;
use hir_expand::{
    fixture::{self, Fixture},
    proc_macro::{ProcMacro, ProcMacros},
};
use intern::Symbol;

use super::{assert_marked, fixture as joined};
use crate::{conf::Conf, tests::lint_given};

const CRATES: &str = r#"
//- /blaze_query_macros.rs crate:proc-macro:blaze_query_macros
#[proc_macro_attribute]
pub fn tracked(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
#[proc_macro_attribute]
pub fn input(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
#[proc_macro_attribute]
pub fn interned(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
//- /blaze_query.rs crate:blaze_query deps:blaze_query_macros
pub use blaze_query_macros::{input, interned, tracked};
//- /base_db.rs crate:base_db
pub struct Db;
//- /lock_api.rs crate:lock_api
pub struct Mutex<R, T>(R, T);
pub struct MutexGuard<'a, R, T>(&'a Mutex<R, T>);
impl<R, T> Mutex<R, T> {
    pub fn lock(&self) -> MutexGuard<'_, R, T> {
        MutexGuard(self)
    }
}
impl<R, T> Drop for MutexGuard<'_, R, T> {
    fn drop(&mut self) {}
}
impl<R, T> core::ops::Deref for MutexGuard<'_, R, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.1
    }
}
//- /parking_lot.rs crate:parking_lot deps:lock_api
pub struct RawMutex;
pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutex, T>;
//- /src/lib.rs crate:main deps:blaze_query,base_db,parking_lot,std
"#;

/// The test proc macro `identity`, as the fixture's proc macros' crate has it.
fn identity() -> ProcMacro {
    let built = fixture::db("//- proc_macros: identity\n//- /main.rs crate:main\n");
    let db = built.db();
    let krate = *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == "proc_macros"))
        .expect("the fixture has the proc macros' crate");
    let loaded = ProcMacros::get_for_crate(db, krate).expect("the proc macros' crate has its macros");
    loaded.macros().expect("they loaded")[0].clone() // clone: a test's macro, once per fixture
}

/// Gives `blaze_query_macros` its three attribute macros, each `identity` under its name.
fn give_query_macros(built: &mut Fixture) {
    let identity = identity();
    let db = built.db();
    let krate = *all_crates(db)
        .iter()
        .find(|it| {
            it.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == "blaze_query_macros")
        })
        .expect("the fixture has `blaze_query_macros`");
    let macros = ["tracked", "input", "interned"]
        .map(|name| ProcMacro { name: Symbol::intern(name), ..identity.clone() }) // clone: a test's macro
        .to_vec();
    built.set_proc_macros(krate, macros);
}

/// A `parking_lot` guard (written, through a renamed type, in a macro) or std's held across a
/// `#[blaze_query::tracked]` function (also through a renamed attribute) or a method of a trait
/// implemented for `base_db::Db` is a finding; a guard dropped before the call, and one held
/// across a call that is no query, are none.
#[test]
fn lock_across_query() {
    let (fixture, source) = joined(
        CRATES,
        r#"
use base_db::Db;
use blaze_query::tracked as query;
use parking_lot::Mutex;
use parking_lot::Mutex as Lock;

#[blaze_query::tracked]
pub fn width(db: &Db, x: u32) -> u32 {
    let _ = db;
    x
}

#[query]
pub fn depth(db: &Db, x: u32) -> u32 {
    let _ = db;
    x
}

pub fn plain(x: u32) -> u32 {
    x
}

pub trait HirDatabase {
    fn infer(&self, x: u32) -> u32;
}

impl HirDatabase for Db {
    fn infer(&self, x: u32) -> u32 {
        x
    }
}

pub struct Cache {
    table: Mutex<u32>,
    other: Lock<u32>,
    std_table: std::sync::Mutex<u32>,
}

pub fn held(db: &Db, cache: &Cache) -> u32 {
    let guard = cache.table.lock();
    let w = width(db, *guard); //~ lock_across_query
    w + *guard
}

pub fn through_a_renamed_type_and_attribute(db: &Db, cache: &Cache) -> u32 {
    let guard = cache.other.lock();
    let d = depth(db, 1); //~ lock_across_query
    d + *guard
}

pub fn through_the_database(db: &Db, cache: &Cache) -> u32 {
    let guard = cache.table.lock();
    db.infer(*guard) //~ lock_across_query
}

pub fn std_guard(db: &Db, cache: &Cache) -> u32 {
    let _guard = cache.std_table.lock();
    width(db, 1) //~ lock_across_query
}

pub fn dropped_first(db: &Db, cache: &Cache) -> u32 {
    let n = {
        let guard = cache.table.lock();
        *guard
    };
    width(db, n)
}

pub fn no_query(cache: &Cache) -> u32 {
    let guard = cache.table.lock();
    let p = plain(1);
    p + *guard
}

macro_rules! locked {
    ($db:expr, $cache:expr) => {{
        let guard = $cache.table.lock();
        width($db, 1) + *guard
    }};
}

pub fn by_macro(db: &Db, cache: &Cache) -> u32 {
    locked!(db, cache) //~ lock_across_query
}
"#,
    );
    let flags = "-A clippy::all --deny=blaze::lock_across_query";
    let out = lint_given(&fixture, flags, Conf::default(), give_query_macros);
    assert_marked(&out, &source, flags);
    assert!(out.contains("the lock guard `guard` (`"), "the guard is named with its type:\n{out}");
    assert!(out.contains("held across the query `width` (made by `#[blaze_query::tracked]`)"), "{out}");
    assert!(out.contains("held across the query `infer` (a method of `HirDatabase` on the database)"), "{out}");
}
