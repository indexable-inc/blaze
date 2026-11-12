//! What a program thread shares with the control thread: the slot words, the published
//! epoch, and the one queue everything a thread says goes into.
//!
//! A program thread does three things and no other. It **loads a word** and jumps to it
//! (every call; in machine code, [`Shared::word`] for a caller written in Rust). It **fires a
//! stub** ([`resolve`]): asks the control thread for a slot's code and waits. It **reports**
//! at a safepoint ([`Mutator::safepoint`]): the epoch it loaded, then the frames of its own
//! stack that lie in the code area. It never writes a word, the epoch or code.
//!
//! A thread is known from before it runs: its parent calls [`Shared::birth`], which loads the
//! epoch and announces the birth, and only then starts it; the thread turns the [`Birth`]
//! into its [`Mutator`], whose drop says the thread ended. This is the order the core asks
//! of its driver (`blaze_launch::launch`, "The driver's part").

use core::cell::Cell;
use core::marker::PhantomData;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use blaze_launch::{Frame, FrameKind, Slot, Thread};
use crossbeam_channel::Sender;

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
    /// A stub fired: `thread` waits on `reply` for `slot`'s code.
    Need { slot: Slot, thread: Thread, reply: Sender<Bound> },
    /// One frame's item from the daemon.
    Frame(Vec<u8>),
    /// The daemon's socket ended, and why.
    Closed(String),
    /// The control thread is to return.
    Stop,
}

/// The answer a stub waits for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Bound {
    /// The slot's word names code: call through it.
    Code,
    /// The function does not compile: the program stops with the daemon's diagnostic.
    Refused(String),
    /// No code can come: the daemon is gone or the control thread failed, and why.
    Lost(String),
}

/// The words, the epoch and the queue of one launcher.
pub struct Shared {
    /// By slot: the address a call of it runs. Stored only by the control thread.
    pub(crate) words: Box<[AtomicUsize]>,
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
    inbox: Sender<Event>,
}

thread_local! {
    /// The id of the thread, once it began as a [`Mutator`]; else 0.
    static CURRENT: Cell<u64> = const { Cell::new(0) };
}

impl Shared {
    pub(crate) fn new(
        slots: u32,
        area: Range<usize>,
        stubs: usize,
        inbox: Sender<Event>,
    ) -> Shared {
        // Every word names its stub from the start, so no load of a word ever finds anything
        // but code, whatever the daemon has declared so far.
        let words = (0..slots as usize)
            .map(|slot| AtomicUsize::new(stubs + arch::STUB_HEAD + slot * arch::STUB))
            .collect();
        Shared {
            words,
            epoch: AtomicU64::new(0),
            waiting: AtomicU64::new(0),
            threads: AtomicU64::new(0),
            area,
            stubs,
            inbox,
        }
    }

    /// The word of `slot`: what a call loads and jumps to. `None` past the table's end.
    pub fn word(&self, slot: Slot) -> Option<&AtomicUsize> {
        self.words.get(slot.0 as usize)
    }

    /// The address of `slot`'s stub.
    pub(crate) fn stub(&self, slot: Slot) -> usize {
        self.stubs + arch::STUB_HEAD + slot.0 as usize * arch::STUB
    }

    /// The epoch last published.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Puts `event` on the control thread's queue. The queue is gone only when the `Driver`
    /// is, and with it the table: there is then no one to tell and nothing to keep right.
    pub(crate) fn send(&self, event: Event) {
        drop(self.inbox.send(event));
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
        CURRENT.with(|it| it.set(0));
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
pub(crate) extern "C" fn resolve(stub: usize, context: *const Shared) -> *const AtomicUsize {
    // SAFETY: `context` is the pointer `Driver::new` wrote into the stub region's head: an
    // `Arc<Shared>` whose count it leaked for exactly this, so the `Shared` lives as long as
    // the process, and a `Shared` is `Sync`.
    let shared = unsafe { &*context };
    let slot = (stub - shared.stubs - arch::STUB_HEAD) / arch::STUB;
    let Some(word) = shared.words.get(slot) else {
        stop(slot, "its stub is not one of the launcher's");
    };
    let (reply, bound) = crossbeam_channel::bounded(1);
    let thread = Thread(CURRENT.with(Cell::get));
    shared.send(Event::Need { slot: Slot(slot as u32), thread, reply });
    // The wait: this thread, for the control thread's answer about `slot`. It has no clock:
    // it ends when the daemon answers the need (code, or a refusal) or its socket closes,
    // each of which the control thread turns into an answer here.
    match bound.recv() {
        Ok(Bound::Code) => word,
        Ok(Bound::Refused(diagnostic)) => stop(slot, &diagnostic),
        Ok(Bound::Lost(why)) => stop(slot, &why),
        Err(_) => stop(slot, "the launcher's control thread is gone"),
    }
}

/// Stops the process: slot `slot` was called and has no code, for `why`.
fn stop(slot: usize, why: &str) -> ! {
    use std::io::Write;

    let mut to = std::io::stderr().lock();
    // The process ends below whether or not its last words could be written.
    drop(writeln!(to, "blaze: the function of slot {slot} was called and has no code: {why}"));
    std::process::exit(101)
}
