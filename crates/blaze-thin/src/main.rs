//! The thin `blaze`: the program (crate docs; docs/client.md, "The thin blaze").
//!
//! In order: read the command line and the environment ([`blaze_thin::route`]); ask the
//! launcher's kept lookup whether this blaze runs here ([`blaze_thin::pin`]); answer
//! `--version`; else make the CLI's request, connect, ask the daemon which engine it is, send
//! the request and take its answer. Each step that can't be sure the engine would do the same
//! ends in [`engine`]: the engine, run with the command line as typed. Once the request is
//! sent, an answer this does not print is handed to the engine whole ([`hand_over`]).

#![deny(unsafe_code)]

#[cfg(unix)]
#[expect(unsafe_code, reason = "a descriptor kept open across the exec that hands it to the engine")]
mod sys;

use std::{ffi::OsString, process::ExitCode};

/// Says how the command line after it would be run (`thin`, or `engine` and why), and runs
/// nothing: for a person or a harness asking why a command was not answered here. What the
/// daemon would answer is not asked, so a `thin` build may still end in the engine's hands.
#[cfg(unix)]
const ROUTE: &str = "--thin-route";

fn main() -> ExitCode {
    let started = std::time::Instant::now();
    let words: Vec<OsString> = std::env::args_os().skip(1).collect(); // alloc: once, the command line
    #[cfg(unix)]
    {
        unix::main(words, started)
    }
    #[cfg(not(unix))]
    {
        let _ = started;
        other::main(&words)
    }
}

/// Where the thin `blaze` has no socket to the daemon: every command line is the engine's,
/// run as a child, whose exit code is this one's.
#[cfg(not(unix))]
mod other {
    use std::{ffi::OsString, io::Write as _, process::ExitCode};

    pub(super) fn main(words: &[OsString]) -> ExitCode {
        let said = |what: String| {
            let _ = std::io::stderr().write_all(format!("error: {what}\n").as_bytes());
            ExitCode::from(101)
        };
        let engine = match std::env::current_exe() {
            Ok(this) => this.with_file_name(format!("{}{}", blaze_thin::ENGINE, std::env::consts::EXE_SUFFIX)),
            Err(error) => return said(format!("where this blaze is, beside which its engine is: {error}")),
        };
        match std::process::Command::new(&engine).args(words).status() {
            Ok(status) => ExitCode::from(u8::try_from(status.code().unwrap_or(101)).unwrap_or(101)),
            Err(error) => said(format!("could not run the engine {}: {error}", engine.display())),
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::OsString,
        io::{IsTerminal as _, Read as _, Write as _},
        os::{
            fd::AsRawFd as _,
            unix::{
                fs::{MetadataExt as _, OpenOptionsExt as _},
                net::UnixStream,
                process::CommandExt as _,
            },
        },
        path::{Path, PathBuf},
        process::ExitCode,
        time::{Duration, Instant},
    };

    use blaze_api::{Envelope, Request, Response, frames};
    use blaze_client::{place, start::Homes};
    use blaze_core::{
        exit::Exit,
        wait::{self, Clock as _, Deadline, Monotonic, Waited},
    };
    use blaze_report::terminal::Config;
    use blaze_thin::{
        ENGINE, VERSION, VERSION_LINE,
        handover::{Began, Held},
        pin, request,
        route::{self, Asked, Route},
        talk::{self, BUILD, Ended, Read},
    };

    use super::{ROUTE, sys};

    /// The most bytes one read takes.
    const RECEIVE: usize = 64 << 10;

    fn failed() -> ExitCode {
        ExitCode::from(u8::try_from(Exit::Failed.code()).unwrap_or(101))
    }

    /// `what` said as an error on stderr: this program's own failures, which are few (the
    /// engine is not beside it, or could not be run).
    fn error(what: &str) -> ExitCode {
        let _ = std::io::stderr().write_all(format!("error: {what}\n").as_bytes());
        failed()
    }

    pub(super) fn main(words: Vec<OsString>, started: Instant) -> ExitCode {
        if words.first().is_some_and(|it| it == ROUTE) {
            let said = match decide(&words[1..]) {
                Ok(_) => "thin\n".to_owned(),
                Err(why) => format!("engine: {why}\n"),
            };
            let _ = std::io::stdout().write_all(said.as_bytes());
            return ExitCode::SUCCESS;
        }
        let why = match decide(&words).and_then(|decided| run(decided, &words, started)) {
            Ok(code) => return code,
            Err(why) => why,
        };
        // Recorded for whoever traces this process; nothing is printed: the engine's output
        // is the command's.
        tracing::debug!(target: "blaze", why = %why, "thin: the engine runs the command");
        engine(&words)
    }

    /// A command line the thin `blaze` answers, with what the launcher's lookup found.
    struct Decided {
        /// `None` for `--version`.
        asked: Option<Asked>,
        home: PathBuf,
        cwd: PathBuf,
        found: pin::Found,
    }

    /// Whether the command line is this program's to answer, as far as the words, the
    /// environment and the kept pin lookup say; else why it is the engine's.
    fn decide(words: &[OsString]) -> Result<Decided, String> {
        let asked = match route::words(words) {
            Route::Engine(why) => return Err(why.to_string()),
            Route::Version => None,
            Route::Cargo(asked) => Some(asked),
        };
        if let Some(why) = route::env(std::env::vars_os().map(|(name, _)| name)) {
            return Err(why.to_string());
        }
        let home = pin::home(|name| std::env::var_os(name), blaze_core::home::record)
            .map_err(|why| format!("no BLAZE_HOME says where blaze's home is, and {why}"))?;
        // As the launcher reads it, unresolved: the kept lookup is named by it.
        let cwd = std::env::current_dir().map_err(|error| format!("the current directory: {error}"))?;
        let found = pin::here(&home, &cwd, VERSION).map_err(|unsure| match unsure {
            pin::Unsure::NoHome => "no home holds the launcher's pin lookup".to_owned(),
            pin::Unsure::NotKept => format!(
                "the launcher keeps no current pin lookup from {}: the engine's launcher makes it",
                cwd.display()
            ),
            pin::Unsure::Pins(pinned) => format!("the workspace pins blaze {pinned}, and this is {VERSION}"),
            pin::Unsure::Default => format!(
                "{} names another release than {VERSION}, or can't be read",
                home.join("default").display()
            ),
        })?;
        Ok(Decided { asked, home, cwd, found })
    }

    /// The command answered here: its exit code. `Err` is why the engine runs it instead, said
    /// before anything was written and before anything was asked of the daemon that the
    /// engine would not ask again.
    fn run(decided: Decided, words: &[OsString], started: Instant) -> Result<ExitCode, String> {
        let Decided { asked, home, cwd, found } = decided;
        let Some(asked) = asked else {
            let this = std::env::current_exe()
                .map_or_else(|_| "this blaze".to_owned(), |it| it.display().to_string());
            let said = format!("{VERSION_LINE}\n{}\n", pin::says(&this, &found));
            std::io::stdout()
                .write_all(said.as_bytes())
                .map_err(|error| format!("writing the version: {error}"))?;
            return Ok(ExitCode::SUCCESS);
        };
        let var = |name: &str| std::env::var(name).ok();
        let config = match std::fs::read_to_string(home.join("config.toml")) {
            Ok(text) => Config::from_toml(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::DEFAULT,
            Err(error) => return Err(format!("blaze's config.toml can't be read: {error}")),
        };
        let terminal = std::io::stderr().is_terminal();
        let color = request::color(request::color_flag(None, &config, var), var, terminal)?;
        let width = terminal.then(place::stderr_columns).flatten().map(u32::from);
        let env = place::env(std::env::vars_os())?;
        let place = place::find(&cwd, None)?;
        let files = place::config_files(&place, Some(&cargo_home(var)?))?;
        let sent = request::Sent {
            cwd: place::utf8(&place.cwd)?,
            root: place::utf8(&place.root)?,
            env,
            files,
            color,
            width,
        };
        let request = request::request(&asked, sent);

        // What an answer shows is the screen's (docs/terminal.md), as the engine shows it: what
        // stderr is, said once before the daemon is talked to.
        let mut detected =
            blaze_report::terminal::detect(&config, var, terminal, width.map(|it| it as u16));
        detected.caps.color = color;
        let caps = match (blaze_screen::live(var, terminal), place::stderr_size()) {
            (true, Some((cols, rows))) => {
                blaze_screen::Caps { live: true, cols, rows, unicode: blaze_screen::unicode(var) }
            }
            _ => blaze_screen::Caps::PIPE,
        };
        blaze_thin::answer::show_with(blaze_thin::answer::Shown {
            options: blaze_screen::Options {
                cwd: place.cwd.to_str().map(str::to_owned),
                home: blaze_core::home::user().ok().and_then(|it| it.into_os_string().into_string().ok()),
                quiet: asked.quiet,
                ..blaze_screen::Options::of(match asked.command {
                    blaze_thin::route::Command::Build => blaze_screen::Command::Build,
                    blaze_thin::route::Command::Run => blaze_screen::Command::Run,
                })
            },
            caps,
            terminal: detected,
            size: blaze_thin::answer::on_disk,
        });

        let homes = Homes::of(None, |name| std::env::var_os(name), blaze_core::home::record)?;
        let engine = Engine::beside()?;
        // No daemon: the engine starts one, saying so, as it always has.
        let stream = UnixStream::connect(&homes.socket)
            .map_err(|error| format!("no daemon answers at {}: {error}", homes.socket.display()))?;
        let mut daemon = Daemon::new(stream);
        let spoken = talk::Spoken {
            asked: &asked,
            request: &request,
            color,
            cwd: &place.cwd,
            pid: std::process::id(),
        };
        // The whole of what is said to the daemon, on this one connection ([`talk::talk`]).
        let is_engine = |theirs: &blaze_api::Executable| match engine.is(theirs) {
            true => Ok(()),
            false => Err(format!(
                "the daemon is {}, not the engine beside this program, {}",
                theirs.path,
                engine.path.display()
            )),
        };
        match talk::talk(&mut daemon, &spoken, is_engine, || started.elapsed()) {
            Ended::Engine(why) => Err(why),
            Ended::HandOver { request } => Ok(hand_over(daemon, &request, &engine, &homes.state, words)),
            Ended::Said { said, exec: None } => Ok(wrote(&said)),
            Ended::Said { said, exec: Some(exec) } => {
                let _ = std::io::stderr().write_all(said.as_bytes());
                let _ = std::io::stderr().flush();
                let mut command = std::process::Command::new(&exec.program);
                command
                    .args(&exec.args)
                    .envs(exec.env.iter().map(|(name, value)| (name, value)))
                    .current_dir(&exec.cwd);
                // This process becomes the program, which ends the command as it ends.
                let error = command.exec();
                // It could not be executed: cargo's error for that is the engine's to say.
                Err(format!("the program {} could not be executed: {error}", exec.program.display()))
            }
        }
    }

    /// `said` written to stderr: the command's exit code, 0 unless stderr is closed under it.
    fn wrote(said: &str) -> ExitCode {
        let mut err = std::io::stderr();
        match err.write_all(said.as_bytes()).and_then(|()| err.flush()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => failed(),
        }
    }

    /// cargo's home as the CLI finds it (`blaze_registry::cargo_home`), where the thin
    /// `blaze` finds the same with no other read: `CARGO_HOME` when it is absolute, else
    /// `.cargo` in an absolute `HOME`. A relative one, or neither, is the engine's to resolve.
    fn cargo_home(var: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
        let set = |name: &str| var(name).filter(|it| !it.is_empty()).map(PathBuf::from);
        match (set("CARGO_HOME"), set("HOME")) {
            (Some(home), _) if home.is_absolute() => Ok(home),
            (Some(_), _) => Err("CARGO_HOME is relative".to_owned()),
            (None, Some(home)) if home.is_absolute() => Ok(home.join(".cargo")),
            (None, _) => Err("no absolute HOME says where cargo's home is".to_owned()),
        }
    }

    /// The engine beside this program.
    struct Engine {
        /// Resolved: the file itself.
        path: PathBuf,
        len: u64,
        inode: u64,
        modified_ns: u64,
    }

    impl Engine {
        /// `blaze-engine` in the directory this program's file is in. An error names where.
        fn beside() -> Result<Engine, String> {
            let this = std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .map_err(|error| format!("where this blaze is, beside which its engine is: {error}"))?;
            let named = this.with_file_name(ENGINE);
            let what = |error: std::io::Error| format!("the engine {}: {error}", named.display());
            let path = std::fs::canonicalize(&named).map_err(what)?;
            let meta = std::fs::metadata(&path).map_err(what)?;
            // As the daemon says its own (`blaze_cargo::serve::executable`).
            let modified_ns = meta
                .modified()
                .map_err(what)?
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |it| it.as_nanos() as u64);
            Ok(Engine { path, len: meta.len(), inode: meta.ino(), modified_ns })
        }

        /// Whether the daemon's executable as it started, `theirs`, is this file, unchanged:
        /// the same file (path resolved, inode) with the same length and modification time.
        /// Any other may still be the same build; telling that hashes the engine, which is
        /// the engine's CLI's to do (`blaze_cargo::cli::daemon::builds`).
        fn is(&self, theirs: &blaze_api::Executable) -> bool {
            theirs.len == self.len
                && theirs.inode == self.inode
                && theirs.modified_ns == self.modified_ns
                && std::fs::canonicalize(&theirs.path).is_ok_and(|it| it == self.path)
        }
    }

    /// The connection to the daemon: requests out, responses in, and every byte read since
    /// [`Daemon::forget`] kept as it came, for the engine should it take the answer over.
    struct Daemon {
        stream: UnixStream,
        clock: Monotonic,
        frames: frames::Frames,
        /// The bytes read since the last [`Daemon::forget`], whole frames and what follows.
        read: Vec<u8>,
        buffer: Vec<u8>,
    }

    impl Daemon {
        fn new(stream: UnixStream) -> Daemon {
            Daemon {
                stream,
                clock: Monotonic::new(),
                frames: frames::Frames::new(),
                read: Vec::new(),            // alloc: none until the daemon answers
                buffer: vec![0; RECEIVE],    // alloc: once a command, every read's
            }
        }

    }

    impl talk::Link for Daemon {
        type Until = Deadline;

        /// `wait` from now, on this connection's clock.
        fn within(&self, wait: Duration) -> Deadline {
            Deadline::after(self.clock.now(), wait)
        }

        /// Sends `request` under `id`: its frame as sent.
        fn send(&mut self, id: u64, request: &Request) -> Result<Vec<u8>, String> {
            let frame = frames::encode(&Envelope::new(id, request)).map_err(|error| error.what.to_string())?;
            self.stream
                .write_all(&frame)
                .map_err(|error| format!("sending a request to the daemon: {error}"))?;
            Ok(frame)
        }

        /// How many bytes are read that are not yet a whole response.
        fn held(&self) -> usize {
            self.frames.held()
        }

        /// Lets go of the bytes read so far: what is read from here on is the next request's
        /// answer.
        fn forget(&mut self) {
            self.read.clear();
        }

        /// The next response, waiting for the daemon until `until`; `what` names the wait.
        fn next(&mut self, until: Deadline, what: &'static str) -> Result<Read, String> {
            // Bounded: each turn gives a response, ends at the deadline, at the daemon's
            // hang-up or on an error, or reads at least one byte of a frame of bounded length.
            loop {
                let item = self.frames.next_ref().map_err(|error| error.what.to_string())?;
                if let Some(item) = item {
                    let envelope = frames::decode::<Response>(item).map_err(|it| it.error.what.to_string())?;
                    return Ok(Read::Response(envelope));
                }
                let mut watched =
                    [libc::pollfd { fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
                let _span = tracing::info_span!(target: "blaze", "thin: wait for the daemon", what).entered();
                match wait::readable(what, &mut watched, until.finite(), &self.clock) {
                    Ok(Waited::Ready(_)) => {}
                    Ok(Waited::Deadline) => return Ok(Read::Deadline),
                    Err(error) => return Err(error.to_string()),
                }
                let read = self
                    .stream
                    .read(&mut self.buffer)
                    .map_err(|error| format!("reading the daemon's answer: {error}"))?;
                if read == 0 {
                    return Ok(Read::HungUp);
                }
                self.read.extend_from_slice(&self.buffer[..read]);
                self.frames.push(&self.buffer[..read]);
            }
        }
    }

    /// The engine, run with the command line as typed: this process becomes it. Returns only
    /// when it could not be run, having said so.
    fn engine(words: &[OsString]) -> ExitCode {
        let engine = match Engine::beside() {
            Ok(engine) => engine,
            Err(why) => return error(&why),
        };
        let failed = std::process::Command::new(&engine.path).args(words).exec();
        error(&format!("could not run the engine {}: {failed}", engine.path.display()))
    }

    /// The engine takes over the command this program began: it is handed the connection
    /// and, in a file with no name, the request and the answer read so far
    /// ([`blaze_thin::handover`]), and this process becomes it. Returns only when that could
    /// not be done, having said so: the request is sent, so the command is not run again.
    fn hand_over(daemon: Daemon, request: &[u8], engine: &Engine, state: &Path, words: &[OsString]) -> ExitCode {
        let handed = || -> Result<std::io::Error, String> {
            let held = Held { request, answer: &daemon.read }.write()?;
            // In the daemon's state directory, which is this user's alone; unlinked as soon
            // as it is open, so it is gone with the engine whatever becomes of it.
            let path = state.join(format!("thin-{}.answer", std::process::id()));
            let io = |what: &str, error: std::io::Error| format!("{what} {}: {error}", path.display());
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io("removing an earlier", error)),
            }
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|error| io("making", error))?;
            std::fs::remove_file(&path).map_err(|error| io("unlinking", error))?;
            file.write_all(&held).map_err(|error| io("writing", error))?;
            sys::keep_across_exec(&file).map_err(|error| io("keeping open", error))?;
            sys::keep_across_exec(&daemon.stream)
                .map_err(|error| format!("keeping the daemon's connection open: {error}"))?;
            let began = Began { socket: daemon.stream.as_raw_fd(), file: file.as_raw_fd(), id: BUILD };
            Ok(std::process::Command::new(&engine.path).args(began.words()).args(words).exec())
        };
        match handed() {
            Ok(failed) => error(&format!("could not run the engine {}: {failed}", engine.path.display())),
            Err(why) => error(&format!("handing the command to the engine: {why}")),
        }
    }
}
