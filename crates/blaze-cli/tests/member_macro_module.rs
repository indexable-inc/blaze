//! A workspace member that is a proc-macro crate gets a module identity, keyed by its files'
//! content and never by where the checkout is (piece member-macro-module, 2026-11-16; rules 10,
//! 21, 25 and 65).
//!
//! The shape is pm-small's (the dependencies lane's two-member workspace): `mac` with
//! `[lib] proc-macro = true` and `#[proc_macro] pub fn same`, and `app` calling `mac::same!`.
//! Rust's rule: the Reference, "Procedural Macros": "Procedural macros must be defined in the
//! root of a crate with the crate type of `proc-macro`", and a function-like one is "invoked
//! ... like `mac::same!(..)`" from a crate that depends on it; Cargo's manifest says a path
//! dependency is a package of this checkout, so its sources are the files under its folder.
//!
//! What blaze does with it (crates/blaze-cli/src/macros.rs): the `MacroModule` task gives
//! `blaze_world::procs::identify` the member's folder from the layout's member directories, so
//! the identity holds `files <sha256>` (`blaze_proc_macro::module::files_digest`: each file's
//! path relative to the package and its bytes) and the module store is looked up under that
//! key. This test's home has an empty module store, so the lookup ends in the one named line
//! "mac 0.1.0: no compiled proc-macro module in .. (key <k>)": the module is built by the
//! bootstrap builder (D52) from the same files under the same key.
//!
//! Checked: (1) no "a path macro crate whose folder was not given" (before the fix every member
//! macro crate said it: macros.rs passed `|_| None` as the folder); (2) the same workspace at two
//! different paths, under two homes, prints the same key (no machine path in it, rule 25);
//! (3) one byte more in `mac/src/lib.rs` prints another key (the files are in it, rule 21).
//!
//! Expected (`--test member_macro_module`): "test blaze-cli: exit 0; 1 passed, 0 failed", the
//! test printing "keyed: <64 hex> then <64 hex>".
//! Control: with macros.rs's `list` passing `|_| None` again, it fails with "the folder of a
//! member macro crate was not given".

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long one `blaze check` of the two small members may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(300);

/// How long the child killed at [`DEADLINE`] may take to be reaped (the kill is SIGKILL).
const REAP: Duration = Duration::from_secs(10);

/// The words of the failure this piece removes (blaze-world procs.rs `PathWithoutFolder`).
const NOT_GIVEN: &str = "a path macro crate whose folder was not given";

/// The start of the line a looked-up member module the store lacks prints (macros.rs).
const LACKS: &str = "mac 0.1.0: no compiled proc-macro module in";

const LIB: &str = "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn same(input: TokenStream) -> TokenStream {\n    input\n}\n";

/// pm-small at `dir`, its macro crate's root `lib`.
fn workspace(dir: &Path, lib: &str) {
    let files: [(&str, &str); 6] = [
        ("Cargo.toml", "[workspace]\nmembers = [\"mac\", \"app\"]\nresolver = \"2\"\n"),
        ("Cargo.lock", "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"mac\",\n]\n\n[[package]]\nname = \"mac\"\nversion = \"0.1.0\"\n"),
        ("mac/Cargo.toml", "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n"),
        ("mac/src/lib.rs", lib),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmac = { path = \"../mac\" }\n"),
        ("app/src/main.rs", "fn main() {\n    let x: u32 = mac::same!(1 + 2);\n    let _ = x;\n}\n"),
    ];
    for (path, text) in files {
        let at = dir.join(path);
        std::fs::create_dir_all(at.parent().expect("a file's folder")).unwrap_or_else(|error| panic!("making the folder of {}: {error}", at.display()));
        std::fs::write(&at, text).unwrap_or_else(|error| panic!("writing {}: {error}", at.display()));
    }
}

/// `blaze check` in `dir` with the home `home`: its standard output and error, together. Both
/// go to files (no pipe can fill and stall the child); the child is waited for in a loop
/// bounded by [`DEADLINE`], killed and reaped within [`REAP`] at it.
fn check(dir: &Path, home: &Path) -> String {
    let said = dir.with_extension("said");
    let file = File::create(&said).unwrap_or_else(|error| panic!("making {}: {error}", said.display()));
    let file_err = file.try_clone().expect("the output file, twice");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("check")
        .current_dir(dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze check` of pm-small's shape, at most DEADLINE.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > DEADLINE => {
                let _ = child.kill();
                let reaped = Instant::now();
                // Waiting for: the killed child's exit, at most REAP.
                while child.try_wait().ok().flatten().is_none() && reaped.elapsed() < REAP {
                    std::thread::sleep(Duration::from_millis(5));
                }
                panic!("`blaze check` in {} did not end within {DEADLINE:?} (rule 18)", dir.display());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze check` in {}: {error}", dir.display());
            }
        }
    }
    std::fs::read_to_string(&said).unwrap_or_else(|error| panic!("reading {}: {error}", said.display()))
}

/// The module key the member's lookup printed: the 64 hex digits after "(key " on the
/// [`LACKS`] line.
fn key(said: &str, at: &Path) -> String {
    assert!(!said.contains(NOT_GIVEN), "the folder of a member macro crate was not given ({}):\n{said}", at.display());
    let line = said.lines().find(|line| line.contains(LACKS)).unwrap_or_else(|| panic!("no line says the member's module was looked up ({}):\n{said}", at.display()));
    let (_, after) = line.split_once("(key ").unwrap_or_else(|| panic!("the lookup's line names no key: {line}"));
    let key: String = after.chars().take_while(char::is_ascii_hexdigit).collect();
    assert_eq!(key.len(), 64, "a module key is a SHA-256 in hex: {line}");
    assert!(line.contains("\"checkout\""), "the bootstrap's input names the checkout the path crate is in: {line}");
    key
}

#[test]
fn a_member_macro_crate_is_looked_up_by_its_files() {
    let a = fresh_home("member_macro_module_a", true);
    let b = fresh_home("member_macro_module_b", true);
    let c = fresh_home("member_macro_module_c", true);
    let (at_a, at_b, at_c) = (a.join("pm"), b.join("deeper/checkout/pm"), c.join("pm"));
    workspace(&at_a, LIB);
    workspace(&at_b, LIB);
    workspace(&at_c, &format!("{LIB}\n"));
    let key_a = key(&check(&at_a, &a.join("home")), &at_a);
    let key_b = key(&check(&at_b, &b.join("home")), &at_b);
    let key_c = key(&check(&at_c, &c.join("home")), &at_c);
    assert_eq!(key_a, key_b, "one workspace at two paths: one module key (rule 25)");
    assert_ne!(key_a, key_c, "a byte more in mac/src/lib.rs: another module key (rule 21)");
    println!("keyed: {key_a} then {key_c}");
}

/// The lookup's own key holds the member's files (member-macro-module-2, rule 21): a module
/// found for `mac`'s files is not used once one byte of `mac/src/lib.rs` changes.
///
/// One checkout, one home. (1) `blaze check`: the store is empty, the lookup names key `k1`.
/// (2) A module record for `k1` is put in the store (`wasm-sha256`, all `find_module` reads):
/// the store's listing moved, the lookup runs again, finds it, and no line names `mac`'s
/// module. (3) One byte more in `mac/src/lib.rs`: the lookup must run again and name a new key
/// `k2`, which the store lacks. Before -2 (the lookup's key was the graph's source and the
/// subject, which hold `mac`'s manifest and not its files) step (3) printed nothing: the found
/// record of step (2) stood, naming the module of the old files.
///
/// Prints `edited: <k1> then <k2>`. Control: with -1's macros.rs/units.rs (no `MacroFiles`
/// row), it fails with "an edit of mac/src/lib.rs left the found module of the old files standing".
#[test]
fn an_edit_of_a_member_macro_crate_moves_its_module_lookup() {
    let base = fresh_home("member_macro_module_edit", true);
    let (home, at) = (base.join("home"), base.join("pm"));
    workspace(&at, LIB);
    let k1 = key(&check(&at, &home), &at);
    let module = home.join(".blaze/v2/proc-macro/modules").join(&k1);
    std::fs::create_dir_all(&module).unwrap_or_else(|error| panic!("making {}: {error}", module.display()));
    std::fs::write(module.join("wasm-sha256"), format!("{}\n", "0".repeat(64))).expect("the planted module's digest");
    let found = check(&at, &home);
    assert!(!found.contains(LACKS) && !found.contains(NOT_GIVEN), "the module put in the store was not found:\n{found}");
    std::fs::write(at.join("mac/src/lib.rs"), format!("{LIB}\n")).expect("the edit of mac/src/lib.rs");
    let edited = check(&at, &home);
    assert!(edited.contains(LACKS), "an edit of mac/src/lib.rs left the found module of the old files standing:\n{edited}");
    let k2 = key(&edited, &at);
    assert_ne!(k1, k2, "the edited files' module key is another");
    println!("edited: {k1} then {k2}");
}
