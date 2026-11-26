//! `blaze test` RUNS tests (crates/blaze-cli/src/test_drive.rs over test_loop.rs and
//! test_run.rs; docs/v2/plan/design/test.md "End state", "One process per test"): a one-crate
//! package, each `#[test]` fn entered in a process of its own under the lazy parent `blaze run`
//! uses, its output captured, its verdict read from how the process ended.
//!
//! The case `a_pass_and_a_failure` (`passes` returns, `fails` runs `assert_eq!(1, 2)`) prints on
//! standard output, after its check's own lines (`CHECK_FIRST`: D147, D151), exactly these lines, in this order (`<t>` is the failing test's time,
//! `d.ddd`; `<n>` its output's byte count; the block between the `----` lines is the panic's
//! words as the program's std writes them, holding `assertion `left == right` failed`; the
//! rule's width is the terminal's, 80 when not a terminal):
//!
//! ```text
//! FAILED fails after <t> s: panicked: assertion `left == right` failed
//!   left: 1
//!  right: 2
//! ---- fails output: <n> bytes ----
//! thread '...' (...) panicked at src/lib.rs:6:5:
//! assertion `left == right` failed
//! ...
//! --------------------------------------------------------------------------------
//! FAILED fails: panicked
//! test result: FAILED. ran 2: 1 passed, 1 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky
//! ```
//!
//! and its status is not 0. The case `a_pass_alone` (`passes` only) prints
//!
//! ```text
//! test result: ok. ran 1: 1 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky
//! ```
//!
//! and its status is 0. "panicked": a harnessed test's panic unwinds into the launcher's
//! catch-all frame (`End::Return`), writes its unwind marker and exits 101, so `blaze test`
//! reports `panicked` with the message extracted from stderr (leaving genuine `SIGABRT`s as
//! `ended by signal 6`).
//!
//! THE CONTROL (rule 23): [`judge`] is the one check both cases use, and `every_test_passes_is
//! _caught` hands it what a driver planted to say "every test passes" would print (the summary
//! `test result: ok. ran 2: 2 passed, 0 failed` with status 0) for the package with `fails`: the
//! check must refuse it. A judge that passed it would pass a driver that runs nothing.
//!
//! Run: `cargo test -p blaze-cli --test test_command_runs` (needs the toolchain's shelf in the
//! owner's home, as tests/command_rungs.rs does). Expected: `test a_pass_and_a_failure ... ok`,
//! `test a_pass_alone ... ok`, `test every_test_passes_is_caught ... ok`.

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// The whole command's bound: two tests, each a launcher start and a few first calls built;
/// past it the command hung, which is a bug (rule 18), named here.
const DEADLINE: Duration = Duration::from_secs(300);

const MANIFEST: &str = "[package]\nname = \"two\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n";

const PASS: &str = "#[test]\nfn passes() {}\n";
const FAIL: &str = "#[test]\nfn fails() {\n    assert_eq!(1, 2);\n}\n";

const BOTH_SUMMARY: &str = "test result: FAILED. ran 2: 1 passed, 1 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";
const PASS_SUMMARY: &str = "test result: ok. ran 1: 1 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";

/// What `blaze test` prints before anything of its tests: its check's own lines, exactly. D147:
/// "`run` and `test` read the engine's answer: one check (`check_for_start`), then the start";
/// D151: "`run` prints the check's own incomplete line". While borrows are notes (body_types.rs
/// `BORROWS_ARE_ERRORS` is false) a clean check is "incomplete" (`blaze explain 3`). [`run`]
/// takes these off and refuses a run that did not print them first; every expectation below is
/// of what follows.
const CHECK_FIRST: &str = "0 errors: blaze check\n";

/// What one `blaze test` printed and how it ended.
struct Said {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

/// `blaze test` in a fresh package holding `lib`, in a scratch home; the run's words.
fn blaze_test(name: &str, lib: &str) -> Said {
    fixture(name, &[("Cargo.toml", MANIFEST), ("src/lib.rs", lib)], |dir, home| {
        run(dir, home, &["--", "--deadline", "20s"])
    })
}

fn write_source(path: &Path, text: &str, what: &str) {
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {what}: {error}"));
}

fn read_file_bytes(path: &Path, what: &str) -> impl AsRef<[u8]> + use<> {
    std::fs::read(path).unwrap_or_else(|error| panic!("reading {what}: {error}"))
}

fn pause_poll() {
    std::thread::sleep(Duration::from_millis(10));
}

fn fixture<T>(name: &str, files: &[(&str, &str)], check: impl FnOnce(&Path, &Path) -> T) -> T {
    let base = fresh_home(name, true);
    // The test profile names the test's folder by its canonical path (blaze-run child.rs
    // `seatbelt` refuses another); the system's temporary folder is behind a link on macOS.
    let base = base.canonicalize().expect("the scratch folder's canonical path");
    let home = base.join("home");
    let dir = base.join("project");
    for &(name, source) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("a file in the fixture"))
            .unwrap_or_else(|error| panic!("making the folder for {name}: {error}"));
        write_source(&path, source, name);
    }
    let result = check(&dir, &home);
    // A detached `blaze gc --background` child may still be unlinking its lock file inside `home`.
    for attempt in 0..100 {
        match std::fs::remove_dir_all(&base) {
            Ok(()) => break,
            Err(_) if !base.exists() => break,
            Err(_) if attempt + 1 < 100 => pause_poll(),
            Err(err) => panic!("remove the scratch command fixture: {err}"),
        }
    }
    result
}

fn run_cmd(dir: &Path, home: &Path, subcommand: &str, args: &[&str]) -> Said {
    // A full pipe must never stall the compiler while the fixture waits for it to exit.
    let stdout = home.join("test.stdout");
    let stderr = home.join("test.stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg(subcommand)
        .args(args)
        .current_dir(dir)
        .env("PWD", dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).expect("capture blaze's standard output"))
        .stderr(std::fs::File::create(&stderr).expect("capture blaze's standard error"))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("a wait on blaze") {
            break status;
        }
        if started.elapsed() > DEADLINE {
            child.kill().expect("stop the timed-out command");
            let killed = Instant::now();
            while child.try_wait().expect("reap the killed command").is_none() {
                assert!(killed.elapsed() < Duration::from_secs(10), "reaping blaze in {}", dir.display());
                pause_poll();
            }
            panic!("`blaze {subcommand}` past {DEADLINE:?} in {}: the command hung (rule 18)", dir.display());
        }
        pause_poll();
    };
    Said {
        stdout: std::fs::read_to_string(stdout).expect("read blaze's standard output"),
        // Lossy on purpose: a test child's raw standard error is quoted in it, and a child that
        // writes bytes that are not UTF-8 is evidence to show, not a reason to hide the rest.
        stderr: String::from_utf8_lossy(&std::fs::read(stderr).expect("read blaze's standard error")).into_owned(),
        code: status.code(),
    }
}

fn run(dir: &Path, home: &Path, args: &[&str]) -> Said {
    let said = run_cmd(dir, home, "test", args);
    // The check's lines first (D147, D151), then the tests' alone.
    let Some(tests) = said.stdout.strip_prefix(CHECK_FIRST) else {
        panic!("{}", evidence(&format!("the check's own lines first, exactly:\n{CHECK_FIRST}"), &said));
    };
    let tests = tests.to_owned();
    Said { stdout: tests, ..said }
}

/// Quote each stream as ASCII so binary data and long lines survive the test log.
fn evidence(why: &str, said: &Said) -> String {
    byte_evidence(why, said.code, said.stderr.as_bytes(), said.stdout.as_bytes())
}

fn byte_evidence(why: &str, code: Option<i32>, stderr: &[u8], stdout: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut report = format!("--- status {code:?}; {why}\n");
    for (name, bytes) in [("stderr", stderr), ("stdout", stdout)] {
        writeln!(report, "--- {name} ({} bytes)", bytes.len()).expect("write the stream length");
        let mut width = 0;
        for &byte in bytes.iter().take(4000) {
            let escaped = std::ascii::escape_default(byte);
            if width + escaped.len() > 100 {
                report.push('\n');
                width = 0;
            }
            width += escaped.len();
            report.extend(escaped.map(char::from));
        }
        if width != 0 {
            report.push('\n');
        }
    }
    report
}

/// THE CHECK both cases and the control use: `Ok` when `said` is exactly the expected run of
/// the package (with `fails` when `failing`), else what differs.
fn judge(said: &Said, failing: bool) -> Result<(), String> {
    let lines: Vec<&str> = said.stdout.lines().collect();
    let tail = if failing { ["FAILED fails: panicked", BOTH_SUMMARY].as_slice() } else { [PASS_SUMMARY].as_slice() };
    if lines.len() < tail.len() || lines[lines.len() - tail.len()..] != *tail {
        return Err(format!("the summary is not\n{}\n", tail.join("\n")));
    }
    if failing {
        if said.code != Some(1) {
            return Err(format!("a suite with a failing test ended with status {:?}", said.code));
        }
        let first = lines[0];
        let time = first
            .strip_prefix("FAILED fails after ")
            .and_then(|rest| rest.split_once(" s: panicked: "))
            .map(|(t, msg)| (t, msg));
        let timed = time.is_some_and(|(t, msg)| {
            t.split_once('.').is_some_and(|(s, ms)| {
                !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && ms.len() == 3 && ms.bytes().all(|b| b.is_ascii_digit())
            }) && msg.contains("assertion `left == right` failed")
        });
        if !timed {
            return Err("the first line is not `FAILED fails after d.ddd s: panicked: assertion `left == right` failed`".into());
        }
        let Some(header_idx) = lines.iter().position(|line| line.starts_with("---- fails output: ")) else {
            return Err("the failure's output block header is missing".into());
        };
        let Some(header) = lines.get(header_idx).and_then(|line| line.strip_prefix("---- fails output: ")).and_then(|s| s.strip_suffix(" ----")) else {
            return Err("the failure's output block header is malformed".into());
        };
        if header.contains(", the first ") {
            return Err("the assertion's output exceeded the reporter's cap and its beginning was cut".into());
        }
        if header.strip_suffix(" bytes").and_then(|n| n.parse::<usize>().ok()).is_none() {
            return Err("the failure's output header has no byte count".into());
        }
        let rule = lines.iter().position(|line| line.len() >= 4 && line.bytes().all(|b| b == b'-'));
        let Some(rule) = rule else { return Err("the failure's output block is not closed by a rule".into()) };
        if !lines[header_idx + 1..rule].iter().any(|line| line.contains("panicked at ") && line.ends_with("src/lib.rs:6:5:")) {
            return Err("the panic's location is not the assert_eq! invocation at src/lib.rs:6:5".into());
        }
        if !lines[header_idx + 1..rule].iter().any(|line| line.contains("assertion `left == right` failed")) {
            return Err("the failure's output does not hold the panic's words".into());
        }
        if lines.len() != rule + 1 + tail.len() {
            return Err("lines between the failure's block and the summary".into());
        }
    } else {
        if said.code != Some(0) {
            return Err(format!("a passing suite ended with status {:?}", said.code));
        }
        if lines.len() != tail.len() {
            return Err("lines before the summary of a passing suite".into());
        }
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn a_pass_and_a_failure() {
    let lib = format!("{PASS}\n{FAIL}");
    let (said, stderr, stdout) = fixture("test_command_runs_both", &[("Cargo.toml", MANIFEST), ("src/lib.rs", &lib)], |dir, home| {
        // The shared runner decodes stdout before returning. Preserve the file evidence
        // even if a malformed byte stops that decode.
        let result = std::panic::catch_unwind(|| run(dir, home, &["--", "--deadline", "20s"]));
        let stderr = read_file_bytes(&home.join("test.stderr"), "the command's stderr bytes");
        let stdout = read_file_bytes(&home.join("test.stdout"), "the command's stdout bytes");
        let (stderr, stdout) = (stderr.as_ref(), stdout.as_ref());
        let said = match result {
            Ok(said) => said,
            Err(payload) => {
                eprintln!("{}", byte_evidence("the command runner did not return a status", None, stderr, stdout));
                std::panic::resume_unwind(payload);
            }
        };
        (said, stderr.to_vec(), stdout.to_vec())
    });
    let report = |why: &str| byte_evidence(why, said.code, &stderr, &stdout);
    let diagnostic = std::str::from_utf8(&stderr);
    assert!(diagnostic.is_ok(), "{}", report("the command wrote non-UTF-8 stderr"));
    if let Ok(diag) = diagnostic {
        assert!(!diag.contains(" signal 6: stderr ("), "{}", report("a normal Rust panic must not trigger a signal 6 crash dump"));
    }
    let judged = judge(&said, true);
    assert!(judged.is_ok(), "{}", report(judged.as_ref().err().map_or("", String::as_str)));
    assert!(
        !said.stdout.contains("Rust cannot catch foreign exceptions"),
        "{}",
        report("a guest test panic unwound into host catch_unwind and aborted as a foreign exception")
    );
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn a_pass_alone() {
    let said = blaze_test("test_command_runs_pass", PASS);
    if let Err(why) = judge(&said, false) {
        panic!("{}", evidence(&why, &said));
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn pinned_core_transparent_arguments_and_results() {
    let source = r#"
use core::{mem::{ManuallyDrop, transmute}, num::NonZeroU32, ptr::NonNull};

extern "C" fn pointer(x: NonNull<u8>) -> NonNull<u8> { x }
extern "C-unwind" fn nullable(x: Option<NonNull<u8>>) -> Option<NonNull<u8>> { x }
extern "C" fn integer(x: NonZeroU32) -> NonZeroU32 { x }
fn manual(x: ManuallyDrop<u64>) -> ManuallyDrop<u64> { x }

#[test]
fn passes() {
    let p: extern "C" fn(NonNull<u8>) -> NonNull<u8> = pointer;
    let o: extern "C-unwind" fn(Option<NonNull<u8>>) -> Option<NonNull<u8>> = nullable;
    let n: extern "C" fn(NonZeroU32) -> NonZeroU32 = integer;
    let m: fn(ManuallyDrop<u64>) -> ManuallyDrop<u64> = manual;
    // SAFETY: these are the documented field and Option ABIs. Only nullable
    // receives null, and the argument to NonZeroU32 is nonzero.
    unsafe {
        let p: extern "C" fn(*mut u8) -> *mut u8 = transmute(p);
        let o: extern "C-unwind" fn(*mut u8) -> *mut u8 = transmute(o);
        let n: extern "C" fn(u32) -> u32 = transmute(n);
        let m: fn(u64) -> u64 = transmute(m);
        let mut byte = 19u8;
        let ptr = &raw mut byte;
        assert_eq!(o(p(ptr)), ptr);
        assert!(o(core::ptr::null_mut()).is_null());
        assert_eq!(n(0xfedcba98), 0xfedcba98);
        assert_eq!(m(0xfedcba9876543210), 0xfedcba9876543210);
    }
}
"#;
    let said = blaze_test("test_command_core_transparent", source);
    if let Err(why) = judge(&said, false) {
        panic!("{}", evidence(&why, &said));
    }
}

/// Linux test sandbox (user/net/ipc/mount namespaces + read-only `/` + Landlock ABI >= 6 + seccomp, `test.md` piece 9):
/// - writes, `chmod`, pathname `AF_UNIX`, and loopback TCP inside `$TMPDIR`/private netns succeed;
/// - writes and `chmod` outside `$TMPDIR` are refused (`ReadOnlyFilesystem`/`PermissionDenied`);
/// - external TCP (`1.1.1.1:80`) is refused (`NetworkUnreachable`) and host `/tmp` UNIX sockets are masked (`NotFound`);
/// - capabilities are dropped (`CapEff == 0`), `RLIMIT_CORE == 0`, and spawning a helper works;
/// - `#[should_panic(expected = ...)]` succeeds via the launcher-owned `fd:4` channel (no guest-
///   writable unwind file in `$TMPDIR/..`), while `std::process::exit(101)` is NOT classified as a panic;
/// - helpers spawned inside a test are killed when the test leader exits.
#[test]
#[cfg(target_os = "linux")]
fn linux_sandbox_confines_writes_and_distinguishes_exit_101() {
    use std::os::unix::net::UnixListener;

    // Bind a real host pathname UNIX socket in `/tmp` outside the test's `$TMPDIR`: the sandbox's
    // empty `/tmp` tmpfs mask must make it completely unreachable from inside the test.
    let host_sock_dir = std::env::temp_dir().join(format!("blaze_host_sock_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&host_sock_dir);
    std::fs::create_dir_all(&host_sock_dir).expect("create host_sock_dir");
    let host_sock_path = host_sock_dir.join("daemon.sock");
    let _host_listener = UnixListener::bind(&host_sock_path).expect("bind host daemon socket");

    let source = format!(r##"
#[test]
fn tmpdir_writes_chmod_unix_and_loopback_work() {{
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{{UnixListener, UnixStream}};
    use std::net::{{TcpListener, TcpStream}};

    let tmp = std::env::temp_dir();
    let p = tmp.join("ok.sh");
    std::fs::write(&p, b"#!/bin/sh\n").expect("write inside TMPDIR");
    assert_eq!(std::fs::read(&p).expect("read back"), b"#!/bin/sh\n");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
        .expect("chmod inside TMPDIR must succeed");

    // Sandbox-local pathname AF_UNIX bind + connect inside TMPDIR (short path under SUN_LEN).
    let sock = tmp.join("s.sock");
    if sock.as_os_str().len() < 100 {{
        let srv = UnixListener::bind(&sock).expect("pathname AF_UNIX bind inside TMPDIR");
        let _cli = UnixStream::connect(&sock).expect("pathname AF_UNIX connect inside TMPDIR");
        let _ = srv.accept().expect("accept local AF_UNIX");
    }}
    let (_a, _b) = UnixStream::pair().expect("anonymous AF_UNIX socketpair allowed");

    // Sandbox-local loopback TCP works inside the private netns.
    let tcp = TcpListener::bind("127.0.0.1:0").expect("loopback TCP bind inside private netns");
    let addr = tcp.local_addr().expect("loopback local_addr");
    let _conn = TcpStream::connect(addr).expect("loopback TCP connect inside private netns");
    let _ = tcp.accept().expect("loopback TCP accept");

    let status = std::process::Command::new("/bin/true").status().expect("spawn helper");
    assert!(status.success());
}}

#[test]
fn outside_writes_chmod_external_net_host_sockets_caps_and_core_are_confined() {{
    use std::os::unix::fs::PermissionsExt;
    let cwd = std::env::current_dir().expect("current dir");
    let escaped = cwd.join("escaped.txt");
    let err = std::fs::write(&escaped, b"nope").expect_err("creating file outside TMPDIR must be denied");
    assert!(
        matches!(err.kind(), std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied),
        "expected EROFS/EACCES creating file outside TMPDIR, got {{err:?}}"
    );
    let err2 = std::fs::OpenOptions::new()
        .write(true)
        .open(cwd.join("Cargo.toml"))
        .expect_err("opening existing file for write outside TMPDIR must be denied");
    assert!(
        matches!(err2.kind(), std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied),
        "expected EROFS/EACCES writing Cargo.toml, got {{err2:?}}"
    );
    let err_chmod = std::fs::set_permissions(cwd.join("Cargo.toml"), std::fs::Permissions::from_mode(0o600))
        .expect_err("chmod outside TMPDIR must be denied by read-only mount");
    assert!(
        matches!(err_chmod.kind(), std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied),
        "expected EROFS/EPERM on outside chmod, got {{err_chmod:?}}"
    );
    // External non-loopback network is unreachable in the private netns.
    let err_ext = std::net::TcpStream::connect("1.1.1.1:80").expect_err("external TCP must fail");
    assert!(
        matches!(err_ext.kind(), std::io::ErrorKind::NetworkUnreachable | std::io::ErrorKind::PermissionDenied),
        "expected ENETUNREACH/EPERM on external TCP, got {{err_ext:?}}"
    );
    // Host pathname UNIX socket in /tmp is invisible behind the empty /tmp tmpfs mask.
    let err_unix = std::os::unix::net::UnixStream::connect("{host_sock}")
        .expect_err("host pathname AF_UNIX socket must be unreachable");
    assert!(
        matches!(err_unix.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied),
        "expected ENOENT/EACCES on host pathname AF_UNIX socket, got {{err_unix:?}}"
    );
    // All capabilities dropped (`CapEff: 0000000000000000`) and core dumps disabled (`Max core file size 0 0`).
    let status_txt = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    assert!(status_txt.contains("CapEff:\t0000000000000000"), "expected CapEff == 0:\n{{status_txt}}");
    assert!(status_txt.contains("CapPrm:\t0000000000000000"), "expected CapPrm == 0:\n{{status_txt}}");
    let limits_txt = std::fs::read_to_string("/proc/self/limits").expect("read /proc/self/limits");
    let core_line = limits_txt.lines().find(|l| l.starts_with("Max core file size")).expect("Max core file size line");
    let cols: Vec<&str> = core_line["Max core file size".len()..].split_whitespace().collect();
    assert_eq!(&cols[..2], &["0", "0"], "expected RLIMIT_CORE == (0, 0), got {{core_line}}");

    // No guest-writable unwind marker exists beside TMPDIR.
    let parent_entries: Vec<_> = std::fs::read_dir(std::env::temp_dir().parent().unwrap())
        .expect("read parent")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("unwound-"))
        .collect();
    assert!(parent_entries.is_empty(), "expected no filesystem unwind marker beside TMPDIR, found {{parent_entries:?}}");
}}

#[test]
#[should_panic(expected = "expected boom")]
fn should_panic_works_with_unwound_channel() {{
    panic!("expected boom");
}}

#[test]
fn exit_101_without_unwind_is_not_panic() {{
    // Also spawn a background sleeper before exiting: the runner must kill the child's process
    // group upon leader exit so no helper outlives the test.
    let child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep helper");
    eprintln!("HELPER_PID={{}}", child.id());
    std::process::exit(101);
}}
"##, host_sock = host_sock_path.display());

    let said = blaze_test("test_command_runs_linux_sandbox", &source);
    let _ = std::fs::remove_dir_all(&host_sock_dir);
    assert_eq!(said.code, Some(1), "{}", evidence("only exit_101 test should fail", &said));
    assert!(
        said.stdout.contains("FAILED exit_101_without_unwind_is_not_panic")
            && !said.stdout.contains("FAILED exit_101_without_unwind_is_not_panic: panicked")
            && said.stdout.contains("3 passed, 1 failed"),
        "{}",
        evidence("3 sandbox/should_panic tests pass; exit(101) fails without being classified as panicked", &said)
    );
    let helper_pid: u32 = said
        .stdout
        .lines()
        .chain(said.stderr.lines())
        .find_map(|l| l.trim().strip_prefix("HELPER_PID=").and_then(|s| s.parse().ok()))
        .expect("expected HELPER_PID=<pid> in captured failure output");
    let mut alive = true;
    for _ in 0..50 {
        let stat_path = format!("/proc/{helper_pid}/stat");
        match std::fs::read_to_string(&stat_path) {
            Err(_) => {
                alive = false;
                break;
            }
            Ok(s) if s.contains(") Z ") => {
                alive = false;
                break;
            }
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    assert!(!alive, "spawned helper pid {helper_pid} outlived its finished test process!");
}

// Deliberately put beta first in the manifest. The command orders results by owner and path.
const WORKSPACE: &str = "[workspace]\nmembers = [\"beta\", \"alpha\"]\nresolver = \"2\"\n";
const ALPHA: &str = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const BETA: &str = "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const SHARED: &str = "mod shared { #[test] fn same() {} }\n";
const LOCK_ALPHA: &str = "version = 4\n\n[[package]]\nname = \"alpha\"\nversion = \"0.1.0\"\n";
const LOCK_BETA: &str = "\n[[package]]\nname = \"beta\"\nversion = \"0.1.0\"\n";

const INVENTORY: &str = concat!(
    "alpha/lib/shared::same: test\n",
    "alpha/test:devcase/uses_dev_dependency: test\n",
    "alpha/test:empty: 0 tests\n",
    "alpha/test:own/main: test\n",
    "beta/lib/shared::same: test\n",
    "4 tests, 0 benchmarks\n",
);

fn listed_each_unit(stdout: &str, code: Option<i32>) -> bool {
    code == Some(0) && stdout == INVENTORY
}

#[test]
fn lists_each_selected_unit_once() {
    let targets = format!(
        "{ALPHA}\n[dev-dependencies]\nbeta = {{ path = \"../beta\" }}\n\n\
         [[test]]\nname = \"own\"\npath = \"tests/own.rs\"\nharness = false\n"
    );
    let dependency = format!("pub fn helper() {{}}\n{SHARED}");
    let lock = format!("{LOCK_ALPHA}dependencies = [\"beta\"]\n{LOCK_BETA}");
    let files = [
        ("Cargo.toml", WORKSPACE),
        ("Cargo.lock", lock.as_str()),
        ("alpha/Cargo.toml", targets.as_str()),
        ("alpha/src/lib.rs", SHARED),
        ("alpha/tests/devcase.rs", "#[test] fn uses_dev_dependency() { beta::helper(); }\n"),
        ("alpha/tests/empty.rs", ""),
        ("alpha/tests/own.rs", "fn main() {}\n#[test] fn hidden_by_harness_false() {}\n"),
        ("beta/Cargo.toml", BETA),
        ("beta/src/lib.rs", dependency.as_str()),
    ];
    fixture("test_command_unit_inventory", &files, |dir, home| {
        let all = run(dir, home, &["--", "--list"]);
        assert!(listed_each_unit(&all.stdout, all.code), "{}", evidence(INVENTORY, &all));

        let repeated = run(dir, home, &["-p", "beta", "--package=alpha", "--package", "beta", "--", "--list"]);
        assert!(listed_each_unit(&repeated.stdout, repeated.code), "{}", evidence(INVENTORY, &repeated));

        let selected = run(dir, home, &["-p", "beta", "--", "--list", "shared::same", "--exact"]);
        let one = "beta/lib/shared::same: test\n1 tests, 0 benchmarks\n";
        assert_eq!(selected.code, Some(0), "{}", evidence(one, &selected));
        assert_eq!(selected.stdout, one, "{}", evidence(one, &selected));

        // The package and target labels identify a result, but libtest matches a filter
        // against its Rust path alone. "beta" is in the label and absent from shared::same.
        let filtered = run(dir, home, &["-p", "beta", "--", "--list", "beta"]);
        assert_eq!(filtered.code, Some(0), "{}", evidence("a filter with no matches", &filtered));
        assert_eq!(filtered.stdout, "0 tests, 0 benchmarks\n");
    });
}

#[test]
fn the_inventory_oracle_catches_one_unit_and_display_path_identity() {
    assert!(listed_each_unit(INVENTORY, Some(0)));
    let one_unit = "alpha/lib/shared::same: test\n1 tests, 0 benchmarks\n";
    assert!(!listed_each_unit(one_unit, Some(0)));
    let by_display_path = INVENTORY.replace("beta/lib/shared::same: test\n", "").replace("4 tests", "3 tests");
    assert!(!listed_each_unit(&by_display_path, Some(0)));
}

const REFUSED: &str = "a `ref` binding or a binding with a subpattern";
const REFUSED_SUMMARY: &str = "test result: FAILED. ran 1: 1 passed, 0 failed, 1 not run (code not built)";

fn only_dependent_test_refused(stdout: &str, code: Option<i32>) -> bool {
    let mut lines = stdout.lines();
    let Some(refusal) = lines.next() else { return false };
    let Some(instance_and_cause) = refusal.strip_prefix("NOT RUN a_needs_callee: ") else { return false };
    instance_and_cause.contains("refused_callee")
        && instance_and_cause.contains(REFUSED)
        && lines.next().is_some_and(|summary| summary.starts_with(REFUSED_SUMMARY))
        && lines.next().is_none()
        && code.is_some_and(|status| status != 0)
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn refused_first_call_does_not_hide_the_next_test() {
    // A `let` binding with an `@` subpattern is refused in walk.rs `plain_binding` with
    // `REFUSED`. Keep it in the *callee* so the launcher first enters a successfully built test.
    let lib = "fn refused_callee() { let _word @ _ = 7u8; }\n\
               #[test] fn a_needs_callee() { refused_callee(); }\n\
               #[test] fn b_returns() {}\n";
    let said = fixture("test_command_refused_call", &[("Cargo.toml", MANIFEST), ("src/lib.rs", lib)], |dir, home| {
        run(dir, home, &["--", "--test-threads", "1", "--deadline", "20s"])
    });
    assert!(only_dependent_test_refused(&said.stdout, said.code), "{}", evidence(REFUSED_SUMMARY, &said));
}

#[test]
fn the_refusal_oracle_catches_one_stopped_and_not_run_as_pass() {
    let refusal = format!("NOT RUN a_needs_callee: refused_callee: {REFUSED}");
    let correct = format!("{refusal}\n{REFUSED_SUMMARY}\n");
    assert!(only_dependent_test_refused(&correct, Some(1)));
    let one_stopped = format!("{refusal}\n");
    assert!(!only_dependent_test_refused(&one_stopped, Some(1)));
    let not_run_as_pass = format!("{refusal}\ntest result: ok. ran 2: 2 passed, 0 failed\n");
    assert!(!only_dependent_test_refused(&not_run_as_pass, Some(0)));
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn runs_both_workspace_packages() {
    let lock = format!("{LOCK_ALPHA}{LOCK_BETA}");
    let manifest = format!("{WORKSPACE}\n[profile.dev]\npanic = \"abort\"\n");
    // Cargo ignores the profile panic setting for a test build. Both unit graphs must
    // select the unwind-only module. This checks cfg selection independently of code keys.
    let lib = format!("#[cfg(panic = \"unwind\")]\n{SHARED}");
    let files = [
        ("Cargo.toml", manifest.as_str()),
        ("Cargo.lock", lock.as_str()),
        ("alpha/Cargo.toml", ALPHA),
        ("alpha/src/lib.rs", lib.as_str()),
        ("beta/Cargo.toml", BETA),
        ("beta/src/lib.rs", lib.as_str()),
    ];
    let said = fixture("test_command_two_packages", &files, |dir, home| {
        run(dir, home, &["--", "--test-threads", "2", "--deadline", "20s"])
    });
    let expected = [
        "test result: ok. ran 2: 2 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky",
    ];
    assert_eq!(said.code, Some(0), "{}", evidence("two passing packages", &said));
    assert!(said.stdout.lines().eq(expected), "{}", evidence(&expected.join("\n"), &said));
}

/// `alpha` depends on `beta`, under `[profile.dev] panic = "abort"`: alpha's test program would
/// link beta's lib, which the unit graph builds once, under the profile's abort, for the bin's
/// graph and the test's alike. Cargo builds a test program's dependencies to unwind (the Cargo
/// Book, Profiles, `panic`), which blaze does not yet (engine_start.rs `abort_dependency`), so
/// `blaze test` stops up front naming the setting, the test and the first such dependency, with
/// status 125 (main.rs STATUSES: "blaze did not start the program"). CONTROLS (rule 23): the same
/// workspace without the setting is not refused; `runs_both_workspace_packages` (abort, no
/// dependency) runs its 2 tests.
#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn tests_over_a_dependency_built_to_abort_stop_up_front() {
    const ALPHA_ON_BETA: &str = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nbeta = { path = \"../beta\" }\n";
    const LOCK: &str = "version = 4\n\n[[package]]\nname = \"alpha\"\nversion = \"0.1.0\"\ndependencies = [\n \"beta\",\n]\n\n[[package]]\nname = \"beta\"\nversion = \"0.1.0\"\n";
    let said = |abort: bool| {
        let manifest = if abort { format!("{WORKSPACE}\n[profile.dev]\npanic = \"abort\"\n") } else { WORKSPACE.to_owned() };
        let files = [
            ("Cargo.toml", manifest.as_str()),
            ("Cargo.lock", LOCK),
            ("alpha/Cargo.toml", ALPHA_ON_BETA),
            ("alpha/src/lib.rs", "pub fn a() -> u8 {\n    beta::b()\n}\n\n#[test]\nfn calls() {\n    assert_eq!(a(), 1);\n}\n"),
            ("beta/Cargo.toml", BETA),
            ("beta/src/lib.rs", "pub fn b() -> u8 {\n    1\n}\n"),
        ];
        let name = if abort { "test_command_abort_dependency" } else { "test_command_abort_dependency_control" };
        fixture(name, &files, |dir, home| run(dir, home, &["--", "--deadline", "20s"]))
    };
    let refused = said(true);
    let words = format!("{}{}", refused.stdout, refused.stderr);
    assert_eq!(refused.code, Some(125), "{}", evidence("blaze did not start the tests (125)", &refused));
    assert!(
        words.contains("tests of a profile with panic = \"abort\" need their dependencies built to unwind: not built yet")
            && words.contains("the tests of `alpha 0.1.0` link `beta 0.1.0`"),
        "{}",
        evidence("the refusal names the setting, the test and its first dependency", &refused)
    );
    let control = said(false);
    let control_words = format!("{}{}", control.stdout, control.stderr);
    assert!(!control_words.contains("need their dependencies built to unwind"), "{}", evidence("CONTROL: an unwinding profile is not refused", &control));
}

/// The control: what a driver planted to say "every test passes" prints for the package with
/// `fails`, which the check must refuse (and does by its summary and its status).
#[test]
fn every_test_passes_is_caught() {
    let planted = Said {
        stdout: "test result: ok. ran 2: 2 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n".into(),
        stderr: String::new(),
        code: Some(0),
    };
    assert!(judge(&planted, true).is_err(), "the check passed a run in which `fails` passed");
    // And one planted to keep the right summary but end with status 0 is refused too.
    let rule = "-".repeat(80);
    let quiet = Said {
        stdout: format!(
            "FAILED fails after 0.012 s: panicked: assertion `left == right` failed\n---- fails output: 79 bytes ----\nthread 'main' (1) panicked at src/lib.rs:6:5:\nassertion `left == right` failed\n{rule}\nFAILED fails: panicked\n{BOTH_SUMMARY}\n"
        ),
        stderr: String::new(),
        code: Some(0),
    };
    assert!(judge(&quiet, true).is_err(), "the check passed a failing suite that ended with status 0");
    // The same transcript with a failing status is the expected run: the check is not refusing everything.
    let right = Said { code: Some(1), ..quiet };
    assert_eq!(judge(&right, true), Ok(()));
    let shifted = Said { stdout: right.stdout.replace("src/lib.rs:6:5", "src/lib.rs:5:5"), ..right };
    assert!(judge(&shifted, true).is_err(), "the check passed a panic at the enclosing fn instead of its assertion");
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn incremental_reruns_only_changed_test_and_reuses_unchanged() {
    let make_lib = |a_expr: &str, b_expr: &str| {
        format!(
            "fn helper_a() -> u32 {{ {a_expr} }}\n\
             fn helper_b() -> u32 {{ {b_expr} }}\n\
             #[test]\nfn test_a() {{ assert_eq!(helper_a(), 42); }}\n\
             #[test]\nfn test_b() {{ assert_eq!(helper_b(), 100); }}\n"
        )
    };
    let initial = make_lib("40 + 2", "100");
    fixture("test_command_incremental_selective", &[("Cargo.toml", MANIFEST), ("src/lib.rs", &initial)], |dir, home| {
        let lib_path = dir.join("src/lib.rs");

        // 1. Cold run: both tests run and both results are kept.
        let cold = run(dir, home, &["--", "--deadline", "20s"]);
        let expect_cold = "test result: ok. ran 2: 2 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        assert_eq!(cold.code, Some(0), "{}", evidence("cold run passes", &cold));
        assert_eq!(cold.stdout, expect_cold, "{}", evidence(expect_cold, &cold));

        // 2. No-op rerun: both tests reused from kept results (ran 0).
        let warm = run(dir, home, &["--", "--deadline", "20s"]);
        let expect_warm = "test result: ok. ran 0: 0 passed, 0 failed; 2 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        assert_eq!(warm.code, Some(0), "{}", evidence("warm rerun passes", &warm));
        assert_eq!(warm.stdout, expect_warm, "{}", evidence(expect_warm, &warm));

        // 3. Edit helper_a only: only test_a reruns; test_b is reused from kept results.
        let edit_1 = make_lib("21 * 2", "100");
        write_source(&lib_path, &edit_1, "edit helper_a");
        let edited = run(dir, home, &["--", "--deadline", "20s"]);
        let expect_edited = "test result: ok. ran 1: 1 passed, 0 failed; 1 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        assert_eq!(edited.code, Some(0), "{}", evidence("only test_a reruns after helper_a edit", &edited));
        assert_eq!(edited.stdout, expect_edited, "{}", evidence(expect_edited, &edited));

        // 4. Edit helper_a to a second value, then revert back to edit_1: Effect::Look finds the
        //    previously kept pass at edit_1's address without re-running test_a (ran 0).
        write_source(&lib_path, &make_lib("41 + 1", "100"), "edit helper_a again");
        let edited2 = run(dir, home, &["--", "--deadline", "20s"]);
        assert_eq!(edited2.code, Some(0), "{}", evidence("second helper_a edit reruns only test_a", &edited2));
        assert_eq!(edited2.stdout, expect_edited, "{}", evidence(expect_edited, &edited2));

        write_source(&lib_path, &edit_1, "revert helper_a to edit_1");
        let reverted = run(dir, home, &["--", "--deadline", "20s"]);
        assert_eq!(reverted.code, Some(0), "{}", evidence("reverted edit reuses kept result", &reverted));
        assert_eq!(reverted.stdout, expect_warm, "{}", evidence(expect_warm, &reverted));

        // 5. --run-all bypasses kept results and runs both tests.
        let forced = run(dir, home, &["--", "--run-all", "--deadline", "20s"]);
        assert_eq!(forced.code, Some(0), "{}", evidence("--run-all passes", &forced));
        assert_eq!(forced.stdout, expect_cold, "{}", evidence(expect_cold, &forced));

        // 6. Soundness control: break helper_a -> only test_a runs and FAILS, test_b stays reused.
        write_source(&lib_path, &make_lib("99", "100"), "break helper_a");
        let broken = run(dir, home, &["--", "--deadline", "20s"]);
        let expect_broken_summary = "test result: FAILED. ran 1: 0 passed, 1 failed; 1 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";
        assert_eq!(broken.code, Some(1), "{}", evidence("broken helper_a must fail", &broken));
        assert!(
            broken.stdout.contains("FAILED test_a")
                && !broken.stdout.contains("test_b")
                && broken.stdout.lines().last() == Some(expect_broken_summary),
            "{}",
            evidence(expect_broken_summary, &broken),
        );
    });
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn impure_test_is_not_kept_and_reruns() {
    let source = "#[test]\nfn reads_clock() {\n    let _ = std::time::SystemTime::now();\n}\n";
    fixture("test_command_impure_clock", &[("Cargo.toml", MANIFEST), ("src/lib.rs", source)], |dir, home| {
        let check = |label: &str, said: &Said| {
            assert_eq!(said.code, Some(0), "{}", evidence(label, said));
            let mut lines = said.stdout.lines();
            let first = lines.next().unwrap_or("");
            let second = lines.next().unwrap_or("");
            assert!(
                first.starts_with("not kept reads_clock: ")
                    && second.starts_with("test result: ok. ran 1: 1 passed, 0 failed; 0 reused from kept results; 1 not kept (")
                    && lines.next().is_none(),
                "{}",
                evidence(label, said),
            );
        };
        let first = run(dir, home, &["--", "--deadline", "20s"]);
        check("first impure run is not kept", &first);

        // Second invocation must NOT reuse the clock-reading test.
        let second = run(dir, home, &["--", "--deadline", "20s"]);
        check("second impure run reruns instead of reusing", &second);
    });
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn termination_returning_tests_pass_fail_and_cache() {
    let source = r#"
use std::process::ExitCode;

fn answer() -> u32 {
    42
}

#[test]
fn plain_unit() {
    assert_eq!(answer(), 42);
}

#[test]
fn result_str_ok() -> Result<(), &'static str> {
    if answer() == 42 { Ok(()) } else { Err("wrong") }
}

#[test]
fn result_string_ok() -> Result<(), String> {
    if answer() == 42 { Ok(()) } else { Err(String::from("wrong")) }
}

#[test]
fn result_string_err() -> Result<(), String> {
    Err(String::from("termination boom"))
}

#[test]
fn exit_code_ok() -> ExitCode {
    ExitCode::SUCCESS
}

#[test]
fn exit_code_42() -> ExitCode {
    ExitCode::from(42)
}
"#;
    fixture("test_command_termination_shim", &[("Cargo.toml", MANIFEST), ("src/lib.rs", source)], |dir, home| {
        // 1. Cold run: 4 pass (plain_unit, result_str_ok, result_string_ok, exit_code_ok),
        //    2 fail cleanly (result_string_err with code 1 + "termination boom", exit_code_42 with code 42),
        //    ZERO `NOT RUN`!
        let cold = run(dir, home, &["--", "--deadline", "20s"]);
        assert_eq!(cold.code, Some(1), "{}", evidence("failing termination tests fail the suite", &cold));
        assert!(!cold.stdout.contains("NOT RUN"), "{}", evidence("no NOT RUN for Termination tests", &cold));
        assert!(cold.stdout.contains("FAILED result_string_err"), "{}", evidence("result_string_err failed", &cold));
        assert!(cold.stdout.contains("termination boom"), "{}", evidence("Error message from Result::report printed", &cold));
        assert!(cold.stdout.contains("FAILED exit_code_42") && cold.stdout.contains("code 42"), "{}", evidence("exit_code_42 failed with code 42", &cold));
        let expect_summary = "test result: FAILED. ran 6: 4 passed, 2 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";
        assert_eq!(cold.stdout.lines().last(), Some(expect_summary), "{}", evidence(expect_summary, &cold));

        // 2. Warm rerun of only the 4 passing tests: all 4 reused from kept results (`ran 0`, `4 reused`).
        let warm = run(dir, home, &["--", "--skip", "err", "--skip", "42", "--deadline", "20s"]);
        let expect_warm = "test result: ok. ran 0: 0 passed, 0 failed; 4 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 2 filtered out; 0 flaky\n";
        assert_eq!(warm.code, Some(0), "{}", evidence("warm passing termination tests reused", &warm));
        assert_eq!(warm.stdout, expect_warm, "{}", evidence(expect_warm, &warm));
    });
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn scoped_test_checks_only_selected_member_and_its_deps() {
    let files: &[(&str, &str)] = &[
        ("Cargo.toml", "[workspace]\nmembers = [\"dep\", \"a\", \"b\"]\nresolver = \"2\"\n"),
        ("dep/Cargo.toml", "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        (
            "dep/src/lib.rs",
            "#[cfg(test)]\nmod tests;\n\npub fn answer() -> u32 {\n    42\n}\n\n#[cfg(test)]\n#[test]\nfn dep_inline_test() {\n    assert_eq!(answer(), 42);\n}\n",
        ),
        ("dep/src/tests.rs", "#[test]\nfn dep_submod_test() {\n    assert_eq!(super::answer(), 42);\n}\n"),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        ),
        ("a/src/lib.rs", "#[test]\nfn a_uses_dep() {\n    assert_eq!(dep::answer(), 42);\n}\n"),
        ("b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        ("b/src/lib.rs", "pub fn broken() -> u8 {\n    \"not a number\"\n}\n\n#[test]\nfn b_test() {}\n"),
    ];
    fixture("test_command_scoped_member", files, |dir, home| {
        // 1. Member directory: `blaze test a` checks only `a` + `dep` (never broken sibling `b`),
        //    has 0 errors (even though `dep` has both an inline `#[cfg(test)]` fn and a
        //    `#[cfg(test)] mod tests;` file), and runs and passes `a`'s test.
        let by_dir = run(dir, home, &["a", "--", "--deadline", "20s"]);
        let expect_cold = "test result: ok. ran 1: 1 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        assert_eq!(by_dir.code, Some(0), "{}", evidence("blaze test a passes without checking b", &by_dir));
        assert_eq!(by_dir.stdout, expect_cold, "{}", evidence(expect_cold, &by_dir));
        assert!(!by_dir.stderr.contains("b/src/lib.rs"), "{}", evidence("b must not be checked by `blaze test a`", &by_dir));

        // 2. `-p a` selects the same package scope: hits the scoped check certificate and reuses
        //    `a`'s kept test result without checking `b`.
        let by_pkg = run(dir, home, &["-p", "a", "--", "--deadline", "20s"]);
        let expect_warm = "test result: ok. ran 0: 0 passed, 0 failed; 1 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        assert_eq!(by_pkg.code, Some(0), "{}", evidence("blaze test -p a passes without checking b", &by_pkg));
        assert_eq!(by_pkg.stdout, expect_warm, "{}", evidence(expect_warm, &by_pkg));
        assert!(!by_pkg.stderr.contains("b/src/lib.rs"), "{}", evidence("b must not be checked by `blaze test -p a`", &by_pkg));

        // 3. Soundness control: whole-workspace `blaze check` was NOT poisoned by the scoped runs
        //    and still reports `b`'s type error.
        let whole_check = run_cmd(dir, home, "check", &[]);
        assert_ne!(whole_check.code, Some(0), "{}", evidence("whole-workspace `blaze check` must report b's error", &whole_check));
        assert!(
            whole_check.stdout.contains("b/src/lib.rs") || whole_check.stderr.contains("b/src/lib.rs"),
            "{}",
            evidence("whole-workspace `blaze check` mentions b/src/lib.rs", &whole_check),
        );

        // 4. Whole-workspace `blaze test` (no member / no -p) also still checks `b` and reports its error.
        let whole_test = run_cmd(dir, home, "test", &["--", "--list"]);
        assert!(
            whole_test.stdout.contains("b/src/lib.rs") || whole_test.stderr.contains("b/src/lib.rs"),
            "{}",
            evidence("whole-workspace `blaze test` still checks b/src/lib.rs", &whole_test),
        );

        // 5. Re-running `blaze test a` after whole-workspace `blaze check` still stays clean.
        let after_whole = run(dir, home, &["a", "--", "--deadline", "20s"]);
        assert_eq!(after_whole.code, Some(0), "{}", evidence("blaze test a stays clean after whole check", &after_whole));
        assert_eq!(after_whole.stdout, expect_warm, "{}", evidence(expect_warm, &after_whole));
    });
}

/// Scans CAS segment files under `store_root` for `BZTH` suite head records (zeroing only the
/// per-row 8-byte `micros` wall-time field so every `TestId`, outcome tag, `NotKept` reason,
/// 32-byte `BZTR` address, asks table, and read table are compared byte-for-byte) and counts
/// kept `BZTR` pass records (`kept == 0`).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn collect_bzth_and_bztr(store_root: &Path) -> (blaze_own::AVec<'static, blaze_own::AVec<'static, u8>>, usize) {
    let alloc = blaze_own::testing::heap().alloc();
    let mut heads = blaze_own::AVec::new_in(alloc);
    let mut bztr_kept = blaze_own::AVec::new_in(alloc);
    let listed = Command::new("/usr/bin/find").arg(store_root).args(["-type", "f"]).output().expect("list store files");
    assert!(listed.status.success(), "find store files failed");
    let paths = core::str::from_utf8(&listed.stdout).expect("utf8 store paths");
    for file in paths.lines() {
        let raw = read_file_bytes(Path::new(file), file);
        let bytes = raw.as_ref();
        let mut i = 0usize;
        while i + 24 <= bytes.len() {
            if &bytes[i..i + 4] == b"BZTH" {
                let ver = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap());
                let rows = u32::from_le_bytes(bytes[i + 8..i + 12].try_into().unwrap()) as usize;
                let asks = u32::from_le_bytes(bytes[i + 12..i + 16].try_into().unwrap()) as usize;
                let reads = u32::from_le_bytes(bytes[i + 16..i + 20].try_into().unwrap()) as usize;
                let pad = u32::from_le_bytes(bytes[i + 20..i + 24].try_into().unwrap());
                if ver == 1 && pad == 0 && rows <= 1024 && asks <= 65536 && reads <= 65536 {
                    let total = 24 + rows * 88 + asks * 40 + reads * 64;
                    if i + total <= bytes.len() {
                        let mut rec = blaze_own::AVec::with_capacity_in(total, alloc);
                        rec.extend_from_slice(&bytes[i..i + total]);
                        for r in 0..rows {
                            // Each 88-byte row stores `micros: u64` at offset 72..80.
                            let off = 24 + r * 88 + 72;
                            rec[off..off + 8].fill(0);
                        }
                        heads.push(rec);
                        i += total;
                        continue;
                    }
                }
            } else if i + 64 <= bytes.len()
                && &bytes[i..i + 4] == b"BZTR"
                && bytes[i + 4] == 1
                && bytes[i + 5] == 0
                && bytes[i + 6..i + 16] == [0u8; 10]
                && bytes[i + 56..i + 64] == [0u8; 8]
            {
                let mut rec = [0u8; 64];
                rec.copy_from_slice(&bytes[i..i + 64]);
                bztr_kept.push(rec);
                i += 64;
                continue;
            }
            i += 1;
        }
    }
    // Background GC compaction may copy records into a newer segment while the superseded
    // segment is still within its retention grace window; deduplicate identical records.
    heads.sort_by(|a, b| a.as_slice().cmp(b.as_slice()));
    heads.dedup_by(|a, b| a.as_slice() == b.as_slice());
    bztr_kept.sort();
    bztr_kept.dedup();
    (heads, bztr_kept.len())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn copy_dir_files(src: &Path, dst: &Path) {
    let status = Command::new("/bin/cp").arg("-R").arg(src).arg(dst).status().expect("cp -R store");
    assert!(status.success(), "cp -R store failed");
}

/// Regression for parallel `Codes::Pooled` + `SuiteCache` + lock-free `SyncTypedFrom`:
/// - compiles cold workspace functions, transitive statics (`BASE -> TABLE`), and body-local items
///   across 4 parallel code workers via `FromRecord` (`SnapshotTyped`);
/// - preserves pure vs impure (`NotKept::Clock`) classification and warm cache reuse;
/// - produces byte-for-byte identical `BZTR` test-result records as `--workers 1` (`Codes::Serial`),
///   and cross-reuses kept results in both directions (`Pooled -> Serial` and `Serial -> Pooled`).
#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn pooled_workers_match_serial_bztr_and_purity() {
    let lib = r#"
static BASE: u64 = 17;
static TABLE: [&u64; 2] = [&BASE, &BASE];

#[inline(never)]
fn shared_compute(x: u64) -> u64 {
    fn local_step(v: u64) -> u64 {
        v.wrapping_mul(3).wrapping_add(1)
    }
    let p: *const u64 = TABLE[0];
    assert!(!p.is_null());
    local_step(x + *TABLE[0] + *TABLE[1])
}

#[test]
fn pure_one() {
    assert_eq!(shared_compute(1), 106);
}

#[test]
fn pure_two() {
    assert_eq!(shared_compute(2), 109);
}

#[test]
fn pure_three() {
    assert_eq!(shared_compute(3), 112);
}

#[test]
fn pure_four() {
    assert_eq!(shared_compute(4), 115);
}

#[test]
fn impure_clock() {
    let t = std::time::Instant::now();
    assert_eq!(shared_compute(5), 118);
    let _ = t.elapsed();
}
"#;
    fixture("test_command_pooled_vs_serial", &[("Cargo.toml", MANIFEST), ("src/lib.rs", lib)], |dir, home| {
        let expect_cold = "test result: ok. ran 5: 5 passed, 0 failed; 0 reused from kept results; 1 not kept (1 read the real clock); 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        let expect_warm = "test result: ok. ran 1: 1 passed, 0 failed; 4 reused from kept results; 1 not kept (1 read the real clock); 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";

        // 0. Warm `blaze check` once via `--list` and snapshot the checked store (empty code store)
        //    so both `--workers 1` (`Codes::Serial`) and `--workers 4` (`Codes::Pooled`) compile
        //    cold code records over the exact same check records.
        let listed = run(dir, home, &["--", "--list"]);
        assert_eq!(listed.code, Some(0), "{}", evidence("warm check via --list", &listed));
        let store_dir = home.join(".blaze/v2/store");
        let checked_snapshot = home.join("store_checked_snapshot");
        copy_dir_files(&store_dir, &checked_snapshot);

        // 1. Cold serial (`--workers 1`).
        let serial_cold = run(dir, home, &["--workers", "1", "--", "--deadline", "20s"]);
        assert_eq!(serial_cold.code, Some(0), "{}", evidence("serial cold passes", &serial_cold));
        assert!(serial_cold.stdout.ends_with(expect_cold), "{}", evidence(expect_cold, &serial_cold));
        let (serial_heads, serial_passes) = collect_bzth_and_bztr(&store_dir);
        assert_eq!(serial_passes, 4, "serial run must keep exactly the 4 pure tests (got {serial_passes})");
        assert_eq!(serial_heads.len(), 1, "expected 1 BZTH head record in serial store");

        // 2. Cross-mode reuse (Serial -> Pooled): `--workers 4` reuses all 4 serial-kept passes.
        let pooled_on_serial = run(dir, home, &["--workers", "4", "--", "--deadline", "20s"]);
        assert_eq!(pooled_on_serial.code, Some(0), "{}", evidence("pooled reuses serial cache", &pooled_on_serial));
        assert!(pooled_on_serial.stdout.ends_with(expect_warm), "{}", evidence(expect_warm, &pooled_on_serial));

        // 3. Restore the warm-checked / empty-code-store snapshot and run cold pooled (`--workers 4`).
        let rm_ok = Command::new("/bin/rm").arg("-rf").arg(&store_dir).status().expect("rm -rf store");
        assert!(rm_ok.success(), "rm -rf store failed");
        copy_dir_files(&checked_snapshot, &store_dir);
        let pooled_cold = run(dir, home, &["--workers", "4", "--", "--deadline", "20s"]);
        assert_eq!(pooled_cold.code, Some(0), "{}", evidence("pooled cold passes", &pooled_cold));
        assert!(pooled_cold.stdout.ends_with(expect_cold), "{}", evidence(expect_cold, &pooled_cold));

        // 4. Rule 25 determinism: `BZTH` head records (every TestId, outcome, NotKept reason,
        //    32-byte `BZTR` address, asks table, and read table) and kept `BZTR` pass count must
        //    be byte-for-byte identical between `--workers 1` (`Codes::Serial`) and `--workers 4`
        //    (`Codes::Pooled`).
        let (pooled_heads, pooled_passes) = collect_bzth_and_bztr(&store_dir);
        assert_eq!(pooled_passes, 4, "pooled run must keep exactly the 4 pure tests (got {pooled_passes})");
        assert_eq!(
            serial_heads, pooled_heads,
            "BZTH head & BZTR addresses under --workers 4 (Codes::Pooled) must match --workers 1 (Codes::Serial) byte-for-byte"
        );

        // 5. Warm pooled (`--workers 4`): reuses all 4 pure tests, reruns only impure_clock.
        let pooled_warm = run(dir, home, &["--workers", "4", "--", "--deadline", "20s"]);
        assert_eq!(pooled_warm.code, Some(0), "{}", evidence("pooled warm passes", &pooled_warm));
        assert!(pooled_warm.stdout.ends_with(expect_warm), "{}", evidence(expect_warm, &pooled_warm));

        // 6. Cross-mode reuse (Pooled -> Serial): `--workers 1` reuses all 4 pooled-kept passes.
        let serial_on_pooled = run(dir, home, &["--workers", "1", "--", "--deadline", "20s"]);
        assert_eq!(serial_on_pooled.code, Some(0), "{}", evidence("serial reuses pooled cache", &serial_on_pooled));
        assert!(serial_on_pooled.stdout.ends_with(expect_warm), "{}", evidence(expect_warm, &serial_on_pooled));
    });
}

/// `CARGO_BIN_EXE_<name>` launcher stubs (`test.md` piece 17): integration tests can read, inspect,
/// shell-exec, and spawn workspace `[[bin]]` targets through BOTH compile-time
/// `env!("CARGO_BIN_EXE_helper")` (`/target/debug/bin/<pkg>/<bin>`) AND runtime
/// `std::env::var("CARGO_BIN_EXE_helper")`, including concurrent multi-child spawns, while unit
/// tests do NOT receive `CARGO_BIN_EXE_*`.
#[test]
fn integration_test_spawns_cargo_bin_exe_stub() {
    let manifest = "\
[package]
name = \"binapp\"
version = \"0.1.0\"
edition = \"2024\"

[[bin]]
name = \"helper\"
path = \"src/main.rs\"
";
    let lib_rs = "\
#[cfg(test)]
mod tests {
    #[test]
    fn unit_test_does_not_receive_cargo_bin_exe() {
        assert!(std::env::var_os(\"CARGO_BIN_EXE_helper\").is_none());
    }
}
";
    let main_rs = "\
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!(\"helper-ok:{}\", args.join(\",\"));
}
";
    let integ_rs = "\
use std::path::Path;
use std::process::Command;

#[test]
fn spawns_via_compile_time_and_runtime_env() {
    let compile_path = env!(\"CARGO_BIN_EXE_helper\");
    assert_eq!(compile_path, \"/target/debug/bin/binapp/helper\");
    assert!(Path::new(compile_path).exists(), \"advertised stub path must exist\");

    // Verify a real file is readable at the advertised compile-time path (not only exec/stat).
    let bytes = std::fs::read(compile_path).expect(\"std::fs::read on advertised CARGO_BIN_EXE path\");
    assert!(!bytes.is_empty(), \"stub executable file must be non-empty\");

    let runtime_path = std::env::var(\"CARGO_BIN_EXE_helper\").expect(\"runtime CARGO_BIN_EXE_helper\");
    assert!(Path::new(&runtime_path).exists(), \"runtime stub path must exist\");

    // Spawn two children concurrently (one via compile-time guest path, one via runtime stub path)
    // to exercise concurrent multi-connection serving on the binary stub Unix listener.
    let child_a = Command::new(compile_path)
        .args([\"alpha\", \"one\"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect(\"spawn compile-time CARGO_BIN_EXE_helper\");
    let child_b = Command::new(&runtime_path)
        .args([\"beta\", \"two\"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect(\"spawn runtime CARGO_BIN_EXE_helper\");

    let out_a = child_a.wait_with_output().expect(\"wait child_a\");
    let out_b = child_b.wait_with_output().expect(\"wait child_b\");
    assert!(out_a.status.success(), \"child_a failed: status={:?} stderr={}\", out_a.status, String::from_utf8_lossy(&out_a.stderr));
    assert!(out_b.status.success(), \"child_b failed: status={:?} stderr={}\", out_b.status, String::from_utf8_lossy(&out_b.stderr));
    assert_eq!(String::from_utf8_lossy(&out_a.stdout).trim(), \"helper-ok:alpha,one\");
    assert_eq!(String::from_utf8_lossy(&out_b.stdout).trim(), \"helper-ok:beta,two\");

    // Copying `CARGO_BIN_EXE_*` to an arbitrary temp path (with no adjacent sidecar file) must
    // remain directly runnable, matching Cargo's self-contained executable behavior.
    let copied = std::env::temp_dir().join(\"blaze_copied_helper_bin\");
    let _ = std::fs::remove_file(&copied);
    std::fs::copy(compile_path, &copied).expect(\"copy CARGO_BIN_EXE_helper to temp path\");
    let copy_out = Command::new(&copied)
        .args([\"copied\", \"ok\"])
        .output()
        .expect(\"spawn copied CARGO_BIN_EXE_helper\");
    let _ = std::fs::remove_file(&copied);
    assert!(copy_out.status.success(), \"copied helper failed: status={:?} stderr={}\", copy_out.status, String::from_utf8_lossy(&copy_out.stderr));
    assert_eq!(String::from_utf8_lossy(&copy_out.stdout).trim(), \"helper-ok:copied,ok\");

    // On Linux, `/target/debug/bin` is bind-mounted in the kernel namespace so even an external
    // `/bin/sh` child sees and executes the exact compile-time path.
    #[cfg(target_os = \"linux\")]
    {
        let sh_out = Command::new(\"/bin/sh\")
            .args([\"-c\", \"\\\"$1\\\" via shell\", \"sh\", compile_path])
            .output()
            .expect(\"spawn /bin/sh with compile-time CARGO_BIN_EXE_helper\");
        assert!(sh_out.status.success(), \"sh failed: status={:?} stderr={}\", sh_out.status, String::from_utf8_lossy(&sh_out.stderr));
        assert_eq!(String::from_utf8_lossy(&sh_out.stdout).trim(), \"helper-ok:via,shell\");
    }
}
";
    fixture(
        "bin_exe",
        &[
            ("Cargo.toml", manifest),
            ("src/lib.rs", lib_rs),
            ("src/main.rs", main_rs),
            ("tests/spawns_bin.rs", integ_rs),
        ],
        |dir, home| {
            let said = run(dir, home, &["--", "--deadline", "60s"]);
            assert_eq!(said.code, Some(0), "{}", evidence("CARGO_BIN_EXE stub test passes", &said));
            assert!(
                said.stdout.contains("2 passed, 0 failed"),
                "{}",
                evidence("2 passed, 0 failed", &said)
            );
        },
    );
}
