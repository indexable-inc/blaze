//! The processes that build a load's build scripts and proc macros. Each build of one is its
//! own program over the same standard library, and most of them link the same crates (`syn`,
//! `quote`, `proc-macro2`): a process of their own apiece loads and resolves all of that again,
//! seconds and a gigabyte each, dozens at once in a cold build. While a load holds [`hold`],
//! the builds of a kind are requests to one engine that took the first of them as its first
//! program (`--builds-only`) and adds each other's `rust-project.json` to the database they
//! share, building the programs that are asked for at once side by side.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// What one build asks: the program, its crates and where what is linked of it goes.
pub struct Asked<'a> {
    pub project: &'a Path,
    /// The crate to build and the others in its program (`proc_macro`).
    pub crates: Vec<&'a str>,
    pub out: &'a Path,
    pub exe: &'a Path,
    pub dep_info: &'a Path,
    pub inputs: Option<&'a Path>,
    pub shared: bool,
    /// What the builds of its kind are compiled for, the flags of the process (`--target`).
    pub kind: Vec<&'a str>,
    /// The variables of the process that builds it.
    pub env: BTreeMap<String, String>,
}

impl Asked<'_> {
    /// The flags of a process started for it alone, or as the first program of one.
    pub fn flags(&self) -> Vec<OsString> {
        let mut flags: Vec<OsString> = vec![self.project.into()];
        for krate in &self.crates {
            flags.extend(["--crate".into(), (*krate).into()]);
        }
        flags.extend(["--out".into(), self.out.into(), "--exe".into(), self.exe.into()]);
        flags.extend(["--dep-info".into(), self.dep_info.into()]);
        if let Some(inputs) = self.inputs {
            flags.extend(["--inputs".into(), inputs.into()]);
        }
        if self.shared {
            flags.push("--shared".into());
        }
        flags.extend(self.kind.iter().map(OsString::from));
        flags
    }

    /// The line that asks a process that is up for it, as `Target::fields` in the engine
    /// reads it: the crates each with a comma after it, as the program has no others.
    fn line(&self) -> Result<String, String> {
        let text = |path: &Path| {
            let text = path.to_str().ok_or_else(|| format!("{} is not text", path.display()))?;
            match text.contains(['\t', '\n']) {
                true => Err(format!("{text:?} has a tab or a line end in it")),
                false => Ok(text.to_owned()),
            }
        };
        let crates: String = self.crates.iter().map(|it| format!("{it},")).collect();
        let inputs = self.inputs.map(text).transpose()?.unwrap_or_default();
        let shared = if self.shared { "shared" } else { "" };
        Ok(format!(
            "sync\t{}\t{crates}\t{}\t\t\t\t{}\t{shared}\t\t{}\t{inputs}\n",
            text(self.project)?,
            text(self.out)?,
            text(self.exe)?,
            text(self.dep_info)?,
        ))
    }
}

/// The builders of the loads that hold them, and how many do: two loads of one process at a
/// time share them, and they end when the last lets go.
static HELD: Mutex<Option<(Arc<Builders>, usize)>> = Mutex::new(None);

/// While this is kept, the builds of this process are requests to builders ([`build`]).
pub struct Held(());

/// Has the builds of build scripts and proc macros from here on go to builders, with their
/// sockets in a directory of `home` and their logs in `logs`, until what this returns is
/// dropped.
pub fn hold(home: &Path, logs: &Path) -> Result<Held, String> {
    let mut held = lock(&HELD);
    match &mut *held {
        Some((_, holders)) => *holders += 1,
        None => {
            let dir = home.join(format!("builders-{}", std::process::id()));
            crate::socket::private_dir(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
            std::fs::create_dir_all(logs).map_err(|it| format!("{}: {it}", logs.display()))?;
            let builders = Builders {
                dir,
                logs: logs.to_owned(),
                kinds: Mutex::default(),
                started: Mutex::new(0),
            };
            *held = Some((Arc::new(builders), 1));
        }
    }
    Ok(Held(()))
}

impl Drop for Held {
    /// The last to let go ends the builders: each once the store has what it built, which
    /// the load does not wait for.
    fn drop(&mut self) {
        let builders = {
            let mut held = lock(&HELD);
            let Some((builders, holders)) = &mut *held else { return };
            *holders -= 1;
            if *holders > 0 {
                return;
            }
            let builders = builders.clone();
            *held = None;
            builders
        };
        let kinds = std::mem::take(&mut *lock(&builders.kinds));
        let mut workers: Vec<Arc<Worker>> = Vec::new();
        for kind in kinds.into_values() {
            workers.extend(lock(&kind.state).up().cloned());
        }
        for worker in &workers {
            drop(lock(&worker.child).stdin.take());
        }
        std::thread::spawn(move || {
            for worker in &workers {
                worker.finish();
            }
            let _ = std::fs::remove_dir_all(&builders.dir);
        });
    }
}

/// Builds what `asked` says with the builders a load holds, or `None` when none does. What
/// it says is what the build printed, or why it did not build.
pub fn build(asked: &Asked<'_>) -> Option<Result<String, String>> {
    let builders = lock(&HELD).as_ref()?.0.clone();
    Some(builders.build(asked))
}

/// Whether a load holds builders.
pub fn held() -> bool {
    lock(&HELD).is_some()
}

struct Builders {
    /// Where their sockets are, which goes with them.
    dir: PathBuf,
    /// Where their logs are, which stay.
    logs: PathBuf,
    /// One for each kind of build: its flags and its variables.
    kinds: Mutex<BTreeMap<(Vec<String>, BTreeMap<String, String>), Arc<Kind>>>,
    /// How many processes were started, which names the next one's socket.
    started: Mutex<usize>,
}

/// The builder of one kind of build, which the first build of it starts.
#[derive(Default)]
struct Kind {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
enum State {
    #[default]
    None,
    /// A build is starting it, as its first program.
    Starting,
    Up(Arc<Worker>),
}

impl State {
    fn up(&self) -> Option<&Arc<Worker>> {
        match self {
            State::Up(worker) => Some(worker),
            _ => None,
        }
    }
}

struct Worker {
    socket: PathBuf,
    log: PathBuf,
    child: Mutex<Child>,
    /// The variables it was started with, which each request says again: one that says
    /// others has it build every program again for a process of those.
    env: ide_db::base_db::Env,
}

/// How long a builder has to end once its input is closed, which it does once the store has
/// what it built.
const SETTLE: Duration = Duration::from_secs(30);

impl Builders {
    fn build(&self, asked: &Asked<'_>) -> Result<String, String> {
        let key = (asked.kind.iter().map(|it| (*it).to_owned()).collect(), asked.env.clone());
        let kind = lock(&self.kinds).entry(key).or_default().clone();
        let mut state = lock(&kind.state);
        loop {
            match &*state {
                State::Up(worker) => {
                    let worker = worker.clone();
                    drop(state);
                    let said = worker.ask(asked);
                    // One that ended is replaced by the next build of its kind; the builds it
                    // had under way failed, saying so.
                    if said.is_err() && worker.ended() {
                        let mut state = lock(&kind.state);
                        if state.up().is_some_and(|it| Arc::ptr_eq(it, &worker)) {
                            *state = State::None;
                        }
                    }
                    return said;
                }
                State::Starting => {
                    state = kind.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
                }
                State::None => {
                    *state = State::Starting;
                    drop(state);
                    let started = self.start(asked);
                    let mut state = lock(&kind.state);
                    let said = match started {
                        Ok((worker, said)) => {
                            *state = State::Up(Arc::new(worker));
                            Ok(said)
                        }
                        Err(why) => {
                            *state = State::None;
                            Err(why)
                        }
                    };
                    kind.changed.notify_all();
                    return said;
                }
            }
        }
    }

    /// Starts a builder with `asked` as its first program, and waits for it to have built it.
    fn start(&self, asked: &Asked<'_>) -> Result<(Worker, String), String> {
        let at = {
            let mut started = lock(&self.started);
            *started += 1;
            *started
        };
        let socket = self.dir.join(format!("{at}.sock"));
        let log = self.logs.join(format!("{at}.log"));
        let mut env = asked.env.clone();
        env.insert(super::daemon::ENDS_WITH_INPUT.to_owned(), "1".to_owned());
        let output = std::fs::File::create(&log).map_err(|it| format!("{}: {it}", log.display()))?;
        let copy = output.try_clone().map_err(|it| it.to_string())?;
        let mut command = Command::new(std::env::current_exe().map_err(|it| it.to_string())?);
        command.args(asked.flags()).arg("--builds-only").arg("--socket").arg(&socket);
        command.env_clear().envs(&env);
        command.stdin(Stdio::piped()).stdout(output).stderr(copy);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command.spawn().map_err(|it| format!("blaze did not run: {it}"))?;
        let env = env.into_iter().collect();
        let worker = Worker { socket, log, child: Mutex::new(child), env };
        // It listens once its first program is built, and ends with the error when that fails.
        loop {
            if let Some(status) = lock(&worker.child).try_wait().map_err(|it| it.to_string())? {
                let said = std::fs::read_to_string(&worker.log).unwrap_or_default();
                let error = said.rfind("\nError: ").map(|at| said[at + 1..].trim_end());
                let last = said.lines().rev().find(|it| !it.trim().is_empty()).unwrap_or("");
                return Err(format!("blaze did not build it ({status}): {}", error.unwrap_or(last)));
            }
            if worker.socket.exists() {
                let said = std::fs::read_to_string(&worker.log).unwrap_or_default();
                return Ok((worker, said));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Worker {
    fn ended(&self) -> bool {
        !matches!(lock(&self.child).try_wait(), Ok(None))
    }

    fn ask(&self, asked: &Asked<'_>) -> Result<String, String> {
        let ended = || match lock(&self.child).try_wait() {
            Ok(Some(status)) => format!("the builder ended ({status}); its log is {}", self.log.display()),
            _ => format!("its log is {}", self.log.display()),
        };
        let start = Instant::now();
        let request = asked.line()?;
        let env = crate::process::encode(&self.env);
        let asking = || -> std::io::Result<String> {
            let mut stream = crate::socket::connect(&self.socket)?;
            stream.write_all(request.as_bytes())?;
            stream.write_all(format!("{env}\n").as_bytes())?;
            let mut reply = String::new();
            BufReader::new(stream).read_line(&mut reply)?;
            Ok(reply.trim_end().to_owned())
        };
        let reply = asking().map_err(|it| format!("asking the builder: {it} ({})", ended()))?;
        match reply {
            _ if reply.starts_with("ok") => {
                Ok(format!("{reply}\nbuilt by the builder whose log is {} in {:.1?}\n", self.log.display(), start.elapsed()))
            }
            _ if reply.is_empty() => Err(format!("blaze did not build it: {}", ended())),
            _ => Err(format!("blaze did not build it: {reply} ({})", ended())),
        }
    }

    /// Waits for it to end once its input is closed, [`SETTLE`] at most, then ends it.
    fn finish(&self) {
        let mut child = lock(&self.child);
        let given_up = Instant::now() + SETTLE;
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < given_up {
            std::thread::sleep(Duration::from_millis(20));
        }
        #[cfg(unix)]
        if let (Ok(None), Ok(group)) = (child.try_wait(), libc::pid_t::try_from(child.id())) {
            // SAFETY: it takes no pointer; the group is the builder's, not yet waited for.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
