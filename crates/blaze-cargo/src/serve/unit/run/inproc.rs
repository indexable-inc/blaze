//! A dev executable on the daemon's code memory (docs/v2/plan/design/compile-time.md):
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
use blaze_cas::Key;
use blaze_inproc::frame::{Frame, Kind, kinds};
use blaze_inproc::host::{self, Host, Program, Session, Started};
use blaze_inproc::{CodeMemory, CodeRange, Format};
use blaze_link::{DevRun, Image, Staged};
use blaze_live::patch::{Answer, Message, MessageView};
use blaze_units::schema::Mode;
use blaze_units::stdlib::DEV_RUNTIME;

use crate::serve::unit::{Job, codegen, compile::output_key, graph::flag};
use blaze_live::patch::ask::{
    self, ANSWER, ANSWERED, NO_PATCH, NO_TIERS, PATCH, PATCH_ASK, TIERS_ASK,
};
use crossbeam_queue::SegQueue;
use blaze_core::lock::{Lock, LockGuard};

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

/// The reservations a dev executable is linked with: the arena, the cells of a tiered run of
/// `functions` functions (none when not tiered), and no fixed maps (no dependency image).
pub(crate) fn dev_run(tiered: Option<u64>) -> DevRun {
    let cells = tiered.map_or(0, |functions| functions.saturating_mul(CELLS_PER_FUNCTION));
    DevRun { arena: ARENA, cells, fixed: 0 }
}

/// The package whose daemon side (feature `host`) no program runs as: the daemon's own.
const DAEMON_SIDE: &str = "blaze-inproc";

/// The runtime that holds the start hook (`blaze_inproc_start`): the standard library's crate
/// blaze links into every dev executable ([`DEV_RUNTIME`], library/blaze_inproc_rt), or the
/// same source as a package of the build's (`blaze-inproc-rt`, which `blaze-inproc` and
/// `blaze-live` depend on), which the load then gives no second one.
fn is_runtime(name: &str) -> bool {
    name.len() == DEV_RUNTIME.len()
        && name
            .bytes()
            .zip(DEV_RUNTIME.bytes())
            .all(|(it, want)| it == want || (it == b'-' && want == b'_'))
}

/// Whether `unit`, built as `policy` says (its own profile, before its mode is chosen), links
/// as a dev executable: a program run on the daemon's code memory and patched while it runs.
/// That is one under a profile that is not a release one ([`super::allows_live`]) that
/// [`not_in_process`] finds nothing against. The daemon's own executable has blaze-inproc's
/// daemon side, and never runs as one.
pub(crate) fn runs_in_process(job: &Job, unit: usize, policy: &Policy) -> Result<bool, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    if super::allows_live(&u.profile.name, policy).is_err() {
        return Ok(false);
    }
    Ok(not_in_process(job, unit)?.is_none())
}

/// Why `unit` is not a dev executable whatever its profile, or `None` when it is one: a
/// binary or example's program (no build script) or a test binary (`--test`, its harness's
/// fork server forking the processes its tests run in, `attach_forking`), for this machine's
/// own target where blaze has a code memory, that links the runtime with the start hook
/// ([`is_runtime`]) and not blaze-inproc's daemon side. The load links the runtime into every
/// such program of a dev build as it links std (`blaze_units::stdlib::DEV_RUNTIME`), whatever
/// it depends on; one without it is one the load gave none: a build of a platform the runtime
/// has no standard library build for. A doctest's programs run under its runner, each a
/// process of its own, never mapped. The reason is what a dev launch refused for it says
/// ([`super::launch`]).
pub(crate) fn not_in_process(job: &Job, unit: usize) -> Result<Option<String>, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let target = &u.target;
    let test = u.mode == Mode::Test && job.graph.has(unit as u32, flag::TEST);
    let bin = u.mode == Mode::Build
        && !target.is_custom_build()
        && !job.graph.has(unit as u32, flag::TEST)
        && target.kind.iter().chain(&target.crate_types).any(|it| it == "bin");
    if !test && !bin {
        // alloc: once per unit asked, its reason
        return Ok(Some("it is neither a binary's program nor a test binary".to_owned()));
    }
    if Format::host().is_none() {
        // alloc: once per unit asked, its reason
        return Ok(Some(format!("blaze has no code memory on {}", std::env::consts::OS)));
    }
    let triple = codegen::triple(job, u);
    let host = &job.input.config.host;
    if triple != *host {
        // alloc: once per unit asked, its reason
        return Ok(Some(format!(
            "it is built for `{triple}`, and a code memory runs only this machine's own target, `{host}`"
        )));
    }
    // The runtime's own crates' library tests (`blaze-inproc-rt`'s, `blaze-inproc`'s) make
    // and attach their own hosts and code memories: run attached, they are not yet checked
    // to hold (inprocrun's F4).
    let own = &job.package(u).map_err(|it| it.what)?.name;
    if target.is_lib() && (is_runtime(own) || own == DAEMON_SIDE) {
        // alloc: once per unit asked, its reason
        return Ok(Some(format!(
            "it is the library test of `{own}`, the dev runtime's own, whose tests are not yet checked to hold when attached"
        )));
    }
    let mut runtime = false;
    for dep in job.graph.closure(unit as u32).into_iter().skip(1) {
        let std = job.graph.has(dep, flag::STD);
        let dep = job.unit(dep as usize).map_err(|it| it.what)?;
        if std {
            // The standard library's runtime: its library unit, named as its crate.
            runtime |= dep.target.is_lib() && dep.target.name == DEV_RUNTIME;
            continue;
        }
        let name = &job.package(dep).map_err(|it| it.what)?.name;
        if name == DAEMON_SIDE && dep.features.iter().any(|it| it == "host") {
            // alloc: once per unit asked, its reason
            return Ok(Some(format!(
                "it links `{DAEMON_SIDE}` with its daemon side (feature `host`), which no program runs as"
            )));
        }
        runtime |= is_runtime(name);
    }
    Ok(match runtime {
        true => None,
        // alloc: once per unit asked, its reason
        false => Some(format!(
            "it links no dev runtime (`{DEV_RUNTIME}`, the start hook): its standard library, for `{triple}` under \
             profile `{}`, was built without it",
            u.profile.name
        )),
    })
}

/// The daemon's dev executables by every path a run starts them from (the path the link makes
/// and the copy cargo runs it from, [`commit`]'s `runs_from`), and the host their runs attach
/// through, made at the first one's link: one for the daemon's life ([`super::super::Shared`]).
/// Nothing is ever taken out: a kept link image let go past its budget
/// ([`crate::serve::job`]'s `kept link images evict`) drops the image, never its program here.
pub(crate) struct DevRuns {
    /// Where the host listens: the daemon's state directory's `inproc`.
    dir: PathBuf,
    host: Lock<Option<Arc<Host>>>,
    programs: SharedMap<String, Arc<Loaded>>,
}

impl DevRuns {
    pub(crate) fn new(dir: PathBuf) -> DevRuns {
        DevRuns {
            dir,
            host: Lock::new("serve.dev_runs.host", None),
            programs: SharedMap::default(),
        }
    }

    /// The host, made and listening at its first use.
    fn host(&self) -> Result<Arc<Host>, String> {
        let mut host = self.host.lock();
        if let Some(it) = &*host {
            return Ok(it.clone());
        }
        let made = Arc::new(
            Host::new(&self.dir)
                .map_err(|it| format!("listening for dev runs in {}: {it}", self.dir.display()))?,
        );
        *host = Some(made.clone());
        Ok(made)
    }

    /// `blaze run`'s start of the executable at `path` as process `pid`: the start hook's
    /// variables when the daemon holds it as a dev executable ([`Loaded::launch`]); `None` for
    /// a file that is no dev executable (its headers reserve nothing for a code memory's
    /// arena), which runs as linked. A dev executable this daemon neither linked nor replayed
    /// from a record ([`DevRuns::adopt`]) is an error naming it, never a run of its file that
    /// edits would not reach.
    pub(crate) fn launch(
        &self,
        path: &str,
        pid: u32,
    ) -> Result<Option<Vec<(String, String)>>, String> {
        let _span = tracing::info_span!(target: "blaze", "dev run launch", path, pid).entered();
        // clone: a count, once per launch, so no guard of the table is held while it writes
        let loaded = self.programs.pin().get(path).cloned();
        if let Some(loaded) = loaded {
            return loaded.launch(path, pid).map(Some);
        }
        let Some(format) = Format::host() else { return Ok(None) };
        if reserves(format, std::path::Path::new(path))? {
            return Err(self.unheld(path));
        }
        Ok(None)
    }

    /// The dev executable at `path` as this daemon holds it, for a run or a test queue of the
    /// daemon's own to start ([`super::Launch::InProcess`]): one this daemon did not link is an
    /// error naming it ([`unheld`]), never a run of its file.
    pub(crate) fn loaded(&self, path: &str) -> Result<Arc<Loaded>, String> {
        // clone: a count, once per program or test binary started
        self.programs.pin().get(path).cloned().ok_or_else(|| self.unheld(path))
    }

    /// Why the dev executable at `path` can't start: this daemon holds no code memory under
    /// that path. One it holds under another path of the same file name (a run asking for a
    /// copy its link did not register, [`commit`]'s `runs_from`) is named, as a bug of blaze's,
    /// never taken for this one; else no build of this daemon's linked or replayed it (nothing
    /// here is ever let go).
    fn unheld(&self, path: &str) -> String {
        let name = path.rsplit('/').next().unwrap_or(path);
        let programs = self.programs.pin();
        // alloc: error path, the paths held under the same file name
        let mut held: Vec<&str> = programs
            .keys()
            .map(String::as_str)
            .filter(|it| it.rsplit('/').next() == Some(name))
            .collect();
        held.sort_unstable();
        match held.is_empty() {
            true => format!(
                "{path} is a dev executable (linked to run on the daemon's code memory), and this daemon holds no code \
                 memory of it: no build of this daemon's linked it or replayed it (this daemon lets go of no dev \
                 executable it holds). Build it again (`blaze build`, `run` or `test`) so this daemon holds it"
            ),
            false => format!(
                "{path} is a dev executable (linked to run on the daemon's code memory), and this daemon holds no code \
                 memory under that path, only under {}: the run asks for a path its link did not register as one the \
                 program runs from (a bug of blaze's: each link registers every path its program is placed at)",
                held.join(", ")
            ),
        }
    }

    /// The dev executable a build replayed from its record to `path` (and to `runs_from`, the
    /// copy a run starts it from), the output the store keeps under `key`, held as the program
    /// a run of either path starts: a record names the file its link made, and that file, not
    /// whatever this daemon linked there last, is what the build's outputs are. Held already
    /// (the program's file is the output under `key`, [`Program::file`]), nothing changes and
    /// the file is not read; otherwise its `bytes` are laid into a code memory of their own
    /// ([`adopted`]), which no kept image patches (the next link of the path holds its own
    /// program again, [`commit`]). A program it replaced that a run still maps is told to start
    /// again.
    pub(crate) fn adopt<'b>(
        &self,
        path: &str,
        runs_from: Option<&str>,
        key: Key,
        bytes: impl FnOnce() -> Result<&'b [u8], String>,
    ) -> Result<Told, String> {
        // clone: a count, once per replayed dev executable
        let held = self.programs.pin().get(path).cloned().filter(|it| it.program.file() == key.0.0);
        let loaded = match held {
            Some(it) => it,
            None => {
                let bytes = bytes()?;
                let len = bytes.len();
                let _span = tracing::info_span!(target: "blaze", "dev run adopt", path, len).entered();
                let host = self.host()?;
                let name = path.rsplit('/').next().unwrap_or(path);
                Arc::new(adopted(&host, name, bytes, key)?)
            }
        };
        let replaced = hold(self, path, runs_from, &loaded);
        // alloc: once per restart, its reason
        Ok(restart_running(replaced, || {
            format!(
                "{path} is now its build's file, kept from an earlier link, in a code memory of its own"
            )
        }))
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
        let more = usize::try_from(len)
            .map_err(|_| format!("{}'s headers say they are {len:#x} bytes", path.display()))?;
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
/// [`DevRun`] reserves. `runs_from` is the copy of the file a run starts it from when that is
/// not `path` (a test's program linked in its unit's directory and run from the profile's
/// `deps`, cargo's new layout; a binary's `target/debug/foo` of `deps/foo-0123`): the program
/// is held under both, so a run of either finds the memory this link committed into. Gives
/// the file's bytes, for the store, and what the runs were told.
#[expect(
    clippy::too_many_arguments,
    reason = "the link's image, its program and where it runs from, each its own"
)]
pub(crate) fn commit(
    runs: &DevRuns,
    path: &str,
    runs_from: Option<&str>,
    image: &mut Image,
    kept: &mut Option<Arc<Loaded>>,
    staged: Staged,
    cells: u64,
    refused: Option<&str>,
) -> Result<(Vec<u8>, Told), String> {
    let span = tracing::info_span!(target: "blaze", "dev run commit", path, generation = staged.generation(), full = staged.full())
        .entered();
    if let Some(loaded) = kept.as_ref().filter(|_| !staged.full()) {
        let (bytes, told) = loaded.patch(image, staged, refused)?;
        // A patch keeps the memory: every path it runs from names it still (and one a link
        // before this registered under no copy now does). A program it replaced there is one
        // adopted from a record since ([`DevRuns::adopt`]): its runs start again.
        let replaced = hold(runs, path, runs_from, loaded);
        // alloc: once per restart, its reason
        let held = restart_running(replaced, || {
            format!("{path} was linked again: its runs start from this link's code memory")
        });
        drop(span);
        let told = match (told, held) {
            (Told::Nothing, held) => held,
            (told, _) => told,
        };
        return Ok((bytes, told));
    }
    let host = runs.host()?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let (loaded, bytes) = load(&host, name, image, staged, cells)?;
    let loaded = Arc::new(loaded);
    *kept = Some(loaded.clone());
    let replaced = hold(runs, path, runs_from, &loaded);
    // alloc: once per restart, its reason
    let told = restart_running(replaced, || {
        format!("{path} was linked again from scratch: its code is laid out anew")
    });
    drop(span);
    Ok((bytes, told))
}

/// Each of `replaced` that a run still maps told to start again, for `why`; what the runs were
/// told. One no run maps is told nothing.
fn restart_running(replaced: Vec<Arc<Loaded>>, why: impl FnOnce() -> String) -> Told {
    let mut running = replaced.into_iter().filter(|it| it.running()).peekable();
    if running.peek().is_none() {
        return Told::Nothing;
    }
    let why = why();
    for old in running {
        old.restart(&why);
    }
    Told::Restart { why }
}

/// `loaded` held under `path` and `runs_from`, the paths its runs start it from; the other
/// programs it replaced there, each once.
fn hold(
    runs: &DevRuns,
    path: &str,
    runs_from: Option<&str>,
    loaded: &Arc<Loaded>,
) -> Vec<Arc<Loaded>> {
    let programs = runs.programs.pin();
    let mut replaced: Vec<Arc<Loaded>> = Vec::new(); // alloc: none unless a program is replaced
    for at in std::iter::once(path).chain(runs_from.filter(|it| *it != path)) {
        if programs.get(at).is_some_and(|it| Arc::ptr_eq(it, loaded)) {
            continue;
        }
        // alloc: once per dev executable laid out whole (or a copy newly registered), its path
        // clone: a count, the program held under one more path
        if let Some(old) = programs.insert(at.to_owned(), loaded.clone()).cloned()
            && !replaced.iter().any(|it| Arc::ptr_eq(it, &old))
        {
            replaced.push(old);
        }
    }
    replaced
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
    patch: Arc<Lock<Patch>>,
    failures: Failures,
    launched: Launched,
    /// The image's lineage ([`Image::lineage`]), or for a file adopted from a record
    /// ([`DevRuns::adopt`]) its descriptor's: what a restart message is for.
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
pub(crate) fn load(
    host: &Arc<Host>,
    name: &str,
    image: &mut Image,
    staged: Staged,
    cells: u64,
) -> Result<(Loaded, Vec<u8>), String> {
    let format =
        Format::host().ok_or("blaze runs no process in its code memory on this platform")?;
    let len = staged.len();
    if len > IMAGE {
        return Err(format!(
            "the dev executable `{name}` is {len:#x} bytes, more than its code memory's {IMAGE:#x}"
        ));
    }
    let generation = staged.generation();
    let mut memory =
        CodeMemory::with_cells(name, IMAGE, ARENA, cells).map_err(|it| it.to_string())?;
    image
        .commit_into(staged, &mut memory)
        .map_err(|it| format!("committing `{name}` into its code memory: {it}"))?;
    memory.lay_out(format).map_err(|it| format!("laying `{name}`'s code memory out: {it}"))?;
    // alloc: once per dev executable linked, the bytes the store keeps.
    let bytes = memory.bytes(CodeRange { offset: 0, len }).map_err(|it| it.to_string())?.to_vec();
    let file = host::Image { key: output_key(&bytes).0.0, len, store: None };
    let loaded = held(host, name, memory, file, generation, image.lineage())?;
    Ok((loaded, bytes))
}

/// `bytes`, a dev executable's file as a link made it and a record kept it, the output the
/// store keeps under `key`, laid into a new code memory named `name` as its image part, for this
/// host's start hook: the program a run of a replayed build starts ([`DevRuns::adopt`]). It is
/// linked with [`dev_run`]`(None)`'s reservations (a dev executable's link reserves no cells);
/// a file whose headers reserve no arena is refused by the layout, naming it. Its lineage and
/// generation are its live descriptor's, read back from the file ([`blaze_link::descriptor_of`]),
/// so a later link of the path tells its runs to start again as it would its own image's. No
/// patch comes to it: no image of this daemon's is its file.
fn adopted(host: &Arc<Host>, name: &str, bytes: &[u8], key: Key) -> Result<Loaded, String> {
    let format =
        Format::host().ok_or("blaze runs no process in its code memory on this platform")?;
    let len = bytes.len() as u64;
    if len > IMAGE {
        return Err(format!(
            "the dev executable `{name}` is {len:#x} bytes, more than its code memory's {IMAGE:#x}"
        ));
    }
    let descriptor = blaze_link::descriptor_of(bytes)
        .map_err(|it| format!("the dev executable `{name}`'s kept file: {it}"))?;
    let mut memory = CodeMemory::with_cells(name, IMAGE, ARENA, dev_run(None).cells)
        .map_err(|it| it.to_string())?;
    memory
        .write(0, bytes)
        .map_err(|it| format!("writing `{name}`'s kept file into its code memory: {it}"))?;
    memory
        .publish(CodeRange { offset: 0, len })
        .map_err(|it| format!("publishing `{name}`'s kept file: {it}"))?;
    memory
        .lay_out(format)
        .map_err(|it| format!("laying `{name}`'s code memory out from its kept file: {it}"))?;
    let file = host::Image { key: key.0.0, len, store: None };
    held(host, name, memory, file, descriptor.generation, descriptor.id)
}

/// `memory`, laid out, as a program whose runs attach through `host`, and the thread that
/// serves them: the newest patch at `generation`, none yet.
fn held(
    host: &Arc<Host>,
    name: &str,
    memory: CodeMemory,
    file: host::Image,
    generation: u64,
    lineage: [u8; 16],
) -> Result<Loaded, String> {
    let patch = Arc::new(Lock::new("serve.dev_run.patch", Patch { generation, message: None }));
    let failures: Failures = Arc::default();
    let (sessions, started) = crossbeam_channel::unbounded();
    let memory = Arc::new(Lock::new("serve.dev_run.memory", memory));
    let program = Arc::new(Program::new(host.clone(), memory, file, sessions));
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
    // alloc: once per dev executable held, its name
    Ok(Loaded { program, patch, failures, launched, lineage, name: name.to_owned() })
}

/// The runs a `blaze run` started by process ID ([`Loaded::launch`]): each one's start, which
/// the program is told has exited when its channel hangs up.
type Launched = Arc<SharedMap<u32, Started>>;

/// What the thread that serves a program's runs holds of it: the program itself only weakly,
/// so dropping the program ends the thread.
struct Served {
    patch: Arc<Lock<Patch>>,
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
    pub(crate) fn patch(
        &self,
        image: &mut Image,
        staged: Staged,
        refused: Option<&str>,
    ) -> Result<(Vec<u8>, Told), String> {
        let name = &self.name;
        let format =
            Format::host().ok_or("blaze runs no process in its code memory on this platform")?;
        let len = staged.len();
        if len > IMAGE {
            return Err(format!(
                "the dev executable `{name}` is {len:#x} bytes, more than its code memory's {IMAGE:#x}"
            ));
        }
        let mut memory = lock(self.program.memory());
        let message = image
            .commit_into(staged, &mut memory)
            .map_err(|it| format!("committing a patch of `{name}` into its code memory: {it}"))?;
        let generation = image.generation();
        let message = match (message, refused) {
            // The linker's own restart says what of the link it could not take: kept.
            (Some(message), _)
                if matches!(Message::view(&message), Ok(MessageView::Restart { .. })) =>
            {
                Some(message)
            }
            // alloc: once per refused patch, its reason
            (_, Some(why)) => Some(
                Message::Restart { id: self.lineage, generation, why: why.to_owned() }.encode(),
            ),
            (message, None) => message,
        };
        memory
            .lay_out(format)
            .map_err(|it| format!("laying `{name}`'s code memory out after a patch: {it}"))?;
        // alloc: once per patch, the bytes the store keeps.
        let bytes =
            memory.bytes(CodeRange { offset: 0, len }).map_err(|it| it.to_string())?.to_vec();
        let file = host::Image { key: output_key(&bytes).0.0, len, store: None };
        self.program.relaid(&memory, file).map_err(|it| format!("`{name}`: {it}"))?;
        let told = match &message {
            None => Told::Nothing,
            Some(message) => match Message::view(message) {
                // alloc: once per restart, its reason
                Ok(MessageView::Restart { why, .. }) if self.running() => {
                    Told::Restart { why: why.to_owned() }
                }
                Ok(MessageView::Restart { .. }) => Told::Nothing,
                Ok(_) => Told::Patched { generation },
                Err(why) => {
                    return Err(format!(
                        "the linker's message for `{name}`'s generation {generation}: {why}"
                    ));
                }
            },
        };
        if let Some(message) = message {
            *lock(&self.patch) = Patch { generation, message: Some(Arc::from(message)) };
            memory.announce(generation);
        }
        Ok((bytes, told))
    }

    /// Whether a run `blaze run` launched still maps the memory.
    pub(crate) fn running(&self) -> bool {
        !self.launched.pin().is_empty()
    }

    /// Tells the runs of this memory to start again, for `why`: its image was laid out anew
    /// in another memory, which they don't map.
    pub(crate) fn restart(&self, why: &str) {
        let memory = lock(self.program.memory());
        let mut patch = lock(&self.patch);
        let generation = patch.generation + 1;
        // alloc: once per restart, its reason
        let message =
            Message::Restart { id: self.lineage, generation, why: why.to_owned() }.encode();
        *patch = Patch { generation, message: Some(Arc::from(message)) };
        memory.announce(generation);
    }

    /// The command that runs `command`'s program from the memory: the executable written at
    /// `command.program` when it is not there yet, and the start hook's variable set. `forks`:
    /// the process forks children that run the program (a test fork server).
    pub(crate) fn command(
        &self,
        command: Command,
        forks: bool,
    ) -> Result<(Command, Started), String> {
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
fn sessions_of(
    started: crossbeam_channel::Receiver<Result<Session, blaze_inproc::Error>>,
    served: &Served,
) {
    for session in started {
        match session {
            Ok(session) => {
                let pid = session.pid;
                // clone: three counts and a weak one, once per run started
                let (patch, failures, launched, program) = (
                    served.patch.clone(),
                    served.failures.clone(),
                    served.launched.clone(),
                    served.program.clone(),
                );
                let spawned = std::thread::Builder::new()
                    .name("blaze dev run asks".to_owned())
                    .spawn(move || {
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
fn serve(session: Session, patch: &Lock<Patch>, failures: &SegQueue<String>) {
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
                    ask::piece_into(patch.generation, message, offset, body).map_err(|why| {
                        format!("the patch of generation {}: {why}", patch.generation)
                    })?;
                    Ok(PATCH)
                }
                _ => Ok(NO_PATCH),
            }
        }
        ANSWER => {
            match Answer::decode(frame.body).map_err(|why| format!("an answer: {why}"))? {
                Answer::Applied { .. } => {}
                Answer::RolledBack { generation, to, why } => failures.push(format!(
                    "process {pid} rolled generation {generation} back to {to}: {why}"
                )),
                Answer::Declined { generation, why } => failures
                    .push(format!("process {pid} did not apply generation {generation}: {why}")),
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

fn lock<T>(it: &Lock<T>) -> LockGuard<'_, T> {
    it.lock()
}
