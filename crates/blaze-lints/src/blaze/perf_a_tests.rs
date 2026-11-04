//! What `hash_twice`, `syscall_in_loop`, `query_in_loop` and `string_key` run over in their
//! tests: a `hashbrown` with a map and a set, an `std` with `fs::metadata` and `File::open`,
//! `blaze_query_macros`' attributes (each the test proc macro `identity`), `base_db`'s `Db`, and
//! `intern`'s `Symbol` and `hir_expand`'s `Name`.

use base_db::all_crates;
use hir_expand::{
    fixture::{self, Fixture},
    proc_macro::{ProcMacro, ProcMacros},
};
use intern::Symbol;

use crate::{conf::Conf, tests::lint_given};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, fn, iterator, iterators, slice, deref, drop, index, builtin_impls\n";

const CRATES: &str = r#"
//- /hashbrown.rs crate:hashbrown
pub struct HashMap<K, V> { k: K, v: V }
pub struct Entry<'a, K, V> { map: &'a mut HashMap<K, V> }
impl<'a, K, V> Entry<'a, K, V> {
    pub fn or_insert(self, v: V) -> &'a mut V { loop {} }
}
impl<K, V> HashMap<K, V> {
    pub fn get(&self, k: &K) -> Option<&V> { None }
    pub fn contains_key(&self, k: &K) -> bool { false }
    pub fn insert(&mut self, k: K, v: V) -> Option<V> { None }
    pub fn remove(&mut self, k: &K) -> Option<V> { None }
    pub fn entry(&mut self, k: K) -> Entry<'_, K, V> { loop {} }
}
pub struct HashSet<T> { t: T }
impl<T> HashSet<T> {
    pub fn contains(&self, t: &T) -> bool { false }
    pub fn insert(&mut self, t: T) -> bool { false }
}
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 { pub use core::prelude::v1::*; }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod fs {
    pub struct Metadata { len: u64 }
    pub fn metadata(path: &str) -> Metadata { loop {} }
    pub struct File { fd: i32 }
    impl File {
        pub fn open(path: &str) -> File { loop {} }
    }
}
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
//- /intern.rs crate:intern
pub struct Symbol { id: u32 }
impl Symbol {
    pub fn as_str(&self) -> &'static str { "" }
}
//- /hir_expand.rs crate:hir_expand deps:intern
pub mod name {
    pub struct Name { symbol: intern::Symbol }
    impl Name {
        pub fn as_str(&self) -> &str { "" }
    }
}
"#;

/// The fixture of the crate `krate` written `main`, with [`CRATES`] beside it.
fn fixture(krate: &str, main: &str) -> String {
    format!(
        "{MINICORE}{CRATES}//- /src/lib.rs crate:{krate} deps:hashbrown,std,blaze_query,base_db,intern,hir_expand\n{}",
        main.trim_start_matches('\n')
    )
}

/// The test proc macro `identity`, as the fixture's proc macros' crate has it.
fn identity() -> ProcMacro {
    let built = fixture::db("//- proc_macros: identity\n//- /main.rs crate:main\n");
    let db = built.db();
    let krate = *all_crates(db)
        .iter()
        .find(|it| {
            it.extra_data(db)
                .display_name
                .as_ref()
                .is_some_and(|it| it.crate_name().as_str() == "proc_macros")
        })
        .expect("the fixture has the proc macros' crate");
    let loaded =
        ProcMacros::get_for_crate(db, krate).expect("the proc macros' crate has its macros");
    loaded.macros().expect("they loaded")[0].clone() // clone: a test's macro, once per fixture
}

/// Gives `blaze_query_macros` its three attribute macros, each `identity` under its name.
fn give_query_macros(built: &mut Fixture) {
    let identity = identity();
    let db = built.db();
    let krate = *all_crates(db)
        .iter()
        .find(|it| {
            it.extra_data(db)
                .display_name
                .as_ref()
                .is_some_and(|it| it.crate_name().as_str() == "blaze_query_macros")
        })
        .expect("the fixture has `blaze_query_macros`");
    let macros = ["tracked", "input", "interned"]
        .map(|name| ProcMacro { name: Symbol::intern(name), ..identity.clone() }) // clone: a test's macro
        .to_vec();
    built.set_proc_macros(krate, macros);
}

/// What `blaze::<name>` alone says of `main`, the crate named `krate`.
fn only(name: &str, krate: &str, main: &str) -> String {
    lint_given(
        &fixture(krate, main),
        &format!("-A clippy::all --warn=blaze::{name}"),
        Conf::default(),
        give_query_macros,
    )
}

/// `hash_twice`: `contains_key` then `insert` is found with the entry API as its fix, a set's
/// `contains` then `insert` with `insert` alone, and `get` then `remove` with advice; a write
/// in another branch than the lookup, and one after the key changed, are not.
#[test]
fn hash_twice_finds_a_lookup_then_a_write_of_one_key() {
    let out = only(
        "hash_twice",
        "main",
        r#"
use hashbrown::{HashMap, HashSet};
pub fn classic(m: &mut HashMap<u32, u32>, k: u32) {
    if !m.contains_key(&k) {
        m.insert(k, 1);
    }
}
pub fn set(s: &mut HashSet<u32>, k: u32) {
    if !s.contains(&k) {
        s.insert(k);
    }
}
pub fn get_then_remove(m: &mut HashMap<u32, u32>, k: u32) -> bool {
    let had = m.get(&k).is_some();
    m.remove(&k);
    had
}
pub fn branches(m: &mut HashMap<u32, u32>, k: u32, b: bool) {
    if b {
        let _ = m.get(&k);
    } else {
        m.insert(k, 2);
    }
}
pub fn changed(m: &mut HashMap<u32, u32>, mut k: u32) {
    if !m.contains_key(&k) {
        k += 1;
        m.insert(k, 1);
    }
}
pub fn other_map(m: &mut HashMap<u32, u32>, n: &mut HashMap<u32, u32>, k: u32) {
    if !m.contains_key(&k) {
        n.insert(k, 1);
    }
}
"#,
    );
    assert_eq!(out.matches("hashes and looks up `k` twice").count(), 3, "{out}");
    assert!(out.contains("m.entry(k).or_insert(1);"), "the fix uses the entry API:\n{out}");
    assert!(out.contains("s.insert(k);"), "a set's fix is `insert` alone:\n{out}");
    assert!(out.contains("`remove` hands back the value it took"), "{out}");
}

/// `syscall_in_loop`: in `blaze_vfs`, a stat in a `for` loop and an open in a `map` closure are
/// found, one through a helper of the crate naming the stat it reaches; a stat outside a loop
/// is not, and the same loop in a crate whose file system work is not batched is not.
#[test]
fn syscall_in_loop_finds_a_stat_per_element_in_store_crates() {
    let main = r#"
pub fn size(p: &str) -> u64 {
    let _ = std::fs::metadata(p);
    0
}
pub fn sizes(paths: [&str; 2]) {
    for p in paths {
        let _ = std::fs::metadata(p);
    }
}
pub fn through_a_helper(paths: [&str; 2]) {
    for p in paths {
        let _ = size(p);
    }
}
pub fn opens(paths: [&str; 2]) {
    let _ = paths.into_iter().map(|p| std::fs::File::open(p));
}
"#;
    let out = only("syscall_in_loop", "blaze_vfs", main);
    assert_eq!(out.matches("stats or opens a path once per element").count(), 3, "{out}");
    assert!(
        out.contains("`size`, which reaches `metadata`,"),
        "the helper names the stat it reaches:\n{out}"
    );
    assert!(out.contains("the closure given to `map`"), "{out}");
    assert!(out.contains("blaze_core::Effect::Stamp"), "the help names the batch:\n{out}");
    let elsewhere = only("syscall_in_loop", "main", main);
    assert!(
        !elsewhere.contains("stats or opens a path"),
        "a crate not batched is not checked:\n{elsewhere}"
    );
}

/// `query_in_loop`: a query called with each element where a batch query beside it returns a
/// slice of what it returns is found naming both; a query with no batch, and a call whose key
/// is the same every time, are not.
#[test]
fn query_in_loop_names_the_batch_query() {
    let out = only(
        "query_in_loop",
        "main",
        r#"
use base_db::Db;

#[blaze_query::tracked]
pub fn width(db: &Db, x: u32) -> u32 {
    let _ = db;
    x
}

#[blaze_query::tracked]
pub fn widths(db: &Db, xs: &'static [u32]) -> &'static [u32] {
    let _ = db;
    xs
}

#[blaze_query::tracked]
pub fn depth(db: &Db, x: u64) -> u64 {
    let _ = db;
    x
}

pub fn each(db: &Db, xs: [u32; 2]) -> u32 {
    let mut total = 0;
    for x in xs {
        total += width(db, x);
    }
    total
}

pub fn no_batch(db: &Db, xs: [u64; 2]) -> u64 {
    let mut total = 0;
    for x in xs {
        total += depth(db, x);
    }
    total
}

pub fn invariant(db: &Db, xs: [u32; 2]) -> u32 {
    let mut total = 0;
    for _ in xs {
        total += width(db, 1);
    }
    total
}
"#,
    );
    assert_eq!(out.matches("is called once per element of").count(), 1, "{out}");
    assert!(out.contains("the query `width`"), "{out}");
    assert!(out.contains("`widths` takes them all in one call"), "{out}");
    assert!(out.contains("records a dependency edge"), "{out}");
}

/// `string_key`: a map keyed by `&str` whose every key is a `Name`'s or a `Symbol`'s text (one
/// through a `let`) is found once; one also given a key from user data is not.
#[test]
fn string_key_finds_identifier_keys_only() {
    let out = only(
        "string_key",
        "main",
        r#"
use hashbrown::HashMap;
use hir_expand::name::Name;
use intern::Symbol;

pub fn names<'a>(a: &'a Name, b: &'a Symbol) -> HashMap<&'a str, u32> {
    let mut m: HashMap<&'a str, u32> = loop {};
    m.insert(a.as_str(), 1);
    let text = b.as_str();
    m.insert(text, 2);
    m
}

pub fn mixed<'a>(a: &'a Name, line: &'a str) -> HashMap<&'a str, u32> {
    let mut m: HashMap<&'a str, u32> = loop {};
    m.insert(a.as_str(), 1);
    m.insert(line, 2);
    m
}
"#,
    );
    assert_eq!(out.matches("key it by `intern::Symbol`").count(), 1, "{out}");
    assert!(out.contains("`m` is keyed by `&str`"), "{out}");
}
