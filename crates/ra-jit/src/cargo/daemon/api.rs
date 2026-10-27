//! The agent API: calls in deterministic CBOR ([`blaze_api`]), framed on the daemon's socket
//! beside the lines of older clients and told from them by the first byte, and over HTTP on
//! the loopback when `RA_JIT_HTTP` names an address.
//!
//! A connection carries any number of calls, each answered as it is done, with the id of the
//! call it answers: one that waits for a build holds up none sent after it. `cargo blaze` asks
//! for its builds as a [`Body::Cargo`] call when the daemon says it takes calls (the file
//! [`marker_in`] its home), and an agent applies files as one transaction with
//! [`Body::Apply`].

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

use anyhow::Context as _;
use blaze_api::{Bytes, SCHEMA, frame};
use serde::{Deserialize, Serialize};

use super::{Daemon, Reply, Request, Warmth, lock};
use crate::socket::{self, private_file};

/// The file in the daemon's home that says the daemon on its socket takes calls, with the
/// version of the schema. A daemon of before the API has none: its clients send lines.
pub(crate) fn marker_in(home: &Path) -> PathBuf {
    home.join("daemon.api")
}

/// The file in the daemon's home with the token an HTTP call has to send, as
/// `Authorization: Bearer <token>`: the loopback is open to every user of the machine, and
/// only this one can read the file.
pub(crate) fn token_in(home: &Path) -> PathBuf {
    home.join("http-token")
}

/// One call.
#[derive(Serialize, Deserialize)]
pub(crate) struct Call {
    /// Said again in the answer.
    pub(crate) id: u64,
    pub(crate) schema: u64,
    /// The version of the `cargo blaze` that calls, which a daemon of another replaces itself
    /// for as for a line. An agent says none.
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) replaces: bool,
    pub(crate) body: Body,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Body {
    /// Which daemon this is and which schema it speaks.
    Hello,
    /// A request of `cargo blaze`'s, as its lines have them.
    Cargo(Request),
    /// Files as one transaction.
    Apply(Apply),
}

/// Files that are what the next build reads, all of them or none: those of one workspace
/// loaded. The worker answers with the root of its sources after them, and, when `build`
/// asks, with its build of the programs that have them, linked as `cargo ra-jit build` links
/// them.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Apply {
    pub(crate) files: Vec<File>,
    #[serde(default)]
    pub(crate) build: bool,
    /// Whether they are written to disk too, which is how a new file joins the workspace.
    #[serde(default = "yes")]
    pub(crate) disk: bool,
}

fn yes() -> bool {
    true
}

/// A file of an [`Apply`]: its absolute path and what it is to hold, or `None` to delete it.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct File {
    pub(crate) path: String,
    pub(crate) bytes: Option<Bytes>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Answer {
    pub(crate) id: u64,
    pub(crate) schema: u64,
    pub(crate) body: Answered,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum Answered {
    Hello {
        version: String,
        schema: u64,
    },
    Cargo(Reply),
    /// The root of the worker's sources after the transaction, its hash, and what its build
    /// said when one was asked for: an `ok` line, or why it failed.
    Applied {
        root: Bytes,
        built: Option<String>,
    },
    /// Why the call was not answered.
    Refused(String),
}

impl Daemon {
    /// Answers the calls of a framed connection, each on a thread of its own.
    pub(super) fn answer_frames(&self, stream: UnixStream) -> anyhow::Result<()> {
        // A connection stays open between calls.
        stream.set_read_timeout(None)?;
        let writer = Mutex::new(stream.try_clone()?);
        let mut reader = stream;
        let ending = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| -> anyhow::Result<()> {
            while let Some(message) = frame::read(&mut reader)? {
                let (writer, ending) = (&writer, &ending);
                scope.spawn(move || {
                    let (answer, ends) = self.call(&message);
                    let bytes = blaze_api::to_vec(&answer).unwrap_or_else(|why| {
                        let refused =
                            Answered::Refused(format!("the answer can't be written: {why}"));
                        let answer = Answer { id: answer.id, schema: SCHEMA, body: refused };
                        blaze_api::to_vec(&answer).unwrap_or_default()
                    });
                    let sent = frame::write(&mut *lock(writer), &bytes);
                    if let Err(why) = sent {
                        eprintln!("an answer is not sent: {why}");
                    }
                    ending.fetch_or(ends, std::sync::atomic::Ordering::SeqCst);
                });
            }
            Ok(())
        })?;
        if ending.load(std::sync::atomic::Ordering::SeqCst) {
            self.end();
        }
        Ok(())
    }

    /// The answer to one call's bytes, and whether the daemon ends once it is sent.
    fn call(&self, message: &[u8]) -> (Answer, bool) {
        let began = Instant::now();
        let call = match blaze_api::from_slice::<Call>(message) {
            Ok(call) => call,
            Err(why) => {
                return (
                    Answer { id: 0, schema: SCHEMA, body: Answered::Refused(why.to_string()) },
                    false,
                );
            }
        };
        let answer = |body| Answer { id: call.id, schema: SCHEMA, body };
        if call.schema != SCHEMA {
            let why = format!("a call of schema {}, and this daemon speaks {SCHEMA}", call.schema);
            return (answer(Answered::Refused(why)), false);
        }
        *lock(&self.asked) = Instant::now();
        match call.body {
            Body::Hello => {
                (answer(Answered::Hello { version: self.version.clone(), schema: SCHEMA }), false)
            }
            Body::Apply(apply) => {
                let body =
                    self.apply(&apply).unwrap_or_else(|why| Answered::Refused(format!("{why:#}")));
                (answer(body), false)
            }
            Body::Cargo(request) => {
                let other = call.version.as_ref().is_some_and(|it| *it != self.version);
                let read = Instant::now();
                let mut reply = match other {
                    true => Reply::Restart { version: self.version.clone() },
                    false => self.handle(request),
                };
                if let Reply::Synced { spans, .. } = &mut reply {
                    let level = Some(crate::trace::DAEMON);
                    spans.push(crate::trace::span("daemon read request", level, began, read));
                    spans.push(crate::trace::span("daemon", level, read, Instant::now()));
                }
                let ends = other && call.replaces || matches!(reply, Reply::Stopping);
                (answer(Answered::Cargo(reply)), ends)
            }
        }
    }

    /// Hands a transaction to the worker that has the workspace of its files loaded.
    fn apply(&self, apply: &Apply) -> anyhow::Result<Answered> {
        let first = apply.files.first().context("a transaction of no files")?;
        let worker = self.worker_of(Path::new(&first.path))?;
        let mut line = String::from("apply\t");
        line.push_str(match (apply.build, apply.disk) {
            (true, true) => "bd",
            (true, false) => "b",
            (false, true) => "d",
            (false, false) => "-",
        });
        for file in &apply.files {
            let path = Path::new(&file.path);
            anyhow::ensure!(path.is_absolute(), "{} is not an absolute path", file.path);
            let other = self.worker_of(path)?;
            anyhow::ensure!(
                Arc::ptr_eq(&other, &worker),
                "{} is in another workspace than {}",
                file.path,
                first.path
            );
            line.push('\t');
            ra_jit_live::format::escape(&file.path, &mut line);
            match &file.bytes {
                Some(bytes) => {
                    let text = std::str::from_utf8(&bytes.0)
                        .with_context(|| format!("{} is not UTF-8", file.path))?;
                    line.push_str("\tw");
                    ra_jit_live::format::escape(text, &mut line);
                }
                None => line.push_str("\td"),
            }
        }
        *lock(&worker.asked) = Instant::now();
        let mut stream = socket::connect(&worker.socket)?;
        stream.write_all(format!("{line}\n\n").as_bytes())?;
        let mut lines = BufReader::new(stream).lines();
        let mut next = || -> anyhow::Result<String> {
            match lines.next() {
                Some(line) => Ok(crate::trace::without_spans(&line?).to_owned()),
                None => anyhow::bail!("the worker hung up; its log is {}", worker.log.display()),
            }
        };
        let said = next()?;
        let Some(root) = said.strip_prefix("root ") else {
            anyhow::bail!("{}", said.strip_prefix("error: ").unwrap_or(&said));
        };
        let root = Bytes(hex(root).with_context(|| format!("the worker said {said:?}"))?);
        let built = next()?;
        let built = (built != "ok applied").then_some(built);
        Ok(Answered::Applied { root, built })
    }

    /// The worker that has a checkout of `path` loaded. A file to make is told by its
    /// directory.
    pub(super) fn worker_of(&self, path: &Path) -> anyhow::Result<Arc<super::Worker>> {
        let path = dunce::canonicalize(path).unwrap_or_else(|_| {
            match (path.parent().and_then(|it| dunce::canonicalize(it).ok()), path.file_name()) {
                (Some(parent), Some(name)) => parent.join(name),
                _ => path.to_owned(),
            }
        });
        let worker = lock(&self.workers).values().find_map(|worker| {
            let checkouts = lock(&worker.checkouts);
            let has = checkouts.values().any(|it| {
                matches!(it.warmth, Warmth::Ready)
                    && dunce::canonicalize(&it.root).is_ok_and(|root| path.starts_with(root))
            });
            has.then(|| worker.clone())
        });
        worker.with_context(|| {
            format!("the daemon has no workspace loaded that has {}", path.display())
        })
    }
}

fn hex(text: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| (c as char).to_digit(16).map(|it| it as u8);
    let text = text.as_bytes();
    if text.len() % 2 != 0 {
        return None;
    }
    text.chunks(2).map(|it| Some(digit(it[0])? << 4 | digit(it[1])?)).collect()
}

/// Serves calls over HTTP at `RA_JIT_HTTP`, when it names an address of the loopback, until
/// the daemon ends.
pub(super) fn serve_http(daemon: Arc<Daemon>) -> anyhow::Result<()> {
    let Some(address) = std::env::var("RA_JIT_HTTP").ok().filter(|it| !it.is_empty()) else {
        return Ok(());
    };
    let listener = TcpListener::bind(&address).with_context(|| format!("RA_JIT_HTTP={address}"))?;
    let bound = listener.local_addr()?;
    anyhow::ensure!(bound.ip().is_loopback(), "RA_JIT_HTTP={address} is not of the loopback");
    let token = token()?;
    let path = token_in(&daemon.home);
    let mut file =
        private_file(std::fs::OpenOptions::new().create(true).write(true).truncate(true), &path)?;
    file.write_all(token.as_bytes())?;
    eprintln!("daemon takes calls over HTTP at http://{bound}/v1 (token in {})", path.display());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (daemon, token) = (daemon.clone(), token.clone());
            std::thread::spawn(move || {
                if let Err(why) = daemon.answer_http(stream, &token) {
                    eprintln!("an HTTP call is not answered: {why:#}");
                }
            });
        }
    });
    Ok(())
}

/// 32 bytes from the system's random source, in hex.
fn token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|it| format!("{it:02x}")).collect())
}

/// The most an HTTP call's head may take.
const LONGEST_HEAD: u64 = 64 << 10;

impl Daemon {
    /// One call per request: `POST /v1` with the call as the body (`application/cbor`); the
    /// answer is CBOR, or JSON for `Accept: application/json`.
    fn answer_http(&self, mut stream: TcpStream, token: &str) -> anyhow::Result<()> {
        stream.set_read_timeout(Some(socket::REQUEST_TIMEOUT))?;
        let mut reader = BufReader::new(&stream);
        let (mut length, mut json, mut authorized) = (0usize, false, false);
        let mut first = String::new();
        let mut read = reader.read_line(&mut first)?;
        loop {
            let mut header = String::new();
            read += reader.read_line(&mut header)?;
            anyhow::ensure!(read as u64 <= LONGEST_HEAD, "a head longer than {LONGEST_HEAD} bytes");
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else { continue };
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.parse().context("a Content-Length")?,
                "accept" => json = value.contains("application/json"),
                "authorization" => authorized = value.strip_prefix("Bearer ") == Some(token),
                _ => {}
            }
        }
        let respond = |stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]| {
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(body)
        };
        let mut parts = first.split_whitespace();
        if (parts.next(), parts.next()) != (Some("POST"), Some("/v1")) {
            return Ok(respond(&mut stream, "404 Not Found", "text/plain", b"POST /v1\n")?);
        }
        if !authorized {
            let why = format!(
                "send Authorization: Bearer <the token in {}>\n",
                token_in(&self.home).display()
            );
            return Ok(respond(&mut stream, "401 Unauthorized", "text/plain", why.as_bytes())?);
        }
        if length > frame::LONGEST {
            return Ok(respond(
                &mut stream,
                "413 Content Too Large",
                "text/plain",
                b"at most 16 MiB\n",
            )?);
        }
        let mut message = vec![0; length];
        reader.read_exact(&mut message)?;
        stream.set_read_timeout(None)?;
        let (answer, ends) = self.call(&message);
        let value = blaze_api::to_value(&answer)?;
        match json {
            true => respond(
                &mut stream,
                "200 OK",
                "application/json",
                blaze_api::json::render(&value).as_bytes(),
            )?,
            false => respond(&mut stream, "200 OK", "application/cbor", &value.encode())?,
        }
        if ends {
            self.end();
        }
        Ok(())
    }
}

/// `cargo ra-jit apply`: the files of its arguments as one transaction of the daemon's,
/// through the socket as an agent calls it. The exit status is 1 when the daemon refused
/// the transaction or the build it asked for failed.
pub(crate) fn run(args: &[String]) -> anyhow::Result<i32> {
    let mut apply = Apply { files: Vec::new(), build: false, disk: true };
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--build" => apply.build = true,
            "--memory" => apply.disk = false,
            "--json" => json = true,
            flag if flag.starts_with('-') => anyhow::bail!("`apply` takes no {flag}"),
            file => {
                let (path, from) = match file.split_once('=') {
                    Some((path, from)) => (path, Some(from)),
                    None => (file, None),
                };
                let path = std::path::absolute(path)?;
                let bytes = match from {
                    Some("") => None,
                    Some(from) => {
                        Some(std::fs::read(from).with_context(|| format!("reading {from}"))?)
                    }
                    None => Some(
                        std::fs::read(&path)
                            .with_context(|| format!("reading {}", path.display()))?,
                    ),
                };
                let path = path
                    .into_os_string()
                    .into_string()
                    .map_err(|it| anyhow::anyhow!("{} is not UTF-8", Path::new(&it).display()))?;
                apply.files.push(File { path, bytes: bytes.map(Bytes) });
            }
        }
    }
    anyhow::ensure!(
        !apply.files.is_empty(),
        "usage: cargo ra-jit apply [--build] [--memory] [--json] PATH[=FROM]..."
    );
    let home = super::home()?;
    anyhow::ensure!(
        marker_in(&home).exists(),
        "the daemon takes no calls: none is running, or it is of a version before them (a build starts one)"
    );
    let mut stream = socket::connect(&super::socket_in(&home)).context("no daemon is running")?;
    let call =
        Call { id: 1, schema: SCHEMA, version: None, replaces: false, body: Body::Apply(apply) };
    frame::write(&mut stream, &blaze_api::to_vec(&call)?)?;
    let message =
        frame::read(&mut stream)?.context("the daemon ended before it answered (killed?)")?;
    if json {
        println!("{}", blaze_api::json::render(&blaze_api::Value::decode(&message)?));
    }
    let answer: Answer = blaze_api::from_slice(&message)?;
    let failed = match &answer.body {
        Answered::Applied { root, built } => {
            if !json {
                let root: String = root.0.iter().map(|it| format!("{it:02x}")).collect();
                println!("root {root}");
                if let Some(built) = built {
                    println!("{built}");
                }
            }
            built.as_ref().is_some_and(|it| !it.starts_with("ok"))
        }
        Answered::Refused(why) => {
            if !json {
                eprintln!("error: {why}");
            }
            true
        }
        other => anyhow::bail!("the daemon answered {other:?}"),
    };
    Ok(i32::from(failed))
}
