//! A `#[track_caller]` `Location`'s line is placed, not built (rule 71): the code record of the
//! caller holds the call's item and its byte in the item's text, and placement fills the line
//! and the column from where the item is today (blaze-emit `PositionFix`, place.rs
//! `resolve_positions`, `FrontEnd::source_position`).
//!
//! The program is `#![no_core]` (no toolchain): a tracked `here()` returns its caller's line
//! through the `caller_location` intrinsic, and `main` returns what `here()` gives. Called from
//! line N of the file, the run's status is N; with two blank lines inserted above `main`, it
//! is N + 2, and `main`'s code record (the id placement binds it under and its bytes) is the
//! same: an edit above an item moves no record.
//!
//! It passes once `FrontAdts::track_caller` (run.rs) answers from the front end; while that is
//! held at `None` the build refuses `here()`'s own `caller_location` ("not said").

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, build_new, run_with};

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[lang = \"panic_location\"]
struct Location<'a> {
    file: &'a str,
    line: u32,
    col: u32,
}

#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;

#[track_caller]
fn here() -> u32 {
    caller_location().line
}
";

const MAIN: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    here() as i32
}
";

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The line (from 1) of `here()`'s call in `text`.
fn call_line(text: &str) -> u8 {
    let n = text.lines().position(|l| l.contains("here() as i32")).expect("the call") + 1;
    u8::try_from(n).expect("the call is on a line below 256")
}

/// `main`'s status and its code record (slot 0, the entry's) when the file is `text`.
fn run_and_record(text: &str) -> (u8, blaze_launch::wire::RecordId, Vec<u8>) {
    let files = [SourceFile { path: "src/main.rs", text }];
    let status = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("TRACK CALLER: refused: {e}"));
    let built = build_new(&files, "src/main.rs", NEW, blaze_own::testing::heap().shared().local())
        .unwrap_or_else(|e| panic!("TRACK CALLER: not built: {e}"));
    let (id, bytes) = built.code_record(0).expect("the entry's slot");
    (status, id, bytes.to_vec())
}

/// Called from line N, `here()` gives N; two blank lines above `main` give N + 2, and `main`'s
/// record id and bytes are equal across the edit. A builder that wrote the line into the code
/// fails the record assert; a placer that did not fill it fails the status assert (0).
///
/// Prints: `TRACK CALLER: line <N> then <N+2>; main's record equal across the edit`
#[test]
fn a_tracked_callers_line_is_placed_and_its_record_does_not_move() {
    let before = format!("{HEAD}{MAIN}");
    let after = format!("{HEAD}\n\n{MAIN}");
    let n = call_line(&before);
    assert_eq!(call_line(&after), n + 2);
    let (status, id, bytes) = run_and_record(&before);
    assert_eq!(status, n, "the status is the call's line");
    let (status2, id2, bytes2) = run_and_record(&after);
    assert_eq!(status2, n + 2, "two lines above `main` move the line by two");
    assert!(id == id2 && bytes == bytes2, "`main`'s code record moved with a line inserted above it");
    println!("TRACK CALLER: line {n} then {}; main's record equal across the edit", n + 2);
}

/// 1-based line and 1-based column of the first occurrence of `needle` in `text`.
fn line_col_of(text: &str, needle: &str) -> (u32, u32) {
    for (idx, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (idx as u32 + 1, col as u32 + 1);
        }
    }
    panic!("needle `{needle}` not found in fixture");
}

/// Runs `text` through the whole-program builder/JIT and returns its exit code.
fn run_status(text: &str) -> u8 {
    let files = [SourceFile { path: "src/main.rs", text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("TRACK CALLER REIFY: refused: {e}"))
}

/// Walks `text` through the lazy record-walk pipeline (`Maker::new` + `walk_records`), verifying
/// both initial compilation and cached verification on a second walk, and asserting that
/// `BodyIds::crate_fact` is NEVER queried for `REIFY_SHIM_ITEM`.
fn walk_lazy_twice(text: &str) {
    use blaze_emit::{Isa, Os};
    use blaze_id::{CrateItemKey, ItemKey as LibKey};
    use blaze_ids::Hasher;
    use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
    use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
    use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
    use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
    use blaze_ladder::front2::keys::checker_of;
    use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
    use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
    use blaze_scope::package_path::PackagePath;
    use blaze_scope::resolve::aligned;
    use blaze_scope::{items_record, names_record};
    use blaze_surface::ItemKind;
    use blaze_toolchain::ToolchainId;
    use blaze_typeck::ItemKey;

    struct SingleCrate<'a> {
        names: &'a [u8],
        items: &'a [u8],
        text: &'a str,
    }

    impl<'a> Load<'a> for SingleCrate<'a> {
        fn names(&self, krate: usize) -> Option<&'a [u8]> {
            (krate == 0).then_some(self.names)
        }
        fn items(&self, krate: usize) -> Option<&'a [u8]> {
            (krate == 0).then_some(self.items)
        }
        fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
            (krate == 0 && path.as_str() == "src/main.rs").then_some(self.text)
        }
        fn expansion(&self, _krate: usize, _call: LibKey) -> Option<LoadedExpansion<'a>> {
            None
        }
    }

    const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];
    struct StrictBodyIds;
    impl BodyIds for StrictBodyIds {
        fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
            Some(BodyId { key: Hasher::new("track caller reify body").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
        }
        fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
            (*fact == READS[0].fact).then_some(READS[0].digest)
        }
        fn crate_fact(&self, item: ItemKey) -> Option<Read> {
            // `REIFY_SHIM_ITEM` is synthetic and must never be queried as a crate item.
            if item == blaze_build::REIFY_SHIM_ITEM {
                return None;
            }
            Some(Read { fact: [8; 32], digest: [9; 32] })
        }
    }

    let alloc = blaze_own::testing::heap().alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 edition");
    let files = [("/ladder/src/main.rs".into(), text.into())];
    let user = UserCrate { name: "ladder", dir: "/ladder/", root: "/ladder/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let tc = Toolchain::none(alloc);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let c = &names.crates()[0];
    let names_bytes = aligned(&names_record::encode(c, alloc).expect("names record"), blaze_own::testing::bump());
    let items_bytes = aligned(&items_record::encode_with(c, &names, alloc).expect("items record"), blaze_own::testing::bump());
    let keys_slice: [CrateItemKey; 1] = [c.key];
    let crate_names: [&str; 1] = ["ladder"];
    let lang_crates: [usize; 1] = [0];
    let producer: ProducerId<CodeBuilder> = Producers::for_tests(b"track caller reify").get::<CodeBuilder>();
    let builder = Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(blaze_build::OverflowChecks::Off, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer,
        edition,
    };
    let mut store = MemoryStore::new_in(alloc);

    // First walk: compiles everything including ReifyShims on demand.
    let frame1 = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame1");
    let log1 = frame1.alloc(Log::new(&crate_names, alloc));
    let load1 = frame1.alloc(SingleCrate { names: names_bytes, items: items_bytes, text });
    let rec1 = frame1.alloc(Records::new(&keys_slice, load1, log1, &frame1, alloc));
    let mut main_key = None;
    assert!(rec1.each_item(0, &mut |key, _| if main_key.is_none() && rec1.kind_of_item(key) == Some(ItemKind::Function) && rec1.name_of(key) == Some("main") {
        main_key = Some(checker_of(key));
    }));
    let main = main_key.expect("main fn");
    let nr1 = NamesRecords { records: rec1, frame: &frame1, lang_crates: &lang_crates, body_sources: None };
    let mut maker1 = Maker::new(nr1, Some(main), &builder, alloc);
    let (_, counts1) = walk_records(main, &StrictBodyIds, &mut store, &builder, &producer, &mut maker1, alloc)
        .unwrap_or_else(|e| panic!("first lazy walk refused: {e}"));
    assert!(counts1.made > 0, "first walk must build entries");

    // Second walk over the same store: every entry (including ReifyShims) is found and verified.
    let frame2 = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame2");
    let log2 = frame2.alloc(Log::new(&crate_names, alloc));
    let load2 = frame2.alloc(SingleCrate { names: names_bytes, items: items_bytes, text });
    let rec2 = frame2.alloc(Records::new(&keys_slice, load2, log2, &frame2, alloc));
    let nr2 = NamesRecords { records: rec2, frame: &frame2, lang_crates: &lang_crates, body_sources: None };
    let mut maker2 = Maker::new(nr2, Some(main), &builder, alloc);
    let (_, counts2) = walk_records(main, &StrictBodyIds, &mut store, &builder, &producer, &mut maker2, alloc)
        .unwrap_or_else(|e| panic!("second lazy walk refused: {e}"));
    assert_eq!(counts2.made, 0, "second walk should reuse all cached entries including ReifyShims: {counts2}");
}

const REIFY_PRELUDE: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, fn_traits)]
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

impl Copy for bool {}
impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for i32 {}
impl Copy for usize {}
impl<T: PointeeSized> Copy for &T {}

#[lang = \"fn_once\"]
trait FnOnce<Args> {
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
trait FnMut<Args>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
trait Fn<Args>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

#[lang = \"panic_location\"]
struct Location<'a> {
    file: &'a str,
    line: u32,
    col: u32,
}

#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;

fn str_eq(a: &str, b: &str) -> bool {
    let a = a as *const str as *const u8;
    let b = b as *const str as *const u8;
    // Check first 11 bytes of \"src/main.rs\"
    let mut i: usize = 0;
    while i < 11 {
        unsafe {
            if *(a.wrapping_add(i)) != *(b.wrapping_add(i)) {
                return false;
            }
        }
        i = i + 1;
    }
    true
}

impl *const u8 {
    fn wrapping_add(self, n: usize) -> Self {
        ((self as usize) + n) as Self
    }
}

enum MyOption<T> {
    Some(T),
    None,
}

impl<T> MyOption<T> {
    fn map<U, F: FnOnce(T) -> U>(self, f: F) -> MyOption<U> {
        match self {
            MyOption::Some(v) => MyOption::Some(f(v)),
            MyOption::None => MyOption::None,
        }
    }
}

enum MyResult<T, E> {
    Ok(T),
    Err(E),
}

impl<T, E> MyResult<T, E> {
    fn map_err<F, O: FnOnce(E) -> F>(self, op: O) -> MyResult<T, F> {
        match self {
            MyResult::Ok(t) => MyResult::Ok(t),
            MyResult::Err(e) => MyResult::Err(op(e)),
        }
    }
}

struct Pair {
    a: u64,
    b: u64,
}
impl Copy for Pair {}

struct Boxed {
    val: u32,
}

trait TrackedTrait {
    #[track_caller]
    fn tr_loc(&self) -> (u32, u32);
}

impl Boxed {
    #[track_caller]
    fn inherent_loc(&self) -> (u32, u32) {
        let l = caller_location();
        (l.line + self.val - self.val, l.col)
    }
}

impl TrackedTrait for Boxed {
    fn tr_loc(&self) -> (u32, u32) {
        let l = caller_location();
        (l.line + self.val - self.val, l.col)
    }
}

#[track_caller]
fn free_tracked(x: u32) -> (u32, u32, u32, &'static str) {
    let l = caller_location();
    (x + 1, l.line, l.col, l.file)
}

#[track_caller]
fn gen_tracked<T: Copy>(x: T) -> (T, u32, u32) {
    let l = caller_location();
    (x, l.line, l.col)
}

#[track_caller]
fn unwrap_like<T: Copy>(x: T) -> (T, u32, u32) {
    let l = caller_location();
    (x, l.line, l.col)
}

fn apply_fn_ptr<T, U>(x: T, f: fn(T) -> U) -> U {
    f(x)
}

// Mirrors `clap_builder::parser::matches::RawValues<T>` holding a concrete `fn(T) -> U`!
struct Mapped<T, U> {
    val: MyOption<T>,
    f: fn(T) -> U,
}

impl<T, U> Mapped<T, U> {
    fn run(self) -> MyOption<U> {
        self.val.map(self.f)
    }
}

const CONST_FREE: fn(u32) -> (u32, u32, u32, &'static str) = free_tracked;
static STATIC_FREE: fn(u32) -> (u32, u32, u32, &'static str) = free_tracked;
";

/// End-to-end execution + lazy record-walk test covering `ReifyShim` for:
/// - a free `#[track_caller]` fn coerced to `fn(...) -> ...` (checking file, line, AND col)
/// - a generic `#[track_caller]` fn with aggregate parameter & sret return coerced to `fn(Pair) -> ...`
/// - an inherent `#[track_caller]` method coerced to `fn(&Boxed) -> ...`
/// - a trait `#[track_caller]` method coerced to `fn(&Boxed) -> ...`
/// - passing a `#[track_caller]` fn to a higher-order function taking `fn(T) -> U`, to
///   `MyOption::map` via a `fn(T) -> U` field (matching `clap_builder::ArgMatches::try_remove_many`),
///   and to `MyResult::map_err` with an explicit `fn(T) -> U` coercion
/// - `const` and `static` fn pointers to a `#[track_caller]` fn.
#[test]
fn tracked_fn_pointer_reify_shim_reports_definition_site() {
    let (free_line, free_col) = line_col_of(REIFY_PRELUDE, "fn free_tracked(");
    let (gen_line, gen_col) = line_col_of(REIFY_PRELUDE, "fn gen_tracked<");
    let (inherent_line, inherent_col) = line_col_of(REIFY_PRELUDE, "fn inherent_loc(");
    let (trait_line, trait_col) = line_col_of(REIFY_PRELUDE, "fn tr_loc(&self) -> (u32, u32) {");
    let (unwrap_line, unwrap_col) = line_col_of(REIFY_PRELUDE, "fn unwrap_like<");

    let program = format!(
        "{REIFY_PRELUDE}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    // 1. Free fn coerced to fn pointer
    let p_free: fn(u32) -> (u32, u32, u32, &'static str) = free_tracked;
    let (v, fl, fc, ff) = p_free(9);
    if v != 10 || fl != {free_line} || fc != {free_col} || !str_eq(ff, \"src/main.rs\") {{
        return 1;
    }}

    // 2. Generic fn with aggregate arg & aggregate (sret) return coerced to fn pointer
    let p_gen: fn(Pair) -> (Pair, u32, u32) = gen_tracked::<Pair>;
    let (pair_out, gl, gc) = p_gen(Pair {{ a: 7, b: 13 }});
    if pair_out.a != 7 || pair_out.b != 13 || gl != {gen_line} || gc != {gen_col} {{
        return 2;
    }}

    // 3. Inherent method coerced to fn pointer
    let b = Boxed {{ val: 5 }};
    let p_inh: fn(&Boxed) -> (u32, u32) = Boxed::inherent_loc;
    let (il, ic) = p_inh(&b);
    if il != {inherent_line} || ic != {inherent_col} {{
        return 3;
    }}

    // 4. Trait method (`#[track_caller]` inherited from trait decl) coerced to fn pointer
    let p_tr: fn(&Boxed) -> (u32, u32) = <Boxed as TrackedTrait>::tr_loc;
    let (tl, tc) = p_tr(&b);
    if tl != {trait_line} || tc != {trait_col} {{
        return 4;
    }}

    // 5. Higher-order function taking `fn(T) -> U`
    let (hv, hl, hc) = apply_fn_ptr(41u32, unwrap_like::<u32>);
    if hv != 41 || hl != {unwrap_line} || hc != {unwrap_col} {{
        return 5;
    }}

    // 6. `Option::map` with a concrete `fn(T) -> U` (matches `clap_builder::ArgMatches::try_remove_many`)
    let mapped = Mapped {{ val: MyOption::Some(Pair {{ a: 1, b: 2 }}), f: unwrap_like::<Pair> }};
    match mapped.run() {{
        MyOption::Some((p, ml, mc)) => {{
            if p.a != 1 || p.b != 2 || ml != {unwrap_line} || mc != {unwrap_col} {{
                return 6;
            }}
        }}
        MyOption::None => return 60,
    }}

    // 7. `Result::map_err` with fn pointer coercion
    let res: MyResult<u32, u32> = MyResult::Err(77);
    let res = res.map_err(unwrap_like::<u32> as fn(u32) -> (u32, u32, u32));
    match res {{
        MyResult::Err((ev, el, ec)) => {{
            if ev != 77 || el != {unwrap_line} || ec != {unwrap_col} {{
                return 7;
            }}
        }}
        MyResult::Ok(_) => return 70,
    }}

    // 8. `const` fn pointer to a `#[track_caller]` fn
    let (cv, cl, cc, _) = CONST_FREE(19);
    if cv != 20 || cl != {free_line} || cc != {free_col} {{
        return 8;
    }}

    // 9. `static` fn pointer to a `#[track_caller]` fn
    let (sv, sl, sc, _) = STATIC_FREE(29);
    if sv != 30 || sl != {free_line} || sc != {free_col} {{
        return 9;
    }}

    0
}}
"
    );

    walk_lazy_twice(&program);
    let status = run_status(&program);
    assert_eq!(status, 0, "expected all 9 ReifyShim checks to pass, got failure code {status}");
    println!("TRACK CALLER REIFY: all 9 cases (free, generic agg, inherent, trait, HOF, Option::map, Result::map_err, const, static) passed whole + lazy");
}

const TRACKED_DROP_PRELUDE: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
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
impl<T: PointeeSized> LegacyReceiver for MyBox<T> {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for bool {}
impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for i32 {}
impl Copy for usize {}
impl<T: PointeeSized> Copy for &T {}
impl<T: PointeeSized> Copy for *const T {}
impl<T: PointeeSized> Copy for *mut T {}

#[lang = \"drop\"]
trait Drop {
    fn drop(&mut self);
}

#[lang = \"drop_in_place\"]
unsafe fn drop_in_place<T: PointeeSized>(_to_drop: *mut T) {}

#[lang = \"panic_cannot_unwind\"]
fn panic_cannot_unwind() -> ! {
    loop {}
}

#[lang = \"unsize\"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = \"coerce_unsized\"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a mut U> for &'a mut T {}
impl<T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<MyBox<U>> for MyBox<T> {}

#[lang = \"panic_location\"]
struct Location<'a> {
    file: &'a str,
    line: u32,
    col: u32,
}

#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;

#[rustc_intrinsic]
const fn size_of<T>() -> usize;

#[rustc_intrinsic]
unsafe fn write_via_move<T>(dst: *mut T, value: T);

fn str_eq(a: &str, b: &str) -> bool {
    let a = a as *const str as *const u8;
    let b = b as *const str as *const u8;
    let mut i: usize = 0;
    while i < 11 {
        unsafe {
            if *((a as usize + i) as *const u8) != *((b as usize + i) as *const u8) {
                return false;
            }
        }
        i = i + 1;
    }
    true
}

static mut ARENA: [u64; 64] = [0; 64];
static mut NEXT_SLOT: usize = 0;
static mut DROP_COUNT: u32 = 0;
static mut DROP_SUM: u32 = 0;
static mut LAST_LINE: u32 = 0;
static mut LAST_COL: u32 = 0;
static mut FILE_OK: bool = true;

#[lang = \"owned_box\"]
struct MyBox<T: PointeeSized>(*mut T);

impl<T> MyBox<T> {
    fn new(v: T) -> MyBox<T> {
        let sz = size_of::<T>();
        unsafe {
            let words = (sz + 7) >> 3;
            let base = &raw mut ARENA as usize;
            let slot = (base + NEXT_SLOT * 8) as *mut T;
            NEXT_SLOT = NEXT_SLOT + words;
            write_via_move(slot, v);
            MyBox(slot)
        }
    }
}

impl<T: PointeeSized> Drop for MyBox<T> {
    fn drop(&mut self) {}
}

trait Marker {}

struct Guard {
    id: u32,
}

impl Marker for Guard {}

impl Drop for Guard {
    #[track_caller]
    fn drop(&mut self) {
        let loc = caller_location();
        unsafe {
            DROP_COUNT = DROP_COUNT + 1;
            DROP_SUM = DROP_SUM + self.id;
            LAST_LINE = loc.line;
            LAST_COL = loc.col;
            if loc.line == 0 || !str_eq(loc.file, \"src/main.rs\") {
                FILE_OK = false;
            }
        }
    }
}

enum MyOption<T> {
    Some(T),
    None,
}

struct Wrapper {
    inner: Guard,
}

#[track_caller]
fn drop_inside_tracked_fn() {
    let _g = Guard { id: 2 };
}
";

/// End-to-end native execution + lazy record-walk test covering `#[track_caller] fn drop(&mut self)`
/// called through `drop_glue` across direct scope exit, a `#[track_caller]` caller, `Option<T>`,
/// `struct Wrapper(T)`, `[T; 2]`, `Box<T>`, and `Box<dyn Trait>`. Verifies that `Location::caller()`
/// receives a valid `&'static Location<'static>` pointing to `drop_in_place`'s definition site
/// (`line > 0`, non-empty matching `.file()`).
#[test]
fn tracked_drop_impl_in_drop_glue_reports_valid_location() {
    let (dip_line, dip_col) = line_col_of(TRACKED_DROP_PRELUDE, "unsafe fn drop_in_place<");

    let program = format!(
        "{TRACKED_DROP_PRELUDE}
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    // 1. Direct scope drop in an untracked fn
    {{
        let _g = Guard {{ id: 1 }};
    }}
    unsafe {{
        if DROP_COUNT != 1 || LAST_LINE != {dip_line} || LAST_COL != {dip_col} || !FILE_OK {{
            return 1;
        }}
    }}

    // 2. Direct scope drop inside a `#[track_caller]` fn
    drop_inside_tracked_fn();
    unsafe {{
        if DROP_COUNT != 2 || LAST_LINE != {dip_line} || LAST_COL != {dip_col} || !FILE_OK {{
            return 2;
        }}
    }}

    // 3. Indirect drop via `Option<Guard>`
    {{
        let _opt = MyOption::Some(Guard {{ id: 4 }});
    }}
    unsafe {{
        if DROP_COUNT != 3 || LAST_LINE != {dip_line} || LAST_COL != {dip_col} || !FILE_OK {{
            return 3;
        }}
    }}

    // 4. Indirect drop via `struct Wrapper`
    {{
        let _w = Wrapper {{ inner: Guard {{ id: 8 }} }};
    }}
    unsafe {{
        if DROP_COUNT != 4 || LAST_LINE != {dip_line} || LAST_COL != {dip_col} || !FILE_OK {{
            return 4;
        }}
    }}

    // 5. Indirect drop via `[Guard; 2]`
    {{
        let _arr = [Guard {{ id: 16 }}, Guard {{ id: 32 }}];
    }}
    unsafe {{
        if DROP_COUNT != 6 || LAST_LINE != {dip_line} || LAST_COL != {dip_col} || !FILE_OK {{
            return 5;
        }}
    }}

    // 6. Indirect drop via `MyBox<Guard>`
    {{
        let _b = MyBox::new(Guard {{ id: 64 }});
    }}
    unsafe {{
        if DROP_COUNT != 7 || LAST_LINE != {dip_line} || LAST_COL != {dip_col} || !FILE_OK {{
            return 6;
        }}
    }}

    // 7. Indirect drop via `MyBox<dyn Marker>` (vtable word 0 -> drop_in_place::<Guard>)
    {{
        let _d: MyBox<dyn Marker> = MyBox::new(Guard {{ id: 128 }});
    }}
    unsafe {{
        if DROP_COUNT != 8 || DROP_SUM != 255 || LAST_LINE != {dip_line} || LAST_COL != {dip_col} || !FILE_OK {{
            return 7;
        }}
    }}

    0
}}
"
    );

    walk_lazy_twice(&program);
    let status = run_status(&program);
    assert_eq!(status, 0, "expected all 7 tracked Drop::drop glue checks to pass, got failure code {status}");
    println!("TRACK CALLER DROP GLUE: all 7 cases (direct, tracked caller, Option, struct, array, Box, dyn Trait) passed whole + lazy");
}
