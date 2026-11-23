//! An edit's names by group (cells/edit.rs, engine.md piece E4's incremental half): the driver
//! enters only the groups whose inputs moved, from the records alone, and its records are
//! byte-equal to a fresh whole-crate solve of the edited text.
//!
//! The solver is the real one-group solve (cells/edit.rs `OneGroup` over the crate solve with
//! its members named, `Par::names_members`): it reads the members' texts alone and answers
//! every other module from the records the driver holds now. The oracle is byte-equality with a
//! fresh whole solve of the edited text: a group the driver fails to enter keeps its old
//! record, and a scope the solver answers wrongly from a record writes other bytes. The floors
//! (rule 67): a private fn added enters 1 group; a `pub use` added 2 (its module's, the reader
//! of the new name); an item added in a glob cycle 1; an import that closes a cycle replaces
//! the 2 old groups by one; nothing changed enters none. The control: the driver with one moved
//! reader skipped is caught. A call whose textual scope walks into a module outside its group
//! is refused by type (`SolveWhole`), and the crate is then solved whole.

use std::string::String;
use std::vec::Vec;

use blaze_id::{ContentId, ItemKey};
use blaze_own::AVec;

use super::entry::{Texts, input};
use super::names_groups::{CUT_T, CYCLE_EDIT_T, CYCLE_T, Groups, PLAIN_FN_T, PLAIN_T, RECUT_T, RULES_ABOVE_T, RULES_BELOW_T, USE_ADDED_T, USE_T};
use crate::cells::{Edit, EditError, EditPlant, Edited, Members, OneGroup, OneGroupRefusal, Solved, re_solve_planted};
use crate::crate_scope::{CrateScopeError, resolve_members};
use crate::graph::Sysroot;
use crate::task::{Par, Serial};
use crate::{GroupRead, NameGroup};


/// The clock's marks of one edit, in order: each part lasts until the next mark (rule 16).
type Marks = core::cell::RefCell<Vec<(String, std::time::Instant)>>;

fn mark(marks: &Marks, part: String) {
    marks.borrow_mut().push((part, std::time::Instant::now()));
}

/// Each part's total time in microseconds, by first mark, as one line.
fn parts(marks: &Marks) -> String {
    let marks = marks.borrow();
    let mut sums: Vec<(String, f64)> = Vec::new();
    for w in marks.windows(2) {
        let us = w[1].1.duration_since(w[0].1).as_secs_f64() * 1e6;
        match sums.iter_mut().find(|(p, _)| *p == w[0].0) {
            Some(s) => s.1 += us,
            None => sums.push((w[0].0.clone(), us)),
        }
    }
    sums.iter().map(|(p, us)| std::format!("{p} {us:.0}")).collect::<Vec<_>>().join(", ")
}

/// The serial crate solve, its stretches marked on `marks`.
struct Only<'r> {
    marks: &'r Marks,
}

impl Par for Only<'_> {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn stage(&self, stage: crate::task::Stage) {
        mark(self.marks, std::format!("{stage:?}"));
    }

    fn names_part(&self, part: crate::task::NamesPart) {
        mark(self.marks, std::format!(".{part:?}"));
    }
}

type Refused = EditError<'static, OneGroupRefusal<CrateScopeError<'static>>>;

/// The driver over `before`'s records after the edit to `texts` (`after` is their fresh solve),
/// with the real one-group solve: what it wrote, the modules of each group entered (by path),
/// and the old groups replaced; or its typed refusal.
fn drive_or(before: &Groups, after: &Groups, texts: &'static Texts, moved: &[([u8; 16], ContentId)], plant: EditPlant) -> Result<(Edited<'static>, Vec<Vec<String>>), Refused> {
    drive_marked(before, after, texts, moved, plant, &Marks::default())
}

/// [`drive_or`] with its parts marked on `marks`: the driver's own work, and each crate
/// solve's stages and cell parts.
fn drive_marked(before: &Groups, after: &Groups, texts: &'static Texts, moved: &[([u8; 16], ContentId)], plant: EditPlant, marks: &Marks) -> Result<(Edited<'static>, Vec<Vec<String>>), Refused> {
    mark(marks, String::from("driver"));
    let alloc = blaze_own::testing::heap().alloc();
    let old: Vec<NameGroup<'static>> = before.groups.iter().map(|(_, g)| g.clone()).collect();
    // Every module now with its own text: what the outlines give the engine.
    let mut own: Vec<(ItemKey, ContentId)> = after.groups.iter().flat_map(|(_, g)| g.members.iter().copied().zip(g.own.iter().copied())).collect();
    own.sort();
    let edit = Edit { old: &old, own: &own, moved };
    let solve_crate = move |members: Members<'_>| -> Result<Solved<'static>, CrateScopeError<'static>> {
        mark(marks, String::from("graph"));
        let g = super::SyncFrame::leaked();
        let solved = resolve_members(&input("k0", "/k0/lib.rs", &[]), &[], Sysroot::default(), texts, 0, &Only { marks }, members, g)?;
        mark(marks, String::from("driver.re_solve"));
        Ok(solved)
    };
    mark(marks, String::from("driver.re_solve"));
    // The module tree record as the records were written: the solve reads its members' files.
    // A solve that walks again hands its new record on (`OneGroup::tree`).
    let edited = re_solve_planted(edit, &mut OneGroup { solve_crate, alloc, tree: Some(before.tree) }, plant, alloc)?;
    mark(marks, String::from("driver.paths"));
    let path = |k: &ItemKey| {
        before.groups.iter().chain(after.groups.iter()).find_map(|(paths, g)| g.members.iter().position(|m| m == k).map(|i| paths[i].clone())).unwrap_or_else(|| std::format!("{k:?}"))
    };
    let entered = edited.entered.iter().map(|ms| ms.iter().map(path).collect()).collect();
    mark(marks, String::from("end"));
    Ok((edited, entered))
}

fn drive(before: &Groups, after: &Groups, texts: &'static Texts, moved: &[([u8; 16], ContentId)], plant: EditPlant) -> (Edited<'static>, Vec<Vec<String>>) {
    drive_or(before, after, texts, moved, plant).unwrap_or_else(|e| panic!("edit: {e:?}"))
}

/// Where the driver's records differ from the fresh solve's (bytes, defines, reads), by the
/// fresh group's paths; empty when equal, in one order.
fn differ(edited: &Edited<'static>, fresh: &Groups) -> Vec<String> {
    let mut want: Vec<&(Vec<String>, NameGroup<'static>)> = fresh.groups.iter().collect();
    want.sort_by_key(|(_, g)| g.members[0]);
    let mut out = Vec::new();
    if edited.groups.len() != want.len() {
        out.push(std::format!("{} groups written, {} fresh", edited.groups.len(), want.len()));
    }
    for (a, (paths, b)) in edited.groups.iter().zip(want) {
        if a.members[..] != b.members[..] {
            out.push(std::format!("{paths:?}: other members"));
        } else if a.bytes[..] != b.bytes[..] || a.defines[..] != b.defines[..] || a.cells_read[..] != b.cells_read[..] {
            out.push(std::format!("{paths:?}: bytes {}, defines {}, reads {}", a.bytes[..] == b.bytes[..], a.defines[..] == b.defines[..], a.cells_read[..] == b.cells_read[..]));
        }
    }
    out
}

fn equal(edited:&Edited<'static>, fresh: &Groups) -> bool {
    differ(edited, fresh).is_empty()
}

fn case(name: &str, before: &'static Texts, after: &'static Texts) -> (Edited<'static>, Vec<Vec<String>>) {
    let (b, a) = (Groups::solve(before), Groups::solve(after));
    let (edited, entered) = drive(&b, &a, after, &[], EditPlant::None);
    std::println!("NAMES EDIT: {name}: entered {} group(s) of {} ({entered:?}), replaced {} old", entered.len(), a.groups.len(), edited.replaced);
    let wrong = differ(&edited, &a);
    assert!(wrong.is_empty(), "{name}: the driver's records are not a fresh solve's: {wrong:?}; before {:?}, after {:?}", b.groups.iter().map(|(p, _)| p).collect::<Vec<_>>(), a.groups.iter().map(|(p, _)| p).collect::<Vec<_>>());
    (edited, entered)
}

fn one(path: &str) -> Vec<String> {
    std::vec![String::from(path)]
}

#[test]
fn names_groups_edit_nothing_changed_enters_nothing() {
    let (edited, entered) = case("nothing changed", &PLAIN_T, &PLAIN_T);
    assert!(entered.is_empty() && edited.replaced == 0, "nothing changed entered {entered:?}");
}

#[test]
fn names_groups_edit_a_private_fn_added_enters_one_group() {
    let (_, entered) = case("a private fn added", &PLAIN_T, &PLAIN_FN_T);
    assert_eq!(entered, std::vec![one("k0::a")], "floor 1: `a`'s group alone");
}

// PLAIN with a new module `e` that asks `c` (which globs `a`) for a name `c` was never asked.
const PLAIN_READER: &str = "#![no_core]
pub mod a {
    pub struct S;
}
pub mod b {
    pub use crate::a::S as T;
}
pub mod c {
    pub use crate::a::*;
}
pub mod d {
    pub use crate::c::S as U;
}
pub mod e {
    pub use crate::c::S as W;
}
";
static PLAIN_READER_T: Texts = Texts(&[&[("/k0/lib.rs", PLAIN_READER)]]);

/// CG's defect: a group's record (its `defines` too) is a function of the group alone, never of
/// which readers asked it what: a new reader of `c`'s glob enters its own group alone, and the
/// records kept are a fresh solve's, `defines` compared whole.
#[test]
fn names_groups_edit_a_new_reader_leaves_the_read_group() {
    let (_, entered) = case("a new reader of a glob", &PLAIN_T, &PLAIN_READER_T);
    // The root's text holds the new `mod` line: its group is entered too (floor 2).
    assert!(entered.contains(&one("k0::e")) && entered.len() == 2, "floor 2: the new module's group and the root's: {entered:?}");
    assert!(!entered.iter().flatten().any(|p| p == "k0::c" || p == "k0::a"), "a module read was entered: {entered:?}");
}

#[test]
fn names_groups_edit_a_pub_use_added_enters_its_reader() {
    let (_, mut entered) = case("a pub use added", &USE_T, &USE_ADDED_T);
    entered.sort();
    assert_eq!(entered, std::vec![one("k0::a"), one("k0::r")], "floor 2: `a`'s and its reader `r`'s");
}

// USE_ADDED with `b`'s text other than its record: `b` no longer declares `T`, which `a`'s new
// `pub use` and `r`'s globs read.
const USE_ADDED_B_LIES: &str = "#![no_core]
pub mod a {
    pub struct S;
    pub use crate::b::T;
}
pub mod b {
    pub struct Other;
}
pub mod r {
    use crate::a::*;
    use crate::b::*;
    pub use self::T as U;
}
pub mod s {
    pub use crate::a::S as V;
}
";
static USE_ADDED_B_LIES_T: Texts = Texts(&[&[("/k0/lib.rs", USE_ADDED_B_LIES)]]);

/// The cause (rule 23): the one-group solve reads its members' texts alone. Handed a text in
/// which `b`, never entered, lost the `T` its record holds, it still writes the fresh solve's
/// records (`b` is answered from its record); the control, the whole solve of that text, writes
/// others.
#[test]
fn names_groups_edit_modules_outside_the_group_come_from_records() {
    let (b, a) = (Groups::solve(&USE_T), Groups::solve(&USE_ADDED_T));
    let (edited, mut entered) = drive(&b, &a, &USE_ADDED_B_LIES_T, &[], EditPlant::None);
    entered.sort();
    assert_eq!(entered, std::vec![one("k0::a"), one("k0::r")], "the groups entered");
    let wrong = differ(&edited, &a);
    assert!(wrong.is_empty(), "a module outside the group was read from its text: {wrong:?}");
    let lying = Groups::solve(&USE_ADDED_B_LIES_T);
    assert!(!differ(&edited, &lying).is_empty(), "the control: `b`'s text changes no record");
}

#[test]
fn names_groups_edit_an_item_added_in_a_glob_cycle_enters_the_cycle() {
    let (_, entered) = case("an item added in a glob cycle", &CYCLE_T, &CYCLE_EDIT_T);
    assert_eq!(entered, std::vec![std::vec![String::from("k0::a"), String::from("k0::b")]], "floor 1: the cycle's group");
}

#[test]
fn names_groups_edit_an_import_that_closes_a_cycle_replaces_both_groups() {
    let (edited, entered) = case("an import that closes a cycle", &CUT_T, &RECUT_T);
    assert_eq!(edited.replaced, 2, "the re-cut replaces the two old groups");
    assert_eq!(entered.len(), 1, "one solve answers the merged group: {entered:?}");
}

#[test]
fn names_groups_edit_control_a_skipped_reader_is_caught() {
    let (b, a) = (Groups::solve(&USE_T), Groups::solve(&USE_ADDED_T));
    let (edited, entered) = drive(&b, &a, &USE_ADDED_T, &[], EditPlant::SkipOneReader);
    assert_eq!(entered, std::vec![one("k0::a")], "the plant did not skip `r`");
    assert!(!equal(&edited, &a), "the oracle did not catch the reader left out");
}

/// A fresh solve's records, by module: what the differential test answers outside modules by.
struct FreshRecords<'g>(&'g Groups);

impl crate::cells::GroupRecords for FreshRecords<'_> {
    fn group_of(&self, module: ItemKey) -> Option<crate::cells::GroupRef<'_>> {
        self.0.groups.iter().find(|(_, g)| g.members.contains(&module)).map(|(_, g)| crate::cells::GroupRef::of(g))
    }
}

/// The one-group solve of `paths` in `texts` with `records` outside, from `tree` (or walking).
fn solve_alone(texts: &'static Texts, records: &Groups, paths: &[&str], tree: Option<&[u8]>) -> Vec<NameGroup<'static>> {
    let mut members: Vec<ItemKey> = records.groups.iter().flat_map(|(ps, g)| ps.iter().zip(g.members.iter()).filter(|(p, _)| paths.contains(&p.as_str())).map(|(_, k)| *k)).collect();
    members.sort();
    let g = super::SyncFrame::leaked();
    let members = Members { members: &members, records: &FreshRecords(records), tree };
    resolve_members(&input("k0", "/k0/lib.rs", &[]), &[], Sysroot::default(), texts, 0, &Serial, members, g).unwrap_or_else(|e| panic!("solve alone: {e:?}")).groups.iter().cloned().collect()
}

/// Where two answers of one solve differ: groups, members, then each list's first unequal row.
fn first_differences(walk: &[NameGroup<'static>], record: &[NameGroup<'static>]) -> Vec<String> {
    let mut out = Vec::new();
    if walk.len() != record.len() {
        out.push(std::format!("{} groups walking, {} from the record: walking {:?}, record {:?}", walk.len(), record.len(), walk.iter().map(|g| &g.members[..]).collect::<Vec<_>>(), record.iter().map(|g| &g.members[..]).collect::<Vec<_>>()));
    }
    for (w, r) in walk.iter().zip(record) {
        if w.members[..] != r.members[..] {
            out.push(std::format!("members {:?} / {:?}", &w.members[..], &r.members[..]));
        }
        if w.own[..] != r.own[..] {
            out.push(std::format!("own {:?} / {:?}", &w.own[..], &r.own[..]));
        }
        let only = |a: &[GroupRead], b: &[GroupRead]| a.iter().filter(|x| !b.contains(x)).copied().collect::<Vec<_>>();
        if w.reads[..] != r.reads[..] {
            out.push(std::format!("reads walking only {:?}; record only {:?}", only(&w.reads, &r.reads), only(&r.reads, &w.reads)));
        }
        if w.defines[..] != r.defines[..] {
            let a: Vec<_> = w.defines.iter().filter(|x| !r.defines.contains(x)).collect();
            let b: Vec<_> = r.defines.iter().filter(|x| !w.defines.contains(x)).collect();
            out.push(std::format!("defines walking only {a:?}; record only {b:?}"));
        }
        if w.cells_read[..] != r.cells_read[..] {
            let a: Vec<_> = w.cells_read.iter().filter(|x| !r.cells_read.contains(x)).collect();
            let b: Vec<_> = r.cells_read.iter().filter(|x| !w.cells_read.contains(x)).collect();
            out.push(std::format!("cells read walking only {a:?}; record only {b:?}"));
        }
        if w.bytes[..] != r.bytes[..] {
            let at = w.bytes.iter().zip(r.bytes.iter()).position(|(x, y)| x != y).unwrap_or(w.bytes.len().min(r.bytes.len()));
            out.push(std::format!("bytes differ at {at} of {} / {}", w.bytes.len(), r.bytes.len()));
        }
    }
    out
}

/// The differential (CM's step B): one group solved alone walking every file and from the module
/// tree record, with the same records outside, writes the same group.
#[test]
fn names_groups_edit_the_tree_record_answers_as_the_walk() {
    let (b, a) = (Groups::solve(&USE_T), Groups::solve(&USE_ADDED_T));
    std::println!("TREE DIFF: tree records of USE and USE_ADDED equal: {}", b.tree == a.tree);
    let mut wrong = Vec::new();
    for (name, texts, records, paths, tree) in [
        ("r of USE_ADDED, its tree", &USE_ADDED_T, &a, &["k0::r"][..], a.tree),
        ("r of USE_ADDED, USE's tree", &USE_ADDED_T, &a, &["k0::r"][..], b.tree),
        ("a of USE_ADDED, USE's tree", &USE_ADDED_T, &b, &["k0::a"][..], b.tree),
    ] {
        let walk = solve_alone(texts, records, paths, None);
        let record = solve_alone(texts, records, paths, Some(tree));
        let d = first_differences(&walk, &record);
        std::println!("TREE DIFF: {name}: {d:?}");
        if !d.is_empty() {
            wrong.push(std::format!("{name}: {d:?}"));
        }
    }
    let (cb, ca) = (Groups::solve(&CUT_T), Groups::solve(&RECUT_T));
    let walk = solve_alone(&RECUT_T, &cb, &["k0::a"], None);
    let record = solve_alone(&RECUT_T, &cb, &["k0::a"], Some(cb.tree));
    let d = first_differences(&walk, &record);
    std::println!("TREE DIFF: a of RECUT, CUT's tree: {d:?}; fresh groups {}", ca.groups.len());
    if !d.is_empty() {
        wrong.push(std::format!("a of RECUT: {d:?}"));
    }
    assert!(wrong.is_empty(), "the tree record answers other than the walk: {wrong:#?}");
}

/// The engine's expansion cells that moved between two solves: each call's made text, as the
/// expansion records hold it (no names record says a `macro_rules!` body moved: a definition
/// is named by its place, and its module's `ModuleDecls` holds no body).
fn expansions_moved(before: &Groups, after: &Groups) -> Vec<([u8; 16], ContentId)> {
    let rows = |g: &Groups| -> Vec<(ItemKey, [u8; 16], ContentId)> {
        g.groups.iter().flat_map(|(_, g)| g.reads.iter().filter(|r| matches!(r, GroupRead::Expansion { .. })).map(|r| r.engine().unwrap_or_else(|m| panic!("speculated {m:?}")))).collect()
    };
    let old = rows(before);
    let mut moved: Vec<([u8; 16], ContentId)> = rows(after).into_iter().filter(|n| !old.contains(n)).map(|(_, c, d)| (c, d)).collect();
    moved.sort();
    moved
}

/// `m`'s call finds `mac` by the textual scope, walking out into the root's text, which is not
/// `m`'s group's: the walk reads the root's sequence before `m`'s `mod` line from the root's
/// record (its `textual` and `marks`, the digest its `defines` hold), so `m`'s group is solved
/// alone, byte-equal to a fresh solve.
#[test]
fn names_groups_edit_two_macros_swapped_enter_the_caller_by_its_expansion() {
    let (b, a) = (Groups::solve(&RULES_BELOW_T), Groups::solve(&RULES_ABOVE_T));
    let moved = expansions_moved(&b, &a);
    assert!(!moved.is_empty(), "the swap moved no expansion");
    let (edited, entered) = drive_or(&b, &a, &RULES_ABOVE_T, &moved, EditPlant::None).unwrap_or_else(|e| panic!("two macros swapped: {e:?}"));
    std::println!("NAMES EDIT: two macros swapped across a mod line: entered {entered:?}");
    assert!(entered.contains(&one("k0::m")), "the caller's group was not entered: {entered:?}");
    let wrong = differ(&edited, &a);
    assert!(wrong.is_empty(), "the caller's walk out of its group was not the whole solve's: {wrong:?}");
    // The control: without the expansion cells the caller keeps its old record, and is caught.
    let (stale, _) = drive(&b, &a, &RULES_ABOVE_T, &[], EditPlant::None);
    assert!(!equal(&stale, &a), "the oracle did not catch the expansion left out");
}

// A module whose fn body holds an `impl` and a local item (a block's module), outside the
// group of the edit (`a`'s, or `c`'s).
const BLOCK: &str = "#![no_core]
pub mod a {
    pub struct S;
}
pub mod b {
    pub struct T;
    pub fn f() {
        struct L;
        impl L {}
        fn local() {}
    }
}
pub mod c {
    pub use crate::b::T as U;
}
";
const BLOCK_FN: &str = "#![no_core]
pub mod a {
    pub struct S;
    fn added() {}
}
pub mod b {
    pub struct T;
    pub fn f() {
        struct L;
        impl L {}
        fn local() {}
    }
}
pub mod c {
    pub use crate::b::T as U;
}
";
static BLOCK_T: Texts = Texts(&[&[("/k0/lib.rs", BLOCK)]]);
static BLOCK_FN_T: Texts = Texts(&[&[("/k0/lib.rs", BLOCK_FN)]]);

/// E4's shape 1: a block's module outside the group is answered from its record (it was
/// refused as `Made`): the private fn added to `a` enters `a`'s group alone, byte-equal.
#[test]
fn names_groups_edit_a_block_outside_the_group_comes_from_its_record() {
    let (_, entered) = case("a block's module outside the group", &BLOCK_T, &BLOCK_FN_T);
    assert_eq!(entered, std::vec![one("k0::a")], "floor 1: `a`'s group alone");
}

// A module made by an expansion (`mk!` in `b` writes `b::inner`), read by `c` through `b`.
const MADE: &str = "#![no_core]
macro_rules! mk { () => { pub mod inner { pub struct I; } } }
pub mod a {
    pub struct S;
}
pub mod b {
    mk!();
}
pub mod c {
    pub use crate::b::inner::I;
}
";
const MADE_A: &str = "#![no_core]
macro_rules! mk { () => { pub mod inner { pub struct I; } } }
pub mod a {
    pub struct S;
    fn added() {}
}
pub mod b {
    mk!();
}
pub mod c {
    pub use crate::b::inner::I;
}
";
const MADE_C: &str = "#![no_core]
macro_rules! mk { () => { pub mod inner { pub struct I; } } }
pub mod a {
    pub struct S;
}
pub mod b {
    mk!();
}
pub mod c {
    pub use crate::b::inner::I;
    fn added() {}
}
";
static MADE_T: Texts = Texts(&[&[("/k0/lib.rs", MADE)]]);
static MADE_A_T: Texts = Texts(&[&[("/k0/lib.rs", MADE_A)]]);
static MADE_C_T: Texts = Texts(&[&[("/k0/lib.rs", MADE_C)]]);

/// E4's shape 1: a module an expansion made, outside the group, is answered from its record:
/// an edit to `a` (which reads nothing of it) and one to `c` (which reads `b::inner` by path)
/// each enter the edited module's group alone, byte-equal.
#[test]
fn names_groups_edit_a_made_module_outside_the_group_comes_from_its_record() {
    let (_, entered) = case("an edit beside a made module", &MADE_T, &MADE_A_T);
    assert_eq!(entered, std::vec![one("k0::a")], "floor 1: `a`'s group alone");
    let (_, entered) = case("an edit to a reader of a made module", &MADE_T, &MADE_C_T);
    assert_eq!(entered, std::vec![one("k0::c")], "floor 1: `c`'s group alone");
}

// A `#[macro_use]` child of the root defines `mac`; `m` calls it, walking out into the root
// and into that child, neither in `m`'s group.
const MACRO_USE: &str = "#![no_core]
#[macro_use]
mod macros {
    macro_rules! mac { () => { pub struct One; } }
    macro_rules! other { () => {} }
}
pub mod a {
    pub struct S;
}
pub mod m {
    mac!();
}
";
const MACRO_USE_M: &str = "#![no_core]
#[macro_use]
mod macros {
    macro_rules! mac { () => { pub struct One; } }
    macro_rules! other { () => {} }
}
pub mod a {
    pub struct S;
}
pub mod m {
    mac!();
    fn added() {}
}
";
const MACRO_USE_ROOT: &str = "#![no_core]
#[macro_use]
mod macros {
    macro_rules! mac { () => { pub struct One; } }
    macro_rules! other { () => {} }
}
pub mod a {
    pub struct S;
}
pub mod m {
    mac!();
}
fn added() {}
";
static MACRO_USE_T: Texts = Texts(&[&[("/k0/lib.rs", MACRO_USE)]]);
static MACRO_USE_M_T: Texts = Texts(&[&[("/k0/lib.rs", MACRO_USE_M)]]);
static MACRO_USE_ROOT_T: Texts = Texts(&[&[("/k0/lib.rs", MACRO_USE_ROOT)]]);

/// E4's shape 2: a call's textual walk out of its group (into the root) and into a
/// `#[macro_use]` child outside it is answered from their records: the edit to `m` enters
/// `m`'s group alone; the edit to the root enters the root's alone, its textual sequence
/// holding the child's from the child's record. Both byte-equal to a fresh solve.
#[test]
fn names_groups_edit_a_macro_use_child_outside_the_group_comes_from_its_record() {
    let (_, entered) = case("a call through a #[macro_use] child outside", &MACRO_USE_T, &MACRO_USE_M_T);
    assert_eq!(entered, std::vec![one("k0::m")], "floor 1: `m`'s group alone");
    let (_, entered) = case("the root beside a #[macro_use] child outside", &MACRO_USE_T, &MACRO_USE_ROOT_T);
    assert_eq!(entered, std::vec![one("k0")], "floor 1: the root's group alone");
}

// A `#[macro_use]` child of the root with no macro yet, and `m`, which calls none.
const FIRST_MACRO: &str = "#![no_core]
#[macro_use]
mod macros {
    pub struct Z;
}
pub mod a {
    pub struct S;
}
pub mod m {
    pub struct Q;
}
";
// In one edit: `macros` gets its FIRST `macro_rules!` (its tree row's macro count moves: its
// solve walks the tree again) and `m`, another group, calls it.
const FIRST_MACRO_CALLED: &str = "#![no_core]
#[macro_use]
mod macros {
    pub struct Z;
    macro_rules! mac { () => { pub struct One; } }
}
pub mod a {
    pub struct S;
}
pub mod m {
    pub struct Q;
    mac!();
}
";
static FIRST_MACRO_T: Texts = Texts(&[&[("/k0/lib.rs", FIRST_MACRO)]]);
static FIRST_MACRO_CALLED_T: Texts = Texts(&[&[("/k0/lib.rs", FIRST_MACRO_CALLED)]]);

/// The stale record within one edit (plan row B2's step 2): the driver hands every solve the
/// record the LAST solve ended with (`OneGroup::tree`). `macros`' solve finds its macro count
/// moved and walks the tree again; `m`'s solve, in the same edit, must lower `macros` for the
/// crate's macro table, which only the new record says it defines. Handed the record of before
/// (macro count 0), `m`'s call finds no definition and its record is not a fresh solve's.
#[test]
fn names_groups_edit_a_first_macro_and_its_call_in_one_edit() {
    let (b, a) = (Groups::solve(&FIRST_MACRO_T), Groups::solve(&FIRST_MACRO_CALLED_T));
    assert!(b.tree != a.tree, "the fixture: the first macro moved no tree record row");
    let moved = expansions_moved(&b, &a);
    let marks = Marks::default();
    let (edited, entered) = drive_marked(&b, &a, &FIRST_MACRO_CALLED_T, &moved, EditPlant::None, &marks).unwrap_or_else(|e| panic!("a first macro and its call: {e:?}"));
    let again = marks.borrow().iter().filter(|(p, _)| p == ".TreeAgain").count();
    std::println!("NAMES EDIT: a first macro and its call in one edit: entered {entered:?}; tree walked again {again} time(s)");
    assert!(entered.contains(&one("k0::m")), "the caller's group was not entered: {entered:?}");
    let wrong = differ(&edited, &a);
    assert!(wrong.is_empty(), "the call did not resolve as a fresh solve's: {wrong:?}");
    // One walk again: the first solve that saw the moved row; every later solve took its record.
    assert_eq!(again, 1, "the tree was walked again {again} times: the new record was not handed on");
}

/// E4's cost (rule 67, the floor is one group's own work): a generated crate of 2,000 modules
/// (a few items each; a chain of re-exports through 50 of them, the rest independent), solved
/// whole, then a private fn added to one independent module answered by the one-group solve.
/// Prints both times, the groups entered and the ratio. Ignored by default (a measurement):
/// `-p blaze-scope --lib -- --ignored names_groups_edit_cost_of_one_module_in_two_thousand --nocapture`.
#[test]
#[ignore = "a measurement: run by name"]
fn names_groups_edit_cost_of_one_module_in_two_thousand() {
    const MODULES: usize = 2000;
    const CHAIN: usize = 50;
    // Each module in its own file, as real crates are: `mod x;` lines in the root.
    let leak = |s: String| -> &'static str { std::boxed::Box::leak(s.into_boxed_str()) };
    let text = |added: bool| -> &'static Texts {
        let mut root = String::from("#![no_core]\n");
        let mut files: Vec<(&'static str, &'static str)> = Vec::with_capacity(MODULES + 1);
        for i in 0..MODULES {
            let (name, body) = if i == 0 {
                (String::from("m0"), String::from("pub struct S;\npub struct S0;\npub fn f0() {}\n"))
            } else if i < CHAIN {
                (std::format!("m{i}"), std::format!("pub use crate::m{}::S;\npub struct S{i};\npub fn f{i}() {{}}\n", i - 1))
            } else {
                let extra = if added && i == MODULES - 1 { "fn added() {}\n" } else { "" };
                (std::format!("o{i}"), std::format!("pub struct X;\npub enum E {{ A, B }}\npub fn g() {{}}\npub use crate::m0::S0 as Y;\n{extra}"))
            };
            root.push_str(&std::format!("pub mod {name};\n"));
            files.push((leak(std::format!("/k0/{name}.rs")), leak(body)));
        }
        files.insert(0, ("/k0/lib.rs", leak(root)));
        let file: &'static [(&'static str, &'static str)] = std::boxed::Box::leak(files.into_boxed_slice());
        let crates: &'static [&'static [(&'static str, &'static str)]] = std::boxed::Box::leak(std::vec![file].into_boxed_slice());
        std::boxed::Box::leak(std::boxed::Box::new(Texts(crates)))
    };
    let (before_t, after_t) = (text(false), text(true));
    let b = Groups::solve(before_t);
    let start = std::time::Instant::now();
    let a = Groups::solve(after_t);
    let whole = start.elapsed().as_secs_f64();
    let marks = Marks::default();
    let start = std::time::Instant::now();
    let (edited, entered) = drive_marked(&b, &a, after_t, &[], EditPlant::None, &marks).unwrap_or_else(|e| panic!("edit: {e:?}"));
    let one = start.elapsed().as_secs_f64();
    std::println!("NAMES EDIT COST PARTS (us): {}", parts(&marks));
    // The edits whose one-group solve found the tree record's inputs moved and walked again.
    let again = usize::from(marks.borrow().iter().any(|(p, _)| p == ".TreeAgain"));
    std::println!(
        "NAMES EDIT COST: {MODULES} modules, {} groups: whole solve {whole:.3} s; one-module edit {one:.3} s, entered {} group(s) ({} module(s)); ratio whole/edit {:.1}; tree walked again: {again} of 1 edits",
        a.groups.len(),
        entered.len(),
        entered.iter().map(|g| g.len()).sum::<usize>(),
        whole / one.max(1e-9)
    );
    let wrong = differ(&edited, &a);
    assert!(wrong.is_empty(), "the edit's records are not a fresh solve's: {wrong:?}");
    assert_eq!(entered.len(), 1, "floor 1: the edited module's group alone: {entered:?}");
}

// `m` globs `b`, which globs `a`: both outside `m`'s group, each filled from its record on
// first read, the second step from the first's glob edge.
const GLOB_CHAIN: &str = "#![no_core]
pub mod a {
    pub struct S;
}
pub mod b {
    pub use crate::a::*;
    pub struct T;
}
pub mod m {
    use crate::b::*;
    pub use self::S as U;
    pub use self::T as V;
}
";
const GLOB_CHAIN_M: &str = "#![no_core]
pub mod a {
    pub struct S;
}
pub mod b {
    pub use crate::a::*;
    pub struct T;
}
pub mod m {
    use crate::b::*;
    pub use self::S as U;
    pub use self::T as V;
    fn added() {}
}
";
static GLOB_CHAIN_T: Texts = Texts(&[&[("/k0/lib.rs", GLOB_CHAIN)]]);
static GLOB_CHAIN_M_T: Texts = Texts(&[&[("/k0/lib.rs", GLOB_CHAIN_M)]]);

/// CK's missing fixture: a member's glob reaches through an outside module's glob into another
/// outside module (two steps, both filled lazily from their records): `m`'s group alone,
/// byte-equal.
#[test]
fn names_groups_edit_a_two_step_glob_chain_outside_comes_from_records() {
    let (_, entered) = case("a two-step glob chain outside the group", &GLOB_CHAIN_T, &GLOB_CHAIN_M_T);
    assert_eq!(entered, std::vec![one("k0::m")], "floor 1: `m`'s group alone");
}

/// A crate of `CHAIN` modules each re-exporting the one before (`m0` defines `S`) and `REST`
/// modules that read nothing: `head` is `m0`'s item.
fn chain_text(head: &str) -> &'static Texts {
    const CHAIN: usize = 100;
    const REST: usize = 100;
    let mut t = String::from("#![no_core]\n");
    t.push_str(&std::format!("pub mod m0 {{ {head} }}\n"));
    for i in 1..CHAIN {
        t.push_str(&std::format!("pub mod m{i} {{ pub use crate::m{}::S; }}\n", i - 1));
    }
    for i in 0..REST {
        t.push_str(&std::format!("pub mod o{i} {{ pub struct X; }}\n"));
    }
    let text: &'static str = std::boxed::Box::leak(t.into_boxed_str());
    let file: &'static [(&'static str, &'static str)] = std::boxed::Box::leak(std::vec![("/k0/lib.rs", text)].into_boxed_slice());
    let crates: &'static [&'static [(&'static str, &'static str)]] = std::boxed::Box::leak(std::vec![file].into_boxed_slice());
    std::boxed::Box::leak(std::boxed::Box::new(Texts(crates)))
}

#[test]
fn names_groups_edit_the_head_of_a_chain_enters_the_chain_alone() {
    let after = chain_text("pub enum S {}");
    let (b, a) = (Groups::solve(chain_text("pub struct S;")), Groups::solve(after));
    let (edited, entered) = drive(&b, &a, after, &[], EditPlant::None);
    std::println!("NAMES EDIT: the head of a 100-module re-export chain in 201 modules: entered {} group(s) of {}", entered.len(), a.groups.len());
    let wrong = differ(&edited, &a);
    assert!(wrong.is_empty(), "the driver's records are not a fresh solve's: {wrong:?}");
    assert!(entered.iter().all(|ms| ms.iter().all(|m| m.starts_with("k0::m"))), "a module off the chain was entered: {entered:?}");
    assert_eq!(entered.len(), 100, "floor: the 100 modules of the chain, each once");
}
