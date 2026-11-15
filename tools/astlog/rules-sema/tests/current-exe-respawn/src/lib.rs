//! The shape of agents/annoyances/launch3-4.json and `crates/blaze-engine/src/program.rs:204`:
//! a process reads its own path when it is about to run itself again. On Linux that path is
//! `<path> (deleted)` once a build has placed a new file there.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

// Found: reads its own path and runs it in the same function.
pub fn run_again(words: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    let this = std::env::current_exe()?;
    let mut command = Command::new(&this);
    command.args(words);
    command.status()
}

fn program(this: Option<&Path>) -> PathBuf {
    this.map_or_else(|| PathBuf::from("blaze"), Path::to_path_buf)
}

fn spawn(program: &OsStr) -> std::io::Result<()> {
    Command::new(program).spawn().map(drop)
}

fn start_daemon(blaze: &Path) -> std::io::Result<()> {
    spawn(blaze.as_os_str())
}

// Found: reads its own path, and a function it calls two calls down runs a program
// (`crates/blaze-client/src/driver.rs:112` and `start::daemon`).
pub fn connect() -> std::io::Result<()> {
    let this = std::env::current_exe().ok();
    let blaze = program(this.as_deref());
    start_daemon(&blaze)
}

// Not found: reads its own path to find a file beside it, and runs nothing.
pub fn sidecar() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let mut dir = exe.into_os_string();
    dir.push(".doctests");
    Ok(PathBuf::from(dir))
}

// Not found: runs a program, but never reads its own path.
pub fn run_tool() -> std::io::Result<()> {
    spawn(OsStr::new("git"))
}

#[cfg(test)]
mod tests {
    // Not found: a test that runs its own binary again, which no build replaces mid-test.
    #[test]
    fn child() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
