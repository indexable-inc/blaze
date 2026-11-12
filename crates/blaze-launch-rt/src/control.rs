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
//! | (start) | says `Hello` |
//! | `Welcome { slots }` | declares the slots |
//! | `Start { entry, inits }` | gives the thread that waits to run the program the addresses of its initializers and its entry |
//! | `Record { id, bytes }` | hands the record to [`Records`], which keeps it for the message that names it |
//! | `Statics { defs }` | has [`Records`] give each static its memory and its value |
//! | a stub's need of a slot | says `Need` once a slot, however many threads wait in its stub |
//! | `Edit { txn, binds, declare, together }` | declares, runs [`Driver::edit`], says `Applied` (and lets the stubs of the slots it bound go on) or `Failed` |
//! | `Refused { slot, diagnostic }` | gives the diagnostic to the threads in that slot's stub, which stop the program with it |
//! | `Restart { txn, why }` | says `Restarting` and starts the program again in place (the caller's `restart`) |
//! | `Probe` | not built (launcher.md, step 5): a failure that says so |
//!
//! A failure (the socket ends, a message is not of the schema, the driver breaks) does not
//! end the loop: program threads may still be running and reporting, and one may yet fire a
//! stub. The first failure is kept and returned when the loop is stopped; from then on every
//! stub that fires or waits is answered `Lost` with it, which stops the program by name
//! (rule `no-fallback`: nothing runs on quietly without its daemon).
//!
//! The waits of this module, each on a thread of its own and none on a fiber: the control
//! thread on its queue ([`Driver::wait`]); the reader in `read`; a write of a frame to the
//! socket (the daemon reads as fast as it is asked). The launcher has no profile to open
//! spans in yet, so these are named here and not in a trace.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

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
const ARCH: wire::Arch = wire::Arch::Aarch64;
#[cfg(target_arch = "x86_64")]
const ARCH: wire::Arch = wire::Arch::X86_64;
#[cfg(target_os = "macos")]
const OS: wire::Os = wire::Os::Macos;
#[cfg(target_os = "linux")]
const OS: wire::Os = wire::Os::Linux;

struct Control<'a> {
    driver: Driver,
    socket: UnixStream,
    records: &'a mut dyn Records,
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

/// Runs the control thread's loop on this thread until [`Shared::stop`]: says `Hello` for
/// `program`, then serves the daemon on `socket` and the threads of `driver`. Gives the
/// driver back, with the first failure if there was one.
pub fn run(
    driver: Driver,
    socket: UnixStream,
    program: ProgramId,
    records: &mut dyn Records,
    ready: Sender<Result<Start, String>>,
    restart: fn(&str) -> String,
) -> (Driver, Result<(), ControlError>) {
    let shared = driver.shared();
    let mut control = Control {
        driver,
        socket,
        records,
        needs: Vec::new(),
        said: 0,
        lost: None,
        out: Vec::new(),
        ready,
        restart,
    };
    let reader = match control.begin(program, &shared) {
        Ok(reader) => Some(reader),
        Err(why) => {
            control.lose(why);
            None
        }
    };
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
    control.end(reader)
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

impl Control<'_> {
    /// Starts the reader and says `Hello`.
    fn begin(
        &mut self,
        program: ProgramId,
        shared: &std::sync::Arc<Shared>,
    ) -> Result<std::thread::JoinHandle<()>, ControlError> {
        let socket = |call: &'static str| {
            move |why: std::io::Error| ControlError::Socket { call, why: why.to_string() }
        };
        let page = sys::page().map_err(Error::Sys)?;
        let from = self.socket.try_clone().map_err(socket("try_clone"))?;
        let theirs = std::sync::Arc::clone(shared);
        let reader = std::thread::Builder::new()
            .name("blaze-launch-read".to_owned())
            .spawn(move || read(from, &theirs))
            .map_err(socket("starting the reader thread"))?;
        let hello = Ask::Hello {
            program,
            pid: u64::from(std::process::id()),
            arch: ARCH,
            os: OS,
            page: page as u32,
        };
        // The reader is running: a failure from here on is kept by `run`, and `end` joins it.
        if let Err(why) = self.ask(None, &hello) {
            self.lose(why);
        }
        Ok(reader)
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
        // The wait: for the socket to take the frame.
        self.socket
            .write_all(&self.out)
            .map_err(|why| ControlError::Socket { call: "write", why: why.to_string() })
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
    fn handle(&mut self, event: Event) -> Result<bool, ControlError> {
        match event {
            Event::Thread(told) => self.driver.told(told)?,
            Event::Need { slot, thread, reply } => self.need(slot, thread, reply)?,
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
        match self.needs.iter_mut().find(|(waited, _)| *waited == slot) {
            Some((_, waiters)) => waiters.push(reply),
            None => {
                // alloc: the threads in one slot's stub; a list a slot being fetched.
                self.needs.push((slot, vec![reply]));
                self.ask(None, &Ask::Need { slot, thread })?;
            }
        }
        Ok(())
    }

    /// A message of the daemon's.
    fn tell(&mut self, item: &[u8]) -> Result<(), ControlError> {
        let (id, tell) = Tell::decode(item).map_err(ControlError::Wire)?;
        match tell {
            Tell::Welcome { slots } => {
                self.driver.declare(slots)?;
            }
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
            Tell::Unheld { why } => return Err(ControlError::Unheld { why: why.to_owned() }),
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
    fn end(
        mut self,
        reader: Option<std::thread::JoinHandle<()>>,
    ) -> (Driver, Result<(), ControlError>) {
        // The reader's `read` returns once the socket is shut down; a socket the daemon
        // already closed refuses the shutdown (ENOTCONN), and the reader has ended.
        drop(self.socket.shutdown(std::net::Shutdown::Both));
        if let Some(reader) = reader
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
