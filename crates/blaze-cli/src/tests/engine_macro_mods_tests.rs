//! A MODULE A MACRO CALL DECLARES IS SERVED (turso 2026-11-21: `check` stopped in Names wave 17
//! on `core/io/unix.rs`, the file of a `mod unix;` that `cfg_block! { #[cfg(..)] { mod unix; } }`
//! writes in `turso_core`'s `core/io/mod.rs`, read by the crate's names and never outlined). The
//! file walk plans no `mod` a macro writes: the crate's names expand the call under the unit's
//! cfg, read the file where the expansion's module puts it (the Reference, "Modules";
//! rustc_expand module.rs `mod_file_path` from the expansion's module directory; blaze-scope
//! resolve.rs `make_modules`), outline it on their worker and it is filed after them (D161,
//! engine_check.rs `serve_found`): outlined, its names read and its bodies checked, as a `mod`
//! written plainly. Until 2026-11-21 a token scan of each macro call planned the `mod NAME;` it
//! held literally, every cfg arm; deleted once the serve took every case, it outlined the files
//! of cfg arms that are off. A child of main.rs's tests: its helpers are theirs.
//!
//! The rule (the Reference, "Macros By Example"): `$($t:tt)*` transcribed is the call's own
//! tokens, so `m! { mod a; }` declares `mod a;` in the module of the call; "Type coercions":
//! `fn f() -> u8 { 1u16 }` is one E0308, the error that shows a file's bodies were checked.

use super::engine_check_tests::{boxed, engine, error_lines, opts};
use super::*;

/// The macro that writes its input back: what `cfg_block!` and `cfg_if!` do with each arm.
const ECHO: &str = "macro_rules! m {\n    ($($t:tt)*) => { $($t)* };\n}\n";

/// A body that checks.
pub(super) const GOOD: &str = "pub fn f() -> u8 { 1 }\n";
/// A body with one E0308: the file's bodies were checked when it prints.
pub(super) const BAD: &str = "pub fn f() -> u8 { 1u16 }\n";

/// One fresh check of `files` (`Cargo.toml` written too): whether it failed, and what it printed.
pub(super) fn checked(seed: u64, files: &[(&str, &str)]) -> (bool, std::string::String) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    for (path, text) in files {
        write(&io, path, text);
    }
    let before = stdout(&io).len();
    let (_, checked) = engine(&io, 2);
    let said = stdout(&io).split_off(before);
    (checked.status.0 == Status::FAILED.0, said)
}

/// One fresh check of `files` that must stop: the error's words. A check that answers fails the
/// test with what it printed.
pub(super) fn refused(seed: u64, files: &[(&str, &str)]) -> std::string::String {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    for (path, text) in files {
        write(&io, path, text);
    }
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let (out, err) = (io.need_out().expect("out"), io.need_err().expect("err"));
    let mut phases = boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let args = io.args();
    let net = io.net();
    match engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, opts(&io, 2, true), &mut shown, &mut phases, &out, &err) {
        Ok(checked) => panic!("the check answered (status {}) where it must stop:\n{}", checked.status.0, stdout(&io)),
        Err(error) => std::format!("{error}"),
    }
}

/// The good and the bad form of the file at `path` beside `lib` (and `more`): the good one checks
/// clean, the bad one prints its one E0308 placed in `path`, so the file was outlined, its names
/// read and its bodies checked. Neither stops on a read the plan did not hand.
pub(super) fn good_and_bad(seed: u64, lib: &str, path: &str, more: &[(&str, &str)]) {
    for (text, wrong) in [(GOOD, false), (BAD, true)] {
        let mut files = std::vec::Vec::from([("src/lib.rs", lib), (path, text)]);
        files.extend_from_slice(more);
        let (failed, said) = checked(seed, &files);
        println!("macro-declared module {path} (wrong {wrong}): failed {failed}\n{said}");
        assert!(!said.contains("which its plan did not hand it"), "the read of {path} is planned:\n{said}");
        assert_eq!(failed, wrong, "the check fails exactly when {path} holds the bad body:\n{said}");
        let errors = error_lines(&said);
        assert_eq!(errors.len(), usize::from(wrong), "the bad body's one error and no other:\n{said}");
        let shown = std::format!("{path}:");
        assert!(errors.iter().all(|line| line.contains(&shown)), "the error is placed in {path}:\n{said}");
    }
}

/// `m! { mod a; }`: the file `src/a.rs` is planned beside the crate root, `top` resolves
/// `a::f` through it, and `a.rs`'s body is checked.
#[test]
fn a_module_a_macro_call_declares_is_checked() {
    let lib = std::format!("{ECHO}m! {{ mod a; }}\npub fn top() -> u8 {{ a::f() }}\n");
    good_and_bad(11, &lib, "src/a.rs", &[]);
}

/// THE CONTROL (rule 23): the same crate with the serve removed (engine_check.rs
/// `without_found_serve`) stops as turso did, on the read of `src/a.rs` its plan did not hand,
/// named by the file: the serve is the one thing that plans a macro-declared module (the walk
/// plans none).
#[test]
fn without_the_serve_the_macro_declared_file_is_the_unseen_read() {
    let lib = std::format!("{ECHO}m! {{ mod a; }}\npub fn top() -> u8 {{ a::f() }}\n");
    let words = crate::engine_check::without_found_serve(|| refused(11, &[("src/lib.rs", lib.as_str()), ("src/a.rs", GOOD)]));
    println!("without the serve: {words}");
    assert!(words.contains("which its plan did not hand it") && words.contains("`src/a.rs`"), "the refusal names the file:\n{words}");
}

/// A `mod` under a false `cfg` inside the call (`cfg_block!`'s other arms): the expansion under
/// the unit's cfg writes no `mod off;`, so no names read `src/off.rs`: it is never outlined
/// (no place in the stored answer), its bad body prints nothing and the check is clean.
/// CONTROL: the predicate made true, the same file is served and its E0308 prints, so the
/// silence is the cfg's, not a file left out.
#[test]
fn a_cfg_false_module_in_a_macro_call_is_no_module() {
    for (predicate, on) in [("any()", false), ("all()", true)] {
        let lib = std::format!("{ECHO}m! {{\n    #[cfg({predicate})]\n    mod off;\n    mod a;\n}}\npub fn top() -> u8 {{ a::f() }}\n");
        let files = [("src/lib.rs", lib.as_str()), ("src/a.rs", GOOD), ("src/off.rs", BAD)];
        let (failed, said) = checked(12, &files);
        println!("cfg({predicate}) mod off in a macro call: failed {failed}\n{said}");
        assert!(!said.contains("which its plan did not hand it"), "every read is planned:\n{said}");
        assert_eq!(failed, on, "only a cfg-true `off` fails the check:\n{said}");
        let errors = error_lines(&said);
        assert_eq!(errors.len(), usize::from(on), "off.rs's one error exactly when its cfg holds:\n{said}");
        assert!(errors.iter().all(|line| line.contains("src/off.rs:")), "the error is off.rs's:\n{said}");
        let place = super::engine_found_files_tests::stored_file(&super::engine_found_files_tests::world(12, &files), "src/off.rs").1;
        assert_eq!(place.is_some(), on, "off.rs is outlined and placed exactly when its cfg holds: {place:?}");
    }
}

/// `m! { #[path = "x.rs"] mod a; }`: the file the attribute names, relative to the crate root's
/// directory, is planned and checked; `src/a.rs` is never needed.
#[test]
fn a_path_attribute_inside_a_macro_call_names_the_file() {
    let lib = std::format!("{ECHO}m! {{ #[path = \"x.rs\"] mod a; }}\npub fn top() -> u8 {{ a::f() }}\n");
    good_and_bad(13, &lib, "src/x.rs", &[]);
}

/// A plain `mod b;` inside the file a macro call declared: `src/a.rs` is a module's file that
/// is no `mod.rs`, so `b` is at `src/a/b.rs`, planned as the walk goes on from `a.rs`.
#[test]
fn a_module_inside_the_macro_declared_file_is_planned() {
    let lib = std::format!("{ECHO}m! {{ mod a; }}\npub fn top() -> u8 {{ a::b::f() }}\n");
    good_and_bad(14, &lib, "src/a/b.rs", &[("src/a.rs", "pub mod b;\n")]);
}

/// A `mod` whose name the macro computes (`mod $n;`): no token of the call is `mod`, so the walk
/// plans nothing; the crate's names read `src/a.rs`, outline it on their worker and it is
/// planned after them (D161, engine_check.rs `serve_found`): checked as a plain `mod a;` is.
/// The control (the serve planted away, the read stops the check by name) and the rest of the
/// found files' cases: engine_found_files_tests.rs.
#[test]
fn a_module_a_macro_names_from_its_input_is_served() {
    let lib = "macro_rules! m {\n    ($n:ident) => { mod $n; };\n}\nm!(a);\npub fn top() -> u8 { a::f() }\n";
    good_and_bad(15, lib, "src/a.rs", &[]);
}
