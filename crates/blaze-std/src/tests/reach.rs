//! Every reach outside the program's memory goes through the driver (docs/v2/plan/design/std.md,
//! "Every reach goes through the driver"): no file of the crate but the driver's backends names
//! a system call or a std function that reaches the system. A text check, so it sees names, not
//! calls: what it cannot see is listed on [`REACHES`].

use std::path::{Path, PathBuf};

/// The names that reach the system, each matched where it starts a path (not inside a longer
/// name: `bstd::net` is not `std::net`).
///
/// What this cannot see: a reach through a name imported under another (`use std::io as x`
/// and `x::stdout()`), through a macro that builds the path, through another crate's function
/// that reaches the system itself (blaze-core's, blaze-mem's maps), or through inline
/// assembly. Those are kept out by review of `use` lines and by the crate's dependencies,
/// which are the tree's own.
const REACHES: &[&str] = &[
    "std::io::stdout",
    "std::io::stderr",
    "std::io::stdin",
    "std::env::",
    "std::process::",
    "std::time::SystemTime",
    "std::time::Instant",
    "std::fs::",
    "std::net::",
    "std::random",
    "std::os::",
    "RandomState",
    "blaze_sys::",
    "blaze_core::cores::machine",
    "blaze_core::wait::process_age",
    "blaze_core::native(",
    "blaze_core::native_in(",
    "Os::process()",
    "extern \"C\" {",
    "getrandom",
    "mach_absolute_time",
    "clock_gettime",
];

/// The driver's backends, where the names above belong: the native runtime, the native
/// backend's host facts, the one line of `io/native_in.rs` that makes the native driver with
/// the root of real-OS authority, and the places that take that root: the `main!` macro's
/// process main (entry.rs) and, until batch B1 deletes it, `Io::native` (io.rs), which reaches
/// it itself.
///
/// And the hermetic runner's native port (D177; procs/hermetic.rs): its core (`machine.rs`,
/// `plan.rs`, `execution.rs`, `report.rs`) is a sans-IO state machine that reaches nothing and
/// asks its [`Port`](crate::procs::hermetic) for each effect; `sim.rs` is the simulated port
/// and `native.rs` with its per-OS confiners `seatbelt.rs` (macOS: the host clock's timebase,
/// `proc_pid_rusage`, `killpg`) and `landlock.rs` (Linux: the ruleset's `openat`s, the group
/// kill) is the native port: the thin driver that IS the seam's other side, as `rt/` is for
/// the program. Moving those calls into `rt/` would add a second seam between a driver and
/// its own system calls, not keep one out of a core. So these three files may name blaze-sys,
/// and only blaze-sys: any other reach in them is still found.
fn allowed(file: &str, reach: &str) -> bool {
    file == "rt.rs"
        || file.starts_with("rt/")
        || file == "native.rs"
        || (file == "io/native_in.rs" && reach == "blaze_core::native_in(")
        || (file == "io.rs" && reach == "Os::process()")
        || (file == "entry.rs" && reach == "Os::process()")
        || (matches!(file, "procs/hermetic/native.rs" | "procs/hermetic/seatbelt.rs" | "procs/hermetic/landlock.rs") && reach == "blaze_sys::")
        // The sampler (D178): `sampler/mac.rs` and `sampler/linux.rs` are its two drivers (they
        // stop a thread and read its registers through blaze-sys); `sampler.rs` re-exports the
        // pure ring, slot and walk, which live in blaze-sys because every `unsafe` of the
        // sampler is kept there (its line 85): a type path, not a call into the system.
        || (matches!(file, "telemetry/sampler/mac.rs" | "telemetry/sampler/linux.rs" | "telemetry/sampler.rs") && reach == "blaze_sys::")
        // `local.rs`: compile-time `const _` assertion checking `SOCKET_PATH_MOST == sockaddr_un::ZERO.sun_path.len()`;
        // a pure layout check, not a runtime syscall.
        || (file == "local.rs" && reach == "blaze_sys::")
}

/// Files that are tests, which run on the host's std and are not the program: `tests.rs`,
/// `tests/`, and a test module of its own file (`x_tests.rs`, declared under `#[cfg(test)]`:
/// procs/hermetic/process_tests.rs, io/lend_tests.rs, sync/once_tests.rs,
/// task/threads/find_tests.rs).
fn is_test(file: &str) -> bool {
    file == "tests.rs" || file.starts_with("tests/") || file.ends_with("/tests.rs") || file.ends_with("_tests.rs")
}

/// Each `(file, line, reach)` of `text` outside comments.
fn bypasses(file: &str, text: &str) -> Vec<(String, usize, &'static str)> {
    let mut found = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        for reach in REACHES {
            let starts = code.match_indices(reach).any(|(start, _)| {
                code[..start].chars().next_back().is_none_or(|before| !(before.is_alphanumeric() || before == '_'))
            });
            if starts && !allowed(file, reach) {
                found.push((file.to_owned(), at + 1, *reach));
            }
        }
    }
    found
}

fn sources(dir: &Path, under: &Path, into: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the crate's source directory is readable") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            sources(&path, under, into);
        } else if path.extension().is_some_and(|it| it == "rs") {
            into.push(path.strip_prefix(under).expect("under src").to_path_buf());
        }
    }
}

#[test]
fn no_file_but_the_backends_names_a_reach() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &src, &mut files);
    assert!(files.len() > 50, "the walk found the crate's files: {}", files.len());
    let mut found = Vec::new();
    for file in &files {
        let name = file.to_string_lossy().replace('\\', "/");
        if is_test(&name) {
            continue;
        }
        let text = std::fs::read_to_string(src.join(file)).expect("a source file is UTF-8");
        found.extend(bypasses(&name, &text));
    }
    assert!(found.is_empty(), "reaches outside the driver's backends: {found:?}");
}

/// The check on itself: a bypass put back is found, in a file that is not a backend, and a
/// longer name that only contains a reach is not.
#[test]
fn a_bypass_put_back_is_found() {
    let planted = "fn f() {\n    let _ = std::io::stdout().write_all(b\"x\");\n}\n";
    assert_eq!(bypasses("out.rs", planted), vec![("out.rs".to_owned(), 2, "std::io::stdout")]);
    assert_eq!(bypasses("rt.rs", planted), vec![], "the runtime is a backend");
    assert_eq!(bypasses("io.rs", "use bstd::net::Net; // std::fs::read\n"), vec![]);
    assert_eq!(bypasses("guest.rs", "let seed = std::random::random(..);\n"), vec![("guest.rs".to_owned(), 1, "std::random")]);
    // The hermetic native port may name blaze-sys and nothing else; its sans-IO core may not.
    let sys = "let t = blaze_sys::macos::timebase();\n";
    assert_eq!(bypasses("procs/hermetic/seatbelt.rs", sys), vec![]);
    assert_eq!(bypasses("procs/hermetic/machine.rs", sys), vec![("procs/hermetic/machine.rs".to_owned(), 1, "blaze_sys::")]);
    assert_eq!(bypasses("procs/hermetic/seatbelt.rs", "let _ = std::fs::read(p);\n"), vec![("procs/hermetic/seatbelt.rs".to_owned(), 1, "std::fs::")]);
}
