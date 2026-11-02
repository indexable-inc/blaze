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

use parking_lot::{Mutex, MutexGuard};

use crate::{Completion, Cursor, Done, FileName, Journaled, Lost, Tag};

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

const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_WATCH_ROOT: u32 = 0x04;
const CREATE_FILE_EVENTS: u32 = 0x10;
const EVENT_MUST_SCAN_SUB_DIRS: u32 = 0x01;
const EVENT_USER_DROPPED: u32 = 0x02;
const EVENT_KERNEL_DROPPED: u32 = 0x04;
const EVENT_IDS_WRAPPED: u32 = 0x08;
const EVENT_HISTORY_DONE: u32 = 0x10;
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

// SAFETY: these signatures match CoreFoundation's CFString.h, CFArray.h and CFUUID.h (`CFRef`
// is `CFTypeRef` and `CFUUIDRef`, `CFIndex` is `isize`, `CFUUIDBytes` is laid out as its
// sixteen `UInt8` fields); a mismatch would be UB at every call.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFStringCreateWithFileSystemRepresentation(allocator: CFRef, path: *const c_char) -> CFRef;
    fn CFArrayCreate(
        allocator: CFRef,
        values: *const CFRef,
        count: CFIndex,
        callbacks: *const CFArrayCallBacks,
    ) -> CFRef;
    fn CFUUIDGetUUIDBytes(uuid: CFRef) -> CFUUIDBytes;
    fn CFRelease(object: CFRef);
}

// SAFETY: these signatures match FSEvents.h (`FSEventStreamEventId` is `UInt64`,
// `FSEventStreamCreateFlags` is `UInt32`, `dev_t` is `__darwin_dev_t`, an `int32_t`).
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFRef,
        callback: Callback,
        context: *const Context,
        paths: CFRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(device: libc::dev_t) -> CFRef;
}

// libdispatch, in libSystem.
// SAFETY: these signatures match dispatch/queue.h and dispatch/object.h.
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attributes: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(queue: DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

/// Where a watch's completions go, from its queue's thread.
pub(crate) trait Tell: Send + Sync + 'static {
    /// Hands `completion` on, waiting up to `wait` for room: `Err(Some(it))` when there was
    /// none by then, `Err(None)` when nobody takes completions any more.
    fn tell(&self, completion: Completion, wait: Duration) -> Result<(), Option<Completion>>;
}

/// A bounded channel waits up to `wait` for room and allocates nothing per completion; an
/// unbounded one always has room, and allocates a block per 31 completions.
impl Tell for crossbeam_channel::Sender<Completion> {
    fn tell(&self, completion: Completion, wait: Duration) -> Result<(), Option<Completion>> {
        self.send_timeout(completion, wait).map_err(|error| match error {
            crossbeam_channel::SendTimeoutError::Timeout(completion) => Some(completion),
            crossbeam_channel::SendTimeoutError::Disconnected(_) => None,
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
struct Journal {
    /// The UUID of the watched directory's volume's store.
    volume: [u8; 16],
    /// The highest event ID told.
    last: AtomicU64,
    /// Whether `kFSEventStreamEventFlagHistoryDone` has been heard.
    caught_up: AtomicBool,
}

/// What the callback reads, on the watch's queue. Set by [`Watch`] only while no stream runs.
struct Shared<S> {
    tag: Tag,
    /// The watched directory, from the first start on.
    dir: Option<FileName>,
    /// The root with its symlinks resolved, as FSEvents names paths.
    root: PathBuf,
    /// The watched directory with its symlinks resolved, where it is one of its own.
    real_dir: PathBuf,
    /// This watch's cookies, symlinks resolved: watched with `real_dir`, named by the tag of
    /// the flush that wrote each.
    cookies: PathBuf,
    /// Each flush whose cookie the stream has not heard of yet.
    flushes: Mutex<Vec<Tag>>,
    /// `cookies`, kept to name a flush's cookie in without allocating ([`with_cookie`]).
    writing: Mutex<PathBuf>,
    /// The callback's buffers, kept for the watch's life: on its queue only, so never waited
    /// for.
    heard: Mutex<Heard>,
    sink: S,
    stopped: AtomicBool,
    mode: Mode,
    /// What the callbacks allocated beyond what their completions carry.
    #[cfg(test)]
    overhead: AtomicU64,
    /// What the running callback allocated for what its completions carry.
    #[cfg(test)]
    payload: AtomicU64,
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
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

/// A directory of cookies for one watch, made empty: under the temporary directory, which
/// is the process's to write wherever the watched directory is.
fn cookies() -> io::Result<PathBuf> {
    let number = WATCHES.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        // alloc: setup, once per watch.
        .join(format!("blaze-fsevents-{}", std::process::id()))
        .join(number.to_string()); // alloc: setup, once per watch.
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::canonicalize(&dir)
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
    let mut resolved = [0 as c_char; libc::PATH_MAX as usize];
    // SAFETY: a NUL-terminated path, and a buffer of `PATH_MAX` bytes, the most realpath
    // writes.
    let got = unsafe { libc::realpath(path.as_ptr().cast(), resolved.as_mut_ptr()) };
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

/// Whether `name` is `dir` or under it, as [`FileName::is_under`].
fn is_under(name: &str, dir: &str) -> bool {
    name == dir || name.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// The UUID of the FSEvents store of the volume `path` is on: of its nearest ancestor that is
/// there, for a directory not made yet. A volume with no store (one fseventsd does not keep)
/// fails, naming the device.
fn store_of(path: &Path) -> io::Result<[u8; 16]> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = path
        .ancestors()
        .find_map(|it| std::fs::metadata(it).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "none of its directories is there"))?;
    // `st_dev` is an `int32_t` that `dev()` widened; narrowing gives it back.
    let device = meta.dev() as libc::dev_t;
    // SAFETY: no pointers; the result is a CFUUID retained for the caller, or null.
    let uuid = unsafe { FSEventsCopyUUIDForDevice(device) };
    if uuid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
            format!("the volume of device {device} keeps no FSEvents store, so no journal of it"),
        ));
    }
    // SAFETY: a CFUUID, alive until it is released below.
    let bytes = unsafe { CFUUIDGetUUIDBytes(uuid) };
    // SAFETY: retained by the copy above for this function, released once, not used after.
    unsafe { CFRelease(uuid) };
    Ok(bytes.bytes)
}

impl<S: Tell> Shared<S> {
    fn dir(&self) -> &FileName {
        self.dir.as_ref().expect("a stream runs only once its directory is set")
    }

    /// Where under the watched directory a path FSEvents gave is: `None` for one outside it,
    /// `Some(None)` for one under it that no [`FileName`] can name, else its name, written at
    /// the end of `text`.
    fn name(&self, path: &Path, text: &mut String) -> Option<Option<Span>> {
        let dir = self.dir().as_str();
        let start = text.len();
        let span = |text: &String| (start as u32, text.len() as u32);
        if let Ok(rest) = path.strip_prefix(&self.real_dir) {
            if rest.as_os_str().is_empty() {
                text.push_str(dir);
                return Some(Some(span(text)));
            }
            let Some(rest) = rest.to_str() else { return Some(None) };
            text.push_str(dir);
            text.push('/');
            text.push_str(rest);
            if !FileName::is_valid(&text[start..]) {
                text.truncate(start);
                return Some(None);
            }
            return Some(Some(span(text)));
        }
        let rest = path.strip_prefix(&self.root).ok()?;
        let Some(rest) = rest.to_str() else { return Some(None) };
        if !FileName::is_valid(rest) || !is_under(rest, dir) {
            return None;
        }
        text.push_str(rest);
        Some(Some(span(text)))
    }

    /// The cookie number of a path in this watch's cookies: `None` for one outside them,
    /// `Some(None)` for the directory itself or a name that is no cookie's.
    fn cookie(&self, path: &Path) -> Option<Option<u64>> {
        let rest = path.strip_prefix(&self.cookies).ok()?;
        Some(rest.to_str().and_then(|it| it.parse::<u64>().ok()))
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
    /// A watch telling `sink`, with its directory of cookies and its queue made: none of them
    /// is made again for any stream it runs.
    pub(crate) fn new(sink: S) -> io::Result<Watch<S>> {
        let cookies = cookies().map_err(|error| {
            // alloc: an error path.
            io::Error::new(error.kind(), format!("making the cookies of a watch: {error}"))
        })?;
        // alloc: setup, once per watch: room for the paths it names to the system.
        let mut c_path = Vec::with_capacity(256);
        c_string(&cookies, &mut c_path)?;
        // SAFETY: a NUL-terminated path; the string is released in `Drop`.
        let cookie_string =
            unsafe { CFStringCreateWithFileSystemRepresentation(std::ptr::null(), c_path.as_ptr().cast()) };
        if cookie_string.is_null() {
            _ = std::fs::remove_dir_all(&cookies);
            return Err(io::Error::other("CoreFoundation made no string of the cookies' path"));
        }
        // SAFETY: a NUL-terminated label; no attributes is a serial queue.
        let queue = unsafe { dispatch_queue_create(c"blaze-fsevents".as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            // SAFETY: made above, released once, not used after.
            unsafe { CFRelease(cookie_string) };
            _ = std::fs::remove_dir_all(&cookies);
            return Err(io::Error::other("libdispatch made no queue"));
        }
        let shared = Shared {
            tag: Tag(0),
            dir: None,
            // alloc: setup, once per watch: the resolved paths, at the size most have.
            root: PathBuf::with_capacity(256),
            real_dir: PathBuf::with_capacity(256), // alloc: as above.
            writing: Mutex::new(cookie_buffer(&cookies)),
            // alloc: setup, once per watch: the callback's buffers at the size a batch of a
            // few names and cookies needs.
            heard: Mutex::new(Heard {
                cookies: Vec::with_capacity(4),
                told: Vec::with_capacity(4),
                path: cookie_buffer(&cookies),
                text: String::with_capacity(1024),
                names: Vec::with_capacity(32),
                lost: Vec::with_capacity(4),
            }),
            cookies,
            flushes: Mutex::new(Vec::with_capacity(4)), // alloc: setup, once per watch.
            sink,
            stopped: AtomicBool::new(true),
            mode: Mode::Watch,
            #[cfg(test)]
            overhead: AtomicU64::new(0),
            #[cfg(test)]
            payload: AtomicU64::new(0),
        };
        Ok(Watch { shared: Owned::new(shared), queue, cookie_string, stream: std::ptr::null_mut(), c_path })
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.stream.is_null()
    }

    /// Watches `dir` under `root` (kFSEventStreamCreateFlagFileEvents | NoDefer | WatchRoot,
    /// latency 0, on the watch's queue), telling a `Completion { tag, done: Done::Changed {
    /// files, lost } }` for each batch, names relative to `root` as `FileName`s, each once;
    /// kFSEventStreamEventFlagMustScanSubDirs, UserDropped or KernelDropped make `lost: true`.
    pub(crate) fn start(&mut self, root: &Path, dir: FileName, tag: Tag) -> io::Result<()> {
        self.locate(root, &dir)?;
        self.open(dir, tag, SINCE_NOW, Mode::Watch, || {})
    }

    /// The journal of `dir` under `root` from `since` ([`crate::Effect::Journal`]): a stream as
    /// [`Watch::start`]'s, telling `Done::Journaled` batches. From a cursor of this volume's
    /// store at or before the system's current event ID it starts `sinceWhen` that ID, its
    /// history told with `caught_up: false` until HistoryDone; from anything else it starts at
    /// the current ID, read before the stream is made so no event falls between, with a first
    /// batch handed to `first` on the caller's thread just before the stream starts, for the
    /// caller to tell before anything the stream tells: empty, at that ID, and naming `dir`
    /// lost as [`Lost::Reset`] unless `since` was [`Cursor::None`].
    pub(crate) fn journal(
        &mut self,
        root: &Path,
        dir: FileName,
        since: Cursor,
        tag: Tag,
        first: impl FnOnce(Journaled),
    ) -> io::Result<()> {
        self.locate(root, &dir)?;
        let volume = store_of(&self.shared.get().real_dir).map_err(|error| {
            let at = root.join(dir.as_str()); // alloc: an error path.
            // alloc: an error path.
            io::Error::new(error.kind(), format!("the journal of {}: {error}", at.display()))
        })?;
        // SAFETY: no arguments; reads the system's last event ID.
        let now = unsafe { FSEventsGetCurrentEventId() };
        let resumed = match since {
            Cursor::FsEvents { event, volume: of } if of == volume && event <= now => Some(event),
            _ => None,
        };
        let lost = match (resumed, since) {
            (Some(_), _) => None,
            (None, Cursor::None) => Some(Vec::new()), // alloc: none, an empty vector.
            (None, _) => Some(vec![(dir.clone(), Lost::Reset)]), // alloc: the batch's payload.
        };
        let batch = lost.map(|lost| Journaled {
            files: Vec::new(), // alloc: none, an empty vector.
            lost,
            cursor: Cursor::FsEvents { event: now, volume },
            caught_up: false,
        });
        let from = resumed.unwrap_or(now);
        let journal =
            Journal { volume, last: AtomicU64::new(from), caught_up: AtomicBool::new(false) };
        let before = || {
            if let Some(batch) = batch {
                first(batch);
            }
        };
        self.open(dir, tag, from, Mode::Journal(journal), before)
    }

    /// `root` and the watched directory under it resolved into the shared paths, as FSEvents
    /// names them; a directory that is not there (yet) is watched as the root names it.
    fn locate(&mut self, root: &Path, dir: &FileName) -> io::Result<()> {
        assert!(!self.is_running(), "a watch runs one stream at a time");
        // SAFETY: no stream runs, asserted above.
        let Shared { root: real_root, real_dir, .. } = unsafe { self.shared.get_mut() };
        let resolved = c_string(root, &mut self.c_path).and_then(|()| realpath(&self.c_path, real_root));
        resolved.map_err(|error| {
            // alloc: an error path.
            io::Error::new(error.kind(), format!("watching under {}: {error}", root.display()))
        })?;
        real_dir.as_mut_os_string().clear();
        real_dir.push(&*real_root);
        real_dir.push(dir.as_str());
        c_string(real_dir, &mut self.c_path)?;
        _ = realpath(&self.c_path, real_dir);
        Ok(())
    }

    /// The stream of `shared.real_dir` and the cookies from `since`, `before` run once it is
    /// made, just before it starts.
    fn open(&mut self, dir: FileName, tag: Tag, since: u64, mode: Mode, before: impl FnOnce()) -> io::Result<()> {
        let failed = |shared: &Shared<S>, why: &str| {
            // alloc: an error path.
            io::Error::other(format!("watching {}: {why}", shared.real_dir.display()))
        };
        assert!(!self.is_running(), "a watch runs one stream at a time");
        {
            // SAFETY: no stream runs, asserted above.
            let shared = unsafe { self.shared.get_mut() };
            shared.tag = tag;
            shared.dir = Some(dir);
            shared.mode = mode;
        }
        c_string(&self.shared.get().real_dir, &mut self.c_path)?;
        // SAFETY: a NUL-terminated path; the string is released below.
        let dir_string = unsafe {
            CFStringCreateWithFileSystemRepresentation(std::ptr::null(), self.c_path.as_ptr().cast())
        };
        if dir_string.is_null() {
            return Err(failed(self.shared.get(), "CoreFoundation made no string of the path"));
        }
        let strings = [dir_string, self.cookie_string];
        // SAFETY: two CFStrings, retained by the array with the callbacks of CF types.
        let paths = unsafe {
            CFArrayCreate(std::ptr::null(), strings.as_ptr(), 2, &raw const kCFTypeArrayCallBacks)
        };
        // SAFETY: made above, released once; the array holds its own reference.
        unsafe { CFRelease(dir_string) };
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
            FSEventStreamCreate(
                std::ptr::null(),
                heard::<S>,
                &context,
                paths,
                since,
                0.0,
                CREATE_FILE_EVENTS | CREATE_NO_DEFER | CREATE_WATCH_ROOT,
            )
        };
        // SAFETY: the stream holds its own reference to the paths.
        unsafe { CFRelease(paths) };
        if stream.is_null() {
            self.shared.get().stopped.store(true, Ordering::Release);
            return Err(failed(self.shared.get(), "FSEventStreamCreate made no stream"));
        }
        // SAFETY: a stream not yet started and the watch's queue, which outlives it.
        unsafe { FSEventStreamSetDispatchQueue(stream, self.queue) };
        before();
        // SAFETY: the stream is scheduled on the queue.
        if unsafe { FSEventStreamStart(stream) } == 0 {
            // SAFETY: the stream did not start, so no callback runs during or after this.
            unsafe { FSEventStreamInvalidate(stream) };
            // SAFETY: the stream made above, released once and not used after.
            unsafe { FSEventStreamRelease(stream) };
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
        let mut stream = self.stream;
        // SAFETY: the stream is stopped and invalidated on its own queue, so no callback runs
        // during it; `stream` outlives the call, which returns once the work has run.
        unsafe { dispatch_sync_f(self.queue, (&raw mut stream).cast(), end_on_queue) };
        // SAFETY: a serial queue: this runs after any callback queued before the invalidation.
        unsafe { dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing) };
        // SAFETY: invalidated, with no callback left to run; released once, not used after.
        unsafe { FSEventStreamRelease(self.stream) };
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
}

impl<S> Drop for Watch<S> {
    fn drop(&mut self) {
        if !self.stream.is_null() {
            self.shared.get().stopped.store(true, Ordering::Release);
            let mut stream = self.stream;
            // SAFETY: as in `stop`.
            unsafe { dispatch_sync_f(self.queue, (&raw mut stream).cast(), end_on_queue) };
            // SAFETY: as in `stop`.
            unsafe { dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing) };
            // SAFETY: as in `stop`.
            unsafe { FSEventStreamRelease(self.stream) };
        }
        // SAFETY: no stream uses the queue any more; released once, not used after.
        unsafe { dispatch_release(self.queue) };
        // SAFETY: made in `new`, released once, not used after; every array that held it is
        // released with its stream.
        unsafe { CFRelease(self.cookie_string) };
        _ = std::fs::remove_dir_all(&self.shared.get().cookies);
    }
}

extern "C" fn end_on_queue(stream: *mut c_void) {
    // SAFETY: `stop` passes a pointer to its copy of the stream, alive for this call.
    let stream = unsafe { *stream.cast::<FSEventStreamRef>() };
    // SAFETY: a stream started and not yet released, stopped on its own queue.
    unsafe { FSEventStreamStop(stream) };
    // SAFETY: as above; no callback is scheduled once this returns.
    unsafe { FSEventStreamInvalidate(stream) };
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
    let seen = heard.lost.iter().any(|&(it, was, _)| was == why && heard.str(it) == heard.str(name));
    if !seen {
        let order = heard.lost.len() as u32;
        heard.lost.push((name, why, order));
    }
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
    heard.text.push_str(shared.dir().as_str());
    let dir: Span = (0, heard.text.len() as u32);
    let before = journal.last.load(Ordering::Relaxed);
    let was_caught_up = journal.caught_up.load(Ordering::Relaxed);
    let (mut last, mut caught_up) = (before, was_caught_up);
    for ((&flag, &path), &id) in flags.iter().zip(paths).zip(ids) {
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
    let (files, bad) = shared.payload(|| make_names(heard));
    if bad {
        lose(heard, dir, Lost::Rescan);
    }
    {
        let Heard { text, lost, .. } = &mut *heard;
        let text: &str = text;
        let at = |span: Span| &text[span.0 as usize..span.1 as usize];
        lost.sort_unstable_by(|a, b| at(a.0).cmp(at(b.0)).then(a.2.cmp(&b.2)));
    }
    let lost = shared.payload(|| {
        let mut lost = Vec::with_capacity(heard.lost.len());
        for &(span, why, _) in &heard.lost {
            // The watched directory's name, or one `name` checked, under it.
            // clone: the batch's payload, where a checked name was not one after all.
            let name = FileName::new(heard.str(span)).unwrap_or_else(|| shared.dir().clone());
            lost.push((name, why));
        }
        lost
    });
    journal.last.store(last, Ordering::Relaxed);
    journal.caught_up.store(caught_up, Ordering::Relaxed);
    let cursor = Cursor::FsEvents { event: last, volume: journal.volume };
    let journaled = Journaled { files, lost, cursor, caught_up };
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
        // Its removal is heard too, of a flush told already: nothing.
        _ = with_cookie(path, tag, |it| std::fs::remove_file(it));
        shared.send(Completion { tag, done: Done::Flushed });
    }
}

/// Gives fseventsd `long` to put what was written into its store, for a test that reads it
/// back from a cursor: it writes its store on its own schedule, and nothing says when an event
/// is in it.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "fseventsd's store has no event that says what it holds")]
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

    /// A root under the temporary directory, which is `/var/...` named and `/private/var/...`
    /// real, with `src` and `target` made in it.
    fn root(what: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("blaze-fsevents-{what}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        root
    }

    /// A bounded channel, as the driver's: no allocation per completion.
    type Sink = crossbeam_channel::Sender<Completion>;

    type Heard = crossbeam_channel::Receiver<Completion>;

    fn watch() -> (Watch<Sink>, Heard) {
        let (send, heard) = crossbeam_channel::bounded(1024);
        (Watch::new(send).unwrap(), heard)
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
        let (mut watch, heard) = watch();
        let dir = FileName::new("src").unwrap();
        watch.start(&root, dir.clone(), Tag(4)).unwrap();
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
        let (mut watch, heard) = watch();
        watch.start(&root, FileName::new("src").unwrap(), Tag(1)).unwrap();
        hear_written(&root, "src/a.rs", &heard, Tag(1));
        watch.stop(|_| {});
        while heard.try_recv().is_ok() {}
        let other = FileName::new("other").unwrap();
        watch.start(&root, other.clone(), Tag(2)).unwrap();
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
        let (mut watch, heard) = watch();
        watch.start(&root, FileName::new("src").unwrap(), Tag(7)).unwrap();
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
        let (mut watch, heard) = watch();
        watch.start(&root, FileName::new("src").unwrap(), Tag(1)).unwrap();
        hear_written(&root, "src/warm.rs", &heard, Tag(1));
        watch.stop(|_| {});
        watch.take_overhead();
        watch.start(&root, FileName::new("src").unwrap(), Tag(1)).unwrap();
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
        watch.journal(root, dir.clone(), since, tag, |it| first = Some(it)).unwrap();
        first
    }

    fn event(cursor: Cursor) -> (u64, [u8; 16]) {
        match cursor {
            Cursor::FsEvents { event, volume } => (event, volume),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_journal_resumes_from_its_cursor_with_what_changed_while_it_was_stopped() {
        let root = root("journal");
        let dir = FileName::new("src").unwrap();
        let (mut watch, heard) = watch();
        let first = journal(&mut watch, &root, &dir, Cursor::None, Tag(1));
        // The first batch comes at once, before any the stream hears.
        let first = first.expect("the first batch, at once");
        assert_eq!((first.files.len(), first.lost.len(), first.caught_up), (0, 0, false));
        let (start, volume) = event(first.cursor);
        let mut last = until_caught_up(&heard, Tag(1)).last().unwrap().cursor;
        // A change heard live moves the cursor past it.
        let before = FileName::new("src/before.rs").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut heard_before = false;
        while !heard_before && Instant::now() < deadline {
            std::fs::write(root.join("src/before.rs"), b"fn f() {}").unwrap();
            while let Ok(completion) = heard.recv_timeout(Duration::from_millis(100)) {
                let batch = journaled(completion, Tag(1));
                assert!(batch.caught_up);
                assert!(batch.files.iter().all(|it| it.is_under(&dir)), "{batch:?}");
                assert!(event(batch.cursor).0 >= event(last).0);
                heard_before |= batch.files.contains(&before);
                last = batch.cursor;
            }
        }
        assert!(heard_before);
        assert!(event(last).0 > start);
        assert_eq!(event(last).1, volume);
        watch.stop(|_| {});
        // Written while nothing listens, then given time to reach fseventsd's store.
        std::fs::create_dir_all(root.join("src/down")).unwrap();
        std::fs::write(root.join("src/down/lib.rs"), b"fn g() {}").unwrap();
        std::fs::write(root.join("target/out"), b"not watched").unwrap();
        fseventsd_keeps(Duration::from_secs(1));
        while heard.try_recv().is_ok() {}
        // The same watch, started again from the cursor.
        let first = journal(&mut watch, &root, &dir, last, Tag(2));
        assert!(first.is_none(), "resumed: no first batch");
        let batches = until_caught_up(&heard, Tag(2));
        watch.stop(|_| {});
        // The history, told up to and with the batch of HistoryDone, which may hold some of it.
        let (done, history) = batches.split_last().unwrap();
        assert!(done.caught_up && history.iter().all(|it| !it.caught_up), "{batches:?}");
        let down = FileName::new("src/down/lib.rs").unwrap();
        let told: BTreeSet<_> = batches.iter().flat_map(|it| it.files.iter().cloned()).collect();
        assert!(told.contains(&down), "{batches:?}");
        assert!(!told.contains(&before), "told again from before the cursor: {batches:?}");
        assert!(batches.iter().all(|it| it.lost.is_empty()), "{batches:?}");
        assert!(batches.iter().flat_map(|it| &it.files).all(|it| it.is_under(&dir)), "{batches:?}");
        assert!(batches.iter().all(|it| event(it.cursor).0 >= event(last).0));
        for batch in &batches {
            let mut sorted = batch.files.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted, batch.files);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_cursor_of_another_store_or_past_now_is_lost_as_reset() {
        let root = root("reset");
        let dir = FileName::new("src").unwrap();
        let (mut watch, _heard) = watch();
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
            assert_eq!(first.lost, [(dir.clone(), Lost::Reset)], "{since:?}");
            assert!(first.files.is_empty() && !first.caught_up);
            let (event, of) = event(first.cursor);
            assert!(event >= now && event < u64::MAX - 1, "{since:?}: {event}");
            assert_eq!(of, volume);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
