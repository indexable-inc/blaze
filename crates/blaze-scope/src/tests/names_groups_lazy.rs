//! Names on demand (cells/lazy.rs, engine.md piece E13): a crate of 500 modules of which 3 are
//! asked. The driver over the real one-group solve (cells/edit.rs `OneGroup` over the crate
//! solve with its members named) solves exactly the groups of the closure of the asked
//! modules' reads, as the whole solve's records predict it (followed from the asked modules'
//! groups through each record's `cells_read`), each record byte-equal to the whole solve's
//! group, in either order of demands (rule 25). The closure holds a re-export, a glob through
//! `super` (its path read, its source's names not: no lookup goes through it), and a cycle of
//! imports (two frames on the demand stack merged into one group).
//! The control: a module nothing asked (`m5` reads `m1`) is never solved.

use std::collections::{BTreeMap, BTreeSet};
use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_own::AVec;

use super::entry::{Texts, input};
use super::names_groups::Groups;
use crate::cells::{Demanded, DemandedOpts, Members, OneGroup, Solved, demand};
use crate::crate_scope::{CrateScopeError, Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::task::{Par, Serial};
use crate::NameGroup;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const MODULES: usize = 500;

/// The serial crate solve of the members named alone, their groups asked for.
struct Only<'r> {
    members: Members<'r>,
}

impl Par for Only<'_> {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn names_groups(&self) -> bool {
        true
    }

    fn names_members(&self) -> Option<Members<'_>> {
        Some(self.members)
    }
}

fn text() -> &'static Texts {
    let mut t = String::from("#![no_core]\n");
    for i in 0..MODULES {
        let body = match i {
            1 => String::from("pub use crate::m7::T; pub struct A;"),
            2 => String::from("pub use super::m9::*; pub struct B;"),
            // A cycle of named imports: one group (a glob alone reads its source's path, not
            // its names: the source is read when a lookup goes through the glob).
            3 => String::from("pub use crate::m4::D as D3; pub struct C;"),
            4 => String::from("pub use crate::m3::C as C4; pub struct D;"),
            5 => String::from("pub use crate::m1::A as A5;"),
            7 => String::from("pub struct T;"),
            9 => String::from("pub struct U;"),
            _ => std::format!("pub struct S{i}; pub fn f() {{}}"),
        };
        t.push_str(&std::format!("pub mod m{i} {{ {body} }}\n"));
    }
    let text: &'static str = std::boxed::Box::leak(t.into_boxed_str());
    let file: &'static [(&'static str, &'static str)] = std::boxed::Box::leak(std::vec![("/k0/lib.rs", text)].into_boxed_slice());
    let crates: &'static [&'static [(&'static str, &'static str)]] = std::boxed::Box::leak(std::vec![file].into_boxed_slice());
    std::boxed::Box::leak(std::boxed::Box::new(Texts(crates)))
}

/// The groups the whole solve's records say the asked modules reach: their groups, and each
/// group a record's `cells_read` names a module of, transitively; by first member.
fn predicted(whole: &Groups, asked: &[ItemKey]) -> BTreeSet<ItemKey> {
    let mut of: BTreeMap<ItemKey, usize> = BTreeMap::new();
    for (i, (_, g)) in whole.groups.iter().enumerate() {
        for &m in g.members.iter() {
            of.insert(m, i);
        }
    }
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut todo: Vec<usize> = asked.iter().map(|m| of[m]).collect();
    while let Some(i) = todo.pop() {
        if !seen.insert(i) {
            continue;
        }
        todo.extend(whole.groups[i].1.cells_read.iter().filter_map(|r| of.get(&r.0).copied()));
    }
    seen.iter().map(|&i| whole.groups[i].1.members[0]).collect()
}

/// The asked modules' groups on demand, `order` giving the demands (one call each).
fn on_demand(texts: &'static Texts, order: &[&[ItemKey]]) -> Demanded<'static> {
    let alloc = blaze_own::testing::heap().alloc();
    let solve_crate = move |members: Members<'_>| -> Result<Solved<'static>, CrateScopeError<'static>> {
        let bump = super::SyncFrame::leaked();
        let g = bump.alloc();
        let inputs = [input("k0", "/k0/lib.rs", &[])];
        let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Only { members }, &Ready::serial(WAIT), bump).map_err(|(_, e)| e)?;
        let mut out: AVec<'static, NameGroup<'static>> = AVec::new_in(g);
        out.extend(solved[0].names.groups.iter().cloned());
        Ok(Solved { groups: out, tree: solved[0].names.tree_record })
    };
    let mut solver = OneGroup { solve_crate, alloc, tree: None };
    let mut into = Demanded::new(DemandedOpts { alloc });
    for asked in order {
        demand(asked, &mut solver, &mut into, alloc).unwrap_or_else(|e| panic!("demand of {asked:?}: {e:?}"));
    }
    into
}

#[test]
fn names_groups_lazy_three_asked_of_five_hundred_solve_their_closure_alone() {
    let texts = text();
    let whole = Groups::solve(texts);
    let key = |path: &str| -> ItemKey { whole.groups.iter().find_map(|(ps, g)| ps.iter().position(|p| p == path).map(|i| g.members[i])).unwrap_or_else(|| panic!("no module {path}")) };
    let asked = [key("k0::m1"), key("k0::m2"), key("k0::m3")];
    let want = predicted(&whole, &asked);
    let by_first: BTreeMap<ItemKey, &(Vec<String>, NameGroup<'static>)> = whole.groups.iter().map(|e| (e.1.members[0], e)).collect();
    let paths: Vec<&Vec<String>> = want.iter().map(|k| &by_first[k].0).collect();

    let forward = on_demand(texts, &[&asked]);
    let (c, b, a) = (asked[2], asked[1], asked[0]);
    let backward = on_demand(texts, &[&[c], &[b, a]]);
    for (name, got) in [("forward", &forward), ("backward", &backward)] {
        let solved: BTreeSet<ItemKey> = got.groups.iter().map(|g| g.members[0]).collect();
        std::println!(
            "NAMES LAZY {name}: {} of {} groups solved on demand ({} solves asked, stack {} deep), predicted {} ({paths:?})",
            solved.len(),
            whole.groups.len(),
            got.solves,
            got.deepest,
            want.len()
        );
        assert_eq!(solved, want, "{name}: the groups solved are not the closure the whole solve's records predict ({paths:?})");
        for g in got.groups.iter() {
            let (p, w) = by_first[&g.members[0]];
            assert!(g.members[..] == w.members[..], "{name}: {p:?}: other members than the whole solve's");
            assert!(g.bytes[..] == w.bytes[..] && g.defines[..] == w.defines[..] && g.cells_read[..] == w.cells_read[..], "{name}: {p:?}: its record differs from the whole solve's group");
        }
        assert!(!got.holds(key("k0::m5")), "{name}: m5, which nothing asked, was solved");
    }
    let cycle = by_first.values().find(|(ps, _)| ps.iter().any(|p| p == "k0::m3")).expect("m3's group");
    assert_eq!(cycle.0.len(), 2, "m3 and m4 are one group: {:?}", cycle.0);
    assert!(want.len() < 12, "the closure is a handful of groups: {paths:?}");
}
