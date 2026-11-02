# Requests of `usercode-win-launcher` (child of lane `usercode`)

The warm sandbox launcher on Windows (v2/requests/usercode.md, "speed-link-edge-sol-usercode: one
process creation per sandboxed run on Windows, by a warm launcher"), with one `Confine` shape that
cargo's request 14 (the warm launcher on Linux and macOS) uses too. The launcher's side is written:
`blaze sandbox --v2-serve` (`crates/blaze-cargo/src/sandbox/windows/serve.rs`), the kept read
checks (`sandbox/windows/reads.rs`), the run given the driver's handles (`sandbox/windows.rs`,
`Given`), and `blaze_usercode::confine::Launcher`'s `warm` and `hand` with the frame's reader
(`read_hand`). What is left is the driver's side, below. Until 1 is applied, `blaze-usercode` and
`blaze-cargo` don't build (they use `blaze_core::{Handed, Answer}` and the two trait methods).

## 1. core: `Confine` can hand a run to a warm launcher (`crates/blaze-core/src/lib.rs`)

What: two provided methods on `Confine`, and two types beside `Confined`. Every `Confine` today
(the drivers' test fakes included) keeps building unchanged: both methods have defaults, and a
confine whose `warm` is `None` is used exactly as now.

Replace the trait (lines 50-62) with:

```rust
/// Makes the sandbox of a process: given to a driver by whoever owns the platform's sandbox
/// (`blaze-usercode`), so the driver starts what it says in place of the program. A driver
/// given none fails every spawn whose [`Sandbox`] is on.
pub trait Confine: Send + Sync {
    /// What to start so that `command` runs in its sandbox, usually a launcher with the
    /// program after it. `scratch` is a directory of this run's own, empty, made by the
    /// driver and removed once [`Confine::traced`] has been asked: where the launcher keeps
    /// its record. An error says why no sandbox can be made here; nothing runs then.
    fn confine(&self, command: &Command, scratch: &std::path::Path) -> Result<Confined, String>;
    /// What the sandbox recorded the process read, once it has exited: asked only of a
    /// sandbox that traces.
    fn traced(&self, command: &Command, scratch: &std::path::Path) -> Result<Traced, String>;
    /// The warm launcher this sandbox hands its runs to, where it has one: the process a driver
    /// starts once, and again only after it ended, outside any sandbox and in no job of the
    /// driver's. Its standard input is a pipe the driver writes [`Confine::hand`]'s frames to,
    /// its standard output a pipe the driver reads [`Answer`]s from, and its standard error the
    /// driver's own. A driver given a confine that has one hands it every sandboxed spawn and
    /// never asks [`Confine::confine`] for one; a driver that can't hand a run fails the spawn,
    /// naming the launcher. Asked once, as the driver is given the confine.
    fn warm(&self) -> Option<Confined> {
        None
    }
    /// The frame of `command`'s run for the warm launcher, with `handed` naming what the driver
    /// passed it for the run: written whole, frames in the order of their spawns. `scratch` is
    /// as for [`Confine::confine`], and [`Confine::traced`] is asked as for a run it started.
    /// An error says why the run can't be handed; nothing runs then.
    fn hand(&self, command: &Command, scratch: &std::path::Path, handed: &Handed) -> Result<Vec<u8>, String> {
        _ = (command, scratch, handed);
        // alloc: an error path.
        Err("this sandbox has no warm launcher to hand a run to".to_owned())
    }
}

/// What a driver passed a warm launcher for one run ([`Confine::hand`]). On Windows each is a
/// handle the driver duplicated into the launcher's process, the launcher's from then on: the
/// standard ones inheritable, the job not. On Unix (the warm launcher of cargo's request 14)
/// each is the place, from 1, of its descriptor in the `SCM_RIGHTS` message the frame is sent
/// with. 0 is none.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Handed {
    /// The driver's name for the run, unique among its launcher's runs in flight, which the
    /// run's [`Answer`] carries back.
    pub run: u64,
    pub stdin: u64,
    pub stdout: u64,
    pub stderr: u64,
    /// The job its processes go in (Windows): made by the driver as for a spawn it starts
    /// itself (killed on close, a crash ending a process, the command's memory limit), and
    /// followed, killed and measured by the driver as that spawn's job is.
    pub job: u64,
}

/// What a warm launcher writes on its standard output as each run ends: [`Answer::LEN`]
/// bytes, the run and the code it ended with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Answer {
    pub run: u64,
    pub code: i32,
}

impl Answer {
    pub const LEN: usize = 12;

    /// The run, then the code, little-endian.
    pub fn to_bytes(self) -> [u8; Answer::LEN] {
        let mut out = [0; Answer::LEN];
        out[..8].copy_from_slice(&self.run.to_le_bytes());
        out[8..].copy_from_slice(&self.code.to_le_bytes());
        out
    }

    pub fn from_bytes(bytes: [u8; Answer::LEN]) -> Answer {
        let (run, code) = bytes.split_at(8);
        Answer {
            run: u64::from_le_bytes(run.try_into().expect("8 bytes")),
            code: i32::from_le_bytes(code.try_into().expect("4 bytes")),
        }
    }
}
```

And in `crates/blaze-core/src/tests.rs`, beside the `Confine` test (line ~394):

```rust
#[test]
fn an_answer_is_its_bytes_and_a_confine_hands_nothing_unless_it_says() {
    use crate::{Answer, Command, Confine, Confined, Handed, Sandbox, Traced};
    for answer in [Answer { run: 0, code: 0 }, Answer { run: u64::MAX, code: -1 }, Answer { run: 7, code: 125 }] {
        assert_eq!(Answer::from_bytes(answer.to_bytes()), answer);
    }
    assert_eq!(Answer { run: 1, code: 2 }.to_bytes(), [1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0]);
    struct Cold;
    impl Confine for Cold {
        fn confine(&self, _: &Command, _: &std::path::Path) -> Result<Confined, String> {
            Err("no".into())
        }
        fn traced(&self, _: &Command, _: &std::path::Path) -> Result<Traced, String> {
            Ok(Traced::default())
        }
    }
    let command = Command {
        program: "x".into(),
        args: Vec::new(),
        env: Vec::new(),
        dir: ".".into(),
        stdin: Vec::new(),
        sandbox: Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false },
        memory: None,
    };
    assert!(Cold.warm().is_none());
    let handed = Handed { run: 1, stdin: 0, stdout: 0, stderr: 0, job: 0 };
    assert!(Cold.hand(&command, std::path::Path::new("."), &handed).is_err());
}
```

Why: the launcher's own process creation is as costly as the program's on Windows (blaze's
large image, its DLLs, Rust's start), so a run is 2 to 3 times its bound; with the launcher warm
it is one `CreateProcessW` (docs/v2/plan/lanes/usercode/speed-of-light.md, "The Windows
sandbox"). A small client command in front of the launcher would be a process again, so the
driver has to reach a running launcher, which `Confine` can't say today. The same two methods
serve cargo's request 14 on Linux and macOS (the descriptors go with `SCM_RIGHTS`, `job` is 0).

> forwarded to mem-core (holds `crates/blaze-core/`; core is DONE) (requests, pass 20; told by tmux).

> applied by mem-core (2026-10-29): `Confine::{warm, hand}`, `Handed`, `Answer` in lib.rs; the test in tests.rs (with `services: Vec::new()`, the field `Sandbox::On` gained since).

## 2. core: the IOCP driver hands sandboxed spawns to the warm launcher (`crates/blaze-core/src/iocp.rs`, `iocp/process.rs`)

What: when the confine has a warm launcher, a sandboxed `Effect::Spawn` is handed to it instead
of started. Everything about a spawn but its process stays as it is: its job, its pipes, its
input, its output, its memory, its kill, its trace. Precisely:

1. **`Iocp::confined(confine)`** also keeps `confine.warm()` (`warm: Option<Confined>`). With
   `warm` `Some`, every spawn whose sandbox is on goes by 2 to 8; `confine()` is never asked
   (no-fallback). A spawn with its sandbox off is started as now.
2. **Starting the launcher**, at the first such spawn and the first after one ended: two pipes made
   as `process::pipe` makes them, its control (its stdin, which the driver writes) and its answers
   (its stdout, which the driver reads), the driver's ends overlapped and on the port; its stderr
   a duplicate of the driver's own, inheritable, or none. `CreateProcessW` of `warm`'s program,
   args, env (a block as `environment` makes) and dir, with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
   of those three handles, `CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT |
   EXTENDED_STARTUPINFO_PRESENT`, not suspended, and in **no job of the driver's**: Windows
   refuses to nest a job with UI limits (the run's, which the launcher sets) under a job the
   launcher is in. Its end is waited on as a spawn's (`Waiting` with a serial of its own, posting
   `KEY_EXIT`). One read of its answers is kept in flight. If it can't be started, the spawn fails
   with `"{program}: starting the sandbox's warm launcher {launcher}: {error}"`, nothing is kept,
   and the next spawn tries again.
3. **A run's handles:** its job as `process::job(command.memory)` makes it
   (`KILL_ON_JOB_CLOSE | DIE_ON_UNHANDLED_EXCEPTION`, the memory limit), followed on the port as
   now; its stdout and stderr pipes, and stdin's pipe or `NUL`, as now. Each child end is
   duplicated into the launcher, `DuplicateHandle(GetCurrentProcess(), end, launcher_process,
   &mut remote, 0, TRUE, DUPLICATE_SAME_ACCESS)`, and the driver's copy closed; the job is
   duplicated with `bInheritHandle` FALSE, and the driver keeps its own. `run` is a counter of the
   driver's. If a duplicate fails, the ones made are closed in the launcher
   (`DuplicateHandle(launcher_process, remote, null, null, 0, FALSE, DUPLICATE_CLOSE_SOURCE)`)
   and the spawn fails with the error.
4. **The frame:** the scratch directory made as for `confine` now; then
   `confine.hand(&command, &scratch, &Handed { run, stdin, stdout, stderr, job })` with the remote
   values (0 for none). An error removes the scratch directory, closes the duplicates in the
   launcher as in 3, and fails the spawn with `"{program}: no sandbox can be made here: {why}"`,
   as `confine`'s does. The frame is queued on the control pipe: one write in flight at a time,
   in the order of the spawns (as a connection's sends are).
5. **In flight:** the spawn is kept as one the driver started, without a process: it is ended by
   its answer instead of its process's end. Its output and input are moved as now. The driver keeps
   `run → tag` for the launcher's runs in flight.
6. **Answers:** each completed read of the answers pipe is cut into `Answer::LEN`-byte records,
   a record cut across two reads kept until the rest comes. Each `Answer { run, code }` takes its
   run out of the runs in flight, and its spawn has exited with `code`; it finishes as now, once
   its output has ended: `Exit { code: Some(code), signal: None, stdout, stderr, traced,
   peak_memory: peak(job) }`, with `traced` asked of the confine as now for a sandbox that
   traces, then the scratch directory removed and the job closed. An answer for a run not in
   flight is the launcher's fault: 7.
7. **Kill** of a handed spawn: the job's `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` set with
   `ActiveProcessLimit` 0 (the other limit flags and the memory limit kept), then
   `TerminateJobObject(job, KILLED)`. A program not started yet can then not start (the
   launcher's `CreateProcessW` fails, and it answers 125, `COULD_NOT`, with a report that says
   why); one started is ended. Either way the answer ends the spawn.
8. **The launcher's end**, told by its wait, an error or end of file on its answers, a failed
   write of a frame, or an answer for no run: the launcher is let go (its pipes and process
   closed, its wait unregistered, frames not yet written dropped). Every run in flight in it
   fails with `"{program}: the sandbox's warm launcher {launcher} (process {pid}) ended {how}
   before the run did"`, `how` being `with code {n}` (`GetExitCodeProcess`) or `({error})`;
   its job is closed, which kills what of it runs, and its scratch directory removed. The next
   sandboxed spawn starts a new launcher (2). A run is never started again some other way.
9. **Drop of the driver:** the control pipe is closed first: the launcher reads its end, ends
   its runs' jobs and takes their grants back (for up to 4 s), and exits. Then handed spawns are
   abandoned as started ones are: their jobs closed.
10. **Tests** (`iocp.rs`): a test confine whose `warm` is the test binary itself, run with a
    variable that makes it a tiny launcher (reads `[len u32][run u64][stdout u64][code i32]`
    frames its `hand` writes, writes `run` to the handed stdout, answers `code`, closes the
    handle): two runs answered out of order end with their own codes and outputs; a killed run
    ends; a launcher that exits with a run in flight fails that run with a message naming it,
    and the next run starts a new launcher and succeeds; a confine whose `warm` is `None` still
    goes by `confine` (the `Recording` test as it is).

Why: as 1. With it a sandboxed run on Windows is one process creation, and `windows::run`
keeps its SIDs, its lock handle and its read checks across runs: about 176 system calls a run
down to about 70 (docs/v2/plan/lanes/usercode/speed-of-light.md, "The Windows sandbox").

> forwarded to mem-core (holds `iocp.rs`, `iocp/*`) (requests, pass 20; told by tmux).
>
> applied by mem-core-warm (2026-10-29): items 1 to 10 in `iocp.rs` and `iocp/{warm,rundirs,process,sys}.rs` (not built or run). With `warm` `Some`, a `Sandbox::On` spawn is only handed: one launcher (`warm()`'s command, control pipe in, answers out, daemon's stderr, waited on with key `KEY_WARM`) in a `Slab<Warm>`; each run's job (memory limit, kill-on-close), three pipes and run directory made by the driver, the child's ends (inheritable) and the job duplicated in, `confine.hand`'s frame queued and written one at a time; the run an entry of the spawn slab with no process, found by `runs` from its answer. Kill seals the job (`ActiveProcessLimit` 0), then ends it. An answer for a run not in flight, or a frame that can't be written, ends the launcher; end of file on the answers ends it once its process has exited, so a run answered just before keeps its answer. Its runs then fail with `the sandbox's warm launcher <program> (process <pid>) ended with code <n> before the run did` (or `(<why>)`), jobs ended; the next spawn starts a new one. Nothing allocated in the driver beyond the frame (`a_warm_handed_run_allocates_only_its_frame_exit_and_output`). Launcher-side note: the driver closes the control pipe on drop or once answers end, and ends each run's job itself where the launcher ends first. Notes: docs/v2/plan/lanes/mem/core.md, "The warm launcher on Windows".

## 3. cargo: the daemon on Windows runs the IOCP driver (`crates/blaze-cargo/src/serve.rs`, `serve`)

What: the last arm (`not(any(linux, macos, bsd))`) starts `blaze_core::threads::Threads`, which
fails every `Effect::Spawn` on Windows ("processes and sockets are built on Unix only"). On
Windows it becomes:

```rust
    #[cfg(windows)]
    {
        let driver = blaze_core::iocp::Iocp::new(std::path::Path::new("/"), threads).context("starting IOCP")?;
        run(driver.confined(confine).scratch(scratch), options, steps, runner)
    }
```

with the `Threads` arm's `cfg` narrowed to `not(any(linux, macos, freebsd, openbsd, netbsd,
windows))`. (`Iocp::scratch` is cargo's request to core for every driver, cargo.md "To core,
`crates/blaze-core/src/{uring,kqueue,threads,iocp}.rs`".)

Why: without it no sandboxed run happens on Windows at all, warm launcher or not.

> forwarded to cargo (requests, pass 20; told by tmux).

## Notes for cargo (`crates/blaze-cargo/src/serve.rs`), nothing to change

- `blaze_usercode::confine::Launcher { program }` is unchanged; on Windows its `warm()` is
  `blaze sandbox --v2-serve`, which `sandbox::command` dispatches.
- On Windows the launcher is now given the daemon's `SystemRoot`, `LOCALAPPDATA`, `TMP`, `TEMP`
  and `USERPROFILE` (`confine::LAUNCHER_READS`), for `--v2` as for `--v2-serve`: with an empty
  environment it made each run's temporary directory in the Windows directory, which a user
  can't write, and made the AppContainer profile again every run.

> applied: nothing to change (notes; cargo told with 3) (requests, pass 20).
