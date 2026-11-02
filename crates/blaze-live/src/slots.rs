//! The runtime of a program blaze-link linked: it takes patches while it runs.
//!
//! Every function a patch can replace is called through a slot of a jump table: a call loads
//! the table cell, then the slot. A patch's code is already where the linker placed it: the
//! program runs from the daemon's code memory (blaze-inproc), and the commit wrote the code
//! there. The switch checks that code against the message's hashes, writes the patch's data,
//! then publishes a table with its slots changed in one atomic store to the cell, so every
//! thread's next call through any slot sees all of the patch or none of it. Going back to a
//! kept generation (the last [`KEPT`](switch::KEPT)) is one store again: when the linker says
//! so ([`crate::patch::Message::RollBack`]), or when code of a generation panics under
//! [`Live::guard`].
//!
//! The descriptor ([`crate::patch::Descriptor`]) says where everything is. Each commit
//! announces its generation in a word of the shared control page, so [`Live::poll`] is one
//! load while nothing came. When something did, the program asks the daemon for the message
//! that takes it on over blaze-inproc's channel ([`crate::patch::ask`]), a piece at a time
//! into a buffer kept from message to message, and tells it what became of it.
//!
//! An image linked with an execution log ([`exec`]) keeps its cell on the log's table, and a
//! switch stores only the header's current table; [`publish`] writes one slot into every
//! generation's table, for lazy compilation's code placed after the link.
//!
//! Linux on x86-64 and AArch64, macOS on AArch64 and Windows on x86-64, in a program the
//! daemon started; elsewhere, or in a program started by hand, [`Live::start`] says so.

use std::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use blaze_inproc::runtime::{self, Attached};

use crate::ask::{self, Kinds};
use crate::patch::{self, Answer, DESCRIPTOR, DESCRIPTOR_MAGIC, Descriptor, Message};
pub use exec::publish;
use os::backtrace;
use switch::{Layout, Switch};

mod column;
pub mod exec;
mod hook;
mod image;
#[cfg(all(test, blaze_check))]
mod model;
mod switch;
#[cfg(test)]
mod tests;

#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
))]
mod unix;

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod linux;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
use linux as os;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use macos as os;

#[cfg(all(windows, target_arch = "x86_64"))]
mod windows;
#[cfg(all(windows, target_arch = "x86_64"))]
use windows as os;

#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
)))]
mod os {
    use std::ops::Range;

    use super::switch::{Dwarf, Layout, Memory, Writes};

    const PLATFORMS: &str =
        "live patches run on Linux x86-64 and AArch64, macOS AArch64 and Windows x86-64";

    pub(super) enum Os {}

    impl Os {
        pub(super) fn new(_: &Layout) -> Result<Os, String> {
            // alloc: error path, once at start on a platform it doesn't run on
            Err(PLATFORMS.to_owned())
        }
    }

    impl Memory for Os {
        const DWARF: Dwarf = Dwarf::Not;
        const SEH: bool = false;

        fn cell(&self, _: u64) -> u64 {
            match *self {}
        }

        fn in_image(&self, _: &Range<u64>) -> Result<(), String> {
            match *self {}
        }

        fn bytes(&self, _: &Range<u64>) -> Result<&'static [u8], String> {
            match *self {}
        }

        fn loaders(&self, _: u64, _: u64) -> &[std::sync::atomic::AtomicU64] {
            match *self {}
        }

        fn data(&mut self, _: &Writes<'_>) -> Result<(), String> {
            match *self {}
        }

        fn frames(&mut self, _: &[u64]) -> Result<(), String> {
            match *self {}
        }

        fn functions(&mut self, _: &[(u64, u64, u64)]) -> Result<(), String> {
            match *self {}
        }

        fn publish(&mut self, _: u64, _: u64) -> Result<(), String> {
            match *self {}
        }
    }

    pub(super) fn descriptor() -> Result<(u64, &'static [u8]), String> {
        // alloc: error path, on a platform it doesn't run on
        Err(PLATFORMS.to_owned())
    }

    pub(super) fn in_image_of(_: &Range<u64>, _: u64) -> Result<(), String> {
        // alloc: error path, on a platform it doesn't run on
        Err(PLATFORMS.to_owned())
    }

    pub(super) fn backtrace(_: &mut Vec<u64>) {}

    pub(super) const GRAIN: usize = 64 << 10;

    /// No address space is reserved here: the switch's columns, and so the switch, can't be
    /// made.
    pub(super) enum Reserved {}

    impl Reserved {
        pub(super) fn new(_: usize) -> Result<Reserved, String> {
            // alloc: error path, once at start on a platform it doesn't run on
            Err(PLATFORMS.to_owned())
        }

        pub(super) fn base(&self) -> u64 {
            match *self {}
        }

        pub(super) fn len(&self) -> usize {
            match *self {}
        }

        pub(super) fn committed(&self) -> usize {
            match *self {}
        }

        pub(super) fn commit(&mut self, _: usize) -> Result<(), String> {
            match *self {}
        }
    }
}

/// What became of a message, or of a panic under [`Live::guard`].
#[derive(PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The program runs `generation` now.
    Applied { generation: u64, functions: u64, statics: u64 },
    /// The program left `generation` for `to`, for this reason: the linker asked, or a panic
    /// in its code (`panicked at <file>:<line>:<column>: <message>`).
    RolledBack { generation: u64, to: u64, why: String },
    /// The edit needs the program started again, for this reason.
    Restart { generation: u64, why: String },
    /// The message could not be taken, for this reason; the program runs on as it was.
    Declined { generation: u64, why: String },
    /// What became of a message or a panic, which the daemon was not told, for this reason.
    Unanswered { outcome: Box<Outcome>, why: String },
    /// The channel to the daemon failed, for this reason: no patch after `generation` is
    /// taken.
    Disconnected { generation: u64, why: String },
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Applied { generation, functions, statics } => {
                write!(f, "generation {generation} applied: {functions} functions")?;
                if *statics != 0 {
                    write!(f, ", {statics} statics")?;
                }
                Ok(())
            }
            Outcome::RolledBack { generation, to, why } => {
                write!(f, "generation {generation} rolled back to {to}: {why}")
            }
            Outcome::Restart { generation, why } => {
                write!(f, "restart needed at generation {generation}: {why}")
            }
            Outcome::Declined { generation, why } => {
                write!(f, "generation {generation} not applied: {why}")
            }
            Outcome::Unanswered { outcome, why } => {
                write!(f, "{outcome}; the daemon was not told: {why}")
            }
            Outcome::Disconnected { generation, why } => {
                write!(f, "no patch after generation {generation} is taken: {why}")
            }
        }
    }
}

/// Whether a [`Live`] was started in this process: the image has one table cell.
static STARTED: AtomicBool = AtomicBool::new(false);

/// How long [`Live::apply_as_they_come`]'s thread sleeps for an announcement before it looks
/// again: where the daemon's announce wakes it (Linux, macOS) only a lost wake waits this
/// long; on Windows, where it can't, this is how late a patch is taken at most.
const LOOK_EVERY: Duration = Duration::from_millis(16);

/// The asks of a message.
const PATCHES: Kinds =
    Kinds { ask: patch::ask::PATCH_ASK, piece: patch::ask::PATCH, none: patch::ask::NO_PATCH };

/// A program taking patches.
pub struct Live {
    switch: Switch<os::Os>,
    attached: &'static Attached,
    /// What the daemon had announced and the generation the program was at when it last
    /// asked.
    looked: Option<(u64, u64)>,
    /// Why the channel failed, while [`Outcome::Disconnected`] is still to be said.
    lost: Option<String>,
    /// Whether the channel failed: nothing is asked any more.
    cut: bool,
    /// The bytes of the last message, its buffer kept for the next.
    message: Vec<u8>,
    /// The bytes of the last answer, its buffer kept for the next.
    answer: Vec<u8>,
}

impl Live {
    /// Finds this image's descriptor and the daemon's channel, and installs the panic hook
    /// [`Live::guard`] needs. It reserves about 435 MiB of address space for the log of
    /// generations, which costs memory only as patches fill it; under a `ulimit -v` that leaves
    /// less, it fails naming the call and the size. A program the daemon did not start (no
    /// start hook ran, or none attached) fails saying why: its code is the file's, which no
    /// patch reaches.
    pub fn start() -> Result<Live, String> {
        if STARTED.swap(true, Ordering::AcqRel) {
            // alloc: error path
            return Err("blaze-live's slots were started already in this program".to_owned());
        }
        let live = Live::open();
        if live.is_err() {
            STARTED.store(false, Ordering::Release);
        }
        live
    }

    fn open() -> Result<Live, String> {
        let attached = runtime::attach()
            // alloc: error path
            .map_err(|it| {
                format!("patches come only to a program blaze started on its code memory: {it}")
            })?;
        let (at, bytes) = os::descriptor()?;
        let descriptor = Descriptor::decode(bytes).ok_or_else(|| {
            // alloc: error path
            format!(
                "{DESCRIPTOR} at {at:#x} does not start with {}",
                // alloc: error path
                String::from_utf8_lossy(&DESCRIPTOR_MAGIC)
            )
        })?;
        let layout = Layout::new(&descriptor, at)?;
        let memory = os::Os::new(&layout)?;
        let switch = Switch::new(memory, layout)?;
        hook::install();
        Ok(Live {
            switch,
            attached,
            looked: None,
            lost: None,
            cut: false,
            // alloc: grown to the largest message once, then reused for every message
            message: Vec::new(),
            // alloc: grown to the largest answer once, then reused for every answer
            answer: Vec::new(),
        })
    }

    /// The generation the program runs.
    pub fn generation(&self) -> u64 {
        self.switch.current()
    }

    /// Takes every patch the daemon announced since the last call, in order, answers each, and
    /// says what became of the last. Call it where nothing of the program's is half done, such
    /// as between two frames: it is the calling thread's quiescent point for the code memory's
    /// fences, and no code a thread can be in is replaced, so a function running on any thread
    /// when its patch lands finishes as it was. Data a patch changes in place (a static's new
    /// value) is written with plain stores, so no other thread may use it while this runs.
    /// While nothing came it is a few loads. Once the channel to the daemon fails, the next call
    /// says why ([`Outcome::Disconnected`]) and later ones take nothing.
    pub fn poll(&mut self) -> Option<Outcome> {
        if self.cut {
            return self
                .lost
                .take()
                .map(|why| Outcome::Disconnected { generation: self.switch.current(), why });
        }
        self.attached.quiesce();
        let mut last = None;
        loop {
            let announced = self.attached.announced();
            let generation = self.switch.current();
            if self.looked == Some((announced, generation)) {
                return last;
            }
            self.looked = Some((announced, generation));
            match self.next(generation) {
                Ok(Some(outcome)) => last = Some(outcome),
                Ok(None) => {}
                Err(why) => {
                    self.cut = true;
                    return match last {
                        Some(outcome) => {
                            self.lost = Some(why);
                            Some(outcome)
                        }
                        None => Some(Outcome::Disconnected { generation, why }),
                    };
                }
            }
        }
    }

    /// Takes each patch as soon as the daemon announces it, on a thread of its own, instead of
    /// at [`Live::poll`], and tells `told` what became of it. For a program whose functions can
    /// change between any two calls: one of many functions that work together can be in the
    /// middle of a frame when some of them do, and data a patch changes in place is stored
    /// while other threads may use it, as [`Live::poll`] says. The thread ends once the channel
    /// fails, after telling why.
    pub fn apply_as_they_come(mut self, mut told: impl FnMut(Outcome) + Send + 'static) {
        let applying = move || {
            loop {
                if let Some(outcome) = self.poll() {
                    told(outcome);
                }
                if self.cut {
                    if let Some(outcome) = self.poll() {
                        told(outcome);
                    }
                    return;
                }
                let seen = self.looked.map_or(0, |(announced, _)| announced);
                self.attached.wait_announced(seen, LOOK_EVERY);
            }
        };
        std::thread::Builder::new()
            // alloc: setup, the thread's name, once
            .name("blaze-live-apply".to_owned())
            .spawn(applying)
            .expect("spawning the thread that applies patches");
    }

    /// Takes the daemon's message for a program at `generation`, if it has one: what became of
    /// it. An error is the channel's.
    fn next(&mut self, generation: u64) -> Result<Option<Outcome>, String> {
        let body = |offset| patch::ask::patch_ask(generation, offset);
        if ask::fetch(self.attached, &PATCHES, "the next patch", body, &mut self.message)?.is_none()
        {
            return Ok(None);
        }
        let (outcome, said) = match Message::view(&self.message) {
            Ok(message) => self.switch.take(&message),
            Err(why) => {
                // alloc: error path
                let why = format!("the daemon's message for generation {generation}: {why}");
                (
                    // clone: a declined message's reason, one for the program and one for the daemon
                    Outcome::Declined { generation, why: why.clone() },
                    Some(Answer::Declined { generation, why }),
                )
            }
        };
        let Some(said) = said else { return Ok(Some(outcome)) };
        Ok(Some(match ask::tell(self.attached, &said, &mut self.answer) {
            Ok(()) => outcome,
            // alloc: error path
            Err(why) => Outcome::Unanswered { outcome: Box::new(outcome), why },
        }))
    }

    /// Runs `f`, such as one frame of the program, under `catch_unwind`. A panic with a frame
    /// in the code of a generation the program runs rolls the newest such generation back to
    /// the one it was applied on, is answered, and comes back as
    /// [`Outcome::RolledBack`]; any other panic is resumed as it was. What `f` changed before
    /// it panicked stays changed: the guard is where the program says that is fine. A
    /// rollback the daemon could not be told of comes back as [`Outcome::Unanswered`].
    pub fn guard<R>(&mut self, f: impl FnOnce() -> R) -> Result<R, Outcome> {
        let (attached, buffer) = (self.attached, &mut self.answer);
        let mut unanswered = None;
        let guarded = self.switch.guard(f, &mut |said| {
            if let Err(why) = ask::tell(attached, said, buffer) {
                unanswered = Some(why);
            }
        });
        match (guarded, unanswered) {
            // alloc: error path
            (Err(outcome), Some(why)) => {
                Err(Outcome::Unanswered { outcome: Box::new(outcome), why })
            }
            (guarded, _) => guarded,
        }
    }
}
