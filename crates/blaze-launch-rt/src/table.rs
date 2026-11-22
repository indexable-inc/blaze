//! What a program thread shares with the control thread: the slot words, the published
//! epoch, and the one queue everything a thread says goes into.
//!
//! A program thread does three things and no other. It **loads a word** and jumps to it
//! (every call; in machine code, [`Shared::target`] for a caller written in Rust). It **fires a
//! stub** ([`resolve`]): asks the control thread for a slot's code and waits. It **reports**
//! at a safepoint ([`Mutator::safepoint`]): the epoch it loaded, then the frames of its own
//! stack that lie in the code area. It never writes a word, the epoch or code.
//!
//! A thread is known from before it runs: its parent calls [`Shared::birth`], which loads the
//! epoch and announces the birth, and only then starts it; the thread turns the [`Birth`]
//! into its [`Mutator`], whose drop says the thread ended. This is the order the core asks
//! of its driver (`blaze_launch::launch`, "The driver's part").

use core::cell::Cell;
use core::ffi::c_void;
use core::marker::PhantomData;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use blaze_launch::{Frame, FrameKind, Slot, Thread};
use blaze_queue::SegQueue;

use blaze_core::wait::{Clock, Deadline, Monotonic};

use crate::stub_wait::{self, Answer, Waiters};
use crate::{arch, sys};

/// What a thread tells the core.
#[derive(Debug)]
pub enum Told {
    /// A thread will begin, its parent having loaded `epoch`.
    Began { thread: Thread, epoch: u64 },
    Ended { thread: Thread },
    /// A thread's stack as it was after the thread loaded `epoch`.
    Report { thread: Thread, epoch: u64, frames: Vec<Frame>, complete: bool },
}

/// What the control thread's queue holds.
#[derive(Debug)]
pub enum Event {
    Thread(Told),
    /// A stub fired: `thread` waits in row `row` of the stub waiters for `slot`'s code, and
    /// is answered once, through `Waiters::answer` (stub_wait.rs).
    Need { slot: Slot, thread: Thread, row: u32 },
    /// One frame's item from the daemon.
    Frame(Vec<u8>),
    /// The daemon's socket ended, and why.
    Closed(String),
    /// The control thread is to return.
    Stop,
}


/// The words, the epoch and the queue of one launcher.
pub struct Shared {
    /// The address of slot 0's word, in the launcher's mapping: word `n` is 8 bytes times `n`
    /// further. Stored only by the control thread (`sys::Code::store`), and only for a
    /// declared slot; the word of a slot not yet declared is zero.
    table: usize,
    /// How many words the table has.
    slots: u32,
    /// The epoch last published. Stored only by the control thread.
    pub(crate) epoch: AtomicU64,
    /// How many batches wait to be cleared: a safepoint reports only while this is not zero.
    pub(crate) waiting: AtomicU64,
    /// The last thread id given out. 0 is no thread's.
    threads: AtomicU64,
    /// The addresses of the code area.
    area: Range<usize>,
    /// The address of the stub region.
    stubs: usize,
    /// The address of slot 0's entry thunk.
    thunks: usize,
    /// What threads say to the control thread, in the order they said it. A queue, not a
    /// channel: a program thread says things here at any point of its life, its end included
    /// (`sys::Handoff` says why that rules a channel out). A stub's need is not here: it is a
    /// row of `waiters`, which allocates nothing.
    queue: SegQueue<Event, std::alloc::Global>,
    /// The control thread's bell: a counter each [`Shared::ring`] adds one to and wakes
    /// (`blaze_core::wait::wake_shared`), which the control thread sleeps on in
    /// [`Shared::wait`]. One word for the queue and the waiters' ring alike, so a stub (which
    /// may run in a signal handler) rings it with an atomic add and a kernel wake: no lock,
    /// no allocation.
    bell: AtomicU64,
    /// The rows stubs wait in, reserved at the start (stub_wait.rs, THREADS.md D6).
    pub(crate) waiters: Waiters,
    /// The process the launcher was made in: a forked child of it has these words and no
    /// control thread.
    pid: u32,
}

thread_local! {
    /// The id of the thread, once it began as a [`Mutator`]; else 0.
    static CURRENT: Cell<u64> = const { Cell::new(0) };
}

impl Shared {
    /// `table` is the address of `slots` words, `stubs` that of the stub region's head and
    /// `thunks` that of slot 0's entry thunk, all in a `sys::Code` mapping (which is never
    /// unmapped); `area` is the code area's addresses.
    /// `waiters` are the rows a stub waits in (`Waiters::reserve`).
    pub(crate) fn new(
        slots: u32,
        table: usize,
        stubs: usize,
        thunks: usize,
        area: Range<usize>,
        waiters: Waiters,
    ) -> Shared {
        Shared {
            table,
            slots,
            epoch: AtomicU64::new(0),
            waiting: AtomicU64::new(0),
            threads: AtomicU64::new(0),
            area,
            stubs,
            thunks,
            queue: SegQueue::new(),
            bell: AtomicU64::new(0),
            waiters,
            pid: std::process::id(),
        }
    }

    /// The address of `slot`'s word, which is the slot's for the life of the process: what
    /// a call's instructions name. `None` past the table's end.
    pub fn word(&self, slot: Slot) -> Option<usize> {
        (slot.0 < self.slots).then(|| self.table + slot.0 as usize * 8)
    }

    /// What a call of `slot` loads from its word: the address it jumps to. Zero for a slot
    /// not yet declared, which nothing calls. `None` past the table's end.
    pub fn target(&self, slot: Slot) -> Option<usize> {
        let word = self.word(slot)? as *mut usize;
        // SAFETY: `word` is one of the table's words (`Shared::word`): in a mapping that is
        // never unmapped, so valid for any lifetime; aligned for a `usize` (the mapping is
        // page aligned and a word is 8 bytes); and only ever touched atomically, by this
        // load, by the loads of machine code and by the control thread's
        // `sys::Code::store`. This thread only loads: it could not store (the page is not
        // writable at this address, or not on this thread).
        let word = unsafe { AtomicUsize::from_ptr(word) };
        Some(word.load(Ordering::Acquire))
    }

    /// The address of `slot`'s entry thunk: the function's address, as a `fn` pointer holds
    /// it. Code is there once the slot is declared. `None` past the table's end.
    pub fn thunk(&self, slot: Slot) -> Option<usize> {
        (slot.0 < self.slots).then(|| self.thunks + slot.0 as usize * arch::THUNK)
    }

    /// The address of `slot`'s stub.
    pub(crate) fn stub(&self, slot: Slot) -> usize {
        self.stubs + arch::STUB_HEAD + slot.0 as usize * arch::STUB
    }

    /// The epoch last published.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Puts `event` on the control thread's queue and rings its bell.
    pub(crate) fn send(&self, event: Event) {
        self.queue.push(event);
        self.ring();
    }

    /// Puts `event` on the queue without ringing: for a run of events one [`Shared::ring`]
    /// follows (the frames of one read of the daemon's socket: a launcher's start is some
    /// two hundred frames, and a wake of the control thread for each was most of its time).
    pub(crate) fn push(&self, event: Event) {
        self.queue.push(event);
    }

    /// Rings the control thread's bell: for what [`Shared::push`] queued, and for a row a stub
    /// pushed onto the waiters' ring. An atomic add and a kernel wake: what a stub in a signal
    /// handler may call.
    pub(crate) fn ring(&self) {
        // Release: what was pushed before is seen by the control thread's acquire of the bell.
        self.bell.fetch_add(1, Ordering::Release);
        blaze_core::wait::wake_shared(&self.bell);
    }

    /// The next event, when one is there: the queue first (a thread's birth before its need),
    /// then a row a stub pushed onto the waiters' ring.
    pub(crate) fn next(&self) -> Option<Event> {
        if let Some(event) = self.queue.pop() {
            return Some(event);
        }
        self.waiters.popped().map(|it| Event::Need { slot: Slot(it.slot), thread: Thread(it.thread), row: it.row })
    }

    /// How long the control thread sleeps before it looks again with nothing rung: a look,
    /// not a retry (an idle launcher waits as long as its program runs).
    const IDLE_LOOK: core::time::Duration = core::time::Duration::from_secs(60);

    /// Waits for the next event: the control thread's one wait while it has nothing to do.
    pub(crate) fn wait(&self) -> Event {
        let clock = Monotonic::new();
        loop {
            // The bell is read before the look: a ring after this load changes the word, so
            // the kernel's wait below returns at once rather than sleep past it.
            let rung = self.bell.load(Ordering::Acquire);
            if let Some(event) = self.next() {
                return event;
            }
            // The wait: for a thread to say something (a report, a stub's row, a frame of the
            // daemon's, the word to stop), to a look every `IDLE_LOOK`. The kernel compares
            // the bell's low half with `rung` as it blocks.
            let deadline = Deadline::after(clock.now(), Self::IDLE_LOOK);
            let waited = blaze_core::wait::wait_shared(
                "the launcher's control thread: its next event (table::Shared::wait)",
                &self.bell,
                rung as u32,
                deadline,
                &clock,
            );
            if let Err(why) = waited {
                stop_control(&why);
            }
        }
    }

    /// Tells the control thread to return from its loop.
    pub fn stop(&self) {
        self.send(Event::Stop);
    }

    /// A thread that is about to be started: loads the epoch, announces the birth, and gives
    /// what the new thread begins with. The caller starts the thread only after this returns.
    pub fn birth(self: &Arc<Self>) -> Birth {
        let epoch = self.epoch();
        let thread = Thread(self.threads.fetch_add(1, Ordering::Relaxed) + 1);
        self.send(Event::Thread(Told::Began { thread, epoch }));
        Birth { shared: Some(Arc::clone(self)), thread }
    }
}

/// A thread announced and not yet running. Dropped unused, it says the thread ended.
pub struct Birth {
    /// `None` once the thread began.
    shared: Option<Arc<Shared>>,
    thread: Thread,
}

impl Birth {
    /// Called on the new thread, first thing.
    pub fn begin(mut self) -> Mutator {
        let thread = self.thread;
        CURRENT.with(|it| it.set(thread.0));
        match self.shared.take() {
            Some(shared) => Mutator { shared, thread, clear: Cell::new(None), here: PhantomData },
            // `shared` is `Some` from `Shared::birth` until this call, which consumes `self`.
            None => unreachable!("a birth begins once"),
        }
    }
}

impl Drop for Birth {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            shared.send(Event::Thread(Told::Ended { thread: self.thread }));
        }
    }
}

/// A running program thread's side of the launcher. Not `Send`: a report is of the stack of
/// the thread that makes it.
pub struct Mutator {
    shared: Arc<Shared>,
    thread: Thread,
    /// The epoch of the last report that was complete and had no frame in the area: until the
    /// epoch moves, another report could clear nothing more.
    clear: Cell<Option<u64>>,
    here: PhantomData<*const ()>,
}

impl Mutator {
    pub fn thread(&self) -> Thread {
        self.thread
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// A safepoint: when a batch waits, loads the epoch, walks this thread's stack and
    /// reports the frames in the code area. One load and a branch when nothing waits.
    pub fn safepoint(&self) {
        if self.shared.waiting.load(Ordering::Relaxed) == 0 {
            return;
        }
        // The epoch first, the walk after: the report is of a stack as it was once every
        // store that unlinked what the epoch stamps was done.
        let epoch = self.shared.epoch();
        if self.clear.get() == Some(epoch) {
            return;
        }
        // alloc: the frames found in the area, handed to the control thread; few, and only
        // while a batch waits.
        let mut frames = Vec::new();
        let complete = walk(&self.shared.area, &mut frames);
        if complete && frames.is_empty() {
            self.clear.set(Some(epoch));
        }
        let thread = self.thread;
        self.shared.send(Event::Thread(Told::Report { thread, epoch, frames, complete }));
    }
}

impl Drop for Mutator {
    fn drop(&mut self) {
        // This may run in a destructor of the dying thread, after its thread-locals are
        // gone: nothing here may need one (`sys::Handoff`).
        let _ = CURRENT.try_with(|it| it.set(0));
        self.shared.send(Event::Thread(Told::Ended { thread: self.thread }));
    }
}

/// The most frames a walk follows before it gives up (and so holds everything).
const DEEPEST: usize = 1 << 16;

/// Walks the calling thread's stack by its frame records and pushes each return address that
/// lies in `area`, as an offset in it. Gives whether the walk reached the stack's outermost
/// frame; a walk that did not holds everything, by the core's rule.
///
/// The walk is right where every frame keeps a frame record (the caller's frame pointer,
/// then the return address): always on AArch64 macOS, whose ABI requires one, and wherever
/// the code under it is built to keep them. Step 3 of launcher.md replaces it with the
/// records' unwind entries, which need no frame pointer.
#[inline(never)]
pub(crate) fn walk(area: &Range<usize>, frames: &mut Vec<Frame>) -> bool {
    let Ok((low, high)) = sys::stack() else {
        return false;
    };
    let mut record = sys::frame_pointer();
    for _ in 0..DEEPEST {
        // The thread's first frame has no caller: its record's frame pointer is zero.
        if record == 0 {
            return true;
        }
        let aligned = record % core::mem::align_of::<usize>() == 0;
        if !aligned || record < low || record > high.saturating_sub(16) {
            return false;
        }
        let at = record as *const usize;
        // SAFETY: `record..record + 16` is inside this thread's own stack (`low..high`,
        // checked above) and aligned for a `usize`; a live thread's stack is mapped and
        // readable for its whole length. Read volatile: other frames own these bytes.
        let next = unsafe { at.read_volatile() };
        // SAFETY: as above, for the record's second word.
        let returns = unsafe { at.add(1).read_volatile() };
        if area.start < returns && returns <= area.end {
            let at = (returns - area.start) as u64;
            frames.push(Frame { at, kind: FrameKind::Return });
        }
        // A caller's record is above its callee's: anything else is not a chain of records.
        if next != 0 && next <= record {
            return false;
        }
        record = next;
    }
    false
}

/// What a stub calls, through `arch::resolver`: asks the control thread for the code of the
/// slot whose stub is at `stub` and waits for it; gives the address of the slot's word, which
/// the resolver jumps through. When no code can come it stops the process, naming why
/// (launcher.md, section 4: a stub that fires on a function with an error stops the program
/// with the diagnostic).
pub(crate) extern "C" fn resolve(stub: usize, context: *const c_void) -> *const usize {
    // SAFETY: `context` is the pointer `Driver::new` wrote into the stub region's head: an
    // `Arc<Shared>` whose count it leaked for exactly this, so the `Shared` lives as long as
    // the process, and a `Shared` is `Sync`.
    let shared = unsafe { &*context.cast::<Shared>() };
    let slot = (stub - shared.stubs - arch::STUB_HEAD) / arch::STUB;
    let Some(word) = u32::try_from(slot).ok().and_then(|slot| shared.word(Slot(slot))) else {
        stop(slot, "its stub is not one of the launcher's");
    };
    // A forked child of the program has the parent's words and stubs and none of its
    // threads: no control thread would ever answer. It stops and says so, where it would
    // otherwise wait for ever (a `fork` without an `exec` that reaches a function the parent
    // never called; binding in the child is not built).
    if std::process::id() != shared.pid {
        const WHY: &[u8] = b"blaze: a function was first called in a forked child of the program, \
            which has no control thread to ask the daemon for its code (the launcher does not \
            serve a fork yet): the child stops\n";
        // A forked child of a threaded process may hold any lock another thread held at the
        // fork, so it says this with one `write` and ends with `_exit`, as a signal handler
        // would.
        let _ = blaze_sys::posix::io::write(blaze_sys::Fd::STDERR, WHY);
        blaze_sys::posix::signal::exit_now(127)
    }
    // Nothing below may need a thread-local of the launcher's: a stub can fire while its
    // thread is ending, after they are gone (`sys::Handoff`). And nothing below allocates
    // or takes a lock but the safepoint's report: a stub can fire inside a signal handler
    // (THREADS.md D6, stub_wait.rs).
    let thread = Thread(CURRENT.try_with(Cell::get).unwrap_or(0));
    let row = match shared.waiters.claim(slot as u32, thread.0) {
        Ok(row) => row,
        Err(full) => full.stop(),
    };
    shared.ring();
    // The wait: this thread, for the control thread's answer about `slot`, to the stub bound
    // (rule 18): the control thread answers every row it pops, with code, a refusal or why
    // none can come; past the bound the process stops, naming the slot.
    let clock = Monotonic::new();
    let deadline = Deadline::after(clock.now(), stub_wait::STUB_BOUND);
    let answer = match shared.waiters.wait(row, deadline, &clock) {
        Ok(answer) => answer,
        Err(overran) => overran.stop(),
    };
    shared.waiters.release(row);
    match answer {
        Answer::Bound => {
            // The resolver's exit is a safepoint (launcher.md, section 1): a thread that
            // began as a mutator reports its stack here while a batch waits.
            if thread.0 != 0 && shared.waiting.load(Ordering::Relaxed) != 0 {
                let epoch = shared.epoch();
                // alloc: the frames found in the area; few, and only while a batch waits.
                let mut frames = Vec::new();
                let complete = walk(&shared.area, &mut frames);
                shared.send(Event::Thread(Told::Report { thread, epoch, frames, complete }));
            }
            word as *const usize
        }
        // The control thread said why (`stub_wait::say_no_code`) before it answered.
        Answer::Refused | Answer::Lost => stub_wait::stop_said(),
    }
}

/// Stops the process: slot `slot` was called and has no code, for `why`. One `write` and
/// `_exit`, as a stub in a signal handler may (`stub_wait::Line`).
fn stop(slot: usize, why: &str) -> ! {
    stub_wait::Line::new()
        .text("blaze: the function of slot ")
        .number(slot as u64)
        .text(" was called and has no code: ")
        .text(why)
        .text("\n")
        .stop()
}

/// The control thread's kernel wait failed: no stub could be answered any more, so the
/// process stops, naming the wait (rule 18). The control thread is no signal handler.
fn stop_control(why: &blaze_core::wait::WaitError) -> ! {
    use std::io::Write;

    let mut to = std::io::stderr().lock();
    // The process ends below whether or not its last words could be written.
    drop(writeln!(to, "blaze: the launcher's control thread could not wait for its next event: {why}"));
    std::process::exit(101)
}
