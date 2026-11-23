//! D112, brief N8 (main's ruling of 2026-11-16): a NATIVE build script whose `links`
//! dependency ran as wasm finds that dependency's header through `DEP_LZ4_INCLUDE`, as
//! librocksdb-sys's script hands lz4-sys's `include` to `cc` (`-I$DEP_LZ4_INCLUDE`, then
//! `#include "lz4.h"`).
//!
//! The producer is lz4-sys as its wasm run's record holds it: `OUT_DIR`
//! `/target/debug/build/lz4-sys-0123456789abcdef/out` with `include/lz4.h`, metadata
//! `include=<OUT_DIR>/include` and `root=/package/inc` (its served tree, holding `inc/b.h`).
//! `dep_mount::native::lay` lays both trees under the run's own folder; the run is confined
//! (`native_script::run_script`, Seatbelt on the Mac) with the laid folders granted read-only.
//!
//! This test binary (harness = false) is two things by how it is started: with `CHILD` set it
//! is the consuming build script, which does `cc`'s include search for `lz4.h` in
//! `DEP_LZ4_INCLUDE` and `b.h` in `DEP_LZ4_ROOT` (clang's own line when it is missing:
//! "fatal error: 'lz4.h' file not found") and forwards `include` as aws-lc-rs forwards
//! aws-lc-sys's; run plainly it is the test.
//!
//! CONTROL (the cause removed): the record's virtual value passed through unresolved, as
//! before N8: the script fails with "'lz4.h' file not found".
//!
//! Prints "native dep tree: lz4.h and b.h read from the laid trees, the record keeps
//! /target/debug/build/lz4-sys-0123456789abcdef/out/include; the virtual path passed through:
//! 'lz4.h' file not found".

use std::{sync::Arc, time::Duration};

use blaze_build_script::{
    Env,
    env::DepMetadata,
    key::RunLimits,
    toolchain::{CInputs, Shelf},
};
use blaze_own::{AString, String, Vec};
use blaze_proc_macro::{
    native_script::{NativeScript, NativeScriptError, run_script},
    script_sandbox::{
        ScriptRoot, Tree,
        dep_mount::{DepProducer, native::lay},
    },
};

const CHILD: &str = "BLAZE_N8_CHILD";
const LZ4_OUT: &str = "/target/debug/build/lz4-sys-0123456789abcdef/out";
const LZ4_INCLUDE: &str = "/target/debug/build/lz4-sys-0123456789abcdef/out/include";
const HEADER: &[u8] = b"#define LZ4_VERSION_MAJOR 1\n";

fn main() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    // A harness-less test's panic is not a libtest block: it is said on one `error:` line, so a
    // runner that keeps error lines shows why.
    std::panic::set_hook(std::boxed::Box::new(|info| eprintln!("error: native_dep_tree: {}", info.to_string().replace('\n', " | "))));
    a_native_script_reads_a_wasm_producers_header();
}

/// The consuming build script: `cc`'s include search, then the forwarded metadata.
fn child() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let search = |var: &str, name: &str| -> std::vec::Vec<u8> {
        let dir = std::env::var(var).unwrap_or_default();
        match std::fs::read(format!("{dir}/{name}")) {
            Ok(bytes) => bytes,
            Err(_) => {
                eprintln!("main.c:1:10: fatal error: '{name}' file not found");
                std::process::exit(1);
            }
        }
    };
    let header = search("DEP_LZ4_INCLUDE", "lz4.h");
    let other = search("DEP_LZ4_ROOT", "b.h");
    std::fs::write(format!("{out_dir}/lz4.h.seen"), header).expect("OUT_DIR/lz4.h.seen");
    std::fs::write(format!("{out_dir}/b.h.seen"), other).expect("OUT_DIR/b.h.seen");
    println!("cargo::metadata=include={}", std::env::var("DEP_LZ4_INCLUDE").unwrap_or_default());
}

/// A digest's lowercase hex: a stored file's name in the file area.
fn hex(digest: &[u8; 32]) -> std::string::String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn producer() -> DepProducer {
    DepProducer {
        tree: Tree::Mem { files: Arc::new(vec![("inc/b.h".to_owned(), b"int b;\n".to_vec())]), git: None },
        root: ScriptRoot::package(),
        out_dir: LZ4_OUT.to_owned(),
        out: Arc::new(vec![("include/lz4.h".to_owned(), HEADER.to_vec())]),
        stored: Arc::new(std::vec::Vec::new()),
        stored_files: Arc::new(std::vec::Vec::new()),
        upstream: Arc::new(std::vec::Vec::new()),
    }
}

/// One confined run: `resolved` lays the trees and gives the run's values; else the record's
/// values are passed as written (the control).
fn run_once(name: &str, resolved: bool) -> Result<std::vec::Vec<(std::string::String, std::string::String, std::vec::Vec<u8>)>, NativeScriptError> {
    let alloc = blaze_own::testing::heap().alloc();
    let root = std::env::temp_dir().join(format!("blaze-n8-{}-{name}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    for dir in ["package", "out", "shelf", "sdk", "work", "files"] {
        std::fs::create_dir_all(root.join(dir)).expect("a test folder");
    }
    let root = root.canonicalize().expect("the folder's real path");
    let text = |dir: &str| root.join(dir).to_str().expect("a UTF-8 temp path").to_owned();
    let (package, out, shelf, sdk, work, files) = (text("package"), text("out"), text("shelf"), text("sdk"), text("work"), text("files"));
    let exe = std::env::current_exe().expect("the test binary").canonicalize().expect("its real path");
    let exe = exe.to_str().expect("a UTF-8 path").to_owned();

    let lz4 = producer();
    let producers = [("lz4", &lz4)];
    let deps = [
        DepMetadata { links: "lz4", key: "INCLUDE", value: LZ4_INCLUDE },
        DepMetadata { links: "lz4", key: "ROOT", value: "/package/inc" },
    ];
    let (values, reads, shown): (std::vec::Vec<std::string::String>, std::vec::Vec<std::string::String>, std::vec::Vec<(std::string::String, std::string::String)>) = if resolved {
        let laid = lay(&work, "deps/k", &producers, &deps, alloc).unwrap_or_else(|e| panic!("{e}"));
        (
            laid.values.iter().map(|v| v.as_str().to_owned()).collect(),
            laid.folders.iter().map(|f| f.as_str().to_owned()).collect(),
            laid.shown.iter().map(|(r, s)| (r.as_str().to_owned(), s.as_str().to_owned())).collect(),
        )
    } else {
        (deps.iter().map(|d| d.value.to_owned()).collect(), std::vec::Vec::new(), std::vec::Vec::new())
    };
    if resolved {
        // Each value is a real folder under the run's own root, at the record's place inside.
        assert_eq!(values[0], format!("{work}/deps/k/lz4/out/include"));
        assert_eq!(values[1], format!("{work}/deps/k/lz4/package/inc"));
        assert_eq!(reads, [format!("{work}/deps/k/lz4/out"), format!("{work}/deps/k/lz4/package")]);
    }

    let mut vars = Vec::new_in(alloc);
    let pair = |n: &str, v: &str| {
        let pair: (AString<'_>, AString<'_>) = (String::from_str_in(n, alloc), String::from_str_in(v, alloc));
        pair
    };
    vars.push(pair("CARGO_MANIFEST_DIR", "/package"));
    vars.push(pair("OUT_DIR", "/target/debug/build/librocksdb-sys-0/out"));
    vars.push(pair("DEP_LZ4_INCLUDE", &values[0]));
    vars.push(pair("DEP_LZ4_ROOT", &values[1]));
    vars.push(pair(CHILD, "1"));
    // `Env` is sorted by name (its `get` searches it).
    vars.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let env = Env { vars };
    let c = CInputs {
        shelf: Shelf { dir: &shelf, clang_major: "23" },
        sysroot: &sdk,
        out_dir: &out,
        package: &package,
        seen: blaze_expand::seen::SeenRoot::Package,
        macos_min: "11.0",
    };
    let read_refs: std::vec::Vec<&str> = reads.iter().map(|r| r.as_str()).collect();
    let shown_refs: std::vec::Vec<(&str, &str)> = shown.iter().map(|(r, s)| (r.as_str(), s.as_str())).collect();
    let script = NativeScript {
        // A test's run: no record is kept, so any key names its trees.
        key: blaze_ids::ContentId([7; 32]),
        program: &exe,
        c,
        env: &env,
        reads: &read_refs,
        shown: &shown_refs,
        host_tools: &[],
        facts: None,
        jobs: None,
        confined: None,
        limits: RunLimits::STANDARD,
        deadline: Duration::from_secs(60),
        files: &files,
    };
    let ran = run_script(&script, alloc);
    // The file the script left is a stored file of the record: its bytes are in the file area
    // under its digest, read here (before the folder goes) as a reader of the record would.
    let file = match &ran {
        Ok(ran) => ran.outputs.stored_file("lz4.h.seen").map(|stored| std::fs::read(format!("{files}/{}", hex(&stored.sha256))).expect("the stored file in the area")).unwrap_or_default(),
        Err(_) => std::vec::Vec::new(),
    };
    _ = std::fs::remove_dir_all(&root);
    let ran = ran?;
    let mut seen = std::vec::Vec::new();
    for (key, value) in ran.outputs.metadata.iter() {
        seen.push((key.as_str().to_owned(), value.as_str().to_owned(), file.clone()));
    }
    Ok(seen)
}

fn a_native_script_reads_a_wasm_producers_header() {
    let seen = run_once("laid", true).unwrap_or_else(|e| panic!("{e}"));
    // The header's bytes reached the script, and its forwarded path is kept in the record's
    // form: no path of this run (rule 25).
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, "include");
    assert_eq!(seen[0].1, LZ4_INCLUDE, "the record holds the wasm sight's path, never the laid folder");
    assert_eq!(seen[0].2, HEADER);

    // Control: the virtual path given as written (before N8).
    let control = run_once("virtual", false);
    let Err(NativeScriptError::Failed { stderr, .. }) = control else { panic!("the virtual path must fail the script: {control:?}") };
    assert!(stderr.contains("'lz4.h' file not found"), "{stderr}");
    println!(
        "native dep tree: lz4.h and b.h read from the laid trees, the record keeps {LZ4_INCLUDE}; the virtual path passed through: 'lz4.h' file not found"
    );
}
