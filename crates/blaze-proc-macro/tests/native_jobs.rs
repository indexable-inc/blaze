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
    env, fs,
    io::{ErrorKind, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use blaze_build_script::{
    Env,
    key::RunLimits,
    toolchain::{CInputs, Shelf},
};
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_proc_macro::{
    jobserver::{Jobserver, JobserverOpts},
    native_facts::Asked,
    native_script::{NativeScript, run_script},
    native_unit::{CertifyInput, certify_member_reads},
    script_sandbox::{RealRoot, Tree},
};

const CHILD: &str = "BLAZE_JOBS_CHILD";
const CERT_MODE: &str = "BLAZE_CERT_MODE";

fn main() {
    if let Ok(mode) = env::var(CERT_MODE) {
        cert_child(&mode);
        return;
    }
    if env::var_os(CHILD).is_some() {
        child();
        return;
    }
    a_native_script_shares_the_commands_workers();
    confined_certification_proves_exact_reads_and_catches_liars();
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
        // A test's run: no record is kept, so any key names its trees.
        key: blaze_ids::ContentId([7; 32]),
        program: &exe,
        c,
        env: &env,
        reads: &[],
        shown: &[],
        host_tools: &[],
        facts: None,
        jobs: server.as_ref(),
        confined: None,
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

fn cert_child(mode: &str) {
    let manifest_dir = match env::var("CARGO_MANIFEST_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => process::exit(2),
    };
    let out_dir = match env::var("OUT_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => process::exit(3),
    };
    match mode {
        "vergen" => {
            println!("cargo:rerun-if-changed=build.rs");
            println!("cargo:rustc-env=VERGEN_GIT_SHA=deadbeef");
        }
        "precompiles" => {
            println!("cargo:rerun-if-changed=src/pcrs.json");
            let data = match fs::read_to_string(manifest_dir.join("src/pcrs.json")) {
                Ok(s) => s,
                Err(_) => process::exit(10),
            };
            if fs::write(out_dir.join("pcrs.rs"), data.as_bytes()).is_err() {
                process::exit(11);
            }
            println!("cargo:rustc-env=PCRS_LEN={}", data.len());
        }
        "chainspec" => {
            println!("cargo::rerun-if-changed=build.rs");
            let Some(root) = manifest_dir.ancestors().find(|dir| dir.join("mainnet-identity.json").is_file()) else {
                process::exit(20);
            };
            let path = root.join("mainnet-identity.json");
            println!("cargo::rerun-if-changed={}", path.display());
            let data = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => process::exit(21),
            };
            println!("cargo::rustc-env=TEMPO_ID={}", data.trim());
        }
        "liar" => {
            println!("cargo:rerun-if-changed=build.rs");
            let data = match fs::read_to_string(manifest_dir.join("../other/secret.txt")) {
                Ok(s) => s,
                Err(_) => process::exit(30),
            };
            println!("cargo:rustc-env=LEAKED={}", data.trim());
        }
        "absent" => {
            println!("cargo:rerun-if-changed=opt.json");
            let present = manifest_dir.join("opt.json").is_file();
            println!("cargo:rustc-env=HAS_OPT={present}");
        }
        "dir" => {
            println!("cargo:rerun-if-changed=assets");
            let data = match fs::read_to_string(manifest_dir.join("assets/a.txt")) {
                Ok(s) => s,
                Err(_) => process::exit(40),
            };
            println!("cargo:rustc-env=ASSET={}", data.trim());
        }
        "pkg_only" => {
            let data = match fs::read_to_string(manifest_dir.join("src/local.txt")) {
                Ok(s) => s,
                Err(_) => process::exit(50),
            };
            println!("cargo:rustc-env=LOCAL={}", data.trim());
        }
        _ => process::exit(99),
    }
}

fn cert_scenario<'a>(
    ws_real: &str,
    pkg_rel: &str,
    work: &str,
    shelf: &str,
    sdk: &str,
    files: &str,
    exe: &str,
    mode: &str,
    alloc: Alloc<'a>,
) -> Option<AVec<'a, (&'static str, AString<'a>, Option<blaze_ids::ContentId>)>> {
    let pkg_real = blaze_own::format_in!(alloc, "{ws_real}/{pkg_rel}");
    let out_dir = blaze_own::format_in!(alloc, "{work}/out-primary-{mode}");
    assert!(fs::create_dir_all(out_dir.as_str()).is_ok());
    let seen = match blaze_expand::seen::SeenRoot::member(pkg_rel) {
        Ok(s) => s,
        Err(_) => {
            assert!(false, "valid member dir");
            return None;
        }
    };
    let shown_pkg = blaze_own::format_in!(alloc, "/workspace/{pkg_rel}");
    let mut vars: AVec<'a, (AString<'a>, AString<'a>)> = Vec::new_in(alloc);
    for &(k, v) in &[
        (CERT_MODE, mode),
        ("CARGO_MANIFEST_DIR", shown_pkg.as_str()),
        ("OUT_DIR", "/target/debug/build/demo-0/out"),
        ("TARGET", "aarch64-apple-darwin"),
        ("HOST", "aarch64-apple-darwin"),
        ("PROFILE", "debug"),
    ] {
        vars.push((String::from_str_in(k, alloc), String::from_str_in(v, alloc)));
    }
    vars.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let env = Env { vars };
    let c = CInputs {
        shelf: Shelf { dir: shelf, clang_major: "23" },
        sysroot: sdk,
        out_dir: out_dir.as_str(),
        package: pkg_real.as_str(),
        seen,
        macos_min: "11.0",
    };
    let dep_reads = [ws_real];
    let run = NativeScript {
        key: blaze_ids::ContentId([9; 32]),
        program: exe,
        c,
        env: &env,
        reads: &dep_reads,
        shown: &[],
        host_tools: &[],
        facts: None,
        jobs: None,
        confined: None,
        limits: RunLimits::STANDARD,
        deadline: Duration::from_secs(60),
        files,
    };
    let ran = match run_script(&run, alloc) {
        Ok(r) => r,
        Err(_) => {
            assert!(false, "primary run failed for mode {mode}");
            return None;
        }
    };
    let real_root = match RealRoot::resolve(Path::new(ws_real)) {
        Ok(r) => r,
        Err(_) => {
            assert!(false, "resolve workspace root");
            return None;
        }
    };
    let tree = Tree::Dir(real_root);
    let asked = Asked::default();
    certify_member_reads(
        CertifyInput {
            tree: &tree,
            ws_real,
            pkg_real: pkg_real.as_str(),
            out_dir: out_dir.as_str(),
            work,
            key_hex: mode,
            package_label: mode,
            run: &run,
            laid_folders: &[],
            shim: None,
            pins: None,
            ran: &ran,
            asked: &asked,
        },
        alloc,
    )
}

fn confined_certification_proves_exact_reads_and_catches_liars() {
    let alloc = blaze_own::testing::heap().alloc();
    let pid = process::id();
    let root_path = env::temp_dir().join(blaze_own::format_in!(alloc, "blaze-native-cert-{pid}").as_str());
    let _ = fs::remove_dir_all(&root_path);
    for dir in [
        "ws/crates/node/src",
        "ws/crates/precompiles/src",
        "ws/crates/chainspec/src",
        "ws/crates/liar",
        "ws/crates/opt",
        "ws/crates/dirpkg/assets",
        "ws/crates/pkgonly/src",
        "ws/crates/other",
        "work",
        "shelf",
        "sdk",
        "files",
    ] {
        assert!(fs::create_dir_all(root_path.join(dir)).is_ok());
    }
    for (rel, body) in [
        ("ws/mainnet-identity.json", "{\"chain\":\"mainnet\"}\n"),
        ("ws/crates/node/build.rs", "fn main() {}\n"),
        ("ws/crates/node/src/lib.rs", "pub fn x() {}\n"),
        ("ws/crates/precompiles/build.rs", "fn main() {}\n"),
        ("ws/crates/precompiles/src/pcrs.json", "{\"pcr0\":\"aa\"}\n"),
        ("ws/crates/precompiles/src/lib.rs", "pub fn y() {}\n"),
        ("ws/crates/chainspec/build.rs", "fn main() {}\n"),
        ("ws/crates/chainspec/src/spec.rs", "pub fn spec() {}\n"),
        ("ws/crates/liar/build.rs", "fn main() {}\n"),
        ("ws/crates/opt/build.rs", "fn main() {}\n"),
        ("ws/crates/dirpkg/build.rs", "fn main() {}\n"),
        ("ws/crates/dirpkg/assets/a.txt", "hello\n"),
        ("ws/crates/pkgonly/build.rs", "fn main() {}\n"),
        ("ws/crates/pkgonly/src/local.txt", "local-data\n"),
        ("ws/crates/other/secret.txt", "top-secret\n"),
        ("ws/crates/other/unrelated.rs", "pub fn z() {}\n"),
    ] {
        assert!(fs::write(root_path.join(rel), body).is_ok());
    }
    let canon = match fs::canonicalize(&root_path) {
        Ok(p) => p,
        Err(_) => {
            assert!(false, "canonicalize root");
            return;
        }
    };
    let dir_str = |sub: &str| -> AString<'_> {
        let p = canon.join(sub);
        match p.to_str() {
            Some(s) => String::from_str_in(s, alloc),
            None => {
                assert!(false, "utf8 path");
                String::new_in(alloc)
            }
        }
    };
    let ws = dir_str("ws");
    let work = dir_str("work");
    let shelf = dir_str("shelf");
    let sdk = dir_str("sdk");
    let files = dir_str("files");
    let exe_path = match env::current_exe().and_then(fs::canonicalize) {
        Ok(p) => p,
        Err(_) => {
            assert!(false, "current exe");
            return;
        }
    };
    let exe = match exe_path.to_str() {
        Some(s) => String::from_str_in(s, alloc),
        None => {
            assert!(false, "utf8 exe");
            return;
        }
    };

    let vergen = cert_scenario(ws.as_str(), "crates/node", work.as_str(), shelf.as_str(), sdk.as_str(), files.as_str(), exe.as_str(), "vergen", alloc);
    assert!(vergen.is_some(), "vergen script must certify");
    if let Some(specs) = vergen {
        assert_eq!(specs.len(), 1);
        assert_eq!((specs[0].0, specs[0].1.as_str()), ("file:", "crates/node/build.rs"));
    }

    let pre = cert_scenario(ws.as_str(), "crates/precompiles", work.as_str(), shelf.as_str(), sdk.as_str(), files.as_str(), exe.as_str(), "precompiles", alloc);
    assert!(pre.is_some(), "precompiles script must certify");
    if let Some(specs) = pre {
        assert_eq!(specs.len(), 1);
        assert_eq!((specs[0].0, specs[0].1.as_str()), ("file:", "crates/precompiles/src/pcrs.json"));
    }

    let chain = cert_scenario(ws.as_str(), "crates/chainspec", work.as_str(), shelf.as_str(), sdk.as_str(), files.as_str(), exe.as_str(), "chainspec", alloc);
    assert!(chain.is_some(), "chainspec script must certify");
    if let Some(specs) = chain {
        assert_eq!(specs.len(), 2);
        assert_eq!((specs[0].0, specs[0].1.as_str()), ("file:", "crates/chainspec/build.rs"));
        assert_eq!((specs[1].0, specs[1].1.as_str()), ("file:", "mainnet-identity.json"));
    }

    let liar = cert_scenario(ws.as_str(), "crates/liar", work.as_str(), shelf.as_str(), sdk.as_str(), files.as_str(), exe.as_str(), "liar", alloc);
    assert!(liar.is_none(), "lying script MUST fail confined certification");

    let absent = cert_scenario(ws.as_str(), "crates/opt", work.as_str(), shelf.as_str(), sdk.as_str(), files.as_str(), exe.as_str(), "absent", alloc);
    assert!(absent.is_some(), "absent rerun-if-changed must certify");
    if let Some(specs) = absent {
        let opt_read = specs.iter().find(|(p, r, _)| *p == "file:" && r.as_str() == "crates/opt/opt.json");
        assert!(opt_read.is_some(), "missing file must be recorded");
        if let Some((_, _, digest)) = opt_read {
            assert_eq!(*digest, None, "absent file must have None digest");
        }
    }

    let dirpkg = cert_scenario(ws.as_str(), "crates/dirpkg", work.as_str(), shelf.as_str(), sdk.as_str(), files.as_str(), exe.as_str(), "dir", alloc);
    assert!(dirpkg.is_some(), "dir rerun-if-changed must certify");
    if let Some(specs) = dirpkg {
        assert!(specs.iter().any(|(p, r, _)| *p == "dir:" && r.as_str() == "crates/dirpkg/assets"));
        assert!(specs.iter().any(|(p, r, _)| *p == "file:" && r.as_str() == "crates/dirpkg/assets/a.txt"));
        assert!(!specs.iter().any(|(_, r, _)| r.as_str().contains("crates/other")));
    }

    let pkgonly = cert_scenario(ws.as_str(), "crates/pkgonly", work.as_str(), shelf.as_str(), sdk.as_str(), files.as_str(), exe.as_str(), "pkg_only", alloc);
    assert!(pkgonly.is_some(), "package-only script must certify");
    if let Some(specs) = pkgonly {
        assert!(specs.iter().any(|(p, r, _)| *p == "dir:" && r.as_str() == "crates/pkgonly"));
        assert!(specs.iter().any(|(p, r, _)| *p == "dir:" && r.as_str() == "crates/pkgonly/src"));
        assert!(specs.iter().any(|(p, r, _)| *p == "file:" && r.as_str() == "crates/pkgonly/src/local.txt"));
        assert!(!specs.iter().any(|(_, r, _)| r.as_str().contains("crates/other")));
        assert!(!specs.iter().any(|(_, r, _)| r.as_str().contains("crates/chainspec")));
    }

    let _ = fs::remove_dir_all(&root_path);
    println!("native certify: all 7 confined certification & soundness scenarios passed");
}
