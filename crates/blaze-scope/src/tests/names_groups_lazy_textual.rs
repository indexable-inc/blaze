//! Names on demand through the textual scope of `macro_rules!` (cells/lazy_deps.rs over
//! cells/mod.rs `textual_recorded`): `dep`'s root declares `macro_rules! m` BEFORE `mod a;`,
//! and a second `macro_rules! m` (shadowing the first) before `mod b;`; `a` and `b` each call
//! `m!()`, `a`'s writing `pub fn f`, `b`'s `pub fn g`. The app reads `dep::a::f` and
//! `dep::b::g`. On demand, `a`'s and `b`'s one-group solves walk out of their text into the
//! root's: the root has no record yet, so it is demanded (`MemberRefusal::NoRecord`), and then
//! each walk reads the root's textual sequence before its `mod` line from the root's record.
//! The root itself calls `k!()` from its `#[macro_use] mod mac;` (app reads `dep::r`): its
//! walk into `mac`, which has no record yet, demands `mac` (before this, the typed refusal
//! `MemberRefusal::Textual` that stopped a std hello world in core). Groups solved: the root,
//! `mac`, `a` and `b`, each byte-equal (and equal in reads) to the whole
//! solve's group. The control (rule 21): an edit that adds a `macro_rules! m` between the
//! first and `mod a;` rewrites `a`'s record; the edit driver's entry rule over the on-demand
//! records enters `a` by its textual read of the root, and with that read dropped (the plant)
//! keeps `a`'s stale record.

use std::collections::{BTreeMap, BTreeSet};
use std::string::String;
use std::vec::Vec;

use blaze_id::{ContentId, ItemKey};
use blaze_own::AVec;

use super::entry::{Texts, input};
use crate::GroupRead;
use crate::NameGroup;
use crate::cells::{names_on_demand, textual_cell};
use crate::graph::Sysroot;
use crate::resolve::{CrateInput, resolve_crates};
use crate::task::{Par, Serial};

struct Grouped;

impl Par for Grouped {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn names_groups(&self) -> bool {
        true
    }
}

const DEP: &str = "#![no_core]
#[macro_use]
mod mac;
k!();
macro_rules! m { () => { pub fn f() {} } }
pub mod a;
macro_rules! m { () => { pub fn g() {} } }
pub mod b;
";
// A definition added between the first and `mod a;`: `a`'s call now writes `h`.
const DEP_EDITED: &str = "#![no_core]
#[macro_use]
mod mac;
k!();
macro_rules! m { () => { pub fn f() {} } }
macro_rules! m { () => { pub fn h() {} } }
pub mod a;
macro_rules! m { () => { pub fn g() {} } }
pub mod b;
";
const CALL: &str = "m!();\n";
const MAC: &str = "macro_rules! k { () => { pub fn r() {} } }\n";
const APP: &str = "#![no_core]\npub use dep::a::f;\npub use dep::b::g;\npub use dep::r;\n";

static BOTH: Texts = Texts(&[&[("/dep/lib.rs", DEP), ("/dep/mac.rs", MAC), ("/dep/a.rs", CALL), ("/dep/b.rs", CALL)], &[("/app/lib.rs", APP)]]);
static EDITED: Texts = Texts(&[&[("/dep/lib.rs", DEP_EDITED), ("/dep/mac.rs", MAC), ("/dep/a.rs", CALL), ("/dep/b.rs", CALL)]]);

fn inputs() -> [CrateInput<'static>; 2] {
    [input("dep", "/dep/lib.rs", &[]), input("app", "/app/lib.rs", &[("dep", 0)])]
}

/// The group of `groups` holding `module`.
fn holding<'g, 'a>(groups: &'g [NameGroup<'a>], module: ItemKey) -> Option<&'g NameGroup<'a>> {
    groups.iter().find(|g| g.members.binary_search(&module).is_ok())
}

#[test]
fn a_macro_rules_walk_out_of_a_group_reads_the_parents_record_or_demands_it() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let inputs = inputs();
    let whole = resolve_crates(&inputs, Sysroot::default(), &BOTH, &Grouped, bump).unwrap_or_else(|e| panic!("the whole solve: {e}"));
    let (dep, app) = (whole.krate(0), whole.krate(1));
    let path = |k: &ItemKey| dep.path_of(*k).map_or_else(|| std::format!("{k:?}"), String::from);
    let key = |p: &str| dep.tree.modules.iter().find(|m| m.path.as_str() == p).unwrap_or_else(|| panic!("no module `{p}`")).key;
    let (root, a, b, mac) = (dep.root, key("dep::a"), key("dep::b"), key("dep::mac"));
    let want: BTreeSet<ItemKey> = [root, a, b, mac].iter().map(|&m| holding(&dep.groups, m).expect("a whole-solve group").members[0]).collect();

    let lazy = names_on_demand(&inputs, Sysroot::default(), &BOTH, &[1], bump).unwrap_or_else(|e| panic!("names on demand: {e:?}"));
    let got = &lazy.demanded[0];
    let solved: BTreeSet<ItemKey> = got.groups.iter().map(|g| g.members[0]).collect();
    let paths: Vec<String> = got.groups.iter().flat_map(|g| g.members.iter().map(path)).collect();
    std::println!(
        "TEXTUAL ON DEMAND dep: {} of {} groups solved ({} solves asked, {} walks, deepest {}): {paths:?}; app solved {} times",
        solved.len(),
        dep.groups.len(),
        got.solves,
        lazy.walks[0],
        got.deepest,
        lazy.root_solves
    );
    assert_eq!(solved, want, "the groups solved on demand are not the root, `mac`, `a` and `b` ({paths:?})");
    // The root's walk into its `#[macro_use]` child `mac`, which has no record yet, demanded it
    // (a frame above the root's).
    assert!(got.deepest >= 2, "`mac` was not demanded by the root's textual walk (deepest {})", got.deepest);
    let by_first: BTreeMap<ItemKey, &NameGroup<'_>> = dep.groups.iter().map(|g| (g.members[0], g)).collect();
    for g in got.groups.iter() {
        let w = by_first[&g.members[0]];
        let p = path(&g.members[0]);
        assert!(g.members[..] == w.members[..], "{p}: other members than the whole solve's");
        assert!(g.reads[..] == w.reads[..] && g.cells_read[..] == w.cells_read[..], "{p}: other reads than the whole solve's");
        assert!(g.bytes[..] == w.bytes[..], "{p}: its record differs from the whole solve's group");
    }
    for m in [a, b] {
        let g = holding(&got.groups, m).expect("solved");
        assert!(g.reads.iter().any(|r| matches!(*r, GroupRead::Textual { module, .. } if module == root)), "{}: no textual read of the root", path(&m));
    }
    let app_lazy = lazy.roots[1].expect("app was solved");
    assert_eq!(app_lazy.groups.len(), app.groups.len(), "app's groups");
    for (x, y) in app_lazy.groups.iter().zip(app.groups.iter()) {
        assert!(x.members[..] == y.members[..] && x.bytes[..] == y.bytes[..], "app's group records differ from the whole solve's");
    }

    // The control: the edited text solved whole; the edit driver's entry rule (edit.rs) over
    // the on-demand records: a group is entered when a member's own text moved or a cell it
    // read is defined now with another digest (or no longer).
    let edited_in = [input("dep", "/dep/lib.rs", &[])];
    let edited = resolve_crates(&edited_in, Sysroot::default(), &EDITED, &Grouped, bump).unwrap_or_else(|e| panic!("the edited solve: {e}"));
    let after = edited.krate(0);
    let entered = |g: &NameGroup<'_>, keep: &dyn Fn(&(ItemKey, [u8; 16], ContentId)) -> bool| -> bool {
        let own_moved = g.members.iter().zip(g.own.iter()).any(|(&m, own)| {
            holding(&after.groups, m).and_then(|h| h.members.iter().position(|&x| x == m).map(|i| h.own[i])) != Some(*own)
        });
        // A cell no group defines (a call's expansion) moves only when the caller says so
        // (`Edit::moved`): not here.
        let read_moved = g.cells_read.iter().filter(|r| keep(r)).any(|&(t, cell, seen)| match holding(&after.groups, t) {
            Some(h) => h.defines.iter().find(|d| d.0 == cell).map(|d| d.1) != Some(seen),
            None => false,
        });
        own_moved || read_moved
    };
    let was = holding(&got.groups, a).expect("`a` solved");
    let now = holding(&after.groups, a).expect("`a` in the edited solve");
    assert!(was.bytes[..] != now.bytes[..], "the edit did not rewrite `a`'s record: the control shows nothing");
    assert!(entered(was, &|_| true), "`a`'s record is stale after the edit and its reads do not enter it");
    let planted = |r: &(ItemKey, [u8; 16], ContentId)| r.1 != textual_cell(r.0);
    assert!(!entered(was, &planted), "the control: with the textual read dropped `a` is still entered, so the read is not what catches the edit");
}
