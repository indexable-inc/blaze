//! Build scripts on the stand-in toolchain: the pure ones with the effects and variables their
//! proofs must have, the impure ones with the reason said, and the identity of the instance
//! graph (design sections 1, 3, 4).

use base_db::Db;
use blaze_ids::ContentId;
use hir_def::FunctionId;

use super::fixture::{crate_named, db_with};
use crate::{Effects, Proof, Purity, main_of, script_purity};

/// The `main` of the build script crate `build_script` of a fixture of the toolchain and
/// `crates` (`//- /build.rs crate:build_script deps:std` first, then its crates).
#[track_caller]
fn script(crates: &str) -> (Db, FunctionId) {
    let mut db = db_with(crates);
    blaze_item_hash::identity::set_build(&mut db, ContentId([7; 32]));
    let krate = crate_named(&db, "build_script");
    let main = main_of(&db, krate).expect("the script has a `main`");
    (db, main)
}

/// What the walk finds of the script's `main`.
#[track_caller]
fn purity_of(crates: &str) -> Purity {
    let (db, main) = script(crates);
    hir_ty::attach_db(&db, || script_purity(&db, main).clone())
}

/// The proof of the script in `crates`, which must be pure with `effects` and read exactly the
/// variables `env` (sorted).
#[track_caller]
fn pure(crates: &str, effects: Effects, env: &[&str]) -> Proof {
    let proof = match purity_of(crates) {
        Purity::Pure(proof) => proof,
        Purity::Impure(why) => panic!("the script should be pure, but {}", why.said()),
    };
    let names: Vec<&str> = proof.env.iter().map(|it| it.as_str()).collect();
    assert_eq!(names, env, "the variables the proof says the script can read");
    assert_eq!(proof.effects, effects, "the effects of the proof");
    assert!(proof.instances > 0, "the walk reached `main` at least");
    proof
}

/// The script in `crates` must be impure, and what is said of it must contain `says`.
#[track_caller]
fn impure(crates: &str, says: &str) {
    match purity_of(crates) {
        Purity::Impure(why) => {
            let said = why.said();
            assert!(said.contains(says), "`{said}` should say `{says}`");
        }
        Purity::Pure(proof) => panic!("the script should be impure ({says}), but is pure: {proof:?}"),
    }
}

fn identity(crates: &str) -> ContentId {
    match purity_of(crates) {
        Purity::Pure(proof) => proof.identity,
        Purity::Impure(why) => panic!("the script should be pure, but {}", why.said()),
    }
}

#[test]
fn printing_constants_is_pure() {
    pure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-cfg=has_x");
    eprintln!("a warning to the log");
}
"#,
        Effects::PRINT,
        &[],
    );
}

#[test]
fn writing_out_dir_is_pure() {
    pure(
        r#"
//- /build.rs crate:build_script deps:std
use std::{env, fs, path::Path};

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    fs::write(Path::new(&out).join("x.rs"), "pub const X: u32 = 1;").unwrap();
}
"#,
        Effects::WRITE,
        &["OUT_DIR"],
    );
}

#[test]
fn printing_a_cfg_of_target_is_pure() {
    pure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    let target = std::env::var("TARGET").unwrap();
    println!("cargo:rustc-cfg=target_{}", target);
}
"#,
        Effects::PRINT,
        &["TARGET"],
    );
}

#[test]
fn reading_the_manifest_is_pure() {
    pure(
        r#"
//- /build.rs crate:build_script deps:std
use std::{env, fs, path::Path};

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let manifest = fs::read_to_string(Path::new(&dir).join("Cargo.toml")).unwrap();
    let again = fs::read("Cargo.toml").unwrap();
    let _ = again;
    println!("cargo:rerun-if-changed={}", manifest);
}
"#,
        Effects::PRINT.union(Effects::READ),
        &["CARGO_MANIFEST_DIR"],
    );
}

#[test]
fn a_helper_given_the_path_is_pure() {
    pure(
        r#"
//- /build.rs crate:build_script deps:std
use std::{env, fs, path::Path};

fn emit(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
}

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    let out = Path::new(&out);
    emit(&out.join("a.rs"), "pub const A: u8 = 0;");
    emit(&out.join("b.rs"), "pub const B: u8 = 1;");
}
"#,
        Effects::WRITE,
        &["OUT_DIR"],
    );
}

#[test]
fn a_helper_joining_its_directory_is_pure() {
    pure(
        r#"
//- /build.rs crate:build_script deps:std
use std::{env, fs, path::Path};

fn emit(dir: &Path) {
    let mut path = dir.to_path_buf();
    path.push("gen");
    fs::write(dir.join("x.rs"), "pub const X: u8 = 0;").unwrap();
    fs::create_dir_all(path).unwrap();
}

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    emit(Path::new(&out));
}
"#,
        Effects::WRITE,
        &["OUT_DIR"],
    );
    impure(
        r#"
//- /build.rs crate:build_script deps:std
use std::{env, fs, path::Path};

fn emit(dir: &Path) {
    fs::write(dir.join("x.rs"), "pub const X: u8 = 0;").unwrap();
}

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    emit(Path::new(&dir));
}
"#,
        "writes a path in its package, not inside its OUT_DIR",
    );
}

#[test]
fn a_closure_given_the_path_is_pure() {
    pure(
        r#"
//- /build.rs crate:build_script deps:std
use std::{env, fs, path::Path};

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    let emit = |path: &Path| fs::write(path, "pub const X: u8 = 0;").unwrap();
    emit(&Path::new(&out).join("x.rs"));
}
"#,
        Effects::WRITE,
        &["OUT_DIR"],
    );
}

#[test]
fn dyn_write_and_a_function_pointer_are_pure() {
    pure(
        r#"
//- /build.rs crate:build_script deps:std
use std::io::Write;

fn say(out: &mut dyn Write, text: &str) {
    out.write_all(text.as_bytes()).unwrap();
}

fn line(text: &str) {
    let mut stdout = std::io::stdout();
    say(&mut stdout, text);
}

fn main() {
    let f: fn(&str) = line;
    f("cargo:rustc-cfg=has_x\n");
}
"#,
        Effects::PRINT,
        &[],
    );
}

#[test]
fn printing_an_io_error_is_pure() {
    // `io::Error`'s `Debug` reaches `strerror_r`, whose text is the C locale's: fixed.
    pure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    if let Err(error) = std::fs::read("Cargo.toml") {
        eprintln!("cargo:warning={:?}", error);
    }
}
"#,
        Effects::PRINT.union(Effects::READ),
        &[],
    );
}

#[test]
fn reading_path_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    let path = std::env::var("PATH").unwrap();
    println!("{}", path);
}
"#,
        "reads the variable `PATH`",
    );
}

#[test]
fn reading_rustc_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    let rustc = std::env::var_os("RUSTC");
    let _ = rustc;
}
"#,
        "reads the variable `RUSTC`",
    );
}

#[test]
fn a_variable_named_by_a_value_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    let name = std::env::var("CARGO_PKG_NAME").unwrap();
    let value = std::env::var(name).unwrap();
    println!("{}", value);
}
"#,
        "reads a variable named by a value not known",
    );
}

#[test]
fn writing_the_package_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    std::fs::write("src/gen.rs", "pub const X: u8 = 0;").unwrap();
}
"#,
        "writes `src/gen.rs`, a path in its package, not inside its OUT_DIR",
    );
}

#[test]
fn reading_outside_the_package_is_not_pure() {
    for (path, says) in [
        ("../x", "reads `../x`, not a path inside its package"),
        ("/etc/x", "reads `/etc/x`, not a path inside its package"),
    ] {
        impure(
            &format!(
                r#"
//- /build.rs crate:build_script deps:std
fn main() {{
    let text = std::fs::read_to_string("{path}").unwrap();
    println!("{{}}", text);
}}
"#
            ),
            says,
        );
    }
}

#[test]
fn a_spawn_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    let output = std::process::Command::new("cc").output().unwrap();
    let _ = output;
}
"#,
        "the foreign function `posix_spawnp`",
    );
}

#[test]
fn a_foreign_call_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
unsafe extern "C" {
    fn getpid() -> i32;
}

fn main() {
    let pid = unsafe { getpid() };
    let _ = pid;
}
"#,
        "the foreign function `getpid`",
    );
}

#[test]
fn inline_assembly_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    unsafe { core::arch::asm!("nop") }
}
"#,
        "inline assembly",
    );
}

#[test]
fn a_static_mut_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
static mut RUNS: u32 = 0;

fn main() {
    unsafe { RUNS = 1 };
    println!("cargo:rustc-cfg=ran");
}
"#,
        "a `static mut`",
    );
}

#[test]
fn a_thread_local_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
#![feature(thread_local)]

#[thread_local]
static DEPTH: u32 = 0;

fn main() {
    if DEPTH == 0 {
        println!("cargo:rustc-cfg=top");
    }
}
"#,
        "a thread-local",
    );
}

#[test]
fn a_function_pointer_made_from_a_number_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
fn main() {
    let f: fn() = unsafe { core::mem::transmute(0x1000usize as *const ()) };
    f();
}
"#,
        "an address made from a number",
    );
}

#[test]
fn a_vtable_of_a_foreign_static_is_not_pure() {
    impure(
        r#"
//- /build.rs crate:build_script deps:std
trait Hook {
    fn run(&self);
}

unsafe extern "C" {
    static HOOK: &'static dyn Hook;
}

fn main() {
    unsafe { HOOK.run() }
}
"#,
        "a foreign static",
    );
}

const HELPER_SCRIPT: &str = r#"
//- /build.rs crate:build_script deps:std
use std::{env, fs, path::Path};

fn emit(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
}

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    emit(&Path::new(&out).join("x.rs"), "pub const X: u8 = 0;");
    println!("cargo:rerun-if-changed=build.rs");
}
"#;

/// Asserts the scripts `a` and `b` (`how` says what differs) reach the same instances with the
/// same part of the identity each, and that their `main`s' bodies have the same identity,
/// naming the first that differs.
#[track_caller]
fn same_parts(a: &str, b: &str, how: &str) {
    let parts = |crates: &str| {
        let (db, main) = script(crates);
        hir_ty::attach_db(&db, || {
            let main_body = blaze_item_hash::identity::identity_parts(&db, main.into());
            (main_body, crate::walk::identity_parts(&db, main))
        })
    };
    let ((main_a, parts_a), (main_b, parts_b)) = (parts(a), parts(b));
    match (main_a, main_b) {
        (Ok(main_a), Ok(main_b)) => {
            for ((name, a), (_, b)) in main_a.iter().zip(&main_b) {
                assert_eq!(a, b, "{how}: the {name} part of the identity of `main`'s body (blaze_item_hash)");
            }
        }
        (main_a, main_b) => {
            assert_eq!(main_a, main_b, "{how}: the identity of `main`'s body (blaze_item_hash)")
        }
    }
    let names = |parts: &[(String, Result<ContentId, String>)]| parts.iter().map(|it| it.0.clone()).collect::<Vec<_>>();
    assert_eq!(names(&parts_a), names(&parts_b), "{how}: the instances the walk reaches");
    for ((name, a), (_, b)) in parts_a.iter().zip(&parts_b) {
        assert_eq!(a, b, "{how}: the part of the identity of {name}");
    }
}

#[test]
fn a_moved_or_reformatted_script_has_the_same_identity() {
    let first = identity(HELPER_SCRIPT);
    let moved_text = HELPER_SCRIPT.replace("//- /build.rs ", "//- /tools/gen/build.rs ");
    same_parts(HELPER_SCRIPT, &moved_text, "moved");
    let moved = identity(&moved_text);
    assert_eq!(first, moved, "the script's file is not in its identity");
    let reformatted_text = r#"
//- /build.rs crate:build_script deps:std
use std::{env, fs, path::Path};

// The entry point.
fn main()
{
    let out = env::var( "OUT_DIR" ).unwrap();

    emit( &Path::new( &out ).join( "x.rs" ), "pub const X: u8 = 0;" );
    println!( "cargo:rerun-if-changed=build.rs" );
}

/// Writes `text` at `path`.
fn emit(path: &Path, text: &str)
{
    fs::write(path, text).unwrap();
}
"#;
    same_parts(HELPER_SCRIPT, reformatted_text, "reformatted");
    let reformatted = identity(reformatted_text);
    assert_eq!(first, reformatted, "whitespace, comments and item order are not in the identity");
}

const DEP_SCRIPT: &str = r#"
//- /build.rs crate:build_script deps:std,helper
fn main() {
    println!("cargo:rustc-cfg={}", helper::name());
}
//- /helper.rs crate:helper deps:std
pub fn name() -> &'static str {
    "first"
}
"#;

#[test]
fn a_changed_callee_in_a_dependency_changes_the_identity() {
    let first = identity(DEP_SCRIPT);
    assert_eq!(first, identity(DEP_SCRIPT), "the identity is deterministic");
    let changed = identity(&DEP_SCRIPT.replace("\"first\"", "\"second\""));
    assert_ne!(first, changed, "a reached body of a dependency is in the identity");
}

const IMPL_SCRIPT: &str = r#"
//- /build.rs crate:build_script deps:std,helper
use helper::Say;

fn main() {
    helper::Chosen::new().say();
}
//- /helper.rs crate:helper deps:std
pub trait Say {
    fn say(&self);
}

pub struct A;
pub struct B;

impl A {
    pub fn new() -> A {
        A
    }
}

impl B {
    pub fn new() -> B {
        B
    }
}

impl Say for A {
    fn say(&self) {
        println!("cargo:rustc-cfg=said");
    }
}

impl Say for B {
    fn say(&self) {
        println!("cargo:rustc-cfg=said");
    }
}

pub type Chosen = A;
"#;

#[test]
fn a_call_resolved_to_another_impl_changes_the_identity() {
    let first = identity(IMPL_SCRIPT);
    let other = identity(&IMPL_SCRIPT.replace("pub type Chosen = A;", "pub type Chosen = B;"));
    assert_ne!(first, other, "which impl a call resolves to is in the identity");
}
