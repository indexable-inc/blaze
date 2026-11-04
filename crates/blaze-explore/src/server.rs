//! The server: one WebTransport endpoint on `127.0.0.1` at a port the OS picks, on a thread of
//! its own with a current-thread tokio runtime, so the daemon's threads never wait on it.
//!
//! A session is refused before it opens unless its CONNECT is to [`link::PATH`] (404) from an
//! allowed Origin (403). Opened, its first request must be `hello` with the token: a wrong
//! token is answered `bad_token` and the session closed with code [`CLOSE_BAD_TOKEN`];
//! anything else before it is answered `unauthenticated`. After hello the server opens the
//! live stream and answers each bidirectional stream on its own, at most [`MAX_IN_FLIGHT`] at
//! once (one more is answered `overloaded`). A request the page stops (`STOP_SENDING`, a
//! reset) is dropped where it waits and its DuckDB process killed (`sql::run_with`); the rest
//! of its work on the blocking pool (the tiles, Arrow encoding) ends with that and is thrown
//! away.
//!
//! The transport is blaze-http's HTTP/3 core (`blaze_http::h3`) under its tokio driver, which
//! owns the UDP socket and the timers.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_http::h3::{
    self, Identity, ServerConfig, Transport,
    tokio_driver::{Driver, RecvStream, Request as Incoming, SendStream, Session},
};
use blaze_profile::{clock, live, schema};
use serde_json::{Map, Value};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::MissedTickBehavior,
};

use crate::{
    cert::{Cert, CertError, Rotation},
    frame,
    link::{self, Origins, Token},
    ops::{self, Context, History},
    wire::{self, Kind, Request, WireError},
};

/// How often the wall clock is asked whether the certificate is due.
pub const CHECK: Duration = Duration::from_secs(600);
/// Requests one session may have in flight.
pub const MAX_IN_FLIGHT: usize = 16;
/// The longest request: SQL a person or agent wrote, never data.
pub const MAX_REQUEST: usize = 1 << 20;
/// How long a request may take to arrive whole.
pub const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// The code a session is closed with after `bad_token`.
pub const CLOSE_BAD_TOKEN: u32 = 1;
/// The code every session is closed with when the explorer stops.
pub const CLOSE_STOPPED: u32 = 0;
/// How long dropping an [`Explorer`] waits for the blocking pool's work (a `tiles` sweep, a
/// Flecs answer) before leaving it to finish on its own; DuckDB is killed at once.
pub const SHUTDOWN: Duration = Duration::from_secs(1);
const KEEP_ALIVE: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(30);

/// What the explorer serves.
pub struct Config {
    pub history: History,
    /// Flecs's world of the builds, for `world`; `None` answers `no_world`.
    pub world: Option<Arc<blaze_web::Graphs>>,
    /// Accept sessions from [`link::DEV_ORIGIN`] too.
    pub dev_origin: bool,
    /// The daemon's version, as `hello` says it.
    pub daemon: &'static str,
}

/// The certificate being served, as sessions are told it.
#[derive(Debug, PartialEq, Eq)]
struct Current {
    hash: String,
    not_after_s: u64,
}

struct Shared {
    token: Token,
    origins: Origins,
    port: u16,
    daemon: &'static str,
    cx: Arc<Context>,
    cert: watch::Sender<Arc<Current>>,
}

/// A running explorer; dropping it closes every session and stops the server, waiting at most
/// [`SHUTDOWN`] for work under way.
pub struct Explorer {
    shared: Arc<Shared>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Explorer {
    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// The SHA-256 of the certificate served now, as the link carries it.
    pub fn certhash(&self) -> String {
        // clone: the hash out of the watch's lock, once a link
        self.shared.cert.borrow().hash.clone()
    }

    /// The link to print and open: it changes when the certificate is made anew.
    pub fn link(&self) -> String {
        link::link(self.shared.port, &self.shared.token, &self.shared.cert.borrow().hash)
    }
}

impl Drop for Explorer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            _ = thread.join();
        }
    }
}

/// Why the explorer did not start.
#[derive(Debug)]
pub enum StartError {
    Clock(std::time::SystemTimeError),
    Cert(CertError),
    Runtime(std::io::Error),
    Bind(std::io::Error),
    Thread(std::io::Error),
    Config(h3::Error),
    /// jank(2026-11-04): this blaze is built without the explorer's HTTP/3 transport; see
    /// [`start`]. Remove with it.
    Unbuilt,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Clock(why) => {
                write!(f, "the wall clock is before 1970, so no certificate can be dated: {why}")
            }
            StartError::Cert(why) => write!(f, "{why}"),
            StartError::Runtime(why) => {
                write!(f, "the explorer's tokio runtime did not start: {why}")
            }
            StartError::Bind(why) => {
                write!(f, "the explorer could not listen on 127.0.0.1 (UDP): {why}")
            }
            StartError::Thread(why) => write!(f, "the explorer's thread did not start: {why}"),
            StartError::Unbuilt => write!(
                f,
                "this blaze has no explorer: its HTTP/3 transport (blaze-http on quinn-proto) is left out until the next stage-0 promotion, as stage 0 0.20261030.1 cannot compile quinn-proto"
            ),
            StartError::Config(why) => {
                write!(f, "the explorer's HTTP/3 endpoint refused its config: {why}")
            }
        }
    }
}

impl std::error::Error for StartError {}

/// Starts an explorer; it serves until the [`Explorer`] is dropped.
///
/// jank(2026-11-04): refused with [`StartError::Unbuilt`], so that nothing blaze-engine runs
/// reaches quinn-proto, which stage 0 0.20261030.1 cannot compile (`quinn_proto::Connection`'s
/// methods: layout of {type error}), refusing the whole of blaze-engine. [`start_h3`] is the real
/// start. Undo (rename `start_h3` back to `start`, drop `Unbuilt`) after the next stage-0
/// promotion.
pub fn start(_config: Config) -> Result<Explorer, StartError> {
    Err(StartError::Unbuilt)
}

/// The explorer's real start ([`start`]): it serves until the [`Explorer`] is dropped.
#[allow(dead_code, reason = "jank(2026-11-04): unreached until `start` calls it again")]
pub(crate) fn start_h3(config: Config) -> Result<Explorer, StartError> {
    let rotation = Rotation::new(unix_now_s()?);
    let cert = Cert::make(rotation.current()).map_err(StartError::Cert)?;
    let identity = cert.identity().map_err(StartError::Cert)?;
    let (ready_to, ready) = crossbeam_channel::unbounded::<Result<Arc<Shared>, StartError>>();
    let (stop, stopped) = oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("blaze-explore".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(it) => it,
                Err(why) => {
                    _ = ready_to.send(Err(StartError::Runtime(why)));
                    return;
                }
            };
            runtime.block_on(async move {
                let bound = std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                    .map_err(StartError::Bind)
                    .and_then(|socket| {
                        let runtime = tokio::runtime::Handle::current();
                        Driver::spawn(&runtime, socket, Some(server_config(identity)), None)
                            .map_err(StartError::Config)
                    })
                    .map(|(driver, incoming)| {
                        let port = driver.local_addr().port();
                        ((driver, incoming), port)
                    });
                let (transport, port) = match bound {
                    Ok(it) => it,
                    Err(why) => {
                        _ = ready_to.send(Err(why));
                        return;
                    }
                };
                let current = Current { hash: cert.hash, not_after_s: cert.validity.not_after_s };
                let shared = Arc::new(Shared {
                    token: Token::random(),
                    origins: Origins { dev: config.dev_origin },
                    port,
                    daemon: config.daemon,
                    cx: Arc::new(Context { history: config.history, world: config.world }),
                    cert: watch::Sender::new(Arc::new(current)),
                });
                if ready_to.send(Ok(Arc::clone(&shared))).is_err() {
                    return;
                }
                serve(transport, shared, rotation, stopped).await;
            });
            runtime.shutdown_timeout(SHUTDOWN);
        })
        .map_err(StartError::Thread)?;
    let started = ready.recv();
    match started {
        Ok(Ok(shared)) => Ok(Explorer { shared, stop: Some(stop), thread: Some(thread) }),
        Ok(Err(why)) => {
            _ = thread.join();
            Err(why)
        }
        Err(_) => {
            _ = thread.join();
            Err(StartError::Thread(std::io::Error::other(
                "the explorer's thread ended before it listened",
            )))
        }
    }
}

fn unix_now_s() -> Result<u64, StartError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH).map_err(StartError::Clock)?.as_secs())
}

fn server_config(identity: Identity) -> ServerConfig {
    ServerConfig { identity, transport: Transport { idle: IDLE, keep_alive: Some(KEEP_ALIVE) } }
}

async fn serve(
    (driver, mut incoming): (Driver, mpsc::UnboundedReceiver<Incoming>),
    shared: Arc<Shared>,
    mut rotation: Rotation,
    mut stop: oneshot::Receiver<()>,
) {
    let mut check = tokio::time::interval(CHECK);
    check.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            request = incoming.recv() => match request {
                Some(request) => {
                    tokio::spawn(session(Arc::clone(&shared), request));
                }
                None => break,
            },
            _ = check.tick() => rotate(&driver, &shared, &mut rotation).await,
            _ = &mut stop => break,
        }
    }
    // Each session is told the code; a page that does not acknowledge in LINGER is closed
    // anyway, and SHUTDOWN bounds the whole of it.
    _ = tokio::time::timeout(SHUTDOWN, driver.close(CLOSE_STOPPED, "blaze explore stopped")).await;
}

/// Makes and serves a new certificate when [`Rotation`] says it is due. One not made is said
/// on the daemon's log and tried again at the next check; the one served keeps being served
/// until then, and until it expires.
async fn rotate(driver: &Driver, shared: &Shared, rotation: &mut Rotation) {
    let now = match unix_now_s() {
        Ok(it) => it,
        Err(why) => {
            eprintln!("blaze explore: the certificate is not checked: {why}");
            return;
        }
    };
    let Some(validity) = rotation.due(now) else { return };
    let served = match Cert::make(validity).and_then(|cert| Ok((cert.identity()?, cert))) {
        Ok((identity, cert)) => {
            driver.set_identity(identity).await.map(|()| cert).map_err(StartError::Config)
        }
        Err(why) => Err(StartError::Cert(why)),
    };
    match served {
        Ok(cert) => {
            rotation.rotated(validity);
            shared.cert.send_replace(Arc::new(Current {
                hash: cert.hash,
                not_after_s: validity.not_after_s,
            }));
        }
        Err(why) => eprintln!(
            "blaze explore: the certificate was not made anew ({why}); the one served expires at {} (Unix \
             seconds) and a new one is tried in {} minutes",
            rotation.current().not_after_s,
            CHECK.as_secs() / 60
        ),
    }
}

async fn session(shared: Arc<Shared>, request: Incoming) {
    if request.path() != link::PATH {
        request.refuse(404);
        return;
    }
    if !shared.origins.allows(request.origin()) {
        request.refuse(403);
        return;
    }
    let Ok(connection) = request.accept().await else { return };
    if !hello(&shared, &connection).await {
        return;
    }
    // clone: a handle (an `Arc`) of the session's connection
    let live = tokio::spawn(live(connection.clone(), shared.cert.subscribe()));
    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    while let Ok((send, recv)) = connection.accept_bi().await {
        match Arc::clone(&in_flight).try_acquire_owned() {
            Ok(permit) => {
                // clone: a handle (an `Arc`) of the session's connection
                tokio::spawn(stream(Arc::clone(&shared), connection.clone(), send, recv, permit));
            }
            Err(_) => {
                tokio::spawn(refuse(send, recv));
            }
        }
    }
    live.abort();
}

/// Answers the session's requests, one at a time, until a good `hello`; `false` when the
/// session is over.
async fn hello(shared: &Shared, connection: &Session) -> bool {
    loop {
        let Ok((mut send, mut recv)) = connection.accept_bi().await else { return false };
        let refused = match read(&mut recv).await {
            Ok(Request::Hello { token }) => {
                return said_hello(shared, connection, &mut send, &token).await;
            }
            Ok(_) => WireError::new(
                Kind::Unauthenticated,
                "the first request of a session is hello with the link's token",
            ),
            Err(why) => why,
        };
        send_bytes(&mut send, &error_bytes(&refused)).await;
    }
}

/// Answers `hello` with `token`: `true` when it was the token.
async fn said_hello(
    shared: &Shared,
    connection: &Session,
    send: &mut SendStream,
    token: &str,
) -> bool {
    if shared.token.matches(token) {
        send_bytes(send, &hello_bytes(shared)).await;
        return true;
    }
    let refused = WireError::new(
        Kind::BadToken,
        "the token is not this daemon's: run `blaze explore` for its link",
    );
    send_bytes(send, &error_bytes(&refused)).await;
    connection.close(CLOSE_BAD_TOKEN, "bad_token");
    false
}

fn hello_bytes(shared: &Shared) -> Vec<u8> {
    let current = Arc::clone(&shared.cert.borrow());
    let header = [
        ("op", Value::from("hello")),
        ("daemon", shared.daemon.into()),
        ("schema", schema::VERSION.into()),
        ("clock", schema::CLOCK.into()),
        ("now_ns", clock::mono_ns().into()),
        ("certhash", current.hash.as_str().into()),
        ("cert_not_after_unix_s", current.not_after_s.into()),
        ("live_every_ms", (live::EVERY.as_millis() as u64).into()),
        ("source", source_descriptor()),
    ];
    // alloc: once a session
    let header: Map<String, Value> =
        header.into_iter().map(|(key, value)| (key.to_owned(), value)).collect();
    let mut out = Vec::new();
    encode_head(&mut out, &header);
    out
}

/// What kind of profile source this server is, for the page's generic views
/// (docs/v2/plan/lanes/observe/explore.md, "One explorer, many sources"): the blaze daemon,
/// whose builds the generic views read through `spans` and `sql`, and which alone has the
/// `blaze` extension (builds, SQL, views, the build world). A page that reads the field shows the
/// extension's views only when it is listed.
fn source_descriptor() -> Value {
    let mut source = Map::new();
    // alloc: once a session
    source.insert("kind".to_owned(), "blaze".into());
    // alloc: once a session
    source.insert("extensions".to_owned(), Value::Array(vec![Value::from("blaze")]));
    Value::Object(source)
}

/// A request's stream: read, answered on the blocking pool unless the page stops it, written.
async fn stream(
    shared: Arc<Shared>,
    connection: Session,
    mut send: SendStream,
    mut recv: RecvStream,
    _permit: OwnedSemaphorePermit,
) {
    let started = Instant::now();
    let request = match read(&mut recv).await {
        Ok(Request::Hello { token }) => {
            said_hello(&shared, &connection, &mut send, &token).await;
            return;
        }
        Ok(it) => it,
        Err(why) => {
            send_bytes(&mut send, &error_bytes(&why)).await;
            return;
        }
    };
    let cx = Arc::clone(&shared.cx);
    // Dropped as the stream ends however it ends (answered, stopped by the page, the session
    // gone, the server stopping): the sender gone kills a DuckDB query still under way.
    let (_stop, stopping) = crossbeam_channel::bounded::<()>(0);
    let work =
        tokio::task::spawn_blocking(move || encode(ops::answer(&cx, request, &stopping), started));
    let bytes = tokio::select! {
        done = work => match done {
            Ok(bytes) => bytes,
            Err(why) => error_bytes(&WireError::new(Kind::Encode, format!("the answer's work did not end: {why}"))),
        },
        _ = send.stopped() => return,
    };
    send_bytes(&mut send, &bytes).await;
}

async fn refuse(mut send: SendStream, mut recv: RecvStream) {
    recv.stop(0);
    let message =
        format!("the session has {MAX_IN_FLIGHT} requests in flight: wait for one to end");
    let refused = WireError::new(Kind::Overloaded, message).with("in_flight", MAX_IN_FLIGHT);
    send_bytes(&mut send, &error_bytes(&refused)).await;
}

/// The request on `recv`: at most [`MAX_REQUEST`] bytes, whole within [`REQUEST_WAIT`].
async fn read(recv: &mut RecvStream) -> Result<Request, WireError> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let reading = async {
        loop {
            match recv.read(&mut chunk).await {
                Ok(Some(n)) if bytes.len() + n > MAX_REQUEST => {
                    let message = format!("the request is over {MAX_REQUEST} bytes");
                    return Err(WireError::new(Kind::BadArgs, message));
                }
                // alloc: a request, at most MAX_REQUEST
                Ok(Some(n)) => bytes.extend_from_slice(&chunk[..n]),
                Ok(None) => return Ok(()),
                Err(why) => {
                    return Err(WireError::new(
                        Kind::BadArgs,
                        format!("the request was not read whole: {why}"),
                    ));
                }
            }
        }
    };
    match tokio::time::timeout(REQUEST_WAIT, reading).await {
        Ok(Ok(())) => wire::parse(&bytes),
        Ok(Err(why)) => Err(why),
        Err(_) => Err(WireError::new(
            Kind::BadArgs,
            format!(
                "no whole request in {} s: write one JSON object, then close the stream's side",
                REQUEST_WAIT.as_secs()
            ),
        )),
    }
}

async fn send_bytes(send: &mut SendStream, bytes: &[u8]) {
    if send.write_all(bytes).await.is_ok() {
        send.finish();
    }
}

/// An answer as the wire carries it: its header, `rows` and `elapsed_ns` added, then its
/// table's IPC; or the error it became.
fn encode(answer: Result<ops::Answer, WireError>, started: Instant) -> Vec<u8> {
    let mut out = Vec::new();
    let encoded = answer.and_then(|mut answer| {
        if let Some(table) = &answer.table {
            // alloc: once an answer
            answer.header.insert("rows".to_owned(), table.rows().into());
        }
        // alloc: once an answer
        answer.header.insert("elapsed_ns".to_owned(), (started.elapsed().as_nanos() as u64).into());
        encode_head(&mut out, &answer.header);
        if let Some(table) = &answer.table {
            frame::ipc(&mut out, &table.schema, &table.batches).map_err(|why| {
                WireError::new(Kind::Encode, format!("the answer as Arrow IPC: {why}"))
            })?;
        }
        Ok(())
    });
    if let Err(why) = encoded {
        out.clear();
        encode_head(&mut out, &why.header());
    }
    out
}

fn error_bytes(error: &WireError) -> Vec<u8> {
    let mut out = Vec::new();
    encode_head(&mut out, &error.header());
    out
}

/// [`frame::head`] of a header that is a JSON map of strings to JSON values, which
/// serde_json always writes.
fn encode_head(out: &mut Vec<u8>, header: &Map<String, Value>) {
    frame::head(out, header).expect("a JSON map always serializes");
}

/// The live stream: every [`live::EVERY`], a frame of each table with rows, read from this
/// session's own place in the rings (and an `open` frame of no rows when the open set empties);
/// and a `cert` frame when the certificate is made anew.
///
/// A page that reads slower than the frames come falls behind by at most one frame: while a
/// write waits on the page, ticks are skipped, not queued, and the next read takes everything
/// since this session's last one. What the rings dropped meanwhile is counted in `skipped`.
async fn live(connection: Session, mut cert: watch::Receiver<Arc<Current>>) {
    let Ok(mut send) = connection.open_uni().await else { return };
    let mut every = tokio::time::interval(live::EVERY);
    every.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // alloc: once a session, the feed moved to and from the blocking pool each tick
    let mut state = Box::new((live::Feed::new(), live::Frame::default()));
    let mut out = Vec::new();
    let mut open_shown = false;
    loop {
        tokio::select! {
            _ = every.tick() => {
                let read = tokio::task::spawn_blocking(move || {
                    let (feed, frame) = &mut *state;
                    let read = feed.next(frame);
                    (state, read)
                });
                let Ok((back, read)) = read.await else { return };
                state = back;
                let frame = &state.1;
                let written = match read {
                    Ok(()) => live_tables(&mut send, &mut out, frame, &mut open_shown).await,
                    Err(why) => {
                        let error = WireError::new(Kind::Encode, format!("the live frame: {why}"));
                        let mut header = error.header();
                        // alloc: an error path
                        header.insert("op".to_owned(), "live".into());
                        live_frame(&mut send, &mut out, &header, &[]).await
                    }
                };
                if !written {
                    return;
                }
            }
            changed = cert.changed() => {
                if changed.is_err() {
                    return;
                }
                let current = Arc::clone(&cert.borrow_and_update());
                let mut header = Map::new();
                // alloc: every 12 days
                header.insert("op".to_owned(), "cert".into());
                header.insert("certhash".to_owned(), current.hash.as_str().into());
                header.insert("not_after_unix_s".to_owned(), current.not_after_s.into());
                if !live_frame(&mut send, &mut out, &header, &[]).await {
                    return;
                }
            }
        }
    }
}

/// What a tick sends of one table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Emit {
    Nothing,
    /// A frame with its rows and IPC.
    Rows,
    /// A frame of `rows: 0` and no IPC: the `open` set the page shows emptied. `open` replaces
    /// the last set, so the page needs the empty one once; `spans` and `counters` append, so
    /// they never need it.
    Cleared,
}

fn sends(table: &str, rows: usize, open_shown: bool) -> Emit {
    match (rows, table) {
        (0, "open") if open_shown => Emit::Cleared,
        (0, _) => Emit::Nothing,
        _ => Emit::Rows,
    }
}

/// Writes a frame of each of `frame`'s tables that has rows, and of `open` once when it
/// empties (`open_shown`: the page shows a non-empty set); `false` when the page is gone.
async fn live_tables(
    send: &mut SendStream,
    out: &mut Vec<u8>,
    frame: &live::Frame,
    open_shown: &mut bool,
) -> bool {
    let tables = [
        ("spans", frame.spans_rows, &frame.spans),
        ("counters", frame.counters_rows, &frame.counters),
        ("open", frame.open_rows, &frame.open),
    ];
    for (table, rows, ipc) in tables {
        let ipc: &[u8] = match sends(table, rows, *open_shown) {
            Emit::Nothing => continue,
            Emit::Rows => ipc,
            Emit::Cleared => &[],
        };
        let header = [
            ("op", Value::from("live")),
            ("table", table.into()),
            ("rows", rows.into()),
            ("at_ns", frame.at_ns.into()),
            ("skipped", frame.skipped.into()),
            ("lost", frame.lost.into()),
        ];
        // alloc: a live frame's header, at most 12 a second
        let header: Map<String, Value> =
            header.into_iter().map(|(key, value)| (key.to_owned(), value)).collect();
        if !live_frame(send, out, &header, ipc).await {
            return false;
        }
        if table == "open" {
            *open_shown = rows > 0;
        }
    }
    true
}

async fn live_frame(
    send: &mut SendStream,
    out: &mut Vec<u8>,
    header: &Map<String, Value>,
    ipc: &[u8],
) -> bool {
    out.clear();
    frame::live_head(out, header, ipc.len()).expect("a JSON map always serializes");
    send.write_all(out).await.is_ok() && send.write_all(ipc).await.is_ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use base64::Engine as _;
    use blaze_http::h3::{ClientConfig, Closed};

    use super::*;

    fn explorer() -> Explorer {
        let history = History { duckdb: None, dir: PathBuf::from("/nowhere") };
        start_h3(Config { history, world: None, dev_origin: false, daemon: "test" }).unwrap()
    }

    /// A client pinning the explorer's certificate, as the page does; on the runtime it is
    /// called on.
    fn client(explorer: &Explorer) -> Driver {
        let raw =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(explorer.certhash()).unwrap();
        let config =
            ClientConfig { pins: vec![raw.try_into().unwrap()], transport: Transport::default() };
        let socket = std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        Driver::spawn(&tokio::runtime::Handle::current(), socket, None, Some(config)).unwrap().0
    }

    async fn connect(
        client: &Driver,
        explorer: &Explorer,
        path: &str,
        origin: Option<&str>,
    ) -> Result<Session, Closed> {
        let url = format!("https://127.0.0.1:{}{path}", explorer.port());
        let to = SocketAddr::from((Ipv4Addr::LOCALHOST, explorer.port()));
        let headers: Vec<(&str, &str)> = origin.map(|it| ("origin", it)).into_iter().collect();
        client.connect(to, &url, &headers).await
    }

    /// The header and the IPC of the answer to `request`.
    async fn ask(connection: &Session, request: &str) -> (Value, Vec<u8>) {
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        send.write_all(request.as_bytes()).await.unwrap();
        send.finish();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Some(n) = recv.read(&mut chunk).await.unwrap() {
            bytes.extend_from_slice(&chunk[..n]);
        }
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!((4 + len) % frame::ALIGN, 0, "the IPC starts 8-aligned");
        let header: Value = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
        (header, bytes[4 + len..].to_vec())
    }

    fn token_of(link: &str) -> String {
        let fragment = link.split_once('#').unwrap().1;
        let parts: Vec<&str> = fragment.split('/').collect();
        assert_eq!(parts.len(), 3, "{link}");
        parts[1].to_owned()
    }

    #[test]
    fn a_session_is_refused_until_hello_then_answered_and_fed() {
        blaze_profile::plot(blaze_profile::Plot::Queued, 7);
        let explorer = explorer();
        let link = explorer.link();
        assert!(
            link.starts_with(&format!("https://ix.dev/explore#127.0.0.1:{}/", explorer.port())),
            "{link}"
        );
        assert!(link.ends_with(&format!("/{}", explorer.certhash())), "{link}");
        let token = token_of(&link);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let client = client(&explorer);
            for (path, origin, status) in [
                (link::PATH, Some("https://evil.example"), 403),
                (link::PATH, Some(link::DEV_ORIGIN), 403),
                (link::PATH, None, 403),
                ("/elsewhere", Some(link::ORIGIN), 404),
            ] {
                let refused = connect(&client, &explorer, path, origin).await.map(|it| it.id());
                assert_eq!(refused, Err(Closed::Refused { status }), "{path} {origin:?}");
            }

            let connection =
                connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).await.unwrap();
            let (header, ipc) = ask(&connection, r#"{"op":"views"}"#).await;
            assert_eq!(header["error"]["kind"], "unauthenticated", "{header}");
            assert!(ipc.is_empty());

            let (header, _) =
                ask(&connection, &format!(r#"{{"op":"hello","token":"{token}"}}"#)).await;
            assert_eq!(header["op"], "hello", "{header}");
            assert_eq!(header["schema"], schema::VERSION);
            assert_eq!(header["certhash"], explorer.certhash().as_str());
            assert_eq!(header["live_every_ms"], 250);
            assert_eq!(header["source"]["extensions"][0], "blaze", "{header}");

            let (header, ipc) = ask(&connection, r#"{"op":"views"}"#).await;
            assert_eq!(header["op"], "views", "{header}");
            assert_eq!(header["rows"], blaze_profile::views::VIEWS.len());
            let batches = live::decode(&ipc).unwrap();
            assert_eq!(
                batches.iter().map(|it| it.num_rows()).sum::<usize>(),
                blaze_profile::views::VIEWS.len()
            );

            let (header, _) = ask(&connection, r#"{"op":"frobnicate"}"#).await;
            assert_eq!(header["error"]["kind"], "unknown_op", "{header}");
            let (header, _) = ask(&connection, r#"{"op":"world","path":"/entity/flecs"}"#).await;
            assert_eq!(header["error"]["kind"], "no_world", "{header}");

            let mut live = connection.accept_uni().await.unwrap();
            let mut first = Vec::new();
            let mut chunk = [0u8; 4096];
            let read = async {
                loop {
                    let n = live.read(&mut chunk).await.unwrap().unwrap();
                    first.extend_from_slice(&chunk[..n]);
                    if first.len() >= 4
                        && first.len()
                            >= 4 + u32::from_le_bytes(first[..4].try_into().unwrap()) as usize
                    {
                        break;
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(5), read)
                .await
                .expect("a live frame within 5 s");
            let body_len = u32::from_le_bytes(first[..4].try_into().unwrap()) as usize;
            let body = &first[4..4 + body_len];
            let head_len = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
            assert_eq!(
                (4 + head_len) % frame::ALIGN,
                0,
                "a live frame's IPC is 8-aligned in its body"
            );
            let header: Value = serde_json::from_slice(&body[4..4 + head_len]).unwrap();
            assert_eq!(header["op"], "live", "{header}");
            assert!(header["rows"].as_u64().unwrap() > 0, "{header}");
            let batches = live::decode(&body[4 + head_len..]).unwrap();
            assert_eq!(
                batches.iter().map(|it| it.num_rows() as u64).sum::<u64>(),
                header["rows"].as_u64().unwrap()
            );

            let other = connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).await.unwrap();
            let wrong = if token.starts_with('0') {
                token.replacen('0', "1", 1)
            } else {
                format!("0{}", &token[1..])
            };
            let (header, _) = ask(&other, &format!(r#"{{"op":"hello","token":"{wrong}"}}"#)).await;
            assert_eq!(header["error"]["kind"], "bad_token", "{header}");
            tokio::time::timeout(Duration::from_secs(5), other.closed())
                .await
                .expect("bad_token closes the session");
            let (header, _) = ask(&connection, r#"{"op":"views"}"#).await;
            assert_eq!(header["op"], "views", "another session's bad token leaves this one open");
        });
    }

    #[test]
    fn a_session_from_the_dev_server_is_taken_only_when_asked() {
        let history = History { duckdb: None, dir: PathBuf::from("/nowhere") };
        let explorer =
            start_h3(Config { history, world: None, dev_origin: true, daemon: "test" }).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let client = client(&explorer);
            connect(&client, &explorer, link::PATH, Some(link::DEV_ORIGIN)).await.unwrap();
            connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).await.unwrap();
        });
    }

    #[test]
    fn an_emptied_open_set_is_sent_once_as_no_rows() {
        assert_eq!(sends("open", 3, false), Emit::Rows);
        assert_eq!(sends("open", 3, true), Emit::Rows);
        assert_eq!(sends("open", 0, true), Emit::Cleared, "the page clears what it shows");
        assert_eq!(sends("open", 0, false), Emit::Nothing, "an idle daemon sends nothing");
        for table in ["spans", "counters"] {
            assert_eq!(sends(table, 0, true), Emit::Nothing, "{table} appends: nothing to clear");
            assert_eq!(sends(table, 5, false), Emit::Rows);
        }
    }

    #[test]
    fn an_answer_that_fails_to_encode_becomes_its_error() {
        let bytes = encode(Err(WireError::new(Kind::Sql, "no")), Instant::now());
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 4 + len, "an error has no IPC");
        let header: Value = serde_json::from_slice(&bytes[4..]).unwrap();
        assert_eq!(header["error"]["kind"], "sql");
    }
}
