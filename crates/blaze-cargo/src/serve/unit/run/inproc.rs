//! A dev executable on the daemon's code memory (docs/v2/plan/lanes/fastcompile/inproc.md):
//! linked with the start hook's reservations ([`dev_run`]), committed into a
//! [`CodeMemory`] instead of a file, written to cargo's path once per content key by
//! [`Program::command`], and overlaid by the memory before `main`. Each patch is
//! [`Image::commit_into`] the same memory and announced; the run's `blaze-live` asks for its
//! message over the channel a piece at a time, and says what became of it, answered by
//! [`serve`] ([`blaze_live::patch::ask`]). What a `run` and a test binary both start through.

use std::path::PathBuf;
use std::sync::Arc;

use blaze_codegen::plan::Policy;
use blaze_core::Command;
use blaze_hash::SharedMap;
use blaze_ids::ContentId;
use blaze_inproc::frame::{Frame, Kind, kinds};
use blaze_inproc::host::{self, Host, Program, Session, Started};
use blaze_inproc::{CodeMemory, CodeRange, Format};
use blaze_link::{DevRun, Image, Staged};
use blaze_live::patch::{Answer, Message, MessageView};
use blaze_units::schema::Mode;

use crate::serve::unit::{Job, codegen, graph::flag};
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

/// The package whose runtime holds the start hook (`blaze_inproc_start`). A program whose
/// build links it without its daemon side (feature `host`) runs on the daemon's code memory.
const RUNTIME: &str = "blaze-inproc";

/// Whether `unit`, built as `policy` says (its own profile, before its mode is chosen), links
/// as a dev executable: a program run on the daemon's code memory and patched while it runs.
/// That is a binary or an example's (no test, no build script), under a profile that is not a
/// release one, for this machine's own target where blaze has a code memory, whose closure
/// links blaze-inproc's runtime without its daemon side: a program opts in by depending on
/// it (through `blaze-live`). The daemon's own executable has the daemon side, and never runs
/// as one.
pub(crate) fn runs_in_process(job: &Job, unit: usize, policy: &Policy) -> Result<bool, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let target = &u.target;
    let bin = target.kind.iter().chain(&target.crate_types).any(|it| it == "bin");
    if u.mode != Mode::Build || !bin || target.is_custom_build() || job.graph.has(unit as u32, flag::TEST) {
        return Ok(false);
    }
    if Format::host().is_none() || codegen::triple(job, u) != job.input.config.host {
        return Ok(false);
    }
    if super::allows_live(&u.profile.name, policy).is_err() {
        return Ok(false);
    }
    for dep in job.graph.closure(unit as u32).into_iter().skip(1) {
        if job.graph.has(dep, flag::STD) {
            continue;
        }
        let dep = job.unit(dep as usize).map_err(|it| it.what)?;
        if job.package(dep).map_err(|it| it.what)?.name == RUNTIME {
            return Ok(!dep.features.iter().any(|it| it == "host"));
        }
    }
    Ok(false)
}

/// The daemon's dev executables by path, for a run to start from, and the host their runs
/// attach through, made at the first one's link: one for the daemon's life ([`super::super::Shared`]).
pub(crate) struct DevRuns {
    /// Where the host listens: the daemon's state directory's `inproc`.
    dir: PathBuf,
    host: parking_lot::Mutex<Option<Arc<Host>>>,
    programs: SharedMap<String, Arc<Loaded>>,
}

impl DevRuns {
    pub(crate) fn new(dir: PathBuf) -> DevRuns {
        DevRuns { dir, host: parking_lot::Mutex::new(None), programs: SharedMap::default() }
    }

    /// The host, made and listening at its first use.
    fn host(&self) -> Result<Arc<Host>, String> {
        let mut host = self.host.lock();
        if let Some(it) = &*host {
            return Ok(it.clone());
        }
        let made = Arc::new(
            Host::new(&self.dir).map_err(|it| format!("listening for dev runs in {}: {it}", self.dir.display()))?,
        );
        *host = Some(made.clone());
        Ok(made)
    }

    /// `blaze run`'s start of the executable at `path` as process `pid`: the start hook's
    /// variables when the daemon holds it as a dev executable ([`Loaded::launch`]); `None` for
    /// a file that is no dev executable (its headers reserve nothing for a code memory's
    /// arena), which runs as linked. A dev executable this daemon did not link (an earlier
    /// daemon did, and this one's builds found it done) is an error naming it, never a run of
    /// its file that edits would not reach.
    pub(crate) fn launch(&self, path: &str, pid: u32) -> Result<Option<Vec<(String, String)>>, String> {
        let _span = tracing::info_span!(target: "blaze", "dev run launch", path, pid).entered();
        // clone: a count, once per launch, so no guard of the table is held while it writes
        let loaded = self.programs.pin().get(path).cloned();
        if let Some(loaded) = loaded {
            return loaded.launch(path, pid).map(Some);
        }
        let Some(format) = Format::host() else { return Ok(None) };
        if reserves(format, std::path::Path::new(path))? {
            return Err(format!(
                "{path} is a dev executable (linked to run on the daemon's code memory), and this daemon holds no \
                 code memory of it: an earlier daemon linked it. Edit the program, or remove the file, and run again \
                 so this daemon links it"
            ));
        }
        Ok(None)
    }
}

/// Whether the executable at `path` reserves address space for a code memory's arena: a dev
/// executable's headers ([`dev_run`]). A file whose headers this host's reader can't lay out
/// is not one.
fn reserves(format: Format, path: &std::path::Path) -> Result<bool, String> {
    use std::io::Read as _;
    let what = |it: std::io::Error| format!("reading {}'s headers: {it}", path.display());
    let mut file = std::fs::File::open(path).map_err(what)?;
    // alloc: once per launch, the file's first page, then its headers
    let mut headers = vec![0; 4096];
    let read = file.read(&mut headers).map_err(what)?;
    headers.truncate(read);
    let Some(len) = blaze_inproc::image::headers_len(format, &headers) else { return Ok(false) };
    if len > headers.len() as u64 {
        let more = usize::try_from(len).map_err(|_| format!("{}'s headers say they are {len:#x} bytes", path.display()))?;
        headers.resize(more, 0);
        file.read_exact(&mut headers[read..]).map_err(what)?;
    }
    // The smallest granule of any host: only the reservations are asked of the layout, and a
    // host's own granule is a multiple of it.
    const PAGE: u64 = 4096;
    Ok(blaze_inproc::image::layout(format, &headers, PAGE).is_ok_and(|it| !it.reserved.is_empty()))
}

/// What a link of a dev executable did to the programs running it: nothing (its first), a
/// patch they apply, or a restart they were told to make, and why. The why names the function
/// or the change where the linker does.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Told {
    Nothing,
    Patched { generation: u64 },
    Restart { why: String },
}

/// `staged`, a link of the dev executable `image` at `path`, made the image: committed into
/// the code memory its runs map, as a patch they apply, when it was placed into the room the
/// image kept; else into a new code memory, and the runs of the old one told to start again.
/// `kept` is the image's program, from its last link; `cells` the counter cells its
/// [`DevRun`] reserves. Gives the file's bytes, for the store, and what the runs were told.
pub(crate) fn commit(
    runs: &DevRuns,
    path: &str,
    image: &mut Image,
    kept: &mut Option<Arc<Loaded>>,
    staged: Staged,
    cells: u64,
    refused: Option<&str>,
) -> Result<(Vec<u8>, Told), String> {
    let span = tracing::info_span!(target: "blaze", "dev run commit", path, generation = staged.generation(), full = staged.full())
        .entered();
    if let Some(loaded) = kept.as_ref().filter(|_| !staged.full()) {
        let made = loaded.patch(image, staged, refused);
        drop(span);
        return made;
    }
    let host = runs.host()?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let (loaded, bytes) = load(&host, name, image, staged, cells)?;
    let loaded = Arc::new(loaded);
    *kept = Some(loaded.clone());
    // alloc: once per dev executable laid out whole, its path
    let old = runs.programs.pin().insert(path.to_owned(), loaded).cloned();
    let told = match old {
        Some(old) if old.running() => {
            // alloc: once per restart, its reason
            let why = format!("{path} was linked again from scratch: its code is laid out anew");
            old.restart(&why);
            Told::Restart { why }
        }
        // No run maps the old memory: nothing to tell.
        Some(_) | None => Told::Nothing,
    };
    drop(span);
    Ok((bytes, told))
}

/// The newest patch of a program's image, for the runs that ask; `generation` is the last
/// committed into the memory, with or without a message.
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
    launched: Launched,
    /// The image's lineage ([`Image::lineage`]): what a restart message is for.
    lineage: [u8; 16],
    /// The executable's file name, for errors.
    name: String,
}

/// `staged`, a link of `image` laid out whole (linked with [`dev_run`]'s reservations),
/// committed into a new code memory named `name` and laid out for this host's start hook; with
/// the file's bytes, which go to the store as a written file's would. `cells` is
/// [`dev_run`]'s for the same link. A later full link of the image commits a message that
/// tells the runs of its old memory to start again; it is the caller's to send them
/// ([`Loaded::restart`]), so it is not kept here.
pub(crate) fn load(host: &Arc<Host>, name: &str, image: &mut Image, staged: Staged, cells: u64) -> Result<(Loaded, Vec<u8>), String> {
    let format = Format::host().ok_or("blaze runs no process in its code memory on this platform")?;
    let len = staged.len();
    if len > IMAGE {
        return Err(format!("the dev executable `{name}` is {len:#x} bytes, more than its code memory's {IMAGE:#x}"));
    }
    let generation = staged.generation();
    let mut memory = CodeMemory::with_cells(name, IMAGE, ARENA, cells).map_err(|it| it.to_string())?;
    image.commit_into(staged, &mut memory).map_err(|it| format!("committing `{name}` into its code memory: {it}"))?;
    memory.lay_out(format).map_err(|it| format!("laying `{name}`'s code memory out: {it}"))?;
    // alloc: once per dev executable linked, the bytes the store keeps.
    let bytes = memory.bytes(CodeRange { offset: 0, len }).map_err(|it| it.to_string())?.to_vec();
    let file = host::Image { key: ContentId::of(DOMAIN, &bytes).0, len, store: None };
    let patch: Arc<Mutex<Patch>> = Arc::new(Mutex::new(Patch { generation, message: None }));
    let failures: Failures = Arc::default();
    let (sessions, started) = crossbeam_channel::unbounded();
    let program = Arc::new(Program::new(host.clone(), Arc::new(Mutex::new(memory)), file, sessions));
    let launched: Launched = Arc::default();
    let served = Served {
        patch: patch.clone(),
        failures: failures.clone(),
        launched: launched.clone(),
        program: Arc::downgrade(&program),
    };
    std::thread::Builder::new()
        .name("blaze dev run sessions".to_owned())
        .spawn(move || sessions_of(started, &served))
        .map_err(|it| format!("starting the thread that serves `{name}`'s runs: {it}"))?;
    // alloc: once per dev executable laid out whole, its name
    Ok((Loaded { program, patch, failures, launched, lineage: image.lineage(), name: name.to_owned() }, bytes))
}

/// The runs a `blaze run` started by process ID ([`Loaded::launch`]): each one's start, which
/// the program is told has exited when its channel hangs up.
type Launched = Arc<SharedMap<u32, Started>>;

/// What the thread that serves a program's runs holds of it: the program itself only weakly,
/// so dropping the program ends the thread.
struct Served {
    patch: Arc<Mutex<Patch>>,
    failures: Failures,
    launched: Launched,
    program: std::sync::Weak<Program>,
}

impl Loaded {
    /// `staged`, a later link of `image`, committed into the memory the runs map, and announced
    /// to them; with the file's bytes for the store. A patch of a memory written out since is
    /// refused when the next run's executable is written ([`Host::write_executable`]), never a
    /// file that differs from the memory.
    ///
    /// The memory's image part is then the new file, laid out again, so a run started after
    /// is written from it and maps it ([`Program::relaid`]). What the runs were told is the
    /// patch's generation, or the restart the linker's message asks for and why (a new
    /// import, a moved thread-local, an address the loader slides in patch code).
    ///
    /// `refused` is why the runs can't take it although the linker could patch it (a changed
    /// signature or layout, `codegen::changed_assumptions`): they are told
    /// to start again instead, for that reason, unless the linker's own message already is a
    /// restart.
    pub(crate) fn patch(&self, image: &mut Image, staged: Staged, refused: Option<&str>) -> Result<(Vec<u8>, Told), String> {
        let name = &self.name;
        let format = Format::host().ok_or("blaze runs no process in its code memory on this platform")?;
        let len = staged.len();
        if len > IMAGE {
            return Err(format!("the dev executable `{name}` is {len:#x} bytes, more than its code memory's {IMAGE:#x}"));
        }
        let mut memory = lock(self.program.memory());
        let message = image
            .commit_into(staged, &mut memory)
            .map_err(|it| format!("committing a patch of `{name}` into its code memory: {it}"))?;
        let generation = image.generation();
        let message = match (message, refused) {
            // The linker's own restart says what of the link it could not take: kept.
            (Some(message), _) if matches!(Message::view(&message), Ok(MessageView::Restart { .. })) => Some(message),
            // alloc: once per refused patch, its reason
            (_, Some(why)) => Some(Message::Restart { id: self.lineage, generation, why: why.to_owned() }.encode()),
            (message, None) => message,
        };
        memory.lay_out(format).map_err(|it| format!("laying `{name}`'s code memory out after a patch: {it}"))?;
        // alloc: once per patch, the bytes the store keeps.
        let bytes = memory.bytes(CodeRange { offset: 0, len }).map_err(|it| it.to_string())?.to_vec();
        let file = host::Image { key: ContentId::of(DOMAIN, &bytes).0, len, store: None };
        self.program.relaid(&memory, file).map_err(|it| format!("`{name}`: {it}"))?;
        let told = match &message {
            None => Told::Nothing,
            Some(message) => match Message::view(message) {
                // alloc: once per restart, its reason
                Ok(MessageView::Restart { why, .. }) if self.running() => Told::Restart { why: why.to_owned() },
                Ok(MessageView::Restart { .. }) => Told::Nothing,
                Ok(_) => Told::Patched { generation },
                Err(why) => return Err(format!("the linker's message for `{name}`'s generation {generation}: {why}")),
            },
        };
        if let Some(message) = message {
            *lock(&self.patch) = Patch { generation, message: Some(Arc::from(message)) };
            memory.announce(generation);
        }
        Ok((bytes, told))
    }

    /// Whether a run `blaze run` launched still maps the memory.
    fn running(&self) -> bool {
        !self.launched.pin().is_empty()
    }

    /// Tells the runs of this memory to start again, for `why`: its image was laid out anew
    /// in another memory, which they don't map.
    pub(crate) fn restart(&self, why: &str) {
        let memory = lock(self.program.memory());
        let mut patch = lock(&self.patch);
        let generation = patch.generation + 1;
        // alloc: once per restart, its reason
        let message = Message::Restart { id: self.lineage, generation, why: why.to_owned() }.encode();
        *patch = Patch { generation, message: Some(Arc::from(message)) };
        memory.announce(generation);
    }

    /// The command that runs `command`'s program from the memory: the executable written at
    /// `command.program` when it is not there yet, and the start hook's variable set. `forks`:
    /// the process forks children that run the program (a test fork server).
    pub(crate) fn command(&self, command: Command, forks: bool) -> Result<(Command, Started), String> {
        self.program.command(command, forks).map_err(|it| it.to_string())
    }

    /// What `blaze run` adds to the environment of process `pid`, which runs the executable at
    /// `path` once it execs it: the start hook's variable, which names this host's socket and
    /// a launch waiting for that process. The executable is written there first from the
    /// memory when it is not the memory's file ([`Program::command`]). The run is the CLI's
    /// process, outside any sandbox, as a `run` of a file is.
    pub(crate) fn launch(&self, path: &str, pid: u32) -> Result<Vec<(String, String)>, String> {
        let command = Command {
            program: PathBuf::from(path),
            // alloc: none, empty: the CLI has the arguments, the environment and the directory
            args: Vec::new(),
            env: Vec::new(),
            dir: PathBuf::new(),
            stdin: Vec::new(),
            sandbox: blaze_core::Sandbox::Off,
            memory: None,
        };
        let (command, started) = self.command(command, false)?;
        self.launched.pin().insert(pid, started);
        command
            .env
            .into_iter()
            .map(|(name, value)| match (name.into_string(), value.into_string()) {
                (Ok(name), Ok(value)) => Ok((name, value)),
                _ => Err(format!("the start hook's variable for `{path}` is not UTF-8")),
            })
            .collect()
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
/// dropped; a start that failed is kept as a failure. A run [`Loaded::launch`] started is
/// told to the program as exited once its channel hangs up, so no fence waits for it.
fn sessions_of(started: crossbeam_channel::Receiver<Result<Session, blaze_inproc::Error>>, served: &Served) {
    for session in started {
        match session {
            Ok(session) => {
                let pid = session.pid;
                // clone: three counts and a weak one, once per run started
                let (patch, failures, launched, program) =
                    (served.patch.clone(), served.failures.clone(), served.launched.clone(), served.program.clone());
                let spawned = std::thread::Builder::new().name("blaze dev run asks".to_owned()).spawn(move || {
                    serve(session, &patch, &failures);
                    let started = launched.pin().remove(&pid).copied();
                    if let (Some(started), Some(program)) = (started, program.upgrade()) {
                        program.exited(started);
                    }
                });
                if let Err(error) = spawned {
                    served.failures.push(format!("serving process {pid}'s asks: {error}"));
                }
            }
            Err(error) => served.failures.push(error.to_string()),
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
