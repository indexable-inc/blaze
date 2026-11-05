//! `borrow_workaround` and `nll_workaround`: one test per shape that is found, one per near
//! miss (a fix the borrow checker would refuse, a fix that would change what runs).

use blaze_query::db::Setter as _;
use hir_expand::fixture::Fixture;

use crate::{conf::Conf, tests::lint_given};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, index, drop, deref\n";

const MAP: &str = r#"
//- /hashbrown.rs crate:hashbrown
pub struct HashMap<K, V> { k: K, v: V }
impl<K, V> HashMap<K, V> {
    pub fn get(&self, k: &K) -> Option<&V> { None }
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> { None }
    pub fn contains_key(&self, k: &K) -> bool { false }
    pub fn insert(&mut self, k: K, v: V) -> Option<V> { None }
}
impl<K, V> core::ops::Index<&K> for HashMap<K, V> {
    type Output = V;
    fn index(&self, k: &K) -> &V { loop {} }
}
"#;

const FLAGS: &str = "-A clippy::all --warn=blaze::borrow_workaround --warn=blaze::nll_workaround";

fn fixture(main: &str) -> String {
    format!(
        "{MINICORE}{MAP}//- /src/lib.rs crate:main deps:hashbrown\n{}",
        main.trim_start_matches('\n')
    )
}

fn run(main: &str, given: fn(&mut Fixture)) -> String {
    lint_given(&fixture(main), FLAGS, Conf::default(), given)
}

fn as_built(_: &mut Fixture) {}

/// The database checks borrows as stable Rust does (`BLAZE_BORROWCK=nll`).
fn nll(built: &mut Fixture) {
    let settings = base_db::FrontSettings::get(built.db());
    settings.set_borrowck(built.db_mut()).to(base_db::BorrowckMode::Nll);
}

/// NLL problem case #3: `contains_key` and then `get_mut` returned, where stable Rust rejects
/// `if let Some(found) = m.get_mut(&k) { return found; }` and blaze's checker accepts it.
const CASE_3: &str = r#"
use hashbrown::HashMap;
pub fn get_default(m: &mut HashMap<u32, u32>, k: u32) -> &mut u32 {
    if m.contains_key(&k) {
        return m.get_mut(&k).unwrap();
    }
    m.insert(k, 0);
    m.get_mut(&k).unwrap()
}
"#;

/// Problem case #3 is `nll_workaround`, its fix one lookup in an `if let`.
#[test]
fn nll_workaround_finds_problem_case_3() {
    let out = run(CASE_3, as_built);
    assert_eq!(out.matches("is looked up twice").count(), 1, "{out}");
    assert!(out.contains("nll_workaround"), "{out}");
    assert!(!out.contains("borrow_workaround`"), "{out}");
    assert!(out.contains("let Some(found) = m.get_mut(&k)"), "{out}");
    assert!(out.contains("return found;"), "{out}");
}

/// Near miss: in a database that checks as stable Rust does, the fix would not build, so
/// neither lint fires.
#[test]
fn nothing_when_the_fix_fails_the_checker() {
    let out = run(CASE_3, nll);
    assert!(!out.contains("is looked up twice"), "{out}");
}

/// A shared lookup, as `get` and as `&m[&k]`, is `borrow_workaround`: every checker accepts
/// the fix.
#[test]
fn borrow_workaround_finds_a_shared_second_lookup() {
    let out = run(
        r#"
use hashbrown::HashMap;
pub fn get(m: &HashMap<u32, u32>, k: u32) -> u32 {
    if m.contains_key(&k) {
        return *m.get(&k).unwrap();
    }
    0
}
pub fn index(m: &HashMap<u32, u32>, k: u32) -> u32 {
    if m.get(&k).is_some() {
        let v = &m[&k];
        return *v;
    }
    1
}
"#,
        as_built,
    );
    assert_eq!(out.matches("is looked up twice").count(), 2, "{out}");
    assert!(out.contains("borrow_workaround"), "{out}");
    assert!(out.contains("return *found;"), "{out}");
    assert!(out.contains("let v = found;"), "{out}");
}

/// `is_some` and then `as_ref` of the same option is `borrow_workaround`.
#[test]
fn borrow_workaround_finds_an_option_tested_then_unwrapped() {
    let out = run(
        r#"
pub fn first(o: &Option<u32>) -> u32 {
    if o.is_some() {
        return *o.as_ref().unwrap();
    }
    0
}
"#,
        as_built,
    );
    assert_eq!(out.matches("is looked up twice").count(), 1, "{out}");
    assert!(out.contains("let Some(found) = o.as_ref()"), "{out}");
}

/// Near misses that would change what runs: another key, and a call between the test and the
/// lookup (it may change the map, and the fix would run the lookup before it).
#[test]
fn nothing_when_the_fix_changes_what_runs() {
    let out = run(
        r#"
use hashbrown::HashMap;
fn touch(m: &mut HashMap<u32, u32>) {}
pub fn other_key(m: &mut HashMap<u32, u32>, a: u32, b: u32) -> &mut u32 {
    if m.contains_key(&a) {
        return m.get_mut(&b).unwrap();
    }
    m.insert(a, 0);
    m.get_mut(&a).unwrap()
}
pub fn call_between(m: &mut HashMap<u32, u32>, k: u32) -> &mut u32 {
    if m.contains_key(&k) {
        touch(m);
        return m.get_mut(&k).unwrap();
    }
    m.insert(k, 0);
    m.get_mut(&k).unwrap()
}
"#,
        as_built,
    );
    assert!(!out.contains("is looked up twice"), "{out}");
}
