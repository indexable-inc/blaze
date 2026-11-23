//! The program's ONE panic strategy (UNWIND.md 3.1, piece P1, main's decision on P1/P2;
//! src/panic_strategy.rs): one fact of the program's build setting, carried in the program's
//! inputs (`UserCrate::panic`), the program crate's `cfg(panic = "..")` derived from it, and
//! front2's binding of the panic runtime read from it.
//!
//! The Reference, linkage.html [link.unwinding.prohibited]: "If a Rust artifact is potentially
//! unwinding, then all its crates must be built with the unwind panic strategy."
//! conditional-compilation.html [cfg.panic]: "Key-value option set depending on the panic
//! strategy." LIB panic_unwind/src/lib.rs:23 and panic_abort/src/lib.rs:9 `#![panic_runtime]`;
//! rustc links the runtime the strategy names.
//!
//! What is promised, and the line each test prints:
//! - the manifest's two read values map to the builder's strategies (the spellings are
//!   blaze-manifest's test, src/layout/panic_tests.rs):
//!   `of_profile: 2 of 2 manifest values mapped; strategy and of_strategy inverse`
//! - the program's inputs derive crate 0's cfg (one `panic` atom, the program's), and front2
//!   reads the same strategy back:
//!   `inputs: abort -> cfg panic="abort", read Abort; unwind -> cfg panic="unwind", read Unwind`
//! - an Abort program binds panic_abort's runtime, an Unwind one panic_unwind's, end to end
//!   through front2's reach walk and table:
//!   `panic runtime: abort binds panic_abort, unwind binds panic_unwind`
//!
//! CONTROL (rule 8): replace the read in front2/reach.rs and front2/program.rs `resolve`
//! (`crate::panic_strategy::of_program(..)`) with the constant `Panic::Unwind`: the third test
//! fails with "the Abort program tabled panic_unwind's `__rust_start_panic`".
//!
//! Not proven here: that an Abort program's code has no landing pad (the builder's input is
//! `Adts::panic`, answered from the same fact on the record path, run.rs `build_records`);
//! that is x0-unwind-01 under the run mode (UNWIND.md section 5, proof 1).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_build::Panic;
use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{Inputs, Toolchain, syntax_of};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::panic_strategy::{derive_cfg, of_profile, of_program, of_strategy, strategy};
use blaze_manifest::layout::panic::ProfilePanic;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_ladder::seam::FrontEnd;
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_unit_graph::implicit;
use edition::PackageEdition;

/// A program with no `core` that names nothing: what the inputs test resolves.
const PLAIN: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    0
}
";

/// The program: the lang items a `#![no_core]` program declares, std's declaration of the
/// runtime's entry as std writes it (the payload a `usize` here), and a `main` that calls it.
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

/// panic_unwind as the library ships it (crate name and attribute), its entry returning 7.
const UNWIND: &str = "#![feature(no_core, rustc_attrs, panic_runtime)]
#![no_core]
#![panic_runtime]

#[rustc_std_internal_symbol]
pub fn __rust_start_panic(_payload: usize) -> u32 {
    7
}
";

/// panic_abort as the library ships it, its entry returning 9.
const ABORT: &str = "#![feature(no_core, rustc_attrs, panic_runtime)]
#![no_core]
#![panic_runtime]

#[rustc_std_internal_symbol]
pub fn __rust_start_panic(_payload: usize) -> u32 {
    9
}
";

const START_PANIC: &str = "__rust_start_panic";

#[test]
fn of_profile_answers() {
    // The spellings themselves (no key, "unwind", "abort", "immediate-abort", "Abort") are
    // blaze-manifest's test (src/layout/panic_tests.rs); here only the map.
    assert_eq!(of_profile(ProfilePanic::Unwind), Panic::Unwind);
    assert_eq!(of_profile(ProfilePanic::Abort), Panic::Abort);
    for p in [Panic::Unwind, Panic::Abort] {
        assert_eq!(of_strategy(strategy(p)), p, "the two maps are inverse");
    }
    println!("of_profile: 2 of 2 manifest values mapped; strategy and of_strategy inverse");
}

/// The cfg the inputs give crate 0 for `panic`, and the strategy front2 reads back.
fn derived(panic: Panic) -> (Vec<String>, Panic) {
    let files = [SourceFile { path: "src/main.rs", text: PLAIN }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, panic).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let atoms: Vec<String> = crates[0].cfg.iter().filter(|(n, _)| *n == "panic").map(|(n, v)| format!("{n}={v:?}")).collect();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let read = of_program(&names).unwrap_or_else(|e| panic!("read back: {e}"));
    (atoms, read)
}

#[test]
fn the_inputs_derive_crate_zeros_cfg_and_front2_reads_it() {
    let (abort_atoms, abort) = derived(Panic::Abort);
    assert_eq!(abort_atoms, ["panic=Some(\"abort\")"], "one `panic` atom, the program's, in place of the target list's `unwind`");
    assert_eq!(abort, Panic::Abort);
    let (unwind_atoms, unwind) = derived(Panic::Unwind);
    assert_eq!(unwind_atoms, ["panic=Some(\"unwind\")"]);
    assert_eq!(unwind, Panic::Unwind);
    println!("inputs: abort -> cfg panic=\"abort\", read Abort; unwind -> cfg panic=\"unwind\", read Unwind");
}

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

/// The crate whose `__rust_start_panic` front2 tabled WITH its body when the program `ROOT` is
/// built with `panic` over both runtimes (crate 1 panic_unwind, crate 2 panic_abort, as std
/// depends on both). Crate 0's cfg is `Toolchain::program_cfg(user.panic)`, derived as
/// `crates::Inputs::new` derives it ([`derive_cfg`]); the runtimes keep their records' cut.
fn tabled_runtime(panic: Panic) -> String {
    let libs: [(&str, &str); 2] = [("panic_unwind", UNWIND), ("panic_abort", ABORT)];
    let files = [SourceFile { path: "src/main.rs", text: ROOT }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, panic).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    // The program crate's cfg from the program's own strategy (its inputs' carrier); it must be
    // what `derive_cfg` makes of the target list, so the target's `unwind` never reaches it.
    let program_cfg = tc.program_cfg(user.panic);
    let mut derived = tc.program_cfg(Panic::Unwind);
    derive_cfg(&mut derived, user.panic);
    assert_eq!(program_cfg, derived, "program_cfg({:?}) is not the derived cfg", user.panic);
    // The runtimes are toolchain crates: they keep the strategy their records were cut under,
    // named (the shelf's cut is Unwind), not taken from the program.
    let target = tc.program_cfg(Panic::Unwind);
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
        cfg: &program_cfg,
        script: None,
    });
    for (i, (name, _)) in libs.iter().enumerate() {
        crates.push(CrateInput {
            name: *name,
            key: CrateItemKey(ContentId::of("panic_strategy test crate", name.as_bytes())),
            edition: syntax_of(rust),
            implicit: lib,
            root: &roots[i].0,
            shown_from: "/",
            features: &[],
            deps: &[],
            cfg: &target,
            script: None,
        });
    }
    let sources = Files { user: user.files, libs: &roots };
    let names = resolve_crates(&crates, Sysroot::default(), &sources, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    assert_eq!(of_program(&names), Ok(panic), "front2 reads the program's strategy");
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn the walk reaches; left out: {left:?}");
    // The runtime entry tabled with its body, found by its KEY and its crate by the key's crate:
    // a module-level fn's row path is its bare name (front2 row_path.rs), so the path's text
    // never said which crate (the test read `__rust_start_panic` as a crate name: red on the
    // green tree until 2026-11-18).
    let defined: Vec<_> = (0..names.crates().len())
        .flat_map(|krate| names.items(krate))
        .map(|(lib, _, _)| lib)
        .filter(|&lib| names.item_text(lib).is_some_and(|item| item.name == Some(START_PANIC)))
        .filter(|&lib| program.fn_index(checker_of(lib)).is_some_and(|i| program.function(i).body.is_some()))
        .collect();
    assert_eq!(defined.len(), 1, "one runtime entry tabled with its body: {defined:?}");
    let krate = names.crate_of(defined[0]).expect("the tabled runtime definition's crate");
    names.krate(krate).name.to_string()
}

#[test]
fn abort_binds_panic_abort_and_unwind_binds_panic_unwind() {
    let abort = tabled_runtime(Panic::Abort);
    assert_eq!(abort, "panic_abort", "the Abort program tabled {abort}'s `__rust_start_panic`");
    let unwind = tabled_runtime(Panic::Unwind);
    assert_eq!(unwind, "panic_unwind", "the Unwind program tabled {unwind}'s `__rust_start_panic`");
    println!("panic runtime: abort binds panic_abort, unwind binds panic_unwind");
}
