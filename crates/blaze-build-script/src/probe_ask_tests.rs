//! A probe's compiler line read, keyed and answered (probe_ask.rs): tempo's three real asks
//! that `blaze check` stopped on (thiserror 2.0.21's `build/probe.rs`, num-traits' autocfg
//! empty crate, rustix 1.1.5's `LowerExp` snippet, each with the arguments the check printed),
//! a refusal by name, one key for one question, and the decider agreeing with every row.

use blaze_own::{AVec, Alloc};

use crate::pinned::{THISERROR_2_PROBE, TEMPO};
use crate::probe::{Row, Table, pinned_rows};
use crate::probe_ask::{Answered, Ask, By, Compile, Decided, Needs, Open, Refused, Rejection, Verdict, Why, answer, classify, decide, feature_rows, ships_rows};

const WASM: &str = "wasm32-wasip1";
const HOST: &str = "aarch64-apple-darwin";
const SYSROOT: &[&str] = &["alloc", "core", "proc_macro", "std"];

/// A side's table as scripts.rs makes it: the pinned rows for `target` and its sysroot's
/// ships facts, sorted.
fn table_for<'a>(target: &str, sysroot: &[&str], alloc: Alloc<'a>) -> AVec<'a, Row> {
    let mut rows = pinned_rows(target, TEMPO, alloc.clone());
    rows.extend_from_slice(&ships_rows(target, sysroot.iter().copied(), alloc));
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

fn compile<'a, 's>(args: &[&'s str], alloc: Alloc<'a>) -> Compile<'a, 's> {
    match classify(args, alloc) {
        Ok(Ask::Compile(compile)) => compile,
        other => panic!("{args:?} is not a try-compile: {other:?}"),
    }
}

fn answered<'t>(args: &[&'t str], snippet: &str, rows: &[Row], alloc: Alloc<'_>) -> Answered<'t> {
    let c = compile(args, alloc);
    answer(&c, HOST, snippet.as_bytes(), &Table { rows })
}

const THISERROR: &[&str] = &[
    "--edition=2018",
    "--crate-name=thiserror",
    "--crate-type=lib",
    "--cap-lints=allow",
    "--emit=dep-info,metadata",
    "--out-dir",
    "/target/debug/build/thiserror-e1d54a3d39238d9e/out/probe",
    "build/probe.rs",
    "--target",
    "wasm32-wasip1",
];
const NUM_TRAITS: &[&str] = &[
    "--crate-name",
    "autocfg_dca8b2ac880013e3_0",
    "--crate-type=lib",
    "--out-dir",
    "/target/debug/build/num-traits-b230a6c6998cc448/out",
    "--emit=llvm-ir",
    "--target",
    "wasm32-wasip1",
    "-",
];
const RUSTIX: &[&str] = &["--crate-type=rlib", "--emit=metadata", "--target", "wasm32-wasip1", "-o", "/target/debug/build/rustix-b7706fda628b31da/out/rustix_test_can_compile", "-"];
const RUSTIX_SNIPPET: &str = "fn a(x: &core::num::NonZeroI32, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { core::fmt::LowerExp::fmt(x, f) }\n";

/// The three asks tempo's check stopped on, for the macro side's `wasm32-wasip1`, each answered:
/// num-traits' empty crate decided by blaze (a `lib` of no items, std shipped for the target),
/// thiserror's and rustix's by their pinned rows made for that target.
/// Prints "probe_ask: three real probes answered for wasm32-wasip1".
/// Control: key the rows with `--target aarch64-apple-darwin` in their arguments (as before)
/// and thiserror's and rustix's are `Unanswered`.
#[test]
fn probe_ask_tempos_three_real_probes_are_answered_for_the_macro_side() {
    let alloc = blaze_own::testing::heap().alloc();
    let rows = table_for(WASM, SYSROOT, alloc.clone());
    let thiserror = answered(THISERROR, THISERROR_2_PROBE, &rows, alloc.clone());
    assert_eq!(thiserror.verdict, Verdict::Accepted(By::Table), "thiserror 2.0.21's probe");
    assert_eq!(thiserror.target, WASM);
    let num_traits = answered(NUM_TRAITS, "", &rows, alloc.clone());
    assert_eq!(num_traits.verdict, Verdict::Accepted(By::Decided), "autocfg's empty crate");
    let rustix = answered(RUSTIX, RUSTIX_SNIPPET, &rows, alloc.clone());
    assert_eq!(rustix.verdict, Verdict::Accepted(By::Table), "rustix's LowerExp probe");
    // The host's table answers none of the three: a row of one triple never answers another,
    // and it holds no ships fact for wasm32-wasip1, so not even the empty crate is decided.
    let host_rows = table_for(HOST, SYSROOT, alloc.clone());
    assert!(matches!(answered(THISERROR, THISERROR_2_PROBE, &host_rows, alloc.clone()).verdict, Verdict::Unanswered(_)));
    assert!(matches!(answered(RUSTIX, RUSTIX_SNIPPET, &host_rows, alloc.clone()).verdict, Verdict::Unanswered(_)));
    assert_eq!(answered(NUM_TRAITS, "", &host_rows, alloc).verdict, Verdict::Unanswered(Open::Sysroot(Needs::Std)));
    println!("probe_ask: three real probes answered for wasm32-wasip1");
}

/// What blaze decides itself, and what it leaves: a snippet with a type error is never
/// accepted (it has an item, so it is left to the table, which has no row: unanswered); a `bin`
/// of no items is E0601; an edition rustc lacks is refused at the options.
/// Prints "probe_ask: decided and left".
#[test]
fn probe_ask_a_type_error_is_never_accepted_and_what_blaze_decides_is_exact() {
    let alloc = blaze_own::testing::heap().alloc();
    let rows = table_for(HOST, SYSROOT, alloc.clone());
    let lib = ["--crate-type=lib", "--emit=metadata", "-"];
    let typed = answered(&lib, "pub fn f() -> u32 { \"x\" }\n", &rows, alloc.clone());
    assert_eq!(typed.verdict, Verdict::Unanswered(Open::Items { at: 0 }), "a type error is not decided here: {typed:?}");
    let bin = answered(&["--emit=metadata", "-"], "// nothing\n#![allow(dead_code)]\n", &rows, alloc.clone());
    assert_eq!(bin.verdict, Verdict::Rejected(Rejection::NoMain));
    let edition = compile(&["--edition", "2017", "--crate-type=lib", "-"], alloc.clone());
    assert_eq!(decide(&edition, b""), Decided::Rejected(Rejection::Edition));
    assert!(Rejection::Edition.stderr(&edition, b"", alloc.clone()).as_str().contains("(instead was `2017`)"));
    // Inner attributes that change nothing are decided; one that does is left, by its byte.
    let c = compile(&lib, alloc.clone());
    assert_eq!(decide(&c, b"/* a */ #![no_std] //! doc\n#![warn(missing_docs)]"), Decided::Accepted { needs: Needs::Core, library: false });
    assert_eq!(decide(&c, b"#![allow(stable_features)]\n#![feature(core_c_str)]\n"), Decided::Accepted { needs: Needs::Std, library: true });
    assert_eq!(decide(&c, b"#![cfg_attr(x, feature(a))]\n"), Decided::Open(Open::Attribute { at: 0 }));
    assert_eq!(decide(&c, b"#![feature(a, a)]\n"), Decided::Open(Open::Attribute { at: 0 }), "a name twice is left");
    assert_eq!(decide(&c, b"/// doc\n"), Decided::Open(Open::Items { at: 0 }));
    assert_eq!(decide(&c, b"/* open"), Decided::Open(Open::Text));
    // A flag it does not model leaves the probe.
    let deny = compile(&["--crate-type=lib", "-D", "warnings", "-"], alloc);
    assert_eq!(decide(&deny, b""), Decided::Open(Open::Flag));
    println!("probe_ask: decided and left");
}

/// turso's thiserror 2.0.16 (2026-11-21): its build/probe.rs passes `--cap-lints=allow` twice
/// (build.rs:121 and :123), and blaze stopped it with `RustcLine { at: 5, why: Repeated }`. The
/// real line, reduced to the two flags: read, and keyed exactly as the line with one (so a row
/// made for thiserror 2.0.21's one-flag line answers it when the snippet is the same). Twin:
/// `--edition` twice is still `Repeated`. Control: two different levels are refused by name
/// (`Differs`), never one picked. Prints "probe_ask: --cap-lints again".
#[test]
fn probe_ask_cap_lints_given_twice_is_one_question() {
    let alloc = blaze_own::testing::heap().alloc();
    let twice = [
        "--edition=2018",
        "--crate-name=thiserror",
        "--crate-type=lib",
        "--cap-lints=allow",
        "--emit=dep-info,metadata",
        "--cap-lints=allow",
        "--out-dir",
        "/target/debug/build/thiserror-115620d781064c78/out/probe",
        "build/probe.rs",
        "--target",
        "wasm32-wasip1",
    ];
    let (a, b) = (compile(&twice, alloc.clone()), compile(THISERROR, alloc.clone()));
    assert_eq!(a.cap_lints, Some("allow"));
    assert_eq!(a.key(HOST, THISERROR_2_PROBE.as_bytes()), b.key(HOST, THISERROR_2_PROBE.as_bytes()), "the flag again is the same question");
    let refused = |args: &[&str]| classify(args, alloc.clone()).err();
    assert_eq!(refused(&["--edition=2018", "--edition=2018", "-"]), Some(Refused { at: 1, why: Why::Repeated }));
    assert_eq!(refused(&["--cap-lints=allow", "--cap-lints", "warn", "-"]), Some(Refused { at: 1, why: Why::Differs }));
    println!("probe_ask: --cap-lints again");
}

/// An argument the reader does not know is refused by its index, never skipped.
/// Prints "probe_ask: refused by name".
#[test]
fn probe_ask_an_unknown_flag_is_refused_by_name() {
    let alloc = blaze_own::testing::heap().alloc();
    let refused = |args: &[&str]| classify(args, alloc.clone()).err();
    assert_eq!(refused(&["--crate-type=lib", "--frobnicate", "-"]), Some(Refused { at: 1, why: Why::UnknownFlag }));
    assert_eq!(refused(&["-Q", "-"]), Some(Refused { at: 0, why: Why::UnknownFlag }));
    assert_eq!(refused(&["-vX"]), Some(Refused { at: 0, why: Why::UnknownFlag }));
    assert_eq!(refused(&["--crate-type=lib", "--edition"]), Some(Refused { at: 1, why: Why::NoValue }));
    assert_eq!(refused(&["--edition=2018", "--edition=2021", "-"]), Some(Refused { at: 1, why: Why::Repeated }));
    assert_eq!(refused(&["a.rs", "-"]), Some(Refused { at: 1, why: Why::TwoInputs }));
    assert_eq!(refused(&["--crate-type=lib"]), Some(Refused { at: 1, why: Why::NoInput }));
    assert_eq!(refused(&["--emit=wasm", "-"]), Some(Refused { at: 0, why: Why::Emit }));
    assert!(matches!(classify(&["--print", "cfg", "--target", WASM], alloc.clone()), Ok(Ask::Print { what: "cfg", target: Some(WASM) })));
    assert!(matches!(classify(&["-vV"], alloc), Ok(Ask::Version { verbose: true })));
    println!("probe_ask: refused by name");
}

/// One question, one key: the output paths, the crate name, the order of the flags and the
/// spelling `--x v` / `--x=v` do not split it; the target, the crate type, the emit class, a
/// cfg and a byte of the snippet do.
/// Prints "probe_ask: one question one key".
/// Control: hash `--crate-name` into `Compile::key` and the first assertion fails.
#[test]
fn probe_ask_the_same_question_is_one_key() {
    let alloc = blaze_own::testing::heap().alloc();
    let key = |args: &[&str], snippet: &str| compile(args, alloc.clone()).key(HOST, snippet.as_bytes());
    let base = key(NUM_TRAITS, "");
    let renamed = ["--crate-name", "autocfg_ffff_7", "--out-dir", "/elsewhere", "--target", WASM, "--emit=llvm-ir", "--crate-type", "lib", "-"];
    assert_eq!(base, key(&renamed, ""), "output-only flags or order split one question");
    let cfg_a = ["--cfg", "a", "--cfg=b", "--crate-type=lib", "-"];
    let cfg_b = ["--cfg=b", "--crate-type=lib", "--cfg", "a", "-"];
    assert_eq!(key(&cfg_a, ""), key(&cfg_b, ""), "cfgs are a set");
    assert_ne!(base, key(&["--crate-type=lib", "--emit=llvm-ir", "-"], ""), "the target (wasm32-wasip1 against the host) is not keyed");
    assert_ne!(base, key(&["--crate-type=lib", "--emit=metadata", "--target", WASM, "-"], ""), "whether code is generated is not keyed");
    assert_ne!(base, key(&["--crate-type=rlib", "--emit=llvm-ir", "--target", WASM, "-"], ""), "the crate type is not keyed");
    assert_ne!(base, key(NUM_TRAITS, " "), "the snippet is not keyed");
    assert_ne!(key(&cfg_a, ""), key(&["--cfg", "a", "--crate-type=lib", "-"], ""), "a cfg is not keyed");
    println!("probe_ask: one question one key");
}

/// Every pinned row reads as a try-compile (`pinned_rows` panics otherwise), its own row
/// answers it, and none is a probe blaze decides itself: each has items (`Open::Items`), what
/// only the item probe removes. A row the decider covers is dead and is taken out.
/// Prints "probe_ask: N rows read, none decided".
/// Control: put back rustix's `#![feature(core_c_str)]` row and it fails naming it.
#[test]
fn pinned_rows_read_as_try_compiles() {
    let alloc = blaze_own::testing::heap().alloc();
    let rows = table_for(WASM, SYSROOT, alloc.clone());
    assert_eq!(rows.len(), TEMPO.len() + SYSROOT.len());
    for known in TEMPO {
        let mut args = blaze_own::Vec::new_in(alloc.clone());
        args.extend_from_slice(known.args);
        args.push("-");
        let c = compile(&args, alloc.clone());
        assert!(c.target.is_none(), "{}: a row names no --target: it is keyed for its side's", known.probe);
        // Items, or an inner `cfg_attr` over them (anyhow's): both the item probe's.
        assert!(matches!(decide(&c, known.snippet.as_bytes()), Decided::Open(Open::Items { .. } | Open::Attribute { .. })), "{}: blaze decides this probe itself; its row is dead", known.probe);
        let got = answer(&c, WASM, known.snippet.as_bytes(), &Table { rows: &rows });
        match got.verdict {
            Verdict::Accepted(By::Table) => assert!(known.accepted),
            Verdict::Rejected(Rejection::Table) => assert!(!known.accepted),
            other => panic!("{}: its own row does not answer it: {other:?}", known.probe),
        }
    }
    println!("probe_ask: {} rows read, none decided", TEMPO.len());
}

/// rustix's `can_compile` line for the macro side, as tempo's check printed it.
const RUSTIX_FEATURE: &str = "#![allow(stable_features)]\n#![feature(wasi_ext)]\n";

/// A side's table with its library listing for a std probe: `declared` and `mentioned` as
/// blaze-scope's `lib_features` lists them (its tests read std's `os::wasi` under each cfg).
fn listed_table<'a>(target: &str, declared: &[&str], mentioned: &[&str], alloc: Alloc<'a>) -> AVec<'a, Row> {
    let mut rows = table_for(target, SYSROOT, alloc.clone());
    rows.extend_from_slice(&feature_rows(target, Needs::Std, declared, mentioned, alloc));
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

/// The four kinds of name `#![feature]` gives, each answered as the pinned nightly would:
/// - a language feature (`default_field_values`, unstable: enabled) is accepted with no
///   listing at all, and a removed one (`box_syntax`) is E0557;
/// - a library feature the target's std declares (`wasi_ext`, `os::wasi` of wasm32-wasip1):
///   rustix's real probe is accepted for the macro side, by blaze;
/// - the same feature for the host, whose std has no `os::wasi`: E0635 naming it, status 1;
/// - a name nothing declares: E0635; one only mentioned in a macro's tokens is left open, and
///   with no listing for the target nothing is claimed.
/// Prints "probe_ask: four feature kinds answered".
/// Control: list `wasi_ext` as declared for the host and the third case is accepted.
#[test]
fn probe_ask_features_are_answered_from_the_language_list_and_the_library_listing() {
    let alloc = blaze_own::testing::heap().alloc();
    let wasm = listed_table(WASM, &["alloc_ffi", "core_c_str", "rust1", "wasi_ext"], &["in_macro_only"], alloc.clone());
    let host = listed_table(HOST, &["alloc_ffi", "core_c_str", "rust1"], &["in_macro_only"], alloc.clone());
    let bare = table_for(HOST, SYSROOT, alloc.clone());
    let lib = ["--crate-type=rlib", "--emit=metadata", "-"];

    // A language feature: decided from the language list alone.
    let lang = answered(&lib, "#![feature(default_field_values)]\n", &bare, alloc.clone());
    assert_eq!(lang.verdict, Verdict::Accepted(By::Decided), "a language feature");
    let removed = answered(&lib, "#![feature(box_syntax)]\n", &bare, alloc.clone());
    assert_eq!(removed.verdict, Verdict::Rejected(Rejection::RemovedFeature { at: 11, len: 10 }));

    // A library feature the target declares: rustix's probe, its line as the check printed it.
    let present = answered(RUSTIX, RUSTIX_FEATURE, &wasm, alloc.clone());
    assert_eq!(present.verdict, Verdict::Accepted(By::Decided), "rustix's wasi_ext probe for wasm32-wasip1");
    assert_eq!(present.target, WASM);

    // The same feature for the host: E0635, as rustc says it.
    let absent = answered(&lib, RUSTIX_FEATURE, &host, alloc.clone());
    let Verdict::Rejected(rejection) = absent.verdict else { panic!("wasi_ext for the host: {absent:?}") };
    assert_eq!(rejection, Rejection::UnknownFeature { at: 38, len: 8 });
    let c = compile(&lib, alloc.clone());
    assert_eq!(rejection.stderr(&c, RUSTIX_FEATURE.as_bytes(), alloc.clone()).as_str(), "error[E0635]: unknown feature `wasi_ext`\n");

    // An unknown name: E0635 wherever the listing is; a mention alone or no listing: open.
    let unknown = answered(RUSTIX, "#![feature(core_c_str, no_such_feature)]\n", &wasm, alloc.clone());
    assert_eq!(unknown.verdict, Verdict::Rejected(Rejection::UnknownFeature { at: 23, len: 15 }));
    let mentioned = answered(RUSTIX, "#![feature(in_macro_only)]\n", &wasm, alloc.clone());
    assert_eq!(mentioned.verdict, Verdict::Unanswered(Open::Feature { at: 11 }));
    let unlisted = answered(&lib, RUSTIX_FEATURE, &bare, alloc);
    assert_eq!(unlisted.verdict, Verdict::Unanswered(Open::Features(Needs::Std)));
    println!("probe_ask: four feature kinds answered");
}
