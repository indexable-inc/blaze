//! Watching a tree on macOS with FSEvents: one stream per watch, file-level events with no
//! latency, delivered on a serial dispatch queue of the watch's own and handed on as
//! [`Done::Changed`].
//!
//! A flush is a sync cookie, as watchman's: each watch also watches a directory of cookies of
//! its own, a flush writes a file there, and the flush is told once the stream hears of that
//! file. fseventsd reads the kernel's one queue of events in order and hands a stream its
//! events in that order, so by then every change made before the cookie has been heard.
//! `FSEventStreamFlushSync` is no barrier: it flushes what fseventsd holds back for latency,
//! not what the kernel queued that fseventsd has not read yet, and it blocks the caller.
//!
//! A journal ([`Watch::journal`]) is a stream started `sinceWhen` a saved event ID, which
//! fseventsd answers from its store of the volume's events before the live ones, ending the
//! history with `kFSEventStreamEventFlagHistoryDone`. Event IDs are the system's, in the order
//! events happened; a saved ID means something only while the volume's store has the UUID it
//! was saved with (`FSEventsCopyUUIDForDevice`) and the system's IDs have not passed below it
//! (wrapped).
//!
//! Memory (docs/v2/plan/design/memory.md): a [`Watch`] is made once and runs one stream after
//! another. Its directory of cookies, its dispatch queue, the CFString of its cookies and every
//! buffer the callback uses are kept across streams, so a warm watch's start, batches and stop
//! allocate only what a completion carries (its `Vec` and each name told once). Completions go
//! to a [`Tell`] the driver gives, not a boxed closure. FSEvents' callback holds the address of
//! the watch's [`Shared`], which is an allocation of its own (made once per watch, in
//! [`Watch::new`]) held by raw pointer: the watch itself may move while a stream runs, so its
//! API is safe, and a `&mut Watch` never covers what the queue's thread reads. A `Box` would
//! not do: moving one asserts it unique, which the callback's reads would break.
//!
//! Why unsafe: FSEvents, CoreFoundation and dispatch are C APIs that std does not wrap, and the
//! one safe way to see changes without them is to walk and `stat` the tree on every poll, a
//! system call per file per poll where FSEvents tells only what changed. The callback's arrays
//! are taken as slices of the `count` FSEvents passes, after an empty batch is returned from.

use std::{
    ffi::{CStr, OsStr, c_char, c_void},
    io,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use blaze_sys::posix::{proc, thread::{self, RtldFlags}, types::dev_t};
use crate::lock::{Lock, LockGuard};

use crate::{Completion, Cursor, DirName, Done, FileName, Journaled, Lost, Tag};

type CFIndex = isize;
type CFRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

type Callback = extern "C" fn(
    stream: CFRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
);

#[repr(C)]
struct Context {
    version: CFIndex,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    describe: Option<extern "C" fn(*const c_void) -> CFRef>,
}

#[repr(C)]
struct CFArrayCallBacks {
    _opaque: [u8; 0],
}

/// CFUUID.h's `CFUUIDBytes`: sixteen `UInt8`s, returned by value.
#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes {
    bytes: [u8; 16],
}

// What a journal stands on, quoted from the SDK's FSEvents.h (CoreServices.framework,
// FSEvents.framework/Headers/FSEvents.h), each where the code relies on it:
//
// - `sinceWhen` (FSEventStreamCreate): "The service will supply events that have happened
//   after the given event ID." Then (step 3, FSEventStreamStart): "'historical' events will be
//   sent via your callback first, then a HistoryDone event, then 'contemporary' events".
// - The place is per volume and kept with its UUID (FSEventStreamGetLatestEventId): "Clients
//   can store this value persistently as long as they also store the UUID for the device
//   (obtained via FSEventsCopyUUIDForDevice()) ... This works because the FSEvents service
//   stores events in a persistent, per-volume database. In this regard, the stream of event
//   IDs acts like a global, system-wide clock". FSEventsGetCurrentEventId: "Fetches the most
//   recently generated event ID, system-wide (not just for one stream)."
// - The UUID (FSEventsCopyUUIDForDevice): "If this (non-NULL) UUID is different than one that
//   you stored from a previous run then the event stream is different (for example, because
//   FSEvents were purged, because the disk was erased, or because the event ID counter wrapped
//   around back to zero). A NULL return value indicates that 'historical' events are not
//   available". So a cursor of another UUID, or of an ID past the current one, is a reset
//   ([`Watch::journal`], and again journal_sweep.rs `judge` in blaze-cli).

const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_WATCH_ROOT: u32 = 0x04;
const CREATE_FILE_EVENTS: u32 = 0x10;
/// `kFSEventStreamCreateFlagFullHistory` (macOS 10.15): "When requesting historical events it
/// is possible that some events may get skipped due to the way they are stored. With this flag
/// all historical events in a given chunk are returned even if their event-id is less than the
/// sinceWhen id." And: "deliver all the events in the first chunk of historical events that
/// contains the sinceWhen id so that none are skipped even if their id is less than the
/// sinceWhen id." Passed for every journal, so no event after the place is skipped; the events
/// of that chunk at or before the place are then dropped here ([`after_place`]), never told:
/// told, they were names a resume repeats on every command (the test
/// `a_journal_resumes_from_its_cursor_with_what_changed_while_it_was_stopped` failed so on
/// 2026-11-19, `src/before.rs` told again from before its cursor, 3 runs of 3).
const CREATE_FULL_HISTORY: u32 = 0x80;
/// "Your application must rescan not just the directory given in the event, but all its
/// children, recursively. This can happen if there was a problem whereby events were coalesced
/// hierarchically": a [`Lost::Rescan`] of that subtree.
const EVENT_MUST_SCAN_SUB_DIRS: u32 = 0x01;
/// With MustScanSubDirs, "a problem occurred in buffering the events ... and that the client
/// must do a full scan of any directories (and their subdirectories, recursively) being
/// monitored by this stream": [`Lost::Dropped`] of the whole folder.
const EVENT_USER_DROPPED: u32 = 0x02;
const EVENT_KERNEL_DROPPED: u32 = 0x04;
/// "the 64-bit event ID counter wrapped around. As a result, previously-issued event ID's are
/// no longer valid arguments for the sinceWhen parameter": [`Lost::Reset`].
const EVENT_IDS_WRAPPED: u32 = 0x08;
/// "a sentinel event sent to mark the end of the 'historical' events ... The client should
/// ignore the path supplied in this callback": the batch is `caught_up`.
const EVENT_HISTORY_DONE: u32 = 0x10;
/// "a change to one of the directories along the path to one of the directories you asked to
/// watch. When this flag is set, the event ID is zero": [`Lost::RootChanged`], the folder whole.
const EVENT_ROOT_CHANGED: u32 = 0x20;
const EVENT_MOUNT: u32 = 0x40;
const EVENT_UNMOUNT: u32 = 0x80;
/// Events after which the names given are not all that changed.
const LOSES: u32 = EVENT_MUST_SCAN_SUB_DIRS
    | EVENT_USER_DROPPED
    | EVENT_KERNEL_DROPPED
    | EVENT_ROOT_CHANGED
    | EVENT_MOUNT
    | EVENT_UNMOUNT;

/// How long the callback waits for room before it looks again at whether its stream is
/// stopping: a stop waits on the queue, so a callback waiting on a full sink must give up.
const ROOM: Duration = Duration::from_millis(1);

/// CoreFoundation's and FSEvents' calls this driver makes, found when the first watch is made
/// ([`Frameworks::get`]), never linked: a program that never watches loads neither framework at
/// start (docs/v2/plan/design/std.md, "What a program pays for at start"). dyld would otherwise
/// load and initialise both, with everything they depend on, on every run of every program
/// with blaze-core in it. A `Watch` holds the `&'static Frameworks` it was made with, so every
/// call below is made only once they are loaded.
///
/// The signatures are CoreFoundation's CFString.h, CFArray.h and CFUUID.h (`CFRef` is
/// `CFTypeRef` and `CFUUIDRef`, `CFIndex` is `isize`, `CFUUIDBytes` is laid out as its sixteen
/// `UInt8` fields) and FSEvents.h (`FSEventStreamEventId` is `UInt64`,
/// `FSEventStreamCreateFlags` is `UInt32`, `dev_t` is `__darwin_dev_t`, an `int32_t`); a
/// mismatch would be UB at every call.
struct Frameworks {
    /// `kCFTypeArrayCallBacks`: a constant of CoreFoundation's, never written.
    type_array_callbacks: *const CFArrayCallBacks,
    string_create_with_file_system_representation:
        unsafe extern "C" fn(allocator: CFRef, path: *const c_char) -> CFRef,
    array_create: unsafe extern "C" fn(
        allocator: CFRef,
        values: *const CFRef,
        count: CFIndex,
        callbacks: *const CFArrayCallBacks,
    ) -> CFRef,
    uuid_get_uuid_bytes: unsafe extern "C" fn(uuid: CFRef) -> CFUUIDBytes,
    release: unsafe extern "C" fn(object: CFRef),
    stream_create: unsafe extern "C" fn(
        allocator: CFRef,
        callback: Callback,
        context: *const Context,
        paths: CFRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef,
    stream_set_dispatch_queue: unsafe extern "C" fn(stream: FSEventStreamRef, queue: DispatchQueue),
    stream_start: unsafe extern "C" fn(stream: FSEventStreamRef) -> u8,
    stream_stop: unsafe extern "C" fn(stream: FSEventStreamRef),
    stream_invalidate: unsafe extern "C" fn(stream: FSEventStreamRef),
    stream_release: unsafe extern "C" fn(stream: FSEventStreamRef),
    get_current_event_id: unsafe extern "C" fn() -> u64,
    copy_uuid_for_device: unsafe extern "C" fn(device: dev_t) -> CFRef,
}

// SAFETY: function pointers, and the address of a constant of CoreFoundation's that nothing
// writes; both frameworks stay loaded for the life of the process (never `dlclose`d).
unsafe impl Send for Frameworks {}
// SAFETY: as above: nothing in it is written after it is made.
unsafe impl Sync for Frameworks {}

/// A framework, or a symbol in it, that could not be found at first use.
#[derive(Clone, Copy, Debug)]
struct Missing {
    /// The framework's binary, as `dlopen` was given it.
    framework: &'static CStr,
    /// The symbol it lacked; `None` when the framework itself did not load.
    symbol: Option<&'static CStr>,
}

impl Missing {
    fn to_error(self) -> io::Error {
        let framework = self.framework.to_string_lossy();
        // alloc: an error path.
        io::Error::new(io::ErrorKind::Unsupported, match self.symbol {
            None => format!("watching needs {framework}, which this system cannot load"),
            Some(symbol) => format!(
                "watching needs `{}` from {framework}, which this system's copy does not have",
                symbol.to_string_lossy()
            ),
        })
    }
}

const CORE_FOUNDATION: &CStr = c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
const CORE_SERVICES: &CStr = c"/System/Library/Frameworks/CoreServices.framework/CoreServices";

impl Frameworks {
    /// Both frameworks, loaded and their symbols found on the first call of the process (each
    /// later call reads what that one found, an error included): the one place this driver
    /// pays for them.
    fn get() -> io::Result<&'static Frameworks> {
        static LOADED: std::sync::OnceLock<Result<Frameworks, Missing>> = std::sync::OnceLock::new();
        match LOADED.get_or_init(Frameworks::load) {
            Ok(it) => Ok(it),
            Err(missing) => Err(missing.to_error()),
        }
    }

    fn load() -> Result<Frameworks, Missing> {
        let cf = open(CORE_FOUNDATION)?;
        let cs = open(CORE_SERVICES)?;
        // SAFETY: each type is the symbol's as the header declares it (the struct's doc).
        unsafe {
            Ok(Frameworks {
                type_array_callbacks: symbol(cf, CORE_FOUNDATION, c"kCFTypeArrayCallBacks")?,
                string_create_with_file_system_representation: symbol(
                    cf,
                    CORE_FOUNDATION,
                    c"CFStringCreateWithFileSystemRepresentation",
                )?,
                array_create: symbol(cf, CORE_FOUNDATION, c"CFArrayCreate")?,
                uuid_get_uuid_bytes: symbol(cf, CORE_FOUNDATION, c"CFUUIDGetUUIDBytes")?,
                release: symbol(cf, CORE_FOUNDATION, c"CFRelease")?,
                stream_create: symbol(cs, CORE_SERVICES, c"FSEventStreamCreate")?,
                stream_set_dispatch_queue: symbol(cs, CORE_SERVICES, c"FSEventStreamSetDispatchQueue")?,
                stream_start: symbol(cs, CORE_SERVICES, c"FSEventStreamStart")?,
                stream_stop: symbol(cs, CORE_SERVICES, c"FSEventStreamStop")?,
                stream_invalidate: symbol(cs, CORE_SERVICES, c"FSEventStreamInvalidate")?,
                stream_release: symbol(cs, CORE_SERVICES, c"FSEventStreamRelease")?,
                get_current_event_id: symbol(cs, CORE_SERVICES, c"FSEventsGetCurrentEventId")?,
                copy_uuid_for_device: symbol(cs, CORE_SERVICES, c"FSEventsCopyUUIDForDevice")?,
            })
        }
    }
}

/// The framework at `path`, loaded for the life of the process.
fn open(path: &'static CStr) -> Result<*mut c_void, Missing> {
    // SAFETY: a NUL-terminated path; the handle is never closed, so what it gives stays valid.
    let handle = unsafe { thread::dlopen(path.as_ptr(), (RtldFlags::LAZY | RtldFlags::LOCAL).bits()) };
    match handle.is_null() {
        true => Err(Missing { framework: path, symbol: None }),
        false => Ok(handle),
    }
}

/// The symbol `name` of the framework `handle` loaded, as an `F`.
///
/// # Safety
///
/// `F` is a pointer type (a function pointer or a raw pointer) that is the symbol's type as its
/// header declares it; a call through another would be UB.
unsafe fn symbol<F: Copy>(
    handle: *mut c_void,
    framework: &'static CStr,
    name: &'static CStr,
) -> Result<F, Missing> {
    const {
        assert!(size_of::<F>() == size_of::<*mut c_void>(), "a symbol is one address");
        assert!(align_of::<F>() == align_of::<*mut c_void>(), "a symbol has pointer alignment");
    }
    // SAFETY: a handle `open` returned, never closed, and a NUL-terminated name.
    let found = unsafe { thread::dlsym(handle, name.as_ptr()) };
    if found.is_null() {
        return Err(Missing { framework, symbol: Some(name) });
    }
    // SAFETY: `F` has pointer size & alignment (asserted above) and names the loaded symbol's ABI.
    Ok(unsafe { core::ptr::read((&raw const found).cast::<F>()) })
}

// libdispatch, in libSystem: through blaze-sys's table of calls found at first use, so a
// program that never watches binds none of them at load. The signatures (dispatch/queue.h and
// dispatch/object.h) are the table's.
use blaze_sys::macos::cold::{dispatch_queue_create, dispatch_release, dispatch_sync_f};

/// Where a watch's completions go, from its queue's thread.
pub(crate) trait Tell: Send + Sync + 'static {
    /// Hands `completion` on, waiting up to `wait` for room: `Err(Some(it))` when there was
    /// none by then, `Err(None)` when nobody takes completions any more.
    fn tell(&self, completion: Completion, wait: Duration) -> Result<(), Option<Completion>>;
}

/// A bounded channel waits up to `wait` for room and allocates nothing per completion; an
/// unbounded one always has room, and allocates a block per 31 completions.
impl Tell for crate::chan::Sender<Completion> {
    fn tell(&self, completion: Completion, wait: Duration) -> Result<(), Option<Completion>> {
        self.send_timeout(completion, wait).map_err(|error| match error {
            crate::chan::SendTimeoutError::Timeout(completion) => Some(completion),
            crate::chan::SendTimeoutError::Disconnected(_) => None,
        })
    }
}

/// What a stream tells of what it hears.
enum Mode {
    /// [`Done::Changed`], from when it started.
    Watch,
    /// [`Done::Journaled`], from a place in the volume's store.
    Journal(Journal),
}

/// A journal's place, read and moved on the stream's queue only.
///
/// **The rule of a place** (stated once here; bstd fs/journal.rs and blaze-cli journal_sweep.rs
/// stand on it). A place `P` a journal hands on means: (1) every change beneath the folder whose
/// event ID is at or before `P` has been told to whoever took `P`, and (2) a journal started
/// from `P` tells exactly the changes whose IDs are after `P`. (1) holds because `P` is the
/// highest ID told, and FSEvents hands one stream its events in ID order (FSEvents.h,
/// `eventIds`: "Event IDs all come from a single global source. They are guaranteed to always
/// be increasing"), so a caller that waits for a flush's cookie (written after the changes it
/// must see) and takes the cursor of the batches told before `Flushed` holds a place at or
/// after every change made before that flush. Never `FSEventsGetCurrentEventId()` read after
/// the flush: it is the system's newest ID, and an event of this folder below it may not have
/// been told yet, so a place there would skip it. (2) holds by [`CREATE_FULL_HISTORY`] (no
/// event after `P` skipped) and [`after_place`] (none at or before `P` told).
struct Journal {
    /// The UUID of the watched directory's volume's store.
    volume: [u8; 16],
    /// The place the stream was started from: an event at or before it is not told.
    from: u64,
    /// The highest event ID told.
    last: AtomicU64,
    /// Whether `kFSEventStreamEventFlagHistoryDone` has been heard.
    caught_up: AtomicBool,
}

/// What the callback reads, on the watch's queue. Set by [`Watch`] only while no stream runs.
struct Shared<S> {
    tag: Tag,
    /// The watched directory, from the first start on: a named one, or the root itself.
    dir: Option<DirName>,
    /// The root with its symlinks resolved, as FSEvents names paths.
    root: PathBuf,
    /// The watched directory with its symlinks resolved, where it is one of its own.
    real_dir: PathBuf,
    /// This watch's cookies, symlinks resolved: watched with `real_dir`, named by the tag of
    /// the flush that wrote each.
    cookies: PathBuf,
    /// Every watch's cookies under the root, symlinks resolved ([`cookies`]): a name under it
    /// is never a change.
    cookie_area: PathBuf,
    /// Each flush whose cookie the stream has not heard of yet.
    flushes: Lock<Vec<Tag>>,
    /// `cookies`, kept to name a flush's cookie in without allocating ([`with_cookie`]).
    writing: Lock<PathBuf>,
    /// The callback's buffers, kept for the watch's life: on its queue only, so never waited
    /// for.
    heard: Lock<Heard>,
    sink: S,
    stopped: AtomicBool,
    mode: Mode,
    /// What the callbacks allocated beyond what their completions carry.
    #[cfg(test)]
    overhead: AtomicU64,
    /// What the running callback allocated for what its completions carry.
    #[cfg(test)]
    payload: AtomicU64,
    /// Every event a journal stream heard, with what became of it: the evidence a journal test
    /// prints when it fails (rule 19). Journals only; taken by [`Watch::take_trace`].
    #[cfg(test)]
    trace: Lock<Vec<Traced>>,
}

/// One event a journal heard, as its test reads it back.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct Traced {
    pub(crate) id: u64,
    pub(crate) flags: u32,
    pub(crate) path: String,
    /// The place the stream was started from.
    pub(crate) from: u64,
    /// Heard before HistoryDone.
    pub(crate) in_history: bool,
    /// What `journaled` did with it.
    pub(crate) verdict: &'static str,
}

/// `id`, `flag` and `path` of one event of a journal stream, traced with what `journaled` does
/// with it (its arms, in its order).
#[cfg(test)]
fn trace<S: Tell>(shared: &Shared<S>, journal: &Journal, flag: u32, path: *const c_char, id: u64, in_history: bool) {
    // SAFETY: a path FSEvents passed, alive for the callback.
    let path = unsafe { path_of(path) };
    let verdict = if flag & EVENT_HISTORY_DONE != 0 {
        "history done"
    } else if flag & EVENT_IDS_WRAPPED != 0 {
        "ids wrapped: a reset"
    } else if flag & (EVENT_USER_DROPPED | EVENT_KERNEL_DROPPED) != 0 {
        "dropped: a loss, kept whatever its id"
    } else if shared.cookie(path).is_some() {
        "a cookie"
    } else if flag & (EVENT_ROOT_CHANGED | EVENT_MOUNT | EVENT_UNMOUNT) != 0 {
        "root changed or mount: a loss, kept whatever its id"
    } else if after_place(journal.from, id) {
        "kept: after the place (or id 0)"
    } else {
        "dropped: at or before the place"
    };
    lock(&shared.trace).push(Traced {
        id,
        flags: flag,
        path: path.display().to_string(),
        from: journal.from,
        in_history,
        verdict,
    });
}

/// A name in [`Heard::text`]: its start and end.
type Span = (u32, u32);

/// The callback's buffers, grown once to their most and reused, so a batch allocates only what
/// its completion carries (docs/v2/plan/design/memory.md: scratch reused per worker).
struct Heard {
    /// The cookies a batch named.
    cookies: Vec<u64>,
    /// The flushes those cookies tell.
    told: Vec<Tag>,
    /// `cookies`, to name a cookie in for its removal.
    path: PathBuf,
    /// The names a batch gave, end to end: sorted and made unique before any [`FileName`] is
    /// made of them.
    text: String,
    names: Vec<Span>,
    /// A journal batch's lost subtrees: the name, why, and the order it was heard in.
    lost: Vec<(Span, Lost, u32)>,
}

impl Heard {
    fn str(&self, span: Span) -> &str {
        &self.text[span.0 as usize..span.1 as usize]
    }
}

fn lock<T>(mutex: &Lock<T>) -> LockGuard<'_, T> {
    mutex.lock()
}

/// `dir` in a buffer with room for a cookie's name after it, for [`with_cookie`].
fn cookie_buffer(dir: &Path) -> PathBuf {
    // alloc: setup, once per watch.
    let mut buffer = PathBuf::with_capacity(dir.as_os_str().len() + 21);
    buffer.push(dir);
    buffer
}

/// Runs `f` on the path of the cookie of the flush `tag`, made in `dir`, the cookies'
/// directory: its name is pushed and popped again, so a buffer grown once allocates no more.
/// std names a path this short to the system from the stack.
fn with_cookie<R>(dir: &mut PathBuf, tag: Tag, f: impl FnOnce(&Path) -> R) -> R {
    let mut digits = [0; 20];
    dir.push(OsStr::from_bytes(crate::decimal::decimal(tag.0, &mut digits).as_bytes()));
    let result = f(dir);
    dir.pop();
    result
}

/// Numbers each watch's directory of cookies in the process's.
static WATCHES: AtomicU64 = AtomicU64::new(0);

/// Where under a driver's root every watch's cookies are: [`cookies`].
const COOKIE_AREA: &str = ".blaze/v2/fsevents";

/// The cookie area under `root` and a directory of cookies for one watch in it, made empty,
/// both with their symlinks resolved: `<root>/.blaze/v2/fsevents/<pid>-<n>`.
///
/// Under the root, never the temporary directory: a journal's stream names its cookies beside
/// its watched folder, and fseventsd answers a stream started `sinceWhen` a place from the
/// store of EVERY volume its paths are on. With the cookies in `$TMPDIR` (the boot volume's
/// Data volume, the busiest one) a checkout on another volume had each replay also read the
/// Data volume's history since its place, a cost that grows with that volume's writes and not
/// the checkout's. Under the root, a stream's paths are on one volume, and for a checkout run
/// from its own folder they are ONE path ([`Watch::open`]): the cookies are under it.
///
/// Every name under the area is a cookie's, this process's or another's ([`Shared::cookie`]):
/// a history holds earlier commands' cookies, and none of them is a change of the tree.
///
/// **Its name is its owner's identity:** `<pid>-<start>-<n>`, `<start>` the instant the kernel
/// recorded for the process's exec ([`started`]). A folder is left behind only by a process
/// that was killed or crashed (a watch removes its own when dropped), so each new one first
/// sweeps the area of folders whose owner is dead ([`sweep_dead`]): a command when it makes its
/// own, the journal helper at its start (both through here).
fn cookies(root: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let number = WATCHES.fetch_add(1, Ordering::Relaxed);
    let area = root.join(COOKIE_AREA); // alloc: setup, once per watch.
    let pid = std::process::id();
    let start = match started(pid) {
        Started::At(start) => start,
        Started::NoProcess | Started::Unknown(_) => {
            return Err(io::Error::other("proc_pid_rusage of this very process gave no start: its cookies' folder cannot be named by its owner"));
        }
    };
    sweep_dead(&area, started);
    // alloc: setup, once per watch.
    let dir = area.join(format!("{pid}-{start}-{number}"));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok((std::fs::canonicalize(&area)?, std::fs::canonicalize(&dir)?))
}

/// What the kernel says of process `pid`'s start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Started {
    /// It runs, and was exec'd at this instant (`ri_proc_start_abstime`, Mach ticks).
    At(u64),
    /// No process has that id (`ESRCH`).
    NoProcess,
    /// The kernel would not say (another user's process: `EPERM`; the errno kept).
    Unknown(i32),
}

/// `pid`'s start: `proc_pid_rusage(RUSAGE_INFO_V4)`'s `ri_proc_start_abstime` (as wait/macos.rs
/// `process_age` reads its own), one call with no file opened.
fn started(pid: u32) -> Started {
    let Ok(pid) = i32::try_from(pid) else { return Started::NoProcess };
    let mut info = std::mem::MaybeUninit::<thread::rusage_info_v4>::zeroed();
    // SAFETY: the flavour says the buffer is a `rusage_info_v4`, which `info` is, for the
    // kernel to fill; it keeps nothing.
    let got = unsafe { thread::proc_pid_rusage(pid, thread::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
    if got != 0 {
        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        // ESRCH is 3 on every Unix.
        return if errno == 3 { Started::NoProcess } else { Started::Unknown(errno) };
    }
    // sys/resource.h: ri_proc_start_abstime is word 8 following the UUID in rusage_info_v4.
    const START_ABSTIME: usize = 8;
    // SAFETY: filled by the call that returned 0.
    Started::At(unsafe { info.assume_init() }.words[START_ABSTIME])
}

/// Whether the folder named `name` in the cookie area is of a process that is dead, asked of
/// `started` (sans-IO: [`sweep_dead`] is its driver, the test its simulation).
///
/// **Not racy, and why:** the verdict "dead" is drawn only from a fact that cannot change
/// back. A folder's name is written whole at its `mkdir`, by its owner, with the owner's own
/// `(pid, start)`; so a live owner always reads `At(start)` equal to its name's, from before its
/// folder exists to its exit, and is never swept. "Dead" is `NoProcess`, or a process at that
/// pid with ANOTHER start: the pid was reused, and the start of the process holding it now
/// is not the one named, which no later moment makes true again (a process's start never
/// changes, and the named one has exited). A pid reused within one Mach tick of the same start
/// would alias, which no kernel does. `Unknown` (the kernel would not say: another user's
/// process) keeps the folder. A name of the form before this one (`<pid>-<n>`, no start) is
/// swept only when no process has that pid at all: one that does may be a live older blaze.
fn owner_is_dead(name: &str, started: impl Fn(u32) -> Started) -> bool {
    let mut parts = name.split('-');
    let (Some(pid), Some(second), third, None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let Ok(pid) = pid.parse::<u32>() else { return false };
    if second.parse::<u64>().is_err() || third.is_some_and(|n| n.parse::<u64>().is_err()) {
        return false;
    }
    match (third, started(pid)) {
        (_, Started::NoProcess) => true,
        (_, Started::Unknown(_)) => false,
        // `<pid>-<start>-<n>`: dead when the process at that pid started at another instant.
        (Some(_), Started::At(now)) => second.parse::<u64>().is_ok_and(|named| named != now),
        // `<pid>-<n>`: a live pid may be its owner; kept.
        (None, Started::At(_)) => false,
    }
}

/// Each folder in `area` whose owner is dead ([`owner_is_dead`]) removed. A name of no folder
/// of this form (not ours) is left. A removal another sweeper finished first (`NotFound`) is
/// the same outcome; any other failure leaves that folder for the next sweep: a stale folder of
/// cookies is never a change of the tree ([`Shared::cookie`]), so leaving one is a cost of disk
/// and a name in a listing, not of correctness, and it does not fail this watch.
fn sweep_dead(area: &Path, started: impl Fn(u32) -> Started) {
    let Ok(entries) = std::fs::read_dir(area) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| owner_is_dead(name, &started)) {
            _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// `path` NUL-terminated in `into`, which keeps its room.
fn c_string(path: &Path, into: &mut Vec<u8>) -> io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"));
    }
    into.clear();
    into.extend_from_slice(bytes);
    into.push(0);
    Ok(())
}

/// The path NUL-terminated in `path` with its symlinks resolved, written to `out` (left as it
/// is on an error): `realpath(3)` into a buffer on the stack, as `std::fs::canonicalize` does
/// into one it allocates.
fn realpath(path: &[u8], out: &mut PathBuf) -> io::Result<()> {
    debug_assert_eq!(path.last(), Some(&0));
    let mut resolved = [0 as c_char; proc::PATH_MAX as usize];
    // SAFETY: a NUL-terminated path, and a buffer of `PATH_MAX` bytes, the most realpath
    // writes.
    let got = unsafe { proc::realpath(path.as_ptr().cast(), resolved.as_mut_ptr()) };
    if got.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: realpath wrote a NUL-terminated path into `resolved`, alive for this borrow.
    let bytes = unsafe { CStr::from_ptr(resolved.as_ptr()) }.to_bytes();
    let out = out.as_mut_os_string();
    out.clear();
    out.push(OsStr::from_bytes(bytes));
    Ok(())
}

/// Whether `name` is `dir` or under it, as [`FileName::is_under`]; every name is under the
/// root (`dir` empty).
fn is_under(name: &str, dir: &str) -> bool {
    dir.is_empty() || name == dir || name.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// A watched directory's name under the root, as [`Shared::name`] writes names: `""` for the
/// root itself.
fn dir_text(dir: &DirName) -> &str {
    dir.name().map_or("", FileName::as_str)
}

/// The UUID of the FSEvents store of the volume `path` is on: of its nearest ancestor that is
/// there, for a directory not made yet. A volume with no store (one fseventsd does not keep)
/// fails, naming the device.
fn store_of(fw: &Frameworks, path: &Path) -> io::Result<[u8; 16]> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = path.ancestors().find_map(|it| std::fs::metadata(it).ok()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "none of its directories is there")
    })?;
    // `st_dev` is an `int32_t` that `dev()` widened; narrowing gives it back.
    let device = meta.dev() as dev_t;
    // SAFETY: no pointers; the result is a CFUUID retained for the caller, or null.
    let uuid = unsafe { (fw.copy_uuid_for_device)(device) };
    if uuid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
            format!("the volume of device {device} keeps no FSEvents store, so no journal of it"),
        ));
    }
    // SAFETY: a CFUUID, alive until it is released below.
    let bytes = unsafe { (fw.uuid_get_uuid_bytes)(uuid) };
    // SAFETY: retained by the copy above for this function, released once, not used after.
    unsafe { (fw.release)(uuid) };
    Ok(bytes.bytes)
}

impl<S: Tell> Shared<S> {
    /// The watched directory's name under the root: `""` for the root itself.
    fn dir(&self) -> &str {
        dir_text(self.dir.as_ref().expect("a stream runs only once its directory is set"))
    }

    /// Where under the watched directory a path FSEvents gave is: `None` for one outside it,
    /// `Some(None)` for one under it that no [`FileName`] can name, else its name, written at
    /// the end of `text`. The watched directory itself is its own name, `""` for the root (a
    /// span no [`FileName`] is made of: [`journaled`] tells it as `itself` or `whole`).
    fn name(&self, path: &Path, text: &mut String) -> Option<Option<Span>> {
        let dir = self.dir();
        let start = text.len();
        let span = |text: &String| (start as u32, text.len() as u32);
        if let Ok(rest) = path.strip_prefix(&self.real_dir) {
            if rest.as_os_str().is_empty() {
                text.push_str(dir);
                return Some(Some(span(text)));
            }
            let Some(rest) = rest.to_str() else { return Some(None) };
            if !dir.is_empty() {
                text.push_str(dir);
                text.push('/');
            }
            text.push_str(rest);
            if !FileName::is_valid(&text[start..]) {
                text.truncate(start);
                return Some(None);
            }
            return Some(Some(span(text)));
        }
        let rest = path.strip_prefix(&self.root).ok()?;
        let Some(rest) = rest.to_str() else { return Some(None) };
        if rest.is_empty() && dir.is_empty() {
            return Some(Some(span(text)));
        }
        if !FileName::is_valid(rest) || !is_under(rest, dir) {
            return None;
        }
        text.push_str(rest);
        Some(Some(span(text)))
    }

    /// The cookie number of a path in this watch's cookies: `None` for one outside the cookie
    /// area, `Some(None)` for this watch's directory itself, a name in it that is no cookie's,
    /// or anything else in the area (another watch's or an earlier process's cookies, and the
    /// area's own folders).
    fn cookie(&self, path: &Path) -> Option<Option<u64>> {
        if let Ok(rest) = path.strip_prefix(&self.cookies) {
            return Some(rest.to_str().and_then(|it| it.parse::<u64>().ok()));
        }
        path.starts_with(&self.cookie_area).then_some(None)
    }

    /// Hands `completion` to the sink, waiting for room while the stream runs: a stream being
    /// stopped tells nothing more.
    fn send(&self, mut completion: Completion) {
        loop {
            match self.sink.tell(completion, ROOM) {
                Ok(()) | Err(None) => return,
                Err(Some(back)) if self.stopped.load(Ordering::Acquire) => {
                    drop(back);
                    return;
                }
                Err(Some(back)) => completion = back,
            }
        }
    }

    /// Runs `f`, which makes what a completion carries: counted apart from the callback's own
    /// allocations in the allocation-budget tests.
    #[inline]
    fn payload<R>(&self, f: impl FnOnce() -> R) -> R {
        #[cfg(test)]
        {
            let (result, used) = blaze_alloc_count::measure(f);
            self.payload.fetch_add(used.allocations, Ordering::Relaxed);
            result
        }
        #[cfg(not(test))]
        f()
    }
}

/// A watch of one directory at a time under a driver's root, telling `S` what it hears. Made
/// once ([`Watch::new`]) and started and stopped any number of times; a start after a stop
/// reuses everything but the stream.
pub(crate) struct Watch<S> {
    /// The frameworks every call below is made through, loaded before the watch was made.
    fw: &'static Frameworks,
    shared: Owned<S>,
    /// Serial: the stream's callbacks, and its stop, run on it one at a time.
    queue: DispatchQueue,
    /// `shared.cookies` as a CFString, for every stream's list of paths.
    cookie_string: CFRef,
    /// The running stream, null while none runs.
    stream: FSEventStreamRef,
    /// Where a path is made NUL-terminated for the system.
    c_path: Vec<u8>,
}

// SAFETY: the stream and the queue are FSEvents' and libdispatch's objects, which may be
// stopped and released from any thread; `Shared` is read on the queue while a stream runs and
// written only while none does (`Owned::get_mut`), and `S: Send + Sync`.
unsafe impl<S: Tell> Send for Watch<S> {}

/// A watch's [`Shared`], at an address of its own that the stream's callback reads by: made
/// in [`Watch::new`], freed when the watch drops, after its `Drop` has ended any stream.
struct Owned<S>(std::ptr::NonNull<Shared<S>>);

impl<S> Owned<S> {
    fn new(shared: Shared<S>) -> Owned<S> {
        // alloc: setup, once per watch: the callback's state at an address that never moves.
        Owned(std::ptr::NonNull::from(Box::leak(Box::new(shared))))
    }

    fn get(&self) -> &Shared<S> {
        // SAFETY: leaked by `new` and freed only by `drop`, so live while `self` is; every other
        // reference to it is shared (the callback's) or `get_mut`'s, made while none of those
        // is held. Read after the free, it would be freed memory.
        unsafe { self.0.as_ref() }
    }

    /// # Safety
    ///
    /// No stream of the watch runs (none made, or ended by `stop`), so no callback holds a
    /// reference to it; a callback reading it meanwhile would race this `&mut`.
    unsafe fn get_mut(&mut self) -> &mut Shared<S> {
        // SAFETY: live as in `get`; the caller's contract makes this the one reference, and
        // `&mut self` excludes `get`'s.
        unsafe { self.0.as_mut() }
    }

    /// The address the stream's callback is given.
    fn as_ptr(&self) -> *mut Shared<S> {
        self.0.as_ptr()
    }
}

impl<S> Drop for Owned<S> {
    fn drop(&mut self) {
        // SAFETY: leaked by `new` from a `Box`, freed once, here. The watch's `Drop`, which runs
        // before its fields drop, ended its stream, so no callback reads it after.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

impl<S: Tell> Watch<S> {
    /// A watch of folders under the driver's `root` telling `sink`, with its directory of
    /// cookies (under `root`: [`cookies`]) and its queue made: none of them is made again for
    /// any stream it runs.
    pub(crate) fn new(sink: S, root: &Path) -> io::Result<Watch<S>> {
        let fw = Frameworks::get()?;
        let (cookie_area, cookies) = cookies(root).map_err(|error| {
            // alloc: an error path.
            io::Error::new(error.kind(), format!("making the cookies of a watch under {}: {error}", root.display()))
        })?;
        // alloc: setup, once per watch: room for the paths it names to the system.
        let mut c_path = Vec::with_capacity(256);
        c_string(&cookies, &mut c_path)?;
        // SAFETY: a NUL-terminated path; the string is released in `Drop`.
        let cookie_string = unsafe {
            (fw.string_create_with_file_system_representation)(std::ptr::null(), c_path.as_ptr().cast())
        };
        if cookie_string.is_null() {
            _ = std::fs::remove_dir_all(&cookies);
            return Err(io::Error::other("CoreFoundation made no string of the cookies' path"));
        }
        // SAFETY: a NUL-terminated label; no attributes is a serial queue.
        let queue = unsafe { dispatch_queue_create(c"blaze-fsevents".as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            // SAFETY: made above, released once, not used after.
            unsafe { (fw.release)(cookie_string) };
            _ = std::fs::remove_dir_all(&cookies);
            return Err(io::Error::other("libdispatch made no queue"));
        }
        let shared = Shared {
            tag: Tag(0),
            dir: None,
            // alloc: setup, once per watch: the resolved paths, at the size most have.
            root: PathBuf::with_capacity(256),
            real_dir: PathBuf::with_capacity(256), // alloc: as above.
            writing: Lock::new("core.fsevents.writing", cookie_buffer(&cookies)),
            // alloc: setup, once per watch: the callback's buffers at the size a batch of a
            // few names and cookies needs.
            heard: Lock::new("core.fsevents.heard", Heard {
                cookies: Vec::with_capacity(4),
                told: Vec::with_capacity(4),
                path: cookie_buffer(&cookies),
                text: String::with_capacity(1024),
                names: Vec::with_capacity(32),
                lost: Vec::with_capacity(4),
            }),
            cookies,
            cookie_area,
            flushes: Lock::new("core.fsevents.flushes", Vec::with_capacity(4)), // alloc: setup, once per watch.
            sink,
            stopped: AtomicBool::new(true),
            mode: Mode::Watch,
            #[cfg(test)]
            overhead: AtomicU64::new(0),
            #[cfg(test)]
            payload: AtomicU64::new(0),
            #[cfg(test)]
            trace: Lock::new("core.fsevents.trace", Vec::new()),
        };
        Ok(Watch {
            fw,
            shared: Owned::new(shared),
            queue,
            cookie_string,
            stream: std::ptr::null_mut(),
            c_path,
        })
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.stream.is_null()
    }

    /// Watches `dir` under `root` (kFSEventStreamCreateFlagFileEvents | NoDefer | WatchRoot,
    /// latency 0, on the watch's queue), telling a `Completion { tag, done: Done::Changed {
    /// files, lost } }` for each batch, names relative to `root` as `FileName`s, each once;
    /// kFSEventStreamEventFlagMustScanSubDirs, UserDropped or KernelDropped make `lost: true`.
    pub(crate) fn start(&mut self, root: &Path, dir: DirName, tag: Tag) -> io::Result<()> {
        self.locate(root, &dir)?;
        self.open(dir, tag, SINCE_NOW, Mode::Watch, || {})
    }

    /// The journal of `dir` under `root` from `since` ([`crate::Effect::Journal`]): a stream as
    /// [`Watch::start`]'s, telling `Done::Journaled` batches. From a cursor of this volume's
    /// store at or before the system's current event ID it starts `sinceWhen` that ID, its
    /// history (the changes after that ID alone: [`Journal`]'s rule) told with
    /// `caught_up: false` until HistoryDone; from anything else it starts at
    /// the current ID, read before the stream is made so no event falls between, with a first
    /// batch handed to `first` on the caller's thread just before the stream starts, for the
    /// caller to tell before anything the stream tells: empty, at that ID, and `dir` lost whole
    /// as [`Lost::Reset`] unless `since` was [`Cursor::None`].
    ///
    /// `dir` may be the root itself ([`DirName::Root`]: a checkout run from its own folder).
    /// What that relies on (FSEvents.h and the stream's flags): a stream on a path reports
    /// events for that path's descendants at every depth, and with `FileEvents` each made,
    /// removed or renamed entry is an event of its own path, the root's direct entries too (a
    /// rename is an event at its source and one at its destination, each in its own folder's
    /// path); the watched path's own changes (its metadata, with `ItemIsDir`) are an event of
    /// that path, told as `itself`; `WatchRoot` makes a move or removal of the path itself
    /// `RootChanged`, told as `whole`. So the root needs no unit beneath it: every change under
    /// it is a name of this one stream, or a loss of it.
    pub(crate) fn journal(
        &mut self,
        root: &Path,
        dir: DirName,
        since: Cursor,
        tag: Tag,
        first: impl FnOnce(Journaled),
    ) -> io::Result<()> {
        self.locate(root, &dir)?;
        let volume = store_of(self.fw, &self.shared.get().real_dir).map_err(|error| {
            let at = root.join(dir_text(&dir)); // alloc: an error path.
            // alloc: an error path.
            io::Error::new(error.kind(), format!("the journal of {}: {error}", at.display()))
        })?;
        // SAFETY: no arguments; reads the system's last event ID.
        let now = unsafe { (self.fw.get_current_event_id)() };
        let resumed = match since {
            Cursor::FsEvents { event, volume: of } if of == volume && event <= now => Some(event),
            _ => None,
        };
        let whole = match (resumed, since) {
            (Some(_), _) => None,
            (None, Cursor::None) => Some(None),
            (None, _) => Some(Some(Lost::Reset)),
        };
        let batch = whole.map(|whole| Journaled {
            files: Vec::new(), // alloc: none, an empty vector.
            lost: Vec::new(), // alloc: none, an empty vector.
            whole,
            itself: false,
            cursor: Cursor::FsEvents { event: now, volume },
            caught_up: false,
        });
        let from = resumed.unwrap_or(now);
        let journal =
            Journal { volume, from, last: AtomicU64::new(from), caught_up: AtomicBool::new(false) };
        let before = || {
            if let Some(batch) = batch {
                first(batch);
            }
        };
        self.open(dir, tag, from, Mode::Journal(journal), before)
    }

    /// `root` and the watched directory under it resolved into the shared paths, as FSEvents
    /// names them; a directory that is not there (yet) is watched as the root names it.
    fn locate(&mut self, root: &Path, dir: &DirName) -> io::Result<()> {
        assert!(!self.is_running(), "a watch runs one stream at a time");
        // SAFETY: no stream runs, asserted above.
        let Shared { root: real_root, real_dir, .. } = unsafe { self.shared.get_mut() };
        let resolved =
            c_string(root, &mut self.c_path).and_then(|()| realpath(&self.c_path, real_root));
        resolved.map_err(|error| {
            // alloc: an error path.
            io::Error::new(error.kind(), format!("watching under {}: {error}", root.display()))
        })?;
        real_dir.as_mut_os_string().clear();
        real_dir.push(&*real_root);
        // The root is `real_root` itself: `push("")` would end it in a `/`, a path FSEvents
        // names no event by.
        if let DirName::Named(dir) = dir {
            real_dir.push(dir.as_str());
        }
        c_string(real_dir, &mut self.c_path)?;
        _ = realpath(&self.c_path, real_dir);
        Ok(())
    }

    /// The stream of `shared.real_dir` and the cookies from `since`, `before` run once it is
    /// made, just before it starts.
    fn open(
        &mut self,
        dir: DirName,
        tag: Tag,
        since: u64,
        mode: Mode,
        before: impl FnOnce(),
    ) -> io::Result<()> {
        let failed = |shared: &Shared<S>, why: &str| {
            // alloc: an error path.
            io::Error::other(format!("watching {}: {why}", shared.real_dir.display()))
        };
        assert!(!self.is_running(), "a watch runs one stream at a time");
        // A journal's history whole (`CREATE_FULL_HISTORY`'s doc); a live watch has none.
        let history = match mode {
            Mode::Journal(_) => CREATE_FULL_HISTORY,
            Mode::Watch => 0,
        };
        {
            // SAFETY: no stream runs, asserted above.
            let shared = unsafe { self.shared.get_mut() };
            shared.tag = tag;
            shared.dir = Some(dir);
            shared.mode = mode;
        }
        c_string(&self.shared.get().real_dir, &mut self.c_path)?;
        // SAFETY: a NUL-terminated path; the string is released below.
        let fw = self.fw;
        let dir_string = unsafe {
            (fw.string_create_with_file_system_representation)(
                std::ptr::null(),
                self.c_path.as_ptr().cast(),
            )
        };
        if dir_string.is_null() {
            return Err(failed(self.shared.get(), "CoreFoundation made no string of the path"));
        }
        let strings = [dir_string, self.cookie_string];
        // The cookies under the watched folder (the root's, a checkout run from its own folder)
        // are heard through its path: one path, so one volume's history and no event twice.
        let count: CFIndex = match self.shared.get().cookies.starts_with(&self.shared.get().real_dir) {
            true => 1,
            false => 2,
        };
        // SAFETY: `count` CFStrings of `strings`, retained by the array with the callbacks of
        // CF types.
        let paths = unsafe {
            (fw.array_create)(std::ptr::null(), strings.as_ptr(), count, fw.type_array_callbacks)
        };
        // SAFETY: made above, released once; the array holds its own reference.
        unsafe { (fw.release)(dir_string) };
        if paths.is_null() {
            return Err(failed(self.shared.get(), "CoreFoundation made no array of the path"));
        }
        self.shared.get().stopped.store(false, Ordering::Release);
        let context = Context {
            version: 0,
            info: self.shared.as_ptr().cast(),
            retain: None,
            release: None,
            describe: None,
        };
        // SAFETY: the context is copied by the call; the `Shared` it names is `Owned`'s, at an
        // address of its own, freed only after `Drop` or `stop` has run the stream's last
        // callback, however the watch moves.
        let stream = unsafe {
            (fw.stream_create)(
                std::ptr::null(),
                heard::<S>,
                &context,
                paths,
                since,
                0.0,
                CREATE_FILE_EVENTS | CREATE_NO_DEFER | CREATE_WATCH_ROOT | history,
            )
        };
        // SAFETY: the stream holds its own reference to the paths.
        unsafe { (fw.release)(paths) };
        if stream.is_null() {
            self.shared.get().stopped.store(true, Ordering::Release);
            return Err(failed(self.shared.get(), "FSEventStreamCreate made no stream"));
        }
        // SAFETY: a stream not yet started and the watch's queue, which outlives it.
        unsafe { (fw.stream_set_dispatch_queue)(stream, self.queue) };
        before();
        // SAFETY: the stream is scheduled on the queue.
        if unsafe { (fw.stream_start)(stream) } == 0 {
            // SAFETY: the stream did not start, so no callback runs during or after this.
            unsafe { (fw.stream_invalidate)(stream) };
            // SAFETY: the stream made above, released once and not used after.
            unsafe { (fw.stream_release)(stream) };
            self.shared.get().stopped.store(true, Ordering::Release);
            return Err(failed(self.shared.get(), "FSEventStreamStart refused it"));
        }
        self.stream = stream;
        Ok(())
    }

    /// Stops and releases the stream; nothing is told once this returns, and each flush not
    /// yet told is handed to `untold` (its cookie removed), for the caller to fail. The caller
    /// tells the watch's `Done::Unwatched` after. Nothing for a watch not running.
    pub(crate) fn stop(&mut self, mut untold: impl FnMut(Tag)) {
        if self.stream.is_null() {
            return;
        }
        self.shared.get().stopped.store(true, Ordering::Release);
        let mut ending = Ending { fw: self.fw, stream: self.stream };
        // SAFETY: the stream is stopped and invalidated on its own queue, so no callback runs
        // during it; `ending` outlives the call, which returns once the work has run.
        unsafe { dispatch_sync_f(self.queue, (&raw mut ending).cast(), end_on_queue) };
        // SAFETY: a serial queue: this runs after any callback queued before the invalidation.
        unsafe { dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing) };
        // SAFETY: invalidated, with no callback left to run; released once, not used after.
        unsafe { (self.fw.stream_release)(self.stream) };
        self.stream = std::ptr::null_mut();
        let mut writing = lock(&self.shared.get().writing);
        for tag in lock(&self.shared.get().flushes).drain(..) {
            _ = with_cookie(&mut writing, tag, |it| std::fs::remove_file(it));
            untold(tag);
        }
    }

    /// Asks for a flush `tag`: its cookie is written, and once the stream hears of it
    /// `Completion { tag, done: Done::Flushed }` is told, after every batch of changes made
    /// before (for a journal, after its history too). An error says the cookie could not be
    /// written, or no stream runs, and nothing is told.
    pub(crate) fn flush(&self, tag: Tag) -> io::Result<()> {
        if !self.is_running() {
            return Err(io::Error::other("the watch is not running"));
        }
        let shared = self.shared.get();
        lock(&shared.flushes).push(tag);
        let written = with_cookie(&mut lock(&shared.writing), tag, |it| std::fs::write(it, b""));
        match written {
            Ok(()) => Ok(()),
            Err(error) => {
                // A part made may have been heard, and the flush told already.
                let mut flushes = lock(&shared.flushes);
                match flushes.iter().position(|it| *it == tag) {
                    Some(at) => {
                        flushes.swap_remove(at);
                        Err(error)
                    }
                    None => Ok(()),
                }
            }
        }
    }

    /// What the callbacks allocated beyond what their completions carry, since last asked.
    #[cfg(test)]
    pub(crate) fn take_overhead(&self) -> u64 {
        self.shared.get().overhead.swap(0, Ordering::Relaxed)
    }

    /// Every event its journal streams heard since last asked, with what became of each.
    #[cfg(test)]
    pub(crate) fn take_trace(&self) -> Vec<Traced> {
        std::mem::take(&mut *lock(&self.shared.get().trace))
    }
}

impl<S> Drop for Watch<S> {
    fn drop(&mut self) {
        if !self.stream.is_null() {
            self.shared.get().stopped.store(true, Ordering::Release);
            let mut ending = Ending { fw: self.fw, stream: self.stream };
            // SAFETY: as in `stop`.
            unsafe { dispatch_sync_f(self.queue, (&raw mut ending).cast(), end_on_queue) };
            // SAFETY: as in `stop`.
            unsafe { dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing) };
            // SAFETY: as in `stop`.
            unsafe { (self.fw.stream_release)(self.stream) };
        }
        // SAFETY: no stream uses the queue any more; released once, not used after.
        unsafe { dispatch_release(self.queue) };
        // SAFETY: made in `new`, released once, not used after; every array that held it is
        // released with its stream.
        unsafe { (self.fw.release)(self.cookie_string) };
        _ = std::fs::remove_dir_all(&self.shared.get().cookies);
    }
}

/// What [`end_on_queue`] is handed: the stream to end, and the frameworks to end it with.
struct Ending {
    fw: &'static Frameworks,
    stream: FSEventStreamRef,
}

extern "C" fn end_on_queue(ending: *mut c_void) {
    // SAFETY: `stop` and `Drop` pass a pointer to their `Ending`, alive for this call.
    let Ending { fw, stream } = unsafe { &*ending.cast::<Ending>() };
    // SAFETY: a stream started and not yet released, stopped on its own queue.
    unsafe { (fw.stream_stop)(*stream) };
    // SAFETY: as above; no callback is scheduled once this returns.
    unsafe { (fw.stream_invalidate)(*stream) };
}

extern "C" fn nothing(_: *mut c_void) {}

/// A path FSEvents passed.
///
/// # Safety
///
/// `path` is a NUL-terminated string alive for `'a`.
unsafe fn path_of<'a>(path: *const c_char) -> &'a Path {
    // SAFETY: the caller's.
    let path = unsafe { CStr::from_ptr(path) };
    Path::new(OsStr::from_bytes(path.to_bytes()))
}

extern "C" fn heard<S: Tell>(
    _stream: CFRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    // SAFETY: `info` is the `Owned` `Shared` of the `Watch` that made the stream, which neither
    // moves nor is freed until the queue has run its last callback (`stop`, `Drop`), and is
    // not written meanwhile (`Owned::get_mut` needs no stream); if freed, this reads freed memory.
    let shared = unsafe { &*info.cast::<Shared<S>>() };
    // An empty batch tells nothing, and its arrays need not be valid pointers, which
    // `from_raw_parts` requires even of an empty slice.
    if shared.stopped.load(Ordering::Acquire) || count == 0 {
        return;
    }
    // SAFETY: FSEvents passes `count` aligned `FSEventStreamEventFlags` (`u32`), alive and
    // unchanged for the callback; a shorter array would be read past.
    let flags = unsafe { std::slice::from_raw_parts(flags, count) };
    // SAFETY: as above, `count` C string pointers: the stream is made without
    // kFSEventStreamCreateFlagUseCFTypes (`start`), so `paths` is a `char **`, not a CFArray.
    let paths = unsafe { std::slice::from_raw_parts(paths.cast::<*const c_char>(), count) };
    // SAFETY: as above, `count` `FSEventStreamEventId`s (`u64`).
    let ids = unsafe { std::slice::from_raw_parts(ids, count) };
    #[cfg(test)]
    {
        let ((), used) = blaze_alloc_count::measure(|| hear(shared, flags, paths, ids));
        let payload = shared.payload.swap(0, Ordering::Relaxed);
        shared.overhead.fetch_add(used.allocations.saturating_sub(payload), Ordering::Relaxed);
    }
    #[cfg(not(test))]
    hear(shared, flags, paths, ids);
}

fn hear<S: Tell>(shared: &Shared<S>, flags: &[u32], paths: &[*const c_char], ids: &[u64]) {
    let mut heard = lock(&shared.heard);
    heard.cookies.clear();
    match &shared.mode {
        Mode::Watch => changed(shared, flags, paths, &mut heard),
        Mode::Journal(journal) => journaled(shared, journal, flags, paths, ids, &mut heard),
    }
    if !heard.cookies.is_empty() {
        tell_flushes(shared, &mut heard);
    }
}

/// The names in `heard.names` sorted and each once.
fn sort_names(heard: &mut Heard) {
    let Heard { text, names, .. } = heard;
    let text: &str = text;
    let at = |span: &Span| &text[span.0 as usize..span.1 as usize];
    names.sort_unstable_by(|a, b| at(a).cmp(at(b)));
    names.dedup_by(|a, b| at(a) == at(b));
}

/// The [`FileName`]s of `heard.names`, in their order; whether one could not be made.
fn make_names(heard: &Heard) -> (Vec<FileName>, bool) {
    let mut files = Vec::with_capacity(heard.names.len());
    let mut bad = false;
    for &span in &heard.names {
        match FileName::new(heard.str(span)) {
            Some(name) => files.push(name),
            None => bad = true,
        }
    }
    (files, bad)
}

/// A watch's batch, as [`Done::Changed`].
fn changed<S: Tell>(shared: &Shared<S>, flags: &[u32], paths: &[*const c_char], heard: &mut Heard) {
    heard.text.clear();
    heard.names.clear();
    let mut lost = false;
    for (&flag, &path) in flags.iter().zip(paths) {
        lost |= flag & LOSES != 0;
        // SAFETY: a path FSEvents passed, alive for the callback.
        let path = unsafe { path_of(path) };
        if let Some(cookie) = shared.cookie(path) {
            heard.cookies.extend(cookie);
            continue;
        }
        match shared.name(path, &mut heard.text) {
            Some(Some(span)) => heard.names.push(span),
            Some(None) => lost = true,
            None => {}
        }
    }
    if heard.names.is_empty() && !lost {
        return;
    }
    sort_names(heard);
    let (files, bad) = shared.payload(|| make_names(heard));
    let done = Done::Changed { files, lost: lost || bad };
    shared.send(Completion { tag: shared.tag, done });
}

/// `name` lost for `why`, once.
fn lose(heard: &mut Heard, name: Span, why: Lost) {
    let seen =
        heard.lost.iter().any(|&(it, was, _)| was == why && heard.str(it) == heard.str(name));
    if !seen {
        let order = heard.lost.len() as u32;
        heard.lost.push((name, why, order));
    }
}

/// Whether a name or rescan event of ID `id`, heard by a journal started from the place `from`,
/// is told: only one after the place ([`Journal`]'s rule, part 2). FSEvents.h, of `eventIds`:
/// "Each event ID comes from the most recent event being reported in the corresponding
/// directory", so an event whose ID is at or before the place reports no change after it. ID 0
/// is no event's place (FSEvents.h: RootChanged's "event ID is zero"): told.
#[inline]
fn after_place(from: u64, id: u64) -> bool {
    id == 0 || id > from
}

/// A journal's batch, as [`Done::Journaled`]: told when it names anything, moves the cursor or
/// ends the history.
fn journaled<S: Tell>(
    shared: &Shared<S>,
    journal: &Journal,
    flags: &[u32],
    paths: &[*const c_char],
    ids: &[u64],
    heard: &mut Heard,
) {
    heard.text.clear();
    heard.names.clear();
    heard.lost.clear();
    heard.text.push_str(shared.dir());
    let dir: Span = (0, heard.text.len() as u32);
    let before = journal.last.load(Ordering::Relaxed);
    let was_caught_up = journal.caught_up.load(Ordering::Relaxed);
    let (mut last, mut caught_up) = (before, was_caught_up);
    for ((&flag, &path), &id) in flags.iter().zip(paths).zip(ids) {
        #[cfg(test)]
        trace(shared, journal, flag, path, id, !caught_up);
        // Its path is to be ignored, and its ID is no event's of the watched paths.
        if flag & EVENT_HISTORY_DONE != 0 {
            caught_up = true;
            continue;
        }
        if flag & EVENT_IDS_WRAPPED != 0 {
            last = id;
            lose(heard, dir, Lost::Reset);
            continue;
        }
        // RootChanged carries ID 0.
        last = last.max(id);
        if flag & (EVENT_USER_DROPPED | EVENT_KERNEL_DROPPED) != 0 {
            lose(heard, dir, Lost::Dropped);
            continue;
        }
        // SAFETY: a path FSEvents passed, alive for the callback.
        let path = unsafe { path_of(path) };
        if let Some(cookie) = shared.cookie(path) {
            // A history may hold the cookies of an earlier stream that had this directory;
            // a flush is told only by one written once the history is told.
            if caught_up {
                heard.cookies.extend(cookie);
            }
            continue;
        }
        if flag & (EVENT_ROOT_CHANGED | EVENT_MOUNT | EVENT_UNMOUNT) != 0 {
            lose(heard, dir, Lost::RootChanged);
            continue;
        }
        // FullHistory's chunk holds events from before the place: a change the place covers.
        // Losses above are kept whatever their ID: one told again is a rescan, never a miss.
        if !after_place(journal.from, id) {
            continue;
        }
        let name = shared.name(path, &mut heard.text);
        if flag & EVENT_MUST_SCAN_SUB_DIRS != 0 {
            let name = match name {
                Some(Some(name)) => name,
                Some(None) => dir,
                None if shared.real_dir.starts_with(path) => dir,
                None => continue,
            };
            lose(heard, name, Lost::Rescan);
            continue;
        }
        match name {
            Some(Some(name)) => heard.names.push(name),
            Some(None) => lose(heard, dir, Lost::Rescan),
            None => {}
        }
    }
    let unchanged = last == before && caught_up == was_caught_up;
    if heard.names.is_empty() && heard.lost.is_empty() && unchanged {
        return;
    }
    sort_names(heard);
    // The watched directory's own name (the root's is `""`) is `itself`, never a file: one
    // rule for a named directory and the root.
    let itself = {
        let Heard { text, names, .. } = &mut *heard;
        let at = |span: &Span| &text[span.0 as usize..span.1 as usize];
        let own = &text[dir.0 as usize..dir.1 as usize];
        let before = names.len();
        names.retain(|span| at(span) != own);
        names.len() != before
    };
    let (files, bad) = shared.payload(|| make_names(heard));
    {
        let Heard { text, lost, .. } = &mut *heard;
        let text: &str = text;
        let at = |span: Span| &text[span.0 as usize..span.1 as usize];
        lost.sort_unstable_by(|a, b| at(a.0).cmp(at(b.0)).then(a.2.cmp(&b.2)));
    }
    // The watched directory lost (its own name, or a name that was no `FileName` after all):
    // `whole`, the first heard; a name `make_names` could not make: a rescan of all of it.
    let mut whole = None;
    let lost = shared.payload(|| {
        let mut lost = Vec::with_capacity(heard.lost.len());
        let own = heard.str(dir);
        for &(span, why, _) in &heard.lost {
            match FileName::new(heard.str(span)) {
                Some(name) if heard.str(span) != own => lost.push((name, why)),
                _ => whole = whole.or(Some(why)),
            }
        }
        lost
    });
    if bad {
        whole = whole.or(Some(Lost::Rescan));
    }
    journal.last.store(last, Ordering::Relaxed);
    journal.caught_up.store(caught_up, Ordering::Relaxed);
    let cursor = Cursor::FsEvents { event: last, volume: journal.volume };
    let journaled = Journaled { files, lost, whole, itself, cursor, caught_up };
    // alloc: the batch's payload, boxed as `Done` carries it.
    let done = shared.payload(|| Done::Journaled(Box::new(journaled)));
    shared.send(Completion { tag: shared.tag, done });
}

/// Tells each flush whose cookie was heard, through the callback's buffers.
fn tell_flushes<S: Tell>(shared: &Shared<S>, heard: &mut Heard) {
    let Heard { cookies, told, path, .. } = heard;
    told.clear();
    lock(&shared.flushes).retain(|tag| {
        let heard = cookies.contains(&tag.0);
        if heard {
            told.push(*tag);
        }
        !heard
    });
    for &tag in told.iter() {
        shared.send(Completion { tag, done: Done::Flushed });
        // Its removal is heard too, of a flush told already: nothing. Done after `send` so the
        // unlink syscall stays off the waiter's critical path.
        _ = with_cookie(path, tag, |it| std::fs::remove_file(it));
    }
}

/// Gives fseventsd `long` to put what was written into its store, for a test that reads it
/// back from a cursor: it writes its store on its own schedule, and nothing says when an event
/// is in it.
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "fseventsd's store has no event that says what it holds"
)]
pub(crate) fn fseventsd_keeps(long: Duration) {
    std::thread::sleep(long);
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        time::{Duration, Instant},
    };

    use super::*;

    /// The sweep of dead owners' cookie folders against the real kernel: this process's folder
    /// (its pid and its start) is kept; the same pid named with another start (a reused pid's
    /// earlier owner) is swept; an older form's folder of this live pid is kept; names not of
    /// the form are left. The control is the same names before the sweep: all there.
    #[test]
    fn a_dead_owners_cookie_folder_is_swept_and_a_live_ones_kept() {
        let root = root("sweep");
        let area = root.join(COOKIE_AREA);
        let pid = std::process::id();
        let Started::At(start) = started(pid) else { panic!("this process has no start") };
        let names = [
            (format!("{pid}-{start}-0"), true),
            (format!("{pid}-{}-0", start.wrapping_add(1)), false),
            (format!("{pid}-7"), true),
            ("not-a-cookie-folder".to_owned(), true),
            ("12-ab-3".to_owned(), true),
        ];
        for (name, _) in &names {
            std::fs::create_dir_all(area.join(name)).unwrap();
        }
        for (name, _) in &names {
            assert!(area.join(name).exists(), "the control: {name} made");
        }
        sweep_dead(&area, started);
        for (name, kept) in &names {
            assert_eq!(area.join(name).exists(), *kept, "{name}: kept {kept}");
        }
    }

    /// [`owner_is_dead`]'s verdicts by the kernel's answer, the oracle written as the rule:
    /// dead only for no process, or another start at a named start; kept for a live match,
    /// an unknown answer, and an older form's live pid.
    #[test]
    fn an_owner_is_dead_only_by_a_fact_that_cannot_change_back() {
        let table = |answer: Started| move |_: u32| answer;
        for (name, answer, dead) in [
            ("5-100-0", Started::At(100), false),
            ("5-100-0", Started::At(101), true),
            ("5-100-0", Started::NoProcess, true),
            ("5-100-0", Started::Unknown(1), false),
            ("5-0", Started::At(9), false),
            ("5-0", Started::NoProcess, true),
            ("5-0", Started::Unknown(1), false),
            ("x-100-0", Started::NoProcess, false),
            ("5-100-0-1", Started::NoProcess, false),
            ("5", Started::NoProcess, false),
        ] {
            assert_eq!(owner_is_dead(name, table(answer)), dead, "{name} with {answer:?}");
        }
    }

    /// A root under the temporary directory, which is `/var/...` named and `/private/var/...`
    /// real, with `src` and `target` made in it.
    /// The guard removes it when the test ends.
    fn root(what: &str) -> crate::temp::TempDir {
        let root = crate::temp::TempDir::for_test(&format!("blaze-fsevents-{what}-"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        root
    }

    /// A bounded channel, as the driver's: no allocation per completion.
    type Sink = crate::chan::Sender<Completion>;

    type Heard = crate::chan::Receiver<Completion>;

    fn watch(root: &Path) -> (Watch<Sink>, Heard) {
        let (send, heard) = crate::chan::bounded(1024);
        (Watch::new(send, root).unwrap(), heard)
    }

    /// Every symbol the driver finds at first use is in this OS's frameworks (rule 45): a
    /// framework or symbol gone from a later macOS fails here, naming it, not in a user's watch.
    #[test]
    fn every_symbol_is_found_on_this_os() {
        if let Err(error) = Frameworks::get() {
            panic!("{error}");
        }
    }

    /// A missing framework or symbol is an error naming both, not a crash at the call.
    #[test]
    fn a_missing_symbol_is_named() {
        let fw = open(CORE_FOUNDATION).unwrap_or_else(|missing| panic!("{}", missing.to_error()));
        // SAFETY: never called: the symbol is not there.
        let found = unsafe { symbol::<unsafe extern "C" fn()>(fw, CORE_FOUNDATION, c"CFNoSuchFunction") };
        let error = found.map(|_| ()).unwrap_err().to_error().to_string();
        assert!(error.contains("CFNoSuchFunction") && error.contains("CoreFoundation"), "{error}");
    }

    fn journaled(completion: Completion, tag: Tag) -> Journaled {
        assert_eq!(completion.tag, tag);
        match completion.done {
            Done::Journaled(it) => *it,
            other => panic!("{other:?}"),
        }
    }

    /// Writes `file` under `root` until `watch` tells it, or the deadline: what it told.
    fn hear_written(root: &Path, file: &str, heard: &Heard, tag: Tag) -> BTreeSet<FileName> {
        let mut files = BTreeSet::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        let wanted = FileName::new(file).unwrap();
        while !files.contains(&wanted) && Instant::now() < deadline {
            std::fs::write(root.join("target/out"), b"not watched").unwrap();
            std::fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
            std::fs::write(root.join(file), b"fn f() {}").unwrap();
            while let Ok(completion) = heard.recv_timeout(Duration::from_millis(100)) {
                assert_eq!(completion.tag, tag);
                let Done::Changed { files: batch, .. } = completion.done else {
                    panic!("{:?}", completion.done)
                };
                let unique: BTreeSet<_> = batch.iter().cloned().collect();
                assert_eq!(unique.len(), batch.len(), "a name twice in one batch: {batch:?}");
                let mut sorted = batch.clone();
                sorted.sort();
                assert_eq!(sorted, batch, "a batch's names sorted");
                files.extend(batch);
            }
        }
        assert!(files.contains(&wanted), "{files:?}");
        files
    }

    #[test]
    fn a_file_written_under_the_watched_directory_is_heard_and_one_outside_is_not() {
        let root = root("watch");
        let (mut watch, heard) = watch(&root);
        let dir = FileName::new("src").unwrap();
        watch.start(&root, DirName::Named(dir.clone()), Tag(4)).unwrap();
        let files = hear_written(&root, "src/deep/lib.rs", &heard, Tag(4));
        watch.stop(|tag| panic!("no flush was asked: {tag:?}"));
        assert!(files.iter().all(|it| it.is_under(&dir)), "{files:?}");
        // Nothing is told once it has stopped.
        std::fs::write(root.join("src/after"), b"late").unwrap();
        while let Ok(completion) = heard.recv_timeout(Duration::from_millis(200)) {
            let Done::Changed { files, .. } = completion.done else { panic!() };
            assert!(!files.contains(&FileName::new("src/after").unwrap()), "{files:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A watch stopped and started again, on another directory with another tag, hears that
    /// one only: everything kept across its streams is the new stream's.
    #[test]
    fn a_watch_started_again_hears_its_new_directory() {
        let root = root("again");
        std::fs::create_dir_all(root.join("other")).unwrap();
        let (mut watch, heard) = watch(&root);
        watch.start(&root, DirName::Named(FileName::new("src").unwrap()), Tag(1)).unwrap();
        hear_written(&root, "src/a.rs", &heard, Tag(1));
        watch.stop(|_| {});
        while heard.try_recv().is_ok() {}
        let other = FileName::new("other").unwrap();
        watch.start(&root, DirName::Named(other.clone()), Tag(2)).unwrap();
        let files = hear_written(&root, "other/b.rs", &heard, Tag(2));
        assert!(files.iter().all(|it| it.is_under(&other)), "{files:?}");
        watch.flush(Tag(3)).unwrap();
        watch.stop(|tag| assert_eq!(tag, Tag(3)));
        assert!(watch.flush(Tag(4)).is_err(), "a flush of a stopped watch");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The callback reads the watch's `Shared` at an address of its own, so a running watch
    /// may move (into a vector that then grows) and goes on hearing, then stops and drops.
    #[test]
    fn a_running_watch_moved_goes_on_hearing() {
        let root = root("moved");
        let (mut watch, heard) = watch(&root);
        watch.start(&root, DirName::Named(FileName::new("src").unwrap()), Tag(7)).unwrap();
        hear_written(&root, "src/before.rs", &heard, Tag(7));
        let mut moved = vec![watch];
        moved.reserve(64);
        hear_written(&root, "src/after.rs", &heard, Tag(7));
        let mut watch = moved.pop().unwrap();
        watch.stop(|_| {});
        drop(watch);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// After a first stream has grown its buffers, a second stream's batches allocate nothing
    /// beyond the names and the `Vec` they carry, measured on the stream's queue.
    #[test]
    fn a_warm_watch_batch_allocates_only_its_payload() {
        let root = root("budget");
        let (mut watch, heard) = watch(&root);
        watch.start(&root, DirName::Named(FileName::new("src").unwrap()), Tag(1)).unwrap();
        hear_written(&root, "src/warm.rs", &heard, Tag(1));
        watch.stop(|_| {});
        watch.take_overhead();
        watch.start(&root, DirName::Named(FileName::new("src").unwrap()), Tag(1)).unwrap();
        hear_written(&root, "src/warm.rs", &heard, Tag(1));
        watch.stop(|_| {});
        assert_eq!(watch.take_overhead(), 0, "a warm batch allocates beyond its payload");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_name_is_under_a_dir_as_file_name_is_under_says() {
        let dir = FileName::new("src").unwrap();
        for name in ["src", "src/a", "srca", "sr", "src/a/b", "other/src"] {
            let it = FileName::new(name).unwrap();
            assert_eq!(is_under(name, dir.as_str()), it.is_under(&dir), "{name:?}");
        }
    }

    /// Takes batches until one is caught up, or the deadline: every one of them.
    fn until_caught_up(heard: &Heard, tag: Tag) -> Vec<Journaled> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut batches = Vec::new();
        while Instant::now() < deadline {
            let Ok(completion) = heard.recv_timeout(Duration::from_millis(100)) else { continue };
            let batch = journaled(completion, tag);
            let caught_up = batch.caught_up;
            batches.push(batch);
            if caught_up {
                return batches;
            }
        }
        panic!("never caught up: {batches:?}")
    }

    /// [`Watch::journal`], with the first batch it hands over.
    fn journal(
        watch: &mut Watch<Sink>,
        root: &Path,
        dir: &FileName,
        since: Cursor,
        tag: Tag,
    ) -> Option<Journaled> {
        let mut first = None;
        watch.journal(root, DirName::Named(dir.clone()), since, tag, |it| first = Some(it)).unwrap();
        first
    }

    /// Asks the flush `flush` of the running journal told under `tag`, then takes its batches
    /// until the flush is told, or the deadline: the names told before it, the cursor of the
    /// last of those batches (`last` when none came): the place to save, and each of those
    /// batches' cursor IDs, in order (how the place was reached).
    fn flushed(
        watch: &Watch<Sink>,
        heard: &Heard,
        tag: Tag,
        flush: Tag,
        mut last: Cursor,
    ) -> (BTreeSet<FileName>, Cursor, Vec<u64>) {
        watch.flush(flush).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut told = BTreeSet::new();
        let mut cursors = Vec::new();
        while Instant::now() < deadline {
            let Ok(completion) = heard.recv_timeout(Duration::from_millis(100)) else { continue };
            if completion.tag == flush {
                assert!(matches!(completion.done, Done::Flushed), "{:?}", completion.done);
                return (told, last, cursors);
            }
            let batch = journaled(completion, tag);
            assert!(batch.caught_up, "{batch:?}");
            assert!(event(batch.cursor).0 >= event(last).0, "{batch:?}");
            told.extend(batch.files);
            last = batch.cursor;
            cursors.push(event(last).0);
        }
        panic!("the flush {flush:?} was not told in 10 s; told before it: {told:?}")
    }

    /// FSEvents.h's names of the bits of `flags`, joined by `|`.
    fn flag_names(flags: u32) -> String {
        const NAMES: [(u32, &str); 23] = [
            (0x01, "MustScanSubDirs"),
            (0x02, "UserDropped"),
            (0x04, "KernelDropped"),
            (0x08, "EventIdsWrapped"),
            (0x10, "HistoryDone"),
            (0x20, "RootChanged"),
            (0x40, "Mount"),
            (0x80, "Unmount"),
            (0x100, "ItemCreated"),
            (0x200, "ItemRemoved"),
            (0x400, "ItemInodeMetaMod"),
            (0x800, "ItemRenamed"),
            (0x1000, "ItemModified"),
            (0x2000, "ItemFinderInfoMod"),
            (0x4000, "ItemChangeOwner"),
            (0x8000, "ItemXattrMod"),
            (0x10000, "ItemIsFile"),
            (0x20000, "ItemIsDir"),
            (0x40000, "ItemIsSymlink"),
            (0x80000, "OwnEvent"),
            (0x100000, "ItemIsHardlink"),
            (0x200000, "ItemIsLastHardlink"),
            (0x400000, "ItemCloned"),
        ];
        let mut out = String::new();
        for (bit, name) in NAMES {
            if flags & bit != 0 {
                if !out.is_empty() {
                    out.push('|');
                }
                out.push_str(name);
            }
        }
        let rest = flags & !0x7f_ffff;
        if rest != 0 {
            out.push_str(&format!("|unknown {rest:#x}"));
        }
        if out.is_empty() {
            out.push_str("none");
        }
        out
    }

    /// One line per traced event: id, flags decoded, path, the stream's place, history or
    /// live, and what `journaled` did with it.
    fn traced(events: &[Traced]) -> String {
        let mut out = String::new();
        for it in events {
            out.push_str(&format!(
                "\n    id {} ({}) {} [from {}, {}]: {}",
                it.id,
                flag_names(it.flags),
                it.path,
                it.from,
                if it.in_history { "history" } else { "live" },
                it.verdict
            ));
        }
        out
    }

    /// A journal of `src`, its place taken behind a flush after `src/before.rs` is written; then
    /// `change` run while it is stopped, and the journal resumed from the place.
    struct Resumed {
        /// What the resumed stream's batches told, to and with the caught-up one (each
        /// batch's own shape is checked in `resumed_after`).
        told: BTreeSet<FileName>,
        place: Cursor,
        /// The evidence a failure prints (rule 19): the place and how it was reached, and every
        /// event each stream heard with what became of it.
        evidence: String,
    }

    fn resumed_after(what: &str, change: impl FnOnce(&Path)) -> Resumed {
        let root = root(what);
        std::fs::create_dir_all(root.join("other")).unwrap();
        let dir = FileName::new("src").unwrap();
        let (mut watch, heard) = watch(&root);
        let first = journal(&mut watch, &root, &dir, Cursor::None, Tag(1));
        // The first batch comes at once, before any the stream hears.
        let first = first.expect("the first batch, at once");
        assert_eq!((first.files.len(), first.lost.len(), first.caught_up), (0, 0, false));
        let (start, volume) = event(first.cursor);
        let last = until_caught_up(&heard, Tag(1)).last().unwrap().cursor;
        // The place is taken behind a barrier (`Journal`'s rule): a flush asked after the write
        // is told after the write's event, so the cursor of the batches before it is at or after
        // that event's ID. A cursor read from the last batch heard by some timeout may lag it.
        let before = FileName::new("src/before.rs").unwrap();
        std::fs::write(root.join("src/before.rs"), b"fn f() {}").unwrap();
        let (live_told, place, cursors) = flushed(&watch, &heard, Tag(1), Tag(9), last);
        watch.stop(|_| {});
        let live = watch.take_trace();
        let mut evidence = format!(
            "\n  the place saved: {} (the cursor of the last batch before `Flushed`; their cursors in order: {cursors:?}; the first stream started from {start})\n  the first stream heard:{}",
            event(place).0,
            traced(&live)
        );
        assert!(live_told.contains(&before), "the write before the place was not heard live: {live_told:?}{evidence}");
        assert!(event(place).0 > start, "{evidence}");
        assert_eq!(event(place).1, volume);
        // Changed while nothing listens, then given time to reach fseventsd's store.
        change(&root);
        fseventsd_keeps(Duration::from_secs(1));
        while heard.try_recv().is_ok() {}
        // The same watch, started again from the place.
        let first = journal(&mut watch, &root, &dir, place, Tag(2));
        assert!(first.is_none(), "resumed: no first batch");
        let batches = until_caught_up(&heard, Tag(2));
        watch.stop(|_| {});
        evidence.push_str(&format!("\n  the resumed stream heard:{}\n  its batches: {batches:?}", traced(&watch.take_trace())));
        // The history, told up to and with the batch of HistoryDone, which may hold some of it.
        let (done, history) = batches.split_last().unwrap();
        assert!(done.caught_up && history.iter().all(|it| !it.caught_up), "{evidence}");
        assert!(batches.iter().all(|it| it.lost.is_empty() && it.whole.is_none()), "{evidence}");
        assert!(batches.iter().flat_map(|it| &it.files).all(|it| it.is_under(&dir) && *it != dir), "{evidence}");
        assert!(batches.iter().all(|it| event(it.cursor).0 >= event(place).0), "{evidence}");
        for batch in &batches {
            let mut sorted = batch.files.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted, batch.files, "{evidence}");
        }
        let told = batches.iter().flat_map(|it| it.files.iter().cloned()).collect();
        std::fs::remove_dir_all(root).unwrap();
        Resumed { told, place, evidence }
    }

    // The journal's conformance suite (D167 part c, `conform.rs`): each case one
    // `JournalCase`, run on fseventsd itself (`real_resume`, a `process` test) and on the model
    // of its store (`model_resume`, under the tape's choices and the planted bugs) by
    // `journal_suite!`. Before 2026-11-20 the three real cases were tests of their own and the
    // model drew its writes apart from them.

    /// What a case changes while the journal is stopped: a file written, or a folder made.
    #[derive(Clone, Copy, Debug)]
    enum Change {
        Write(&'static str),
        MakeDir(&'static str),
    }

    /// The names a case is judged on: what a resume tells, of these, is its outcome. A folder's
    /// own event (`src/down`, `src` changed by it) is the store's to coalesce or not, on the
    /// real one and the model alike, and is not judged.
    const JUDGED: [&str; 4] = ["src/before.rs", "src/down/lib.rs", "other/after.rs", "target/out"];

    /// One case: a journal of `src`, its place taken behind a flush after the command saw
    /// `seen` written; `changes` made while it is stopped; then resumed from the place. ONE
    /// outcome: the judged names the resume tells are exactly `told`.
    struct JournalCase<'a> {
        what: &'a str,
        seen: &'a [&'a str],
        changes: &'a [Change],
        told: &'a [&'a str],
    }

    /// Case one: after the place nothing beneath `src` changes (a file in `other/` and one in
    /// `target/` are written): the resume tells nothing, `src/before.rs` least of all.
    const STILL: JournalCase<'static> = JournalCase {
        what: "still",
        seen: &["src/before.rs"],
        changes: &[Change::Write("other/after.rs"), Change::Write("target/out")],
        told: &[],
    };

    /// Case two: after the place `src/before.rs` is written again and `src/down/lib.rs` made:
    /// the resume tells both.
    const MOVED: JournalCase<'static> = JournalCase {
        what: "moved",
        seen: &["src/before.rs"],
        changes: &[
            Change::Write("src/before.rs"),
            Change::MakeDir("src/down"),
            Change::Write("src/down/lib.rs"),
            Change::Write("target/out"),
        ],
        told: &["src/before.rs", "src/down/lib.rs"],
    };

    /// Case three, the first fixture's: after the place only a sibling is made
    /// (`src/down/lib.rs`, which changes `src` itself too); `src/before.rs` is not touched. The
    /// sibling is told and `src/before.rs` is not: a failure's evidence says whether the store
    /// re-reported it with an id after the place (coalesced with its folder's change) or the
    /// place lagged its write.
    const SIBLING: JournalCase<'static> = JournalCase {
        what: "sibling",
        seen: &["src/before.rs"],
        changes: &[Change::MakeDir("src/down"), Change::Write("src/down/lib.rs")],
        told: &["src/down/lib.rs"],
    };

    /// Of `told`, the judged names, in order.
    fn judged(told: &BTreeSet<FileName>) -> Vec<&'static str> {
        JUDGED.into_iter().filter(|it| told.contains(&FileName::new(it).expect("a name"))).collect()
    }

    /// The case on fseventsd: `resumed_after` writes `src/before.rs` before the place (the one
    /// `seen` a real case has), the changes are made on the disk while nothing listens.
    fn real_resume(case: &JournalCase) {
        assert_eq!(case.seen, ["src/before.rs"], "the real side writes this before its place");
        let resumed = resumed_after(&format!("journal-{}", case.what), |root| {
            for change in case.changes {
                match *change {
                    Change::Write(file) => std::fs::write(root.join(file), file.as_bytes()).unwrap(),
                    Change::MakeDir(dir) => std::fs::create_dir_all(root.join(dir)).unwrap(),
                }
            }
        });
        assert_eq!(
            judged(&resumed.told),
            case.told,
            "a resume from {} told {:?}{}",
            event(resumed.place).0,
            resumed.told,
            resumed.evidence
        );
    }

    /// Declares each case twice: `<case>::model::kind_simulation` and
    /// `<case>::fseventsd::kind_process`.
    macro_rules! journal_suite {
        ($($case:ident = $value:ident),* $(,)?) => {$(
            mod $case {
                blaze_sim::kind_test! {
                    simulation fn model() {
                        blaze_sim::check(blaze_sim::Config::cases(500), |tape| {
                            super::model_resume(tape, &super::$value, super::ResumePlant::None)
                        });
                    }
                }

                blaze_sim::kind_test! {
                    process fn fseventsd() {
                        super::real_resume(&super::$value);
                    }
                }
            }
        )*};
    }

    journal_suite! {
        a_journal_resumed_with_nothing_changed_beneath_it_tells_nothing_from_before_its_place = STILL,
        a_journal_resumes_from_its_cursor_with_what_changed_while_it_was_stopped = MOVED,
        a_journal_resumed_after_a_sibling_was_made_tells_the_sibling_alone = SIBLING,
    }

    fn event(cursor: Cursor) -> (u64, [u8; 16]) {
        match cursor {
            Cursor::FsEvents { event, volume } => (event, volume),
            other => panic!("{other:?}"),
        }
    }

    /// A bug planted in [`model_resume`], for its oracle to catch.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ResumePlant {
        None,
        /// FullHistory's chunk told whole: no [`after_place`] (the driver before 2026-11-20).
        Unfiltered,
        /// The place read before the barrier: the last ID told while the last write seen was
        /// not yet heard (the test before 2026-11-20 took its cursor so).
        CursorLags,
    }

    /// The cookie's name in the model: no judged name, and never under `src`.
    const COOKIE: &str = ".blaze-cookie";

    /// A case's events in the model of fseventsd's store: the command's `seen` writes, the
    /// cookie, the case's changes, each its own event at the system's next ID, with gaps (other
    /// folders' events) the tape draws. The cookie's index too.
    fn events_of<'a>(tape: &blaze_sim::Tape, case: &JournalCase<'a>) -> (Vec<(u64, &'a str)>, usize) {
        let mut events: Vec<(u64, &'a str)> = Vec::with_capacity(case.seen.len() + case.changes.len() + 1);
        let mut id: u64 = 1_000;
        let mut push = |events: &mut Vec<(u64, &'a str)>, name: &'a str, why: &'static str| {
            id += 1 + tape.below(4, why);
            events.push((id, name));
        };
        for seen in case.seen {
            push(&mut events, seen, "other events before a write the command saw");
        }
        push(&mut events, COOKIE, "other events before the cookie");
        let cookie = events.len() - 1;
        for change in case.changes {
            let (Change::Write(name) | Change::MakeDir(name)) = *change;
            push(&mut events, name, "other events before a change");
        }
        (events, cookie)
    }

    /// The case on [`Journal`]'s rule over a model of fseventsd: the command sees `seen`
    /// written (it read the files after them), asks a flush (the cookie, its own event after
    /// them) and saves the place the batches before `Flushed` reached. A resume from the place
    /// is told, as FullHistory does, every event of the store's chunk that holds the place, from
    /// a start the tape draws, and every later one, each judged by [`after_place`] and kept
    /// where it is under the watched `src`. THE ORACLE, the case's one outcome: the judged
    /// names told are exactly the case's `told`.
    fn model_resume(tape: &blaze_sim::Tape, case: &JournalCase, plant: ResumePlant) -> Result<(), blaze_sim::Broke> {
        let (events, cookie) = events_of(tape, case);
        let place = match plant {
            // The last write seen (just before the cookie) not yet heard: the place is the
            // event before it, or the stream's start.
            ResumePlant::CursorLags if cookie >= 2 => events[cookie - 2].0,
            ResumePlant::CursorLags if cookie == 1 => 1_000,
            ResumePlant::None | ResumePlant::Unfiltered | ResumePlant::CursorLags => events[cookie].0,
        };
        let holding = events.iter().position(|(it, _)| *it > place).unwrap_or(events.len());
        let chunk = tape.below(holding as u64 + 1, "where the chunk holding the place starts") as usize;
        let told: BTreeSet<FileName> = events[chunk..]
            .iter()
            .filter(|(id, _)| plant == ResumePlant::Unfiltered || after_place(place, *id))
            .filter(|(_, name)| name.starts_with("src/"))
            .map(|(_, name)| FileName::new(name).expect("a name"))
            .collect();
        let told = judged(&told);
        if let Some(file) = told.iter().copied().find(|it| !case.told.contains(it)) {
            return Err(blaze_sim::Broke::Oracle(format!(
                "told again: {file}, not changed after the command's flush, by a resume from {place} (case {}, events {events:?})",
                case.what
            )));
        }
        if let Some(file) = case.told.iter().copied().find(|it| !told.contains(it)) {
            return Err(blaze_sim::Broke::Oracle(format!(
                "not told: {file}, changed after the command's flush, by a resume from {place} (case {}, events {events:?})",
                case.what
            )));
        }
        Ok(())
    }

    /// The invariant: an FSEvents journal resume from a saved place tells nothing from before the command's flush and every change under the watched directory after it.
    /// A case the tape draws, beside the three the real side runs too: writes of the judged
    /// names, the first `seen` of them before the place; told, those under `src` written
    /// after it. Run on the model alone (each real run waits on fseventsd's store, seconds),
    /// and on the same runner as the fixed cases.
    fn drawn_resume(tape: &blaze_sim::Tape, plant: ResumePlant) -> Result<(), blaze_sim::Broke> {
        let writes = 1 + tape.below(12, "writes") as usize;
        let seen = tape.below(writes as u64 + 1, "writes the command saw before its flush") as usize;
        let names: Vec<&'static str> =
            (0..writes).map(|_| JUDGED[tape.below(JUDGED.len() as u64, "the file written") as usize]).collect();
        let changes: Vec<Change> = names[seen..].iter().copied().map(Change::Write).collect();
        let told: Vec<&str> =
            JUDGED.into_iter().filter(|it| it.starts_with("src/") && names[seen..].contains(it)).collect();
        let case = JournalCase { what: "drawn", seen: &names[..seen], changes: &changes, told: &told };
        model_resume(tape, &case, plant)
    }

    blaze_sim::kind_test! {
        /// Expected: passes. A resume tells nothing from before its place and everything after.
        simulation fn a_resume_tells_nothing_from_before_its_place_and_everything_after() {
            blaze_sim::check(blaze_sim::Config::cases(2000), |tape| drawn_resume(tape, ResumePlant::None));
        }
    }

    /// Control: FullHistory's chunk told whole is caught, on a fixed case and on drawn ones.
    #[test]
    fn the_oracle_catches_a_resume_that_tells_the_chunk_before_its_place() {
        blaze_sim::control(blaze_sim::Config::cases(4000), ResumePlant::None, ResumePlant::Unfiltered, "told again", |tape, plant| {
            model_resume(tape, &STILL, plant)
        });
        blaze_sim::control(blaze_sim::Config::cases(4000), ResumePlant::None, ResumePlant::Unfiltered, "told again", drawn_resume);
    }

    /// Control: a place read before the last write seen was heard (the lag) is caught.
    #[test]
    fn the_oracle_catches_a_place_taken_before_the_last_write_was_heard() {
        blaze_sim::control(blaze_sim::Config::cases(4000), ResumePlant::None, ResumePlant::CursorLags, "told again", |tape, plant| {
            model_resume(tape, &SIBLING, plant)
        });
        blaze_sim::control(blaze_sim::Config::cases(4000), ResumePlant::None, ResumePlant::CursorLags, "told again", drawn_resume);
    }

    #[test]
    fn a_cursor_of_another_store_or_past_now_is_lost_as_reset() {
        let root = root("reset");
        let dir = FileName::new("src").unwrap();
        let (mut watch, _heard) = watch(&root);
        let first = journal(&mut watch, &root, &dir, Cursor::None, Tag(1));
        let (now, volume) = event(first.unwrap().cursor);
        watch.stop(|_| {});
        let cursors = [
            Cursor::FsEvents { event: 1, volume: [0xab; 16] },
            Cursor::FsEvents { event: u64::MAX - 1, volume },
            Cursor::Usn { volume: 1, journal: 2, next: 3 },
            Cursor::Sim { journal: 1, next: 0 },
        ];
        for (n, since) in cursors.into_iter().enumerate() {
            let tag = Tag(10 + n as u64);
            let first = journal(&mut watch, &root, &dir, since, tag);
            watch.stop(|_| {});
            let first = first.expect("the first batch, at once");
            assert_eq!((first.lost.as_slice(), first.whole), (&[][..], Some(Lost::Reset)), "{since:?}");
            assert!(first.files.is_empty() && !first.caught_up);
            let (event, of) = event(first.cursor);
            assert!(event >= now && event < u64::MAX - 1, "{since:?}: {event}");
            assert_eq!(of, volume);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
