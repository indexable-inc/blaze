//! Where the daemon is, and its start when none listens there.
//!
//! The socket is the one the CLI finds ([`blaze_api::client::socket`]): `v2.sock` in
//! `$BLAZE_HOME`, else in `$XDG_RUNTIME_DIR/blaze`, else in `~/.cache/blaze`. A connect to it is
//! all a warm client does. Where launchd or systemd socket-activates the daemon
//! ([`blaze_api::units`]), that connect starts it too. Only where no unit serves the socket and
//! nothing listens is the `blaze` program run, once, as the CLI runs it
//! (`crates/blaze-cargo/src/cli/daemon.rs`, `started` and `heard`): `blaze serve --ready-fd N`
//! in a process group of its own, its output appended to `serve.log` in its state directory,
//! waited for until it says on the pipe that it listens. The pipe's bytes are
//! `blaze_cargo::serve::Phase`'s and its `INSIDE` record; they are read here by [`Hearing`]
//! because this crate links none of blaze-cargo. That is two readers of one pipe: the CLI's
//! is to become this one, once its start moves onto this crate.
//!
//! A client is of no build of blaze, so the CLI's two checks of the daemon it reaches (that it
//! is the CLI's own build, and keeps the settings the command sets) are not made here: the
//! schema is what a client and a daemon must share, and a daemon of another is refused by name
//! on its first answer ([`crate::Fault::Schema`]).

use std::{
    ffi::OsString,
    fmt, io,
    path::{Path, PathBuf},
};

use blaze_api::units::Installed;
use blaze_core::wait::{self, Deadline, Monotonic, Waited};

/// Whether a daemon is started when none answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    IfMissing,
    Never,
}

/// Where a daemon listens and keeps its state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Homes {
    pub socket: PathBuf,
    /// The daemon's state directory, `v2` under blaze's root: where its log is.
    pub state: PathBuf,
    /// The `BLAZE_HOME` a daemon started for this client is given; `None` leaves it the
    /// client's own environment.
    pub home: Option<PathBuf>,
}

impl Homes {
    /// Of `home` when one is named (`BLAZE_HOME`'s meaning: the socket and the state are both
    /// in it), else of the environment `var` reads: the socket as the CLI finds it, the state
    /// under blaze's root (`$BLAZE_HOME`, else `$XDG_CACHE_HOME/blaze`, else `~/.cache/blaze`,
    /// as `blaze_manifest::home::root`). An empty variable is unset.
    pub fn of(home: Option<&Path>, var: impl Fn(&str) -> Option<OsString>) -> Result<Homes, String> {
        let set = |name: &str| var(name).filter(|it| !it.is_empty()).map(PathBuf::from);
        let absolute = |dir: PathBuf| {
            std::path::absolute(&dir).map_err(|error| {
                // alloc: error path, a relative home and no current directory.
                format!("blaze's home {} against the current directory: {error}", dir.display())
            })
        };
        if let Some(home) = home.map(Path::to_owned).or_else(|| set("BLAZE_HOME")) {
            let home = absolute(home)?;
            return Ok(Homes {
                socket: home.join("v2.sock"),
                state: home.join("v2"),
                home: Some(home),
            });
        }
        let user = set("HOME").or_else(|| set("USERPROFILE"));
        let Some(user) = user else {
            return Err(
                "no BLAZE_HOME or HOME says where blaze's daemon listens and keeps its state".to_owned()
            );
        };
        let runtime = set("XDG_RUNTIME_DIR").map(|it| it.join("blaze"));
        let cache = set("XDG_CACHE_HOME").map(|it| it.join("blaze"));
        let socket = runtime.unwrap_or_else(|| user.join(".cache/blaze")).join("v2.sock");
        let state = cache.unwrap_or_else(|| user.join(".cache/blaze")).join("v2");
        Ok(Homes { socket: absolute(socket)?, state: absolute(state)?, home: None })
    }
}

/// Why no daemon was started.
#[derive(Debug)]
pub enum StartError {
    /// A unit socket-activates the daemon at `socket` and nothing listens there: starting one
    /// beside it would be a second daemon on one state.
    Activated { socket: PathBuf, unit: PathBuf },
    /// The installed units could not be read.
    Units(blaze_api::units::UnitError),
    /// No `blaze` program to start: where it was looked for.
    NoBlaze { looked: Vec<PathBuf> },
    /// The state directory, the log or the pipe could not be made, or the program not run.
    Io { what: String, error: io::Error },
    /// The daemon said something on its ready pipe that is no part of a start.
    Said { what: String },
    /// It was started and did not say it listens by the deadline.
    NotListening { socket: PathBuf, log: PathBuf, pid: u32, doing: &'static str, inside: Option<String> },
    /// It was started and closed its ready pipe before it listened: `ended` is how its process
    /// ended, when it had.
    Ended { socket: PathBuf, log: PathBuf, pid: u32, doing: &'static str, ended: Option<String> },
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Activated { socket, unit } => write!(
                f,
                "blaze's daemon is socket-activated for {} ({}) and nothing listens there: `blaze daemon \
                 install` loads its units again",
                socket.display(),
                unit.display()
            ),
            StartError::Units(error) => write!(f, "reading blaze's socket units: {error}"),
            StartError::NoBlaze { looked } => {
                f.write_str(
                    "no daemon listens and no `blaze` program was found to start one (rule no-fallback: \
                     nothing else is tried); looked at",
                )?;
                for (at, path) in looked.iter().enumerate() {
                    write!(f, "{} {}", if at == 0 { "" } else { "," }, path.display())?;
                }
                f.write_str("; name it in the connect's `blaze`, or put it on PATH")
            }
            StartError::Io { what, error } => write!(f, "{what}: {error}"),
            StartError::Said { what } => f.write_str(what),
            StartError::NotListening { socket, log, pid, doing, inside } => write!(
                f,
                "the daemon (`blaze serve`, pid {pid}) was started and did not listen at {} by the connect's \
                 deadline: it was {doing}{}; its log is {}",
                socket.display(),
                Inside(inside.as_deref()),
                log.display()
            ),
            StartError::Ended { socket, log, pid, doing, ended } => write!(
                f,
                "the daemon (`blaze serve`, pid {pid}) was started and {} while {doing}, before it listened \
                 at {}; its log {} says why",
                match ended {
                    Some(how) => how.as_str(),
                    None => "closed its ready pipe",
                },
                socket.display(),
                log.display()
            ),
        }
    }
}

impl std::error::Error for StartError {}

/// What a start last said its thread was inside, as an error says it.
struct Inside<'a>(Option<&'a str>);

impl fmt::Display for Inside<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None | Some("") => Ok(()),
            Some(span) => write!(f, ", inside {span}"),
        }
    }
}

/// A phase of the daemon's start, as it says them on its ready pipe
/// (`blaze_cargo::serve::Phase`), in order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Phase {
    Launched,
    Waiting,
    State,
    Setup,
    Listening,
}

impl Phase {
    const ALL: [Phase; 5] =
        [Phase::Launched, Phase::Waiting, Phase::State, Phase::Setup, Phase::Listening];

    const fn byte(self) -> u8 {
        match self {
            Phase::Launched => b'0',
            Phase::Waiting => b'w',
            Phase::State => b's',
            Phase::Setup => b'p',
            Phase::Listening => b'\n',
        }
    }

    /// What the daemon does in it, as a line says.
    pub const fn doing(self) -> &'static str {
        match self {
            Phase::Launched => "finding its build and its state",
            Phase::Waiting => "waiting for the daemon before it, which writes its image, to end",
            Phase::State => "reading its state (the VFS journal, the daemon image and the store)",
            Phase::Setup => "installing its store and the toolchain's proc macros",
            Phase::Listening => "listening",
        }
    }
}

/// The tag of the pipe's other record (`blaze_cargo::serve::INSIDE`): what the start's thread
/// is inside, as the tag, a little-endian `u16` length and that many bytes of UTF-8.
const INSIDE: u8 = b'i';

/// The most bytes of text an [`INSIDE`] record carries (`blaze_cargo::serve::INSIDE_MAX`).
const INSIDE_MAX: usize = 512;

/// What the daemon said on its ready pipe so far, read as its bytes come, cut anywhere.
#[derive(Debug)]
pub struct Hearing {
    phase: Phase,
    /// The text of the last record of what the start is inside.
    inside: Option<String>,
    /// Bytes of a record not yet whole.
    held: Vec<u8>,
}

impl Default for Hearing {
    fn default() -> Hearing {
        Hearing::new()
    }
}

impl Hearing {
    pub fn new() -> Hearing {
        // alloc: one start's pipe, a record of at most 515 bytes held.
        Hearing { phase: Phase::Launched, inside: None, held: Vec::new() }
    }

    /// The last phase said.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Takes the pipe's next `bytes`: true once the daemon says it listens. A phase said again
    /// or out of order, a byte that is neither a phase nor a record, and a record too long or
    /// not UTF-8 are refused, named.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<bool, String> {
        self.held.extend_from_slice(bytes);
        let mut at = 0;
        // Bounded: each turn takes at least one byte of `held`, or ends.
        let listening = loop {
            let Some(&tag) = self.held.get(at) else { break false };
            if tag == INSIDE {
                let Some(&[low, high]) = self.held.get(at + 1..at + 3) else { break false };
                let len = usize::from(u16::from_le_bytes([low, high]));
                if len > INSIDE_MAX {
                    // alloc: error path, a record too long.
                    return Err(format!(
                        "the daemon said on its ready pipe, while {}, what its start is inside in {len} bytes: \
                         at most {INSIDE_MAX} are said",
                        self.phase.doing()
                    ));
                }
                let Some(text) = self.held.get(at + 3..at + 3 + len) else { break false };
                let text = std::str::from_utf8(text).map_err(|error| {
                    // alloc: error path, a record that is not text.
                    format!(
                        "the daemon said on its ready pipe, while {}, what its start is inside in text that \
                         is not UTF-8: {error}",
                        self.phase.doing()
                    )
                })?;
                // alloc: the last record of a start, kept for its error.
                self.inside = Some(text.to_owned());
                at += 3 + len;
                continue;
            }
            match Phase::ALL.into_iter().find(|it| it.byte() == tag) {
                Some(Phase::Listening) => break true,
                Some(next) if next > self.phase => {
                    self.phase = next;
                    at += 1;
                }
                said => {
                    // alloc: error path, a byte that is no later phase.
                    return Err(format!(
                        "the daemon said {:?} on its ready pipe while {}: no later phase of a start",
                        char::from(tag),
                        said.unwrap_or(self.phase).doing()
                    ));
                }
            }
        };
        self.held.drain(..at);
        Ok(listening)
    }
}

/// The `blaze` program to start the daemon with: the one `named`, else `blaze` beside this
/// program, else the first `blaze` on `path` (`PATH`'s value). The places are one search, in
/// that order, and the error names each.
pub fn program(
    named: Option<&Path>,
    this: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, StartError> {
    if let Some(named) = named {
        return match std::fs::metadata(named) {
            Ok(it) if it.is_file() => Ok(named.to_owned()),
            // alloc: error path, the one place looked at.
            _ => Err(StartError::NoBlaze { looked: vec![named.to_owned()] }),
        };
    }
    let beside = this.and_then(Path::parent).map(|it| it.join("blaze"));
    let on_path = path.into_iter().flat_map(std::env::split_paths).map(|it| it.join("blaze"));
    // alloc: once a start, the places looked at, for its error.
    let mut looked = Vec::new();
    for candidate in beside.into_iter().chain(on_path) {
        if std::fs::metadata(&candidate).is_ok_and(|it| it.is_file()) {
            return Ok(candidate);
        }
        looked.push(candidate);
    }
    Err(StartError::NoBlaze { looked })
}

/// Whether a connect failed because nothing listens at the socket: no file there, or one a
/// daemon that is gone left.
pub fn nothing_listens(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
}

/// Starts the daemon of `homes` with the program `blaze` and waits, at most until `deadline`
/// on `clock`, for it to say it listens.
pub fn daemon(
    homes: &Homes,
    blaze: &Path,
    deadline: Deadline,
    clock: &Monotonic,
) -> Result<(), StartError> {
    use std::{
        io::Read as _,
        os::{
            fd::AsRawFd as _,
            unix::{
                fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
                process::CommandExt as _,
            },
        },
    };
    let failed = |what: String| move |error: io::Error| StartError::Io { what, error };
    if blaze_api::activation::Manager::here().is_some()
        && std::env::var_os("HOME").is_some_and(|it| !it.is_empty())
    {
        let place = blaze_api::units::place_here().map_err(StartError::Units)?;
        let installed =
            blaze_api::units::installed(&place, &homes.socket).map_err(StartError::Units)?;
        if installed == Installed::Yes {
            return Err(StartError::Activated {
                // clone: error path.
                socket: homes.socket.clone(),
                unit: blaze_api::units::main_file(&place),
            });
        }
    }
    // The state directory is the user's alone, as the CLI makes it (blaze-cargo's
    // `socket::private_dir`): made closed, one of the user's left open is closed, and another
    // user's is refused. The daemon checks the directories above it as it starts.
    let state = &homes.state;
    // alloc: error path texts below, each made once a start.
    let named = format!("the daemon's directory {}", state.display());
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(state).map_err(failed(named.clone()))?;
    let is = std::fs::symlink_metadata(state).map_err(failed(named.clone()))?;
    if !is.is_dir() || is.uid() != blaze_api::activation::uid() {
        let why = if is.is_dir() { "belongs to another user" } else { "is not a directory" };
        let error = io::Error::new(io::ErrorKind::PermissionDenied, why);
        return Err(StartError::Io { what: named, error });
    }
    if is.mode() & 0o077 != 0 {
        std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700))
            .map_err(failed(named.clone()))?;
    }
    let log = state.join("serve.log");
    let logged = format!("the daemon's log {}", log.display());
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log)
        .map_err(failed(logged.clone()))?;
    let (mut ready, said) =
        std::io::pipe().map_err(failed("making the pipe the daemon says it listens on".to_owned()))?;
    let fd = said.as_raw_fd();
    // `Command::new`, as the CLI's own start of the daemon: the program is `blaze`, not a
    // tool of the toolchain, and its working directory is this process's, as the CLI's.
    let mut daemon = std::process::Command::new(blaze);
    daemon.arg("serve").arg("--ready-fd").arg(fd.to_string());
    if let Some(home) = &homes.home {
        daemon.env("BLAZE_HOME", home);
    }
    daemon
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone().map_err(failed(logged))?)
        .stderr(file);
    daemon.process_group(0);
    crate::sys::inherit_in_child(&mut daemon, fd);
    let mut child = daemon
        .spawn()
        .map_err(failed(format!("starting the daemon (`{} serve`)", blaze.display())))?;
    // The daemon's copy alone is left, so its end closes the pipe.
    drop(said);
    let pid = child.id();
    let _span = tracing::info_span!(
        target: "blaze",
        "client: wait for the daemon it started to listen",
        pid,
    )
    .entered();
    let mut hearing = Hearing::new();
    let mut bytes = [0u8; 1024];
    // Bounded: each turn reads at least one byte, or ends; every wait is to `deadline`.
    loop {
        let mut watched = [libc::pollfd { fd: ready.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        let waited = wait::readable(
            "the daemon it started to say it listens",
            &mut watched,
            deadline.finite(),
            clock,
        );
        // clone: error paths, the places an error names.
        let places = || (homes.socket.clone(), log.clone());
        let doing = hearing.phase.doing();
        match waited {
            Ok(Waited::Ready(_)) => {}
            Ok(Waited::Deadline) => {
                let inside = hearing.inside.take();
                let (socket, log) = places();
                return Err(StartError::NotListening { socket, log, pid, doing, inside });
            }
            Err(error) => {
                let what = "waiting for the daemon it started to say it listens".to_owned();
                return Err(StartError::Io { what, error: io::Error::other(error) });
            }
        }
        let read = match ready.read(&mut bytes) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let what = "reading the pipe the daemon says it listens on".to_owned();
                return Err(StartError::Io { what, error });
            }
        };
        if read == 0 {
            // Its pipe closed with no word that it listens: how it ended, if it has by now.
            let ended = match child.try_wait() {
                Ok(Some(status)) => Some(format!("ended ({status})")), // alloc: error path
                Ok(None) | Err(_) => None,
            };
            let (socket, log) = places();
            return Err(StartError::Ended { socket, log, pid, doing, ended });
        }
        if hearing.feed(&bytes[..read]).map_err(|what| StartError::Said { what })? {
            // The daemon runs on in its own process group; its handle is let go, as the CLI's
            // is when its command ends.
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_home_holds_the_socket_and_the_state() {
        let homes = Homes::of(Some(Path::new("/h")), |_| None).unwrap();
        assert_eq!(homes.socket, Path::new("/h/v2.sock"));
        assert_eq!(homes.state, Path::new("/h/v2"));
        assert_eq!(homes.home.as_deref(), Some(Path::new("/h")));
    }

    /// The socket is where the CLI finds it and the state where the daemon keeps it: the
    /// runtime directory holds the one, the cache directory the other.
    #[test]
    fn the_environment_places_the_socket_and_the_state_as_the_cli_does() {
        let var = |pairs: &'static [(&str, &str)]| {
            move |name: &str| {
                pairs.iter().find(|(it, _)| *it == name).map(|(_, value)| OsString::from(value))
            }
        };
        let homes = Homes::of(None, var(&[("HOME", "/u")])).unwrap();
        assert_eq!(homes.socket, Path::new("/u/.cache/blaze/v2.sock"));
        assert_eq!(homes.state, Path::new("/u/.cache/blaze/v2"));
        let split = [("HOME", "/u"), ("XDG_RUNTIME_DIR", "/run/u"), ("XDG_CACHE_HOME", "/c")];
        let homes = Homes::of(None, var(&split)).unwrap();
        assert_eq!(homes.socket, Path::new("/run/u/blaze/v2.sock"));
        assert_eq!(homes.state, Path::new("/c/blaze/v2"));
        let homes = Homes::of(None, var(&[("HOME", "/u"), ("BLAZE_HOME", "/b")])).unwrap();
        assert_eq!(homes.socket, Path::new("/b/v2.sock"));
        assert_eq!(homes.state, Path::new("/b/v2"));
        let refused = Homes::of(None, var(&[("BLAZE_HOME", "")])).unwrap_err();
        assert!(refused.starts_with("no BLAZE_HOME or HOME"), "{refused}");
    }

    /// A start's bytes, cut at every byte: the phases in order, a record of what it is
    /// inside, then that it listens.
    #[test]
    fn a_start_is_heard_whole_however_its_bytes_are_cut() {
        let mut said = vec![b's', INSIDE, 4, 0];
        said.extend_from_slice(b"open");
        said.extend_from_slice(b"p\n");
        for cut in 0..said.len() {
            let mut hearing = Hearing::new();
            assert_eq!(hearing.feed(&said[..cut]), Ok(false), "cut at {cut}");
            assert_eq!(hearing.feed(&said[cut..]), Ok(true), "cut at {cut}");
            assert_eq!(hearing.phase(), Phase::Setup);
            assert_eq!(hearing.inside.as_deref(), Some("open"));
        }
        let mut hearing = Hearing::new();
        let refused = hearing.feed(b"sw").unwrap_err();
        assert!(refused.contains("no later phase of a start"), "{refused}");
    }
}
