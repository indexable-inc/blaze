//! The smallest input that still fails, as libFuzzer's `-minimize_crash`: a failure may end
//! the process (an AddressSanitizer report), so the tries run in a child, `<this binary>
//! serve`, which runs them one after another and answers each over a pipe. A passing try
//! costs a round trip; after a failing one the child is replaced, since a panic may have left
//! state behind (a poisoned lock) that would make the next try fail too. Chunks are cut out,
//! halving their size down to one byte, then each byte is tried as zero; a try counts when it
//! fails the way the input did (a failure, or a timeout for a timeout).

use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio},
    time::{Duration, Instant},
};

use crate::{TIMED_OUT, corpus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Passed,
    Failed,
    TimedOut,
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

struct Tries<'a> {
    exe: &'a Path,
    timeout: Duration,
    deadline: Instant,
    runs: u64,
    server: Option<Server>,
    /// The frame being sent, reused.
    frame: Vec<u8>,
}

impl Tries<'_> {
    fn spawn(&self) -> Result<Server, String> {
        let mut child = Command::new(self.exe)
            .arg("serve")
            .args(["--timeout-ms", &self.timeout.as_millis().to_string()])  // alloc: a server's command line, once per server
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{}: {e}", self.exe.display()))?;  // alloc: error message
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            _ = child.kill();
            return Err(format!("{} serve: no pipes", self.exe.display()));  // alloc: error message
        };
        Ok(Server { child, stdin, stdout })
    }

    fn outcome(&mut self, bytes: &[u8]) -> Result<Outcome, String> {
        self.runs += 1;
        let len = u32::try_from(bytes.len()).map_err(|_| format!("{} bytes: past what `serve` takes", bytes.len()))?;  // alloc: error message
        if self.server.is_none() {
            self.server = Some(self.spawn()?);
        }
        let server = self.server.as_mut().expect("started above");
        self.frame.clear();
        self.frame.extend_from_slice(&len.to_le_bytes());
        self.frame.extend_from_slice(bytes);
        let mut answer = [0u8];
        let answered = server
            .stdin
            .write_all(&self.frame)
            .and_then(|()| server.stdin.flush())
            .and_then(|()| server.stdout.read_exact(&mut answer))
            .is_ok();
        if answered && answer[0] == 0 {
            return Ok(Outcome::Passed);
        }
        // It panicked (and the server ends at the end of its stdin), or it ended the server.
        let Server { mut child, stdin, stdout } = self.server.take().expect("started above");
        drop((stdin, stdout));
        let status = child.wait().map_err(|e| e.to_string())?;  // alloc: error message
        Ok(if answered {
            Outcome::Failed
        } else if status.code() == Some(TIMED_OUT) {
            Outcome::TimedOut
        } else if status.success() {
            Outcome::Passed
        } else {
            Outcome::Failed
        })
    }

    fn left(&self) -> bool {
        Instant::now() < self.deadline
    }
}

impl Drop for Tries<'_> {
    fn drop(&mut self) {
        if let Some(Server { mut child, stdin, stdout }) = self.server.take() {
            drop((stdin, stdout));
            _ = child.wait();
        }
    }
}

pub(crate) fn minimize(input: &Path, out: &Path, budget: Duration, timeout: Duration) -> Result<ExitCode, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this binary's path: {e}"))?;  // alloc: error message
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;  // alloc: error message
    let mut tries =
        Tries { exe: &exe, timeout, deadline: Instant::now() + budget, runs: 0, server: None, frame: Vec::new() };  // alloc: none until the first frame, then reused
    let mut best = corpus::read(input)?;
    let wanted = tries.outcome(&best)?;
    if wanted == Outcome::Passed {
        return Err(format!("{} passes: nothing to minimize", input.display()));  // alloc: error message
    }
    let mut chunk = best.len().div_ceil(2).max(1);
    let mut smaller = Vec::with_capacity(best.len());
    while !best.is_empty() && tries.left() {
        let mut cut = false;
        let mut at = 0;
        while at < best.len() && tries.left() {
            smaller.clear();
            smaller.extend_from_slice(&best[..at]);
            smaller.extend_from_slice(&best[(at + chunk).min(best.len())..]);
            if tries.outcome(&smaller)? == wanted {
                std::mem::swap(&mut best, &mut smaller);
                cut = true;
            } else {
                at += chunk;
            }
        }
        if chunk == 1 && !cut {
            break;
        }
        if !cut {
            chunk = chunk.div_ceil(2);
        }
    }
    for at in 0..best.len() {
        if !tries.left() {
            break;
        }
        let byte = best[at];
        if byte == 0 {
            continue;
        }
        best[at] = 0;
        if tries.outcome(&best)? != wanted {
            best[at] = byte;
        }
    }
    let path = corpus::write(out, "minimized-", &best)?;
    eprintln!(
        "blaze-fuzzer: minimized {} to {} bytes in {} runs ({:?}): {}",
        input.display(),
        best.len(),
        tries.runs,
        wanted,
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}
