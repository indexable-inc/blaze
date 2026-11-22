//! THE FIRST RUNG OF D175 in the world-run family (world_run.rs): `#[derive(Hello)]`
//! (crates/blaze-comptime/rungs/derive-hello) expanded NATIVELY, from the macro's code records
//! run under the launcher's `comptime:` form with its C names bound through blaze-comptime-sys's
//! table, and compared with the expansion the rung states (`expected.rs`) by
//! `blaze_comptime::first_difference`. With Wasmtime deleted (D175) the other side is the
//! rung's stated expansion, rendered as the token server renders a stream.
//!
//! The rung's test is `#[ignore]`d naming its blockers ([`Blocker`]); run with `--ignored` it
//! FAILS naming them, never passes or skips quietly. Its control runs always: the comparison
//! finds no difference between the stated expansion and itself, and finds the first byte of a
//! one-byte change (rule 23: the comparison is what decides).

use core::fmt;

use blaze_comptime::{Difference, Outcome, first_difference};

use super::world_run::World;

/// The rung's macro crate, as the rung keeps it.
const RUNG_MANIFEST: &str = include_str!("../../../blaze-comptime/rungs/derive-hello/Cargo.toml");
const RUNG_LIB: &str = include_str!("../../../blaze-comptime/rungs/derive-hello/src/lib.rs");
/// The item the derive is on, and the expansion the rung states.
const INPUT: &str = include_str!("../../../blaze-comptime/rungs/derive-hello/input.rs");
const EXPECTED: &str = include_str!("../../../blaze-comptime/rungs/derive-hello/expected.rs");

/// The crate that uses the derive: the rung's input as its library.
const USER_MANIFEST: &str = "[package]\nname = \"greeter\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nderive-hello = { path = \"derive-hello\" }\n";

/// What stands between the rung and a native expansion, each with the file it is in. Not
/// here: G1 (world_run.rs `Gap::MakeKeepsNoLir`), which only the evaluator's differential (G4)
/// waits on, and G5 (`Gap::LauncherOwnsTheProcess`), closed by run_mode's `End::Return`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Blocker {
    /// The macro's code records have no entry: the front end synthesizes no proc-macro
    /// harness static (`__rustc_proc_macro_decls_<hash>__`) nor the entry
    /// `comptime:blaze_proc_macro_expand` that walks it (blaze-scope task/lower.rs, where
    /// `#[proc_macro_derive]` is recognized, ProcKind::Derive).
    NoExpandEntry,
    /// `proc_macro` and `std` are not code records yet, with the bridge shim's native import
    /// block: the macro's calls into `proc_macro` have nothing to bind (comptime-native.md,
    /// "The first rung", step 3).
    NoProcMacroRecords,
    /// On Linux the compile-time layer cannot install: blaze-sys has no `dlsym` there, which
    /// blaze-comptime-sys's `Real` requires (blaze-launcher comptime.rs `real`; macOS has it).
    #[cfg(target_os = "linux")]
    NoRealFunctionsOnLinux,
    /// No parent serves a compile-time run: nothing spawns the launcher with `comptime:`, the
    /// records on descriptor 3 and the compile-time host on descriptor 4, and collects its
    /// `Exit` and tokens into an [`Outcome`].
    NoComptimeParent,
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::NoExpandEntry => f.write_str(
                "no proc-macro harness static nor `comptime:blaze_proc_macro_expand` entry (blaze-scope task/lower.rs)",
            ),
            Blocker::NoProcMacroRecords => f.write_str("`proc_macro` and `std` are not code records with the shim's native import block"),
            #[cfg(target_os = "linux")]
            Blocker::NoRealFunctionsOnLinux => f.write_str("on Linux blaze-sys gives no `dlsym` for the layer's `Real` (blaze-launcher comptime.rs `real`)"),
            Blocker::NoComptimeParent => f.write_str("no parent spawns a `comptime:` launcher with its host on descriptor 4"),
        }
    }
}

/// The blockers of the native rung, in the order D175 lists them, on the system the test runs
/// on.
#[cfg(target_os = "linux")]
const BLOCKERS: [Blocker; 4] = [Blocker::NoExpandEntry, Blocker::NoProcMacroRecords, Blocker::NoRealFunctionsOnLinux, Blocker::NoComptimeParent];
#[cfg(not(target_os = "linux"))]
const BLOCKERS: [Blocker; 3] = [Blocker::NoExpandEntry, Blocker::NoProcMacroRecords, Blocker::NoComptimeParent];

/// The world of the rung: the macro crate under `derive-hello/` and the crate using it.
fn rung_world() -> World {
    World::new(1175)
        .file("derive-hello/Cargo.toml", RUNG_MANIFEST)
        .file("derive-hello/src/lib.rs", RUNG_LIB)
        .file("Cargo.toml", USER_MANIFEST)
        .file("src/lib.rs", INPUT)
}

/// A stream as the comparison holds it: its tokens separated by one space, as `expected.rs`
/// is written (the token server's rendering of a stream with call-site spans).
fn rendered(text: &str) -> std::string::String {
    text.split_whitespace().collect::<std::vec::Vec<_>>().join(" ")
}

/// The stated side: the rung's expansion, no diagnostics, no reads, no facts (the macro reads
/// nothing: its lib.rs doc).
fn stated(tokens: &[u8]) -> Outcome<'_> {
    Outcome { tokens, diagnostics: &[], reads: &[], facts: &[] }
}

/// The derive on `world`'s `Greeter` expanded natively, or what blocks it.
fn expand_native(world: &World) -> Result<std::string::String, &'static [Blocker]> {
    // The world is built so the day a blocker goes, its piece has the inputs here.
    let _ = world;
    Err(&BLOCKERS)
}

/// `#[derive(Hello)]` on `Greeter`, expanded natively, equals the rung's stated expansion in
/// every field `first_difference` compares.
#[test]
#[ignore = "D175 rung blocked: no `comptime:blaze_proc_macro_expand` entry nor harness static (blaze-scope task/lower.rs); proc_macro/std not code records; on Linux blaze-sys gives no `dlsym` for the layer (blaze-launcher comptime.rs `real`); no parent spawns a `comptime:` launcher"]
fn derive_hello_expands_natively_as_the_rung_states() {
    let world = rung_world();
    let native = match expand_native(&world) {
        Ok(native) => native,
        Err(blockers) => {
            let said: std::vec::Vec<_> = blockers.iter().map(|it| it.to_string()).collect();
            panic!("the native rung is blocked: {}", said.join("; "));
        }
    };
    let (want, got) = (rendered(EXPECTED), rendered(&native));
    assert_eq!(first_difference(&stated(want.as_bytes()), &stated(got.as_bytes())), None, "native: {got}");
}

/// CONTROL (rule 23): the comparison the rung uses decides. The stated expansion equals
/// itself; a one-byte change (`Greeter` to `Greetes` in the literal) is found at its byte; the
/// rendering joins the rung's line as written. Prints "world_comptime: the rung's comparison
/// finds the changed byte".
#[test]
fn the_rungs_comparison_finds_a_changed_byte() {
    let want = rendered(EXPECTED);
    assert_eq!(want, "impl Greeter { pub const HELLO: &'static str = \"Greeter\"; }");
    assert_eq!(first_difference(&stated(want.as_bytes()), &stated(want.as_bytes())), None);
    let changed = want.replace("\"Greeter\"", "\"Greetes\"");
    let at = want.find("\"Greeter\"").expect("the literal") + 7;
    assert_eq!(
        first_difference(&stated(want.as_bytes()), &stated(changed.as_bytes())),
        Some(Difference::Tokens { at, wasm: Some(b'r'), native: Some(b's') })
    );
    println!("world_comptime: the rung's comparison finds the changed byte");
}
