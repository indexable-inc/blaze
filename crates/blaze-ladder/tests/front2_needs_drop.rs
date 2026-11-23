//! `core::mem::needs_drop::<T>()` in constants and statics, evaluated by front2's constant
//! evaluator with the real core and alloc (front2/const_drop.rs answers consts.rs
//! `ConstsWorld`'s `drop_impl` and `manually_drop`). Before it, tempo's build census 5 (tree
//! ffc4e070ba) left 14 constants out with "the world has no `Drop` impl or `ManuallyDrop`
//! answer (`needs_drop`)", e.g. crossbeam-epoch 0.9's
//! `thread_local! { static HANDLE: LocalHandle = collector().register(); }` (src/default.rs),
//! whose std expansion asks `needs_drop::<LocalHandle>()` in a constant, `LocalHandle` holding
//! `impl Drop for LocalHandle` (src/collector.rs).
//!
//! The rule: the pinned core, library/core/src/mem/mod.rs, `pub const fn needs_drop<T: ?Sized>()
//! -> bool`, "Returns `true` if dropping values of type `T` matters"; the Reference,
//! "Destructors": "If `T: Drop`, calling `<T as core::ops::Drop>::drop`", then "If `T` is a
//! struct, drop each field", "If `T` is an enum, drop the fields of the active variant";
//! library/core/src/mem/manually_drop.rs: `ManuallyDrop<T>` "inhibit[s] the compiler from
//! automatically calling `T`'s destructor"; library/alloc/src/vec/mod.rs:
//! `unsafe impl<#[may_dangle] T, A: Allocator> Drop for Vec<T, A>`.
//!
//! Lines it prints (and asserts):
//!   `MD: 0` (`ManuallyDrop<D>` with `D: Drop`)
//!   `HOLDS: 0` (a struct whose only droppable part is inside `ManuallyDrop`)
//!   `HOLDS_STATIC: 0` (the same, as a static)
//!   `DD: 1` (`D`, a `Drop` impl in this crate)
//!   `VEC: 1` (a `Drop` impl in another crate: alloc's `Vec`)
//!   `STRING: 1` (another crate's ADT with no impl of its own, whose field `Vec<u8>` has one)
//!   `WRAPPING: 0` (another crate's ADT with no impl and no part that needs drop)
//!   `HANDLE: 1` (the census shape: `Option<LocalHandle>`, core's enum over a local `Drop` type)
//!
//! A wrong fix fails here: answering "no impl" while the impls of `Drop` were not read gives
//! `VEC: 0`; ignoring `ManuallyDrop` gives `MD: 1`; ignoring the fields gives `STRING: 0` and
//! `HANDLE: 0`. Without the fix every line is the refusal naming the ADT. The control for a
//! missing fact (unread impls are no answer, never `false`) is the unit test
//! `front2::const_drop::tests::unread_impls_are_no_answer`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_consts.rs
//! does. NOT RUN by the writer (a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::consts::{Consts, int_of};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "extern crate alloc;
use alloc::{string::String, vec::Vec};
use core::mem::{ManuallyDrop, needs_drop};

fn main() {}

pub struct D(u8);

impl Drop for D {
    fn drop(&mut self) {}
}

pub struct Holds {
    m: ManuallyDrop<D>,
    n: u8,
}

pub struct LocalHandle {
    local: *const u8,
}

impl Drop for LocalHandle {
    fn drop(&mut self) {}
}

const MD: bool = needs_drop::<ManuallyDrop<D>>();
const HOLDS: bool = needs_drop::<Holds>();
static HOLDS_STATIC: bool = needs_drop::<Holds>();
const DD: bool = needs_drop::<D>();
const VEC: bool = needs_drop::<Vec<u8>>();
const STRING: bool = needs_drop::<String>();
const WRAPPING: bool = needs_drop::<core::num::Wrapping<u8>>();
const HANDLE: bool = needs_drop::<Option<LocalHandle>>();
";

/// A line of the outcome, as it happens: appended to the file `FRONT2_WORK_LIST` names, since a
/// passing test's output is not shown by the lane's check script.
fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

#[test]
fn needs_drop_reads_the_records() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/drops/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "drops", dir: "/drops/", root: "/drops/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut buckets = None;
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let cases: [(&str, u128); 8] = [("MD", 0), ("HOLDS", 0), ("HOLDS_STATIC", 0), ("DD", 1), ("VEC", 1), ("STRING", 1), ("WRAPPING", 0), ("HANDLE", 1)];
    let mut keys = Vec::new();
    for (name, _) in cases {
        let k = lib(name);
        keys.push(front.keys.item(k).unwrap_or_else(|_| panic!("{name}'s key")));
    }
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let mut consts = Consts::default();
    let mut lines = Vec::new();
    for ((name, _), key) in cases.iter().zip(keys) {
        let line = match consts.value(&mut records, prepared, key) {
            Ok(rec) => match int_of(rec) {
                Some(v) => format!("{name}: {v}"),
                None => format!("{name}: a record that is no `bool`"),
            },
            Err(e) => format!("{name}: refused: {e}"),
        };
        note(&line);
        lines.push(line);
    }
    let want: Vec<String> = cases.iter().map(|(n, v)| format!("{n}: {v}")).collect();
    assert_eq!(lines, want);
}
