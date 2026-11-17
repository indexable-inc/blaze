//! D112 interim, brief N7 (main's ruling): a NATIVE build script that runs
//! `sw_vers -productVersion` (libusb1-sys 0.7.0's build.rs:27) gets the pinned version from the
//! fact shim, and the run's read list holds that question; with no pins the shim exits 127 and
//! names the question, never reaching the machine's `/usr/bin/sw_vers`.
//!
//! This test binary (harness = false) plays three parts by how it is started, as
//! native_sandbox's hostile child does: run as `sw_vers` (a link in the run's `bin/`) it IS the
//! fact shim (`native_facts::shim_main`); run with `CHILD` set it is the build script, which
//! spawns `sw_vers -productVersion` found by `PATH` and writes what it got to `OUT_DIR/answer`;
//! run plainly it is the test, which runs itself confined through `native_script::run_script`.
//! Prints "native fact shim: sw_vers -productVersion answered 27.0.1 from the pins, read
//! listed; without pins exit 127, refused by name".

use std::time::Duration;

use blaze_build_script::{
    Env,
    fact_shim::{PROGRAMS, program_name},
    key::RunLimits,
    toolchain::{CInputs, Shelf},
};
use blaze_own::{AString, Heap, String, Vec};
use blaze_proc_macro::{
    native_facts::{self, ShimRun},
    native_script::{NativeScript, run_script},
    spawn_route::FactPins,
};

const CHILD: &str = "BLAZE_N7_CHILD";

fn main() {
    let argv0 = std::env::args().next().unwrap_or_default();
    if PROGRAMS.contains(&program_name(&argv0)) {
        std::process::exit(native_facts::shim_main(Heap::process()));
    }
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    the_native_script_gets_the_pinned_version();
}

/// The build script: libusb1-sys's `Command::new("sw_vers").args(["-productVersion"])`.
fn child() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let said = match std::process::Command::new("sw_vers").args(["-productVersion"]).output() {
        Ok(out) => format!(
            "code={:?}\nstdout={}\nstderr={}",
            out.status.code(),
            std::string::String::from_utf8_lossy(&out.stdout).trim_end(),
            std::string::String::from_utf8_lossy(&out.stderr).trim_end()
        ),
        Err(e) => format!("spawn failed: {e}"),
    };
    std::fs::write(format!("{out_dir}/answer"), said).expect("OUT_DIR/answer");
}

struct Run {
    answer: std::string::String,
    reads: std::vec::Vec<(std::string::String, bool)>,
}

fn run_once(name: &str, pins: Option<&FactPins>) -> Run {
    let alloc = blaze_own::testing::heap().alloc();
    let root = std::env::temp_dir().join(format!("blaze-n7-{}-{name}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    for dir in ["package", "out", "shelf", "sdk", "work"] {
        std::fs::create_dir_all(root.join(dir)).expect("a test folder");
    }
    let root = root.canonicalize().expect("the folder's real path");
    let text = |dir: &str| root.join(dir).to_str().expect("a UTF-8 temp path").to_owned();
    let (package, out, shelf, sdk, work) = (text("package"), text("out"), text("shelf"), text("sdk"), text("work"));
    let exe = std::env::current_exe().expect("the test binary").canonicalize().expect("its real path");
    let exe = exe.to_str().expect("a UTF-8 path").to_owned();

    // The command's edge, then the run: the shim (this binary) placed, a facts folder made.
    let shim = native_facts::place_shim(&work, &exe).unwrap_or_else(|e| panic!("{e}"));
    let facts: ShimRun = native_facts::prepare(&shim, &work, "k", pins, alloc).unwrap_or_else(|e| panic!("{e}"));

    let mut vars = Vec::new_in(alloc);
    let pair = |n: &str, v: &str| {
        let pair: (AString<'_>, AString<'_>) = (String::from_str_in(n, alloc), String::from_str_in(v, alloc));
        pair
    };
    vars.push(pair("CARGO_MANIFEST_DIR", "/package"));
    vars.push(pair("OUT_DIR", "/target/debug/build/n7-0/out"));
    vars.push(pair(CHILD, "1"));
    let env = Env { vars };
    let c = CInputs {
        shelf: Shelf { dir: &shelf, clang_major: 23 },
        sysroot: &sdk,
        out_dir: &out,
        package: &package,
        seen: blaze_expand::seen::SeenRoot::Package,
        macos_min: "11.0",
    };
    let script = NativeScript {
        program: &exe,
        c,
        env: &env,
        reads: &[],
        shown: &[],
        host_tools: &[],
        facts: Some(&facts),
        limits: RunLimits::STANDARD,
        deadline: Duration::from_secs(60),
    };
    let ran = run_script(&script, alloc).unwrap_or_else(|e| panic!("{e}"));
    let answer = ran.outputs.file("answer").map(|bytes| std::string::String::from_utf8_lossy(bytes).into_owned()).expect("the script's OUT_DIR/answer");
    let asked = native_facts::reads(&facts, pins, alloc).unwrap_or_else(|e| panic!("{e}"));
    let reads = asked.reads.iter().map(|r| (r.subject.clone(), r.digest.is_some())).collect();
    _ = std::fs::remove_dir_all(&root);
    Run { answer, reads }
}

fn the_native_script_gets_the_pinned_version() {
    let pins = FactPins {
        clang_version_text: "clang version 23.1.4\nTarget: arm64-apple-darwin\n".to_owned(),
        macos_product_version: "27.0.1".to_owned(),
        macos_build_version: "26A434".to_owned(),
    };
    let pinned = run_once("pinned", Some(&pins));
    assert!(pinned.answer.starts_with("code=Some(0)\nstdout=27.0.1\n"), "{}", pinned.answer);
    assert_eq!(pinned.reads, [("sw_vers\n-productVersion".to_owned(), true)]);

    // Control: the pin gone: exit 127, the question named, no answer; the read still listed.
    let unpinned = run_once("unpinned", None);
    assert!(unpinned.answer.starts_with("code=Some(127)\nstdout=\n"), "{}", unpinned.answer);
    assert!(unpinned.answer.contains("`sw_vers -productVersion` refused"), "{}", unpinned.answer);
    assert_eq!(unpinned.reads, [("sw_vers\n-productVersion".to_owned(), false)]);
    println!("native fact shim: sw_vers -productVersion answered 27.0.1 from the pins, read listed; without pins exit 127, refused by name");
}
