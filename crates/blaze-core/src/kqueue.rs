//! A [`Driver`] for macOS: one kqueue the polling thread waits on, for processes, sockets and
//! the end of work done elsewhere; timers are a [`Wheel`] whose next deadline ends the wait,
//! with no registration in the kernel. File calls block, so they run on threads of the
//! driver's own; watches and journals are FSEvents streams on dispatch queues of their own.
//! Both hand their completions back through a bounded channel and wake the kqueue with an
//! `EVFILT_USER` trigger, fired once for all the hand-backs between two drains.
//!
//! Every registration carries a handle of the driver's in `udata`: the bits of a
//! [`blaze_mem::Handle`] into a slab of what each names, so an event of a registration that
//! has ended (its descriptor closed and reused within one batch, its process reaped, its slot
//! holding another since) is a stale handle, dropped. Changes are made with the next wait, all
//! in one call, their failures coming back as events.
//!
//! Memory (docs/v2/plan/design/memory.md): processes, listens, connects, connections and
//! watches sit in [`Slab`]s tagged [`MemTag::Io`], named by their handles (a [`Conn`] is one's
//! bits), with maps from the caller's [`Tag`] to the handle reserved at setup. A file effect
//! goes to its lane's threads (crate::threads::lanes: a fast lane, and a device lane for what
//! waits on the drive; each effect at its asker's QoS class) through a bounded channel of the lane's and comes
//! back through another, each holding [`FILES`] a lane, the most at its threads at once (more
//! wait in a backlog the driver keeps a lane, up to [`BACKLOG`]), so
//! neither ever waits and a round trip allocates nothing. Watches tell through a channel of
//! their own, and a stopped watch is kept with its buffers for the next. A warm submit and
//! poll allocate only what a completion carries.
//!
//! Why unsafe: std has no kqueue. One `kevent` call makes every change and takes every event of
//! a wait, written into the driver's kept list; there is no safe equivalent short of a thread
//! per descriptor. An event's `udata` is never followed as a pointer: it is a handle's bits,
//! looked up in a slab by index and generation (`dispatch`).

use std::{
    collections::VecDeque,
    io,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
    path::{Path, PathBuf},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_mem::{MemError, Slab, Tag as MemTag};
use blaze_own::Share;
use blaze_sys::{
    Errno, Fd,
    posix::{
        io::{self as sys_io, Filter, KeventFlags, NoteFlags},
        net as sys_net,
    },
};
use crate::chan::{Receiver, SendTimeoutError, Sender};
use std::alloc::Global;

use crate::{
    Address, Command, Completion, Confine, Conn, Cursor, DirName, Done, Driver, Effect, IoError,
    Tag,
    descriptors::Descriptors,
    fsevents::{self, Tell},
    handoff::Wake,
    net, process,
    threads::{
        lanes::{self, Dispatch, LANES, Lane, Limits, Placed, Split, Ticket},
        opening::{self, OpenCount, Opening},
        qos::{self, Lowered, Qos},
    },
    wait::{self, Clock as _, Deadline, Monotonic, Waited},
    wheel::{self, Wheel},
};

/// The `udata` of the trigger that says a channel has completions: no handle's bits, since a
/// slab issues odd generations only.
const TRIGGER: u64 = 0;
/// Events taken off the kqueue at once, besides the failures of the changes made with them.
const EVENTS: usize = 64;
/// File effects at the threads at once: queued, running, or done and not yet taken. Each
/// channel between the driver and the threads holds this many, so neither a submit nor a file
/// thread ever waits on one; more wait in the driver's backlog.
const FILES: usize = 1024;
/// File effects a lane holds in its backlog past its [`FILES`] at the threads: past it an effect
/// is refused with [`lanes::Full`], naming the lane. A bound, not a budget: the store asks a few
/// thousand at once at most; the backlog's memory is taken only as it fills.
const BACKLOG: usize = 1 << 20;
/// Completions of watches not yet taken: a watch that hears more waits for room (its queue
/// holds fseventsd back, which drops what it cannot hold and says so).
const WATCHED: usize = 4096;
/// The most registrations, processes, listens, connects, connections and watches the driver
/// holds at once: each slab's reservation, committed only as used.
const MOST: u32 = 1 << 20;
/// Watches kept stopped for the next, their cookies, queue and buffers made already.
const SPARE_WATCHES: usize = 64;
/// Tags watched, spawned or listening at once that the maps hold before they grow.
const TAGS: usize = 256;

/// The kernel's filter of memory pressure (`<sys/event_private.h>`, private, rule
/// macos-private-apis): libdispatch's `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` is a knote of it,
/// on a dispatch queue's thread of its own. Registered on the driver's own kqueue, the
/// pressure comes as one more event of the wait the loop already makes, with no thread, no
/// queue and no hand-back through a channel. `EVFILT_VM` with `NOTE_VM_PRESSURE`, the public
/// header's, is no filter any more: XNU's table maps it to `bad_filtops`
/// (`bsd/kern/kern_event.c`), so a registration of it fails. Any process may register this
/// one: `memorystatus_knote_register` (`bsd/kern/kern_memorystatus_notify.c`) checks only
/// that the flags asked are userspace's.
const EVFILT_MEMORYSTATUS: i16 = -14;
/// The system's pressure has changed to its warning level.
const NOTE_MEMORYSTATUS_PRESSURE_WARN: u32 = 0x0000_0002;
/// The system's pressure has changed to its critical level.
const NOTE_MEMORYSTATUS_PRESSURE_CRITICAL: u32 = 0x0000_0004;

/// `struct kevent` as the system lays it out (`<sys/event.h>` packs it to 4), with `udata` the
/// driver's handle, so the lists of changes and events are plain data of the driver's own.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct Kevent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: usize,
}

const _: () = assert!(
    size_of::<Kevent>() == size_of::<sys_io::Kevent>()
        && align_of::<Kevent>() == align_of::<sys_io::Kevent>()
);

/// A file effect on its way to its lane's threads: its ticket, and when it was asked on the
/// driver's clock.
struct Queued {
    effect: Effect,
    ticket: Ticket,
    asked: Duration,
    /// The class of the thread that asked for it, at the ask: the taking thread runs it at most
    /// there (`work_on`), so an effect asked after the answer (in a [`Lowered`] stretch) runs
    /// lowered and one asked before it at the command's class, whichever lane it waits in.
    asker: Qos,
}

/// A file effect's completion handed back: its ticket, and how long it waited in its lane's
/// queue before a thread took it.
struct Finished {
    completion: Completion,
    ticket: Ticket,
    waited_ns: u64,
}

/// The kqueue and the channels, shared with the file threads and the watches' queues.
struct Notify {
    kq: OwnedFd,
    files: Sender<Finished>,
    watched: Sender<Completion>,
    /// Whether a hand-back since the driver last drained the channels has fired the trigger:
    /// a batch of them costs one `kevent`, not one each.
    woke: Wake,
    /// A [`crate::Waker`] asked the wait to end ([`Driver::waker`]): set before the trigger is
    /// fired, read by the polling thread after it drained and before it waits, so a wake asked
    /// while another hand-back's trigger is pending is not lost with that trigger.
    asked: std::sync::atomic::AtomicBool,
    /// What the file threads allocated doing their effects and handing them back.
    #[cfg(test)]
    file_allocations: std::sync::atomic::AtomicU64,
    /// Triggers fired.
    #[cfg(test)]
    triggers: std::sync::atomic::AtomicU64,
    /// The class the last file effect ran at, on its thread (`Qos::raw`).
    #[cfg(test)]
    ran_at: std::sync::atomic::AtomicU32,
}

impl Notify {
    fn change(
        &self,
        ident: usize,
        filter: i16,
        flags: u16,
        fflags: u32,
        data: isize,
        id: u64,
    ) -> io::Result<()> {
        let event = Kevent { ident, filter, flags, fflags, data, udata: id as usize };
        // With no room for events the call never waits: the clock only bounds its retries.
        // SAFETY: one change, laid out as a `kevent`, to read and no events to write.
        let changed = unsafe {
            wait::kevent(
                "a change of the kqueue",
                self.kq.as_raw_fd(),
                (&raw const event).cast(),
                1,
                std::ptr::null_mut(),
                0,
                None,
                &Monotonic::new(),
            )
        };
        changed.map(drop).map_err(io::Error::from)
    }

    /// After a hand-back into a channel: wakes the polling thread, unless one since its last
    /// drain ([`Kqueue::take`]) has. The trigger fired stays pending until a wait takes it, and
    /// every wait is followed by a drain before the next, so no hand-back is left unwoken.
    fn wake(&self) {
        if self.woke.wake() {
            #[cfg(test)]
            self.triggers.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            _ = self.change(TRIGGER as usize, Filter::USER.0, 0, NoteFlags::TRIGGER.bits(), 0, TRIGGER);
        }
    }
}

/// The kqueue's [`crate::Waker`]: `asked` is set first, then the trigger fired unless one is
/// pending. The polling thread drains (clearing `woke`), then reads `asked`, then waits: a wake
/// that found `woke` set was made before that drain, so the read after it sees `asked`; one that
/// found it clear fired the trigger, which ends the wait, and the next turn of the poll reads
/// `asked`.
impl crate::Wake for Notify {
    fn wake(&self) {
        self.asked.store(true, std::sync::atomic::Ordering::SeqCst);
        Notify::wake(self);
    }
}

/// Where the host's effects ([`crate::host`]) are told: a channel of their own, and a wake.
struct HostSink {
    done: Sender<Completion>,
    notify: Arc<Notify>,
}

impl crate::host::Sink for HostSink {
    fn tell(&self, completion: Completion) {
        // A driver dropped takes nothing.
        if self.done.send(completion).is_ok() {
            self.notify.wake();
        }
    }
}

/// Where a watch tells what it hears: the watches' channel, and a wake.
struct Watched(Arc<Notify>);

impl Tell for Watched {
    fn tell(&self, completion: Completion, wait: Duration) -> Result<(), Option<Completion>> {
        match self.0.watched.send_timeout(completion, wait) {
            Ok(()) => {
                self.0.wake();
                Ok(())
            }
            Err(SendTimeoutError::Timeout(completion)) => Err(Some(completion)),
            Err(SendTimeoutError::Disconnected(_)) => Err(None),
        }
    }
}

type Id = blaze_mem::Handle<Owner>;
type SpawnId = blaze_mem::Handle<Spawned>;
type ListenId = blaze_mem::Handle<Listening>;
type ConnectId = blaze_mem::Handle<Connecting>;
type ConnId = blaze_mem::Handle<Connection>;
type WatchId = blaze_mem::Handle<fsevents::Watch<Watched>>;

/// What an event is about, by the handle in its `udata`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Owner {
    Exit(SpawnId),
    Output { spawn: SpawnId, err: bool },
    Input(SpawnId),
    Listen(ListenId),
    Connecting(ConnectId),
    Read(ConnId),
    Write(ConnId),
    /// The [`Effect::Pressure`] of this tag: its `EVFILT_MEMORYSTATUS` knote.
    Pressure(Tag),
}

/// What each registration's `udata` names. Ids are handles' bits: one of a registration that
/// has ended names nothing, even once its slot holds another's.
struct Owners {
    slab: LazySlab<Owner>,
}

impl Owners {
    fn new() -> Result<Owners, MemError> {
        Ok(Owners { slab: LazySlab::new() })
    }

    fn own(&mut self, owner: Owner) -> u64 {
        self.slab.insert(owner).to_bits()
    }

    fn get(&self, id: u64) -> Option<Owner> {
        self.slab.get(Id::from_bits(id)).copied()
    }

    fn disown(&mut self, id: u64) -> Option<Owner> {
        self.slab.remove(Id::from_bits(id))
    }
}

/// A process's registrations: its output, its error, its input and its end.
#[derive(Default)]
struct Ids {
    ids: [u64; 4],
    len: usize,
}

impl Ids {
    fn push(&mut self, id: u64) {
        self.ids[self.len] = id;
        self.len += 1;
    }

    fn as_slice(&self) -> &[u64] {
        &self.ids[..self.len]
    }
}

struct Spawned {
    tag: Tag,
    pid: blaze_sys::posix::types::pid_t,
    /// Open until its input is all written.
    stdin: Option<OwnedFd>,
    written: usize,
    /// Open until each ends.
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    out: Vec<u8>,
    err: Vec<u8>,
    exited: bool,
    /// Why its output or input could not all be moved.
    failed: Option<io::Error>,
    finish: process::Finish,
    ids: Ids,
}

struct Listening {
    tag: Tag,
    listener: net::Listener,
    id: u64,
}

struct Connecting {
    tag: Tag,
    fd: OwnedFd,
    at: Address,
    unix: bool,
}

struct Connection {
    socket: net::Socket,
    /// Each receive, its most and the buffer it appends to.
    recvs: VecDeque<(Tag, usize, Vec<u8>)>,
    /// Each send, and how much of it is sent.
    sends: VecDeque<(Tag, Vec<u8>, usize)>,
    read: u64,
    write: u64,
}

/// The handle `conn` names. A generation no live value has (an even one) becomes a slot past
/// `MOST`, which no lookup finds: a free slot's generation is even too, and `Slab` would answer
/// for it.
fn conn_id(conn: Conn) -> ConnId {
    let id = ConnId::from_bits(conn.0);
    if id.generation() & 1 == 1 { id } else { ConnId::from_bits(u64::from(u32::MAX)) }
}

fn no_slab(error: MemError) -> io::Error {
    io::Error::other(error)
}

/// A [`Slab`] whose region is reserved at its first insert, not with the driver: a program that
/// never spawns, listens, connects or watches reserves nothing for them (std.md, "What a program
/// pays for at start": macOS charges a reservation by its size, and these were seven of a
/// start's map calls). Reads of one never inserted into find nothing.
struct LazySlab<T> {
    slab: Option<Slab<T>>,
}

impl<T> LazySlab<T> {
    const fn new() -> LazySlab<T> {
        LazySlab { slab: None }
    }

    /// The slab, reserved now if it was not.
    fn made(&mut self) -> Result<&mut Slab<T>, MemError> {
        if self.slab.is_none() {
            self.slab = Some(Slab::new(MOST, MemTag::Io)?);
        }
        Ok(self.slab.as_mut().expect("made just above"))
    }

    fn try_insert(&mut self, value: T) -> Result<blaze_mem::Handle<T>, MemError> {
        self.made()?.try_insert(value)
    }

    /// As [`Slab::insert`]: a refusal of its region or its room panics, naming the tag.
    #[track_caller]
    fn insert(&mut self, value: T) -> blaze_mem::Handle<T> {
        match self.made() {
            Ok(slab) => slab.insert(value),
            Err(error) => panic!("the kqueue driver's slab of {}: {error}", core::any::type_name::<T>()),
        }
    }

    fn get(&self, handle: blaze_mem::Handle<T>) -> Option<&T> {
        self.slab.as_ref()?.get(handle)
    }

    fn get_mut(&mut self, handle: blaze_mem::Handle<T>) -> Option<&mut T> {
        self.slab.as_mut()?.get_mut(handle)
    }

    fn remove(&mut self, handle: blaze_mem::Handle<T>) -> Option<T> {
        self.slab.as_mut()?.remove(handle)
    }

    fn len(&self) -> usize {
        self.slab.as_ref().map_or(0, Slab::len)
    }

    #[cfg(test)]
    fn contains(&self, handle: blaze_mem::Handle<T>) -> bool {
        self.slab.as_ref().is_some_and(|slab| slab.contains(handle))
    }

    /// Whether its region was reserved: a test's view.
    #[cfg(test)]
    fn reserved(&self) -> bool {
        self.slab.is_some()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn iter(&self) -> impl Iterator<Item = (blaze_mem::Handle<T>, &T)> {
        self.slab.iter().flat_map(Slab::iter)
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (blaze_mem::Handle<T>, &mut T)> {
        self.slab.iter_mut().flat_map(Slab::iter_mut)
    }
}

/// `tags`, made at [`TAGS`] at its first use and grown past that only as a table grows.
fn first_use<V>(tags: &mut Map<Tag, V, FixedState, Global>) -> &mut Map<Tag, V, FixedState, Global> {
    if tags.capacity() == 0 {
        // alloc: once per driver and table, at its first tag.
        tags.reserve(TAGS);
    }
    tags
}

/// The macOS driver. `S` is which lane of file threads each file effect waits in
/// (crate::threads::lanes), `O` how those threads open the file of an effect a kept descriptor
/// serves (crate::threads::opening): the defaults are the shipped choice, the other the
/// tested control.
pub struct Kqueue<S: Split = lanes::TwoLanes, O: Opening = opening::Kept> {
    start: Instant,
    /// The machine's boot, read once as the driver is made ([`crate::Driver::boot`]).
    boot: Result<crate::Boot, crate::NoBoot>,
    root: Arc<Path>,
    notify: Arc<Notify>,
    files: Receiver<Finished>,
    watched: Receiver<Completion>,
    /// What the host told ([`HostSink`]).
    hosted: Receiver<Completion>,
    /// The datagrams, the standard streams, the clock and randomness ([`crate::host`]).
    host: crate::host::Host,
    /// Each lane's queue to its threads, by [`Lane::index`].
    work: Option<[Sender<Queued>; LANES]>,
    /// File effects of each lane asked for beyond the [`FILES`] at its threads, in the order
    /// asked.
    backlog: [VecDeque<Queued>; LANES],
    /// Which lane each file effect waits in; each lane's threads, room and backlog counted;
    /// the waits (crate::threads::lanes).
    dispatch: Dispatch<S>,
    /// The file threads started so far, of both lanes: one more in a lane each time a file
    /// effect finds every one of its lane busy, up to the lane's cap (the `threads` it was made
    /// with).
    workers: Vec<JoinHandle<()>>,
    /// Each lane's threads' end of its queue, cloned into each as it starts.
    queues: [Receiver<Queued>; LANES],
    // dyn: the platform's sandbox, given once at setup and asked once per spawn.
    confine: Option<Arc<dyn Confine>>,
    /// Where each sandboxed spawn's own directory is made: `root/.blaze-spawn` unless
    /// [`Kqueue::scratch`] says otherwise.
    spawn_dirs: PathBuf,
    /// Where a process's output is read into.
    buffer: Vec<u8>,
    /// Made with the next wait.
    changes: Vec<Kevent>,
    events: Vec<Kevent>,
    /// Completions made on the polling thread, in the order they happened.
    ready: Vec<Completion>,
    owners: Owners,
    /// By ticks since `start`; the wait ends at the next one due. Made with the first timer
    /// (`LazySlab`'s reason).
    timers: Option<Wheel<Tag>>,
    watches: LazySlab<fsevents::Watch<Watched>>,
    watch_tags: Map<Tag, WatchId, FixedState, Global>,
    /// Stopped, for the next watch.
    spare: Vec<fsevents::Watch<Watched>>,
    /// The flushes a stopping watch had not told.
    untold: Vec<Tag>,
    spawns: LazySlab<Spawned>,
    spawn_tags: Map<Tag, SpawnId, FixedState, Global>,
    listens: LazySlab<Listening>,
    listen_tags: Map<Tag, ListenId, FixedState, Global>,
    connecting: LazySlab<Connecting>,
    conns: LazySlab<Connection>,
    /// The file locks of [`Effect::Lock`], each taken as it is asked for.
    locks: crate::locks::Locks,
    /// The [`Effect::Pressure`] in flight and its registration's id: one a kqueue, since the
    /// kernel keys a knote by its filter and ident, and a second `EV_ADD` would take the first's.
    pressure: Option<(Tag, u64)>,
    in_flight: usize,
    /// Whether a [`crate::Waker`] was handed out ([`Driver::waker`]): a poll with nothing in
    /// flight then parks for its wake, to its deadline, rather than returning at once (which
    /// turned a caller's loop into a spin). Read and written by the polling thread alone, so
    /// relaxed; an atomic so the driver's auto traits do not change with it.
    waker_given: std::sync::atomic::AtomicBool,
    /// What the watches' callbacks allocated beyond their payloads, as each watch stopped.
    #[cfg(test)]
    watch_overhead: u64,
    /// The descriptors the file threads keep for reads into memory, and under
    /// [`opening::Kept`] for maps, write-backs and queued reads: a file opened once.
    descriptors: Arc<Descriptors>,
    /// The opens by name of the effects a kept descriptor could have served.
    opened_by_name: Arc<OpenCount>,
    _opening: core::marker::PhantomData<O>,
    /// The class and relative priority of the thread that made the driver: every file thread's
    /// own, set at its start whatever thread started it (one started inside a [`Lowered`]
    /// stretch would otherwise keep that class for the driver's life); each effect then runs at
    /// most at its asker's ([`Queued::asker`]).
    class: (Qos, i32),
    /// The file locks this driver holds now ([`Effect::Lock`] answered `Locked`, not yet
    /// unlocked). While any is held, a file effect runs at the driver's class whatever its
    /// asker's: `flock` lends no priority to its holder, so a holder lowered after its answer
    /// (the store's flip, whose writes and barriers are asked one at a time under the lock)
    /// would keep another process's commit, at its command's class, waiting on throttled IO.
    /// Never above the driver's class.
    locks_held: usize,
}

impl Kqueue {
    /// A driver whose files are under `root`, with `threads` threads a lane for the file calls.
    pub fn new(root: impl Into<PathBuf>, threads: usize) -> io::Result<Kqueue> {
        Self::made(root, threads)
    }
}

impl<S: Split, O: Opening> Kqueue<S, O> {
    /// How many files the threads opened through the kept descriptors, and closed.
    pub fn opens(&self) -> crate::Opens {
        self.descriptors.opens()
    }

    /// How many files the threads opened by name for an effect a kept descriptor could serve
    /// (crate::threads::opening): zero under [`opening::Kept`].
    pub fn opens_by_name(&self) -> u64 {
        self.opened_by_name.by_name()
    }

    /// Each lane's waits in its queue, and the longest wait of a fast effect behind a device one
    /// (crate::threads::lanes).
    pub fn lanes(&self) -> lanes::Waits {
        self.dispatch.waits()
    }

    /// [`Kqueue::new`] of the lanes `S` and the opening `O`: for a choice not the default (the
    /// tests' controls).
    pub fn made(root: impl Into<PathBuf>, threads: usize) -> io::Result<Kqueue<S, O>> {
        let root: Arc<Path> = root.into().into();
        let kq = sys_io::kqueue()
            .map_err(|errno| io::Error::from_raw_os_error(i32::from(errno.raw())))?;
        // SAFETY: into_raw transfers sole ownership of the new kqueue descriptor.
        let kq = unsafe { OwnedFd::from_raw_fd(kq.into_raw()) };
        net::set_cloexec(kq.as_raw_fd())?;
        // Each lane's room at its threads; one lane's driver makes the device lane's channel of
        // one slot, which nothing is ever sent through.
        let room = [FILES, if S::TWO { FILES } else { 1 }];
        // alloc: setup, once per driver: each channel's slots.
        let (files_done, files) = crate::chan::bounded(room[0] + room[1]);
        let (watched_done, watched) = crate::chan::bounded(WATCHED); // alloc: as above.
        let (fast_work, fast_queue) = crate::chan::bounded::<Queued>(room[0]); // alloc: as above.
        let (device_work, device_queue) = crate::chan::bounded::<Queued>(room[1]); // alloc: as above.
        let limits = |at_threads| Limits { threads: threads.max(1), at_threads, backlog: BACKLOG };
        let notify = Arc::new(Notify {
            kq,
            files: files_done,
            watched: watched_done,
            woke: Wake::new(),
            asked: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            file_allocations: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            triggers: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            ran_at: std::sync::atomic::AtomicU32::new(0),
        });
        notify.change(
            TRIGGER as usize,
            Filter::USER.0,
            (KeventFlags::ADD | KeventFlags::CLEAR).bits(),
            0,
            0,
            TRIGGER,
        )?;
        // alloc: setup, once per driver: the host's channel, which grows with what it tells
        // between two polls.
        let (host_done, hosted) = crate::chan::unbounded();
        // clone: setup, a count of the kqueue for the host's wakes.
        let sink = HostSink { done: host_done, notify: Arc::clone(&notify) };
        let host = crate::host::Host::new(Arc::new(sink)); // alloc: setup, once per driver.
        // alloc: setup, once per driver: the table the file threads share.
        let descriptors = Arc::new(Descriptors::new());
        // No file thread yet: each is started when a file effect finds every one of its lane
        // busy ([`Kqueue::to_threads`]), up to `threads` a lane.
        // alloc: setup, once per driver: the handles, at their most.
        let workers = Vec::with_capacity(threads.max(1) * if S::TWO { 2 } else { 1 });
        // Each table of tags, the backlog, the spares and the output buffer are empty until
        // their first use, then made at their usual most at once (`LazySlab`'s reason: a
        // program that never watches, spawns or listens dirties none of their pages; measured
        // 2026-11-16, hello's footprint at exit).
        fn tags<V>() -> Map<Tag, V, FixedState, Global> {
            Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None })
        }
        let spawn_dirs = root.join(".blaze-spawn"); // alloc: setup, once per driver.
        Ok(Kqueue {
            start: Instant::now(),
            boot: crate::boot::system(),
            root,
            notify,
            files,
            watched,
            hosted,
            host,
            work: Some([fast_work, device_work]),
            backlog: [VecDeque::new(), VecDeque::new()],
            dispatch: Dispatch::new([limits(room[0]), limits(room[1])]),
            workers,
            queues: [fast_queue, device_queue],
            confine: None,
            spawn_dirs,
            buffer: Vec::new(),
            // alloc: setup, once per driver: the lists of a wait at their usual most.
            changes: Vec::with_capacity(EVENTS),
            events: Vec::with_capacity(2 * EVENTS), // alloc: as above.
            ready: Vec::with_capacity(EVENTS),      // alloc: as above.
            owners: Owners::new().map_err(no_slab)?,
            timers: None,
            watches: LazySlab::new(),
            watch_tags: tags(),
            spare: Vec::new(),
            untold: Vec::with_capacity(16),           // alloc: setup, once per driver.
            spawns: LazySlab::new(),
            spawn_tags: tags(),
            listens: LazySlab::new(),
            listen_tags: tags(),
            connecting: LazySlab::new(),
            conns: LazySlab::new(),
            locks: crate::locks::Locks::new(),
            pressure: None,
            in_flight: 0,
            waker_given: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            watch_overhead: 0,
            descriptors,
            // alloc: setup, once per driver: the count the file threads share.
            opened_by_name: Arc::new(OpenCount::default()),
            _opening: core::marker::PhantomData,
            class: qos::this_thread()?,
            locks_held: 0,
        })
    }

    /// The driver, starting each process whose sandbox is on as `confine` makes it.
    // dyn: the platform's sandbox, given once at setup
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Self {
        self.confine = Some(confine);
        self
    }

    /// The driver, making each sandboxed spawn's own directory under `dir`, made here.
    pub fn scratch(mut self, dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        self.spawn_dirs = dir;
        Ok(self)
    }

    /// A change made with the next wait; a failure of it comes back as an event of `id`'s with
    /// `EV_ERROR`.
    fn change(&mut self, ident: usize, filter: i16, flags: u16, fflags: u32, data: isize, id: u64) {
        self.changes.push(Kevent { ident, filter, flags, fflags, data, udata: id as usize });
    }

    /// `ident` of `filter` registered as `owner`'s, with the next wait.
    fn register(
        &mut self,
        owner: Owner,
        ident: usize,
        filter: i16,
        flags: u16,
        fflags: u32,
    ) -> u64 {
        let id = self.owners.own(owner);
        self.change(ident, filter, flags, fflags, 0, id);
        id
    }

    /// `id` ends: no event of it is acted on, and a change of it not yet made is dropped, so it
    /// cannot land on a descriptor closed and reused since.
    fn disown(&mut self, id: u64) {
        self.owners.disown(id);
        self.changes.retain(|it| it.udata != id as usize);
    }

    fn begin(&mut self, effect: Effect) -> io::Result<()> {
        match effect {
            // A read into memory of the asker's, or a write from it, is a `pread` or `pwrite` on
            // a file thread (`threads::run`), a read on the file's kept descriptor (no open
            // after the first) and a batch one `preadv` a run, `F_NOCACHE` where it bypasses the cache and at
            // `IOPOL_THROTTLE` where it is idle; the store's pool gives its driver threads
            // enough to keep a drive's queue deep (docs/v2/plan/design/store-pool.md).
            effect @ (Effect::ReadAll { .. }
            | Effect::Read { .. }
            | Effect::ReadInto { .. }
            | Effect::ReadMany { .. }
            | Effect::WriteFrom { .. }
            | Effect::ReadLink { .. }
            | Effect::Append { .. }
            | Effect::Sync { .. }
            | Effect::Barrier { .. }
            | Effect::WriteBack { .. }
            | Effect::SyncDir { .. }
            | Effect::Truncate { .. }
            | Effect::Write { .. }
            | Effect::WriteUnsynced { .. }
            | Effect::WriteAt { .. }
            | Effect::Rename { .. }
            | Effect::RenameUnsynced { .. }
            | Effect::Mode { .. }
            | Effect::Seal { .. }
            | Effect::Private { .. }
            | Effect::Times { .. }
            | Effect::Clone { .. }
            | Effect::CopyData { .. }
            | Effect::Prewarm { .. }
            | Effect::Link { .. }
            | Effect::Stat { .. }
            | Effect::Lstat { .. }
            | Effect::List { .. }
            | Effect::Remove { .. }
            | Effect::MakeDir { .. }
            | Effect::RemoveDir { .. }
            | Effect::Ancestors { .. }
            | Effect::Above { .. }
            | Effect::Stamp { .. }
            | Effect::Entries { .. }
            | Effect::Map { .. }
            | Effect::MapWrite { .. }) => self.to_lane(effect),
            // A lock never waits, so it is taken here rather than on the threads.
            Effect::Lock { file, mode, tag } => {
                let done = self.locks.lock(&self.root, &file, mode, tag)?;
                if done == Done::Locked {
                    self.locks_held += 1;
                }
                self.ready.push(Completion { tag, done });
                Ok(())
            }
            Effect::Unlock { lock, tag } => {
                let done = self.locks.unlock(lock)?;
                if done == Done::Unlocked {
                    // Only a held lock unlocks, and each was counted at its `Locked`.
                    self.locks_held -= 1;
                }
                self.ready.push(Completion { tag, done });
                Ok(())
            }
            Effect::Timer { after, tag } => {
                let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                if self.timers.is_none() {
                    self.timers = Some(Wheel::try_new(wheel::MAX_TIMERS).map_err(no_slab)?);
                }
                self.timers.as_mut().expect("made just above").insert(due, tag);
                Ok(())
            }
            Effect::Watch { dir, tag } => self.watch(DirName::Named(dir), None, tag),
            Effect::Journal { dir, since, tag } => self.watch(dir, Some(since), tag),
            // Told through the watches' channel by the stream, after the changes it heard first.
            Effect::Flush { watch, tag } => {
                match self.watch_tags.get(&watch).and_then(|&id| self.watches.get(id)) {
                    Some(stream) => stream.flush(tag).map_err(|error| {
                        // alloc: an error path.
                        io::Error::new(error.kind(), format!("flushing the watch: {error}"))
                    }),
                    // alloc: an error path.
                    None => Err(io::Error::other(format!("nothing is watched as {watch:?}"))),
                }
            }
            Effect::Spawn { command, tag } => self.spawn(command, tag),
            Effect::Listen { at, options, tag } => self.listen(&at, &options, tag),
            Effect::Connect { at, options, tag } => self.connect(at, &options, tag),
            Effect::Recv { conn, most, into, tag } => {
                match self.connection(conn) {
                    Ok(connection) => connection.recvs.push_back((tag, most, into)),
                    Err(error) => {
                        self.ready.push(Completion { tag, done: Done::unread(into, error) })
                    }
                }
                self.serve_recvs(conn_id(conn));
                Ok(())
            }
            Effect::Send { conn, bytes, tag } => {
                match self.connection(conn) {
                    Ok(connection) => connection.sends.push_back((tag, bytes, 0)),
                    Err(error) => {
                        self.ready.push(Completion { tag, done: Done::unsent(bytes, error) })
                    }
                }
                self.serve_sends(conn_id(conn));
                Ok(())
            }
            Effect::Hangup { conn, tag } => self.hangup(conn, tag),
            Effect::Pressure { tag } => {
                if let Some((watching, _)) = self.pressure {
                    return Err(io::Error::other(format!(
                        "memory pressure is watched as {watching:?} already, and a kqueue has one \
                         knote of EVFILT_MEMORYSTATUS"
                    ))); // alloc: an error path.
                }
                let id = self.register(
                    Owner::Pressure(tag),
                    0,
                    EVFILT_MEMORYSTATUS,
                    (KeventFlags::ADD | KeventFlags::CLEAR).bits(),
                    NOTE_MEMORYSTATUS_PRESSURE_WARN | NOTE_MEMORYSTATUS_PRESSURE_CRITICAL,
                );
                self.pressure = Some((tag, id));
                Ok(())
            }
            // Told through the host's channel.
            effect @ (Effect::Bind { .. }
            | Effect::SendTo { .. }
            | Effect::RecvFrom { .. }
            | Effect::Unbind { .. }
            | Effect::Resolve { .. }
            | Effect::ReadStdin { .. }
            | Effect::WriteOut { .. }
            | Effect::Now { .. }
            | Effect::Random { .. }
            | Effect::Roots { .. }) => {
                self.host.submit(effect);
                Ok(())
            }
            Effect::Kill { .. } | Effect::Unwatch { .. } | Effect::Unlisten { .. } => {
                unreachable!("taken where they are submitted")
            }
        }
    }

    /// A file effect into its lane (crate::threads::lanes `lane_of`, then `S`): onto the lane's
    /// threads where they have room and its backlog is empty, else into its backlog, else
    /// refused naming the lane and its caps.
    fn to_lane(&mut self, effect: Effect) -> io::Result<()> {
        let class = lanes::lane_of(&effect).expect("a file effect, which a lane's threads run");
        // Under a held lock, the driver's class ([`Kqueue::locks_held`]).
        let asker = if self.locks_held > 0 { self.class.0 } else { qos::this_thread()?.0 };
        let (ticket, placed) = self.dispatch.place(class).map_err(io::Error::other)?;
        let queued = Queued { effect, ticket, asked: self.start.elapsed(), asker };
        match placed {
            Placed::Threads { start } => self.to_threads(queued, start),
            Placed::Backlog => {
                let backlog = &mut self.backlog[ticket.lane.index()];
                if backlog.capacity() == 0 {
                    // alloc: once per driver and lane, at its first effect beyond its threads.
                    backlog.reserve(FILES);
                }
                backlog.push_back(queued);
                Ok(())
            }
        }
    }

    /// A file effect onto its lane's queue, which has room (the dispatcher's word): at most
    /// [`FILES`] are there. With `start` (every thread of the lane started so far is busy, and
    /// fewer than its cap are started) one more is started first, so a driver that is asked no
    /// file call starts no thread and one asked one at a time starts one a lane. Every thread,
    /// of either lane, runs at the driver's class ([`Kqueue::class`]). A thread the system
    /// refuses fails this effect, naming it; the effect is not queued.
    fn to_threads(&mut self, queued: Queued, start: bool) -> io::Result<()> {
        let lane = queued.ticket.lane;
        if start {
            let n = self.dispatch.started(lane) - 1;
            // clone: one count of each per file thread, at its start.
            let (root, queue, notify) = (Arc::clone(&self.root), self.queues[lane.index()].share(), Arc::clone(&self.notify));
            let kept = Arc::clone(&self.descriptors); // clone: as above.
            let count = Arc::clone(&self.opened_by_name); // clone: as above.
            let (clock, class) = (self.clock(), self.class);
            let started = std::thread::Builder::new()
                // alloc: once per thread.
                .name(match lane {
                    Lane::Fast => format!("blaze-kqueue-{n}"),
                    Lane::Device => format!("blaze-kqueue-device-{n}"),
                })
                .spawn(move || work_on::<O>(lane, class, &root, &kept, &count, &queue, &notify, &clock))
                .map_err(|error| {
                    // alloc: an error path.
                    io::Error::new(error.kind(), format!("file thread {n} of the kqueue driver's {} lane: {error}", lane.name()))
                });
            match started {
                Ok(started) => self.workers.push(started),
                Err(error) => {
                    self.dispatch.not_started(queued.ticket);
                    return Err(error);
                }
            }
        }
        let work = self.work.as_ref().expect("the driver is running");
        if work[lane.index()].try_send(queued).is_err() {
            unreachable!("room for a file effect at the {} lane's threads, which are running (the dispatcher's count)", lane.name());
        }
        Ok(())
    }

    fn connection(&mut self, conn: Conn) -> io::Result<&mut Connection> {
        self.conns.get_mut(conn_id(conn)).ok_or_else(|| no_connection(conn))
    }

    /// Watches `dir` as `tag`, a journal of it from `since`: on a stopped watch kept from an
    /// earlier one where there is one. A watch of `tag` already ends first, telling nothing
    /// more.
    fn watch(&mut self, dir: DirName, since: Option<Cursor>, tag: Tag) -> io::Result<()> {
        if let Some(old) = self.watch_tags.remove(&tag) {
            self.retire(old);
        }
        let watch = match self.spare.pop() {
            Some(watch) => watch,
            // clone: one count per watch made, only when no spare one is left; never per event.
            None => fsevents::Watch::new(Watched(Arc::clone(&self.notify)), &self.root)?,
        };
        let id = self.watches.insert(watch);
        let watch = self.watches.get_mut(id).expect("inserted above");
        let mut first = None;
        let started = match since {
            None => watch.start(&self.root, dir, tag),
            Some(since) => watch.journal(&self.root, dir, since, tag, |it| first = Some(it)),
        };
        match started {
            Ok(()) => {
                if let Some(first) = first {
                    // alloc: the batch's payload.
                    let done = Done::Journaled(Box::new(first));
                    self.ready.push(Completion { tag, done });
                }
                first_use(&mut self.watch_tags).insert(tag, id);
                Ok(())
            }
            Err(error) => {
                let watch = self.watches.remove(id).expect("inserted above");
                self.keep(watch);
                Err(error)
            }
        }
    }

    /// Stops the watch `id` and takes it out: what it told is moved to `ready` first, then
    /// each flush it had not told, failed.
    fn retire(&mut self, id: WatchId) {
        let Some(watch) = self.watches.get_mut(id) else { return };
        let untold = &mut self.untold;
        watch.stop(|tag| untold.push(tag));
        #[cfg(test)]
        {
            self.watch_overhead += watch.take_overhead();
        }
        while let Ok(completion) = self.watched.try_recv() {
            self.ready.push(completion);
        }
        for tag in self.untold.drain(..) {
            // alloc: an error path, a flush its watch ended before.
            let why = IoError("the watch ended before the flush".into());
            self.ready.push(Completion { tag, done: Done::Failed(why) });
        }
        let watch = self.watches.remove(id).expect("looked up above");
        self.keep(watch);
    }

    /// A stopped watch kept for the next, up to [`SPARE_WATCHES`].
    fn keep(&mut self, watch: fsevents::Watch<Watched>) {
        debug_assert!(!watch.is_running());
        if self.spare.len() < SPARE_WATCHES {
            // alloc: once per driver, at the first watch stopped.
            self.spare.reserve_exact(SPARE_WATCHES);
            self.spare.push(watch);
        }
    }

    fn spawn(&mut self, command: Box<Command>, tag: Tag) -> io::Result<()> {
        // A sandbox's scratch folder is named in its profile, which takes absolute paths; a
        // driver over `.` (a program's start folder, not read at start) makes it absolute here,
        // at the first spawn that could need it, once.
        if self.spawn_dirs.is_relative() {
            self.spawn_dirs = std::path::absolute(&self.spawn_dirs)?; // alloc: once per driver.
        }
        let process::Started { pid, stdin, stdout, stderr, finish } =
            process::start(&self.spawn_dirs, command, self.confine.as_ref())?;
        // Reaped only as this driver's loop polls its exit: a stall report names the tag.
        finish.waited_by(tag);
        let spawn = self.spawns.insert(Spawned {
            tag,
            pid,
            stdin,
            written: 0,
            stdout: Some(stdout),
            stderr: Some(stderr),
            out: Vec::new(), // alloc: none until it writes, the exit's payload.
            err: Vec::new(), // alloc: as above.
            exited: false,
            failed: None,
            finish,
            ids: Ids::default(),
        });
        match self.follow(spawn) {
            Ok(()) => {
                // A spawn of `tag` already runs on, followed by its own handle.
                first_use(&mut self.spawn_tags).insert(tag, spawn);
                Ok(())
            }
            Err(error) => {
                let spawned = self.spawns.remove(spawn).expect("inserted above");
                for &id in spawned.ids.as_slice() {
                    self.disown(id);
                }
                // alloc: an error path.
                let program = spawned.finish.command.program.display().to_string();
                abandon(spawned);
                // alloc: an error path.
                Err(io::Error::new(error.kind(), format!("following {program}: {error}")))
            }
        }
    }

    /// The process's pipes and its end registered on the kqueue.
    fn follow(&mut self, spawn: SpawnId) -> io::Result<()> {
        let spawned = self.spawns.get(spawn).expect("followed as it is made");
        let pipe = |it: &Option<OwnedFd>| it.as_ref().expect("a pipe just made").as_raw_fd();
        let outputs = [(pipe(&spawned.stdout), false), (pipe(&spawned.stderr), true)];
        let stdin = spawned.stdin.as_ref().map(|it| it.as_raw_fd());
        let pid = spawned.pid;
        for (fd, err) in outputs {
            set_nonblocking(fd)?;
            let owner = Owner::Output { spawn, err };
            let id = self.register(owner, fd as usize, Filter::READ.0, KeventFlags::ADD.bits(), 0);
            self.spawns.get_mut(spawn).expect("followed as it is made").ids.push(id);
        }
        if let Some(fd) = stdin {
            set_nonblocking(fd)?;
            let id = self.register(
                Owner::Input(spawn),
                fd as usize,
                Filter::WRITE.0,
                KeventFlags::ADD.bits(),
                0,
            );
            self.spawns.get_mut(spawn).expect("followed as it is made").ids.push(id);
        }
        let (pid, flags) = (pid as usize, (KeventFlags::ADD | KeventFlags::ONESHOT).bits());
        let id = self.register(Owner::Exit(spawn), pid, Filter::PROC.0, flags, NoteFlags::EXIT.bits());
        self.spawns.get_mut(spawn).expect("followed as it is made").ids.push(id);
        Ok(())
    }

    fn read_output(&mut self, spawn: SpawnId, err: bool) {
        let Some(spawned) = self.spawns.get_mut(spawn) else { return };
        let (pipe, into) = match err {
            true => (&mut spawned.stderr, &mut spawned.err),
            false => (&mut spawned.stdout, &mut spawned.out),
        };
        let Some(fd) = pipe.as_ref().map(|it| it.as_raw_fd()) else { return };
        if self.buffer.is_empty() {
            // alloc: once per driver, at the first output read.
            self.buffer.resize(64 << 10, 0);
        }
        loop {
            match sys_io::read(Fd::new(fd), &mut self.buffer) {
                Ok(0) => {
                    *pipe = None;
                    break;
                }
                Ok(n) => into.extend_from_slice(&self.buffer[..n]),
                Err(errno) => {
                    let error = io::Error::from_raw_os_error(i32::from(errno.raw()));
                    match error.kind() {
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::WouldBlock => break,
                        _ => {
                            spawned.failed.get_or_insert(error);
                            *pipe = None;
                            break;
                        }
                    }
                }
            }
        }
        self.finish_spawn(spawn);
    }

    fn write_input(&mut self, spawn: SpawnId) {
        let Some(spawned) = self.spawns.get_mut(spawn) else { return };
        let Some(fd) = spawned.stdin.as_ref().map(|it| it.as_raw_fd()) else { return };
        let input = &spawned.finish.command.stdin;
        while spawned.written < input.len() {
            let rest = &input[spawned.written..];
            match sys_io::write(Fd::new(fd), rest) {
                Ok(0) => {
                    spawned.failed.get_or_insert(io::ErrorKind::WriteZero.into());
                    break;
                }
                Ok(n) => spawned.written += n,
                Err(errno) => {
                    let error = io::Error::from_raw_os_error(i32::from(errno.raw()));
                    match error.kind() {
                        io::ErrorKind::Interrupted => {}
                        io::ErrorKind::WouldBlock => return,
                        // It stopped reading: what it did not read it was not given, as with
                        // any pipe.
                        io::ErrorKind::BrokenPipe => break,
                        _ => {
                            spawned.failed.get_or_insert(error);
                            break;
                        }
                    }
                }
            }
        }
        // Closed: the end of its input.
        spawned.stdin = None;
    }

    /// Ends the spawn once it has exited and its output has ended.
    fn finish_spawn(&mut self, spawn: SpawnId) {
        match self.spawns.get(spawn) {
            Some(it) if it.exited && it.stdout.is_none() && it.stderr.is_none() => {}
            _ => return,
        }
        let spawned = self.spawns.remove(spawn).expect("looked up above");
        if self.spawn_tags.get(&spawned.tag) == Some(&spawn) {
            self.spawn_tags.remove(&spawned.tag);
        }
        for &id in spawned.ids.as_slice() {
            self.disown(id);
        }
        let Spawned { tag, pid, out, err, failed, finish, .. } = spawned;
        // alloc: an error path; `exited` names the program in its own errors.
        let failed = failed.map(|error| format!("{}: {error}", finish.command.program.display()));
        let ended = finish.exited(pid, out, err);
        let done = match (ended, failed) {
            // alloc: the exit's payload.
            (Ok(exit), None) => Done::Exited(Box::new(exit)),
            // alloc: an error path.
            (Err(error), _) => Done::Failed(IoError(error.to_string())),
            (Ok(_), Some(why)) => Done::Failed(IoError(why)),
        };
        self.ready.push(Completion { tag, done });
    }

    fn listen(&mut self, at: &Address, options: &crate::sockopt::Tcp, tag: Tag) -> io::Result<()> {
        let context =
            // alloc: an error path.
            |error: io::Error| io::Error::new(error.kind(), format!("listening at {at:?}: {error}"));
        let listener = net::listen(&self.root, at, options).map_err(context)?;
        listener.set_nonblocking(true).map_err(context)?;
        let fd = listener.fd() as usize;
        let listen = self.listens.insert(Listening { tag, listener, id: TRIGGER });
        let id = self.register(Owner::Listen(listen), fd, Filter::READ.0, KeventFlags::ADD.bits(), 0);
        self.listens.get_mut(listen).expect("inserted above").id = id;
        // A listen of `tag` already ends, as the one it is replaced by begins.
        if let Some(old) = first_use(&mut self.listen_tags).insert(tag, listen)
            && let Some(old) = self.listens.remove(old)
        {
            self.disown(old.id);
        }
        Ok(())
    }

    fn accept(&mut self, listen: ListenId) {
        let fail = |this: &mut Self, tag: Tag, error: io::Error| {
            // A failure ends the listen.
            this.end_listen(listen);
            // alloc: an error path.
            let done = Done::Failed(IoError(format!("accepting a connection: {error}")));
            this.ready.push(Completion { tag, done });
        };
        loop {
            let Some(listening) = self.listens.get(listen) else { return };
            let tag = listening.tag;
            match listening.listener.accept() {
                Ok(socket) => {
                    // Only peer-gone errors during accepted-socket setup (including macOS
                    // returning EINVAL from SO_NOSIGPIPE when a probing client closed immediately
                    // after connect()) drop that socket alone; genuine OS setup failures end the
                    // listen via Done::Failed.
                    let setup = match socket.set_nonblocking(true) {
                        Ok(()) => no_sigpipe(socket.fd()).map_err(|error| (error, true)),
                        Err(error) => Err((error, false)),
                    };
                    match setup {
                        Ok(()) => {
                            let conn = self.adopt(socket);
                            self.ready.push(Completion { tag, done: Done::Accepted { conn } });
                        }
                        Err((error, allow_einval)) if accept_setup_peer_gone(&error, allow_einval) => {}
                        Err((error, _)) => {
                            fail(self, tag, error);
                            return;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(error) => {
                    fail(self, tag, error);
                    return;
                }
            }
        }
    }

    /// The listen `listen` taken out, its registration and its tag's name of it with it.
    fn end_listen(&mut self, listen: ListenId) -> Option<Listening> {
        let listening = self.listens.remove(listen)?;
        self.disown(listening.id);
        if self.listen_tags.get(&listening.tag) == Some(&listen) {
            self.listen_tags.remove(&listening.tag);
        }
        Some(listening)
    }

    fn adopt(&mut self, socket: net::Socket) -> Conn {
        let conn = self.conns.insert(Connection {
            socket,
            recvs: VecDeque::new(),
            sends: VecDeque::new(),
            read: TRIGGER,
            write: TRIGGER,
        });
        let read = self.owners.own(Owner::Read(conn));
        let write = self.owners.own(Owner::Write(conn));
        let connection = self.conns.get_mut(conn).expect("inserted above");
        connection.read = read;
        connection.write = write;
        Conn(conn.to_bits())
    }

    fn connect(&mut self, at: Address, options: &crate::sockopt::Tcp, tag: Tag) -> io::Result<()> {
        let context =
            // alloc: an error path.
            |error: io::Error| io::Error::new(error.kind(), format!("connecting to {at:?}: {error}"));
        let address = net::sockaddr(&self.root, &at).map_err(context)?;
        let fd = net::stream_socket(address.domain).map_err(context)?;
        crate::sockopt::for_stream(fd.as_raw_fd(), address.domain, options, crate::sockopt::Side::Connect)
            .map_err(context)?;
        set_nonblocking(fd.as_raw_fd()).map_err(context)?;
        no_sigpipe(fd.as_raw_fd()).map_err(context)?;
        let unix = address.domain == sys_net::AF_UNIX;
        match sys_net::connect(Fd::new(fd.as_raw_fd()), &address.storage, address.len) {
            Ok(()) => return self.connected(tag, fd, unix).map_err(context),
            Err(Errno::InProgress | Errno::Intr) => {}
            Err(errno) => return Err(context(io::Error::from_raw_os_error(i32::from(errno.raw())))),
        }
        let ident = fd.as_raw_fd() as usize;
        let flags = (KeventFlags::ADD | KeventFlags::ONESHOT).bits();
        let connecting = self.connecting.insert(Connecting { tag, fd, at, unix });
        self.register(Owner::Connecting(connecting), ident, Filter::WRITE.0, flags, 0);
        Ok(())
    }

    /// A connect in flight has ended, made or not.
    fn ended_connecting(&mut self, connecting: ConnectId) {
        let Some(Connecting { tag, fd, at, unix }) = self.connecting.remove(connecting) else {
            return;
        };
        let made = socket_error(fd.as_raw_fd()).and_then(|()| self.connected(tag, fd, unix));
        if let Err(error) = made {
            // alloc: an error path.
            let done = Done::Failed(IoError(format!("connecting to {at:?}: {error}")));
            self.ready.push(Completion { tag, done });
        }
    }

    fn connected(&mut self, tag: Tag, fd: OwnedFd, unix: bool) -> io::Result<()> {
        // Its options were set before it connected (`connect`): setting no-delay here would
        // undo a caller's `no_delay: Must(false)`.
        let socket = net::Socket::from_fd(fd, unix);
        let conn = self.adopt(socket);
        self.ready.push(Completion { tag, done: Done::Connected { conn } });
        Ok(())
    }

    fn serve_recvs(&mut self, conn: ConnId) {
        let Some(connection) = self.conns.get_mut(conn) else { return };
        while let Some((tag, most, into)) = connection.recvs.front_mut() {
            let tag = *tag;
            // Straight into the asker's buffer, which keeps its room for the next receive.
            let done = match connection.socket.read_into(into, *most) {
                Ok(_) => Done::Read(std::mem::take(into)),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.changes.push(Kevent {
                        ident: connection.socket.fd() as usize,
                        filter: Filter::READ.0,
                        flags: (KeventFlags::ADD | KeventFlags::ONESHOT).bits(),
                        fflags: 0,
                        data: 0,
                        udata: connection.read as usize,
                    });
                    return;
                }
                Err(error) => Done::unread(std::mem::take(into), error),
            };
            connection.recvs.pop_front();
            self.ready.push(Completion { tag, done });
        }
    }

    fn serve_sends(&mut self, conn: ConnId) {
        let Some(connection) = self.conns.get_mut(conn) else { return };
        while let Some((tag, bytes, put)) = connection.sends.front_mut() {
            let tag = *tag;
            let done = match *put == bytes.len() {
                true => {
                    bytes.clear();
                    Done::Sent(std::mem::take(bytes))
                }
                false => match connection.socket.write(&bytes[*put..]) {
                    Ok(0) => {
                        Done::unsent(std::mem::take(bytes), "the connection took no more bytes")
                    }
                    Ok(n) => {
                        *put += n;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.changes.push(Kevent {
                            ident: connection.socket.fd() as usize,
                            filter: Filter::WRITE.0,
                            flags: (KeventFlags::ADD | KeventFlags::ONESHOT).bits(),
                            fflags: 0,
                            data: 0,
                            udata: connection.write as usize,
                        });
                        return;
                    }
                    Err(error) => Done::unsent(std::mem::take(bytes), error),
                },
            };
            connection.sends.pop_front();
            self.ready.push(Completion { tag, done });
        }
    }

    fn hangup(&mut self, conn: Conn, tag: Tag) -> io::Result<()> {
        let connection = self.conns.remove(conn_id(conn)).ok_or_else(|| no_connection(conn))?;
        self.disown(connection.read);
        self.disown(connection.write);
        let shut = connection.socket.shutdown();
        for (recv, _, into) in connection.recvs {
            self.ready.push(Completion { tag: recv, done: Done::Read(into) });
        }
        for (send, bytes, _) in connection.sends {
            // alloc: an error path.
            let done =
                Done::unsent(bytes, format_args!("{conn:?} hung up before all of it was sent"));
            self.ready.push(Completion { tag: send, done });
        }
        shut?;
        self.ready.push(Completion { tag, done: Done::HungUp });
        Ok(())
    }

    /// Makes the changes asked for and waits until `until` (forever for `None`), or until the
    /// next timer is due, for events, and acts on them; then fires the timers due.
    fn wait(&mut self, until: Option<Deadline>) {
        let timer = self.timers.as_mut().and_then(Wheel::next_deadline).map(|due| Deadline::at(Duration::from_nanos(due)));
        let until = match (until, timer) {
            (Some(until), Some(timer)) => Some(until.min(timer)),
            (until, timer) => until.or(timer),
        };
        self.wait_events(until);
        let ready = &mut self.ready;
        if let Some(timers) = &mut self.timers {
            timers.advance(wheel::ticks(self.start.elapsed()), |_, tag| {
                ready.push(Completion { tag, done: Done::Fired });
            });
        }
    }

    fn wait_events(&mut self, until: Option<Deadline>) {
        let clock = Monotonic::since(self.start);
        let mut changes = std::mem::take(&mut self.changes);
        let mut events = std::mem::take(&mut self.events);
        // Room for a failure of every change besides the events: a failure with no room for it
        // would fail the whole call.
        let room = changes.len() + EVENTS;
        events.clear();
        events.reserve(room);
        // SAFETY: the changes and the room for events laid out as `kevent`s, both kept until
        // the call returns.
        let got = unsafe {
            wait::kevent(
                "the kqueue driver's events",
                self.notify.kq.as_raw_fd(),
                changes.as_ptr().cast(),
                changes.len(),
                events.as_mut_ptr().cast(),
                room,
                until,
                &clock,
            )
        };
        // Made by the first call, before it waited, interrupted or not.
        changes.clear();
        self.changes = changes;
        let got = match got {
            Ok(Waited::Ready(got)) => got,
            Ok(Waited::Deadline) => 0,
            Err(error) => panic!("{error}"),
        };
        // SAFETY: the call wrote `got` events, at most the `room` it was given, which `reserve`
        // made capacity for; a `Kevent` is integers only, so any bytes it wrote are one. What
        // `udata` names is checked in `dispatch` (a slab lookup by index and generation),
        // never followed as a pointer.
        unsafe { events.set_len(got) };
        for &event in &events {
            self.dispatch(event);
        }
        self.events = events;
    }

    /// Acts on one event, by what its `udata` names now: nothing for the trigger, or for a
    /// registration that has ended (a stale handle, its slot free or another's).
    fn dispatch(&mut self, event: Kevent) {
        let id = event.udata as u64;
        if id == TRIGGER {
            return;
        }
        let Some(owner) = self.owners.get(id) else { return };
        if event.flags & KeventFlags::ERROR.bits() != 0 {
            if event.data != 0 {
                self.refused(id, owner, io::Error::from_raw_os_error(event.data as i32));
            }
            return;
        }
        match owner {
            Owner::Exit(spawn) => {
                self.disown(id);
                if let Some(spawned) = self.spawns.get_mut(spawn) {
                    spawned.exited = true;
                    spawned.stdin = None;
                }
                self.finish_spawn(spawn);
            }
            Owner::Output { spawn, err } => self.read_output(spawn, err),
            Owner::Input(spawn) => self.write_input(spawn),
            Owner::Listen(listen) => self.accept(listen),
            Owner::Connecting(connecting) => {
                self.disown(id);
                self.ended_connecting(connecting);
            }
            Owner::Read(conn) => self.serve_recvs(conn),
            Owner::Write(conn) => self.serve_sends(conn),
            Owner::Pressure(tag) => {
                let critical = event.fflags & NOTE_MEMORYSTATUS_PRESSURE_CRITICAL != 0;
                self.ready.push(Completion { tag, done: Done::Pressed { critical } });
            }
        }
    }

    /// The kqueue refused a change of `id`'s.
    fn refused(&mut self, id: u64, owner: Owner, error: io::Error) {
        match owner {
            Owner::Exit(spawn) => {
                self.disown(id);
                let Some(spawned) = self.spawns.get_mut(spawn) else { return };
                // `ESRCH`: exited already, a child not reaped, which the kernel no longer
                // follows. Anything else: its end cannot be heard, so it does not run on.
                if error.raw_os_error() != Some(i32::from(Errno::Srch.raw())) {
                    process::kill(spawned.pid);
                    if let Err(waiting) = process::wait_exited(spawned.pid) {
                        spawned.failed.get_or_insert(waiting);
                    }
                    spawned.failed.get_or_insert(error);
                }
                spawned.exited = true;
                spawned.stdin = None;
                self.finish_spawn(spawn);
            }
            // Its output or input cannot be moved: it does not run on.
            Owner::Output { spawn, err } => {
                let Some(spawned) = self.spawns.get_mut(spawn) else { return };
                process::kill(spawned.pid);
                spawned.failed.get_or_insert(error);
                match err {
                    true => spawned.stderr = None,
                    false => spawned.stdout = None,
                }
                self.finish_spawn(spawn);
            }
            Owner::Input(spawn) => {
                let Some(spawned) = self.spawns.get_mut(spawn) else { return };
                process::kill(spawned.pid);
                spawned.failed.get_or_insert(error);
                spawned.stdin = None;
            }
            Owner::Listen(listen) => {
                let Some(listening) = self.end_listen(listen) else { return };
                // alloc: an error path.
                let done = Done::Failed(IoError(format!("listening: {error}")));
                self.ready.push(Completion { tag: listening.tag, done });
            }
            Owner::Connecting(connecting) => {
                self.disown(id);
                let Some(connecting) = self.connecting.remove(connecting) else { return };
                // alloc: an error path.
                let done =
                    Done::Failed(IoError(format!("connecting to {:?}: {error}", connecting.at)));
                self.ready.push(Completion { tag: connecting.tag, done });
            }
            Owner::Read(conn) => {
                let Some(connection) = self.conns.get_mut(conn) else { return };
                if let Some((tag, _, into)) = connection.recvs.pop_front() {
                    self.ready.push(Completion { tag, done: Done::unread(into, &error) });
                }
                self.serve_recvs(conn);
            }
            Owner::Write(conn) => {
                let Some(connection) = self.conns.get_mut(conn) else { return };
                if let Some((tag, bytes, _)) = connection.sends.pop_front() {
                    self.ready.push(Completion { tag, done: Done::unsent(bytes, &error) });
                }
                self.serve_sends(conn);
            }
            Owner::Pressure(tag) => {
                self.disown(id);
                self.pressure = None;
                // alloc: an error path.
                let done = Done::Failed(IoError(format!(
                    "watching the system's memory pressure (a kqueue knote of \
                     EVFILT_MEMORYSTATUS): {error}"
                )));
                self.ready.push(Completion { tag, done });
            }
        }
    }

    /// Moves every completion ready into `out`: the polling thread's, then the file threads'
    /// (each making room at the threads for one from the backlog), then the watches'.
    fn take(&mut self, out: &mut Vec<Completion>) {
        out.append(&mut self.ready);
        // Cleared before the channels are read: a hand-back that finds it set was made before
        // this, so the reads below see it (`Wake`, model-checked in `handoff`).
        self.notify.woke.drain();
        while let Ok(Finished { completion, ticket, waited_ns }) = self.files.try_recv() {
            self.dispatch.ended(ticket, waited_ns);
            out.push(completion);
        }
        for lane in Lane::ALL {
            while let Some(start) = self.dispatch.promote(lane) {
                let queued = self.backlog[lane.index()].pop_front().expect("the dispatcher's backlog count is the driver's");
                let tag = queued.effect.tag();
                if let Err(error) = self.to_threads(queued, start) {
                    // alloc: an error path.
                    out.push(Completion { tag, done: Done::Failed(IoError(error.to_string())) });
                }
            }
        }
        while let Ok(completion) = self.watched.try_recv() {
            out.push(completion);
        }
        while let Ok(completion) = self.hosted.try_recv() {
            out.push(completion);
        }
    }
}

impl<S: Split, O: Opening> Drop for Kqueue<S, O> {
    fn drop(&mut self) {
        for (_, watch) in self.watches.iter_mut() {
            watch.stop(|_| {});
        }
        for (_, spawn) in self.spawn_tags.drain() {
            if let Some(spawned) = self.spawns.remove(spawn) {
                abandon(spawned);
            }
        }
        // Spawns whose tag a later spawn took.
        loop {
            let next = self.spawns.iter().next().map(|(spawn, _)| spawn);
            let Some(spawn) = next else { break };
            let spawned = self.spawns.remove(spawn).expect("listed above");
            abandon(spawned);
        }
        self.work = None;
        for worker in self.workers.drain(..) {
            _ = worker.join();
        }
    }
}

impl<S: Split, O: Opening> Driver for Kqueue<S, O> {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let tag = effect.tag();
            match effect {
                Effect::Kill { tag } => {
                    let spawn = self.spawn_tags.get(&tag).and_then(|&it| self.spawns.get(it));
                    if let Some(spawned) = spawn {
                        process::kill(spawned.pid);
                    }
                }
                // Its knote deleted with the next wait; an event of it already taken is told
                // before this end, and one in that wait's list names no owner, so is dropped.
                Effect::Unwatch { tag } if self.pressure.is_some_and(|(it, _)| it == tag) => {
                    let (_, id) = self.pressure.take().expect("matched above");
                    self.disown(id);
                    self.change(0, EVFILT_MEMORYSTATUS, KeventFlags::DELETE.bits(), 0, 0, id);
                    self.ready.push(Completion { tag, done: Done::Unwatched });
                }
                // Its end after every change it told, which `retire` moves to `ready` first.
                Effect::Unwatch { tag } => match self.watch_tags.remove(&tag) {
                    Some(id) => {
                        self.retire(id);
                        self.ready.push(Completion { tag, done: Done::Unwatched });
                    }
                    None => {
                        self.in_flight += 1;
                        // alloc: an error path.
                        let done = Done::Failed(IoError(format!("nothing is watched as {tag:?}")));
                        self.ready.push(Completion { tag, done });
                    }
                },
                // The end of the listen, which is in flight since it was asked for.
                Effect::Unlisten { tag } => match self.listen_tags.get(&tag).copied() {
                    Some(listen) => {
                        self.end_listen(listen);
                        self.ready.push(Completion { tag, done: Done::Unlistened });
                    }
                    None => {
                        self.in_flight += 1;
                        // alloc: an error path.
                        let done = Done::Failed(IoError(format!("nothing listens as {tag:?}")));
                        self.ready.push(Completion { tag, done });
                    }
                },
                effect => {
                    self.in_flight += 1;
                    if let Err(error) = self.begin(effect) {
                        // alloc: an error path.
                        let done = Done::Failed(IoError(error.to_string()));
                        self.ready.push(Completion { tag, done });
                    }
                }
            }
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        let before = out.len();
        let clock = Monotonic::since(self.start);
        loop {
            self.take(out);
            // A waker's ask ends the poll with what there is, which may be nothing: what it
            // woke the loop for is in a queue of the caller's own.
            let asked = self.notify.asked.swap(false, std::sync::atomic::Ordering::SeqCst);
            // Nothing in flight and no waker: nothing can come, so the poll only looks. With a
            // waker handed out it parks for the wake, to its deadline (the rule on
            // `Driver::waker`).
            let nothing_can_come = self.in_flight == 0
                && !self.waker_given.load(std::sync::atomic::Ordering::Relaxed);
            if out.len() > before || nothing_can_come || asked {
                // Changes asked for are made now, not once nothing else is ready.
                if !self.changes.is_empty() {
                    self.wait(Some(Deadline::PASSED));
                    self.take(out);
                }
                break;
            }
            self.wait(until.finite());
            if until.passed(clock.now()) {
                self.take(out);
                break;
            }
        }
        let ended = out[before..]
            .iter()
            .filter(|it| {
                !matches!(
                    it.done,
                    Done::Changed { .. }
                        | Done::Journaled(_)
                        | Done::Accepted { .. }
                        | Done::Pressed { .. }
                )
            })
            .count();
        self.in_flight -= ended;
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn wall_ns(&self) -> u64 {
        crate::system_wall_ns()
    }

    fn boot(&self) -> Result<crate::Boot, crate::NoBoot> {
        self.boot.clone()
    }

    fn waker(&self) -> Option<crate::Waker> {
        // Only the polling thread reads it, and only after this call on that same thread.
        self.waker_given.store(true, std::sync::atomic::Ordering::Relaxed);
        // clone: a handle to the kqueue's notify, once per waker asked for
        Some(crate::Waker::new(Arc::clone(&self.notify) as Arc<dyn crate::Wake>))
    }

    /// The file threads' calls, made on the asker's thread: the same [`crate::threads::run`]
    /// over the same root and kept descriptors (crate::threads::inline, its table).
    fn inline_files(&self) -> Option<crate::threads::inline::Inline> {
        // clone: two counts, once per asker's world.
        Some(crate::threads::inline::Inline::new(Arc::clone(&self.root), Arc::clone(&self.descriptors)))
    }
}

impl<S: Split, O: Opening> Kqueue<S, O> {
    /// [`Driver::now`]'s clock as a value read with no `&self`: what an asker holds beside
    /// the lock it keeps the driver under, so a clock read takes no lock (std.md, "End state:
    /// efficiency", piece 6). On the instant the driver began, as `now` and every poll's
    /// deadline are.
    pub fn clock(&self) -> crate::wait::Monotonic {
        crate::wait::Monotonic::since(self.start)
    }
}

/// Runs the file effects this thread takes off its lane's `queue`, which every thread of the
/// lane takes from with no lock, until the driver drops its end; each as `O` opens its file
/// (crate::threads::opening), timed from its ask to its take on the driver's `clock`. The
/// completions channel has room for each: the driver keeps at most [`FILES`] a lane at the
/// threads. The thread first takes the driver's `class` (the lane does not choose it: a
/// barrier a command's answer waits on runs at the command's class, unthrottled), then runs
/// each effect at most at its asker's class ([`Lowered`], which never raises), given back
/// before it hands the effect back. A class the system refuses fails the effect, or every
/// effect for the thread's own, naming why.
fn work_on<O: Opening>(
    lane: Lane,
    class: (Qos, i32),
    root: &Path,
    kept: &Descriptors,
    count: &OpenCount,
    queue: &Receiver<Queued>,
    notify: &Notify,
    clock: &Monotonic,
) {
    let refused = match qos::this_thread() {
        Ok(now) if now == class => None,
        Ok(_) => qos::set_this_thread(class.0.settable(), class.1).err(),
        Err(error) => Some(error),
    };
    while let Ok(Queued { effect, ticket, asked, asker }) = queue.recv() {
        let waited_ns = u64::try_from(clock.now().saturating_sub(asked).as_nanos()).unwrap_or(u64::MAX);
        let sent = counted(notify, || {
            let tag = effect.tag();
            let done = match refused {
                // alloc: an error path.
                Some(error) => Done::Failed(IoError(format!("a thread of the kqueue driver's {} lane: {error}", lane.name()))),
                None => match Lowered::at_most(asker) {
                    Ok(_lowered) => {
                        #[cfg(test)]
                        if let Ok((now, _)) = qos::this_thread() {
                            notify.ran_at.store(now.raw(), std::sync::atomic::Ordering::Relaxed);
                        }
                        opening::run::<O>(root, kept, count, effect)
                            // alloc: an error path.
                            .unwrap_or_else(|error| Done::Failed(IoError(error.to_string())))
                    }
                    // alloc: an error path.
                    Err(error) => Done::Failed(IoError(format!("{tag:?} at its asker's class on the kqueue driver's {} lane: {error}", lane.name()))),
                },
            };
            let sent = notify.files.send(Finished { completion: Completion { tag, done }, ticket, waited_ns }).is_ok();
            if sent {
                notify.wake();
            }
            sent
        });
        if !sent {
            return;
        }
    }
}

/// Runs `f`, a file thread's effect and its hand-back: counted in the allocation-budget tests.
#[inline]
fn counted<R>(notify: &Notify, f: impl FnOnce() -> R) -> R {
    #[cfg(test)]
    {
        let (result, used) = blaze_alloc_count::measure(f);
        notify.file_allocations.fetch_add(used.allocations, std::sync::atomic::Ordering::Relaxed);
        result
    }
    #[cfg(not(test))]
    {
        _ = notify;
        f()
    }
}

/// A process the driver no longer follows, killed and reaped.
fn abandon(spawned: Spawned) {
    let Spawned { pid, finish, stdin, stdout, stderr, .. } = spawned;
    drop((stdin, stdout, stderr));
    process::kill(pid);
    // alloc: none, empty vectors.
    _ = finish.exited(pid, Vec::new(), Vec::new());
}

fn no_connection(conn: Conn) -> io::Error {
    // alloc: an error path.
    io::Error::new(io::ErrorKind::NotConnected, format!("no connection {conn:?}"))
}

/// `O_NONBLOCK` set on `fd`: one `FIONBIO`, which sets the flag `fcntl`'s `F_SETFL` would,
/// where a read-modify-write of the flags is two calls.
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    sys_io::fionbio(Fd::new(fd), true)
        .map_err(|errno| io::Error::from_raw_os_error(i32::from(errno.raw())))
}

/// A write to a socket whose peer is gone fails rather than raising SIGPIPE.
fn no_sigpipe(fd: RawFd) -> io::Result<()> {
    sys_net::setsockopt_int(Fd::new(fd), sys_net::SOL_SOCKET, sys_net::SO_NOSIGPIPE, 1)
        .map_err(|errno| io::Error::from_raw_os_error(i32::from(errno.raw())))
}

/// Whether an error from setting up a freshly accepted socket means only that the peer already
/// closed/reset the connection, rather than a genuine OS setup failure that must report
/// [`Done::Failed`]. On macOS, `setsockopt(SO_NOSIGPIPE)` additionally returns `EINVAL` when the
/// peer disconnected before the option was set (`allow_einval`).
fn accept_setup_peer_gone(error: &io::Error, allow_einval: bool) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
    ) || matches!(
        error.raw_os_error(),
        Some(code)
            if code == i32::from(Errno::ConnReset.raw())
                || code == i32::from(Errno::ConnAborted.raw())
                || code == i32::from(Errno::NotConn.raw())
                || code == i32::from(Errno::Pipe.raw())
                || (allow_einval && code == i32::from(Errno::Inval.raw()))
    )
}

/// How a nonblocking connect of `fd` ended.
fn socket_error(fd: RawFd) -> io::Result<()> {
    match sys_net::pending_error(Fd::new(fd)) {
        Ok(None) => Ok(()),
        Ok(Some(errno)) | Err(errno) => Err(io::Error::from_raw_os_error(i32::from(errno.raw()))),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, sync::atomic::Ordering};

    use super::*;
    use crate::{Confined, Exit, FileName, Sandbox, Traced, run_until_idle, sim::Rng, threads::Threads};

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    /// A new empty directory for the test `what`, removed when the guard drops: hold it for as
    /// long as a driver works in it.
    fn dir(what: &str) -> crate::temp::TempDir {
        crate::temp::TempDir::for_test(&format!("blaze-kq-{what}-"))
    }

    /// A new driver reserves no slab and no timer wheel (the start's map calls): each is made by
    /// the first effect that needs it, and a timer made that way still fires.
    #[test]
    fn a_new_driver_reserves_nothing_until_asked() {
        let root = dir("lazy");
        let mut driver = Kqueue::new(&*root, 1).unwrap();
        let slabs = |d: &Kqueue| {
            [d.owners.slab.reserved(), d.watches.reserved(), d.spawns.reserved(), d.listens.reserved()]
                .into_iter()
                .chain([d.connecting.reserved(), d.conns.reserved(), d.timers.is_some()])
                .filter(|reserved| *reserved)
                .count()
        };
        assert_eq!(slabs(&driver), 0, "a new driver reserved a region");
        driver.submit(&mut vec![Effect::Timer { after: Duration::from_millis(1), tag: Tag(3) }]);
        assert_eq!(slabs(&driver), 1, "a timer makes the wheel and nothing else");
        let mut done = Vec::new();
        while done.is_empty() {
            driver.poll(&mut done, Deadline::after(driver.now(), Duration::from_secs(5)));
        }
        assert_eq!(done[0].tag, Tag(3));
        assert!(matches!(done[0].done, Done::Fired), "{:?}", done[0].done);
    }

    fn command(program: &str, args: &[&str], stdin: &[u8]) -> Box<Command> {
        Box::new(Command {
            program: program.into(),
            args: args.iter().map(|it| OsString::from(*it)).collect(),
            env: Vec::new(),
            dir: "/".into(),
            stdin: stdin.to_vec(),
            sandbox: Sandbox::Off,
            memory: None,
        })
    }

    /// Runs `effects` to their ends: what each tag ended with.
    fn run(driver: &mut impl Driver, effects: Vec<Effect>) -> BTreeMap<u64, Done> {
        let mut effects = effects;
        let mut seen = BTreeMap::new();
        run_until_idle(driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, done.done))
        .expect("the effects end");
        assert_eq!(driver.in_flight(), 0);
        seen
    }

    /// Polls until `out` holds `n` completions, or the deadline.
    fn poll_for(driver: &mut Kqueue, out: &mut Vec<Completion>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while out.len() < n {
            assert!(Instant::now() < deadline, "never came: {out:?}");
            driver.poll(out, Deadline::after(driver.now(), Duration::from_millis(100)));
        }
    }

    fn exit(done: &Done) -> &Exit {
        match done {
            Done::Exited(exit) => exit,
            other => panic!("{other:?}"),
        }
    }

    /// The waker's claim ([`Driver::waker`]): a wake from another thread ends a poll that no
    /// completion ends, long before its deadline, with nothing in `out`; a wake made before the
    /// poll ends that poll at once; and one wake ends one poll, not the one after. The control:
    /// with no wake the same poll runs to its deadline.
    #[test]
    fn a_waker_ends_a_poll_no_completion_ends() {
        let root = dir("waker");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        // A timer far off keeps an effect in flight, so the poll waits.
        driver.submit(&mut vec![Effect::Timer { after: Duration::from_secs(600), tag: Tag(1) }]);
        let waker = driver.waker().expect("the kqueue driver has a waker");
        let mut out = Vec::new();
        let far = |driver: &Kqueue| Deadline::after(driver.now(), Duration::from_secs(20));
        // Made before the poll: the poll does not wait.
        waker.wake();
        let began = Instant::now();
        assert_eq!(driver.poll(&mut out, far(&driver)), 0);
        assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
        // Made while it waits, from another thread.
        let other = waker.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            other.wake();
        });
        let began = Instant::now();
        assert_eq!(driver.poll(&mut out, far(&driver)), 0);
        let took = began.elapsed();
        assert!(took >= Duration::from_millis(40) && took < Duration::from_secs(5), "{took:?}");
        thread.join().unwrap();
        // The control: no wake is left over, so this poll runs to its deadline.
        let began = Instant::now();
        let near = Deadline::after(driver.now(), Duration::from_millis(60));
        assert_eq!(driver.poll(&mut out, near), 0);
        assert!(began.elapsed() >= Duration::from_millis(55), "{:?}", began.elapsed());
        assert!(out.is_empty(), "{out:?}");
    }

    /// The invariant ([`Driver::waker`]): a wake sent after the polling thread has parked in
    /// `kevent` ends the poll, first with nothing in flight (the poll parks for the wake rather
    /// than returning at once and spinning its caller), then with a timer far past the deadline
    /// in flight; each through the trigger (`EVFILT_USER`, registered in [`Kqueue::new`]),
    /// counted, not through the `asked` look before the wait. The Linux driver's test of the
    /// same name holds io_uring to the same rule.
    ///
    /// Its control is the driver before the rule: with nothing in flight the first poll
    /// returned at once, before the wake was sent, and the bound on `waited` fails.
    #[test]
    fn a_wake_ends_a_wait_parked_in_the_kernel() {
        use std::sync::atomic::{AtomicBool, Ordering};
        const BOUND: Duration = Duration::from_secs(20);
        /// How long the waking thread lets the polling thread get into `kevent`.
        const SETTLE: Duration = Duration::from_millis(100);
        let root = dir("wake-parked");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let waker = driver.waker().expect("the kqueue driver has a waker");
        for timer_in_flight in [false, true] {
            if timer_in_flight {
                driver.submit(&mut vec![Effect::Timer { after: BOUND * 2, tag: Tag(1) }]);
            }
            assert_eq!(driver.in_flight(), usize::from(timer_in_flight));
            let polling = Arc::new(AtomicBool::new(false));
            // clone: the flag and the waker, for the waking thread.
            let (seen, other) = (Arc::clone(&polling), waker.clone());
            let waking = std::thread::spawn(move || {
                blaze_spin::SpinWait::within("the poll to begin", BOUND, || seen.load(Ordering::SeqCst));
                std::thread::sleep(SETTLE);
                other.wake();
            });
            let triggers = driver.notify.triggers.load(Ordering::Relaxed);
            let mut out = Vec::new();
            let until = Deadline::after(driver.now(), BOUND);
            polling.store(true, Ordering::SeqCst);
            let began = Instant::now();
            driver.poll(&mut out, until);
            let waited = began.elapsed();
            waking.join().unwrap();
            assert!(out.is_empty(), "a wake completes nothing: {out:?}");
            assert!(
                waited >= SETTLE && waited < BOUND / 2,
                "with a timer in flight: {timer_in_flight}, the poll waited {waited:?}"
            );
            assert_eq!(
                driver.notify.triggers.load(Ordering::Relaxed),
                triggers + 1,
                "the wake ended the wait through one trigger"
            );
        }
    }

    #[test]
    fn processes_end_with_their_output_code_and_signal() {
        let root = dir("spawn");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let big: Vec<u8> = (0..1u32 << 20).map(|n| n as u8).collect();
        let script = "cat; echo err >&2; exit 7";
        let limited =
            Box::new(Command { memory: Some(1 << 30), ..*command("/bin/echo", &["hi"], b"") });
        let mut on = command("/bin/echo", &["hi"], b"");
        on.sandbox = Sandbox::On {
            read: Vec::new(),
            write: Vec::new(),
            network: false,
            trace: false,
            services: Vec::new(),
        };
        let seen = run(
            &mut driver,
            vec![
                Effect::Spawn {
                    command: command("/bin/sh", &["-c", script], b"hello"),
                    tag: Tag(1),
                },
                Effect::Spawn {
                    command: command("/bin/sh", &["-c", "kill -TERM $$"], b""),
                    tag: Tag(2),
                },
                Effect::Spawn { command: command("/bin/sleep", &["60"], b""), tag: Tag(3) },
                Effect::Kill { tag: Tag(3) },
                Effect::Spawn { command: on, tag: Tag(4) },
                Effect::Spawn { command: limited, tag: Tag(5) },
                // More input than a pipe holds, read back while it is written.
                Effect::Spawn { command: command("/bin/cat", &[], &big), tag: Tag(6) },
                Effect::Kill { tag: Tag(99) },
            ],
        );
        let one = exit(&seen[&1]);
        assert_eq!((one.code, one.signal), (Some(7), None));
        assert_eq!((one.stdout.as_slice(), one.stderr.as_slice()), (&b"hello"[..], &b"err\n"[..]));
        assert!(one.peak_memory > 0);
        let two = exit(&seen[&2]);
        assert_eq!((two.code, two.signal), (None, Some(blaze_sys::posix::signal::SIGTERM)));
        let three = exit(&seen[&3]);
        assert_eq!((three.code, three.signal), (None, Some(blaze_sys::posix::signal::SIGKILL)));
        assert!(
            matches!(&seen[&4], Done::Failed(why) if why.0.contains("Confine")),
            "{:?}",
            seen[&4]
        );
        let five = exit(&seen[&5]);
        assert_eq!((five.code, five.stdout.as_slice()), (Some(0), &b"hi\n"[..]));
        let six = exit(&seen[&6]);
        assert_eq!(six.code, Some(0));
        assert!(six.stdout == big, "{} bytes back of {}", six.stdout.len(), big.len());
        assert!(!seen.contains_key(&99));
        assert!(driver.spawns.is_empty() && driver.spawn_tags.is_empty());
        assert!(driver.owners.slab.is_empty(), "every registration of a spawn ends with it");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Starts the program after a launcher that leaves a record in the scratch directory.
    struct Recording;

    impl Confine for Recording {
        fn confine(&self, command: &Command, scratch: &Path) -> Result<Confined, String> {
            let mut args: Vec<OsString> = vec![
                "-c".into(),
                r#"printf read > "$0/record"; exec "$@""#.into(),
                scratch.into(),
                command.program.clone().into(),
            ];
            args.extend(command.args.iter().cloned());
            Ok(Confined {
                program: "/bin/sh".into(),
                args,
                env: command.env.clone(),
                dir: command.dir.clone(),
            })
        }

        fn traced(&self, _: &Command, scratch: &Path) -> Result<Traced, String> {
            match std::fs::read(scratch.join("record")) {
                Ok(record) if record == b"read" => Ok(Traced {
                    files: vec![(PathBuf::from("/etc/hosts"), None)],
                    ..Traced::default()
                }),
                other => Err(format!("no record: {other:?}")),
            }
        }
    }

    #[test]
    fn a_sandboxed_process_runs_as_its_confine_says_and_its_trace_comes_back() {
        let root = dir("confine");
        let mut driver = Kqueue::new(&root, 1).unwrap().confined(Arc::new(Recording));
        let mut traced = command("/bin/echo", &["hi"], b"");
        traced.sandbox = Sandbox::On {
            read: Vec::new(),
            write: Vec::new(),
            network: false,
            trace: true,
            services: Vec::new(),
        };
        let seen = run(&mut driver, vec![Effect::Spawn { command: traced, tag: Tag(1) }]);
        let exit = exit(&seen[&1]);
        assert_eq!((exit.code, exit.stdout.as_slice()), (Some(0), &b"hi\n"[..]));
        let wanted =
            Traced { files: vec![(PathBuf::from("/etc/hosts"), None)], ..Traced::default() };
        assert_eq!(exit.traced, Some(wanted));
        // Its scratch directory is gone.
        let left = std::fs::read_dir(root.join(".blaze-spawn")).unwrap().count();
        assert_eq!(left, 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Listens, connects, sends each way, hangs up each end and stops listening.
    /// A TCP `at` of port 0 is listened on at a port the system picks, read back from the
    /// listener, and connected to there.
    fn round_trip(root: &Path, at: Address) {
        let mut driver = Kqueue::new(root, 1).unwrap();
        // The listen is made as it is submitted.
        driver.submit(&mut vec![Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) }]);
        let at = match at {
            Address::Tcp(_) => {
                let bound = driver.listens.iter().find_map(|(_, it)| it.listener.local_addr());
                Address::Tcp(bound.expect("the TCP listen is made and bound"))
            }
            other => other,
        };
        let mut effects = vec![Effect::Connect { at, options: crate::sockopt::Tcp::DEFAULT, tag: Tag(2) }];
        let (mut server, mut client) = (None, None);
        let mut ends = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, effects| match (done.tag.0, done.done) {
            (1, Done::Accepted { conn }) => {
                server = Some(conn);
                effects.push(Effect::Recv { conn, most: 64, into: Vec::new(), tag: Tag(3) });
            }
            (2, Done::Connected { conn }) => {
                client = Some(conn);
                effects.push(Effect::Send { conn, bytes: b"ping".to_vec(), tag: Tag(4) });
            }
            (3, Done::Read(bytes)) => {
                assert_eq!(bytes, b"ping");
                let (server, client) = (server.unwrap(), client.unwrap());
                effects.push(Effect::Send { conn: server, bytes: b"pong".to_vec(), tag: Tag(5) });
                effects.push(Effect::Recv {
                    conn: client,
                    most: 64,
                    into: Vec::new(),
                    tag: Tag(6),
                });
            }
            (6, Done::Read(bytes)) => {
                assert_eq!(bytes, b"pong");
                effects.push(Effect::Hangup { conn: client.unwrap(), tag: Tag(7) });
                effects.push(Effect::Recv {
                    conn: server.unwrap(),
                    most: 64,
                    into: Vec::new(),
                    tag: Tag(8),
                });
            }
            (8, Done::Read(bytes)) => {
                assert!(bytes.is_empty(), "{bytes:?}");
                effects.push(Effect::Hangup { conn: server.unwrap(), tag: Tag(9) });
                effects.push(Effect::Unlisten { tag: Tag(1) });
            }
            (tag @ (4 | 5), Done::Sent(bytes)) if bytes.is_empty() => ends.push(tag),
            (tag @ (7 | 9), Done::HungUp) | (tag @ 1, Done::Unlistened) => ends.push(tag),
            (tag, other) => panic!("{tag}: {other:?}"),
        }).expect("the effects end");
        ends.sort();
        assert_eq!(ends, [1, 4, 5, 7, 9]);
        assert_ne!(server, client);
        assert_eq!(driver.in_flight(), 0);
        assert!(
            driver.conns.is_empty() && driver.listens.is_empty() && driver.connecting.is_empty()
        );
        assert!(driver.owners.slab.is_empty(), "every registration ends with what it was of");
    }

    #[test]
    fn a_unix_socket_round_trip() {
        let root = dir("unix");
        round_trip(&root, Address::Unix(name("s")));
        // The socket's name goes with its listener.
        assert!(!root.join("s").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_tcp_round_trip() {
        // Port 0, so the listen holds the port it is given: a port found free, let go of and
        // listened on later races whoever binds it between.
        let root = dir("tcp");
        round_trip(&root, Address::Tcp("127.0.0.1:0".parse().unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn what_names_nothing_fails() {
        let root = dir("nothing");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let seen = run(
            &mut driver,
            vec![
                Effect::Unwatch { tag: Tag(1) },
                Effect::Unlisten { tag: Tag(2) },
                Effect::Recv { conn: Conn(77), most: 8, into: b"kept".to_vec(), tag: Tag(3) },
                Effect::Hangup { conn: Conn(77), tag: Tag(4) },
                Effect::Connect { at: Address::Unix(name("nobody")), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(5) },
                Effect::Kill { tag: Tag(6) },
            ],
        );
        assert_eq!(seen.keys().copied().collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
        assert!(matches!(&seen[&3], Done::Broken { buffer, .. } if buffer == b"kept"), "{seen:?}");
        assert!(
            seen.iter().all(|(tag, it)| *tag == 3 || matches!(it, Done::Failed(_))),
            "{seen:?}"
        );
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `EVFILT_MEMORYSTATUS` is private (rule macos-private-apis): its knote is registered on
    /// this macOS and taken, or this fails naming it. Then its watch ends as asked.
    #[test]
    fn the_memory_pressure_filter_is_there() {
        let root = dir("pressure");
        let mut driver = Kqueue::new(root.path(), 1).unwrap();
        driver.submit(&mut vec![Effect::Pressure { tag: Tag(1) }]);
        let mut out = Vec::new();
        // The registration is made with this wait; a refusal comes back as its event.
        driver.poll(&mut out, Deadline::after(driver.now(), Duration::from_millis(20)));
        assert!(
            out.iter().all(|it| matches!(it.done, Done::Pressed { .. })),
            "the kernel refused a knote of EVFILT_MEMORYSTATUS (-14), the filter of \
             DISPATCH_SOURCE_TYPE_MEMORYPRESSURE: {out:?}"
        );
        assert_eq!(driver.in_flight(), 1, "the watch stays in flight");
        out.clear();
        driver.submit(&mut vec![Effect::Unwatch { tag: Tag(1) }]);
        poll_for(&mut driver, &mut out, 1);
        assert!(matches!(out.last().map(|it| &it.done), Some(Done::Unwatched)), "{out:?}");
        assert_eq!(driver.in_flight(), 0);
    }

    #[test]
    fn timers_fire_in_their_order() {
        let root = dir("timers");
        let mut driver = Kqueue::new(root.path(), 1).unwrap();
        let mut effects: Vec<Effect> = (0..3)
            .map(|n| Effect::Timer { after: Duration::from_millis(10 * (3 - n)), tag: Tag(n) })
            .collect();
        effects.push(Effect::Timer { after: Duration::ZERO, tag: Tag(3) });
        let mut fired = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| {
            assert_eq!(done.done, Done::Fired);
            fired.push(done.tag.0);
        }).expect("the effects end");
        assert_eq!(fired, [3, 2, 1, 0]);
    }

    #[test]
    fn a_wait_ends_at_its_time() {
        let root = dir("wait");
        let mut driver = Kqueue::new(root.path(), 1).unwrap();
        driver.submit(&mut vec![Effect::Timer { after: Duration::from_secs(60), tag: Tag(1) }]);
        let began = Instant::now();
        let mut out = Vec::new();
        let until = Deadline::after(driver.now(), Duration::from_millis(20));
        assert_eq!(driver.poll(&mut out, until), 0);
        let waited = began.elapsed();
        assert!(
            waited >= Duration::from_millis(19) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        assert_eq!(driver.in_flight(), 1);
    }

    #[test]
    fn a_mapping_is_the_file() {
        let root = dir("map");
        let mut driver = Kqueue::new(&root, 2).unwrap();
        let file = name("store/pack");
        let seen = run(
            &mut driver,
            vec![Effect::Write { file: file.clone(), bytes: b"packed".to_vec(), tag: Tag(1) }],
        );
        assert_eq!(seen[&1], Done::Written);
        let seen = run(
            &mut driver,
            vec![
                Effect::Map { file, extent: crate::Extent::Whole, tag: Tag(2) },
                Effect::Map { file: name("missing"), extent: crate::Extent::Whole, tag: Tag(3) },
            ],
        );
        match &seen[&2] {
            Done::Mapped(mapping) => assert_eq!(mapping.bytes(), b"packed"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(seen[&3], Done::Failed(_)));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The file effects, one after another, end as on the threads driver.
    #[test]
    fn files_end_as_on_threads() {
        let file = name("vfs/journal");
        fn script(driver: &mut impl Driver, file: &FileName) -> Vec<String> {
            let mut effects =
                vec![Effect::Write { file: file.clone(), bytes: b"one".to_vec(), tag: Tag(1) }];
            let mut seen = Vec::new();
            run_until_idle(driver, &mut effects, |done, effects| {
                seen.push(match done.done {
                    Done::Stat(Some(meta)) => format!("stat {} {}", meta.len, meta.dir),
                    Done::Listed(entries) => {
                        let entries: Vec<_> = entries
                            .iter()
                            .map(|(name, meta)| (name.as_str(), meta.len, meta.dir))
                            .collect();
                        format!("listed {entries:?}")
                    }
                    Done::Failed(_) => "failed".to_owned(),
                    other => format!("{other:?}"),
                });
                let next = match done.tag.0 {
                    1 => Effect::Append { file: file.clone(), bytes: b"two".to_vec(), tag: Tag(2) },
                    2 => Effect::Sync { file: file.clone(), tag: Tag(3) },
                    3 => Effect::Read { file: file.clone(), at: 2, len: 3, tag: Tag(4) },
                    4 => Effect::Truncate { file: file.clone(), len: 4, tag: Tag(5) },
                    5 => Effect::ReadAll { file: file.clone(), tag: Tag(6) },
                    6 => Effect::Stat { file: file.clone(), tag: Tag(7) },
                    7 => Effect::Stat { file: name("vfs"), tag: Tag(8) },
                    8 => Effect::Clone { from: file.clone(), to: name("out/c"), tag: Tag(9) },
                    9 => Effect::Link { from: file.clone(), to: name("out/l"), tag: Tag(10) },
                    10 => Effect::ReadAll { file: name("out/l"), tag: Tag(11) },
                    11 => Effect::Remove { file: name("out/l"), tag: Tag(12) },
                    12 => Effect::Stat { file: name("out/l"), tag: Tag(13) },
                    13 => Effect::ReadAll { file: name("missing"), tag: Tag(14) },
                    14 => Effect::Read { file: name("missing"), at: 0, len: 1, tag: Tag(15) },
                    15 => Effect::WriteAt {
                        file: file.clone(),
                        at: 6,
                        bytes: b"!".to_vec(),
                        tag: Tag(16),
                    },
                    16 => Effect::ReadAll { file: file.clone(), tag: Tag(17) },
                    17 => Effect::Mode { file: file.clone(), mode: 0o600, tag: Tag(18) },
                    18 => {
                        Effect::Rename { from: file.clone(), to: name("vfs/moved"), tag: Tag(19) }
                    }
                    19 => Effect::List { dir: name("vfs").into(), tag: Tag(20) },
                    20 => Effect::List { dir: name("missing").into(), tag: Tag(21) },
                    21 => Effect::WriteAt {
                        file: name("missing"),
                        at: 0,
                        bytes: vec![1],
                        tag: Tag(22),
                    },
                    _ => return,
                };
                effects.push(next);
            }).expect("the effects end");
            seen
        }
        let (ours, theirs, control) = (dir("files"), dir("files-threads"), dir("files-control"));
        let kqueue = script(&mut Kqueue::new(&ours, 2).unwrap(), &file);
        let threads = script(&mut Threads::new(&theirs, 2), &file);
        assert_eq!(kqueue, threads);
        // The controls of the two seams (one lane, opens by name) end the same.
        let before = script(&mut Kqueue::<lanes::OneLane, opening::ByName>::made(&control, 2).unwrap(), &file);
        assert_eq!(kqueue, before);
        std::fs::remove_dir_all(control).unwrap();
        assert_eq!(kqueue[5], format!("{:?}", Done::Read(b"onet".to_vec())));
        assert_eq!(kqueue[3], format!("{:?}", Done::Read(b"etw".to_vec())));
        assert_eq!(kqueue[6], "stat 4 false");
        assert!(kqueue[7].starts_with("stat ") && kqueue[7].ends_with(" true"), "{}", kqueue[7]);
        assert_eq!(kqueue[14], "failed");
        // Past the end, after the four bytes truncation left: a hole of zeros, then the byte.
        assert_eq!(kqueue[16], format!("{:?}", Done::Read(b"onet\0\0!".to_vec())));
        assert_eq!(kqueue[17], format!("{:?}", Done::Moded));
        assert_eq!(kqueue[18], format!("{:?}", Done::Renamed));
        assert!(
            kqueue[19].starts_with("listed ") && kqueue[19].contains("moved"),
            "{}",
            kqueue[19]
        );
        assert!(!kqueue[19].contains("journal"), "{}", kqueue[19]);
        assert_eq!((kqueue[20].as_str(), kqueue[21].as_str()), ("failed", "failed"));
        for it in [ours, theirs] {
            std::fs::remove_dir_all(it).unwrap();
        }
    }

    /// More file effects at once than the threads' channels hold: the rest wait in the backlog
    /// and every one ends, in the order asked on one thread.
    #[test]
    fn file_effects_past_the_threads_room_wait_and_all_end() {
        let root = dir("backlog");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let file = name("log");
        let n = 3 * FILES as u64 + 7;
        let effects = (0..n)
            .map(|at| Effect::Append {
                file: file.clone(),
                bytes: at.to_le_bytes().to_vec(),
                tag: Tag(at),
            })
            .collect();
        let seen = run(&mut driver, effects);
        assert_eq!(seen.len() as u64, n);
        assert!(seen.values().all(|it| *it == Done::Appended));
        assert!(driver.backlog.iter().all(VecDeque::is_empty));
        assert_eq!((driver.dispatch.held(Lane::Fast), driver.dispatch.held(Lane::Device)), ((0, 0), (0, 0)));
        let bytes = std::fs::read(root.join("log")).unwrap();
        let wanted: Vec<u8> = (0..n).flat_map(u64::to_le_bytes).collect();
        assert!(bytes == wanted, "appended out of order");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Two lanes: a barrier runs on the device lane's thread and a stat on the fast lane's, both
    /// threads at the class of the thread that made the driver; each lane's waits are counted.
    /// Control: one lane starts no device thread and counts both in the fast lane.
    #[test]
    fn a_barrier_runs_on_the_device_lane_and_a_stat_on_the_fast_lane() {
        use std::os::unix::thread::JoinHandleExt as _;
        fn class_of(handle: &JoinHandle<()>) -> crate::threads::qos::Qos {
            let mut class = blaze_sys::posix::proc::QOS_CLASS_UNSPECIFIED;
            let mut relative = 0;
            // SAFETY: a live thread of the driver's (joined only at its drop); the two locals
            // are written by the call.
            let got = unsafe {
                blaze_sys::posix::thread::pthread_get_qos_class_np(handle.as_pthread_t() as _, &mut class, &mut relative)
            };
            assert_eq!(got, 0, "pthread_get_qos_class_np");
            crate::threads::qos::Qos::of_raw(class).expect("a class sys/qos.h lists")
        }
        let root = dir("lanes");
        std::fs::write(root.join("f"), b"bytes").unwrap();
        let effects = || vec![Effect::Barrier { file: name("f"), tag: Tag(1) }, Effect::Stat { file: name("f"), tag: Tag(2) }];
        let mut two = Kqueue::<lanes::TwoLanes, opening::Kept>::made(&root, 1).unwrap();
        let done = run(&mut two, effects());
        assert_eq!(done.get(&1), Some(&Done::Ordered));
        let waits = two.lanes();
        assert_eq!((waits.lanes[Lane::Fast.index()].taken, waits.lanes[Lane::Device.index()].taken), (1, 1), "{waits:?}");
        assert_eq!(waits.fast_behind_device, 0, "{waits:?}");
        assert_eq!(two.workers.len(), 2, "one thread a lane");
        let names: Vec<_> = two.workers.iter().map(|it| it.thread().name().unwrap().to_owned()).collect();
        let device = two.workers.iter().position(|it| it.thread().name() == Some("blaze-kqueue-device-0")).expect("a device thread");
        let fast = 1 - device;
        for lane in [device, fast] {
            assert_eq!(class_of(&two.workers[lane]), qos::this_thread().unwrap().0, "the driver's class: {names:?}");
        }
        drop(two);
        let mut one = Kqueue::<lanes::OneLane, opening::ByName>::made(&root, 1).unwrap();
        run(&mut one, effects());
        let waits = one.lanes();
        assert_eq!((waits.lanes[Lane::Fast.index()].taken, waits.lanes[Lane::Device.index()].taken), (2, 0), "{waits:?}");
        assert_eq!(one.workers.len(), 1);
        drop(one);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// An effect runs at its asker's class, not its lane's: a barrier asked at the command's
    /// class (`user-initiated`) runs there, one asked inside a [`Lowered`] stretch (after the
    /// answer) at `utility`, and the thread is given back its class after it. A device thread
    /// first started inside the stretch still takes the driver's class for the effects asked
    /// after it. Inside the stretch, a barrier asked while the driver holds a file lock runs
    /// at the driver's class, the asker's own left lowered, and the next after the unlock is
    /// lowered again. Control: the effect asked lowered does run lowered (the class changes).
    #[test]
    fn an_effect_runs_at_its_askers_class_on_either_lane() {
        let root = dir("asker-class");
        std::fs::write(root.join("f"), b"bytes").unwrap();
        let ran_at = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    qos::set_this_thread(Qos::UserInitiated, 0).unwrap();
                    let mut driver = Kqueue::<lanes::TwoLanes, opening::Kept>::made(&root, 1).unwrap();
                    let barrier = |driver: &mut Kqueue<lanes::TwoLanes, opening::Kept>, n: u64| {
                        let done = run(driver, vec![Effect::Barrier { file: name("f"), tag: Tag(n) }]);
                        assert_eq!(done.get(&n), Some(&Done::Ordered));
                        Qos::of_raw(driver.notify.ran_at.load(Ordering::Relaxed)).expect("a class")
                    };
                    let mut seen = Vec::new();
                    {
                        // The device thread starts here, inside the stretch.
                        let lowered = Lowered::at_most(Qos::Utility).unwrap();
                        assert_eq!(lowered.was(), Some(Qos::UserInitiated));
                        seen.push(barrier(&mut driver, 1));
                    }
                    seen.push(barrier(&mut driver, 2));
                    {
                        let _lowered = Lowered::at_most(Qos::Utility).unwrap();
                        seen.push(barrier(&mut driver, 3));
                        // The critical section of a lock taken after the answer: the driver's
                        // class, given up at the unlock.
                        let lock = run(&mut driver, vec![Effect::Lock { file: name("lock"), mode: crate::LockMode::Exclusive, tag: Tag(10) }]);
                        assert_eq!(lock.get(&10), Some(&Done::Locked));
                        seen.push(barrier(&mut driver, 11));
                        let unlock = run(&mut driver, vec![Effect::Unlock { lock: Tag(10), tag: Tag(12) }]);
                        assert_eq!(unlock.get(&12), Some(&Done::Unlocked));
                        seen.push(barrier(&mut driver, 13));
                        assert_eq!(qos::this_thread().unwrap().0, Qos::Utility, "the asker stays lowered");
                    }
                    seen.push(barrier(&mut driver, 4));
                    assert_eq!(driver.workers.len(), 1, "one device thread for the barriers");
                    assert_eq!(driver.locks_held, 0);
                    drop(driver);
                    seen
                })
                .join()
                .unwrap()
        });
        assert_eq!(
            ran_at,
            [Qos::Utility, Qos::UserInitiated, Qos::Utility, Qos::UserInitiated, Qos::Utility, Qos::UserInitiated]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A map, a write-back, a read and a map again of one file open it once, through the kept
    /// descriptors. Control: by name, each opens it, and the table opens nothing.
    #[test]
    fn an_effect_sequence_on_one_file_opens_it_once() {
        fn sequence(driver: &mut impl Driver) {
            let file = name("f");
            let steps = [
                Effect::Map { file: file.clone(), extent: crate::Extent::Whole, tag: Tag(1) },
                Effect::WriteBack { file: file.clone(), tag: Tag(2) },
                Effect::Read { file: file.clone(), at: 1, len: 3, tag: Tag(3) },
                Effect::Map { file: file.clone(), extent: crate::Extent::Range { at: 1, len: 3 }, tag: Tag(4) },
                Effect::Read { file: file.clone(), at: 0, len: 5, tag: Tag(5) },
            ];
            // One at a time: no two threads race to the first open.
            for effect in steps {
                let tag = effect.tag();
                let done = run(driver, vec![effect]);
                match &done[&tag.0] {
                    Done::Mapped(mapping) => assert_eq!(mapping.bytes(), if tag.0 == 1 { &b"bytes"[..] } else { &b"yte"[..] }),
                    Done::WrittenBack => {}
                    Done::Read(bytes) => assert_eq!(&bytes[..], if tag.0 == 3 { &b"yte"[..] } else { &b"bytes"[..] }),
                    other => panic!("effect {tag:?}: {other:?}"),
                }
            }
        }
        let root = dir("opens-once");
        std::fs::write(root.join("f"), b"bytes").unwrap();
        let mut kept = Kqueue::<lanes::TwoLanes, opening::Kept>::made(&root, 1).unwrap();
        sequence(&mut kept);
        assert_eq!((kept.opens().opened, kept.opens_by_name()), (1, 0), "kept: {:?}", kept.opens());
        drop(kept);
        let mut by_name = Kqueue::<lanes::TwoLanes, opening::ByName>::made(&root, 1).unwrap();
        sequence(&mut by_name);
        assert_eq!((by_name.opens().opened, by_name.opens_by_name()), (0, 5), "by name: {:?}", by_name.opens());
        drop(by_name);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Writes in place and a cut of one file, as a segment is written and sealed: under the kept
    /// opening the writes open its writable descriptor once, its barrier and sync one read-only
    /// descriptor, and nothing opens by name; a barrier and a cut of a file that is missing make
    /// it by its name (opening.rs's rule). The by-name control opens each by name. Both leave the
    /// same bytes.
    #[test]
    fn a_write_sequence_on_one_file_opens_it_once() {
        /// The opens of the table and by name after the writes, after the syncs, after the
        /// effects on missing files.
        fn sequence<S: Split, O: Opening>(driver: &mut Kqueue<S, O>) -> [(u64, u64); 3] {
            let file = name("w");
            let mut seen = [(0, 0); 3];
            let steps = [
                Effect::WriteAt { file: file.clone(), at: 0, bytes: b"B".to_vec(), tag: Tag(1) },
                Effect::Truncate { file: file.clone(), len: 4, tag: Tag(2) },
                Effect::WriteAt { file: file.clone(), at: 4, bytes: b"S!".to_vec(), tag: Tag(3) },
                Effect::WriteAt { file: file.clone(), at: 1, bytes: b"Y".to_vec(), tag: Tag(4) },
                Effect::Barrier { file: file.clone(), tag: Tag(5) },
                Effect::Sync { file: file.clone(), tag: Tag(6) },
                // The cut makes `made/` too; a barrier makes its file, not its directory.
                Effect::Truncate { file: name("made/cut"), len: 3, tag: Tag(7) },
                Effect::Barrier { file: name("made/barrier"), tag: Tag(8) },
            ];
            // One at a time: no two threads race to the first open.
            for effect in steps {
                let tag = effect.tag();
                let done = run(driver, vec![effect]);
                match (tag.0, &done[&tag.0]) {
                    (1 | 3 | 4, Done::Written) | (2 | 7, Done::Truncated) | (5 | 8, Done::Ordered) | (6, Done::Synced) => {}
                    (_, other) => panic!("effect {tag:?}: {other:?}"),
                }
                let now = (driver.opens().opened, driver.opens_by_name());
                match tag.0 {
                    4 => seen[0] = now,
                    6 => seen[1] = now,
                    8 => seen[2] = now,
                    _ => {}
                }
            }
            seen
        }
        let ends = |root: &Path| {
            (
                std::fs::read(root.join("w")).unwrap(),
                std::fs::read(root.join("made/barrier")).unwrap(),
                std::fs::read(root.join("made/cut")).unwrap(),
            )
        };
        let root = dir("writes-open-once");
        std::fs::write(root.join("w"), b"bytes").unwrap();
        let mut kept = Kqueue::<lanes::TwoLanes, opening::Kept>::made(&root, 1).unwrap();
        let opens = sequence(&mut kept);
        // The writes: one writable open. The syncs: one read-only open more (a sealed file
        // refuses a writable one). The missing files: two made by name, none in the table.
        assert_eq!(opens, [(1, 0), (2, 0), (2, 2)], "kept: {:?}", kept.opens());
        drop(kept);
        let kept_ends = ends(root.path());
        assert_eq!(kept_ends, (b"BYteS!".to_vec(), Vec::new(), vec![0; 3]));
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("w"), b"bytes").unwrap();
        let mut by_name = Kqueue::<lanes::TwoLanes, opening::ByName>::made(&root, 1).unwrap();
        let opens = sequence(&mut by_name);
        assert_eq!(opens, [(0, 4), (0, 6), (0, 8)], "by name: {:?}", by_name.opens());
        drop(by_name);
        assert_eq!(ends(root.path()), kept_ends);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// File effects the threads all hand back while the driver does not drain fire the
    /// trigger once, not once each; the next batch, after a drain, once more; and every
    /// completion is taken.
    #[test]
    fn a_batch_of_file_effects_wakes_the_driver_once() {
        const N: usize = 64;
        let root = dir("one-wake");
        std::fs::write(root.join("f"), b"bytes").unwrap();
        let mut driver = Kqueue::new(&root, 4).unwrap();
        let file = name("f");
        for batch in 1..=3u64 {
            let mut effects: Vec<Effect> =
                (0..N as u64).map(|at| Effect::Stat { file: file.clone(), tag: Tag(at) }).collect();
            driver.submit(&mut effects);
            let deadline = Instant::now() + Duration::from_secs(10);
            while driver.files.len() < N {
                assert!(
                    Instant::now() < deadline,
                    "the threads handed back {} of {N}",
                    driver.files.len()
                );
                std::thread::yield_now();
            }
            let triggers = driver.notify.triggers.load(Ordering::Relaxed);
            assert_eq!(
                triggers, batch,
                "batch {batch}: {triggers} triggers in all for {N} hand-backs each"
            );
            let mut out = Vec::with_capacity(N);
            poll_for(&mut driver, &mut out, N);
            assert_eq!(out.len(), N);
            assert!(out.iter().all(|it| matches!(it.done, Done::Stat(Some(_)))), "{out:?}");
            assert_eq!(driver.in_flight(), 0);
        }
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `FIONBIO` sets the flag `fcntl` reads back, on both ends of a pipe; a read of an empty
    /// one then would block.
    #[test]
    fn a_descriptor_made_nonblocking_has_the_flag_fcntl_reads() {
        use std::os::fd::AsRawFd;
        let (read, write) = std::io::pipe().unwrap();
        for fd in [read.as_raw_fd(), write.as_raw_fd()] {
            let nonblocking = || {
                sys_io::status_flags(Fd::new(fd))
                    .map(|flags| flags.contains(sys_io::OpenFlags::NONBLOCK))
            };
            assert_eq!(nonblocking(), Ok(false));
            set_nonblocking(fd).unwrap();
            assert_ne!(nonblocking(), Ok(false));
        }
        let mut byte = [0u8; 1];
        let n = sys_io::read(Fd::new(read.as_raw_fd()), &mut byte)
            .map_err(|errno| io::Error::from_raw_os_error(i32::from(errno.raw())).kind());
        assert_eq!(n, Err(io::ErrorKind::WouldBlock));
        drop(write);
    }

    #[test]
    fn a_watch_hears_a_file_written_under_it_then_ends() {
        let root = dir("watch");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(
            &mut driver,
            vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }],
        );
        driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(7) }]);
        let mut heard = Vec::new();
        let mut asked_end = false;
        let mut ended = false;
        let mut done = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut n = 0;
        while !ended && Instant::now() < deadline {
            if !asked_end {
                n += 1;
                std::fs::write(root.join("outside"), b"not watched").unwrap();
                driver.submit(&mut vec![Effect::Write {
                    file: name("src/deep/lib.rs"),
                    bytes: b"fn f() {}".to_vec(),
                    tag: Tag(100 + n),
                }]);
            }
            driver.poll(&mut done, Deadline::after(driver.now(), Duration::from_millis(50)));
            for it in done.drain(..) {
                match it.done {
                    Done::Changed { files, .. } => {
                        assert!(!ended, "a change after the end");
                        heard.extend(files)
                    }
                    Done::Unwatched => ended = true,
                    Done::Written => {}
                    other => panic!("{other:?}"),
                }
            }
            if !asked_end && heard.contains(&name("src/deep/lib.rs")) {
                asked_end = true;
                driver.submit(&mut vec![Effect::Unwatch { tag: Tag(7) }]);
            }
        }
        assert!(ended, "the watch never ended; heard {heard:?}");
        assert!(!heard.contains(&name("outside")), "{heard:?}");
        run(&mut driver, Vec::new());
        assert!(driver.watches.is_empty() && driver.watch_tags.is_empty());
        assert_eq!(driver.spare.len(), 1, "the stopped watch is kept for the next");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_flush_is_told_after_the_changes_made_before_it() {
        let root = dir("flush");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(
            &mut driver,
            vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }],
        );
        driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(7) }]);
        for round in 0..20u64 {
            let file = format!("src/edit-{round}");
            std::fs::write(root.join(&file), b"edited").unwrap();
            let flush = Tag(100 + round);
            driver.submit(&mut vec![Effect::Flush { watch: Tag(7), tag: flush }]);
            let mut heard = Vec::new();
            let mut done = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut flushed = false;
            while !flushed && Instant::now() < deadline {
                driver.poll(&mut done, Deadline::after(driver.now(), Duration::from_millis(50)));
                for it in done.drain(..) {
                    match it.done {
                        Done::Changed { files, .. } => heard.extend(files),
                        Done::Flushed if it.tag == flush => flushed = true,
                        other => panic!("{other:?}"),
                    }
                }
            }
            assert!(flushed, "round {round}: the flush never came");
            assert!(heard.contains(&name(&file)), "round {round}: {heard:?}");
        }
        let mut effects =
            vec![Effect::Flush { watch: Tag(8), tag: Tag(9) }, Effect::Unwatch { tag: Tag(7) }];
        let mut refused = false;
        run_until_idle(&mut driver, &mut effects, |it, _| match (it.tag.0, it.done) {
            (9, Done::Failed(_)) => refused = true,
            (7, Done::Changed { .. } | Done::Unwatched) => {}
            other => panic!("{other:?}"),
        }).expect("the effects end");
        assert!(refused);
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A flush asked and the watch ended before it is told: the flush fails, after the changes
    /// the watch told and before its end.
    #[test]
    fn a_flush_its_watch_ends_before_fails_before_the_end() {
        let root = dir("flush-end");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(
            &mut driver,
            vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }],
        );
        let mut effects = vec![
            Effect::Watch { dir: name("src"), tag: Tag(7) },
            Effect::Flush { watch: Tag(7), tag: Tag(8) },
            Effect::Unwatch { tag: Tag(7) },
        ];
        let mut order = Vec::new();
        run_until_idle(&mut driver, &mut effects, |it, _| match (it.tag.0, it.done) {
            (8, Done::Flushed) => order.push("flushed"),
            (8, Done::Failed(_)) => order.push("failed"),
            (7, Done::Changed { .. }) => {}
            (7, Done::Unwatched) => order.push("unwatched"),
            other => panic!("{other:?}"),
        }).expect("the effects end");
        assert!(matches!(order.as_slice(), ["flushed" | "failed", "unwatched"]), "{order:?}");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Polls the journal `tag` until `until` says of a batch, or the deadline: every batch.
    fn journal_until(
        driver: &mut Kqueue,
        tag: Tag,
        mut until: impl FnMut(&crate::Journaled) -> bool,
    ) -> Vec<crate::Journaled> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut batches, mut done) = (Vec::new(), Vec::new());
        while Instant::now() < deadline {
            driver.poll(&mut done, Deadline::after(driver.now(), Duration::from_millis(50)));
            for it in done.drain(..) {
                assert_eq!(it.tag, tag);
                let Done::Journaled(batch) = it.done else { panic!("{:?}", it.done) };
                let stop = until(&batch);
                batches.push(*batch);
                if stop {
                    return batches;
                }
            }
        }
        panic!("never came: {batches:?}")
    }

    /// Ends the journal `tag`: what it told after the unwatch was asked, before its end.
    fn unwatch(driver: &mut Kqueue, tag: Tag) {
        let mut effects = vec![Effect::Unwatch { tag }];
        run_until_idle(driver, &mut effects, |it, _| match it.done {
            Done::Journaled(_) | Done::Unwatched => assert_eq!(it.tag, tag),
            other => panic!("{other:?}"),
        }).expect("the effects end");
        assert_eq!(driver.in_flight(), 0);
    }

    #[test]
    fn a_journal_resumes_from_its_cursor_through_the_driver() {
        let root = dir("journal");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(
            &mut driver,
            vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }],
        );
        let src = || crate::DirName::Named(name("src"));
        driver.submit(&mut vec![Effect::Journal {
            dir: src(),
            since: crate::Cursor::None,
            tag: Tag(7),
        }]);
        let batches = journal_until(&mut driver, Tag(7), |it| it.caught_up);
        assert!(matches!(batches[0].cursor, crate::Cursor::FsEvents { .. }), "{batches:?}");
        assert!(!batches[0].caught_up && batches[0].lost.is_empty() && batches[0].whole.is_none());
        // A batch does not end it.
        assert_eq!(driver.in_flight(), 1);
        let cursor = batches.last().unwrap().cursor;
        unwatch(&mut driver, Tag(7));
        std::fs::write(root.join("src/while-down.rs"), b"fn f() {}").unwrap();
        fsevents::fseventsd_keeps(Duration::from_secs(1));
        driver.submit(&mut vec![Effect::Journal { dir: src(), since: cursor, tag: Tag(8) }]);
        let batches = journal_until(&mut driver, Tag(8), |it| it.caught_up);
        let told: Vec<_> = batches.iter().flat_map(|it| &it.files).collect();
        assert!(told.contains(&&name("src/while-down.rs")), "{batches:?}");
        assert!(batches.iter().all(|it| it.lost.is_empty() && it.whole.is_none()), "{batches:?}");
        assert_eq!(driver.in_flight(), 1);
        unwatch(&mut driver, Tag(8));
        // Of another store: lost, the whole directory.
        let other = crate::Cursor::FsEvents { event: 1, volume: [7; 16] };
        driver.submit(&mut vec![Effect::Journal { dir: src(), since: other, tag: Tag(9) }]);
        let batches = journal_until(&mut driver, Tag(9), |_| true);
        assert_eq!((batches[0].lost.as_slice(), batches[0].whole), (&[][..], Some(crate::Lost::Reset)));
        unwatch(&mut driver, Tag(9));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A journal of the driver's ROOT (a checkout run from its own folder) resumes from its
    /// cursor and names what changed at the root's own level, not only beneath its folders: a
    /// file made there, one removed, and one renamed in from a folder (named at its source and
    /// at its destination). CONTROL: a file written in a folder is named as before.
    #[test]
    fn a_journal_of_the_root_names_its_own_entries() {
        let root = dir("journal-root");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(
            &mut driver,
            vec![
                Effect::Write { file: name("gone.toml"), bytes: Vec::new(), tag: Tag(1) },
                Effect::Write { file: name("src/moved.rs"), bytes: Vec::new(), tag: Tag(2) },
            ],
        );
        driver.submit(&mut vec![Effect::Journal { dir: crate::DirName::Root, since: crate::Cursor::None, tag: Tag(7) }]);
        let batches = journal_until(&mut driver, Tag(7), |it| it.caught_up);
        let cursor = batches.last().unwrap().cursor;
        unwatch(&mut driver, Tag(7));
        std::fs::write(root.join("made.toml"), b"[a]").unwrap();
        std::fs::remove_file(root.join("gone.toml")).unwrap();
        std::fs::rename(root.join("src/moved.rs"), root.join("moved.rs")).unwrap();
        std::fs::write(root.join("src/edited.rs"), b"fn f() {}").unwrap();
        fsevents::fseventsd_keeps(Duration::from_secs(1));
        driver.submit(&mut vec![Effect::Journal { dir: crate::DirName::Root, since: cursor, tag: Tag(8) }]);
        let batches = journal_until(&mut driver, Tag(8), |it| it.caught_up);
        let told: Vec<_> = batches.iter().flat_map(|it| &it.files).collect();
        for wanted in ["made.toml", "gone.toml", "moved.rs", "src/moved.rs", "src/edited.rs"] {
            assert!(told.contains(&&name(wanted)), "{wanted}: {batches:?}");
        }
        assert!(batches.iter().all(|it| it.whole.is_none()), "{batches:?}");
        unwatch(&mut driver, Tag(8));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stamps_and_entries_are_the_file_systems() {
        let root = dir("stamps");
        let mut driver = Kqueue::new(&root, 2).unwrap();
        run(
            &mut driver,
            vec![
                Effect::Write {
                    file: name("src/lib.rs"),
                    bytes: b"fn f() {}".to_vec(),
                    tag: Tag(1),
                },
                Effect::Write { file: name("src/deep/mod.rs"), bytes: Vec::new(), tag: Tag(2) },
            ],
        );
        std::os::unix::fs::symlink("lib.rs", root.join("src/link")).unwrap();
        let mut names = crate::Names::default();
        for it in ["src/lib.rs", "src/deep", "src/link", "src/missing", "src/lib.rs/under"] {
            assert!(names.push_str(it));
        }
        let seen = run(
            &mut driver,
            vec![
                Effect::Stamp { files: Arc::new(names.clone()), tag: Tag(3) },
                Effect::Entries { dir: name("src").into(), tag: Tag(4) },
                Effect::Entries { dir: name("missing").into(), tag: Tag(5) },
            ],
        );
        let stamps = crate::fs::stamps(&root, &names).unwrap();
        assert_eq!(seen[&3], Done::Stamped(stamps.clone()));
        assert!(stamps[0].is_file() && stamps[0].size == 9, "{stamps:?}");
        assert!(stamps[1].is_dir() && stamps[2].is_link(), "{stamps:?}");
        assert!(stamps[3].is_gone() && stamps[4].is_gone(), "{stamps:?}");
        let entries = crate::fs::entries(&root.join("src"), &name("src").into()).unwrap();
        assert_eq!(seen[&4], Done::Entries(Box::new(entries.clone())));
        let named: Vec<_> = entries.iter().map(|(it, _)| it.as_str()).collect();
        assert_eq!(named, ["deep", "lib.rs", "link"]);
        assert!(matches!(seen[&5], Done::Failed(_)));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The map of registrations the driver kept before its slab: ids counted up from the
    /// trigger's, never reused.
    struct OldOwners {
        next: u64,
        owners: Map<u64, Owner, FixedState, Global>,
    }

    impl OldOwners {
        fn own(&mut self, owner: Owner) -> u64 {
            let id = self.next;
            self.next += 1;
            self.owners.insert(id, owner);
            id
        }

        fn get(&self, id: u64) -> Option<Owner> {
            self.owners.get(&id).copied()
        }

        fn disown(&mut self, id: u64) -> Option<Owner> {
            self.owners.remove(&id)
        }
    }

    fn any_owner(rng: &mut Rng) -> Owner {
        let bits = rng.next_u64();
        match rng.below(7) {
            0 => Owner::Exit(SpawnId::from_bits(bits)),
            1 => Owner::Output { spawn: SpawnId::from_bits(bits), err: rng.one_in(2) },
            2 => Owner::Input(SpawnId::from_bits(bits)),
            3 => Owner::Listen(ListenId::from_bits(bits)),
            4 => Owner::Connecting(ConnectId::from_bits(bits)),
            5 => Owner::Read(ConnId::from_bits(bits)),
            _ => Owner::Write(ConnId::from_bits(bits)),
        }
    }

    /// Against the map it replaced, on every seed: registrations made, ended (twice, too),
    /// looked up live and ended, and ids no registration had. Both answer the same each time;
    /// an ended one's id names nothing even once its slot is another's, and no id is issued
    /// twice.
    #[test]
    fn owners_answer_as_the_map_they_replaced() {
        for seed in 0..300 {
            let mut rng = Rng::new(seed);
            let mut owners = Owners::new().unwrap();
            let mut old = OldOwners { next: TRIGGER + 1, owners: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }) };
            // Each registration made: its id in each, and whether it is live.
            let mut made: Vec<(u64, u64, bool)> = Vec::new();
            for _ in 0..500 {
                match rng.below(8) {
                    0..=2 => {
                        let owner = any_owner(&mut rng);
                        let (id, old_id) = (owners.own(owner), old.own(owner));
                        assert_ne!(id, TRIGGER, "seed {seed}");
                        assert!(
                            made.iter().all(|it| it.0 != id),
                            "seed {seed}: {id:#x} issued twice"
                        );
                        made.push((id, old_id, true));
                    }
                    3 | 4 if !made.is_empty() => {
                        let at = rng.below(made.len() as u64) as usize;
                        let (id, old_id, live) = made[at];
                        let ended = owners.disown(id);
                        assert_eq!(ended, old.disown(old_id), "seed {seed}");
                        assert_eq!(ended.is_some(), live, "seed {seed}");
                        assert_eq!(owners.disown(id), None, "seed {seed}: ended twice");
                        made[at].2 = false;
                    }
                    5 if !made.is_empty() => {
                        let (id, old_id, live) = made[rng.below(made.len() as u64) as usize];
                        assert_eq!(owners.get(id), old.get(old_id), "seed {seed}");
                        assert_eq!(owners.get(id).is_some(), live, "seed {seed}");
                    }
                    6 => {
                        let bits = rng.next_u64();
                        let live = made.iter().any(|it| it.0 == bits && it.2);
                        assert_eq!(owners.get(bits).is_some(), live, "seed {seed}: {bits:#x}");
                        assert_eq!(owners.get(TRIGGER), None, "seed {seed}");
                    }
                    _ => {
                        for &(id, old_id, _) in &made {
                            assert_eq!(owners.get(id), old.get(old_id), "seed {seed}");
                        }
                    }
                }
                assert_eq!(owners.slab.len(), old.owners.len(), "seed {seed}");
            }
        }
    }

    /// A kernel event carrying the handle of a registration that ended, its slot then another
    /// registration's, is dropped; the same event carrying the live one's is acted on.
    #[test]
    fn an_event_of_a_registration_ended_and_its_slot_reused_is_dropped() {
        let root = dir("stale");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let connecting = |driver: &mut Kqueue, tag| {
            let fd = net::stream_socket(sys_net::AF_UNIX).unwrap();
            driver.connecting.insert(Connecting {
                tag,
                fd,
                at: Address::Unix(name("x")),
                unix: true,
            })
        };
        let first = connecting(&mut driver, Tag(1));
        let ended = driver.owners.own(Owner::Connecting(first));
        driver.disown(ended);
        drop(driver.connecting.remove(first));
        let second = connecting(&mut driver, Tag(2));
        let live = driver.owners.own(Owner::Connecting(second));
        assert_eq!(Id::from_bits(ended).index(), Id::from_bits(live).index(), "the slot is reused");
        assert_eq!(first.index(), second.index(), "the connect's slot is reused");
        assert_ne!(ended, live);
        // An `EVFILT_USER` of its own, its `udata` the ended registration's, then triggered.
        let user = |driver: &Kqueue, id: u64| {
            let flags = (KeventFlags::ADD | KeventFlags::CLEAR).bits();
            driver.notify.change(1, Filter::USER.0, flags, 0, 0, id).unwrap();
            driver.notify.change(1, Filter::USER.0, 0, NoteFlags::TRIGGER.bits(), 0, id).unwrap();
        };
        user(&driver, ended);
        driver.wait(Some(Deadline::after(driver.now(), Duration::from_millis(50))));
        let mut out = Vec::new();
        driver.take(&mut out);
        assert!(out.is_empty(), "{out:?}");
        assert!(driver.connecting.contains(second), "the live connect is left as it is");
        assert!(driver.connecting.get(first).is_none());
        user(&driver, live);
        let deadline = Instant::now() + Duration::from_secs(5);
        while out.is_empty() && Instant::now() < deadline {
            driver.wait(Some(Deadline::after(driver.now(), Duration::from_millis(50))));
            driver.take(&mut out);
        }
        assert!(
            matches!(out.as_slice(), [Completion { tag: Tag(2), done: Done::Connected { .. } }]),
            "{out:?}"
        );
        assert!(!driver.connecting.contains(second));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A Sync, a Stat and a Read through the file threads, once the driver's buffers and the
    /// threads' have grown: nothing allocated on the driver's thread, and on the file thread
    /// only the Read's bytes.
    #[test]
    fn a_warm_file_cycle_allocates_only_the_reads_bytes() {
        const WARM: u64 = 8;
        const ROUNDS: u64 = 200;
        let root = dir("budget-files");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let file = name("f");
        run(
            &mut driver,
            vec![Effect::Write { file: file.clone(), bytes: b"bytes".to_vec(), tag: Tag(1) }],
        );
        let (mut effects, mut out) = (Vec::with_capacity(3), Vec::with_capacity(16));
        for round in 0..WARM + ROUNDS {
            if round == WARM {
                driver.notify.file_allocations.store(0, Ordering::Relaxed);
            }
            // The caller's: made before the cycle.
            effects.extend([
                Effect::Sync { file: file.clone(), tag: Tag(10) },
                Effect::Stat { file: file.clone(), tag: Tag(11) },
                Effect::Read { file: file.clone(), at: 1, len: 3, tag: Tag(12) },
            ]);
            let ((), used) = blaze_alloc_count::measure(|| {
                driver.submit(&mut effects);
                poll_for(&mut driver, &mut out, 3);
            });
            if round >= WARM {
                assert_eq!(
                    used.allocations, 0,
                    "round {round}: a warm Sync, Stat and Read, on the driver's thread"
                );
            }
            out.sort_by_key(|it| it.tag);
            assert!(
                matches!(
                    out.as_slice(),
                    [
                        Completion { done: Done::Synced, .. },
                        Completion { done: Done::Stat(Some(_)), .. },
                        Completion { done: Done::Read(bytes), .. },
                    ] if bytes == b"yte"
                ),
                "{out:?}"
            );
            out.clear();
        }
        let threads = driver.notify.file_allocations.load(Ordering::Relaxed);
        assert!(
            threads <= ROUNDS,
            "the file thread: {threads} allocations over {ROUNDS} warm cycles; each may make the Read's Vec<u8> only"
        );
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A file's times set through the file threads, once the driver's buffers and the threads'
    /// have grown: nothing allocated on the driver's thread, nor by the file thread's
    /// `utimensat` and its hand-back.
    #[test]
    fn a_warm_times_set_allocates_nothing() {
        const WARM: u64 = 8;
        const ROUNDS: u64 = 200;
        let root = dir("budget-times");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let file = name("f");
        run(
            &mut driver,
            vec![Effect::Write { file: file.clone(), bytes: b"bytes".to_vec(), tag: Tag(1) }],
        );
        let (mut effects, mut out) = (Vec::with_capacity(1), Vec::with_capacity(4));
        for round in 0..WARM + ROUNDS {
            if round == WARM {
                driver.notify.file_allocations.store(0, Ordering::Relaxed);
            }
            // The caller's: made before the cycle.
            let modified_ns = 1_700_000_000_000_000_000 + 1000 * round;
            effects.push(Effect::Times { file: file.clone(), modified_ns, tag: Tag(round) });
            let ((), used) = blaze_alloc_count::measure(|| {
                driver.submit(&mut effects);
                poll_for(&mut driver, &mut out, 1);
            });
            if round >= WARM {
                assert_eq!(
                    used.allocations, 0,
                    "round {round}: a warm set of times, on the driver's thread"
                );
            }
            assert_eq!(out.as_slice(), [Completion { tag: Tag(round), done: Done::Timed }]);
            out.clear();
        }
        let threads = driver.notify.file_allocations.load(Ordering::Relaxed);
        assert_eq!(
            threads, 0,
            "the file thread: {threads} allocations over {ROUNDS} warm sets of times"
        );
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_warm_timer_arm_and_fire_allocates_nothing() {
        let root = dir("budget-timers");
        let mut driver = Kqueue::new(root.path(), 1).unwrap();
        let (mut effects, mut out) = (Vec::with_capacity(1), Vec::with_capacity(4));
        for round in 0..1000u64 {
            let ((), used) = blaze_alloc_count::measure(|| {
                effects.push(Effect::Timer { after: Duration::ZERO, tag: Tag(round) });
                driver.submit(&mut effects);
                poll_for(&mut driver, &mut out, 1);
            });
            if round >= 8 {
                assert_eq!(used.allocations, 0, "round {round}: a warm timer arm and fire");
            }
            assert_eq!(out.as_slice(), [Completion { tag: Tag(round), done: Done::Fired }]);
            out.clear();
        }
    }

    /// A watch added, a change heard and the watch removed, once a stopped watch is kept for
    /// the next: nothing allocated on the driver's thread, nor by the watch's callbacks beyond
    /// the names `Done::Changed` carries.
    #[test]
    fn a_warm_watch_add_change_and_remove_allocates_only_the_changes() {
        const WARM: u32 = 2;
        let root = dir("budget-watch");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(
            &mut driver,
            vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }],
        );
        let (mut effects, mut out) = (Vec::with_capacity(1), Vec::with_capacity(64));
        let (src, edited, path) = (name("src"), name("src/edit"), root.join("src/edit"));
        let mut on_driver = 0;
        for round in 0..WARM + 4 {
            if round == WARM {
                driver.watch_overhead = 0;
                on_driver = 0;
            }
            // The caller's: made before the cycle.
            effects.push(Effect::Watch { dir: src.clone(), tag: Tag(7) });
            let ((), used) = blaze_alloc_count::measure(|| driver.submit(&mut effects));
            on_driver += used.allocations;
            let (mut heard, deadline) = (false, Instant::now() + Duration::from_secs(10));
            while !heard {
                assert!(Instant::now() < deadline, "round {round}: the change was never heard");
                std::fs::write(&path, b"edited").unwrap();
                let until = Deadline::after(driver.now(), Duration::from_millis(50));
                let (_, used) =
                    blaze_alloc_count::measure(|| driver.poll(&mut out, until));
                on_driver += used.allocations;
                for it in out.drain(..) {
                    match it.done {
                        Done::Changed { files, .. } => heard |= files.contains(&edited),
                        other => panic!("round {round}: {other:?}"),
                    }
                }
            }
            effects.push(Effect::Unwatch { tag: Tag(7) });
            let ((), used) = blaze_alloc_count::measure(|| {
                driver.submit(&mut effects);
                poll_for(&mut driver, &mut out, 1);
                while !matches!(out.last(), Some(Completion { done: Done::Unwatched, .. })) {
                    driver.poll(&mut out, Deadline::after(driver.now(), Duration::from_millis(50)));
                }
            });
            on_driver += used.allocations;
            for it in out.drain(..) {
                assert!(matches!(it.done, Done::Changed { .. } | Done::Unwatched), "{it:?}");
            }
            assert_eq!(driver.in_flight(), 0);
        }
        assert_eq!(on_driver, 0, "four warm watch cycles, on the driver's thread");
        assert_eq!(
            driver.watch_overhead, 0,
            "the watches' callbacks, beyond what Done::Changed carries"
        );
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A send on one end and a receive on the other, on a warm connection, each buffer handed
    /// back and given to the next: nothing allocated. Then both ends hung up and their slots
    /// another connection's: the old `Conn`s name nothing, and their buffers come back.
    #[test]
    fn a_warm_send_and_receive_allocates_nothing() {
        let root = dir("budget-conn");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let at = Address::Unix(name("s"));
        let mut effects = vec![
            Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) },
            Effect::Connect { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(2) },
        ];
        driver.submit(&mut effects);
        let mut out = Vec::with_capacity(16);
        let accept = |driver: &mut Kqueue, out: &mut Vec<Completion>, connected: Tag| {
            let (mut server, mut client) = (None, None);
            poll_for(driver, out, 2);
            for it in out.drain(..) {
                match it.done {
                    Done::Accepted { conn } if it.tag == Tag(1) => server = Some(conn),
                    Done::Connected { conn } if it.tag == connected => client = Some(conn),
                    other => panic!("{other:?}"),
                }
            }
            (server.unwrap(), client.unwrap())
        };
        let (server, client) = accept(&mut driver, &mut out, Tag(2));
        let (mut sending, mut receiving) = (Vec::new(), Vec::new());
        for round in 0..200u64 {
            sending.extend_from_slice(b"ping");
            receiving.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                let (bytes, into) = (std::mem::take(&mut sending), std::mem::take(&mut receiving));
                effects.push(Effect::Send { conn: client, bytes, tag: Tag(10) });
                effects.push(Effect::Recv { conn: server, most: 64, into, tag: Tag(11) });
                driver.submit(&mut effects);
                poll_for(&mut driver, &mut out, 2);
            });
            if round >= 8 {
                assert_eq!(
                    used.allocations, 0,
                    "round {round}: a warm send and receive, their buffers reused"
                );
            }
            out.sort_by_key(|it| it.tag);
            match &mut out[..] {
                [
                    Completion { done: Done::Sent(sent), .. },
                    Completion { done: Done::Read(read), .. },
                ] if sent.is_empty() && read == b"ping" => {
                    (sending, receiving) = (std::mem::take(sent), std::mem::take(read));
                }
                other => panic!("{other:?}"),
            }
            out.clear();
        }
        assert!(
            sending.capacity() >= 4 && receiving.capacity() >= 64,
            "the buffers keep their room"
        );
        effects.extend([
            Effect::Hangup { conn: client, tag: Tag(20) },
            Effect::Hangup { conn: server, tag: Tag(21) },
        ]);
        driver.submit(&mut effects);
        poll_for(&mut driver, &mut out, 2);
        assert!(out.iter().all(|it| it.done == Done::HungUp), "{out:?}");
        out.clear();
        // A made-up `Conn` with the generation a freed slot has now.
        let free = Conn(client.0 + (1 << 32));
        effects.push(Effect::Recv { conn: free, most: 8, into: receiving, tag: Tag(22) });
        driver.submit(&mut effects);
        poll_for(&mut driver, &mut out, 1);
        assert!(
            matches!(&out[..], [Completion { tag: Tag(22), done: Done::Broken { buffer, .. } }] if buffer.capacity() >= 64),
            "{out:?}"
        );
        out.clear();
        effects.push(Effect::Connect { at, options: crate::sockopt::Tcp::DEFAULT, tag: Tag(3) });
        driver.submit(&mut effects);
        let (again, other) = accept(&mut driver, &mut out, Tag(3));
        let index = |conn: Conn| conn_id(conn).index();
        let mut old = [index(server), index(client)];
        let mut new = [index(again), index(other)];
        old.sort();
        new.sort();
        assert_eq!(old, new, "the hung-up connections' slots are the new ones'");
        effects.extend([
            Effect::Recv { conn: client, most: 8, into: Vec::new(), tag: Tag(30) },
            Effect::Send { conn: server, bytes: b"x".to_vec(), tag: Tag(31) },
        ]);
        driver.submit(&mut effects);
        poll_for(&mut driver, &mut out, 2);
        assert!(
            out.iter()
                .all(|it| matches!(&it.done, Done::Broken { buffer, .. } if buffer.is_empty())),
            "stale: {out:?}"
        );
        out.clear();
        effects.extend([
            Effect::Hangup { conn: again, tag: Tag(40) },
            Effect::Hangup { conn: other, tag: Tag(41) },
            Effect::Unlisten { tag: Tag(1) },
        ]);
        driver.submit(&mut effects);
        poll_for(&mut driver, &mut out, 3);
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Invariant: a sync and a barrier of a file sealed read-only succeed through this
    /// driver and the threads driver, and of a missing file make it
    /// ([`crate::fs::assert_syncs_a_sealed_file`], with its control).
    #[test]
    fn a_sync_of_a_sealed_file_succeeds_on_each_driver() {
        let root = dir("sync-sealed");
        crate::fs::assert_syncs_a_sealed_file(&mut Kqueue::new(&root, 1).unwrap(), &root);
        let threads = dir("sync-sealed-threads");
        crate::fs::assert_syncs_a_sealed_file(&mut Threads::new(&threads, 2), &threads);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(threads).unwrap();
    }

    #[test]
    fn accept_setup_peer_gone_distinguishes_peer_drop_from_os_failure() {
        let from = |errno: Errno| io::Error::from_raw_os_error(i32::from(errno.raw()));
        for errno in [Errno::ConnReset, Errno::ConnAborted, Errno::NotConn, Errno::Pipe] {
            assert!(accept_setup_peer_gone(&from(errno), false), "{errno} is peer-gone");
            assert!(accept_setup_peer_gone(&from(errno), true), "{errno} is peer-gone");
        }
        assert!(!accept_setup_peer_gone(&from(Errno::Inval), false), "EINVAL from set_nonblocking is not peer-gone");
        assert!(accept_setup_peer_gone(&from(Errno::Inval), true), "EINVAL from SO_NOSIGPIPE is peer-gone on macOS");
        for errno in [Errno::BadF, Errno::NoMem, Errno::NoProtoOpt, Errno::Acces, Errno::Fault] {
            assert!(!accept_setup_peer_gone(&from(errno), false), "{errno} must surface as Done::Failed");
            assert!(!accept_setup_peer_gone(&from(errno), true), "{errno} must surface as Done::Failed");
        }
    }
}
