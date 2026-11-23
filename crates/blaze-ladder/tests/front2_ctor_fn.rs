//! A tuple struct's or tuple variant's CTOR used as a function, not as the written callee of a
//! call: built as a function of its own (blaze-build ctor_fn.rs) and called, BUILT AND RUN.
//!
//! The census of 2026-11-17 03:32 (port27/tempo-run/build-census-g033201.txt) refused 283
//! instances with "callee 0x..: which is tabled with no body and is no outside symbol"; its
//! example is core's `<impl<'b> Pattern for &'b [char]>::is_prefix_of`, which core's
//! `pattern_methods!` writes as `($pmap)(self).is_prefix_of(haystack)` with `$pmap` the tuple
//! struct `MultiCharEqPattern`: a call whose callee is a PARENTHESIZED path. The walk builds a
//! ctor in place only when the callee expression is the ctor's path itself (walk.rs `construct`
//! by `expr_def(callee)`); every other use (`(W)(x)`, a fn item value `let f = E::A; f(x)`, a fn
//! pointer `let p: fn(..) -> W = W;`, a `Fn*` value `.map(Some)` / `map_err(Error::Io)`) calls
//! the ctor's fn, and the ctor's row has no body, so run.rs `build_from` refused it.
//!
//! The rules (the Rust Reference): "Structs": a tuple struct "also defines a constructor
//! function"; "Enumerations": a tuple-like variant's name likewise; "Function item types": the
//! ctor's name is a value of a fn item type, "Type coercions": it coerces to a fn pointer, and
//! "All function items implement `Fn`, `FnMut`, `FnOnce`". "Grouped expressions": `(expr)` has
//! the value of `expr`.
//!
//! The programs are `#![no_core]` (no toolchain), built and run in this process; the statuses
//! are the arithmetic the comments give.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

/// Overflow wraps: a `no_core` program declares no `panic_const_add_overflow`, and no sum here
/// overflows.
const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The lang items, a struct and an enum with tuple ctors, and the readers of their values.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}
impl Copy for u8 {}

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

struct W(u32, u8);

enum E {
    A(u32),
    B(u8, u8),
    C,
}

fn get(w: W) -> u32 {
    w.0 + w.1 as u32
}

fn e(v: E) -> u32 {
    match v {
        E::A(x) => x,
        E::B(a, b) => a as u32 * 10 + b as u32,
        E::C => 100,
    }
}
";

/// Builds and runs `HEAD` + `main`: the status, or the refusal on one line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| {
        // One short line: a refusal's whole text can run past what a log line shows.
        let said = format!("{e}").replace('\n', " ");
        said.char_indices().nth(400).map_or(said.clone(), |(i, _)| said[..i].to_string())
    })
}

/// The census's shape: a parenthesized ctor path as the callee, written directly and through a
/// `macro_rules!` `$p:expr` (core's `pattern_methods!`). `get(W(3, 4))` = 7, `e(E::A(5))` = 5,
/// `e(E::B(1, 2))` = 12: status 24.
/// Prints `CTOR THROUGH PARENS: Ok(24)`.
#[test]
fn a_parenthesized_ctor_callee_builds_and_runs() {
    let got = run("
macro_rules! call {
    ($p:expr, $x:expr) => {
        ($p)($x)
    };
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (get((W)(3, 4)) + e(call!(E::A, 5)) + e((E::B)(1, 2))) as i32
}
");
    println!("CTOR THROUGH PARENS: {got:?}");
    assert_eq!(got, Ok(24));
}

/// A ctor as a fn item VALUE and as a fn POINTER: `e(f(9))` = 9, `get(p(6, 1))` = 7: status 16.
/// Prints `CTOR AS A VALUE: Ok(16)`.
#[test]
fn a_ctor_as_a_value_and_a_pointer_builds_and_runs() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let f = E::A;
    let p: fn(u32, u8) -> W = W;
    (e(f(9)) + get(p(6, 1))) as i32
}
");
    println!("CTOR AS A VALUE: {got:?}");
    assert_eq!(got, Ok(16));
}

/// A ctor as an `Fn*` value (`.map(Some)`'s shape): the `FnOnce::call_once` shim at the ctor's
/// fn item calls the ctor's fn. `e(apply(E::A, 11))` = 11, `get(apply2(W, 2, 3))` = 5: status 16.
/// Prints `CTOR AS AN FN VALUE: Ok(16)`.
#[test]
fn a_ctor_as_an_fn_trait_value_builds_and_runs() {
    let got = run("
fn apply<F: FnOnce(u32) -> E>(f: F, x: u32) -> E {
    f(x)
}

fn apply2<F: Fn(u32, u8) -> W>(f: F, x: u32, y: u8) -> W {
    f(x, y)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (e(apply(E::A, 11)) + get(apply2(W, 2, 3))) as i32
}
");
    println!("CTOR AS AN FN VALUE: {got:?}");
    assert_eq!(got, Ok(16));
}

/// A tuple struct `S` whose field `D` implements `Drop`, its ctor used as a fn POINTER and as an
/// `Fn*` value (`.map(S)`'s shape). Building `S`'s ctor fn lays out `S`, which asks whether `D`
/// has a `Drop` impl (blaze-build `Adts::drop_impl`): the ctor's row has no body, so the
/// question is asked in its ADT's world (front2 world_items.rs `ctor_world`), never a body's.
/// The fault this guards (2026-11-17, every tempo census since a2b1388119): the question was
/// asked in the row's body world and panicked "the back half asks only in a fn it builds, and
/// every one has its body" (program.rs `in_world`); this program panicked the same way.
///
/// `take` drops its `S` at its end, and with it the `D` inside (the Reference, "Destructors":
/// a struct's fields are dropped after it): `D(1)` then `D(10)`, each ONCE, so `DROPS` = 11
/// (a second drop of either would make it 12, 21 or 22). Status `2 + 3 + 11 * 10` = 115.
const DROP_PROGRAM: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
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

impl Copy for u32 {}

#[lang = \"drop\"]
trait Drop {
    fn drop(&mut self);
}

#[lang = \"drop_in_place\"]
unsafe fn drop_in_place<T: PointeeSized>(to_drop: *mut T) {
    unsafe { drop_in_place(to_drop) }
}

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

static mut DROPS: u32 = 0;

struct D(u32);

impl Drop for D {
    fn drop(&mut self) {
        unsafe {
            DROPS = DROPS + self.0;
        }
    }
}

struct S(D, u32);

fn take(s: S) -> u32 {
    s.1
}

fn apply<F: FnOnce(D, u32) -> S>(f: F, d: D, x: u32) -> S {
    f(d, x)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let p: fn(D, u32) -> S = S;
    let a = take(p(D(1), 2));
    let b = take(apply(S, D(10), 3));
    (a + b + unsafe { DROPS } * 10) as i32
}
";

/// [`DROP_PROGRAM`] built and RUN (front2's whole program, `Front2Program::new`): each `D`
/// dropped once. Prints `CTOR WITH A DROP FIELD: Ok(115)`.
#[test]
fn a_ctor_with_a_drop_field_builds_and_drops_once() {
    let files = [SourceFile { path: "src/main.rs", text: DROP_PROGRAM }];
    let got = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| {
        let said = format!("{e}").replace('\n', " ");
        said.char_indices().nth(400).map_or(said.clone(), |(i, _)| said[..i].to_string())
    });
    println!("CTOR WITH A DROP FIELD: {got:?}");
    assert_eq!(got, Ok(115));
}

/// [`DROP_PROGRAM`] through the census's path: `Front2Program::survey_from` at `main`, then
/// `build_survey` (tempo's build census, blaze-real's `tempo_build`). Every instance builds,
/// `S`'s ctor fn and its `FnOnce` shim among them; none is refused and the survey goes on.
/// Prints `CTOR SURVEY: built n of n (0 refused: )`.
#[test]
fn a_ctor_with_a_drop_field_builds_in_the_census_survey() {
    use blaze_ladder::crates::{Inputs, Toolchain};
    use blaze_ladder::front2::program::Front2Program;
    use blaze_ladder::program_texts::ProgramTexts;
    use blaze_scope::names::{self, Written};
    use blaze_surface::{Namespace, PathKind};
    use std::alloc::Global;
    let files = [SourceFile { path: "src/main.rs", text: DROP_PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let texts = ProgramTexts::new(&files, "src/main.rs", NEW, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let segments: &[&str] = &["main"];
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments }, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("`main` by path: {e:?}"));
    let main = found.whole(1).unwrap_or_else(|| panic!("`main` resolves whole")).key;
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey_from(&names, &frame, &files, &[main], &mut left_out, None, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn of the program; left out: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = blaze_ladder::run::build_survey(&mut program, &mut refused, NEW.overflow, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the survey stopped: {e}"));
    let rows: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("CTOR SURVEY: built {} of {} ({} refused: {})", counts.built, counts.tried, rows.len(), rows.join("; "));
    assert!(rows.is_empty(), "no instance refused: {rows:?}");
    assert!(counts.tried > 0 && counts.built == counts.tried, "every instance built: {rows:?}");
}

/// The records the program's crates are read from, as tests/d106_records.rs keeps them (the
/// command's effect layer is blaze-cli front_load.rs `StoreLoad`).
#[derive(Clone, Default)]
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<std::collections::HashMap<String, &'static str>>,
}

impl blaze_ladder::front2::source::Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    /// The program writes no macro call: no expansion is asked.
    fn expansion(&self, _krate: usize, _call: blaze_id::ItemKey) -> Option<blaze_ladder::front2::source::LoadedExpansion<'static>> {
        None
    }
}

/// A BodyCheck key for every item, a digest of its key (tests/d106_records.rs `EveryItem`).
struct EveryItem;

const READS: [blaze_ladder::body_ids::Read; 1] = [blaze_ladder::body_ids::Read { fact: [3; 32], digest: [4; 32] }];

impl blaze_ladder::body_ids::BodyIds for EveryItem {
    fn body_check(&self, item: blaze_typeck::ItemKey) -> Option<blaze_ladder::body_ids::BodyId<'_>> {
        Some(blaze_ladder::body_ids::BodyId { key: blaze_ids::Hasher::new("ctor fn test every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: blaze_typeck::ItemKey) -> Option<blaze_ladder::body_ids::Read> {
        Some(blaze_ladder::body_ids::Read { fact: [8; 32], digest: [9; 32] })
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

const LANG_CRATES: [usize; 1] = [0];

/// [`DROP_PROGRAM`] over NAMES RECORDS (the lazy program behind `run::make_one::Maker`, what
/// `blaze run` reads, tests/d106_records.rs's setup): solved once, its records written, the
/// solve dropped, then walked from `main` by the record walk. Every reached instance is made,
/// `S`'s ctor fn (its row made by the seam's `head_row`, built by `build_ctor_fn`) and the
/// `FnOnce` shim of it among them; nothing is refused.
/// Prints `CTOR OVER RECORDS: <counts>`.
#[test]
fn a_ctor_with_a_drop_field_is_made_over_records() {
    use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
    use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
    use blaze_ladder::front2::source::{Log, Records};
    use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
    let a = blaze_own::testing::heap().alloc();
    let panic = blaze_build::Panic::Abort;
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let files = [(String::from("/ladder/src/main.rs"), String::from(DROP_PROGRAM))];
    let user = UserCrate { name: "ladder", dir: "/ladder/", root: "/ladder/src/main.rs", files: &files, edition, panic };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let keys: &'static [blaze_id::CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let mut kept = Kept::default();
    let main = {
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("the one solve: {e}"));
        for c in names.crates().iter() {
            kept.names.push(blaze_scope::resolve::aligned(&blaze_scope::names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.items.push(blaze_scope::resolve::aligned(&blaze_scope::items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        }
        let segments: &[&str] = &["main"];
        let written = blaze_scope::names::Written { kind: blaze_surface::PathKind::Plain, segments };
        let found = blaze_scope::names::path(&names, names.root(0), blaze_surface::Namespace::Values, written, a).unwrap_or_else(|e| panic!("`main` by path: {e:?}"));
        blaze_ladder::front2::keys::checker_of(found.whole(1).unwrap_or_else(|| panic!("`main` resolves whole")).key)
    };
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, a)));
    let reader = NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None };
    let builder = Builder {
        isa: blaze_emit::Isa { lse: true, os: blaze_emit::Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(NEW.overflow, panic),
        toolchain: blaze_toolchain::ToolchainId([7; 32]),
        producer: blaze_ids::producer::Producers::for_tests(b"ctor fn over records").get::<blaze_ids::producer::CodeBuilder>(),
        edition,
    };
    let producer = blaze_ids::producer::Producers::for_tests(b"ctor fn over records").get::<blaze_ids::producer::CodeBuilder>();
    let mut store = MemoryStore::new_in(a);
    let mut maker = Maker::new(reader, Some(main), &builder, a);
    let walked = walk_records(main, &EveryItem, &mut store, &builder, &producer, &mut maker, a);
    let logged: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    let counts = match walked {
        Ok((_, counts)) => counts,
        Err(e) => {
            let said = format!("{e}").replace('\n', " ");
            panic!("the record walk: {}", said.char_indices().nth(600).map_or(said.clone(), |(i, _)| said[..i].to_string()));
        }
    };
    println!("CTOR OVER RECORDS: {counts}");
    assert!(logged.is_empty(), "the records filed {logged:?}");
    assert!(counts.reached > 0 && counts.made == counts.reached, "every reached instance made: {counts}");
}
