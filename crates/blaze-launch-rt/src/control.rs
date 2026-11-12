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
//! | `Welcome { slots, store, statics }` | opens the records, declares the slots |
//! | a stub's need of a slot | says `Need` once a slot, however many threads wait in its stub |
//! | `Edit { txn, binds, declare, together }` | declares, runs [`Driver::edit`], says `Applied` (and lets the stubs of the slots it bound go on) or `Failed` |
//! | `Refused { slot, diagnostic }` | gives the diagnostic to the threads in that slot's stub, which stop the program with it |
//! | `Probe`, `Restart` | not built (launcher.md, step 5): a failure that says so |
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
    /// [`Records::open`] refused the store.
    Store(String),
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
            ControlError::Store(why) => write!(f, "the store: {why}"),
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
    needs: Vec<(Slot, Vec<Sender<Bound>>)>,
    /// The id of the last message this side began.
    said: u64,
    /// The first failure: no code can come any more.
    lost: Option<ControlError>,
    /// One frame on its way out.
    out: Vec<u8>,
}

/// Runs the control thread's loop on this thread until [`Shared::stop`]: says `Hello` for
/// `program`, then serves the daemon on `socket` and the threads of `driver`. Gives the
/// driver back, with the first failure if there was one.
pub fn run(
    driver: Driver,
    socket: UnixStream,
    program: ProgramId,
    records: &mut dyn Records,
) -> (Driver, Result<(), ControlError>) {
    let shared = driver.shared();
    let mut control =
        Control { driver, socket, records, needs: Vec::new(), said: 0, lost: None, out: Vec::new() };
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
                if let Err(why) = control.driver.collect() {
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

/// The reader thread: every whole frame of `from` onto the queue, then why it ended.
fn read(mut from: UnixStream, shared: &Shared) {
    // alloc: the socket's bytes not yet a whole frame; one buffer for the thread's life.
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let why = loop {
        let (item, took) = match wire::split(&bytes) {
            Ok(Some((item, took))) => (item.to_vec(), took),
            Ok(None) => {
                // The wait: for the daemon's next bytes. It ends when the daemon writes,
                // closes, or the control thread shuts the socket down to stop.
                match from.read(&mut chunk) {
                    Ok(0) => break "the daemon closed it".to_owned(),
                    Ok(got) => bytes.extend_from_slice(&chunk[..got]),
                    Err(why) if why.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(why) => break format!("read failed: {why}"),
                }
                continue;
            }
            Err(why) => break format!("a frame was refused: {why}"),
        };
        drop(bytes.drain(..took));
        shared.send(Event::Frame(item));
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
        self.answer(&why.to_string());
        self.lost.get_or_insert(why);
    }

    /// Answers every stub that waits: no code can come, for `why`.
    fn answer(&mut self, why: &str) {
        for (_, waiters) in self.needs.drain(..) {
            for waiter in waiters {
                // A waiter is a thread blocked on this answer; were it gone, there is no one
                // to tell.
                drop(waiter.send(Bound::Lost(why.to_owned())));
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
        reply: Sender<Bound>,
    ) -> Result<(), ControlError> {
        if let Some(lost) = &self.lost {
            drop(reply.send(Bound::Lost(lost.to_string())));
            return Ok(());
        }
        // Bound since the thread loaded the word: it calls through the word again.
        if matches!(self.driver.target(slot), Ok(Target::Body(_) | Target::Gate { .. })) {
            drop(reply.send(Bound::Code));
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
            Tell::Welcome { slots, store, statics } => {
                self.records.open(store, statics).map_err(ControlError::Store)?;
                self.driver.declare(slots)?;
            }
            Tell::Edit { txn, binds, declare, together } => {
                if declare > 0 {
                    self.driver.declare(declare)?;
                }
                // alloc: the edit's binds out of the frame, for the length of the edit.
                let binds: Vec<wire::Bind> = binds.iter().collect();
                match self.driver.edit(txn, &binds, together, &mut *self.records) {
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
                    drop(waiter.send(Bound::Refused(diagnostic.to_owned())));
                }
            }
            Tell::Probe { .. } => {
                return Err(ControlError::NotBuilt { what: "Probe (launcher.md, step 5)" });
            }
            Tell::Restart { .. } => {
                return Err(ControlError::NotBuilt { what: "Restart (launcher.md, step 5)" });
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
                drop(waiter.send(Bound::Code));
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
                    drop(reply.send(Bound::Lost(why.to_owned())));
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
