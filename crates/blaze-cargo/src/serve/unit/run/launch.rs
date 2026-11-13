//! A program run by blaze's launcher (docs/v2/plan/design/launcher.md, steps 4 and 5): the
//! daemon's driver of `blaze_launch_host::Program`.
//!
//! `blaze run` of a host binary is a launched run ([`launched`]): its file at cargo's path is a
//! clone of blaze's launcher (`blaze-launcher`, beside blaze's own executable) and a sidecar
//! beside it names the program and this daemon's launch socket ([`link`]). Nothing is walked
//! or linked: the build makes the program's start (`Build::start`: the entry, what start
//! sections name, the statics) and hands it to the program's core ([`Program::started`]).
//! Started, the launcher connects here and asks for the rest a function at a time, each first
//! call answered in this order, the same for every crate:
//!
//! 1. the function's bind record (`blaze_launch_host::bind`), when whoever named the slot gave
//!    its key, with each static it names by the static's own: one `get` of the store each,
//!    which reads the machine's sealed layer behind its own records; a miss asks the
//!    configured pack sources (`packs::asked`) and reads again;
//! 2. made here: the need goes to the task holding the checkout's front end
//!    ([`serve_needs`], driving the core of [`super::needs`]), which runs `Build::one` and puts
//!    the function's bind record when it may have one; where no task holds it, the daemon
//!    sends itself the `blaze run` it last answered for the program ([`super::needs::Front`],
//!    the daemon's `launch_turn`), and the need waits for that build's task;
//! 3. refused by name, when that build fails, makes no start of the program, or holds no
//!    front end within [`super::needs::BUILD`] (rule 3).
//!
//! A program's whole state is kept as a start image ([`keep`]) the loop puts and marks once
//! durable; after a daemon's start the program is restored from it at its first look
//! ([`restore`], [`restored_of`]).
//!
//! `blaze build`, `check`, `test` and release profiles are not launched: they walk whole and
//! refuse ahead. What a launched run defers is `blaze_codegen::driver::start`'s list.
//!
//! What this module owns is the IO: the socket, a thread that accepts, and for each launcher
//! a thread that reads its messages and one that writes what it is told. The core decides
//! under one named lock a program; frames are queued to a launcher's writer under that lock,
//! so a launcher is written what the core decided, in the order it decided it.
//!
//! A launched program's unit is replayed from its record only where the core holds the code
//! of the build that record is of, and is built otherwise ([`not_replayed`]). A launcher
//! started by hand against a daemon that holds none of it and has no image of it marked
//! stops and says so.

use std::alloc::Global;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use blaze_api::ApiError;
use blaze_codegen::backend::{Changed, Frontend as _};
use blaze_codegen::driver::{Build, Callee, Lookup, One, OneStatic, Put, Start, runs_at_start};
use blaze_codegen::memo::Memo;
use blaze_codegen::plan::{Origin, Policy};
use blaze_codegen::{Arch, CodegenError, StaticId, Target};
use blaze_core::Waker;
use blaze_core::lock::Lock;
use blaze_core::wait::{Clock as _, Monotonic, Parker};
use blaze_hash::{HashMap, HashSet, SharedMap};
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use blaze_launch::wire::{self, Ask, Tell};
use blaze_launch_host::record::{self, Defines, Made, Named, Positions};
use blaze_launch_host::{Bound, Conn, Names, Of, Piece, Program, Said, Told, Under, bind};
use blaze_units::query::Command;
use blaze_units::schema::Mode;
use crossbeam_channel::{Receiver, Sender};
use hir_ty_codegen::v2::{Backends, BuildTime, HashSource, HirFrontend, HirInstance, HirStatic};

use super::needs::{Front, Heard, Need, Serve, Served, Step, Want};
use crate::serve::work::{LaunchKept, LaunchMarked};
use crate::serve::unit::{Job, KeptLowered, UnitReader, codegen, files::Mounted, graph::flag};

/// Whether `unit`, built as `policy` says, is a launcher's program: a binary's or example's
/// program (not a test binary, not a build script) of this machine's own target, built for
/// `blaze run` under a profile that is not a release one, on aarch64 (the launcher never ran
/// on x86-64). `blaze build`, `check` and `test` walk whole.
pub(crate) fn launched(job: &Job, unit: usize, policy: &Policy) -> Result<bool, String> {
    if !cfg!(target_arch = "aarch64") || job.input.config.invocation.command != Command::Run {
        return Ok(false);
    }
    let u = job.unit(unit).map_err(|it| it.what)?;
    let target = &u.target;
    let bin = u.mode == Mode::Build
        && !target.is_custom_build()
        && !job.graph.has(unit as u32, flag::TEST)
        && target.kind.iter().chain(&target.crate_types).any(|it| it == "bin");
    if !bin || super::allows_live(&u.profile.name, policy).is_err() {
        return Ok(false);
    }
    Ok(codegen::triple(job, u) == job.input.config.host)
}

/// Whether `unit`, whose kept record names `outputs` (path, key, whether it is executable)
/// and whose whole record key is `key` now, is a launcher's program whose record must not be
/// replayed: the unit is then built, so that the program's core holds the code of exactly
/// this build before a launcher asks for it.
///
/// A record says which file a build made, and a launched program's file is the launcher's,
/// the same bytes for every build of it: a replay gives the core nothing. So a record is
/// replayed only where the core already holds the code of the build that record is of: the
/// program's last launch link was of a build with this same record key (a key covers every
/// input of its unit, rule keys-cover-every-input). A source edited and then put back has
/// the first build's key while the core holds the second's (found by the measurement's check
/// of its output, 2026-11-12); a daemon that restarted holds none. `false` for a unit that
/// makes no program, and for one built any other way.
pub(crate) fn not_replayed(
    job: &Job,
    unit: usize,
    outputs: &[(String, blaze_cas::Key, bool)],
    key: Option<blaze_cas::Key>,
) -> Result<bool, String> {
    if !outputs.iter().any(|it| it.2) {
        return Ok(false);
    }
    let policy = codegen::toolchain(job, unit).and_then(|it| codegen::policy(job, unit, it))?;
    if policy.mode != blaze_codegen::plan::Mode::Launched {
        return Ok(false);
    }
    let plan = job.plan(unit).map_err(|it| it.what)?;
    let made = super::super::artifact::made(plan);
    let host = &job.shared.launches.host;
    Ok(outputs.iter().any(|(path, _, executable)| {
        *executable
            && made.contains(&path.as_str())
            && !host.program(&program_id(path)).is_some_and(|held| key.is_some() && *held.record.lock() == key)
    }))
}

/// What a launched build made its front end and its `Build` from, kept for the task that
/// makes the program's first calls with the checkout's front end ([`serve_needs`]), and the
/// store a launcher's bind records are read from.
pub(crate) struct Serving {
    policy: Policy,
    build_time: BuildTime,
    mounted: Arc<Mounted>,
    memo: Arc<Memo>,
    store: blaze_cas::Reader,
    packs: Option<Arc<crate::serve::packs::Packs>>,
    under: Under,
    /// The program's own symbols and what each is, sorted by name: [`Positions::defines`].
    defines: Vec<(String, Defines)>,
    /// The front end's view the build read ([`Frontend::view`]): what the next build asks
    /// [`Frontend::changed_since`] of.
    view: Option<u64>,
    /// The unit's target, which the build was owned by.
    owner: String,
    /// The checkout's root ([`Mounted::root`]): whose kept front end makes its first calls.
    root: String,
    arch: Arch,
}

/// What the program holds of a record with no bind key, to find out whether a later build
/// moved its positions: the items its span tables are of, and each table's anchor and id.
#[derive(Clone)] // clone: once a build, the program's list of them
struct Tables {
    items: Vec<ItemKey>,
    ids: Vec<(ContentId, ContentId)>,
}

/// One program this daemon built for a launcher, or restored from its start image, and the
/// launchers running it.
struct Held {
    /// The path its link was asked for: what errors name it by.
    path: String,
    /// The path the program runs from, where `blaze run` starts it: cargo's `executable`, its
    /// uplifted copy (`target/debug/hello` of `target/debug/deps/hello-0123`) where it has
    /// one, else `path`. What a build's answer names the program by (`Artifact::executable`),
    /// so what the daemon's own build for a first call is asked by ([`Launches::wants`]).
    /// Set by each build and by a restore, from its mark.
    runs_as: Lock<String>,
    core: Lock<Core>,
    /// The whole record key of the unit's build the program was last given: the build whose
    /// code the core holds ([`not_replayed`], [`Launches::held_code`]). Beside the core, so a
    /// look at it (the loop's, for a request certificate) never waits for a core's decision;
    /// written only under the core's lock, after it.
    record: Lock<Option<blaze_cas::Key>>,
    /// The newest build's, from its link; none for a program restored and not built since.
    serving: Lock<Option<Arc<Serving>>>,
    /// What the newest build's bind records and start image are made under, and its checkout:
    /// what [`keep`] keys an image by. Set by a build and by a restore.
    of: Lock<Option<(Under, String)>>,
    /// Where its bind records are read from: the newest build's store and pack sources, or the
    /// home's store for a program restored ([`Host::records`]).
    records: Lock<Option<(blaze_cas::Reader, Option<Arc<crate::serve::packs::Packs>>)>>,
    /// By slot identity, the instance of each function a record named that has no portable
    /// ask (`HirFrontend::ask_bytes` gave none): the one thing of a program a start loses.
    /// Every other function's ask is the program's own ([`Program::ask_of`]).
    unported: Lock<HashMap<ContentId, (HirInstance, Origin), Global>>,
    /// The first calls that wait for the front end ([`serve_needs`]).
    needs: (Sender<Need>, Receiver<Need>),
    /// The task serving the program's needs, while one runs: woken by each need, launcher
    /// attached or gone, and request that wants the front end.
    waker: Lock<Option<Arc<Parker>>>,
    /// How many launchers are attached.
    attached: AtomicU32,
    /// Who holds the checkout's front end for these needs ([`Front`]): a task, a build the
    /// daemon sent itself, or no one. Taken after the core's lock where both are held.
    front: Lock<Front>,
}

impl Held {
    /// A program at `path`, run from `runs_as`, holding `program`, with nothing of a build yet.
    fn new(path: &str, runs_as: &str, program: Program) -> Held {
        Held {
            path: path.to_owned(), // alloc: once per program
            runs_as: Lock::new("serve.launches.program.runs_as", runs_as.to_owned()), // alloc: once per program
            core: Lock::new(
                "serve.launches.program",
                Core {
                    program,
                    writers: Vec::new(),              // alloc: once per program
                    statics: HashSet::new_in(Global), // alloc: once per program
                    tables: HashMap::new_in(Global),  // alloc: once per program
                },
            ),
            record: Lock::new("serve.launches.program.record", None),
            serving: Lock::new("serve.launches.program.serving", None),
            of: Lock::new("serve.launches.program.of", None),
            records: Lock::new("serve.launches.program.records", None),
            unported: Lock::new("serve.launches.program.unported", HashMap::new_in(Global)), // alloc: once per program
            needs: crossbeam_channel::unbounded(), // alloc: once per program
            waker: Lock::new("serve.launches.program.waker", None),
            attached: AtomicU32::new(0),
            front: Lock::new("serve.launches.program.front", Front::new()),
        }
    }
}

struct Core {
    program: Program,
    /// Each launcher's writer, by connection.
    writers: Vec<(Conn, Sender<Vec<u8>>)>,
    /// The statics the program was given: a piece gives only the others.
    statics: HashSet<StaticId, Global>,
    /// By slot identity, of each function given with no bind key.
    tables: HashMap<ContentId, Tables, Global>,
}

impl Core {
    /// What a piece's making adds to the program's lists.
    fn keep(&mut self, held: &Held, making: Making) {
        self.statics.extend(making.statics);
        self.tables.extend(making.tables);
        let mut unported = held.unported.lock();
        for (identity, ask) in making.unported {
            unported.insert(identity, ask);
        }
    }

    /// `frames` to launcher `conn`'s writer, if it is still there.
    fn write(&self, conn: Conn, frames: Vec<u8>) {
        if frames.is_empty() {
            return;
        }
        if let Some((_, writer)) = self.writers.iter().find(|it| it.0 == conn) {
            // A writer that is gone is a launcher that ended: its reader takes it out.
            drop(writer.send(frames));
        }
    }

    /// Each of `needs`, of `held`, refused with `why` naming the program and the need.
    fn refuse(&mut self, held: &Held, needs: impl IntoIterator<Item = Need>, why: &str) {
        for need in needs {
            let mut out = Vec::new(); // alloc: one frame
            // alloc: a refusal's text, naming the program and the need
            let said = format!("{}: the first call of slot {} (identity {:?}): {why}", held.path, need.slot, need.identity);
            match self.program.refused(need.conn, need.slot, &said, &mut out) {
                Ok(()) => self.write(need.conn, out),
                Err(refusal) => tracing::error!(target: "blaze", "{}: refusing slot {}: {refusal}", held.path, need.slot),
            }
        }
    }
}

/// A planted bug the tests' oracle must catch: always `Plant::None` outside them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
#[cfg_attr(not(test), expect(blaze::unwired, reason = "wired: a planted-bug control only its tests make"))]
enum Plant {
    None,
    /// A restore takes the image under the mark's key as the build the mark names, without
    /// comparing the record the image holds.
    TrustsTheMark,
    /// A build that gives the program another record keeps the start files beside it: a
    /// launcher would start from the image of a build its file is no longer of.
    KeepsStartFile,
}

/// The launch host: the daemon's launcher programs, by id, the socket their launchers
/// connect to, and what a program is restored from after a start. Made once, for the daemon's
/// life; shared by the accept thread and each launcher's reader, which may restore a program.
struct Host {
    /// Where the socket is: the daemon's state directory's `launch`.
    dir: PathBuf,
    /// Whether the accept thread runs.
    listening: Lock<bool>,
    programs: SharedMap<[u8; 32], Arc<Held>>,
    /// The last connection's number.
    conns: std::sync::atomic::AtomicU64,
    /// blaze's launcher, read once a daemon: its bytes and the store's key of them.
    launcher: Lock<Option<(Arc<Vec<u8>>, blaze_cas::Key)>>,
    /// The journal's marks of start images, by program id: its paths, the record key of the
    /// build the image holds and the image's key. Handed over once, at the daemon's start; an
    /// entry whose restore failed is taken out.
    marked: Lock<HashMap<[u8; 32], LaunchMarked, Global>>,
    /// The home's store, which a restore reads images and a restored program its bind records
    /// from; none before the daemon's start hands it over.
    records: Lock<Option<blaze_cas::Reader>>,
    /// Start images to put and mark ([`keep`]), taken by the loop ([`Launches::kept`]).
    kept: (Sender<LaunchKept>, Receiver<LaunchKept>),
    /// Programs a first call of which asked for a build ([`wanted`]), taken by the loop
    /// ([`Launches::wants`]).
    /// Each is the program's [`Held::runs_as`], what the daemon keys the builds it answered by.
    wants: (Sender<String>, Receiver<String>),
    /// What ends the loop's wait, once a kept image or a want is queued.
    waker: Lock<Option<Waker>>,
    /// Held while a program is restored: two looks at one program restore it once.
    restoring: Lock<()>,
    /// The clock [`Front`] is given the time from.
    clock: Monotonic,
    /// What is known of each launched program's process so far, until its two spans are
    /// whole ([`ProgramTimes`]); the spans made are taken by the loop
    /// ([`Launches::program_spans`]).
    times: Lock<ProgramTimes>,
    /// Each start image [`keep`] sent and each [`restore`] read, by its check word (bytes
    /// 8..40): what a running launcher that says it holds an older image is joined from
    /// ([`joined`]).
    images: Lock<HashMap<[u8; 32], blaze_cas::Key, Global>>,
    /// The `blaze` program beside the daemon's own (the thin client a launcher runs to ask
    /// for a build), the sidecar's `blaze=`.
    blaze: String,
    /// `BLAZE_HOME` as the daemon was started with, or empty: the sidecar's `home=`.
    home: String,
}

/// The most processes [`ProgramTimes`] keeps a part of: past it the oldest is let go, said by
/// name. A launcher started by hand (no client said it became it) is one such.
const TIMES_KEPT: usize = 64;

/// What a launched program did, as the profile's spans of the build that ran it (handoff-12
/// request 2): `program: exec to Hello`, from the client's exec of it (the end of its
/// `client: answer to exec`, [`blaze_api::Timed::exec_of`]) to its launcher's `Hello` here,
/// and `program: Hello to exit`, from that `Hello` to its socket's end. Each is made once,
/// here, when its process's three instants are known, in whichever order they came: the
/// client's rows reach the daemon's loop while the launcher's reader sees `Hello` and its end.
///
/// The sans-IO core of it: the instants are given, the spans taken; the clock and the
/// threads are the driver's ([`read_launcher`], the loop).
#[derive(Default)]
pub(crate) struct ProgramTimes {
    /// By process id, oldest first: (build, exec, Hello, exit), each once known.
    open: std::collections::VecDeque<(u32, Instants)>, // alloc: a few processes, once a daemon
    /// Whole: taken by [`ProgramTimes::take`].
    made: Vec<ProgramSpans>,
}

#[derive(Default, Clone, Copy)]
struct Instants {
    /// The build and its client's exec of the program.
    exec: Option<(u64, i64)>,
    hello: Option<i64>,
    exit: Option<i64>,
}

/// One launched program's two spans, of build `build`, its process `pid`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProgramSpans {
    pub(crate) build: u64,
    pub(crate) pid: u32,
    pub(crate) timed: [blaze_api::Timed; 2],
}

/// The names of a program's spans ([`ProgramTimes`]).
pub(crate) const EXEC_TO_HELLO: &str = "program: exec to Hello";
pub(crate) const HELLO_TO_EXIT: &str = "program: Hello to exit";

impl ProgramTimes {
    /// The client of build `build` became process `pid`'s program at `exec_ns`.
    pub(crate) fn execd(&mut self, pid: u32, build: u64, exec_ns: i64) {
        self.at(pid, |it| it.exec = Some((build, exec_ns)));
    }

    /// Process `pid`'s launcher said `Hello` at `at_ns`.
    pub(crate) fn hello(&mut self, pid: u32, at_ns: i64) {
        self.at(pid, |it| it.hello = Some(at_ns));
    }

    /// Process `pid`'s launcher's socket ended at `at_ns`.
    pub(crate) fn exited(&mut self, pid: u32, at_ns: i64) {
        self.at(pid, |it| it.exit = Some(at_ns));
    }

    /// The spans made since the last take.
    pub(crate) fn take(&mut self) -> Vec<ProgramSpans> {
        std::mem::take(&mut self.made)
    }

    fn at(&mut self, pid: u32, set: impl FnOnce(&mut Instants)) {
        let index = match self.open.iter().position(|it| it.0 == pid) {
            Some(index) => index,
            None => {
                if self.open.len() == TIMES_KEPT {
                    if let Some((gone, _)) = self.open.pop_front() {
                        tracing::warn!(
                            target: "blaze",
                            pid = gone,
                            "launch: process {gone}'s program spans are let go unmade: no client said it became it, \
                             or its launcher never said Hello or ended, within the last {TIMES_KEPT} processes"
                        );
                    }
                }
                self.open.push_back((pid, Instants::default()));
                self.open.len() - 1
            }
        };
        set(&mut self.open[index].1);
        let Instants { exec: Some((build, exec)), hello: Some(hello), exit: Some(exit) } = self.open[index].1 else {
            return;
        };
        self.open.remove(index);
        let timed = |name: &str, start_ns: i64, end_ns: i64| blaze_api::Timed { name: name.to_owned(), start_ns, end_ns }; // alloc: a span's name, twice a run
        // alloc: one entry a run, taken by the loop's next turn
        self.made.push(ProgramSpans {
            build,
            pid,
            timed: [timed(EXEC_TO_HELLO, exec, hello), timed(HELLO_TO_EXIT, hello, exit)],
        });
    }
}

impl Host {
    /// The program of id `id`: held, or restored from its marked start image ([`restore`]).
    fn program(self: &Arc<Host>, id: &[u8; 32]) -> Option<Arc<Held>> {
        // clone: a count
        self.programs.pin().get(id).cloned().or_else(|| restore(self, id))
    }

    /// The loop, woken: something for it is queued.
    fn wake_loop(&self) {
        if let Some(waker) = self.waker.lock().as_ref() {
            waker.wake();
        }
    }
}

/// The daemon's launcher programs ([`Host`]): made once, for the daemon's life
/// ([`super::super::Shared`]).
pub(crate) struct Launches {
    host: Arc<Host>,
}

/// The id of the program whose link is asked for at `path`: what its sidecar names.
fn program_id(path: &str) -> [u8; 32] {
    ContentId::of("blaze launch program 1", path.as_bytes()).0
}

/// The path of the program unit `unit` makes, its one output that is no library; and the path
/// it runs from ([`Held::runs_as`]).
fn program_path(job: &Job, unit: usize) -> Result<(String, String), String> {
    let plan = job.plan(unit).map_err(|it| it.what)?;
    let u = job.unit(unit).map_err(|it| it.what)?;
    // alloc: once a build, its outputs' paths
    let paths: Vec<&str> = super::super::artifact::made(plan)
        .into_iter()
        .filter(|it| !it.ends_with(".rlib") && !it.ends_with(".rmeta"))
        .collect();
    match paths[..] {
        [one] => {
            let runs_as = super::super::artifact::executable(u, plan).ok_or_else(|| {
                // alloc: an error
                format!("unit {unit} is a launched run of {one}, and cargo names no executable of it to run")
            })?;
            Ok((one.to_owned(), runs_as)) // alloc: once a build
        }
        // alloc: an error
        _ => Err(format!(
            "unit {unit} is a launched run and makes {} programs ({}): a launched build makes one",
            paths.len(),
            paths.join(", ")
        )),
    }
}

/// A slot of a launched program, woken: the task serving its needs, if one runs.
fn wake(held: &Held) {
    if let Some(parker) = held.waker.lock().as_ref() {
        parker.unpark();
    }
}

/// The program a start image holds, checked against the mark that names it: `bytes` (the
/// image under the mark's key) restored as program `id` ([`Program::restored`], which checks
/// the image's own hash and layout), and the build it holds equal to `marked`, the record key
/// the mark names; an `Err` names both where they differ. No daemon is needed: a process that
/// reads the home's store and the journal's mark restores a program with this alone.
pub(crate) fn restored_of(bytes: &[u8], id: &[u8; 32], marked: &ContentId) -> Result<(Program, Of), String> {
    restored_planted(bytes, id, marked, Plant::None)
}

fn restored_planted(bytes: &[u8], id: &[u8; 32], marked: &ContentId, plant: Plant) -> Result<(Program, Of), String> {
    let (program, of) = Program::restored(bytes, id)?;
    if plant != Plant::TrustsTheMark && of.record != *marked {
        // alloc: an error
        return Err(format!(
            "the image holds the build of record {:?}, and its mark names the build of record {marked:?}",
            of.record
        ));
    }
    Ok((program, of))
}

/// The program of id `id` restored from the start image its mark names ([`restored_of`]), held
/// from then on with its socket bound: what a daemon's first look at a program it did not
/// build gives. `None` where no mark names it, and where the restore fails: each failure is
/// said once naming the path, the key and why, and the mark is let go of, so the program is
/// built as one no image holds (the certificate misses).
fn restore(host: &Arc<Host>, id: &[u8; 32]) -> Option<Arc<Held>> {
    let _restoring = host.restoring.lock();
    // clone: a count
    if let Some(held) = host.programs.pin().get(id).cloned() {
        return Some(held);
    }
    // clone: the mark's entry, once a restore
    let LaunchMarked { program: path, runs_as, record, key } = host.marked.lock().get(id).cloned()?;
    let span = tracing::info_span!(
        target: "blaze",
        "launch: restore a program from its start image",
        program = %path,
        bytes = tracing::field::Empty,
        slots = tracing::field::Empty,
        held = tracing::field::Empty,
    )
    .entered();
    let failed = |why: String| -> Option<Arc<Held>> {
        tracing::error!(target: "blaze", "{path}: its start image {key:?} is not restored, and the program is built: {why}");
        host.marked.lock().remove(id);
        None
    };
    let Some(store) = host.records.lock().clone() else {
        // alloc: an error
        return failed("the daemon's start handed the launch host no store".to_owned());
    };
    let guard = blaze_cas::enter();
    let bytes = match store.get(&guard, &key) {
        Ok(Some(entry)) => entry.bytes,
        // alloc: an error
        Ok(None) => return failed("the store holds no value under its key".to_owned()),
        Err(corrupt) => return failed(corrupt.to_string()),
    };
    span.record("bytes", bytes.len() as u64);
    let (program, of) = match restored_of(bytes, id, &record) {
        Ok(restored) => restored,
        Err(why) => return failed(why),
    };
    host.images.lock().insert(bytes[8..40].try_into().expect("a restored image has its check"), key);
    if let Err(why) = listen(host) {
        return failed(why);
    }
    let statics = program.statics_held();
    span.record("slots", program.awaited().len() as u64);
    span.record("held", statics.len() as u64);
    let held = Arc::new(Held::new(&path, &runs_as, program));
    {
        let mut core = held.core.lock();
        core.statics.extend(statics);
        *held.record.lock() = Some(blaze_cas::Key(of.record));
    }
    *held.of.lock() = Some((of.under, of.root));
    *held.records.lock() = Some((store, None));
    host.programs.pin().insert(*id, Arc::clone(&held));
    Some(held)
}

/// The start image of `held`, when it gives a new one ([`Program::kept`]): queued for the loop
/// to put and mark, and the loop woken. Of the newest build's record and `Under`; nothing
/// before a build or a restore set them, or before a launcher was answered.
fn keep(host: &Host, held: &Held) {
    let kept = {
        let mut core = held.core.lock();
        let record = *held.record.lock();
        // clone: what the image is of, once an image
        let of = held.of.lock().clone();
        let (Some(record), Some((under, root))) = (record, of) else { return };
        let of = Of { program: program_id(&held.path), record: record.0, under, root };
        let _span = tracing::info_span!(target: "blaze", "launch: keep the program's start image").entered();
        core.program.kept(&of).map(|it| it.map(|(key, bytes)| (record.0, key, bytes)))
    };
    match kept {
        Ok(Some((record, key, bytes))) => {
            host.images.lock().insert(bytes[8..40].try_into().expect("an encoded image has its check"), key);
            // clone (each): the paths, once an image
            let image = LaunchKept { program: held.path.clone(), runs_as: held.runs_as.lock().clone(), record, key, bytes };
            // The loop holds the receiver for the daemon's life.
            drop(host.kept.0.send(image));
            host.wake_loop();
        }
        Ok(None) => {}
        Err(why) => tracing::error!(target: "blaze", "{}: its start image is not kept: {why}", held.path),
    }
}

/// The socket, listened on from the first program's link or restore.
fn listen(host: &Arc<Host>) -> Result<PathBuf, String> {
    let path = host.dir.join("sock");
    let mut listening = host.listening.lock();
    if *listening {
        return Ok(path);
    }
    let what = |it: std::io::Error| format!("listening for launchers at {}: {it}", path.display());
    std::fs::create_dir_all(&host.dir).map_err(what)?;
    // A socket file a daemon before this one left: nothing listens on it.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
        Err(why) => return Err(what(why)),
    }
    let listener = UnixListener::bind(&path).map_err(what)?;
    let accepting = Arc::clone(host);
    std::thread::Builder::new()
        .name("blaze launch accept".to_owned())
        .spawn(move || accept(&listener, &accepting))
        .map_err(what)?;
    *listening = true;
    Ok(path)
}

/// The `blaze` beside the daemon's own program, as `blaze_client::start::program` finds it
/// beside the engine; empty where this process's own path does not read (said once).
fn own_blaze() -> String {
    match std::env::current_exe() {
        Ok(exe) => exe.with_file_name("blaze").to_string_lossy().into_owned(), // alloc: once a daemon
        Err(why) => {
            tracing::error!(target: "blaze", "the daemon's own path does not read, and launchers are given no `blaze`: {why}");
            String::new() // alloc: none, empty
        }
    }
}

impl Launches {
    pub(crate) fn new(dir: PathBuf) -> Launches {
        let host = Host {
            dir,
            listening: Lock::new("serve.launches.listening", false),
            programs: SharedMap::default(),
            conns: std::sync::atomic::AtomicU64::new(0),
            launcher: Lock::new("serve.launches.launcher", None),
            marked: Lock::new("serve.launches.marked", HashMap::new_in(Global)), // alloc: once a daemon
            records: Lock::new("serve.launches.records", None),
            kept: crossbeam_channel::unbounded(),  // alloc: once a daemon
            wants: crossbeam_channel::unbounded(), // alloc: once a daemon
            waker: Lock::new("serve.launches.waker", None),
            restoring: Lock::new("serve.launches.restoring", ()),
            clock: Monotonic::new(),
            times: Lock::new("serve.launches.times", ProgramTimes::default()),
            images: Lock::new("serve.launches.images", HashMap::new_in(Global)), // alloc: once a daemon
            blaze: own_blaze(),
            // alloc: once a daemon
            home: std::env::var_os("BLAZE_HOME").map(|it| it.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        Launches { host: Arc::new(host) } // alloc: once a daemon
    }

    /// The client of build `build` became process `pid`'s program at `exec_ns` (the end of its
    /// `client: answer to exec`, [`blaze_api::Timed::exec_of`]): told by the loop as it takes
    /// the client's rows.
    pub(crate) fn execd(&self, pid: u32, build: u64, exec_ns: i64) {
        self.host.times.lock().execd(pid, build, exec_ns);
    }

    /// The launched programs' spans made since the last take ([`ProgramTimes`]): taken by the
    /// loop, which writes them to the profile history as rows of the build each names.
    pub(crate) fn program_spans(&self) -> Vec<ProgramSpans> {
        self.host.times.lock().take()
    }

    /// How many launchers are connected now, over every program.
    pub(crate) fn connected(&self) -> usize {
        self.host.programs.pin().values().map(|it| it.attached.load(Ordering::Acquire) as usize).sum()
    }

    /// The daemon's start: the home's store, the loop's waker and the journal's marks of start
    /// images ([`crate::serve::work::Steps::launch_marked`]). Nothing is read until a program
    /// is first asked for.
    pub(crate) fn marked(
        &self,
        store: blaze_cas::Reader,
        waker: Option<Waker>,
        marked: Vec<LaunchMarked>,
    ) {
        *self.host.records.lock() = Some(store);
        *self.host.waker.lock() = waker;
        let mut held = self.host.marked.lock();
        for mark in marked {
            held.insert(program_id(&mark.program), mark);
        }
    }

    /// The start images kept since the loop last asked ([`keep`]), oldest first.
    pub(crate) fn kept(&self) -> Vec<LaunchKept> {
        self.host.kept.1.try_iter().collect() // alloc: once a turn with images kept
    }

    /// The programs whose first calls asked for a build since the loop last asked, each once,
    /// by the path each runs from ([`Held::runs_as`]);
    /// and, of every program, the needs waiting past [`super::needs::BUILD`] for a build that
    /// held nothing, refused.
    pub(crate) fn wants(&self) -> Vec<String> {
        let now = self.host.clock.now();
        for held in self.host.programs.pin().values() {
            let mut core = held.core.lock();
            let late = held.front.lock().tick(now);
            if late.is_some() {
                let why = "the build the daemon sent itself for this first call held no front end within 120 s";
                core.refuse(held, held.needs.1.try_iter(), why);
            }
        }
        // alloc: once a turn with wants
        let mut wants: Vec<String> = self.host.wants.1.try_iter().collect();
        wants.sort_unstable();
        wants.dedup();
        wants
    }

    /// The daemon's own request for the program run from `program` ([`Launches::wants`]) ended;
    /// `Err`: why none ran or it failed. Where it held no front end for the program, every need
    /// queued is refused.
    pub(crate) fn built_by_daemon(&self, program: &str, built: Result<(), String>) {
        // clone: a count
        let Some(held) = self.host.programs.pin().values().find(|it| *it.runs_as.lock() == program).cloned() else {
            return;
        };
        let mut core = held.core.lock();
        let refused = held.front.lock().built(built.is_ok());
        let why = match (refused, &built) {
            (None, _) => return,
            // alloc: a refusal's reason
            (Some(_), Err(why)) => format!("the `blaze run` the daemon sent itself for it failed: {why}"),
            // alloc: a refusal's reason
            (Some(_), Ok(())) => "the `blaze run` the daemon sent itself for it makes no start of this program".to_owned(),
        };
        core.refuse(&held, held.needs.1.try_iter(), &why);
    }

    /// blaze's launcher (`blaze-launcher`, beside blaze's own executable `blaze`): its bytes
    /// and their key, read at the first program's link.
    fn launcher(&self, blaze: &std::path::Path, path: &str) -> Result<(Arc<Vec<u8>>, blaze_cas::Key), String> {
        // clone: two counts, once a link
        if let Some(held) = self.host.launcher.lock().clone() {
            return Ok(held);
        }
        let launcher = blaze
            .parent()
            .map(|it| it.join("blaze-launcher"))
            .ok_or_else(|| format!("{} has no directory to find blaze-launcher in", blaze.display()))?;
        // Read outside the lock: two links that race read it twice and keep one.
        let file = std::fs::read(&launcher).map_err(|why| {
            format!(
                "{path} is a launched run (`blaze run`), and blaze's launcher {} does not read: {why}",
                launcher.display()
            )
        })?;
        let key = crate::serve::unit::compile::output_key(&file);
        let held = (Arc::new(file), key);
        *self.host.launcher.lock() = Some(held.clone()); // clone: two counts
        Ok(held)
    }

    /// The code the program linked at `path` holds now, by the whole record key of the unit's
    /// build it was last given ([`Held::record`]); `None` where this daemon built no such
    /// program and no start image restores one. A request certificate's output
    /// (`Steps::held_code`): the launcher's file is the same bytes at every build, so this,
    /// not the file, says what a run executes. After a start the program is restored here,
    /// at the certificate's first look, from the image its mark names.
    pub(crate) fn held_code(&self, path: &str) -> Option<crate::serve::work::HeldCode> {
        let held = self.host.program(&program_id(path))?;
        let record = *held.record.lock();
        Some(match record {
            Some(key) => crate::serve::work::HeldCode::Is(key.0),
            None => crate::serve::work::HeldCode::Unnamed,
        })
    }

    /// A request of some checkout wants its front end: every task serving needs is woken, and
    /// the one of that checkout, whose `wanted` is set, lets its front end go
    /// ([`crate::serve::work::Steps::wake_needs`]).
    pub(crate) fn wake(&self) {
        for held in self.host.programs.pin().values() {
            wake(held);
        }
    }

    /// The program linked at `path`, run from `runs_as`: held, restored from its image, or made
    /// on its first link.
    fn held(&self, path: &str, runs_as: &str) -> Arc<Held> {
        let id = program_id(path);
        if let Some(held) = self.host.program(&id) {
            let mut was = held.runs_as.lock();
            if *was != runs_as {
                *was = runs_as.to_owned(); // alloc: the path it runs from moved, once
            }
            drop(was);
            return held;
        }
        let made = Arc::new(Held::new(path, runs_as, Program::new()));
        // Two links of one path are one unit's, which builds once a job.
        self.host.programs.pin().insert(id, Arc::clone(&made));
        made
    }

    /// A build of the program linked at `path`: given to its core, and each running launcher
    /// written what the core says. Gives what each was told. The program's start image is
    /// kept after it ([`keep`]).
    fn built(&self, path: &str, prepared: Prepared, record: Option<blaze_cas::Key>) -> Result<Vec<Told>, String> {
        let held = self.held(path, &prepared.runs_as);
        let what = {
            // The lock is held for the core's decision alone: no IO, no query; what it decides
            // is queued to each launcher's writer under it, so the order of frames is the order
            // of decisions.
            let mut core = held.core.lock();
            // A build of another record than the program held (or its first here) makes the
            // start files beside it stale: removed before the record is set, so a launcher
            // never starts from an image of a build its file is no longer of. The daemon
            // places the new one once its image is durable (`daemon/launch.rs`).
            let was = *held.record.lock();
            retire_start_files(path, &prepared.runs_as, was, record, Plant::None)?;
            // Until this build is the core's, no record is the core's: a failure below leaves
            // the next build to be made, never replayed, and no certificate written of it.
            *held.record.lock() = None;
            let _span = tracing::info_span!(target: "blaze", "launch link: the program takes the build's start").entered();
            let Prepared { serving, entry, symbols, inits, piece, changed, making, .. } = prepared;
            // A build under another policy or record maker than the program's records were
            // made under starts the program anew (`Program`'s contract: a piece is of the
            // newest build): what it held is of code this build would not make.
            let other = held.of.lock().as_ref().is_some_and(|(under, _)| {
                under.policy != serving.under.policy || under.maker != serving.under.maker
            });
            if other {
                tracing::info!(target: "blaze", "{path}: built under another policy or record maker, so the program starts anew");
                core.program = Program::new();
                core.statics.clear();
                core.tables.clear();
                held.unported.lock().clear();
            }
            let told = core
                .program
                .started(entry, symbols, inits, piece, &changed)
                .map_err(|why| format!("{}: {why}", held.path))?;
            core.keep(&held, making);
            // clone: the root, once a build
            *held.of.lock() = Some((serving.under, serving.root.clone()));
            // clone (each): two counts
            *held.records.lock() = Some((serving.store.clone(), serving.packs.clone()));
            *held.serving.lock() = Some(serving);
            // The checkout's front end serves the program's needs from the task after the
            // build's answer ([`serve_needs`]) until it lets go.
            held.front.lock().held();
            *held.record.lock() = record;
            let mut what = Vec::with_capacity(told.len()); // alloc: once a build
            for (conn, frames, said) in told {
                core.write(conn, frames);
                what.push(said);
            }
            what
        };
        keep(&self.host, &held);
        Ok(what)
    }
}

/// The start files beside the program linked at `path` and run from `runs_as` removed where
/// the build of `record` is not the build of `was` the program held (or it held none): what
/// a start file is of is the record it was written from, so after this none is of another.
fn retire_start_files(
    path: &str,
    runs_as: &str,
    was: Option<blaze_cas::Key>,
    record: Option<blaze_cas::Key>,
    plant: Plant,
) -> Result<(), String> {
    if plant == Plant::KeepsStartFile || (was.is_some() && was == record) {
        return Ok(());
    }
    remove_start_files(path, runs_as)
}

/// The start files beside the program linked at `path` and run from `runs_as` removed
/// (`blaze_certify::slot::start_file`): none there is nothing; any other error fails the
/// link, naming the file.
fn remove_start_files(path: &str, runs_as: &str) -> Result<(), String> {
    for program in [path, runs_as] {
        let file = blaze_certify::slot::start_file(program);
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
            Err(why) => return Err(format!("{path}: its stale start file {file} is not removed: {why}")), // alloc: an error
        }
    }
    Ok(())
}

/// The accept thread: a reader thread for each launcher that connects.
fn accept(listener: &UnixListener, host: &Arc<Host>) {
    loop {
        // The wait: for a launcher to connect, for the daemon's life.
        let _span = tracing::info_span!(target: "blaze", "launch accept").entered();
        let socket = match listener.accept() {
            Ok((socket, _)) => socket,
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(why) => {
                tracing::error!(target: "blaze", "accepting a launcher failed: {why}");
                return;
            }
        };
        let conn = Conn(host.conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1);
        let host = Arc::clone(host);
        let started = std::thread::Builder::new()
            .name("blaze launch read".to_owned())
            .spawn(move || read_launcher(conn, socket, &host));
        if let Err(why) = started {
            tracing::error!(target: "blaze", "a launcher's reader thread did not start: {why}");
        }
    }
}

/// One whole frame's item from `from`, or `None` at its end.
fn frame(from: &mut UnixStream, pending: &mut Vec<u8>) -> Result<Option<Vec<u8>>, String> {
    loop {
        match wire::split(pending).map_err(|why| format!("a launcher's frame: {why}"))? {
            Some((item, took)) => {
                let item = item.to_vec(); // alloc: one message of a launcher's
                drop(pending.drain(..took));
                return Ok(Some(item));
            }
            None => {
                let mut chunk = [0u8; 4096];
                // The wait: for the launcher's next bytes; it ends when the launcher writes
                // or its process ends.
                match from.read(&mut chunk) {
                    Ok(0) => return Ok(None),
                    Ok(got) => pending.extend_from_slice(&chunk[..got]),
                    Err(why) if why.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(why) => return Err(format!("reading a launcher's socket: {why}")),
                }
            }
        }
    }
}

/// Where a program's bind records are read from ([`Held::records`]): a store, and the pack
/// sources a miss asks.
type Records = (blaze_cas::Reader, Option<Arc<crate::serve::packs::Packs>>);

/// The bind record under `key`, read from `records`' store, and on a miss from its pack
/// sources; `None` where none has it.
fn bound_of((store, packs): &Records, key: &blaze_cas::Key) -> Result<Option<Bound>, String> {
    let _span = tracing::info_span!(target: "blaze", "launch: read a bind record").entered();
    let guard = blaze_cas::enter();
    let corrupt = |why: blaze_cas::Corrupt| ApiError::new(format!("the bind record {key:?}: {why}"), None);
    let read = || -> Result<Option<Bound>, ApiError> {
        let entry = match store.get(&guard, key).map_err(corrupt)? {
            Some(entry) => Some(entry),
            None if crate::serve::packs::missed(key) => store.get(&guard, key).map_err(corrupt)?,
            None => None,
        };
        entry
            .map(|it| {
                bind::decode(it.bytes)
                    // alloc: an error
                    .map_err(|why| ApiError::new(format!("the bind record {key:?} does not read: {why}"), None))
            })
            .transpose()
    };
    match packs {
        Some(packs) => crate::serve::packs::asked(packs, &[*key], false, read),
        None => read(),
    }
    .map_err(|it| it.what)
}

/// The statics `bound` names, each by its bind key.
fn statics_named(bound: &Bound) -> impl Iterator<Item = (StaticId, Option<blaze_cas::Key>)> + '_ {
    bound.names.iter().filter_map(|it| match it {
        Names::Static { id, bind } => Some((*id, *bind)),
        Names::Function { .. } | Names::Symbol(_) => None,
    })
}

/// The piece of the function whose bind record is under `key`, with every static it names,
/// at any depth, that the program does not hold (`given`), each by its own bind record.
/// `None` where a record is missing or a static it needs has no bind key: the call is then
/// the front end's.
fn fetched(records: &Records, key: &blaze_cas::Key, given: &HashSet<StaticId, Global>) -> Result<Option<Piece>, String> {
    let Some(bound) = bound_of(records, key)? else { return Ok(None) };
    // alloc (each): once a first call, the statics its record names
    let mut next: Vec<(StaticId, Option<blaze_cas::Key>)> = statics_named(&bound).collect();
    let mut seen: HashSet<StaticId, Global> = HashSet::new_in(Global);
    let mut piece = Piece { functions: vec![bound], statics: Vec::new() }; // alloc: a piece of one record
    while let Some((id, bind)) = next.pop() {
        if given.contains(&id) || !seen.insert(id) {
            continue;
        }
        let Some(bind) = bind else { return Ok(None) };
        let Some(bound) = bound_of(records, &bind)? else { return Ok(None) };
        next.extend(statics_named(&bound));
        piece.statics.push((id, bound));
    }
    Ok(Some(piece))
}

/// Launcher `conn`'s first call of `slot`, which the program holds no record of: answered
/// from its bind record (with the statics it names, by theirs), else queued for the front
/// end ([`Front::want`]): a task holds it, a build the daemon sent itself will, or the daemon
/// sends itself one now ([`Launches::wants`]). A record that does not read refuses it by name.
fn wanted(
    host: &Host,
    held: &Held,
    program: [u8; 32],
    conn: Conn,
    (slot, identity, bind): (u32, ContentId, Option<blaze_cas::Key>),
) -> Result<(), String> {
    // clone (each): two counts, once a first call
    let records = held.records.lock().clone();
    if let (Some(key), Some(records)) = (bind, &records) {
        // clone: the program's statics, once a first call, so no read waits under the core
        let given = held.core.lock().statics.clone();
        let answered = fetched(records, &key, &given);
        let mut core = held.core.lock();
        let mut out = Vec::new(); // alloc: the frames of one answer
        match answered {
            Ok(Some(piece)) => {
                // alloc: once a first call, the statics it gives
                let statics: Vec<StaticId> = piece.statics.iter().map(|it| it.0).collect();
                match core.program.took(conn, slot, piece, &mut out) {
                    Ok(()) => core.statics.extend(statics),
                    Err(why) => core.program.refused(conn, slot, &why, &mut out)?,
                }
                core.write(conn, out);
                return Ok(());
            }
            Ok(None) => {}
            Err(why) => {
                // alloc: a refusal's text
                let why = format!("{}: the first call of slot {slot}: {why}", held.path);
                core.program.refused(conn, slot, &why, &mut out)?;
                core.write(conn, out);
                return Ok(());
            }
        }
    }
    let now = host.clock.now();
    let build = {
        // The lock order is the core's, then the front's: the task that lets the front end go
        // takes both, so a need is queued either before (and it asks again) or after (and this
        // asks), never between.
        let _core = held.core.lock();
        let want = held.front.lock().want(now);
        drop(held.needs.0.send(Need { program, conn, slot, identity }));
        if want == Want::Build {
            // The unit is built, not replayed, by the request the daemon sends itself.
            *held.record.lock() = None;
        }
        want == Want::Build
    };
    wake(held);
    if build {
        // clone: the path it runs from, once a build asked; the loop holds the receiver for the
        // daemon's life.
        drop(host.wants.0.send(held.runs_as.lock().clone()));
        host.wake_loop();
    }
    Ok(())
}

/// A launcher's reader: its `Hello` names the program; from then on each of its messages is
/// the core's, and what the core says goes to its writer.
fn read_launcher(conn: Conn, mut socket: UnixStream, host: &Arc<Host>) {
    let _span = tracing::info_span!(target: "blaze", "launch serve", conn = conn.0).entered();
    let mut pending = Vec::new(); // alloc: the launcher's bytes not yet a whole frame
    let hello = match frame(&mut socket, &mut pending) {
        Ok(Some(item)) => item,
        Ok(None) => return,
        Err(why) => return tracing::error!(target: "blaze", "launcher {}: {why}", conn.0),
    };
    let hello_ns = blaze_profile::clock::mono_ns();
    let (program, pid, holds, running) = match Ask::decode(&hello) {
        Ok((_, Ask::Hello { program, pid, holds, running, .. })) => (program, pid, holds, running),
        Ok((_, other)) => {
            return tracing::error!(target: "blaze", "launcher {} began with {other:?}, not Hello", conn.0);
        }
        Err(why) => return tracing::error!(target: "blaze", "launcher {}'s first message: {why}", conn.0),
    };
    let Some(held) = host.program(&program.0) else {
        // This daemon built no such program since it started and no start image of it is
        // marked: the launcher is told, and stops naming it.
        let why = "this daemon holds no code of the program (it was built by a daemon that has since \
                   stopped, and its build was kept): change a source file of it and build it again";
        let mut out = Vec::new(); // alloc: one frame
        let unheld = Tell::Unheld { why };
        if unheld.encode(1, &mut out).is_ok() {
            // The launcher may be gone already; there is then no one to tell.
            drop(socket.write_all(&out));
        }
        return;
    };
    let writer = match socket.try_clone() {
        Ok(writer) => writer,
        Err(why) => return tracing::error!(target: "blaze", "launcher {}'s socket: {why}", conn.0),
    };
    let (send, queued) = crossbeam_channel::unbounded::<Vec<u8>>();
    let written = std::thread::Builder::new().name("blaze launch write".to_owned()).spawn(move || {
        let mut writer = writer;
        // The wait: for the core's next frames for this launcher; it ends when the reader
        // drops the queue's sender, as the launcher's socket ends.
        while let Ok(frames) = queued.recv() {
            if writer.write_all(&frames).is_err() {
                return;
            }
        }
    });
    let written = match written {
        Ok(written) => written,
        Err(why) => {
            return tracing::error!(target: "blaze", "launcher {}'s writer thread did not start: {why}", conn.0);
        }
    };
    let unheld = {
        let mut core = held.core.lock();
        core.program.connected(conn);
        core.writers.push((conn, send.clone())); // clone: the queue's sender, once a launcher
        match holds {
            // A launcher that ran from an older start file while no daemon served it: bound
            // as one welcomed with that image, before its `Hello` is read.
            Some(check) if running && core.program.kept_check() != Some(check) => {
                let mut out = Vec::new(); // alloc: one frame
                match joined(host, &held, &mut core.program, conn, check, &mut out) {
                    Ok(()) => {
                        drop(send.send(out));
                        None
                    }
                    Err(why) => Some(why),
                }
            }
            _ => None,
        }
    };
    if let Some(why) = &unheld {
        tracing::info!(target: "blaze", "launcher {} of {} is not joined: {why}", conn.0, held.path);
        let mut out = Vec::new(); // alloc: one frame
        if (Tell::Unheld { why }).encode(1, &mut out).is_ok() {
            drop(send.send(out));
        }
    }
    held.attached.fetch_add(1, Ordering::AcqRel);
    wake(&held);
    // A pid past 32 bits is no process of this system's: its program has no spans.
    let pid = u32::try_from(pid).ok();
    if let Some(pid) = pid {
        host.times.lock().hello(pid, hello_ns);
    }
    // A launcher told it is unheld says nothing more that is read.
    let mut item = if unheld.is_some() { None } else { Some(hello) };
    let ended = loop {
        let Some(message) = item.take() else { break Ok(()) };
        let said = {
            let mut core = held.core.lock();
            let mut out = Vec::new(); // alloc: the frames of one answer
            let said = core.program.said(conn, &message, &mut out);
            if !out.is_empty() {
                // Queued under the lock: in the order the core decided.
                drop(send.send(out));
            }
            said
        };
        let answered = match said {
            Ok(Said::Told) => Ok(()),
            Ok(Said::Want { slot, identity, bind }) => wanted(host, &held, program.0, conn, (slot, identity, bind)),
            Err(why) => Err(why),
        };
        if let Err(why) = answered {
            break Err(why);
        }
        match frame(&mut socket, &mut pending) {
            Ok(next) => item = next,
            Err(why) => break Err(why),
        }
    };
    if let Some(pid) = pid {
        host.times.lock().exited(pid, blaze_profile::clock::mono_ns());
    }
    if let Err(why) = ended {
        tracing::error!(target: "blaze", "launcher {} of {}: {why}", conn.0, held.path);
        // The launcher is told why it is served no more, so the program stops naming it.
        let mut out = Vec::new(); // alloc: one frame
        let unheld = Tell::Unheld { why: &why };
        if unheld.encode(u64::MAX, &mut out).is_ok() {
            // Queued after everything the core decided for it: written last.
            drop(send.send(out));
        }
    }
    {
        let mut core = held.core.lock();
        core.program.disconnected(conn);
        core.writers.retain(|it| it.0 != conn);
    }
    held.attached.fetch_sub(1, Ordering::AcqRel);
    wake(&held);
    // What the launcher asked for since the last image is kept for the next start.
    keep(host, &held);
    // Both senders are gone now, so the writer ends once it wrote what was queued. The wait:
    // for that; it ends when the queue is drained or the launcher's socket refuses a write
    // (a launcher that is alive and reads nothing holds this one thread, and nothing else).
    drop(send);
    drop(written.join());
    // The launcher's process is told nothing more: closing its socket ends its code service,
    // which it says by name if a stub still fires.
    drop(socket.shutdown(std::net::Shutdown::Both));
}

/// The launcher `conn` of `held`, which says it holds the start image of check `check` and was
/// running before it connected, joined to the program from that image's table
/// ([`Program::join`]): the image is the one this daemon kept or restored under that check
/// ([`Host::images`]), read from the home's store. `Err` names the image where this daemon
/// holds none of it, and both builds where it is of another build than the program's.
fn joined(host: &Host, held: &Held, program: &mut Program, conn: Conn, check: [u8; 32], out: &mut Vec<u8>) -> Result<(), String> {
    let _span = tracing::info_span!(target: "blaze", "launch: join a launcher from its start image", program = %held.path).entered();
    let unheld = || format!("this daemon holds no start image {:?}: run the program again", ContentId(check)); // alloc: an error
    let Some(key) = host.images.lock().get(&check).copied() else { return Err(unheld()) };
    let Some(record) = *held.record.lock() else {
        // alloc: an error
        return Err(format!("{}: the program holds no build now, so no image of one is joined: run it again", held.path));
    };
    // clone: a count
    let Some(store) = host.records.lock().clone() else { return Err(unheld()) };
    let guard = blaze_cas::enter();
    let bytes = match store.get(&guard, &key) {
        Ok(Some(entry)) => entry.bytes,
        Ok(None) => return Err(unheld()),
        Err(corrupt) => return Err(format!("the start image {:?} under {key:?}: {corrupt}", ContentId(check))), // alloc: an error
    };
    let table = blaze_launch_host::start::table_of(bytes)?;
    program.join(conn, &table, &record.0, out)
}

/// The store as a build reads it: with no lock, under the reader's guard.
struct Looked<'a> {
    store: &'a blaze_cas::Reader,
    guard: &'a blaze_cas::Guard,
}

impl Lookup for Looked<'_> {
    fn read(&self, key: &blaze_cas::Key) -> Result<Option<&[u8]>, String> {
        match self.store.get(self.guard, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes)),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}

/// The span tables a record is made against, by anchor, and the program's own symbols.
struct Spans<'a> {
    tables: &'a [(ContentId, Vec<u8>)],
    defines: &'a [(String, Defines)],
}

impl Positions for Spans<'_> {
    fn of(&self, span: &blaze_format::SpanRef) -> Result<(&str, blaze_link::Position), String> {
        match self.tables.binary_search_by(|it| it.0.cmp(&span.anchor)) {
            Ok(at) => record::position_in(&self.tables[at].1, span.index),
            // alloc: an error
            Err(_) => Err(format!("no span table of anchor {:?} was read for it", span.anchor)),
        }
    }

    fn defines(&self, name: &str) -> Option<Defines> {
        let at = self.defines.binary_search_by(|it| it.0.as_str().cmp(name)).ok()?;
        Some(self.defines[at].1)
    }
}

/// What a record's making reads: the front end's database and file names, the bind keys'
/// common inputs, and the program's symbols.
struct Reads<'a, 'db> {
    db: &'a base_db::Db,
    hashes: &'a HashSource,
    frontend: &'a HirFrontend<'db>,
    under: &'a Under,
    defines: &'a [(String, Defines)],
    arch: Arch,
}

impl Reads<'_, '_> {
    /// The span tables of `anchors`, of `items`' bodies, sorted by anchor.
    fn tables(&self, items: &[ItemKey], anchors: Vec<ContentId>) -> Result<Vec<(ContentId, Vec<u8>)>, String> {
        let mut tables = hir_ty_codegen::v2::span_tables_of(self.db, self.hashes, items, anchors)?;
        tables.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(tables)
    }

    /// The bind key of the function `asked` whose describe record is under `ask`.
    fn bind(&self, asked: &HirInstance, ask: Option<blaze_cas::Key>) -> Option<blaze_cas::Key> {
        let naming = self.frontend.naming_of(asked)?;
        Some(bind::bind_key(&ask?, &naming, self.under))
    }

    /// The bind key of the static `value`, of id `id` in this build: by its own key and
    /// naming (`HirFrontend::static_key`, `static_naming_of`), as a function's is by its
    /// describe's. `None` for a static of a crate with no image, which has no record to key.
    fn bind_static(&self, value: &HirStatic, id: &StaticId) -> Option<blaze_cas::Key> {
        let key = self.frontend.static_key(value, id)?;
        let naming = self.frontend.static_naming_of(value)?;
        Some(bind::bind_key(&key, &naming, self.under))
    }

    /// The portable ask of `asked` (`HirFrontend::ask_bytes`): empty where it has none, and
    /// then it is pushed to `unported` under `slot`, kept by the launch host in memory alone.
    fn ask(&self, slot: ContentId, asked: &(HirInstance, Origin), unported: &mut Vec<(ContentId, (HirInstance, Origin))>) -> Vec<u8> {
        match self.frontend.ask_bytes(asked) {
            Some(ask) if !ask.is_empty() => ask,
            _ => {
                unported.push((slot, asked.clone())); // clone: the ask's handle
                Vec::new()
            }
        }
    }
}

/// Every anchor a position in `relocs` names.
fn anchors_in<'a>(targets: impl IntoIterator<Item = &'a Target>) -> impl Iterator<Item = ContentId> {
    targets.into_iter().filter_map(|it| match it {
        Target::Position(span) => Some(span.anchor),
        _ => None,
    })
}

/// What fills each numbered place of `made`, in order: a symbol the program defines, else
/// the function or static the place names, each function with its slot, bind key and ask
/// (one with no portable ask pushed to `unported`), each static with its bind key, of the
/// record's `statics`.
fn names_of(
    made: &Made,
    (callees, statics): (&[Callee<HirInstance>], &[(HirStatic, StaticId)]),
    reads: &Reads<'_, '_>,
    unported: &mut Vec<(ContentId, (HirInstance, Origin))>,
    what: &str,
) -> Result<Vec<Names>, String> {
    let places = made.named.len() + made.symbols.len();
    let (mut named, mut symbols) = (made.named.iter(), made.symbols.iter().peekable());
    // alloc: once a record made, its places
    let mut names = Vec::with_capacity(places);
    for place in 0..places as u32 {
        if let Some((_, name)) = symbols.next_if(|it| it.0 == place) {
            names.push(Names::Symbol(name.clone())); // clone: a symbol's name, a few a program
            continue;
        }
        names.push(match named.next() {
            Some(Named::Function(key)) => {
                let Some(callee) = callees.iter().find(|it| it.key == *key) else {
                    // alloc: an error
                    return Err(format!("{what}: its place {place} names {key:?}, which is none of its callees"));
                };
                let slot = bind::slot_identity(&callee.identity, &callee.abi);
                Names::Function {
                    key: *key,
                    slot,
                    bind: reads.bind(&callee.asked.0, callee.ask),
                    ask: reads.ask(slot, &callee.asked, unported),
                }
            }
            Some(Named::Static(id)) => {
                let Some((value, _)) = statics.iter().find(|it| it.1 == *id) else {
                    // alloc: an error
                    return Err(format!("{what}: its place {place} names static {:?}, which is none of its statics", id.0));
                };
                Names::Static { id: *id, bind: reads.bind_static(value, id) }
            }
            // alloc: an error
            None => return Err(format!("{what}: its record numbers {places} places and names fewer")),
        });
    }
    Ok(names)
}

/// What a piece's making adds to the program's lists ([`Core::keep`]).
#[derive(Default)]
struct Making {
    /// By slot identity, each function named with no portable ask ([`Held::unported`]).
    unported: Vec<(ContentId, (HirInstance, Origin))>,
    tables: Vec<(ContentId, Tables)>,
    statics: Vec<StaticId>,
}

/// Whether every name of `bound` has a bind key or is the program's own symbol: what a bind
/// record of it needs to be served after a start.
fn bound_all(bound: &Bound) -> bool {
    bound.names.iter().all(|it| match it {
        Names::Function { bind, .. } | Names::Static { bind, .. } => bind.is_some(),
        Names::Symbol(_) => true,
    })
}

/// A piece, its puts (each function's template and describe records, and the bind records of
/// those that may have one) and what its making adds to the program's lists.
struct Pieced {
    piece: Piece,
    puts: Vec<Put>,
    making: Making,
}

/// The piece of `ones` (each with what asks for it, where known) and `statics`: each one's
/// launch record made against its own span tables, bound to what fills its places. A function
/// with a describe record whose places all have a bind key or are the program's symbols is
/// put under its own bind key, so the next run is answered from the store.
fn piece_of(
    reads: &Reads<'_, '_>,
    ones: Vec<(One<HirInstance, HirStatic>, Option<HirInstance>)>,
    statics: Vec<OneStatic<HirInstance, HirStatic>>,
) -> Result<Pieced, String> {
    let _span = tracing::info_span!(target: "blaze", "launch: records of a piece", functions = ones.len(), statics = statics.len())
        .entered();
    let mut pieced = Pieced { piece: Piece::default(), puts: Vec::new(), making: Making::default() };
    for (one, asked) in ones {
        let function = &one.function;
        let data = function.data.iter().flat_map(|it| it.relocs.targets());
        let lines = function.lines.iter().map(|it| it.span.anchor);
        // alloc: once a function made, the anchors it names
        let mut anchors: Vec<ContentId> = anchors_in(function.relocs.targets().iter().chain(data)).chain(lines).collect();
        anchors.sort_unstable();
        anchors.dedup();
        let tables = reads.tables(&one.items, anchors)?;
        let made = record::function(function, &one.name, &Spans { tables: &tables, defines: reads.defines })?;
        let names = names_of(&made, (&one.callees, &one.statics), reads, &mut pieced.making.unported, &one.name)?;
        let slot = bind::slot_identity(&one.identity, &one.abi);
        let bound = Bound { made, slot, code: one.code, names };
        let own = asked.as_ref().and_then(|it| reads.bind(it, one.ask));
        match own {
            Some(key) if bound_all(&bound) => pieced.puts.push(Put {
                key,
                kind: blaze_cas::Kind::Code,
                bytes: bind::encode(&bound),
            }),
            _ => {
                let ids = tables.iter().map(|it| (it.0, hir_ty_codegen::v2::span_table_id(&it.1))).collect();
                // clone: the items its tables are of, kept to look at them again next build
                pieced.making.tables.push((slot, Tables { items: one.items.clone(), ids }));
            }
        }
        pieced.puts.extend(one.puts);
        pieced.piece.functions.push(bound);
    }
    for one in statics {
        // alloc: a static's name, for a refusal
        let name = format!("static {:?}", one.id.0);
        let data = one.value.data.iter().flat_map(|it| it.relocs.targets());
        let init: &[Target] = match &one.value.init {
            blaze_codegen::Init::Bytes { relocs, .. } => relocs.targets(),
            blaze_codegen::Init::Zeroed { .. } => &[],
        };
        // alloc: once a static made, the anchors it names
        let mut anchors: Vec<ContentId> = anchors_in(init.iter().chain(data)).collect();
        anchors.sort_unstable();
        anchors.dedup();
        let tables = reads.tables(&[one.id.0], anchors)?;
        let made = record::of_static(&one.value, &name, reads.arch, &Spans { tables: &tables, defines: reads.defines })?;
        let names = names_of(&made, (&one.callees, &one.statics), reads, &mut pieced.making.unported, &name)?;
        let bound = Bound { made, slot: ContentId::default(), code: CodeHash(ContentId::default()), names };
        // Put under its own bind key once every name of it is bound, as a function's is: a
        // first call of a function naming it after a start then reads it from the store.
        if bound_all(&bound)
            && let Some(key) = reads.bind_static(&one.asked, &one.id)
        {
            pieced.puts.push(Put { key, kind: blaze_cas::Kind::Code, bytes: bind::encode(&bound) });
        }
        pieced.puts.extend(one.puts);
        pieced.making.statics.push(one.id);
        pieced.piece.statics.push((one.id, bound));
    }
    Ok(pieced)
}

/// Each of `named` the program was not given yet, and each static those name, made alone.
fn statics_of<L: Lookup + ?Sized>(
    build: &Build<'_, Backends<'_>>,
    store: &L,
    given: &HashSet<StaticId, Global>,
    named: &[(HirStatic, StaticId)],
) -> Result<Vec<OneStatic<HirInstance, HirStatic>>, CodegenError> {
    // alloc (each): per piece, its new statics
    let mut seen: HashSet<StaticId, Global> = HashSet::new_in(Global);
    // clone (each): a static's handle, once a static made
    let mut next: Vec<(HirStatic, StaticId)> = named.iter().filter(|it| !given.contains(&it.1)).cloned().collect();
    let mut made = Vec::new();
    while let Some((value, id)) = next.pop() {
        if !seen.insert(id) {
            continue;
        }
        let one = build.one_static(&value, store)?;
        next.extend(one.statics.iter().filter(|it| !given.contains(&it.1)).cloned());
        made.push(one);
    }
    Ok(made)
}

/// What a launched build's codegen made for its link ([`prepare`]): the program's start as a
/// piece, with what the link gives [`Program::started`].
pub(crate) struct Prepared {
    path: String,
    /// The path the program runs from ([`Held::runs_as`]).
    runs_as: String,
    serving: Arc<Serving>,
    entry: ContentId,
    symbols: Vec<(String, Names)>,
    inits: Vec<StaticId>,
    piece: Piece,
    /// Every slot identity this build made anew, or asked to and could not.
    changed: Vec<ContentId>,
    making: Making,
}

impl Prepared {
    /// The program's path, which the link is asked for.
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
}

/// The launched build of `unit`: of the start `Build::start` made, and of each function a
/// launcher of the program bound or waits for with no bind key that this build may make
/// otherwise (the front end names it changed since the last build's view, or its span tables
/// moved), a piece; with the puts of all of it. `Err` within for the start's own errors,
/// which refuse the build; a function made again that fails is deferred to its first call,
/// as every instance a launched run makes is.
#[expect(clippy::too_many_arguments, reason = "what `generate` made its front end and its build from")]
pub(crate) fn prepare<L: Lookup + ?Sized>(
    job: &Job,
    unit: usize,
    reader: &UnitReader,
    mounted: &Arc<Mounted>,
    build_time: BuildTime,
    (frontend, build, policy): (&HirFrontend<'_>, &Build<'_, Backends<'_>>, &Policy),
    view: Option<u64>,
    start: Start<HirInstance, HirStatic>,
    store: &L,
) -> Result<(Prepared, Vec<Put>), String> {
    let _span = tracing::info_span!(target: "blaze", "launch: the start's piece").entered();
    let u = job.unit(unit).map_err(|it| it.what)?;
    let (path, runs_as) = program_path(job, unit)?;
    let held = job.shared.launches.host.program(&program_id(&path));
    // The program's own symbols: every root's that defines one, with its portable ask (one
    // with none is kept in memory alone, [`Held::unported`]).
    // alloc (each below): once a build
    let mut symbols: Vec<(String, Names)> = Vec::new();
    let mut unported: Vec<(ContentId, (HirInstance, Origin))> = Vec::new();
    for root in &start.declared {
        if let Some((name, _)) = &root.symbol {
            let slot = bind::slot_identity(&root.identity, &root.abi);
            let ask = match frontend.ask_bytes(&root.asked) {
                Some(ask) if !ask.is_empty() => ask,
                _ => {
                    unported.push((slot, root.asked.clone())); // clone: the ask's handle
                    Vec::new()
                }
            };
            // clone: a symbol's name, a few a program
            symbols.push((name.clone(), Names::Function { key: root.key, slot, bind: None, ask }));
        }
    }
    for value in &start.statics {
        if let Some((name, _)) = &value.symbol {
            symbols.push((name.clone(), Names::Static { id: value.id, bind: None })); // clone: as above
        }
    }
    symbols.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let under = Under { policy: policy.id(), symbols: bind::symbols_id(&symbols), maker: crate::serve::record_code()?.0 };
    let mut defines: Vec<(String, Defines)> = symbols
        .iter()
        .map(|(name, names)| {
            let kind = match names {
                Names::Static { .. } => Defines::Static,
                Names::Function { .. } | Names::Symbol(_) => Defines::Function,
            };
            (name.clone(), kind) // clone: as above
        })
        .collect();
    defines.dedup_by(|a, b| a.0 == b.0);
    let Some(first) = start.functions.first() else {
        return Err(format!("{path}: the start made no function")); // alloc: an error
    };
    let entry = bind::slot_identity(&first.identity, &first.abi);
    let arch = first.function.arch;
    let hashes = codegen::hash_source(reader.db(), reader.paths(), reader.spellings(), mounted);
    let reads = Reads { db: reader.db(), hashes: &hashes, frontend, under: &under, defines: &defines, arch };
    // Each root's bind key, now the bind keys' common inputs are known (a root's key does not
    // go into `Under::symbols`, which is by name and kind).
    for (_, names) in &mut symbols {
        if let Names::Function { key, bind, .. } = names {
            let root = start.declared.iter().find(|it| it.key == *key);
            *bind = root.and_then(|it| reads.bind(&it.asked.0, it.ask));
        }
    }
    let mut inits = Vec::new();
    for value in &start.statics {
        if let Some(section) = &value.value.section
            && runs_at_start(section)
        {
            if !(section.contains("init_array") || section.contains("__mod_init_func")) {
                return Err(blaze_report::say!(
                    blaze_report::say::At(&path),
                    ": static ",
                    blaze_report::say::Words(&format!("{:?}", value.id.0)),
                    " is in section ",
                    blaze_report::say::Code(section),
                    ", a finalizer array, which the launcher does not run yet"
                )
                .flat());
            }
            inits.push(value.id);
        }
    }

    // What asks for each function of the start: the roots, and what the start's own code names.
    let mut asked_of: HashMap<blaze_codegen::InstanceKey, HirInstance, Global> = HashMap::new_in(Global);
    let callees = start.declared.iter().chain(start.functions.iter().flat_map(|it| &it.callees));
    for callee in callees.chain(start.statics.iter().flat_map(|it| &it.callees)) {
        asked_of.entry(callee.key).or_insert_with(|| callee.asked.0.clone()); // clone: the ask's handle
    }
    // The functions a launcher bound or waits for with no bind key, made again where this
    // build may make them otherwise.
    let mut changed: Vec<ContentId> = Vec::new();
    let (mut again, mut given) = (Vec::new(), HashSet::new_in(Global));
    if let Some(held) = &held {
        let (awaited, tables) = {
            let core = held.core.lock();
            // clone (each): the program's lists, once a build, so nothing compiles under its lock
            given = core.statics.clone();
            (core.program.awaited(), core.tables.clone())
        };
        let before = held.serving.lock().as_ref().and_then(|it| it.view);
        let named = match before {
            Some(view) => frontend.changed_since(view),
            // alloc: once a build
            None => Changed::Unknown("the program's last build read no view of the front end".to_owned()),
        };
        // alloc: once a build, what the front end names
        let (all, named): (bool, HashSet<(HirInstance, Origin), Global>) = match named {
            Changed::Named { functions, .. } => {
                let mut named = HashSet::new_in(Global);
                named.extend(functions);
                (false, named)
            }
            Changed::Unknown(why) => {
                tracing::info!(target: "blaze", "{path}: every awaited function is made again: {why}");
                (true, HashSet::new_in(Global))
            }
        };
        // The asks of the awaited, copied out once a build, so nothing compiles under a lock:
        // the program's own (portable) first, else the one kept in memory.
        // alloc (each): once a build
        let asks: Vec<(ContentId, Option<Vec<u8>>, Option<(HirInstance, Origin)>)> = {
            let core = held.core.lock();
            let unported = held.unported.lock();
            awaited
                .iter()
                .map(|it| match core.program.ask_of(it) {
                    Some(bytes) => (*it, Some(bytes.to_vec()), None),
                    None => (*it, None, unported.get(it).cloned()), // clone: the ask's handle
                })
                .collect()
        };
        for (identity, bytes, kept) in asks {
            let ask = match (bytes, kept) {
                (Some(bytes), _) => frontend.ask_of(&bytes),
                (None, Some(kept)) => Ok(kept),
                // alloc: the reason
                (None, None) => Err("no ask of it is kept: it had no portable one, and the daemon started since".to_owned()),
            };
            let ask = match ask {
                Ok(ask) => ask,
                // Let go of: the program drops its code, and its next first call is a want.
                Err(why) => {
                    tracing::info!(target: "blaze", "{path}: slot identity {identity:?} is let go of, not made again: {why}");
                    changed.push(identity);
                    continue;
                }
            };
            let moved = match tables.get(&identity) {
                Some(kept) => {
                    let anchors = kept.ids.iter().map(|it| it.0).collect();
                    let now = reads.tables(&kept.items, anchors)?;
                    now.len() != kept.ids.len()
                        || now.iter().zip(&kept.ids).any(|(it, was)| hir_ty_codegen::v2::span_table_id(&it.1) != was.1)
                }
                None => false,
            };
            if !(all || moved || named.contains(&ask)) {
                continue;
            }
            changed.push(identity);
            let _span = tracing::info_span!(target: "blaze", "launch: an awaited function made again").entered();
            match build.one(&ask, store) {
                Ok(one) => again.push((one, Some(ask.0))),
                // Deferred to its first call, as every instance a launched run makes: the
                // program lets the old code go, and its next first call is refused by name.
                Err(why) => tracing::info!(target: "blaze", "{path}: making {:?} again: {}", ask.0, why.what),
            }
        }
    }
    let mut ones = Vec::with_capacity(start.functions.len() + again.len());
    for one in start.functions {
        let asked = asked_of.get(&one.key).cloned(); // clone: the ask's handle
        ones.push((one, asked));
    }
    ones.extend(again);
    for (one, _) in &ones {
        changed.push(bind::slot_identity(&one.identity, &one.abi));
    }
    changed.sort_unstable();
    changed.dedup();
    // The statics the functions made again name that the program was not given.
    // clone (each): a static's handle, once a build
    let named: Vec<(HirStatic, StaticId)> = ones.iter().flat_map(|it| it.0.statics.iter().cloned()).collect();
    for value in &start.statics {
        given.insert(value.id);
    }
    let mut statics = start.statics;
    statics.extend(statics_of(build, store, &given, &named).map_err(|it| it.what)?);
    let mut pieced = piece_of(&reads, ones, statics)?;
    pieced.making.unported.extend(unported);
    let serving = Arc::new(Serving {
        policy: policy.clone(), // clone: the policy, once a build, for the needs' builds
        build_time,
        mounted: Arc::clone(mounted),
        memo: Arc::clone(&job.memo),
        store: job.input.store.clone(), // clone: a count
        packs: job.input.packs.clone(), // clone: a count
        under,
        defines,
        view,
        owner: u.target.name.clone(), // clone: once a build
        root: mounted.root().to_owned(), // alloc: once a build
        arch,
    });
    let prepared = Prepared {
        path,
        runs_as,
        serving,
        entry,
        symbols,
        inits,
        piece: pieced.piece,
        changed,
        making: pieced.making,
    };
    Ok((prepared, pieced.puts))
}

/// What a launcher program's "link" makes: the launcher's own bytes for the file at the
/// program's path, the sidecar's for each path the program runs from, and what the running
/// launchers were told of this build.
pub(crate) struct Linked {
    /// The launcher binary, the file at cargo's path, and the store's key of it.
    pub(crate) file: Arc<Vec<u8>>,
    pub(crate) key: blaze_cas::Key,
    /// The sidecar's text, the same beside every path.
    pub(crate) sidecar: Vec<u8>,
    pub(crate) told: Vec<Told>,
    /// Whether this is the program's first build in this daemon.
    pub(crate) first: bool,
}

/// The sidecar's path for the program file at `path`.
pub(crate) fn sidecar_of(path: &str) -> String {
    format!("{path}.blaze-program") // alloc: once per path a link
}

/// "Links" the launcher program at `path`: nothing is laid out. The start codegen made
/// ([`prepare`]) is given to the program's core, the running launchers are told, and the
/// file's and the sidecar's bytes are handed back for the loop to place.
pub(crate) fn link(
    job: &Job,
    unit: usize,
    path: &str,
    generated: &codegen::Generated<'_>,
) -> Result<Linked, String> {
    let _span = tracing::info_span!(target: "blaze", "launch link", unit = %path).entered();
    let Some(prepared) = generated.launch.and_then(|it| it.lock().take()) else {
        // alloc: an error
        return Err(format!("{path}: the launched build made no start, or its start was linked already"));
    };
    if prepared.path() != path {
        // alloc: an error
        return Err(format!("{path}: the launched build's start is of {}", prepared.path()));
    }
    let launches = &job.shared.launches;
    let socket = listen(&launches.host)?;
    let (file, key) = launches.launcher(&job.shared.launcher, path)?;
    let first = launches.host.programs.pin().get(&program_id(path)).is_none();
    let record = tracing::info_span!(target: "blaze", "launch link: the unit's record key")
        .in_scope(|| super::super::compile::record_key(job, unit))
        .map_err(|it| it.what)?;
    let told = launches.built(path, prepared, record)?;
    use std::fmt::Write as _;
    let mut sidecar = String::new(); // alloc: once a link
    // A `String` takes every write.
    let _ = writeln!(sidecar, "socket={}", socket.display());
    let _ = write!(sidecar, "program=");
    for byte in program_id(path) {
        let _ = write!(sidecar, "{byte:02x}");
    }
    sidecar.push('\n');
    let host = &launches.host;
    let _ = writeln!(sidecar, "blaze={}", host.blaze);
    let state = host.dir.parent().unwrap_or(&host.dir);
    let _ = writeln!(sidecar, "state={}", state.display());
    let _ = writeln!(sidecar, "home={}", host.home);
    Ok(Linked { file, key, sidecar: sidecar.into_bytes(), told, first })
}

/// What the serving task heard next, if anything: a request that wants the front end, a
/// change of how many launchers are attached, a queued need.
fn next(programs: &[Arc<Held>], wanted: &AtomicBool, told: &mut bool, attached: &mut u32) -> Option<Heard> {
    if !*told && wanted.load(Ordering::Acquire) {
        *told = true;
        return Some(Heard::Woken { wanted: true });
    }
    let now: u32 = programs.iter().map(|it| it.attached.load(Ordering::Acquire)).sum();
    if now != *attached {
        *attached = now;
        return Some(Heard::Attached(now));
    }
    programs.iter().find_map(|it| it.needs.1.try_recv().ok()).map(Heard::Need)
}

/// The piece `need` wants, made with the checkout's front end `front`; and its puts.
fn made_here(front: &KeptLowered, held: &Held, need: &Need, puts: &mut Vec<Put>) -> Result<(Piece, Making), String> {
    // clone: a count
    let Some(serving) = held.serving.lock().clone() else {
        // alloc: an error
        return Err(format!("{}: the first call of slot {}: no build of this daemon made the program", held.path, need.slot));
    };
    let lowered = &front.lowered;
    let frontend = codegen::frontend_over(lowered.db(), lowered.paths(), lowered.spellings(), &serving.mounted, serving.build_time)
        // What the builds of this database found and witnessed, kept with its front end.
        .with_walks(&front.walks)
        .with_policy(&serving.policy)
        .with_image(false);
    // The program's portable ask, read in this front end's database; else the one kept in
    // memory by a build of this daemon.
    // alloc: the ask's bytes, once a need
    let bytes = held.core.lock().program.ask_of(&need.identity).map(<[u8]>::to_vec);
    let ask = match bytes {
        Some(bytes) => frontend
            .ask_of(&bytes)
            .map_err(|why| format!("{}: the first call of slot {}: {why}", held.path, need.slot))?,
        // clone: the ask's handle
        None => held.unported.lock().get(&need.identity).cloned().ok_or_else(|| {
            // alloc: an error
            format!("{}: the first call of slot {}: no ask is kept for its identity {:?}", held.path, need.slot, need.identity)
        })?,
    };
    let hashes = codegen::hash_source(lowered.db(), lowered.paths(), lowered.spellings(), &serving.mounted);
    let backends = Backends::default();
    let build = Build::new(&backends, &frontend, &serving.policy, &serving.memo)
        .owned_by(&serving.owner)
        .recording(&serving.store);
    let guard = blaze_cas::enter();
    let store = Looked { store: &serving.store, guard: &guard };
    let one = build.one(&ask, &store).map_err(|it| it.what)?;
    let given = held.core.lock().statics.clone(); // clone: the program's statics, once a need
    let statics = statics_of(&build, &store, &given, &one.statics).map_err(|it| it.what)?;
    let reads = Reads {
        db: lowered.db(),
        hashes: &hashes,
        frontend: &frontend,
        under: &serving.under,
        defines: &serving.defines,
        arch: serving.arch,
    };
    let pieced = piece_of(&reads, vec![(one, Some(ask.0))], statics)?; // alloc: a piece of one
    puts.extend(pieced.puts);
    Ok((pieced.piece, pieced.making))
}

/// The task after a build's answer, holding checkout `front`'s front end: makes the first
/// calls of the checkout's launched programs ([`super::needs`]) until a request wants the
/// front end, every launcher has gone, or none attached in time. Gives what it made for the
/// store. Returns at once where no program of the checkout is served.
pub(crate) fn serve_needs(front: &mut KeptLowered, launches: &Launches, wanted: &AtomicBool) -> Vec<Put> {
    let root = &front.root;
    // clone (each): a count, once a task
    let programs: Vec<Arc<Held>> = launches
        .host
        .programs
        .pin()
        .values()
        .filter(|it| it.front.lock().is_held() && it.serving.lock().as_ref().is_some_and(|it| it.root == *root))
        .cloned()
        .collect();
    let mut puts = Vec::new();
    if programs.is_empty() {
        return puts;
    }
    let _span = tracing::info_span!(target: "blaze", "launch: serve first calls", checkout = %root, programs = programs.len())
        .entered();
    let parker = Arc::new(Parker::new()); // alloc: once a task
    for held in &programs {
        *held.waker.lock() = Some(Arc::clone(&parker));
    }
    let clock = Monotonic::new();
    let mut attached: u32 = programs.iter().map(|it| it.attached.load(Ordering::Acquire)).sum();
    let mut serve = Serve::new(clock.now(), attached);
    let mut told = false;
    let mut step = serve.step(Heard::Deadline(clock.now()));
    let served = loop {
        match step {
            Step::Done(served) => break served,
            Step::Make(need) => {
                let _span = tracing::info_span!(target: "blaze", "launch: make a first call here", slot = need.slot).entered();
                if let Some(held) = programs.iter().find(|it| program_id(&it.path) == need.program) {
                    let made = made_here(front, held, &need, &mut puts);
                    let mut core = held.core.lock();
                    let mut out = Vec::new(); // alloc: the frames of one answer
                    let answered = match made {
                        Ok((piece, making)) => {
                            let took = core.program.took(need.conn, need.slot, piece, &mut out);
                            if took.is_ok() {
                                core.keep(held, making);
                            }
                            took
                        }
                        Err(why) => Err(why),
                    };
                    if let Err(why) = answered
                        && let Err(refusal) = core.program.refused(need.conn, need.slot, &why, &mut out)
                    {
                        tracing::error!(target: "blaze", "{}: refusing slot {}: {refusal}", held.path, need.slot);
                    }
                    core.write(need.conn, out);
                }
                step = serve.step(Heard::Made);
            }
            Step::Wait { until } => {
                if let Some(heard) = next(&programs, wanted, &mut told, &mut attached) {
                    step = serve.step(heard);
                    continue;
                }
                let waited = tracing::info_span!(target: "blaze", "launch: wait for a first call")
                    .in_scope(|| parker.park("launch: wait for a first call", until.finite(), &clock));
                if let Err(why) = waited {
                    tracing::error!(target: "blaze", "{root}: the wait for a launcher's first call failed: {why:?}");
                    break Served::Detached;
                }
                step = match next(&programs, wanted, &mut told, &mut attached) {
                    Some(heard) => serve.step(heard),
                    None => serve.step(Heard::Deadline(clock.now())),
                };
            }
        }
    };
    for held in &programs {
        *held.waker.lock() = None;
    }
    match served {
        // The front end goes to the request that wants it; what was not made waits for the
        // task after its build, which holds the front end again.
        Served::Wanted => {
            for need in serve.left() {
                if let Some(held) = programs.iter().find(|it| program_id(&it.path) == need.program) {
                    drop(held.needs.0.send(need));
                }
            }
        }
        // No task holds the front end for these programs any more ([`Front::let_go`]): a need
        // still queued is queued again, and asks the daemon for a build ([`Front::want`]), as a
        // later one does ([`wanted`]).
        Served::Detached | Served::NeverAttached => {
            let left = serve.left();
            let now = launches.host.clock.now();
            for held in &programs {
                let id = program_id(&held.path);
                let build = {
                    let _core = held.core.lock();
                    let mut front = held.front.lock();
                    front.let_go();
                    for need in left.iter().filter(|it| it.program == id) {
                        drop(held.needs.0.send(*need));
                    }
                    let build = !held.needs.1.is_empty() && front.want(now) == Want::Build;
                    if build {
                        *held.record.lock() = None;
                    }
                    build
                };
                if build {
                    // clone: the path it runs from, once a build asked
                    drop(launches.host.wants.0.send(held.runs_as.lock().clone()));
                    launches.host.wake_loop();
                }
            }
        }
    }
    puts
}

#[cfg(test)]
mod tests {
    use blaze_codegen::{CompiledFunction, InstanceKey, Relocation, Relocs};

    use super::*;

    /// The invariant: a launched program's process, given its client's exec, its `Hello` and
    /// its end in any of the six orders, makes `program: exec to Hello` and `program: Hello to
    /// exit` exactly once each, of the build the client named, with the instants given; and
    /// nothing more however its instants are given again after. Control: the same with the
    /// client's exec dropped makes no span (a run whose client said nothing is not timed from
    /// a guess).
    #[test]
    fn a_programs_spans_are_made_once_whatever_order_its_instants_come_in() {
        let orders: [[u8; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
        for order in orders {
            for dropped in [None, Some(0u8)] {
                let mut times = ProgramTimes::default();
                for step in order.into_iter().filter(|it| Some(*it) != dropped) {
                    match step {
                        0 => times.execd(41, 7, 100),
                        1 => times.hello(41, 130),
                        _ => times.exited(41, 900),
                    }
                }
                // Another process's instants are not this one's.
                times.hello(42, 140);
                let made = times.take();
                if dropped.is_some() {
                    assert_eq!(made, Vec::new(), "the control, {order:?}: no exec, no span");
                    continue;
                }
                let span = |name: &str, start_ns, end_ns| blaze_api::Timed { name: name.to_owned(), start_ns, end_ns };
                let want = ProgramSpans {
                    build: 7,
                    pid: 41,
                    timed: [span(EXEC_TO_HELLO, 100, 130), span(HELLO_TO_EXIT, 130, 900)],
                };
                assert_eq!(made, vec![want], "{order:?}");
                // The process ran once: an instant said again starts no second span.
                times.exited(41, 950);
                assert_eq!(times.take(), Vec::new(), "{order:?}");
            }
        }
    }

    /// The invariant: the processes kept part-timed are bounded ([`TIMES_KEPT`]): the oldest
    /// is let go first, and a process still kept is made whole as before.
    #[test]
    fn the_part_timed_processes_kept_are_bounded() {
        let mut times = ProgramTimes::default();
        for pid in 0..TIMES_KEPT as u32 + 1 {
            times.hello(pid, 1);
        }
        assert_eq!(times.open.len(), TIMES_KEPT);
        assert!(times.open.iter().all(|it| it.0 != 0), "the oldest, 0, is let go");
        times.execd(TIMES_KEPT as u32, 3, 0);
        times.exited(TIMES_KEPT as u32, 2);
        assert_eq!(times.take().len(), 1);
    }

    const PATH: &str = "/checkout/target/debug/hello";

    fn id(byte: u8) -> ContentId {
        ContentId([byte; 32])
    }

    /// The start image a program of one function, `main`, keeps as of build `record`, once a
    /// launcher's `Hello` was answered.
    fn image(record: ContentId) -> Vec<u8> {
        let compiled = CompiledFunction {
            key: InstanceKey(id(1)),
            arch: Arch::Aarch64,
            align: 4,
            code: vec![0; 8],
            relocs: Relocs::from_iter(std::iter::empty::<Relocation>()),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            coverage: None,
            profile: None,
        };
        let made = record::function(&compiled, "main", &Spans { tables: &[], defines: &[] }).expect("a function's record");
        let slot = id(2);
        let bound = Bound { made, slot, code: CodeHash(id(3)), names: Vec::new() };
        let mut program = Program::new();
        let piece = Piece { functions: vec![bound], statics: Vec::new() };
        program.started(slot, Vec::new(), Vec::new(), piece, &[slot]).expect("started");
        let conn = Conn(1);
        program.connected(conn);
        let mut frame = Vec::new();
        let hello = Ask::Hello {
            program: wire::ProgramId(program_id(PATH)),
            pid: 1,
            arch: wire::Arch::Aarch64,
            os: wire::Os::Linux,
            page: 16384,
            holds: None,
            running: false,
        };
        hello.encode(1, &mut frame).expect("fits a frame");
        program.said(conn, &frame[4..], &mut Vec::new()).expect("its Hello is answered");
        let under = Under { policy: id(4), symbols: bind::symbols_id(&[]), maker: id(5) };
        let of = Of { program: program_id(PATH), record, under, root: "/checkout".to_owned() };
        program.kept(&of).expect("kept").expect("an image").1
    }

    /// The oracle: a restore that holds a program holds the build its mark names.
    fn oracle(bytes: &[u8], marked: &ContentId, plant: Plant) -> Result<(), String> {
        match restored_planted(bytes, &program_id(PATH), marked, plant) {
            Ok((_, of)) if of.record != *marked => {
                Err(format!("holds the build of record {:?} under a mark of {marked:?}", of.record))
            }
            Ok(_) => Ok(()),
            Err(why) if why.contains(&format!("{marked:?}")) => Ok(()),
            Err(why) => Err(format!("refused without naming the mark's record: {why}")),
        }
    }

    /// The invariant: a program is restored only from an image of the build its mark names;
    /// an image of another build is refused naming both records. The control:
    /// `Plant::TrustsTheMark` holds the older build's code under the newer record, and the
    /// oracle catches it.
    #[test]
    fn an_image_of_another_build_than_its_mark_is_refused_by_name() {
        let (older, newer) = (id(6), id(7));
        let bytes = image(older);
        match restored_of(&bytes, &program_id(PATH), &older) {
            Ok((_, of)) => assert_eq!(of.record, older, "the image's own record"),
            Err(why) => panic!("an image of the build its mark names is refused: {why}"),
        }
        match restored_of(&bytes, &program_id(PATH), &newer) {
            Ok(_) => panic!("an image of another build than its mark is restored"),
            Err(why) => {
                assert!(why.contains(&format!("{older:?}")) && why.contains(&format!("{newer:?}")), "{why}");
            }
        }
        assert_eq!(oracle(&bytes, &newer, Plant::None), Ok(()));
        assert!(oracle(&bytes, &newer, Plant::TrustsTheMark).is_err(), "the control is caught");
    }

    /// The oracle of start files: each one there beside `programs` holds the record it was
    /// written from (its bytes, in this test), and that is `record`, the build the program
    /// holds now.
    fn starts_of(programs: &[&str], record: Option<blaze_cas::Key>) -> Result<(), String> {
        for program in programs {
            let file = blaze_certify::slot::start_file(program);
            match std::fs::read(&file) {
                Ok(bytes) if record.is_some_and(|it| bytes == it.0.0) => {}
                Ok(bytes) => return Err(format!("{file} was written from {bytes:?}, and the program holds {record:?}")),
                Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
                Err(why) => return Err(format!("{file}: {why}")),
            }
        }
        Ok(())
    }

    /// The invariant: a start file exists only for the record it was written from. Over a
    /// run of builds whose records repeat and change, each start file (beside the linked path
    /// and the path it runs from) written after a build is of that build, and after every
    /// build the files left are of the record the program then holds. Control:
    /// `Plant::KeepsStartFile` keeps them across a build of another record, and the oracle
    /// catches it.
    #[test]
    fn a_start_file_is_only_of_the_record_it_was_written_from() {
        for plant in [Plant::None, Plant::KeepsStartFile] {
            let dir = tempfile::tempdir().expect("a directory");
            let path = dir.path().join("deps/hello-0123");
            let runs_as = dir.path().join("hello");
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("deps");
            let (path, runs_as) = (path.to_str().expect("UTF-8"), runs_as.to_str().expect("UTF-8"));
            let key = |byte: u8| Some(blaze_cas::Key(id(byte)));
            // A first build, a rebuild of it, two of another record, back to the first, none.
            let builds = [key(1), key(1), key(2), key(2), key(1), None, key(3)];
            let mut held = None;
            let mut caught = None;
            for (n, record) in builds.into_iter().enumerate() {
                retire_start_files(path, runs_as, held, record, plant).expect("removed");
                held = record;
                if let Err(why) = starts_of(&[path, runs_as], held) {
                    caught = Some(format!("build {n}: {why}"));
                    break;
                }
                // The daemon places the build's start files once its image is durable.
                if let Some(it) = record {
                    for program in [path, runs_as] {
                        std::fs::write(blaze_certify::slot::start_file(program), it.0.0).expect("placed");
                    }
                }
            }
            match plant {
                Plant::None => assert_eq!(caught, None, "the invariant"),
                _ => assert!(caught.is_some(), "the control: a start file kept across another record is caught"),
            }
        }
    }
}
