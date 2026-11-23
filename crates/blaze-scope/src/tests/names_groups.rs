//! Names per group (cells/groups.rs, engine.md piece E4): a crate's cold solve writes one record
//! per strongly connected component of its modules' reads.
//!
//! The oracle (rule 21, the key covers the record), from the records alone: over every pair of
//! fixtures, a group of the second solve whose members, members' own texts (each one's
//! `ModuleDecls` digest, a child `mod` by its line) and reads outside it (each cell with what it
//! held) are a group's of the first has that group's bytes. The floors (rule 67): a
//! private fn added rewrites ONE record, its module's group; a `pub use` added rewrites its
//! module's group and the group that read the name, not one that read another name of it; a glob
//! cycle of two modules is one group and its edit rewrites one record; an import that closes a
//! cycle re-cuts two groups into one. The control: the oracle over reads with the expansion rows
//! left out is caught on two `macro_rules!` swapped across a `mod` line.

use std::vec::Vec;

use blaze_id::ItemKey;

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::task::{Par, Serial};

/// The serial solve, with the groups asked for.
pub struct Grouped;

impl Par for Grouped {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut blaze_own::AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn names_groups(&self) -> bool {
        true
    }
}
use crate::{GroupRead, NameGroup};

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// One crate's groups, solved from `texts`: each group's members (by path) and record.
pub(super) struct Groups {
    pub(super) groups: Vec<(Vec<std::string::String>, NameGroup<'static>)>,
    /// The crate's module tree record, as the solve wrote it.
    pub(super) tree: &'static [u8],
}

impl Groups {
    pub(super) fn solve(texts: &'static Texts) -> Groups {
        let g = super::SyncFrame::leaked();
        let inputs = [input("k0", "/k0/lib.rs", &[])];
        let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Grouped, &Ready::serial(WAIT), g).unwrap_or_else(|e| panic!("solve: {e:?}"));
        let names = solved[0].names;
        assert!(names.notes.is_empty(), "notes {:?}", names.notes);
        let path = |k: &ItemKey| names.path_of(*k).map_or_else(|| std::format!("{k:?}"), std::string::ToString::to_string);
        let mut groups: Vec<(Vec<std::string::String>, NameGroup<'static>)> = names.groups.iter().map(|grp| (grp.members.iter().map(path).collect(), grp.clone())).collect();
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        // The engine's forms: sorted, each cell once.
        for (m, grp) in groups.iter() {
            assert!(grp.defines.windows(2).all(|w| w[0].0 < w[1].0), "{m:?}: defines not sorted by cell, each once");
            assert!(grp.cells_read.windows(2).all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)), "{m:?}: reads not sorted by cell, each once");
            assert_eq!(grp.own.len(), grp.members.len(), "{m:?}: an own text per member");
        }
        Groups { groups, tree: names.tree_record }
    }

    /// The members of each group, by path.
    fn cuts(&self) -> Vec<Vec<std::string::String>> {
        self.groups.iter().map(|(m, _)| m.clone()).collect()
    }

    fn group(&self, member: &str) -> &NameGroup<'static> {
        self.groups.iter().find(|(m, _)| m.iter().any(|p| p == member)).map(|(_, g)| g).unwrap_or_else(|| panic!("no group holds `{member}`: {:?}", self.cuts()))
    }

    /// The groups whose record is not one of `before`'s, by members.
    fn rewritten(&self, before: &Groups) -> Vec<Vec<std::string::String>> {
        self.groups.iter().filter(|(_, g)| !before.groups.iter().any(|(_, b)| b.bytes == g.bytes)).map(|(m, _)| m.clone()).collect()
    }
}

/// The oracle: a group of `after` with a group of `before`'s members and reads has its bytes.
/// `keep` filters the reads compared (the control drops the textual ones). The groups it held
/// valid, or each one it held valid whose bytes differ.
fn oracle(before: &Groups, after: &Groups, keep: &dyn Fn(&GroupRead) -> bool) -> Result<usize, Vec<std::string::String>> {
    let mut wrong = Vec::new();
    let mut valid = 0;
    for (members, now) in after.groups.iter() {
        let Some((_, was)) = before.groups.iter().find(|(m, _)| m == members) else { continue };
        // From the records alone: the members' own texts and the reads.
        let same = was.own == now.own && was.members == now.members && was.reads.iter().filter(|r| keep(r)).eq(now.reads.iter().filter(|r| keep(r)));
        if same {
            valid += 1;
            if was.bytes != now.bytes {
                wrong.push(std::format!("{members:?}: valid by its reads, and its bytes differ"));
            }
        }
    }
    if wrong.is_empty() { Ok(valid) } else { Err(wrong) }
}

fn holds(before: &Groups, after: &Groups) {
    let both = [oracle(before, after, &|_| true), oracle(after, before, &|_| true)];
    for got in both {
        if let Err(wrong) = got {
            panic!("the reads do not cover the record:\n{}", wrong.join("\n"));
        }
    }
}

// A private fn added to `a`; `b` names `a`'s `S`, `c` globs `a`.
const PLAIN: &str = "#![no_core]
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
";
const PLAIN_FN: &str = "#![no_core]
pub mod a {
    pub struct S;
    fn added() {}
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
";
pub(super) static PLAIN_T: Texts = Texts(&[&[("/k0/lib.rs", PLAIN)]]);
pub(super) static PLAIN_FN_T: Texts = Texts(&[&[("/k0/lib.rs", PLAIN_FN)]]);

#[test]
fn a_private_fn_added_rewrites_its_modules_group_alone() {
    let (before, again, after) = (Groups::solve(&PLAIN_T), Groups::solve(&PLAIN_T), Groups::solve(&PLAIN_FN_T));
    assert_eq!(before.cuts(), again.cuts(), "two solves of one text cut two ways");
    assert!(again.rewritten(&before).is_empty(), "two solves of one text wrote other bytes (rule 25)");
    assert!(before.groups.iter().all(|(m, _)| m.len() == 1), "a crate with no cycle has a group of two: {:?}", before.cuts());
    let rewritten = after.rewritten(&before);
    std::println!("NAMES GROUPS: a private fn added rewrote {} of {} group record(s) (floor 1): {rewritten:?}", rewritten.len(), after.groups.len());
    assert_eq!(rewritten, std::vec![std::vec![std::string::String::from("k0::a")]], "the records rewritten are not `a`'s group alone");
    holds(&before, &after);
}

/// A record gives each member's finished scope back (what a one-group solve answers a module
/// outside the group by), and refuses a module that is not a member by name.
#[test]
fn names_groups_record_gives_each_members_scope() {
    for texts in [&PLAIN_T, &CYCLE_T] {
        let solved = Groups::solve(texts);
        for (paths, g) in solved.groups.iter() {
            for &m in g.members.iter() {
                let bytes = g.scope_of(m).unwrap_or_else(|e| panic!("{paths:?}: {e:?}"));
                let mut buf = std::vec![0u8; bytes.len() + 16];
                let pad = buf.as_ptr().align_offset(16);
                buf[pad..pad + bytes.len()].copy_from_slice(bytes);
                let aligned = &buf[pad..pad + bytes.len()];
                let view = blaze_surface::ScopeView::read(aligned).unwrap_or_else(|e| panic!("{paths:?}: the scope does not read: {e:?}"));
                assert_eq!(view.module(), m, "{paths:?}: another member's scope");
            }
            let other = solved.groups.iter().map(|(_, o)| o.members[0]).find(|k| !g.members.contains(k));
            if let Some(other) = other {
                assert_eq!(g.scope_of(other), Err(crate::GroupRecordError::NotMember { module: other }), "{paths:?}: a non-member's scope");
            }
        }
    }
}

// A `pub use` added to `a` of a name `r` reads through two globs (absent in `a` before);
// `s` reads `a`'s `S` alone.
const USE: &str = "#![no_core]
pub mod a {
    pub struct S;
}
pub mod b {
    pub struct T;
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
const USE_ADDED: &str = "#![no_core]
pub mod a {
    pub struct S;
    pub use crate::b::T;
}
pub mod b {
    pub struct T;
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
pub(super) static USE_T: Texts = Texts(&[&[("/k0/lib.rs", USE)]]);
pub(super) static USE_ADDED_T: Texts = Texts(&[&[("/k0/lib.rs", USE_ADDED)]]);

#[test]
fn a_pub_use_added_rewrites_its_readers_groups_alone() {
    let (before, after) = (Groups::solve(&USE_T), Groups::solve(&USE_ADDED_T));
    let rewritten = after.rewritten(&before);
    std::println!("NAMES GROUPS: a pub use added rewrote {} of {} group record(s) (floor 2: its module's, its reader's): {rewritten:?}", rewritten.len(), after.groups.len());
    let mut want = std::vec![std::vec![std::string::String::from("k0::a")], std::vec![std::string::String::from("k0::r")]];
    want.sort();
    assert_eq!(rewritten, want, "the records rewritten are not `a`'s and its reader `r`'s");
    holds(&before, &after);
}

// `a` and `b` glob each other and each imports a name through its glob from the other: one
// group. `c` names `Y` through `a`'s glob and `X` through `b`'s: it reads both (a lookup another
// module asks is the asker's read, never the module's, so a glob no own import reads through
// makes no cycle).
const CYCLE: &str = "#![no_core]
pub mod a {
    pub use crate::b::*;
    pub struct X;
    use self::Y as Y2;
}
pub mod b {
    pub use crate::a::*;
    pub struct Y;
    use self::X as X2;
}
pub mod c {
    pub use crate::a::Y as W;
    pub use crate::b::X as V;
}
";
const CYCLE_EDIT: &str = "#![no_core]
pub mod a {
    pub use crate::b::*;
    pub struct X;
    use self::Y as Y2;
}
pub mod b {
    pub use crate::a::*;
    pub struct Y;
    use self::X as X2;
    pub struct Z;
}
pub mod c {
    pub use crate::a::Y as W;
    pub use crate::b::X as V;
}
";
pub(super) static CYCLE_T: Texts = Texts(&[&[("/k0/lib.rs", CYCLE)]]);
pub(super) static CYCLE_EDIT_T: Texts = Texts(&[&[("/k0/lib.rs", CYCLE_EDIT)]]);

#[test]
fn a_glob_cycle_of_two_modules_is_one_group() {
    let (before, after) = (Groups::solve(&CYCLE_T), Groups::solve(&CYCLE_EDIT_T));
    let pair = std::vec![std::string::String::from("k0::a"), std::string::String::from("k0::b")];
    assert!(before.cuts().contains(&pair), "`a` and `b` glob each other and are not one group: {:?}", before.cuts());
    assert_eq!(before.group("k0::a").members.len(), 2);
    let rewritten = after.rewritten(&before);
    std::println!("NAMES GROUPS: an item added in a glob cycle rewrote {} of {} group record(s) (floor 1: the cycle's): {rewritten:?}", rewritten.len(), after.groups.len());
    assert_eq!(rewritten, std::vec![pair], "the records rewritten are not the cycle's one");
    holds(&before, &after);
}

// `b` reads `a`'s `S`; then `a` imports `b`'s `Q`, which closes a cycle (only `a`'s text moved).
const CUT: &str = "#![no_core]
pub mod a {
    pub struct S;
}
pub mod b {
    pub use crate::a::S as T;
    pub struct Q;
}
";
const RECUT: &str = "#![no_core]
pub mod a {
    pub struct S;
    pub use crate::b::Q;
}
pub mod b {
    pub use crate::a::S as T;
    pub struct Q;
}
";
pub(super) static CUT_T: Texts = Texts(&[&[("/k0/lib.rs", CUT)]]);
pub(super) static RECUT_T: Texts = Texts(&[&[("/k0/lib.rs", RECUT)]]);

#[test]
fn an_import_that_closes_a_cycle_re_cuts_the_groups() {
    let (before, after) = (Groups::solve(&CUT_T), Groups::solve(&RECUT_T));
    let (a, b) = (std::string::String::from("k0::a"), std::string::String::from("k0::b"));
    assert!(before.cuts().contains(&std::vec![a.clone()]) && before.cuts().contains(&std::vec![b.clone()]), "before: {:?}", before.cuts());
    assert!(after.cuts().contains(&std::vec![a, b]), "the import that closes the cycle did not re-cut: {:?}", after.cuts());
    holds(&before, &after);
}

// Two `macro_rules! mac` around `mod m`, swapped: `m`'s call finds the one above by the
// textual scope (its parent's textual cell) and writes another item (its expansion row).
const RULES_BELOW: &str = "#![no_core]
macro_rules! mac { () => { pub struct One; } }
pub mod m {
    mac!();
}
macro_rules! mac { () => { pub struct Two; } }
";
const RULES_ABOVE: &str = "#![no_core]
macro_rules! mac { () => { pub struct Two; } }
pub mod m {
    mac!();
}
macro_rules! mac { () => { pub struct One; } }
";
pub(super) static RULES_BELOW_T: Texts = Texts(&[&[("/k0/lib.rs", RULES_BELOW)]]);
pub(super) static RULES_ABOVE_T: Texts = Texts(&[&[("/k0/lib.rs", RULES_ABOVE)]]);

#[test]
fn a_macro_moved_above_a_mod_line_moves_the_textual_cell() {
    let (before, after) = (Groups::solve(&RULES_BELOW_T), Groups::solve(&RULES_ABOVE_T));
    let textual = |g: &Groups| g.group("k0::m").reads.iter().any(|r| matches!(r, GroupRead::Textual { .. }));
    assert!(textual(&before) && textual(&after), "`m`'s call read no textual cell of its parent");
    holds(&before, &after);
    // The control: a `macro_rules!` is named by its place, so the textual cell is equal over the
    // swap; without the expansion rows the oracle holds `m`'s group valid and is wrong.
    let control = oracle(&before, &after, &|r| !matches!(r, GroupRead::Expansion { .. }));
    assert!(control.is_err(), "the oracle did not see the expansion rows left out: {control:?}");
}
