//! A file read by `include_str!` or `include_bytes!` contributes its bytes' digest to the
//! read lists of the crate's names and the body containing the call.
//! tempo's first stop (2026-11-16 19:03): "the names of package `tempo-node` read
//! `crates/node/tests/assets/test-genesis.json`, which no manifest's targets and no file's
//! `mod` lines list". A child of main.rs's tests: its helpers are theirs.
//!
//! The crates here are `#![no_core]` and declare the two builtins as core does
//! (`#[rustc_builtin_macro] macro_rules! include_str`), so the names step expands the calls;
//! each call is a statement call in a body, the place tempo's are (`tests/it/tip20.rs`).

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
    assert!(checked.failed.is_empty(), "{:?}\n{}", checked.failed, checked.profile);
    // A refused body is reported at exit 4 even when every task answered. The check may
    // still report the command-wide coverage limit (exit 3).
    assert!(matches!(checked.status, Some(0 | 3)), "status {:?}\n{}\n{}", checked.status, stdout(io), checked.profile);
}

/// (1) An `include_str!` of a non-Rust file under `tests/assets` and an `include_bytes!` of a
/// binary file check: the request answers (no `Unlisted` stop), no task fails, and each data
/// file is outlined as data: three `FileItems` records (`src/lib.rs` and the two data files).
/// Without data_include.rs's listing in work.rs and names.rs's `read_data` the request stops
/// as tempo's did: "the names of package `wide` read `tests/assets/test-genesis.json`, which
/// no manifest's targets and no file's `mod` lines list".
///
/// The cold check includes core, alloc and std (blaze-world list.rs), the library and its
/// test unit (drive.md, “a library's test unit (`TestLib`) is a second `CrateScope`”).
/// Only the Rust file has a surface. Its one function has one body record across both owners.
///
/// Prints `data includes: FileItems made 3, CrateScope made 5`.
#[test]
fn a_data_include_is_a_read_of_the_names_never_a_module() {
    let io = package();
    let whole = match ask(&io, (Command::Check, "check"), work::Sizes::USUAL) {
        Ok(whole) => whole,
        Err(error) => panic!("the request stops: {error}"),
    };
    assert_includes_checked(&io, &whole);
    let (items, scopes) = (made(&whole.profile, "FileItems"), made(&whole.profile, "CrateScope"));
    assert_eq!((items, scopes, made(&whole.profile, "Surface")), (3, 5, 1), "{}\n{}", stdout(&io), whole.profile);
    assert_eq!(made(&whole.profile, "BodyCheck"), 1, "both includes belong to f:\n{}", whole.profile);
    println!("data includes: FileItems made {items}, CrateScope made {scopes}");
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
/// second check makes one `FileItems` (the JSON's, its out its bytes' id) and two `CrateScope`
/// records. Both the library and its test unit in `a` read the old digest. The two scopes of
/// `b` and the three toolchain scopes stand. Only the body containing the include reads the
/// JSON. A third check with nothing changed reuses that body's answer as well.
///
/// Prints `json edited: FileItems made 1, CrateScope made 2`.
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
    let first = check(&io, work::Sizes::USUAL);
    assert_includes_checked(&io, &first);
    assert_eq!(
        (made(&first.profile, "FileItems"), made(&first.profile, "CrateScope")),
        (3, 7),
        "two libraries and one JSON file, four member units and three toolchain crates:\n{}",
        first.profile,
    );
    assert_eq!((made(&first.profile, "BodyCheck"), made(&first.profile, "Surface")), (3, 2), "f, unrelated and h in two Rust files:\n{}", first.profile);
    write(&io, "a/assets/g.json", "{\"n\": 2}\n");
    let edited = check(&io, work::Sizes::USUAL);
    assert_includes_checked(&io, &edited);
    let (items, scopes) = (made(&edited.profile, "FileItems"), made(&edited.profile, "CrateScope"));
    assert_eq!((items, scopes, made(&edited.profile, "Surface")), (1, 2, 0), "only the JSON and the names of its two readers:\n{}", edited.profile);
    assert_eq!(made(&edited.profile, "BodyCheck"), 1, "only f reads the edited JSON:\n{}", edited.profile);
    let same = check(&io, work::Sizes::USUAL);
    assert_includes_checked(&io, &same);
    assert_eq!((made(&same.profile, "FileItems"), made(&same.profile, "CrateScope")), (0, 0), "{}", same.profile);
    assert_eq!(made(&same.profile, "BodyCheck"), 0, "the included bytes are unchanged:\n{}", same.profile);
    assert_eq!(made(&same.profile, "Surface"), 0, "both Rust files are unchanged:\n{}", same.profile);
    println!("json edited: FileItems made {items}, CrateScope made {scopes}");
}
