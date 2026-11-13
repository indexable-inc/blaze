//! The control thread: the launcher's one thread that changes anything. It owns the
//! [`Driver`], speaks the patch protocol (`blaze_launch::wire`) with the daemon over a Unix
//! socket, serves the stubs that fire and takes what threads report.
//!
//! [`run`] is its loop, on the thread that calls it. Everything arrives on one queue, in the
//! order it happened: a thread's birth, report or end; a stub's need; a frame the reader
//! thread took off the socket; the socket's end; the word to stop. So nothing here waits on
//! two things, and the order the core asks for (every announced birth before a collect) is
//! the queue's own. The reader thread does one thing: it blocks in `read` on the socket and
//! puts each whole frame on the queue.
//!
//! What it says and hears:
//!
//! | arrives | the control thread |
//! |---|---|
//! | (start) | says `Hello` (with no socket yet, nothing: see "A start from the program's file") |
//! | `Welcome { slots }` | declares the slots; for a launcher that holds its start image, takes that image instead |
//! | `Start { entry, inits }` | gives the thread that waits to run the program the addresses of its initializers and its entry |
//! | `Record { id, bytes }` | hands the record to [`Records`], which keeps it for the message that names it |
//! | `Statics { defs }` | has [`Records`] give each static its memory and its value |
//! | a stub's need of a slot | says `Need` once a slot, however many threads wait in its stub |
//! | `Edit { txn, binds, declare, together }` | declares, runs [`Driver::edit`], says `Applied` (and lets the stubs of the slots it bound go on) or `Failed` |
//! | `Refused { slot, diagnostic }` | gives the diagnostic to the threads in that slot's stub, which stop the program with it |
//! | `Restart { txn, why }` | says `Restarting` and starts the program again in place (the caller's `restart`) |
//! | `Probe` | not built (launcher.md, step 5): a failure that says so |
//! | `Image { bytes }` | its whole start in one message: declares the image's slots, copies its data and its code in with their addresses moved to this process's ([`Driver::image`]), and gives the waiting thread its initializers and its entry |
//!
//! A failure (the socket ends, a message is not of the schema, the driver breaks) does not
//! end the loop: program threads may still be running and reporting, and one may yet fire a
//! stub. The first failure is kept and returned when the loop is stopped; from then on every
//! stub that fires or waits is answered `Lost` with it, which stops the program by name
//! (rule `no-fallback`: nothing runs on quietly without its daemon).
//!
//! # A start from the program's file
//!
//! A launcher may hold its start already ([`Held`]: the `Tell::Image` frame of the program's
//! start file, `<program>.blaze-start`, which the daemon places beside the program once the
//! image is durable). Then:
//!
//! - with a socket ([`Join::Now`]) it says `Hello { holds: Some(check), running: false }` and
//!   waits: `Welcome` (the daemon holds that image) takes the held frame, an `Image` is taken
//!   as it is told (the daemon holds another), `Unheld` ends as it always has;
//! - with none ([`Join::Later`]: no daemon listened) it takes the held frame at once and the
//!   program runs, asking no one. The first stub that fires runs the join, once a process: the
//!   caller's closure starts a daemon and connects (bounded by its own deadline,
//!   [`JOIN_WITHIN`]), the launcher says `Hello { holds: Some(check), running: true }` and,
//!   once `Welcome`d, asks for every slot that waited. A join that failed, or an `Unheld`,
//!   answers that need and every later one [`Bound::Refused`] with why: the program stops on
//!   that call, by name, never on other code.
//!
//! The waits of this module, each on a thread of its own and none on a fiber: the control
//! thread on its queue ([`Driver::wait`]); the reader in `read`; a write of a frame to the
//! socket (the daemon reads as fast as it is asked). The launcher has no profile to open
//! spans in yet, so these are named here and not in a trace.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use blaze_launch::wire::{self, Ask, ProgramId, Tell, WireError};
use blaze_launch::{Slot, Target, Thread};
use crossbeam_channel::Sender;

use crate::drive::{Driver, EditError, Error, Records};
use crate::sys;
use crate::table::{Bound, Event, Shared};

/// Why the control thread could not go on serving code.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ControlError {
    Driver(Error),
    /// A frame that is not a message of the schema.
    Wire(WireError),
    /// A call on the socket failed.
    Socket { call: &'static str, why: String },
    /// The daemon's socket ended.
    Closed(String),
    /// [`Records`] refused a record or a static's definition.
    Store(String),
    /// The program's start file (`<program>.blaze-start`) does not hold one whole image.
    StartFile(String),
    /// A `Start` whose entry is no slot of the table.
    Entry { entry: Slot },
    /// A `Start` that names as an initializer a static the daemon did not define, or a
    /// thread-local.
    Init { number: u32 },
    /// The daemon said the program must start again ([`Tell::Restart`]), and why.
    Restart { why: String },
    /// The daemon holds no code of the program, and why.
    Unheld { why: String },
    /// A `Refused` for a slot no thread asked for.
    Unasked { slot: Slot },
    /// A message whose handling is not built yet.
    NotBuilt { what: &'static str },
}

impl core::fmt::Display for ControlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ControlError::Driver(why) => write!(f, "{why}"),
            ControlError::Wire(why) => write!(f, "the daemon's message: {why}"),
            ControlError::Socket { call, why } => {
                write!(f, "the daemon's socket: {call} failed: {why}")
            }
            ControlError::Closed(why) => write!(f, "the daemon's socket ended: {why}"),
            ControlError::Store(why) => write!(f, "the daemon's records: {why}"),
            ControlError::StartFile(why) => write!(f, "the program's start file: {why}"),
            ControlError::Entry { entry } => write!(
                f,
                "the daemon named slot {} as the program's entry, which is not declared",
                entry.0
            ),
            ControlError::Init { number } => write!(
                f,
                "the daemon named static {number} as an initializer, which is not a static \
                 with an address here"
            ),
            ControlError::Restart { why } => write!(f, "restart needed: {why}"),
            ControlError::Unheld { why } => write!(f, "{why}"),
            ControlError::Unasked { slot } => {
                write!(f, "the daemon refused slot {}, which no thread asked for", slot.0)
            }
            ControlError::NotBuilt { what } => {
                write!(f, "the daemon sent {what}, which this launcher does not do yet")
            }
        }
    }
}

impl core::error::Error for ControlError {}

impl From<Error> for ControlError {
    fn from(why: Error) -> ControlError {
        ControlError::Driver(why)
    }
}

#[cfg(target_arch = "aarch64")]
pub(crate) const ARCH: wire::Arch = wire::Arch::Aarch64;
#[cfg(target_arch = "x86_64")]
pub(crate) const ARCH: wire::Arch = wire::Arch::X86_64;
#[cfg(target_os = "macos")]
pub(crate) const OS: wire::Os = wire::Os::Macos;
#[cfg(target_os = "linux")]
pub(crate) const OS: wire::Os = wire::Os::Linux;

/// The longest a launcher that started from its file waits, at a first call no image held,
/// for the daemon it starts to listen (the join's closure holds itself to it).
pub const JOIN_WITHIN: Duration = Duration::from_secs(120);

/// A start file's start: its `Tell::Image` frame, length first as a daemon sends it, and the
/// check word of the image it is the frame of (the start file's bytes 8..40).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Held<'a> {
    pub frame: &'a [u8],
    pub check: [u8; 32],
}

/// How the launcher reaches its daemon.
pub enum Join {
    /// A daemon listened: its socket.
    Now(UnixStream),
    /// None listened: what starts one and connects to it, run at most once a process, at the
    /// first call no held image answers. `dyn`: one a process.
    Later(Box<dyn FnMut() -> Result<UnixStream, String> + Send>),
}

/// Where the launcher is with its daemon.
enum Joining {
    /// Connected, `Hello` said: what the daemon says is served as it comes.
    Joined,
    /// Connected, `Hello { holds, running: false }` said for the held image, which the
    /// daemon's `Welcome` gives the launcher.
    Holds,
    /// No daemon yet: the closure that joins one, at the first need.
    Later(Box<dyn FnMut() -> Result<UnixStream, String> + Send>),
    /// Connected after the program started from its image: `Hello { running: true }` said,
    /// and the needs wait, unasked, for the daemon's `Welcome`.
    Waits,
    /// No daemon can serve this program: every need is refused with why.
    Refused(String),
}

/// A bug planted in the join, for its test's control.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    /// The join is not remembered: each need runs the closure again.
    JoinsTwice,
}

/// Whether [`Plant::JoinsTwice`] is planted; never outside a test.
#[cfg(test)]
pub(crate) static JOINS_TWICE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    match plant {
        Plant::JoinsTwice => JOINS_TWICE.load(std::sync::atomic::Ordering::Relaxed),
    }
}

#[cfg(not(test))]
fn planted(_: Plant) -> bool {
    false
}

#[cfg(not(test))]
#[derive(Clone, Copy)]
enum Plant {
    JoinsTwice,
}

struct Control<'a, 'h> {
    driver: Driver,
    /// `None` until a [`Join::Later`] joins.
    socket: Option<UnixStream>,
    joining: Joining,
    /// The program, for a `Hello` said at a join.
    program: ProgramId,
    /// The start file's start, until it is taken.
    held: Option<Held<'h>>,
    /// The check word of the start file's image, for a `Hello`: `None` with no start file.
    holds: Option<[u8; 32]>,
    /// The reader thread of the socket, once there is one.
    reader: Option<std::thread::JoinHandle<()>>,
    records: &'a mut dyn Records,
    /// The slots whose stub fired, each with the thread that fired it first, while no `Need`
    /// can be said yet (no daemon, or one that has not welcomed the launcher): asked once it
    /// has.
    unasked: Vec<(Slot, Thread)>,
    /// By slot: the threads waiting in its stub, for which one `Need` was said.
    needs: Vec<(Slot, Vec<std::sync::Arc<sys::Handoff<Bound>>>)>,
    /// The id of the last message this side began.
    said: u64,
    /// The first failure: no code can come any more.
    lost: Option<ControlError>,
    /// One frame on its way out.
    out: Vec<u8>,
    /// Where the program's main thread waits for what to run ([`Start`], once the daemon
    /// said the statics are defined), or for why nothing can.
    ready: Sender<Result<Start, String>>,
    /// What a [`Tell::Restart`] does: starts the program again in place. It returns only
    /// when it could not, with why.
    restart: fn(&str) -> String,
}

/// What the program's main thread runs: the addresses of its initializer statics (each
/// holds the address of a function to call first, in this order), then of its entry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Start {
    pub entry: usize,
    pub inits: Vec<usize>,
}

/// Runs the control thread's loop on this thread until [`Shared::stop`]: reaches the daemon
/// as `join` says, starts from `held` where the launcher holds its start (module docs, "A
/// start from the program's file"), says `Hello` for `program`, then serves the daemon and the
/// threads of `driver`. Gives the driver back, with the first failure if there was one.
pub fn run(
    driver: Driver,
    join: Join,
    held: Option<Held<'_>>,
    program: ProgramId,
    records: &mut dyn Records,
    ready: Sender<Result<Start, String>>,
    restart: fn(&str) -> String,
) -> (Driver, Result<(), ControlError>) {
    let shared = driver.shared();
    let mut control = Control {
        driver,
        socket: None,
        joining: Joining::Joined,
        program,
        holds: held.map(|it| it.check),
        held,
        reader: None,
        records,
        unasked: Vec::new(),
        needs: Vec::new(),
        said: 0,
        lost: None,
        out: Vec::new(),
        ready,
        restart,
    };
    let begun = match join {
        Join::Now(socket) => {
            let holds = control.held.map(|it| it.check);
            if holds.is_some() {
                control.joining = Joining::Holds;
            }
            control.connect(socket, holds, false, &shared)
        }
        Join::Later(mut join) => match control.held {
            // The program runs from its file, asking no one; the daemon is joined at the
            // first call the image does not hold.
            Some(_) => {
                control.joining = Joining::Later(join);
                control.start_held()
            }
            // Nothing to run without a daemon: it is joined now.
            None => match join() {
                Ok(socket) => control.connect(socket, None, false, &shared),
                Err(why) => Err(ControlError::Unheld { why }),
            },
        },
    };
    if let Err(why) = begun {
        control.lose(why);
    }
    loop {
        // Collect only once the queue is empty: every birth announced so far is then known.
        let event = match control.driver.ready() {
            Some(event) => event,
            None => {
                if let Err(why) = control.driver.collect(&mut *control.records) {
                    control.lose(why.into());
                }
                control.driver.wait()
            }
        };
        match control.handle(event, &shared) {
            Ok(true) => {}
            Ok(false) => break,
            Err(why) => control.lose(why),
        }
    }
    control.end()
}

/// The reader thread: every whole frame of `from` onto the queue, then why it ended. The
/// frames of one read are queued together and the control thread is woken once for them.
fn read(mut from: UnixStream, shared: &Shared) {
    // alloc: the socket's bytes not yet a whole frame; one buffer for the thread's life.
    let mut bytes = Vec::new();
    // alloc: one read's bytes; on the heap, as a reader thread's stack need not hold it.
    let mut chunk = vec![0u8; 256 << 10];
    let why = loop {
        // Every whole frame that is here.
        let mut at = 0;
        let refused = loop {
            match wire::split(&bytes[at..]) {
                Ok(Some((item, took))) => {
                    shared.push(Event::Frame(item.to_vec())); // alloc: one message of the daemon's
                    at += took;
                }
                Ok(None) => break None,
                Err(why) => break Some(format!("a frame was refused: {why}")),
            }
        };
        if at > 0 {
            drop(bytes.drain(..at));
            shared.ring();
        }
        if let Some(why) = refused {
            break why;
        }
        // The wait: for the daemon's next bytes. It ends when the daemon writes, closes, or
        // the control thread shuts the socket down to stop.
        match from.read(&mut chunk) {
            Ok(0) => break "the daemon closed it".to_owned(),
            Ok(got) => bytes.extend_from_slice(&chunk[..got]),
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => {}
            Err(why) => break format!("read failed: {why}"),
        }
    };
    shared.send(Event::Closed(why));
}

impl Control<'_, '_> {
    /// Takes `socket` as the daemon's: starts its reader and says `Hello`, holding `holds`,
    /// with the program already `running` or not.
    fn connect(
        &mut self,
        socket: UnixStream,
        holds: Option<[u8; 32]>,
        running: bool,
        shared: &std::sync::Arc<Shared>,
    ) -> Result<(), ControlError> {
        let call = |call: &'static str| {
            move |why: std::io::Error| ControlError::Socket { call, why: why.to_string() }
        };
        let page = sys::page().map_err(Error::Sys)?;
        let from = socket.try_clone().map_err(call("try_clone"))?;
        self.socket = Some(socket);
        let theirs = std::sync::Arc::clone(shared);
        let reader = std::thread::Builder::new()
            .name("blaze-launch-read".to_owned())
            .spawn(move || read(from, &theirs))
            .map_err(call("starting the reader thread"))?;
        self.reader = Some(reader);
        let hello = Ask::Hello {
            program: self.program,
            pid: u64::from(std::process::id()),
            arch: ARCH,
            os: OS,
            page: page as u32,
            holds,
            running,
        };
        self.ask(None, &hello)
    }

    /// Takes the held start file's frame as the `Tell::Image` it is: the program's whole
    /// start, copied in, and its main thread given what to run. A frame that is not one whole
    /// `Image` is refused naming where.
    fn start_held(&mut self) -> Result<(), ControlError> {
        let Some(held) = self.held.take() else {
            return Err(ControlError::StartFile("the launcher holds no start file's frame to start from".to_owned()));
        };
        let frame = held.frame;
        let item = match wire::split(frame).map_err(ControlError::Wire)? {
            Some((item, took)) if took == frame.len() => item,
            Some((_, took)) => {
                return Err(ControlError::StartFile(format!(
                    "the start file's frame ends at byte {took} and {} more bytes follow it",
                    frame.len() - took
                )));
            }
            None => {
                return Err(ControlError::StartFile(format!(
                    "the start file's frame is cut at byte {}: its length says more",
                    frame.len()
                )));
            }
        };
        match Tell::decode(item).map_err(ControlError::Wire)? {
            (_, Tell::Image { bytes }) => self.image(bytes),
            (_, other) => Err(ControlError::StartFile(format!(
                "the start file's frame is not an image of the program's start: {}",
                told(&other)
            ))),
        }
    }

    /// An image of the program's whole start ([`Tell::Image`]): what `Welcome`, `Statics`,
    /// the first `Edit` and `Start` leave, copied in, and the main thread told what to run.
    fn image(&mut self, bytes: &[u8]) -> Result<(), ControlError> {
        let image = blaze_launch::image::Image::decode(bytes)
            .map_err(|why| ControlError::Store(why.to_string()))?;
        let started = self.driver.image(image, &mut *self.records);
        self.records.release();
        let (entry, inits) = started?;
        // The program's main thread waits for this; were it gone, the process is ending and
        // there is no one to tell.
        drop(self.ready.send(Ok(Start { entry, inits })));
        Ok(())
    }

    /// The first need of a program that started from its file with no daemon: the join, run
    /// once a process. Its `Err` refuses this need and every later one.
    fn join(&mut self, slot: Slot, shared: &std::sync::Arc<Shared>) {
        let Joining::Later(join) = &mut self.joining else { return };
        // The wait: the closure's, for a daemon to start and listen, bounded by its own
        // deadline (`JOIN_WITHIN`). The launcher opens no spans; it is named here: the first
        // call of the slot that fired waits for a daemon to start.
        let joined = join();
        match joined {
            Ok(socket) => {
                if !planted(Plant::JoinsTwice) {
                    self.joining = Joining::Waits;
                }
                if let Err(why) = self.connect(socket, self.holds, true, shared) {
                    self.refuse(format!("the first call of slot {} joined a daemon, and saying hello failed: {why}", slot.0));
                }
            }
            Err(why) => {
                self.refuse(format!("the first call of slot {} waits for a daemon to start, and it did not: {why}", slot.0));
            }
        }
    }

    /// No daemon can serve this program: every need waiting and to come is refused with
    /// `why`, which stops the program on that call, by name.
    fn refuse(&mut self, why: String) {
        self.unasked.clear();
        for (_, waiters) in self.needs.drain(..) {
            for waiter in waiters {
                waiter.put(Bound::Refused(why.clone())); // clone: one answer a waiter
            }
        }
        if !planted(Plant::JoinsTwice) {
            self.joining = Joining::Refused(why);
        }
    }

    /// Writes one message to the daemon: under `id` when it answers a message of the
    /// daemon's, else under this side's next.
    fn ask(&mut self, id: Option<u64>, ask: &Ask<'_>) -> Result<(), ControlError> {
        let id = id.unwrap_or_else(|| {
            self.said += 1;
            self.said
        });
        self.out.clear();
        ask.encode(id, &mut self.out).map_err(ControlError::Wire)?;
        let Some(socket) = &mut self.socket else {
            return Err(ControlError::Socket { call: "write", why: "no daemon is joined yet".to_owned() });
        };
        // The wait: for the socket to take the frame.
        socket.write_all(&self.out).map_err(|why| ControlError::Socket { call: "write", why: why.to_string() })
    }

    /// No code can come any more, for `why`: keeps the first such failure and answers every
    /// stub that waits.
    fn lose(&mut self, why: ControlError) {
        let said = why.to_string();
        self.answer(&said);
        // A main thread still waiting for its entry is told too (one that has it ignores
        // the queue from then on).
        drop(self.ready.try_send(Err(said)));
        self.lost.get_or_insert(why);
    }

    /// Answers every stub that waits: no code can come, for `why`.
    fn answer(&mut self, why: &str) {
        for (_, waiters) in self.needs.drain(..) {
            for waiter in waiters {
                // A waiter is a thread blocked on this answer; were it gone, there is no one
                // to tell.
                waiter.put(Bound::Lost(why.to_owned()));
            }
        }
    }

    /// Serves one event. `false` is the word to stop.
    fn handle(&mut self, event: Event, shared: &std::sync::Arc<Shared>) -> Result<bool, ControlError> {
        match event {
            Event::Thread(told) => self.driver.told(told)?,
            Event::Need { slot, thread, reply } => self.need(slot, thread, reply, shared)?,
            Event::Frame(item) => self.tell(&item)?,
            Event::Closed(why) => return Err(ControlError::Closed(why)),
            Event::Stop => return Ok(false),
        }
        Ok(true)
    }

    /// A stub fired.
    fn need(
        &mut self,
        slot: Slot,
        thread: Thread,
        reply: std::sync::Arc<sys::Handoff<Bound>>,
        shared: &std::sync::Arc<Shared>,
    ) -> Result<(), ControlError> {
        if let Some(lost) = &self.lost {
            reply.put(Bound::Lost(lost.to_string()));
            return Ok(());
        }
        // Bound since the thread loaded the word: it calls through the word again.
        if matches!(self.driver.target(slot), Ok(Target::Body(_) | Target::Gate { .. })) {
            reply.put(Bound::Code);
            return Ok(());
        }
        if let Joining::Refused(why) = &self.joining {
            reply.put(Bound::Refused(why.clone())); // clone: one answer a waiter
            return Ok(());
        }
        match self.needs.iter_mut().find(|(waited, _)| *waited == slot) {
            Some((_, waiters)) => waiters.push(reply),
            None => {
                // alloc: the threads in one slot's stub; a list a slot being fetched.
                self.needs.push((slot, vec![reply]));
                match self.joining {
                    Joining::Joined => self.ask(None, &Ask::Need { slot, thread })?,
                    // Asked once the daemon has welcomed the launcher (`Tell::Welcome`).
                    Joining::Holds | Joining::Waits | Joining::Refused(_) => self.unasked.push((slot, thread)),
                    Joining::Later(_) => {
                        self.unasked.push((slot, thread));
                        self.join(slot, shared);
                    }
                }
            }
        }
        Ok(())
    }

    /// The daemon welcomed a launcher that joined it after starting from its image: every
    /// slot whose stub waits is asked for, under the thread that first fired it.
    fn ask_waiting(&mut self) -> Result<(), ControlError> {
        for (slot, thread) in std::mem::take(&mut self.unasked) {
            self.ask(None, &Ask::Need { slot, thread })?;
        }
        Ok(())
    }

    /// A message of the daemon's.
    fn tell(&mut self, item: &[u8]) -> Result<(), ControlError> {
        let (id, tell) = Tell::decode(item).map_err(ControlError::Wire)?;
        match tell {
            Tell::Welcome { slots } => match self.joining {
                Joining::Joined => {
                    self.driver.declare(slots)?;
                }
                // The daemon holds the image this launcher holds: it is the start.
                Joining::Holds => {
                    self.joining = Joining::Joined;
                    self.start_held()?;
                    self.ask_waiting()?;
                }
                // Joined after starting from the image: the daemon bound this launcher as
                // welcomed with that image, whose slots are declared already.
                Joining::Waits => {
                    self.joining = Joining::Joined;
                    self.ask_waiting()?;
                }
                Joining::Later(_) | Joining::Refused(_) => {
                    return Err(ControlError::NotBuilt { what: "a Welcome to a launcher that said no Hello" });
                }
            },
            Tell::Start { entry, inits } => {
                let Some(thunk) = self.driver.thunk(entry) else {
                    return Err(ControlError::Entry { entry });
                };
                // alloc: the addresses of the program's initializer statics, once a process
                let mut addresses = Vec::with_capacity(inits.len());
                for number in inits.iter() {
                    match self.records.static_at(number) {
                        Some(address) => addresses.push(address),
                        None => return Err(ControlError::Init { number }),
                    }
                }
                // The program's main thread waits for this; were it gone, the process is
                // ending and there is no one to tell.
                drop(self.ready.send(Ok(Start { entry: thunk, inits: addresses })));
            }
            Tell::Record { id, bytes } => {
                self.records.record(id, bytes).map_err(ControlError::Store)?;
            }
            Tell::Statics { defs } => {
                // alloc: the definitions out of the frame, for the length of the message.
                let defs: Vec<wire::StaticDef<'_>> = defs.iter().collect();
                let place = self.driver.place();
                let defined = self.records.statics(&defs, place);
                self.records.release();
                defined.map_err(ControlError::Store)?;
            }
            Tell::Edit { txn, binds, declare, together } => {
                if declare > 0 {
                    self.driver.declare(declare)?;
                }
                // alloc: the edit's binds out of the frame, for the length of the edit.
                let binds: Vec<wire::Bind<'_>> = binds.iter().collect();
                // An edit that only adds functions binds nothing: the new slots are stubs.
                if binds.is_empty() {
                    self.ask(Some(id), &Ask::Applied { txn, epoch: self.driver.shared().epoch() })?;
                    return Ok(());
                }
                let edited = self.driver.edit(txn, &binds, together, &mut *self.records);
                self.records.release();
                match edited {
                    Ok(committed) => {
                        self.wake();
                        self.ask(Some(id), &Ask::Applied { txn, epoch: committed.epoch })?;
                    }
                    Err(EditError::Failed(why)) => {
                        let why = why.to_string();
                        self.ask(Some(id), &Ask::Failed { txn, why: &why })?;
                    }
                    Err(EditError::Broken(why)) => return Err(ControlError::Driver(why)),
                }
            }
            Tell::Refused { slot, diagnostic } => {
                let Some(at) = self.needs.iter().position(|(waited, _)| *waited == slot) else {
                    return Err(ControlError::Unasked { slot });
                };
                for waiter in self.needs.swap_remove(at).1 {
                    waiter.put(Bound::Refused(diagnostic.to_owned()));
                }
            }
            Tell::Unheld { why } => match self.joining {
                // The program runs from its image and the daemon holds no code of it: each
                // first call from here on stops the program, naming why.
                Joining::Waits => self.refuse(why.to_owned()),
                _ => return Err(ControlError::Unheld { why: why.to_owned() }),
            },
            Tell::Image { bytes } => {
                // The whole start in one message: what `Welcome`, `Statics`, the first `Edit`
                // and `Start` leave, copied in. A launcher that held another image is told
                // this one: the daemon's is the program's build.
                match self.joining {
                    Joining::Holds => {
                        self.joining = Joining::Joined;
                        self.held = None;
                    }
                    // The program runs: another start under it would be another program.
                    Joining::Waits | Joining::Later(_) | Joining::Refused(_) => {
                        return Err(ControlError::NotBuilt {
                            what: "an image of the program's start to a launcher whose program already runs",
                        });
                    }
                    Joining::Joined => {}
                }
                self.image(bytes)?;
                self.ask_waiting()?;
            }
            Tell::Probe { .. } => {
                return Err(ControlError::NotBuilt { what: "Probe (launcher.md, step 5)" });
            }
            Tell::Restart { txn, why } => {
                // The launcher's answer, then the program again in place: the same process
                // id, terminal and environment, on the code the daemon has now. Nothing of
                // the old program runs on: `restart` does not come back unless it failed, and
                // then every stub is answered with why and the program stops on its next
                // first call (`lose`), never quietly on stale code.
                self.ask(Some(id), &Ask::Restarting { txn })?;
                let failed = (self.restart)(why);
                return Err(ControlError::Restart { why: failed });
            }
        }
        Ok(())
    }

    /// Lets every thread go on whose stub's slot has code now. After the commit's stores: the
    /// thread loads the word again and finds the body.
    fn wake(&mut self) {
        let driver = &self.driver;
        self.needs.retain(|(slot, waiters)| {
            if !matches!(driver.target(*slot), Ok(Target::Body(_) | Target::Gate { .. })) {
                return true;
            }
            for waiter in waiters {
                waiter.put(Bound::Code);
            }
            false
        });
    }

    /// The loop was stopped: ends the reader, answers what is left on the queue, and gives
    /// the driver back.
    fn end(mut self) -> (Driver, Result<(), ControlError>) {
        // The reader's `read` returns once the socket is shut down; a socket the daemon
        // already closed refuses the shutdown (ENOTCONN), and the reader has ended.
        if let Some(socket) = &self.socket {
            drop(socket.shutdown(std::net::Shutdown::Both));
        }
        if let Some(reader) = self.reader.take()
            && reader.join().is_err()
        {
            self.lose(ControlError::Socket {
                call: "the reader thread",
                why: "it panicked".to_owned(),
            });
        }
        self.answer("the launcher's control thread was stopped");
        // What arrived while stopping: thread events are the core's; a stub that fired is
        // answered; the reader's last words are dropped.
        let mut failed = None;
        while let Some(event) = self.driver.ready() {
            match event {
                Event::Thread(told) => {
                    if let Err(why) = self.driver.told(told) {
                        failed.get_or_insert(ControlError::Driver(why));
                    }
                }
                Event::Need { reply, .. } => {
                    let why = "the launcher's control thread was stopped";
                    reply.put(Bound::Lost(why.to_owned()));
                }
                Event::Frame(_) | Event::Closed(_) | Event::Stop => {}
            }
        }
        let result = match (self.lost, failed) {
            (Some(why), _) | (None, Some(why)) => Err(why),
            (None, None) => Ok(()),
        };
        (self.driver, result)
    }
}

/// The name of a message of the daemon's, for an error that names what came.
fn told(tell: &Tell<'_>) -> &'static str {
    match tell {
        Tell::Welcome { .. } => "Welcome",
        Tell::Start { .. } => "Start",
        Tell::Record { .. } => "Record",
        Tell::Statics { .. } => "Statics",
        Tell::Edit { .. } => "Edit",
        Tell::Refused { .. } => "Refused",
        Tell::Probe { .. } => "Probe",
        Tell::Restart { .. } => "Restart",
        Tell::Unheld { .. } => "Unheld",
        Tell::Image { .. } => "Image",
    }
}
