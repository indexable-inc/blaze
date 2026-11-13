//! The daemon's reads of its stores, with no page pool (lane M3 of the memory wave,
//! claude-opus-5-5; docs/v2/plan/design/store-pool.md, "Superseded for the read path
//! (2026-11-13)"): every read of a record is [`blaze_cas::direct`]'s, its bytes in the reading
//! task's frame ([`read`], [`read_many`]) or, for a value kept as a file of its own, read in
//! place through a shared read-only mapping ([`value`]'s [`Value::Mapped`]). The kernel's page
//! cache is the one cache: nothing here keeps a record's bytes past the task that read them.
//!
//! **The driver a read runs on.** A [`Direct`] drives its reads on the caller's driver. A task
//! runs on a worker, which has none of the loop's, so each thread keeps one of its own
//! ([`Reads::Native`]: a native driver over the daemon's root, made on the thread's first read
//! and kept for its life). Where the loop runs its tasks in line (the simulation's runner,
//! [`Reads::Lent`]) the loop lends its own driver to them while it runs them ([`lend`]): the
//! simulation's files are in no file system a driver of a thread's own could read. A read on
//! the lent driver may be handed completions of the loop's own effects: they are kept and
//! given back to the loop when the lend ends, never dropped.
//!
//! **What a thread keeps.** Its driver, and the [`Direct`]s of the last [`KEPT`] daemons' stores
//! it read (one a store: a reader's file table and its plans' tables, which a warm read
//! reuses), keyed by the daemon's [`Stores::id`]. A thread borrows them for one read at a time
//! and holds no borrow across anything but that read's own wait on its driver.
//!
//! **Tags.** A thread's own driver serves its reads alone. A lent driver is the loop's, so the
//! readers ask with [`HOME_TAGS`] and [`STD_TAGS`], past every range of the daemon's own
//! (`daemon::RUNS_END` is `1 << 62`).
//!
//! **Floors.** A record read costs one positional read of its bytes (or one run of neighbours,
//! [`read_many`]), plus a lookup in the index, which is memory: the floor (`direct`'s module
//! docs: 593 ns a cached record on the Mac). What this layer adds is one thread-local borrow
//! and, on the first read of a thread, its driver's making (a kqueue and a thread on macOS):
//! once a thread, not a read. The number to move: `direct::counts` (`records`, `reads`, `runs`)
//! grows and the pool's counters stay 0 (`blaze explain memory`'s `pool resident bytes`, gone
//! with the pool); a warm daemon's footprint loses the pool's resident pages (up to its budget).

use std::{
    cell::RefCell,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use blaze_cas::{
    Key, Reader,
    direct::{Direct, Failed, Mapped, Placed, Read},
};
use blaze_core::{Completion, Driver, Effect, wait::Deadline};
use blaze_mem::frame::Frame;

/// The tags the home store's reader asks a lent driver with: past every range of the daemon's.
pub(crate) const HOME_TAGS: u64 = 1 << 62;
/// The tags the std store's reader asks a lent driver with.
pub(crate) const STD_TAGS: u64 = HOME_TAGS + blaze_cas::direct::TAGS;

const _: () = assert!(STD_TAGS.checked_add(blaze_cas::direct::TAGS).is_some());

/// The daemons' readers a thread keeps between reads, the least recently used dropped first:
/// a process runs one daemon, and a test process a few at once.
pub(crate) const KEPT: usize = 4;

/// How a daemon's tasks read its stores (see the module's docs).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reads {
    /// On a native driver of each thread's own over `root`, the root of the daemon's driver,
    /// which the stores' file names are under: `blaze serve`'s.
    Native { root: Arc<Path> },
    /// On the loop's own driver, lent to the tasks it runs in line ([`lend`]): the
    /// simulation's. A task that reads while no driver is lent is a bug, and fails its job.
    Lent,
}

/// Which of a daemon's stores a read is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Which {
    /// The daemon's own (`<state>/store`), which reads the machine's sealed layer behind it.
    Home,
    /// The standard library's, shared by this build's daemons.
    Std,
}

/// A daemon's stores as its tasks read them: made once a daemon, shared by its jobs.
pub(crate) struct Stores {
    /// Tells this daemon's readers on a thread from another's.
    id: u64,
    home: Reader,
    std: Reader,
    reads: Reads,
}

static IDS: AtomicU64 = AtomicU64::new(0);

impl Stores {
    pub(crate) fn new(home: Reader, std: Reader, reads: Reads) -> Arc<Stores> {
        // alloc: once a daemon started
        Arc::new(Stores { id: IDS.fetch_add(1, Ordering::Relaxed), home, std, reads })
    }

    pub(crate) fn reads(&self) -> &Reads {
        &self.reads
    }

    /// The home store's reader, for what reads no value: `has`, `derivable`, a file's name.
    pub(crate) fn home(&self) -> &Reader {
        &self.home
    }
}

/// A value read: its bytes in the reading task's frame, or, for one the store keeps as a file
/// of its own, read in place through a mapping held for as long as it is.
pub(crate) enum Value<'f> {
    Read(&'f [u8]),
    Mapped(Mapped),
}

impl std::ops::Deref for Value<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Value::Read(bytes) => bytes,
            Value::Mapped(mapped) => mapped,
        }
    }
}

/// The driver calls a [`Direct`] makes, without its type: a thread's own native driver or the
/// loop's lent one. Every method is `Driver`'s own.
trait Drive {
    fn submit(&mut self, effects: &mut Vec<Effect>);
    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize;
    fn in_flight(&self) -> usize;
    fn now(&self) -> Duration;
    fn wall_ns(&self) -> u64;
}

impl<D: Driver> Drive for D {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        Driver::submit(self, effects);
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        Driver::poll(self, out, until)
    }

    fn in_flight(&self) -> usize {
        Driver::in_flight(self)
    }

    fn now(&self) -> Duration {
        Driver::now(self)
    }

    fn wall_ns(&self) -> u64 {
        Driver::wall_ns(self)
    }
}

/// One read's driver. A `dyn` here and nowhere past this module: a task is no generic of the
/// daemon's driver, and a read's cost is its system calls, not one indirect call each.
// dyn: the thread's driver or the loop's, once a read
struct Driving<'a>(&'a mut dyn Drive);

impl Driver for Driving<'_> {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        self.0.submit(effects);
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        self.0.poll(out, until)
    }

    fn in_flight(&self) -> usize {
        self.0.in_flight()
    }

    fn now(&self) -> Duration {
        self.0.now()
    }

    fn wall_ns(&self) -> u64 {
        self.0.wall_ns()
    }
}

/// What a thread keeps for its reads (see the module's docs).
struct Thread {
    /// Its own native driver, and the root it is over.
    native: Option<(Arc<Path>, blaze_core::Native)>,
    /// The loop's driver while it lends it ([`lend`]); its lifetime is the lend's, which
    /// clears it before it returns or unwinds.
    // dyn: the loop's driver, lent
    lent: Option<*mut (dyn Drive + 'static)>,
    /// Completions of the loop's own effects a read on the lent driver was handed: the loop's,
    /// given back when the lend ends.
    // alloc: kept between lends, emptied by each
    others: Vec<Completion>,
    /// The readers of the last [`KEPT`] daemons' stores, the most recently used last.
    // alloc: at most `KEPT`, for the thread's life
    directs: Vec<(u64, Direct, Direct)>,
}

thread_local! {
    static THREAD: RefCell<Thread> = RefCell::new(Thread {
        native: None,
        lent: None,
        // alloc: empty
        others: Vec::new(),
        // alloc: empty
        directs: Vec::new(),
    });
}

/// `f` run with the loop's `driver` lent to every read a task makes on this thread meanwhile
/// ([`Reads::Lent`]): what the loop calls around the step that runs its tasks in line. The
/// completions of the loop's own effects those reads were handed are pushed to `back`, for the
/// loop to take as it takes any other.
pub(crate) fn lend<D: Driver, R>(driver: &mut D, back: &mut Vec<Completion>, f: impl FnOnce() -> R) -> R {
    lend_dyn(driver, back, f)
}

/// The loop's driver as the jobs' view of the daemon carries it (`job::World::reads`): what
/// a store read the loop makes itself runs on (a unit's freshness asked at post, the records
/// filed of a build whose front end is not kept, a certificate's record), in either mode.
pub(crate) struct Lender<'a> {
    // dyn: the loop's driver, one type a daemon; never a hot path
    driver: &'a mut dyn Drive,
    back: &'a mut Vec<Completion>,
}

impl<'a> Lender<'a> {
    pub(crate) fn of<D: Driver>(driver: &'a mut D, back: &'a mut Vec<Completion>) -> Lender<'a> {
        Lender { driver, back }
    }

    /// `f` run with the loop's driver lent to its store reads, in a frame of the loop's own
    /// (`Tag::Plane`, the loop's turns) for what they read, given back when `f` returns: what
    /// the loop reads it decodes before then. Completions of the loop's own effects the reads
    /// were handed go to the loop's queue of them.
    pub(crate) fn lend<R>(&mut self, f: impl FnOnce() -> R) -> R {
        lend_dyn(self.driver, self.back, || blaze_mem::frame::task(blaze_mem::Tag::Plane, |_| f()))
    }
}

fn lend_dyn<R>(driver: &mut dyn Drive, back: &mut Vec<Completion>, f: impl FnOnce() -> R) -> R {
    struct Unlend;
    impl Drop for Unlend {
        fn drop(&mut self) {
            _ = THREAD.try_with(|it| it.borrow_mut().lent = None);
        }
    }
    let lent: &mut dyn Drive = driver;
    // SAFETY: only the lifetime is erased. The pointer is used only by reads on this thread
    // while `f` runs, during which `driver` is borrowed mutably here and so by nothing else;
    // `Unlend` clears it before this returns or unwinds.
    let lent: *mut (dyn Drive + 'static) = unsafe { std::mem::transmute(lent as *mut dyn Drive) };
    THREAD.with(|it| {
        let mut it = it.borrow_mut();
        assert!(it.lent.is_none(), "the loop lent its driver to its tasks twice at once");
        it.lent = Some(lent);
    });
    let unlend = Unlend;
    let out = f();
    drop(unlend);
    THREAD.with(|it| back.append(&mut it.borrow_mut().others));
    out
}

/// What this thread's readers read, summed over every daemon's it keeps: exact where one
/// thread runs every read (the simulation's in-line runner), where the process's
/// [`blaze_cas::direct::counts`] counts every test's at once.
pub(crate) fn thread_counts() -> blaze_cas::direct::Counts {
    THREAD.with(|thread| {
        let thread = thread.borrow();
        let mut all = blaze_cas::direct::Counts::default();
        for (_, home, std) in &thread.directs {
            for own in [home.own_counts(), std.own_counts()] {
                all.records += own.records;
                all.bytes += own.bytes;
                all.reads += own.reads;
                all.runs += own.runs;
                all.files_opened += own.files_opened;
                all.maps += own.maps;
                all.mapped_bytes += own.mapped_bytes;
                all.leaked_bytes += own.leaked_bytes;
            }
        }
        all
    })
}

/// `f` of `which` of `stores`' readers on this thread, with the driver its reads run on.
fn with<R>(stores: &Stores, which: Which, f: impl FnOnce(&mut Direct, &mut Driving<'_>) -> R) -> R {
    THREAD.with(|thread| {
        let Ok(mut thread) = thread.try_borrow_mut() else {
            panic!("a store read began inside another on one thread (serve::direct): reads do not nest");
        };
        let Thread { native, lent, others, directs } = &mut *thread;
        let at = match directs.iter().position(|it| it.0 == stores.id) {
            Some(at) => at,
            None => {
                if directs.len() == KEPT {
                    directs.remove(0);
                }
                // clone: refcounts, once a daemon a thread
                let home = Direct::new(stores.home.clone(), HOME_TAGS);
                let std = Direct::new(stores.std.clone(), STD_TAGS); // clone: as above
                directs.push((stores.id, home, std));
                directs.len() - 1
            }
        };
        let last = directs.len() - 1;
        directs.swap(at, last);
        let (_, home, std) = &mut directs[last];
        let direct = match which {
            Which::Home => home,
            Which::Std => std,
        };
        // A driver lent to this thread serves its reads in either mode: the loop's own
        // (`Lender`), or the in-line runner's lend of it to its tasks.
        let out = match (&stores.reads, *lent) {
            (_, Some(driver)) => {
                // SAFETY: the lend is live (it clears `lent` before it ends) and this thread
                // is inside its `f`, so the loop's driver is borrowed by nothing else.
                let out = f(direct, &mut Driving(unsafe { &mut *driver }));
                others.append(&mut direct.others());
                out
            }
            (Reads::Native { root }, None) => {
                let fresh = native.as_ref().is_none_or(|(had, _)| **had != **root);
                if fresh {
                    let made = blaze_core::native(root, 1).unwrap_or_else(|why| {
                        panic!(
                            "a store read on this thread needs a native driver over {} of its own (serve::direct, Reads::Native): {why}",
                            root.display()
                        )
                    });
                    *native = Some((Arc::clone(root), made)); // clone: a refcount, once a thread
                }
                let (_, driver) = native.as_mut().expect("made above");
                let out = f(direct, &mut Driving(driver));
                let stray = direct.others();
                assert!(
                    stray.is_empty(),
                    "a thread's own driver for store reads completed {} effects no read of it asked (serve::direct): {stray:?}",
                    stray.len()
                );
                out
            }
            (Reads::Lent, None) => panic!(
                "a store read with no driver lent to it (serve::direct, Reads::Lent): only the loop lends one, to its in-line tasks and its own reads"
            ),
        };
        out
    })
}

/// The value of `key` in `which` store, read whole, verified, into `frame`: `None` when the
/// store keeps none. Its bytes stay as they are until the frame is reset or its task ends.
pub(crate) fn read<'f>(
    stores: &Stores,
    which: Which,
    key: &Key,
    frame: &'f Frame,
) -> Result<Option<&'f [u8]>, Failed> {
    let mut into = Vec::new_in(frame);
    match with(stores, which, |direct, driver| direct.read(driver, key, &mut into)) {
        Read::Found { len } => {
            let all = into.leak();
            Ok(Some(&all[all.len() - len..]))
        }
        Read::Absent => Ok(None),
        Read::Failed(failed) => Err(failed),
    }
}

/// The values of `keys` in `which` store, each read whole and verified into `frame`, one
/// answer each in the order of `keys`: the records in file order, neighbours in one read.
pub(crate) fn read_many<'f>(
    stores: &Stores,
    which: Which,
    keys: &[Key],
    frame: &'f Frame,
) -> Vec<Result<Option<&'f [u8]>, Failed>, &'f Frame> {
    let mut into = Vec::new_in(frame);
    let mut places = Vec::with_capacity_in(keys.len(), frame);
    with(stores, which, |direct, driver| direct.read_many(driver, keys, &mut into, &mut places));
    let all: &'f [u8] = into.leak();
    let mut out = Vec::with_capacity_in(keys.len(), frame);
    out.extend(places.into_iter().map(|placed| match placed {
        Placed::Found { at, len } => Ok(Some(&all[at..at + len])),
        Placed::Absent => Ok(None),
        Placed::Failed(failed) => Err(failed),
    }));
    out
}

/// The value of `key` in `which` store read in place through a mapping: one of
/// [`blaze_cas::direct::MAP_FROM`] bytes or more kept raw, or kept as a file of its own.
pub(crate) fn map(stores: &Stores, which: Which, key: &Key) -> Result<Option<Mapped>, Failed> {
    with(stores, which, |direct, driver| direct.map(driver, key))
}

/// The value of `key` in `which` store, however it is kept: read into `frame`, or, kept as a
/// file of its own (an output long enough), read in place through a mapping. What a reader of
/// a value of any size asks: a unit's output, a module's machine code.
pub(crate) fn value<'f>(
    stores: &Stores,
    which: Which,
    key: &Key,
    frame: &'f Frame,
) -> Result<Option<Value<'f>>, Failed> {
    match read(stores, which, key, frame) {
        Ok(found) => Ok(found.map(Value::Read)),
        // The one form a read refuses that a mapping serves; told by the form its error names
        // until `direct::Failed::Form` carries the form itself (lane M3's "needed elsewhere").
        Err(Failed::Form { form: "as a file of its own", .. }) => Ok(map(stores, which, key)?.map(Value::Mapped)),
        Err(failed) => Err(failed),
    }
}

/// The value of `key` in `which` store as bytes of the caller's own, for a value that outlives
/// the reading task (a module handed to its loader, a put into another store): read straight
/// into the one owned buffer, never through the frame, or, kept as a file of its own, copied
/// once out of its mapping.
pub(crate) fn owned(stores: &Stores, which: Which, key: &Key) -> Result<Option<Vec<u8>>, Failed> {
    let mut into = Vec::new(); // alloc: the value, owned by what outlives the task
    match with(stores, which, |direct, driver| direct.read(driver, key, &mut into)) {
        Read::Found { .. } => Ok(Some(into)),
        Read::Absent => Ok(None),
        Read::Failed(Failed::Form { form: "as a file of its own", .. }) => {
            // alloc: as above
            Ok(map(stores, which, key)?.map(|mapped| mapped.to_vec()))
        }
        Read::Failed(failed) => Err(failed),
    }
}

/// The slots of a record key in the home store (`unit::reads::slot`), `keys`, read at once:
/// what a unit record's lookup and filing walk, whose keys are known before any is valued. One
/// [`read_many`] where the walk read a slot at a time; a slot not kept costs a lookup and no
/// read. A slot whose read failed is `Err`.
pub(crate) fn slots<'f>(stores: &Stores, keys: impl Iterator<Item = Key>, frame: &'f Frame) -> Slots<'f> {
    let mut all = Vec::new_in(frame);
    all.extend(keys);
    let values = read_many(stores, Which::Home, &all, frame);
    Slots { keys: all, values }
}

/// A record key's slots, read ([`slots`]).
pub(crate) struct Slots<'f> {
    keys: Vec<Key, &'f Frame>,
    values: Vec<Result<Option<&'f [u8]>, Failed>, &'f Frame>,
}

impl<'f> Slots<'f> {
    /// The slot `key`'s value as read: `None` for a key that is no slot of these.
    pub(crate) fn get(&self, key: &Key) -> Option<&Result<Option<&'f [u8]>, Failed>> {
        self.keys.iter().position(|it| it == key).map(|at| &self.values[at])
    }

    /// [`Slots::get`] as `unit::reads`' walks ask it: a slot's bytes, or none where the slot
    /// is not kept, is no slot of these, or did not read (each walk's own rule for a read that
    /// failed is its caller's: [`Slots::failed`]).
    pub(crate) fn bytes(&self, key: &Key) -> Option<&'f [u8]> {
        match self.get(key) {
            Some(Ok(found)) => *found,
            _ => None,
        }
    }

    /// The first slot whose read failed, with its key.
    pub(crate) fn failed(&self) -> Option<(Key, &Failed)> {
        self.keys.iter().zip(&self.values).find_map(|(key, value)| value.as_ref().err().map(|it| (*key, it)))
    }
}

/// The store's own page pool, which stays until blaze-cas deletes it (lane M4 kept it): the
/// store's commits, opens and collections load through it, and so do the reads that could not
/// yet move to [`blaze_cas::direct`] (lane M3's report lists them). Every `load_tags` and
/// `loader` blaze-cargo hands a store is made here, so the day the pool goes this module and
/// the two fields at each config are the one deletion. Its loads ask with a range of their
/// own, apart from the direct readers' ([`HOME_TAGS`], [`STD_TAGS`]) and the store's.
pub(crate) mod pool {
    use std::path::Path;

    use blaze_cas::Loader;

    /// The loads' tags of a store asking with `tags` where its config names no other range:
    /// the next `2^32`, as `blaze_cas::Config::load_tags` asks.
    pub(crate) const fn loads_after(tags: u64) -> u64 {
        tags + (1 << 32)
    }

    /// How a daemon's stores' pools load: on the store's own thread over the same root as a
    /// worker's own driver ([`super::Reads::Native`]), or as effects the loop's driver carries
    /// out, where the loop's driver is lent to the tasks too ([`super::Reads::Lent`]).
    pub(crate) fn loader(reads: &super::Reads) -> Loader {
        match reads {
            super::Reads::Native { root } => thread(root),
            super::Reads::Lent => pumped(),
        }
    }

    /// Loads on the store's own thread, over `root`: a tool's store on its own.
    pub(crate) fn thread(root: &Path) -> Loader {
        Loader::Thread { root: root.to_path_buf() } // alloc: once per store opened, a path
    }

    /// Loads as effects of the store's that whoever drives it carries out: a test's store on
    /// the simulation.
    pub(crate) fn pumped() -> Loader {
        Loader::Pumped
    }
}
