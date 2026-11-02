//! The warm launcher's side of the driver ([`crate::Confine::warm`]): the process a driver
//! starts once, outside any sandbox and in no job of its own, and hands each sandboxed run to.
//!
//! Its standard input is the control pipe, which the driver writes each run's frame to
//! ([`crate::Confine::hand`]); its standard output the answers pipe, which the driver reads
//! [`Answer`]s from; its standard error a duplicate of the driver's. A run's job and pipes are
//! made as for a process the driver starts ([`super::process`]); the child's ends and the job
//! are duplicated into the launcher, whose from then on they are, and the frame names them.
//!
//! Windows can't nest a job with UI limits (the run's, which the launcher sets) under a job the
//! launcher is in, so the launcher is in none of the driver's.

use std::{
    io,
    os::windows::io::{AsRawHandle as _, OwnedHandle},
    path::Path,
    sync::Arc,
};

use windows_sys::Win32::{
    Foundation::{DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetProcessId,
        WaitForSingleObject,
    },
};

use super::{
    process::{self, Finish, Launch, Standard},
    rundirs::RunDirs,
    sys,
};
use crate::{Answer, Command, Confine, Confined, Handed};

/// The warm launcher, started: its process and the driver's ends of its two pipes.
pub(super) struct Launched {
    pub(super) process: OwnedHandle,
    pub(super) pid: u32,
    /// Its standard input, which the driver writes frames to.
    pub(super) control: OwnedHandle,
    /// Its standard output, which the driver reads answers from.
    pub(super) answers: OwnedHandle,
}

/// The launcher `warm` says, started and running; `n` names its pipes.
pub(super) fn launch(warm: &Confined, n: u64) -> io::Result<Launched> {
    let (control, child_control) = process::pipe(false, n, "warm-in")?;
    let (answers, child_answers) = process::pipe(true, n, "warm-out")?;
    let stderr = driver_stderr()?;
    let mut inherited = [child_control.as_raw_handle(), child_answers.as_raw_handle(), std::ptr::null_mut()];
    let count = match &stderr {
        Some(stderr) => {
            inherited[2] = stderr.as_raw_handle();
            3
        }
        None => 2,
    };
    let launch = Launch { program: &warm.program, args: &warm.args, env: &warm.env, dir: &warm.dir };
    let flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    let (process, thread) = process::create(&launch, inherited, &inherited[..count], flags)?;
    drop(thread);
    // Its ends are its own now.
    drop((child_control, child_answers, stderr));
    // SAFETY: the process just made, open.
    let pid = unsafe { GetProcessId(process.as_raw_handle()) };
    Ok(Launched { process, pid, control, answers })
}

/// The driver's standard error duplicated, inheritable, for the launcher: none where the
/// driver has none.
fn driver_stderr() -> io::Result<Option<OwnedHandle>> {
    let own = std::io::stderr().as_raw_handle();
    if own.is_null() || own == INVALID_HANDLE_VALUE {
        return Ok(None);
    }
    let mut copy = std::ptr::null_mut();
    // SAFETY: this process's own handle of its standard error, which std keeps open for the
    // process's life, copied into this process.
    sys::check(unsafe {
        DuplicateHandle(GetCurrentProcess(), own, GetCurrentProcess(), &mut copy, 0, 1, DUPLICATE_SAME_ACCESS)
    })?;
    sys::owned(copy).map(Some)
}

/// Whether the launcher's process has ended.
pub(super) fn ended(process: &OwnedHandle) -> bool {
    // SAFETY: an open process; a wait of no time.
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_OBJECT_0 }
}

/// A run's handles duplicated into the warm launcher: closed there as this drops, unless
/// [`Remote::keep`] says the launcher may have read the frame that names them, from when they
/// are its own.
pub(super) struct Remote {
    /// The launcher's process. Open while this lives: the driver keeps it in the launcher's
    /// entry, which it lets go of only after the `Remote` of every run it handed is gone (each
    /// lives within one handing, `Iocp::hand_spawn`).
    launcher: HANDLE,
    pub(super) handed: Handed,
}

impl Remote {
    /// `handle`, of this process, duplicated into the launcher with its access: the value it
    /// has there.
    fn give(&self, handle: &OwnedHandle, inherit: bool) -> io::Result<u64> {
        let mut remote = std::ptr::null_mut();
        // SAFETY: an open handle of this process; the launcher's process handle is open (see
        // `launcher`) and has `PROCESS_DUP_HANDLE` (`CreateProcessW` gives all access).
        sys::check(unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                handle.as_raw_handle(),
                self.launcher,
                &mut remote,
                0,
                i32::from(inherit),
                DUPLICATE_SAME_ACCESS,
            )
        })?;
        Ok(remote as usize as u64)
    }

    /// The handles are the launcher's from here: the frame naming them is queued.
    pub(super) fn keep(mut self) {
        self.handed = Handed { run: self.handed.run, stdin: 0, stdout: 0, stderr: 0, job: 0 };
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let Handed { stdin, stdout, stderr, job, .. } = self.handed;
        for value in [stdin, stdout, stderr, job] {
            if value == 0 {
                continue;
            }
            // SAFETY: a handle this driver made in the launcher for this run and named in no
            // frame the launcher may have read (`keep` was not called), so the launcher has not
            // adopted it and nothing else there names it; the launcher's process is open (see
            // `launcher`). With no target and `DUPLICATE_CLOSE_SOURCE` the call closes it there.
            unsafe {
                DuplicateHandle(
                    self.launcher,
                    value as usize as HANDLE,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    DUPLICATE_CLOSE_SOURCE,
                )
            };
        }
    }
}

/// A run made to hand to the launcher: its job and the driver's ends of its pipes, the frame
/// for the launcher, and the handles given it.
pub(super) struct Handing {
    pub(super) job: OwnedHandle,
    pub(super) stdin: Option<OwnedHandle>,
    pub(super) stdout: OwnedHandle,
    pub(super) stderr: OwnedHandle,
    pub(super) frame: Vec<u8>,
    pub(super) remote: Remote,
    pub(super) finish: Finish,
}

/// The run of `command`, named `run`, made for the launcher whose process is `launcher` (at
/// `program`): its job as for a started process, its pipes, each child end duplicated into the
/// launcher (inheritable) and closed here, the job duplicated (not inheritable) and kept, its
/// run directory, and the frame `confine` makes of it. An error ends what was made, the
/// duplicates closed in the launcher.
pub(super) fn hand(
    dirs: &mut RunDirs,
    command: Box<Command>,
    // dyn: the caller's sandbox maker, asked once per sandboxed run.
    confine: &Arc<dyn Confine>,
    launcher: &OwnedHandle,
    program: &Path,
    run: u64,
) -> io::Result<Handing> {
    let n = process::serial();
    let handing = |error: io::Error| {
        io::Error::new(
            error.kind(),
            // alloc: an error path.
            format!(
                "{}: handing it to the sandbox's warm launcher {}: {error}",
                command.program.display(),
                program.display()
            ),
        )
    };
    let job = process::job(command.memory).map_err(handing)?;
    let Standard { stdin, stdout, stderr, theirs } = process::standard(&command, n).map_err(handing)?;
    let mut remote = Remote {
        launcher: launcher.as_raw_handle(),
        handed: Handed { run, stdin: 0, stdout: 0, stderr: 0, job: 0 },
    };
    let [child_stdin, child_stdout, child_stderr] = &theirs;
    remote.handed.stdin = remote.give(child_stdin, true).map_err(handing)?;
    remote.handed.stdout = remote.give(child_stdout, true).map_err(handing)?;
    remote.handed.stderr = remote.give(child_stderr, true).map_err(handing)?;
    remote.handed.job = remote.give(&job, false).map_err(handing)?;
    // The launcher's copies are the run's: the pipes end when they and the program's close.
    drop(theirs);
    let scratch = dirs.make(n).map_err(handing)?;
    let frame = match confine.hand(&command, &scratch, &remote.handed) {
        Ok(frame) => frame,
        Err(why) => {
            dirs.remove(scratch);
            // alloc: an error path.
            return Err(io::Error::other(format!("{}: no sandbox can be made here: {why}", command.program.display())));
        }
    };
    Ok(Handing {
        job,
        stdin,
        stdout,
        stderr,
        frame,
        remote,
        // clone: one reference count, the sandbox's to ask what the run read.
        finish: Finish { command, confine: Some(confine.clone()), scratch: Some(scratch) },
    })
}

/// The answers pipe's bytes cut into [`Answer`]s, a record cut across two reads kept until the
/// rest comes.
#[derive(Default)]
pub(super) struct Cut {
    carry: [u8; Answer::LEN],
    len: usize,
}

impl Cut {
    /// The next whole answer in `bytes`, which is moved past what it took; `None` once all of
    /// them are taken, a part of one at their end kept.
    pub(super) fn next(&mut self, bytes: &mut &[u8]) -> Option<Answer> {
        if self.len == 0
            && let Some((record, rest)) = bytes.split_first_chunk::<{ Answer::LEN }>()
        {
            *bytes = rest;
            return Some(Answer::from_bytes(*record));
        }
        let take = (Answer::LEN - self.len).min(bytes.len());
        self.carry[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        *bytes = &bytes[take..];
        if self.len < Answer::LEN {
            return None;
        }
        self.len = 0;
        Some(Answer::from_bytes(self.carry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers cut at random places across reads come out as the records the launcher wrote,
    /// in order, as a reference that cuts the whole stream at once says.
    #[test]
    fn answers_cut_across_reads_are_the_records_written() {
        for seed in 0..64 {
            let mut rng = crate::sim::Rng::new(seed);
            let count = rng.below(40) as usize;
            let written: Vec<Answer> =
                (0..count).map(|_| Answer { run: rng.next_u64(), code: rng.next_u64() as i32 }).collect();
            let stream: Vec<u8> = written.iter().flat_map(|it| it.to_bytes()).collect();
            let reference: Vec<Answer> = stream
                .chunks_exact(Answer::LEN)
                .map(|it| Answer::from_bytes(it.try_into().unwrap()))
                .collect();
            assert_eq!(reference, written);
            let mut cut = Cut::default();
            let mut read = Vec::new();
            let mut at = 0;
            while at < stream.len() {
                let len = (1 + rng.below(30) as usize).min(stream.len() - at);
                let mut bytes = &stream[at..at + len];
                while let Some(answer) = cut.next(&mut bytes) {
                    read.push(answer);
                }
                assert!(bytes.is_empty());
                at += len;
            }
            assert_eq!(read, written, "seed {seed}");
            assert_eq!(cut.len, 0);
        }
    }
}
