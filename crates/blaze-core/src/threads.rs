//! A [`Driver`] on the real file system: a few threads run blocking calls, and the thread that
//! polls keeps the timers. Portable: the reference the other drivers are tested against. Two
//! appends to one file in flight at once may land in either order, as with any asynchronous
//! driver; a state machine that needs their order waits for the first.
//!
//! What may wait on a peer for as long as the peer likes (a process, a listen, a receive, a
//! send, a connect, a watch on Linux) is an errand, run by a pool of threads that grows to the
//! most errands at once and keeps them: a warm driver starts no thread. A process's input and
//! output are one poll loop on its errand's thread, not a thread per pipe.
//!
//! Every thread tells the driver through one mailbox, a queue under a lock that never blocks
//! the teller; the polling thread takes everything in it at once, swapping its queue for an
//! empty one, so neither side allocates once both have grown to the most told between polls.
//! What the driver says itself (a failure it sees at once, a timer, a watch's end on macOS)
//! goes straight to its own list of ready completions.
//!
//! Sockets, spawns, listens and watches are the driver's, in slabs ([`tagged::Tagged`] where a
//! tag names them): a [`crate::Conn`] is a slab handle's bits, so a stale one is detected, and
//! what a thread tells of an entry carries its handle, so a completion of one that was
//! replaced or ended is dropped rather than told as another's.

use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex, MutexGuard};

#[cfg(unix)]
use blaze_mem::{Slab, Tag as MemTag};

use crate::{
    Completion, Confine, Done, Driver, Effect, IoError, Tag, fs,
    wheel::{self, Wheel},
};

/// Tags in flight at once a driver makes room for before it grows.
const RESERVE: usize = 256;

/// Stopped watches kept for the next.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const SPARE: usize = 16;

/// The most connections at once.
#[cfg(unix)]
const CONNECTIONS: u32 = 1 << 20;

/// Why a process or a socket fails here.
#[cfg(not(unix))]
const UNIX_ONLY: &str =
    "processes and sockets are built on Unix only; Windows' are lane usercode's (AppContainer and job objects)";

/// Values in slots of their own, each named by a [`Tag`] and by a generational handle: what a
/// driver keeps of the spawns, watches and listens in flight. A handle of one removed, or
/// replaced by another of its tag, names nothing.
pub(crate) mod tagged {
    use blaze_hash::HashMap;
    use blaze_mem::{Slab, Tag as MemTag};

    use crate::Tag;

    /// The most in flight at once.
    const MOST: u32 = 1 << 20;

    pub(crate) struct Entry<T> {
        tag: Tag,
        value: T,
    }

    pub(crate) type Id<T> = blaze_mem::Handle<Entry<T>>;

    pub(crate) struct Tagged<T> {
        slab: Slab<Entry<T>>,
        by_tag: HashMap<Tag, Id<T>>,
    }

    impl<T> Tagged<T> {
        /// Room for `reserve` tags before the map grows; the slab's room is reserved address
        /// space, committed as it fills.
        pub(crate) fn new(reserve: usize) -> Tagged<T> {
            // alloc: none, address space reserved.
            let slab = match Slab::new(MOST, MemTag::Io) {
                Ok(it) => it,
                Err(error) => panic!("{error}"),
            };
            // alloc: once per driver, the room of the tags in flight at once.
            let by_tag = HashMap::with_capacity_and_hasher(reserve, Default::default());
            Tagged { slab, by_tag }
        }

        /// `value` as `tag`'s, and the value `tag` named before, whose handle names nothing
        /// now.
        pub(crate) fn insert(&mut self, tag: Tag, value: T) -> (Id<T>, Option<T>) {
            let old = self.remove(tag);
            let id = self.slab.insert(Entry { tag, value });
            self.by_tag.insert(tag, id);
            (id, old)
        }

        pub(crate) fn id(&self, tag: Tag) -> Option<Id<T>> {
            self.by_tag.get(&tag).copied()
        }

        pub(crate) fn get(&self, tag: Tag) -> Option<&T> {
            self.slab.get(self.id(tag)?).map(|it| &it.value)
        }

        pub(crate) fn get_mut(&mut self, tag: Tag) -> Option<&mut T> {
            let id = self.id(tag)?;
            self.slab.get_mut(id).map(|it| &mut it.value)
        }

        pub(crate) fn remove(&mut self, tag: Tag) -> Option<T> {
            let id = self.by_tag.remove(&tag)?;
            self.slab.remove(id).map(|it| it.value)
        }

        pub(crate) fn by_id(&self, id: Id<T>) -> Option<(Tag, &T)> {
            self.slab.get(id).map(|it| (it.tag, &it.value))
        }

        pub(crate) fn by_id_mut(&mut self, id: Id<T>) -> Option<(Tag, &mut T)> {
            self.slab.get_mut(id).map(|it| (it.tag, &mut it.value))
        }

        /// The value `id` names, taken out with its tag.
        pub(crate) fn remove_id(&mut self, id: Id<T>) -> Option<(Tag, T)> {
            let entry = self.slab.remove(id)?;
            if self.by_tag.get(&entry.tag) == Some(&id) {
                self.by_tag.remove(&entry.tag);
            }
            Some((entry.tag, entry.value))
        }

        /// In slot order: the same for the same inserts and removes.
        pub(crate) fn iter(&self) -> impl Iterator<Item = (Id<T>, Tag, &T)> {
            self.slab.iter().map(|(id, it)| (id, it.tag, &it.value))
        }

        #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
        pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (Id<T>, Tag, &mut T)> {
            self.slab.iter_mut().map(|(id, it)| (id, it.tag, &mut it.value))
        }

        pub(crate) fn len(&self) -> usize {
            self.slab.len()
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.slab.is_empty()
        }

        pub(crate) fn clear(&mut self) {
            self.slab.clear();
            self.by_tag.clear();
        }
    }

    #[cfg(test)]
    mod tests {
        use blaze_hash::HashMap;

        use super::Tagged;
        use crate::{Tag, sim::Rng};

        #[test]
        fn tagged_answers_as_a_map_of_tags_did() {
            for seed in 0..50 {
                let mut rng = Rng::new(seed);
                let mut tagged = Tagged::new(4);
                let mut model: HashMap<Tag, u64> = HashMap::default();
                let mut ids = Vec::new();
                for step in 0..2000u64 {
                    let tag = Tag(rng.below(16));
                    match rng.below(4) {
                        0 | 1 => {
                            let (id, old) = tagged.insert(tag, step);
                            assert_eq!(old, model.insert(tag, step), "seed {seed} step {step}");
                            ids.push(id);
                        }
                        2 => assert_eq!(tagged.remove(tag), model.remove(&tag), "seed {seed} step {step}"),
                        _ if !ids.is_empty() => {
                            let id = ids[rng.below(ids.len() as u64) as usize];
                            let live = tagged.by_id(id).map(|(tag, &value)| (tag, value));
                            if let Some((tag, value)) = live {
                                assert_eq!(model.get(&tag), Some(&value), "seed {seed} step {step}");
                                assert_eq!(tagged.id(tag), Some(id), "seed {seed} step {step}");
                                if rng.one_in(2) {
                                    assert_eq!(tagged.remove_id(id), Some((tag, value)));
                                    model.remove(&tag);
                                }
                            }
                        }
                        _ => {}
                    }
                    assert_eq!(tagged.len(), model.len(), "seed {seed} step {step}");
                    assert_eq!(tagged.get(tag), model.get(&tag), "seed {seed} step {step}");
                }
            }
        }

        #[test]
        fn a_warm_tagged_allocates_nothing() {
            let mut tagged = Tagged::new(64);
            for n in 0..64 {
                tagged.insert(Tag(n), n);
            }
            tagged.clear();
            blaze_alloc_count::assert_at_most(0, "a warm insert, lookup and remove", || {
                for n in 0..64 {
                    let (id, _) = tagged.insert(Tag(n), n);
                    assert_eq!(tagged.by_id(id), Some((Tag(n), &n)));
                }
                for n in 0..64 {
                    assert_eq!(tagged.remove(Tag(n)), Some(n));
                }
            });
        }
    }
}

#[cfg_attr(not(unix), allow(unused_imports))]
use tagged::{Id, Tagged};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

/// A queue many threads put into and one or a few take from, under a lock. A put never
/// blocks, and wakes a taker only when one waits.
struct Mailbox<T> {
    state: Mutex<Inbox<T>>,
    ready: Condvar,
}

struct Inbox<T> {
    items: VecDeque<T>,
    waiting: usize,
    closed: bool,
}

impl<T> Mailbox<T> {
    fn new(room: usize) -> Mailbox<T> {
        // alloc: once per driver, the room of what is told between two polls.
        let items = VecDeque::with_capacity(room);
        Mailbox { state: Mutex::new(Inbox { items, waiting: 0, closed: false }), ready: Condvar::new() }
    }

    /// `item` put in, or back once the mailbox is closed.
    fn push(&self, item: T) -> Result<(), T> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(item);
        }
        state.items.push_back(item);
        if state.waiting > 0 {
            self.ready.notify_one();
        }
        Ok(())
    }

    /// The next item, waiting for one; `None` once it is closed and empty.
    fn pop(&self) -> Option<T> {
        let mut state = lock(&self.state);
        loop {
            if let Some(item) = state.items.pop_front() {
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state.waiting += 1;
            self.ready.wait(&mut state);
            state.waiting -= 1;
        }
    }

    /// Everything in it moved to `into`, waiting up to `wait` for something when it is empty:
    /// swapped for `into`'s queue when that is empty, so both keep their room.
    fn take_all(&self, into: &mut VecDeque<T>, wait: Duration) {
        let mut state = lock(&self.state);
        if state.items.is_empty() && !state.closed && !wait.is_zero() {
            state.waiting += 1;
            self.ready.wait_while_for(&mut state, |it| it.items.is_empty() && !it.closed, wait);
            state.waiting -= 1;
        }
        match into.is_empty() {
            true => std::mem::swap(&mut state.items, into),
            false => into.append(&mut state.items),
        }
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
    }
}

/// What a thread tells the driver.
enum Told {
    /// A completion that ends the effect of its tag.
    Done(Completion),
    /// What a watch of FSEvents' heard: a batch of the watch's, or a flush's end.
    #[cfg(target_os = "macos")]
    Heard(Completion),
    /// What the watch `watch` heard, or its end.
    #[cfg(target_os = "linux")]
    Watch { watch: Id<Watching>, completion: Completion },
    /// The spawn `spawn` ended.
    #[cfg(unix)]
    Exited { spawn: Id<Arc<Life>>, completion: Completion },
    #[cfg(unix)]
    Connected { tag: Tag, socket: crate::net::Socket },
    /// A connection the listen `listen` accepted.
    #[cfg(unix)]
    Accepted { listen: Id<Listening>, tag: Tag, socket: crate::net::Socket },
    /// The listen `listen` ended.
    #[cfg(unix)]
    Listened { listen: Id<Listening>, completion: Completion },
}

/// What the driver's threads share with it.
struct Shared {
    root: PathBuf,
    done: Mailbox<Told>,
    files: Mailbox<Effect>,
    #[cfg(unix)]
    errands: Mutex<Errands>,
    #[cfg(unix)]
    errand: Condvar,
    #[cfg(test)]
    off: OffThread,
}

/// What the driver's threads allocated, for the allocation-budget tests: each file effect and
/// each errand is measured on the thread that runs it, and what a completion carries apart.
#[cfg(test)]
#[derive(Default)]
struct OffThread {
    allocations: std::sync::atomic::AtomicU64,
    payload: std::sync::atomic::AtomicU64,
}

impl Shared {
    /// Runs `f`, a file effect or an errand: counted in the allocation-budget tests.
    #[inline]
    fn counted<R>(&self, f: impl FnOnce() -> R) -> R {
        #[cfg(test)]
        {
            let (result, used) = blaze_alloc_count::measure(f);
            self.off.allocations.fetch_add(used.allocations, std::sync::atomic::Ordering::Relaxed);
            result
        }
        #[cfg(not(test))]
        f()
    }

    /// Runs `f`, which makes what a completion carries: counted apart in the
    /// allocation-budget tests.
    #[inline]
    #[cfg_attr(not(unix), allow(dead_code))]
    fn payload<R>(&self, f: impl FnOnce() -> R) -> R {
        #[cfg(test)]
        {
            let (result, used) = blaze_alloc_count::measure(f);
            self.off.payload.fetch_add(used.allocations, std::sync::atomic::Ordering::Relaxed);
            result
        }
        #[cfg(not(test))]
        f()
    }
}

pub struct Threads {
    start: Instant,
    shared: Arc<Shared>,
    /// What the threads told, taken from their mailbox at once.
    inbox: VecDeque<Told>,
    /// Completions to hand out at the next poll, in order.
    ready: Vec<Completion>,
    /// How many of `ready` end an effect in flight.
    ready_ends: usize,
    workers: Vec<JoinHandle<()>>,
    /// By ticks since `start`.
    timers: Wheel<Tag>,
    watches: Tagged<Watching>,
    /// Stopped watches kept for the next.
    #[cfg(target_os = "linux")]
    spare: Vec<Arc<linux::Control>>,
    #[cfg(target_os = "macos")]
    spare: Vec<crate::fsevents::Watch<Sink>>,
    /// Flushes a watch being stopped had not told.
    #[cfg(target_os = "macos")]
    untold: Vec<Tag>,
    // dyn: the platform's sandbox, given by whoever owns it; called once per spawn.
    confine: Option<Arc<dyn Confine>>,
    #[cfg(unix)]
    spawns: Tagged<Arc<Life>>,
    #[cfg(unix)]
    listens: Tagged<Listening>,
    /// Each connection's [`crate::Conn`] is its handle's bits.
    #[cfg(unix)]
    conns: Slab<Connection>,
    in_flight: usize,
}

impl Threads {
    /// A driver whose files are under `root`, with `threads` threads for blocking calls.
    pub fn new(root: impl Into<PathBuf>, threads: usize) -> Threads {
        // alloc: once per driver, the state its threads share.
        let shared = Arc::new(Shared {
            root: root.into(),
            done: Mailbox::new(RESERVE),
            files: Mailbox::new(RESERVE),
            #[cfg(unix)]
            // alloc: once per driver, the room of the errands asked for at once.
            errands: Mutex::new(Errands { errands: VecDeque::with_capacity(RESERVE), idle: 0, closed: false }),
            #[cfg(unix)]
            errand: Condvar::new(),
            #[cfg(test)]
            off: OffThread::default(),
        });
        let workers = (0..threads.max(1))
            .map(|n| {
                let shared = Arc::clone(&shared); // clone: once per worker, the state it shares.
                std::thread::Builder::new()
                    // alloc: once per worker, its name.
                    .name(format!("blaze-io-{n}"))
                    .spawn(move || work_on(&shared))
                    .expect("starting an IO thread")
            })
            // alloc: once per driver.
            .collect();
        Threads {
            start: Instant::now(),
            shared,
            // alloc: once per driver, swapped with the mailbox's queue.
            inbox: VecDeque::with_capacity(RESERVE),
            // alloc: once per driver.
            ready: Vec::with_capacity(RESERVE),
            ready_ends: 0,
            workers,
            timers: Wheel::new(),
            watches: Tagged::new(RESERVE),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            // alloc: once per driver.
            spare: Vec::with_capacity(SPARE),
            #[cfg(target_os = "macos")]
            // alloc: once per driver.
            untold: Vec::with_capacity(4),
            confine: None,
            #[cfg(unix)]
            spawns: Tagged::new(RESERVE),
            #[cfg(unix)]
            listens: Tagged::new(RESERVE),
            #[cfg(unix)]
            // alloc: none, address space reserved.
            conns: match Slab::new(CONNECTIONS, MemTag::Io) {
                Ok(it) => it,
                Err(error) => panic!("{error}"),
            },
            in_flight: 0,
        }
    }

    /// The same driver, making the sandbox of each spawn whose sandbox is on with `confine`.
    // dyn: as `Threads::confine`.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Threads {
        self.confine = Some(confine);
        self
    }

    /// The address the TCP listen `listen` was given (its port, where it asked for port 0);
    /// `None` for a Unix socket or a listen not in flight.
    #[cfg(unix)]
    pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr> {
        self.listens.get(listen)?.local
    }

    /// What the driver's threads allocated since last asked, and how much of it completions
    /// carry.
    #[cfg(test)]
    pub(crate) fn off_thread(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        let off = &self.shared.off;
        (off.allocations.swap(0, Ordering::Relaxed), off.payload.swap(0, Ordering::Relaxed))
    }

    /// `completion`, which ends an effect in flight, for the next poll.
    fn end(&mut self, completion: Completion) {
        self.ready.push(completion);
        self.ready_ends += 1;
    }

    /// Says `done` of `tag`, an effect that ends as it is asked for.
    fn tell(&mut self, tag: Tag, done: Done) {
        self.in_flight += 1;
        self.end(Completion { tag, done });
    }

    fn fire_timers(&mut self) {
        let (ready, ends) = (&mut self.ready, &mut self.ready_ends);
        self.timers.advance(wheel::ticks(self.start.elapsed()), |_, tag| {
            ready.push(Completion { tag, done: Done::Fired });
            *ends += 1;
        });
    }

    /// Takes in what the threads told.
    fn take_told(&mut self) {
        while let Some(told) = self.inbox.pop_front() {
            self.took(told);
        }
    }

    fn took(&mut self, told: Told) {
        match told {
            Told::Done(completion) => self.end(completion),
            #[cfg(target_os = "macos")]
            Told::Heard(completion) => match completion.done {
                Done::Flushed => self.end(completion),
                // A watch's batches are all taken in before it is stopped (`retire`).
                _ => self.ready.push(completion),
            },
            #[cfg(target_os = "linux")]
            Told::Watch { watch, completion } => {
                // A watch replaced by another of its tag tells nothing more.
                if self.watches.by_id(watch).is_none() {
                    return;
                }
                match completion.done {
                    Done::Changed { .. } | Done::Journaled(_) => self.ready.push(completion),
                    _ => {
                        if let Some((_, watching)) = self.watches.remove_id(watch) {
                            self.keep(watching.control);
                        }
                        self.end(completion);
                    }
                }
            }
            #[cfg(unix)]
            Told::Exited { spawn, completion } => {
                self.spawns.remove_id(spawn);
                self.end(completion);
            }
            #[cfg(unix)]
            Told::Connected { tag, socket } => {
                let conn = self.connection(socket);
                self.end(Completion { tag, done: Done::Connected { conn } });
            }
            #[cfg(unix)]
            Told::Accepted { listen, tag, socket } => {
                // Of a listen replaced by another of its tag: closed as it is dropped.
                if self.listens.by_id(listen).is_some() {
                    let conn = self.connection(socket);
                    self.ready.push(Completion { tag, done: Done::Accepted { conn } });
                }
            }
            #[cfg(unix)]
            Told::Listened { listen, completion } => {
                if self.listens.remove_id(listen).is_some() {
                    self.end(completion);
                }
            }
        }
    }

    #[cfg(unix)]
    fn connection(&mut self, socket: crate::net::Socket) -> crate::Conn {
        // alloc: once per connection, shared with the errands that read and write it.
        let socket = Arc::new(socket);
        crate::Conn(self.conns.insert(Connection { socket }).to_bits())
    }

    #[cfg(unix)]
    fn socket(&self, conn: crate::Conn) -> Option<Arc<crate::net::Socket>> {
        // clone: the connection's socket, for the errand that reads or writes it.
        self.conns.get(blaze_mem::Handle::from_bits(conn.0)).map(|it| Arc::clone(&it.socket))
    }

    #[cfg(unix)]
    fn dispatch(&mut self, errand: Errand) {
        dispatch(&self.shared, errand);
    }

    /// A stopped watch's control kept for the next, once its errand has let go of it.
    #[cfg(target_os = "linux")]
    fn keep(&mut self, control: Arc<linux::Control>) {
        if self.spare.len() < SPARE && Arc::strong_count(&control) == 1 {
            self.spare.push(control);
        }
    }

    /// Watches `dir` as `tag`, a journal of it from `since`. A watch of `tag` already ends
    /// first, telling nothing more. Returns once the system watches the tree, so a change made
    /// after is heard.
    #[cfg(target_os = "linux")]
    fn watch(&mut self, dir: crate::FileName, since: Option<crate::Cursor>, tag: Tag) -> io::Result<()> {
        if let Some(old) = self.watches.remove(tag) {
            old.control.stop();
            self.in_flight -= 1;
        }
        let control = match self.spare.pop() {
            Some(control) => control,
            None => linux::Control::new()?,
        };
        control.reset();
        let first = since.as_ref().map(|since| crate::journal::first_on_linux(&dir, since));
        // clone: the watch's control, kept by the driver.
        let (watch, _) = self.watches.insert(tag, Watching { control: control.clone() });
        // clone: the watch's control, for its errand.
        let errand = Errand::Watch { dir, journal: since.is_some(), control: control.clone(), tag, watch };
        self.dispatch(errand);
        match control.started() {
            Ok(()) => {
                self.in_flight += 1;
                if let Some(first) = first {
                    self.ready.push(Completion { tag, done: first });
                }
                Ok(())
            }
            Err(error) => {
                drop(control);
                if let Some((_, watching)) = self.watches.remove_id(watch) {
                    self.keep(watching.control);
                }
                Err(error)
            }
        }
    }

    /// Watches `dir` as `tag`, a journal of it from `since`: on a stopped watch kept from an
    /// earlier one where there is one. A watch of `tag` already ends first, telling nothing
    /// more.
    #[cfg(target_os = "macos")]
    fn watch(&mut self, dir: crate::FileName, since: Option<crate::Cursor>, tag: Tag) -> io::Result<()> {
        if self.watches.get(tag).is_some() {
            self.retire(tag, false);
        }
        let watch = match self.spare.pop() {
            Some(watch) => watch,
            // clone: once per watch made, the state its sink tells into.
            None => crate::fsevents::Watch::new(Sink(Arc::clone(&self.shared)))?,
        };
        let (id, _) = self.watches.insert(tag, Watching(watch));
        let (_, Watching(watch)) = self.watches.by_id_mut(id).expect("inserted above");
        let mut first = None;
        let started = match since {
            None => watch.start(&self.shared.root, dir, tag),
            Some(since) => watch.journal(&self.shared.root, dir, since, tag, |it| first = Some(it)),
        };
        let started = started.map(|()| first);
        match started {
            Ok(first) => {
                self.in_flight += 1;
                if let Some(first) = first {
                    // alloc: the batch's payload.
                    let done = Done::Journaled(Box::new(first));
                    self.ready.push(Completion { tag, done });
                }
                Ok(())
            }
            Err(error) => {
                let (_, Watching(watch)) = self.watches.remove_id(id).expect("inserted above");
                if self.spare.len() < SPARE {
                    self.spare.push(watch);
                }
                Err(error)
            }
        }
    }

    /// Stops the watch of `tag` in place and takes it out: what it told is taken in first,
    /// then each flush it had not told fails, then its end, where `told`.
    #[cfg(target_os = "macos")]
    fn retire(&mut self, tag: Tag, told: bool) {
        let Some(Watching(watch)) = self.watches.get_mut(tag) else { return };
        let untold = &mut self.untold;
        watch.stop(|flush| untold.push(flush));
        #[cfg(test)]
        self.shared.off.allocations.fetch_add(watch.take_overhead(), std::sync::atomic::Ordering::Relaxed);
        self.shared.done.take_all(&mut self.inbox, Duration::ZERO);
        self.take_told();
        while let Some(flush) = self.untold.pop() {
            // alloc: an error path, a flush its watch ended before.
            let why = IoError("the watch ended before the flush".into());
            self.end(Completion { tag: flush, done: Done::Failed(why) });
        }
        match told {
            true => self.end(Completion { tag, done: Done::Unwatched }),
            false => self.in_flight -= 1,
        }
        let Watching(watch) = self.watches.remove(tag).expect("looked up above");
        if self.spare.len() < SPARE {
            self.spare.push(watch);
        }
    }

    /// Stops the watch of `tag`, whose errand tells its end: `false` for a watch not in flight.
    #[cfg(target_os = "linux")]
    fn unwatch(&mut self, tag: Tag) -> bool {
        let Some(watching) = self.watches.get(tag) else { return false };
        watching.control.stop();
        true
    }

    /// Stops the watch of `tag` and tells its end: `false` for a watch not in flight.
    #[cfg(target_os = "macos")]
    fn unwatch(&mut self, tag: Tag) -> bool {
        if self.watches.get(tag).is_none() {
            return false;
        }
        self.retire(tag, true);
        true
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn watch(&mut self, _: crate::FileName, since: Option<crate::Cursor>, _: Tag) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            match since {
                None => "watching files is built on Linux (inotify, fanotify) and macOS (FSEvents) only",
                Some(_) => {
                    "the threads driver reads journals of changes on Linux and macOS only; on \
                     Windows the USN journal is read by the IOCP driver"
                }
            },
        ))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn unwatch(&mut self, _: Tag) -> bool {
        false
    }

    /// Asks the watch `watch` for a flush `tag`: `false` for a watch not in flight.
    #[cfg(target_os = "linux")]
    fn flush(&mut self, watch: Tag, tag: Tag) -> bool {
        let Some(watching) = self.watches.get(watch) else { return false };
        self.in_flight += 1;
        if !watching.control.flush(tag) {
            // alloc: an error path, a flush its watch ended before.
            let why = IoError("the watch ended before the flush".into());
            self.end(Completion { tag, done: Done::Failed(why) });
        }
        true
    }

    #[cfg(target_os = "macos")]
    fn flush(&mut self, watch: Tag, tag: Tag) -> bool {
        let Some(Watching(watching)) = self.watches.get(watch) else { return false };
        self.in_flight += 1;
        if let Err(error) = watching.flush(tag) {
            // alloc: an error path.
            let why = IoError(format!("flushing the watch: {error}"));
            self.end(Completion { tag, done: Done::Failed(why) });
        }
        true
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn flush(&mut self, _: Tag, _: Tag) -> bool {
        false
    }

    #[cfg(unix)]
    fn listen(&mut self, at: &crate::Address, tag: Tag) -> io::Result<()> {
        use std::os::fd::{AsRawFd as _, OwnedFd};
        let listener = crate::net::listen(&self.shared.root, at)?;
        let (read_end, write_end) = std::io::pipe()?;
        let (read_end, write_end) = (OwnedFd::from(read_end), OwnedFd::from(write_end));
        crate::net::set_cloexec(read_end.as_raw_fd())?;
        crate::net::set_cloexec(write_end.as_raw_fd())?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr();
        let (listen, old) = self.listens.insert(tag, Listening { stop: write_end, local });
        if let Some(old) = old {
            old.stop();
            self.in_flight -= 1;
        }
        self.in_flight += 1;
        self.dispatch(Errand::Listen { listener, stop: read_end, tag, listen });
        Ok(())
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        self.shared.files.close();
        #[cfg(target_os = "linux")]
        for (_, _, watching) in self.watches.iter() {
            watching.control.stop();
        }
        #[cfg(target_os = "macos")]
        {
            for (_, _, Watching(watch)) in self.watches.iter_mut() {
                watch.stop(|_| {});
            }
            // Stopped in place: they may move now.
            self.watches.clear();
        }
        #[cfg(unix)]
        {
            for (_, _, listening) in self.listens.iter() {
                listening.stop();
            }
            for (_, _, life) in self.spawns.iter() {
                life.kill();
            }
            for (_, connection) in self.conns.iter() {
                _ = connection.socket.shutdown();
            }
            // What a watch or a listen holds (a socket's file, a watched tree) is let go of
            // before the driver is gone: their errands end once stopped.
            while !self.watches.is_empty() || !self.listens.is_empty() {
                self.shared.done.take_all(&mut self.inbox, Duration::from_secs(1));
                self.take_told();
            }
            let mut errands = lock(&self.shared.errands);
            errands.closed = true;
            drop(errands);
            self.shared.errand.notify_all();
        }
        self.shared.done.close();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// Runs the file effects this thread takes off the driver's queue until it is closed.
fn work_on(shared: &Shared) {
    while let Some(effect) = shared.files.pop() {
        let tag = effect.tag();
        let done = shared.counted(|| {
            // alloc: an error's message.
            run(&shared.root, effect).unwrap_or_else(|error| Done::Failed(IoError(error.to_string())))
        });
        if shared.done.push(Told::Done(Completion { tag, done })).is_err() {
            return;
        }
    }
}

/// `f` of `file` under `root`, joined in the thread's path buffer.
fn at<R>(root: &Path, file: &crate::FileName, f: impl FnOnce(&Path) -> io::Result<R>) -> io::Result<R> {
    blaze_mem::scratch::with_path(|path| {
        path.push(root);
        path.push(file.as_str());
        f(path)
    })
}

/// A file effect done with blocking calls, for this driver's threads and for the drivers that
/// keep their file calls on threads. Allocates only what its completion carries.
pub(crate) fn run(root: &Path, effect: Effect) -> io::Result<Done> {
    match effect {
        Effect::ReadAll { file, .. } => match at(root, &file, |path| std::fs::read(path)) {
            Ok(bytes) => Ok(Done::Read(bytes)),
            // alloc: none, an empty read.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Done::Read(Vec::new())),
            Err(error) => Err(error),
        },
        Effect::ReadLink { file, .. } => read_link(root, &file),
        Effect::Read { file, at: from, len, .. } => {
            let file = at(root, &file, |path| File::open(path))?;
            // alloc: the read's payload.
            let mut bytes = vec![0; len];
            let mut read = 0;
            while read < len {
                let n = read_at(&file, &mut bytes[read..], from + read as u64)?;
                if n == 0 {
                    break;
                }
                read += n;
            }
            bytes.truncate(read);
            Ok(Done::Read(bytes))
        }
        Effect::Append { file, bytes, .. } => at(root, &file, |path| {
            let open = || OpenOptions::new().create(true).append(true).open(path);
            fs::making_parent(path, open)?.write_all(&bytes)?;
            Ok(Done::Appended)
        }),
        // `sync_all` is `F_FULLFSYNC` on Apple's systems, where `fsync` leaves the bytes in the
        // drive's cache.
        Effect::Sync { file, .. } => at(root, &file, |path| {
            OpenOptions::new().append(true).create(true).open(path)?.sync_all()?;
            Ok(Done::Synced)
        }),
        Effect::Truncate { file, len, .. } => at(root, &file, |path| {
            let open = || OpenOptions::new().write(true).create(true).truncate(false).open(path);
            fs::making_parent(path, open)?.set_len(len)?;
            Ok(Done::Truncated)
        }),
        Effect::Write { file, bytes, .. } => at(root, &file, |path| {
            fs::write_atomically(path, &bytes)?;
            Ok(Done::Written)
        }),
        Effect::WriteAt { file, at: offset, bytes, .. } => at(root, &file, |path| {
            fs::write_at(path, offset, &bytes)?;
            Ok(Done::Written)
        }),
        Effect::Rename { from, to, .. } => at(root, &from, |from| {
            at(root, &to, |to| fs::rename(from, to))?;
            Ok(Done::Renamed)
        }),
        Effect::Mode { file, mode, .. } => at(root, &file, |path| {
            fs::set_mode(path, mode)?;
            Ok(Done::Moded)
        }),
        Effect::Private { dir, .. } => Ok(Done::Private(at(root, &dir, |path| fs::private(path, &dir))?)),
        Effect::Times { file, modified_ns, .. } => at(root, &file, |path| {
            fs::set_times(path, modified_ns)?;
            Ok(Done::Timed)
        }),
        Effect::Clone { from, to, .. } => at(root, &from, |from| {
            at(root, &to, |to| fs::clone(from, to))?;
            Ok(Done::Cloned)
        }),
        Effect::Link { from, to, .. } => at(root, &from, |from| {
            at(root, &to, |to| fs::link(from, to))?;
            Ok(Done::Linked)
        }),
        Effect::Stat { file, .. } => Ok(Done::Stat(at(root, &file, |path| fs::stat(path))?)),
        #[cfg(unix)]
        Effect::Stamp { files, .. } => Ok(Done::Stamped(fs::stamps(root, &files)?)),
        #[cfg(unix)]
        Effect::Entries { dir, .. } => Ok(Done::Entries(at(root, &dir, |path| fs::entries(path, &dir))?)),
        #[cfg(windows)]
        Effect::Stamp { files, .. } => Ok(Done::Stamped(crate::iocp::stamps(root, &files)?)),
        #[cfg(windows)]
        Effect::Entries { dir, .. } => {
            Ok(Done::Entries(at(root, &dir, |path| crate::iocp::entries(path, &dir))?))
        }
        Effect::List { dir, .. } => Ok(Done::Listed(at(root, &dir, |path| fs::list(path, &dir))?)),
        Effect::Remove { file, .. } => at(root, &file, |path| {
            fs::remove(path)?;
            Ok(Done::Removed)
        }),
        Effect::Map { file, .. } => Ok(Done::Mapped(at(root, &file, |path| fs::map(path))?)),
        Effect::Timer { .. }
        | Effect::Watch { .. }
        | Effect::Journal { .. }
        | Effect::Unwatch { .. }
        | Effect::Flush { .. }
        | Effect::Spawn { .. }
        | Effect::Kill { .. }
        | Effect::Listen { .. }
        | Effect::Unlisten { .. }
        | Effect::Connect { .. }
        | Effect::Recv { .. }
        | Effect::Send { .. }
        | Effect::Hangup { .. } => {
            unreachable!("timers, watches, processes and sockets are kept by the driver")
        }
    }
}

/// [`Effect::ReadLink`] of `file` under `root`.
pub(crate) fn read_link(root: &Path, file: &crate::FileName) -> io::Result<Done> {
    match at(root, file, |path| std::fs::read_link(path)) {
        Ok(target) => Ok(Done::Read(link_bytes(file, target)?)),
        // alloc: an error path.
        Err(error) => Err(io::Error::new(error.kind(), format!("{file}: {error}"))),
    }
}

/// The bytes of the symbolic link `file`'s `target`, as [`Effect::ReadLink`] gives them.
#[cfg(unix)]
fn link_bytes(_: &crate::FileName, target: std::path::PathBuf) -> io::Result<Vec<u8>> {
    Ok(std::os::unix::ffi::OsStringExt::into_vec(target.into_os_string()))
}

/// The bytes of the symbolic link `file`'s `target`, as [`Effect::ReadLink`] gives them.
#[cfg(windows)]
fn link_bytes(file: &crate::FileName, target: std::path::PathBuf) -> io::Result<Vec<u8>> {
    match target.into_os_string().into_string() {
        Ok(target) => Ok(target.replace('\\', "/").into_bytes()),
        // alloc: an error path.
        Err(_) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("{file}: a link's target that is not UTF-8"))),
    }
}

#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, at)
}

#[cfg(windows)]
fn read_at(file: &File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, at)
}

/// A connection: its socket, shared with the errands reading and writing it.
#[cfg(unix)]
struct Connection {
    socket: Arc<crate::net::Socket>,
}

/// A listen's errand, and how to stop it: a byte to its pipe, or the pipe's end dropped.
#[cfg(unix)]
struct Listening {
    stop: std::os::fd::OwnedFd,
    local: Option<std::net::SocketAddr>,
}

#[cfg(unix)]
impl Listening {
    fn stop(&self) {
        use std::os::fd::AsFd as _;
        // A full pipe fails it, and is readable already: the stop is seen either way.
        _ = crate::raw::write(self.stop.as_fd(), b"x");
    }
}

/// Where a spawn is, shared by the driver, which kills it, and its errand, which starts and
/// reaps it.
#[cfg(unix)]
#[derive(Default)]
struct Life(Mutex<Stage>);

#[cfg(unix)]
#[derive(Default)]
enum Stage {
    /// Asked for and not yet started.
    #[default]
    Starting,
    /// Not yet started, and a kill came first: killed as it starts.
    Killed,
    /// Started and not yet reaped: its pid is no one else's.
    Running(libc::pid_t),
    /// Exited, or never started: nothing to kill.
    Exited,
}

#[cfg(unix)]
impl Life {
    fn kill(&self) {
        let mut stage = lock(&self.0);
        match *stage {
            Stage::Starting => *stage = Stage::Killed,
            Stage::Running(pid) => crate::process::kill(pid),
            Stage::Killed | Stage::Exited => {}
        }
    }

    /// `pid` started: killed at once where a kill came first.
    fn started(&self, pid: libc::pid_t) {
        let mut stage = lock(&self.0);
        if let Stage::Killed = *stage {
            crate::process::kill(pid);
        }
        *stage = Stage::Running(pid);
    }

    /// Exited, before it is reaped: no kill reaches its pid after.
    fn exited(&self) {
        *lock(&self.0) = Stage::Exited;
    }
}

/// The errands asked for and not yet taken, and the threads waiting for one.
#[cfg(unix)]
struct Errands {
    errands: VecDeque<Errand>,
    /// Threads that will take an errand without one more being started.
    idle: usize,
    closed: bool,
}

/// Work that may wait on a peer as long as the peer likes, run on a thread of the pool. Kept
/// by value: asking for one allocates nothing.
#[cfg(unix)]
enum Errand {
    Spawn {
        command: Box<crate::Command>,
        // dyn: as `Threads::confine`.
        confine: Option<Arc<dyn Confine>>,
        life: Arc<Life>,
        spawn: Id<Arc<Life>>,
        tag: Tag,
    },
    Connect {
        at: crate::Address,
        tag: Tag,
    },
    Recv {
        socket: Arc<crate::net::Socket>,
        most: usize,
        into: Vec<u8>,
        tag: Tag,
    },
    Send {
        socket: Arc<crate::net::Socket>,
        bytes: Vec<u8>,
        tag: Tag,
    },
    Listen {
        listener: crate::net::Listener,
        stop: std::os::fd::OwnedFd,
        listen: Id<Listening>,
        tag: Tag,
    },
    #[cfg(target_os = "linux")]
    Watch {
        dir: crate::FileName,
        /// Told as [`Done::Journaled`].
        journal: bool,
        control: Arc<linux::Control>,
        watch: Id<Watching>,
        tag: Tag,
    },
}

/// What an errand thread keeps across errands.
#[cfg(unix)]
#[derive(Default)]
struct Room {
    /// Of the watches it reads: grown once to [`crate::watch::BUFFER`].
    #[cfg(target_os = "linux")]
    buffer: Vec<u8>,
    /// Flushes taken from a watch's control.
    #[cfg(target_os = "linux")]
    taken: Vec<Tag>,
}

/// `errand` to an idle thread of the pool, or to a thread more when none is idle.
#[cfg(unix)]
fn dispatch(shared: &Arc<Shared>, errand: Errand) {
    let tag = errand.tag();
    let mut errands = lock(&shared.errands);
    errands.errands.push_back(errand);
    if errands.idle > 0 {
        errands.idle -= 1;
        drop(errands);
        shared.errand.notify_one();
        return;
    }
    drop(errands);
    let pool = Arc::clone(shared); // clone: the state a thread more shares.
    // alloc: a thread more, for the most errands at once; kept for the next.
    let started = std::thread::Builder::new().name("blaze-errand".into()).spawn(move || errand_thread(&pool));
    if let Err(error) = started {
        let mut errands = lock(&shared.errands);
        let at = errands.errands.iter().rposition(|it| it.tag() == tag);
        let errand = at.and_then(|at| errands.errands.remove(at));
        drop(errands);
        if let Some(told) = errand.and_then(|it| it.failed(error)) {
            _ = shared.done.push(told);
        }
    }
}

/// An errand thread: runs errands until the driver is gone.
#[cfg(unix)]
fn errand_thread(shared: &Shared) {
    let mut room = Room::default();
    let mut next = shared.next_errand(false);
    while let Some(errand) = next {
        let told = shared.counted(|| errand.run(shared, &mut room));
        next = shared.next_errand_after(told);
    }
}

#[cfg(unix)]
impl Shared {
    /// The next errand, waiting for one; `None` once the pool is closed.
    fn next_errand(&self, idle: bool) -> Option<Errand> {
        let mut errands = lock(&self.errands);
        loop {
            if let Some(errand) = errands.errands.pop_front() {
                return Some(errand);
            }
            if errands.closed {
                if idle {
                    errands.idle -= 1;
                }
                return None;
            }
            self.errand.wait(&mut errands);
        }
    }

    /// Tells what an errand ended with, then the next errand: the thread is idle before the
    /// driver hears, so an errand it asks for next starts no thread more.
    fn next_errand_after(&self, told: Option<Told>) -> Option<Errand> {
        lock(&self.errands).idle += 1;
        if let Some(told) = told {
            _ = self.done.push(told);
        }
        self.next_errand(true)
    }
}

#[cfg(unix)]
impl Errand {
    fn tag(&self) -> Tag {
        match self {
            Errand::Spawn { tag, .. }
            | Errand::Connect { tag, .. }
            | Errand::Recv { tag, .. }
            | Errand::Send { tag, .. }
            | Errand::Listen { tag, .. } => *tag,
            #[cfg(target_os = "linux")]
            Errand::Watch { tag, .. } => *tag,
        }
    }

    /// What says it could not run, for `why`.
    fn failed(self, why: io::Error) -> Option<Told> {
        // alloc: an error path.
        let done = Done::Failed(IoError(why.to_string()));
        match self {
            Errand::Spawn { life, spawn, tag, .. } => {
                life.exited();
                Some(Told::Exited { spawn, completion: Completion { tag, done } })
            }
            Errand::Connect { tag, .. } => Some(Told::Done(Completion { tag, done })),
            Errand::Recv { into, tag, .. } => Some(Told::Done(Completion { tag, done: Done::unread(into, why) })),
            Errand::Send { bytes, tag, .. } => Some(Told::Done(Completion { tag, done: Done::unsent(bytes, why) })),
            Errand::Listen { listen, tag, .. } => Some(Told::Listened { listen, completion: Completion { tag, done } }),
            #[cfg(target_os = "linux")]
            Errand::Watch { control, .. } => {
                control.start(Err(why));
                None
            }
        }
    }

    /// Runs the errand to its end: what it ends with, `None` when there is nothing to tell.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    fn run(self, shared: &Shared, room: &mut Room) -> Option<Told> {
        let failed = |tag: Tag, error: io::Error| {
            // alloc: an error path.
            Told::Done(Completion { tag, done: Done::Failed(IoError(error.to_string())) })
        };
        match self {
            Errand::Spawn { command, confine, life, spawn, tag } => {
                let done = match run_process(&shared.root, command, confine.as_ref(), &life) {
                    // alloc: the exit's payload.
                    Ok(exit) => Done::Exited(Box::new(exit)),
                    // alloc: an error path.
                    Err(error) => Done::Failed(IoError(error.to_string())),
                };
                Some(Told::Exited { spawn, completion: Completion { tag, done } })
            }
            Errand::Connect { at, tag } => Some(match crate::net::connect(&shared.root, &at) {
                Ok(socket) => Told::Connected { tag, socket },
                Err(error) => failed(tag, error),
            }),
            Errand::Recv { socket, most, mut into, tag } => {
                // The asker's buffer, grown only where it has no room for `most` more.
                let done = match shared.payload(|| socket.read_into(&mut into, most)) {
                    Ok(_) => Done::Read(into),
                    Err(error) => Done::unread(into, error),
                };
                Some(Told::Done(Completion { tag, done }))
            }
            Errand::Send { socket, mut bytes, tag } => {
                let done = match socket.write_all(&bytes) {
                    Ok(()) => {
                        bytes.clear();
                        Done::Sent(bytes)
                    }
                    Err(error) => Done::unsent(bytes, error),
                };
                Some(Told::Done(Completion { tag, done }))
            }
            Errand::Listen { listener, stop, listen, tag } => {
                let done = accept_until_stopped(shared, &listener, &stop, listen, tag)?;
                Some(Told::Listened { listen, completion: Completion { tag, done } })
            }
            #[cfg(target_os = "linux")]
            Errand::Watch { dir, journal, control, watch, tag } => {
                linux::watch(shared, room, &dir, journal, control, watch, tag)
            }
        }
    }
}

/// `command` started, fed its input, read and reaped, on this thread.
#[cfg(unix)]
fn run_process(
    root: &Path,
    command: Box<crate::Command>,
    // dyn: as `Threads::confine`.
    confine: Option<&Arc<dyn Confine>>,
    life: &Life,
) -> io::Result<crate::Exit> {
    let started = match crate::process::start(root, command, confine) {
        Ok(started) => started,
        Err(error) => {
            life.exited();
            return Err(error);
        }
    };
    let pid = started.pid;
    life.started(pid);
    let crate::process::Started { stdin, stdout, stderr, mut finish, .. } = started;
    let input = std::mem::take(&mut finish.command.stdin);
    let communicated = crate::process::communicate(stdin, &input, stdout, stderr);
    if communicated.is_err() {
        crate::process::kill(pid);
    }
    let waited = crate::process::wait_exited(pid);
    life.exited();
    match (communicated, waited) {
        (Ok((output, errors)), Ok(())) => finish.exited(pid, output, errors),
        (Err(error), _) | (_, Err(error)) => {
            // Reaped, and what was made for it removed; the error is what is told.
            // alloc: none, empty outputs.
            _ = finish.exited(pid, Vec::new(), Vec::new());
            Err(error)
        }
    }
}

/// A listen's errand: accepts until its pipe is written or closed. What ends it; `None` once
/// the driver is gone.
#[cfg(unix)]
fn accept_until_stopped(
    shared: &Shared,
    listener: &crate::net::Listener,
    stop: &std::os::fd::OwnedFd,
    listen: Id<Listening>,
    tag: Tag,
) -> Option<Done> {
    use std::os::fd::AsRawFd as _;
    // alloc: an error path.
    let failed = |error: io::Error| Some(Done::Failed(IoError(error.to_string())));
    loop {
        let mut fds = [
            libc::pollfd { fd: listener.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        match crate::raw::poll(&mut fds, -1) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return failed(error),
        }
        if fds[1].revents != 0 {
            return Some(Done::Unlistened);
        }
        match listener.accept() {
            Ok(socket) => {
                if let Err(error) = socket.set_nonblocking(false) {
                    return failed(error);
                }
                if shared.done.push(Told::Accepted { listen, tag, socket }).is_err() {
                    return None;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                ) => {}
            Err(error) => return failed(error),
        }
    }
}

/// A watch on Linux: its tree read on an errand, told through the driver's mailbox.
#[cfg(target_os = "linux")]
struct Watching {
    control: Arc<linux::Control>,
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        io,
        os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd},
        sync::Arc,
    };

    use parking_lot::{Condvar, Mutex};

    use super::{Id, Room, Shared, Told, Watching, lock};
    use crate::{Completion, Done, FileName, IoError, Tag};

    /// How the driver stops a watch's errand and asks it for flushes: made once, and kept
    /// for the next watch once the errand has let go of it.
    pub(super) struct Control {
        stop: OwnedFd,
        /// Written once flushes are added.
        wake: OwnedFd,
        state: Mutex<State>,
        started: Condvar,
    }

    struct State {
        /// Asked for and not yet taken by the errand.
        flushes: Vec<Tag>,
        /// Whether the errand takes flushes: a flush asked after it ends fails rather than
        /// waiting for nothing.
        open: bool,
        /// How the watch started, once it has.
        started: Option<io::Result<()>>,
        /// What the last tree watched with it left for the next.
        spare: Option<crate::watch::Spare>,
    }

    /// One added to an eventfd's count. A full count (2^64 - 2) fails with `EAGAIN`, which
    /// still leaves it readable: the signal is seen either way.
    fn signal(fd: BorrowedFd<'_>) {
        _ = crate::raw::write(fd, &1u64.to_ne_bytes());
    }

    /// An eventfd's count read, which resets it; nothing to read fails with `EAGAIN` and
    /// changes nothing.
    fn reset(fd: BorrowedFd<'_>) {
        _ = crate::raw::read(fd, &mut [0; 8]);
    }

    impl Control {
        pub(super) fn new() -> io::Result<Arc<Control>> {
            let (stop, wake) = (crate::raw::eventfd()?, crate::raw::eventfd()?);
            // alloc: once per watch at once, kept for the next: room for a few flushes at once.
            let flushes = Vec::with_capacity(4);
            let state = State { flushes, open: false, started: None, spare: None };
            // alloc: as above.
            Ok(Arc::new(Control { stop, wake, state: Mutex::new(state), started: Condvar::new() }))
        }

        /// Ready for a watch: nothing signalled, nothing asked for.
        pub(super) fn reset(&self) {
            reset(self.stop.as_fd());
            reset(self.wake.as_fd());
            let mut state = lock(&self.state);
            state.flushes.clear();
            state.open = true;
            state.started = None;
        }

        pub(super) fn stop(&self) {
            signal(self.stop.as_fd());
        }

        /// Asks for a flush `tag`, which the errand answers: `false` once it ended.
        pub(super) fn flush(&self, tag: Tag) -> bool {
            let mut state = lock(&self.state);
            if !state.open {
                return false;
            }
            state.flushes.push(tag);
            signal(self.wake.as_fd());
            true
        }

        /// The errand says how the watch started.
        pub(super) fn start(&self, started: io::Result<()>) {
            let mut state = lock(&self.state);
            if started.is_err() {
                state.open = false;
            }
            state.started = Some(started);
            self.started.notify_all();
        }

        /// How the watch started, waiting until it has.
        pub(super) fn started(&self) -> io::Result<()> {
            let mut state = lock(&self.state);
            loop {
                if let Some(started) = state.started.take() {
                    return started;
                }
                self.started.wait(&mut state);
            }
        }
    }

    /// A watch's errand: watches `dir`, then reads its events until it is stopped, answering
    /// each flush once it has read every event queued when the flush was taken. What ends the
    /// watch, `None` when it did not start (told through `control`) or the driver is gone.
    pub(super) fn watch(
        shared: &Shared,
        room: &mut Room,
        dir: &FileName,
        journal: bool,
        control: Arc<Control>,
        watch: Id<Watching>,
        tag: Tag,
    ) -> Option<Told> {
        let spare = lock(&control.state).spare.take();
        let mut tree = match crate::watch::Tree::reusing(spare, &shared.root, dir, false) {
            Ok(tree) => tree,
            Err(error) => {
                control.start(Err(error));
                return None;
            }
        };
        control.start(Ok(()));
        if room.buffer.is_empty() {
            // alloc: once per errand thread that reads a watch, kept for the next.
            room.buffer = vec![0; crate::watch::BUFFER];
        }
        let tell = |done: Done| {
            let completion = Completion { tag, done };
            shared.done.push(Told::Watch { watch, completion }).is_ok()
        };
        let heard = |done: Done| match journal {
            true => crate::journal::live(dir, done),
            false => done,
        };
        let end = read_until_stopped(shared, room, &mut tree, &control, &tell, &heard);
        let mut state = lock(&control.state);
        state.open = false;
        state.spare = Some(tree.end());
        for flush in state.flushes.drain(..).chain(room.taken.drain(..)) {
            // alloc: an error path, a flush its watch ended before.
            let why = IoError("the watch ended before the flush".into());
            _ = shared.done.push(Told::Done(Completion { tag: flush, done: Done::Failed(why) }));
        }
        drop(state);
        // Let go of before the end is told, so the driver may keep it for the next watch.
        drop(control);
        let completion = Completion { tag, done: end? };
        Some(Told::Watch { watch, completion })
    }

    fn read_until_stopped(
        shared: &Shared,
        room: &mut Room,
        tree: &mut crate::watch::Tree,
        control: &Control,
        tell: &impl Fn(Done) -> bool,
        heard: &impl Fn(Done) -> Done,
    ) -> Option<Done> {
        let (stop, wake) = (control.stop.as_raw_fd(), control.wake.as_raw_fd());
        loop {
            let mut fds = [
                libc::pollfd { fd: tree.raw(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: stop, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 },
            ];
            match crate::raw::poll(&mut fds, -1) {
                Ok(_) => {}
                Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
                // alloc: an error path.
                Err(error) => return Some(Done::Failed(IoError(error.to_string()))),
            }
            if fds[2].revents != 0 {
                reset(control.wake.as_fd());
                // Drained, not taken: both lists keep their room, so a flush allocates nothing.
                room.taken.extend(lock(&control.state).flushes.drain(..));
                let mut gone = false;
                // What a drain allocates is the batches it reads; `tell` allocates nothing.
                let drained = shared.payload(|| tree.drain(&mut room.buffer, |done| gone |= !tell(heard(done))));
                if gone {
                    return None;
                }
                for flush in room.taken.drain(..) {
                    let done = match &drained {
                        Ok(()) => Done::Flushed,
                        // alloc: an error path.
                        Err(error) => Done::Failed(IoError(format!("flushing the watch: {error}"))),
                    };
                    if shared.done.push(Told::Done(Completion { tag: flush, done })).is_err() {
                        return None;
                    }
                }
                continue;
            }
            if fds[1].revents != 0 {
                return Some(Done::Unwatched);
            }
            if fds[0].revents != 0 {
                let buffer = &mut room.buffer;
                // A failed read is looked at again by the next poll, as one that read nothing.
                if let Ok(n @ 1..) = tree.read(buffer) {
                    let done = shared.payload(|| heard(tree.take(&buffer[..n])));
                    if !tell(done) {
                        return None;
                    }
                }
            }
        }
    }
}

/// A watch of FSEvents', which tells on a queue of its own.
#[cfg(target_os = "macos")]
struct Watching(crate::fsevents::Watch<Sink>);

/// Where a watch of FSEvents' tells: the driver's mailbox, which always has room.
#[cfg(target_os = "macos")]
struct Sink(Arc<Shared>);

#[cfg(target_os = "macos")]
impl crate::fsevents::Tell for Sink {
    fn tell(&self, completion: Completion, _: Duration) -> Result<(), Option<Completion>> {
        self.0.done.push(Told::Heard(completion)).map_err(|_| None)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
enum Watching {}

/// Why a flush of a watch not in flight fails.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn no_watch(watch: Tag) -> String {
    // alloc: an error path.
    format!("nothing is watched as {watch:?}")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn no_watch(watch: Tag) -> String {
    // alloc: an error path.
    format!(
        "nothing is watched as {watch:?}: watching files, and so flushing a watch, is built on \
         Linux (inotify, fanotify) and macOS (FSEvents) only"
    )
}

impl Driver for Threads {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            match effect {
                Effect::Timer { after, tag } => {
                    self.in_flight += 1;
                    let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                    self.timers.insert(due, tag);
                }
                Effect::Watch { dir, tag } => {
                    if let Err(error) = self.watch(dir, None, tag) {
                        // alloc: an error path.
                        self.tell(tag, Done::Failed(IoError(error.to_string())));
                    }
                }
                Effect::Journal { dir, since, tag } => {
                    if let Err(error) = self.watch(dir, Some(since), tag) {
                        // alloc: an error path.
                        self.tell(tag, Done::Failed(IoError(error.to_string())));
                    }
                }
                Effect::Unwatch { tag } => {
                    if !self.unwatch(tag) {
                        // alloc: an error path.
                        self.tell(tag, Done::Failed(IoError(format!("nothing is watched as {tag:?}"))));
                    }
                }
                Effect::Flush { watch, tag } => {
                    if !self.flush(watch, tag) {
                        self.tell(tag, Done::Failed(IoError(no_watch(watch))));
                    }
                }
                #[cfg(unix)]
                Effect::Spawn { command, tag } => {
                    self.in_flight += 1;
                    // alloc: once per spawn, where it is, shared with its errand.
                    let life = Arc::new(Life::default());
                    // clone: as above.
                    let (spawn, _) = self.spawns.insert(tag, Arc::clone(&life));
                    // clone: the driver's sandbox maker, for the spawn.
                    let confine = self.confine.clone();
                    self.dispatch(Errand::Spawn { command, confine, life, spawn, tag });
                }
                #[cfg(unix)]
                Effect::Kill { tag } => {
                    if let Some(life) = self.spawns.get(tag) {
                        life.kill();
                    }
                }
                #[cfg(unix)]
                Effect::Listen { at, tag } => {
                    if let Err(error) = self.listen(&at, tag) {
                        // alloc: an error path.
                        self.tell(tag, Done::Failed(IoError(error.to_string())));
                    }
                }
                #[cfg(unix)]
                Effect::Unlisten { tag } => match self.listens.get(tag) {
                    Some(listening) => listening.stop(),
                    None => self.tell(
                        tag,
                        // alloc: an error path.
                        Done::Failed(IoError(format!("nothing is listened on as {tag:?}"))),
                    ),
                },
                #[cfg(unix)]
                Effect::Connect { at, tag } => {
                    self.in_flight += 1;
                    self.dispatch(Errand::Connect { at, tag });
                }
                #[cfg(unix)]
                Effect::Recv { conn, most, into, tag } => match self.socket(conn) {
                    Some(socket) => {
                        self.in_flight += 1;
                        self.dispatch(Errand::Recv { socket, most, into, tag });
                    }
                    None => self.tell(tag, Done::unread(into, format_args!("no connection {conn:?}"))),
                },
                #[cfg(unix)]
                Effect::Send { conn, bytes, tag } => match self.socket(conn) {
                    Some(socket) => {
                        self.in_flight += 1;
                        self.dispatch(Errand::Send { socket, bytes, tag });
                    }
                    None => self.tell(tag, Done::unsent(bytes, format_args!("no connection {conn:?}"))),
                },
                #[cfg(unix)]
                Effect::Hangup { conn, tag } => {
                    let done = match self.conns.remove(blaze_mem::Handle::from_bits(conn.0)) {
                        // A receive in flight on it ends with nothing.
                        Some(connection) => match connection.socket.shutdown() {
                            Ok(()) => Done::HungUp,
                            // alloc: an error path.
                            Err(error) => Done::Failed(IoError(error.to_string())),
                        },
                        // alloc: an error path.
                        None => Done::Failed(IoError(format!("no connection {conn:?}"))),
                    };
                    self.tell(tag, done);
                }
                #[cfg(not(unix))]
                Effect::Kill { .. } => {}
                #[cfg(not(unix))]
                Effect::Recv { into, tag, .. } => self.tell(tag, Done::unread(into, UNIX_ONLY)),
                #[cfg(not(unix))]
                Effect::Send { bytes, tag, .. } => self.tell(tag, Done::unsent(bytes, UNIX_ONLY)),
                #[cfg(not(unix))]
                effect @ (Effect::Spawn { .. }
                | Effect::Listen { .. }
                | Effect::Unlisten { .. }
                | Effect::Connect { .. }
                | Effect::Hangup { .. }) => {
                    // alloc: an error path.
                    self.tell(effect.tag(), Done::Failed(IoError(UNIX_ONLY.into())))
                }
                effect => {
                    self.in_flight += 1;
                    if self.shared.files.push(effect).is_err() {
                        unreachable!("the IO threads' queue is closed only as the driver is dropped");
                    }
                }
            }
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let before = out.len();
        self.fire_timers();
        let wait = match self.ready.is_empty() {
            true => {
                let now = wheel::ticks(self.start.elapsed());
                let until_timer =
                    self.timers.next_deadline().map(|at| Duration::from_nanos(at.saturating_sub(now)));
                until_timer.map_or(wait, |it| it.min(wait))
            }
            false => Duration::ZERO,
        };
        self.shared.done.take_all(&mut self.inbox, wait);
        self.take_told();
        self.fire_timers();
        self.in_flight -= self.ready_ends;
        self.ready_ends = 0;
        out.append(&mut self.ready);
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}
