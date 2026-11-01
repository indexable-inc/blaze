//! The daemon this CLI is a client of: found at its socket ([`crate::serve::socket`]), started
//! as `blaze serve` when none answers there, spoken to through [`blaze_api::client::Client`].

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use blaze_api::{
    Request, ResponseRef, Status,
    client::{Client, ClientError},
};
#[cfg(unix)]
use blaze_api::units::Installed;

use super::args::DaemonVerb;

/// How long a daemon that was started is waited for, and one that was told to stop.
pub const PATIENCE: Duration = Duration::from_secs(10);
/// A socket that comes up within a millisecond is used within one.
const POLL: Duration = Duration::from_millis(1);

/// What the commands ask of the daemon, which the tests fake.
pub trait Api {
    /// Sends `request` and hands each of its responses to `on`, read where it lies in its frame
    /// ([`ResponseRef`]), until its Done (handed too); Done's `ok`. `on`'s second argument says
    /// whether another whole response is already held ([`Client::call_ref`]'s), so a caller that
    /// prints writes what it holds only when it is false: one write per burst the daemon sent.
    fn call<F: FnMut(ResponseRef<'_>, bool)>(&mut self, request: Request, on: F) -> Result<bool, ClientError>;

    /// What came of a daemon that hung up on a call ([`hung_up`]); `None` where there is no
    /// daemon's process and log to ask, as with the tests' fakes.
    fn died(&mut self) -> Option<anyhow::Result<Died>> {
        None
    }
}

/// One request per connection: anything held once its Done has come (a response to another
/// request, or more of this one) is the daemon's mistake, said as one.
impl Api for Client {
    fn call<F: FnMut(ResponseRef<'_>, bool)>(&mut self, request: Request, on: F) -> Result<bool, ClientError> {
        let ok = self.call_ref(request, on)?;
        if self.buffered() {
            let said = "the daemon sent more on a connection whose one request had its Done";
            // alloc: error path, the daemon's mistake.
            return Err(ClientError::Refused(blaze_api::ApiError::new(said, None)));
        }
        Ok(ok)
    }

    fn died(&mut self) -> Option<anyhow::Result<Died>> {
        Some(died())
    }
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
    /// The daemon was started and nothing listened at its socket within [`PATIENCE`], after
    /// `tries` connects.
    NotListening { socket: PathBuf, log: PathBuf, tries: u32, error: ClientError },
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Connect { socket, error } => {
                write!(f, "connecting to blaze's daemon at {}: {error}", socket.display())
            }
            DaemonError::NotListening { socket, log, tries, error } => {
                // alloc: error path, said once.
                write!(f, "{} ({tries} connects tried)", not_listening(socket, log, &error.to_string()))
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
#[cfg(any(unix, windows))]
pub fn connect(start: Start) -> anyhow::Result<Option<Client>> {
    let socket = crate::serve::socket()?;
    let refused = match Client::unix(&socket) {
        Ok(client) => return Ok(Some(client)),
        Err(error) if !nothing_listens(&error) => return Err(DaemonError::Connect { socket, error }.into()),
        Err(_) if start == Start::Never => return Ok(None),
        Err(error) => error,
    };
    #[cfg(unix)]
    match activation(&socket)? {
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
    #[cfg(windows)]
    eprintln!("blaze: starting the daemon ({refused})");
    let log = started()?;
    let asked = Instant::now();
    let mut tries = 0u32;
    loop {
        tries += 1;
        match Client::unix(&socket) {
            Ok(client) => return Ok(Some(client)),
            Err(error) if !nothing_listens(&error) => return Err(DaemonError::Connect { socket, error }.into()),
            Err(error) if asked.elapsed() > PATIENCE => {
                return Err(DaemonError::NotListening { socket, log, tries, error }.into());
            }
            Err(_) => std::thread::sleep(POLL),
        }
    }
}

/// Whether a launchd or systemd unit serves `socket`. Units live under `HOME`: without one,
/// none is installed.
#[cfg(unix)]
fn activation(socket: &Path) -> anyhow::Result<Installed> {
    if blaze_api::activation::Manager::here().is_none() || std::env::var_os("HOME").is_none_or(|it| it.is_empty()) {
        return Ok(Installed::No);
    }
    Ok(blaze_api::units::installed(&blaze_api::units::place_here()?, socket)?)
}

/// What a start that never listened says.
pub fn not_listening(socket: &Path, log: &Path, error: &str) -> String {
    format!(
        "the daemon (`blaze serve`) was started and nothing listens at {} after {}s ({error}); \
         its log is {}",
        socket.display(),
        PATIENCE.as_secs(),
        log.display()
    )
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
}

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
                write!(f, "the daemon (`blaze serve`, pid {pid}) exited with code {code}")?;
            }
            Some((pid, Ended::Gone)) => write!(f, "the daemon (`blaze serve`, pid {pid}) is gone")?,
            Some((pid, Ended::Running)) => {
                write!(f, "the daemon (`blaze serve`, pid {pid}) still runs: it hung up on this request alone")?;
            }
            None => write!(f, "the last {} KiB of the daemon's log name no start of it", TAIL_BYTES >> 10)?,
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
            return Ok(());
        }
        write!(f, "; its log {} ends:", self.log.display())?;
        for line in &self.said {
            write!(f, "\n  {line}")?;
        }
        Ok(())
    }
}

/// The pid of the daemon's last start in `log` (its [`crate::serve::LISTENING`] line), and
/// its lines after that are not blank, the last [`TAIL`]; `None` when `log` has no start.
pub fn last_start(log: &str) -> Option<(u32, Vec<&str>)> {
    // alloc: an error path, once a command
    let lines: Vec<&str> = log.lines().collect();
    let (at, pid) = lines.iter().enumerate().rev().find_map(|(at, line)| Some((at, listening_pid(line)?)))?;
    // alloc: as above
    let mut said: Vec<&str> = lines[at + 1..].iter().copied().filter(|it| !it.trim().is_empty()).collect();
    said.drain(..said.len().saturating_sub(TAIL));
    Some((pid, said))
}

/// `7` of `blaze serve: listening on /run/blaze/v2.sock (pid 7)`.
fn listening_pid(line: &str) -> Option<u32> {
    let (_, pid) = line.strip_prefix(crate::serve::LISTENING)?.rsplit_once(" (pid ")?;
    pid.strip_suffix(')')?.parse().ok()
}

/// What came of the daemon that hung up on a request: its log's last start and what it wrote
/// since, and how its process ended, given [`ENDING`] to.
#[cfg(any(unix, windows))]
pub fn died() -> anyhow::Result<Died> {
    use std::io::{Read as _, Seek as _};

    use anyhow::Context as _;
    let log = crate::serve::state()?.join("serve.log");
    let read = |log: &Path| -> io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(log)?;
        let len = file.metadata()?.len();
        file.seek(io::SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
        let mut bytes = Vec::new(); // alloc: an error path, once a command
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    // alloc: an error path, once a command
    let bytes = read(&log).with_context(|| format!("reading the end of the daemon's log {}", log.display()))?;
    let text = String::from_utf8_lossy(&bytes); // alloc: as above, where the log is not UTF-8
    let Some((pid, said)) = last_start(&text) else {
        return Ok(Died { log, ended: None, said: Vec::new() }); // alloc: none, empty
    };
    let said = said.into_iter().map(str::to_owned).collect(); // alloc: as above
    let asked = Instant::now();
    let ended = loop {
        // Only a unix parent hears how its child ended; a Windows one has its exit code only from a handle it kept.
        #[cfg(unix)]
        if let Some(reaped) = crate::raw::process::reap(pid) {
            break Ended::Reaped(reaped);
        }
        // A process that is gone but not yet waited for by its parent is still `alive`.
        if !crate::raw::process::alive(pid) {
            break Ended::Gone;
        }
        if asked.elapsed() > ENDING {
            break Ended::Running;
        }
        std::thread::sleep(POLL);
    };
    Ok(Died { log, ended: Some((pid, ended)), said })
}

/// Starts `blaze serve` in a process group of its own, so that what ends this command or its
/// terminal's foreground does not end it, with its output appended to its log; the log.
#[cfg(any(unix, windows))]
fn started() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context as _;
    let state = crate::serve::state()?;
    crate::socket::private_dir(&state).with_context(|| format!("the daemon's directory {}", state.display()))?;
    let log = state.join("serve.log");
    let mut options = std::fs::OpenOptions::new();
    let file = crate::socket::private_file(options.create(true).append(true), &log)
        .with_context(|| format!("the daemon's log {}", log.display()))?;
    let mut daemon = std::process::Command::new(std::env::current_exe().context("finding blaze's own executable")?);
    daemon.arg("serve");
    daemon.stdin(std::process::Stdio::null()).stdout(file.try_clone()?).stderr(file);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut daemon, 0);
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW: out of this console's Ctrl-C and close.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut daemon, 0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    // The daemon inherits no pipe of ours: whoever reads this command's output would wait for it to end.
    #[cfg(windows)]
    let _withheld = crate::raw::process::withhold_inheritance();
    daemon.spawn().context("starting the daemon (`blaze serve`)")?;
    Ok(log)
}

/// `blaze stop`: the daemon made durable and ended, waited for until its socket is gone.
// dyn: the CLI's output writer, written a few times per command.
pub fn stop(err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    let Some(mut client) = connect(Start::Never)? else {
        writeln!(err, "no daemon is running")?;
        return Ok(0);
    };
    let mut said = Vec::new();
    let ok = Api::call(&mut client, Request::Stop, |response, _| {
        if let ResponseRef::Error(error) = response {
            // alloc: error path, the daemon's errors, said once it is done.
            said.push(error.what.to_owned());
        }
    })?;
    for what in said {
        err.write_all(super::say::error(&what, color).as_bytes())?;
    }
    if !ok {
        return Ok(super::FAILED);
    }
    let socket = crate::serve::socket()?;
    // With a unit installed the socket answers on (the manager listens, and a connect would
    // start the daemon again): the daemon hangs up its connection last, once its image is
    // written.
    #[cfg(unix)]
    if activation(&socket)? == Installed::Yes {
        if let Err(error) = client.until_hung_up(PATIENCE) {
            let what = format!("the daemon at {} was told to stop: {error}", socket.display());
            err.write_all(super::say::error(&what, color).as_bytes())?;
            return Ok(super::FAILED);
        }
        return Ok(0);
    }
    drop(client);
    let asked = Instant::now();
    while answers(&socket)? {
        if asked.elapsed() > PATIENCE {
            let what = format!("the daemon at {} was told to stop and still answers after {}s", socket.display(), PATIENCE.as_secs());
            err.write_all(super::say::error(&what, color).as_bytes())?;
            return Ok(super::FAILED);
        }
        std::thread::sleep(POLL);
    }
    Ok(0)
}

/// Whether the daemon still answers at `socket`; a connect that fails for another reason than
/// that nothing listens is the error.
#[cfg(any(unix, windows))]
fn answers(socket: &Path) -> Result<bool, DaemonError> {
    match Client::unix(socket) {
        Ok(_) => Ok(true),
        Err(error) if nothing_listens(&error) => Ok(false),
        Err(error) => Err(DaemonError::Connect { socket: socket.to_owned(), error }),
    }
}

/// `blaze status`: what the daemon is doing, on stdout.
// dyn: the CLI's output writer, written a few times per command.
pub fn status(out: &mut dyn std::io::Write, err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    let Some(mut client) = connect(Start::Never)? else {
        writeln!(out, "no daemon is running")?;
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
    if let Some(status) = status {
        out.write_all(status_lines(&status).as_bytes())?;
    }
    Ok(if ok { 0 } else { super::FAILED })
}

/// `blaze daemon install`: a running daemon stopped, then the launchd agent or systemd units
/// written and loaded ([`blaze_api::units::install`]), so the system listens on the socket and
/// starts the daemon at the first connection. `blaze daemon uninstall`: the daemon stopped,
/// the units unloaded and removed.
#[cfg(unix)]
// dyn: the CLI's output writer, written a few times per command.
pub fn units(verb: DaemonVerb, err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    use anyhow::Context as _;
    use blaze_api::units;
    let socket = crate::serve::socket()?;
    let place = units::place_here()?;
    let steps = match verb {
        DaemonVerb::Install => {
            let program = std::env::current_exe().context("finding blaze's own executable")?;
            let log = crate::serve::state()?.join("serve.log");
            let (unit, place) = units::here(program, socket.clone(), log)?;
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
            "installed {}: the system starts the daemon at the first connection to {}",
            units::main_file(&place).display(),
            socket.display()
        )?,
        DaemonVerb::Uninstall => writeln!(err, "uninstalled the units in {}", place.dir.display())?,
    }
    Ok(0)
}

#[cfg(not(unix))]
// dyn: the CLI's output writer, written a few times per command.
pub fn units(_: DaemonVerb, _: &mut dyn std::io::Write, _: bool) -> anyhow::Result<i32> {
    anyhow::bail!("blaze daemon: socket activation is systemd's and launchd's, and this system has neither")
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

#[cfg(test)]
mod refusals {
    use super::*;

    fn failed(kind: io::ErrorKind) -> ClientError {
        ClientError::Io(io::Error::new(kind, "it"))
    }

    #[test]
    fn only_a_socket_nothing_listens_at_is_no_daemon() {
        assert!(nothing_listens(&failed(io::ErrorKind::NotFound)));
        assert!(nothing_listens(&failed(io::ErrorKind::ConnectionRefused)));
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::InvalidInput, io::ErrorKind::TimedOut] {
            assert!(!nothing_listens(&failed(kind)), "{kind:?}");
        }
        assert!(!nothing_listens(&ClientError::Schema { theirs: 0 }));
    }

    #[test]
    fn a_refused_connect_names_the_socket_and_a_start_the_tries() {
        let socket = PathBuf::from("/run/blaze/v2.sock");
        let said = DaemonError::Connect { socket: socket.clone(), error: failed(io::ErrorKind::PermissionDenied) }.to_string();
        assert!(said.starts_with("connecting to blaze's daemon at /run/blaze/v2.sock: "), "{said}");
        let log = PathBuf::from("/c/blaze/v2/serve.log");
        let said = DaemonError::NotListening { socket, log, tries: 9001, error: failed(io::ErrorKind::ConnectionRefused) }.to_string();
        assert!(said.contains("nothing listens at /run/blaze/v2.sock after 10s"), "{said}");
        assert!(said.ends_with("(9001 connects tried)"), "{said}");
    }

    #[test]
    fn only_a_closed_or_reset_connection_is_a_hang_up() {
        for kind in [io::ErrorKind::UnexpectedEof, io::ErrorKind::ConnectionReset, io::ErrorKind::BrokenPipe] {
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

    #[cfg(unix)]
    #[test]
    fn a_daemon_that_died_says_how_and_quotes_its_log() {
        use crate::raw::process::Reaped;
        let log = PathBuf::from("/b/v2/serve.log");
        let (pid, said) = last_start(OVERFLOWED).unwrap();
        let said: Vec<String> = said.into_iter().map(str::to_owned).collect();
        let died = Died { log: log.clone(), ended: Some((pid, Ended::Reaped(Reaped::Signal(libc::SIGABRT)))), said };
        let text = died.to_string();
        assert!(
            text.starts_with(&format!(
                "the daemon (`blaze serve`, pid 4242) was ended by signal {}, SIGABRT: process abort signal; \
                 its log /b/v2/serve.log ends:\n  thread '<unknown>'",
                libc::SIGABRT
            )),
            "{text}"
        );
        assert!(text.ends_with("\n  blaze serve: pid 4242 ended by signal 6, SIGABRT: process abort signal"), "{text}");
        let killed = Died { log: log.clone(), ended: Some((7, Ended::Gone)), said: Vec::new() }.to_string();
        assert!(killed.starts_with("the daemon (`blaze serve`, pid 7) is gone; its log /b/v2/serve.log says nothing"), "{killed}");
        assert!(killed.contains("SIGKILL"), "{killed}");
        let running = Died { log, ended: Some((7, Ended::Running)), said: Vec::new() }.to_string();
        assert!(running.contains("still runs: it hung up on this request alone") && !running.contains("SIGKILL"), "{running}");
    }
}
