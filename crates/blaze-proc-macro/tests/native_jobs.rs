//! A NATIVE build script is told the command's parallelism and shares it through the command's
//! jobserver: `NUM_JOBS` the workers and `CARGO_MAKEFLAGS` exactly as cargo writes it
//! (`-j --jobserver-fds=fifo:P --jobserver-auth=fifo:P`), and inside its sandbox it can open the
//! fifo as the `jobserver` crate does (read and write) and take a token. It exits keeping that
//! token: the command gets it back by count when the run ends (jobserver.rs).
//!
//! This test binary (harness = false) plays two parts by how it is started, as native_fact_shim
//! does: run with `CHILD` set it is the build script, which reads its environment, takes one
//! token and writes what it saw to `OUT_DIR/jobs`; run plainly it is the test, which runs itself
//! confined through `native_script::run_script` with a jobserver of 6 workers.
//! Control: the same run with no jobserver is told `NUM_JOBS=1` and no `CARGO_MAKEFLAGS`.
//! Prints "native jobs: NUM_JOBS=6, CARGO_MAKEFLAGS=-j --jobserver-fds=fifo:<fifo>
//! --jobserver-auth=fifo:<fifo>, a token taken in the sandbox and returned by count; without a
//! jobserver NUM_JOBS=1".

use std::{
    io::{ErrorKind, Read},
    os::unix::fs::OpenOptionsExt,
    time::Duration,
};

use blaze_build_script::{
    Env,
    key::RunLimits,
    toolchain::{CInputs, Shelf},
};
use blaze_own::{AString, String, Vec};
use blaze_proc_macro::{
    jobserver::{Jobserver, JobserverOpts},
    native_script::{NativeScript, run_script},
};

const CHILD: &str = "BLAZE_JOBS_CHILD";

fn main() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    a_native_script_shares_the_commands_workers();
}

/// The build script: what `cc`'s `parallel` reads, and one token taken as the `jobserver` crate
/// takes it (`fifo:` opened read and write, read without blocking).
fn child() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let jobs = std::env::var("NUM_JOBS").unwrap_or_else(|_| "unset".to_owned());
    let flags = std::env::var("CARGO_MAKEFLAGS").unwrap_or_else(|_| "unset".to_owned());
    let token = match flags.rsplit_once("--jobserver-auth=fifo:") {
        None => "no jobserver".to_owned(),
        Some((_, path)) => match std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path) {
            Err(error) => format!("open refused: {error}"),
            Ok(mut fifo) => {
                let mut byte = [0u8; 1];
                match fifo.read(&mut byte) {
                    Ok(1) => format!("took {}", byte[0] as char),
                    Ok(_) => "empty".to_owned(),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => "no token".to_owned(),
                    Err(error) => format!("read refused: {error}"),
                }
                // Exits keeping it, as a killed child would.
            }
        },
    };
    let said = format!("NUM_JOBS={jobs}\nCARGO_MAKEFLAGS={flags}\ntoken={token}");
    std::fs::write(format!("{out_dir}/jobs"), said).expect("OUT_DIR/jobs");
}

fn run_once(name: &str, workers: Option<u32>) -> (std::string::String, Option<(std::string::String, u64)>) {
    let alloc = blaze_own::testing::heap().alloc();
    let root = std::env::temp_dir().join(format!("blaze-jobs-{}-{name}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    for dir in ["package", "out", "shelf", "sdk", "work", "files"] {
        std::fs::create_dir_all(root.join(dir)).expect("a test folder");
    }
    let root = root.canonicalize().expect("the folder's real path");
    let text = |dir: &str| root.join(dir).to_str().expect("a UTF-8 temp path").to_owned();
    let (package, out, shelf, sdk, work, files) = (text("package"), text("out"), text("shelf"), text("sdk"), text("work"), text("files"));
    let exe = std::env::current_exe().expect("the test binary").canonicalize().expect("its real path");
    let exe = exe.to_str().expect("a UTF-8 path").to_owned();
    let server = workers.map(|n| {
        let workers = core::num::NonZeroU32::new(n).expect("a worker count");
        Jobserver::new(JobserverOpts { work: &work, workers, id: 1 }).unwrap_or_else(|e| panic!("{e}"))
    });

    let mut vars = Vec::new_in(alloc);
    let pair = |n: &str, v: &str| {
        let pair: (AString<'_>, AString<'_>) = (String::from_str_in(n, alloc), String::from_str_in(v, alloc));
        pair
    };
    vars.push(pair(CHILD, "1"));
    vars.push(pair("CARGO_MANIFEST_DIR", "/package"));
    vars.push(pair("OUT_DIR", "/target/debug/build/jobs-0/out"));
    let env = Env { vars };
    // The keyed environment holds no job count: a key made of it is one for every count.
    assert_eq!(env.get("NUM_JOBS"), None);
    let c = CInputs {
        shelf: Shelf { dir: &shelf, clang_major: "23" },
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
        facts: None,
        jobs: server.as_ref(),
        limits: RunLimits::STANDARD,
        deadline: Duration::from_secs(60),
        files: &files,
    };
    let ran = run_script(&script, alloc).unwrap_or_else(|e| panic!("{e}"));
    // The file the script left is a stored file of the record: its bytes are in the file area
    // under its digest's lowercase hex.
    let stored = ran.outputs.stored_file("jobs").expect("the script's OUT_DIR/jobs");
    let name: std::string::String = stored.sha256.iter().map(|byte| format!("{byte:02x}")).collect();
    let said = std::string::String::from_utf8_lossy(&std::fs::read(format!("{files}/{name}")).expect("the stored file in the area")).into_owned();
    let served = server.map(|server| {
        let fifo = server.fifo().to_owned();
        let restored = server.restored().unwrap_or_else(|e| panic!("{e}"));
        server.end().unwrap_or_else(|e| panic!("{e}"));
        assert!(std::fs::metadata(&fifo).is_err(), "the fifo is removed at the end");
        (fifo, restored)
    });
    _ = std::fs::remove_dir_all(&root);
    (said, served)
}

fn a_native_script_shares_the_commands_workers() {
    let (said, served) = run_once("shared", Some(6));
    let (fifo, restored) = served.expect("a jobserver");
    let flags = format!("-j --jobserver-fds=fifo:{fifo} --jobserver-auth=fifo:{fifo}");
    assert_eq!(said, format!("NUM_JOBS=6\nCARGO_MAKEFLAGS={flags}\ntoken=took +"), "what the native script was told and could do");
    assert_eq!(restored, 1, "the token the script exited holding came back by count");

    // Control: no jobserver: one job, nothing to share.
    let (alone, none) = run_once("alone", None);
    assert!(none.is_none());
    assert_eq!(alone, "NUM_JOBS=1\nCARGO_MAKEFLAGS=unset\ntoken=no jobserver");
    println!("native jobs: NUM_JOBS=6, CARGO_MAKEFLAGS={flags}, a token taken in the sandbox and returned by count; without a jobserver NUM_JOBS=1");
}
