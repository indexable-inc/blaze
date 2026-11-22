//! A probe answered by the thing it asks about (probe_check.rs, D193 point 2), with the fake
//! checker: the real lines that stopped turso's scripts on 2026-11-21 (reports/turso/c1647
//! check.out, `CannotAnswerYet`: rustix 0.38, indexmap's and bigdecimal's autocfg, proc-macro2's
//! probe file for the macro side) read as the questions they are; two spellings of one question
//! share a key; a flag blaze does not model is refused by name; an answer is stored once and
//! read by the next script; an `Unknown` stops the script and is never stored as a failure.

use core::cell::Cell;

use blaze_ids::ContentId;
use blaze_own::Alloc;

use crate::probe_ask::Snippet;
use crate::probe_check::fake::{FakeChecker, FakeRow, FakeVerdict, MemRecords, Twice};
use crate::probe_check::{
    Cause, Child, CheckerId, CompileProbe, CrateName, Edition, PrintKind, Probe, ProbeKey, ProbeRecords, Question, Refused, Stop, Stored, Why, answer,
    checker_id, decode, encode, probe, read_line,
};

const HOST: &str = "aarch64-apple-darwin";
const WASM: &str = "wasm32-wasip1";

/// rustix 0.38.44 `can_compile` (build.rs:178), its line as turso's check printed it.
const RUSTIX_038: &[&str] = &["--crate-type=rlib", "--emit=metadata", "--target", "aarch64-apple-darwin", "-o", "-", "-"];
const RUSTIX_038_SNIPPET: &str = "const unsafe fn foo(p: *const u8) -> isize { p.offset_from(p) }\n";
/// indexmap's autocfg `probe_sysroot_crate("std")`.
const INDEXMAP: &[&str] = &[
    "--crate-name",
    "autocfg_dca8b2ac880013e3_1",
    "--crate-type=lib",
    "--out-dir",
    "/target/debug/build/indexmap-8e6ed7ffaf5e6fc8/out",
    "--emit=llvm-ir",
    "--target",
    "aarch64-apple-darwin",
    "-",
];
const INDEXMAP_SNIPPET: &str = "extern crate std as probe;";
/// bigdecimal's autocfg `probe_expression("0i64.unsigned_abs()")`.
const BIGDECIMAL: &[&str] = &[
    "--crate-name",
    "autocfg_dca8b2ac880013e3_1",
    "--crate-type=lib",
    "--out-dir",
    "/target/debug/build/bigdecimal-2b3ac0ba196a8ef1/out",
    "--emit=llvm-ir",
    "--target",
    "aarch64-apple-darwin",
    "-",
];
const BIGDECIMAL_SNIPPET: &str = "pub fn probe() { let _ = 0i64.unsigned_abs(); }";
/// proc-macro2's `compile_probe` of `build/probe.rs` for the macro side.
const PROC_MACRO2: &[&str] = &[
    "--edition=2021",
    "--crate-name=proc_macro2",
    "--crate-type=lib",
    "--cap-lints=allow",
    "--emit=dep-info,metadata",
    "--out-dir",
    "/target/debug/build/proc-macro2-2d8ca2f1445150ff/out/probe",
    "build/probe.rs",
    "--target",
    "wasm32-wasip1",
];
const PROC_MACRO2_SNIPPET: &str = concat!(
    "// This code exercises the surface area that we expect of Span's unstable API.\n",
    "// If the current toolchain is able to compile it, then proc-macro2 is able to\n",
    "// offer these APIs too.\n\n#![feature(proc_macro_span)]\n\nextern crate proc_macro;\n\n",
    "use core::ops::{Range, RangeBounds};\nuse proc_macro::{Literal, Span};\n\n",
    "pub fn byte_range(this: &Span) -> Range<usize> {\n    this.byte_range()\n}\n\n",
    "pub fn start(this: &Span) -> Span {\n    this.start()\n}\n\n",
    "pub fn end(this: &Span) -> Span {\n    this.end()\n}\n\n",
    "pub fn line(this: &Span) -> usize {\n    this.line()\n}\n\n",
    "pub fn column(this: &Span) -> usize {\n    this.column()\n}\n\n",
    "pub fn join(this: &Span, other: Span) -> Option<Span> {\n    this.join(other)\n}\n\n",
    "pub fn subspan<R: RangeBounds<usize>>(this: &Literal, range: R) -> Option<Span> {\n    this.subspan(range)\n}\n\n",
    "// Include in sccache cache key.\nconst _: Option<&str> = option_env!(\"RUSTC_BOOTSTRAP\");\n",
);
/// thiserror 2.0.16's `build/probe.rs` line: `--cap-lints=allow` twice (build.rs:121, :123).
const THISERROR_2016: &[&str] = &[
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
/// rustix 1.1.5's `use_thumb_mode` (build.rs:200): rejected by any target but 32-bit arm's
/// thumb mode; the fake's rejection row.
const THUMB_SNIPPET: &str = "pub unsafe fn f() { core::arch::asm!(\"udf #16\", in(\"r7\") 0); }\n";

const QUIET: Child<'static> = Child { host: HOST, env: &[], stdout_read: false };

fn id() -> CheckerId {
    checker_id(&ContentId([1; 32]), &["default_field_values", "never_type"], &ContentId([2; 32]))
}

fn asked<'a, 's>(args: &[&'s str], source: &'s str, child: &Child<'s>, alloc: Alloc<'a>) -> Result<Probe<'a, 's>, Refused> {
    let line = read_line(args, alloc)?;
    probe(line, args, source.as_bytes(), child, alloc)
}

fn compile<'a, 's>(args: &[&'s str], source: &'s str, child: &Child<'s>, alloc: Alloc<'a>) -> CompileProbe<'a, 's> {
    match asked(args, source, child, alloc) {
        Ok(Probe::Asks(Question::Compile(c))) => c,
        other => panic!("{args:?} is not a try-compile: {other:?}"),
    }
}

fn key_of(args: &[&str], source: &str, child: &Child<'_>, checker: &CheckerId, alloc: Alloc<'_>) -> ProbeKey {
    match asked(args, source, child, alloc) {
        Ok(Probe::Asks(question)) => question.key(checker),
        other => panic!("{args:?} asks the checker nothing: {other:?}"),
    }
}

/// Each line that stopped a turso script, and the version and print lines, read as the
/// question it is: the target, the edition, the crate types, whether code is generated,
/// where the snippet comes from, the crate name, and the variables the key holds.
/// Prints "probe_check: real lines read".
/// Control: read `--target` as dropped (`compile.target` unused in `probe`) and rustix's and
/// proc-macro2's targets are the host.
#[test]
fn probe_check_the_real_lines_read_as_their_questions() {
    let alloc = blaze_own::testing::heap().alloc();
    let rustix = compile(RUSTIX_038, RUSTIX_038_SNIPPET, &QUIET, alloc);
    assert_eq!((rustix.target, rustix.edition, rustix.line.input), (HOST, Edition::E2015, Snippet::Stdin));
    assert_eq!(&rustix.line.crate_types[..], ["rlib"]);
    assert!(!rustix.line.codegen && rustix.line.emits.has("metadata") && !rustix.line.emits.has("link"));
    assert_eq!(rustix.line.out, Some("-"));
    assert_eq!(rustix.crate_name, CrateName::Plain("rust_out"), "stdin's crate is rustc's `rust_out`");
    assert!(rustix.env.is_empty() && rustix.bootstrap.is_none());

    for (args, snippet) in [(INDEXMAP, INDEXMAP_SNIPPET), (BIGDECIMAL, BIGDECIMAL_SNIPPET)] {
        let c = compile(args, snippet, &QUIET, alloc);
        assert_eq!((c.target, c.edition), (HOST, Edition::E2015));
        assert_eq!(&c.line.crate_types[..], ["lib"]);
        assert!(c.line.codegen && c.line.emits.has("llvm-ir"), "autocfg's --emit=llvm-ir generates code");
        assert_eq!(c.crate_name, CrateName::Plain("autocfg_dca8b2ac880013e3_1"));
    }

    let env = [("RUSTC_BOOTSTRAP", "1"), ("OUT_DIR", "/out")];
    let bootstrapped = Child { host: HOST, env: &env, stdout_read: false };
    let pm2 = compile(PROC_MACRO2, PROC_MACRO2_SNIPPET, &bootstrapped, alloc);
    assert_eq!((pm2.target, pm2.edition, pm2.line.input), (WASM, Edition::E2021, Snippet::File("build/probe.rs")));
    assert_eq!(pm2.line.cap_lints, Some("allow"));
    assert!(!pm2.line.codegen && pm2.line.emits.has("dep-info") && pm2.line.emits.has("metadata"));
    assert_eq!(pm2.crate_name, CrateName::Plain("proc_macro2"));
    assert_eq!(pm2.bootstrap, Some("1"));
    assert_eq!(&pm2.env[..], [("RUSTC_BOOTSTRAP", Some("1"))], "its `option_env!(\"RUSTC_BOOTSTRAP\")` is held");
    let unset = compile(PROC_MACRO2, PROC_MACRO2_SNIPPET, &QUIET, alloc);
    assert_eq!(&unset.env[..], [("RUSTC_BOOTSTRAP", None)]);

    let thiserror = compile(THISERROR_2016, "", &QUIET, alloc);
    assert_eq!((thiserror.target, thiserror.edition, thiserror.line.cap_lints), (WASM, Edition::E2018, Some("allow")));

    assert!(matches!(asked(&["--version"], "", &QUIET, alloc), Ok(Probe::Version { verbose: false, host: HOST })));
    assert!(matches!(asked(&["-vV"], "", &QUIET, alloc), Ok(Probe::Version { verbose: true, host: HOST })));
    let Ok(Probe::Asks(Question::Print(cfg))) = asked(&["--print", "cfg", "--target", WASM], "", &QUIET, alloc) else { panic!("--print cfg") };
    assert_eq!((cfg.what, cfg.target), (PrintKind::Cfg, WASM));
    let Ok(Probe::Asks(Question::Print(list))) = asked(&["--print=target-list"], "", &QUIET, alloc) else { panic!("--print target-list") };
    assert_eq!((list.what, list.target), (PrintKind::Other("target-list"), HOST));
    println!("probe_check: real lines read");
}

/// One question, one key: autocfg's numbered crate names, the output folders, `--x v` against
/// `--x=v`, the order of flags, the inert `-C` keys and `-g`, and a variable the snippet does
/// not name do not split it; the target, `RUSTC_BOOTSTRAP`, a variable the snippet names, a kept
/// `-C`, the snippet and the checker's identity do.
/// Prints "probe_check: one question one key".
/// Control: hash `c.crate_name.name()` in `Question::key` and the first assertion fails.
#[test]
fn probe_check_two_spellings_share_a_key() {
    let alloc = blaze_own::testing::heap().alloc();
    let checker = id();
    let key = |args: &[&str], source: &str, child: &Child<'_>| key_of(args, source, child, &checker, alloc);
    let base = key(INDEXMAP, INDEXMAP_SNIPPET, &QUIET);
    let respelled = ["--target=aarch64-apple-darwin", "--emit=llvm-ir", "--crate-type", "lib", "--crate-name=autocfg_ffff_0", "--out-dir=/elsewhere", "-"];
    assert_eq!(base, key(&respelled, INDEXMAP_SNIPPET, &QUIET), "names, folders, order or spelling split one question");
    let inert = ["--crate-type=lib", "--emit=llvm-ir", "--target", HOST, "-C", "codegen-units=1", "-Cincremental=/tmp/inc", "-g", "-L", "native=/x", "-"];
    assert_eq!(base, key(&inert, INDEXMAP_SNIPPET, &QUIET), "an inert -C, -g or -L split one question");
    let env = [("OUT_DIR", "/out/a"), ("PATH", "/bin")];
    assert_eq!(base, key(INDEXMAP, INDEXMAP_SNIPPET, &Child { host: HOST, env: &env, stdout_read: false }), "a variable the snippet does not name split it");
    let print = key(&["--print", "cfg", "--target", WASM], "", &QUIET);
    assert_eq!(print, key(&["--print=cfg", "--target=wasm32-wasip1"], "", &QUIET));

    let wasm = ["--crate-type=lib", "--emit=llvm-ir", "--target", WASM, "-"];
    assert_ne!(base, key(&wasm, INDEXMAP_SNIPPET, &QUIET), "the target is not keyed");
    let stable = [("RUSTC_BOOTSTRAP", "-1")];
    assert_ne!(base, key(INDEXMAP, INDEXMAP_SNIPPET, &Child { host: HOST, env: &stable, stdout_read: false }), "RUSTC_BOOTSTRAP is not keyed");
    let opt = ["--crate-type=lib", "--emit=llvm-ir", "--target", HOST, "-C", "opt-level=3", "-"];
    assert_ne!(base, key(&opt, INDEXMAP_SNIPPET, &QUIET), "-C opt-level (debug_assertions) is not keyed");
    assert_ne!(base, key(INDEXMAP, BIGDECIMAL_SNIPPET, &QUIET), "the snippet is not keyed");
    let other = checker_id(&ContentId([1; 32]), &["default_field_values"], &ContentId([2; 32]));
    assert_ne!(base, key_of(INDEXMAP, INDEXMAP_SNIPPET, &QUIET, &other, alloc), "the implemented features are not keyed");
    let named = [("CARGO_PKG_NAME", "a")];
    let renamed = [("CARGO_PKG_NAME", "b")];
    let reads = "pub fn probe() { let _ = env!(\"CARGO_PKG_NAME\"); }";
    assert_ne!(
        key(INDEXMAP, reads, &Child { host: HOST, env: &named, stdout_read: false }),
        key(INDEXMAP, reads, &Child { host: HOST, env: &renamed, stdout_read: false }),
        "a variable the snippet names is not keyed"
    );
    println!("probe_check: one question one key");
}

/// A flag blaze does not model is refused by name, at its argument, never dropped or guessed;
/// a line rustc itself refuses is its rejection, decided from the line.
/// Prints "probe_check: refused by name".
/// Control: add `made-up` to `CODEGEN_DROPPED` and the first assertion fails.
#[test]
fn probe_check_a_flag_not_understood_is_refused_by_name() {
    let alloc = blaze_own::testing::heap().alloc();
    let refused = |args: &[&str], child: &Child<'_>| asked(args, "", child, alloc).err();
    assert_eq!(refused(&["--crate-type=lib", "-C", "made-up=1", "-"], &QUIET), Some(Refused { at: 2, why: Why::Codegen }));
    assert_eq!(refused(&["-Cmade-up=1", "--crate-type=lib", "-"], &QUIET), Some(Refused { at: 0, why: Why::Codegen }));
    assert_eq!(refused(&["--crate-type=lib", "-Z", "treat-err-as-bug", "-"], &QUIET), Some(Refused { at: 2, why: Why::Unstable }));
    assert_eq!(refused(&["--extern", "foo=/x/libfoo.rlib", "--crate-type=lib", "-"], &QUIET), Some(Refused { at: 1, why: Why::Extern }));
    assert_eq!(refused(&["--sysroot", "/x", "--crate-type=lib", "-"], &QUIET), Some(Refused { at: 1, why: Why::Sysroot }));
    assert_eq!(refused(&["--crate-type=lib", "--frobnicate", "-"], &QUIET), Some(Refused { at: 1, why: Why::Line(crate::probe_ask::Why::UnknownFlag) }));
    assert_eq!(refused(&["--edition", "future", "--crate-type=lib", "-"], &QUIET), Some(Refused { at: 1, why: Why::FutureEdition }));
    assert_eq!(refused(&["--print", "cfg", "--cfg", "x"], &QUIET), Some(Refused { at: 2, why: Why::PrintFlag }));
    assert_eq!(refused(&["--print", "cfg", "--print", "sysroot"], &QUIET), Some(Refused { at: 2, why: Why::PrintTwice }));
    assert_eq!(refused(&["--print", "cfg=/out/cfg.txt"], &QUIET), Some(Refused { at: 1, why: Why::PrintToFile }));
    assert_eq!(refused(&["-"], &QUIET), Some(Refused { at: 0, why: Why::Links }), "a bin with no --emit links");
    assert_eq!(refused(&["--crate-type=lib", "-"], &QUIET), None, "an rlib's link is an archive");
    // rustix 0.38's `-o -`: its stdout is `Stdio::null()`, so read; piped, it is refused.
    let piped = Child { host: HOST, env: &[], stdout_read: true };
    assert_eq!(refused(RUSTIX_038, &QUIET), None);
    assert_eq!(refused(RUSTIX_038, &piped), Some(Refused { at: 5, why: Why::OutputRead }));
    // rustc's own refusals of the line: answered exit 1 with no checker.
    let rows: [FakeRow<'_>; 0] = [];
    let checker = FakeChecker { id: id(), rows: &rows, calls: Cell::new(0) };
    let mut records = MemRecords::new(alloc);
    for args in [&["--edition=2017", "--crate-type=lib", "-"][..], &["--crate-name=a-b", "--crate-type=lib", "-"][..]] {
        let p = asked(args, "", &QUIET, alloc).expect("a line rustc refuses itself");
        assert!(matches!(p, Probe::Rejected { .. }), "{args:?}: {p:?}");
        let exit = answer(&p, &checker, &mut records, alloc).expect("answered from the line");
        assert_eq!(exit.code, 1);
        assert!(exit.stderr.starts_with(b"error"), "{args:?}");
    }
    assert_eq!((checker.calls.get(), records.writes), (0, 0));
    println!("probe_check: refused by name");
}

/// An answer is the checker's once, then the record's: the next script asking the same
/// question (another crate name, another folder) reads it and the checker is not asked; a
/// rejection is exit 1 with its first error; a version is the pin's text, asked of no one.
/// Prints "probe_check: answered once".
/// Control: skip `records.put` in `answer` and the second ask calls the checker again.
#[test]
fn probe_check_an_answer_is_stored_once_and_read_by_the_next_script() {
    let alloc = blaze_own::testing::heap().alloc();
    let rows = [
        FakeRow { source: INDEXMAP_SNIPPET.as_bytes(), target: HOST, verdict: FakeVerdict::Accepted },
        FakeRow { source: THUMB_SNIPPET.as_bytes(), target: HOST, verdict: FakeVerdict::Rejected("error: invalid register `r7`: unknown register") },
        FakeRow { source: b"cfg", target: WASM, verdict: FakeVerdict::Printed("panic=\"abort\"\ntarget_arch=\"wasm32\"\n") },
    ];
    let checker = FakeChecker { id: id(), rows: &rows, calls: Cell::new(0) };
    let mut records = MemRecords::new(alloc);
    let ask = |args: &[&str], source: &str, records: &mut MemRecords<'_>| {
        let p = asked(args, source, &QUIET, alloc).expect("a question");
        answer(&p, &checker, records, alloc).expect("answered")
    };
    let first = ask(INDEXMAP, INDEXMAP_SNIPPET, &mut records);
    assert_eq!((first.code, &first.stdout[..], &first.stderr[..]), (0, &b""[..], &b""[..]));
    let other_script = ["--crate-name", "autocfg_0123456789abcdef_0", "--crate-type=lib", "--out-dir", "/target/debug/build/num-traits-1/out", "--emit=llvm-ir", "--target", HOST, "-"];
    let again = ask(&other_script, INDEXMAP_SNIPPET, &mut records);
    assert_eq!(again, first);
    assert_eq!((checker.calls.get(), records.writes, records.hits.get()), (1, 1, 1), "the second script was not answered from the record");

    let thumb = ["--crate-type=rlib", "--emit=metadata", "--target", HOST, "-o", "/out/rustix_test_can_compile", "-"];
    let rejected = ask(&thumb, THUMB_SNIPPET, &mut records);
    assert_eq!((rejected.code, &rejected.stderr[..]), (1, &b"error: invalid register `r7`: unknown register\n"[..]));
    assert_eq!(ask(&thumb, THUMB_SNIPPET, &mut records), rejected);
    assert_eq!(checker.calls.get(), 2);

    let cfg = ask(&["--print", "cfg", "--target", WASM], "", &mut records);
    assert_eq!((cfg.code, &cfg.stdout[..]), (0, &b"panic=\"abort\"\ntarget_arch=\"wasm32\"\n"[..]));
    let version = ask(&["-vV"], "", &mut records);
    assert_eq!(&version.stdout[..], crate::probe::version(true, HOST, alloc).as_bytes());
    assert_eq!((checker.calls.get(), records.writes), (3, 3), "a version is the pin's, asked of no checker");
    println!("probe_check: answered once");
}

/// An incomplete check is neither success nor failure (D193 point 5): it stops the script by
/// name and writes nothing, so when the checker can answer, the next ask is answered and not
/// read as a failure. A read of a variable the key does not hold stops it the same way.
/// Prints "probe_check: unknown stops, never stored".
/// Control: store `Stored::Rejected` for `ProbeAnswer::Unknown` in `answer` and the second
/// ask is exit 1 from the record.
#[test]
fn probe_check_unknown_stops_the_script_and_is_not_stored() {
    let alloc = blaze_own::testing::heap().alloc();
    let gap = [FakeRow { source: PROC_MACRO2_SNIPPET.as_bytes(), target: WASM, verdict: FakeVerdict::Incomplete }];
    let checker = FakeChecker { id: id(), rows: &gap, calls: Cell::new(0) };
    let mut records = MemRecords::new(alloc);
    let p = asked(PROC_MACRO2, PROC_MACRO2_SNIPPET, &QUIET, alloc).expect("a question");
    let Err(Stop::Unknown { key, cause: Cause::Incomplete { .. } }) = answer(&p, &checker, &mut records, alloc) else { panic!("an incomplete check answered") };
    assert_eq!(records.writes, 0, "an unknown was stored");
    assert_eq!(records.get(&key, alloc).map(|found| found.is_none()), Ok(true));

    let fixed = [FakeRow { source: PROC_MACRO2_SNIPPET.as_bytes(), target: WASM, verdict: FakeVerdict::Accepted }];
    let fixed = FakeChecker { id: id(), rows: &fixed, calls: Cell::new(0) };
    let exit = answer(&p, &fixed, &mut records, alloc).expect("answered once the checker can");
    assert_eq!(exit.code, 0);
    assert_eq!((fixed.calls.get(), records.writes), (1, 1));

    // `env!(concat!(..))`: the name it reads is in no quotes, so the key does not hold it.
    let built = "pub fn probe() { let _ = env!(concat!(\"CARGO_\", \"PKG_NAME\")); }";
    let reads = [
        FakeRow { source: built.as_bytes(), target: HOST, verdict: FakeVerdict::ReadsEnv("CARGO_PKG_NAME") },
        FakeRow { source: INDEXMAP_SNIPPET.as_bytes(), target: HOST, verdict: FakeVerdict::ReadsEnv("RUSTC_BOOTSTRAP") },
    ];
    let reader = FakeChecker { id: id(), rows: &reads, calls: Cell::new(0) };
    let p = asked(INDEXMAP, built, &QUIET, alloc).expect("a question");
    let Err(Stop::Unknown { cause: Cause::EnvOutsideKey { name }, .. }) = answer(&p, &reader, &mut records, alloc) else { panic!("a read outside the key answered") };
    assert_eq!(name.as_str(), "CARGO_PKG_NAME");
    let p = asked(INDEXMAP, INDEXMAP_SNIPPET, &QUIET, alloc).expect("a question");
    assert_eq!(answer(&p, &reader, &mut records, alloc).expect("RUSTC_BOOTSTRAP is keyed on its own").code, 0);
    assert_eq!(records.writes, 2);
    println!("probe_check: unknown stops, never stored");
}

/// A record reads back as written, byte for byte; a byte more or less is malformed; another
/// answer under a written key is refused (rule 21), the same one again is no write (rule 25).
/// Prints "probe_check: records round-trip".
#[test]
fn probe_check_records_round_trip() {
    let alloc = blaze_own::testing::heap().alloc();
    for stored in [Stored::Accepted, Stored::Rejected { first: "error[E0599]: no method" }, Stored::Printed { text: "unix\n" }] {
        let bytes = encode(&stored, alloc);
        assert_eq!(decode(&bytes), Ok(stored));
        assert!(decode(&bytes[..bytes.len() - 1]).is_err() || matches!(stored, Stored::Accepted));
        let mut longer = blaze_own::Vec::new_in(alloc);
        longer.extend_from_slice(&bytes);
        longer.push(0);
        assert!(decode(&longer).is_err(), "{stored:?} with a byte more");
    }
    let mut records = MemRecords::new(alloc);
    let key = ProbeKey(ContentId([9; 32]));
    assert_eq!(records.put(&key, &encode(&Stored::Accepted, alloc)), Ok(()));
    assert_eq!(records.put(&key, &encode(&Stored::Accepted, alloc)), Ok(()));
    assert_eq!(records.put(&key, &encode(&Stored::Rejected { first: "error" }, alloc)), Err(Twice(key)));
    assert_eq!(records.writes, 1);
    println!("probe_check: records round-trip");
}
