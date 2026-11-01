//! A dev executable on the daemon's code memory (docs/v2/plan/lanes/fastcompile/inproc.md):
//! linked with the start hook's reservations ([`dev_run`]), committed into a
//! [`CodeMemory`] instead of a file, written to cargo's path once per content key by
//! [`Program::command`], and overlaid by the memory before `main`. Each patch is
//! [`Image::commit_into`] the same memory and announced; the run's `blaze-live` asks for its
//! message over the channel a piece at a time, and says what became of it, answered by
//! [`serve`] ([`blaze_live::patch::ask`]). What a `run` and a test binary both start through.

use std::sync::Arc;

use blaze_core::Command;
use blaze_ids::ContentId;
use blaze_inproc::frame::{Frame, Kind, kinds};
use blaze_inproc::host::{self, Host, Program, Session, Started};
use blaze_inproc::{CodeMemory, CodeRange, Format};
use blaze_link::{DevRun, Image, Staged};
use blaze_live::patch::Answer;
use blaze_live::patch::ask::{self, ANSWER, ANSWERED, NO_PATCH, NO_TIERS, PATCH, PATCH_ASK, TIERS_ASK};
use crossbeam_queue::SegQueue;
use parking_lot::{Mutex, MutexGuard};

#[cfg(test)]
mod tests;

/// The image part of a dev executable's memory: the file and every later link of it in place.
/// Pages take memory only once written.
pub(crate) const IMAGE: u64 = 1 << 32;

/// The arena the executable reserves for code placed after the link (blaze-lazy's functions,
/// tier-ups), the same size in the link's reservation and in the memory.
pub(crate) const ARENA: u64 = 1 << 30;

/// The cells of a tiered run: a pair of words for each tier a function counts in.
pub(crate) const CELLS_PER_FUNCTION: u64 = 32;

/// The content domain of a dev executable's bytes, its key for [`host::Executable`].
const DOMAIN: &str = "blaze v2 dev executable 1";

/// The reservations a dev executable is linked with: the arena, the cells of a tiered run of
/// `functions` functions (none when not tiered), and no fixed maps (no dependency image).
pub(crate) fn dev_run(tiered: Option<u64>) -> DevRun {
    let cells = tiered.map_or(0, |functions| functions.saturating_mul(CELLS_PER_FUNCTION));
    DevRun { arena: ARENA, cells, fixed: 0 }
}

/// The newest patch of a program's image, for the runs that ask.
#[derive(Default)]
struct Patch {
    generation: u64,
    message: Option<Arc<[u8]>>,
}

/// What a program's runs failed with outside their own output: a start hook that never
/// attached, an ask that could not be answered, a patch a run did not apply.
type Failures = Arc<SegQueue<String>>;

/// A dev executable in its code memory, which every run and test process of it maps.
pub(crate) struct Loaded {
    program: Arc<Program>,
    patch: Arc<Mutex<Patch>>,
    failures: Failures,
}

/// `staged`, the first link of `image` (linked with [`dev_run`]'s reservations), committed
/// into a new code memory named `name` and laid out for this host's start hook; with the
/// file's bytes, which go to the store as a written file's would. `cells` is
/// [`dev_run`]'s for the same link.
pub(crate) fn load(host: &Arc<Host>, name: &str, image: &mut Image, staged: Staged, cells: u64) -> Result<(Loaded, Vec<u8>), String> {
    let format = Format::host().ok_or("blaze runs no process in its code memory on this platform")?;
    let len = staged.len();
    if len > IMAGE {
        return Err(format!("the dev executable `{name}` is {len:#x} bytes, more than its code memory's {IMAGE:#x}"));
    }
    let mut memory = CodeMemory::with_cells(name, IMAGE, ARENA, cells).map_err(|it| it.to_string())?;
    if image.commit_into(staged, &mut memory).map_err(|it| it.to_string())?.is_some() {
        return Err(format!("the dev executable `{name}` was committed into a new code memory after a link before"));
    }
    memory.lay_out(format).map_err(|it| it.to_string())?;
    // alloc: once per dev executable linked, the bytes the store keeps.
    let bytes = memory.bytes(CodeRange { offset: 0, len }).map_err(|it| it.to_string())?.to_vec();
    let file = host::Image { key: ContentId::of(DOMAIN, &bytes).0, len, store: None };
    let patch: Arc<Mutex<Patch>> = Arc::default();
    let failures: Failures = Arc::default();
    let (sessions, started) = crossbeam_channel::unbounded();
    let program = Arc::new(Program::new(host.clone(), Arc::new(Mutex::new(memory)), file, sessions));
    let (to_patch, to_failures) = (patch.clone(), failures.clone());
    std::thread::Builder::new()
        .name("blaze dev run sessions".to_owned())
        .spawn(move || sessions_of(started, to_patch, to_failures))
        .map_err(|it| format!("starting the thread that serves `{name}`'s runs: {it}"))?;
    Ok((Loaded { program, patch, failures }, bytes))
}

impl Loaded {
    /// `staged`, a later link of `image`, committed into the memory the runs map, and announced
    /// to them; with the file's bytes for the store. A patch of a memory written out since is
    /// refused when the next run's executable is written ([`Host::write_executable`]), never a
    /// file that differs from the memory.
    pub(crate) fn patch(&mut self, image: &mut Image, staged: Staged) -> Result<Vec<u8>, String> {
        let len = staged.len();
        if len > IMAGE {
            return Err(format!("the dev executable is {len:#x} bytes, more than its code memory's {IMAGE:#x}"));
        }
        let mut memory = lock(self.program.memory());
        let message = image.commit_into(staged, &mut memory).map_err(|it| it.to_string())?;
        let generation = image.generation();
        // alloc: once per patch, the bytes the store keeps.
        let bytes = memory.bytes(CodeRange { offset: 0, len }).map_err(|it| it.to_string())?.to_vec();
        if let Some(message) = message {
            *lock(&self.patch) = Patch { generation, message: Some(message.into()) };
            memory.announce(generation);
        }
        Ok(bytes)
    }

    /// The command that runs `command`'s program from the memory: the executable written at
    /// `command.program` when it is not there yet, and the start hook's variable set. `forks`:
    /// the process forks children that run the program (a test fork server).
    pub(crate) fn command(&self, command: Command, forks: bool) -> Result<(Command, Started), String> {
        self.program.command(command, forks).map_err(|it| it.to_string())
    }

    /// The program, for a test queue that starts its binary's processes itself
    /// (`blaze_usercode::test::queue::Loaded`).
    pub(crate) fn program(&self) -> &Arc<Program> {
        &self.program
    }

    /// The process `started` and every child it forked have exited.
    pub(crate) fn exited(&self, started: Started) {
        self.program.exited(started);
    }

    /// What the runs failed with since the last call, outside their own output.
    pub(crate) fn failures(&self) -> Vec<String> {
        // alloc: the failures, few, once a run
        std::iter::from_fn(|| self.failures.pop()).collect()
    }

}

/// Hands each started run's session to a thread that serves its asks, until the program is
/// dropped; a start that failed is kept as a failure.
fn sessions_of(started: crossbeam_channel::Receiver<Result<Session, blaze_inproc::Error>>, patch: Arc<Mutex<Patch>>, failures: Failures) {
    for session in started {
        match session {
            Ok(session) => {
                let pid = session.pid;
                // clone: two counts, once per run started
                let (its_patch, its_failures) = (patch.clone(), failures.clone());
                let spawned = std::thread::Builder::new()
                    .name("blaze dev run asks".to_owned())
                    .spawn(move || serve(session, &its_patch, &its_failures));
                if let Err(error) = spawned {
                    failures.push(format!("serving process {pid}'s asks: {error}"));
                }
            }
            Err(error) => failures.push(error.to_string()),
        }
    }
}

/// Answers one run's asks until it hangs up: `blaze-live`'s, and a refusal naming the kind
/// for any other.
fn serve(session: Session, patch: &Mutex<Patch>, failures: &SegQueue<String>) {
    let pid = session.pid;
    let (mut reader, sender) = session.split();
    // alloc: once per run, the answers' body, reused
    let mut body = Vec::with_capacity(blaze_inproc::ask::MOST_BODY);
    loop {
        let Ok(frame) = reader.recv() else { return };
        let seq = frame.seq;
        let answered = answer(&frame, &lock(patch), pid, failures, &mut body);
        let sent = match answered {
            Ok(kind) => sender.send(kind, seq, &body),
            Err(why) => sender.send(kinds::REFUSED, seq, why.as_bytes()),
        };
        if let Err(error) = sent {
            let why = format!("answering process {pid}'s ask {seq}: {error}");
            let _ = sender.kill(&why);
            failures.push(why);
            return;
        }
    }
}

/// The answer to one ask of process `pid`: its kind, with its body in `body`, or why it is
/// refused. A patch the run did not apply is kept in `failures`.
fn answer(
    frame: &Frame<'_>,
    patch: &Patch,
    pid: u32,
    failures: &SegQueue<String>,
    body: &mut Vec<u8>,
) -> Result<Kind, String> {
    body.clear();
    match frame.kind {
        PATCH_ASK => {
            let (at, offset) = ask::read_patch_ask(frame.body)?;
            match &patch.message {
                Some(message) if patch.generation > at => {
                    ask::piece_into(patch.generation, message, offset, body)
                        .map_err(|why| format!("the patch of generation {}: {why}", patch.generation))?;
                    Ok(PATCH)
                }
                _ => Ok(NO_PATCH),
            }
        }
        ANSWER => {
            match Answer::decode(frame.body).map_err(|why| format!("an answer: {why}"))? {
                Answer::Applied { .. } => {}
                Answer::RolledBack { generation, to, why } => {
                    failures.push(format!("process {pid} rolled generation {generation} back to {to}: {why}"))
                }
                Answer::Declined { generation, why } => {
                    failures.push(format!("process {pid} did not apply generation {generation}: {why}"))
                }
            }
            Ok(ANSWERED)
        }
        TIERS_ASK => {
            ask::read_tiers_ask(frame.body)?;
            Ok(NO_TIERS)
        }
        Kind(kind) if kinds::LAZY.contains(&kind) => {
            Err(format!("ask {kind:#06x} is blaze-lazy's, and no lazy runner serves this run"))
        }
        Kind(kind) => Err(format!("no part of the daemon answers ask {kind:#06x}")),
    }
}

fn lock<T>(it: &Mutex<T>) -> MutexGuard<'_, T> {
    it.lock()
}
