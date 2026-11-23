//! Every shape of a CONSTANT'S VALUE HOLDING A FN POINTER, on the path `blaze run` takes: the
//! lazy program over NAMES RECORDS that makes each fn on its first call (run/make_one.rs
//! `Maker`, walked as tests/d106_records.rs walks it), then built and run whole (`run_with`) for
//! its status. One `#![no_core]` program per row; each returns a status computed THROUGH the
//! pointer, so a pointer to the wrong instance gives a wrong status.
//!
//! The gap (tests/foreign_abi_everywhere.rs, 2026-11-17): "the record walk: expression N: a
//! constant's value holds a fn pointer to instance InstanceKey(..), which the build's `Adts`
//! does not give". A constant's record held each fn word as `DataTarget::Fn(key)`, a hash with no
//! inverse, and run.rs `FrontAdts::const_fn_instance` searched the program's NON-GENERIC fns
//! whose body was already CHECKED for one keying equal: on the lazy path a fn named only in the
//! constant's own initializer was never checked by the maker, and a generic instance or a
//! closure's `call_once` was never a candidate on either path. Now the evaluator's record names
//! the instance of each of its fn words (blaze-consteval `ConstRecord::fns`) and the front end
//! answers from it (front2 consts.rs `Consts::fn_instance`).
//!
//! The helpers are tests/d106_callables.rs's (fixture, reader, builder, the every-item
//! stand-in), copied: a test file is its own crate. Its `HEAD` too: the rows with a closure
//! coerced to a fn pointer need the `Fn` family declared, as any real program has it (core).

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Counts, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/ladder/";
const ROOT: &str = "/ladder/src/main.rs";
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

/// The lang items (core's marker.rs, ops/unsize.rs, ops/function.rs, as tests/d106_callables.rs
/// declares them) and the fns the rows point to.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, fn_traits, rustc_attrs, thread_local, intrinsics)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"legacy_receiver\"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}
impl<T: PointeeSized> LegacyReceiver for &mut T {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}
impl Copy for u8 {}

#[lang = \"unsize\"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = \"coerce_unsized\"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a mut U> for &'a mut T {}

#[lang = \"dispatch_from_dyn\"]
trait DispatchFromDyn<T> {}

impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a U> for &'a T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a mut U> for &'a mut T {}

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

fn seven() -> u8 {
    7
}

fn double(x: u8) -> u8 {
    x * 2
}

fn add3(x: u8) -> u8 {
    x + 3
}

fn id<T>(x: T) -> T {
    x
}
";

/// `HEAD`, then `tail` (the row's items and its `main`).
fn program(tail: &str) -> String {
    format!("{HEAD}{tail}")
}

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
    Producers::for_tests(b"const fn pointers").get::<CodeBuilder>()
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve (tests/d106_records.rs `Kept`).
#[derive(Clone, Default)]
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<LibKey, &'static str>>,
    origins: Vec<HashMap<LibKey, &'static [u8]>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        // The fixture keeps the origins in the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none). None kept for the call: none served.
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn made_origins(call: LibKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

/// One program's names records: the solve's output, the solve itself gone.
struct Fixture {
    kept: Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

/// `text` solved once, each crate's two records written from the solve.
fn fixture(text: &str) -> Fixture {
    let files = [(String::from(ROOT), String::from(text))];
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "ladder", dir: DIR, root: ROOT, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("the fixture's one solve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    Fixture { kept, keys, crate_names }
}

/// A FRESH reader of `f`'s records, as one `blaze run` process makes it.
fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

fn logged(records: &Records<'_>) -> Vec<String> {
    records.errors().iter().map(|e| e.to_string()).collect()
}

/// The checker key of crate 0's fn named `name`.
fn fn_named(f: &Fixture, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(records.each_item(0, &mut |key, _| if found.is_none() && records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
        found = Some(checker_of(key));
    }), "the program's items record lists its items");
    found.unwrap_or_else(|| panic!("no fn named `{name}`: {:?}", logged(records)))
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// The command's BodyCheck keys, answered for EVERY item (tests/d106_records.rs `EveryItem`).
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("const fn pointers every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// One walk of `f`'s program from `main` into `store`, over a FRESH reader and a FRESH `Maker`
/// (one `blaze run` process): its counts, or the refusal with the records' log beside it.
fn walk_into(f: &Fixture, store: &mut MemoryStore) -> Result<Counts, String> {
    let main = fn_named(f, "main");
    let names = reader(f);
    let b = builder();
    let mut maker = Maker::new(names, Some(main), &b, alloc());
    match walk_records(main, &EveryItem, store, &b, &producer(), &mut maker, alloc()) {
        Ok((_, counts)) if logged(names.records).is_empty() => Ok(counts),
        Ok((_, counts)) => Err(format!("the walk ({counts}) left the records' log {:?}", logged(names.records))),
        Err(e) => Err(format!("the record walk: {e}; the records filed {:?}", logged(names.records))),
    }
}

/// `text` built by the new front end and run whole: its status, or the refusal's text.
fn run_whole(text: &str) -> Result<u8, String> {
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };
    blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared()).map_err(|e| e.to_string())
}

/// The row `name`: `HEAD` + `tail` walked over names records (every instance made, the gap's
/// path), then run whole to `want`. Prints "const fn pointers <name>: walk <counts>; status <n>".
fn row(name: &str, tail: &str, want: u8) {
    let text = program(tail);
    let f = fixture(&text);
    let mut store = MemoryStore::new_in(alloc());
    let counts = walk_into(&f, &mut store).unwrap_or_else(|e| panic!("{name}: {e}"));
    let status = run_whole(&text).unwrap_or_else(|e| panic!("{name}: the whole run refused: {e}"));
    println!("const fn pointers {name}: walk {counts}; status {status}");
    assert_eq!(status, want, "{name}: the status computed through the pointer");
}

/// A constant item holding a non-generic fn item's pointer: `seven()` = 7.
#[test]
fn a_const_item_holding_a_fn_pointer() {
    row(
        "const item",
        "
const F: fn() -> u8 = seven;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    F() as i32
}
",
        7,
    );
}

/// A GENERIC fn's instance: the record names `id` at `[u8]`, which no search of the
/// program's non-generic fns found. `id(37)` = 37.
#[test]
fn a_const_item_holding_a_generic_instance() {
    row(
        "generic instance",
        "
const G: fn(u8) -> u8 = id::<u8>;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    G(37) as i32
}
",
        37,
    );
}

/// Two fn words in one constant's allocation, a tuple and an array, each called: `double(seven())`
/// = 14, `add3(double(20))` = 43, 57 in all. The array is destructured, so no bounds check (no
/// `panic_bounds_check` in a `no_core` program).
#[test]
fn a_const_tuple_and_array_of_fn_pointers() {
    row(
        "tuple and array",
        "
const P: (fn() -> u8, fn(u8) -> u8) = (seven, double);
const A: [fn(u8) -> u8; 2] = [double, add3];

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let [first, second] = A;
    ((P.1)((P.0)()) + second(first(20))) as i32
}
",
        57,
    );
}

/// A STATIC holding a fn pointer: `seven()` = 7.
#[test]
fn a_static_holding_a_fn_pointer() {
    row(
        "static",
        "
static T: fn() -> u8 = seven;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    T() as i32
}
",
        7,
    );
}

#[test]
fn a_static_points_to_a_ground_generic_instance() {
    row(
        "generic static",
        "
#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn size_of<T>() -> usize;

fn width<T>() -> usize { size_of::<T>() }
static BYTE: fn() -> usize = width::<u8>;
static WORD: fn() -> usize = width::<u32>;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    (BYTE() * 10 + WORD()) as i32
}
",
        14,
    );
}

#[test]
fn a_thread_local_holding_a_fn_pointer() {
    row(
        "thread-local fn word",
        r#"
unsafe extern "C" fn destroy(value: *mut u8) {
    unsafe { *value = 42; }
}

#[thread_local]
static DTOR: unsafe extern "C" fn(*mut u8) = destroy;

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    let mut value = 0u8;
    unsafe { DTOR(&raw mut value); }
    value as i32
}
"#,
        42,
    );
}

/// A const block in a generic fn, evaluated at each instance's arguments (`const_at`): the
/// record of each names `id` at that instance's `T`. `pick::<u8>(5) + pick::<u32>(30)` = 35.
#[test]
fn a_const_block_in_a_generic_fn_at_two_instances() {
    row(
        "const block at two instances",
        "
fn pick<T>(x: T) -> T {
    let f: fn(T) -> T = const { id::<T> as fn(T) -> T };
    f(x)
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    pick::<u8>(5) as i32 + pick::<u32>(30) as i32
}
",
        35,
    );
}

/// A PROMOTED borrow of a fn pointer (`&'static`): its record names `seven`. 7.
#[test]
fn a_promoted_fn_pointer() {
    row(
        "promoted",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let r: &'static fn() -> u8 = &(seven as fn() -> u8);
    (*r)() as i32
}
",
        7,
    );
}

/// eyre's `&ErrorVTable { object_drop: object_drop::<E>, .. }` (tempo tests t0850, 3 NOT RUN:
/// "fn .. in a promoted value has no instance key in the build's `Adts`"): a PROMOTED struct of
/// fn pointers in a GENERIC fn, at two instances, its fn named only there (no body calls
/// `id`, so the record walk has no row for it until the build names it: run/make_one.rs
/// `Step::Table`). Each word is read back to its instance from the evaluator's record
/// (`const_fn_instance`), so `id::<u8>` and `id::<u32>` are two: a word read at the other
/// instance gives a wrong status. 5 + 30 = 35.
#[test]
fn a_promoted_fn_table_in_a_generic_fn_at_two_instances() {
    row(
        "promoted table at two instances",
        "
struct Ops<T> {
    f: fn(T) -> T,
}

fn ops<T: 'static>() -> &'static Ops<T> {
    &Ops { f: id::<T> }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    (ops::<u8>().f)(5) as i32 + (ops::<u32>().f)(30) as i32
}
",
        35,
    );
}

/// The twin, not generic: a promoted struct literal whose field is a plain fn item coerced
/// (not an `as` cast), in a fn other than `main`, and named nowhere else. `double(21)` = 42.
#[test]
fn a_promoted_fn_table_of_a_plain_fn() {
    row(
        "promoted table of a plain fn",
        "
struct Plain {
    f: fn(u8) -> u8,
}

fn plain() -> &'static Plain {
    &Plain { f: double }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    (plain().f)(21) as i32
}
",
        42,
    );
}

/// A non-capturing CLOSURE coerced to a fn pointer in a constant item: the record names its
/// `call_once` at `[{closure}, (u8,)]`, the instance blaze-build's `closure_fn_addr` takes for
/// the same coercion in a body. The closure's owner is the constant `C` (reach/initializers.rs,
/// the body-owner rule): a row of the program like a fn's. `41 + 1` = 42.
#[test]
fn a_closure_coerced_in_a_const_item() {
    row(
        "closure in a const item",
        "
const C: fn(u8) -> u8 = |x| x + 1;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    C(41) as i32
}
",
        42,
    );
}

/// A struct constant with a fn-pointer field, called through a method taking `&self`:
/// `double(21)` = 42.
#[test]
fn a_struct_const_with_a_fn_pointer_field() {
    row(
        "struct field through a method",
        "
struct Ops {
    f: fn(u8) -> u8,
}

impl Ops {
    fn run(&self, x: u8) -> u8 {
        (self.f)(x)
    }
}

const OPS: Ops = Ops { f: double };

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    OPS.run(21) as i32
}
",
        42,
    );
}

/// std's `thread_local!` shape (the coordinator's shape 1, from lane RF4's local statics): a
/// constant struct whose field is a closure, holding a local static, coerced to a fn pointer.
/// `*V` = 3.
const THREAD_LOCAL: &str = "
struct Key {
    get: fn() -> &'static u8,
}

const KEY: Key = Key {
    get: || {
        static V: u8 = 3;
        &V
    },
};

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    *(KEY.get)() as i32
}
";

#[test]
fn the_thread_local_shape() {
    row("thread-local shape", THREAD_LOCAL, 3);
}

/// A closure in a STATIC initializer, coerced to a fn pointer: its owner is the static `S` (the
/// body-owner rule). `40 + 2` = 42.
#[test]
fn a_closure_in_a_static_initializer() {
    row(
        "closure in a static",
        "
static S: fn(u8) -> u8 = |x| x + 2;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    S(40) as i32
}
",
        42,
    );
}

/// A closure as an anonymous const's value, called at once: `const { || 9 }` is part of
/// `main`'s body, so `main` owns the closure (the body-owner rule). 9.
#[test]
fn a_closure_in_an_anonymous_const_called_at_once() {
    row(
        "closure in an anonymous const",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    (const { || 9u8 })() as i32
}
",
        9,
    );
}

/// A closure in an ASSOCIATED constant of an impl, coerced to a fn pointer: its owner is the
/// impl's constant `Ops::F` (the body-owner rule; its row's target is `LowerTarget::Member`).
/// `double(5) + 1` = 11.
#[test]
fn a_closure_in_an_associated_const() {
    row(
        "closure in an associated const",
        "
struct Ops;

impl Ops {
    const F: fn(u8) -> u8 = |x| double(x) + 1;
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    Ops::F(5) as i32
}
",
        11,
    );
}

/// A non-capturing closure coerced to a fn in a constant, called through the constant TWICE:
/// one instance (the closure's `call_once` at `(u8,)`), made by the first walk and found by key
/// on the second. `C(1) + C(2)` = `11 + 12` = 23.
#[test]
fn a_const_closure_called_twice_is_one_instance() {
    twice(
        "const closure called twice",
        "
const C: fn(u8) -> u8 = |x| x + 10;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    (C(1) + C(2)) as i32
}
",
        23,
    );
}

/// The coordinator's shape 2: a `const { .. }` block whose value is a closure coerced to a fn
/// pointer inside the block, in a generic fn evaluated at its instance's arguments.
/// `key::<u8>()` = `3 + 4` = 7, `key::<u32>()` the same: 14.
#[test]
fn a_closure_coerced_in_a_const_block() {
    row(
        "closure in a const block",
        "
fn key<T>() -> u8 {
    let f: fn(u8) -> u8 = const {
        let f: fn(u8) -> u8 = |x: u8| {
            x + 4
        };
        f
    };
    f(3)
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    (key::<u8>() + key::<u32>()) as i32
}
",
        14,
    );
}

/// A SECOND RUN over one store: a fresh process (reader, maker) walks again and makes NO
/// record, every instance found by key, and the pointer still resolves: `main`'s record
/// declares the instance the constant points to (run/make_one.rs `name_refs` over
/// `Built::refs`), so no later process asks the constant again. The thread-local shape too,
/// whose instance is a closure's. Prints "const fn pointers second run <name>: first <counts>;
/// second <counts>".
fn twice(name: &str, tail: &str, want: u8) {
    let text = program(tail);
    let f = fixture(&text);
    let mut store = MemoryStore::new_in(alloc());
    let first = walk_into(&f, &mut store).unwrap_or_else(|e| panic!("{name}, first walk: {e}"));
    assert!(first.reached > 1 && first.made == first.reached && first.found == 0, "{name}: the first walk makes every instance, the pointer's too: {first}");
    let second = walk_into(&f, &mut store).unwrap_or_else(|e| panic!("{name}, second walk: {e}"));
    assert_eq!((second.reached, second.found, second.made, second.make_calls()), (first.reached, first.reached, 0, 0), "{name}: the second walk finds every instance: {second}");
    assert_eq!(run_whole(&text), Ok(want), "{name}: the status through the pointer");
    println!("const fn pointers second run {name}: first {first}; second {second}");
}

#[test]
fn a_second_run_makes_nothing_and_still_resolves() {
    twice(
        "const item",
        "
const F: fn() -> u8 = seven;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    F() as i32
}
",
        7,
    );
    twice("thread-local shape", THREAD_LOCAL, 3);
}
