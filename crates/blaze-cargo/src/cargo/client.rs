//! The connection of a `blaze` to the user's daemon: finding it, starting it when
//! there is none, and replacing one of another version.

use std::{
    fs::OpenOptions,
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::Context as _;

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

use blaze_protocol::Message;

use super::{
    daemon::{Answer, Envelope, Reply, Request, SPARE, home, socket_in, version},
    say,
};
use crate::socket::{connect, private_dir, private_file};

/// How long a daemon that was started is waited for, and one that was told to exit.
const PATIENCE: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(5);

/// Whether a daemon is started when none listens.
#[derive(Clone, Copy, PartialEq)]
pub enum Start {
    IfMissing,
    Never,
}

/// Sends `request` to the daemon. `None` when there is none and none was to be started;
/// `Reply::Restart` when the one there is is of another version and none was to be started.
pub fn ask(request: Request, start: Start) -> anyhow::Result<Option<Reply>> {
    let home = home()?;
    let socket = socket_in(&home);
    // A daemon of another version exits for a client that starts its own, and for one that
    // tells it to stop.
    let replaces = start == Start::IfMissing || matches!(request, Request::Stop);
    // A whole build is asked for at once: no cargo builds meanwhile for a spare to load beside.
    let spares = !matches!(&request, Request::Build(checkout) if checkout.config.whole);
    let started = |home: &Path| started(home, spares);
    let exchange = |stream: &mut UnixStream, envelope: &[u8]| exchange(stream, envelope, replaces);
    let request = request.of_this_command();
    let envelope = framed(&Envelope { version: version(), replaces, request })?;
    let mut stream = match (connect(&socket), start) {
        (Ok(stream), _) => stream,
        (Err(_), Start::Never) => return Ok(None),
        (Err(_), Start::IfMissing) => started(&home)?,
    };
    let mut reply = match exchange(&mut stream, &envelope[..]) {
        // A daemon that was on its way out when it was asked is as good as none.
        Err(_) if start == Start::IfMissing => {
            gone(&socket);
            exchange(&mut started(&home)?, &envelope[..])?
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
        reply = exchange(&mut started(&home)?, &envelope[..])?;
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
pub fn gone(socket: &Path) {
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
pub fn why_none() -> String {
    let left = home().map(|it| (socket_in(&it), it.join("daemon.log")));
    match left {
        Ok((socket, log)) if socket.exists() => format!(
            "the daemon is gone without having shut down (killed?), its log is {}",
            log.display()
        ),
        _ => "no daemon is running".to_owned(),
    }
}

/// `envelope` as the daemon reads it: a frame of a [`Message`], and a newline after it for a
/// daemon of before the frames, which reads a line ([`crate::socket::read_asked`]).
fn framed(envelope: &Envelope) -> anyhow::Result<Vec<u8>> {
    let item = blaze_protocol::cbor::to_vec(&Message::new(ASKED, envelope))?;
    let mut framed = Vec::with_capacity(item.len() + 5);
    blaze_protocol::write_frame(&mut framed, &item)?;
    framed.push(b'\n');
    Ok(framed)
}

/// The id of the one request a connection carries.
const ASKED: u64 = 1;

/// How often what a command shows of the build it waits for is drawn again.
const TICK: Duration = Duration::from_millis(100);

/// What the daemon sent next.
enum Got {
    Answer(Answer),
    /// A line of JSON: the daemon is of a blaze from before the frames.
    Older(String),
}

/// Sends `envelope` and reads the answer, showing the records of the build it waits for as
/// they come ([`say::Shown`]), and drawing them again every [`TICK`] that brings none. A
/// daemon from before the frames is told to stop in its own JSON, when the client
/// `replaces` it, and answers as one of another version.
fn exchange(stream: &mut UnixStream, envelope: &[u8], replaces: bool) -> anyhow::Result<Reply> {
    stream.write_all(envelope)?;
    stream.set_read_timeout(Some(TICK))?;
    let mut shown = say::Shown::new();
    // What was read and is not a whole frame yet.
    let mut got = Vec::new();
    let mut chunk = vec![0; 64 << 10];
    loop {
        while let Some(next) =
            next(&mut got).context("the daemon's answer is not one of this version")?
        {
            match next {
                Got::Answer(Answer::Record(record)) => shown.record(&record),
                Got::Answer(Answer::Reply(reply)) => {
                    shown.done();
                    return Ok(reply);
                }
                Got::Older(line) => {
                    drop(shown);
                    let reply = serde_json::from_str::<Reply>(&line);
                    let version = match reply {
                        Ok(Reply::Restart { version }) => version,
                        _ => "of before the frames".to_owned(),
                    };
                    if replaces {
                        stop_older(&socket_in(&home()?));
                    }
                    return Ok(Reply::Restart { version });
                }
            }
        }
        match std::io::Read::read(stream, &mut chunk) {
            Ok(0) => break,
            Ok(read) => got.extend_from_slice(&chunk[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                shown.tick();
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            // One that took its socket with it, or hung up, is as good as gone.
            Err(_) => break,
        }
    }
    drop(shown);
    let log = home()?.join("daemon.log");
    anyhow::bail!("the daemon ended before it answered (killed?), its log is {}", log.display())
}

/// The next whole thing the daemon sent of what was read, `got`, taken out of it; `None`
/// until the rest of it is read.
fn next(got: &mut Vec<u8>) -> std::io::Result<Option<Got>> {
    if got.first() == Some(&b'{') {
        let Some(end) = got.iter().position(|&it| it == b'\n') else { return Ok(None) };
        let line: Vec<u8> = got.drain(..=end).collect();
        return Ok(Some(Got::Older(String::from_utf8_lossy(&line).into_owned())));
    }
    let Some(len) = got.first_chunk::<4>().map(|it| u32::from_be_bytes(*it)) else {
        return Ok(None);
    };
    if len > blaze_protocol::LONGEST_FRAME {
        let said = format!("a frame of {len} bytes");
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, said));
    }
    let end = 4 + len as usize;
    if got.len() < end {
        return Ok(None);
    }
    let message = blaze_protocol::cbor::from_slice::<Message<Answer>>(&got[4..end])
        .map_err(|it| std::io::Error::new(std::io::ErrorKind::InvalidData, it));
    got.drain(..end);
    Ok(Some(Got::Answer(message?.body)))
}

/// Has the daemon at `socket`, of a blaze from before the frames, exit: it reads a request
/// in JSON, and one of another version that replaces it ends it.
fn stop_older(socket: &Path) {
    let Ok(mut stream) = connect(socket) else { return };
    let stop = "{\"version\":\"\",\"replaces\":true,\"request\":\"Stop\"}\n";
    if stream.write_all(stop.as_bytes()).is_ok() {
        ended(&mut stream);
    }
}

/// Starts a daemon in a process group of its own, so that what ends this command or its
/// terminal's foreground does not end the daemon, and waits for it to listen.
fn started(home: &Path, spares: bool) -> anyhow::Result<UnixStream> {
    private_dir(home).with_context(|| format!("the daemon's directory {}", home.display()))?;
    let log = private_file(OpenOptions::new().create(true).append(true), &home.join("daemon.log"))?;
    let mut daemon = Command::new(std::env::current_exe()?);
    daemon.arg("daemon");
    // Each build the client asks to profile says so itself (`Checkout::profile`): the
    // daemon outlives this one.
    daemon.env_remove(crate::trace::VAR);
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
