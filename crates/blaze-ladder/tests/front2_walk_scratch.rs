//! The records' ONE glob walk scratch (front2 source.rs `Records::scratch`, lane S4 2026-11-18):
//! every path a `Records` resolves walks globs in one `ThroughScratch` it holds, cleared per
//! walk, where `names::path` made a fresh one per path, per segment and per lexical layer (the
//! stack sample of tempo's engine: `names::walk` 3,653 of 17,644, `through_in` 2,728).
//!
//! Over one crate with glob imports (a cycle of them among them), nested modules and a prelude
//! module of its own (`#[prelude_import]`, core's shape):
//! - `a_held_scratch_walks_every_path_as_a_fresh_one`: every path of the corpus resolves EQUAL
//!   through the records' held scratch (`NamesSource::walk`) and through a fresh scratch per
//!   path (`Records::walk_fresh`, the oracle), forward and then backward; the backward pass, a
//!   warm one, grows no buffer of the scratch. Prints `walks N, scratches made 1, bytes copied 0,
//!   growth on the warm pass 0`.
//! - `a_scratch_not_cleared_is_caught`: the control (rule 23): records whose held scratch keeps
//!   its values between walks (`Records::plant_stale_scratch`) answer some path of the same
//!   corpus otherwise than the oracle. Prints the count of paths that differ (more than 0).
//! - `an_items_record_is_opened_once`: N reads of one crate's file rows through
//!   `Records::items_record` (blaze-cli body_types/heads.rs's read, one per head file), between
//!   walks of the corpus, open the record ONCE (`Records::opened().items`: crates 1, one strings
//!   check); the control opens it per read, as heads.rs did, and counts N. Prints
//!   `items record: reads N, opens 1 (an open per read: N)`.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_mem::Frame;
use blaze_own::{Alloc, Erased};
use blaze_scope::graph::Sysroot;
use blaze_scope::names::{PathError, Resolved, Written};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const APP: &str = "#![no_core]
#![feature(prelude_import)]
pub mod prelude {
    pub mod v1 {
        pub use crate::base::*;
    }
}
#[prelude_import]
use prelude::v1::*;
pub mod base {
    pub struct Thing;
    pub struct Other;
    pub fn helper() {}
    pub mod deep {
        pub struct Deep;
        pub use super::*;
    }
}
pub mod a {
    pub struct X;
    pub fn fx() {}
    pub mod inner {
        pub struct In;
        pub use crate::b::*;
    }
}
pub mod b {
    pub struct Y;
    pub fn fy() {}
    pub use super::a::*;
    pub use super::c::*;
}
pub mod c {
    pub use super::a::*;
    pub use super::b::*;
    pub struct W;
    pub mod n {
        pub use super::*;
        pub struct Z;
    }
}
pub mod user {
    pub fn f() {}
}
";

struct Texts;

impl<'a> SourceTexts<'a> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == "/app/lib.rs").then_some(APP)
    }
}

/// The one crate's two records, by crate index.
struct Kept<'n> {
    names: &'n [u8],
    items: &'n [u8],
}

impl<'n> Load<'n> for Kept<'n> {
    fn names(&self, krate: usize) -> Option<&'n [u8]> {
        (krate == 0).then_some(self.names)
    }

    fn items(&self, krate: usize) -> Option<&'n [u8]> {
        (krate == 0).then_some(self.items)
    }

    fn file(&self, _krate: usize, _path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'n str> {
        None
    }

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<LoadedExpansion<'n>> {
        None
    }
}

fn input() -> CrateInput<'static> {
    CrateInput {
        name: "app",
        key: CrateItemKey(ContentId::of("blaze-ladder walk scratch test crate", b"app")),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root: "/app/lib.rs",
        shown_from: "/",
        features: &[],
        deps: &[],
        cfg: &[],
        script: None,
    }
}

/// The module whose path's last segment is `name` (`""`: the root).
fn module(names: &Names<'_>, name: &str) -> ItemKey {
    names
        .modules(0)
        .find(|m| if name.is_empty() { m.parent.is_none() } else { m.parent.is_some() && m.path.rsplit("::").next() == Some(name) })
        .map(|m| m.key)
        .unwrap_or_else(|| panic!("app's module `{name}`"))
}

/// One path of the corpus: the module it is written in, how it begins, its segments and the
/// namespace of its last.
struct Case {
    at: &'static str,
    kind: PathKind,
    segments: &'static [&'static str],
    ns: Namespace,
}

const fn case(at: &'static str, kind: PathKind, segments: &'static [&'static str], ns: Namespace) -> Case {
    Case { at, kind, segments, ns }
}

const T: Namespace = Namespace::Types;
const V: Namespace = Namespace::Values;

/// Paths through globs (one to a glob cycle, `b` and `c`), nested modules, `super`, and the
/// prelude layer, hits and misses alike.
const CORPUS: &[Case] = &[
    case("", PathKind::Crate, &["c", "X"], T),
    case("", PathKind::Crate, &["c", "Y"], T),
    case("", PathKind::Crate, &["c", "W"], T),
    case("", PathKind::Crate, &["c", "n", "X"], T),
    case("", PathKind::Crate, &["c", "n", "Z"], T),
    case("", PathKind::Crate, &["c", "n", "Y"], T),
    case("", PathKind::Crate, &["c", "fx"], V),
    case("", PathKind::Crate, &["c", "fy"], V),
    case("", PathKind::Crate, &["c", "missing"], T),
    case("", PathKind::Crate, &["a", "inner", "Y"], T),
    case("", PathKind::Crate, &["a", "inner", "X"], T),
    case("", PathKind::Crate, &["a", "inner", "fy"], V),
    case("", PathKind::Crate, &["a", "inner", "W"], T),
    case("", PathKind::Crate, &["b", "X"], T),
    case("", PathKind::Crate, &["b", "W"], T),
    case("", PathKind::Crate, &["b", "fx"], V),
    case("", PathKind::Crate, &["b", "n", "Z"], T),
    case("", PathKind::Crate, &["base", "deep", "Thing"], T),
    case("", PathKind::Crate, &["base", "deep", "deep", "Deep"], T),
    case("", PathKind::Crate, &["base", "deep", "helper"], V),
    case("user", PathKind::Plain, &["Thing"], T),
    case("user", PathKind::Plain, &["Other"], T),
    case("user", PathKind::Plain, &["helper"], V),
    case("user", PathKind::Plain, &["nothing"], T),
    case("user", PathKind::Super(1), &["c", "X"], T),
    case("user", PathKind::Super(1), &["c", "n", "W"], T),
    case("c", PathKind::Plain, &["X"], T),
    case("c", PathKind::Plain, &["fy"], V),
    case("c", PathKind::Plain, &["Thing"], T),
    case("n", PathKind::Plain, &["Y"], T),
    case("n", PathKind::Plain, &["Z"], T),
    case("n", PathKind::Plain, &["fx"], V),
    case("inner", PathKind::Plain, &["X"], T),
    case("inner", PathKind::Plain, &["In"], T),
    case("b", PathKind::Plain, &["W"], T),
];

/// The corpus walked through `records`, in the order `order` gives: each path's answer through
/// the held scratch and through a fresh one. Returns how many differ.
fn walk_all<'n>(records: &'n Records<'n>, names: &Names<'_>, order: impl Iterator<Item = usize>) -> usize {
    let mut differ = 0;
    for i in order {
        let c = &CORPUS[i];
        let at = module(names, c.at);
        let written = Written { kind: c.kind, segments: c.segments };
        let held: Result<Resolved, PathError> = NamesSource::walk(records, at, c.ns, written);
        let fresh: Result<Resolved, PathError> = records.walk_fresh(at, c.ns, written);
        if held != fresh {
            println!("path {i} {:?} in `{}`: held {held:?}, fresh {fresh:?}", c.segments, c.at);
            differ += 1;
        }
    }
    differ
}

/// Resolves the crate, writes its two records in `frame` and opens records over them.
fn records<'n>(frame: &'n Frame, a: Alloc<'n>) -> (&'n Names<'n>, &'n Records<'n>) {
    let (names, records, _) = records_and_items(frame, a);
    (names, records)
}

/// [`records`], and the items record's bytes the records read.
fn records_and_items<'n>(frame: &'n Frame, a: Alloc<'n>) -> (&'n Names<'n>, &'n Records<'n>, &'n [u8]) {
    let crates = [input()];
    let names: &'n Names<'n> = frame.alloc(resolve_crates(&crates, Sysroot::default(), &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}")));
    let c = &names.crates()[0];
    let names_bytes = aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("app's names record: {e:?}")), blaze_own::testing::bump());
    let items_bytes = aligned(&items_record::encode(c, a).unwrap_or_else(|e| panic!("app's items record: {e:?}")), blaze_own::testing::bump());
    let kept: &'n Kept<'n> = frame.alloc(Kept { names: names_bytes, items: items_bytes });
    let keys: &'n [CrateItemKey] = frame.alloc_slice_copy(&[c.key]);
    let crate_names: &'n [&'n str] = frame.alloc_slice_copy(&["app"]);
    let log: &'n Log<'n> = frame.alloc(Log::new(crate_names, a));
    let records: &'n Records<'n> = frame.alloc(Records::new(keys, kept, log, frame, a));
    (names, records, items_bytes)
}

#[test]
fn a_held_scratch_walks_every_path_as_a_fresh_one() {
    // The test's one allocator, made at its top: a frame, its memory from the OS.
    let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let erased = Erased::new(&frame);
    let a: Alloc<'_> = erased.handle();
    let (names, records) = records(&frame, a);
    let forward = walk_all(records, names, 0..CORPUS.len());
    assert_eq!(forward, 0, "{forward} paths differ between the held scratch and a fresh one (forward)");
    let (warm_from, caps) = records.scratch_use();
    assert!(warm_from > 0, "the corpus walks through globs: {warm_from} walks");
    let backward = walk_all(records, names, (0..CORPUS.len()).rev());
    assert_eq!(backward, 0, "{backward} paths differ between the held scratch and a fresh one (backward)");
    let (walks, after) = records.scratch_use();
    let growth: usize = after.iter().zip(caps.iter()).map(|(x, y)| x - y).sum();
    assert_eq!(after, caps, "the held scratch grew on the warm (backward) pass");
    // The corpus exercises what it is for: the globs, the cycle and the nesting resolve.
    let in_root = module(names, "");
    for segments in [&["c", "X"][..], &["c", "Y"][..], &["a", "inner", "Y"][..], &["c", "n", "Z"][..]] {
        let r = NamesSource::walk(records, in_root, T, Written { kind: PathKind::Crate, segments });
        assert_eq!(r.map(|r| r.consumed), Ok(segments.len()), "crate::{segments:?} resolves whole");
    }
    println!("walk scratch: paths {} x 2 orders, walks {walks}, scratches made 1, bytes copied 0, growth on the warm pass {growth}", CORPUS.len());
}

#[test]
fn a_scratch_not_cleared_is_caught() {
    let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let erased = Erased::new(&frame);
    let a: Alloc<'_> = erased.handle();
    let (names, records) = records(&frame, a);
    records.plant_stale_scratch();
    let differ = walk_all(records, names, 0..CORPUS.len()) + walk_all(records, names, (0..CORPUS.len()).rev());
    println!("walk scratch control: a scratch not cleared between walks answers {differ} paths otherwise than a fresh one");
    assert!(differ > 0, "the oracle missed a held scratch whose values are not cleared between walks");
}

#[test]
fn an_items_record_is_opened_once() {
    const READS: usize = 64;
    let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let erased = Erased::new(&frame);
    let a: Alloc<'_> = erased.handle();
    let (names, records, items_bytes) = records_and_items(&frame, a);
    let files = records.items_record(0).expect("app's items record").files();
    assert!(files > 0, "app has a file row");
    let mut rows = 0usize;
    for read in 0..READS {
        let items = records.items_record(0).expect("app's items record");
        let index = (read % files) as u32;
        assert!(matches!(items.file_surface(index), Ok(Some(_))), "file {index} of app reads");
        rows += 1;
        // Questions of the records between the reads: they share the one open.
        if read % 8 == 0 {
            assert_eq!(walk_all(records, names, 0..CORPUS.len()), 0, "the corpus walks as a fresh scratch does");
        }
    }
    let opened = records.opened().items;
    assert_eq!(opened.crates, 1, "{READS} reads of one items record opened it {} times", opened.crates);
    assert_eq!(opened.string_checks, 1, "the record's strings were checked {} times", opened.string_checks);
    // The control (rule 23): the read heads.rs made before, an open per read, counts an open each.
    let mut per_read = 0usize;
    for read in 0..READS {
        let items = items_record::ItemsRecord::open(items_bytes, a).expect("app's items record opens");
        assert!(matches!(items.file_surface((read % files) as u32), Ok(Some(_))));
        per_read += items.decoded().crates;
    }
    assert_eq!(per_read, READS, "an open per read is counted per read");
    println!("items record: reads {rows}, opens {} (an open per read: {per_read})", opened.crates);
}
