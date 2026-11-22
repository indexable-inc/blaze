//! The test zygote on the host, end to end (docs/v2/plan/design/test.md, "End state", piece 9):
//! this process is the zygote; it forks three sandboxed children, one per row, three at once.
//! Row 0 returns (a pass), row 1 aborts (a signal), row 2 sleeps 5 s against a 1 s deadline
//! (asked for its stacks by `SIGQUIT`, then killed with its group).
//!
//! Run (macOS arm64; on Linux it stops by name: no test sandbox is built there):
//!
//! ```text
//! LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/mixcheck.sh blaze-launch-rt run fork_smoke
//! ```
//!
//! It prints one line per row, in the order the runs ended (row 1 and row 0 in either order,
//! within milliseconds; row 2 last, about 1.2 s in: its deadline, then the 200 ms grace for its
//! stacks), then a summary, and exits 0. `after` is counted from the fork to the deadline the
//! pool acted on, so it reads 1.0 s:
//!
//! ```text
//! row 1: signal 6
//! row 0: passed
//! row 2: timed out after 1.0 s (N return addresses)
//! fork_smoke: 3 rows as expected
//! ```
//!
//! `N` is the number of frames the child's `SIGQUIT` handler walked: above 0 shows the frame
//! walk from a handler crosses `_sigtramp` into the sleeping test (a fact to verify); 0 says it
//! did not, and the line still counts the row as expected. Any other line, or a run that stops
//! with an error, is a finding: the error names the call and the row.
//!
//! What a pass of this run proves on the host: `sandbox_init` takes the profile blaze-run's
//! `child::seatbelt` writes; `proc_pidinfo(PROC_PIDTASKINFO)` counts one thread in a plain Rust
//! `main`; `PROC_PIDLISTFDS` lists the descriptors to close; a std program loads no system
//! framework; a child's own process group is killed whole.

use std::process::ExitCode;

use blaze_launch_rt::zygote::{self, Asked, Monotonic, Suite, Waits};
use blaze_own::AVec;
use blaze_run::child::Spec;
use blaze_run::postings::FnName;
use blaze_run::zygote::How;
use blaze_run::{Nanos, TestId};

struct Smoke {
    temps: [String; 3],
}

impl Suite for Smoke {
    fn spec(&self, row: u32) -> Spec<'_> {
        Spec { row, temp: &self.temps[row as usize], env: &[], control: None }
    }

    fn call(&self, row: u32) {
        match row {
            0 => {}
            1 => std::process::abort(),
            _ => std::thread::sleep(std::time::Duration::from_secs(5)),
        }
    }

    fn entered(&self, _: &mut AVec<'_, FnName>) {}

    fn control(&self, _: &str) -> Result<(), i32> {
        // No spec names a socket, so no child takes this step.
        Err(libc::ENOSYS)
    }
}

struct Quiet;

impl Waits for Quiet {
    fn waiting(&mut self, _: &'static str, _: Option<Nanos>) {}
    fn woke(&mut self) {}
}

fn main() -> ExitCode {
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here and handed down.
    let frame = blaze_mem::Frame::with_reserve(1 << 30, blaze_mem::Tag::Other).expect("the entry's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let pid = std::process::id();
    // Canonical: `/tmp` is a link to `/private/tmp` on macOS.
    let base = if cfg!(target_os = "macos") { "/private/tmp" } else { "/tmp" };
    let suite = Smoke { temps: [0, 1, 2].map(|row| format!("{base}/blaze-fork-smoke-{pid}-{row}")) };
    let asked = [0u32, 1, 2].map(|row| Asked {
        row,
        test: TestId([row as u8; 32]),
        budget: Nanos(1_000_000_000),
    });
    let mut expected = 0;
    let mut sink = |finished: &blaze_run::zygote::Finished<'_>| {
        let (line, fits) = match (finished.row, finished.how) {
            (0, How::Reported(blaze_run::zygote::Said::Returned)) => ("passed".to_owned(), true),
            (1, How::Signal { number }) => (format!("signal {number}"), number == libc::SIGABRT),
            (2, How::TimedOut { after }) => (
                format!("timed out after {:.1} s ({} return addresses)", after.0 as f64 / 1e9, finished.stacks.len()),
                after.0 >= 1_000_000_000,
            ),
            (_, how) => (format!("unexpected: {how:?}"), false),
        };
        println!("row {}: {line}", finished.row);
        expected += usize::from(fits);
    };
    let result = zygote::run(&suite, &asked, 3, &Monotonic, &mut Quiet, alloc, &mut sink);
    for temp in &suite.temps {
        // The children's folders; a row that never made its folder has none to remove.
        let _ = std::fs::remove_dir_all(temp);
    }
    if let Err(error) = result {
        eprintln!("fork_smoke: {error}");
        return ExitCode::FAILURE;
    }
    match expected {
        3 => {
            println!("fork_smoke: 3 rows as expected");
            ExitCode::SUCCESS
        }
        n => {
            println!("fork_smoke: {n} of 3 rows as expected");
            ExitCode::FAILURE
        }
    }
}
