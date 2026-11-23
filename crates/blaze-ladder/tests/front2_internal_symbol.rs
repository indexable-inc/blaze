//! The panic runtime bound by name (front2/internal_symbol.rs): a call of a fn declared in an
//! `extern "Rust"` block with `#[rustc_std_internal_symbol]` is a direct call of its one
//! definition in the program's graph, and for the panic runtime's two names the definition is
//! the one in the `#![panic_runtime]` crate the program's strategy names: `panic_unwind` built
//! `panic = "unwind"`, `panic_abort` built `panic = "abort"`. It needs no toolchain: the
//! programs are three `#![no_core]` crates written as files.
//!
//! The pinned std (library/std/src/panicking.rs, nightly-2026-10-27, lines 47-64):
//! ```text
//! // The standard library is tagged with `#![needs_panic_runtime]` (introduced in
//! // RFC 1513) to indicate that it requires some other crate tagged with
//! // `#![panic_runtime]` to exist somewhere. Each panic runtime is intended to
//! // implement these symbols (with the same signatures) so we can get matched up
//! // to them.
//! ..
//! unsafe extern "Rust" {
//!     #[rustc_std_internal_symbol]
//!     fn __rust_panic_cleanup(payload: *mut u8) -> Box<dyn Any + Send + 'static>;
//!     #[rustc_std_internal_symbol]
//!     safe fn __rust_start_panic(payload: &mut dyn PanicPayload) -> u32;
//! }
//! ```
//! The pinned panic_unwind (library/panic_unwind/src/lib.rs) says `#![panic_runtime]` (line 23)
//! and defines (lines 88-92) `#[rustc_std_internal_symbol] pub fn __rust_start_panic(payload:
//! &mut dyn PanicPayload) -> u32 { imp::panic(payload) }`; the pinned panic_abort
//! (library/panic_abort/src/lib.rs) says `#![panic_runtime]` (line 9) and defines the same name.
//! The pinned std (library/std/src/lib.rs, line 239): `#![needs_panic_runtime]`, "Tell the
//! compiler to link to either panic_abort or panic_unwind". The Rust Reference,
//! [panic.strategy.intro]: "When generating a binary, dylib, cdylib, or staticlib and linking
//! with std, the `-C panic` CLI flag also influences which panic handler is used."
//!
//! Here the runtimes' payload is a `usize` and each returns its own number (panic_unwind 7,
//! panic_abort 9), so which one a call runs is visible in the item it resolves to.
//!
//! The lines these tests print when they pass:
//! ```text
//! front2_internal_symbol: unwind binds panic_unwind, abort binds panic_abort
//! test unwind_binds_panic_unwind_abort_binds_panic_abort ... ok
//! NO PANIC RUNTIME: `__rust_start_panic`: the program is built `panic = "unwind"` and no `#![panic_runtime]` crate `panic_unwind` is in its graph
//! test a_graph_without_the_runtime_is_refused_by_name ... ok
//! ```
//! What a wrong fix fails: without the piece the call resolves to a foreign symbol
//! `__rust_start_panic` (the `no foreign __rust_start_panic` assertion), and panic_unwind's
//! definition is never tabled (`tabled with its body`); a binding by name alone (any fn named
//! so) fails `two candidates` (both runtimes define it) or picks the wrong crate; a binding by
//! the crate attribute alone fails the abort case; a binding by the crate name alone fails the
//! impostor case (a crate named `panic_unwind` without `#![panic_runtime]`); a silent switch to
//! the other runtime fails `NoPanicRuntime`.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_build::Panic;
use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{Toolchain, syntax_of};
use blaze_ladder::front::FnKind;
use blaze_ladder::front2::internal_symbol::{InternalRefused, START_PANIC, definition};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};
use blaze_ladder::seam::FrontEnd;
use blaze_own::Alloc;
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_unit_graph::implicit;
use edition::PackageEdition;

/// The program: the lang items a `#![no_core]` program declares, std's declaration as std
/// writes it (the payload a `usize` here), and a `main` that calls it.
const ROOT: &str = "#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

unsafe extern \"Rust\" {
    #[rustc_std_internal_symbol]
    fn __rust_start_panic(payload: usize) -> u32;
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    unsafe { __rust_start_panic(0) as i32 }
}
";

/// panic_unwind as the library ships it, its runtime entry returning 7.
const UNWIND: &str = "#![feature(no_core, rustc_attrs, panic_runtime)]
#![no_core]
#![panic_runtime]

#[rustc_std_internal_symbol]
pub fn __rust_start_panic(_payload: usize) -> u32 {
    7
}
";

/// panic_abort as the library ships it, its runtime entry returning 9.
const ABORT: &str = "#![feature(no_core, rustc_attrs, panic_runtime)]
#![no_core]
#![panic_runtime]

#[rustc_std_internal_symbol]
pub fn __rust_start_panic(_payload: usize) -> u32 {
    9
}
";

/// A crate NAMED `panic_unwind` whose root does not say `#![panic_runtime]`: not a runtime.
const IMPOSTOR: &str = "#![feature(no_core, rustc_attrs)]
#![no_core]

#[rustc_std_internal_symbol]
pub fn __rust_start_panic(_payload: usize) -> u32 {
    5
}
";

/// The texts: the program's files (crate 0), then each library crate's one root file.
struct Files<'t> {
    user: &'t [(String, String)],
    libs: &'t [(String, &'t str)],
}

impl<'t> SourceTexts<'t> for Files<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        if krate == 0 {
            return self.user.iter().find(|(p, _)| p == path).map(|(_, t)| t.as_str());
        }
        let (root, text) = self.libs.get(krate - 1)?;
        (root.as_str() == path).then_some(*text)
    }
}

/// Resolves the program `ROOT` over the library crates `libs` (name, root text; crate `i + 1`
/// each, every one a dependency of the program, as std depends on both runtimes), and hands
/// the names (`&Names`) and the program's files (`&[SourceFile]`) to the body, inline: a
/// macro, so the names' borrow is the body's own scope, as in front2_alloc_shim.rs.
macro_rules! with_program {
    ($libs:expr, |$names:ident, $files:ident| $body:block) => {{
    let libs: &[(&str, &str)] = $libs;
    let files = [SourceFile { path: "src/main.rs", text: ROOT }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    // The program's strategy, Unwind (Rust's default): front2 reads it back from this cfg.
    let cfg = tc.program_cfg(Panic::Unwind);
    let roots: Vec<(String, &str)> = libs.iter().map(|(name, text)| (format!("/{name}/src/lib.rs"), *text)).collect();
    let deps: Vec<(&str, usize)> = libs.iter().enumerate().map(|(i, (name, _))| (*name, i + 1)).collect();
    let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    let lib = implicit(rust, &blaze_unit_graph::UnitKind::Lib);
    let mut crates = Vec::with_capacity(libs.len() + 1);
    crates.push(CrateInput {
        name: user.name,
        key: CrateItemKey(ContentId::of("blaze program crate", user.name.as_bytes())),
        edition: syntax_of(user.edition),
        implicit: implicit(user.edition, &blaze_unit_graph::UnitKind::Lib),
        root: user.root,
        shown_from: user.dir,
        features: &[],
        deps: &deps,
        cfg: &cfg,
        script: None,
    });
    for (i, (name, _)) in libs.iter().enumerate() {
        crates.push(CrateInput {
            name: *name,
            key: CrateItemKey(ContentId::of("front2_internal_symbol test crate", name.as_bytes())),
            edition: syntax_of(rust),
            implicit: lib,
            root: &roots[i].0,
            shown_from: "/",
            features: &[],
            deps: &[],
            cfg: &cfg,
            script: None,
        });
    }
    let sources = Files { user: user.files, libs: &roots };
    let names = resolve_crates(&crates, Sysroot::default(), &sources, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let $names = &names;
    let $files = &files[..];
    $body
    }};
}

/// The crate the definition `name` binds to under `panic`, by its name.
fn bound_crate(names: &Names<'_>, panic: Panic) -> Result<String, InternalRefused> {
    let key = definition(names, START_PANIC, panic, Alloc::global())?;
    let krate = names.crate_of(key).expect("a definition's crate");
    Ok(names.krate(krate).name.to_string())
}

/// Built Unwind, the call of `__rust_start_panic` resolves to panic_unwind's item; built
/// Abort, to panic_abort's; both graphs hold both runtimes, as the library's std depends on
/// both (library/std/Cargo.toml: `panic_unwind` optional behind `panic-unwind`, `panic_abort`
/// always). End to end (the strategy at the call sites is Unwind until P1 threads the
/// program's): the walk queues panic_unwind's definition and tables it with its body, the
/// declaration is no foreign symbol, and every body builds.
///
/// Prints: `front2_internal_symbol: unwind binds panic_unwind, abort binds panic_abort` and
/// `test unwind_binds_panic_unwind_abort_binds_panic_abort ... ok`
#[test]
fn unwind_binds_panic_unwind_abort_binds_panic_abort() {
    with_program!(&[("panic_unwind", UNWIND), ("panic_abort", ABORT)], |names, files| {
        assert_eq!(bound_crate(names, Panic::Unwind).as_deref(), Ok("panic_unwind"), "unwind binds panic_unwind");
        assert_eq!(bound_crate(names, Panic::Abort).as_deref(), Ok("panic_abort"), "abort binds panic_abort");

        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
        let mut left_out = Vec::new_in(Global);
        let mut program = Front2Program::survey(names, &frame, files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
        let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
        assert!(left.is_empty(), "front2 tables every fn the walk reaches; left out: {left:?}");
        // Each row with the crate it comes from, asked by the crate's identity: the crate whose
        // items hold the row's key (a row's path carries no crate segment).
        let crate_of_row = |key: blaze_typeck::ItemKey| {
            names.crates().iter().enumerate().find(|(krate, _)| names.items(*krate).any(|(lib, _, _)| checker_of(lib) == key)).map(|(_, c)| c.name.to_string())
        };
        let rows: Vec<(String, bool, FnKind, String, Option<String>)> = (0..program.fn_count())
            .map(|i| {
                let f = program.function(i);
                (f.path.to_string(), f.body.is_some(), f.kind, f.symbol.to_string(), crate_of_row(f.key))
            })
            .collect();
        let foreign: Vec<&(String, bool, FnKind, String, Option<String>)> =
            rows.iter().filter(|(_, _, kind, symbol, _)| *kind == FnKind::Foreign && symbol == START_PANIC).collect();
        assert!(foreign.is_empty(), "no foreign __rust_start_panic: {rows:?}");
        let defined: Vec<Option<&str>> = rows.iter().filter(|(path, body, ..)| *body && path.ends_with(START_PANIC)).map(|(.., krate)| krate.as_deref()).collect();
        assert_eq!(defined.len(), 1, "one runtime entry tabled with its body: {rows:?}");
        assert_eq!(defined[0], Some("panic_unwind"), "panic_unwind's `__rust_start_panic` tabled with its body, not panic_abort's: {rows:?}");
        let mut refused = Vec::new_in(Global);
        let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the survey stopped: {e}"));
        let refusals: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
        assert!(refusals.is_empty(), "every body builds, the call bound to a known instance: {refusals:?}");
        assert_eq!((counts.built, counts.tried), (2, 2), "`main` and panic_unwind's `__rust_start_panic`");
    });
    println!("front2_internal_symbol: unwind binds panic_unwind, abort binds panic_abort");
}

/// A graph with neither runtime is `NoPanicRuntime` for both strategies, and so is one whose
/// only `panic_unwind` is an impostor (named so, no `#![panic_runtime]`: rustc picks the
/// runtime by the attribute AND the name); one with only panic_abort is refused built Unwind,
/// never switched to abort. The walk files the refusal against the declaration by its class.
///
/// Prints: `NO PANIC RUNTIME: `__rust_start_panic`: the program is built `panic = "unwind"`
/// and no `#![panic_runtime]` crate `panic_unwind` is in its graph` and
/// `test a_graph_without_the_runtime_is_refused_by_name ... ok`
#[test]
fn a_graph_without_the_runtime_is_refused_by_name() {
    with_program!(&[], |names, files| {
        assert_eq!(bound_crate(names, Panic::Unwind), Err(InternalRefused::NoPanicRuntime { strategy: Panic::Unwind }));
        assert_eq!(bound_crate(names, Panic::Abort), Err(InternalRefused::NoPanicRuntime { strategy: Panic::Abort }));
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
        let mut left_out = Vec::new_in(Global);
        let made = Front2Program::survey(names, &frame, files, &mut left_out, blaze_own::Alloc::global());
        let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
        let refusal = match made {
            Err(e) => format!("{e}"),
            Ok(_) => left.join("; "),
        };
        let class = "the program is built `panic = \"unwind\"` and no `#![panic_runtime]` crate `panic_unwind` is in its graph";
        assert!(refusal.contains(class), "refused by name: {refusal}");
        println!("NO PANIC RUNTIME: `{START_PANIC}`: {class}");
    });
    with_program!(&[("panic_unwind", IMPOSTOR), ("panic_abort", ABORT)], |names, _files| {
        assert_eq!(bound_crate(names, Panic::Unwind), Err(InternalRefused::NoPanicRuntime { strategy: Panic::Unwind }), "an impostor is no runtime");
        assert_eq!(bound_crate(names, Panic::Abort).as_deref(), Ok("panic_abort"));
    });
}
