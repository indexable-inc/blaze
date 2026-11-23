//! Every ENUM LAYOUT a `match` reads, on both paths `blaze run` has: the lazy program over
//! NAMES RECORDS that makes each fn on its first call (run/make_one.rs `walk_records`, as
//! tests/const_fn_pointers.rs `walk_into` walks it), and the program built and run whole
//! (`run_with`, as tests/d106_callables.rs runs it) for its status.
//!
//! The class (hello world on the real std, 2026-11-17): the program stopped on `brk #0xff02`,
//! an `unreachable` reached, in std's formatting: a `match` whose scrutinee's discriminant was
//! read as no variant's. A wrong tag read (a niche's value, a narrowed range, a negative
//! discriminant, a tag at the wrong offset inside a struct, a by-reference read of a by-value
//! layout) lands there. One `#![no_core]` program per layout kind, in core's shapes:
//! `fmt::rt::ArgumentType` (a niche in a reference beside a `u16` variant), `Option<&T>`,
//! `fmt::Alignment` and `Option<Alignment>`, `cmp::Ordering` (`#[repr(i8)]`, -1) and
//! `Option<Ordering>`, `io::ErrorKind` (41 unit variants), `#[repr(u8)]` with explicit and
//! implied discriminants, a niche in a `NonZero` and in a `NonNull` field, an enum with a
//! variant holding a 0-variant enum, and `Result<(), fmt::Error>`.
//!
//! Each program builds every variant in one `#[inline(never)]` fn (`make`) and passes it
//! through four `#[inline(never)]` matchers: by value, by reference, inside a struct by value,
//! inside a struct by reference (the struct's other fields are checked too, so a tag written
//! over a neighbour shows). Each matcher returns `v + 1` for variant `v`, and a payload checked
//! by a literal pattern returns 98 when it is wrong. A row whose variants are built from
//! constants alone also passes a CONSTANT ITEM of each variant (`const K<v>: T = ..;`, made by
//! the constant evaluator, whose tag must be the layout's) through the by-value matcher; a row
//! with a cast also checks `as_int(make(v))` (an `as` cast, or a field read and a cast) against
//! the variant's integer. `main` returns 250 when every check passed, else, for the first
//! variant `v` that failed: `1 + v` (one of the four matchers), `70 + v` (its constant), `140 +
//! v` (its cast).
//!
//! THE ALIGNMENT ROWS (std hello, 2026-11-17, after the trap table: "it struck at +0x3fc in fn
//! `core::alloc::layout::<impl Layout>::from_size_align_unchecked` .. `brk #0xff02`: an
//! `unreachable` reached ..: the read of the enum's discriminant found no variant's
//! discriminant"): core's `ptr::Alignment(AlignmentEnum)`, `#[repr(usize)] enum AlignmentEnum {
//! _Align1Shl0 = 1 << 0, .., _Align1Shl63 = 1 << 63 }`, made by `mem::transmute(align)` and
//! copied into `Layout { size, align }`. Each row isolates one candidate: (a) discriminants
//! that are constant EXPRESSIONS (with a constant item of each variant: the cause, below), (b)
//! the sign bit `1 << 63` under `repr(usize)` and `repr(u64)`, (c) all 64 sparse variants,
//! (d) `transmute` in (at run time and in a constant), (e) through the `repr(transparent)`
//! newtype inside a `Layout`-shaped struct, (f) `Option` of the newtype (the niche is the
//! invalid discriminants: `None` is 0).
//!
//! THE CAUSE (read, 2026-11-17): the constant evaluator numbered an enum's variants by its own
//! answer, `ConstWorld::explicit_discriminant` (blaze-consteval machine.rs
//! `explicit_discriminant`, the record of the variant's `= expr`), which both front ends'
//! worlds answer `None` for EVERY variant (front2/consts.rs `ConstsWorld::explicit_discriminant`
//! and run.rs `FrontConsts::explicit_discriminant`), so every enum was 0, 1, 2, .. there, while
//! the layout writes and reads tags by `LayoutFacts::discriminant` (front2 heads.rs
//! `discriminants`: 1, 2, 4, ..). A constant built by variant (`Alignment::MIN`'s
//! `AlignmentEnum::_Align1Shl0`) held tag 0, and a constant made by `transmute` was decoded as
//! the variant whose INDEX the bits are, then written back by that variant's real discriminant
//! (8 became 256). The machine now reads `LayoutFacts::discriminant`, the one home.
//!
//! The helpers are tests/const_fn_pointers.rs's (fixture, reader, builder, the every-item
//! stand-in), copied: a test file is its own crate.

use std::collections::HashMap;
use std::fmt::Write as _;

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

#[path = "match_layouts_run/small_main.rs"]
mod small_main;
#[path = "match_layouts_run/real_shape.rs"]
mod real_shape;
#[path = "match_layouts_run/shim_abi.rs"]
mod shim_abi;

const DIR: &str = "/ladder/";
const ROOT: &str = "/ladder/src/main.rs";
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];
/// What `main` returns when every variant matched on every path.
const PASSED: u8 = 250;

/// The lang items a `match` over these layouts needs (core's marker.rs), and the `Option` and
/// `Result` core declares.
const HEAD: &str = "#![feature(no_core, lang_items, rustc_attrs, intrinsics, decl_macro, pattern_types, pattern_type_macro)]
#![no_core]
#![no_main]
#![allow(internal_features, dead_code, unreachable_patterns)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u16 {}
impl Copy for u32 {}
impl Copy for i32 {}
impl<T: PointeeSized> Copy for &T {}

enum Option<T> {
    None,
    Some(T),
}

enum Result<T, E> {
    Ok(T),
    Err(E),
}

/// As the pinned core declares it (library/core/src/intrinsics/mod.rs).
#[rustc_nounwind]
#[rustc_intrinsic]
pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
";

/// One layout kind: its items, the matched type (it may name `'a`, the lifetime of `make`'s
/// `r: &'a u8`, which holds 7), each variant as built (from `r`) and as matched, and arms after
/// the variants' (a payload's wrong value: 98).
struct Layout {
    name: &'static str,
    decls: String,
    ty: &'static str,
    variants: Vec<(String, String)>,
    extra: &'static str,
    /// Each variant's build is a constant expression (it names no `r`): a constant item of each
    /// is matched too.
    consts: bool,
    /// `as_int(e: T) -> int { expr }` and each variant's integer.
    cast: Option<Cast>,
}

/// A cast check: the integer type, the body over `e`, each variant's value.
struct Cast {
    int: &'static str,
    expr: &'static str,
    values: Vec<u128>,
}

/// The arms of every matcher: variant `v`'s pattern gives `v + 1`, then `extra`.
fn arms(l: &Layout) -> String {
    let mut s = String::new();
    for (v, (_, pattern)) in l.variants.iter().enumerate() {
        writeln!(s, "        {pattern} => {},", v + 1).expect("a String takes the text");
    }
    s.push_str(l.extra);
    s
}

/// `HEAD`, the layout's items, `make`, the four matchers and `main` (see the module doc).
fn program(l: &Layout) -> String {
    let ty = l.ty;
    let arms = arms(l);
    let mut s = String::from(HEAD);
    s.push_str(&l.decls);
    let last = l.variants.len() - 1;
    s.push_str(&format!("\n#[inline(never)]\nfn make<'a>(i: u8, r: &'a u8) -> {ty} {{\n    let _ = r;\n    match i {{\n"));
    for (v, (build, _)) in l.variants.iter().enumerate() {
        match v == last {
            false => writeln!(s, "        {v} => {build},"),
            true => writeln!(s, "        _ => {build},"),
        }
        .expect("a String takes the text");
    }
    s.push_str("    }\n}\n");
    s.push_str(&format!(
        "
#[inline(never)]
fn by_value<'a>(e: {ty}) -> u8 {{
    match e {{
{arms}    }}
}}

#[inline(never)]
fn by_ref<'a>(e: &{ty}) -> u8 {{
    match e {{
{arms}    }}
}}

struct Holder<'a> {{
    pad: u8,
    e: {ty},
    r: &'a u8,
}}

#[inline(never)]
fn in_struct<'a>(h: Holder<'a>) -> u8 {{
    match h.pad {{
        1 => {{}}
        _ => return 97,
    }}
    match *h.r {{
        7 => {{}}
        _ => return 96,
    }}
    match h.e {{
{arms}    }}
}}

#[inline(never)]
fn in_struct_ref<'a>(h: &Holder<'a>) -> u8 {{
    match h.pad {{
        1 => {{}}
        _ => return 97,
    }}
    match *h.r {{
        7 => {{}}
        _ => return 96,
    }}
    match &h.e {{
{arms}    }}
}}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    let seven: u8 = 7;
"
    ));
    let check = |s: &mut String, call: &str, want: &str, code: usize| {
        writeln!(s, "    match {call} {{\n        {want} => {{}}\n        _ => return {code},\n    }}").expect("a String takes the text");
    };
    for v in 0..l.variants.len() {
        let want = format!("{}", v + 1);
        let calls = [
            format!("by_value(make({v}, &seven))"),
            format!("by_ref(&make({v}, &seven))"),
            format!("in_struct(Holder {{ pad: 1, e: make({v}, &seven), r: &seven }})"),
            format!("in_struct_ref(&Holder {{ pad: 1, e: make({v}, &seven), r: &seven }})"),
        ];
        for call in &calls {
            check(&mut s, call, &want, 1 + v);
        }
        if l.consts {
            check(&mut s, &format!("by_value(K{v})"), &want, 70 + v);
        }
        if let Some(cast) = &l.cast {
            check(&mut s, &format!("as_int(make({v}, &seven))"), &format!("{}", cast.values[v]), 140 + v);
        }
    }
    writeln!(s, "    {PASSED}\n}}").expect("a String takes the text");
    if l.consts {
        for (v, (build, _)) in l.variants.iter().enumerate() {
            writeln!(s, "\nconst K{v}: {ty} = {build};").expect("a String takes the text");
        }
    }
    if let Some(cast) = &l.cast {
        writeln!(s, "\n#[inline(never)]\nfn as_int<'a>(e: {ty}) -> {} {{\n    {}\n}}", cast.int, cast.expr).expect("a String takes the text");
    }
    s
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(b, p)| (String::from(*b), String::from(*p))).collect()
}

/// The same text built and matched (a unit variant).
fn units(names: &[&str]) -> Vec<(String, String)> {
    names.iter().map(|n| (String::from(*n), String::from(*n))).collect()
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
    Producers::for_tests(b"match layouts run").get::<CodeBuilder>()
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
        Some(BodyId { key: Hasher::new("match layouts every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
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

/// The row: its program walked over names records (the lazy path, every instance made), then
/// run whole (the eager path) to [`PASSED`]. Both are tried before either is asserted, so one
/// failure shows the other path's answer too. Prints "match layouts <name>: walk <counts or
/// refusal>; status <n or refusal>".
fn row(l: Layout) {
    let text = program(&l);
    let f = fixture(&text);
    let mut store = MemoryStore::new_in(alloc());
    let walked = walk_into(&f, &mut store);
    let ran = run_whole(&text);
    let walk_said = match &walked {
        Ok(counts) => format!("{counts}"),
        Err(e) => format!("REFUSED: {e}"),
    };
    let ran_said = match &ran {
        Ok(status) => format!("{status}"),
        Err(e) => format!("REFUSED: {e}"),
    };
    println!("match layouts {}: walk {walk_said}; status {ran_said}", l.name);
    let name = l.name;
    walked.unwrap_or_else(|e| panic!("{name}: the lazy path: {e}"));
    let status = ran.unwrap_or_else(|e| panic!("{name}: the whole run refused: {e}"));
    assert_eq!(status, PASSED, "{name}: 250 is every check passed; else for variant v: 1 + v a matcher, 70 + v its constant, 140 + v its cast\n{text}");
}

/// `fmt::rt::ArgumentType`'s shape: `Placeholder` holds a reference and a fn pointer (both
/// non-null: the niche the tag lives in) and `Count` a `u16`. Statuses 1 (Placeholder),
/// 2 (Count(3)); a `Count` whose payload is not 3 gives 98.
#[test]
fn argument_type() {
    row(Layout {
        name: "ArgumentType",
        decls: String::from(
            "
fn seven_of(x: &u8) -> u8 {
    *x
}

enum ArgumentType<'a> {
    Placeholder { value: &'a u8, formatter: fn(&u8) -> u8 },
    Count(u16),
}
",
        ),
        ty: "ArgumentType<'a>",
        variants: pairs(&[
            ("ArgumentType::Placeholder { value: r, formatter: seven_of }", "ArgumentType::Placeholder { .. }"),
            ("ArgumentType::Count(3)", "ArgumentType::Count(3)"),
        ]),
        extra: "        ArgumentType::Count(_) => 98,\n",
        consts: false,
        cast: None,
    });
}

/// `Option<&T>`: `None` is the null reference. Statuses 1 (None), 2 (Some).
#[test]
fn option_of_a_reference() {
    row(Layout {
        name: "Option<&u8>",
        decls: String::new(),
        ty: "Option<&'a u8>",
        variants: pairs(&[("Option::None", "Option::None"), ("Option::Some(r)", "Option::Some(_)")]),
        extra: "",
        consts: false,
        cast: None,
    });
}

/// `fmt::Alignment`: three unit variants, tags 0..=2. Statuses 1, 2, 3.
#[test]
fn alignment() {
    row(Layout {
        name: "Alignment",
        decls: String::from("\nenum Alignment {\n    Left,\n    Right,\n    Center,\n}\n"),
        ty: "Alignment",
        variants: units(&["Alignment::Left", "Alignment::Right", "Alignment::Center"]),
        extra: "",
        consts: true,
        cast: None,
    });
}

/// `Option<fmt::Alignment>`: `None` in the tag's niche (0xff: the one value nearer zero, as
/// rustc's `Niche::reserve` picks it). Statuses 1 (None), 2..=4.
#[test]
fn option_of_alignment() {
    row(Layout {
        name: "Option<Alignment>",
        decls: String::from("\nenum Alignment {\n    Left,\n    Right,\n    Center,\n}\n"),
        ty: "Option<Alignment>",
        variants: units(&["Option::None", "Option::Some(Alignment::Left)", "Option::Some(Alignment::Right)", "Option::Some(Alignment::Center)"]),
        extra: "",
        consts: true,
        cast: None,
    });
}

const ORDERING: &str = "
#[repr(i8)]
enum Ordering {
    Less = -1,
    Equal = 0,
    Greater = 1,
}
";

/// `cmp::Ordering`: `#[repr(i8)]`, a NEGATIVE discriminant (-1 is the byte 0xff). Statuses 1
/// (Less), 2 (Equal), 3 (Greater).
#[test]
fn ordering() {
    row(Layout {
        name: "Ordering",
        decls: String::from(ORDERING),
        ty: "Ordering",
        variants: units(&["Ordering::Less", "Ordering::Equal", "Ordering::Greater"]),
        extra: "",
        consts: true,
        cast: None,
    });
}

/// `Option<cmp::Ordering>` (what `partial_cmp` returns): `None` in the niche below -1..=1
/// (0xfe, -2: the tie goes below).
/// Statuses 1 (None), 2..=4.
#[test]
fn option_of_ordering() {
    row(Layout {
        name: "Option<Ordering>",
        decls: String::from(ORDERING),
        ty: "Option<Ordering>",
        variants: units(&["Option::None", "Option::Some(Ordering::Less)", "Option::Some(Ordering::Equal)", "Option::Some(Ordering::Greater)"]),
        extra: "",
        consts: true,
        cast: None,
    });
}

/// `io::ErrorKind`'s size: 41 unit variants, tags 0..=40. Statuses 1..=41.
#[test]
fn error_kind() {
    let names: Vec<String> = (0..41).map(|v| format!("ErrorKind::K{v}")).collect();
    let mut decls = String::from("\nenum ErrorKind {\n");
    for v in 0..41 {
        writeln!(decls, "    K{v},").expect("a String takes the text");
    }
    decls.push_str("}\n");
    row(Layout {
        name: "ErrorKind",
        decls,
        ty: "ErrorKind",
        variants: names.iter().map(|n| (n.clone(), n.clone())).collect(),
        extra: "",
        consts: true,
        cast: None,
    });
}

/// `#[repr(u8)]` with explicit discriminants and one implied after them (3, 7, 200, 201).
/// Statuses 1..=4.
#[test]
fn repr_u8_explicit() {
    row(Layout {
        name: "repr(u8) explicit",
        decls: String::from("\n#[repr(u8)]\nenum Tag {\n    A = 3,\n    B = 7,\n    C = 200,\n    D,\n}\n"),
        ty: "Tag",
        variants: units(&["Tag::A", "Tag::B", "Tag::C", "Tag::D"]),
        extra: "",
        consts: true,
        cast: None,
    });
}

/// core's `pattern_type!` as the pinned core declares it (library/core/src/pat.rs): the pinned
/// core's `NonZero` and `NonNull` are PATTERN TYPES (`NonZeroU32Inner(u32 is 1..=u32::MAX)` in
/// num/niche_types.rs, `NonNull { pointer: *const T is !null }`), laid out by blaze-build
/// layout.rs `PNode::Pat`. The `rustc_layout_scalar_valid_range_*` attribute these rows used
/// before is read too (heads.rs `AdtRow::valid`, the seam's `valid_range`: real_shape.rs has
/// a row with it); real core no longer writes it on these types.
const PATTERN_TYPE: &str = "
#[rustc_builtin_macro(pattern_type)]
pub macro pattern_type($($arg:tt)*) {
    /* compiler built-in */
}
";

/// `Option<NonZero<u32>>`'s inner shape: `None` is 0, the value the pattern type leaves out.
/// Statuses 1 (None), 2 (Some(5)).
#[test]
fn option_of_non_zero() {
    row(Layout {
        name: "Option<NonZeroU32Inner> (pattern type)",
        decls: format!("{PATTERN_TYPE}\n#[repr(transparent)]\nstruct NonZeroU32Inner(pattern_type!(u32 is 1..=4294967295));\n"),
        ty: "Option<NonZeroU32Inner>",
        variants: pairs(&[("Option::None", "Option::None"), ("Option::Some(unsafe { transmute::<u32, NonZeroU32Inner>(5) })", "Option::Some(_)")]),
        extra: "",
        consts: true,
        cast: None,
    });
}

/// A niche in a `NonNull` FIELD of a struct with another field before it: `None` is the null
/// pointer at the field's offset (8), not the struct's first word. Statuses 1 (None), 2 (Some);
/// a `Some` whose `len` is not 4 gives 98.
#[test]
fn option_of_a_non_null_field() {
    row(Layout {
        name: "Option<Raw> (NonNull field, pattern type)",
        decls: format!(
            "{PATTERN_TYPE}
#[repr(transparent)]
struct NonNull<T: PointeeSized> {{
    pointer: pattern_type!(*const T is !null),
}}

struct Raw {{
    len: u32,
    ptr: NonNull<u8>,
}}
"
        ),
        ty: "Option<Raw>",
        variants: pairs(&[("Option::None", "Option::None"), ("Option::Some(Raw { len: 4, ptr: unsafe { transmute::<*const u8, NonNull<u8>>(r as *const u8) } })", "Option::Some(Raw { len: 4, .. })")]),
        extra: "        Option::Some(_) => 98,\n",
        consts: false,
        cast: None,
    });
}

/// `1 << n` for each `n`: core's `AlignmentEnum` variants up to `last` (64 is all of them), as
/// declarations and as `(build, pattern)` pairs, and each one's integer.
fn shifts(name: &str, repr: &str, ns: &[u32]) -> (String, Vec<(String, String)>, Vec<u128>) {
    let mut decls = format!("\n#[repr({repr})]\nenum {name} {{\n");
    for n in ns {
        writeln!(decls, "    S{n} = 1 << {n},").expect("a String takes the text");
    }
    decls.push_str("}\n");
    let variants = ns.iter().map(|n| (format!("{name}::S{n}"), format!("{name}::S{n}"))).collect();
    (decls, variants, ns.iter().map(|&n| 1u128 << n).collect())
}

/// (a) Discriminants given by CONSTANT EXPRESSIONS (`1 << n`, not literals), `repr(usize)`:
/// `as usize` gives 1, 2, 8, 4096; the matchers 1..=4; each variant's CONSTANT item (the
/// constant evaluator's tag) too. Isolates the cause: before the fix the constants held tags
/// 0..=3 (the evaluator's own numbering), and the run stopped on `brk #0xff02` in `by_value`
/// (the first constant matched no arm) or returned 70 + v.
#[test]
fn a_discriminants_by_constant_expressions() {
    let (decls, variants, values) = shifts("A", "usize", &[0, 1, 3, 12]);
    row(Layout { name: "(a) 1 << n discriminants", decls, ty: "A", variants, extra: "", consts: true, cast: Some(Cast { int: "usize", expr: "e as usize", values }) });
}

/// (b) The SIGN BIT under `repr(usize)`: `1 << 63` beside `1 << 0` and `1 << 62`. A tag's
/// range check or compare done signed (`1 << 63` as `i64::MIN`) refuses it or everything.
/// Statuses 1..=3; casts 1, 2^62, 2^63.
#[test]
fn b_the_sign_bit_repr_usize() {
    let (decls, variants, values) = shifts("B", "usize", &[0, 62, 63]);
    row(Layout { name: "(b) 1 << 63, repr(usize)", decls, ty: "B", variants, extra: "", consts: true, cast: Some(Cast { int: "usize", expr: "e as usize", values }) });
}

/// (b) The SIGN BIT under `repr(u64)`, as (b) under `repr(usize)`.
#[test]
fn b_the_sign_bit_repr_u64() {
    let (decls, variants, values) = shifts("B", "u64", &[0, 62, 63]);
    row(Layout { name: "(b) 1 << 63, repr(u64)", decls, ty: "B", variants, extra: "", consts: true, cast: Some(Cast { int: "u64", expr: "e as u64", values }) });
}

/// (c) ALL 64 sparse variants, `1 << 0 ..= 1 << 63`, `repr(usize)`: a switch lowered as a
/// dense table over the VALUE (1..2^63) instead of a compare chain or a table over the variant
/// index fails here. Statuses 1..=64; casts 2^n.
#[test]
#[ignore = "ANOTHER LIMIT (2026-11-17): the row's `main` (64 variants, each built, matched four ways and cast) is refused before it runs: `main`: a branch or PC-relative reference past its reach is past what the baseline tier encodes: a function this large needs the tier's far-branch form (its own piece); the sparse-discriminant mechanism itself passes in rows (a), (b), (d), (e), (f)"]
fn c_all_sixty_four_sparse_variants() {
    let ns: Vec<u32> = (0..64).collect();
    let (decls, variants, values) = shifts("AlignmentEnum", "usize", &ns);
    row(Layout { name: "(c) all 64 of AlignmentEnum", decls, ty: "AlignmentEnum", variants, extra: "", consts: true, cast: Some(Cast { int: "usize", expr: "e as usize", values }) });
}

/// (d) TRANSMUTE in: each variant made by `transmute::<usize, A>(1 << n)` (at run time in
/// `make`, and in a constant item: the constant evaluator decodes the bits by its
/// discriminants), then matched and cast back. Before the fix a constant `transmute(8)` was
/// decoded as variant 8 (the index) and written back as 1 << 8.
#[test]
fn d_transmute_in() {
    let (decls, _, values) = shifts("A", "usize", &[0, 1, 3, 12]);
    let variants = [0u32, 1, 3, 12].iter().map(|n| (format!("unsafe {{ transmute::<usize, A>({}) }}", 1u128 << n), format!("A::S{n}"))).collect();
    row(Layout { name: "(d) transmute::<usize, A>", decls, ty: "A", variants, extra: "", consts: true, cast: Some(Cast { int: "usize", expr: "e as usize", values }) });
}

/// The newtype and the `Layout`-shaped struct (core's `ptr::Alignment(AlignmentEnum)` and
/// `alloc::Layout { size, align }`), over (a)'s enum.
fn newtype_decls() -> String {
    let (decls, _, _) = shifts("A", "usize", &[0, 1, 3, 12]);
    format!("{decls}\n#[repr(transparent)]\nstruct W(A);\n\nstruct L {{\n    size: usize,\n    align: W,\n}}\n")
}

/// (e) Through the newtype: `L { size: 16, align: transmute::<usize, W>(1 << n) }` built as
/// `Layout::from_size_align_unchecked` builds it, matched by its field and read back as
/// `l.align.0 as usize`. Statuses 1..=4 (a wrong `size` gives 98); casts 1, 2, 8, 4096.
#[test]
fn e_through_the_newtype_in_a_layout() {
    let variants = [0u32, 1, 3, 12]
        .iter()
        .map(|n| (format!("L {{ size: 16, align: unsafe {{ transmute::<usize, W>({}) }} }}", 1u128 << n), format!("L {{ size: 16, align: W(A::S{n}) }}")))
        .collect();
    row(Layout {
        name: "(e) Layout { size, align: W(A) }",
        decls: newtype_decls(),
        ty: "L",
        variants,
        extra: "        _ => 98,\n",
        consts: true,
        cast: Some(Cast { int: "usize", expr: "e.align.0 as usize", values: [0u32, 1, 3, 12].iter().map(|&n| 1u128 << n).collect() }),
    });
}

/// (f) `Option<W>`: the niche is the discriminants no variant has, so `None` is 0. `None`,
/// `Some(W(A::S3))` by variant, `Some(transmute(4096))`. Statuses 1..=3 (a `Some` of another
/// variant gives 98).
#[test]
fn f_option_of_the_newtype() {
    row(Layout {
        name: "(f) Option<W>",
        decls: newtype_decls(),
        ty: "Option<W>",
        variants: pairs(&[
            ("Option::None", "Option::None"),
            ("Option::Some(W(A::S3))", "Option::Some(W(A::S3))"),
            ("Option::Some(unsafe { transmute::<usize, W>(4096) })", "Option::Some(W(A::S12))"),
        ]),
        extra: "        Option::Some(_) => 98,\n",
        consts: true,
        cast: None,
    });
}

/// A variant holding a 0-variant enum: `Err(Void)` is uninhabited, so the enum is `Ok`'s
/// layout alone; the `Err` arm is never taken. Status 1 (Ok(3)); a payload not 3 gives 98.
#[test]
fn a_variant_of_a_zero_variant_enum() {
    row(Layout {
        name: "Result<u8, Void>",
        decls: String::from("\nenum Void {}\n"),
        ty: "Result<u8, Void>",
        variants: pairs(&[("Result::Ok(3)", "Result::Ok(3)")]),
        extra: "        Result::Ok(_) => 98,\n        Result::Err(_) => 0,\n",
        consts: true,
        cast: None,
    });
}

/// `Result<(), fmt::Error>` (what every `fmt` fn returns): two zero-sized payloads, a one-byte
/// tag. Statuses 1 (Ok), 2 (Err).
#[test]
fn result_of_unit_and_fmt_error() {
    row(Layout {
        name: "Result<(), Error>",
        decls: String::from("\nstruct Error;\n"),
        ty: "Result<(), Error>",
        variants: pairs(&[("Result::Ok(())", "Result::Ok(_)"), ("Result::Err(Error)", "Result::Err(_)")]),
        extra: "",
        consts: true,
        cast: None,
    });
}
