//! The connection of a `cargo ra-jit` to the user's daemon: finding it, starting it when
//! there is none, and replacing one of another version.

use std::{
    fs::OpenOptions,
    io::{BufRead as _, BufReader, Write as _},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::Context as _;

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

use super::{
    daemon::{Envelope, Reply, Request, SPARE, api, home, socket_in, version},
    switch, wrapper,
};
use crate::socket::{connect, private_dir, private_file};

/// How long a daemon that was started is waited for, and one that was told to exit.
const PATIENCE: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(5);

/// Whether a daemon is started when none listens.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Start {
    IfMissing,
    Never,
}

/// Sends `request` to the daemon. `None` when there is none and none was to be started;
/// `Reply::Restart` when the one there is is of another version and none was to be started.
pub(crate) fn ask(request: Request, start: Start) -> anyhow::Result<Option<Reply>> {
    let home = home()?;
    let socket = socket_in(&home);
    // A daemon of another version exits for a client that starts its own, and for one that
    // tells it to stop.
    let replaces = start == Start::IfMissing || matches!(request, Request::Stop);
    // A whole build is asked for at once: no cargo builds meanwhile for a spare to load beside.
    let spares = !matches!(&request, Request::Build(checkout) if checkout.config.whole);
    let started = |home: &Path| started(home, spares);
    let envelope = Envelope { version: version(), replaces, request };
    let line = serde_json::to_string(&envelope)?;
    let call = api::Call {
        id: 1,
        schema: blaze_api::SCHEMA,
        version: Some(envelope.version),
        replaces,
        body: api::Body::Cargo(envelope.request),
    };
    let envelope = Sent { home: &home, line, call: blaze_api::to_vec(&call)? };
    let mut stream = match (connect(&socket), start) {
        (Ok(stream), _) => stream,
        (Err(_), Start::Never) => return Ok(None),
        (Err(_), Start::IfMissing) => started(&home)?,
    };
    let mut reply = match exchange(&mut stream, &envelope) {
        // A daemon that was on its way out when it was asked is as good as none.
        Err(_) if start == Start::IfMissing => {
            gone(&socket);
            exchange(&mut started(&home)?, &envelope)?
        }
        // One that took its socket with it shut down; one that was killed left it.
        Err(_) if left(&socket) => return Ok(None),
        reply => {
            let reply = reply?;
            if matches!(reply, Reply::Stopping)
                || replaces && matches!(reply, Reply::Restart { .. })
            {
                ended(&mut stream);
            }
            reply
        }
    };
    if let Reply::Restart { .. } = reply {
        if start == Start::Never {
            return Ok(Some(reply));
        }
        gone(&socket);
        reply = exchange(&mut started(&home)?, &envelope)?;
    }
    Ok(Some(reply))
}

/// Waits for a daemon that answered it ends to have exited, with its workers stopped and its
/// log written: it keeps the connection until its process exits, and no other process has it.
fn ended(stream: &mut UnixStream) {
    let _ = stream.set_read_timeout(Some(PATIENCE));
    let mut rest = [0; 64];
    while matches!(std::io::Read::read(stream, &mut rest), Ok(1..)) {}
}

/// Whether the daemon that did not answer has shut down, rather than been killed.
fn left(socket: &Path) -> bool {
    let asked = Instant::now();
    while socket.exists() && asked.elapsed() < Duration::from_millis(200) {
        std::thread::sleep(POLL);
    }
    !socket.exists()
}

/// Waits for a daemon that is exiting to be gone: it exits once it has answered, and its
/// socket goes with it.
pub(crate) fn gone(socket: &Path) {
    let asked = Instant::now();
    while connect(socket).is_ok() && asked.elapsed() < PATIENCE {
        std::thread::sleep(POLL);
    }
}

/// Starts `daemon` in a process group and a console of its own, and until the result is
/// dropped lets it inherit no handle of this process but those it is given. A process is
/// started with every handle of its parent that can be inherited, and those of a pipe this
/// command writes to are among them, under their own numbers and under those the commands
/// above this one had: whoever reads the pipe would wait for the daemon to end.
#[cfg(windows)]
fn on_its_own(daemon: &mut Command) -> impl Drop + use<> {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::{
        Foundation::{GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation},
        System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW},
    };

    /// Handles are multiples of four, and a command as short as this one has few.
    const HANDLES: std::ops::RangeInclusive<usize> = 4..=0x1_0000;

    struct Inherited(Vec<HANDLE>);
    impl Drop for Inherited {
        fn drop(&mut self) {
            for &handle in &self.0 {
                // SAFETY: the handle was open and inheritable when it was listed, and
                // nothing closed one of this command's since.
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
            }
        }
    }

    daemon.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    let mut inherited = Vec::new();
    for handle in HANDLES.step_by(4).map(|it| it as HANDLE) {
        let mut flags = 0;
        // SAFETY: a number that is no open handle is refused, and `flags` is only written.
        let held = unsafe {
            GetHandleInformation(handle, &mut flags) != 0
                && flags & HANDLE_FLAG_INHERIT != 0
                && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0
        };
        if held {
            inherited.push(handle);
        }
    }
    Inherited(inherited)
}

/// Why no daemon answered: there is none, or one ended without taking its socket with it.
pub(crate) fn why_none() -> String {
    let left = home().map(|it| (socket_in(&it), it.join("daemon.log")));
    match left {
        Ok((socket, log)) if socket.exists() => format!(
            "the daemon is gone without having shut down (killed?), its log is {}",
            log.display()
        ),
        _ => "no daemon is running".to_owned(),
    }
}

/// A request as a line of JSON and as a call of the agent API, which a daemon that takes calls
/// is sent.
struct Sent<'a> {
    home: &'a Path,
    line: String,
    call: Vec<u8>,
}

fn exchange(stream: &mut UnixStream, envelope: &Sent<'_>) -> anyhow::Result<Reply> {
    if api::marker_in(envelope.home).exists() {
        blaze_api::frame::write(stream, &envelope.call)?;
        let answer = blaze_api::frame::read(stream).ok().flatten();
        let Some(answer) = answer else {
            let log = envelope.home.join("daemon.log");
            anyhow::bail!(
                "the daemon ended before it answered (killed?), its log is {}",
                log.display()
            );
        };
        let answer: api::Answer = blaze_api::from_slice(&answer)
            .context("the daemon's answer is not one of this schema")?;
        return match answer.body {
            api::Answered::Cargo(reply) => Ok(reply),
            api::Answered::Refused(why) => anyhow::bail!("the daemon refused the request: {why}"),
            other => anyhow::bail!("the daemon answered {other:?}"),
        };
    }
    stream.write_all(format!("{}\n", envelope.line).as_bytes())?;
    let mut line = String::new();
    let read = BufReader::new(stream).read_line(&mut line);
    // A line without its end is one the daemon did not live to finish.
    if !matches!(read, Ok(1..)) || !line.ends_with('\n') {
        let log = home()?.join("daemon.log");
        anyhow::bail!(
            "the daemon ended before it answered (killed?), its log is {}",
            log.display()
        );
    }
    serde_json::from_str(&line).context("the daemon's answer is not one of this version")
}

/// Starts a daemon in a process group of its own, so that what ends this command or its
/// terminal's foreground does not end the daemon, and waits for it to listen.
fn started(home: &Path, spares: bool) -> anyhow::Result<UnixStream> {
    private_dir(home).with_context(|| format!("the daemon's directory {}", home.display()))?;
    let log = private_file(OpenOptions::new().create(true).append(true), &home.join("daemon.log"))?;
    let mut daemon = Command::new(std::env::current_exe()?);
    daemon
        .arg("daemon")
        .env_remove(wrapper::DRIVEN)
        .env_remove(switch::VARIABLE)
        .env_remove(switch::CHAINED);
    if !spares && std::env::var_os(SPARE).is_none() {
        daemon.env(SPARE, "0");
    }
    daemon.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut daemon, 0);
    #[cfg(windows)]
    let kept = on_its_own(&mut daemon);
    daemon.spawn().context("starting the daemon")?;
    #[cfg(windows)]
    drop(kept);
    let socket = socket_in(home);
    let asked = Instant::now();
    loop {
        match connect(&socket) {
            Ok(stream) => return Ok(stream),
            Err(error) if asked.elapsed() > PATIENCE => {
                return Err(error).with_context(|| {
                    format!("the daemon did not start, see {}", home.join("daemon.log").display())
                });
            }
            Err(_) => std::thread::sleep(POLL),
        }
    }
}
