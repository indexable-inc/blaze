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
//! goes to the threads through a bounded channel and comes back through another, each holding
//! [`FILES`], the most at the threads at once (more wait in a backlog the driver keeps), so
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

use blaze_hash::HashMap;
use blaze_mem::{MemError, Slab, Tag as MemTag};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender};

use crate::{
    Address, Command, Completion, Confine, Conn, Cursor, Done, Driver, Effect, FileName, IoError,
    Tag,
    fsevents::{self, Tell},
    handoff::Wake,
    net, process,
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
    size_of::<Kevent>() == size_of::<libc::kevent>()
        && align_of::<Kevent>() == align_of::<libc::kevent>()
);

/// The kqueue and the channels, shared with the file threads and the watches' queues.
struct Notify {
    kq: OwnedFd,
    files: Sender<Completion>,
    watched: Sender<Completion>,
    /// Whether a hand-back since the driver last drained the channels has fired the trigger:
    /// a batch of them costs one `kevent`, not one each.
    woke: Wake,
    /// What the file threads allocated doing their effects and handing them back.
    #[cfg(test)]
    file_allocations: std::sync::atomic::AtomicU64,
    /// Triggers fired.
    #[cfg(test)]
    triggers: std::sync::atomic::AtomicU64,
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
        loop {
            // SAFETY: one change, laid out as a `kevent`, to read and no events to write.
            let changed = unsafe {
                libc::kevent(
                    self.kq.as_raw_fd(),
                    (&raw const event).cast(),
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if changed >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// After a hand-back into a channel: wakes the polling thread, unless one since its last
    /// drain ([`Kqueue::take`]) has. The trigger fired stays pending until a wait takes it, and
    /// every wait is followed by a drain before the next, so no hand-back is left unwoken.
    fn wake(&self) {
        if self.woke.wake() {
            #[cfg(test)]
            self.triggers.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            _ = self.change(TRIGGER as usize, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0, TRIGGER);
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
}

/// What each registration's `udata` names. Ids are handles' bits: one of a registration that
/// has ended names nothing, even once its slot holds another's.
struct Owners {
    slab: Slab<Owner>,
}

impl Owners {
    fn new() -> Result<Owners, MemError> {
        Ok(Owners { slab: Slab::new(MOST, MemTag::Io)? })
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
    pid: libc::pid_t,
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

pub struct Kqueue {
    start: Instant,
    root: Arc<Path>,
    notify: Arc<Notify>,
    files: Receiver<Completion>,
    watched: Receiver<Completion>,
    work: Option<Sender<Effect>>,
    /// File effects asked for beyond the [`FILES`] at the threads, in the order asked.
    backlog: VecDeque<Effect>,
    /// File effects at the threads whose completions are not taken yet.
    at_threads: usize,
    workers: Vec<JoinHandle<()>>,
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
    /// By ticks since `start`; the wait ends at the next one due.
    timers: Wheel<Tag>,
    watches: Slab<fsevents::Watch<Watched>>,
    watch_tags: HashMap<Tag, WatchId>,
    /// Stopped, for the next watch.
    spare: Vec<fsevents::Watch<Watched>>,
    /// The flushes a stopping watch had not told.
    untold: Vec<Tag>,
    spawns: Slab<Spawned>,
    spawn_tags: HashMap<Tag, SpawnId>,
    listens: Slab<Listening>,
    listen_tags: HashMap<Tag, ListenId>,
    connecting: Slab<Connecting>,
    conns: Slab<Connection>,
    in_flight: usize,
    /// What the watches' callbacks allocated beyond their payloads, as each watch stopped.
    #[cfg(test)]
    watch_overhead: u64,
}

impl Kqueue {
    /// A driver whose files are under `root`, with `threads` threads for the file calls.
    pub fn new(root: impl Into<PathBuf>, threads: usize) -> io::Result<Kqueue> {
        let root: Arc<Path> = root.into().into();
        // SAFETY: no pointers; a descriptor of its own or -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a descriptor of its own.
        let kq = unsafe { OwnedFd::from_raw_fd(kq) };
        net::set_cloexec(kq.as_raw_fd())?;
        // alloc: setup, once per driver: each channel's slots.
        let (files_done, files) = crossbeam_channel::bounded(FILES);
        let (watched_done, watched) = crossbeam_channel::bounded(WATCHED); // alloc: as above.
        let (work, queue) = crossbeam_channel::bounded::<Effect>(FILES); // alloc: as above.
        let notify = Arc::new(Notify {
            kq,
            files: files_done,
            watched: watched_done,
            woke: Wake::new(),
            #[cfg(test)]
            file_allocations: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            triggers: std::sync::atomic::AtomicU64::new(0),
        });
        notify.change(
            TRIGGER as usize,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
            0,
            TRIGGER,
        )?;
        let workers = (0..threads.max(1))
            .map(|n| {
                // clone: setup, one count of each per worker thread.
                let (root, queue, notify) = (Arc::clone(&root), queue.clone(), Arc::clone(&notify));
                std::thread::Builder::new()
                    // alloc: setup, once per thread.
                    .name(format!("blaze-kqueue-{n}"))
                    .spawn(move || work_on(&root, &queue, &notify))
            })
            // alloc: setup, once per driver.
            .collect::<io::Result<Vec<_>>>()?;
        fn tags<V>() -> HashMap<Tag, V> {
            HashMap::with_capacity_and_hasher(TAGS, Default::default())
        }
        let spawn_dirs = root.join(".blaze-spawn"); // alloc: setup, once per driver.
        Ok(Kqueue {
            start: Instant::now(),
            root,
            notify,
            files,
            watched,
            work: Some(work),
            backlog: VecDeque::with_capacity(FILES), // alloc: setup, once per driver.
            at_threads: 0,
            workers,
            confine: None,
            spawn_dirs,
            buffer: vec![0; 64 << 10], // alloc: setup, once per driver.
            // alloc: setup, once per driver: the lists of a wait at their usual most.
            changes: Vec::with_capacity(EVENTS),
            events: Vec::with_capacity(2 * EVENTS), // alloc: as above.
            ready: Vec::with_capacity(EVENTS),      // alloc: as above.
            owners: Owners::new().map_err(no_slab)?,
            timers: Wheel::try_new(wheel::MAX_TIMERS).map_err(no_slab)?,
            watches: Slab::new(MOST, MemTag::Io).map_err(no_slab)?,
            watch_tags: tags(),
            spare: Vec::with_capacity(SPARE_WATCHES), // alloc: setup, once per driver.
            untold: Vec::with_capacity(16),           // alloc: setup, once per driver.
            spawns: Slab::new(MOST, MemTag::Io).map_err(no_slab)?,
            spawn_tags: tags(),
            listens: Slab::new(MOST, MemTag::Io).map_err(no_slab)?,
            listen_tags: tags(),
            connecting: Slab::new(MOST, MemTag::Io).map_err(no_slab)?,
            conns: Slab::new(MOST, MemTag::Io).map_err(no_slab)?,
            in_flight: 0,
            #[cfg(test)]
            watch_overhead: 0,
        })
    }

    /// The driver, starting each process whose sandbox is on as `confine` makes it.
    // dyn: the platform's sandbox, given once at setup
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Kqueue {
        self.confine = Some(confine);
        self
    }

    /// The driver, making each sandboxed spawn's own directory under `dir`, made here.
    pub fn scratch(mut self, dir: impl Into<PathBuf>) -> io::Result<Kqueue> {
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
    fn register(&mut self, owner: Owner, ident: usize, filter: i16, flags: u16, fflags: u32) -> u64 {
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
            effect @ (Effect::ReadAll { .. }
            | Effect::Read { .. }
            | Effect::ReadLink { .. }
            | Effect::Append { .. }
            | Effect::Sync { .. }
            | Effect::Truncate { .. }
            | Effect::Write { .. }
            | Effect::WriteAt { .. }
            | Effect::Rename { .. }
            | Effect::Mode { .. }
            | Effect::Private { .. }
            | Effect::Times { .. }
            | Effect::Clone { .. }
            | Effect::Link { .. }
            | Effect::Stat { .. }
            | Effect::List { .. }
            | Effect::Remove { .. }
            | Effect::Stamp { .. }
            | Effect::Entries { .. }
            | Effect::Map { .. }) => {
                match self.at_threads < FILES && self.backlog.is_empty() {
                    true => self.to_threads(effect),
                    false => self.backlog.push_back(effect),
                }
                Ok(())
            }
            Effect::Timer { after, tag } => {
                let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                self.timers.insert(due, tag);
                Ok(())
            }
            Effect::Watch { dir, tag } => self.watch(dir, None, tag),
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
            Effect::Listen { at, tag } => self.listen(&at, tag),
            Effect::Connect { at, tag } => self.connect(at, tag),
            Effect::Recv { conn, most, into, tag } => {
                match self.connection(conn) {
                    Ok(connection) => connection.recvs.push_back((tag, most, into)),
                    Err(error) => self.ready.push(Completion { tag, done: Done::unread(into, error) }),
                }
                self.serve_recvs(conn_id(conn));
                Ok(())
            }
            Effect::Send { conn, bytes, tag } => {
                match self.connection(conn) {
                    Ok(connection) => connection.sends.push_back((tag, bytes, 0)),
                    Err(error) => self.ready.push(Completion { tag, done: Done::unsent(bytes, error) }),
                }
                self.serve_sends(conn_id(conn));
                Ok(())
            }
            Effect::Hangup { conn, tag } => self.hangup(conn, tag),
            Effect::Kill { .. } | Effect::Unwatch { .. } | Effect::Unlisten { .. } => {
                unreachable!("taken where they are submitted")
            }
        }
    }

    /// A file effect onto the threads' queue, which has room: at most [`FILES`] are there.
    fn to_threads(&mut self, effect: Effect) {
        let work = self.work.as_ref().expect("the driver is running");
        work.try_send(effect).expect("room for a file effect at the threads, which are running");
        self.at_threads += 1;
    }

    fn connection(&mut self, conn: Conn) -> io::Result<&mut Connection> {
        self.conns.get_mut(conn_id(conn)).ok_or_else(|| no_connection(conn))
    }

    /// Watches `dir` as `tag`, a journal of it from `since`: on a stopped watch kept from an
    /// earlier one where there is one. A watch of `tag` already ends first, telling nothing
    /// more.
    fn watch(&mut self, dir: FileName, since: Option<Cursor>, tag: Tag) -> io::Result<()> {
        if let Some(old) = self.watch_tags.remove(&tag) {
            self.retire(old);
        }
        let watch = match self.spare.pop() {
            Some(watch) => watch,
            // clone: one count per watch made, only when no spare one is left; never per event.
            None => fsevents::Watch::new(Watched(Arc::clone(&self.notify)))?,
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
                self.watch_tags.insert(tag, id);
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
            self.spare.push(watch);
        }
    }

    fn spawn(&mut self, command: Box<Command>, tag: Tag) -> io::Result<()> {
        let process::Started { pid, stdin, stdout, stderr, finish } =
            process::start(&self.spawn_dirs, command, self.confine.as_ref())?;
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
                self.spawn_tags.insert(tag, spawn);
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
            let id = self.register(owner, fd as usize, libc::EVFILT_READ, libc::EV_ADD, 0);
            self.spawns.get_mut(spawn).expect("followed as it is made").ids.push(id);
        }
        if let Some(fd) = stdin {
            set_nonblocking(fd)?;
            let id =
                self.register(Owner::Input(spawn), fd as usize, libc::EVFILT_WRITE, libc::EV_ADD, 0);
            self.spawns.get_mut(spawn).expect("followed as it is made").ids.push(id);
        }
        let (pid, flags) = (pid as usize, libc::EV_ADD | libc::EV_ONESHOT);
        let id = self.register(Owner::Exit(spawn), pid, libc::EVFILT_PROC, flags, libc::NOTE_EXIT);
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
        loop {
            // SAFETY: an open descriptor and a buffer of the length given.
            let n = unsafe { libc::read(fd, self.buffer.as_mut_ptr().cast(), self.buffer.len()) };
            match n {
                1.. => into.extend_from_slice(&self.buffer[..n as usize]),
                0 => {
                    *pipe = None;
                    break;
                }
                _ => {
                    let error = io::Error::last_os_error();
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
            // SAFETY: an open descriptor and `rest.len()` bytes to read.
            let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
            match n {
                1.. => spawned.written += n as usize,
                0 => {
                    spawned.failed.get_or_insert(io::ErrorKind::WriteZero.into());
                    break;
                }
                _ => {
                    let error = io::Error::last_os_error();
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

    fn listen(&mut self, at: &Address, tag: Tag) -> io::Result<()> {
        let context =
            // alloc: an error path.
            |error: io::Error| io::Error::new(error.kind(), format!("listening at {at:?}: {error}"));
        let listener = net::listen(&self.root, at).map_err(context)?;
        listener.set_nonblocking(true).map_err(context)?;
        let fd = listener.fd() as usize;
        let listen = self.listens.insert(Listening { tag, listener, id: TRIGGER });
        let id = self.register(Owner::Listen(listen), fd, libc::EVFILT_READ, libc::EV_ADD, 0);
        self.listens.get_mut(listen).expect("inserted above").id = id;
        // A listen of `tag` already ends, as the one it is replaced by begins.
        if let Some(old) = self.listen_tags.insert(tag, listen)
            && let Some(old) = self.listens.remove(old)
        {
            self.disown(old.id);
        }
        Ok(())
    }

    fn accept(&mut self, listen: ListenId) {
        loop {
            let Some(listening) = self.listens.get(listen) else { return };
            let tag = listening.tag;
            let accepted = listening.listener.accept().and_then(|socket| {
                socket.set_nonblocking(true)?;
                no_sigpipe(socket.fd())?;
                Ok(socket)
            });
            match accepted {
                Ok(socket) => {
                    let conn = self.adopt(socket);
                    self.ready.push(Completion { tag, done: Done::Accepted { conn } });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(error) => {
                    // A failure ends the listen.
                    self.end_listen(listen);
                    // alloc: an error path.
                    let done = Done::Failed(IoError(format!("accepting a connection: {error}")));
                    self.ready.push(Completion { tag, done });
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

    fn connect(&mut self, at: Address, tag: Tag) -> io::Result<()> {
        let context =
            // alloc: an error path.
            |error: io::Error| io::Error::new(error.kind(), format!("connecting to {at:?}: {error}"));
        let address = net::sockaddr(&self.root, &at).map_err(context)?;
        let fd = net::stream_socket(address.domain).map_err(context)?;
        set_nonblocking(fd.as_raw_fd()).map_err(context)?;
        no_sigpipe(fd.as_raw_fd()).map_err(context)?;
        let unix = address.domain == libc::AF_UNIX;
        // SAFETY: an address of `len` bytes, of the socket's domain.
        let connected = unsafe {
            libc::connect(fd.as_raw_fd(), (&raw const address.storage).cast(), address.len)
        };
        if connected == 0 {
            return self.connected(tag, fd, unix).map_err(context);
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EINTR)) {
            return Err(context(error));
        }
        let ident = fd.as_raw_fd() as usize;
        let flags = libc::EV_ADD | libc::EV_ONESHOT;
        let connecting = self.connecting.insert(Connecting { tag, fd, at, unix });
        self.register(Owner::Connecting(connecting), ident, libc::EVFILT_WRITE, flags, 0);
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
        let socket = net::Socket::from_fd(fd, unix);
        if let net::Socket::Tcp(stream) = &socket {
            stream.set_nodelay(true)?;
        }
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
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_ADD | libc::EV_ONESHOT,
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
                    Ok(0) => Done::unsent(std::mem::take(bytes), "the connection took no more bytes"),
                    Ok(n) => {
                        *put += n;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.changes.push(Kevent {
                            ident: connection.socket.fd() as usize,
                            filter: libc::EVFILT_WRITE,
                            flags: libc::EV_ADD | libc::EV_ONESHOT,
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
            let done = Done::unsent(bytes, format_args!("{conn:?} hung up before all of it was sent"));
            self.ready.push(Completion { tag: send, done });
        }
        shut?;
        self.ready.push(Completion { tag, done: Done::HungUp });
        Ok(())
    }

    /// Makes the changes asked for and waits up to `left` (forever for `None`), or until the
    /// next timer is due, for events, and acts on them; then fires the timers due.
    fn wait(&mut self, left: Option<Duration>) {
        let now = wheel::ticks(self.start.elapsed());
        let until_timer = self.timers.next_deadline().map(|due| Duration::from_nanos(due.saturating_sub(now)));
        let left = match (left, until_timer) {
            (Some(left), Some(until_timer)) => Some(left.min(until_timer)),
            (left, until_timer) => left.or(until_timer),
        };
        self.wait_events(left);
        let ready = &mut self.ready;
        self.timers.advance(wheel::ticks(self.start.elapsed()), |_, tag| {
            ready.push(Completion { tag, done: Done::Fired });
        });
    }

    fn wait_events(&mut self, left: Option<Duration>) {
        let timeout = left.map(|it| libc::timespec {
            tv_sec: libc::time_t::try_from(it.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_nsec: it.subsec_nanos().into(),
        });
        let timeout = timeout.as_ref().map_or(std::ptr::null(), |it| it as *const libc::timespec);
        let mut changes = std::mem::take(&mut self.changes);
        let mut events = std::mem::take(&mut self.events);
        // Room for a failure of every change besides the events: a failure with no room for it
        // would fail the whole call.
        let room = changes.len() + EVENTS;
        events.clear();
        events.reserve(room);
        // SAFETY: the changes and the room for events laid out as `kevent`s, and a timeout that
        // outlives the call.
        let got = unsafe {
            libc::kevent(
                self.notify.kq.as_raw_fd(),
                changes.as_ptr().cast(),
                changes.len() as libc::c_int,
                events.as_mut_ptr().cast(),
                room as libc::c_int,
                timeout,
            )
        };
        // Made before the call waits, interrupted or not.
        changes.clear();
        self.changes = changes;
        if got < 0 {
            let error = io::Error::last_os_error();
            assert!(error.kind() == io::ErrorKind::Interrupted, "waiting on the kqueue: {error}");
            self.events = events;
            return;
        }
        // SAFETY: the call wrote `got` events, at most the `room` it was given, which `reserve`
        // made capacity for; a `Kevent` is integers only, so any bytes it wrote are one. What
        // `udata` names is checked in `dispatch` (a slab lookup by index and generation),
        // never followed as a pointer.
        unsafe { events.set_len(got as usize) };
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
        if event.flags & libc::EV_ERROR != 0 {
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
                if error.raw_os_error() != Some(libc::ESRCH) {
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
                let done = Done::Failed(IoError(format!("connecting to {:?}: {error}", connecting.at)));
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
        }
    }

    /// Moves every completion ready into `out`: the polling thread's, then the file threads'
    /// (each making room at the threads for one from the backlog), then the watches'.
    fn take(&mut self, out: &mut Vec<Completion>) {
        out.append(&mut self.ready);
        // Cleared before the channels are read: a hand-back that finds it set was made before
        // this, so the reads below see it (`Wake`, model-checked in `handoff`).
        self.notify.woke.drain();
        while let Ok(completion) = self.files.try_recv() {
            self.at_threads -= 1;
            out.push(completion);
        }
        while self.at_threads < FILES {
            let Some(effect) = self.backlog.pop_front() else { break };
            self.to_threads(effect);
        }
        while let Ok(completion) = self.watched.try_recv() {
            out.push(completion);
        }
    }
}

impl Drop for Kqueue {
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

impl Driver for Kqueue {
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

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let before = out.len();
        let deadline = Instant::now().checked_add(wait);
        loop {
            self.take(out);
            if out.len() > before || self.in_flight == 0 {
                // Changes asked for are made now, not once nothing else is ready.
                if !self.changes.is_empty() {
                    self.wait(Some(Duration::ZERO));
                    self.take(out);
                }
                break;
            }
            let left = deadline.map(|it| it.saturating_duration_since(Instant::now()));
            self.wait(left);
            if left.is_some_and(|it| it.is_zero()) {
                self.take(out);
                break;
            }
        }
        let ended = out[before..]
            .iter()
            .filter(|it| {
                !matches!(it.done, Done::Changed { .. } | Done::Journaled(_) | Done::Accepted { .. })
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
}

/// Runs the file effects this thread takes off `queue`, which every file thread takes from
/// with no lock, until the driver drops its end. The completions channel has room for each:
/// the driver keeps at most [`FILES`] at the threads.
fn work_on(root: &Path, queue: &Receiver<Effect>, notify: &Notify) {
    while let Ok(effect) = queue.recv() {
        let sent = counted(notify, || {
            let tag = effect.tag();
            let done = crate::threads::run(root, effect)
                // alloc: an error path.
                .unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
            let sent = notify.files.send(Completion { tag, done }).is_ok();
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
    let mut on: libc::c_int = 1;
    // SAFETY: an open descriptor and an int the call reads.
    if unsafe { libc::ioctl(fd, libc::FIONBIO, &raw mut on) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A write to a socket whose peer is gone fails rather than raising SIGPIPE.
fn no_sigpipe(fd: RawFd) -> io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: an open socket and an int of the size given.
    let set = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw const on).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// How a nonblocking connect of `fd` ended.
fn socket_error(fd: RawFd) -> io::Result<()> {
    let mut error: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an open socket and an int of the size given to write.
    let got = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, (&raw mut error).cast(), &mut len)
    };
    if got != 0 {
        return Err(io::Error::last_os_error());
    }
    match error {
        0 => Ok(()),
        error => Err(io::Error::from_raw_os_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, sync::atomic::Ordering};

    use super::*;
    use crate::{Confined, Exit, Sandbox, Traced, run_until_idle, sim::Rng, threads::Threads};

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    fn dir(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-kq-{what}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        run_until_idle(driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, done.done));
        assert_eq!(driver.in_flight(), 0);
        seen
    }

    /// Polls until `out` holds `n` completions, or the deadline.
    fn poll_for(driver: &mut Kqueue, out: &mut Vec<Completion>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while out.len() < n {
            assert!(Instant::now() < deadline, "never came: {out:?}");
            driver.poll(out, Duration::from_millis(100));
        }
    }

    fn exit(done: &Done) -> &Exit {
        match done {
            Done::Exited(exit) => exit,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn processes_end_with_their_output_code_and_signal() {
        let root = dir("spawn");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let big: Vec<u8> = (0..1u32 << 20).map(|n| n as u8).collect();
        let script = "cat; echo err >&2; exit 7";
        let limited = Box::new(Command { memory: Some(1 << 30), ..*command("/bin/echo", &["hi"], b"") });
        let mut on = command("/bin/echo", &["hi"], b"");
        on.sandbox = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false, services: Vec::new() };
        let seen = run(&mut driver, vec![
            Effect::Spawn { command: command("/bin/sh", &["-c", script], b"hello"), tag: Tag(1) },
            Effect::Spawn { command: command("/bin/sh", &["-c", "kill -TERM $$"], b""), tag: Tag(2) },
            Effect::Spawn { command: command("/bin/sleep", &["60"], b""), tag: Tag(3) },
            Effect::Kill { tag: Tag(3) },
            Effect::Spawn { command: on, tag: Tag(4) },
            Effect::Spawn { command: limited, tag: Tag(5) },
            // More input than a pipe holds, read back while it is written.
            Effect::Spawn { command: command("/bin/cat", &[], &big), tag: Tag(6) },
            Effect::Kill { tag: Tag(99) },
        ]);
        let one = exit(&seen[&1]);
        assert_eq!((one.code, one.signal), (Some(7), None));
        assert_eq!((one.stdout.as_slice(), one.stderr.as_slice()), (&b"hello"[..], &b"err\n"[..]));
        assert!(one.peak_memory > 0);
        let two = exit(&seen[&2]);
        assert_eq!((two.code, two.signal), (None, Some(libc::SIGTERM)));
        let three = exit(&seen[&3]);
        assert_eq!((three.code, three.signal), (None, Some(libc::SIGKILL)));
        assert!(matches!(&seen[&4], Done::Failed(why) if why.0.contains("Confine")), "{:?}", seen[&4]);
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
        traced.sandbox = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: true, services: Vec::new() };
        let seen = run(&mut driver, vec![Effect::Spawn { command: traced, tag: Tag(1) }]);
        let exit = exit(&seen[&1]);
        assert_eq!((exit.code, exit.stdout.as_slice()), (Some(0), &b"hi\n"[..]));
        let wanted = Traced { files: vec![(PathBuf::from("/etc/hosts"), None)], ..Traced::default() };
        assert_eq!(exit.traced, Some(wanted));
        // Its scratch directory is gone.
        let left = std::fs::read_dir(root.join(".blaze-spawn")).unwrap().count();
        assert_eq!(left, 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Listens, connects, sends each way, hangs up each end and stops listening.
    fn round_trip(root: &Path, at: Address) {
        let mut driver = Kqueue::new(root, 1).unwrap();
        let mut effects = vec![
            Effect::Listen { at: at.clone(), tag: Tag(1) },
            Effect::Connect { at, tag: Tag(2) },
        ];
        let (mut server, mut client) = (None, None);
        let mut ends = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, effects| {
            match (done.tag.0, done.done) {
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
                    effects.push(Effect::Recv { conn: client, most: 64, into: Vec::new(), tag: Tag(6) });
                }
                (6, Done::Read(bytes)) => {
                    assert_eq!(bytes, b"pong");
                    effects.push(Effect::Hangup { conn: client.unwrap(), tag: Tag(7) });
                    effects.push(Effect::Recv { conn: server.unwrap(), most: 64, into: Vec::new(), tag: Tag(8) });
                }
                (8, Done::Read(bytes)) => {
                    assert!(bytes.is_empty(), "{bytes:?}");
                    effects.push(Effect::Hangup { conn: server.unwrap(), tag: Tag(9) });
                    effects.push(Effect::Unlisten { tag: Tag(1) });
                }
                (tag @ (4 | 5), Done::Sent(bytes)) if bytes.is_empty() => ends.push(tag),
                (tag @ (7 | 9), Done::HungUp) | (tag @ 1, Done::Unlistened) => ends.push(tag),
                (tag, other) => panic!("{tag}: {other:?}"),
            }
        });
        ends.sort();
        assert_eq!(ends, [1, 4, 5, 7, 9]);
        assert_ne!(server, client);
        assert_eq!(driver.in_flight(), 0);
        assert!(driver.conns.is_empty() && driver.listens.is_empty() && driver.connecting.is_empty());
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
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = free.local_addr().unwrap();
        drop(free);
        let root = dir("tcp");
        round_trip(&root, Address::Tcp(at));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn what_names_nothing_fails() {
        let root = dir("nothing");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let seen = run(&mut driver, vec![
            Effect::Unwatch { tag: Tag(1) },
            Effect::Unlisten { tag: Tag(2) },
            Effect::Recv { conn: Conn(77), most: 8, into: b"kept".to_vec(), tag: Tag(3) },
            Effect::Hangup { conn: Conn(77), tag: Tag(4) },
            Effect::Connect { at: Address::Unix(name("nobody")), tag: Tag(5) },
            Effect::Kill { tag: Tag(6) },
        ]);
        assert_eq!(seen.keys().copied().collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
        assert!(matches!(&seen[&3], Done::Broken { buffer, .. } if buffer == b"kept"), "{seen:?}");
        assert!(seen.iter().all(|(tag, it)| *tag == 3 || matches!(it, Done::Failed(_))), "{seen:?}");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timers_fire_in_their_order() {
        let mut driver = Kqueue::new(dir("timers"), 1).unwrap();
        let mut effects: Vec<Effect> = (0..3)
            .map(|n| Effect::Timer { after: Duration::from_millis(10 * (3 - n)), tag: Tag(n) })
            .collect();
        effects.push(Effect::Timer { after: Duration::ZERO, tag: Tag(3) });
        let mut fired = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| {
            assert_eq!(done.done, Done::Fired);
            fired.push(done.tag.0);
        });
        assert_eq!(fired, [3, 2, 1, 0]);
    }

    #[test]
    fn a_wait_ends_at_its_time() {
        let mut driver = Kqueue::new(dir("wait"), 1).unwrap();
        driver.submit(&mut vec![Effect::Timer { after: Duration::from_secs(60), tag: Tag(1) }]);
        let began = Instant::now();
        let mut out = Vec::new();
        assert_eq!(driver.poll(&mut out, Duration::from_millis(20)), 0);
        let waited = began.elapsed();
        assert!(waited >= Duration::from_millis(19) && waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!(driver.in_flight(), 1);
    }

    #[test]
    fn a_mapping_is_the_file() {
        let root = dir("map");
        let mut driver = Kqueue::new(&root, 2).unwrap();
        let file = name("store/pack");
        let seen = run(&mut driver, vec![Effect::Write {
            file: file.clone(),
            bytes: b"packed".to_vec(),
            tag: Tag(1),
        }]);
        assert_eq!(seen[&1], Done::Written);
        let seen = run(&mut driver, vec![
            Effect::Map { file, tag: Tag(2) },
            Effect::Map { file: name("missing"), tag: Tag(3) },
        ]);
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
                        let entries: Vec<_> =
                            entries.iter().map(|(name, meta)| (name.as_str(), meta.len, meta.dir)).collect();
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
                    15 => Effect::WriteAt { file: file.clone(), at: 6, bytes: b"!".to_vec(), tag: Tag(16) },
                    16 => Effect::ReadAll { file: file.clone(), tag: Tag(17) },
                    17 => Effect::Mode { file: file.clone(), mode: 0o600, tag: Tag(18) },
                    18 => Effect::Rename { from: file.clone(), to: name("vfs/moved"), tag: Tag(19) },
                    19 => Effect::List { dir: name("vfs"), tag: Tag(20) },
                    20 => Effect::List { dir: name("missing"), tag: Tag(21) },
                    21 => Effect::WriteAt { file: name("missing"), at: 0, bytes: vec![1], tag: Tag(22) },
                    _ => return,
                };
                effects.push(next);
            });
            seen
        }
        let (ours, theirs) = (dir("files"), dir("files-threads"));
        let kqueue = script(&mut Kqueue::new(&ours, 2).unwrap(), &file);
        let threads = script(&mut Threads::new(&theirs, 2), &file);
        assert_eq!(kqueue, threads);
        assert_eq!(kqueue[5], format!("{:?}", Done::Read(b"onet".to_vec())));
        assert_eq!(kqueue[3], format!("{:?}", Done::Read(b"etw".to_vec())));
        assert_eq!(kqueue[6], "stat 4 false");
        assert!(kqueue[7].starts_with("stat ") && kqueue[7].ends_with(" true"), "{}", kqueue[7]);
        assert_eq!(kqueue[14], "failed");
        // Past the end, after the four bytes truncation left: a hole of zeros, then the byte.
        assert_eq!(kqueue[16], format!("{:?}", Done::Read(b"onet\0\0!".to_vec())));
        assert_eq!(kqueue[17], format!("{:?}", Done::Moded));
        assert_eq!(kqueue[18], format!("{:?}", Done::Renamed));
        assert!(kqueue[19].starts_with("listed ") && kqueue[19].contains("moved"), "{}", kqueue[19]);
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
            .map(|at| Effect::Append { file: file.clone(), bytes: at.to_le_bytes().to_vec(), tag: Tag(at) })
            .collect();
        let seen = run(&mut driver, effects);
        assert_eq!(seen.len() as u64, n);
        assert!(seen.values().all(|it| *it == Done::Appended));
        assert!(driver.backlog.is_empty() && driver.at_threads == 0);
        let bytes = std::fs::read(root.join("log")).unwrap();
        let wanted: Vec<u8> = (0..n).flat_map(u64::to_le_bytes).collect();
        assert!(bytes == wanted, "appended out of order");
        drop(driver);
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
                assert!(Instant::now() < deadline, "the threads handed back {} of {N}", driver.files.len());
                std::thread::yield_now();
            }
            let triggers = driver.notify.triggers.load(Ordering::Relaxed);
            assert_eq!(triggers, batch, "batch {batch}: {triggers} triggers in all for {N} hand-backs each");
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
            // SAFETY: an open descriptor.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK, 0);
            set_nonblocking(fd).unwrap();
            // SAFETY: as above.
            assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK, 0);
        }
        let mut byte = [0u8; 1];
        // SAFETY: an open descriptor and a buffer of the length given.
        let n = unsafe { libc::read(read.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
        let error = io::Error::last_os_error();
        assert_eq!((n, error.kind()), (-1, io::ErrorKind::WouldBlock));
        drop(write);
    }

    #[test]
    fn a_watch_hears_a_file_written_under_it_then_ends() {
        let root = dir("watch");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(&mut driver, vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }]);
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
            driver.poll(&mut done, Duration::from_millis(50));
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
        run(&mut driver, vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }]);
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
                driver.poll(&mut done, Duration::from_millis(50));
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
        let mut effects = vec![Effect::Flush { watch: Tag(8), tag: Tag(9) }, Effect::Unwatch { tag: Tag(7) }];
        let mut refused = false;
        run_until_idle(&mut driver, &mut effects, |it, _| match (it.tag.0, it.done) {
            (9, Done::Failed(_)) => refused = true,
            (7, Done::Changed { .. } | Done::Unwatched) => {}
            other => panic!("{other:?}"),
        });
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
        run(&mut driver, vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }]);
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
        });
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
            driver.poll(&mut done, Duration::from_millis(50));
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
        });
        assert_eq!(driver.in_flight(), 0);
    }

    #[test]
    fn a_journal_resumes_from_its_cursor_through_the_driver() {
        let root = dir("journal");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(&mut driver, vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }]);
        driver.submit(&mut vec![Effect::Journal {
            dir: name("src"),
            since: crate::Cursor::None,
            tag: Tag(7),
        }]);
        let batches = journal_until(&mut driver, Tag(7), |it| it.caught_up);
        assert!(matches!(batches[0].cursor, crate::Cursor::FsEvents { .. }), "{batches:?}");
        assert!(!batches[0].caught_up && batches[0].lost.is_empty());
        // A batch does not end it.
        assert_eq!(driver.in_flight(), 1);
        let cursor = batches.last().unwrap().cursor;
        unwatch(&mut driver, Tag(7));
        std::fs::write(root.join("src/while-down.rs"), b"fn f() {}").unwrap();
        fsevents::fseventsd_keeps(Duration::from_secs(1));
        driver.submit(&mut vec![Effect::Journal { dir: name("src"), since: cursor, tag: Tag(8) }]);
        let batches = journal_until(&mut driver, Tag(8), |it| it.caught_up);
        let told: Vec<_> = batches.iter().flat_map(|it| &it.files).collect();
        assert!(told.contains(&&name("src/while-down.rs")), "{batches:?}");
        assert!(batches.iter().all(|it| it.lost.is_empty()), "{batches:?}");
        assert_eq!(driver.in_flight(), 1);
        unwatch(&mut driver, Tag(8));
        // Of another store: lost, the whole directory.
        let other = crate::Cursor::FsEvents { event: 1, volume: [7; 16] };
        driver.submit(&mut vec![Effect::Journal { dir: name("src"), since: other, tag: Tag(9) }]);
        let batches = journal_until(&mut driver, Tag(9), |_| true);
        assert_eq!(batches[0].lost, [(name("src"), crate::Lost::Reset)]);
        unwatch(&mut driver, Tag(9));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stamps_and_entries_are_the_file_systems() {
        let root = dir("stamps");
        let mut driver = Kqueue::new(&root, 2).unwrap();
        run(&mut driver, vec![
            Effect::Write { file: name("src/lib.rs"), bytes: b"fn f() {}".to_vec(), tag: Tag(1) },
            Effect::Write { file: name("src/deep/mod.rs"), bytes: Vec::new(), tag: Tag(2) },
        ]);
        std::os::unix::fs::symlink("lib.rs", root.join("src/link")).unwrap();
        let mut names = crate::Names::default();
        for it in ["src/lib.rs", "src/deep", "src/link", "src/missing", "src/lib.rs/under"] {
            assert!(names.push_str(it));
        }
        let seen = run(&mut driver, vec![
            Effect::Stamp { files: Arc::new(names.clone()), tag: Tag(3) },
            Effect::Entries { dir: name("src"), tag: Tag(4) },
            Effect::Entries { dir: name("missing"), tag: Tag(5) },
        ]);
        let stamps = crate::fs::stamps(&root, &names).unwrap();
        assert_eq!(seen[&3], Done::Stamped(stamps.clone()));
        assert!(stamps[0].is_file() && stamps[0].size == 9, "{stamps:?}");
        assert!(stamps[1].is_dir() && stamps[2].is_link(), "{stamps:?}");
        assert!(stamps[3].is_gone() && stamps[4].is_gone(), "{stamps:?}");
        let entries = crate::fs::entries(&root.join("src"), &name("src")).unwrap();
        assert_eq!(seen[&4], Done::Entries(entries.clone()));
        let named: Vec<_> = entries.iter().map(|(it, _)| it.as_str()).collect();
        assert_eq!(named, ["src/deep", "src/lib.rs", "src/link"]);
        assert!(matches!(seen[&5], Done::Failed(_)));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The map of registrations the driver kept before its slab: ids counted up from the
    /// trigger's, never reused.
    struct OldOwners {
        next: u64,
        owners: HashMap<u64, Owner>,
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
            let mut old = OldOwners { next: TRIGGER + 1, owners: HashMap::default() };
            // Each registration made: its id in each, and whether it is live.
            let mut made: Vec<(u64, u64, bool)> = Vec::new();
            for _ in 0..500 {
                match rng.below(8) {
                    0..=2 => {
                        let owner = any_owner(&mut rng);
                        let (id, old_id) = (owners.own(owner), old.own(owner));
                        assert_ne!(id, TRIGGER, "seed {seed}");
                        assert!(made.iter().all(|it| it.0 != id), "seed {seed}: {id:#x} issued twice");
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
            let fd = net::stream_socket(libc::AF_UNIX).unwrap();
            driver.connecting.insert(Connecting { tag, fd, at: Address::Unix(name("x")), unix: true })
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
            let flags = libc::EV_ADD | libc::EV_CLEAR;
            driver.notify.change(1, libc::EVFILT_USER, flags, 0, 0, id).unwrap();
            driver.notify.change(1, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0, id).unwrap();
        };
        user(&driver, ended);
        driver.wait(Some(Duration::from_millis(50)));
        let mut out = Vec::new();
        driver.take(&mut out);
        assert!(out.is_empty(), "{out:?}");
        assert!(driver.connecting.contains(second), "the live connect is left as it is");
        assert!(driver.connecting.get(first).is_none());
        user(&driver, live);
        let deadline = Instant::now() + Duration::from_secs(5);
        while out.is_empty() && Instant::now() < deadline {
            driver.wait(Some(Duration::from_millis(50)));
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
        run(&mut driver, vec![Effect::Write { file: file.clone(), bytes: b"bytes".to_vec(), tag: Tag(1) }]);
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
                assert_eq!(used.allocations, 0, "round {round}: a warm Sync, Stat and Read, on the driver's thread");
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
        run(&mut driver, vec![Effect::Write { file: file.clone(), bytes: b"bytes".to_vec(), tag: Tag(1) }]);
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
                assert_eq!(used.allocations, 0, "round {round}: a warm set of times, on the driver's thread");
            }
            assert_eq!(out.as_slice(), [Completion { tag: Tag(round), done: Done::Timed }]);
            out.clear();
        }
        let threads = driver.notify.file_allocations.load(Ordering::Relaxed);
        assert_eq!(threads, 0, "the file thread: {threads} allocations over {ROUNDS} warm sets of times");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_warm_timer_arm_and_fire_allocates_nothing() {
        let mut driver = Kqueue::new(dir("budget-timers"), 1).unwrap();
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
        run(&mut driver, vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }]);
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
                let (_, used) =
                    blaze_alloc_count::measure(|| driver.poll(&mut out, Duration::from_millis(50)));
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
                    driver.poll(&mut out, Duration::from_millis(50));
                }
            });
            on_driver += used.allocations;
            for it in out.drain(..) {
                assert!(matches!(it.done, Done::Changed { .. } | Done::Unwatched), "{it:?}");
            }
            assert_eq!(driver.in_flight(), 0);
        }
        assert_eq!(on_driver, 0, "four warm watch cycles, on the driver's thread");
        assert_eq!(driver.watch_overhead, 0, "the watches' callbacks, beyond what Done::Changed carries");
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
            Effect::Listen { at: at.clone(), tag: Tag(1) },
            Effect::Connect { at: at.clone(), tag: Tag(2) },
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
                assert_eq!(used.allocations, 0, "round {round}: a warm send and receive, their buffers reused");
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
        assert!(sending.capacity() >= 4 && receiving.capacity() >= 64, "the buffers keep their room");
        effects.extend([Effect::Hangup { conn: client, tag: Tag(20) }, Effect::Hangup { conn: server, tag: Tag(21) }]);
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
        effects.push(Effect::Connect { at, tag: Tag(3) });
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
            out.iter().all(|it| matches!(&it.done, Done::Broken { buffer, .. } if buffer.is_empty())),
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
}
