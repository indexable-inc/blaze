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
//! A launcher may hold its start already ([`Held`]: a `Tell::Image` frame and the check word
//! of its image). Then it says `Hello { holds: Some(check), running: false }` and waits:
//! `Welcome` (the other side holds that image) takes the held frame, an `Image` is taken as it
//! is told (the other side holds another), `Unheld` ends the launcher, naming why.
//!
//! The waits of this module, each on a thread of its own and none on a fiber: the control
//! thread on its queue ([`Driver::wait`]); the reader in `read`; a write of a frame to the
//! socket (the daemon reads as fast as it is asked). The launcher has no profile to open
//! spans in yet, so these are named here and not in a trace.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use blaze_launch::wire::{self, Ask, ProgramId, Tell, WireError};
use blaze_launch::{Slot, Target, Thread};

use crate::drive::{Driver, EditError, Error, Records};
use crate::sys;
use crate::stub_wait::{self, Answer};
use crate::table::{Event, Shared};

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

/// A held start: its `Tell::Image` frame, length first as the other side sends it, and the
/// check word of the image it is the frame of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Held<'a> {
    pub frame: &'a [u8],
    pub check: [u8; 32],
}

/// Where the launcher is with the other side of its socket.
enum Joining {
    /// Connected, `Hello` said: what the other side says is served as it comes.
    Joined,
    /// Connected, `Hello { holds, running: false }` said for the held image, which the
    /// other side's `Welcome` gives the launcher.
    Holds,
}

struct Control<'a, 'h> {
    driver: Driver,
    /// `None` until [`Control::connect`].
    socket: Option<UnixStream>,
    joining: Joining,
    /// The program, for its `Hello`.
    program: ProgramId,
    /// The held start, until it is taken.
    held: Option<Held<'h>>,
    /// The reader thread of the socket, once there is one.
    reader: Option<std::thread::JoinHandle<()>>,
    records: &'a mut dyn Records,
    /// The slots whose stub fired, each with the thread that fired it first, while no `Need`
    /// can be said yet (the other side has not welcomed the launcher): asked once it has.
    unasked: Vec<(Slot, Thread)>,
    /// By slot: the stub waiters' rows of the threads waiting in its stub, for which one
    /// `Need` was said. Each row is answered once (`Waiters::answer`).
    needs: Vec<(Slot, Vec<u32>)>,
    /// The id of the last message this side began.
    said: u64,
    /// The first failure: no code can come any more.
    lost: Option<ControlError>,
    /// One frame on its way out.
    out: Vec<u8>,
    /// Where the program's main thread waits for what to run ([`Start`], once the daemon
    /// said the statics are defined), or for why nothing can.
    ready: Ready,
    /// What a [`Tell::Restart`] does: starts the program again in place. It returns only
    /// when it could not, with why.
    restart: fn(&str) -> String,
}

/// Where the program's main thread waits for its [`Start`], or for why there is none.
#[expect(
    clippy::disallowed_types,
    reason = "below blaze-core, whose `chan` is the one channel: the launcher's runtime links no more than std, and this channel carries one message"
)]
pub type Ready = std::sync::mpsc::SyncSender<Result<Start, String>>;

/// The channel of a launch's [`Start`]: its [`Ready`] for [`run`], and the end the program's
/// main thread waits on.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "below blaze-core, whose `chan` is the one channel: see `Ready`"
)]
pub fn ready() -> (Ready, std::sync::mpsc::Receiver<Result<Start, String>>) {
    std::sync::mpsc::sync_channel(1)
}

/// What the program's main thread runs: the addresses of its initializer statics (each
/// holds the address of a function to call first, in this order), then of its entry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Start {
    pub entry: usize,
    pub inits: Vec<usize>,
}

/// Runs the control thread's loop on this thread until [`Shared::stop`]: takes `socket`,
/// holds `held` where the launcher holds its start (module docs, "A start from the program's
/// file"), says `Hello` for `program`, then serves the other side and the threads of
/// `driver`. Gives the driver back, with the first failure if there was one.
pub fn run(
    driver: Driver,
    socket: UnixStream,
    held: Option<Held<'_>>,
    program: ProgramId,
    records: &mut dyn Records,
    ready: Ready,
    restart: fn(&str) -> String,
) -> (Driver, Result<(), ControlError>) {
    let shared = driver.shared();
    let holds = held.map(|it| it.check);
    let mut control = Control {
        driver,
        socket: None,
        joining: if holds.is_some() { Joining::Holds } else { Joining::Joined },
        program,
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
    let begun = control.connect(socket, holds, false, &shared);
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
        match control.handle(event) {
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
            return Err(ControlError::Socket { call: "write", why: "no socket is connected yet".to_owned() });
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

    /// Answers every stub that waits: no code can come, for `why` (said first, once a slot:
    /// a waiter answered LOST ends the process without words of its own).
    fn answer(&mut self, why: &str) {
        for (slot, rows) in self.needs.drain(..) {
            stub_wait::say_no_code(slot.0, why);
            for row in rows {
                self.driver.waiters().answer(row, Answer::Lost);
            }
        }
    }

    /// Serves one event. `false` is the word to stop.
    fn handle(&mut self, event: Event) -> Result<bool, ControlError> {
        match event {
            Event::Thread(told) => self.driver.told(told)?,
            Event::Need { slot, thread, row } => self.need(slot, thread, row)?,
            Event::Frame(item) => self.tell(&item)?,
            Event::Closed(why) => return Err(ControlError::Closed(why)),
            Event::Stop => return Ok(false),
        }
        Ok(true)
    }

    /// A stub fired.
    fn need(&mut self, slot: Slot, thread: Thread, row: u32) -> Result<(), ControlError> {
        if let Some(lost) = &self.lost {
            stub_wait::say_no_code(slot.0, &lost.to_string());
            self.driver.waiters().answer(row, Answer::Lost);
            return Ok(());
        }
        // Bound since the thread loaded the word: it calls through the word again.
        if matches!(self.driver.target(slot), Ok(Target::Body(_) | Target::Gate { .. })) {
            self.driver.waiters().answer(row, Answer::Bound);
            return Ok(());
        }
        match self.needs.iter_mut().find(|(waited, _)| *waited == slot) {
            Some((_, rows)) => rows.push(row),
            None => {
                // alloc: the rows in one slot's stub; a list a slot being fetched.
                self.needs.push((slot, vec![row]));
                match self.joining {
                    Joining::Joined => self.ask(None, &Ask::Need { slot, thread })?,
                    // Asked once the other side has welcomed the launcher (`Tell::Welcome`).
                    Joining::Holds => self.unasked.push((slot, thread)),
                }
            }
        }
        Ok(())
    }

    /// The other side welcomed a launcher that holds its start: every slot whose stub waits
    /// is asked for, under the thread that first fired it.
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
                stub_wait::say_no_code(slot.0, diagnostic);
                for row in self.needs.swap_remove(at).1 {
                    self.driver.waiters().answer(row, Answer::Refused);
                }
            }
            Tell::Unheld { why } => return Err(ControlError::Unheld { why: why.to_owned() }),
            Tell::Image { bytes } => {
                // The whole start in one message: what `Welcome`, `Statics`, the first `Edit`
                // and `Start` leave, copied in. A launcher that held another image is told
                // this one: the daemon's is the program's build.
                match self.joining {
                    Joining::Holds => {
                        self.joining = Joining::Joined;
                        self.held = None;
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
            for &row in waiters.iter() {
                driver.waiters().answer(row, Answer::Bound);
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
                Event::Need { slot, row, .. } => {
                    stub_wait::say_no_code(slot.0, "the launcher's control thread was stopped");
                    self.driver.waiters().answer(row, Answer::Lost);
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
