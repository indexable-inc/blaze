//! A dev run that checks it is exact: `inproc-exact <path>`, started by blaze as the
//! executable written at `<path>` (cargo's), exits 0 when
//!
//! - `current_exe` names `<path>` (on Linux `/proc/self/exe`, on macOS dyld's image 0, on
//!   Windows the module the PEB names);
//! - its start hook attached it to the daemon's code memory before `main`, and took
//!   `BLAZE_INPROC` out of the environment;
//! - a backtrace taken in code the overlay put in place names this file's functions.
//!
//! Each check prints one line; a failed one prints why and the exit code is 1.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[inline(never)]
fn inproc_exact_marker() -> String {
    std::backtrace::Backtrace::force_capture().to_string()
}

fn same(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn main() -> ExitCode {
    let Some(expected) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: inproc-exact <path this executable was written to>");
        return ExitCode::from(2);
    };
    let mut failed = false;
    let mut check = |what: &str, ok: bool, detail: String| {
        println!("{} {what}: {detail}", if ok { "ok" } else { "FAILED" });
        failed |= !ok;
    };

    match std::env::current_exe() {
        Ok(exe) => check("current_exe", same(&exe, &expected), format!("{} (expected {})", exe.display(), expected.display())),
        Err(error) => check("current_exe", false, error.to_string()),
    }
    match blaze_inproc::runtime::attach() {
        Ok(attached) => check("attached", true, format!("slide {:#x}", attached.slide())),
        Err(error) => check("attached", false, error.to_string()),
    }
    check(
        "environment",
        std::env::var_os("BLAZE_INPROC").is_none(),
        "BLAZE_INPROC is not visible to the program".to_owned(),
    );
    let backtrace = inproc_exact_marker();
    check(
        "backtrace",
        backtrace.contains("inproc_exact_marker") && backtrace.contains("exact.rs"),
        backtrace.lines().take(8).collect::<Vec<_>>().join(" | "),
    );
    blaze_inproc::runtime::quiesce();

    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
