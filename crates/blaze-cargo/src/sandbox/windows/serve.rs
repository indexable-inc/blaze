//! `blaze sandbox --v2-serve`: the warm launcher on Windows. A driver starts one per daemon
//! ([`blaze_core::Confine::warm`]), outside the daemon and outside any sandbox, and hands it each
//! sandboxed run: a frame on its standard input ([`confine::read_hand`]) with the spec, the run's
//! standard handles and its job, which the driver duplicated into this process. Each run is
//! [`super::super::v2::launch_with`] of [`super::run`] on a thread of its own, which writes the
//! report as `blaze sandbox --v2` does and then the run's [`Answer`], whole, on standard output.
//!
//! What a launcher of one run makes again each time is made once here: the container's and the
//! well-known SIDs ([`super::kept`]), the session lock's handle, and the read checks
//! ([`super::reads`]). So a run is one process creation, the program's.
//!
//! A fault ends the launcher: a frame it can't read, an answer it can't write, a thread it can't
//! start, a panic anywhere. Every run in it ends with it, its job ended, and the driver fails each
//! naming the launcher, then starts another for the next run. No run is ever outside its sandbox,
//! and none is started again some other way.

use std::{io::Write as _, time::Duration};

use blaze_core::Answer;
use blaze_usercode::confine;

use super::{Given, end_every_run};
use crate::sandbox::COULD_NOT;

/// How long a launcher that ends waits for its runs to take their grants back.
const STOPPING: Duration = Duration::from_secs(4);

/// The code the jobs of a launcher that ends are ended with: a kill's, as the driver's.
const ENDED: u32 = 1;

/// Serves runs until the driver closes the launcher's standard input, or a fault.
pub fn serve() -> ! {
    // Not ending the jobs, whose lock the panic may be under: the driver's handles of them are
    // closed as it fails their runs, and the jobs kill what is in them then.
    // alloc: once per launcher, the hook.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("blaze sandbox --v2-serve: {info}");
        std::process::abort();
    }));
    super::reads::keep();
    let mut input = std::io::stdin().lock();
    // alloc: once per launcher, the buffer every frame is read into.
    let mut buffer = Vec::with_capacity(64 << 10);
    let mut started = 0u64;
    loop {
        let (handed, spec) = match confine::read_hand(&mut input, &mut buffer) {
            Ok(Some(run)) => run,
            // The driver is gone, or done: nobody hears of a run in flight any more.
            Ok(None) => stop(0, None),
            Err(why) => stop(COULD_NOT, Some(&why)),
        };
        // SAFETY: the driver duplicated each handle `handed` names into this process for this
        // run alone (`blaze_core::Handed`), and each frame is read once, so each is adopted once.
        let given = unsafe { Given::adopt(&handed) };
        started += 1;
        let thread = std::thread::Builder::new()
            // alloc: once per run, its thread's name.
            .name(format!("blaze-sandbox-run-{started}"))
            .spawn(move || {
                let code = crate::sandbox::v2::launch_with(&spec, |spec, _live| super::run(spec, given));
                answer(Answer { run: handed.run, code });
            });
        if let Err(error) = thread {
            // alloc: the fault, once, and the launcher ends.
            stop(COULD_NOT, Some(&format!("starting the thread of run {}: {error}", handed.run)));
        }
    }
}

/// Writes `answer` whole on standard output, which every run's thread shares.
fn answer(answer: Answer) {
    let mut out = std::io::stdout().lock();
    if let Err(error) = out.write_all(&answer.to_bytes()).and_then(|()| out.flush()) {
        drop(out);
        // alloc: the fault, once, and the launcher ends.
        stop(COULD_NOT, Some(&format!("writing the answer of run {}: {error}", answer.run)));
    }
}

/// Ends the launcher with `code`: every run's job ended, and a while for the runs to take their
/// grants back.
fn stop(code: i32, why: Option<&str>) -> ! {
    if let Some(why) = why {
        eprintln!("blaze sandbox --v2-serve: {why}");
    }
    end_every_run(ENDED, STOPPING);
    std::process::exit(code)
}
