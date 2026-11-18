//! Compare a specific declaration's row after checking its caller. The whole program reads
//! solved names and the lazy program reads the two records written from that same solve.

use blaze_build::{Abi, Intrinsic, OverflowChecks, Panic, intrinsics::target::Target};
use blaze_id::ItemKey as LibKey;
use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front::{At, FnKind};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::program::{Front2Program, lazy::LazyProgram};
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_ladder::seam::FrontEnd;
use blaze_mem::Frame;
use blaze_own::Erased;
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_typeck::ItemKey;

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

/// Each fixture has one file, one crate and no expansions.
struct Kept<'a> {
    names: &'a [u8],
    items: &'a [u8],
    text: &'a str,
}

impl<'a> Load<'a> for Kept<'a> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> {
        (krate == 0).then_some(self.names)
    }

    fn items(&self, krate: usize) -> Option<&'a [u8]> {
        (krate == 0).then_some(self.items)
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
        (krate == 0 && path.as_str() == ROOT).then_some(self.text)
    }

    fn expansion(&self, _: usize, _: LibKey) -> Option<&'a str> {
        None
    }

    fn expansion_origins(&self, _: usize, _: LibKey) -> Option<(&'a [u8], &'a [u8])> {
        None
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
        let kept = frame.alloc(Kept {
            names: aligned(&names_record::encode(c, a).expect("names record"), a),
            items: aligned(&items_record::encode_with(c, &names, a).expect("items record"), a),
            text,
        });
        let log = frame.alloc(Log::new(&["ladder"], a));
        let records = frame.alloc(Records::new(frame.alloc_slice_copy(&[c.key]), kept, log, frame, a));
        let mut lazy = LazyProgram::from_records(records, frame, &[0], Some(entry), a).unwrap_or_else(|e| panic!("lazy program: {e}"));
        lazy.decide_start().unwrap_or_else(|e| panic!("lazy entry: {e}"));
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
    // The checker uses the same stack allowance as front_equal.rs.
    if let Err(panic) = std::thread::Builder::new().stack_size(256 << 20).spawn(run).expect("fixture thread").join() {
        std::panic::resume_unwind(panic);
    }
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
