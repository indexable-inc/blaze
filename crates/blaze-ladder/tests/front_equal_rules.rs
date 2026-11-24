//! Compare function facts over solved names and the two records written from the same solve.

#![feature(allocator_api)]

use blaze_build::{Abi, Intrinsic, OverflowChecks, Panic, intrinsics::target::Target};
use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey as LibKey};
use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front::{At, FnKind};
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::internal_symbol::{InternalRefused, definition};
use blaze_ladder::front2::program::{Front2Program, Front2Refusal, lazy::LazyProgram};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts, TableRead};
use blaze_ladder::front2::tables::task::{BodyFrame, Make, MakeRefused, check_body};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, RunError};
use blaze_ladder::seam::FrontEnd;
use blaze_mem::Frame;
use blaze_own::Erased;
use blaze_scope::package_path::PackagePath;
use blaze_scope::graph::Sysroot;
use blaze_scope::names_cells::{NameCell, Section};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_typeck::{Expr, ExprId, ItemKey, List, PortableTys};

const ROOT: &str = "src/main.rs";
const ISB: &str = "llvm.aarch64.isb";

// stdarch's arm_shared/barrier/mod.rs, with the aarch64 cfg_attr applied. The C block
// declares the same link name so that changing only the block ABI exercises the boundary.
const LLVM: &str = r#"#![feature(no_core, lang_items, link_llvm_intrinsics)]
#![no_core]
#![no_main]
#[lang = "sized"] trait Sized {}
#[lang = "copy"] trait Copy {}
unsafe extern "llvm-intrinsic" {
    #[link_name = "llvm.aarch64.isb"]
    fn isb(_: i32);
}
unsafe extern "C" {
    #[link_name = "llvm.aarch64.isb"]
    fn c_isb(_: i32);
}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe { isb(15); c_isb(15); }
    0
}
"#;

// The module-level attribute and block used in alloc/src/alloc.rs. Its allocator names
// belong to alloc_shim, so use panic_unwind's declaration and supply the definition the
// whole program's reach walk requires in a separate module.
const INTERNAL: &str = r#"#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]
#[lang = "sized"] trait Sized {}
#[lang = "copy"] trait Copy {}
unsafe extern "Rust" {
    #[rustc_std_internal_symbol]
    safe fn __rust_drop_panic() -> !;
}
mod runtime {
    #[rustc_std_internal_symbol]
    pub fn __rust_drop_panic() -> ! { loop {} }
}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 { __rust_drop_panic() }
"#;

const TRACKED: &str = r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#[lang = "sized"] trait Sized {}
#[lang = "copy"] trait Copy {}
#[lang = "panic_location"]
struct Location<'a> { file: &'a str, line: u32, col: u32 }
#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;
impl<'a> Location<'a> {
    fn plain_member() {}
    #[track_caller]
    pub const fn caller() -> &'static Location<'static> { caller_location() }
}
#[track_caller]
fn tracked_free() { plain_free(); Location::caller(); }
fn plain_free() {}

struct Token;
trait Report {
    fn explicit(&self);
    #[track_caller] fn inherited(&self);
    fn plain(&self);
    #[track_caller] fn provided(&self) {}
    #[track_caller] fn overridden(&self) {}
}
impl Report for Token {
    #[track_caller] fn explicit(&self) {}
    fn inherited(&self) {}
    fn plain(&self) {}
    fn overridden(&self) {}
}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    Location::plain_member();
    tracked_free();
    Report::explicit(&Token);
    Report::inherited(&Token);
    Report::plain(&Token);
    Report::provided(&Token);
    Report::overridden(&Token);
    0
}
"#;

/// Each fixture crate has one source file.
#[derive(Clone)]
struct KeptCrate<'a> {
    key: CrateItemKey,
    names: &'a [u8],
    items: &'a [u8],
    path: &'a str,
    text: &'a str,
    expansions: Vec<(LibKey, &'a str, &'a [u8])>,
}

struct Kept<'a>(Vec<KeptCrate<'a>>);

impl<'a> Load<'a> for Kept<'a> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> {
        Some(self.0.get(krate)?.names)
    }

    fn items(&self, krate: usize) -> Option<&'a [u8]> {
        Some(self.0.get(krate)?.items)
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
        let c = self.0.get(krate)?;
        (path.as_str() == c.path).then_some(c.text)
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'a>> {
        let row = self.0.get(krate)?.expansions.iter().find(|e| e.0 == call)?;
        let bytes = row.2;
        let origins = if bytes.is_empty() { (&[][..], &[][..]) } else { blaze_scope::expansion_origins::split(bytes).expect("the fixture expansion's origin column") };
        Some(LoadedExpansion { text: row.1, origins: Some(origins) })
    }
}

fn keep<'a>(names: &Names<'_>, frame: &'a Frame, alloc: blaze_own::Alloc<'a>) -> &'a Kept<'a> {
    let crates = names.crates().iter().map(|c| {
        let file = &c.files[0];
        KeptCrate {
            key: c.key,
            names: aligned(&names_record::encode(c, alloc).expect("names record"), blaze_own::testing::bump()),
            items: aligned(&items_record::encode_with(c, names, alloc).expect("items record"), blaze_own::testing::bump()),
            path: frame.alloc_str(file.package.as_str()),
            text: frame.alloc_str(file.text),
            expansions: c.expansions.iter().filter_map(|(call, e)| {
                use blaze_scope::expansion_origins::OriginsError;
                let made = e.made?;
                let origins: &[u8] = match made.origins {
                    Ok(bytes) => frame.alloc_slice_copy(bytes),
                    Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
                    Err(error) => panic!("origins of expansion {call:x}: {error}"),
                };
                Some((*call, &*frame.alloc_str(made.text), origins))
            }).collect(),
        }
    }).collect();
    frame.alloc(Kept(crates))
}

fn on_checker_stack<R: Send + 'static>(run: impl FnOnce() -> R + Send + 'static) -> R {
    match std::thread::Builder::new().stack_size(256 << 20).spawn(run).expect("fixture thread").join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Every field of a declaration's function facts, plus the seam questions keyed by its head.
#[derive(Debug, PartialEq, Eq)]
struct Row<'a> {
    key: ItemKey,
    path: &'a str,
    at: At,
    text: &'a str,
    kind: FnKind,
    abi: Abi,
    symbol: &'a str,
    export: Option<&'a str>,
    has_body: bool,
    const_fn: bool,
    condition: bool,
    call_abi: Option<Abi>,
    intrinsic: Option<Intrinsic>,
    nounwind: Option<bool>,
    track_caller: Option<bool>,
}

fn row(front: &dyn FrontEnd, key: ItemKey) -> Row<'_> {
    let f = front.function(front.fn_index(key).expect("the caller reached the declaration"));
    Row {
        key: f.key,
        path: f.path,
        at: f.at,
        text: f.text,
        kind: f.kind,
        abi: f.abi,
        symbol: f.symbol,
        export: f.export,
        has_body: f.body.is_some(),
        const_fn: f.const_fn,
        condition: f.condition,
        call_abi: front.fn_abi(key),
        intrinsic: front.intrinsic(key),
        nounwind: front.nounwind(key),
        track_caller: front.track_caller(key),
    }
}

fn with_programs<R: Send + 'static>(text: &'static str, inspect: impl for<'n> FnOnce(&'n Names<'n>, &dyn FrontEnd, &mut dyn FrontEnd) -> R + Send + 'static) -> R {
    let run = move || {
        let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("the fixture's frame")));
        let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
        let a = erased.handle();
        let files = [SourceFile { path: ROOT, text }];
        let tc = Toolchain::none(a);
        let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };
        let texts = ProgramTexts::new(&files, ROOT, new, &tc, Panic::Unwind).unwrap_or_else(|e| panic!("fixture texts: {e}"));
        let user = texts.user();
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("fixture inputs: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("fixture names: {e}"));
        let whole = Front2Program::new(&names, frame, &files, a).unwrap_or_else(|e| panic!("whole program: {e}"));
        let entry = whole.function(whole.entry().expect("the fixture exports main")).key;

        let c = names.krate(0);
        let kept = keep(&names, frame, a);
        let log = frame.alloc(Log::new(&["ladder"], a));
        let records = frame.alloc(Records::new(frame.alloc_slice_copy(&[c.key]), kept, log, frame, a));
        let mut lazy = LazyProgram::from_records(records, frame, &[0], Some(entry), a).unwrap_or_else(|e| panic!("lazy program: {e}"));
        lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule).unwrap_or_else(|e| panic!("lazy entry: {e}"));
        lazy.check_one(entry).unwrap_or_else(|e| panic!("checking main: {e}")).expect("main has a body");
        let result = inspect(&names, &whole, &mut lazy);
        assert!(records.errors().is_empty(), "fixture record reads: {:?}", records.errors());
        result
    };
    on_checker_stack(run)
}

fn compare(text: &'static str, name: &'static str, block: &'static str, inspect: fn(Row<'_>, Row<'_>)) {
    with_programs(text, move |names, whole, lazy| {
        let lib = names.items(0).map(|(k, _, _)| k).find(|&k| names.name_of(k) == Some(name) && names.extern_abi(k) == Some(Some(block)))
            .unwrap_or_else(|| panic!("no declaration of {name} in an extern {block} block"));
        let key = checker_of(lib);
        let (w, l) = (row(whole, key), row(lazy, key));
        assert_eq!(w, l, "row of {name}");
        assert_eq!(w.key, key);
        assert!(!w.has_body);
        assert_eq!(w.call_abi, Some(w.abi));
        assert_eq!(w.track_caller, Some(false));
        inspect(w, l);
    });
}

#[test]
fn llvm_declaration_rows_agree() {
    compare(LLVM, "isb", "llvm-intrinsic", |w, _| {
        assert_eq!((w.kind, w.symbol, w.abi, w.nounwind), (FnKind::Free, "", Abi::C, Some(true)));
        assert_eq!(w.intrinsic, Some(Intrinsic::Target(Target::Aarch64Isb)));
        println!("isb has a symbol-less C nounwind row for llvm.aarch64.isb");
    });
}

#[test]
fn c_declaration_keeps_its_link_name() {
    compare(LLVM, "c_isb", "C", |w, _| {
        assert_eq!((w.kind, w.symbol, w.abi, w.nounwind, w.intrinsic), (FnKind::Foreign, ISB, Abi::C, Some(true), None));
        println!("c_isb has the foreign symbol llvm.aarch64.isb");
    });
}

#[test]
fn internal_declaration_rows_agree() {
    compare(INTERNAL, "__rust_drop_panic", "Rust", |w, _| {
        assert_eq!((w.kind, w.symbol, w.abi, w.nounwind, w.intrinsic), (FnKind::Free, "", Abi::Rust, Some(false), None));
        println!("__rust_drop_panic has a symbol-less free Rust row");
    });
}

#[test]
fn a_missing_llvm_target_is_caught() {
    compare(LLVM, "isb", "llvm-intrinsic", |w, mut l| {
        l.intrinsic = None;
        assert_ne!(w, l, "removing the lazy target must change the row");
        println!("caught a missing lazy intrinsic for isb");
    });
}

#[test]
fn a_planted_internal_symbol_is_caught() {
    compare(INTERNAL, "__rust_drop_panic", "Rust", |w, mut l| {
        l.symbol = "__rust_drop_panic";
        assert_ne!(w, l, "giving the lazy declaration a link symbol must change the row");
        println!("caught a planted lazy link symbol for __rust_drop_panic");
    });
}

const DROP_PANIC: &str = "__rust_drop_panic";
const CALLER: &str = r#"#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]
#[lang = "sized"] trait Sized {}
#[lang = "copy"] trait Copy {}
unsafe extern "Rust" {
    #[rustc_std_internal_symbol]
    safe fn __rust_drop_panic() -> !;
}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 { __rust_drop_panic() }
"#;
const PROVIDER: &str = r#"#![feature(no_core, rustc_attrs)]
#![no_core]
mod runtime { #[rustc_std_internal_symbol] pub fn __rust_drop_panic() -> ! { loop {} } }
"#;
const DECOY: &str = r#"#![feature(no_core)]
#![no_core]
mod runtime { pub fn __rust_drop_panic() -> ! { loop {} } }
"#;
const WRONG: &str = r#"#![feature(no_core, rustc_attrs)]
#![no_core]
mod impostor { #[rustc_std_internal_symbol] pub fn __rust_drop_panic() -> ! { loop {} } }
"#;

struct FixtureFiles<'a>(&'a [(&'a str, &'static str)]);

/// Supply the real digest of the items sections used by binding. The remaining checker
/// cells need only be stable within this fixture.
impl<'a> Make<'a> for Kept<'_> {
    fn make(&self, want: &CellId, _: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        for c in &self.0 {
            let items = NameCell::Whole { krate: c.key.prefix(), section: Section::Items };
            if want.of == blaze_drive::key::crate_scope_name(&c.key) && want.cell.0 == items.key() {
                return Ok(blaze_scope::names_cells::whole_digest(Section::Items, c.items));
            }
            let syms = NameCell::InternalSymbols { krate: c.key.prefix() };
            if want.of == blaze_drive::key::crate_scope_name(&c.key) && want.cell.0 == syms.key() {
                let record = blaze_scope::names_record::NamesRecord::open(blaze_scope::names_record::Verified::check(c.names).expect("names record"));
                let text = c.text;
                let items_rec = blaze_scope::items_record::ItemsRecord::open_with_files(c.items, blaze_own::testing::heap().alloc(), move |index, _, _, edition| {
                    if index != 0 { return Ok(None); }
                    blaze_scope::items_record::CurrentFile::from_text(index, text, edition, blaze_own::testing::bump()).map(Some)
                }).expect("items record");
                let read = |of: blaze_scope::items_record::TextOf<'_>| match of {
                    blaze_scope::items_record::TextOf::File { index: 0, .. } => Some(text),
                    _ => None,
                };
                return Ok(blaze_scope::names_cells::seen_by_record(&record, Some((&items_rec, &read)), &[], syms).expect("internal symbols cell").unwrap_or(ABSENT));
            }
        }
        Ok(ContentId::of("internal binding fixture cell", &want.cell.0))
    }
}

impl<'a> SourceTexts<'a> for FixtureFiles<'_> {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        let &(root, text) = self.0.get(krate)?;
        (root == path).then_some(text)
    }
}

/// Use independent module paths for the planted definition, while keeping the declaration,
/// its caller, the crate keys and the definition's signature identical.
fn compare_binding(provider: &'static str, plant: bool) {
    on_checker_stack(move || {
        let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame")));
        let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
        let a = erased.handle();
        let files = [SourceFile { path: ROOT, text: CALLER }];
        let tc = Toolchain::none(a);
        let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };
        let texts = ProgramTexts::new(&files, ROOT, new, &tc, Panic::Unwind).expect("fixture texts");
        let user = texts.user();
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let app = inputs.crates()[0];
        let crates = [
            CrateInput { deps: &[("provider", 1)], ..app },
            CrateInput {
                name: "provider", key: CrateItemKey(ContentId::of("binding fixture crate", b"provider")),
                implicit: blaze_unit_graph::implicit(user.edition, &blaze_unit_graph::UnitKind::Lib),
                root: "/provider/src/lib.rs", shown_from: "/provider", deps: &[], ..app
            },
        ];
        let solve = |provider| {
            let roots = [(app.root, CALLER), (crates[1].root, provider)];
            resolve_crates(&crates, Sysroot::default(), &FixtureFiles(&roots), &Serial, blaze_own::testing::bump()).expect("fixture names")
        };
        let names = solve(provider);
        let lib = |krate, name| names.items(krate).map(|(k, _, _)| k).find(|&k| names.name_of(k) == Some(name)).expect("named fixture item");
        let declaration = lib(0, DROP_PANIC);
        let main = lib(0, "main");
        let entry = checker_of(main);
        let whole = Front2Program::new(&names, frame, &files, a);
        let planted;
        let from = if plant {
            planted = solve(WRONG);
            &planted
        } else {
            &names
        };
        let log = frame.alloc(Log::new(&["ladder", "provider"], a));
        let kept = keep(from, frame, a);
        let records = frame.alloc(Records::new(frame.alloc_slice_copy(&[crates[0].key, crates[1].key]), kept, log, frame, a));
        let mut lazy = LazyProgram::from_records(records, frame, &[0], Some(entry), a).expect("lazy fixture");
        lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule).expect("exported main");
        let checked = lazy.check_one(entry);

        // Empty searches read the items section too. Adding an attributed definition to the
        // provider must invalidate the answer just as moving an existing definition does.
        let recorder = Recorder::new(RecorderOpts { alloc: a });
        let view = Tables::new(TablesOpts { records, impls: None, lang_crates: &[0], reads: &recorder });
        let target = definition(view, DROP_PANIC, Panic::Unwind, a);
        let planned = Planned {
            name: Name::new(Kind::BodyCheck, main.0), phase: Phase::new(Stage::Bodies, 0),
            source: ABSENT, file: Name::new(Kind::FileItems, ABSENT),
        };
        let body = BodyFrame {
            frame, alloc: a, plant_drop: None, cost: None, needs: blaze_ladder::front2::heads::NeedsMet::All,
            heads: None, sources: None, goals: None, prepared: None,
        };
        let filed = check_body(view, &Task { planned, key: Key(ABSENT) }, &body, kept, blaze_own::testing::heap().shared())
            .expect("the caller's BodyCheck read list");
        let reads = recorder.finish().expect("binding reads");
        for c in crates {
            let syms = TableRead::Names(NameCell::InternalSymbols { krate: c.key.prefix() });
            let items = TableRead::Names(NameCell::Whole { krate: c.key.prefix(), section: Section::Items });
            assert!(lazy.internal_binding_reads(entry).contains(&syms), "lazy main must record its internal-symbol lookup in {}", c.name);
            assert!(reads.contains(&items), "lookup read {}'s items", c.name);
            assert!(filed.reads.contains(&syms), "the caller must retain the lookup's read of {}'s internal symbols", c.name);
        }
        assert!(records.errors().is_empty(), "record reads: {:?}", records.errors());
        if provider == DECOY {
            let why = InternalRefused::NoDefinition { name: DROP_PANIC.into() };
            assert_eq!(target, Err(why.clone()));
            let Err(Front2Refusal::Unchecked { function, class, first }) = whole else { panic!("whole front must refuse the missing definition") };
            let Err(RunError::Front2(Front2Refusal::Unchecked { function: lf, class: lc, first: ld })) = checked else { panic!("lazy front must refuse the missing definition") };
            let shown = blaze_ladder::front2::item_shown::ItemShown::of_lib(&names, declaration).boxed();
            assert_eq!((&*function, &*class, &*first), (&*shown, why.class(), &*why.detail()));
            assert_eq!((lf, lc, ld), (function, class, first));
            return;
        }
        let whole = whole.expect("whole fixture");
        let (i, checked) = checked.expect("lazy check").expect("main has a body");
        let &[called] = checked.callees.column::<1>() else { panic!("main calls just the declaration") };
        assert_eq!(called, checker_of(declaration));
        let mut ground = PortableTys::new_in(a);
        let w = whole.resolve(whole.entry().expect("whole main"), called, List::EMPTY, &mut ground).expect("whole binding");
        let l = lazy.resolve(i, called, List::EMPTY, &mut ground).expect("lazy binding");
        assert_eq!(w, (checker_of(lib(1, DROP_PANIC)), List::EMPTY));
        assert_eq!(l, (checker_of(target.expect("record definition")), List::EMPTY));
        if plant {
            assert_ne!(w, l, "the comparison must catch the other module's definition");
            let syms = NameCell::InternalSymbols { krate: crates[1].key.prefix() };
            let orig_names = blaze_scope::names_record::encode(names.krate(1), a).expect("original provider names");
            let orig_items = items_record::encode_with(names.krate(1), &names, a).expect("original provider items");
            let digest_of = |n_bytes: &[u8], i_bytes: &[u8], text: &'static str| {
                let record = blaze_scope::names_record::NamesRecord::open(blaze_scope::names_record::Verified::check(n_bytes).expect("names record"));
                let items_rec = blaze_scope::items_record::ItemsRecord::open_with_files(i_bytes, blaze_own::testing::heap().alloc(), move |index, _, _, edition| {
                    if index != 0 { return Ok(None); }
                    blaze_scope::items_record::CurrentFile::from_text(index, text, edition, blaze_own::testing::bump()).map(Some)
                }).expect("items record");
                let read = |of: blaze_scope::items_record::TextOf<'_>| match of {
                    blaze_scope::items_record::TextOf::File { index: 0, .. } => Some(text),
                    _ => None,
                };
                blaze_scope::names_cells::seen_by_record(&record, Some((&items_rec, &read)), &[], syms).expect("internal symbols").unwrap_or(ABSENT)
            };
            let before = digest_of(&orig_names, &orig_items, provider);
            let after = digest_of(kept.0[1].names, kept.0[1].items, kept.0[1].text);
            assert_ne!(before, after, "moving the definition must change the recorded internal-symbol cell");
            let id = CellId { of: blaze_drive::key::crate_scope_name(&crates[1].key), cell: blaze_drive::cells::Cell(syms.key()) };
            assert!(filed.done.reads.contains(&(id, after)), "the body record must carry the changed provider digest");
        } else {
            assert_eq!(w, l);
            assert!(lazy.check_one(l.0).expect("check the resolved definition").is_some());
        }
        assert!(records.errors().is_empty(), "resolution read every required record: {:?}", records.errors());
    });
}

#[test]
fn internal_call_binds_across_crates() {
    compare_binding(PROVIDER, false);
    println!("both calls reach provider::runtime::__rust_drop_panic");
}

#[test]
fn missing_internal_definition_is_refused() {
    compare_binding(DECOY, false);
    println!("both callers report the missing attributed definition of __rust_drop_panic");
}

#[test]
fn a_planted_internal_binding_is_caught() {
    compare_binding(PROVIDER, true);
    println!("caught the lazy binding to provider::impostor::__rust_drop_panic");
}

#[test]
fn panic_runtime_calls_bind_by_program_strategy() {
    const MAIN: &str = r#"#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]
#[lang = "sized"] trait Sized {}
#[lang = "copy"] trait Copy {}
unsafe extern "Rust" {
    #[rustc_std_internal_symbol] fn __rust_start_panic(payload: usize) -> u32;
    #[rustc_std_internal_symbol] fn __rust_panic_cleanup(payload: usize) -> u32;
}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe { __rust_panic_cleanup(0); __rust_start_panic(0) as i32 }
}
"#;
    const RUNTIME: &str = r#"#![feature(no_core, rustc_attrs, panic_runtime)]
#![no_core]
#![panic_runtime]
#[rustc_std_internal_symbol] pub fn __rust_start_panic(_payload: usize) -> u32 { 7 }
#[rustc_std_internal_symbol] pub fn __rust_panic_cleanup(_payload: usize) -> u32 { 9 }
"#;
    on_checker_stack(|| {
        for (panic, selected, other) in [(Panic::Abort, 2, 1), (Panic::Unwind, 1, 2)] {
            let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame")));
            let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
            let a = erased.handle();
            let files = [SourceFile { path: ROOT, text: MAIN }];
            let tc = Toolchain::none(a);
            let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };
            let texts = ProgramTexts::new(&files, ROOT, new, &tc, panic).expect("program texts");
            let user = texts.user();
            let inputs = Inputs::new(&user, &tc).expect("program inputs");
            let app = inputs.crates()[0];
            // As in panic_strategy.rs, the runtime crates keep the toolchain's cfg.
            let target = tc.program_cfg(Panic::Unwind);
            let runtime = |name: &'static str, root: &'static str| CrateInput {
                name, key: CrateItemKey(ContentId::of("runtime fixture crate", name.as_bytes())),
                implicit: blaze_unit_graph::implicit(user.edition, &blaze_unit_graph::UnitKind::Lib),
                root, shown_from: "/", deps: &[], cfg: &target, ..app
            };
            let crates = [
                CrateInput { deps: &[("panic_unwind", 1), ("panic_abort", 2)], ..app },
                runtime("panic_unwind", "/panic_unwind/src/lib.rs"),
                runtime("panic_abort", "/panic_abort/src/lib.rs"),
            ];
            let roots = [(app.root, MAIN), (crates[1].root, RUNTIME), (crates[2].root, RUNTIME)];
            let names = resolve_crates(&crates, Sysroot::default(), &FixtureFiles(&roots), &Serial, blaze_own::testing::bump()).expect("fixture names");
            let key = |krate, name: &str| {
                let (lib, _, _) = names.items(krate).find(|(lib, _, _)| names.name_of(*lib) == Some(name)).expect("named fixture item");
                checker_of(lib)
            };
            let entry = key(0, "main");
            let whole = Front2Program::new(&names, frame, &files, a).expect("whole program");
            let kept = keep(&names, frame, a);
            let log = frame.alloc(Log::new(&["ladder", "panic_unwind", "panic_abort"], a));
            let records = frame.alloc(Records::new(frame.alloc_slice_copy(&crates.map(|c| c.key)), kept, log, frame, a));
            let mut lazy = LazyProgram::from_records(records, frame, &[0], Some(entry), a).expect("lazy program");
            lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule).expect("exported main");
            let (i, checked) = lazy.check_one(entry).expect("check main").expect("main has a body");
            for symbol in ["__rust_start_panic", "__rust_panic_cleanup"] {
                let decl = key(0, symbol);
                assert!(checked.callees.column::<1>().contains(&decl));
                let expected = key(selected, symbol);
                let mut ground = PortableTys::new_in(a);
                let w = whole.resolve(whole.entry().expect("whole main"), decl, List::EMPTY, &mut ground).expect("whole binding");
                let l = lazy.resolve(i, decl, List::EMPTY, &mut ground).expect("lazy binding");
                assert_eq!(w, (expected, List::EMPTY), "{panic:?} {symbol}");
                assert_eq!(l, w, "{panic:?} {symbol}");
                assert!(whole.fn_index(key(other, symbol)).is_none(), "the other runtime is unreachable");
                lazy.check_one(l.0).expect("check the selected runtime").expect("the definition has a body");
                for front in [&whole as &dyn FrontEnd, &lazy as &dyn FrontEnd] {
                    assert!(front.function(front.fn_index(decl).expect("declaration row")).body.is_none());
                    let body = front.function(front.fn_index(expected).expect("selected definition row"));
                    assert!(body.body.is_some());
                    assert_eq!(body.key, expected, "{panic:?} {symbol}");
                    let lib = records.key_beginning(body.key.0.to_be_bytes()).expect("the definition row's library key");
                    assert_eq!(names.crate_of(lib), Some(selected), "{panic:?} {symbol}");
                }
            }
            assert!(records.errors().is_empty(), "runtime record reads: {:?}", records.errors());
        }
    });
    println!("both front ends bind start and cleanup to panic_abort for Abort and panic_unwind for Unwind");
}

/// Identify an impl by one of its methods. Both the trait and its impl declare `inherited`,
/// so a search for that method by name alone would exercise the wrong key.
fn impl_with<'n>(names: &'n Names<'n>, method: &str) -> ItemKey {
    let mut owners = names.items(0).map(|(k, _, _)| k).filter(|&k| {
        names.kind_of_item(k) == Some(blaze_surface::ItemKind::Impl)
            && names.members(k).is_some_and(|members| members.iter().any(|m| m.name == Some(method)))
    });
    let key = owners.next().unwrap_or_else(|| panic!("fixture impl declaring {method}"));
    assert!(owners.next().is_none(), "two fixture impls declare {method}");
    checker_of(key)
}

fn tracking(whole: &dyn FrontEnd, lazy: &dyn FrontEnd, key: ItemKey) -> [Option<bool>; 2] {
    [whole.track_caller(key), lazy.track_caller(key)]
}

#[test]
fn track_caller_reads_the_function_and_its_trait_declaration() {
    with_programs(TRACKED, |names, whole, lazy| {
        let item = |name: &str| {
            let (key, _, _) = names.items(0).find(|&(k, _, _)| names.name_of(k) == Some(name)).unwrap_or_else(|| panic!("fixture item {name}"));
            checker_of(key)
        };
        let location = impl_with(names, "caller");
        let report = item("Report");
        let token = impl_with(names, "inherited");
        let member = |owner, name: &str| member_key_of(owner, MemberKind::Fn, name);
        let caller = member(location, "caller");
        // main names the sibling in the same impl. Only tracked_free's body calls caller.
        assert!(lazy.fn_index(caller).is_none(), "caller must have no lazy function row yet");
        assert!(lazy.fn_index(item("plain_free")).is_none(), "plain_free must have no lazy function row yet");
        for (key, label, expected) in [
            (caller, "Location::caller before its row", true),
            (member(location, "plain_member"), "Location::plain_member", false),
            (item("tracked_free"), "tracked_free", true),
            (item("plain_free"), "plain_free", false),
            (member(report, "explicit"), "Report::explicit", false),
            (member(token, "explicit"), "impl's own attribute", true),
            (member(token, "inherited"), "required trait declaration", true),
            (member(token, "plain"), "plain trait impl method", false),
            (member(report, "provided"), "provided trait method", true),
            (member(token, "overridden"), "override of provided method", true),
        ] {
            assert_eq!(tracking(whole, lazy, key), [Some(expected); 2], "{label}");
        }
        lazy.check_one(caller).expect("checking Location::caller").expect("caller has a body");
        assert_eq!(tracking(whole, lazy, caller), [Some(true); 2], "Location::caller with its row");
        println!("both front ends track Location::caller, tracked_free, and annotated or inheriting methods");
    });
}

#[test]
fn a_missing_lazy_track_caller_is_caught() {
    with_programs(TRACKED, |names, whole, lazy| {
        let key = member_key_of(impl_with(names, "caller"), MemberKind::Fn, "caller");
        let mut answers = tracking(whole, lazy, key);
        let expected = [Some(true); 2];
        assert_eq!(answers, expected, "Location::caller is tracked");
        answers[1] = None;
        assert_ne!(answers, expected, "losing the lazy answer must fail the same comparison");
        println!("caught a missing lazy track_caller answer for Location::caller");
    });
}

#[test]
fn a_stored_caller_site_uses_the_current_file_before_building_a_body() {
    with_programs(TRACKED, |names, whole, lazy| {
        let (lib, _, _) = names.items(0).find(|&(k, _, _)| names.name_of(k) == Some("main")).unwrap();
        let key = checker_of(lib);
        let (_, start) = names.item_start(lib).expect("main is written in the fixture file");
        let call = TRACKED.find("    tracked_free();").unwrap() + 4;
        let site = u32::try_from(call).unwrap() - start;
        let line = 1 + TRACKED[..call].bytes().filter(|&b| b == b'\n').count() as u32;
        assert_eq!(whole.source_position(key, site), Ok((line, 5)));
        assert_eq!(lazy.source_position(key, site), Ok((line, 5)));

        // Use the original names and items bytes over the changed file, exactly as a
        // first-call store hit does. No program has been made over these records.
        let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).unwrap()));
        let erased = frame.alloc(Erased::new(frame));
        let a = erased.handle();
        let c = names.krate(0);
        let kept = keep(names, frame, a);
        let mut moved = kept.0.clone();
        moved[0].text = frame.alloc_str(&format!("\n\n\n{TRACKED}"));
        let kept = frame.alloc(Kept(moved));
        let log = frame.alloc(Log::new(&["ladder"], a));
        let records = frame.alloc(Records::new(frame.alloc_slice_copy(&[c.key]), kept, log, frame, a));
        assert_eq!(records.source_position(key, site, a), Ok((line + 3, 5)));
        assert!(records.errors().is_empty(), "{:?}", records.errors());
        println!("stored caller site: three lines later in the current file, still column 5");
    });
}

const SITE_HEAD: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]
#[lang = "sized"] trait Sized {}
#[lang = "copy"] trait Copy {}
impl Copy for u8 {}
#[track_caller] const fn tracked(_: u8) -> u8 { 7 }
"#;

/// Place a previously sealed site using the original names records and today's file.
fn cold_position(names: &Names<'_>, current: &'static str, item: ItemKey, offset: u32) -> Result<(u32, u32), blaze_ladder::seam::PositionGap> {
    let frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("records reader's frame")));
    let erased = frame.alloc(Erased::new(&*frame));
    let a = erased.handle();
    let mut old = keep(names, frame, a).0.clone();
    old[0].text = current;
    let kept = frame.alloc(Kept(old));
    let keys = frame.alloc_slice_copy(&[names.krate(0).key]);
    let log = frame.alloc(Log::new(&["ladder"], a));
    let records = frame.alloc(Records::new(keys, kept, log, frame, a));
    let result = records.source_position(item, offset, a);
    if result.is_ok() {
        assert!(records.errors().is_empty(), "{:?}", records.errors());
    }
    result
}

/// Each fixture has exactly one direct call in the body under inspection. The expected
/// coordinates come from the written program, independently of the lowering and either
/// front's idea of the beginning of the item.
fn check_site(decls: &str, main: &str, owner: &'static str, written: &'static str, insert_before: &str) {
    let text = format!("{SITE_HEAD}{decls}\n#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 {{ {main} }}");
    let insert = text.find(insert_before).expect("edit anchor");
    let mut moved = text.clone();
    moved.insert_str(insert, "\n\n\n");
    let inspect = |text: &'static str, current: &'static str, shift: u32| with_programs(text, move |names, whole, lazy| {
        let at = text.find(written).expect("the invocation is written in the fixture");
        assert_eq!(text.matches(written).count(), 1);
        let prefix = &text[..at];
        let line = 1 + prefix.bytes().filter(|&b| b == b'\n').count() as u32;
        let col = 1 + prefix.rsplit('\n').next().unwrap().chars().count() as u32;
        let ident = owner.split_whitespace().last().expect("fixture owner name");
        let mut owners = (0..whole.fn_count()).map(|i| whole.function(i))
            .filter(|f| f.body.is_some() && f.path.rsplit("::").next() == Some(ident));
        let body = owners.next().unwrap_or_else(|| panic!("no live body for {owner}"));
        assert!(owners.next().is_none(), "ambiguous owner {owner}");
        if let Some((outer, _, _)) = names.items(0).find(|&(k, _, _)| names.name_of(k) == Some("outer") && names.kind_of_item(k) == Some(blaze_surface::ItemKind::Function)) {
            lazy.check_one(checker_of(outer)).expect("check the enclosing body that declares the local item").expect("enclosing body");
        }
        lazy.check_one(body.key).expect("check the caller").expect("the caller has a body");
        let site = |label: &str, front: &dyn FrontEnd| {
            let row = front.function(front.fn_index(body.key).expect("owner row"));
            let exprs = &row.body.expect("live caller").0.exprs;
            let mut calls = exprs.iter().enumerate().filter(|(_, e)| matches!(e, Expr::Call { .. }));
            let (i, _) = calls.next().expect("a direct call to tracked");
            assert!(calls.next().is_none(), "only the tracked call belongs to this body");
            let s = front.caller_site(row.key, ExprId(i as u32)).unwrap_or_else(|gap| panic!("{label}: call in {owner} has no source site: {gap}"));
            assert!(s.file.ends_with(ROOT), "shown path {}", s.file);
            if written.starts_with("tracked(") {
                let start = prefix.rfind(owner).expect("written declaration containing the tracked call");
                assert_eq!(s.offset as usize, at - start, "{label}: offset from the written declaration");
            }
            if owner == "const SAMPLE" || owner == "static SAMPLE" {
                let (lib, _, _) = names.items(0).find(|&(k, _, _)| names.name_of(k) == Some(ident)).expect("initializer item");
                assert_eq!(s.item, checker_of(lib), "the initializer owns its tracked call");
            }
            assert_eq!(front.source_position(s.item, s.offset), Ok((line, col)), "{label}: {owner}");
            (s.file.to_owned(), s.item, s.offset)
        };
        let saved = site("lazy", lazy);
        assert_eq!(whole.source_position(saved.1, saved.2), Ok((line, col)), "site from a different front before this front produced it");
        assert_eq!(site("whole", whole), saved);
        assert_eq!(cold_position(names, current, saved.1, saved.2), Ok((line + shift, col)), "cold publication");
        (saved, line, col)
    });
    let text = Box::leak(text.into_boxed_str());
    let moved = Box::leak(moved.into_boxed_str());
    let (saved, line, col) = inspect(text, moved, 3);
    let after = inspect(moved, moved, 0);
    assert_eq!(after, (saved, line + 3, col));
    println!("{owner}: both fronts and cold records place the call three lines later in the same column");
}

#[test]
fn a_free_function_call_site_moves_with_its_item() {
    check_site("fn sample() -> u8 { tracked(1) }", "sample() as i32", "fn sample", "tracked(1)", "fn sample");
}

#[test]
fn an_inherent_method_call_site_starts_at_the_method() {
    check_site("struct S;\nimpl S {\n    fn sibling() {}\n    fn sample() -> u8 { tracked(2) }\n}",
        "S::sample() as i32", "fn sample", "tracked(2)", "    fn sample");
}

#[test]
fn a_member_offset_applied_to_the_impl_start_is_caught() {
    let text = Box::leak(format!("{SITE_HEAD}struct S;\nimpl S {{\n    fn other() {{}}\n    fn sample() -> u8 {{ tracked(2) }}\n}}\n#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 {{ S::sample() as i32 }}").into_boxed_str());
    with_programs(text, |names, whole, lazy| {
        let imp = impl_with(names, "sample");
        let key = member_key_of(imp, MemberKind::Fn, "sample");
        let (_, checked) = lazy.check_one(key).expect("check the method").expect("method body");
        let &[call] = checked.callees.column::<0>() else { panic!("sample calls tracked once") };
        let site = lazy.caller_site(key, call).expect("method's call site");
        let offset = "fn sample() -> u8 { ".len() as u32;
        let expected = (SITE_HEAD.lines().count() as u32 + 4, 5 + offset);
        for (label, front) in [("whole", whole), ("lazy", &*lazy)] {
            assert_eq!(front.source_position(site.item, site.offset), Ok(expected), "{label}: method's own declaration");
            let planted = front.source_position(imp, offset).expect("the impl has a start");
            assert_ne!(planted, expected, "using the impl's start must change the answer");
        }
        println!("caught the method offset placed at the impl's first byte");
    });
}

#[test]
fn a_trait_impl_method_call_site_starts_at_the_method() {
    check_site("struct S;\ntrait T { fn sample(&self) -> u8; }\nimpl T for S {\n    fn sample(&self) -> u8 { tracked(3) }\n}",
        "T::sample(&S) as i32", "fn sample", "tracked(3)", "    fn sample");
}

#[test]
fn a_provided_method_call_site_starts_at_the_method() {
    check_site("struct S;\ntrait T {\n    fn sample(&self) -> u8 { tracked(4) }\n}\nimpl T for S {}",
        "T::sample(&S) as i32", "fn sample", "tracked(4)", "    fn sample");
}

#[test]
fn a_closures_call_site_uses_its_enclosing_method() {
    check_site("struct S;\nimpl S {\n    fn sample() { let _f = || tracked(5); }\n}",
        "S::sample(); 0", "fn sample", "tracked(5)", "    fn sample");
}

#[test]
fn an_async_blocks_call_site_uses_its_enclosing_method() {
    check_site("struct S;\nimpl S {\n    fn sample() { let _f = async { tracked(6) }; }\n}",
        "S::sample(); 0", "fn sample", "tracked(6)", "    fn sample");
}

#[test]
fn a_nested_functions_call_site_uses_its_own_body() {
    check_site("fn outer() -> u8 {\n    fn sample() -> u8 { tracked(7) }\n    sample()\n}",
        "outer() as i32", "fn sample", "tracked(7)", "    fn sample");
}

/// curve25519-dalek 4.1.3 field.rs:116's shape: a fn local to a body, two doc comments and
/// an attribute above it. The lowering ranges it from its first `///`, the text scan from its
/// `#[inline(always)]`; before the fix no declaration matched and the call had no site
/// (`Adts::caller_site`, tempo's tests t1530). Its site counts from the attribute (seam_rules
/// local_position.rs `first_token`), warm, from the other front and cold, and moves with an
/// edit above it. The twin is [`a_documented_nested_functions_call_site_uses_its_own_body`];
/// the control, with no comment, [`a_nested_functions_call_site_uses_its_own_body`].
#[test]
fn a_documented_attributed_nested_functions_call_site_uses_its_own_body() {
    check_site("fn outer() -> u8 {\n    /// Helper to multiply.\n    /// Two lines.\n    #[inline(always)]\n    fn sample() -> u8 { tracked(17) }\n    sample()\n}",
        "outer() as i32", "#[inline(always)]\n    fn sample", "tracked(17)", "    /// Helper");
}

/// A fn declared in a body is printed by the path of the item whose body declares it and its
/// own name, as rustc spells a local item (`outer::sample`): front2 item_shown.rs
/// `ItemShown::of_in`. tempo t1530's NOT RUN lines named curve25519-dalek's `fn m` (local to
/// `<&FieldElement51 as Mul>::mul`) "an item the names source holds no path for". The control
/// is the cause removed: before the enclosing body is checked the store holds no local row
/// for it, and the printer has no path (`None`), as every body-local item had before the fix.
#[test]
fn a_body_local_fn_is_named_by_its_enclosing_items_path() {
    let text = format!("{SITE_HEAD}fn outer() -> u8 {{\n    fn sample() -> u8 {{ 1 }}\n    sample()\n}}\n#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 {{ outer() as i32 }}");
    let text: &'static str = Box::leak(text.into_boxed_str());
    with_programs(text, |names, whole, lazy| {
        let local = (0..whole.fn_count()).map(|i| whole.function(i)).find(|f| f.path.rsplit("::").next() == Some("sample")).expect("the local fn's row in the whole program");
        let key = local.key;
        assert_eq!(lazy.item_named(key), None, "control: no local row before its enclosing body is checked");
        let (outer, _, _) = names.items(0).find(|&(k, _, _)| names.name_of(k) == Some("outer") && names.kind_of_item(k) == Some(blaze_surface::ItemKind::Function)).expect("outer");
        lazy.check_one(checker_of(outer)).expect("check the enclosing body").expect("outer has a body");
        let named = lazy.item_named(key).expect("a body-local fn has a path through its enclosing item");
        println!("BODY-LOCAL FN NAMED: {named}");
        assert!(named.ends_with("outer::sample"), "rustc's spelling of a local item: {named}");
    });
}

/// The twin: a doc comment and no attribute; the site counts from `fn`.
#[test]
fn a_documented_nested_functions_call_site_uses_its_own_body() {
    check_site("fn outer() -> u8 {\n    /** Block doc. */\n    /// Line doc.\n    fn sample() -> u8 { tracked(18) }\n    sample()\n}",
        "outer() as i32", "fn sample", "tracked(18)", "    /** Block");
}

#[test]
fn a_method_in_a_function_local_impl_uses_its_written_declaration() {
    check_site("fn outer() -> u8 {\n    struct S;\n    impl S {\n        fn sample() -> u8 { tracked(13) }\n    }\n    S::sample()\n}",
        "outer() as i32", "fn sample", "tracked(13)", "        fn sample");
}

#[test]
fn an_edit_inside_outer_moves_the_local_closures_caller() {
    check_site("fn outer() {\n    fn sample() { let _f = || tracked(14); }\n    sample()\n}",
        "outer(); 0", "fn sample", "tracked(14)", "    fn sample");
}

#[test]
fn an_edit_inside_a_local_impl_moves_its_methods_closure() {
    check_site("fn outer() {\n    struct S;\n    impl S {\n        fn sibling() {}\n        fn sample() { let _f = || tracked(15); }\n    }\n    S::sample()\n}",
        "outer(); 0", "fn sample", "tracked(15)", "        fn sample");
}

#[test]
fn removing_a_function_local_method_refuses_its_stored_site() {
    let text: &'static str = Box::leak(format!("{SITE_HEAD}fn outer() -> u8 {{ struct S; impl S {{ fn sample() -> u8 {{ tracked(16) }} }} S::sample() }}\n#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 {{ outer() as i32 }}").into_boxed_str());
    let changed = Box::leak(text.replace("sample", "sibling").into_boxed_str());
    with_programs(text, move |names, _, _| {
        let member = names.items(0).find_map(|(lib, _, _)| {
            names.members(lib)?.iter().find(|m| m.name == Some("sample")).map(|m| m.key)
        }).expect("the original method row");
        let offset = "fn sample() -> u8 { ".len() as u32;
        let col = text.lines().find_map(|line| line.find("tracked(16)")).expect("original call") as u32 + 1;
        assert_eq!(cold_position(names, text, checker_of(member), offset), Ok((SITE_HEAD.lines().count() as u32 + 1, col)));
        assert_eq!(cold_position(names, changed, checker_of(member), offset), Err(blaze_ladder::seam::PositionGap::NoItem));
        println!("removed local method: NoItem for its stored site");
    });
}

#[test]
fn an_expanded_calls_site_is_its_outermost_invocation() {
    check_site("macro_rules! inner { () => { tracked(8) } }\nmacro_rules! outer { () => { inner!() } }\nfn sample() -> u8 { outer!() }",
        "sample() as i32", "fn sample", "outer!()", "fn sample");
}

#[test]
fn a_macro_generated_functions_call_site_is_the_item_invocation() {
    check_site("macro_rules! define { () => { fn sample() -> u8 { tracked(9) } } }\ndefine!();",
        "sample() as i32", "fn sample", "define!()", "define!();");
}

#[test]
fn a_macro_generated_methods_call_site_is_the_member_invocation() {
    check_site("macro_rules! define_method { () => { fn sample() -> u8 { tracked(12) } } }\nstruct S;\nimpl S {\n    define_method!();\n}",
        "S::sample() as i32", "fn sample", "define_method!()", "    define_method!();");
}

// A closure gives the whole-program census a live owner row. Its value is the
// empty environment. Only the direct const fn call supplies the initializer's value.
#[test]
fn a_const_initializers_call_site_uses_the_constant() {
    check_site("const SAMPLE: u8 = { let _ = || (); tracked(10) };",
        "SAMPLE as i32", "const SAMPLE", "tracked(10)", "const SAMPLE");
}

#[test]
fn a_static_initializers_call_site_uses_the_static() {
    check_site("static SAMPLE: u8 = { let _ = || (); tracked(11) };",
        "SAMPLE as i32", "static SAMPLE", "tracked(11)", "static SAMPLE");
}

#[test]
fn pure_initializers_place_their_calls_without_a_runtime_function_row() {
    let inspect = |space: &'static str, shift: u32| {
        let text = Box::leak(format!("{SITE_HEAD}{space}const C: u8 = tracked(1);\nstatic S: u8 = tracked(2);\n#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 {{ let _ = C; S as i32 }}").into_boxed_str());
        with_programs(text, move |names, whole, lazy| {
            ["C", "S"].map(|name| {
                let (lib, _, _) = names.items(0).find(|&(lib, _, _)| names.name_of(lib) == Some(name)).expect("initializer");
                let key = checker_of(lib);
                let (_, checked) = lazy.check_one(key).expect("check initializer as a body").expect("initializer body");
                let &[call] = checked.callees.column::<0>() else { panic!("one tracked call in {name}") };
                let site = lazy.caller_site(key, call).expect("initializer call site");
                assert!(site.file.ends_with(ROOT));
                let (line, col) = match name {
                    "C" => (1, "const C: u8 = ".len() + 1),
                    _ => (2, "static S: u8 = ".len() + 1),
                };
                let expected = (SITE_HEAD.lines().count() as u32 + line + shift, col as u32);
                assert_eq!(whole.source_position(site.item, site.offset), Ok(expected));
                assert_eq!(lazy.source_position(site.item, site.offset), Ok(expected));
                (site.file.to_owned(), site.item, site.offset)
            })
        })
    };
    assert_eq!(inspect("", 0), inspect("\n\n\n", 3));
    println!("const and static initializer calls move by three lines with identical stored sites");
}
