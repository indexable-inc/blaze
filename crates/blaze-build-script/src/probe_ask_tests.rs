//! A probe's compiler line read, keyed and answered (probe_ask.rs): tempo's three real asks
//! that `blaze check` stopped on (thiserror 2.0.21's `build/probe.rs`, num-traits' autocfg
//! empty crate, rustix 1.1.5's `LowerExp` snippet, each with the arguments the check printed),
//! a refusal by name, one key for one question, and the decider agreeing with every row.

use blaze_own::{AVec, Alloc};

use crate::pinned::{THISERROR_2_PROBE, TEMPO};
use crate::probe::{Row, Table, pinned_rows};
use crate::probe_ask::{Answered, Ask, By, Compile, Decided, Open, Refused, Rejection, Verdict, Why, answer, classify, decide, ships_rows};

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
    assert_eq!(answered(NUM_TRAITS, "", &host_rows, alloc).verdict, Verdict::Unanswered(Open::Sysroot(crate::probe_ask::Needs::Std)));
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
    assert!(Rejection::Edition.stderr(&edition, alloc.clone()).as_str().contains("(instead was `2017`)"));
    // Inner attributes that change nothing are decided; one that does is left, by its byte.
    let c = compile(&lib, alloc.clone());
    assert_eq!(decide(&c, b"/* a */ #![no_std] //! doc\n#![warn(missing_docs)]"), Decided::Accepted { needs: crate::probe_ask::Needs::Core });
    assert_eq!(decide(&c, b"#![allow(stable_features)]\n#![feature(core_c_str)]\n"), Decided::Open(Open::Attribute { at: 27 }));
    assert_eq!(decide(&c, b"/// doc\n"), Decided::Open(Open::Items { at: 0 }));
    assert_eq!(decide(&c, b"/* open"), Decided::Open(Open::Text));
    // A flag it does not model leaves the probe.
    let deny = compile(&["--crate-type=lib", "-D", "warnings", "-"], alloc);
    assert_eq!(decide(&deny, b""), Decided::Open(Open::Flag));
    println!("probe_ask: decided and left");
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
    assert!(matches!(classify(&["--print", "cfg", "--target", WASM], alloc.clone()), Ok(Ask::Print { what: "cfg" })));
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

/// Every pinned row reads as a try-compile (`pinned_rows` panics otherwise), and wherever blaze
/// decides a row's snippet itself, it agrees with the row.
/// Prints "probe_ask: rows read, N decided agree".
#[test]
fn pinned_rows_read_as_try_compiles() {
    let alloc = blaze_own::testing::heap().alloc();
    let rows = table_for(WASM, SYSROOT, alloc.clone());
    assert_eq!(rows.len(), TEMPO.len() + SYSROOT.len());
    let mut agreed = 0;
    for known in TEMPO {
        let mut args = blaze_own::Vec::new_in(alloc.clone());
        args.extend_from_slice(known.args);
        args.push("-");
        let c = compile(&args, alloc.clone());
        assert!(c.target.is_none(), "{}: a row names no --target: it is keyed for its side's", known.probe);
        let got = answer(&c, WASM, known.snippet.as_bytes(), &Table { rows: &rows });
        match got.verdict {
            Verdict::Accepted(By::Decided) => {
                assert!(known.accepted, "{}: blaze accepts what the row rejects", known.probe);
                agreed += 1;
            }
            Verdict::Rejected(_) => assert!(!known.accepted, "{}: blaze rejects what the row accepts", known.probe),
            Verdict::Accepted(By::Table) => assert!(known.accepted),
            Verdict::Unanswered(why) => panic!("{}: its own row does not answer it: {why:?}", known.probe),
        }
    }
    assert_eq!(agreed, 2, "the empty crate and `#![no_std]` are decided by blaze");
    println!("probe_ask: rows read, {agreed} decided agree");
}
