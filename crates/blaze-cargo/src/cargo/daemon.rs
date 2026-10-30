//! The daemon of a user: one process that every `blaze` of the user finds through one
//! socket. It loads nothing itself. It owns the workers, each a `blaze <project> --socket`
//! process with one database, starts them when a build asks, passes the builds on to them,
//! and stops them and itself when nobody has asked for a while.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::OpenOptions,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context as _;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

use super::{plan::Plan, units::stamp_of};
use blaze_protocol::{
    Message,
    events::{Event, Record, Subscribe},
};

use crate::socket::{self, Asked, NotServed, Served, private_dir, private_file};

/// How long the daemon and each worker live without being asked, unless
/// `BLAZE_IDLE_SECS` says otherwise.
const IDLE: Duration = Duration::from_secs(1800);
/// How long a worker that is stopped has to finish writing to the store.
const SETTLE: Duration = Duration::from_secs(60);

/// How often the daemon looks for what is idle.
const TICK: Duration = Duration::from_secs(1);

/// How long a worker whose first build compiled from cold goes unasked before the daemon
/// starts one from the store in its place ([`Daemon::settle`]): builds asked while it does
/// wait for the new one to load.
const SETTLE_AFTER: Duration = Duration::from_secs(10);

/// How many checkers the daemon keeps loaded, each of another build: a check for several
/// targets, or checks of several workspaces in turn.
const CHECKERS: usize = 4;

/// The share of the machine's memory the workers may hold when `BLAZE_MEMORY_MAX` does not
/// say: one part in this many.
const MEMORY_SHARE: u64 = 2;

/// The most the workers may hold together, from `BLAZE_MEMORY_MAX`: a size, or `off` for no
/// limit. Half of the machine's memory when it does not say, and no limit where the system
/// does not tell how much that is.
fn memory_max() -> Result<Option<u64>, String> {
    match std::env::var("BLAZE_MEMORY_MAX") {
        Err(_) => Ok(crate::memory::of_the_machine().map(|it| it / MEMORY_SHARE)),
        Ok(text) if text == "off" => Ok(None),
        Ok(text) => blaze_store::parse_size(&text)
            .map(Some)
            .ok_or_else(|| format!("BLAZE_MEMORY_MAX=`{text}` is neither a size nor `off`")),
    }
}

/// The setting of when the daemon has a spare worker.
pub const SPARE: &str = "BLAZE_SPARE";

/// When the daemon has a spare worker, from `BLAZE_SPARE`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Spares {
    /// `0`: none. A workspace is loaded once cargo has built it.
    Never,
    /// Without it: one as the daemon starts, for the build that started it, and one for each
    /// build after it that the daemon is told of as cargo begins it. Each loads its workspace
    /// beside cargo; none waits for a build nobody has asked for.
    Asked,
    /// `ready`: as well, one is started again once a workspace is loaded, and waits with the
    /// library loaded for the next. The daemon holds it, 1.3 GB on Linux, for as long as it runs.
    Ready,
}

fn spares() -> Result<Spares, String> {
    match std::env::var(SPARE) {
        Err(_) => Ok(Spares::Asked),
        Ok(text) if text == "0" => Ok(Spares::Never),
        Ok(text) if text == "ready" => Ok(Spares::Ready),
        Ok(text) => Err(format!("BLAZE_SPARE=`{text}` is neither `0` nor `ready`")),
    }
}

/// What a worker holds and how much it is in use.
struct Held {
    bytes: u64,
    asked: Instant,
    /// Answering a request or loading a checkout.
    busy: bool,
}

/// The workers to stop for the rest to hold no more than `most`: the ones asked longest ago
/// first. One that is busy is left, and so is the one asked last, which would only be
/// started again.
fn over(holding: &[(String, Held)], most: u64) -> Vec<String> {
    let mut all: u64 = holding.iter().map(|it| it.1.bytes).sum();
    let last = holding.iter().map(|it| it.1.asked).max();
    let mut idle: Vec<&(String, Held)> =
        holding.iter().filter(|it| !it.1.busy && Some(it.1.asked) != last).collect();
    idle.sort_by_key(|it| it.1.asked);
    let mut stopped = Vec::new();
    for (id, held) in idle {
        if all <= most {
            break;
        }
        all -= held.bytes;
        stopped.push(id.clone());
    }
    stopped
}

/// How often a worker that is loading is looked at.
const POLL: Duration = Duration::from_millis(20);

/// How long a worker that hung up without an answer is given to be seen to have exited: its
/// socket closes as it exits, a moment before it can be waited for.
const EXITING: Duration = Duration::from_secs(2);

/// How many of the last lines of a worker's log the daemon's log shows when it ends.
const LAST_LINES: usize = 12;

/// The variable that has a worker end when its standard input does, which is when the
/// daemon that started it is gone.
pub const ENDS_WITH_INPUT: &str = "BLAZE_ENDS_WITH_INPUT";

/// Where a worker that listens at `socket` leaves the errors it found in the source as it
/// loaded the workspace, in JSON, before it ends.
pub fn errors_beside(socket: &Path) -> PathBuf {
    socket.with_file_name("errors")
}

/// The file a worker that listens at `socket` writes once its first build compiled from
/// cold and the store has all of it: a worker started now would load it from the store, and
/// hold what a warm start holds.
pub fn settles_beside(socket: &Path) -> PathBuf {
    socket.with_file_name("settles")
}

/// What the daemon reads of a request of a blaze from before the frames, which sent JSON:
/// whether it starts a daemon of its own.
#[derive(Deserialize)]
struct Older {
    #[serde(default)]
    replaces: bool,
}

/// A request with the version of the client that sends it.
#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub version: String,
    /// Whether the client starts a daemon of its own version when this one is of another.
    pub replaces: bool,
    pub request: Request,
}

#[derive(Serialize, Deserialize)]
pub enum Request {
    /// Load a checkout that cargo has just built, for the builds to come.
    Warm(Checkout),
    /// Load a checkout cargo has just built what stands in for the crates of, and answer
    /// when its programs are built.
    Build(Checkout),
    /// `blaze check` of a build the daemon has no programs of: what a checker that
    /// loaded the workspace as it would finds in its crates, with no rustc.
    Check(Ahead),
    /// `blaze clippy`: what blaze's lints find in the crates the checker of the build
    /// loads, asked with a [`blaze_lints::serve::Ask`] in JSON.
    Lint(Ahead, String),
    Status,
    Stop,
    /// The records of the builds `subscribe` names: those of before, and with `follow` each
    /// one after as it comes ([`Watched`]).
    Watch {
        subscribe: Subscribe,
        follow: bool,
    },
    /// Where the build page is (docs/web.md), its server started if it is not yet.
    Ui,
}

impl Request {
    /// The request, with the variables of this command its build's scripts are given
    /// ([`WorkerConfig::command_env`]): set as it is sent, so that no state on disk keeps them.
    pub fn of_this_command(mut self) -> Request {
        if let Request::Warm(it) | Request::Build(it) = &mut self {
            it.config.command_env = super::cli::command_env();
        }
        if let Request::Check(it) | Request::Lint(it, _) = &mut self {
            it.config.command_env = super::cli::command_env();
        }
        self
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Reply {
    /// The daemon is of another version. It exits when the client said it replaces it.
    Restart {
        version: String,
    },
    /// The daemon has no part in this build, and why. Cargo builds.
    Cold {
        reason: String,
    },
    /// What the worker said of each program, in the order asked: its `ok` line, or why it
    /// did not build it.
    Synced {
        programs: Vec<Result<String, NotBuilt>>,
        /// The daemon's steps, for the client's trace. The worker's are in its `ok` lines.
        #[serde(default)]
        spans: Vec<crate::trace::Span>,
    },
    Warming,
    /// What the checker of a [`Request::Check`] found: no error, or the errors.
    Checked {
        errors: Option<crate::errors::Said>,
    },
    /// What the checker of a [`Request::Lint`] found: a [`blaze_lints::serve::Answer`] in
    /// JSON.
    Linted {
        answer: String,
    },
    Status(Status),
    Stopping,
    /// The records of a [`Request::Watch`] that does not follow; one that does was sent them
    /// as they came.
    Watched(Vec<Record>),
    /// Where the build page is, or why it is not served.
    Ui(Result<String, String>),
}

/// Why the worker did not build a program.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum NotBuilt {
    /// The source has errors: the build fails with them.
    Error(crate::errors::Said),
    /// The daemon failed.
    Failed(String),
    /// blaze does not build the source as it is, and cargo does.
    Cargos(String),
}

impl NotBuilt {
    pub fn why(self) -> String {
        match self {
            NotBuilt::Error(said) => said.to_string(),
            NotBuilt::Failed(why) | NotBuilt::Cargos(why) => why,
        }
    }

    /// What the worker's reply other than `ok` says.
    fn of(reply: &str) -> NotBuilt {
        if let Some(why) = reply.strip_prefix("cargo: ") {
            return NotBuilt::Cargos(why.to_owned());
        }
        match reply.strip_prefix("error: ") {
            Some(said) => NotBuilt::Error(crate::errors::Said::of_reply(said)),
            None => NotBuilt::Failed(reply.to_owned()),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    pub idle_secs: Option<u64>,
    /// The most the workers may hold together, in bytes.
    #[serde(default)]
    pub memory_max: Option<u64>,
    pub workers: Vec<WorkerStatus>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WorkerStatus {
    pub pid: u32,
    pub project: PathBuf,
    pub log: PathBuf,
    /// The bytes it holds, where the system tells.
    #[serde(default)]
    pub bytes: Option<u64>,
    /// Each checkout the worker holds and how warm it is.
    pub checkouts: Vec<(PathBuf, String)>,
}

/// What a worker is loaded with. Two builds with the same one share a worker.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WorkerConfig {
    /// What the checkouts of one repository have in common: the directory of its `.git`.
    pub repository: PathBuf,
    /// The directory of the package being built, from the root of the checkout.
    pub project: PathBuf,
    pub cfg_test: bool,
    /// Which build the worker stands in for, as the daemon's own plan of it has it.
    pub selection: crate::plan::Selection,
    /// The variables the worker runs with that decide what it compiles and with what.
    pub env: BTreeMap<String, String>,
    /// The rest of the variables of the command ([`super::cli::command_env`]), which its build
    /// scripts are given of as cargo's are. Not what picks the worker ([`Self::id`]): one
    /// loads the workspace again when a variable a script reads is another.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub command_env: BTreeMap<String, String>,
    /// Whether its programs are built whole, with nothing of rustc's ([`Plan::whole`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole: bool,
    /// What a whole build of tests tells its integration tests and benchmarks of where it
    /// puts things. It is in the crates' environment from the load on, so it picks the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_told: Option<TestsTold>,
}

/// Where a whole build of `cargo test`'s tests puts what cargo tells an integration test or
/// a benchmark of: the directory of the programs, which it is told the binaries of its
/// package in as `CARGO_BIN_EXE_<name>`, and the directory it may write in,
/// `CARGO_TARGET_TMPDIR`. Each from the root of the checkout when it is in it, as
/// [`WorkerConfig::project`] is: the same build in a worktree is loaded by the same worker.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TestsTold {
    pub executables: PathBuf,
    pub scratch: PathBuf,
}

impl WorkerConfig {
    /// 16 hex digits: it names the worker's directory, and the path of the socket in it has
    /// to fit the 104 bytes macOS has for one, with the daemon's directory before it.
    pub fn id(&self) -> String {
        let picks = WorkerConfig { command_env: BTreeMap::new(), ..self.clone() };
        let text = serde_json::to_vec(&picks).unwrap_or_default();
        Sha256::digest(text).iter().take(8).map(|it| format!("{it:02x}")).collect()
    }

    /// Every variable of the build: [`Self::env`] over [`Self::command_env`].
    pub fn builds_env(&self) -> BTreeMap<String, String> {
        let mut env = self.command_env.clone();
        env.extend(self.env.clone());
        env
    }
}

impl WorkerConfig {
    /// What a worker's command line says of the workspace under `root` and of `crates` of
    /// it: all that loading it and compiling its functions reads.
    fn args(&self, root: &Path, crates: &[PathBuf]) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![root.join(&self.project).into(), "--own-target".into()];
        args.extend(self.selection.flags().into_iter().map(Into::into));
        if self.cfg_test {
            args.push("--cfg-test".into());
        }
        if self.selection.dev_deps {
            args.push("--dev-deps".into());
        }
        if let Some(told) = &self.tests_told {
            args.extend(["--executables".into(), root.join(&told.executables).into()]);
            args.extend(["--scratch".into(), root.join(&told.scratch).into()]);
        }
        // Where the build was asked, from the root of the checkout when it is in it.
        if let Some(asked_in) = &self.selection.asked_in {
            let asked_in = match asked_in.as_os_str().is_empty() {
                true => root.to_owned(),
                false => root.join(asked_in),
            };
            args.extend(["--asked-in".into(), asked_in.into()]);
        }
        for krate in crates {
            args.push("--crate".into());
            args.push(krate.strip_prefix(root).unwrap_or(krate).into());
        }
        args
    }
}

/// What is known of a build before cargo has built anything of it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Ahead {
    pub config: WorkerConfig,
    /// The root of the checkout.
    pub root: PathBuf,
    /// Where the source of each crate the daemon compiles starts.
    pub crates: Vec<PathBuf>,
    /// The flags of a check that pick targets (`--tests`, `--bin NAME`, ...), each with its
    /// value. What is loaded does not turn on them, so they are not the checker's
    /// [`WorkerConfig`]: they are told it with each request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
}

/// A checkout as cargo last built it, and the programs of it the daemon is asked for.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Checkout {
    /// The build this is: the checkout, the command and its flags.
    pub key: String,
    /// The newest of rustc's output the programs are linked against. A worker that loaded
    /// the checkout with another one starts again, unless it is `kept`.
    pub generation: u128,
    /// Whether cargo built only programs the daemon did not, given the daemon's executables
    /// of the others, whose plans are the ones the worker built them by: a running worker
    /// loads the checkout again in place, keeping what it built of those, rather than start
    /// again and link every program anew under the programs running them.
    #[serde(default)]
    pub kept: bool,
    /// When cargo started, as a stamp: source newer than this is not what rustc compiled.
    pub built_at: u128,
    pub config: WorkerConfig,
    /// The root of the checkout.
    pub root: PathBuf,
    pub programs: Vec<Plan>,
    /// The source files rustc read. Only a request to warm names them.
    pub sources: Vec<PathBuf>,
    /// The variables `env!` reads in the checkout: the ones cargo set to say where its
    /// programs are, and in a request under them the ones of the process that asks.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The file of what cargo said of the build scripts it ran and the proc macros it
    /// built, which a worker loads the checkout with.
    #[serde(default)]
    pub scripts: Option<PathBuf>,
    /// Where the client asked the profile of the build to go ([`crate::trace::VAR`]): the
    /// worker records one from then on, and one it starts records its load too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<PathBuf>,
}

impl Checkout {
    /// Whether one of the source files was written after `then`.
    fn written_since(&self, then: SystemTime) -> bool {
        let written = |file: &PathBuf| std::fs::metadata(file).and_then(|it| it.modified());
        self.sources.iter().any(|file| written(file).is_ok_and(|it| it > then))
    }

    fn project(&self) -> PathBuf {
        self.root.join(&self.config.project)
    }

    /// The line that asks a worker for `plan`.
    fn sync_line(&self, plan: &Plan) -> String {
        let named =
            |path: &PathBuf| path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
        // Each with a comma after it: the program is of these crates and no others.
        let crates: String = plan.crates.iter().map(|it| named(it) + ",").collect();
        // A program built whole is linked by blaze into `binary`, with a line of the
        // worker's own, and its edits are spliced into that.
        let (splice, exe) = match plan.whole {
            true => (String::new(), plan.binary.display().to_string()),
            false => (plan.binary.display().to_string(), String::new()),
        };
        // A shared library is linked again for each build, not patched: see [`Plan::shared`].
        let (shared, patch) = match (plan.shared, plan.staticlib) {
            (true, true) => ("staticlib", String::new()),
            (true, false) => ("shared", String::new()),
            (false, _) => ("", plan.patch.display().to_string()),
        };
        let unit_test = if plan.unit_test { super::plan::UNIT_TEST } else { "" };
        let mut line = format!(
            "sync\t{}\t{}\t{}\t{patch}\t{}\t{splice}\t{exe}\t{shared}\t{unit_test}",
            self.project().display(),
            crates,
            plan.object.display(),
            plan.link_cmd,
        );
        for file in &plan.symbol_files {
            line.push('\t');
            line.push_str(&file.display().to_string());
        }
        line.push('\n');
        line
    }
}

/// How far a worker is with a checkout.
#[derive(Clone)]
enum Warmth {
    Loading(Instant),
    Ready,
    /// Why the worker has no part in the checkout as cargo last built it.
    Failed(String),
    /// The errors in the source the worker found loading it or building it first: each
    /// program's build fails with them until a source is written.
    Broken(crate::errors::Said, SystemTime),
}

impl Warmth {
    fn describe(&self) -> String {
        match self {
            Warmth::Loading(since) => format!("loading for {:.0?}", since.elapsed()),
            Warmth::Ready => "ready".to_owned(),
            Warmth::Failed(why) => format!("failed: {why}"),
            Warmth::Broken(..) => "the source has an error".to_owned(),
        }
    }
}

struct Loaded {
    generation: u128,
    root: PathBuf,
    warmth: Warmth,
    /// What it was loaded with, which a worker started in its place loads.
    checkout: Checkout,
}

/// A `blaze <project> --socket` process and the checkouts it holds.
struct Worker {
    project: PathBuf,
    socket: PathBuf,
    log: PathBuf,
    child: Mutex<Child>,
    checkouts: Mutex<FxHashMap<String, Loaded>>,
    asked: Mutex<Instant>,
    /// The requests it is answering now.
    answering: AtomicUsize,
    /// Whether the daemon's log says it ended: it says so once.
    noted: AtomicBool,
    /// Whether it was started in place of one that built from cold: it is not swapped again.
    settled: bool,
}

/// Counts a request among those a worker is answering for as long as it lives, also when the
/// thread that asks unwinds.
struct Answering<'a>(&'a AtomicUsize);

impl<'a> Answering<'a> {
    fn new(count: &'a AtomicUsize) -> Answering<'a> {
        count.fetch_add(1, Relaxed);
        Answering(count)
    }
}

impl Drop for Answering<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

/// A checker a check has, counted among those it is answering ([`Daemon::checker`]) until
/// this is dropped.
struct Answers(Arc<Checker>);

impl Drop for Answers {
    fn drop(&mut self) {
        self.0.answering.fetch_sub(1, Relaxed);
    }
}

impl Worker {
    /// Starts a worker on the first program of `checkout`, with its files in `dir`.
    /// `spare` is a process that waits to be told a build, and is told this one.
    fn start(checkout: &Checkout, dir: PathBuf, spare: Option<Spare>) -> anyhow::Result<Worker> {
        let first = checkout.programs.first().context("a checkout without programs")?;
        private_dir(&dir)?;
        let (socket, mut log) = (dir.join("sock"), dir.join("log"));
        // The worker of a daemon that was killed may still be there, on its way out: it
        // is not the one to connect to.
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(errors_beside(&socket));
        let _ = std::fs::remove_file(settles_beside(&socket));
        blaze_protocol::follow::forget_beside(&socket);
        let config = &checkout.config;
        let mut args = config.args(&checkout.root, &first.crates);
        let rustcs: [(&str, &Path); 5] = [
            ("--out", &first.object),
            ("--patch", &first.patch),
            ("--link-cmd", Path::new(&first.link_cmd)),
            ("--splice", &first.binary),
            ("--socket", &socket),
        ];
        let whole: [(&str, &Path); 4] = [
            ("--out", &first.object),
            ("--patch", &first.patch),
            ("--exe", &first.binary),
            ("--socket", &socket),
        ];
        let named: &[(&str, &Path)] = if first.whole { &whole } else { &rustcs };
        // A shared library is linked again for each build, not patched: see [`Plan::shared`].
        let patched = |flag: &str| !(first.shared && flag == "--patch");
        for &(flag, path) in named.iter().filter(|(flag, _)| patched(flag)) {
            args.push(flag.into());
            args.push(path.into());
        }
        if first.shared {
            args.push("--shared".into());
        }
        if first.staticlib {
            args.push("--staticlib".into());
        }
        if first.unit_test {
            args.push("--unit-test".into());
        }
        // In a file: a program of many crates has more of them than fit the command line
        // of a process on Windows.
        let symbols = dir.join("symbols");
        let listed: String =
            first.symbol_files.iter().map(|it| format!("{}\n", it.display())).collect();
        std::fs::write(&symbols, listed)?;
        args.push("--symbols-in".into());
        args.push(symbols.into());
        if let Some(scripts) = &checkout.scripts {
            args.push("--scripts-from".into());
            args.push(scripts.into());
        }
        // What `env!` reads of the places cargo set, as each request has them: the load
        // compiles the crates before the first request is read, and an integration test
        // that names a program (`CARGO_BIN_EXE_<name>`) would not compile without them.
        let mut started: BTreeMap<String, String> = checkout
            .env
            .iter()
            .filter(|(name, _)| super::load_wrapper::names_a_place(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        started.extend(config.builds_env());
        if let Some(profile) = checkout.profile.as_ref().and_then(|it| it.to_str()) {
            started.insert(crate::trace::VAR.to_owned(), profile.to_owned());
        }
        let told = spare.and_then(|it| it.told(&args, &started, &checkout.root));
        let child = match told {
            Some(spare) => {
                log = spare.log;
                spare.child
            }
            None => {
                let mut command = Command::new(std::env::current_exe()?);
                command.args(&args);
                match store() {
                    Some(store) => command.env("BLAZE_CACHE", store),
                    None => command.env_remove("BLAZE_CACHE"),
                };
                let output =
                    private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
                command.envs(&started).env(ENDS_WITH_INPUT, "1");
                exact_env(&mut command, &started);
                command.current_dir(&checkout.root);
                command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
                own_group(&mut command);
                command.spawn().context("starting a worker")?
            }
        };
        let loading = Loaded {
            generation: checkout.generation,
            root: checkout.root.clone(),
            warmth: Warmth::Loading(Instant::now()),
            checkout: checkout.clone(),
        };
        Ok(Worker {
            project: checkout.project(),
            socket,
            log,
            child: Mutex::new(child),
            checkouts: Mutex::new(FxHashMap::from_iter([(checkout.key.clone(), loading)])),
            asked: Mutex::new(Instant::now()),
            answering: AtomicUsize::new(0),
            noted: AtomicBool::new(false),
            settled: false,
        })
    }

    /// Whether the process has ended, and how: its exit status and what it said last.
    fn ended(&self) -> Option<String> {
        let status = exited(&mut lock(&self.child));
        status.map(|status| format!("{status}: {}", last_words(&self.log)))
    }

    /// Whether the process has ended, given `patience` to.
    fn ended_within(&self, patience: Duration) -> Option<String> {
        let asked = Instant::now();
        loop {
            match self.ended() {
                None if asked.elapsed() < patience => std::thread::sleep(POLL),
                ended => return ended,
            }
        }
    }

    /// Says in the daemon's log, once, that the process ended by itself: how, and the end
    /// of its own log, which has what it panicked with.
    fn note_end(&self) {
        let (pid, status) = {
            let mut child = lock(&self.child);
            (child.id(), exited(&mut child))
        };
        let Some(status) = status else { return };
        if self.noted.swap(true, Relaxed) {
            return;
        }
        eprintln!(
            "the worker of pid {pid} for {} ended ({status}); its log {} ends with:\n{}",
            self.project.display(),
            self.log.display(),
            log_tail(&self.log)
        );
    }

    fn stop(&self) {
        // One that ended by itself says so before it goes.
        self.note_end();
        kill(&mut lock(&self.child));
        let _ = std::fs::remove_file(&self.socket);
    }

    /// Ends its input, which ends it once the store has what its last build kept. After a
    /// build from nothing that takes a while, helix's about 20 s, and what is lost the next
    /// daemon compiles again.
    fn let_go(&self) {
        // Its end is then not news.
        self.note_end();
        self.noted.store(true, Relaxed);
        drop(lock(&self.child).stdin.take());
    }

    /// Stops it once it has gone by itself after [`Self::let_go`], or [`SETTLE`] after.
    fn finish(&self) {
        {
            let mut child = lock(&self.child);
            let given_up = Instant::now() + SETTLE;
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < given_up {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        self.stop();
    }

    /// Says how loading `checkout` went, unless another generation of it is loading since.
    fn set(&self, checkout: &Checkout, warmth: Warmth) {
        if let Some(loaded) = lock(&self.checkouts).get_mut(&checkout.key)
            && loaded.generation == checkout.generation
        {
            loaded.warmth = warmth;
        }
    }

    /// Waits for the process to listen, then has it build every program of `checkout` once.
    fn warm(&self, checkout: &Checkout) {
        let warmth = match self.warmed(checkout) {
            Ok(None) => Warmth::Ready,
            Ok(Some(said)) => Warmth::Broken(said, SystemTime::now()),
            // The worker leaves the errors before it ends, which under load can be after the
            // daemon has stopped waiting for it to; `start` took away those of any before it.
            Err(why) => match std::fs::read(errors_beside(&self.socket)) {
                Ok(said) => match serde_json::from_slice(&said) {
                    Ok(said) => Warmth::Broken(said, SystemTime::now()),
                    Err(_) => Warmth::Failed(format!("{why:#}")),
                },
                _ => Warmth::Failed(format!("{why:#}")),
            },
        };
        eprintln!("{}: {}", checkout.root.display(), warmth.describe());
        self.set(checkout, warmth);
        // Nobody could ask it for a build until now.
        *lock(&self.asked) = Instant::now();
    }

    /// Whether a checkout is still being loaded.
    fn loads(&self) -> bool {
        lock(&self.checkouts).values().any(|it| matches!(it.warmth, Warmth::Loading(_)))
    }

    /// The errors in the source, when the worker found any.
    fn warmed(&self, checkout: &Checkout) -> anyhow::Result<Option<crate::errors::Said>> {
        self.connected()?;
        // A source written since cargo built is the daemon's like any edit: it compiles
        // every function and static of the workspace from what it reads. The program the
        // worker was started on is built already, and is asked for too: what the worker then
        // compiles again by itself, for the first edit to be a patch, is part of warming up.
        // A program the worker does not build is cargo's, and the others are still its own:
        // the checkout failed to load only when it builds none. The programs are asked for
        // all at once, as `sync` does: the worker builds them side by side, so their links
        // overlap, where one after another each waited for the link of the one before.
        let asked: Vec<anyhow::Result<Result<String, NotBuilt>>> = std::thread::scope(|scope| {
            let asking: Vec<_> = checkout
                .programs
                .iter()
                .map(|plan| scope.spawn(|| self.ask(checkout, plan)))
                .collect();
            let panicked = || Err(anyhow::anyhow!("asking the worker panicked"));
            asking.into_iter().map(|it| it.join().unwrap_or_else(|_| panicked())).collect()
        });
        let mut not_built = None;
        let mut built = false;
        for asked in asked {
            match asked? {
                Ok(_) => built = true,
                Err(NotBuilt::Error(said)) => return Ok(Some(said)),
                Err(why) => _ = not_built.get_or_insert(why),
            }
        }
        match not_built {
            Some(why) if !built => anyhow::bail!(why.why()),
            _ => Ok(None),
        }
    }

    /// A connection to the process, once it has loaded the workspace and listens.
    fn connected(&self) -> anyhow::Result<UnixStream> {
        loop {
            match socket::connect(&self.socket) {
                Ok(stream) => return Ok(stream),
                Err(_) => {
                    if let Some(how) = self.ended() {
                        self.note_end();
                        anyhow::bail!(
                            "the daemon could not load the workspace ({how}); the worker's log is {}",
                            self.log.display()
                        );
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }

    /// Asks the process for `plan` of `checkout`. The outer error is the worker's failing,
    /// the inner one its answer that it does not build this.
    fn ask(&self, checkout: &Checkout, plan: &Plan) -> anyhow::Result<Result<String, NotBuilt>> {
        let _answering = Answering::new(&self.answering);
        let failed = || match self.ended_within(EXITING) {
            Some(how) => {
                self.note_end();
                format!("the daemon's worker ended ({how}); its log is {}", self.log.display())
            }
            None => {
                format!("the daemon's worker did not answer; its log is {}", self.log.display())
            }
        };
        let mut stream = self.connected()?;
        let mut env = requests_env(&checkout.config);
        for (name, value) in &checkout.env {
            env.set(name, value);
        }
        if let Some(profile) = checkout.profile.as_ref().and_then(|it| it.to_str()) {
            env.set(crate::trace::VAR, profile);
        }
        stream.write_all(checkout.sync_line(plan).as_bytes()).with_context(failed)?;
        stream
            .write_all(format!("{}\n", crate::process::encode(&env)).as_bytes())
            .with_context(failed)?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).with_context(failed)?;
        let reply = reply.trim_end();
        match reply {
            _ if reply.is_empty() => Err(anyhow::anyhow!(failed())),
            _ if reply.starts_with("ok") => Ok(Ok(reply.to_owned())),
            _ => Ok(Err(NotBuilt::of(reply))),
        }
    }

    /// Brings the programs of `checkout` up to date, all at once: the worker builds the
    /// requests that wait for it together.
    fn sync(&self, checkout: &Checkout) -> Reply {
        let spans = Mutex::new(Vec::new());
        let asked: Vec<anyhow::Result<Result<String, NotBuilt>>> = std::thread::scope(|scope| {
            let asking: Vec<_> = checkout
                .programs
                .iter()
                .map(|plan| {
                    scope.spawn(|| {
                        let start = Instant::now();
                        let asked = self.ask(checkout, plan);
                        let level = Some(crate::trace::ASKED);
                        let span =
                            crate::trace::span("daemon↔worker", level, start, Instant::now());
                        lock(&spans).push(span);
                        asked
                    })
                })
                .collect();
            let panicked = || Err(anyhow::anyhow!("asking the worker panicked"));
            asking.into_iter().map(|it| it.join().unwrap_or_else(|_| panicked())).collect()
        });
        match asked.into_iter().collect::<anyhow::Result<Vec<_>>>() {
            Ok(programs) => {
                Reply::Synced { programs, spans: spans.into_inner().unwrap_or_default() }
            }
            // The worker is gone or stuck, which says nothing of the checkout: the next
            // build that has nothing to compile has it loaded again.
            Err(why) => {
                lock(&self.checkouts).remove(&checkout.key);
                Reply::Cold { reason: format!("{why:#}") }
            }
        }
    }

    /// Whether it is loading a checkout.
    fn loading(&self) -> bool {
        lock(&self.checkouts).values().any(|it| matches!(it.warmth, Warmth::Loading(_)))
    }

    /// What it holds now, and how much it is in use.
    fn held(&self) -> Held {
        Held {
            bytes: crate::memory::held_by(lock(&self.child).id()).unwrap_or(0),
            asked: *lock(&self.asked),
            busy: self.answering.load(Relaxed) != 0 || self.loads(),
        }
    }

    fn status(&self) -> WorkerStatus {
        let checkouts = lock(&self.checkouts);
        let pid = lock(&self.child).id();
        WorkerStatus {
            pid,
            project: self.project.clone(),
            log: self.log.clone(),
            bytes: crate::memory::held_by(pid),
            checkouts: checkouts
                .values()
                .map(|it| (it.root.clone(), it.warmth.describe()))
                .collect(),
        }
    }
}

/// A worker started before a build asked for one. It has the library of the toolchain
/// loaded and its names resolved, which the first build of a workspace then does not wait
/// for, and reads the build it is the worker of from its standard input.
struct Spare {
    child: Child,
    log: PathBuf,
}

/// Takes away from a worker's `command` each variable a worker has only as its build has it
/// that its build, `env`, does not set, which it would otherwise have from the daemon.
fn exact_env(command: &mut Command, env: &BTreeMap<String, String>) {
    for name in super::cli::not_the_builds(env) {
        command.env_remove(name);
    }
}

/// The variables a request of the build `config` says, under its checkout's: those a worker
/// of it runs with. The worker loads the workspace again when the flags or a variable a
/// build script reads are another than it loaded it with, and its `env!` reads them.
fn requests_env(config: &WorkerConfig) -> ide_db::base_db::Env {
    let builds = config.builds_env();
    let not_ours = super::cli::not_the_builds(&builds);
    let mut env: ide_db::base_db::Env = Vec::from(crate::process::own())
        .into_iter()
        .filter(|(name, _)| !not_ours.contains(name))
        .collect();
    for (name, value) in &builds {
        env.set(name, value);
    }
    env.set(ENDS_WITH_INPUT, "1");
    env
}

impl Spare {
    /// Starts one with its files in `dir`.
    fn start(dir: &Path) -> anyhow::Result<Spare> {
        private_dir(dir)?;
        let log = dir.join("log");
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("--spare").arg(dir.join("package"));
        match store() {
            Some(store) => command.env("BLAZE_CACHE", store),
            None => command.env_remove("BLAZE_CACHE"),
        };
        let output =
            private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
        command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
        own_group(&mut command);
        let child = command.spawn().context("starting a spare worker")?;
        Ok(Spare { child, log })
    }

    /// Tells it to be the worker `args` start, with the variables `env`. `None` when it
    /// can't be told: it has ended, or a path of the build is not text.
    fn told(
        mut self,
        args: &[OsString],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Option<Spare> {
        let told = || {
            let args: Option<Vec<String>> =
                args.iter().map(|it| it.to_str().map(str::to_owned)).collect();
            let told =
                crate::Told { args: args?, env: env.clone(), cwd: cwd.to_owned(), ahead: false };
            let mut line = serde_json::to_string(&told).ok()?;
            line.push('\n');
            Some(line)
        };
        let sent = match (self.child.try_wait(), told()) {
            (Ok(None), Some(line)) => {
                self.child.stdin.as_mut().is_some_and(|it| it.write_all(line.as_bytes()).is_ok())
            }
            _ => false,
        };
        if !sent {
            self.stop();
            return None;
        }
        Some(self)
    }

    fn stop(&mut self) {
        kill(&mut self.child);
    }
}

/// The process that loads a workspace to check it, as the worker of a build with its
/// [`WorkerConfig`] would, and says what rust-analyzer finds in its crates when asked.
struct Checker {
    /// The [`WorkerConfig::id`] it was started with.
    id: String,
    child: Mutex<Child>,
    socket: PathBuf,
    log: PathBuf,
    project: PathBuf,
    asked: Mutex<Instant>,
    /// The checks that have it now, asking or waiting to: it is not made way for meanwhile.
    answering: AtomicUsize,
    /// Held for the whole of an ask. The process answers one request at a time, and the one
    /// that has it load again ends it, with what was sent behind that unread.
    asking: Mutex<()>,
    /// Stopped: a check that waited for [`Self::asking`] asks a new one.
    stopped: AtomicBool,
}

impl Checker {
    fn start(ahead: &Ahead, id: &str, dir: &Path) -> anyhow::Result<Checker> {
        private_dir(dir)?;
        let (socket, log) = (dir.join("sock"), dir.join("log"));
        let _ = std::fs::remove_file(&socket);
        blaze_protocol::follow::forget_beside(&socket);
        let mut args: Vec<OsString> = vec!["--check".into()];
        args.extend(ahead.config.args(&ahead.root, &ahead.crates));
        args.extend(["--out".into(), "none".into(), "--socket".into(), socket.clone().into()]);
        let mut command = Command::new(std::env::current_exe()?);
        command.args(&args);
        match store() {
            Some(store) => command.env("BLAZE_CACHE", store),
            None => command.env_remove("BLAZE_CACHE"),
        };
        let output =
            private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
        let env = ahead.config.builds_env();
        command.envs(&env).env(ENDS_WITH_INPUT, "1");
        exact_env(&mut command, &env);
        command.current_dir(&ahead.root);
        command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
        own_group(&mut command);
        let child = command.spawn().context("starting a checker")?;
        Ok(Checker::of(id, child, socket, log, ahead.root.join(&ahead.config.project)))
    }

    fn of(id: &str, child: Child, socket: PathBuf, log: PathBuf, project: PathBuf) -> Checker {
        Checker {
            id: id.to_owned(),
            child: Mutex::new(child),
            socket,
            log,
            project,
            asked: Mutex::new(Instant::now()),
            answering: AtomicUsize::new(0),
            asking: Mutex::new(()),
            stopped: AtomicBool::new(false),
        }
    }

    fn ended(&self) -> Option<String> {
        exited(&mut lock(&self.child))
    }

    /// Its answer: `ok`, the errors, or [`crate::check::RELOAD`]. Waits for it to have loaded
    /// the workspace and to listen. The caller holds [`Self::asking`].
    fn ask(&self, request: &str) -> anyhow::Result<String> {
        let mut stream = loop {
            match socket::connect(&self.socket) {
                Ok(stream) => break stream,
                Err(_) => {
                    if let Some(how) = self.ended() {
                        anyhow::bail!(
                            "the daemon could not load the workspace to check it ({how}: {}); \
                             its log is {}",
                            last_words(&self.log),
                            self.log.display()
                        );
                    }
                    std::thread::sleep(POLL);
                }
            }
        };
        stream.write_all(format!("{request}\n").as_bytes())?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        match reply.trim_end() {
            "" => anyhow::bail!("the checker hung up; its log is {}", self.log.display()),
            reply => Ok(reply.to_owned()),
        }
    }

    fn status(&self) -> WorkerStatus {
        let pid = lock(&self.child).id();
        WorkerStatus {
            pid,
            project: self.project.clone(),
            log: self.log.clone(),
            bytes: crate::memory::held_by(pid),
            checkouts: vec![(self.project.clone(), "checks".to_owned())],
        }
    }

    fn stop(&self) {
        self.stopped.store(true, Relaxed);
        kill(&mut lock(&self.child));
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Has a worker start in a process group of its own, for `kill` to end what it started with
/// it: the cargo it asks of a workspace, which would go on in the daemon's home.
fn own_group(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Ends a worker, and on Unix the processes it started.
fn kill(child: &mut Child) {
    // One that has exited already has nothing to kill, and its pid may be another's by now.
    #[cfg(unix)]
    if let (Ok(None), Ok(group)) = (child.try_wait(), libc::pid_t::try_from(child.id())) {
        // SAFETY: it takes no pointer; the group is the worker's, which has not been waited for.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The daemon: its version, its workers, and when it was last asked.
struct Daemon {
    version: String,
    home: PathBuf,
    idle: Option<Duration>,
    /// The most the workers may hold together, in bytes.
    memory_max: Option<u64>,
    workers: Mutex<FxHashMap<String, Arc<Worker>>>,
    /// The worker the next workspace is loaded by, when there is one.
    spare: Mutex<Option<Spare>>,
    spare_when: Spares,
    /// How many spare workers were started: each has a directory of its own.
    spares: AtomicUsize,
    /// The checkers of the last [`Request::Check`]s, each of another build and loaded for
    /// the next check of it: a check for several targets asks one for each. Held only to
    /// find, add or drop one, never for an ask, which can wait for a whole load: nothing
    /// else (another workspace's check, `blaze status`, the daemon's end) waits for that.
    checkers: Mutex<Vec<Arc<Checker>>>,
    asked: Arc<Mutex<Instant>>,
    watched: Arc<Mutex<Watched>>,
    relays: Arc<Relays>,
    /// Where the build page is, once a `blaze ui` started its server ([`Request::Ui`]).
    ui: Mutex<Option<String>>,
}

/// The records of the builds under way and of the last of each workspace, for the commands
/// that watch them: `blaze watch` follows every build, `blaze status` shows the one of its
/// workspace.
#[derive(Default)]
struct Watched {
    /// The number of the last build that began.
    builds: u64,
    watching: Vec<Watching>,
    watchers: Vec<Watcher>,
    /// The graph of each workspace's functions, for the build page.
    graphs: Arc<blaze_web::Graphs>,
}

struct Watching {
    build: u64,
    root: PathBuf,
    records: Vec<Record>,
    done: bool,
    /// When it began ([`blaze_protocol::follow::now_ns`]): its records' times are since.
    began_ns: u64,
}

/// A command that is sent the records of the builds of the workspace at `root`, or of every
/// one.
struct Watcher {
    root: Option<PathBuf>,
    to: std::sync::mpsc::Sender<Record>,
}

/// How many workspaces' last builds the daemon keeps the records of.
const WATCHED: usize = 16;

impl Watched {
    fn began(&mut self, root: &Path) -> u64 {
        self.builds += 1;
        self.watching.retain(|it| !(it.done && it.root == root));
        let done = self.watching.iter().filter(|it| it.done).count();
        if let Some(oldest) = self.watching.iter().position(|it| it.done)
            && done >= WATCHED
        {
            self.watching.remove(oldest);
        }
        let watching = Watching {
            build: self.builds,
            root: root.to_owned(),
            records: Vec::new(),
            done: false,
            began_ns: blaze_protocol::follow::now_ns(),
        };
        self.watching.push(watching);
        self.builds
    }

    fn record(&mut self, build: u64, record: &Record) {
        let Some(watching) = self.watching.iter_mut().find(|it| it.build == build) else { return };
        self.graphs.record(&watching.root, build, record.clone());
        watching.records.push(record.clone());
        let done = matches!(record.event, Event::Finished { .. });
        watching.done |= done;
        let root = &watching.root;
        // One that went away is told nothing more, and one of a workspace whose build is
        // done is done too.
        self.watchers.retain(|watcher| {
            let ours = watcher.root.as_ref().is_none_or(|it| it == root);
            !ours || watcher.to.send(record.clone()).is_ok() && !(done && watcher.root.is_some())
        });
    }

    /// The functions build `build` put in its program: the build page's alone.
    fn functions(&mut self, build: u64, record: Record) {
        let Some(watching) = self.watching.iter().find(|it| it.build == build) else { return };
        self.graphs.record(&watching.root, build, record);
    }

    /// The build under way in the workspace `at` is in, or else its last.
    fn of(&self, at: &Path) -> Option<&Watching> {
        let of = |done: bool| {
            self.watching.iter().rev().find(|it| it.done == done && at.starts_with(&it.root))
        };
        of(false).or_else(|| of(true))
    }

    /// The step the build under way in the workspace at `root` is at, as its live view says
    /// it: `Resolving names 2.40s (3/120 crates)`.
    fn under_way(&self, root: &Path) -> Option<String> {
        let watching = self.watching.iter().rev().find(|it| !it.done && it.root == root)?;
        let mut build = blaze_ui::Build::default();
        for record in &watching.records {
            build.apply(record);
        }
        let step = build.running().last()?;
        let now_ns = blaze_protocol::follow::now_ns().saturating_sub(watching.began_ns);
        let took = blaze_ui::took(now_ns.saturating_sub(step.began_ns));
        let count = step.count().map(|it| format!(" ({it})")).unwrap_or_default();
        Some(format!("{} {} {took}{count}", step.step.word(), step.what()))
    }
}

/// Where the daemon of this user keeps its socket, its lock, its log and its workers.
pub fn home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|it| !it.is_empty()) {
        return Ok(Path::new(&runtime).join("blaze"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    Ok(Path::new(&home.context("no BLAZE_HOME, XDG_RUNTIME_DIR or HOME")?).join(".cache/blaze"))
}

/// The disk store of the user's builds: the directory `BLAZE_CACHE` names, none when it
/// says `off`, and without it `blaze/store` in the user's cache directory. Unlike `home`
/// it is not in `XDG_RUNTIME_DIR`, which does not outlive the session.
pub fn store() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("BLAZE_CACHE") {
        return (!blaze_store::is_off(&named)).then(|| PathBuf::from(named));
    }
    let caches = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let caches = caches.or_else(|| Some(PathBuf::from(home?).join(".cache")))?;
    Some(caches.join("blaze/store"))
}

/// Why there is no [`store`]: what to set for one.
pub fn no_store() -> &'static str {
    match std::env::var_os("BLAZE_CACHE") {
        Some(_) => "BLAZE_CACHE=off",
        None => "no BLAZE_CACHE, XDG_CACHE_HOME or HOME says where to keep it",
    }
}

pub fn socket_in(home: &Path) -> PathBuf {
    home.join("daemon.sock")
}

/// What two processes have to agree on to talk: the executable itself, so that a daemon of
/// any other build is replaced. The executable is told by where it is as well as by its size
/// and time: every build in the nix store has the same time, and two builds of a small change
/// can have the same size, but no two have the same path. No version of blaze's: an
/// executable's place, size and time already tell one release from another, and the version is
/// the engine's alone (crates/blaze-engine/build.rs).
pub fn version() -> String {
    let executable = std::env::current_exe().and_then(std::fs::canonicalize);
    let place: String = executable.as_ref().map_or_else(
        |_| String::new(),
        |it| {
            let digest = Sha256::digest(it.as_os_str().as_encoded_bytes());
            digest.iter().take(4).map(|it| format!("{it:02x}")).collect()
        },
    );
    let built = executable.and_then(std::fs::metadata);
    let built = built.map(|it| (it.len(), it.modified().map_or(0, stamp_of)));
    let (len, written) = built.unwrap_or_default();
    format!("{place}-{len:x}-{written:x}")
}

/// `blaze-engine daemon`: serves the user's builds until it has been idle for long enough or is
/// told to stop. It starts:
///
/// 1. the thread that accepts connections, and a thread per connection that answers its
///    one request;
/// 2. the thread that stops idle workers and ends the daemon when it is idle itself;
/// 3. per worker, the process, and a thread that waits for it to load and has it build
///    its programs once.
pub fn serve() -> anyhow::Result<()> {
    let home = home()?;
    private_dir(&home).with_context(|| format!("the daemon's directory {}", home.display()))?;
    let memory_max = memory_max().map_err(anyhow::Error::msg)?;
    let own = crate::memory::held_by(std::process::id()).unwrap_or(0);
    match crate::memory::Budget::now(own).map_err(anyhow::Error::msg)? {
        Some(budget) => eprintln!("memory budget: {budget}"),
        None => eprintln!(
            "memory budget: none ({}=off, or the system says nothing of memory)",
            crate::memory::BUDGET
        ),
    }
    let spare_when = spares().map_err(anyhow::Error::msg)?;
    let socket = socket_in(&home);
    let served = match Served::bind(&socket) {
        Ok(served) => served,
        // Another daemon is the user's: the client that started this one finds it.
        Err(NotServed::Taken) => return Ok(()),
        Err(NotServed::Failed(error)) => {
            // On one line: a worker's log is read for its last line that starts `Error`.
            anyhow::bail!("listening on {}: {error}", socket.display());
        }
    };
    let idle = match std::env::var("BLAZE_IDLE_SECS").ok().and_then(|it| it.parse::<u64>().ok()) {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(IDLE),
    };
    let daemon = Arc::new(Daemon {
        version: version(),
        home,
        idle,
        memory_max,
        workers: Mutex::default(),
        spare: Mutex::default(),
        spare_when,
        spares: AtomicUsize::new(0),
        checkers: Mutex::default(),
        asked: Arc::new(Mutex::new(Instant::now())),
        watched: Arc::default(),
        relays: Arc::default(),
        ui: Mutex::default(),
    });
    // A build starts the daemon, and is given this one.
    daemon.spare();
    eprintln!(
        "daemon {} ({}) of pid {} listens on {}",
        daemon.version,
        crate::version(),
        std::process::id(),
        socket.display()
    );
    {
        let daemon = daemon.clone();
        std::thread::spawn(move || daemon.reap());
    }
    {
        let relays = daemon.relays.clone();
        std::thread::spawn(move || relays.serve());
    }
    for (stream, open) in served.incoming() {
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            let _open = open;
            if let Err(why) = daemon.answer(stream) {
                eprintln!("a request is not answered: {why:#}");
            }
        });
    }
    Ok(())
}

/// How often a command that waits for an answer is handed what the process it waits for
/// said since.
const RELAYED: Duration = Duration::from_millis(100);

/// The process whose records a command is handed, the root of the workspace it builds, and
/// what the command is.
struct Followed {
    dir: PathBuf,
    root: PathBuf,
    command: &'static str,
}

/// What the daemon sends a client in answer to a request: the records of the build it waits
/// for as they come, then the reply.
#[derive(Serialize, Deserialize, Debug)]
pub enum Answer {
    Record(Record),
    Reply(Reply),
}

/// The commands that wait for their answers, each handed what the process it waits for said
/// ([`blaze_protocol::follow`]) every [`RELAYED`] by one thread, rather than a thread each:
/// a quick build is answered before the first.
#[derive(Default)]
struct Relays {
    under_way: Mutex<Vec<Arc<Relay>>>,
    more: std::sync::Condvar,
}

impl Relays {
    fn serve(&self) {
        loop {
            let waiting = lock(&self.under_way);
            let waiting = self.more.wait_while(waiting, |it| it.is_empty());
            drop(waiting.unwrap_or_else(PoisonError::into_inner));
            std::thread::sleep(RELAYED);
            let under_way = lock(&self.under_way).clone();
            for relay in under_way {
                let mut relayed = lock(&relay.relayed);
                if !relayed.done {
                    relay.relay(&mut relayed);
                    relay.flush(&mut relayed);
                }
            }
        }
    }

    fn add(&self, relay: &Arc<Relay>) {
        lock(&self.under_way).push(relay.clone());
        self.more.notify_one();
    }

    fn remove(&self, relay: &Arc<Relay>) {
        lock(&self.under_way).retain(|it| !Arc::ptr_eq(it, relay));
    }
}

/// A command that waits for request `id`, handed the records of build `number`: their times
/// from when it asked, `began_ns`.
struct Relay {
    stream: UnixStream,
    id: u64,
    dir: PathBuf,
    began_ns: u64,
    number: u64,
    watched: Arc<Mutex<Watched>>,
    relayed: Mutex<Relayed>,
}

#[derive(Default)]
struct Relayed {
    follow: blaze_protocol::follow::Follow,
    build: blaze_ui::Build,
    /// Frames not sent yet: they go with the next of [`Relays`], or with the answer.
    unsent: Vec<u8>,
    /// The command went away: it is handed nothing more, and its answer goes nowhere.
    gone: bool,
    /// It was answered: nothing more is its.
    done: bool,
}

impl Relay {
    fn send(&self, relayed: &mut Relayed, event: Event, at_ns: u64) {
        let record = Record { at_ns: at_ns.saturating_sub(self.began_ns), event };
        // The functions of a cold build are some megabytes: they are the build page's alone.
        if matches!(record.event, Event::Functions { .. }) {
            lock(&self.watched).functions(self.number, record);
            return;
        }
        relayed.build.apply(&record);
        lock(&self.watched).record(self.number, &record);
        if !relayed.gone {
            let message = Message::new(self.id, Answer::Record(record));
            relayed.gone = blaze_protocol::send(&mut relayed.unsent, &message).is_err();
        }
    }

    /// What the process said since.
    fn relay(&self, relayed: &mut Relayed) {
        let Some((path, len)) = blaze_protocol::follow::written_in(&self.dir) else { return };
        for record in relayed.follow.read(&path, len) {
            match record {
                Ok(record) => self.send(relayed, record.event, record.at_ns),
                Err(why) => eprintln!("{}: a record is not read: {why}", path.display()),
            }
        }
    }

    fn flush(&self, relayed: &mut Relayed) {
        if !relayed.gone && !relayed.unsent.is_empty() {
            relayed.gone = (&self.stream).write_all(&relayed.unsent).is_err();
        }
        relayed.unsent.clear();
    }
}

/// The answer `answering` gives, with the command that waits for it, request `id`, handed
/// each record the process it waits for writes meanwhile ([`Relays`]), and last how the build
/// went; and the frames of those not sent yet, to go with the answer. A command that says
/// nothing for the minute a load and a first build can take looks stuck.
fn relaying(
    stream: &UnixStream,
    id: u64,
    followed: &Followed,
    daemon: &Daemon,
    answering: impl FnOnce() -> Reply,
) -> anyhow::Result<(Reply, Vec<u8>)> {
    let Followed { dir, root, command } = followed;
    let began_ns = blaze_protocol::follow::now_ns();
    let number = lock(&daemon.watched).began(root);
    let follow = blaze_protocol::follow::Follow::from_now(dir);
    let relay = Arc::new(Relay {
        stream: stream.try_clone()?,
        id,
        dir: dir.clone(),
        began_ns,
        number,
        watched: daemon.watched.clone(),
        relayed: Mutex::new(Relayed { follow, ..Relayed::default() }),
    });
    let began = Event::Began { build: number, command: (*command).to_owned(), root: root.clone() };
    relay.send(&mut lock(&relay.relayed), began, began_ns);
    daemon.relays.add(&relay);
    let reply = answering();
    daemon.relays.remove(&relay);
    let mut relayed = lock(&relay.relayed);
    relayed.done = true;
    // What the process said before it answered.
    relay.relay(&mut relayed);
    let first_line = |text: String| text.lines().next().unwrap_or_default().to_owned();
    let error = match &reply {
        Reply::Synced { programs, .. } => {
            programs.iter().find_map(|it| it.as_ref().err()).map(|it| first_line(it.clone().why()))
        }
        Reply::Checked { errors: Some(said) } => Some(first_line(said.to_string())),
        Reply::Cold { reason } => Some(first_line(reason.clone())),
        _ => None,
    };
    let ok = error.is_none();
    let now_ns = blaze_protocol::follow::now_ns();
    let summary = relayed.build.summary(now_ns.saturating_sub(began_ns), error);
    relay.send(&mut relayed, Event::Finished { ok, summary }, now_ns);
    let unsent = if relayed.gone { Vec::new() } else { std::mem::take(&mut relayed.unsent) };
    Ok((reply, unsent))
}

impl Daemon {
    /// Reads the one request of a connection and answers it.
    fn answer(&self, stream: UnixStream) -> anyhow::Result<()> {
        let began = Instant::now();
        let line = match socket::read_asked(&stream)? {
            Asked::Frame(item) => return self.answer_frame(&stream, &item, began),
            Asked::Line(line) => line,
        };
        // An editor's text, on a line of its own: it is long and comes with every key typed.
        // The worker answers as to `blaze build`.
        if line.starts_with("edit\t") {
            let reply = self.edit(&line).unwrap_or_else(|why| format!("error: {why:#}"));
            // An editor's client reads the errors as text.
            let reply = crate::errors::Said::text_of_reply(&reply);
            return Ok((&stream).write_all(format!("{}\n", reply.replace('\n', " ")).as_bytes())?);
        }
        // Any other line is the JSON of a blaze from before the frames, of another version: it
        // is answered so in its JSON, and the daemon exits for one that starts its own.
        let replaces = serde_json::from_str::<Older>(&line).is_ok_and(|it| it.replaces);
        let mut text = serde_json::to_string(&Reply::Restart { version: self.version.clone() })?;
        text.push('\n');
        let sent = (&stream).write_all(text.as_bytes());
        if replaces {
            self.end();
        }
        Ok(sent?)
    }

    /// Answers the request of a frame, a [`Message`] of an [`Envelope`].
    fn answer_frame(&self, stream: &UnixStream, item: &[u8], began: Instant) -> anyhow::Result<()> {
        // One that can't be read is of another version, and of a client that waits for an
        // answer it can read: it gets none, and the daemon stays.
        let message = blaze_protocol::cbor::from_slice::<Message<Envelope>>(item).ok();
        let (id, envelope) = match message {
            Some(message) if message.schema == blaze_protocol::SCHEMA => {
                (message.id, Some(message.body))
            }
            Some(message) => (message.id, None),
            None => (0, None),
        };
        let replaced =
            envelope.as_ref().is_some_and(|it| it.replaces && it.version != self.version);
        let read = Instant::now();
        let (mut reply, mut unsent) = match envelope {
            Some(envelope) if envelope.version == self.version => {
                match (self.followed_in(&envelope.request), envelope.request) {
                    (_, Request::Watch { subscribe, follow }) => {
                        (self.watch(stream, id, &subscribe, follow), Vec::new())
                    }
                    (Some(followed), request) => {
                        relaying(stream, id, &followed, self, || self.handle(request))?
                    }
                    (None, request) => (self.handle(request), Vec::new()),
                }
            }
            _ => (Reply::Restart { version: self.version.clone() }, Vec::new()),
        };
        if let Reply::Synced { spans, .. } = &mut reply {
            let level = Some(crate::trace::DAEMON);
            spans.push(crate::trace::span("daemon read request", level, began, read));
            spans.push(crate::trace::span("daemon", level, read, Instant::now()));
        }
        let stopping = matches!(reply, Reply::Stopping);
        blaze_protocol::send(&mut unsent, &Message::new(id, Answer::Reply(reply)))?;
        // The records not sent yet and the answer in one write: a quick build's in one read.
        let sent = (&*stream).write_all(&unsent);
        if replaced || stopping {
            self.end();
        }
        Ok(sent?)
    }

    /// Hands `line`, an `edit` of a source file, to the worker that has the checkout of the
    /// file loaded, and gives back its answer.
    fn edit(&self, line: &str) -> anyhow::Result<String> {
        *lock(&self.asked) = Instant::now();
        let escaped = line["edit\t".len()..].split('\t').next().unwrap_or_default();
        let path = PathBuf::from(blaze_live::format::unescape(escaped));
        let path = dunce::canonicalize(&path).unwrap_or(path);
        let has = |worker: &Worker| {
            lock(&worker.checkouts).values().any(|it| {
                matches!(it.warmth, Warmth::Ready)
                    && dunce::canonicalize(&it.root).is_ok_and(|root| path.starts_with(root))
            })
        };
        let worker = lock(&self.workers).values().find(|it| has(it)).cloned();
        let worker = worker.with_context(|| {
            format!("the daemon has no workspace loaded that has {}", path.display())
        })?;
        // Counted, then seen to be ready still: [`Self::settle`] takes a worker that is
        // answering nothing out of use.
        let _answering = Answering::new(&worker.answering);
        anyhow::ensure!(has(&worker), "the daemon is loading the workspace of {}", path.display());
        *lock(&worker.asked) = Instant::now();
        let mut stream = socket::connect(&worker.socket)?;
        // The worker builds it for the process its last build was for.
        stream.write_all(format!("{line}\n\n").as_bytes())?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        match crate::trace::without_spans(reply.trim_end()) {
            "" => anyhow::bail!("the worker hung up; its log is {}", worker.log.display()),
            reply => Ok(reply.to_owned()),
        }
    }

    /// The process that answers `request`, whose records the command that asks is handed
    /// ([`blaze_protocol::follow`]): a build, which may load the workspace first, and a
    /// check, which can take long.
    fn followed_in(&self, request: &Request) -> Option<Followed> {
        let (dir, root, command) = match request {
            Request::Build(checkout) => {
                (self.home.join("workers").join(checkout.config.id()), &checkout.root, "build")
            }
            Request::Check(ahead) => {
                (self.home.join("checkers").join(ahead.config.id()), &ahead.root, "check")
            }
            Request::Lint(ahead, _) => {
                (self.home.join("checkers").join(ahead.config.id()), &ahead.root, "clippy")
            }
            Request::Warm(_)
            | Request::Status
            | Request::Stop
            | Request::Watch { .. }
            | Request::Ui => {
                return None;
            }
        };
        Some(Followed { dir, root: root.clone(), command })
    }

    /// Answers a [`Request::Watch`]: the records of before, and with `follow` each one after
    /// as it comes, until the build of a workspace is done or the command goes away.
    fn watch(&self, stream: &UnixStream, id: u64, subscribe: &Subscribe, follow: bool) -> Reply {
        let mut watched = lock(&self.watched);
        let (root, before) = match subscribe {
            Subscribe::Build { root } => {
                let Some(watching) = watched.of(root) else { return Reply::Watched(Vec::new()) };
                let done = watching.done;
                let watching_root = watching.root.clone();
                let records = watching.records.clone();
                if done || !follow {
                    return Reply::Watched(records);
                }
                (Some(watching_root), records)
            }
            Subscribe::All => {
                let under_way = watched.watching.iter().filter(|it| !it.done);
                (None, under_way.flat_map(|it| it.records.clone()).collect())
            }
        };
        if !follow {
            return Reply::Watched(before);
        }
        let (to, told) = std::sync::mpsc::channel();
        watched.watchers.push(Watcher { root, to });
        drop(watched);
        for record in before.into_iter().chain(told) {
            let sent =
                blaze_protocol::send(&mut &*stream, &Message::new(id, Answer::Record(record)));
            if sent.is_err() {
                break;
            }
        }
        Reply::Watched(Vec::new())
    }

    fn handle(&self, request: Request) -> Reply {
        *lock(&self.asked) = Instant::now();
        match request {
            Request::Warm(checkout) => match self.warm(checkout) {
                Ok(()) => Reply::Warming,
                Err(why) => Reply::Cold { reason: format!("{why:#}") },
            },
            Request::Build(checkout) => match self.warm(checkout.clone()) {
                Ok(()) => self.built(&checkout),
                Err(why) => Reply::Cold { reason: format!("{why:#}") },
            },
            Request::Check(ahead) => self.check(&ahead, "check"),
            Request::Lint(ahead, ask) => {
                self.check(&ahead, &format!("{}{ask}", crate::check::LINT))
            }
            Request::Status => {
                let mut workers: Vec<WorkerStatus> = lock(&self.workers)
                    .values()
                    .map(|it| it.status())
                    .chain(lock(&self.checkers).iter().map(|it| it.status()))
                    .collect();
                // A checkout being loaded says the step its build is at, as the live view of
                // that build does.
                let watched = lock(&self.watched);
                for (root, warmth) in workers.iter_mut().flat_map(|it| it.checkouts.iter_mut()) {
                    if warmth.starts_with("loading")
                        && let Some(step) = watched.under_way(root)
                    {
                        *warmth = step;
                    }
                }
                drop(watched);
                Reply::Status(Status {
                    pid: std::process::id(),
                    version: self.version.clone(),
                    idle_secs: self.idle.map(|it| it.as_secs()),
                    memory_max: self.memory_max,
                    workers,
                })
            }
            Request::Stop => Reply::Stopping,
            Request::Watch { .. } => Reply::Watched(Vec::new()),
            Request::Ui => Reply::Ui(self.ui()),
        }
    }

    /// Where the build page is, its server started the first time it is asked for.
    fn ui(&self) -> Result<String, String> {
        let mut ui = lock(&self.ui);
        if let Some(url) = &*ui {
            return Ok(url.clone());
        }
        let graphs = lock(&self.watched).graphs.clone();
        let asked = self.asked.clone();
        let asked = Arc::new(move || *lock(&asked) = Instant::now());
        let served = blaze_web::serve(graphs, asked)
            .map_err(|why| format!("the build page is not served: {why}"))?;
        eprintln!("the build page is at {}", served.url);
        Ok(ui.insert(served.url).clone())
    }

    /// What the worker that loads `checkout` has to say of its programs once it has. How far
    /// the worker is is read once a round: a load that ends between two reads of it is
    /// answered as it ended, never as still loading.
    fn built(&self, checkout: &Checkout) -> Reply {
        loop {
            let worker = lock(&self.workers).get(&checkout.config.id()).cloned();
            let Some(worker) = worker else {
                return Reply::Cold {
                    reason: "the daemon has not loaded this workspace".to_owned(),
                };
            };
            *lock(&worker.asked) = Instant::now();
            // Counted before it is seen to be ready: [`Self::settle`] takes a worker that is
            // answering nothing out of use.
            let _answering = Answering::new(&worker.answering);
            let warmth = match lock(&worker.checkouts).get(&checkout.key) {
                Some(loaded) if loaded.generation == checkout.generation => loaded.warmth.clone(),
                _ => Warmth::Failed(
                    "the daemon has not loaded this build of the workspace".to_owned(),
                ),
            };
            match warmth {
                Warmth::Ready => return worker.sync(checkout),
                Warmth::Loading(_) => std::thread::sleep(POLL),
                Warmth::Failed(reason) => return Reply::Cold { reason },
                Warmth::Broken(said, _) => {
                    let programs =
                        checkout.programs.iter().map(|_| Err(NotBuilt::Error(said.clone())));
                    return Reply::Synced { programs: programs.collect(), spans: Vec::new() };
                }
            }
        }
    }

    /// Has a worker load `checkout`: the one of its configuration when that has not loaded
    /// another build of the same checkout, and a new one otherwise.
    fn warm(&self, checkout: Checkout) -> anyhow::Result<()> {
        let id = checkout.config.id();
        let mut workers = lock(&self.workers);
        // Source diagnostics stay valid until a source is written. A failed load has not
        // established its inputs: configuration, even a file since deleted, may have caused
        // it. Retry that load on the next request rather than keep its error indefinitely.
        let failed = workers.get(&id).and_then(|it| match lock(&it.checkouts).get(&checkout.key) {
            Some(Loaded { generation, warmth, .. }) if *generation == checkout.generation => {
                match warmth {
                    Warmth::Failed(_) | Warmth::Broken(..) => Some(warmth.clone()),
                    _ => None,
                }
            }
            _ => None,
        });
        match failed {
            Some(Warmth::Broken(_, at)) if !checkout.written_since(at) => return Ok(()),
            Some(_) => {
                if let Some(old) = workers.remove(&id) {
                    old.stop();
                }
            }
            None => {}
        }
        let running = workers.get(&id).filter(|it| it.ended().is_none()).cloned();
        if let Some(worker) = &running {
            let mut checkouts = lock(&worker.checkouts);
            match checkouts.get(&checkout.key) {
                Some(loaded) if loaded.generation == checkout.generation => return Ok(()),
                // What it linked against is not what cargo has now, and one it could not load
                // is started again.
                Some(Loaded { warmth: Warmth::Failed(..), .. }) => {}
                Some(_) if !checkout.kept => {}
                // rustc built only what the daemon did not: the worker keeps the programs it
                // built, the executables running programs take edits into, and starts over
                // with the others (`Program::start_over`). A new worker would link each
                // program again, and every program running would be told to restart.
                _ => {
                    let loading = Loaded {
                        generation: checkout.generation,
                        root: checkout.root.clone(),
                        warmth: Warmth::Loading(Instant::now()),
                        checkout: checkout.clone(),
                    };
                    checkouts.insert(checkout.key.clone(), loading);
                    let worker = worker.clone();
                    std::thread::spawn(move || worker.warm(&checkout));
                    return Ok(());
                }
            }
        }
        if let Some(old) = workers.remove(&id) {
            old.stop();
        }
        // A spare loads its library before it is told the build, which a whole build that
        // asks at once would wait for.
        let spare = match checkout.config.whole {
            true => None,
            false => lock(&self.spare).take(),
        };
        let worker =
            Arc::new(Worker::start(&checkout, self.home.join("workers").join(&id), spare)?);
        eprintln!(
            "worker {id} of pid {} for {}",
            lock(&worker.child).id(),
            worker.project.display()
        );
        workers.insert(id, worker.clone());
        std::thread::spawn(move || worker.warm(&checkout));
        Ok(())
    }

    /// Stops the workers asked longest ago while together they hold more than they may, and
    /// those nobody has asked for the idle time; and the daemon when nobody has asked it.
    fn reap(self: Arc<Self>) {
        loop {
            std::thread::sleep(self.idle.map_or(TICK, |idle| idle.min(TICK)));
            // A worker that ended by itself is in the daemon's log when it ends, not when a
            // build next asks for it.
            for worker in lock(&self.workers).values() {
                worker.note_end();
            }
            // The next spare starts when the worker that was one has loaded its workspace.
            if self.spare_when == Spares::Ready {
                let loading = lock(&self.workers).values().any(|it| it.loading());
                if !loading {
                    self.spare();
                }
            }
            // A program that takes edits is being edited, however long since the last save:
            // a worker started again would link it again on the next one.
            let live = crate::live::any_running();
            if let Some(most) = self.memory_max.filter(|_| !live) {
                self.fit(most);
            }
            if !live {
                self.settle();
            }
            let Some(idle) = self.idle else { continue };
            if live {
                *lock(&self.asked) = Instant::now();
                for worker in lock(&self.workers).values() {
                    *lock(&worker.asked) = Instant::now();
                }
                continue;
            }
            // Loading is what the last build asked for: the idle time starts after it.
            for worker in lock(&self.workers).values().filter(|it| it.loading()) {
                *lock(&worker.asked) = Instant::now();
                *lock(&self.asked) = Instant::now();
            }
            if lock(&self.asked).elapsed() >= idle {
                eprintln!("idle for {idle:.0?}");
                self.end();
            }
            lock(&self.workers).retain(|_, worker| {
                // Loading can take longer than the idle time, and is what was asked for.
                let keep = worker.loads() || lock(&worker.asked).elapsed() < idle;
                if !keep {
                    worker.stop();
                }
                keep
            });
            lock(&self.checkers).retain(|it| {
                let keep = it.answering.load(Relaxed) != 0 || lock(&it.asked).elapsed() < idle;
                if !keep {
                    it.stop();
                }
                keep
            });
        }
    }

    /// Stops the workers asked longest ago until the rest hold no more than `most` bytes. The
    /// next build of a workspace whose worker was stopped is cargo's while a worker loads it
    /// again.
    fn fit(&self, most: u64) {
        let mut workers = lock(&self.workers);
        let holding: Vec<(String, Held)> =
            workers.iter().map(|(id, worker)| (id.clone(), worker.held())).collect();
        let all: u64 = holding.iter().map(|it| it.1.bytes).sum();
        for id in over(&holding, most) {
            let Some(worker) = workers.remove(&id) else { continue };
            eprintln!(
                "worker {id} of {} stopped: the workers hold {} MB, over the {} MB of \
                 BLAZE_MEMORY_MAX, and it was asked longest ago",
                worker.project.display(),
                all >> 20,
                most >> 20
            );
            worker.stop();
        }
    }

    /// Starts a worker from the store in place of one whose first build compiled from cold,
    /// once that has gone [`SETTLE_AFTER`] unasked (Rik Arends' settle rebuild in Relax,
    /// `compiler/src/settle_rebuild.rs`). The new one holds what a warm start holds, half of
    /// what a cold build leaves (2.3 GB rather than 4.4 GB of a 94-crate program, #1299), and
    /// its edits do not expand again the macros of crates the cold build had no frozen def
    /// maps of. The old one has gone before the new one starts, for the two not to write the
    /// same programs; a build asked meanwhile waits for the new one as for any that loads.
    /// Not while a program takes edits: a new worker links every program again.
    fn settle(self: &Arc<Self>) {
        let due = lock(&self.workers).iter().find_map(|(id, worker)| {
            let due = !worker.settled
                && lock(&worker.asked).elapsed() >= SETTLE_AFTER
                && settles_beside(&worker.socket).exists()
                && worker.ended().is_none();
            due.then(|| (id.clone(), worker.clone()))
        });
        let Some((id, old)) = due else { return };
        let checkout = {
            let mut checkouts = lock(&old.checkouts);
            let mut loaded = checkouts.values_mut();
            match (loaded.next(), loaded.next()) {
                (Some(loaded), None) if matches!(loaded.warmth, Warmth::Ready) => {
                    loaded.warmth = Warmth::Loading(Instant::now());
                    loaded.checkout.clone()
                }
                // One that holds several checkouts loads the others again when asked.
                _ => {
                    _ = std::fs::remove_file(settles_beside(&old.socket));
                    return;
                }
            }
        };
        // A request counts itself before it looks at the warmth set above.
        if old.answering.load(Relaxed) != 0 {
            old.set(&checkout, Warmth::Ready);
            return;
        }
        _ = std::fs::remove_file(settles_beside(&old.socket));
        eprintln!(
            "worker {id} for {} built from cold: starting one from the store in its place",
            old.project.display()
        );
        let daemon = self.clone();
        std::thread::spawn(move || {
            old.let_go();
            old.finish();
            let mut workers = lock(&daemon.workers);
            // A build of another generation started a worker of its own meanwhile.
            if !workers.get(&id).is_some_and(|it| Arc::ptr_eq(it, &old)) {
                return;
            }
            workers.remove(&id);
            let dir = daemon.home.join("workers").join(&id);
            let log = dir.join("log");
            if old.log == log {
                _ = std::fs::rename(&log, dir.join("log.cold"));
            }
            let spare = match checkout.config.whole {
                true => None,
                false => lock(&daemon.spare).take(),
            };
            let mut worker = match Worker::start(&checkout, dir, spare) {
                Ok(worker) => worker,
                Err(why) => {
                    eprintln!(
                        "worker {id}: not started in place of the one built from cold: {why:#}"
                    );
                    return;
                }
            };
            worker.settled = true;
            let worker = Arc::new(worker);
            eprintln!(
                "worker {id} of pid {} for {}, from the store",
                lock(&worker.child).id(),
                worker.project.display()
            );
            workers.insert(id, worker.clone());
            drop(workers);
            worker.warm(&checkout);
        });
    }

    /// Has the checker of `ahead` say what it finds in the source, started when there is
    /// none of its build or it ended, and started again when it can only take the source in
    /// by loading it again. The one asked longest ago makes way for it when there are
    /// [`CHECKERS`]. `request` is the line the checker is asked with.
    fn check(&self, ahead: &Ahead, request: &str) -> Reply {
        let id = ahead.config.id();
        let request = crate::check::request(&ahead.targets, request);
        let mut loads = 0;
        while loads < 2 {
            let held = match self.checker(ahead, &id) {
                Ok(held) => held,
                Err(why) => return Reply::Cold { reason: format!("{why:#}") },
            };
            let checker = &*held.0;
            let asking = lock(&checker.asking);
            // Made way for, or loaded again by the check before this one.
            if checker.stopped.load(Relaxed) {
                continue;
            }
            *lock(&checker.asked) = Instant::now();
            let asked = checker.ask(&request);
            drop(asking);
            match asked {
                Ok(reply) if reply == "ok" => return Reply::Checked { errors: None },
                Ok(reply) if reply.starts_with(crate::check::LINT) => {
                    return Reply::Linted { answer: reply[crate::check::LINT.len()..].to_owned() };
                }
                Ok(reply) if reply == crate::check::RELOAD => {
                    loads += 1;
                    self.drop_checker(checker);
                }
                Ok(reply) => match NotBuilt::of(&reply) {
                    NotBuilt::Error(said) => return Reply::Checked { errors: Some(said) },
                    why => return Reply::Cold { reason: why.why() },
                },
                Err(why) => {
                    self.drop_checker(checker);
                    return Reply::Cold { reason: format!("{why:#}") };
                }
            }
        }
        Reply::Cold {
            reason: "the checker loaded the workspace twice and still had to again".to_owned(),
        }
    }

    /// The checker of the build `id`, started when there is none or it ended, counted among
    /// those answering under the lock that would make way for it. The one asked longest ago
    /// that nobody is asking makes way for a new one when there are [`CHECKERS`].
    fn checker(&self, ahead: &Ahead, id: &str) -> anyhow::Result<Answers> {
        let mut held = lock(&self.checkers);
        held.retain(|it| {
            let ended = it.ended().is_some();
            if ended {
                it.stop();
            }
            !ended
        });
        let checker = match held.iter().find(|it| it.id == id) {
            Some(checker) => checker.clone(),
            None => {
                let idle =
                    held.iter().enumerate().filter(|(_, it)| it.answering.load(Relaxed) == 0);
                let oldest = idle.min_by_key(|(_, it)| *lock(&it.asked)).map(|(at, _)| at);
                if held.len() >= CHECKERS
                    && let Some(oldest) = oldest
                {
                    held.remove(oldest).stop();
                }
                let dir = self.home.join("checkers").join(id);
                let checker = Arc::new(Checker::start(ahead, id, &dir)?);
                held.push(checker.clone());
                checker
            }
        };
        checker.answering.fetch_add(1, Relaxed);
        Ok(Answers(checker))
    }

    fn drop_checker(&self, checker: &Checker) {
        lock(&self.checkers).retain(|it| !std::ptr::eq(&**it, checker));
        checker.stop();
    }

    /// Starts a spare worker when there is none.
    fn spare(&self) {
        let mut spare = lock(&self.spare);
        if let Some(old) = spare.as_mut() {
            let Some(status) = exited(&mut old.child) else { return };
            eprintln!(
                "the spare worker of pid {} ended ({status}); its log {} ends with:\n{}",
                old.child.id(),
                old.log.display(),
                log_tail(&old.log)
            );
        }
        *spare = self.new_spare();
    }

    /// A spare worker started now, unless `BLAZE_SPARE=0` says there are none.
    fn new_spare(&self) -> Option<Spare> {
        if self.spare_when == Spares::Never {
            return None;
        }
        let dir = self.home.join("spares").join(self.spares.fetch_add(1, Relaxed).to_string());
        Spare::start(&dir).inspect_err(|why| eprintln!("no spare worker: {why:#}")).ok()
    }

    /// Stops the workers and the daemon. A client told it ends waits for its connection to
    /// close, which it does when this process exits.
    fn end(&self) -> ! {
        // Held until the process exits, so that no other thread starts a worker or a spare once
        // these are stopped: one started then would write in the home and outlive the daemon.
        // In the order a worker that loads a workspace takes them.
        let workers = lock(&self.workers);
        let mut spare = lock(&self.spare);
        if let Some(spare) = spare.as_mut() {
            spare.stop();
        }
        let checkers = lock(&self.checkers);
        for checker in checkers.iter() {
            checker.stop();
        }
        for worker in workers.values() {
            worker.let_go();
        }
        for worker in workers.values() {
            worker.finish();
        }
        let _ = std::fs::remove_file(socket_in(&self.home));
        eprintln!("daemon of pid {} ends at {:?}", std::process::id(), SystemTime::now());
        std::process::exit(0)
    }
}

/// The exit status of a process that has ended, or why it can't be told.
fn exited(child: &mut Child) -> Option<String> {
    match child.try_wait() {
        Ok(None) => None,
        Ok(Some(status)) => Some(status.to_string()),
        Err(error) => Some(format!("not known: {error}")),
    }
}

/// The line of a worker's log that says what went wrong: what it panicked with, else the
/// last error, else its last line.
fn last_words(log: &Path) -> String {
    let log = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    if let Some(panic) = last_panic(&log) {
        // `thread '…' panicked at <place>:`, and the message on the lines after it.
        let (first, message) = panic.split_once('\n').unwrap_or((panic, ""));
        let place = first.split_once(" panicked at ").map_or(first, |it| it.1);
        let message = message.lines().map(str::trim).collect::<Vec<_>>().join("; ");
        return format!("panicked at {place} {message}");
    }
    match last_error(&log) {
        Some(error) => {
            format!("Error: {}", error.lines().map(str::trim).collect::<Vec<_>>().join("; "))
        }
        None => lines.last().copied().unwrap_or("no output").to_owned(),
    }
}

/// The last panic of a thread that `said`, a process's stderr, has: `thread '…' panicked at
/// <place>:` and its message, up to a blank line or the note on backtraces. Not the panic
/// `std::thread::scope` then raises, which says only that a thread panicked, unless it is the
/// only one; the last, as one a compile caught was printed before it all the same.
pub fn last_panic(said: &str) -> Option<&str> {
    const SCOPE: &str = "a scoped thread panicked";
    let panics = said.match_indices("thread '").filter_map(|(at, _)| {
        let starts = at == 0 || said.as_bytes()[at - 1] == b'\n';
        let rest = &said[at..];
        (starts && rest.lines().next()?.contains("' panicked at ")).then_some(rest)
    });
    let panics = panics.map(|rest| {
        let mut end = 0;
        for (n, line) in rest.split_inclusive('\n').enumerate() {
            let next = n > 0 && line.starts_with("thread '");
            if next || line.trim().is_empty() || line.starts_with("note: ") {
                break;
            }
            end += line.len();
        }
        rest[..end].trim_end()
    });
    let (own, scope): (Vec<_>, Vec<_>) = panics.partition(|it| !it.ends_with(SCOPE));
    own.last().or(scope.last()).copied()
}

/// The last error a worker's log has, with what caused it, each cause on a line of its own:
/// the outermost error says what blaze was doing, and the innermost why it could not, which
/// is what the user acts on.
pub fn last_error(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let at = lines.iter().rposition(|it| it.starts_with("Error"))?;
    let mut said = lines[at].strip_prefix("Error: ").unwrap_or(lines[at]).to_owned();
    // An error of more than one line (`refused_error`'s functions) goes on until a blank line.
    let more = lines[at + 1..].iter().take_while(|it| !it.trim().is_empty()).count();
    for line in &lines[at + 1..at + 1 + more] {
        said.push_str(&format!("\n{line}"));
    }
    let mut rest = lines[at + 1 + more..].iter().skip_while(|it| it.trim().is_empty());
    if rest.next().is_some_and(|it| it.trim() == "Caused by:") {
        let mut caused = false;
        for line in rest.take_while(|it| it.starts_with("    ")) {
            let line = line.trim();
            // `0: what`, or a line more of the cause before it.
            let numbered = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
            match line.split_once(": ").filter(|(n, _)| numbered(n)) {
                Some((_, cause)) => said.push_str(&format!("\n{cause}")),
                None if caused => said.push_str(&format!("\n  {line}")),
                None => said.push_str(&format!("\n{line}")),
            }
            caused = true;
        }
    }
    Some(said)
}

/// The end of a worker's log, for the daemon's: where it panicked and with what, then its
/// last lines without the frames of a backtrace, each indented.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let frame = |line: &str| {
        let line = line.trim_start();
        let numbered = line.split_once(':').is_some_and(|(number, _)| {
            !number.is_empty() && number.bytes().all(|it| it.is_ascii_digit())
        });
        numbered || line.starts_with("at ") || line.to_lowercase().starts_with("stack backtrace")
    };
    let mut kept: Vec<&str> = match lines.iter().rposition(|it| it.contains(" panicked at ")) {
        Some(at) => lines[at..].iter().take(2).copied().collect(),
        None => Vec::new(),
    };
    let mut last: Vec<&str> =
        lines.iter().rev().copied().filter(|it| !frame(it)).take(LAST_LINES).collect();
    last.reverse();
    for line in last {
        if !kept.contains(&line) {
            kept.push(line);
        }
    }
    if kept.is_empty() {
        return "  (nothing)".to_owned();
    }
    kept.iter().map(|it| format!("  {it}")).collect::<Vec<_>>().join("\n")
}

/// The lock, also after a thread panicked with it: what it guards is whole between any two
/// statements here.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watcher_is_handed_the_builds_it_watches_and_a_workspace_keeps_its_last() {
        let began =
            |root: &str| Event::Began { build: 0, command: "build".into(), root: root.into() };
        let finished = Event::Finished { ok: true, summary: Default::default() };
        let record = |event: Event| Record { at_ns: 0, event };
        let mut watched = Watched::default();
        let first = watched.began(Path::new("/w/a"));
        watched.record(first, &record(began("/w/a")));
        let (to, every) = std::sync::mpsc::channel();
        watched.watchers.push(Watcher { root: None, to });
        let (to, of_a) = std::sync::mpsc::channel();
        watched.watchers.push(Watcher { root: Some("/w/a".into()), to });
        let other = watched.began(Path::new("/w/b"));
        watched.record(other, &record(began("/w/b")));
        watched.record(first, &record(finished.clone()));
        assert_eq!(every.try_iter().count(), 2, "every build's records");
        assert_eq!(of_a.try_iter().count(), 1, "only a's");
        assert_eq!(watched.watchers.len(), 1, "a's watcher is done when a's build is");
        let of = watched.of(Path::new("/w/a/src")).unwrap();
        assert!(
            of.done && of.records.len() == 2,
            "the last build of the workspace of a directory in it"
        );
        let again = watched.began(Path::new("/w/a"));
        watched.record(again, &record(began("/w/a")));
        assert!(!watched.of(Path::new("/w/a")).unwrap().done, "the one under way before the last");
        assert_eq!(watched.watching.len(), 2, "a's last is dropped once another begins");
    }

    /// A check that waits for its checker (one still loading, or stuck) keeps nothing else
    /// waiting: not `blaze status`, not a check of another build.
    #[cfg(unix)]
    #[test]
    fn a_check_waiting_for_its_checker_holds_up_nothing_else() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("blaze-checkers-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ahead = |project: &str| Ahead {
            config: WorkerConfig {
                repository: dir.clone(),
                project: project.into(),
                cfg_test: false,
                selection: crate::plan::Selection {
                    profile: "dev".to_owned(),
                    features: Vec::new(),
                    no_default_features: false,
                    all_features: false,
                    packages: Vec::new(),
                    workspace: false,
                    excludes: Vec::new(),
                    unstable: Vec::new(),
                    target: None,
                    dev_deps: false,
                    config: Vec::new(),
                    asked_in: None,
                },
                env: BTreeMap::new(),
                command_env: BTreeMap::new(),
                whole: false,
                tests_told: None,
            },
            root: dir.clone(),
            crates: Vec::new(),
            targets: Vec::new(),
        };
        let (stuck, answers) = (ahead("stuck"), ahead("answers"));
        // A checker that takes the request and never answers, and one that answers at once.
        let stuck_at = dir.join("stuck.sock");
        let never = UnixListener::bind(&stuck_at).unwrap();
        let answers_at = dir.join("answers.sock");
        let at_once = UnixListener::bind(&answers_at).unwrap();
        std::thread::spawn(move || {
            for stream in at_once.incoming() {
                let stream = stream.unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                (&stream).write_all(b"ok\n").unwrap();
            }
        });
        let checker = |ahead: &Ahead, socket: PathBuf| {
            let child = Command::new("sleep").arg("600").spawn().unwrap();
            Arc::new(Checker::of(&ahead.config.id(), child, socket, dir.join("log"), dir.clone()))
        };
        let daemon = Arc::new(Daemon {
            version: String::new(),
            home: dir.clone(),
            idle: None,
            memory_max: None,
            workers: Mutex::default(),
            spare: Mutex::default(),
            spare_when: Spares::Never,
            spares: AtomicUsize::new(0),
            checkers: Mutex::new(vec![checker(&stuck, stuck_at), checker(&answers, answers_at)]),
            asked: Arc::new(Mutex::new(Instant::now())),
            watched: Arc::default(),
            relays: Arc::default(),
            ui: Mutex::default(),
        });
        let waiting = {
            let daemon = daemon.clone();
            std::thread::spawn(move || daemon.check(&stuck, "check"))
        };
        // Its request is read once it waits for the answer.
        never.set_nonblocking(true).unwrap();
        let given_up = Instant::now() + Duration::from_secs(10);
        let asked = loop {
            match never.accept() {
                Ok((asked, _)) => break asked,
                Err(_) if waiting.is_finished() => {
                    panic!("the check asked nothing: {:?}", waiting.join().unwrap())
                }
                Err(_) if Instant::now() < given_up => std::thread::sleep(POLL),
                Err(why) => panic!("the check asked nothing in 10 s: {why}"),
            }
        };
        asked.set_nonblocking(false).unwrap();
        let mut line = String::new();
        BufReader::new(&asked).read_line(&mut line).unwrap();
        assert!(!waiting.is_finished(), "the check did not wait for its checker");
        let within = |what: &'static str, ask: Box<dyn FnOnce() -> Reply + Send>| {
            let (to, told) = std::sync::mpsc::channel();
            std::thread::spawn(move || _ = to.send(ask()));
            told.recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("{what} waited for another build's check"))
        };
        let status = {
            let daemon = daemon.clone();
            within("`blaze status`", Box::new(move || daemon.handle(Request::Status)))
        };
        assert!(matches!(status, Reply::Status(Status { ref workers, .. }) if workers.len() == 2));
        let other = {
            let daemon = daemon.clone();
            within("a check of another build", Box::new(move || daemon.check(&answers, "check")))
        };
        assert!(matches!(other, Reply::Checked { errors: None }), "{other:?}");
        // The stuck one hangs up, which ends its check.
        drop(asked);
        assert!(matches!(waiting.join().unwrap(), Reply::Cold { .. }));
        for checker in lock(&daemon.checkers).iter() {
            checker.stop();
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_workers_asked_longest_ago_are_stopped_for_the_rest_to_fit() {
        let now = Instant::now();
        let held = |bytes: u64, ago: u64, busy: bool| Held {
            bytes,
            asked: now - Duration::from_secs(ago),
            busy,
        };
        let holding = vec![
            ("oldest".to_owned(), held(4, 30, false)),
            ("old".to_owned(), held(4, 20, false)),
            ("answering".to_owned(), held(4, 40, true)),
            ("last".to_owned(), held(4, 0, false)),
        ];
        assert_eq!(over(&holding, 16), Vec::<String>::new());
        assert_eq!(over(&holding, 12), ["oldest"]);
        assert_eq!(over(&holding, 8), ["oldest", "old"]);
        // The busy one and the one asked last stay, whatever they hold.
        assert_eq!(over(&holding, 0), ["oldest", "old"]);
    }

    #[test]
    fn a_workers_last_error_is_said_with_what_caused_it() {
        let log = "symbols: 0 in 138.9µs\n\
            Error: Failed to build the build scripts of /w\n\
            \n\
            Caused by:\n    \
            0: blaze could not link the proc macro of `clap_derive v4.6.7` for the load\n    \
            1: the proc macro `clap_derive` cannot run as WebAssembly: no wasm32-wasip1 target\n         \
            name its package in `native-proc-macros` to expand it natively\n";
        assert_eq!(
            last_error(log).unwrap(),
            "Failed to build the build scripts of /w\n\
             blaze could not link the proc macro of `clap_derive v4.6.7` for the load\n\
             the proc macro `clap_derive` cannot run as WebAssembly: no wasm32-wasip1 target\n  \
             name its package in `native-proc-macros` to expand it natively"
        );
        assert_eq!(last_error("Error: one\n\nCaused by:\n    two\n").unwrap(), "one\ntwo");
        assert_eq!(last_error("Error: alone\n").unwrap(), "alone");
        let refused = "Error: `src/main.rs` reaches 1 function blaze could not compile, so it is not built:\n  \
            main: unsupported: callee `f`\n";
        assert_eq!(
            last_error(refused).unwrap(),
            "`src/main.rs` reaches 1 function blaze could not compile, so it is not built:\n  \
             main: unsupported: callee `f`"
        );
        assert_eq!(
            last_error("Error: two lines:\n  first\n\nCaused by:\n    why\n").unwrap(),
            "two lines:\n  first\nwhy"
        );
        assert_eq!(last_error("nothing wrong\n"), None);
    }

    /// The gate's dynamo jobs said only the scope's panic or the note on backtraces, of a
    /// build whose cause was the panic of a thread it had spawned.
    #[test]
    fn the_panic_said_is_the_threads_not_the_scopes() {
        let said = "   Compiling pear_codegen\n\
                    thread 'w1' panicked at crates/a.rs:1:2:\ncaught, compiled again\n\
                    thread 'w2' panicked at crates/hir-ty-codegen/mod.rs:10:5:\n\
                    no layout for `T`\nof `pear_codegen::parse`\n\
                    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\
                    thread 'main' panicked at crates/b.rs:3:4:\na scoped thread panicked\n";
        assert_eq!(
            last_panic(said).unwrap(),
            "thread 'w2' panicked at crates/hir-ty-codegen/mod.rs:10:5:\n\
             no layout for `T`\nof `pear_codegen::parse`"
        );
        let scope = "thread 'main' panicked at b.rs:3:4:\na scoped thread panicked\n\nnote: x\n";
        assert_eq!(
            last_panic(scope).unwrap(),
            "thread 'main' panicked at b.rs:3:4:\na scoped thread panicked"
        );
        assert_eq!(last_panic("a thread 'x' panicked at y\n"), None);
        assert_eq!(last_panic("Error: no `foo`\n"), None);
    }

    /// A build that waits for its workspace to load is answered with how the load ended, also
    /// when it ends between two looks at it: never with "still loading" (smoke (std) on main,
    /// 2026-10-29, after 22 s and after 238 s of loading).
    #[cfg(unix)]
    #[test]
    fn a_build_waiting_for_a_load_is_answered_with_how_it_ended() {
        let dir = std::env::temp_dir().join(format!("blaze-loading-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = WorkerConfig {
            repository: dir.clone(),
            project: "p".into(),
            cfg_test: false,
            selection: crate::plan::Selection {
                profile: "dev".to_owned(),
                features: Vec::new(),
                no_default_features: false,
                all_features: false,
                packages: Vec::new(),
                workspace: false,
                excludes: Vec::new(),
                unstable: Vec::new(),
                target: None,
                dev_deps: false,
                config: Vec::new(),
                asked_in: None,
            },
            env: BTreeMap::new(),
            command_env: BTreeMap::new(),
            whole: true,
            tests_told: None,
        };
        let checkout = Checkout {
            key: "k".to_owned(),
            generation: 1,
            kept: false,
            built_at: 0,
            config: config.clone(),
            root: dir.clone(),
            programs: Vec::new(),
            sources: Vec::new(),
            env: BTreeMap::new(),
            scripts: None,
        };
        let said = crate::errors::Said(Vec::new());
        for round in 0..200u32 {
            let loading = Loaded {
                generation: 1,
                root: dir.clone(),
                warmth: Warmth::Loading(Instant::now()),
                checkout: checkout.clone(),
            };
            let worker = Arc::new(Worker {
                project: dir.clone(),
                socket: dir.join("sock"),
                log: dir.join("log"),
                child: Mutex::new(Command::new("true").spawn().unwrap()),
                checkouts: Mutex::new(FxHashMap::from_iter([("k".to_owned(), loading)])),
                asked: Mutex::new(Instant::now()),
                answering: AtomicUsize::new(0),
                noted: AtomicBool::new(true),
                settled: true,
            });
            let daemon = Daemon {
                version: String::new(),
                home: dir.clone(),
                idle: None,
                memory_max: None,
                workers: Mutex::new(FxHashMap::from_iter([(config.id(), worker.clone())])),
                spare: Mutex::default(),
                spare_when: Spares::Never,
                spares: AtomicUsize::new(0),
                checkers: Mutex::default(),
                asked: Arc::new(Mutex::new(Instant::now())),
                watched: Arc::default(),
                relays: Arc::default(),
                ui: Mutex::default(),
            };
            let reply = std::thread::scope(|scope| {
                let waiting = scope.spawn(|| daemon.built(&checkout));
                // Ends the load at a different moment of the build's polling each round.
                let micros = u64::from(round * 97 % 20_000);
                std::thread::sleep(POLL * (round % 3) + Duration::from_micros(micros));
                worker.set(&checkout, Warmth::Broken(said.clone(), SystemTime::now()));
                waiting.join().unwrap()
            });
            assert!(matches!(reply, Reply::Synced { .. }), "round {round}: {reply:?}");
            _ = lock(&worker.child).wait();
        }
        _ = std::fs::remove_dir_all(&dir);
    }
}
