//! Compare a specific declaration's row after checking its caller. The whole program reads
//! solved names and the lazy program reads the two records written from that same solve.

#![feature(allocator_api)]

use blaze_build::{Abi, Intrinsic, OverflowChecks, Panic, intrinsics::target::Target};
use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey as LibKey};
use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front::{At, FnKind};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::internal_symbol::{InternalRefused, definition};
use blaze_ladder::front2::program::{Front2Program, Front2Refusal, lazy::LazyProgram};
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
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
use blaze_typeck::{ItemKey, List, PortableTys};

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

/// Each fixture crate has one file and no expansions.
struct KeptCrate<'a> {
    key: CrateItemKey,
    names: &'a [u8],
    items: &'a [u8],
    path: &'a str,
    text: &'a str,
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

    fn expansion(&self, _: usize, _: LibKey) -> Option<&'a str> {
        None
    }

    fn expansion_origins(&self, _: usize, _: LibKey) -> Option<(&'a [u8], &'a [u8])> {
        None
    }
}

fn keep<'a>(names: &Names<'_>, frame: &'a Frame, alloc: blaze_own::Alloc<'a>) -> &'a Kept<'a> {
    let crates = names.crates().iter().map(|c| {
        let file = &c.files[0];
        KeptCrate {
            key: c.key,
            names: aligned(&names_record::encode(c, alloc).expect("names record"), alloc),
            items: aligned(&items_record::encode_with(c, names, alloc).expect("items record"), alloc),
            path: frame.alloc_str(file.package.as_str()),
            text: frame.alloc_str(file.text),
        }
    }).collect();
    frame.alloc(Kept(crates))
}

fn on_checker_stack(run: impl FnOnce() + Send + 'static) {
    if let Err(panic) = std::thread::Builder::new().stack_size(256 << 20).spawn(run).expect("fixture thread").join() {
        std::panic::resume_unwind(panic);
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

fn row(front: &impl FrontEnd, key: ItemKey) -> Row<'_> {
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

fn compare(text: &'static str, name: &'static str, block: &'static str, inspect: fn(Row<'_>, Row<'_>)) {
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
        let names = inputs.resolve(&crates, a).unwrap_or_else(|e| panic!("fixture names: {e}"));
        let lib = names.items(0).map(|(k, _, _)| k).find(|&k| names.name_of(k) == Some(name) && names.extern_abi(k) == Some(Some(block)))
            .unwrap_or_else(|| panic!("no declaration of {name} in an extern {block} block"));
        let key = checker_of(lib);
        let whole = Front2Program::new(&names, frame, &files, a).unwrap_or_else(|e| panic!("whole program: {e}"));
        let entry = whole.function(whole.entry().expect("the fixture exports main")).key;

        let c = names.krate(0);
        let kept = keep(&names, frame, a);
        let log = frame.alloc(Log::new(&["ladder"], a));
        let records = frame.alloc(Records::new(frame.alloc_slice_copy(&[c.key]), kept, log, frame, a));
        let mut lazy = LazyProgram::from_records(records, frame, &[0], Some(entry), a).unwrap_or_else(|e| panic!("lazy program: {e}"));
        lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule).unwrap_or_else(|e| panic!("lazy entry: {e}"));
        lazy.check_one(entry).unwrap_or_else(|e| panic!("checking main: {e}")).expect("main has a body");
        let (w, l) = (row(&whole, key), row(&lazy, key));
        assert!(records.errors().is_empty(), "record reads for {name}: {:?}", records.errors());
        assert_eq!(w, l, "row of {name}");
        assert_eq!(w.key, key);
        assert!(!w.has_body);
        assert_eq!(w.call_abi, Some(w.abi));
        assert_eq!(w.track_caller, Some(false));
        inspect(w, l);
    };
    on_checker_stack(run);
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
            resolve_crates(&crates, Sysroot::default(), &FixtureFiles(&roots), &Serial, a).expect("fixture names")
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
            heads: None, sources: None, goals: None,
        };
        let filed = check_body(view, &Task { planned, key: Key(ABSENT) }, &body, kept, blaze_own::testing::heap().shared())
            .expect("the caller's BodyCheck read list");
        let reads = recorder.finish().expect("binding reads");
        for c in crates {
            let items = TableRead::Names(NameCell::Whole { krate: c.key.prefix(), section: Section::Items });
            assert!(lazy.internal_binding_reads(entry).contains(&items), "lazy main must record its lookup in {}", c.name);
            assert!(reads.contains(&items), "lookup read {}'s items", c.name);
            assert!(filed.reads.contains(&items), "the caller must retain the lookup's read of {}'s items", c.name);
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
            let original = items_record::encode_with(names.krate(1), &names, a).expect("original provider items");
            let before = blaze_scope::names_cells::whole_digest(Section::Items, &original);
            let after = blaze_scope::names_cells::whole_digest(Section::Items, kept.0[1].items);
            assert_ne!(before, after, "moving the definition must change the recorded input");
            let cell = NameCell::Whole { krate: crates[1].key.prefix(), section: Section::Items };
            let id = CellId { of: blaze_drive::key::crate_scope_name(&crates[1].key), cell: blaze_drive::cells::Cell(cell.key()) };
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
            let names = resolve_crates(&crates, Sysroot::default(), &FixtureFiles(&roots), &Serial, a).expect("fixture names");
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
