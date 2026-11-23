//! A file read by `include_str!` or `include_bytes!` contributes its bytes' digest to the
//! read lists of the crate's names and the body containing the call.
//! tempo's first stop (2026-11-16 19:03): "the names of package `tempo-node` read
//! `crates/node/tests/assets/test-genesis.json`, which no manifest's targets and no file's
//! `mod` lines list". A child of main.rs's tests: its helpers are theirs.
//!
//! The crates here are `#![no_core]` and declare the two builtins as core does
//! (`#[rustc_builtin_macro] macro_rules! include_str`); each call is a statement call in a
//! body, the place tempo's are (`tests/it/tip20.rs`). Since 2026-11-20 the names step does not
//! expand them there (their output is a literal and declares nothing: blaze-scope resolve.rs
//! `LITERAL_BUILTINS`): the body is their one reader. A body's `include!` is read by both
//! (tests 4 and 5).

use super::*;

/// core `macros/mod.rs`: `include_str!` "Includes a UTF-8 encoded file as a string. The file is
/// located relative to the current file"; `include_bytes!` "Includes a file as a reference to a
/// byte array" (any bytes: here two that are not UTF-8).
const LIB: &str = "#![no_core]
#[rustc_builtin_macro]
macro_rules! include_str { () => {} }
#[rustc_builtin_macro]
macro_rules! include_bytes { () => {} }
pub fn f() {
    include_str!(\"../tests/assets/test-genesis.json\");
    include_bytes!(\"blob.bin\");
}
";

fn publish(io: &Io, path: &str, bytes: &[u8]) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(bytes), Durable::No).expect("write");
}

/// The one package of tests (1) and (2): a JSON file under `tests/assets` and a binary blob
/// (two bytes that are not UTF-8).
fn package() -> Io {
    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", LIB);
    write(&io, "tests/assets/test-genesis.json", "{\"config\": {\"chainId\": 1337}}\n");
    publish(&io, "src/blob.bin", b"\xff\xfe\x00\x01");
    io
}

fn assert_includes_checked(io: &Io, checked: &Checked) {
    assert_eq!(checked.failures, 0, "{}\n{}", stdout(io), checked.profile);
    assert!(!stdout(io).contains("which no manifest's targets"), "a data include is no unlisted module:\n{}", stdout(io));
    // A refused body is reported at exit 4 even when every task answered. The check may
    // still report the command-wide coverage limit (exit 3).
    assert!(matches!(checked.status, Some(0 | 3)), "status {:?}\n{}\n{}", checked.status, stdout(io), checked.profile);
}

/// (1) An `include_str!` of a non-Rust file under `tests/assets` and an `include_bytes!` of a
/// binary file check through the engine: no stop, no task fails, and the one function holding
/// both includes is one body entered once. (Ported 2026-11-19 from the old checker, whose
/// `FileItems`, `CrateScope` and `Surface` made counts this test also pinned; the engine's
/// stages count those otherwise.) Before data includes were listed the command stopped as
/// tempo's did: "the names of package `wide` read `tests/assets/test-genesis.json`, which no
/// manifest's targets and no file's `mod` lines list".
///
/// Prints `data includes: Bodies entered 1, Names entered <n>`.
#[test]
fn a_data_include_is_a_read_of_the_names_never_a_module() {
    let io = package();
    let whole = check(&io);
    assert_includes_checked(&io, &whole);
    let (bodies, names) = (whole.entered(blaze_engine::Stage::Bodies), whole.entered(blaze_engine::Stage::Names));
    assert_eq!(bodies, 2, "both includes belong to f, checked in its 2 units' rows (D191: the library's and its test build's):\n{}\n{}", stdout(&io), whole.profile);
    assert!(names > 0, "the names are resolved over the includes:\n{}", whole.profile);
    println!("data includes: Bodies entered {bodies}, Names entered {names}");
}

// (2) The control (rule 23) is a run of test (1) with the kind ignored again: blaze-scope
// resolve.rs `Host::include` reading `IncludeAs::Str | IncludeAs::Bytes` through
// `SourceTexts::read` (the old path: every include a module file wanting its `Surface`). The
// names' crate scope runs on a pool worker, so no thread's switch reaches it: the run is the
// integrator's, by rule 8 (the controls folder), and test (1) then fails with "the request
// stops: the names of package `wide` read `tests/assets/test-genesis.json`, which no
// manifest's targets and no file's `mod` lines list" or, the JSON being a listed data part
// now, `failed` holding "a part its file's record does not list".

/// (3) A changed data file moves exactly the records that read it. Two members: `a` includes
/// `a/assets/g.json`, `b` reads no data. After the first check, the JSON alone is edited: the
/// second check enters the one body that reads it (`f`; not `unrelated`, not `h`), and fewer
/// names than cold (`a`'s readers, not `b`'s or the toolchain's). A third check with nothing
/// changed stands on its certificate. (Ported 2026-11-19 from the old checker's made counts.)
///
/// Prints `json edited: Bodies entered 1, Names entered <n> of <cold>`.
#[test]
fn a_changed_data_file_moves_exactly_its_readers() {
    let io = Io::simulated(SimWorld::new(42, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(
        &io,
        "a/src/lib.rs",
        "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include_str { () => {} }\npub fn f() {\n    include_str!(\"../assets/g.json\");\n}\npub fn unrelated() {}\n",
    );
    write(&io, "a/assets/g.json", "{\"n\": 1}\n");
    write(&io, "b/src/lib.rs", "#![no_core]\npub fn h() {}\n");
    let first = check(&io);
    assert_includes_checked(&io, &first);
    let cold = first.entered(blaze_engine::Stage::Names);
    // D191: every unit that holds a body checks it; each member's library and its test build
    // hold its bodies, so a body is 2 rows.
    assert_eq!(first.entered(blaze_engine::Stage::Bodies), 6, "f, unrelated and h, 2 units' rows each (D191):\n{}", first.profile);
    write(&io, "a/assets/g.json", "{\"n\": 2}\n");
    let edited = check(&io);
    assert_includes_checked(&io, &edited);
    let (bodies, names) = (edited.entered(blaze_engine::Stage::Bodies), edited.entered(blaze_engine::Stage::Names));
    assert_eq!(bodies, 2, "only f reads the edited JSON, in its 2 units' rows (D191):\n{}", edited.profile);
    assert!(names < cold, "only `a`'s names read the JSON, not `b`'s or the toolchain's: {names} of {cold}\n{}", edited.profile);
    let same = check(&io);
    assert_includes_checked(&io, &same);
    assert!(same.stood(), "the included bytes are unchanged: the certificate\n{}", same.profile);
    println!("json edited: Bodies entered {bodies}, Names entered {names} of {cold}");
}

/// A body's `include!` in expression position: core `include!`, "Parses a file as an expression
/// or an item according to the context"; here the file is one block expression that declares
/// a struct and its impl. The names step expands it (its block may declare: blaze-scope
/// resolve.rs `expand_call`) and reads `gen.in`, so the plan must hand that read before the
/// names phase: the outline of `lib.rs` lists it as a data part (data_include.rs), and both the
/// names and the body read it as data.
const EXPR_INCLUDE: &str = "#![no_core]
#[rustc_builtin_macro]
macro_rules! include { () => {} }
pub fn f() -> u32 {
    let v = include!(\"gen.in\");
    v
}
";

const GEN_IN: &str = "{ struct S; impl S { fn n(&self) -> u32 { 7 } } S.n() }\n";

fn expr_include(seed: u64, with_file: bool) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", EXPR_INCLUDE);
    if with_file {
        write(&io, "src/gen.in", GEN_IN);
    }
    io
}

/// (4) One `check` of [`EXPR_INCLUDE`] with `src/gen.in` present is clean: no task fails, no
/// error, and the names phase is not stopped by an unplanned read (tempo, 2026-11-20 21:41:
/// "check: Name { kind: CrateScope, .. } needs Name { kind: FileItems, .. }, which its plan did
/// not hand it", when an expression call's file was read and no outline listed it).
///
/// CONTROL (rule 23): with `include` dropped from data_include.rs's names (the body's
/// `include!` no part), this fails with that "which its plan did not hand it" stop.
///
/// Prints `expression include: Bodies entered <n>, Names entered <m>`.
#[test]
fn an_expression_include_in_a_body_is_planned_and_checks_clean() {
    let io = expr_include(43, true);
    let checked = check(&io);
    let out = stdout(&io);
    assert!(!out.contains("which its plan did not hand it") && !checked.profile.contains("which its plan did not hand it"), "the read of gen.in is planned:\n{out}\n{}", checked.profile);
    assert_eq!((checked.failures, checked.errors), (0, 0), "{:?}\n{out}\n{}", checked.words, checked.profile);
    assert!(matches!(checked.status, Some(0 | 3)), "status {:?}\n{out}\n{}", checked.status, checked.profile);
    let (bodies, names) = (checked.entered(blaze_engine::Stage::Bodies), checked.entered(blaze_engine::Stage::Names));
    assert!(bodies >= 1 && names > 0, "f is checked over its include:\n{}", checked.profile);
    println!("expression include: Bodies entered {bodies}, Names entered {names}");
}

/// (5) The twin: `src/gen.in` missing. The read is still planned (an absent data part, no
/// finding of its own), and the call that reads it reports it: a typed error naming the file
/// (`including `gen.in`: no such file`, blaze-expand failed.rs) and the call (`the call of
/// `include``, names/failed_call.rs), at `src/lib.rs`; no stop, no failed task.
///
/// Prints `missing include: <the finding's line>`.
#[test]
fn a_missing_expression_include_is_an_error_naming_the_file_and_the_call() {
    let io = expr_include(44, false);
    let checked = check(&io);
    let out = stdout(&io);
    assert!(!out.contains("which its plan did not hand it"), "the read of gen.in is planned:\n{out}");
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert!(checked.errors >= 1, "a missing include is an error:\n{out}");
    let line = out.lines().find(|line| line.contains("including `gen.in`: no such file")).unwrap_or_else(|| panic!("no finding names gen.in:\n{out}"));
    assert!(out.contains("the call of `include`"), "a finding names the call:\n{out}");
    assert!(out.contains("src/lib.rs"), "the call's file is named:\n{out}");
    println!("missing include: {line}");
}

/// `include_str!` and `include_bytes!` in a body (statements, as tempo's `tests/it/tip20.rs`)
/// and at item level (a `const` and a `static`, bodies of their own): each names a file of its
/// own. The names step never expands them (blaze-scope resolve.rs `LITERAL_BUILTINS`); the
/// body is their one reader.
const DATA_CALLS: &str = "#![no_core]
#[rustc_builtin_macro]
macro_rules! include_str { () => {} }
#[rustc_builtin_macro]
macro_rules! include_bytes { () => {} }
pub fn f() {
    include_str!(\"in_body.json\");
    include_bytes!(\"in_body.bin\");
}
pub const S: &str = include_str!(\"at_item.json\");
pub static B: &[u8; 2] = include_bytes!(\"at_item.bin\");
";

const DATA_FILES: [&str; 4] = ["in_body.json", "in_body.bin", "at_item.json", "at_item.bin"];

fn data_calls(seed: u64, with_files: bool) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", DATA_CALLS);
    if with_files {
        write(&io, "src/in_body.json", "{}\n");
        write(&io, "src/at_item.json", "{}\n");
        publish(&io, "src/in_body.bin", b"\xff\xfe");
        publish(&io, "src/at_item.bin", b"\x00\x01");
    }
    io
}

/// (6) Every data file of [`DATA_CALLS`] missing: one error at each call naming its file
/// (rustc: "couldn't read `src/in_body.json`: No such file or directory", once a call), no stop
/// of the command, no failed task, and the rest still checked (the body and both items are
/// entered). Before the absent file was an answer (data_include.rs `absent_out`), the plan
/// stopped at the first: `EngineCheck(Absent { path: Path("src/in_body.json") })`.
///
/// THE TWIN: the same crate with the four files present names no missing file.
///
/// Prints `missing data includes: <errors> errors, Bodies entered <n>`.
#[test]
fn a_missing_data_include_is_an_error_at_its_call_in_a_body_and_at_item_level() {
    let io = data_calls(45, false);
    let checked = check(&io);
    let out = stdout(&io);
    assert!(!out.contains("a module's file is absent"), "an absent data file stops nothing:\n{out}");
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    for file in DATA_FILES {
        assert!(out.contains(&std::format!("including `{file}`: no such file")), "no finding names {file}:\n{out}");
    }
    assert!(checked.errors >= 4, "one error a call:\n{out}");
    let bodies = checked.entered(blaze_engine::Stage::Bodies);
    assert!(bodies >= 3, "f, S and B are checked:\n{}", checked.profile);
    let twin = data_calls(46, true);
    let present = check(&twin);
    let twin_out = stdout(&twin);
    assert_eq!(present.failures, 0, "{twin_out}\n{}", present.profile);
    assert!(!twin_out.contains("no such file"), "the files are there:\n{twin_out}");
    println!("missing data includes: {} errors, Bodies entered {bodies}", checked.errors);
}

/// (7) A missing file created later moves its cell and enters its reader again: the first
/// check of [`EXPR_INCLUDE`] without `src/gen.in` reports it at the call; once the file is
/// written, the second check reports nothing missing and enters the body `f` again (its read
/// of `gen.in`'s cell moved from the absent answer to the bytes' digest). A third check with
/// nothing changed stands on its certificate.
///
/// THE TWIN: without the write, the second check stands (no reader entered: the absent answer
/// is stable, so a missing file costs nothing on a later run).
///
/// Prints `created include: Bodies entered <n> after the file was written`.
#[test]
fn a_created_include_file_enters_its_reader_again() {
    let io = expr_include(47, false);
    let first = check(&io);
    assert!(stdout(&io).contains("including `gen.in`: no such file"), "the first check reports it:\n{}", stdout(&io));
    assert_eq!(first.failures, 0, "{}", first.profile);
    let again = check(&io);
    assert!(again.stood(), "nothing changed, the absent answer stands:\n{}", again.profile);
    write(&io, "src/gen.in", GEN_IN);
    let created = check(&io);
    let out = stdout(&io);
    assert_eq!(created.failures, 0, "{out}\n{}", created.profile);
    let bodies = created.entered(blaze_engine::Stage::Bodies);
    assert!(bodies >= 1, "the reader of gen.in is entered again:\n{}", created.profile);
    let same = check(&io);
    assert!(same.stood(), "the created file is unchanged: the certificate\n{}", same.profile);
    println!("created include: Bodies entered {bodies} after the file was written");
}

/// (8) A Rust source file read BOTH as a `mod` (in `lib.rs` / `sub/mod.rs`) AND as data via
/// `include_str!` (in another target or deeper module), in BOTH walk orders:
/// - Wave-earlier data include (the `blaze-ladder` repro: `tests/a_reader.rs` at wave 0 does
///   `include_str!("../src/sub/body.rs")` and `include_str!("../src/sub/mod.rs")`, while
///   `src/lib.rs` -> `src/sub/mod.rs` -> `pub mod body;` only reaches `body.rs` at wave 2).
/// - Wave-later data include (`src/lib.rs` reaches `src/foo.rs` at wave 1 via `pub mod foo;`,
///   while `src/deep/reader.rs` reaches `src/foo.rs` at wave 2 via `include_str!("../foo.rs")`).
///
/// Neither walk order stops with `UnseenFile`; `src/sub/body.rs` is checked as Rust (not shadowed
/// by `DATA_FILE`); a comment-only edit in `src/sub/body.rs` moves its `DATA` cell and re-enters
/// the `include_str!` body without re-entering unrelated bodies or `Names`; and replay stands.
#[test]
fn a_rust_module_also_read_by_include_str_checks_in_both_walk_orders_and_invalidates_on_comment_edit() {
    let manifest = "\
[package]
name = \"dual\"
version = \"0.1.0\"
edition = \"2024\"
";
    let lib = "\
#![no_core]
#[rustc_builtin_macro]
macro_rules! include_str { () => {} }
pub mod sub;
pub mod foo;
pub mod deep;
pub fn call_both() -> u32 { sub::body::val() + foo::val() }
";
    let sub_mod = "pub mod body;\n";
    let body_v1 = "// version 1\npub fn val() -> u32 { 1 }\n";
    let body_v2_comment = "// version 2 (comment-only edit)\npub fn val() -> u32 { 1 }\n";
    let foo_v1 = "// foo 1\npub fn val() -> u32 { 2 }\n";
    let deep_mod = "pub mod reader;\n";
    let deep_reader = "\
pub fn read_foo() {
    include_str!(\"../foo.rs\");
}
";
    // Integration test root (wave 0): names `../src/sub/body.rs` and `../src/sub/mod.rs` BEFORE
    // `src/sub/mod.rs` reaches `pub mod body;` at wave 2.
    let test_reader = "\
#![no_core]
#[rustc_builtin_macro]
macro_rules! include_str { () => {} }
pub fn reads_dual() {
    include_str!(\"../src/sub/body.rs\");
    include_str!(\"../src/sub/mod.rs\");
}
pub fn unrelated_test() {}
";

    let io = Io::simulated(SimWorld::new(48, Limits::USUAL));
    write(&io, "Cargo.toml", manifest);
    write(&io, "src/lib.rs", lib);
    write(&io, "src/sub/mod.rs", sub_mod);
    write(&io, "src/sub/body.rs", body_v1);
    write(&io, "src/foo.rs", foo_v1);
    write(&io, "src/deep/mod.rs", deep_mod);
    write(&io, "src/deep/reader.rs", deep_reader);
    write(&io, "tests/a_reader.rs", test_reader);

    let first = check(&io);
    let out = stdout(&io);
    assert!(!out.contains("which its plan did not hand it"), "dual-included module must not fail with UnseenFile:\n{out}\n{}", first.profile);
    assert_includes_checked(&io, &first);
    assert_eq!(first.errors, 0, "clean dual-include workspace must report 0 errors:\n{out}\n{}", first.profile);

    // A comment-only edit in `src/sub/body.rs` leaves its Rust `Surface` and `BodyCheck` intact
    // while moving its `DATA` cell: ONLY `tests/a_reader.rs::reads_dual` re-enters (not
    // `unrelated_test`, not `body::val`, not `Names`).
    write(&io, "src/sub/body.rs", body_v2_comment);
    let edited = check(&io);
    assert_includes_checked(&io, &edited);
    assert_eq!(edited.entered(blaze_engine::Stage::Names), 0, "comment edit must not re-enter Names:\n{}", edited.profile);
    assert_eq!(edited.entered(blaze_engine::Stage::Bodies), 1, "only the integration test body reading body.rs via include_str! must re-enter:\n{}", edited.profile);

    // Unchanged replay stands on its certificate.
    let same = check(&io);
    assert_includes_checked(&io, &same);
    assert!(same.stood(), "unchanged dual-include workspace must stand:\n{}", same.profile);

    // Same-wave reverse order: `tests/a_reader.rs` includes `../src/foo.rs` (wave 1) alongside
    // `src/lib.rs`'s `pub mod foo;` (wave 1). Both are in wave 0; `a_reader.rs` sorts before
    // `lib.rs` in the target list, so `DATA_FILE` is encountered before `pub mod foo;`.
    write(
        &io,
        "tests/a_reader.rs",
        "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include_str { () => {} }\npub fn reads_dual() {\n    include_str!(\"../src/sub/body.rs\");\n    include_str!(\"../src/foo.rs\");\n}\n",
    );
    let same_wave = check(&io);
    assert!(!stdout(&io).contains("which its plan did not hand it"), "same-wave dual-include must not fail with UnseenFile:\n{}", stdout(&io));
    assert_includes_checked(&io, &same_wave);
    assert_eq!(same_wave.errors, 0, "same-wave dual-include must check clean:\n{}", stdout(&io));
    println!("dual include (both walk orders + comment edit): ok");
}

/// (9) A file first walked as `DATA_FILE` (`include_str!("../src/gen_mod.rs")` in a test target)
/// and then discovered as a Rust module during `Names` via a macro-written `mod $n;`: `unoutlined`
/// upgrades it to Rust, `serve_found` replaces the `DATA_FILE` plan cleanly, and replay stands.
#[test]
fn a_data_included_file_also_declared_by_macro_mod_upgrades_cleanly() {
    let manifest = "\
[package]
name = \"dual_found\"
version = \"0.1.0\"
edition = \"2024\"
";
    let lib = "\
#![no_core]
macro_rules! decl { ($n:ident) => { pub mod $n; }; }
decl!(gen_mod);
pub fn top() -> u32 { gen_mod::v() }
";
    let gen_mod = "pub fn v() -> u32 { 7 }\n";
    let test_reader = "\
#![no_core]
#[rustc_builtin_macro]
macro_rules! include_str { () => {} }
pub fn read_gen() { include_str!(\"../src/gen_mod.rs\"); }
";

    let io = Io::simulated(SimWorld::new(49, Limits::USUAL));
    write(&io, "Cargo.toml", manifest);
    write(&io, "src/lib.rs", lib);
    write(&io, "src/gen_mod.rs", gen_mod);
    write(&io, "tests/reader.rs", test_reader);

    let first = check(&io);
    let out = stdout(&io);
    assert!(!out.contains("which its plan did not hand it"), "macro-found dual-include must upgrade from DATA_FILE:\n{out}\n{}", first.profile);
    assert_includes_checked(&io, &first);
    assert_eq!(first.errors, 0, "macro-found dual-include must check clean:\n{out}\n{}", first.profile);

    let again = check(&io);
    assert_includes_checked(&io, &again);
    assert!(again.stood(), "replay after macro-found dual-include upgrade must stand:\n{}", again.profile);
    println!("dual include macro-found upgrade: ok");
}
