//! `core::intrinsics::type_name::<T>()` in constants, evaluated by front2's constant evaluator
//! with the real core and alloc: consts.rs `ConstsWorld` answers `ConstWorld::type_names` with
//! the item paths of the names source (`type_name_path`), printed by blaze-build's one printer
//! (type_name.rs). Before it, the hello copy (round 3, tree 4dd8a4485e) stopped six times at
//! "`type_name` in a world that names no items (`ConstWorld::type_names`) is not evaluated yet",
//! reached through `core::any::type_name`'s `const { intrinsics::type_name::<T>() }`.
//!
//! The rule: the pinned core, library/core/src/any.rs, `type_name`: "Returns the name of a type
//! as a string slice"; rustc prints an item by its def path from its crate's name (the
//! declaring module, never a re-export: `alloc::string::String`, not `std::string::String`).
//!
//! Lines it prints (and asserts):
//!   `OPTION: core::option::Option<alloc::string::String>` (two crates' paths, an argument)
//!   `LOCAL: type_names::D` (this crate's item, by the crate's name)
//!   `MIXED: (u8, &str, [i32; 3])` (primitives, a reference, a tuple, an array's length)
//!   `FN_PTR: fn(u8) -> bool`
//!   `DYN: &dyn core::fmt::Debug` (a trait's path)
//!   `ANY: u8` (through `core::any::type_name`, the path hello reaches)
//!
//! A wrong fix fails here: a path read from a re-export gives `std::option::Option<..>`; a path
//! without the crate's name gives `option::Option<..>` and `D`. Without the fix every line is
//! the refusal "`type_name` in a world that names no items".
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_consts.rs
//! does. NOT RUN by the writer (a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "#![feature(core_intrinsics)]

use core::intrinsics::type_name;

fn main() {}

pub struct D(u8);

const OPTION: &str = type_name::<Option<String>>();
const LOCAL: &str = type_name::<D>();
const MIXED: &str = type_name::<(u8, &'static str, [i32; 3])>();
const FN_PTR: &str = type_name::<fn(u8) -> bool>();
const DYN: &str = type_name::<&'static dyn core::fmt::Debug>();
const ANY: &str = core::any::type_name::<u8>();
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
fn type_name_prints_item_paths_from_the_records() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/type_names/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "type_names", dir: "/type_names/", root: "/type_names/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let cases: [(&str, &str); 6] = [
        ("OPTION", "core::option::Option<alloc::string::String>"),
        ("LOCAL", "type_names::D"),
        ("MIXED", "(u8, &str, [i32; 3])"),
        ("FN_PTR", "fn(u8) -> bool"),
        ("DYN", "&dyn core::fmt::Debug"),
        ("ANY", "u8"),
    ];
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
            Ok(rec) => {
                // A `&str` record: its root is the (address, length) pair, the text another
                // allocation of the same record the address points to.
                let root = rec.data.allocs.first().map(|a| a.bytes.as_slice()).unwrap_or(&[]);
                let len = root.get(8..16).map(|b| u64::from_le_bytes(b.try_into().expect("eight bytes")));
                let text = rec.data.allocs.iter().skip(1).find_map(|a| std::str::from_utf8(a.bytes.as_slice()).ok().filter(|t| Some(t.len() as u64) == len));
                match text {
                    Some(t) => format!("{name}: {t}"),
                    None => format!("{name}: a record that is no `&str`: {rec:?}"),
                }
            }
            Err(e) => format!("{name}: refused: {e}"),
        };
        note(&line);
        lines.push(line);
    }
    let want: Vec<String> = cases.iter().map(|(n, v)| format!("{n}: {v}")).collect();
    assert_eq!(lines, want);
}
