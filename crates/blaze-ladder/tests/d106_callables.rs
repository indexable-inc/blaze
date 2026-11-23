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

// (e) A fn item held by an instance's ARGUMENTS whose naming body is FOUND by its record.

/// `generic`'s BodyCheck key moved (its text edited, as tests/d106_records.rs `Edited` moves
/// one), every other item's [`EveryItem`]'s: a second walk finds `main`'s record and makes only
/// `generic`'s instance, in a fresh process that checks no body naming the fn item.
struct Rekeyed(ItemKey);

impl BodyIds for Rekeyed {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        if item != self.0 {
            let every: &'static EveryItem = &EveryItem;
            return every.body_check(item);
        }
        Some(BodyId { key: Hasher::new("d106 callables every item").part(&item.0.to_le_bytes()[..]).part(b"edited").finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        EveryItem.today(fact)
    }

    fn crate_fact(&self, item: ItemKey) -> Option<Read> {
        EveryItem.crate_fact(item)
    }
}

/// The stop's shape (tempo's tests t1530, 36 of them: "fn item ..: the build's `Adts` gave no
/// ABI for it" at `Result::map` with `NonZero::<u64>::new`, `Option::map` with `Into::into`,
/// fastrand's `try_with_rng(Rng::fork)`, a fn handed to `OnceLock::get_or_init`): the body that
/// passes a fn item as a value was found by its record, so no check of this process made the
/// item's head, and the build of the generic's instance asks the item's ABI (`Adts::abi`) as
/// it builds `f(x)` at `F` = the item's type. The Reference, "Function item types": the value
/// is zero-sized and its type "explicitly identifies the function", so the call is a call of
/// that fn with its own ABI. The make takes the item's row from its head
/// (run/make_one.rs `held_owners`, `FrontEnd::head_row`).
///
/// The control is the first walk of the same text: `main` made in the same process, its check
/// made the head, which passed before the fix too. The second walk is the cause: `main`
/// found, only `generic` made (before the fix, `NoAbi` there). Then run whole to `want`.
/// Prints "d106 callables <name>: first <counts>; caller found <counts>; status <n>".
fn row_with_caller_found(name: &str, tail: &str, generic: &str, want: u8) {
    let text = program(tail);
    let f = fixture(&text);
    let main = fn_named(&f, "main");
    let callee = fn_named(&f, generic);
    let mut store = MemoryStore::new_in(alloc());
    let (_, first) = walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut Maker::new(reader(&f), Some(main), &builder(), alloc()), alloc())
        .unwrap_or_else(|e| panic!("{name}: the control (main checked in this process) refused: {e}"));
    let (_, second) = walk_records(main, &Rekeyed(callee), &mut store, &builder(), &producer(), &mut Maker::new(reader(&f), Some(main), &builder(), alloc()), alloc())
        .unwrap_or_else(|e| panic!("{name}: the walk with main's record found and `{generic}` made refused: {e}"));
    assert_eq!(second.made, 1, "{name}: only `{generic}`'s instance is made the second time: {second}");
    assert!(second.found >= 1, "{name}: main's record is found the second time: {second}");
    let status = run_whole(&text).unwrap_or_else(|e| panic!("{name}: the whole run refused: {e}"));
    println!("d106 callables {name}: first {first}; caller found {second}; status {status}");
    assert_eq!(status, want, "{name}: the status computed through the call");
}

/// A free fn: `double` passed to `apply_mut`, made with `main` found: `double(21)` = 42.
#[test]
fn a_held_fn_item_made_with_its_caller_found() {
    row_with_caller_found(
        "free fn item, caller found",
        "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_mut(double, 21) as i32
}
",
        "apply_mut",
        42,
    );
}

/// An inherent impl's fn (fastrand's `try_with_rng(Rng::fork)`, `NonZero::new`): `S::twice`
/// passed to `apply_fn`: `twice(20)` = 40.
#[test]
fn a_held_impl_fn_item_made_with_its_caller_found() {
    row_with_caller_found(
        "impl fn item, caller found",
        "
struct S;

impl S {
    fn twice(x: u8) -> u8 {
        x * 2
    }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_fn(S::twice, 20) as i32
}
",
        "apply_fn",
        40,
    );
}

/// A trait's fn by its trait's path (`Option::map(Into::into)`): `<u8 as Triple>::triple`
/// passed to `apply_once`, resolved to the impl's at the build: `triple(13)` = 39.
#[test]
fn a_held_trait_fn_item_made_with_its_caller_found() {
    row_with_caller_found(
        "trait fn item, caller found",
        "
trait Triple {
    fn triple(x: u8) -> u8;
}

impl Triple for u8 {
    fn triple(x: u8) -> u8 {
        x * 3
    }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_once(<u8 as Triple>::triple, 13) as i32
}
",
        "apply_once",
        39,
    );
}

/// A generic fn item (`NonZero::<u64>::new`'s `<u64>`): `id::<u8>` passed to `apply_fn`:
/// `id(37)` = 37.
#[test]
fn a_held_generic_fn_item_made_with_its_caller_found() {
    row_with_caller_found(
        "generic fn item, caller found",
        "
fn id<T>(x: T) -> T {
    x
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_fn(id::<u8>, 37) as i32
}
",
        "apply_fn",
        37,
    );
}

/// A tuple struct's ctor (the Reference: "the constructor of a tuple-like struct" yields a
/// value of its fn item type too; its ABI is the Rust ABI): `W` passed to `apply_w`, made with
/// `main` found; its ADT's head is made from the ctor's key (`want_of_key`): `W(42).0` = 42.
#[test]
fn a_held_ctor_made_with_its_caller_found() {
    row_with_caller_found(
        "tuple struct ctor, caller found",
        "
struct W(u8);

fn apply_w<F: FnOnce(u8) -> W>(f: F, x: u8) -> u8 {
    f(x).0
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    apply_w(W, 42) as i32
}
",
        "apply_w",
        42,
    );
}

// (f) A trait implemented for primitives BY A MACRO, its projection and method asked at an
// instance's ground arguments (tempo t1530, 13 tests: arbitrary 1.4.2's `int_in_range_impl<T:
// Int>` at `u64` and `usize`, "the world has no answer to ImplsOf(<trait>, Uint(U64))"; its
// `impl_int!` writes `impl Int for $ty { type Unsigned = $unsigned_ty; .. }` once per integer,
// unstructured.rs:865). The Reference, "Macros By Example": a macro invocation in item
// position expands to items, so each `impl Int for u64` is an impl of the crate as if written
// there, and `<u64 as Int>::Unsigned` normalizes by it. A LEAD, not a proven cause: the twin
// writes the same impls by hand, the control calls the macro-made impl with no generic
// instance; which of the three fails says whether the macro or the instance-time question is
// the cause.

/// The trait and a generic that normalizes its projection at the instance (`T::Unsigned`).
const INT_TRAIT: &str = "
trait Int {
    type Unsigned;
    fn to_unsigned(self) -> Self::Unsigned;
}

fn spread<T: Int>(x: T) -> T::Unsigned {
    x.to_unsigned()
}
";

/// The site's shape: the impls written by `impl_int!`, `spread` at `u64` and `u8`: 40 + 2.
#[test]
fn a_macro_made_primitive_impl_at_an_instance() {
    row(
        "macro-made primitive impl at an instance",
        &format!(
            "{INT_TRAIT}
macro_rules! impl_int {{
    ( $( $ty:ty : $unsigned_ty:ty ; )* ) => {{
        $(
            impl Int for $ty {{
                type Unsigned = $unsigned_ty;

                fn to_unsigned(self) -> Self::Unsigned {{
                    self as $unsigned_ty
                }}
            }}
        )*
    }}
}}

impl_int! {{
    u8: u8;
    u64: u64;
}}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    spread(40u64) as i32 + spread(2u8) as i32
}}
"
        ),
        42,
    );
}

/// The twin: the same impls written by hand, `spread` at `u64` and `u8`: 40 + 2.
#[test]
fn a_written_primitive_impl_at_an_instance() {
    row(
        "written primitive impl at an instance",
        &format!(
            "{INT_TRAIT}
impl Int for u8 {{
    type Unsigned = u8;

    fn to_unsigned(self) -> Self::Unsigned {{
        self as u8
    }}
}}

impl Int for u64 {{
    type Unsigned = u64;

    fn to_unsigned(self) -> Self::Unsigned {{
        self as u64
    }}
}}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    spread(40u64) as i32 + spread(2u8) as i32
}}
"
        ),
        42,
    );
}

/// The control: the macro-made impl's fn called by its path, no generic instance asks the
/// impl: 40 + 2.
#[test]
fn a_macro_made_primitive_impl_called_by_path() {
    row(
        "macro-made primitive impl by path",
        &format!(
            "{INT_TRAIT}
macro_rules! impl_int {{
    ( $( $ty:ty : $unsigned_ty:ty ; )* ) => {{
        $(
            impl Int for $ty {{
                type Unsigned = $unsigned_ty;

                fn to_unsigned(self) -> Self::Unsigned {{
                    self as $unsigned_ty
                }}
            }}
        )*
    }}
}}

impl_int! {{
    u8: u8;
    u64: u64;
}}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    <u64 as Int>::to_unsigned(40) as i32 + <u8 as Int>::to_unsigned(2) as i32
}}
"
        ),
        42,
    );
}

/// Row 4: the REAL site's text (arbitrary 1.4.2 unstructured.rs:300-370 and :820-923), cut only
/// where `no_core` has no item for it (`ops::*` supertraits, `mem::size_of`, the `Iterator` the
/// bytes come through, inherent `<$ty>::wrapping_*`, `Result`). Rows 1-3 pass (19:08), so the
/// cause is in what the real text has and they lack; each kept below as written there: the
/// trait's supertrait (`Int: Copy`), its BOUNDED, self-recursive associated type (`type
/// Unsigned: Int;`), its associated CONSTS read through a two-level projection path
/// (`T::Unsigned::ZERO`, an item of `<<T as Int>::Unsigned as Int>`, found by the associated
/// type's bound, not by `T`'s), an associated fn by that path (`T::Unsigned::from_u8`), a
/// method on a value of the projection type (`arbitrary_int.wrapping_add(..)`), the `where T:
/// Int` form, the callee an associated fn of `impl<'a> Unstructured<'a>` reached as
/// `Self::int_in_range_impl` with an `impl Trait` argument, and the `usize: usize` row (the
/// site fails at `Uint(Usize)` as at `Uint(U64)`). `30 + 2` at `u64` and `8 + 2` at `usize`: 42.
#[test]
fn a_macro_made_primitive_impl_as_arbitrary_writes_it() {
    row(
        "macro-made primitive impl as arbitrary writes it",
        "
impl Copy for u64 {}
impl Copy for usize {}

trait Int: Copy {
    type Unsigned: Int;

    const ZERO: Self;

    const ONE: Self;

    fn from_u8(b: u8) -> Self;

    fn wrapping_add(self, rhs: Self) -> Self;

    fn wrapping_sub(self, rhs: Self) -> Self;

    fn to_unsigned(self) -> Self::Unsigned;

    fn from_unsigned(unsigned: Self::Unsigned) -> Self;
}

macro_rules! impl_int {
    ( $( $ty:ty : $unsigned_ty: ty ; )* ) => {
        $(
            impl Int for $ty {
                type Unsigned = $unsigned_ty;

                const ZERO: Self = 0;

                const ONE: Self = 1;

                fn from_u8(b: u8) -> Self {
                    b as Self
                }

                fn wrapping_add(self, rhs: Self) -> Self {
                    self + rhs
                }

                fn wrapping_sub(self, rhs: Self) -> Self {
                    self - rhs
                }

                fn to_unsigned(self) -> Self::Unsigned {
                    self as $unsigned_ty
                }

                fn from_unsigned(unsigned: $unsigned_ty) -> Self {
                    unsigned as Self
                }
            }
        )*
    }
}

impl_int! {
    u8: u8;
    u64: u64;
    usize: usize;
}

trait Bytes {
    fn next_byte(&mut self) -> u8;
}

struct One {
    byte: u8,
}

impl Bytes for One {
    fn next_byte(&mut self) -> u8 {
        self.byte
    }
}

struct Unstructured<'a> {
    data: &'a u8,
}

impl<'a> Unstructured<'a> {
    fn int_in_range<T>(&mut self, start: T, end: T) -> T
    where
        T: Int,
    {
        Self::int_in_range_impl(start, end, One { byte: *self.data })
    }

    fn int_in_range_impl<T>(start: T, end: T, mut bytes: impl Bytes) -> T
    where
        T: Int,
    {
        let start = start.to_unsigned();
        let end = end.to_unsigned();
        let delta = end.wrapping_sub(start);
        let mut arbitrary_int = T::Unsigned::ZERO;
        arbitrary_int = arbitrary_int.wrapping_add(T::Unsigned::from_u8(bytes.next_byte()));
        let offset = arbitrary_int.wrapping_add(delta.wrapping_sub(delta)).wrapping_add(T::Unsigned::ONE).wrapping_sub(T::Unsigned::ONE);
        let result = start.wrapping_add(offset);
        T::from_unsigned(result)
    }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let byte = 2u8;
    let mut u = Unstructured { data: &byte };
    u.int_in_range(30u64, 50u64) as i32 + u.int_in_range(8usize, 20usize) as i32
}
",
        42,
    );
}

/// The part of row 4 a bisecting row leaves out (rows 5..11): ONE each, the rest as row 4.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// `trait Int` with no supertrait (`Copy` gone).
    Supertrait,
    /// No item named through the projection path (`T::Unsigned::ZERO`, `::ONE`, `::from_u8`):
    /// the zero is `start.wrapping_sub(start)`, the byte `T::from_u8(..).to_unsigned()`.
    ProjectionPaths,
    /// No method call on a value of the projection type: each `a.wrapping_add(b)` there is
    /// `Int::wrapping_add(a, b)` (the trait's fn by path, its `Self` inferred).
    ProjectionMethods,
    /// The callee a FREE generic fn `int_in_range_impl<T: Int, B: Bytes>` called by name, no
    /// `Self::`, no `impl Trait` argument, no inherent impl with a lifetime.
    SelfAndImplTrait,
    /// The `usize` call gone (`30 + 2` at `u64`, plus 10 written as a literal).
    SecondPrimitive,
    /// Nothing asked at the projection: the body only `T::from_unsigned(start.to_unsigned()
    /// .wrapping_add(..))`-free: `T::from_unsigned(T::to_unsigned(start))` plus the byte through
    /// `T` (`T::from_u8`, `T::wrapping_add`). The bound `type Unsigned: Int;` kept.
    AllProjectionUse,
    /// As [`Cut::AllProjectionUse`], and the bound gone too (`type Unsigned;`).
    AllProjectionUseAndBound,
}

/// Row 4's text with the part `cut` left out (see [`Cut`]): the status stays 42.
fn arbitrary_cut(cut: Cut) -> String {
    let supertrait = if cut == Cut::Supertrait { "" } else { ": Copy" };
    let bound = if cut == Cut::AllProjectionUseAndBound { "" } else { ": Int" };
    let projection_free = matches!(cut, Cut::AllProjectionUse | Cut::AllProjectionUseAndBound);
    let body = if projection_free {
        "        let byte = T::from_u8(bytes.next_byte());
        let delta = T::wrapping_sub(end, start);
        let offset = T::wrapping_add(byte, T::wrapping_sub(delta, delta));
        T::from_unsigned(T::to_unsigned(T::wrapping_add(start, offset)))"
    } else if cut == Cut::ProjectionPaths {
        "        let start = start.to_unsigned();
        let end = end.to_unsigned();
        let delta = end.wrapping_sub(start);
        let mut arbitrary_int = start.wrapping_sub(start);
        arbitrary_int = arbitrary_int.wrapping_add(T::from_u8(bytes.next_byte()).to_unsigned());
        let offset = arbitrary_int.wrapping_add(delta.wrapping_sub(delta));
        let result = start.wrapping_add(offset);
        T::from_unsigned(result)"
    } else if cut == Cut::ProjectionMethods {
        "        let start = start.to_unsigned();
        let end = end.to_unsigned();
        let delta = Int::wrapping_sub(end, start);
        let mut arbitrary_int = T::Unsigned::ZERO;
        arbitrary_int = Int::wrapping_add(arbitrary_int, T::Unsigned::from_u8(bytes.next_byte()));
        let offset = Int::wrapping_sub(Int::wrapping_add(Int::wrapping_add(arbitrary_int, Int::wrapping_sub(delta, delta)), T::Unsigned::ONE), T::Unsigned::ONE);
        let result = Int::wrapping_add(start, offset);
        T::from_unsigned(result)"
    } else {
        "        let start = start.to_unsigned();
        let end = end.to_unsigned();
        let delta = end.wrapping_sub(start);
        let mut arbitrary_int = T::Unsigned::ZERO;
        arbitrary_int = arbitrary_int.wrapping_add(T::Unsigned::from_u8(bytes.next_byte()));
        let offset = arbitrary_int.wrapping_add(delta.wrapping_sub(delta)).wrapping_add(T::Unsigned::ONE).wrapping_sub(T::Unsigned::ONE);
        let result = start.wrapping_add(offset);
        T::from_unsigned(result)"
    };
    let callee = if cut == Cut::SelfAndImplTrait {
        format!(
            "impl<'a> Unstructured<'a> {{
    fn int_in_range<T>(&mut self, start: T, end: T) -> T
    where
        T: Int,
    {{
        int_in_range_impl(start, end, One {{ byte: *self.data }})
    }}
}}

fn int_in_range_impl<T: Int, B: Bytes>(start: T, end: T, mut bytes: B) -> T {{
{body}
}}"
        )
    } else {
        format!(
            "impl<'a> Unstructured<'a> {{
    fn int_in_range<T>(&mut self, start: T, end: T) -> T
    where
        T: Int,
    {{
        Self::int_in_range_impl(start, end, One {{ byte: *self.data }})
    }}

    fn int_in_range_impl<T>(start: T, end: T, mut bytes: impl Bytes) -> T
    where
        T: Int,
    {{
{body}
    }}
}}"
        )
    };
    let second = if cut == Cut::SecondPrimitive { "10" } else { "u.int_in_range(8usize, 20usize) as i32" };
    format!(
        "
impl Copy for u64 {{}}
impl Copy for usize {{}}

trait Int{supertrait} {{
    type Unsigned{bound};

    const ZERO: Self;

    const ONE: Self;

    fn from_u8(b: u8) -> Self;

    fn wrapping_add(self, rhs: Self) -> Self;

    fn wrapping_sub(self, rhs: Self) -> Self;

    fn to_unsigned(self) -> Self::Unsigned;

    fn from_unsigned(unsigned: Self::Unsigned) -> Self;
}}

macro_rules! impl_int {{
    ( $( $ty:ty : $unsigned_ty: ty ; )* ) => {{
        $(
            impl Int for $ty {{
                type Unsigned = $unsigned_ty;

                const ZERO: Self = 0;

                const ONE: Self = 1;

                fn from_u8(b: u8) -> Self {{
                    b as Self
                }}

                fn wrapping_add(self, rhs: Self) -> Self {{
                    self + rhs
                }}

                fn wrapping_sub(self, rhs: Self) -> Self {{
                    self - rhs
                }}

                fn to_unsigned(self) -> Self::Unsigned {{
                    self as $unsigned_ty
                }}

                fn from_unsigned(unsigned: $unsigned_ty) -> Self {{
                    unsigned as Self
                }}
            }}
        )*
    }}
}}

impl_int! {{
    u8: u8;
    u64: u64;
    usize: usize;
}}

trait Bytes {{
    fn next_byte(&mut self) -> u8;
}}

struct One {{
    byte: u8,
}}

impl Bytes for One {{
    fn next_byte(&mut self) -> u8 {{
        self.byte
    }}
}}

struct Unstructured<'a> {{
    data: &'a u8,
}}

{callee}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let byte = 2u8;
    let mut u = Unstructured {{ data: &byte }};
    u.int_in_range(30u64, 50u64) as i32 + {second}
}}
"
    )
}

/// Row 5: row 4 without the supertrait `Copy`.
#[test]
fn arbitrary_cut_supertrait() {
    row("arbitrary cut: supertrait", &arbitrary_cut(Cut::Supertrait), 42);
}

/// Row 6: row 4 with no item named through `T::Unsigned::..`.
#[test]
fn arbitrary_cut_projection_paths() {
    row("arbitrary cut: projection paths", &arbitrary_cut(Cut::ProjectionPaths), 42);
}

/// Row 7: row 4 with no method call on a value of the projection type.
#[test]
fn arbitrary_cut_projection_methods() {
    row("arbitrary cut: projection methods", &arbitrary_cut(Cut::ProjectionMethods), 42);
}

/// Row 8: row 4 with a free generic callee (no `Self::`, no `impl Trait` argument).
#[test]
fn arbitrary_cut_self_and_impl_trait() {
    row("arbitrary cut: Self and impl Trait", &arbitrary_cut(Cut::SelfAndImplTrait), 42);
}

/// Row 9: row 4 at `u64` only.
#[test]
fn arbitrary_cut_second_primitive() {
    row("arbitrary cut: second primitive", &arbitrary_cut(Cut::SecondPrimitive), 42);
}

/// Row 10: nothing asked at the projection (the bound kept).
#[test]
fn arbitrary_cut_all_projection_use() {
    row("arbitrary cut: all projection use", &arbitrary_cut(Cut::AllProjectionUse), 42);
}

/// Row 11: nothing asked at the projection and no bound on it.
#[test]
fn arbitrary_cut_all_projection_use_and_bound() {
    row("arbitrary cut: projection use and bound", &arbitrary_cut(Cut::AllProjectionUseAndBound), 42);
}

// (g) `Clone::clone` at a FN POINTER or a FN ITEM: the language's impl, no impl item (tempo
// t1800, 30 tests: proptest 1.x's `#[derive(Clone)] struct Config { .., failure_persistence:
// .., source_file: .., ..}` with a `fn(..)` field, "calls trait fn `core::clone::Clone::clone`
// .. with Self a fn pointer: .. no impl of trait .. holds"). The Reference, "Special types and
// traits": `Clone` and `Copy` are implemented for function item types and function pointer
// types; core's clone.rs `mod impls`: those "cannot be described in Rust" (no impl item). The
// record walk's maker had no rule for it (only run.rs `build_from` had, clone_shim.rs
// `Clones`): it now tables the instance at the trait's `clone` row and builds the copy
// (clone_shim.rs `language_clone`).

/// The lang `Clone` as core declares it (no default body), and a struct holding a fn pointer
/// whose `clone` clones the field as `#[derive(Clone)]` writes it.
const CLONE: &str = "
#[lang = \"clone\"]
trait Clone: Sized {
    fn clone(&self) -> Self;
}

struct Holder {
    f: fn(u8) -> u8,
}

impl Clone for Holder {
    fn clone(&self) -> Holder {
        Holder { f: Clone::clone(&self.f) }
    }
}
";

/// The site's shape: two holders of ONE pointer type to two fns, each cloned and called, so a
/// clone that made the pointer from its type (not its bytes) gives one status for both:
/// `triple(14) + double(1)` = 44.
#[test]
fn a_fn_pointer_field_cloned() {
    row(
        "fn pointer field cloned",
        &format!(
            "{CLONE}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let a = Holder {{ f: triple }};
    let b = Holder {{ f: double }};
    let c = Clone::clone(&a);
    let d = Clone::clone(&b);
    ((c.f)(14) + (d.f)(1)) as i32
}}
"
        ),
        44,
    );
}

/// The twin: `Clone::clone` at a FN ITEM (zero-sized), the clone called: `add3(40)` = 43.
#[test]
fn a_fn_item_cloned() {
    row(
        "fn item cloned",
        &format!(
            "{CLONE}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let g = Clone::clone(&add3);
    g(40) as i32
}}
"
        ),
        43,
    );
}

/// The control: the holder's pointer called with no clone (the field read as is):
/// `triple(14) + double(1)` = 44, so a wrong status above is the clone's.
#[test]
fn a_fn_pointer_field_called_uncloned() {
    row(
        "fn pointer field uncloned",
        &format!(
            "{CLONE}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let a = Holder {{ f: triple }};
    let b = Holder {{ f: double }};
    ((a.f)(14) + (b.f)(1)) as i32
}}
"
        ),
        44,
    );
}

// (h) `Clone::clone` at a TUPLE and at a CLOSURE: the language's impl, FIELD-WISE (tempo
// t1855d, 30 tests: proptest 1.11's derived `Clone for Config` cloning a tuple field, "calls
// trait fn `core::clone::Clone::clone` .. with Self a tuple: .. no impl of trait .. holds").
// The Reference, "Special types and traits", `Clone`: tuples "of types that implement `Clone`"
// and closures "that capture .. only values that implement `Clone`" implement it, by no impl
// item. clone_shim.rs `language_clone`: each field through its own `clone`, in order. `Count`'s
// `clone` adds one, so a clone that copied the bytes instead gives a smaller status.

/// A non-`Copy` field type whose `clone` is observable.
const COUNT: &str = "
struct Count(u8);

impl Clone for Count {
    fn clone(&self) -> Count {
        Count(self.0 + 1)
    }
}
";

/// The site's shape: a tuple of two `Count`s cloned by `Clone::clone`: `21 + 21` = 42.
#[test]
fn a_tuple_cloned_field_wise() {
    row(
        "tuple cloned field-wise",
        &format!(
            "{CLONE}{COUNT}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let t = (Count(20), Count(20));
    let c = Clone::clone(&t);
    (c.0.0 + c.1.0) as i32
}}
"
        ),
        42,
    );
}

/// The twin: a tuple inside a tuple (the inner one is a builtin clone called by the outer's)
/// and a closure capturing a `Count` by move: `(14 + 14) + 14` here, `41 + 1` below.
#[test]
fn a_nested_tuple_cloned_field_wise() {
    row(
        "nested tuple cloned field-wise",
        &format!(
            "{CLONE}{COUNT}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let t = ((Count(13), Count(13)), Count(13));
    let c = Clone::clone(&t);
    (c.0.0.0 + c.0.1.0 + c.1.0) as i32
}}
"
        ),
        42,
    );
}

/// The closure twin: a `move` closure holding a `Count`, cloned, then called: `41 + 1` = 42.
/// The closure names `a` whole (`read(&a)`), so it captures `a: Count` by value (the
/// Reference, "Closure expressions", capture modes: a `move` closure captures each place it
/// uses by value), and is `Clone` because `Count` is ("Closure types", `Clone`: when every
/// captured value is). The fixture before read `a.0`, which captures the place `a.0: u8`
/// alone; `u8: Clone` is core's impl, which this `no_core` text has none of, so the check's
/// E0277 (19:22 run) was Rust's answer, and with such an impl the clone would not have been
/// `Count`'s (`40 + 1`).
#[test]
fn a_closure_cloned_field_wise() {
    row(
        "closure cloned field-wise",
        &format!(
            "{CLONE}{COUNT}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let a = Count(40);
    let f = move || read(&a);
    let g = Clone::clone(&f);
    (call_none(g) + 1) as i32
}}

fn read(c: &Count) -> u8 {{
    c.0
}}

fn call_none<F: Fn() -> u8>(f: F) -> u8 {{
    f()
}}
"
        ),
        42,
    );
}

/// The control: the same tuple read with no clone: `21 + 21` = 42, so a wrong status above is
/// the clone's.
#[test]
fn a_tuple_read_uncloned() {
    row(
        "tuple read uncloned",
        &format!(
            "{CLONE}{COUNT}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let t = (Count(21), Count(21));
    (t.0.0 + t.1.0) as i32
}}
"
        ),
        42,
    );
}
