//! journal_sweep.rs's sans-IO decision under a deterministic simulation (rules 14, 64): whether
//! the checkout's answer is trusted ([`judge`]), and which rows its names reach ([`Index::of`],
//! [`marks`]).
//!
//! THE MODEL: a checkout's files and folders (a module slot `a/x.rs` with its `a/x/mod.rs`, a
//! slot under a folder, two listings, a manifest, a build script's stat and listing, a
//! root-level slot, a deep slot, blaze's and cargo's folders, a git fact, a fact-mount file),
//! the answer's rows over it, a few edits (each naming its file, and each folder it made, as
//! FSEvents does), and the system's answer for them, degraded as FSEvents may: an event
//! coalesced into a subtree to scan again, a folder named for a file in it, events dropped
//! (the folder lost whole), a reset store (another UUID, some events gone), an ID from the
//! future, no place saved. The checkout is one unit, its own folder, named or the driver's root
//! alike (blaze-core journals the root with its own level).
//!
//! THE ORACLE, independent of the code: every row whose state (as the model computes it from
//! the files) differs after the edits is looked at; and with nothing edited and the answer
//! exact, no row inside a unit is looked at but the ones always followed (a git fact, a fact
//! file). Planted, each caught: a lost subtree ignored, a folder's name not reaching the rows
//! beneath it, a reset or a future ID trusted. (The fact mount's absolute rows making the root
//! a unit, planted until 2026-11-20, is no bug now: the root is the unit.)

use std::collections::{BTreeMap, BTreeSet};
use std::string::{String, ToString};
use std::vec::Vec as StdVec;

use blaze_ids::ContentId;
use blaze_sim::{Broke, Config, Tape, check, control};
use bstd::mem::Tag;

use super::*;
use crate::sweep::Rows;

/// Every file the model may hold.
const FILES: [&str; 12] = [
    "a/x.rs",
    "a/x/mod.rs",
    "a/b/y.rs",
    "a/b/lib.rs",
    "a/b/tmp.123",
    "a/lib.rs",
    "c/Cargo.toml",
    "c/build.rs",
    "top.rs",
    ".blaze/v2/store",
    "target/debug/o",
    "d/e/deep.rs",
];

/// The answer's rows: what it read.
const ROWS: [(LeafKind, u8, &str); 11] = [
    (LeafKind::Slot, MODULE, "a/x.rs"),
    (LeafKind::Slot, 0, "a/b/y.rs"),
    (LeafKind::Listing, 0, "a/b"),
    (LeafKind::Listing, 0, "a"),
    (LeafKind::Manifest, 0, "c/Cargo.toml"),
    (LeafKind::ScriptStat, 0, "c/build.rs"),
    (LeafKind::ScriptListing, 0, "c"),
    (LeafKind::Slot, 0, "top.rs"),
    (LeafKind::Slot, 0, "d/e/deep.rs"),
    (LeafKind::ScriptGit, 0, "HEAD"),
    (LeafKind::ScriptFile, 0, "/blaze/facts/pins"),
];

/// The place the stored answer saved.
const SINCE: Cursor = Cursor::FsEvents { event: 1_000, volume: [1; 16] };

fn frame() -> Frame {
    Frame::with_reserve(1 << 20, Tag::Other).expect("the test's frame")
}

/// The leaf table of `rows`, as the answer stores it.
fn table<'f>(rows: &[(LeafKind, u8, &str)], frame: &'f Frame) -> blaze_own::Vec<u8, &'f Frame> {
    let mut table = Rows::new_in(frame);
    for (kind, flags, path) in rows {
        table.push(*kind, *flags, path, None, ContentId([0; 32]));
    }
    Leaves::bytes_in(&table, frame).expect("the leaf table")
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(folder, _)| folder)
}

/// `path` relative to the unit `unit` (`""`: the checkout), if under it.
fn under<'p>(unit: &str, path: &'p str) -> Option<&'p str> {
    match unit.is_empty() {
        true => Some(path),
        false => path.strip_prefix(unit).and_then(|rest| rest.strip_prefix('/')),
    }
}

/// The checkout: each file's version, and the folders made.
#[derive(Clone, Default)]
struct Tree {
    files: BTreeMap<String, u32>,
    dirs: BTreeSet<String>,
}

impl Tree {
    fn ver(&self, path: &str) -> Option<u32> {
        self.files.get(path).copied()
    }

    /// A folder's entries (name, is a folder); `None` when the folder is not there.
    fn entries(&self, dir: &str) -> Option<BTreeSet<(String, bool)>> {
        if !self.dirs.contains(dir) {
            return None;
        }
        let last = |path: &str| path.rsplit('/').next().unwrap_or(path).to_string();
        let files = self.files.keys().filter(|it| parent(it) == dir).map(|it| (last(it), false));
        let dirs = self.dirs.iter().filter(|it| parent(it) == dir).map(|it| (last(it), true));
        Some(files.chain(dirs).collect())
    }

    /// What a reader of the row sees, by the language's and the layout's rules.
    fn state(&self, kind: LeafKind, flags: u8, path: &str) -> String {
        match kind {
            LeafKind::Slot if flags & MODULE != 0 => {
                let stem = path.strip_suffix(".rs").unwrap_or(path);
                std::format!("{:?}", (self.ver(path), self.ver(&std::format!("{stem}/mod.rs"))))
            }
            LeafKind::Listing => {
                let last = path.rsplit('/').next().unwrap_or(path);
                // The fixture's `target` is cargo's, as the replays' index takes it
                // (`OwnFolders { target_tagged: true }`).
                let wanted = self.entries(path).map(|entries| entries.into_iter().filter(|(name, dir)| sweep::wanted(last, name, *dir, true)).collect::<StdVec<_>>());
                std::format!("{wanted:?}")
            }
            LeafKind::ScriptListing => std::format!("{:?}", self.entries(path)),
            // Not of the checkout's files: never moved by an edit here.
            LeafKind::ScriptGit => String::new(),
            LeafKind::ScriptFile if path.starts_with('/') => String::new(),
            _ => std::format!("{:?}", self.ver(path)),
        }
    }

    /// A file made or written, its folders made: each named as FSEvents names it.
    fn write(&mut self, file: &str, events: &mut StdVec<String>) {
        for (at, c) in file.char_indices() {
            if c == '/' && self.dirs.insert(file[..at].to_string()) {
                events.push(file[..at].to_string());
            }
        }
        *self.files.entry(file.to_string()).or_insert(0) += 1;
        events.push(file.to_string());
    }

    fn edit(&mut self, tape: &Tape, events: &mut StdVec<String>) {
        let file = FILES[tape.index(FILES.len(), "the file edited")];
        if self.files.contains_key(file) && tape.coin("removed") {
            self.files.remove(file);
            events.push(file.to_string());
            return;
        }
        self.write(file, events);
    }
}

/// How the system's answer tells the edits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    Exact,
    /// One event coalesced into a subtree to scan again (`MustScanSubDirs`), any ancestor.
    Coalesced,
    /// One file's event told as its folder's.
    FolderNamed,
    /// Events dropped: the folder lost whole, the names maybe gone.
    Dropped,
    /// The store made again (another UUID): some events gone.
    Reset,
    /// The answer ends before the saved place: some events gone.
    Future,
    /// No place was saved.
    NoPlace,
}

fn case(tape: &Tape, plant: JournalPlant) -> Result<(), Broke> {
    let frame = frame();
    let bytes = table(&ROWS, &frame);
    let (leaves, _) = Leaves::of(&bytes).map_err(|error| Broke::Harness(std::format!("the leaf table: {error}")))?;
    // The checkout before.
    let mut before = Tree::default();
    for file in FILES {
        if tape.coin("the file is there at first") {
            before.write(file, &mut StdVec::new());
        }
    }
    let mut after = before.clone();
    let mut names = StdVec::new();
    let edits = tape.below(4, "edits");
    for _ in 0..edits {
        after.edit(tape, &mut names);
    }
    // The system's answer for them.
    let answer = match tape.below(10, "the answer") {
        0..=3 => Answer::Exact,
        4 => Answer::Coalesced,
        5 => Answer::FolderNamed,
        6 => Answer::Dropped,
        7 => Answer::Reset,
        8 => Answer::Future,
        _ => Answer::NoPlace,
    };
    let mut since = Some(SINCE);
    let mut now = Cursor::FsEvents { event: 1_000 + names.len() as u64, volume: [1; 16] };
    let mut lost: StdVec<(String, Lost)> = StdVec::new();
    match answer {
        Answer::Exact => {}
        Answer::Coalesced if !names.is_empty() => {
            let event = names.remove(tape.index(names.len(), "the event coalesced"));
            let mut ancestors: StdVec<&str> = std::vec![""];
            ancestors.extend(event.char_indices().filter(|(_, c)| *c == '/').map(|(at, _)| &event[..at]));
            let at = ancestors[tape.index(ancestors.len(), "the subtree to scan again")].to_string();
            lost.push((at, Lost::Rescan));
        }
        Answer::Coalesced => {}
        Answer::FolderNamed => {
            let deep: StdVec<usize> = (0..names.len()).filter(|&i| parent(&names[i]).contains('/')).collect();
            if !deep.is_empty() {
                let i = deep[tape.index(deep.len(), "the file told as its folder")];
                names[i] = parent(&names[i]).to_string();
            }
        }
        Answer::Dropped => {
            lost.push((String::new(), Lost::Dropped));
            if tape.coin("the names dropped too") {
                names.clear();
            }
        }
        Answer::Reset | Answer::Future => {
            now = match answer {
                Answer::Reset => Cursor::FsEvents { event: 5, volume: [2; 16] },
                _ => Cursor::FsEvents { event: 999, volume: [1; 16] },
            };
            names.retain(|_| tape.coin("an event told after the break"));
        }
        Answer::NoPlace => {
            since = None;
            names.clear();
        }
    }
    names.sort();
    names.dedup();
    // The one unit, the checkout's folder (named, or the driver's root: journaled alike), and
    // its answer.
    let units = [""];
    let mut files_of: StdVec<StdVec<&str>> = StdVec::new();
    let mut lost_of: StdVec<StdVec<(&str, Lost)>> = StdVec::new();
    let mut refused: StdVec<bool> = StdVec::new();
    for unit in units.iter() {
        let unit: &str = unit;
        files_of.push(names.iter().filter_map(|name| under(unit, name)).collect());
        let lost_here: StdVec<(&str, Lost)> = lost
            .iter()
            .filter_map(|(at, why)| match at.is_empty() || at == unit {
                true => Some(("", *why)),
                false => under(unit, at).map(|rel| (rel, *why)),
            })
            .collect();
        let whole = lost_here.iter().find(|(rel, _)| rel.is_empty()).map(|(_, why)| *why);
        refused.push(judge(since, now, whole, plant).is_some());
        lost_of.push(lost_here);
    }
    let mut views: StdVec<UnitView<'_>> = StdVec::new();
    for (k, unit) in units.iter().enumerate() {
        let index = match refused[k] {
            true => None,
            false => Index::of(files_of[k].iter().copied(), lost_of[k].iter().copied(), unit.is_empty().then_some(OwnFolders { target_tagged: true }), plant, &frame),
        };
        views.push(UnitView { at: Where::Checkout, name: unit, index });
    }
    let looked = marks(&leaves, &views, plant, &frame);
    let said = || std::format!("answer {answer:?}, names {names:?}, lost {lost:?}");
    for i in 0..leaves.len() {
        let (row, path) = (leaves.row(i), leaves.path(i));
        let moved = before.state(row.kind, row.flags, path) != after.state(row.kind, row.flags, path);
        tape.reached(match moved {
            true => "a row moved",
            false => "a row kept",
        });
        if moved && !looked[i] {
            return Err(Broke::Oracle(std::format!("{path} ({:?}) moved but was not looked at; {}", row.kind, said())));
        }
    }
    if answer == Answer::Exact && edits == 0 {
        for i in 0..leaves.len() {
            let (row, path) = (leaves.row(i), leaves.path(i));
            let always = row.kind == LeafKind::ScriptGit || path.starts_with('/');
            let in_unit = units.iter().any(|unit| under(unit, path).is_some_and(|rel| !rel.is_empty()));
            if in_unit && !always && looked[i] {
                return Err(Broke::Oracle(std::format!("nothing was named, yet {path} was looked at; {}", said())));
            }
        }
    }
    Ok(())
}

/// The invariant: the journal sweep inspects every row affected by a change, folder event, lost subtree, or untrusted journal continuation, and inspects no checkout row when nothing moved.
#[test]
fn the_rows_looked_at_cover_every_change_and_no_more_when_nothing_moved() {
    check(Config::cases(2000), |tape| case(tape, JournalPlant::None));
}

#[test]
fn the_oracle_catches_a_lost_subtree_ignored() {
    control(Config::cases(4000), JournalPlant::None, JournalPlant::LostIgnored, "moved but was not looked at", case);
}

#[test]
fn the_oracle_catches_a_folder_name_not_reaching_beneath_it() {
    control(Config::cases(4000), JournalPlant::None, JournalPlant::FolderNameIgnored, "moved but was not looked at", case);
}

#[test]
fn the_oracle_catches_a_reset_or_a_future_id_trusted() {
    control(Config::cases(4000), JournalPlant::None, JournalPlant::ResetTrusted, "moved but was not looked at", case);
}

/// A checkout at the driver's root is one unit, its own folder: with its journal quiet, no row
/// of it is looked at, root-level rows and blaze's folder included; only the rows in no unit
/// (the fact mount's, the home's listings) are. CONTROL: one root-level name told reaches its
/// row and no other.
#[test]
fn a_quiet_journal_of_the_root_looks_at_no_row_of_the_checkout() {
    let frame = frame();
    let rows = [
        (LeafKind::ScriptFile, 0, "/blaze/facts/pins"),
        (LeafKind::ScriptFile, 0, "/blaze/facts/reply/a"),
        (LeafKind::Manifest, 0, ".blaze/v2/m"),
        (LeafKind::HomeListing, 0, "v2/sources"),
        (LeafKind::Slot, 0, "crates/a/src/lib.rs"),
        (LeafKind::Slot, 0, "crates/b/src/lib.rs"),
        (LeafKind::Manifest, 0, "crates/a/Cargo.toml"),
        (LeafKind::Manifest, 0, "bin/x/Cargo.toml"),
        (LeafKind::Manifest, 0, "Cargo.toml"),
    ];
    let bytes = table(&rows, &frame);
    let (leaves, _) = Leaves::of(&bytes).expect("the leaf table");
    let looked_with = |told: &'static [&'static str]| {
        let index = Index::of(told.iter().copied(), core::iter::empty(), Some(OwnFolders { target_tagged: true }), JournalPlant::None, &frame);
        let views = [UnitView { at: Where::Checkout, name: "", index }];
        marks(&leaves, &views, JournalPlant::None, &frame)
    };
    let quiet = looked_with(&[]);
    let told = looked_with(&["Cargo.toml"]);
    for i in 0..leaves.len() {
        let path = leaves.path(i);
        let outside = leaves.row(i).kind == LeafKind::HomeListing || path.starts_with('/');
        assert_eq!(quiet[i], outside, "{path}");
        assert_eq!(told[i], outside || path == "Cargo.toml", "CONTROL: {path}");
    }
}

/// A unit's answer is trusted only as a continuation of the journal its place is of.
#[test]
fn judge_trusts_only_a_continuation_of_the_saved_journal() {
    let at = |event, volume| Cursor::FsEvents { event, volume: [volume; 16] };
    let none = JournalPlant::None;
    assert_eq!(judge(Some(at(10, 1)), at(10, 1), None, none), None, "the same place: trusted");
    assert_eq!(judge(Some(at(10, 1)), at(90, 1), None, none), None, "later on the same store: trusted");
    assert_eq!(judge(None, at(90, 1), None, none), Some(Refused::NoCursor));
    assert_eq!(judge(Some(Cursor::None), at(90, 1), None, none), Some(Refused::NoCursor));
    assert_eq!(judge(Some(at(10, 1)), at(90, 2), None, none), Some(Refused::Lost(Lost::Reset)), "another UUID: the store was made again");
    assert_eq!(judge(Some(at(10, 1)), at(9, 1), None, none), Some(Refused::Lost(Lost::Reset)), "an ID the journal is now before");
    assert_eq!(judge(Some(at(10, 1)), Cursor::Sim { journal: 1, next: 90 }, None, none), Some(Refused::Lost(Lost::Reset)), "a place of another kind");
    for why in [Lost::Reset, Lost::Truncated, Lost::Dropped, Lost::Rescan, Lost::RootChanged, Lost::Unnamed, Lost::NoJournal] {
        assert_eq!(judge(Some(at(10, 1)), at(90, 1), Some(why), none), Some(Refused::Lost(why)), "the folder itself lost: {why}");
    }
    // CONTROL: the planted bug trusts another store.
    assert_eq!(judge(Some(at(10, 1)), at(90, 2), None, JournalPlant::ResetTrusted), None);
}

/// A change-driven answer's place ([`place_of`], blaze-build-script git `Gen`) orders as the
/// sweep's judge does: `b` reaches on from `a` exactly when the journal continues from `a` to
/// `b` (the same kind and store, not before); no place has none.
#[test]
fn a_journal_place_orders_as_the_judge_continues() {
    let cursors = [
        Cursor::FsEvents { event: 10, volume: [1; 16] },
        Cursor::FsEvents { event: 90, volume: [1; 16] },
        Cursor::FsEvents { event: 90, volume: [2; 16] },
        Cursor::Usn { volume: 1, journal: 1, next: 10 },
        Cursor::Usn { volume: 1, journal: 2, next: 90 },
        Cursor::Usn { volume: 2, journal: 1, next: 90 },
        Cursor::Sim { journal: 1, next: 10 },
        Cursor::Sim { journal: 1, next: 90 },
        Cursor::Sim { journal: 2, next: 10 },
    ];
    for a in cursors {
        for b in cursors {
            let (pa, pb) = (place_of(a).expect("a place"), place_of(b).expect("a place"));
            assert_eq!(pa.journal == pb.journal && pa.at <= pb.at, continues(a, b), "{a:?} to {b:?}");
        }
    }
    assert_eq!(place_of(Cursor::None), None);
    assert_eq!(place_of(Cursor::Usn { volume: 1, journal: 1, next: -1 }), None);
}

/// What no rule of the decision catches (journal_sweep.rs, "What no rule here catches"): an
/// edit the system's answer leaves out, with the journal otherwise continuing, is not looked
/// at. CONTROL: the same edit, named, is.
#[test]
fn an_edit_the_answer_omits_is_missed_by_events_alone() {
    let frame = frame();
    let bytes = table(&ROWS, &frame);
    let (leaves, _) = Leaves::of(&bytes).expect("the leaf table");
    let row = (0..leaves.len()).find(|&i| leaves.path(i) == "a/b/y.rs").expect("the slot's row");
    let looked_with = |names: &[&'static str]| {
        assert_eq!(judge(Some(SINCE), Cursor::FsEvents { event: 1_001, volume: [1; 16] }, None, JournalPlant::None), None);
        let index = Index::of(names.iter().copied(), core::iter::empty(), Some(OwnFolders { target_tagged: true }), JournalPlant::None, &frame);
        let views = [UnitView { at: Where::Checkout, name: "", index }];
        marks(&leaves, &views, JournalPlant::None, &frame)[row]
    };
    assert!(!looked_with(&[]), "an omitted edit is not looked at: the gap the periodic audit would close");
    assert!(looked_with(&["a/b/y.rs"]), "CONTROL: the edit named is looked at");
}

/// cargo's `target` is left out of the root's listing only with its `CACHEDIR.TAG`
/// (blaze_build_script::served, sweep.rs `wanted`), so the journal must move the root's listing
/// when the tag (or `target` itself) changes, and drop a name under a tagged `target` only.
/// CONTROL: a build's name in a tagged `target` moves nothing; under an untagged `target` (a
/// package's folder) its slot is looked at.
#[test]
fn cargos_tag_moves_the_root_listing_and_only_a_tagged_target_is_dropped() {
    let frame = frame();
    let rows = [(LeafKind::Listing, 0, ""), (LeafKind::Slot, 0, "target/gen.rs"), (LeafKind::Manifest, 0, "Cargo.toml")];
    let bytes = table(&rows, &frame);
    let (leaves, _) = Leaves::of(&bytes).expect("the leaf table");
    let at = |path: &str| (0..leaves.len()).find(|&i| leaves.path(i) == path).expect("the row");
    let (listing, slot) = (at(""), at("target/gen.rs"));
    let looked = |names: &'static [&'static str], lost: &'static [(&'static str, Lost)], target_tagged: bool| {
        let index = Index::of(names.iter().copied(), lost.iter().copied(), Some(OwnFolders { target_tagged }), JournalPlant::None, &frame);
        let views = [UnitView { at: Where::Checkout, name: "", index }];
        let marked = marks(&leaves, &views, JournalPlant::None, &frame);
        (marked[listing], marked[slot])
    };
    for tagged in [true, false] {
        assert!(looked(&["target/CACHEDIR.TAG"], &[], tagged).0, "the tag's change moves the root's listing (tagged {tagged})");
        assert!(looked(&[], &[("target", Lost::Rescan)], tagged).0, "a lost `target` moves the root's listing (tagged {tagged})");
    }
    assert_eq!(looked(&["target/debug/build/a-0/output"], &[], true), (false, false), "CONTROL: a build's file under a tagged `target` moves nothing");
    assert_eq!(looked(&["target/gen.rs"], &[], false), (false, true), "an untagged `target` is a package's folder: its file is looked at");
    assert_eq!(looked(&[], &[], false), (false, false));
}

/// The profile's line, in D150's form.
#[test]
fn the_sweep_line_says_by_events_or_full_and_why() {
    let counts = |journal, since, streams| SweepCounts { journal, events: 2, statted: 3, leaves: 9, settled: 0, since, streams, replays: Replays::default(), parts: None, status: None, helper: None, looks: None };
    assert_eq!(
        std::format!("{}", counts(JournalUse::Used, Some(7), 1)),
        "sweep: statted 3 of 9 files (by events since id 7; 2 event(s) in 1 folder(s)); settled classes checked 0"
    );
    let reset = std::format!("{}", counts(JournalUse::Refused(Refused::Lost(Lost::Reset)), None, 0));
    assert!(reset.starts_with("sweep: statted 3 of 9 files (full, refused: the journal of changes was reset"), "{reset}");
    let linux = std::format!("{}", counts(JournalUse::NoneOnThisOs, None, 0));
    assert!(linux.starts_with("sweep: statted 3 of 9 files (full: none on this OS"), "{linux}");
}

/// No stream replays the home: its listings, statted on every request, are counted apart, so
/// `statted S of F` is the checkout's rows (a no-op still reads `statted 0`). The replays' wall
/// time is said when a stream ran. CONTROL: with every row statted (no parts), nothing is taken
/// out.
#[test]
fn the_homes_listings_are_counted_apart_and_the_replays_timed() {
    let replays = Replays { units: 1, ns: 21_400_000, most_ns: 21_400_000, longest: bstd::fs::ReplayTimes::default(), place_age_ns: None };
    let counts = SweepCounts { journal: JournalUse::Used, events: 0, statted: 2, leaves: 9, settled: 0, since: Some(7), streams: 1, replays, parts: None, status: None, helper: None, looks: None };
    let parts = SweepParts { home: 2, whole: true, ..SweepParts::default() };
    let apart = counts.clone().with_parts(Some(parts));
    assert_eq!((apart.statted, apart.leaves), (0, 7), "the home's two listings out of both counts");
    let line = std::format!("{apart}");
    assert!(line.starts_with("sweep: statted 0 of 7 files (by events since id 7"), "{line}");
    assert!(line.contains("replays: 1 stream(s) in 21.4 ms"), "{line}");
    assert!(line.contains("2 home listing(s) followed by their stamps"), "{line}");
    let all = counts.with_parts(None);
    assert_eq!((all.statted, all.leaves), (2, 9), "CONTROL: every row statted, none taken out");
}

/// A replay's stretches are each said (rule 15): the longest replay's start, history, cookie and
/// stop, so a slow one names where it was slow; the longest of two is the one kept. CONTROL: a
/// replay with no parts (refused before its stream ended) prints none.
#[test]
fn the_longest_replays_stretches_are_said() {
    let fast = bstd::fs::ReplayTimes { start_ns: 2_000_000, history_ns: 5_000_000, history_batches: 1, ..bstd::fs::ReplayTimes::default() };
    let slow = bstd::fs::ReplayTimes { start_ns: 2_000_000, first_ns: 160_000_000, history_ns: 161_000_000, history_batches: 2, cookie_heard_ns: 3_000_000, ..bstd::fs::ReplayTimes::default() };
    let mut replays = Replays::default();
    replays.add(15_000_000, fast, Some(400_000_000));
    replays.add(170_000_000, slow, Some(183_000_000_000));
    assert_eq!(replays.longest, slow, "the longest replay's parts are kept");
    let line = std::format!("{replays}");
    assert!(line.contains("stream started 2.0 ms, first batch 160.0 ms, HistoryDone 161.0 ms after 2 batch(es)"), "{line}");
    assert!(line.contains("cookie heard 3.0 ms"), "{line}");
    assert!(line.ends_with("; its place 183.00 s old"), "the longest one's place's age: {line}");
    let mut refused = Replays::default();
    refused.add(9_000_000, bstd::fs::ReplayTimes::default(), None);
    let line = std::format!("{refused}");
    assert!(!line.contains("stream started"), "CONTROL: no parts, none said: {line}");
    assert!(line.contains("its place's age unknown"), "CONTROL: no time kept, said so: {line}");
}

/// The places' wall time rides beside them and reads back; places kept before it (no time)
/// stay whole and say none; a trailer of another length is not whole.
#[test]
fn the_places_keep_the_time_they_were_taken() {
    let frame = frame();
    let cursor = SINCE;
    let mut with = Vec::new_in(&frame);
    Stored::encode_units([(0u8, "", cursor)].into_iter(), Some(1_700_000_000_123_456_789), &mut with);
    let stored = Stored::of(&with).expect("places with a time are whole");
    assert_eq!(stored.taken_wall_ns(), Some(1_700_000_000_123_456_789));
    assert_eq!(stored.units().count(), 1);
    let mut without = Vec::new_in(&frame);
    Stored::encode_units([(0u8, "", cursor)].into_iter(), None, &mut without);
    assert_eq!(Stored::of(&without).expect("an older form is whole").taken_wall_ns(), None);
    with.push(0);
    assert!(Stored::of(&with).is_none(), "a trailer of nine bytes is not this form");
}

/// Direct (symlink-free, non-own-folder) script read rows are only inspected when the journal
/// reaches them or an ancestor folder, while non-direct rows (potential symlinks), own-folder
/// rows (`.git`), and `ScriptGit` still fall back to `reach.asks()` on any edit.
#[test]
fn direct_script_rows_skip_unrelated_edits_while_symlinks_and_git_stay_inspected() {
    let frame = frame();
    let rows = [
        (LeafKind::ScriptFile, crate::sweep::DIRECT_PATH, "bin/tempo/src/main.rs"),
        (LeafKind::ScriptFile, crate::sweep::DIRECT_PATH, "crates/other/src/lib.rs"),
        (LeafKind::ScriptFile, 0, "crates/linked/src/lib.rs"),
        (LeafKind::ScriptFile, crate::sweep::DIRECT_PATH, ".git/HEAD"),
        (LeafKind::ScriptGit, 0, ""),
    ];
    let bytes = table(&rows, &frame);
    let (leaves, _) = Leaves::of(&bytes).expect("the leaf table");
    let effective = |told: &'static [&'static str]| {
        let index = Index::of(told.iter().copied(), core::iter::empty(), Some(OwnFolders { target_tagged: true }), JournalPlant::None, &frame);
        let views = [UnitView { at: Where::Checkout, name: "", index }];
        let m = marks(&leaves, &views, JournalPlant::None, &frame);
        let mut by_path = std::vec::Vec::new();
        for i in 0..leaves.len() {
            let row = leaves.row(i);
            let path = leaves.path(i);
            let inspected = if script_row_uses_asks(row.kind, row.flags, path, true) { !told.is_empty() } else { m[i] };
            by_path.push((path, inspected));
        }
        by_path
    };
    let on_edit = effective(&["bin/tempo/src/main.rs"]);
    for (path, expected) in [
        ("bin/tempo/src/main.rs", true),
        ("crates/other/src/lib.rs", false),
        ("crates/linked/src/lib.rs", true),
        (".git/HEAD", true),
        ("", true),
    ] {
        let got = on_edit.iter().find(|(p, _)| *p == path).map(|(_, v)| *v);
        assert_eq!(got, Some(expected), "on unrelated edit, `{path}` inspected");
    }
    let on_dir = effective(&["crates/other"]);
    let got_dir = on_dir.iter().find(|(p, _)| *p == "crates/other/src/lib.rs").map(|(_, v)| *v);
    assert_eq!(got_dir, Some(true), "CONTROL: ancestor folder event still reaches the direct script row beneath it");
}
