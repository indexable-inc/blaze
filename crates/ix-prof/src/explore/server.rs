//! The driver: one WebTransport endpoint on `127.0.0.1` at a port the OS picks, on a thread of
//! its own with a current-thread tokio runtime, so the program's threads never wait on it. What
//! a session answers is [`session`]'s; this module reads and writes the streams.
//!
//! A session is refused before it opens unless its CONNECT is to [`link::PATH`] (404) from an
//! allowed Origin (403). Opened, its requests are read and answered one at a time until a good
//! `hello` ([`session::Step`]); then the server opens the live stream and answers each
//! bidirectional stream on its own, at most [`session::MAX_IN_FLIGHT`] at once. A request the
//! page stops (`STOP_SENDING`, a reset) is dropped where it waits and its [`Stop`] set, so the
//! source's work on the blocking pool ends with it and is thrown away.
//!
//! The transport is blaze-http's HTTP/3 core (`blaze_http::h3`) under its tokio driver, which
//! owns the UDP socket and the timers.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blaze_http::h3::{
    self, Identity, ServerConfig, Transport,
    tokio_driver::{Driver, RecvStream, Request as Incoming, SendStream, Session},
};
use blaze_core::lock::Lock;
use serde_json::{Map, Value};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::MissedTickBehavior,
};

use super::{
    CoreWasm, Kind, OpError, Source, Stop, Tick,
    cert::{Cert, CertError, Rotation},
    frame,
    link::{self, Origins, Token},
    session::{self, Emit, Served, Slot, Step},
};
use crate::clock;

/// How often the wall clock is asked whether the certificate is due.
pub const CHECK: Duration = Duration::from_secs(600);
/// How long dropping an [`Explorer`] waits for the blocking pool's work (a source's answer)
/// before leaving it to finish on its own.
pub const SHUTDOWN: Duration = Duration::from_secs(1);
const KEEP_ALIVE: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(30);

/// What the explorer serves, and how it names itself.
pub struct Config {
    pub source: Arc<dyn Source>,
    /// Accept sessions from [`link::DEV_ORIGIN`] too.
    pub dev_origin: bool,
    /// The command that prints the server's link (`blaze explore`): `bad_token` tells the page
    /// to run it, and the server's log lines, its close reason, its certificate's common name
    /// and its thread (`blaze-explore`, the spaces as dashes) are named by it.
    pub name: &'static str,
    /// How often each session's live stream ticks, as `hello`'s `live_every_ms` says it.
    pub every: Duration,
    /// The explorer core this program serves the page (explore-core.md decision 3): `hello`
    /// names it and the `core` op answers its bytes. `Err` is why there is none (where the
    /// program looked and how to make one), which `hello` leaves out and `core` answers as
    /// `no_core`.
    pub core: Result<CoreWasm, String>,
}

struct Shared {
    token: Token,
    origins: Origins,
    port: u16,
    name: &'static str,
    every: Duration,
    source: Arc<dyn Source>,
    cert: watch::Sender<Arc<Served>>,
    core: Result<CoreWasm, String>,
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
            let _wait = tracing::info_span!(target: "blaze::ix_prof", "join the explorer's thread", wait = "thread", on = self.shared.name).entered();
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
            StartError::Config(why) => {
                write!(f, "the explorer's HTTP/3 endpoint refused its config: {why}")
            }
        }
    }
}

impl std::error::Error for StartError {}

/// Starts an explorer of `config.source`; it serves until the [`Explorer`] is dropped.
pub fn serve(config: Config) -> Result<Explorer, StartError> {
    let rotation = Rotation::new(unix_now_s()?);
    let cert = Cert::make(rotation.current(), config.name).map_err(StartError::Cert)?;
    let identity = cert.identity().map_err(StartError::Cert)?;
    let (ready_to, ready) = crossbeam_channel::unbounded::<Result<Arc<Shared>, StartError>>();
    let (stop, stopped) = oneshot::channel();
    let name = config.name;
    let thread = std::thread::Builder::new()
        // alloc: once a server, its thread's name
        .name(name.replace(' ', "-"))
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
                let served = Served { hash: cert.hash, not_after_s: cert.validity.not_after_s };
                let shared = Arc::new(Shared {
                    token: Token::random(),
                    origins: Origins { dev: config.dev_origin },
                    port,
                    name: config.name,
                    every: config.every,
                    source: config.source,
                    cert: watch::Sender::new(Arc::new(served)),
                    core: config.core,
                });
                if ready_to.send(Ok(Arc::clone(&shared))).is_err() {
                    return;
                }
                run(transport, shared, rotation, stopped).await;
            });
            runtime.shutdown_timeout(SHUTDOWN);
        })
        .map_err(StartError::Thread)?;
    let started = {
        let _wait = tracing::info_span!(target: "blaze::ix_prof", "wait for the explorer to listen", wait = "channel", on = name).entered();
        ready.recv()
    };
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

async fn run(
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
    let reason = format!("{} stopped", shared.name);
    _ = tokio::time::timeout(SHUTDOWN, driver.close(session::CLOSE_STOPPED, &reason)).await;
}

/// Makes and serves a new certificate when [`Rotation`] says it is due. One not made is said
/// on stderr and tried again at the next check; the one served keeps being served until then,
/// and until it expires.
async fn rotate(driver: &Driver, shared: &Shared, rotation: &mut Rotation) {
    let now = match unix_now_s() {
        Ok(it) => it,
        Err(why) => {
            eprintln!("{}: the certificate is not checked: {why}", shared.name);
            return;
        }
    };
    let Some(validity) = rotation.due(now) else { return };
    let made = Cert::make(validity, shared.name).and_then(|cert| Ok((cert.identity()?, cert)));
    let served = match made {
        Ok((identity, cert)) => {
            driver.set_identity(identity).await.map(|()| cert).map_err(StartError::Config)
        }
        Err(why) => Err(StartError::Cert(why)),
    };
    match served {
        Ok(cert) => {
            rotation.rotated(validity);
            let served = Served { hash: cert.hash, not_after_s: validity.not_after_s };
            shared.cert.send_replace(Arc::new(served));
        }
        Err(why) => eprintln!(
            "{}: the certificate was not made anew ({why}); the one served expires at {} (Unix \
             seconds) and a new one is tried in {} minutes",
            shared.name,
            rotation.current().not_after_s,
            CHECK.as_secs() / 60
        ),
    }
}

/// A session's core, shared by its streams: each locks it for one step.
type Core = Arc<Lock<session::Session>>;

/// A request's [`Slot`], given back to the core however the request's task ends.
struct Held {
    core: Core,
    /// `Some` until dropped.
    slot: Option<Slot>,
    started_ns: i64,
}

impl Held {
    fn new(core: &Core, slot: Slot) -> Held {
        // clone: a handle (an `Arc`) of the session's core
        Held { core: Arc::clone(core), started_ns: slot.started_ns(), slot: Some(slot) }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.core.lock().ended(slot);
        }
    }
}

/// The session's next request stream and its slot; `None` when the session is over. A stream
/// over the limit is refused on its own task and the next one taken.
async fn next_stream(connection: &Session, core: &Core) -> Option<(SendStream, RecvStream, Held)> {
    loop {
        let (send, recv) = connection.accept_bi().await.ok()?;
        let opened = core.lock().opened(clock::now_ns());
        match opened {
            Ok(slot) => return Some((send, recv, Held::new(core, slot))),
            Err(refused) => {
                tokio::spawn(refuse(send, recv, refused));
            }
        }
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
    let core: Core = Arc::new(Lock::new("ixprof.explore.core", session::Session::new(shared.name)));
    // Until a good hello, one request at a time.
    loop {
        let Some((mut send, mut recv, held)) = next_stream(&connection, &core).await else {
            return;
        };
        let step = step(&shared, &core, &mut recv).await;
        let first = answer(&shared, &connection, &mut send, step, held).await;
        if first {
            break;
        }
        if core.lock().phase() == session::Phase::Closed {
            return;
        }
    }
    // clone: a handle (an `Arc`) of the session's connection
    let feed = live(Arc::clone(&shared), connection.clone(), shared.cert.subscribe());
    let live = tokio::spawn(feed);
    while let Some((send, recv, held)) = next_stream(&connection, &core).await {
        let (shared, core) = (Arc::clone(&shared), Arc::clone(&core));
        // clone: a handle (an `Arc`) of the session's connection
        tokio::spawn(stream(shared, connection.clone(), core, send, recv, held));
    }
    live.abort();
}

/// A request's stream after hello: read, answered, written.
async fn stream(
    shared: Arc<Shared>,
    connection: Session,
    core: Core,
    mut send: SendStream,
    mut recv: RecvStream,
    held: Held,
) {
    let step = step(&shared, &core, &mut recv).await;
    answer(&shared, &connection, &mut send, step, held).await;
}

/// The core's step for the request on `recv`.
async fn step(shared: &Shared, core: &Core, recv: &mut RecvStream) -> Step {
    match read(recv).await {
        Ok(bytes) => core.lock().request(&shared.token, &bytes),
        Err(why) => Step::Refuse(why),
    }
}

/// Carries out `step` on `send`: whether it was the session's first good hello.
async fn answer(
    shared: &Shared,
    connection: &Session,
    send: &mut SendStream,
    step: Step,
    held: Held,
) -> bool {
    match step {
        Step::Hello { first } => {
            let served = Arc::clone(&shared.cert.borrow());
            let core = shared.core.as_ref().ok();
            let bytes = session::hello_bytes(&shared.source.hello(), &served, shared.every, core);
            send_bytes(send, &bytes).await;
            first
        }
        Step::Core => {
            send_bytes(send, &session::core_bytes(&shared.core)).await;
            false
        }
        Step::Refuse(error) => {
            send_bytes(send, &session::error_bytes(&error)).await;
            false
        }
        Step::Close { error, code } => {
            send_bytes(send, &session::error_bytes(&error)).await;
            connection.close(code, error.kind);
            false
        }
        Step::Op { name, args } => {
            let source = Arc::clone(&shared.source);
            let started_ns = held.started_ns;
            // Dropped as this ends however it ends (answered, stopped by the page, the session
            // gone, the server stopping): the sender gone is the source's stop.
            let (_stop, stopping): (_, Stop) = crossbeam_channel::bounded::<()>(0);
            let work = tokio::task::spawn_blocking(move || {
                let answer = source
                    .op(&name, &args, &stopping)
                    .unwrap_or_else(|| Err(session::unknown_op(&name, &source.hello())));
                frame::answer_bytes(answer, session::elapsed_ns(started_ns, clock::now_ns()))
            });
            let bytes = tokio::select! {
                done = work => match done {
                    Ok(bytes) => bytes,
                    Err(why) => session::error_bytes(&OpError::new(
                        Kind::Encode,
                        format!("the answer's work did not end: {why}"),
                    )),
                },
                _ = send.stopped() => return false,
            };
            send_bytes(send, &bytes).await;
            drop(held);
            false
        }
    }
}

async fn refuse(mut send: SendStream, mut recv: RecvStream, refused: OpError) {
    recv.stop(0);
    send_bytes(&mut send, &session::error_bytes(&refused)).await;
}

/// The request on `recv`: at most [`session::MAX_REQUEST`] bytes, whole within
/// [`session::REQUEST_WAIT`].
async fn read(recv: &mut RecvStream) -> Result<Vec<u8>, OpError> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let reading = async {
        loop {
            match recv.read(&mut chunk).await {
                Ok(Some(n)) if bytes.len() + n > session::MAX_REQUEST => {
                    return Err(session::too_long());
                }
                // alloc: a request, at most MAX_REQUEST
                Ok(Some(n)) => bytes.extend_from_slice(&chunk[..n]),
                Ok(None) => return Ok(()),
                Err(why) => return Err(session::unread(why)),
            }
        }
    };
    match tokio::time::timeout(session::REQUEST_WAIT, reading).await {
        Ok(Ok(())) => Ok(bytes),
        Ok(Err(why)) => Err(why),
        Err(_) => Err(session::late()),
    }
}

async fn send_bytes(send: &mut SendStream, bytes: &[u8]) {
    if send.write_all(bytes).await.is_ok() {
        send.finish();
    }
}

/// The live stream: every [`Config::every`], a frame of each table of the session's own feed
/// with rows (and of a [`super::Rule::Replace`] table once when it empties); and a `cert` frame
/// when the certificate is made anew.
///
/// A page that reads slower than the frames come falls behind by at most one frame: while a
/// write waits on the page, ticks are skipped, not queued, and the next tick takes everything
/// since this session's last one. What the source dropped meanwhile is counted in `skipped`.
async fn live(shared: Arc<Shared>, connection: Session, mut cert: watch::Receiver<Arc<Served>>) {
    let Ok(mut send) = connection.open_uni().await else { return };
    let mut every = tokio::time::interval(shared.every);
    every.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // alloc: once a session, the feed moved to and from the blocking pool each tick
    let mut state = Box::new((shared.source.live(), Tick::default()));
    let mut feed = session::Feed::default();
    let mut out = Vec::new();
    loop {
        tokio::select! {
            _ = every.tick() => {
                let at_ns = clock::now_ns();
                let read = tokio::task::spawn_blocking(move || {
                    let (live, tick) = &mut *state;
                    tick.clear(at_ns);
                    let read = live.tick(at_ns, tick);
                    (state, read)
                });
                let Ok((back, read)) = read.await else { return };
                state = back;
                let written = match read {
                    Ok(()) => live_tables(&mut send, &mut out, &state.1, &mut feed).await,
                    Err(error) => {
                        let header = session::live_error_header(&error);
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
                if !live_frame(&mut send, &mut out, &session::cert_header(&current), &[]).await {
                    return;
                }
            }
        }
    }
}

/// Writes the frames [`session::Feed::emit`] says of `tick`'s tables; `false` when the page is
/// gone.
async fn live_tables(
    send: &mut SendStream,
    out: &mut Vec<u8>,
    tick: &Tick,
    feed: &mut session::Feed,
) -> bool {
    for table in &tick.tables {
        let ipc: &[u8] = match feed.emit(table.name, table.rule, table.rows) {
            Emit::Nothing => continue,
            Emit::Rows => &table.ipc,
            Emit::Cleared => &[],
        };
        let header = session::live_header(table.name, table.rows, tick);
        if !live_frame(send, out, &header, ipc).await {
            return false;
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
