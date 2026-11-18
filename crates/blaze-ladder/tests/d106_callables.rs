//! Every BUILT-IN CALLABLE through the `Fn` family, on the path `blaze run` takes: the lazy
//! program over NAMES RECORDS that makes each fn on its first call (run/make_one.rs `Maker`,
//! walked as tests/d106_records.rs walks it), then built and run whole (`run_with`) for its
//! status. One `#![no_core]` program per row; each returns a status computed THROUGH the call,
//! so a wrong shim (an argument spread out of order, the wrong self, the wrong body) gives a
//! wrong status.
//!
//! The class (hello world on the real std, release g194537, 2026-11-17): "`<impl<T>
//! AtomicPtr<T>>::try_update` calls trait fn `core::ops::function::FnMut::call_mut` .. with
//! Self a fn item: .. no impl of trait .. holds at these arguments". A fn item passed as a
//! zero-sized VALUE is named by no body's callees, so the lazy program tabled no row for it,
//! fn_shim.rs `fn_shim_owner` found no index, and the call fell through to impl selection,
//! where the language's built-in `FnMut` for a fn item has no impl item. Each fn-item row
//! below passes the item as a value only (`apply_mut(double, 21)`), which is that shape.
//!
//! The rules (the Reference, "Function item types": every fn item implements `Fn`, `FnMut`,
//! `FnOnce`; "Function pointer types": so does every safe fn pointer of the Rust or C ABI;
//! "Closure types": a closure implements its kind's trait and every weaker one; core's
//! function.rs: `&F` implements the family when `F: Fn`, by LIBRARY impls).
//!
//! The helpers below are tests/d106_intrinsic_fallback.rs's (fixture, reader, builder, the
//! every-item stand-in), copied: a test file is its own crate and tests/d106_records.rs is
//! shared by many lanes.

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
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
/// Overflow wraps: a `no_core` program declares no `panic_const_*` item. A row whose
/// arguments are spread out of order wraps to a different status, never panics.
const OVERFLOW: OverflowChecks = OverflowChecks::Off;
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

/// The lang items (core's marker.rs, ops/unsize.rs, ops/function.rs), as the pinned library's,
/// and the callers every row shares: one per trait of the family, at arity 1.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, fn_traits, rustc_attrs)]
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

fn apply_mut<F: FnMut(u8) -> u8>(mut f: F, x: u8) -> u8 {
    f(x)
}

fn apply_fn<F: Fn(u8) -> u8>(f: F, x: u8) -> u8 {
    f(x)
}

fn apply_once<F: FnOnce(u8) -> u8>(f: F, x: u8) -> u8 {
    f(x)
}

fn double(x: u8) -> u8 {
    x * 2
}

fn add3(x: u8) -> u8 {
    x + 3
}

fn triple(x: u8) -> u8 {
    x * 3
}
";

/// `HEAD`, then `tail` (the row's items and its `main`).
fn program(tail: &str) -> String {
    format!("{HEAD}{tail}")
}

fn isa() -> Isa {
    Isa { lse: true, os: Os::MacOs }
}

fn builder() -> Builder {
    Builder {
        isa: isa(),
        tier: TierName::Reference,
        settings: Settings::baseline(OVERFLOW, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"d106 callables").get::<CodeBuilder>()
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

    fn expansion(&self, krate: usize, call: LibKey) -> Option<&'static str> {
        self.expansions.get(krate)?.get(&call).copied()
    }

    fn expansion_origins(&self, krate: usize, call: LibKey) -> Option<(&'static [u8], &'static [u8])> {
        let bytes = self.origins.get(krate)?.get(&call).copied()?;
        // The fixture keeps the joined form: its two parts, cut where the column starts (empty:
        // the text keeps none).
        if bytes.is_empty() {
            return Some((&[], &[]));
        }
        Some(blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}")))
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
    let names = inputs.resolve(&crates, a).unwrap_or_else(|e| panic!("the fixture's one solve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), a));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), a));
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
        Some(BodyId { key: Hasher::new("d106 callables every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// The walk of `text` over names records from `main`, every instance made: its counts, or the
/// refusal's text. This is the path of the stop: each fn made on its first call.
fn walk_text(text: &str) -> Result<String, String> {
    let f = fixture(text);
    let main = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let mut maker = Maker::new(reader(&f), Some(main), &builder(), alloc());
    walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut maker, alloc()).map(|(_, counts)| counts.to_string()).map_err(|e| e.to_string())
}

/// `text` built by the new front end and run whole: its status, or the refusal's text.
fn run_whole(text: &str) -> Result<u8, String> {
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared()).map_err(|e| e.to_string())
}

/// The row `name`: `HEAD` + `tail` walked over names records (every instance made, the stop's
/// path), then run whole to `want`. Prints "d106 callables <name>: walk <counts>; status <n>".
fn row(name: &str, tail: &str, want: u8) {
    let text = program(tail);
    let counts = walk_text(&text).unwrap_or_else(|e| panic!("{name}: the walk over names records refused: {e}"));
    let status = run_whole(&text).unwrap_or_else(|e| panic!("{name}: the whole run refused: {e}"));
    println!("d106 callables {name}: walk {counts}; status {status}");
    assert_eq!(status, want, "{name}: the status computed through the call");
}

// (a) A FN ITEM as each trait of the family: a direct call of the item, the tuple spread.

/// `double` passed as `impl FnMut(u8) -> u8`, the stop's shape (`try_update`'s `f(prev)`):
/// `double(21)` = 42.
#[test]
fn a_fn_item_as_fn_mut() {
    row(
        "fn item as FnMut",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_mut(double, 21) as i32
}
",
        42,
    );
}

/// `add3` passed as `impl Fn(u8) -> u8`: `add3(40)` = 43.
#[test]
fn a_fn_item_as_fn() {
    row(
        "fn item as Fn",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_fn(add3, 40) as i32
}
",
        43,
    );
}

/// `triple` passed as `impl FnOnce(u8) -> u8` (self by value, zero-sized): `triple(14)` = 42.
#[test]
fn a_fn_item_as_fn_once() {
    row(
        "fn item as FnOnce",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_once(triple, 14) as i32
}
",
        42,
    );
}

/// A GENERIC fn item `id::<u8>` as `impl Fn(u8) -> u8`: the shim keyed by the item AND its
/// arguments (`FnDef(id, [u8])`). `id(37)` = 37.
#[test]
fn a_generic_fn_item_as_fn() {
    row(
        "generic fn item as Fn",
        "
fn id<T>(x: T) -> T {
    x
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_fn(id::<u8>, 37) as i32
}
",
        37,
    );
}

/// Arity 2: `sub` as `impl Fn(u8, u8) -> u8`, the tuple `(50, 8)` spread IN ORDER: 42 (out of
/// order, `8 - 50` wraps to 214).
#[test]
fn a_fn_item_of_two_parameters() {
    row(
        "fn item of two parameters",
        "
fn sub(a: u8, b: u8) -> u8 {
    a - b
}

fn apply2<F: Fn(u8, u8) -> u8>(f: F) -> u8 {
    f(50, 8)
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply2(sub) as i32
}
",
        42,
    );
}

/// Arity 0: `seventeen` as `impl FnMut() -> u8`, the empty tuple spread to no parameter: 17.
#[test]
fn a_fn_item_of_zero_parameters() {
    row(
        "fn item of zero parameters",
        "
fn seventeen() -> u8 {
    17
}

fn apply0<F: FnMut() -> u8>(mut f: F) -> u8 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply0(seventeen) as i32
}
",
        17,
    );
}

/// A fn item returning a ZERO-SIZED value (`Z`) through `FnOnce`: the call's effect is the
/// status (`mark` writes 33 through the pointer it is given), its result a value of no bytes.
#[test]
fn a_fn_item_returning_a_zero_sized_value() {
    row(
        "fn item returning a zero-sized value",
        "
struct Z;

fn mark(p: *mut u8) -> Z {
    unsafe {
        *p = 33;
    }
    Z
}

fn apply_z<F: FnOnce(*mut u8) -> Z>(f: F, p: *mut u8) -> u8 {
    let _z: Z = f(p);
    1
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut v = 0u8;
    let one = apply_z(mark, &mut v as *mut u8);
    (v + one) as i32
}
",
        34,
    );
}

/// A callable passed TWO generic levels down (`outer` as `Fn`, then `inner` as `FnMut`): the
/// call inside `inner` is the stop's shape at one more remove. `double(20) + 1` = 41.
#[test]
fn a_fn_item_passed_two_generic_levels_down() {
    row(
        "fn item two generic levels down",
        "
fn inner<G: FnMut(u8) -> u8>(mut g: G, x: u8) -> u8 {
    g(x)
}

fn outer<F: Fn(u8) -> u8>(f: F, x: u8) -> u8 {
    inner(f, x) + 1
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    outer(double, 20) as i32
}
",
        41,
    );
}

/// `&dyn Fn(u8) -> u8` made from a fn item: the vtable's `call` word is the fn item's shim
/// (run/make_one.rs `vtable_rows`). `double(19)` = 38.
#[test]
fn a_fn_item_as_a_dyn_fn() {
    row(
        "fn item as &dyn Fn",
        "
fn through(f: &dyn Fn(u8) -> u8, x: u8) -> u8 {
    f(x)
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let f = double;
    through(&f, 19) as i32
}
",
        38,
    );
}

// (b) A FN POINTER as each trait of the family: an indirect call through the pointer's value.

/// `fn(u8) -> u8` as `impl FnMut`: the pointer loaded through `&mut self`. `double(21)` = 42.
#[test]
fn a_fn_pointer_as_fn_mut() {
    row(
        "fn pointer as FnMut",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let p: fn(u8) -> u8 = double;
    apply_mut(p, 21) as i32
}
",
        42,
    );
}

/// `fn(u8) -> u8` as `impl Fn`: the pointer loaded through `&self`. `add3(40)` = 43.
#[test]
fn a_fn_pointer_as_fn() {
    row(
        "fn pointer as Fn",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let p: fn(u8) -> u8 = add3;
    apply_fn(p, 40) as i32
}
",
        43,
    );
}

/// `fn(u8) -> u8` as `impl FnOnce`: the pointer is `self` itself. Two pointers of ONE type to
/// two fns, so a shim that called a fn by the pointer's TYPE would give one status for both:
/// `triple(14) + double(1)` = 44.
#[test]
fn a_fn_pointer_as_fn_once() {
    row(
        "fn pointer as FnOnce",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let p: fn(u8) -> u8 = triple;
    let q: fn(u8) -> u8 = double;
    (apply_once(p, 14) + apply_once(q, 1)) as i32
}
",
        44,
    );
}

// (c) A CLOSURE through a trait of a LOWER kind than its own.

/// An `Fn` closure (it only reads its copied capture) called as `FnMut`: `30 + 5` = 35.
#[test]
fn an_fn_closure_as_fn_mut() {
    row(
        "Fn closure as FnMut",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let k = 5u8;
    let c = move |x: u8| x + k;
    apply_mut(c, 30) as i32
}
",
        35,
    );
}

/// An `Fn` closure called as `FnOnce` (self by value; the body takes it by reference):
/// `31 + 5` = 36.
#[test]
fn an_fn_closure_as_fn_once() {
    row(
        "Fn closure as FnOnce",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let k = 5u8;
    let c = move |x: u8| x + k;
    apply_once(c, 31) as i32
}
",
        36,
    );
}

/// An `FnMut` closure (it mutates the captured counter `n`) called as `FnOnce`: the call
/// returns `1 + 9` = 10 and leaves `n` at 10, so `10 * 4 + 10` = 50 (a body run on a copy of
/// the counter leaves `n` at 1: 41).
#[test]
fn an_fn_mut_closure_as_fn_once() {
    row(
        "FnMut closure as FnOnce",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut n = 1u8;
    let c = |x: u8| {
        n = n + x;
        n
    };
    let r = apply_once(c, 9);
    (r * 4 + n) as i32
}
",
        50,
    );
}

// (d) `&F`: a LIBRARY impl (core's function.rs `impl<A: Tuple, F: ?Sized> Fn<A> for &F where
// F: Fn<A>`, and its `FnMut` and `FnOnce`), selected as any impl is; its body calls `F`'s
// `call`, here a fn item's shim.

/// `&double` as `impl Fn(u8) -> u8` through core's `&F` impls, declared here as core declares
/// them: `double(20)` = 40.
#[test]
fn a_reference_to_a_fn_item_through_the_library_impl() {
    row(
        "&fn item through the library impl",
        "
impl<A: Tuple, F: ?Sized> Fn<A> for &F
where
    F: Fn<A>,
{
    extern \"rust-call\" fn call(&self, args: A) -> F::Output {
        (**self).call(args)
    }
}

impl<A: Tuple, F: ?Sized> FnMut<A> for &F
where
    F: Fn<A>,
{
    extern \"rust-call\" fn call_mut(&mut self, args: A) -> F::Output {
        (**self).call(args)
    }
}

impl<A: Tuple, F: ?Sized> FnOnce<A> for &F
where
    F: Fn<A>,
{
    type Output = F::Output;

    extern \"rust-call\" fn call_once(self, args: A) -> F::Output {
        (*self).call(args)
    }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let f = double;
    apply_fn(&f, 20) as i32
}
",
        40,
    );
}
