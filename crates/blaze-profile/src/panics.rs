//! Panics as rows: the process's panic hook ([`record`]) writes each panic, as it is raised,
//! into a fixed ring of [`KEPT`] slots made with the process (no allocation of its own, no
//! lock, no I/O), and the history drains the ring at each flush into the schema's `panics`
//! table ([`since`], [`rows`]). A slot holds what the row needs that only the raising thread
//! knows: the message and location, the thread, the job and the spans it is inside, whether a
//! job's boundary catches it, and its stack as raw return addresses walked from the hook's own
//! frame pointer ([`crate::stack`]), named only when the history writes them.
//!
//! The same ring is where the daemon's plane finds where a panic it caught was raised
//! ([`claim`]): one home for a raised panic, read by both. A fatal signal's handler reads the
//! faulting thread's last panic here too (`crate::crash`), for an abort a panic led to.
//!
//! A slot is a seqlock of atomics, as the flight recorder's rows are: its writer marks it odd
//! (`2 * n + 1` for panic number `n`), writes its words, and marks it even (`2 * n + 2`); a reader
//! copies the words and keeps them only when the mark was `2 * n + 2` before and after. A reader
//! that finds panic `n` still being written stops there and reads it next time.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering, fence},
};

use arrow_array::{
    ArrayRef, DictionaryArray, Int64Array, RecordBatch, StringArray, UInt32Array, UInt64Array,
    builder::{ListBuilder, StringDictionaryBuilder, UInt64Builder},
    types::UInt32Type,
};
use arrow_schema::{ArrowError, DataType};

use crate::{batches, clock, flight, schema, stack};

/// Panics the ring keeps: a 65th raised before a flush drains the first writes over it, counted
/// by [`since`].
pub const KEPT: usize = 64;
/// Bytes of a panic's message a slot keeps; a longer one is cut, and its row says so.
pub const MESSAGE_BYTES: usize = 1024;
/// Bytes of a location (`file:line:column`) a slot keeps.
const LOCATION_BYTES: usize = 256;
/// Bytes of a thread's name a slot keeps.
const NAME_BYTES: usize = 64;
/// Open spans a slot names, outermost first, and the bytes of each name.
pub const OPEN: usize = 16;
const OPEN_BYTES: usize = 64;

/// What a `panics` row is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Panic,
    /// A fatal signal, recovered from a crash file (`crate::crash`).
    Signal,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Panic => "panic",
            Kind::Signal => "signal",
        }
    }
}

/// Text in atomic words: what a slot keeps of a string, cut at `W * 8` bytes. `len` is the
/// whole string's length, so a reader knows it was cut, or [`NO_TEXT`].
struct Text<const W: usize> {
    len: AtomicU64,
    words: [AtomicU64; W],
}

/// A [`Text`]'s `len` for no text at all (a panic payload that is not a string).
const NO_TEXT: u64 = u64::MAX;

impl<const W: usize> Text<W> {
    const BYTES: usize = W * 8;

    const fn new() -> Self {
        Text { len: AtomicU64::new(0), words: [const { AtomicU64::new(0) }; W] }
    }

    fn put(&self, bytes: Option<&[u8]>) {
        let Some(bytes) = bytes else {
            self.len.store(NO_TEXT, Ordering::Relaxed);
            return;
        };
        let kept = &bytes[..bytes.len().min(Self::BYTES)];
        for (word, chunk) in self.words.iter().zip(kept.chunks(8)) {
            let mut eight = [0u8; 8];
            eight[..chunk.len()].copy_from_slice(chunk);
            word.store(u64::from_ne_bytes(eight), Ordering::Relaxed);
        }
        self.len.store(bytes.len() as u64, Ordering::Relaxed);
    }

    /// The bytes kept, into `out` (cut at its length); how many, and the whole text's length
    /// (`None` for no text). Atomics only: a signal handler may call it.
    fn copy(&self, out: &mut [u8]) -> (usize, Option<u64>) {
        let len = self.len.load(Ordering::Relaxed);
        if len == NO_TEXT {
            return (0, None);
        }
        let kept = (len as usize).min(Self::BYTES).min(out.len());
        for (at, word) in self.words.iter().enumerate().take(kept.div_ceil(8)) {
            let eight = word.load(Ordering::Relaxed).to_ne_bytes();
            let from = at * 8;
            let n = (kept - from).min(8);
            out[from..from + n].copy_from_slice(&eight[..n]);
        }
        (kept, Some(len))
    }

    /// The text, ending in `...` when it was cut; `None` for no text.
    fn text(&self) -> Option<String> {
        let mut bytes = [0u8; 1024];
        let (kept, len) = self.copy(&mut bytes);
        let len = len?;
        // alloc: a panic's row, once a flush
        let mut text = String::from_utf8_lossy(&bytes[..kept]).into_owned();
        if len as usize > kept {
            text.push_str("...");
        }
        Some(text)
    }

    /// Whether it holds `text`: no text for `None`, else the same length and the same bytes as
    /// far as it kept them.
    fn holds(&self, text: Option<&str>) -> bool {
        let mut bytes = [0u8; 1024];
        let (kept, len) = self.copy(&mut bytes);
        match (len, text) {
            (None, None) => true,
            (Some(len), Some(text)) => {
                len == text.len() as u64 && text.as_bytes().get(..kept) == Some(&bytes[..kept])
            }
            _ => false,
        }
    }
}

const _: () = assert!(
    MESSAGE_BYTES <= 1024 && LOCATION_BYTES <= 1024,
    "Text::text reads through a 1 KiB buffer"
);

/// One panic, as its hook wrote it.
struct Slot {
    /// `2 * n + 1` while panic `n` is written, `2 * n + 2` once it is whole; 0 before any.
    seq: AtomicU64,
    t_ns: AtomicU64,
    tid: AtomicU64,
    /// [`flight::running_job`], 0 for none.
    job: AtomicU64,
    /// The innermost span, 0 for none.
    span: AtomicU64,
    build: AtomicU64,
    caught: AtomicBool,
    /// A catcher took its location ([`claim`]): a second panic of the same text takes another.
    claimed: AtomicBool,
    depth: AtomicU64,
    /// Return addresses, innermost first.
    pcs: [AtomicU64; stack::DEPTH],
    message: Text<{ MESSAGE_BYTES / 8 }>,
    location: Text<{ LOCATION_BYTES / 8 }>,
    thread: Text<{ NAME_BYTES / 8 }>,
    opened: AtomicU64,
    open: [Text<{ OPEN_BYTES / 8 }>; OPEN],
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            seq: AtomicU64::new(0),
            t_ns: AtomicU64::new(0),
            tid: AtomicU64::new(0),
            job: AtomicU64::new(0),
            span: AtomicU64::new(0),
            build: AtomicU64::new(0),
            caught: AtomicBool::new(false),
            claimed: AtomicBool::new(false),
            depth: AtomicU64::new(0),
            pcs: [const { AtomicU64::new(0) }; stack::DEPTH],
            message: Text::new(),
            location: Text::new(),
            thread: Text::new(),
            opened: AtomicU64::new(0),
            open: [const { Text::new() }; OPEN],
        }
    }
}

/// The ring, made with the process (zeroed, about 190 KiB): what the hook writes.
static SLOTS: [Slot; KEPT] = [const { Slot::new() }; KEPT];
/// Panics ever raised: the next one's number.
static NEXT: AtomicU64 = AtomicU64::new(0);
/// Whether a panic raised on the calling thread now is caught by a job's boundary, as the
/// daemon's plane says it ([`record`]).
static CAUGHT: OnceLock<fn() -> bool> = OnceLock::new();

/// Has every panic of the process written to the ring as it is raised, then printed by the hook
/// that was there before (std's). `caught` says whether a panic raised on the calling thread is
/// caught by a job's boundary (blaze-cargo's plane: a plane task's, or a pool job's on its
/// fiber), for the row's `caught`. Once a process; a later call does nothing, so the hook never
/// wraps itself and writes each panic once (tests call it each).
pub fn record(caught: fn() -> bool) {
    static RECORDING: std::sync::Once = std::sync::Once::new();
    RECORDING.call_once(|| {
        _ = CAUGHT.set(caught);
        stack::ready();
        let printed = std::panic::take_hook();
        // alloc: the process's one panic hook
        std::panic::set_hook(Box::new(move |info| {
            raised(info.payload_as_str(), info.location());
            printed(info);
        }));
    });
}

/// Writes the panic with `message` raised at `location` on this thread to the next slot. No
/// allocation and no lock: the hook runs where the allocator may have failed.
#[inline(never)]
fn raised(message: Option<&str>, location: Option<&std::panic::Location<'_>>) {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = &SLOTS[(n % KEPT as u64) as usize];
    slot.seq.store(2 * n + 1, Ordering::Relaxed);
    fence(Ordering::Release);
    let mut pcs = [0; stack::DEPTH];
    let depth = stack::here(&mut pcs);
    for (word, pc) in slot.pcs.iter().zip(&pcs[..depth]) {
        word.store(*pc, Ordering::Relaxed);
    }
    slot.depth.store(depth as u64, Ordering::Relaxed);
    slot.t_ns.store(clock::mono_ns() as u64, Ordering::Relaxed);
    let tid = crate::sampler::os_tid();
    slot.tid.store(tid, Ordering::Relaxed);
    slot.caught.store(CAUGHT.get().is_some_and(|caught| caught()), Ordering::Relaxed);
    slot.claimed.store(false, Ordering::Relaxed);
    slot.message.put(message.map(str::as_bytes));
    let mut at = Cut { bytes: [0; LOCATION_BYTES], len: 0 };
    if let Some(location) = location {
        _ = std::fmt::Write::write_fmt(
            &mut at,
            format_args!("{}:{}:{}", location.file(), location.line(), location.column()),
        );
    }
    slot.location.put(location.map(|_| &at.bytes[..at.len]));
    let mut name = [0u8; NAME_BYTES];
    let named = thread_name(&mut name);
    slot.thread.put(Some(&name[..named]));
    let mut opened = 0;
    let innermost = flight::open_of_thread(tid as u32, &mut |_, span| {
        if let Some(text) = slot.open.get(opened) {
            text.put(Some(span.as_bytes()));
            opened += 1;
        }
    });
    slot.opened.store(opened as u64, Ordering::Relaxed);
    slot.span.store(innermost, Ordering::Relaxed);
    // A span a panic names is written whole, never folded.
    flight::keep(innermost);
    let build = match flight::current_build() {
        0 => flight::build_of(innermost),
        build => build,
    };
    slot.build.store(u64::from(build), Ordering::Relaxed);
    slot.job.store(flight::running_job(), Ordering::Relaxed);
    slot.seq.store(2 * n + 2, Ordering::Release);
}

/// Text written into a fixed buffer, cut at its end.
struct Cut<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> std::fmt::Write for Cut<N> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let room = N - self.len;
        let n = text.len().min(room);
        self.bytes[self.len..self.len + n].copy_from_slice(&text.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// This thread's name into `out`: std's (whole) when it has one, else the system's; how many
/// bytes. A thread with neither is `thread <tid>`.
fn thread_name(out: &mut [u8; NAME_BYTES]) -> usize {
    let current = std::thread::current();
    let named = current.name().map(|name| {
        let n = name.len().min(NAME_BYTES);
        out[..n].copy_from_slice(&name.as_bytes()[..n]);
        n
    });
    if let Some(n) = named.filter(|n| *n > 0) {
        return n;
    }
    let n = system_thread_name(out);
    if n > 0 {
        return n;
    }
    let mut cut = Cut { bytes: [0; NAME_BYTES], len: 0 };
    _ = std::fmt::Write::write_fmt(&mut cut, format_args!("thread {}", crate::sampler::os_tid()));
    out[..cut.len].copy_from_slice(&cut.bytes[..cut.len]);
    cut.len
}

/// The calling thread's name as the system keeps it (Linux cuts it at 15 bytes), into `out`; how
/// many bytes, 0 for none. Async-signal-safe: for the calling thread `pthread_getname_np` copies
/// the name it keeps (Linux: `prctl`, a system call), with no lock and no allocation.
pub(crate) fn system_thread_name(out: &mut [u8]) -> usize {
    #[cfg(unix)]
    {
        // SAFETY: `out` is live and its length is passed; see above.
        let named = unsafe {
            libc::pthread_getname_np(libc::pthread_self(), out.as_mut_ptr().cast(), out.len())
        } == 0;
        if !named {
            return 0;
        }
        out.iter().position(|&it| it == 0).unwrap_or(out.len())
    }
    #[cfg(not(unix))]
    {
        _ = out;
        0
    }
}

/// Where the newest panic raised with `message` (`None` for a payload that is no text) that no
/// catcher took yet was raised, `<file:line:column> on thread <name>`, taking it: what the plane
/// says of a task's panic it caught. `None` when the ring holds no such panic: one raised past
/// [`KEPT`] ago, or an unwind no hook saw (the query engine's typed payloads).
pub fn claim(message: Option<&str>) -> Option<String> {
    let next = NEXT.load(Ordering::Acquire);
    for n in (next.saturating_sub(KEPT as u64)..next).rev() {
        let slot = &SLOTS[(n % KEPT as u64) as usize];
        if slot.seq.load(Ordering::Acquire) != 2 * n + 2 || !slot.message.holds(message) {
            continue;
        }
        if slot.claimed.swap(true, Ordering::AcqRel) {
            continue;
        }
        let location = slot.location.text().unwrap_or_else(|| "an unknown place".to_owned()); // alloc: a panic caught, once
        let thread = slot.thread.text().unwrap_or_default();
        fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) != 2 * n + 2 {
            continue;
        }
        // alloc: a panic caught, once
        return Some(format!(
            "{location} on thread {}",
            if thread.is_empty() { "<unnamed>" } else { &thread }
        ));
    }
    None
}

/// The newest whole panic of the thread `tid`: its message into `message` and its location into
/// `location`, with how many bytes of each (0 for none); `None` when the ring has none of it.
/// Atomics only, no lock, no allocation: a fatal signal's handler calls it (`crate::crash`).
pub(crate) fn last_of_thread(
    tid: u64,
    message: &mut [u8],
    location: &mut [u8],
) -> Option<(usize, usize)> {
    let next = NEXT.load(Ordering::Acquire);
    for n in (next.saturating_sub(KEPT as u64)..next).rev() {
        let slot = &SLOTS[(n % KEPT as u64) as usize];
        if slot.seq.load(Ordering::Acquire) != 2 * n + 2 || slot.tid.load(Ordering::Relaxed) != tid
        {
            continue;
        }
        let (said, _) = slot.message.copy(message);
        let (at, _) = slot.location.copy(location);
        fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) == 2 * n + 2 {
            return Some((said, at));
        }
    }
    None
}

/// A panic read from the ring, its stack not named yet.
#[derive(Clone, Debug)]
pub(crate) struct Raised {
    t_ns: i64,
    tid: u64,
    job: u64,
    span: u64,
    build: u64,
    caught: bool,
    /// Innermost first.
    pcs: Vec<u64>,
    message: Option<String>,
    location: Option<String>,
    thread: String,
    open: Vec<String>,
}

impl Raised {
    /// Its stack's addresses, innermost first.
    pub(crate) fn pcs(&self) -> &[u64] {
        &self.pcs
    }
}

/// What reading slot `n` found.
enum Found {
    Whole(Raised),
    /// Still being written: read it next time.
    Writing,
    /// Written over by a newer panic before it was read.
    Gone,
}

fn read(n: u64) -> Found {
    let slot = &SLOTS[(n % KEPT as u64) as usize];
    let before = slot.seq.load(Ordering::Acquire);
    if before < 2 * n + 2 {
        return Found::Writing;
    }
    if before > 2 * n + 2 {
        return Found::Gone;
    }
    let depth = (slot.depth.load(Ordering::Relaxed) as usize).min(stack::DEPTH);
    let opened = (slot.opened.load(Ordering::Relaxed) as usize).min(OPEN);
    let raised = Raised {
        t_ns: slot.t_ns.load(Ordering::Relaxed) as i64,
        tid: slot.tid.load(Ordering::Relaxed),
        job: slot.job.load(Ordering::Relaxed),
        span: slot.span.load(Ordering::Relaxed),
        build: slot.build.load(Ordering::Relaxed),
        caught: slot.caught.load(Ordering::Relaxed),
        // alloc: a panic's row, once a flush
        pcs: slot.pcs[..depth].iter().map(|it| it.load(Ordering::Relaxed)).collect(),
        message: slot.message.text(),
        location: slot.location.text(),
        thread: slot.thread.text().unwrap_or_default(),
        // alloc: a panic's row, once a flush
        open: slot.open[..opened].iter().map(|it| it.text().unwrap_or_default()).collect(),
    };
    fence(Ordering::Acquire);
    match slot.seq.load(Ordering::Relaxed) == before {
        true => Found::Whole(raised),
        false => Found::Gone,
    }
}

/// The panics raised since panic number `next` (of every panic the process raised), into
/// `into`, moving `next` past them; a panic still being written stops the read, to be read next
/// time. Those written over before they were read are left out: their count is the result.
pub(crate) fn since(next: &mut u64, into: &mut Vec<Raised>) -> u64 {
    let raised = NEXT.load(Ordering::Acquire);
    let first = (*next).max(raised.saturating_sub(KEPT as u64));
    let mut skipped = first - (*next).min(first);
    let mut n = first;
    while n < raised {
        match read(n) {
            Found::Whole(it) => into.push(it),
            Found::Writing => break,
            Found::Gone => skipped += 1,
        }
        n += 1;
    }
    *next = n;
    skipped
}

/// A row of the schema's `panics` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanicRow {
    pub build_id: Option<u64>,
    pub process: u32,
    pub thread: u32,
    pub thread_name: String,
    pub t_ns: i64,
    pub kind: Kind,
    pub job: Option<u64>,
    pub span_id: Option<u64>,
    pub message: Option<String>,
    pub location: Option<String>,
    /// Outermost first, each frame named: a function and those inlined into it are a frame
    /// each, so it can be longer than `stack_pcs`.
    pub stack: Vec<String>,
    /// The stack's addresses, outermost first, one a frame of the unwind: named again after the
    /// process is gone through the `images` row (of the same `process` and `boot`) holding each.
    pub stack_pcs: Vec<u64>,
    /// The machine's boot the process ran in, as `images.boot`: with `process`, the key of its
    /// `images` rows. `None` where it can't be read, and for a recovered fatal signal (the
    /// machine may have restarted since; its `images` row has none either).
    pub boot: Option<&'static str>,
    pub caught: bool,
    /// Outermost first.
    pub open_spans: Vec<String>,
    pub signal: Option<&'static str>,
    pub fault_addr: Option<u64>,
}

/// The rows of `raised`, panics of this process, their frames named once for them all.
pub(crate) fn rows(raised: &[Raised]) -> Vec<PanicRow> {
    if raised.is_empty() {
        // alloc: none: an empty `Vec` holds nothing
        return Vec::new();
    }
    let frames = crate::symbols::frames(raised.iter().map(|it| it.pcs.as_slice()));
    let boot = ix_prof::images::boot();
    let process = std::process::id();
    raised
        .iter()
        .map(|it| PanicRow {
            build_id: (it.build != 0).then_some(it.build),
            process,
            thread: it.tid as u32,
            // clone: a panic's row, once a flush
            thread_name: it.thread.clone(),
            t_ns: it.t_ns,
            kind: Kind::Panic,
            job: (it.job != 0).then_some(it.job),
            span_id: (it.span != 0 && it.span != flight::UNRECORDED).then_some(it.span),
            // clone: a panic's row, once a flush
            message: it.message.clone(),
            // clone: a panic's row, once a flush
            location: it.location.clone(),
            stack: crate::symbols::stack(&it.pcs, &frames),
            // alloc: a panic's row, once a flush
            stack_pcs: it.pcs.iter().rev().copied().collect(),
            boot,
            caught: it.caught,
            // clone: a panic's row, once a flush
            open_spans: it.open.clone(),
            signal: None,
            fault_addr: None,
        })
        // alloc: a flush's rows
        .collect()
}

fn column(array: impl arrow_array::Array + 'static) -> ArrayRef {
    // alloc: a batch's column, once a flush
    std::sync::Arc::new(array)
}

fn dict<'a>(values: impl Iterator<Item = Option<&'a str>>) -> ArrayRef {
    column(values.collect::<DictionaryArray<UInt32Type>>())
}

/// A list column of `table`'s column `name`: its Arrow field, as the schema types it.
fn list_field(name: &str) -> std::sync::Arc<arrow_schema::Field> {
    let schema = batches::schema_of(&schema::PANICS);
    let at = schema::PANICS.column(name).unwrap_or_else(|| unreachable!("panics has a {name}"));
    let DataType::List(field) = schema.field(at).data_type() else {
        unreachable!("{name} is a list (arrow.rs)")
    };
    // clone: an `Arc` of the schema's field, once a batch
    field.clone()
}

/// A [`schema::Type::Stack`] column of `stacks`.
fn names_column<'a>(name: &str, stacks: impl Iterator<Item = &'a [String]>) -> ArrayRef {
    let mut out =
        ListBuilder::new(StringDictionaryBuilder::<UInt32Type>::new()).with_field(list_field(name));
    for stack in stacks {
        for frame in stack {
            out.values().append_value(frame);
        }
        out.append(true);
    }
    column(out.finish())
}

/// The `panics` rows of `rows`.
pub fn batch(rows: &[PanicRow]) -> Result<RecordBatch, ArrowError> {
    let mut pcs = ListBuilder::new(UInt64Builder::new()).with_field(list_field("stack_pcs"));
    for row in rows {
        pcs.values().append_slice(&row.stack_pcs);
        pcs.append(true);
    }
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(rows.iter().map(|it| it.build_id))),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| it.process))),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| it.thread))),
        dict(rows.iter().map(|it| Some(it.thread_name.as_str()))),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.t_ns))),
        dict(rows.iter().map(|it| Some(it.kind.name()))),
        column(UInt64Array::from_iter(rows.iter().map(|it| it.job))),
        column(UInt64Array::from_iter(rows.iter().map(|it| it.span_id))),
        column(StringArray::from_iter(rows.iter().map(|it| it.message.as_deref()))),
        column(StringArray::from_iter(rows.iter().map(|it| it.location.as_deref()))),
        names_column("stack", rows.iter().map(|it| it.stack.as_slice())),
        column(pcs.finish()),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| u32::from(it.caught)))),
        names_column("open_spans", rows.iter().map(|it| it.open_spans.as_slice())),
        dict(rows.iter().map(|it| it.signal)),
        column(UInt64Array::from_iter(rows.iter().map(|it| it.fault_addr))),
        dict(rows.iter().map(|it| it.boot)),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::PANICS), columns)
}

/// The frames of a stack (outermost first) a person reads first: innermost first, past the
/// panic machinery's own frames (the hook, std's and core's panicking, up to the innermost
/// frame that entered it), the first `top` of them.
pub fn top_frames(stack: &[String], top: usize) -> Vec<&str> {
    /// What the frames that raise a panic are named: the last of them among a stack's innermost
    /// [`LOOKED`] is where the code that panicked begins.
    const ENTRY: [&str; 6] = [
        "core::panicking::",
        "std::panicking::",
        "rust_begin_unwind",
        "std::sys::backtrace::",
        "blaze_profile::panics::",
        "blaze_profile::stack::",
    ];
    const LOOKED: usize = 24;
    let inner: Vec<&str> = stack.iter().rev().map(String::as_str).collect(); // alloc: a report's lines
    let past = inner
        .iter()
        .take(LOOKED)
        .rposition(|frame| ENTRY.iter().any(|it| frame.contains(it)))
        .map_or(0, |at| at + 1);
    // alloc: a report's lines
    inner[past..].iter().take(top).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn panics_in_a_job(text: &str) {
        std::hint::black_box(());
        panic!("{text}");
    }

    /// A panic a job's boundary catches is one row, with its message, its location, a stack that
    /// names the function that raised it, and `caught`; the plane's claim finds where it was raised.
    #[test]
    fn a_caught_panic_is_one_row_with_its_message_location_and_stack() {
        record(|| true);
        let text = format!("a panic for the panics test {}", std::process::id());
        let mut next = NEXT.load(Ordering::Acquire);
        let caught = std::panic::catch_unwind(|| panics_in_a_job(&text));
        assert!(caught.is_err());
        let at = claim(Some(&text)).expect("the ring has the panic's location");
        assert!(at.contains("panics.rs:"), "{at}");
        // alloc: a test
        let mut raised = Vec::new();
        since(&mut next, &mut raised);
        let rows = rows(&raised);
        let ours: Vec<&PanicRow> =
            rows.iter().filter(|it| it.message.as_deref() == Some(text.as_str())).collect();
        let [row] = &ours[..] else { panic!("one row of the panic: {ours:?}") };
        assert_eq!(row.kind, Kind::Panic);
        assert!(row.caught);
        assert!(
            row.location.as_deref().is_some_and(|it| it.contains("src/panics.rs:")),
            "{:?}",
            row.location
        );
        assert!(
            row.stack.len() >= row.stack_pcs.len(),
            "a frame of the unwind is one name or more"
        );
        assert!(row.stack.iter().any(|it| it.contains("panics_in_a_job")), "{:#?}", row.stack);
        assert!(
            row.stack.iter().all(|it| !it.contains("+0x")),
            "named by function: {:#?}",
            row.stack
        );
        assert_eq!(row.process, std::process::id());
        let batch = batch(&rows).unwrap();
        assert_eq!(batch.num_rows(), rows.len());
        assert_eq!(batch.schema(), batches::schema_of(&schema::PANICS));
        let top = top_frames(&row.stack, 3);
        assert!(top.first().is_some_and(|it| !it.contains("std::panicking")), "{top:?}");
    }

    #[test]
    fn a_long_message_is_cut_and_says_so_and_still_matches() {
        let text = Text::<{ MESSAGE_BYTES / 8 }>::new();
        let long = "x".repeat(MESSAGE_BYTES + 10);
        text.put(Some(long.as_bytes()));
        let said = text.text().unwrap();
        assert!(said.ends_with("...") && said.len() == MESSAGE_BYTES + 3);
        assert!(text.holds(Some(&long)));
        assert!(!text.holds(Some("x")));
        text.put(None);
        assert_eq!(text.text(), None);
        assert!(text.holds(None));
    }
}
