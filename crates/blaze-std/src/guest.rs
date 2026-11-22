//! The program's side of the hosted world (docs/v2/plan/design/run.md, "The hosted world"):
//! the driver of an [`Io`] whose every reach is a call through the host's
//! [`HostTable`] ([`crate::wire`]), and [`start`], what `bstd_hosted_start` runs.
//!
//! **Plain on purpose.** The functions a hosted `hello` runs ([`start`], [`Guest::write`],
//! [`Guest::now`], [`Guest::arg`], [`Guest::grant`]) are the first the new compiler must
//! compile, so they use no generics, closures, trait objects, format calls or atomics, and
//! nothing of blaze-core: loops, slices, raw copies and one indirect call each. The escape
//! ([`Guest::submit`], [`codec`]) is off that path and may use the tree's crates.
//!
//! The program trusts its host: the table's entries and its exchange are the host's promise
//! for the run (the contract of `bstd_hosted_start`). The host does not trust the program
//! ([`crate::hosting`] checks every range and handle).

use alloc::{string::String, vec::Vec};
use core::{
    cell::{Cell, OnceCell, UnsafeCell},
    fmt,
    mem::MaybeUninit,
    time::Duration,
};

use blaze_core::{Completion, Done, Effect, IoError, wait::Deadline};

use crate::{
    args::ArgList,
    cx::{Cx, Placed},
    entry::{MAIN_BUDGET, Outcome, Status},
    grant::Grant,
    io::{Escape, Io, Limits, Offers},
    mem::TASK_FRAME,
    out::Stream,
    wire::{self, HostTable, Refusal},
};

/// The fewest bytes an exchange may have: room for a line of output and an argument's
/// bytes in one piece. A host that hands less is refused at the start.
pub const EXCHANGE_LEAST: usize = 256;

/// What a hosted world offers its program: what the wire carries.
pub const OFFERS: Offers = {
    let mut offers = Offers::NONE;
    offers.out = true;
    offers.err = true;
    offers.clock = true;
    offers.args = true;
    offers.rng = true;
    offers
};

/// A call through the wire was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WireError {
    /// The table's entry: `write`, `clock`, `arg`, `grant`, `effect`.
    pub entry: &'static str,
    pub refusal: Refusal,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.refusal {
            Refusal::NoSuchCode(code) => {
                write!(f, "bstd hosted: the host's `{}` answered code {code}, which wire {} does not have", self.entry, wire::VERSION)
            }
            refusal => write!(f, "bstd hosted: the host's `{}` refused: {}", self.entry, refusal.says()),
        }
    }
}

impl core::error::Error for WireError {}

/// Why a hosted start did not run `main`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartError {
    /// The table pointer is null.
    NoTable,
    /// The table is of another wire version.
    Version { found: u32 },
    /// The table is smaller than this version's.
    Size { found: u32 },
    /// The exchange is null or smaller than [`EXCHANGE_LEAST`].
    Exchange { len: usize },
    /// An argument is not UTF-8.
    ArgNotUtf8 { index: u64 },
    /// The host refused an argument.
    Arg(WireError),
    /// `main`'s frame could not be made.
    Frame,
}

/// One hosted run: the host's table, and what is made from it at its first use. It lives in
/// [`start`]'s frame for the call, and is reached only through the calling thread's
/// [`Run::current`].
///
/// **The invariant that lets a hosted world go without a lock.** A run's state (its
/// exchange above all, which every call writes) is reached only by the thread inside that
/// run's `bstd_hosted_start`, and only during the call. The type that enforces it is `Run`
/// itself: nothing holds a `&Run` or a pointer to one (a hosted [`Io`] is a bare marker);
/// each reach finds it through a thread-local slot that [`start`] sets on its own thread for
/// the call and empties when the call ends, unwinding included ([`Entered`]). Another thread,
/// and the same one after the run, find the slot empty and stop with a panic naming it; the
/// calls are then one at a time, as the wire asks (wire.rs, "Waiting"), with no atomic and
/// no lock. A hello never meets the slot's emptiness: a hosted root has no pool (its layout
/// is `Inline`), bstd's only maker of threads is the native pool, and a program built by
/// blaze has no `std::thread`.
///
/// **When hosted programs have tasks of their own** (a pool of workers in the hosted
/// world), this becomes one run's state per worker: each worker's slot points at its own
/// exchange (the table grows an exchange per worker, or the host hands one per call), the
/// escape is split into a submit and a poll so a worker can park between them, and what is
/// shared across workers (the escape's completions) gets the one lock it then needs.
///
/// **What a run starts with is plain bytes.** What only some programs use ([`Later`]: the
/// command line, the escape's world, the root task's placement and frame) is made in place,
/// in this frame, at the first use of any of it; until then it is uninitialised bytes and a
/// flag, so a hello makes, and drops, none of it (no heap, no lazy cells, no drop glue).
pub(crate) struct Run {
    pub(crate) table: HostTable,
    limits: Limits,
    /// The program's heap, taken by [`start`], the hosted program's entry, from bstd's
    /// (`entry::process_heap`, D120): the escape's encodings are made in it, and a `heap: Heap`
    /// parameter of `main` is handed it ([`heap`]).
    heap: crate::mem::Heap,
    /// Made in place at the first use ([`Run::later`]); initialised exactly when `made`.
    later: UnsafeCell<MaybeUninit<Later>>,
    made: Cell<bool>,
}

/// What a run makes at the first use of any of it.
struct Later {
    /// The command line, asked of the host at the first `io.args()`.
    args: OnceCell<ArgList>,
    /// The world for the escape's effects, made at the first.
    escape: OnceCell<Escape>,
    /// Where the run's root task runs, and its frame (made at its first use): held by the run,
    /// so the root context ([`Cx::hosted_root`]) allocates nothing.
    placed: Placed,
    /// The run's per-worker state (D171): caller cells only, made at the first `on_worker`
    /// unless the program handed its own first ([`Io::on_workers`]).
    workers: OnceCell<crate::workers::Workers>,
}

impl Drop for Run {
    fn drop(&mut self) {
        if self.made.get() {
            // SAFETY: `made` says `later` was initialised, and it is dropped once, here.
            unsafe { self.later.get_mut().assume_init_drop() };
        }
    }
}

/// The run the calling thread is inside: set by [`Entered`] for one `bstd_hosted_start` call.
#[thread_local]
static CURRENT: Cell<*const Run> = Cell::new(core::ptr::null());

/// The calling thread is inside a run: the slot holds it until this is dropped.
struct Entered;

impl Entered {
    /// Enters `run` on the calling thread; `None` when the thread is inside a run already (a
    /// start called from inside a start).
    fn enter(run: &Run) -> Option<Entered> {
        if !CURRENT.get().is_null() {
            return None;
        }
        CURRENT.set(run);
        Some(Entered)
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        CURRENT.set(core::ptr::null());
    }
}

impl Run {
    /// The run the calling thread is inside.
    ///
    /// # Panics
    ///
    /// When it is inside none: a hosted world's `Io` used on another thread than its run's,
    /// or after the run ended.
    pub(crate) fn current<'a>() -> &'a Run {
        let run = CURRENT.get();
        if run.is_null() {
            outside();
        }
        // SAFETY: the slot holds a run only while `start`'s frame, which owns it, is live on
        // this thread (`Entered`), and only this thread reads it. A `&Run` is used inside the
        // call that asked for it and never kept, so none outlives the run.
        unsafe { &*run }
    }

    /// The command line, asked of the host now if this is the first ask. A host whose
    /// argument is refused, too long or not UTF-8 broke the wire: that panics, naming it.
    pub(crate) fn args(&self) -> &ArgList {
        self.later().args.get_or_init(|| {
            // alloc: once per run that reads its command line.
            let mut args = Vec::new();
            let mut index = 0;
            loop {
                match arg_of(&self.table, index) {
                    Ok(Some(arg)) => args.push(arg),
                    Ok(None) => break,
                    Err(StartError::Arg(error)) => broken(error),
                    Err(_) => broken(WireError { entry: "arg", refusal: Refusal::BadBytes }),
                }
                index += 1;
            }
            ArgList::given(args)
        })
    }

    /// The escape's world, made now if this is its first use.
    pub(crate) fn escape(&self) -> &Escape {
        self.later().escape.get_or_init(|| Escape::new(self.table, self.limits, self.heap.shared()))
    }

    /// Where the run's root task runs ([`Cx::hosted_root`]'s placement).
    pub(crate) fn placed(&self) -> &Placed {
        &self.later().placed
    }

    /// The run's per-worker state, made now if this is its first use: bstd's own cells only,
    /// no simulated workers; no seed (the wire carries none).
    pub(crate) fn workers(&self) -> &crate::workers::Workers {
        self.later().workers.get_or_init(|| {
            let setup = crate::workers::WorkersSetup {
                limits: crate::workers::Limits::BARE,
                seed: None,
                models: 0,
            };
            match crate::workers::Workers::bare(setup) {
                Ok(workers) => workers,
                Err(error) => panic!("bstd::guest: the hosted run's workers: {error}"),
            }
        })
    }

    /// The run's per-worker state is `workers`, the program's.
    ///
    /// # Panics
    ///
    /// When the run's were made already (a task used its cell first).
    pub(crate) fn set_workers(&self, workers: crate::workers::Workers) {
        if self.later().workers.set(workers).is_err() {
            panic!("bstd::guest: Io::on_workers after the hosted run's workers were used: hand them before the first on_worker");
        }
    }

    /// What the run makes at a first use, made in place now if this is the first.
    fn later(&self) -> &Later {
        let at = self.later.get().cast::<Later>();
        if !self.made.get() {
            let later = Later {
                args: OnceCell::new(),
                escape: OnceCell::new(),
                placed: Placed::hosted_root(TASK_FRAME),
                workers: OnceCell::new(),
            };
            // SAFETY: not made yet, so nothing borrows it; only the run's own thread reaches
            // the run (`Run`'s invariant), and making `later` does not reach the run again.
            // The run does not move while entered (it stays in `start`'s frame).
            unsafe { at.write(later) };
            self.made.set(true);
        }
        // SAFETY: made above or before, and dropped only with the run.
        unsafe { &*at }
    }
}

/// Reached a hosted world outside its run.
#[cold]
fn outside() -> ! {
    panic!(
        "bstd hosted: a hosted world was reached outside its run (on another thread than the one \
         in `bstd_hosted_start`, or after it returned): its exchange is that call's alone"
    )
}

/// The standard stream `stream`'s wire handle.
fn stream_handle(stream: Stream) -> u32 {
    match stream {
        Stream::Out => wire::stream::OUT,
        Stream::Err => wire::stream::ERR,
    }
}

/// `parts`, in order, on `stream`: copied into the exchange and written with one call a full
/// exchange (one call for a line, which is shorter than the exchange).
pub(crate) fn write_to(table: &HostTable, stream: Stream, parts: &[&[u8]]) -> Result<(), WireError> {
    let handle = stream_handle(stream);
    let room = table.exchange_len;
    let mut held = 0;
    let mut index = 0;
    while index < parts.len() {
        let part = parts[index];
        let mut from = 0;
        while from < part.len() {
            if held == room {
                if let Err(error) = flush(table, handle, held) {
                    return Err(error);
                }
                held = 0;
            }
            let mut take = room - held;
            if part.len() - from < take {
                take = part.len() - from;
            }
            // SAFETY: the exchange is `room` bytes and `held + take <= room`.
            unsafe { put(table.exchange, held, part, from, take) };
            held += take;
            from += take;
        }
        index += 1;
    }
    if held > 0 {
        return flush(table, handle, held);
    }
    Ok(())
}

/// `bytes[from..from + take]` into the exchange at `at`. A byte loop: built-in indexing and
/// a pointer write call no function (a copy call brings its debug checks onto the hello's
/// path), and an optimised build makes it one copy. `bytes` is indexed checked.
///
/// # Safety
///
/// `exchange` is the run's exchange and `at + take` is at most its length.
unsafe fn put(exchange: *mut u8, at: usize, bytes: &[u8], from: usize, take: usize) {
    let mut index = 0;
    while index < take {
        // SAFETY: `at + index < at + take`, inside the exchange the host lent for the run
        // (`bstd_hosted_start`'s contract), by the caller's promise; the host reads it only
        // inside a call.
        unsafe { *exchange.add(at + index) = bytes[from + index] };
        index += 1;
    }
}

/// The exchange's first `held` bytes written to `handle`.
fn flush(table: &HostTable, handle: u32, held: usize) -> Result<(), WireError> {
    // SAFETY: the host's entry, valid for the run (`bstd_hosted_start`'s contract).
    let answer = unsafe { (table.write)(table.host, handle, 0, held) };
    match Refusal::of(answer) {
        Ok(written) if written == held as u64 => Ok(()),
        Ok(_) => Err(WireError { entry: "write", refusal: Refusal::Failed }),
        Err(refusal) => Err(WireError { entry: "write", refusal }),
    }
}

/// A reading of the clock `which`, in ns. A host that will not say the time is broken: every
/// wait is measured by it, so this panics, naming the refusal.
fn clock_of(table: &HostTable, which: u32) -> u64 {
    // SAFETY: as in `flush`.
    let answer = unsafe { (table.clock)(table.host, which) };
    match Refusal::of(answer) {
        Ok(ns) => ns,
        Err(refusal) => broken(WireError { entry: "clock", refusal }),
    }
}

pub(crate) fn now_of(table: &HostTable) -> Duration {
    Duration::from_nanos(clock_of(table, wire::clock::MONOTONIC))
}

pub(crate) fn wall_ns_of(table: &HostTable) -> u64 {
    clock_of(table, wire::clock::WALL)
}

/// The command line's `index`th argument, or `None` past the last.
fn arg_of(table: &HostTable, index: u64) -> Result<Option<String>, StartError> {
    // SAFETY: as in `flush`.
    let answer = unsafe { (table.arg)(table.host, index) };
    let len = match Refusal::of(answer) {
        Ok(len) => len,
        Err(Refusal::End) => return Ok(None),
        Err(refusal) => return Err(StartError::Arg(WireError { entry: "arg", refusal })),
    };
    if len > table.exchange_len as u64 {
        return Err(StartError::Arg(WireError { entry: "arg", refusal: Refusal::TooSmall }));
    }
    // SAFETY: the host wrote `len` bytes at the start of its exchange, which has
    // `exchange_len >= len` bytes.
    let bytes = unsafe { core::slice::from_raw_parts(table.exchange, len as usize) };
    match core::str::from_utf8(bytes) {
        // alloc: the argument, the command line's own.
        Ok(text) => Ok(Some(String::from(text))),
        Err(_) => Err(StartError::ArgNotUtf8 { index }),
    }
}

/// Whether the run grants the parameter `name` of kind `kind`.
pub(crate) fn grant_of(table: &HostTable, kind: &str, name: &str) -> Result<(), WireError> {
    let (kind, name) = (kind.as_bytes(), name.as_bytes());
    if kind.len() + name.len() > table.exchange_len {
        return Err(WireError { entry: "grant", refusal: Refusal::TooSmall });
    }
    // SAFETY: both fit in the exchange, checked above; the entry as in `flush`.
    let answer = unsafe {
        put(table.exchange, 0, kind, 0, kind.len());
        put(table.exchange, kind.len(), name, 0, name.len());
        (table.grant)(table.host, 0, kind.len(), kind.len(), name.len())
    };
    match Refusal::of(answer) {
        Ok(_) => Ok(()),
        Err(refusal) => Err(WireError { entry: "grant", refusal }),
    }
}

/// The run ends with `status`: the host's `exit`, which does not return.
pub(crate) fn exit_to(table: &HostTable, status: i32) -> ! {
    // SAFETY: as in `flush`; the entry does not return.
    unsafe { (table.exit)(table.host, status) }
}

/// The escape's driver: the host's table, and the completions of escaped effects not yet
/// polled. Lives in a run's [`Escape`] world, behind its lock.
pub(crate) struct Guest {
    table: HostTable,
    // alloc: the escaped effects' completions, grown once to the most made between polls.
    done: Vec<Completion>,
    /// Where an escaped effect's encoding is made: the run's heap ([`Run`]).
    alloc: blaze_own::SharedAlloc<'static>,
}

// SAFETY: the table's pointers are the host's for the run. A `Guest` lives only in its run's
// escape world, which only the run's thread reaches (`Run`), behind that world's lock.
unsafe impl Send for Guest {}

impl Guest {
    pub(crate) fn new(table: HostTable, alloc: blaze_own::SharedAlloc<'static>) -> Guest {
        Guest { table, done: Vec::new(), alloc }
    }

    pub(crate) fn write(&mut self, stream: Stream, parts: &[&[u8]]) -> Result<(), WireError> {
        write_to(&self.table, stream, parts)
    }

    pub(crate) fn now(&self) -> Duration {
        now_of(&self.table)
    }

    pub(crate) fn wall_ns(&self) -> u64 {
        wall_ns_of(&self.table)
    }

    /// Each effect sent through the escape and waited for by the host; its completion kept
    /// for the next poll.
    pub(crate) fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let (tag, done) = self.escape(effect);
            self.done.push(Completion { tag, done });
        }
    }

    fn escape(&mut self, effect: Effect) -> (blaze_core::Tag, Done) {
        let mut bytes = blaze_own::Vec::new_in(self.alloc);
        let (tag, budget) = match codec::effect(&mut bytes, &effect) {
            Some(it) => it,
            None => return (effect.tag(), failed("this effect is not on the hosted wire yet")),
        };
        if bytes.len() > self.table.exchange_len {
            return (tag, failed("the effect's encoding is larger than the host's exchange"));
        }
        // SAFETY: the encoding fits in the exchange, checked above; the host writes its answer
        // at the exchange's start, `answer.value` bytes, checked below.
        let answered = unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.table.exchange, bytes.len());
            let answer = (self.table.effect)(self.table.host, 0, bytes.len(), budget);
            match Refusal::of(answer) {
                Ok(len) if len <= self.table.exchange_len as u64 => {
                    Ok(codec::done(core::slice::from_raw_parts(self.table.exchange, len as usize)))
                }
                Ok(_) => Err(Refusal::TooSmall),
                Err(refusal) => Err(refusal),
            }
        };
        match answered {
            Ok(Ok(done)) => (tag, done),
            Ok(Err(_)) => (tag, failed("the host's completion is not one of this wire")),
            Err(refusal) => (tag, failed(refusal.says())),
        }
    }

    pub(crate) fn poll(&mut self, out: &mut Vec<Completion>) -> usize {
        let moved = self.done.len();
        out.append(&mut self.done);
        moved
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.done.len()
    }
}

/// A failed completion saying `why`.
fn failed(why: &str) -> Done {
    // alloc: a failure's words.
    Done::Failed(IoError(String::from(why)))
}

/// The host broke the wire where nothing can go on.
#[cold]
fn broken(error: WireError) -> ! {
    panic!("{error}")
}

/// The escape's encoding: an effect as `[kind, fields..]` and a completion as
/// `[kind, fields..]`, deterministic CBOR (blaze-cbor), the same on both sides. The kinds of
/// this version are the effects a program reaches through the escape today; any other is
/// refused on the program's side with a typed failure naming it, before it crosses.
pub(crate) mod codec {
    use alloc::{string::String, vec::Vec};
    use core::time::Duration;

    use blaze_cbor::Reader;
    use blaze_core::{Done, Effect, FileName, IoError, RANDOM_MOST, Tag};

    pub const TIMER: u64 = 1;
    pub const RANDOM: u64 = 2;
    pub const READ_ALL: u64 = 3;
    pub const WRITE: u64 = 4;
    pub const APPEND: u64 = 5;
    pub const REMOVE: u64 = 6;
    pub const RENAME: u64 = 7;
    pub const MAKE_DIR: u64 = 8;

    pub const FIRED: u64 = 0;
    pub const DRAWN: u64 = 1;
    pub const FAILED: u64 = 2;
    pub const READ: u64 = 3;
    pub const WRITTEN: u64 = 4;
    pub const APPENDED: u64 = 5;
    pub const REMOVED: u64 = 6;
    pub const RENAMED: u64 = 7;
    pub const MADE_DIR: u64 = 8;

    /// The most bytes of a failure's words.
    const WORDS_MOST: usize = 4096;
    /// The most bytes of a file's name.
    pub const NAME_MOST: usize = 4096;
    /// The most bytes of a file's contents one effect carries.
    pub const FILE_MOST: usize = 1 << 24;

    /// `effect` written to `out`, with its tag and how long the host may wait for it; `None`
    /// for an effect this version does not carry.
    pub fn effect<A: core::alloc::Allocator>(out: &mut blaze_own::Vec<u8, A>, effect: &Effect) -> Option<(Tag, u64)> {
        match effect {
            Effect::Timer { after, tag } => {
                blaze_cbor::array(out, 2);
                blaze_cbor::uint(out, TIMER);
                let ns = u64::try_from(after.as_nanos()).unwrap_or(u64::MAX);
                blaze_cbor::uint(out, ns);
                Some((*tag, ns))
            }
            Effect::Random { len, tag } => {
                blaze_cbor::array(out, 2);
                blaze_cbor::uint(out, RANDOM);
                blaze_cbor::uint(out, *len as u64);
                Some((*tag, 0))
            }
            Effect::ReadAll { file, tag } => named(out, READ_ALL, &[file], None, *tag),
            Effect::Write { file, bytes, tag } => named(out, WRITE, &[file], Some(bytes), *tag),
            Effect::Append { file, bytes, tag } => named(out, APPEND, &[file], Some(bytes), *tag),
            Effect::Remove { file, tag } => named(out, REMOVE, &[file], None, *tag),
            Effect::Rename { from, to, tag } => named(out, RENAME, &[from, to], None, *tag),
            Effect::MakeDir { dir, tag } => named(out, MAKE_DIR, &[dir], None, *tag),
            _ => None,
        }
    }

    /// A file effect: `[kind, name.., bytes?]`.
    fn named<A: core::alloc::Allocator>(out: &mut blaze_own::Vec<u8, A>, kind: u64, names: &[&FileName], bytes: Option<&[u8]>, tag: Tag) -> Option<(Tag, u64)> {
        blaze_cbor::array(out, 1 + names.len() as u64 + u64::from(bytes.is_some()));
        blaze_cbor::uint(out, kind);
        for name in names {
            blaze_cbor::text(out, name.as_str());
        }
        if let Some(bytes) = bytes {
            blaze_cbor::bytes(out, bytes);
        }
        Some((tag, 0))
    }

    /// An effect the escape carries, read from the program's bytes before the host has a tag
    /// for it: only [`read_effect`] makes one, so each is an effect this wire has.
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub struct Escaped(Effect);

    impl Escaped {
        /// The effect, under `tag`.
        pub fn under(self, tag: Tag) -> Effect {
            match self.0 {
                Effect::Timer { after, .. } => Effect::Timer { after, tag },
                Effect::Random { len, .. } => Effect::Random { len, tag },
                Effect::ReadAll { file, .. } => Effect::ReadAll { file, tag },
                Effect::Write { file, bytes, .. } => Effect::Write { file, bytes, tag },
                Effect::Append { file, bytes, .. } => Effect::Append { file, bytes, tag },
                Effect::Remove { file, .. } => Effect::Remove { file, tag },
                Effect::Rename { from, to, .. } => Effect::Rename { from, to, tag },
                Effect::MakeDir { dir, .. } => Effect::MakeDir { dir, tag },
                // `read_effect` makes none other; the tag of one would be its own placeholder.
                other => other,
            }
        }

        /// The effect, as its placeholder-tagged view: what the layers judge.
        pub fn effect(&self) -> &Effect {
            &self.0
        }
    }

    /// The effect `bytes` encode: the host's read of the program's bytes, every length
    /// bounded, every name one a driver takes ([`FileName::new`]).
    pub fn read_effect(bytes: &[u8]) -> Result<Escaped, blaze_cbor::Error> {
        let tag = Tag(0);
        let mut reader = Reader::new(bytes);
        let len = reader.array(4, "an escaped effect")?;
        let kind = reader.uint_most(MAKE_DIR, "an escaped effect's kind")?;
        let fields = match kind {
            TIMER | RANDOM | READ_ALL | REMOVE | MAKE_DIR => 1,
            _ => 2,
        };
        if len != 1 + fields {
            return Err(blaze_cbor::Error { at: 0, why: "an escaped effect with the wrong number of fields for its kind" });
        }
        let effect = match kind {
            TIMER => Effect::Timer { after: Duration::from_nanos(reader.uint()?), tag },
            RANDOM => {
                let len = reader.uint_most(RANDOM_MOST as u64, "random bytes asked")?;
                Effect::Random { len: len as usize, tag }
            }
            READ_ALL => Effect::ReadAll { file: name(&mut reader)?, tag },
            WRITE => Effect::Write { file: name(&mut reader)?, bytes: Vec::from(reader.bytes(FILE_MOST, "a file's bytes")?), tag },
            APPEND => Effect::Append { file: name(&mut reader)?, bytes: Vec::from(reader.bytes(FILE_MOST, "a file's bytes")?), tag },
            REMOVE => Effect::Remove { file: name(&mut reader)?, tag },
            RENAME => Effect::Rename { from: name(&mut reader)?, to: name(&mut reader)?, tag },
            MAKE_DIR => Effect::MakeDir { dir: name(&mut reader)?, tag },
            _ => return Err(blaze_cbor::Error { at: 0, why: "an escaped effect's kind this wire does not have" }),
        };
        reader.end()?;
        Ok(Escaped(effect))
    }

    /// A file's name, refused when a driver would not take it (empty, absolute, `..`).
    fn name(reader: &mut Reader<'_>) -> Result<FileName, blaze_cbor::Error> {
        let at = reader.at();
        let text = reader.text(NAME_MOST, "a file's name")?;
        FileName::new(text).ok_or(blaze_cbor::Error { at, why: "a file's name that leaves the root or names nothing" })
    }

    /// `done` written to `out`; `None` for a completion this version does not carry.
    pub fn write_done<A: core::alloc::Allocator>(out: &mut blaze_own::Vec<u8, A>, done: &Done) -> Option<()> {
        match done {
            Done::Fired => {
                blaze_cbor::array(out, 1);
                blaze_cbor::uint(out, FIRED);
            }
            Done::Random(bytes) => {
                blaze_cbor::array(out, 2);
                blaze_cbor::uint(out, DRAWN);
                blaze_cbor::bytes(out, bytes);
            }
            Done::Failed(IoError(why)) => {
                blaze_cbor::array(out, 2);
                blaze_cbor::uint(out, FAILED);
                blaze_cbor::text(out, why);
            }
            Done::Read(bytes) => {
                blaze_cbor::array(out, 2);
                blaze_cbor::uint(out, READ);
                blaze_cbor::bytes(out, bytes);
            }
            Done::Written | Done::Appended | Done::Removed | Done::Renamed | Done::MadeDir => {
                blaze_cbor::array(out, 1);
                blaze_cbor::uint(out, match done {
                    Done::Written => WRITTEN,
                    Done::Appended => APPENDED,
                    Done::Removed => REMOVED,
                    Done::Renamed => RENAMED,
                    _ => MADE_DIR,
                });
            }
            _ => return None,
        }
        Some(())
    }

    /// The completion `bytes` encode: the program's read of the host's answer.
    pub fn done(bytes: &[u8]) -> Result<Done, blaze_cbor::Error> {
        let mut reader = Reader::new(bytes);
        let len = reader.array(2, "an escaped completion")?;
        let done = match (reader.uint_most(MADE_DIR, "an escaped completion's kind")?, len) {
            (FIRED, 1) => Done::Fired,
            // alloc: the drawn bytes, the completion's own.
            (DRAWN, 2) => Done::Random(Vec::from(reader.bytes(RANDOM_MOST, "random bytes drawn")?)),
            // alloc: a failure's words.
            (FAILED, 2) => Done::Failed(IoError(String::from(reader.text(WORDS_MOST, "a failure's words")?))),
            // alloc: the bytes read, the completion's own.
            (READ, 2) => Done::Read(Vec::from(reader.bytes(FILE_MOST, "a file's bytes read")?)),
            (WRITTEN, 1) => Done::Written,
            (APPENDED, 1) => Done::Appended,
            (REMOVED, 1) => Done::Removed,
            (RENAMED, 1) => Done::Renamed,
            (MADE_DIR, 1) => Done::MadeDir,
            _ => return Err(blaze_cbor::Error { at: 0, why: "an escaped completion of the wrong length" }),
        };
        reader.end()?;
        Ok(done)
    }
}

/// The run over `table`, checked: its version, size and exchange; its allocations in `heap`.
fn checked(table: *const HostTable, heap: crate::mem::Heap) -> Result<Run, StartError> {
    if table.is_null() {
        return Err(StartError::NoTable);
    }
    // SAFETY: non-null, and a `HostTable` of this version for the run by
    // `bstd_hosted_start`'s contract; its version and size are checked before any entry is
    // called. Only the fields every version has are read before that check.
    let (version, size) = unsafe { ((*table).version, (*table).size) };
    if version != wire::VERSION {
        return Err(StartError::Version { found: version });
    }
    if (size as usize) < size_of::<HostTable>() {
        return Err(StartError::Size { found: size });
    }
    // SAFETY: as above, now known to be this version's whole table.
    let table = unsafe { *table };
    if table.exchange.is_null() || table.exchange_len < EXCHANGE_LEAST {
        return Err(StartError::Exchange { len: table.exchange_len });
    }
    Ok(Run { table, limits: Limits::USUAL, heap, later: UnsafeCell::new(MaybeUninit::uninit()), made: Cell::new(false) })
}

/// Whether `main`'s budget is unbounded: decided when bstd is compiled, so the start makes
/// no call to ask.
const MAIN_UNBOUNDED: bool = MAIN_BUDGET.as_secs() == Duration::MAX.as_secs() && MAIN_BUDGET.subsec_nanos() == Duration::MAX.subsec_nanos();

/// What `bstd_hosted_start` runs: the run over `table`, entered on this thread for the call
/// ([`Run`]), then `run` under the root context; its status. A start that cannot be made
/// (a table that is not this wire's, or a start inside a start) is [`Status::FAILED`].
///
/// What it builds is what a hello uses, all in this frame and none on the heap: the run, a
/// hosted [`Io`] (a marker), and the root context, whose placement the run holds. The command
/// line, the escape's world and the root's frame are made at their first use.
pub fn start(table: *const HostTable, run: fn(&Cx, Io) -> Status) -> i32 {
    // The hosted program's entry: its heap is taken from bstd's entry (D120), as the native
    // start's is.
    let this = match checked(table, crate::entry::process_heap()) {
        Ok(this) => this,
        Err(_) => return i32::from(Status::FAILED.0),
    };
    let Some(_entered) = Entered::enter(&this) else {
        return i32::from(Status::FAILED.0);
    };
    let io = Io::hosted(OFFERS, this.limits);
    // An unbounded budget has no deadline, and the clock is not read for one.
    let deadline = if MAIN_UNBOUNDED { None } else { Some(Deadline::after(io.now(), MAIN_BUDGET)) };
    let cx = Cx::hosted_root(deadline, io.clone());
    i32::from(run(&cx, io).0)
}

/// The heap of the run the calling thread is inside: what [`start`] was handed, for a
/// `heap: Heap` parameter of a hosted `main` (`Grant::hosted`).
///
/// # Panics
///
/// Outside a run, as every reach of a hosted world does ([`Run::current`]).
pub(crate) fn heap() -> crate::mem::Heap {
    Run::current().heap
}

/// The grant `name` of type `T`, asked of the host: `None` when it refuses, and the program's
/// start then ends as [`Status::REFUSED`].
pub fn grant<T: Grant>(io: &Io, name: &'static str) -> Option<T> {
    // A match, not `.ok()?`: on the hosted hello's path, every helper is one more function
    // the new compiler must build (ladder README, "What the hosted hello executes").
    match io.guest_grant(T::KIND.name(), name) {
        Ok(()) => T::hosted(io, name),
        Err(_) => None,
    }
}

/// What `main` returned, as its status; what a failure says goes to the host's standard
/// error.
///
/// What a failure says goes straight to the wire, piece by piece as its `Display` writes it,
/// and is ended with a newline: nothing is gathered on the heap, and a `main` that returns
/// `Ok` writes nothing.
pub fn finish<R: Outcome>(io: &Io, ran: R) -> Status {
    let mut said = Said { io, wrote: false, failed: false };
    let status = ran.status(&mut said);
    // A world with a buffer (one over the escape, not the hosted reach, which holds none)
    // writes what it holds before the run ends, as the native entry does; a refusal is said
    // after the failure, if any, and fails the run.
    let status = match io.flush_streams("main") {
        Ok(()) => status,
        Err(error) => {
            let after = if said.wrote { "; " } else { "" };
            let _ = core::fmt::Write::write_fmt(&mut said, format_args!("{after}{error}"));
            Status::FAILED
        }
    };
    if said.wrote && !said.failed && io.write_out(Stream::Err, &[b"\n"]).is_err() {
        return Status::FAILED;
    }
    if said.failed {
        return Status::FAILED;
    }
    status
}

/// What a failure says, written to the host's standard error as it is said.
struct Said<'a> {
    io: &'a Io,
    /// Whether anything was said: a line was begun, and needs its end.
    wrote: bool,
    /// Whether a write failed: the run then ends [`Status::FAILED`].
    failed: bool,
}

impl fmt::Write for Said<'_> {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        if piece.is_empty() {
            return Ok(());
        }
        self.wrote = true;
        if self.io.write_out(Stream::Err, &[piece.as_bytes()]).is_err() {
            self.failed = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}
