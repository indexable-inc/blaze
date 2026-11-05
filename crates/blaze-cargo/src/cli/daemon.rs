//! The daemon this CLI is a client of: found at its socket ([`crate::serve::socket`]), started
//! as `blaze serve` when none answers there, spoken to through [`blaze_api::client::Client`].
//!
//! Without blaze-cargo's `native` feature (the wasm seed, docs/future/bootstrap.md) there is no
//! daemon to find or start: [`connect`] hands back a client of the same engine run in this
//! process for this command alone ([`crate::serve::once`]), and `blaze stop`, `status` and
//! `daemon` are refused, naming the feature ([`NO_DAEMON`]).
#![cfg_attr(
    not(feature = "native"),
    allow(
        dead_code,
        unused_imports,
        reason = "the daemon's start and its helpers are not in the seed"
    )
)]

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(all(unix, feature = "native"))]
use blaze_api::units::Installed;
use blaze_api::{
    Request, ResponseRef, Status,
    client::{Client, ClientError},
};

use super::args::DaemonVerb;
use crate::{raw::process::Reaped, serve::Phase};

/// Why a command that needs a running daemon is refused in a blaze built without one.
pub const NO_DAEMON: &str = "this blaze is built without blaze-cargo's `native` feature (the seed profile, \
     docs/future/bootstrap.md): it runs no daemon, and each command builds in its own process and exits";

/// How long a daemon that was told to stop is waited for after the last step of its stop it
/// said ([`blaze_api::Response::Stopping`]): one step is the making of its image, which grows
/// with the daemon (679 MiB took 11 s in the self-build survey), so as long as a step of a
/// start ([`STARTING`]).
pub const PATIENCE: Duration = Duration::from_secs(60);

/// How long a daemon this command started is given for each phase of its start
/// ([`crate::serve::Phase`]): reading a large image is one.
pub const STARTING: Duration = Duration::from_secs(60);

/// What the commands ask of the daemon, which the tests fake.
pub trait Api {
    /// Sends `request` and hands each of its responses to `on`, read where it lies in its frame
    /// ([`ResponseRef`]), until its Done (handed too); Done's `ok`. `on`'s second argument says
    /// whether another whole response is already held ([`Client::call_ref`]'s), so a caller that
    /// prints writes what it holds only when it is false: one write per burst the daemon sent.
    fn call<F: FnMut(ResponseRef<'_>, bool)>(
        &mut self,
        request: Request,
        on: F,
    ) -> Result<bool, ClientError>;

    /// What came of a daemon that hung up on a call ([`hung_up`]); `None` where there is no
    /// daemon's process and log to ask, as with the tests' fakes.
    fn died(&mut self) -> Option<anyhow::Result<Died>> {
        None
    }
}

/// One request per connection: anything held once its Done has come (a response to another
/// request, or more of this one) is the daemon's mistake, said as one.
impl Api for Client {
    fn call<F: FnMut(ResponseRef<'_>, bool)>(
        &mut self,
        request: Request,
        on: F,
    ) -> Result<bool, ClientError> {
        let ok = self.call_ref(request, on)?;
        if self.buffered() {
            let said = "the daemon sent more on a connection whose one request had its Done";
            // alloc: error path, the daemon's mistake.
            return Err(ClientError::Refused(blaze_api::ApiError::new(said, None)));
        }
        Ok(ok)
    }

    #[cfg(feature = "native")]
    fn died(&mut self) -> Option<anyhow::Result<Died>> {
        Some(died())
    }
}

/// Tells the daemon on `client` to stop: Stop's Done, then, on the same request, each step of
/// its stop ([`blaze_api::Response::Stopping`]) until it hangs up. One read may hold the Done
/// and the first steps, so this is [`Client::call_ref`], never [`Api::call`] (which refuses
/// what follows a Done on a connection of one request), and the caller's
/// [`Client::until_hung_up`] takes the steps held. `on` is given each response up to the Done;
/// the Done's `ok`.
pub(crate) fn told_to_stop(
    client: &mut Client,
    on: impl FnMut(ResponseRef<'_>, bool),
) -> Result<bool, ClientError> {
    client.call_ref(Request::Stop, on)
}

/// Whether a daemon is started when none answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    IfMissing,
    Never,
}

/// Why the CLI has no daemon to speak to.
#[derive(Debug)]
pub enum DaemonError {
    /// A connect to the socket failed for another reason than that nothing listens there:
    /// no daemon is started over it.
    Connect { socket: PathBuf, error: ClientError },
    /// The daemon was started and did not say it listens at its socket within [`STARTING`] of
    /// the last phase of its start it said; `inside`, what it last said its start's thread was
    /// inside ([`crate::serve::INSIDE`]), if it said any.
    NotListening { socket: PathBuf, log: PathBuf, pid: u32, phase: Phase, inside: Option<String> },
    /// The daemon was started and ended in `phase`, before it listened, as `ended` says.
    Exited { socket: PathBuf, log: PathBuf, pid: u32, phase: Phase, ended: Reaped },
    /// The daemon was started and closed its ready pipe in `phase`, before it listened, and
    /// had not ended a moment ([`ENDING`]) after; `inside` as [`DaemonError::NotListening`]'s.
    Closed { socket: PathBuf, log: PathBuf, pid: u32, phase: Phase, inside: Option<String> },
    /// The daemon reached keeps for its life settings this command sets to other values
    /// ([`crate::serve::settings::drift`]): refused before any request, each named with both
    /// values. `activated`: a socket unit starts it, with the settings `blaze daemon install`
    /// captured.
    Drifted {
        socket: PathBuf,
        pid: u32,
        drift: Vec<crate::serve::settings::Drift>,
        activated: bool,
    },
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Connect { socket, error } => {
                write!(f, "connecting to blaze's daemon at {}: {error}", socket.display())
            }
            DaemonError::NotListening { socket, log, pid, phase, inside } => {
                f.write_str(&not_listening(socket, log, *pid, *phase, inside.as_deref()))
            }
            DaemonError::Exited { socket, log, pid, phase, ended } => {
                let how = match ended {
                    Reaped::Code(code) => {
                        format!("with exit code {}", blaze_core::exit::said(*code))
                    } // alloc: an error's text
                    Reaped::Signal(signal) => format!("by signal {signal}"), // alloc: as above
                };
                write!(
                    f,
                    "the daemon (`blaze serve`, pid {pid}) was started and ended {how} while {}, before it \
                     listened at {}; its log {} says why",
                    phase.doing(),
                    socket.display(),
                    log.display()
                )
            }
            DaemonError::Closed { socket, log, pid, phase, inside } => write!(
                f,
                "the daemon (`blaze serve`, pid {pid}) was started and closed its ready pipe while {}, {}, before \
                 it listened at {}, and runs on; its log is {}",
                phase.doing(),
                Inside(inside.as_deref()),
                socket.display(),
                log.display()
            ),
            DaemonError::Drifted { socket, pid, drift, activated } => {
                let said = crate::serve::settings::said(drift);
                let fix = match activated {
                    true => {
                        "its socket unit starts it with the daemon settings of the shell that ran `blaze daemon \
                             install`: run `blaze daemon install` again from a shell with this command's values (it \
                             captures them and restarts the daemon), or run this command without them"
                    }
                    false => {
                        "`blaze stop`, then run this again, and the daemon it starts has this command's values; or \
                              run this command without them, to build with the daemon's"
                    }
                };
                write!(
                    f,
                    "the daemon at {} (pid {pid}) keeps for its life other settings than this command sets: {said}. \
                     Nothing was sent to it; {fix}",
                    socket.display()
                )
            }
        }
    }
}

impl std::error::Error for DaemonError {}

/// Whether a connect failed because nothing listens at the socket: no file there, or one a
/// daemon that is gone left.
pub fn nothing_listens(error: &ClientError) -> bool {
    matches!(error, ClientError::Io(it) if matches!(it.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused))
}

/// A connection to the daemon; `None` when none answers and none was to be started.
///
/// Where systemd or launchd is socket-activating the daemon, the connect itself starts it
/// (the manager listens and the request waits in its backlog); the CLI starts `blaze serve`
/// only where no unit serves the socket, and says so.
///
/// A command that may start one ([`Start::IfMissing`]) first asks the daemon that answers which
/// blaze it is and what it was started with ([`builds`]): one of another build is stopped,
/// saying so, and this one started, so no request of this blaze runs on another's; one keeping
/// other values of settings this command sets ([`crate::serve::settings`]) is refused before
/// any request, naming each setting, both values and the fix ([`DaemonError::Drifted`]), so no
/// build runs under values it did not ask for. It is never restarted over them.
#[cfg(all(unix, feature = "native"))]
pub fn connect(start: Start) -> anyhow::Result<Option<Client>> {
    let socket = crate::serve::socket()?;
    let refused = match Client::unix(&socket) {
        Ok(client) if start == Start::Never => return Ok(Some(client)),
        Ok(client) => match this_build(client, &socket)? {
            Some(client) => return Ok(Some(client)),
            None => None,
        },
        Err(error) if !nothing_listens(&error) => {
            return Err(DaemonError::Connect { socket, error }.into());
        }
        Err(_) if start == Start::Never => return Ok(None),
        Err(error) => Some(error),
    };
    if let Some(refused) = refused {
        say_start(&socket, &refused)?;
    }
    use anyhow::Context as _;
    let (log, ready, mut child) = started()?;
    let pid = child.id();
    let heard =
        heard(&ready, STARTING).context("waiting for the daemon it started to say it listens")?;
    match heard {
        Heard::Listening => match Client::unix(&socket) {
            Ok(client) => {
                // Why this daemon starts cold, said where the person who waits sees it: the
                // daemon writes it to its log alone.
                match tail(&log) {
                    Ok(bytes) => {
                        for said in start_said(&String::from_utf8_lossy(&bytes), pid) {
                            eprintln!("blaze: the daemon it started (pid {pid}) {said}");
                        }
                    }
                    Err(error) => eprintln!(
                        "blaze: how the daemon it started (pid {pid}) started is unread: {}: {error}",
                        log.display()
                    ),
                }
                Ok(Some(client))
            }
            Err(error) => Err(DaemonError::Connect { socket, error }.into()),
        },
        Heard::Ended { phase, inside } => {
            crate::raw::process::ended_within(pid, ENDING)
                .context("waiting for the daemon that closed its pipe to end")?;
            let status = child.try_wait().context("asking how the daemon it started ended")?;
            Err(match status.and_then(reaped) {
                Some(ended) => DaemonError::Exited { socket, log, pid, phase, ended },
                None => DaemonError::Closed { socket, log, pid, phase, inside },
            }
            .into())
        }
        Heard::Nothing { phase, inside } => {
            Err(DaemonError::NotListening { socket, log, pid, phase, inside }.into())
        }
    }
}

/// How a process ended, as its status says.
#[cfg(all(unix, feature = "native"))]
fn reaped(status: std::process::ExitStatus) -> Option<Reaped> {
    use std::os::unix::process::ExitStatusExt as _;
    status.code().map(Reaped::Code).or_else(|| status.signal().map(Reaped::Signal))
}

/// How long a daemon of another build that was told to stop is waited for: its image is
/// written first.
pub const REPLACING: Duration = Duration::from_secs(120);

/// Whether the daemon a client reached is this blaze, started as this command asks ([`builds`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Builds {
    Same,
    /// Another build: the daemon's pid and build, and this blaze's.
    Other {
        pid: u32,
        theirs: blaze_api::Build,
        ours: blaze_api::Build,
    },
    /// This build, keeping other values of settings the command's environment sets: the
    /// daemon's pid and each setting that differs.
    Settings {
        pid: u32,
        drift: Vec<crate::serve::settings::Drift>,
    },
}

/// Asks the daemon on `api` for its [`Status`], and for nothing else, and says whether it is
/// this blaze: its executable is `ours`, unchanged, or else this blaze's build (`hash`, called
/// only then, as it reads this blaze's bytes) has its build's hash; and, when it is, whether it
/// keeps the values of the `settings` this command sets ([`crate::serve::settings::drift`]). A
/// daemon of another schema can't be told to stop by this one: that is the error, naming both.
pub fn builds<A: Api>(
    api: &mut A,
    socket: &Path,
    ours: &blaze_api::Executable,
    settings: &[blaze_api::Setting],
    hash: impl FnOnce() -> anyhow::Result<blaze_api::Build>,
) -> anyhow::Result<Builds> {
    let mut status = None;
    let mut said = Vec::new();
    let ok = match api.call(Request::Status, |response, _| match response {
        ResponseRef::Status(it) => status = Some(it),
        // alloc: error path, the daemon's errors.
        ResponseRef::Error(error) => said.push(error.what.to_owned()),
        _ => {}
    }) {
        Ok(ok) => ok,
        Err(ClientError::Schema { theirs }) => anyhow::bail!(
            "the daemon at {} speaks API schema {theirs}, this blaze {}: it is another build of blaze, which \
             this one can't tell to stop; `blaze stop` of that build, or ending its process (its log names its \
             pid), stops it",
            socket.display(),
            blaze_api::SCHEMA
        ),
        Err(error) => return Err(error.into()),
    };
    let Some(status) = status.filter(|_| ok) else {
        anyhow::bail!(
            "the daemon at {} answered its Status with no status: {}",
            socket.display(),
            said.join("; ")
        );
    };
    if status.build.exe != *ours {
        let ours = hash()?;
        if ours.id != status.build.id {
            return Ok(Builds::Other { pid: status.pid, theirs: status.build, ours });
        }
    }
    let drift = crate::serve::settings::drift(settings, &status.settings);
    if drift.is_empty() {
        return Ok(Builds::Same);
    }
    Ok(Builds::Settings { pid: status.pid, drift })
}

/// `build`'s path and the start of its hash, as a line names it.
#[cfg(all(unix, feature = "native"))]
fn named(build: &blaze_api::Build) -> String {
    let id = format!("{:x}", build.id); // alloc: a line, once a replaced daemon
    format!("{} {}", build.exe.path, &id[..id.len().min(12)])
}

/// The connection `client` was, made again for the command's own request, when its daemon is
/// this blaze keeping the settings this command sets ([`DaemonError::Drifted`] when not);
/// `None` once one of another build was stopped, for this one to start.
#[cfg(all(unix, feature = "native"))]
fn this_build(mut client: Client, socket: &Path) -> anyhow::Result<Option<Client>> {
    use anyhow::Context as _;
    let exe = std::env::current_exe().context("finding blaze's own executable")?;
    let file = std::fs::File::open(&exe)
        .with_context(|| format!("reading blaze itself, {}", exe.display()))?;
    let ours = crate::serve::executable(&exe, &file)?;
    let settings = crate::serve::settings::of_client(|name| std::env::var_os(name));
    let (pid, theirs, ours) = match builds(&mut client, socket, &ours, &settings, || {
        crate::serve::build_of(&exe)
    })? {
        Builds::Same => {
            return match Client::unix(socket) {
                Ok(client) => Ok(Some(client)),
                Err(error) => Err(DaemonError::Connect { socket: socket.to_owned(), error }.into()),
            };
        }
        // Never a restart: one daemon serves every lane of a home, and a restart on each
        // difference would cut off a build that arrived since the Status, and two lanes of
        // other values would take turns restarting it (earlyfail, round 2).
        Builds::Settings { pid, drift } => {
            let activated = activation(socket)? == Installed::Yes;
            return Err(
                DaemonError::Drifted { socket: socket.to_owned(), pid, drift, activated }.into()
            );
        }
        Builds::Other { pid, theirs, ours } => (pid, theirs, ours),
    };
    if activation(socket)? == Installed::Yes {
        anyhow::bail!(
            "the daemon socket-activated at {} (pid {pid}) is another build of blaze ({}) than this one ({}): \
             `blaze daemon install` with this blaze serves this one",
            socket.display(),
            named(&theirs),
            named(&ours)
        );
    }
    eprintln!(
        "blaze: the daemon at {} (pid {pid}) is another build of blaze ({}), not this one ({}): stopping it and \
         starting this one",
        socket.display(),
        named(&theirs),
        named(&ours)
    );
    let mut client = Client::unix(socket)
        .map_err(|error| DaemonError::Connect { socket: socket.to_owned(), error })?;
    let ok = told_to_stop(&mut client, |_, _| {}).with_context(|| {
        format!("telling the daemon of another build at {} (pid {pid}) to stop", socket.display())
    })?;
    if !ok {
        anyhow::bail!(
            "the daemon of another build at {} (pid {pid}) refused to stop",
            socket.display()
        );
    }
    client.until_hung_up(REPLACING).with_context(|| {
        format!(
            "the daemon of another build at {} (pid {pid}) was told to stop and did not end within {}s",
            socket.display(),
            REPLACING.as_secs()
        )
    })?;
    if answers(socket)? {
        anyhow::bail!(
            "the daemon of another build at {} (pid {pid}) was told to stop and still answers",
            socket.display()
        );
    }
    // Its end too, not only its hang-up: a daemon as it ends lets go of its socket's name, and
    // one of an older build removes the file there, whoever's it is by then.
    let ended = crate::raw::process::ended_within(pid, REPLACING).with_context(|| {
        format!("waiting for the daemon of another build (pid {pid}) that hung up to end")
    })?;
    if !ended {
        anyhow::bail!(
            "the daemon of another build (pid {pid}) hung up at {} and did not end within {}s: this one starts \
             only once it has",
            socket.display(),
            REPLACING.as_secs()
        );
    }
    Ok(None)
}

/// The line a command that starts the daemon says first, or why it starts none: a socket
/// unit serves `socket` and nothing listens there (`refused`).
#[cfg(all(unix, feature = "native"))]
fn say_start(socket: &Path, refused: &ClientError) -> anyhow::Result<()> {
    match activation(socket)? {
        Installed::Yes => {
            let place = blaze_api::units::place_here()?;
            anyhow::bail!(
                "blaze's daemon is socket-activated for {} ({}) and nothing listens there ({refused}): \
                 `blaze daemon install` loads its units again",
                socket.display(),
                blaze_api::units::main_file(&place).display()
            );
        }
        Installed::Elsewhere(theirs) => eprintln!(
            "blaze: starting the daemon for {}: the installed units serve {} (`blaze daemon install` with this \
             environment serves this one)",
            socket.display(),
            theirs.display()
        ),
        Installed::No => eprintln!(
            "blaze: starting the daemon (no socket activation is installed: `blaze daemon install` has the system \
             start it on demand)"
        ),
    }
    Ok(())
}

/// What the daemon this command started said on its pipe ([`started`]).
#[cfg(all(unix, feature = "native"))]
#[derive(Clone, PartialEq, Eq, Debug)]
enum Heard {
    /// It listens: it said [`Phase::Listening`].
    Listening,
    /// The pipe closed first, `phase` the last it said, `inside` the last it said its start's
    /// thread was inside ([`crate::serve::INSIDE`]).
    Ended { phase: Phase, inside: Option<String> },
    /// Neither, within the wait of `phase`, the last it said; `inside` as `Ended`'s.
    Nothing { phase: Phase, inside: Option<String> },
}

/// How long a start may stay in `phase`, `long` for each but [`Phase::Waiting`]: the daemon
/// itself waits at most [`crate::serve::BEFORE_ENDS`] there, for the one before it.
fn within(phase: Phase, long: Duration) -> Duration {
    match phase {
        Phase::Waiting => crate::serve::BEFORE_ENDS + long,
        Phase::Launched | Phase::State | Phase::Setup | Phase::Listening => long,
    }
}

/// What the daemon said on its ready pipe so far, read as bytes come ([`Hearing::feed`]): its
/// phases' bytes ([`Phase::byte`]) and its records of what its start's thread is inside
/// ([`crate::serve::INSIDE`]), which may arrive cut across reads.
#[cfg(all(unix, feature = "native"))]
#[derive(Debug)]
struct Hearing {
    phase: Phase,
    /// The text of the last record of what the start is inside.
    inside: Option<String>,
    /// Bytes of a record not yet whole.
    held: Vec<u8>,
}

#[cfg(all(unix, feature = "native"))]
impl Hearing {
    fn new() -> Hearing {
        // alloc: one start's pipe, a record of at most 515 bytes held
        Hearing { phase: Phase::Launched, inside: None, held: Vec::new() }
    }

    /// Takes `bytes`, the next the pipe gave; true once [`Phase::Listening`] is said, and what
    /// follows it is not read. A phase said again or out of order, a byte that is neither a
    /// phase nor [`crate::serve::INSIDE`], or a record longer than
    /// [`crate::serve::INSIDE_MAX`] or not UTF-8 is refused, named.
    fn feed(&mut self, bytes: &[u8]) -> io::Result<bool> {
        use crate::serve::{INSIDE, INSIDE_MAX};
        self.held.extend_from_slice(bytes);
        let mut at = 0;
        // Bounded: each turn takes at least one byte of `held`, or ends.
        let listening = loop {
            let Some(&tag) = self.held.get(at) else { break false };
            if tag == INSIDE {
                let Some(&[low, high]) = self.held.get(at + 1..at + 3) else { break false };
                let len = usize::from(u16::from_le_bytes([low, high]));
                if len > INSIDE_MAX {
                    // alloc: an error's text
                    return Err(refused(format!(
                        "the daemon said on its ready pipe, while {}, what its start is inside in {len} bytes: \
                         at most {INSIDE_MAX} are said",
                        self.phase.doing()
                    )));
                }
                let Some(text) = self.held.get(at + 3..at + 3 + len) else { break false };
                let text = std::str::from_utf8(text).map_err(|error| {
                    // alloc: an error's text
                    refused(format!(
                        "the daemon said on its ready pipe, while {}, what its start is inside in text that is \
                         not UTF-8: {error}",
                        self.phase.doing()
                    ))
                })?;
                // alloc: the last record of a start, kept for its error
                self.inside = Some(text.to_owned());
                at += 3 + len;
                continue;
            }
            match Phase::of(tag) {
                Some(Phase::Listening) => break true,
                Some(next) if next > self.phase => {
                    self.phase = next;
                    at += 1;
                }
                said => {
                    // alloc: an error's text
                    return Err(refused(format!(
                        "the daemon said {:?} on its ready pipe while {}: no later phase of a start",
                        char::from(tag),
                        said.unwrap_or(self.phase).doing()
                    )));
                }
            }
        };
        self.held.drain(..at);
        Ok(listening)
    }
}

/// A ready pipe's word refused ([`Hearing::feed`]).
#[cfg(all(unix, feature = "native"))]
fn refused(said: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, said)
}

/// Waits for the daemon to say on `ready` that it listens, or to end: at most [`within`] its
/// phase for each, from when it said the phase. The deadline is taken once, as the phase
/// begins: the records of what the start is inside, which come every 500 ms, never move it.
#[cfg(all(unix, feature = "native"))]
fn heard(ready: &std::io::PipeReader, long: Duration) -> io::Result<Heard> {
    use std::{io::Read as _, os::fd::AsFd as _, time::Instant};
    let mut hearing = Hearing::new();
    let mut deadline = Instant::now() + within(hearing.phase, long);
    let mut bytes = [0u8; 1024];
    // Bounded: each turn reads at least one byte or ends; the phases only move forward, five
    // of them, and within one every turn is before its deadline.
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !crate::raw::process::readable_within(ready.as_fd(), left)? {
            return Ok(Heard::Nothing { phase: hearing.phase, inside: hearing.inside });
        }
        let read = (&mut &*ready).read(&mut bytes)?;
        if read == 0 {
            return Ok(Heard::Ended { phase: hearing.phase, inside: hearing.inside });
        }
        let was = hearing.phase;
        if hearing.feed(&bytes[..read])? {
            return Ok(Heard::Listening);
        }
        if hearing.phase != was {
            deadline = Instant::now() + within(hearing.phase, long);
        }
    }
}

/// Whether a launchd or systemd unit serves `socket`. Units live under `HOME`: without one,
/// none is installed.
#[cfg(all(unix, feature = "native"))]
fn activation(socket: &Path) -> anyhow::Result<Installed> {
    if blaze_api::activation::Manager::here().is_none()
        || std::env::var_os("HOME").is_none_or(|it| it.is_empty())
    {
        return Ok(Installed::No);
    }
    Ok(blaze_api::units::installed(&blaze_api::units::place_here()?, socket)?)
}

#[cfg(all(not(unix), feature = "native"))]
pub fn connect(_: Start) -> anyhow::Result<Option<Client>> {
    anyhow::bail!(
        "blaze's CLI speaks to its daemon over the Unix socket {}, and blaze_api's client has \
         Unix sockets on Unix only",
        crate::serve::socket()?.display()
    )
}

/// The seed's connection: the engine `blaze serve` runs, started in this process for this
/// command alone and stopped as the client closes ([`crate::serve::once`]), whatever `start`
/// says, as there is no daemon running to find.
#[cfg(not(feature = "native"))]
pub fn connect(_: Start) -> anyhow::Result<Option<Client>> {
    crate::serve::once::connect().map(Some)
}

/// What a start that never listened says: its phase, and `inside`, what it last said its
/// start's thread was inside ([`crate::serve::INSIDE`]), if it said any.
pub fn not_listening(
    socket: &Path,
    log: &Path,
    pid: u32,
    phase: Phase,
    inside: Option<&str>,
) -> String {
    // alloc: an error's text
    format!(
        "the daemon (`blaze serve`, pid {pid}) was started and did not listen at {}: it was {} for \
         {}s, {}; its log is {}",
        socket.display(),
        phase.doing(),
        within(phase, STARTING).as_secs(),
        Inside(inside),
        log.display()
    )
}

/// What a start last said its thread was inside, as an error says it: "inside `open store`
/// (1.2s open)", "inside no span", or that it said nothing of it.
struct Inside<'a>(Option<&'a str>);

impl std::fmt::Display for Inside<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => f.write_str("and said nothing about what it was inside"),
            Some("") => f.write_str("inside no span its flight recorder names"),
            Some(span) => write!(f, "inside {span}"),
        }
    }
}

/// Whether the daemon ended a connection that was waiting on it: its end of the socket closed
/// (a read found none) or was reset (a write found none).
pub fn hung_up(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Io(it)
            if matches!(it.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe)
    )
}

/// How many of its log's lines since it listened a daemon that hung up is quoted by, the last.
const TAIL: usize = 20;
/// How much of the end of its log is read for them.
const TAIL_BYTES: u64 = 256 << 10;
/// How long a daemon that hung up is given to end before it is said to run.
const ENDING: Duration = Duration::from_secs(1);

/// How the daemon that hung up on a request ended, as its process says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    /// This command started it, and waited for it once it ended.
    Reaped(crate::raw::process::Reaped),
    /// It is gone; how only its parent heard, and what its log says.
    Gone,
    /// It still runs: it hung up on this request alone.
    Running,
}

/// A daemon that hung up on a request: how it ended and what its log says since it listened.
#[derive(Debug, PartialEq, Eq)]
pub struct Died {
    pub log: PathBuf,
    /// Its pid and how it ended; `None` when the end of its log has no start of it.
    pub ended: Option<(u32, Ended)>,
    /// The log's lines since that start that are not blank, the last [`TAIL`].
    pub said: Vec<String>,
    /// What its threads were inside at its profile history's last reading of them
    /// ([`blaze_profile::reading::last`]), for a daemon that is gone: `Ok(None)` where the
    /// history holds none, the error where it could not be read.
    pub inside: Result<Option<blaze_profile::reading::Reading>, String>,
    /// The fatal signal it recorded as it died, with the faulting thread's stack
    /// ([`blaze_profile::crash`]), for a daemon that is gone: `Ok(None)` where it left no record
    /// of one, the error where the record was refused or could not be read.
    pub crash: Result<Option<blaze_profile::panics::PanicRow>, String>,
}

/// How many frames of a dead daemon's crash record the CLI names.
const CRASH_FRAMES: usize = 12;

/// How many threads of a dead daemon's last reading the CLI names.
const INSIDE_THREADS: usize = 16;

impl std::fmt::Display for Died {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::raw::process::Reaped;
        match self.ended {
            Some((pid, Ended::Reaped(Reaped::Signal(signal)))) => write!(
                f,
                "the daemon (`blaze serve`, pid {pid}) was ended by signal {signal}{}",
                blaze_core::signal::described(signal)
            )?,
            Some((pid, Ended::Reaped(Reaped::Code(code)))) => {
                write!(
                    f,
                    "the daemon (`blaze serve`, pid {pid}) exited with code {}",
                    blaze_core::exit::said(code)
                )?;
            }
            Some((pid, Ended::Gone)) => write!(f, "the daemon (`blaze serve`, pid {pid}) is gone")?,
            Some((pid, Ended::Running)) => {
                write!(
                    f,
                    "the daemon (`blaze serve`, pid {pid}) still runs: it hung up on this request alone"
                )?;
            }
            None => write!(
                f,
                "the last {} KiB of the daemon's log name no start of it",
                TAIL_BYTES >> 10
            )?,
        }
        if self.said.is_empty() {
            write!(f, "; its log {} says nothing since it listened", self.log.display())?;
            if matches!(self.ended, Some((_, Ended::Gone))) {
                // A signal it catches writes its line (`serve::signals::record_ends`).
                f.write_str(
                    " (a SIGKILL, as an out-of-memory kill or `kill -9` sends, or a SIGSEGV that is no stack \
                     overflow ends it so)",
                )?;
            }
        } else {
            write!(f, "; its log {} ends:", self.log.display())?;
            for line in &self.said {
                write!(f, "\n  {line}")?;
            }
        }
        if matches!(self.ended, Some((_, Ended::Running)) | None) {
            return Ok(());
        }
        match &self.inside {
            Ok(Some(reading)) => {
                write!(
                    f,
                    "\nwhat its threads were inside at its profile's last reading of them (`blaze slow` reports its build):"
                )?;
                for thread in reading.threads.iter().take(INSIDE_THREADS) {
                    write!(f, "\n  {}", blaze_profile::slow::thread_line(thread))?;
                }
                let more = reading.threads.len().saturating_sub(INSIDE_THREADS);
                if more > 0 {
                    write!(f, "\n  ({more} more threads)")?;
                }
            }
            Ok(None) => f.write_str(
                "\nits profile history holds no reading of what its threads were inside",
            )?,
            Err(why) => write!(f, "\nwhat its threads were inside is unread: {why}")?,
        }
        match &self.crash {
            Ok(Some(row)) => {
                write!(
                    f,
                    "\nits crash record: {} on thread {} (tid {})",
                    row.message.as_deref().unwrap_or("a fatal signal"),
                    row.thread_name,
                    row.thread
                )?;
                if let Some(at) = &row.location {
                    write!(f, ", its last panic at {at}")?;
                }
                for frame in blaze_profile::panics::top_frames(&row.stack, CRASH_FRAMES) {
                    write!(f, "\n  at {frame}")?;
                }
                if !row.open_spans.is_empty() {
                    write!(f, "\n  inside {}", row.open_spans.join(" > "))?;
                }
            }
            Ok(None) => {}
            Err(why) => write!(f, "\nits crash record is unread: {why}")?,
        }
        Ok(())
    }
}

/// The pid of the daemon's last start in `log` (its [`crate::serve::LISTENING`] line), and
/// its lines after that are not blank, the last [`TAIL`]; `None` when `log` has no start.
#[cfg(feature = "native")]
pub fn last_start(log: &str) -> Option<(u32, Vec<&str>)> {
    // alloc: an error path, once a command
    let lines: Vec<&str> = log.lines().collect();
    let (at, pid) =
        lines.iter().enumerate().rev().find_map(|(at, line)| Some((at, listening_pid(line)?)))?;
    // alloc: as above
    let mut said: Vec<&str> =
        lines[at + 1..].iter().copied().filter(|it| !it.trim().is_empty()).collect();
    said.drain(..said.len().saturating_sub(TAIL));
    Some((pid, said))
}

/// `7` of `blaze serve: listening on /run/blaze/v2.sock (pid 7)`.
#[cfg(feature = "native")]
fn listening_pid(line: &str) -> Option<u32> {
    let (_, pid) = line.strip_prefix(crate::serve::LISTENING)?.rsplit_once(" (pid ")?;
    pid.strip_suffix(')')?.parse().ok()
}

/// What came of the daemon that hung up on a request: its log's last start and what it wrote
/// since, and how its process ended, given [`ENDING`] to.
#[cfg(all(unix, feature = "native"))]
pub fn died() -> anyhow::Result<Died> {
    use anyhow::Context as _;
    let log = crate::serve::state()?.join("serve.log");
    // alloc: an error path, once a command
    let bytes = tail(&log)
        .with_context(|| format!("reading the end of the daemon's log {}", log.display()))?;
    let text = String::from_utf8_lossy(&bytes); // alloc: as above, where the log is not UTF-8
    let Some((pid, said)) = last_start(&text) else {
        return Ok(Died { log, ended: None, said: Vec::new(), inside: Ok(None), crash: Ok(None) }); // alloc: none, empty
    };
    let said = said.into_iter().map(str::to_owned).collect(); // alloc: as above
    let ended = crate::raw::process::ended_within(pid, ENDING)
        .with_context(|| format!("waiting for the daemon (pid {pid}) that hung up to end"))?;
    let ended = match (ended, crate::raw::process::reap(pid)) {
        (_, Some(reaped)) => Ended::Reaped(reaped),
        (true, None) => Ended::Gone,
        (false, None) => Ended::Running,
    };
    let (inside, crash) = match ended {
        Ended::Running => (Ok(None), Ok(None)),
        _ => {
            let history = blaze_profile::history::dir_in(&crate::serve::state()?);
            (
                blaze_profile::reading::last(&history, pid).map_err(|why| why.to_string()),
                blaze_profile::crash::of_process(&history, pid).map_err(|why| why.to_string()),
            )
        }
    };
    Ok(Died { log, ended: Some((pid, ended)), said, inside, crash })
}

/// The last [`TAIL_BYTES`] of the daemon's log.
#[cfg(all(unix, feature = "native"))]
fn tail(log: &Path) -> io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(log)?;
    let len = file.metadata()?.len();
    file.seek(io::SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
    let mut bytes = Vec::new(); // alloc: once a command
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// What the daemon `pid` said in `log` of how it started, before it listened: its lines after
/// `blaze serve: ` that begin [`crate::serve::STARTED`] (why its warm state is gone, how the
/// daemon before it ended).
#[cfg(feature = "native")]
pub fn start_said(log: &str, pid: u32) -> Vec<&str> {
    // alloc: once a start
    let lines: Vec<&str> = log.lines().collect();
    let Some(at) = lines.iter().rposition(|line| listening_pid(line) == Some(pid)) else {
        return Vec::new();
    }; // alloc: none, empty
    let from =
        lines[..at].iter().rposition(|line| listening_pid(line).is_some()).map_or(0, |it| it + 1);
    lines[from..at]
        .iter()
        .filter_map(|line| {
            line.strip_prefix("blaze serve: ").filter(|it| it.starts_with(crate::serve::STARTED))
        })
        .collect()
}

#[cfg(all(not(unix), feature = "native"))]
pub fn died() -> anyhow::Result<Died> {
    anyhow::bail!("blaze's CLI speaks to its daemon over a Unix socket, which this system lacks")
}

/// Starts `blaze serve` in a process group of its own, so that what ends this command or its
/// terminal's foreground does not end it, with its output appended to its log; the log, and
/// the pipe it writes one byte to once it listens, which closes with nothing written if it
/// ends first.
#[cfg(all(unix, feature = "native"))]
fn started() -> anyhow::Result<(std::path::PathBuf, std::io::PipeReader, std::process::Child)> {
    use anyhow::Context as _;
    use std::os::{fd::AsRawFd as _, unix::process::CommandExt as _};
    let state = crate::serve::state()?;
    crate::socket::private_dir(&state)
        .with_context(|| format!("the daemon's directory {}", state.display()))?;
    let log = state.join("serve.log");
    let mut options = std::fs::OpenOptions::new();
    let file = crate::socket::private_file(options.create(true).append(true), &log)
        .with_context(|| format!("the daemon's log {}", log.display()))?;
    let mut daemon = std::process::Command::new(
        std::env::current_exe().context("finding blaze's own executable")?,
    );
    let (ready, said) = std::io::pipe().context("making the pipe the daemon says it listens on")?;
    let fd = said.as_raw_fd();
    daemon.arg("serve").arg("--ready-fd").arg(fd.to_string());
    daemon.stdin(std::process::Stdio::null()).stdout(file.try_clone()?).stderr(file);
    daemon.process_group(0);
    crate::raw::process::inherit_in_child(&mut daemon, fd);
    let child = daemon.spawn().context("starting the daemon (`blaze serve`)")?;
    // The daemon's copy alone is left, so its end closes the pipe.
    drop(said);
    Ok((log, ready, child))
}

/// `blaze stop`: the daemon made durable and ended, waited for until its socket is gone.
// dyn: the CLI's output writer, written a few times per command.
#[cfg(feature = "native")]
pub fn stop(err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    let Some(mut client) = connect(Start::Never)? else {
        writeln!(err, "no daemon is running")?;
        return Ok(0);
    };
    let mut said = Vec::new();
    let ok = told_to_stop(&mut client, |response, _| {
        if let ResponseRef::Error(error) = response {
            // alloc: error path, the daemon's errors, said once it is done.
            said.push(error.what.to_owned());
        }
    })?;
    for what in said {
        err.write_all(super::say::error(&what, color).as_bytes())?;
    }
    if !ok {
        return Ok(super::Exit::Failed.code());
    }
    let socket = crate::serve::socket()?;
    // The daemon stops listening, says each step of its stop, then hangs up its connection
    // last, once its image is written. With a unit installed the socket answers on (the
    // manager listens, and a connect would start the daemon again).
    if let Err(error) = client.until_hung_up(PATIENCE) {
        let what = format!("the daemon at {} was told to stop: {error}", socket.display());
        err.write_all(super::say::error(&what, color).as_bytes())?;
        return Ok(super::Exit::Failed.code());
    }
    #[cfg(unix)]
    if activation(&socket)? != Installed::Yes && answers(&socket)? {
        let what = format!(
            "the daemon at {} was told to stop, hung up, and still answers",
            socket.display()
        );
        err.write_all(super::say::error(&what, color).as_bytes())?;
        return Ok(super::Exit::Failed.code());
    }
    Ok(0)
}

/// Whether the daemon still answers at `socket`; a connect that fails for another reason than
/// that nothing listens is the error.
#[cfg(all(unix, feature = "native"))]
fn answers(socket: &Path) -> Result<bool, DaemonError> {
    match Client::unix(socket) {
        Ok(_) => Ok(true),
        Err(error) if nothing_listens(&error) => Ok(false),
        Err(error) => Err(DaemonError::Connect { socket: socket.to_owned(), error }),
    }
}

/// `blaze status`: what the daemon is doing, on stdout; with `json`, one JSON object
/// ([`status_json`]), with `"daemon": null` when none is running.
// dyn: the CLI's output writer, written a few times per command.
#[cfg(feature = "native")]
pub fn status(
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    color: bool,
    json: bool,
) -> anyhow::Result<i32> {
    // What this blaze knows without a daemon, read before connecting: a home blaze can't name
    // is the error, as it is for every command.
    let place = match json {
        true => Some(StatusPlace {
            home: crate::serve::root()?,
            state: crate::serve::state()?,
            socket: crate::serve::socket()?,
        }),
        false => None,
    };
    let Some(mut client) = connect(Start::Never)? else {
        match &place {
            Some(place) => out.write_all(status_json(place, None)?.as_bytes())?,
            None => writeln!(out, "no daemon is running")?,
        }
        return Ok(0);
    };
    let mut status = None;
    let mut said = Vec::new();
    let ok = Api::call(&mut client, Request::Status, |response, _| match response {
        ResponseRef::Status(it) => status = Some(it),
        // alloc: error path, the daemon's errors, said once it is done.
        ResponseRef::Error(error) => said.push(error.what.to_owned()),
        _ => {}
    })?;
    for what in said {
        err.write_all(super::say::error(&what, color).as_bytes())?;
    }
    match (status, &place) {
        (Some(status), Some(place)) if ok => {
            out.write_all(status_json(place, Some(&status))?.as_bytes())?;
        }
        (Some(status), None) => out.write_all(status_lines(&status).as_bytes())?,
        // A failed answer is said on stderr above; stdout stays empty under `--json`, so a
        // reader never takes a partial object for the daemon's.
        _ => {}
    }
    Ok(if ok { 0 } else { super::Exit::Failed.code() })
}

/// Where this blaze keeps its state, for `blaze status --json`: blaze's root (`BLAZE_HOME`, or
/// `blaze` in the user's cache directory), the daemon's state directory under it, and the
/// socket the CLI connects to.
pub struct StatusPlace {
    pub home: PathBuf,
    pub state: PathBuf,
    pub socket: PathBuf,
}

/// `blaze status --json`'s object, as docs/cargo.md ("The daemon") names its fields. Its
/// fields are always all present, `null` where there is no daemon.
#[derive(serde::Serialize)]
struct StatusJson<'a> {
    /// The JSON's own shape: a new number when a field changes meaning or goes.
    format: u32,
    /// This blaze's version line (`blaze --version`).
    version: String,
    /// The daemon API schema this blaze speaks ([`blaze_api::SCHEMA`]).
    api_schema: u32,
    home: &'a Path,
    state: &'a Path,
    socket: &'a Path,
    daemon: Option<DaemonJson<'a>>,
}

#[derive(serde::Serialize)]
struct DaemonJson<'a> {
    pid: u32,
    /// The hash of the daemon's executable as it started ([`blaze_api::Build::id`]), in hex.
    build: String,
    exe: &'a str,
    uptime_ms: u64,
    memory_bytes: u64,
    budget_bytes: u64,
    /// The bugs it went on past, summed over every site ([`Status::bugs`]).
    bugs: u64,
    requests: &'a [u64],
    views: u32,
    history: Option<&'a str>,
    settings: std::collections::BTreeMap<&'a str, Option<&'a str>>,
    /// The builds clients wait on, as the daemon last looked at them ([`Status::builds`]).
    builds: &'a [blaze_api::Building],
    /// Every download in flight ([`Status::downloads`]).
    downloads: &'a [blaze_api::Downloading],
}

/// The [`StatusJson`] of `place` and the daemon's `status` (`None`: no daemon runs), one
/// line, newline ended.
pub fn status_json(place: &StatusPlace, status: Option<&Status>) -> anyhow::Result<String> {
    let daemon = status.map(|status| DaemonJson {
        pid: status.pid,
        build: format!("{:x}", status.build.id), // alloc: the CLI's answer, once
        exe: &status.build.exe.path,
        uptime_ms: status.uptime_ms,
        memory_bytes: status.memory_bytes,
        budget_bytes: status.budget_bytes,
        bugs: status.bugs.iter().map(|it| it.count).sum(),
        requests: &status.requests,
        views: status.views,
        history: status.history.as_deref(),
        settings: status.settings.iter().map(|it| (&*it.name, it.value.as_deref())).collect(), // alloc: as above
        builds: &status.builds,
        downloads: &status.downloads,
    });
    let json = StatusJson {
        format: 1,
        version: crate::version(),
        api_schema: blaze_api::SCHEMA,
        home: &place.home,
        state: &place.state,
        socket: &place.socket,
        daemon,
    };
    use anyhow::Context as _;
    let mut line = serde_json::to_string(&json).context("`blaze status --json`'s answer as JSON")?;
    line.push('\n');
    Ok(line)
}

/// `blaze daemon install`: a running daemon stopped, then the launchd agent or systemd units
/// written and loaded ([`blaze_api::units::install`]), so the system listens on the socket and
/// starts the daemon at the first connection. `blaze daemon uninstall`: the daemon stopped,
/// the units unloaded and removed.
#[cfg(all(unix, feature = "native"))]
// dyn: the CLI's output writer, written a few times per command.
pub fn units(verb: DaemonVerb, err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    use anyhow::Context as _;
    use blaze_api::units;
    let socket = crate::serve::socket()?;
    let place = units::place_here()?;
    let settings = crate::serve::settings::STARTED_WITH;
    let mut kept = String::new();
    let steps = match verb {
        DaemonVerb::Install => {
            let program = std::env::current_exe().context("finding blaze's own executable")?;
            let log = crate::serve::state()?.join("serve.log");
            let (unit, place) = units::here(program, socket.clone(), log, settings)?;
            for (name, value) in
                unit.env.iter().filter(|(name, _)| settings.contains(&name.as_str()))
            {
                kept.push_str(&format!(" {name}={value}")); // alloc: the install's line, once
            }
            units::install(&unit, &place)?
        }
        DaemonVerb::Uninstall => match units::installed(&place, &socket)? {
            Installed::No => {
                writeln!(err, "no units of blaze's are installed in {}", place.dir.display())?;
                return Ok(0);
            }
            Installed::Yes => units::uninstall(&place, &socket),
            Installed::Elsewhere(theirs) => units::uninstall(&place, &theirs),
        },
    };
    // A daemon the CLI started holds the socket's path: stopped first, so the manager's socket
    // is the only one there, and its exit does not remove the manager's file.
    let stopped = stop(err, color)?;
    if stopped != 0 {
        return Ok(stopped);
    }
    units::run(&steps)?;
    match verb {
        DaemonVerb::Install => writeln!(
            err,
            "installed {}: the system starts the daemon at the first connection to {}, with {}; a command \
             that sets another value of one is refused, and `blaze daemon install` from a shell with the new \
             values changes them",
            units::main_file(&place).display(),
            socket.display(),
            match kept.is_empty() {
                true => "none of the daemon settings set (this shell sets none of them)".to_owned(),
                false => format!("the daemon settings of this shell:{kept}"), // alloc: the install's line, once
            }
        )?,
        DaemonVerb::Uninstall => writeln!(err, "uninstalled the units in {}", place.dir.display())?,
    }
    Ok(0)
}

#[cfg(all(not(unix), feature = "native"))]
// dyn: the CLI's output writer, written a few times per command.
pub fn units(_: DaemonVerb, _: &mut dyn std::io::Write, _: bool) -> anyhow::Result<i32> {
    anyhow::bail!(
        "blaze daemon: socket activation is systemd's and launchd's, and this system has neither"
    )
}

/// `blaze stop`, `blaze status` and `blaze daemon` in a blaze with no daemon: refused, naming
/// the feature, as there is none to stop, ask or install.
#[cfg(not(feature = "native"))]
// dyn: the CLI's output writer, unused: the refusal is the error.
pub fn stop(_: &mut dyn std::io::Write, _: bool) -> anyhow::Result<i32> {
    anyhow::bail!("blaze stop: {NO_DAEMON}")
}

#[cfg(not(feature = "native"))]
// dyn: the CLI's output writers, unused: the refusal is the error.
pub fn status(
    _: &mut dyn std::io::Write,
    _: &mut dyn std::io::Write,
    _: bool,
    _: bool,
) -> anyhow::Result<i32> {
    anyhow::bail!("blaze status: {NO_DAEMON}")
}

#[cfg(not(feature = "native"))]
// dyn: the CLI's output writer, unused: the refusal is the error.
pub fn units(_: DaemonVerb, _: &mut dyn std::io::Write, _: bool) -> anyhow::Result<i32> {
    anyhow::bail!("blaze daemon: {NO_DAEMON}")
}

/// The daemon's [`Status`] for people.
pub fn status_lines(status: &Status) -> String {
    let ids: Vec<String> = status.requests.iter().map(u64::to_string).collect();
    let requests = match ids.is_empty() {
        true => "0".to_owned(),
        false => format!("{} ({})", ids.len(), ids.join(", ")),
    };
    let mut lines = format!(
        "pid: {}\nrequests: {requests}\nviews: {}\nmemory: {} of {}\n",
        status.pid,
        status.views,
        bytes(status.memory_bytes),
        bytes(status.budget_bytes)
    );
    if let Some(why) = &status.history {
        lines.push_str(&format!("history: {why}\n"));
    }
    // The settings it was started with that are set: what a client setting other values is
    // refused over ([`crate::serve::settings`]).
    let set: Vec<String> = status
        .settings
        .iter()
        .filter_map(|it| Some(format!("{}={}", it.name, it.value.as_deref()?)))
        .collect();
    if !set.is_empty() {
        lines.push_str(&format!("settings: {}\n", set.join(" ")));
    }
    // The bugs the daemon went on past, always said, so 0 is read as checked.
    let bugs: u64 = status.bugs.iter().map(|it| it.count).sum();
    lines.push_str(&format!("bugs: {bugs}\n"));
    for bug in &status.bugs {
        lines.push_str(&format!("  {bug}\n"));
    }
    // What each build is doing: its request, command and age, where it is, and the outermost
    // and innermost open spans of each thread working for it.
    if !status.builds.is_empty() {
        lines.push_str(&format!("builds: {}\n", status.builds.len()));
    }
    for build in &status.builds {
        let age = blaze_fetch::progress::Secs(Duration::from_millis(build.for_ms));
        lines.push_str(&format!("  #{} {} ({age}): {}\n", build.id, build.what, build.phase));
        for span in &build.spans {
            lines.push_str(&format!("    {span}\n"));
        }
    }
    if !status.downloads.is_empty() {
        lines.push_str(&format!("downloads: {}\n", status.downloads.len()));
    }
    for it in &status.downloads {
        let line = blaze_fetch::progress::Line {
            done: it.done,
            total: it.total,
            rate: it.rate.map(|rate| rate as f64),
            idle: Duration::from_millis(it.idle_ms),
            elapsed: Duration::from_millis(it.elapsed_ms),
        };
        lines.push_str(&format!("  {}: {line}\n    from {}\n", it.what, it.url));
    }
    lines
}

/// `512 B`, `1.5 KiB`, `2.0 GiB`.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if count < 1024 {
        return format!("{count} B");
    }
    let mut value = count as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(all(test, feature = "native"))]
mod refusals {
    use super::*;

    fn failed(kind: io::ErrorKind) -> ClientError {
        ClientError::Io(io::Error::new(kind, "it"))
    }

    #[test]
    fn only_a_socket_nothing_listens_at_is_no_daemon() {
        assert!(nothing_listens(&failed(io::ErrorKind::NotFound)));
        assert!(nothing_listens(&failed(io::ErrorKind::ConnectionRefused)));
        for kind in
            [io::ErrorKind::PermissionDenied, io::ErrorKind::InvalidInput, io::ErrorKind::TimedOut]
        {
            assert!(!nothing_listens(&failed(kind)), "{kind:?}");
        }
        assert!(!nothing_listens(&ClientError::Schema { theirs: 0 }));
    }

    #[test]
    fn a_refused_connect_names_the_socket_and_a_failed_start_its_log() {
        let socket = PathBuf::from("/run/blaze/v2.sock");
        let said = DaemonError::Connect {
            socket: socket.clone(),
            error: failed(io::ErrorKind::PermissionDenied),
        }
        .to_string();
        assert!(said.starts_with("connecting to blaze's daemon at /run/blaze/v2.sock: "), "{said}");
        let log = PathBuf::from("/c/blaze/v2/serve.log");
        let said = DaemonError::NotListening {
            socket: socket.clone(),
            log: log.clone(),
            pid: 9,
            phase: Phase::State,
            inside: None,
        }
        .to_string();
        assert!(
            said.contains(
                "(`blaze serve`, pid 9) was started and did not listen at /run/blaze/v2.sock"
            ),
            "{said}"
        );
        let reading = "it was reading its state (the VFS journal, the daemon image and the store) for 60s, \
             and said nothing about what it was inside";
        assert!(said.contains(reading), "{said}");
        assert!(said.ends_with("its log is /c/blaze/v2/serve.log"), "{said}");
        let ended = Reaped::Code(101);
        let said = DaemonError::Exited {
            socket: socket.clone(),
            log: log.clone(),
            pid: 9,
            phase: Phase::Setup,
            ended,
        }
        .to_string();
        assert!(
            said.contains(
                "ended with exit code 101 (FAILED: a build or test failed, or blaze met an error of its own) while \
                 installing its store and the toolchain's proc macros, before it \
                 listened at /run/blaze/v2.sock"
            ),
            "{said}"
        );
        let ended = Reaped::Signal(11);
        let said = DaemonError::Exited {
            socket: socket.clone(),
            log: log.clone(),
            pid: 9,
            phase: Phase::Launched,
            ended,
        }
        .to_string();
        assert!(
            said.contains("ended by signal 11 while finding its build and its state"),
            "{said}"
        );
        let said = DaemonError::Closed {
            socket,
            log,
            pid: 9,
            phase: Phase::State,
            inside: Some("`open store` (0.4s open)".to_owned()),
        }
        .to_string();
        assert!(said.contains("closed its ready pipe while reading its state"), "{said}");
        assert!(said.contains("inside `open store` (0.4s open), before it listened"), "{said}");
    }

    #[cfg(unix)]
    #[test]
    fn a_started_daemon_is_heard_at_its_byte_or_its_end_not_at_the_limit() {
        use std::io::Write as _;
        let (ready, mut said) = std::io::pipe().unwrap();
        let started = std::time::Instant::now();
        said.write_all(b"\n").unwrap();
        assert_eq!(heard(&ready, STARTING).unwrap(), Heard::Listening);
        let (ready, said) = std::io::pipe().unwrap();
        drop(said);
        assert_eq!(
            heard(&ready, STARTING).unwrap(),
            Heard::Ended { phase: Phase::Launched, inside: None }
        );
        assert!(started.elapsed() < PATIENCE / 10, "heard after {:?}", started.elapsed());
        let (ready, _said) = std::io::pipe().unwrap();
        assert_eq!(
            heard(&ready, Duration::from_millis(20)).unwrap(),
            Heard::Nothing { phase: Phase::Launched, inside: None }
        );
    }

    /// A start is connected to only once it says it listens: the phases before are read and
    /// named, never taken for it, as the daemon of the 06:26 self-build was (connected to while
    /// it restored its image, before it had bound its socket).
    /// A start waiting for the daemon before it to end is given as long as that daemon is, and
    /// the start's own wait on top; every other phase, the start's.
    #[test]
    fn a_start_waiting_for_the_daemon_before_it_is_given_that_ones_wait() {
        assert_eq!(within(Phase::Waiting, STARTING), crate::serve::BEFORE_ENDS + STARTING);
        for phase in [Phase::Launched, Phase::State, Phase::Setup, Phase::Listening] {
            assert_eq!(within(phase, STARTING), STARTING);
        }
        assert!(Phase::Launched < Phase::Waiting && Phase::Waiting < Phase::State);
    }

    #[cfg(unix)]
    #[test]
    fn a_start_is_listening_only_at_its_last_word_and_stops_naming_the_phase_it_reached() {
        use std::io::Write as _;
        let (ready, mut said) = std::io::pipe().unwrap();
        said.write_all(&[Phase::State.byte(), Phase::Setup.byte(), Phase::Listening.byte()])
            .unwrap();
        assert_eq!(heard(&ready, STARTING).unwrap(), Heard::Listening);
        let (ready, mut said) = std::io::pipe().unwrap();
        said.write_all(&[Phase::State.byte()]).unwrap();
        assert_eq!(
            heard(&ready, Duration::from_millis(20)).unwrap(),
            Heard::Nothing { phase: Phase::State, inside: None }
        );
        said.write_all(&[Phase::Setup.byte()]).unwrap();
        drop(said);
        assert_eq!(
            heard(&ready, STARTING).unwrap(),
            Heard::Ended { phase: Phase::Setup, inside: None }
        );
        // Bounded: a phase said again, or one that is no phase, is refused.
        let (ready, mut said) = std::io::pipe().unwrap();
        said.write_all(&[Phase::Setup.byte(), Phase::State.byte()]).unwrap();
        let why = heard(&ready, STARTING).unwrap_err().to_string();
        assert!(why.contains("no later phase of a start"), "{why}");
        let (ready, mut said) = std::io::pipe().unwrap();
        said.write_all(b"x").unwrap();
        assert!(heard(&ready, STARTING).is_err());
    }

    /// The 2026-11-05 hang said only its phase. A start that says a phase, then what it is
    /// inside twice, then nothing, is named by the last span it said, whole records read across
    /// any cut of the bytes.
    #[cfg(unix)]
    #[test]
    fn a_start_that_never_listens_names_the_last_span_it_said_it_was_inside() {
        use std::io::Write as _;
        use crate::serve::inside_record;
        let mut bytes = vec![Phase::State.byte()];
        inside_record("`read journal` (0.5s open)", &mut bytes);
        inside_record("`open store` (crate=std, 1.0s open)", &mut bytes);
        let (ready, mut said) = std::io::pipe().unwrap();
        said.write_all(&bytes).unwrap();
        let inside = Some("`open store` (crate=std, 1.0s open)".to_owned());
        assert_eq!(
            heard(&ready, Duration::from_millis(20)).unwrap(),
            Heard::Nothing { phase: Phase::State, inside: inside.clone() }
        );
        // Cut anywhere, the same records are read.
        for cut in 0..bytes.len() {
            let mut hearing = Hearing::new();
            assert!(!hearing.feed(&bytes[..cut]).unwrap());
            assert!(!hearing.feed(&bytes[cut..]).unwrap());
            assert_eq!((hearing.phase, &hearing.inside), (Phase::State, &inside), "cut at {cut}");
        }
        let said = not_listening(
            Path::new("/run/blaze/v2.sock"),
            Path::new("/c/blaze/v2/serve.log"),
            9,
            Phase::State,
            inside.as_deref(),
        );
        assert!(
            said.contains(
                "for 60s, inside `open store` (crate=std, 1.0s open); its log is /c/blaze/v2/serve.log"
            ),
            "{said}"
        );
        // The record carries at most its cap, cut at a character's boundary.
        let mut long = Vec::new();
        inside_record(&"é".repeat(crate::serve::INSIDE_MAX), &mut long);
        let mut hearing = Hearing::new();
        assert!(!hearing.feed(&long).unwrap());
        let kept = hearing.inside.unwrap();
        assert!(kept.len() <= crate::serve::INSIDE_MAX && kept.chars().all(|it| it == 'é'));
        assert_eq!(kept.len(), crate::serve::INSIDE_MAX);
        // A record past the cap is refused.
        let mut hearing = Hearing::new();
        let over = u16::try_from(crate::serve::INSIDE_MAX + 1).unwrap().to_le_bytes();
        let why = hearing.feed(&[crate::serve::INSIDE, over[0], over[1]]).unwrap_err().to_string();
        assert!(why.contains("at most 512 are said"), "{why}");
    }

    /// The records of what a start is inside come every 500 ms, faster than a phase's wait:
    /// they never restart it. A writer says them as fast as the pipe takes them, and the wait
    /// of the phase still ends at its deadline.
    #[cfg(unix)]
    #[test]
    fn records_of_what_a_start_is_inside_do_not_extend_its_phase_wait() {
        use std::io::Write as _;
        let (ready, mut said) = std::io::pipe().unwrap();
        let writer = std::thread::spawn(move || {
            let began = std::time::Instant::now();
            let mut record = Vec::new();
            crate::serve::inside_record("`spin` (0.1s open)", &mut record);
            // Bounded: until the reader is gone (the write fails) or 30 s.
            while began.elapsed() < Duration::from_secs(30) {
                if said.write_all(&record).is_err() {
                    break;
                }
            }
        });
        let started = std::time::Instant::now();
        let long = Duration::from_millis(100);
        let heard = heard(&ready, long).unwrap();
        let waited = started.elapsed();
        drop(ready);
        writer.join().unwrap();
        assert_eq!(
            heard,
            Heard::Nothing {
                phase: Phase::Launched,
                inside: Some("`spin` (0.1s open)".to_owned())
            }
        );
        assert!(waited < Duration::from_secs(5), "the phase's 100 ms wait took {waited:?}");
    }

    #[test]
    fn only_a_closed_or_reset_connection_is_a_hang_up() {
        for kind in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
        ] {
            assert!(hung_up(&failed(kind)), "{kind:?}");
        }
        assert!(!hung_up(&failed(io::ErrorKind::NotFound)));
        assert!(!hung_up(&ClientError::Schema { theirs: 0 }));
    }

    /// hello13's log: two starts, the last one's stack overflow.
    const OVERFLOWED: &str = "blaze serve: started: the database starts empty: there is no daemon image\n\
        blaze serve: listening on /b/v2.sock (pid 41)\n\
        an old run's line\n\
        blaze serve: listening on /b/v2.sock (pid 4242)\n\
        \n\
        thread '<unknown>' (51573835) has overflowed its stack\n\
        fatal runtime error: stack overflow, aborting\n\
        blaze serve: pid 4242 ended by signal 6, SIGABRT: process abort signal\n";

    #[test]
    fn a_log_says_its_last_start_and_what_came_after_it() {
        let (pid, said) = last_start(OVERFLOWED).unwrap();
        assert_eq!(pid, 4242);
        assert_eq!(
            said,
            [
                "thread '<unknown>' (51573835) has overflowed its stack",
                "fatal runtime error: stack overflow, aborting",
                "blaze serve: pid 4242 ended by signal 6, SIGABRT: process abort signal",
            ]
        );
        assert_eq!(last_start("blaze serve: started: the database starts empty\n"), None);
        assert_eq!(last_start("blaze serve: listening on /b/v2.sock (pid x)\n"), None);
        let long: String = std::iter::once("blaze serve: listening on /s (pid 1)\n".to_owned())
            .chain((0..TAIL + 5).map(|it| format!("line {it}\n")))
            .collect();
        let (_, said) = last_start(&long).unwrap();
        assert_eq!((said.len(), said[0]), (TAIL, "line 5"));
    }

    #[test]
    fn a_start_says_why_it_is_cold_and_how_the_daemon_before_it_ended() {
        let log = "blaze serve: started: an older start\n\
            blaze serve: listening on /b/v2.sock (pid 41)\n\
            blaze serve: pid 41 asked to stop by SIGTERM\n\
            blaze serve: restored the daemon image x\n\
            blaze serve: started: the database starts empty: there is no daemon image; the daemon before it on this state (pid 41) ended\n\
            blaze serve: listening on /b/v2.sock (pid 42)\n";
        assert_eq!(
            start_said(log, 42),
            [
                "started: the database starts empty: there is no daemon image; the daemon before it on this state (pid 41) ended"
            ]
        );
        assert_eq!(start_said(log, 41), ["started: an older start"]);
        assert!(start_said(log, 7).is_empty());
    }

    #[test]
    fn a_lock_says_how_its_daemon_ended() {
        use crate::serve::Before;
        assert_eq!(Before::of("41 stopped"), Before::Stopped(41));
        assert_eq!(Before::of("41\n"), Before::Ended(41));
        assert_eq!(Before::of(""), Before::None);
        assert!(
            Before::Ended(41).to_string().contains("(pid 41) ended without finishing its stop")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_daemon_that_died_says_how_and_quotes_its_log() {
        use crate::raw::process::Reaped;
        let log = PathBuf::from("/b/v2/serve.log");
        let (pid, said) = last_start(OVERFLOWED).unwrap();
        let said: Vec<String> = said.into_iter().map(str::to_owned).collect();
        let died = Died {
            log: log.clone(),
            ended: Some((pid, Ended::Reaped(Reaped::Signal(libc::SIGABRT)))),
            said,
            inside: Ok(None),
            crash: Ok(None),
        };
        let text = died.to_string();
        assert!(
            text.starts_with(&format!(
                "the daemon (`blaze serve`, pid 4242) was ended by signal {}, SIGABRT: process abort signal; \
                 its log /b/v2/serve.log ends:\n  thread '<unknown>'",
                libc::SIGABRT
            )),
            "{text}"
        );
        assert!(
            text.ends_with("\n  blaze serve: pid 4242 ended by signal 6, SIGABRT: process abort signal\nits profile history holds no reading of what its threads were inside"),
            "{text}"
        );
        let inside = blaze_profile::reading::Reading {
            read_ns: 0,
            threads: vec![blaze_profile::slow::DiedThread {
                thread: 9,
                name: Some("blaze-work-3".to_owned()),
                spans: vec![blaze_profile::slow::DiedSpan {
                    name: "wait".to_owned(),
                    function: None,
                    awaits: Some("the macro server".to_owned()),
                    owner: Some(12),
                    for_ns: 3_000_000_000,
                }],
            }],
        };
        let killed = Died {
            log: log.clone(),
            ended: Some((7, Ended::Gone)),
            said: Vec::new(),
            inside: Ok(Some(inside)),
            crash: Ok(None),
        }
        .to_string();
        assert!(
            killed.contains(
                "\n  blaze-work-3 (tid 9): wait, awaiting the macro server, run by task 12 (3.0 s)"
            ),
            "{killed}"
        );
        assert!(
            killed.starts_with(
                "the daemon (`blaze serve`, pid 7) is gone; its log /b/v2/serve.log says nothing"
            ),
            "{killed}"
        );
        assert!(killed.contains("SIGKILL"), "{killed}");
        let running = Died {
            log,
            ended: Some((7, Ended::Running)),
            said: Vec::new(),
            inside: Ok(None),
            crash: Ok(None),
        }
        .to_string();
        assert!(
            running.contains("still runs: it hung up on this request alone")
                && !running.contains("SIGKILL"),
            "{running}"
        );
    }
}
