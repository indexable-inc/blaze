//! The driver: one WebTransport endpoint on `127.0.0.1` at a port the OS picks, on a thread of
//! its own, so the program's threads never wait on it. Everything the server decides is
//! [`Serving`]'s, a sans-IO machine; this module only runs it: blaze-http's HTTP/3 driver loop
//! ([`Loop`]: the UDP socket and QUIC's timers) and the machine on one blaze-core native
//! driver, whose one wait is its poll, and each [`Job`] the machine asks for (a source's
//! answer, a live tick, a new certificate) on a thread of its own, whose result comes back on
//! a queue and wakes the poll ([`blaze_core::Waker`]).
//!
//! A step of the loop is: the stop asked for, the jobs' results handed to the machine, the
//! machine stepped, its jobs started and its lines said, the certificate it serves published to
//! [`Explorer::link`], the HTTP/3 loop stepped, the effects submitted, and one poll to the
//! earlier of the two machines' deadlines. No thread or wake is added per input beyond the h3
//! driver's own: a job is one thread and one wake.

use std::{
    alloc::Global,
    net::{Ipv4Addr, SocketAddr},
    panic::AssertUnwindSafe,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_core::{Driver as _, Waker, lock::Lock};
use blaze_http::{
    h3::{self, Endpoint, Identity, ServerConfig, Transport, driver::Loop},
    mailbox::{self, Rx, Tx},
};
use crossbeam_queue::SegQueue;
use serde_json::Value;

use super::{
    CoreWasm, Kind, OpError, Source,
    cert::{Cert, CertError, Rotation},
    frame,
    link::{self, Origins, Token},
    serving::{Clock, Done, Feeding, Job, JobId, SHUTDOWN, Serving, Setup, TAGS, Work},
    session::{self, Served},
};
use crate::clock;

/// How long the explorer's thread may take to make its driver and bind its socket: both take
/// milliseconds; past this the thread is stuck and the start says so.
const START_WAIT: Duration = Duration::from_secs(10);
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
    /// How often each session's live stream ticks, as `hello`'s `live_every_ms` says it; never
    /// zero.
    pub every: Duration,
    /// The explorer core this program serves the page (explore-core.md decision 3): `hello`
    /// names it and the `core` op answers its bytes. `Err` is why there is none (where the
    /// program looked and how to make one), which `hello` leaves out and `core` answers as
    /// `no_core`.
    pub core: Result<CoreWasm, String>,
}

/// What the program's threads share with the explorer's.
struct Shared {
    token: Token,
    port: u16,
    name: &'static str,
    /// The certificate served now, as the loop last published it.
    served: Lock<Arc<Served>>,
    /// Set by [`Explorer`]'s drop, then the loop woken.
    stop: AtomicBool,
    waker: Waker,
}

/// A running explorer; dropping it closes every session and stops the server, waiting at most
/// [`SHUTDOWN`] for the pages to acknowledge their closes.
pub struct Explorer {
    shared: Arc<Shared>,
    /// Ends (its sender dropped) as the explorer's thread ends, however it ends.
    ended: Option<Rx<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Explorer {
    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// The SHA-256 of the certificate served now, as the link carries it.
    pub fn certhash(&self) -> String {
        // clone: the hash out of the lock, once a link
        self.shared.served.lock().hash.clone()
    }

    /// The link to print and open: it changes when the certificate is made anew.
    pub fn link(&self) -> String {
        link::link(self.shared.port, &self.shared.token, &self.shared.served.lock().hash)
    }
}

impl Drop for Explorer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.waker.wake();
        let Some(ended) = self.ended.take() else { return };
        // The loop ends at most SHUTDOWN after it sees the stop; past twice that it is stuck,
        // and is left rather than joined.
        let waited = mailbox::block_on("the explorer's thread to close its sessions", Some(SHUTDOWN * 2), ended.recv());
        if let Err(why) = waited {
            eprintln!("{}: the explorer's thread was left running: {why}", self.shared.name);
            return;
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
    /// blaze-core's native driver, or the explorer's thread's start: what failed.
    Driver(String),
    Thread(std::io::Error),
    /// The HTTP/3 endpoint refused its config, or its socket was not bound.
    Config(h3::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Clock(why) => {
                write!(f, "the wall clock is before 1970, so no certificate can be dated: {why}")
            }
            StartError::Cert(why) => write!(f, "{why}"),
            StartError::Driver(why) => write!(f, "the explorer's driver did not start: {why}"),
            StartError::Thread(why) => write!(f, "the explorer's thread did not start: {why}"),
            StartError::Config(why) => {
                write!(f, "the explorer could not listen on 127.0.0.1 (UDP): {why}")
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
    let served = Served { hash: cert.hash, not_after_s: cert.validity.not_after_s };
    let (ready_to, ready) = mailbox::channel::<Result<Arc<Shared>, StartError>>(Some(1));
    let (ended_to, ended) = mailbox::channel::<()>(Some(1));
    let name = config.name;
    let thread = std::thread::Builder::new()
        .name(name.replace(' ', "-"))
        .spawn(move || {
            // Dropped as the thread ends, however it ends: the drop's wait ends with it.
            let _ended = ended_to;
            start(config, identity, served, rotation, &ready_to);
        })
        .map_err(StartError::Thread)?;
    let started = mailbox::block_on("the explorer's thread to bind its socket", Some(START_WAIT + SHUTDOWN), ready.recv());
    match started {
        Ok(Some(Ok(shared))) => Ok(Explorer { shared, ended: Some(ended), thread: Some(thread) }),
        Ok(Some(Err(why))) => {
            _ = thread.join();
            Err(why)
        }
        Ok(None) => {
            _ = thread.join();
            Err(StartError::Thread(std::io::Error::other("the explorer's thread ended before it listened")))
        }
        // The thread is stuck past its own bound: it is left, not joined.
        Err(blocked) => Err(StartError::Driver(blocked.to_string())),
    }
}

fn unix_now_s() -> Result<u64, StartError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH).map_err(StartError::Clock)?.as_secs())
}

fn server_config(identity: Identity) -> ServerConfig {
    ServerConfig {
        identity,
        transport: Transport { idle: IDLE, keep_alive: Some(KEEP_ALIVE) },
        requests: false,
    }
}

/// The explorer's thread: its driver, its endpoint bound, then the loop.
fn start(config: Config, identity: Identity, served: Served, rotation: Rotation, ready_to: &Tx<Result<Arc<Shared>, StartError>>) {
    // A caller that went away wants no answer.
    let refuse = |why: StartError| {
        _ = ready_to.send(Err(why));
    };
    // No file effect is asked: the root is never read.
    let mut driver = match blaze_core::native(Path::new("/"), 1) {
        Ok(it) => it,
        Err(why) => return refuse(StartError::Driver(format!("blaze-core's native driver: {why}"))),
    };
    let Some(waker) = driver.waker() else {
        return refuse(StartError::Driver(
            "blaze-core's native driver gives no waker: a finished job could not end the loop's wait".to_owned(),
        ));
    };
    // quinn-proto's clock is an `Instant`: the epoch is read once, right after the native
    // driver whose clock (`Driver::now`, from its start) it stands for.
    let epoch = Instant::now();
    let endpoint = match Endpoint::new(epoch, Some(server_config(identity)), None) {
        Ok(it) => it,
        Err(why) => return refuse(StartError::Config(why)),
    };
    let at = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let mut core = Loop::new(endpoint, 0, at);
    let mut effects = Vec::new_in(Global);
    let mut done = Vec::new_in(Global);
    // Bounded by START_WAIT on the driver's clock.
    let local = loop {
        if let Some(local) = core.local() {
            break local;
        }
        if let Some(error) = core.failed() {
            return refuse(StartError::Config(error.clone()));
        }
        if driver.now() > START_WAIT {
            return refuse(StartError::Driver(format!("binding the UDP socket at {at}: not bound in {START_WAIT:?}")));
        }
        core.step(driver.now(), &mut effects);
        driver.submit(&mut effects);
        {
            let _wait = tracing::info_span!(target: "blaze::ix_prof", "bind the explorer's socket", wait = "driver", on = config.name).entered();
            driver.poll(&mut done, blaze_core::wait::Deadline::after(driver.now(), START_WAIT));
        }
        for completion in done.drain(..) {
            core.complete(completion);
        }
    };
    let Config { source, dev_origin, name, every, core: wasm } = config;
    let token = Token::random();
    let setup = Setup {
        // clone: once a server, the core's copy beside the link's
        token: token.clone(),
        origins: Origins { dev: dev_origin },
        name,
        every,
        served,
        rotation,
        core: wasm,
    };
    let serving = Serving::new(setup, TAGS, clock_of(&driver));
    let shared = Shared {
        token,
        port: local.port(),
        name,
        served: Lock::new("ixprof.explore.served", Arc::clone(serving.served())),
        stop: AtomicBool::new(false),
        waker,
    };
    let shared = Arc::new_in(shared, Global);
    if ready_to.send(Ok(Arc::clone(&shared))).is_err() {
        return;
    }
    run(&mut driver, core, serving, &shared, &source);
}

fn clock_of(driver: &impl blaze_core::Driver) -> Clock {
    Clock { now: driver.now(), now_ns: clock::now_ns(), wall_s: driver.wall_ns() / 1_000_000_000 }
}

/// The loop, until the explorer stops (and its sessions are closed, or [`SHUTDOWN`] passed) or
/// its socket fails.
fn run(driver: &mut impl blaze_core::Driver, mut core: Loop, mut serving: Serving, shared: &Shared, source: &Arc<dyn Source>) {
    let results: Arc<SegQueue<(JobId, Done)>> = Arc::new_in(SegQueue::new(), Global);
    let mut effects = Vec::new_in(Global);
    let mut done = Vec::new_in(Global);
    let mut jobs = Vec::new_in(Global);
    let mut said = Vec::new_in(Global);
    let mut published = Arc::clone(serving.served());
    loop {
        if shared.stop.load(Ordering::Acquire) {
            serving.stop(driver.now(), format!("{} stopped", shared.name));
        }
        let now = driver.now();
        core.endpoint().advance_clock(now);
        while let Some((id, result)) = results.pop() {
            serving.done(id, result);
        }
        serving.step(clock_of(driver), core.endpoint(), &mut effects);
        serving.take_jobs(&mut jobs);
        for job in jobs.drain(..) {
            run_job(job, source, &results, &shared.waker);
        }
        serving.take_said(&mut said);
        for line in said.drain(..) {
            eprintln!("{line}");
        }
        if !Arc::ptr_eq(&published, serving.served()) {
            published = Arc::clone(serving.served());
            *shared.served.lock() = Arc::clone(&published);
        }
        // Closed, or a page did not acknowledge its close in SHUTDOWN: either way the server is
        // done.
        if serving.over(core.endpoint()).is_some() {
            return;
        }
        if let Some(error) = core.failed() {
            eprintln!("{}: the explorer stopped: {error}", shared.name);
            return;
        }
        core.step(now, &mut effects);
        driver.submit(&mut effects);
        {
            let _wait = tracing::info_span!(target: "blaze::ix_prof", "wait for the explorer's socket, timers and jobs", wait = "driver", on = shared.name).entered();
            driver.poll(&mut done, core.deadline().min(serving.deadline()));
        }
        // What came is received at the time it came, not at the time the poll began.
        core.endpoint().advance_clock(driver.now());
        for completion in done.drain(..) {
            if core.owns(completion.tag) {
                core.complete(completion);
            } else if serving.owns(completion.tag) {
                serving.complete(completion);
            } else {
                panic!("the explorer's loop: {:?} is neither its HTTP/3 driver's nor its serving core's", completion.tag);
            }
        }
    }
}

/// Runs `job` on a thread of its own; its result is queued on `results` and the loop woken.
fn run_job(job: Job, source: &Arc<dyn Source>, results: &Arc<SegQueue<(JobId, Done)>>, waker: &Waker) {
    let Job { id, work } = job;
    let (to, wake, source) = (Arc::clone(results), waker.clone(), Arc::clone(source));
    let spawned = std::thread::Builder::new().spawn(move || {
        let done = match std::panic::catch_unwind(AssertUnwindSafe(|| work_on(work, &*source, &clock::now_ns))) {
            Ok(done) => done,
            Err(panic) => Done::Failed(OpError::new(
                Kind::Encode,
                format!("the answer's work did not end: it panicked: {}", panic_message(&*panic)),
            )),
        };
        to.push((id, done));
        wake.wake();
    });
    if let Err(why) = spawned {
        results.push((id, Done::Failed(OpError::new(Kind::Encode, format!("the answer's work did not start: {why}")))));
        waker.wake();
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    match (panic.downcast_ref::<&'static str>(), panic.downcast_ref::<String>()) {
        (Some(message), _) => message,
        (None, Some(message)) => message,
        (None, None) => "a panic with no message",
    }
}

/// Does `work` with `source`, `now_ns` the source's clock: the one runner of a [`Job`], the
/// native loop's and the simulation's.
pub(crate) fn work_on(work: Work, source: &dyn Source, now_ns: &dyn Fn() -> i64) -> Done {
    match work {
        Work::Hello => Done::Hello(source.hello()),
        Work::Op { name, request, started_ns, stop } => {
            // The request whole, for the source: the core read only its `op`. The core took
            // these bytes as a JSON object, so a failure here is the two readings disagreeing,
            // answered as what it is.
            let answer = match serde_json::from_slice::<Value>(&request) {
                Ok(args) => source
                    .op(&name, &args, &stop)
                    .unwrap_or_else(|| Err(session::unknown_op(&name, &source.hello()))),
                Err(why) => Err(OpError::new(Kind::BadArgs, format!("the request of `{name}` is not JSON: {why}"))
                    .with("op", name.as_str())),
            };
            Done::Op(frame::answer_bytes(answer, session::elapsed_ns(started_ns, now_ns())))
        }
        Work::Tick { mut feeding, at_ns } => {
            let Feeding { live, tick } = &mut *feeding;
            let live = live.get_or_insert_with(|| source.live());
            tick.clear(at_ns);
            let read = live.tick(at_ns, tick);
            Done::Tick { feeding, read }
        }
        Work::Cert { validity, name } => {
            Done::Cert(Cert::make(validity, name).and_then(|cert| Ok((cert.identity()?, cert.hash))))
        }
    }
}
