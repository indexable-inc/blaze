//! Confinement is proven by a hostile script, never by reading (D65, rule 9): this test binary
//! runs itself inside the sandbox as the script (`hostile_child`), which tries one thing out of
//! its grant; each attempt must fail confined and succeed with the confinement planted off.

use std::{path::PathBuf, time::Duration};

use blaze_build_script::native::Grant;
use blaze_own::Alloc;

use super::{NativeRefusal, NativeRun, Plant, run_planted};

const ATTEMPT: &str = "BLAZE_D65_ATTEMPT";

/// The hostile script: does what `BLAZE_D65_ATTEMPT` says and prints what happened. Run
/// directly (no variable) it does nothing.
#[test]
fn hostile_child() {
    let Ok(attempt) = std::env::var(ATTEMPT) else { return };
    let (verb, arg) = attempt.split_once(' ').expect("verb and argument");
    let result: Result<(), std::string::String> = match verb {
        "read" => std::fs::read(arg).map(drop).map_err(|e| e.to_string()),
        "write" => std::fs::write(arg, b"escaped").map_err(|e| e.to_string()),
        "connect" => std::net::TcpStream::connect(arg).map(drop).map_err(|e| e.to_string()),
        "exec" => std::process::Command::new(arg).output().map(drop).map_err(|e| e.to_string()),
        // D71: `perl` found by the `PATH` search, as sha3-asm starts it, running real code
        // (its own modules from the granted folders: `use strict`) and answering.
        "perl" => std::process::Command::new("perl").args(["-e", arg]).output().map_err(|e| e.to_string()).and_then(|out| {
            let said = std::string::String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && said == "2" { Ok(()) } else { Err(format!("exit {:?}, said {said:?}, {}", out.status.code(), std::string::String::from_utf8_lossy(&out.stderr))) }
        }),
        "sleep" => {
            std::thread::sleep(Duration::from_millis(arg.parse().expect("milliseconds")));
            Ok(())
        }
        other => panic!("unknown attempt {other}"),
    };
    match result {
        Ok(()) => println!("ATTEMPT {verb} ok"),
        Err(e) => println!("ATTEMPT {verb} denied: {e}"),
    }
}

struct Place {
    root: PathBuf,
    package: std::string::String,
    out: std::string::String,
    outside: std::string::String,
}

fn place(name: &str) -> Place {
    let root = std::env::temp_dir().join(format!("blaze-d65-{}-{name}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    for dir in ["package", "out", "outside"] {
        std::fs::create_dir_all(root.join(dir)).expect("a test folder");
    }
    let root = root.canonicalize().expect("the folder's real path");
    std::fs::write(root.join("outside/canary.txt"), b"canary").expect("the canary");
    std::fs::write(root.join("package/build.rs"), b"fn main() {}").expect("a package file");
    let text = |dir: &str| root.join(dir).to_str().expect("a UTF-8 temp path").to_owned();
    Place { package: text("package"), out: text("out"), outside: text("outside"), root }
}

/// The hostile script's line for `attempt`, run with `plant`.
fn attempt(place: &Place, attempt: &str, deadline: Duration, plant: Plant) -> Result<std::string::String, NativeRefusal> {
    let exe = std::env::current_exe().expect("the test binary").canonicalize().expect("its real path");
    let exe = exe.to_str().expect("a UTF-8 path");
    let reads = [place.package.as_str()];
    let writes = [place.out.as_str()];
    let run = NativeRun {
        grant: Grant { program: exe, exec: &[], read: &reads, write: &writes },
        args: &["--exact", "native_sandbox::tests::hostile_child", "--nocapture", "--test-threads=1"],
        env: &[(ATTEMPT, attempt)],
        dir: &place.package,
        deadline,
        exec_under: &[],
    };
    let ran = run_planted(&run, plant, Alloc::global())?;
    let stdout = std::string::String::from_utf8_lossy(&ran.stdout);
    // libtest prints its own `test .. ` before the line on the same row.
    let line = stdout.lines().find_map(|l| l.find("ATTEMPT ").map(|at| l[at..].to_owned()));
    Ok(line.unwrap_or_else(|| format!("no line: code {:?}: {stdout} {}", ran.code, std::string::String::from_utf8_lossy(&ran.stderr))))
}

const LONG: Duration = Duration::from_secs(20);

/// Each attempt confined and planted off; `confined` must say denied, `off` ok.
fn hostile(name: &str, make: impl Fn(&Place) -> std::string::String) {
    let place = place(name);
    let what = make(&place);
    let confined = attempt(&place, &what, LONG, Plant::None).expect("the confined run");
    let off = attempt(&place, &what, LONG, Plant::NoConfinement).expect("the planted run");
    println!("{name}: confined: {confined}");
    println!("{name}: planted off: {off}");
    assert!(confined.contains(" denied: "), "{name}: confined: {confined}");
    assert!(off.ends_with(" ok"), "{name}: planted off: {off}");
    _ = std::fs::remove_dir_all(&place.root);
    quote(&format!("{name}: confined: {confined} | planted off: {off}"));
}

/// With `BLAZE_D65_QUOTE` set, a passing test fails with its lines, so a run that shows only
/// failures shows them (evidence for the lane's report); unset, nothing.
fn quote(lines: &str) {
    if std::env::var_os("BLAZE_D65_QUOTE").is_some() {
        panic!("QUOTE {lines}");
    }
}

/// The hostile script's line for `attempt`, confined, granted the pin table's host tools for
/// `package` (D71) exactly as `native_script::run_script` grants them: each tool's real path
/// executed, its `reads` read-only, `PATH` the tools' folders.
fn attempt_with_tools(place: &Place, attempt: &str, package: &str) -> std::string::String {
    use blaze_build_script::{host_tools, toolchain::PIN};
    let alloc = Alloc::global();
    let table = host_tools::host_tools(PIN, "aarch64-apple-darwin", alloc).expect("the macOS host-tool row");
    let tools = host_tools::granted(&table, package, alloc);
    let exe = std::env::current_exe().expect("the test binary").canonicalize().expect("its real path");
    let exe = exe.to_str().expect("a UTF-8 path");
    let exec: std::vec::Vec<&str> = tools.iter().flat_map(|t| t.programs()).collect();
    let mut reads: std::vec::Vec<&str> = vec![place.package.as_str()];
    reads.extend(tools.iter().flat_map(|t| t.read_folders()));
    let mut folders: std::vec::Vec<&str> = tools.iter().map(|t| t.path.rsplit_once('/').map_or("/", |(d, _)| d)).collect();
    folders.sort_unstable();
    folders.dedup();
    let path = folders.join(":");
    let writes = [place.out.as_str()];
    let run = NativeRun {
        grant: Grant { program: exe, exec: &exec, read: &reads, write: &writes },
        args: &["--exact", "native_sandbox::tests::hostile_child", "--nocapture", "--test-threads=1"],
        env: &[(ATTEMPT, attempt), ("PATH", path.as_str())],
        dir: &place.package,
        deadline: LONG,
        exec_under: &[],
    };
    let ran = run_planted(&run, Plant::None, alloc).expect("the confined run");
    let stdout = std::string::String::from_utf8_lossy(&ran.stdout);
    let line = stdout.lines().find_map(|l| l.find("ATTEMPT ").map(|at| l[at..].to_owned()));
    line.unwrap_or_else(|| format!("no line: code {:?}: {stdout} {}", ran.code, std::string::String::from_utf8_lossy(&ran.stderr)))
}

/// D71: sha3-asm's script, confined and granted the table's perl, starts `perl` by name and
/// perl runs (its `strict` module read from /System/Library/Perl); the same confinement without
/// the grant (a package the table does not list) cannot start it; a program not on the list
/// (`/usr/bin/python3`) is denied even with perl granted. Prints
/// "host tool: granted: ATTEMPT perl ok | not granted: ATTEMPT perl denied: .. | python3:
/// ATTEMPT exec denied: ..".
#[test]
#[cfg(target_os = "macos")]
fn a_granted_host_tool_runs_and_an_unlisted_one_is_denied() {
    let place = place("host-tool");
    // A piped open of a quoted command, as cryptogams' perlasm opens arm-xlate.pl: perl starts
    // /bin/sh, which starts /bin/bash, which starts perl again to copy the `2` through.
    let code = r#"use strict; open(STDOUT, "| \"$^X\" -pe 1") or die "pipe: $!"; print 1+1"#;
    let granted = attempt_with_tools(&place, &format!("perl {code}"), "sha3-asm");
    let not_granted = attempt_with_tools(&place, &format!("perl {code}"), "zstd-sys");
    let python = attempt_with_tools(&place, "exec /usr/bin/python3", "sha3-asm");
    println!("host tool: granted: {granted} | not granted: {not_granted} | python3: {python}");
    assert_eq!(granted, "ATTEMPT perl ok", "granted: {granted}");
    assert!(not_granted.contains(" denied: "), "not granted: {not_granted}");
    assert!(python.contains(" denied: "), "python3: {python}");
    _ = std::fs::remove_dir_all(&place.root);
    quote(&format!("host tool: granted: {granted} | not granted: {not_granted} | python3: {python}"));
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "the Linux sandbox's tests run on the VM: remove this on Linux")]
fn a_native_script_cannot_read_outside_its_inputs() {
    hostile("read", |p| format!("read {}/canary.txt", p.outside));
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "the Linux sandbox's tests run on the VM: remove this on Linux")]
fn a_native_script_cannot_write_outside_out_dir() {
    hostile("write", |p| format!("write {}/escaped.txt", p.outside));
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "the Linux sandbox's tests run on the VM: remove this on Linux")]
fn a_native_script_cannot_open_a_socket() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let address = listener.local_addr().expect("its address");
    hostile("connect", |_| format!("connect {address}"));
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "the Linux sandbox's tests run on the VM: remove this on Linux")]
fn a_native_script_cannot_start_a_program_it_was_not_granted() {
    hostile("exec", |_| "exec /bin/echo".to_owned());
}

/// D69: the machine's own C compiler is never reachable: `/usr/bin/cc` (the shim that would
/// start Xcode's clang) cannot be started from inside.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "the Linux sandbox's tests run on the VM: remove this on Linux")]
fn a_native_script_cannot_start_the_machines_cc() {
    hostile("cc", |_| "exec /usr/bin/cc".to_owned());
}

/// D69: nor can Xcode's clang be read (so not copied into `OUT_DIR` and started from there).
#[test]
#[cfg(target_os = "macos")]
fn a_native_script_cannot_read_xcodes_clang() {
    const XCODE_CLANG: &str = "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang";
    assert!(std::path::Path::new(XCODE_CLANG).is_file(), "this machine has no Xcode clang at {XCODE_CLANG}: the test proves nothing");
    hostile("xcode-clang", |_| format!("read {XCODE_CLANG}"));
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "the Linux sandbox's tests run on the VM: remove this on Linux")]
fn a_native_script_inside_its_grant_reads_its_package_and_writes_out_dir() {
    let place = place("inside");
    let read = attempt(&place, &format!("read {}/build.rs", place.package), LONG, Plant::None).expect("a run");
    let write = attempt(&place, &format!("write {}/made.rs", place.out), LONG, Plant::None).expect("a run");
    println!("inside: {read}; {write}");
    assert_eq!((read.as_str(), write.as_str()), ("ATTEMPT read ok", "ATTEMPT write ok"));
    _ = std::fs::remove_dir_all(&place.root);
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "the Linux sandbox's tests run on the VM: remove this on Linux")]
fn a_native_script_does_not_outlive_its_deadline() {
    let place = place("deadline");
    let short = Duration::from_millis(1500);
    let confined = attempt(&place, "sleep 6000", short, Plant::None);
    let off = attempt(&place, "sleep 6000", short, Plant::NoDeadline);
    println!("deadline: confined: {confined:?}");
    println!("deadline: planted off: {off:?}");
    assert!(matches!(confined, Err(NativeRefusal::Deadline { .. })), "{confined:?}");
    assert!(matches!(off.as_deref(), Ok("ATTEMPT sleep ok")), "{off:?}");
    _ = std::fs::remove_dir_all(&place.root);
    quote(&format!("deadline: confined: {} | planted off: {off:?}", confined.map_err(|e| e.to_string()).err().unwrap_or_default()));
}

#[test]
fn a_grant_that_cannot_be_said_runs_nothing() {
    let place = place("unsayable");
    let reads = ["/tmp/../etc"];
    let run = NativeRun {
        grant: Grant { program: "/bin/echo", exec: &[], read: &reads, write: &[] },
        args: &[],
        env: &[],
        dir: &place.package,
        deadline: LONG,
        exec_under: &[],
    };
    let refused = run_planted(&run, Plant::None, Alloc::global());
    assert!(matches!(&refused, Err(NativeRefusal::Grant { why }) if why.contains("/tmp/../etc")), "{refused:?}");
    _ = std::fs::remove_dir_all(&place.root);
}
