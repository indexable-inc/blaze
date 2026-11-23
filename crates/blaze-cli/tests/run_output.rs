//! `blaze run` and `blaze export` as processes:
//! - `blaze run` on a clean rung: its standard output is byte-equal to the rung's `expected`, its
//!   standard error is empty, and its status is the program's.
//! - `blaze export` on `no_core`, `std`, multi-threaded `#[thread_local]`/`thread_local!`, and
//!   multi-package workspaces (`-p`, `--bin`, `-o`, `--workers`, `--stats`): executes the emitted
//!   standalone native binary directly with `env_clear()` (no `blaze-launcher`) and verifies its
//!   stdout, exit status, and executable permissions.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// The names and lengths in the owner's one store, `~/.blaze/v2/store`, read only: a test compares
/// them before and after to show it wrote nothing there. Another blaze the owner runs at the
/// same moment would change them too: the assertion names what changed.
fn real_shared_listing() -> Vec<(String, u64)> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(Path::new(&home).join(".blaze/v2/store")) else { return Vec::new() };
    let mut listed: Vec<(String, u64)> = entries
        .filter_map(Result::ok)
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.metadata().map(|meta| meta.len()).unwrap_or(0)))
        .collect();
    listed.sort();
    listed
}

/// The rungs whose program runs clean today and prints through the C library.
/// `x0-static-05-interior-mut`: six statics, one per shape of interior mutability (and a plain
/// one), written through the child's writable data area and read back.
const CLEAN: &[&str] = &["3-hello", "front2/x0-static-05-interior-mut"];

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn utf8(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("<non-utf8>")
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

fn ensure_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir {}: {e}", parent.display()));
    }
}

fn write_file(path: &Path, text: &str) {
    ensure_parent(path);
    std::fs::write(path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn copy_file(from: &Path, to: &Path) {
    ensure_parent(to);
    std::fs::copy(from, to).unwrap_or_else(|e| panic!("copy {} -> {}: {e}", from.display(), to.display()));
}

fn run_with_deadline(mut cmd: Command, label: &str, deadline: Duration) -> Output {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("{label}: failed to spawn: {e}"));
    let started = Instant::now();
    while child.try_wait().unwrap_or_else(|e| panic!("{label}: try_wait failed: {e}")).is_none() {
        if started.elapsed() > deadline {
            let _ = child.kill();
            panic!("{label}: exceeded {deadline:?} deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap_or_else(|e| panic!("{label}: wait_with_output failed: {e}"))
}

fn run_standalone(bin: &Path, label: &str) -> Output {
    assert!(bin.is_file(), "{label}: exported binary missing at {}", bin.display());
    let mode = std::fs::metadata(bin).expect("metadata").permissions().mode();
    assert_ne!(mode & 0o111, 0, "{label}: exported binary at {} is not executable (mode {mode:#o})", bin.display());
    let mut cmd = Command::new(bin);
    cmd.env_clear();
    run_with_deadline(cmd, label, secs(15))
}

fn blaze_cmd(dir: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_blaze"));
    cmd.current_dir(dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("BLAZE_NO_DAEMON", "1");
    cmd
}

/// `build` is no command of blaze (the owner, 2026-11-15: the commands are `check`, `run`,
/// `test` and later `export`): the process says so, names the two commands that do its work,
/// and fails; it never says `build` is yet to be built. With the old arm
/// (`CliError::NotBuilt { command: "build" }`) the first assertion fails: "`blaze build` is not
/// built on the new item layer yet".
#[test]
fn build_is_no_command_and_names_run_and_export() {
    let base = fresh_home("build", false);
    let output = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("build")
        .env("HOME", base.join("home"))
        .env("CARGO_HOME", base.join("home/.cargo"))
        .stdin(Stdio::null())
        .output()
        .expect("blaze starts");
    cleanup(&base);
    let said = utf8(&output.stderr);
    assert!(said.contains("blaze has no `build` command"), "{said}");
    assert!(said.contains("`blaze run`") && said.contains("`blaze export`"), "{said}");
    assert!(!said.contains("is not built on the new item layer"), "{said}");
    assert_ne!(output.status.code(), Some(0), "{said}");
}

/// Run with `HOME` at an empty temporary folder (its toolchain shelf hard-linked in): the run
/// passes, its one store lands in that folder, and the owner's `~/.blaze/v2/store` lists
/// the same names and lengths before and after (read only). Expected: passes; with `HOME`
/// left to the owner's, the last assertion fails ("the temporary home's shared store").
#[test]
fn a_clean_run_prints_only_its_program() {
    let ladder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ladder");
    let base = fresh_home("run_output", true);
    let home = base.join("home");
    let scratch = base.join("checkouts");
    let before = real_shared_listing();
    for rung in CLEAN {
        let from = ladder.join(rung);
        let dir = scratch.join(rung);
        cleanup(&dir);
        copy_file(&from.join("Cargo.toml"), &dir.join("Cargo.toml"));
        copy_file(&from.join("src/main.rs"), &dir.join("src/main.rs"));
        let expected = std::fs::read(from.join("expected")).expect("the rung's expected");
        let stdout = expected.strip_prefix(b"status 0\nstdout\n".as_slice()).expect("expected: status 0, then stdout");
        let mut cmd = blaze_cmd(&dir, &home);
        cmd.arg("run");
        let output = run_with_deadline(cmd, rung, secs(60));
        assert_eq!(output.status.code(), Some(0), "{rung}: {}", utf8(&output.stderr));
        assert_eq!(utf8(&output.stdout), utf8(stdout), "{rung}: standard output");
        assert_eq!(utf8(&output.stderr), "", "{rung}: a clean run says nothing");
    }
    assert_eq!(real_shared_listing(), before, "the owner's ~/.blaze/v2/store changed while the test ran (another blaze of the owner's, or this test writing there)");
    assert!(home.join(".blaze/v2/store").is_dir(), "the temporary home's shared store: the run kept its records there");
    cleanup(&base);
}

/// Rule 4 (blaze-ladder personality.rs): a `no_core` program built `panic = "unwind"` with no
/// `#[lang = "eh_personality"]` fn runs while no record lands (`3-hello` and
/// `x0-static-05-interior-mut` above), and the one whose `main` holds a value with a `Drop`
/// across a call that may unwind (tests/run_refused/no-routine) is refused at `main`'s record,
/// naming `main` by its path, with status 125 (not run). A parent that refused at the start
/// fails the clean test above; one that placed the record fails here (the child's own
/// `EhError::NoPersonality` names no fn).
#[test]
fn a_landing_record_with_no_routine_is_refused_naming_its_fn() {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run_refused/no-routine");
    let base = fresh_home("run_refused", true);
    let home = base.join("home");
    let dir = base.join("checkouts/no-routine");
    cleanup(&dir);
    copy_file(&from.join("Cargo.toml"), &dir.join("Cargo.toml"));
    copy_file(&from.join("src/main.rs"), &dir.join("src/main.rs"));
    let mut cmd = blaze_cmd(&dir, &home);
    cmd.arg("run");
    let output = run_with_deadline(cmd, "no-routine", secs(60));
    let said = utf8(&output.stderr);
    assert_eq!(output.status.code(), Some(125), "{said}");
    assert!(said.contains("::main`") && said.contains("eh_personality"), "the refusal does not name `main` and the missing routine: {said}");
    assert_eq!(utf8(&output.stdout), "", "a refused program printed");
    println!("run output: no routine, a landing record refused: {said}");
    cleanup(&base);
}

#[test]
fn export_no_core_c_main_runs_standalone() {
    let ladder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ladder/3-hello");
    let base = fresh_home("export_no_core", true);
    let home = base.join("home");
    let dir = base.join("checkouts/3-hello");
    copy_file(&ladder.join("Cargo.toml"), &dir.join("Cargo.toml"));
    copy_file(&ladder.join("src/main.rs"), &dir.join("src/main.rs"));

    let mut cmd = blaze_cmd(&dir, &home);
    cmd.args(["export", "-o", "out/hello-standalone"]);
    let exported = run_with_deadline(cmd, "blaze export 3-hello", secs(120));
    assert_eq!(exported.status.code(), Some(0), "stdout: {}\nstderr: {}", utf8(&exported.stdout), utf8(&exported.stderr));
    assert!(utf8(&exported.stdout).contains("exported out/hello-standalone"), "unexpected summary: {}", utf8(&exported.stdout));

    let ran = run_standalone(&dir.join("out/hello-standalone"), "run hello-standalone");
    assert_eq!(ran.status.code(), Some(0), "stderr: {}", utf8(&ran.stderr));
    assert_eq!(utf8(&ran.stdout), "hello\n");
    cleanup(&base);
}

#[test]
fn export_std_rust_binary_runs_standalone() {
    let base = fresh_home("export_std_main", true);
    let home = base.join("home");
    let dir = base.join("checkouts/hello-std");
    write_file(&dir.join("Cargo.toml"), "[package]\nname = \"hello-std\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_file(
        &dir.join("src/main.rs"),
        "use std::process::ExitCode;\n\nfn main() -> ExitCode {\n    println!(\"exported std hello {}\", 20 + 22);\n    ExitCode::from(42)\n}\n",
    );

    let mut cmd = blaze_cmd(&dir, &home);
    cmd.arg("export");
    let exported = run_with_deadline(cmd, "blaze export hello-std", secs(120));
    assert_eq!(exported.status.code(), Some(0), "stdout: {}\nstderr: {}", utf8(&exported.stdout), utf8(&exported.stderr));

    let ran = run_standalone(&dir.join("hello-std"), "run hello-std");
    assert_eq!(ran.status.code(), Some(42), "stderr: {}", utf8(&ran.stderr));
    assert_eq!(utf8(&ran.stdout), "exported std hello 42\n");
    cleanup(&base);
}

#[test]
fn export_tls_program_runs_standalone() {
    let base = fresh_home("export_tls", true);
    let home = base.join("home");
    let dir = base.join("checkouts/tls-app");
    write_file(&dir.join("Cargo.toml"), "[package]\nname = \"tls-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_file(
        &dir.join("src/main.rs"),
        r#"#![feature(thread_local)]

use std::cell::Cell;
use std::ffi::c_void;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

#[thread_local]
static RAW_TLS: Cell<u32> = Cell::new(10);

static DROPS: AtomicUsize = AtomicUsize::new(0);

struct DropGuard;
impl Drop for DropGuard {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static COUNT: Cell<u32> = const { Cell::new(1) };
    static GUARD: DropGuard = DropGuard;
}

#[repr(C)]
struct ChildOut {
    raw_start: u32,
    raw_end: u32,
    count_start: u32,
    count_end: u32,
}

unsafe extern "C" {
    fn pthread_create(
        thread: *mut usize,
        attr: *const c_void,
        start: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> i32;
    fn pthread_join(thread: usize, retval: *mut *mut c_void) -> i32;
}

extern "C" fn worker(arg: *mut c_void) -> *mut c_void {
    let raw_start = RAW_TLS.get();
    let count_start = COUNT.get();
    RAW_TLS.set(raw_start + 100);
    COUNT.set(count_start + 20);
    GUARD.with(|_| {});
    unsafe {
        let out = &mut *(arg as *mut ChildOut);
        out.raw_start = raw_start;
        out.raw_end = RAW_TLS.get();
        out.count_start = count_start;
        out.count_end = COUNT.get();
    }
    std::ptr::null_mut()
}

fn main() -> ExitCode {
    RAW_TLS.set(RAW_TLS.get() + 5);
    COUNT.set(COUNT.get() + 6);
    let mut child = ChildOut { raw_start: 0, raw_end: 0, count_start: 0, count_end: 0 };
    let mut tid: usize = 0;
    let rc = unsafe {
        pthread_create(&mut tid, std::ptr::null(), worker, (&mut child as *mut ChildOut).cast::<c_void>())
    };
    assert_eq!(rc, 0);
    let jrc = unsafe { pthread_join(tid, std::ptr::null_mut()) };
    assert_eq!(jrc, 0);
    println!(
        "main=({},{}) child=({},{},{},{}) drops={}",
        RAW_TLS.get(),
        COUNT.get(),
        child.raw_start,
        child.raw_end,
        child.count_start,
        child.count_end,
        DROPS.load(Ordering::SeqCst),
    );
    ExitCode::from(7)
}
"#,
    );

    let out_bin = base.join("tls-standalone");
    let mut cmd = blaze_cmd(&dir, &home);
    cmd.args(["export", "-o"]).arg(&out_bin);
    let exported = run_with_deadline(cmd, "blaze export tls-app", secs(120));
    assert_eq!(exported.status.code(), Some(0), "stdout: {}\nstderr: {}", utf8(&exported.stdout), utf8(&exported.stderr));
    assert!(utf8(&exported.stdout).contains("tls"), "expected TLS count in summary: {}", utf8(&exported.stdout));

    let ran = run_standalone(&out_bin, "run tls-standalone");
    assert_eq!(ran.status.code(), Some(7), "stderr: {}", utf8(&ran.stderr));
    assert_eq!(utf8(&ran.stdout), "main=(15,7) child=(10,110,1,21) drops=1\n");
    cleanup(&base);
}

#[test]
fn export_workspace_selects_package_bin_output_and_stats() {
    let base = fresh_home("export_workspace", true);
    let home = base.join("home");
    let ws = base.join("ws");
    write_file(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\nresolver = \"2\"\n");
    write_file(
        &ws.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"first\"\npath = \"src/bin/first.rs\"\n\n[[bin]]\nname = \"second\"\npath = \"src/bin/second.rs\"\n",
    );
    write_file(&ws.join("crates/alpha/src/bin/first.rs"), "fn main() { println!(\"from-first\"); }\n");
    write_file(&ws.join("crates/alpha/src/bin/second.rs"), "fn main() { println!(\"from-second\"); }\n");
    write_file(&ws.join("crates/beta/Cargo.toml"), "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_file(&ws.join("crates/beta/src/main.rs"), "fn main() { println!(\"from-beta\"); }\n");

    let out_bin = base.join("out-dir/second-bin");
    let mut cmd = blaze_cmd(&base, &home);
    cmd.args(["export", "ws", "-p", "alpha", "--bin", "second", "-o"]).arg(&out_bin).args(["--workers", "2", "--stats"]);
    let exported = run_with_deadline(cmd, "blaze export ws -p alpha --bin second", secs(120));
    assert_eq!(exported.status.code(), Some(0), "stdout: {}\nstderr: {}", utf8(&exported.stdout), utf8(&exported.stderr));
    assert!(utf8(&exported.stderr).contains("export stats:"), "expected --stats line on stderr: {}", utf8(&exported.stderr));

    let ran = run_standalone(&out_bin, "run second-bin");
    assert_eq!(ran.status.code(), Some(0), "stderr: {}", utf8(&ran.stderr));
    assert_eq!(utf8(&ran.stdout), "from-second\n");
    cleanup(&base);
}
