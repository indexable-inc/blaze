//! Names on demand across crates (cells/lazy_deps.rs, engine.md piece E13): `dep` has 300
//! modules; `app` reads three names of it, one through a chain of re-exports (`a` -> `m10` ->
//! `m20`), one through a glob (`b` -> `m11::*`), one directly (`m7`). Solved with `app` whole
//! and `dep` on demand: the groups solved in `dep` are exactly the closure the whole solve's
//! records predict (`app`'s groups' reads and their tables' glob sources, which a lookup
//! across the crate line walks, followed through `dep`'s groups' reads), each
//! byte-equal to the whole solve's group, and `app`'s groups are byte-equal to the whole
//! solve's. The control: `m5` (reads `a`, nothing reads it) is never solved.

use std::collections::{BTreeMap, BTreeSet};
use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_own::AVec;

use super::entry::{Texts, input};
use crate::cells::names_on_demand;
use crate::graph::Sysroot;
use crate::resolve::{CrateInput, resolve_crates};
use crate::task::{Par, Serial};
use crate::GroupRead;

const MODULES: usize = 300;

struct Grouped;

impl Par for Grouped {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn names_groups(&self) -> bool {
        true
    }
}

fn texts() -> &'static Texts {
    let mut d = String::from("#![no_core]\n");
    for i in 0..MODULES {
        let body = match i {
            5 => String::from("pub use crate::a::X as X5;"),
            7 => String::from("pub struct Z;"),
            10 => String::from("pub use crate::m20::X;"),
            11 => String::from("pub struct Y;"),
            20 => String::from("pub struct X;"),
            _ => std::format!("pub struct S{i}; pub fn f() {{}}"),
        };
        d.push_str(&std::format!("pub mod m{i} {{ {body} }}\n"));
    }
    d.push_str("pub mod a { pub use crate::m10::X; }\npub mod b { pub use crate::m11::*; }\n");
    let d: &'static str = std::boxed::Box::leak(d.into_boxed_str());
    let app: &'static str = "#![no_core]\npub use dep::a::X;\npub use dep::b::Y;\npub use dep::m7::Z;\n";
    let dep_files: &'static [(&'static str, &'static str)] = std::boxed::Box::leak(std::vec![("/dep/lib.rs", d)].into_boxed_slice());
    let app_files: &'static [(&'static str, &'static str)] = std::boxed::Box::leak(std::vec![("/app/lib.rs", app)].into_boxed_slice());
    let crates: &'static [&'static [(&'static str, &'static str)]] = std::boxed::Box::leak(std::vec![dep_files, app_files].into_boxed_slice());
    std::boxed::Box::leak(std::boxed::Box::new(Texts(crates)))
}

fn inputs() -> [CrateInput<'static>; 2] {
    [input("dep", "/dep/lib.rs", &[]), input("app", "/app/lib.rs", &[("dep", 0)])]
}

#[test]
fn names_on_demand_across_crates_solve_the_closure_of_what_the_root_reads() {
    let texts = texts();
    let inputs = inputs();
    let whole = resolve_crates(&inputs, Sysroot::default(), texts, &Grouped, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the whole solve: {e}"));
    let (dep, app) = (whole.krate(0), whole.krate(1));
    // The prediction: `app`'s groups' reads into `dep`, then `dep`'s groups' reads, transitively.
    let mut of: BTreeMap<ItemKey, usize> = BTreeMap::new();
    for (i, g) in dep.groups.iter().enumerate() {
        for &m in g.members.iter() {
            of.insert(m, i);
        }
    }
    let target = |r: &GroupRead| match *r {
        GroupRead::Binding { table, .. } | GroupRead::Globs { table, .. } => Some(table),
        GroupRead::Textual { module, .. } => Some(module),
        GroupRead::Expansion { .. } => None,
    };
    // A read across the crate line is a lookup's whole answer (`Built::lookup_in`): it walks the
    // table's glob edges where the name is not bound there, and the reader's record names the
    // table alone. So each such table's glob sources, transitively, are reached too (an upper
    // bound: a name bound in the table itself walks none).
    let mut crossing: Vec<ItemKey> = app.groups.iter().flat_map(|g| g.reads.iter().filter_map(target)).filter(|t| of.contains_key(t)).collect();
    let mut seen_globs: BTreeSet<ItemKey> = BTreeSet::new();
    let mut todo: Vec<usize> = Vec::new();
    while let Some(t) = crossing.pop() {
        if !seen_globs.insert(t) {
            continue;
        }
        if let Some(&i) = of.get(&t) {
            todo.push(i);
        }
        if let Some(r) = dep.done.get(&t) {
            crossing.extend(r.scope.globs.iter().map(|e| e.source.key));
        }
    }
    let mut want: BTreeSet<usize> = BTreeSet::new();
    while let Some(i) = todo.pop() {
        if want.insert(i) {
            todo.extend(dep.groups[i].reads.iter().filter_map(target).filter_map(|t| of.get(&t).copied()));
        }
    }
    let want: BTreeSet<ItemKey> = want.iter().map(|&i| dep.groups[i].members[0]).collect();
    let path = |k: &ItemKey| dep.path_of(*k).map_or_else(|| std::format!("{k:?}"), String::from);
    let paths: Vec<String> = want.iter().map(path).collect();

    let lazy = names_on_demand(&inputs, Sysroot::default(), texts, &[1], blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("names on demand: {e:?}"));
    let got = &lazy.demanded[0];
    let solved: BTreeSet<ItemKey> = got.groups.iter().map(|g| g.members[0]).collect();
    std::println!(
        "NAMES ON DEMAND dep: {} of {} groups solved ({} solves asked, {} walks), predicted {} ({paths:?}); app solved {} times",
        solved.len(),
        dep.groups.len(),
        got.solves,
        lazy.walks[0],
        want.len(),
        lazy.root_solves
    );
    assert_eq!(solved, want, "the groups solved in dep are not the closure the whole solve predicts ({paths:?})");
    let by_first: BTreeMap<ItemKey, &crate::NameGroup<'_>> = dep.groups.iter().map(|g| (g.members[0], g)).collect();
    for g in got.groups.iter() {
        let w = by_first[&g.members[0]];
        assert!(g.members[..] == w.members[..], "{}: other members than the whole solve's", path(&g.members[0]));
        assert!(g.bytes[..] == w.bytes[..], "{}: its record differs from the whole solve's group", path(&g.members[0]));
    }
    assert!(!got.holds(dep.tree.modules.iter().find(|m| m.path.as_str() == "dep::m5").expect("m5").key), "m5, which nothing reads, was solved");
    let app_lazy = lazy.roots[1].expect("app was solved");
    assert_eq!(app_lazy.groups.len(), app.groups.len(), "app's groups");
    for (a, b) in app_lazy.groups.iter().zip(app.groups.iter()) {
        assert!(a.members[..] == b.members[..] && a.bytes[..] == b.bytes[..], "app's group records differ from the whole solve's");
    }
    assert!(want.len() < 10, "the closure is a handful of groups: {paths:?}");
}
