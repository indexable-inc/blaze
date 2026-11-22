//! A WRITTEN TYPE whose first segment a `use` declared in a block binds, walked over NAMES
//! RECORDS from `main` (front2 block_type_uses.rs): each row is a `no_core` program whose walk
//! must check every body clean with the records' log empty. Seconds, where the entry-programs
//! test (tests/front2_main.rs `the_entry_programs_as_far_as_each_gets`) takes six minutes.
//!
//! The stop that started it (2026-11-17, lane RF7): the owner's hello world refused
//! `bstd::rt::write_out` with E0412 "cannot find type `IoSlice` in this scope": its body
//! writes `use std::{io::{IoSlice, Write as _}, os::fd::FromRawFd as _};` (blaze-std rt.rs, in
//! the unix `write_out`) and then `let mut left: &mut [IoSlice<'_>] = ..`. The expression
//! `IoSlice::new(..)` was read through the block's import (front2 world_paths.rs); the written
//! type was read at the body's module alone (front2 types.rs, `path_ty`), since a block that
//! holds only `use` items is no module of the names library. Row `nested_group_as_rt_writes`
//! is that form.
//!
//! The Reference: "Use declarations": "Use declarations may appear in modules and blocks,
//! usually at the top."; "Statements", "Item declarations": "Declaring an item within a
//! statement block restricts its scope to the block containing the statement."; "Names",
//! "Scopes", "Item scopes": "The name of an item declared directly in a block is in scope for
//! the entire block" (row `type_before_the_import`); "Name resolution": "Names introduced via
//! use declarations in an outer scope are shadowed by candidates in the same namespace with the
//! same name from an inner scope" (row `import_shadows_the_module_item`).
//!
//! Rule 23: every row fails with the call of `block_type_uses::through_use` in front2 types.rs
//! `Builder::resolve_at` removed: rows but `import_shadows_the_module_item` as E0412 "cannot
//! find type" (the walk's refusal names the fn), that row as a type mismatch (its written type
//! is then the module's `struct Other { b: bool }`, its value `m::Slice`).
//!
//! The test prints one line per row, failures first:
//! ```text
//! block use types nested_group_as_rt_writes: clean (<counts>)
//! ...
//! ```
//!
//! The fixture, reader and walk are tests/local_statics.rs's (one test file cannot use another's
//! helpers), with the allocator a frame made at the top of the test, and the records over the
//! one crate of a `no_core` program, so no collection of the global heap holds them.

#![feature(allocator_api)]

use blaze_emit::{Isa, Os};
use blaze_id::CrateItemKey;
use blaze_id::ItemKey as LibKey;
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
use blaze_mem::Frame;
use blaze_own::{AString, Alloc, Erased};
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/ladder/";
const ROOT: &str = "/ladder/src/main.rs";
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

/// The lang items the walk needs (as tests/local_statics.rs `program` writes them), `Copy` for
/// `i32` (a field read through a reference), and the module the rows import from.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}

#[lang = \"tuple_trait\"]
trait Tuple {}

#[lang = \"fn_once\"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

mod m {
    pub struct Slice {
        pub n: i32,
    }
    pub type Alias = Slice;
    pub mod deep {
        pub struct Pair(pub i32, pub i32);
        pub trait Write {}
    }
}

/// The module's own item of a name a row's block imports another item under.
struct Other {
    b: bool,
}

";

/// One program: its items (a fn `f`) after [`HEAD`], and `main` returning `f()`.
struct Row {
    name: &'static str,
    items: &'static str,
}

const ROWS: &[Row] = &[
    // bstd rt.rs `write_out`'s form: a nested group, a trait imported `as _` beside the type.
    Row {
        name: "nested_group_as_rt_writes",
        items: "fn f() -> i32 {
    use self::m::{deep::{Pair, Write as _}, Slice};
    let s: Slice = Slice { n: 3 };
    let p: Pair = Pair(s.n, 0);
    let r: &Pair = &p;
    r.0
}",
    },
    // A renamed import: the name bound is the rename's.
    Row {
        name: "renamed_import",
        items: "fn f() -> i32 {
    use m::Slice as Cell;
    let c: Cell = Cell { n: 3 };
    c.n
}",
    },
    // The block's import shadows the module's item of the same name.
    Row {
        name: "import_shadows_the_module_item",
        items: "fn f() -> i32 {
    use self::m::Slice as Other;
    let o: Other = Other { n: 3 };
    o.n
}",
    },
    // The type in a block inside the import's block, through `crate::`.
    Row {
        name: "type_in_an_inner_block",
        items: "fn f() -> i32 {
    use crate::m::Slice;
    let n = {
        let s: Slice = Slice { n: 3 };
        s.n
    };
    n
}",
    },
    // The type written before the import: an item of a block is in scope in the whole block.
    Row {
        name: "type_before_the_import",
        items: "fn f() -> i32 {
    let s: Slice = Slice { n: 3 };
    use self::m::Slice;
    s.n
}",
    },
    // An imported module: the written type continues through it.
    Row {
        name: "type_through_an_imported_module",
        items: "fn f() -> i32 {
    use self::m::deep;
    let p: deep::Pair = deep::Pair(3, 0);
    p.0
}",
    },
    // An imported type alias.
    Row {
        name: "imported_alias",
        items: "fn f() -> i32 {
    use self::m::Alias;
    let a: Alias = self::m::Slice { n: 3 };
    a.n
}",
    },
    // Imported types as the elements of a tuple type.
    Row {
        name: "types_inside_a_tuple_type",
        items: "fn f() -> i32 {
    use self::m::{deep::Pair, Slice};
    let t: (Slice, Pair) = (Slice { n: 3 }, Pair(0, 0));
    t.0.n
}",
    },
    // A fn declared in the block sees the block's import in its own body.
    Row {
        name: "local_fn_body_sees_the_block_import",
        items: "fn f() -> i32 {
    use self::m::Slice;
    fn inner() -> i32 {
        let s: Slice = Slice { n: 3 };
        s.n
    }
    inner()
}",
    },
];

const MAIN: &str = "

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    f()
}
";

fn builder() -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"block use types").get::<CodeBuilder>()
}

/// The one crate's records and its one file's text, in the test's frame.
struct Kept {
    names: &'static [u8],
    items: &'static [u8],
    path: &'static str,
    text: &'static str,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.names)
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.items)
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        (krate == 0 && path.as_str() == self.path).then_some(self.text)
    }

    fn expansion(&self, _krate: usize, _call: LibKey) -> Option<LoadedExpansion<'static>> {
        None
    }
}

/// One program's names records: the solve's output, the solve itself gone.
struct Fixture {
    kept: &'static Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

/// `text` solved once, its one crate's two records written from the solve into `frame`.
fn fixture(frame: &'static Frame, a: Alloc<'static>, text: &str) -> Fixture {
    // `UserCrate` takes its files as std strings.
    let files = [(String::from(ROOT), String::from(text))];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "ladder", dir: DIR, root: ROOT, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let [krate] = crates.as_slice() else { panic!("{} crates (a no_core program is one)", crates.len()) };
    let keys: &'static [CrateItemKey] = frame.alloc_slice_copy(&[krate.key]);
    let name: &'static str = frame.alloc_str(krate.name);
    let crate_names: &'static [&'static str] = frame.alloc_slice_copy(&[name]);
    let names = inputs.resolve(&crates, a).unwrap_or_else(|e| panic!("the fixture's one solve: {e}"));
    let [c] = names.crates() else { panic!("{} solved crates (one wanted)", names.crates().len()) };
    let names_bytes = aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record: {e:?}")), a);
    let items_bytes = aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record: {e:?}")), a);
    let [file] = c.files.as_slice() else { panic!("{} files (one wanted)", c.files.len()) };
    let kept: &'static Kept = frame.alloc(Kept { names: names_bytes, items: items_bytes, path: frame.alloc_str(file.package.as_str()), text: frame.alloc_str(file.text) });
    Fixture { kept, keys, crate_names }
}

/// A FRESH reader of `f`'s records, as one `blaze run` process makes it, in `frame`.
fn reader(frame: &'static Frame, a: Alloc<'static>, f: &Fixture) -> NamesRecords<'static> {
    let log: &'static Log<'static> = frame.alloc(Log::new(f.crate_names, a));
    let records: &'static Records<'static> = frame.alloc(Records::new(f.keys, f.kept, log, frame, a));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// A stand-in for the command's BodyCheck keys that answers for every item (as
/// tests/local_statics.rs `EveryItem`).
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("block use types every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// The checker key of crate 0's one free fn named `name`.
fn fn_named(records: &'static Records<'static>, name: &str) -> ItemKey {
    let mut found = None;
    let mut count = 0usize;
    assert!(
        records.each_item(0, &mut |key, _| if records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
            found = Some(key);
            count += 1;
        }),
        "the program's items record lists its items"
    );
    assert_eq!(count, 1, "fns named `{name}` (one wanted)");
    checker_of(found.expect("one fn of the name"))
}

/// Each row's program walked from `main` over a fresh reader and a fresh `Maker`: every body
/// checked clean and nothing filed in the records' log.
#[test]
fn written_types_through_block_imports_check_over_records() {
    // The test's one allocator, made at its top: a frame, its memory from the OS. The record
    // walk takes `Alloc<'static>` (run/make_one.rs `walk_records`), so the frame is kept for
    // the process.
    let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    let a = erased.handle();
    let b = builder();
    let mut failed = 0usize;
    for row in ROWS {
        let mut text = AString::new_in(a);
        text.push_str(HEAD);
        text.push_str(row.items);
        text.push_str(MAIN);
        let f = fixture(frame, a, text.as_str());
        let main = fn_named(reader(frame, a, &f).records, "main");
        let names = reader(frame, a, &f);
        let mut store = MemoryStore::new_in(a);
        let mut maker = Maker::new(names, Some(main), &b, a);
        let walked = walk_records(main, &EveryItem, &mut store, &b, &producer(), &mut maker, a);
        let errors = names.records.errors();
        match walked {
            Ok((_, counts)) if errors.is_empty() => println!("block use types {}: clean ({counts})", row.name),
            Ok((_, counts)) => {
                failed += 1;
                println!("block use types {}: FAILED: the walk ({counts}) left {} errors in the records' log", row.name, errors.len());
                for e in errors.iter() {
                    println!("  logged: {e}");
                }
            }
            Err(e) => {
                failed += 1;
                println!("block use types {}: FAILED: the record walk: {e}", row.name);
                for logged in errors.iter() {
                    println!("  logged: {logged}");
                }
            }
        }
    }
    assert_eq!(failed, 0, "rows whose walk was not clean (each printed above with its refusal)");
}
