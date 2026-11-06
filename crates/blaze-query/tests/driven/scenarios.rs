//! The database's waits on a driven pool ([`Pool::driven`]), as plain functions: the native
//! test binary (`tests/driven.rs`) runs each, and so does the seed's probe, built for
//! `wasm64-blaze-wasip1` and run under `blaze-seed-run`, where every pool is driven because the
//! target has no threads. A query that waits for another's flight switches out and frees the
//! one worker; a wait that would close a cycle across two views is cancelled and asked again.
//! None starts a thread, sleeps or catches a panic (the seed aborts on one).

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use blaze_format::{FormatError, Persist, Record, SchemaHash, Writer};
use blaze_graph::{
    pool::Pool,
    sched::{Event, EventWait},
    topology::Topology,
};
use blaze_ids::ContentId;
use blaze_query::{Cancelled, Config, Cx, Database, DbError, MemTree, Query, QueryKind};
use blaze_vfs::{View, ViewId};
use blaze_core::lock::Lock;

#[derive(Clone, Copy)]
#[repr(C)]
struct Word(u64);

const WORD: SchemaHash = SchemaHash(ContentId([0x58; 32]));

// SAFETY: a `u64` in a `repr(C)` struct: no padding, no pointer, every bit pattern valid.
unsafe impl Record for Word {
    const SCHEMA: SchemaHash = WORD;
    fn check(_: &[u8], _: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }
}

struct Num(u64);

impl Persist for Num {
    const SCHEMA: SchemaHash = WORD;
    type Ref<'a> = u64;
    fn write(&self, out: &mut Writer) -> u64 {
        out.push(&Word(self.0))
    }
    fn read(buffer: &[u8]) -> Result<u64, FormatError> {
        blaze_format::open::<Word>(buffer).map(|word| word.0)
    }
}

fn key_id(key: &str) -> ContentId {
    ContentId::of("blaze v2 driven db test key 1", key.as_bytes())
}

/// The length of a file, `u64::MAX` for none.
struct Len;

impl Query for Len {
    const KIND: QueryKind = QueryKind(0x2f40);
    const NAME: &'static str = "len";
    const PRODUCER: ContentId = ContentId([0x40; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        Ok(Num(cx.file(key)?.map_or(u64::MAX, |file| file.bytes.len() as u64)))
    }
}

/// What [`Gate`] waits on, set by the scenario before it is asked.
static GATE: Lock<Option<EventWait>> = Lock::new("query.test.driven.gate", None);

/// 5, once the scenario's event fired: a query that waits inside its run, holding its key.
struct Gate;

impl Query for Gate {
    const KIND: QueryKind = QueryKind(0x2f41);
    const NAME: &'static str = "gate";
    const PRODUCER: ContentId = ContentId([0x41; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("gate")
    }
    fn execute(_: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        let waiter = GATE.lock().take().expect("the scenario set the gate's event");
        waiter.wait().expect("a wait on an event closes no cycle");
        Ok(Num(5))
    }
}

/// 1 + [`Gate`]: a query that waits for a key another job's query runs.
struct AsksGate;

impl Query for AsksGate {
    const KIND: QueryKind = QueryKind(0x2f42);
    const NAME: &'static str = "asks gate";
    const PRODUCER: ContentId = ContentId([0x42; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("asks gate")
    }
    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        Ok(Num(1 + cx.get::<Gate>(&())?))
    }
}

/// How many runs of [`Chain`] began, and the event its first waits on for the second.
static MET: AtomicUsize = AtomicUsize::new(0);
static MEETING: Lock<Option<Event>> = Lock::new("query.test.driven.meeting", None);

/// The file names another; 1 + [`Chain`] of it, 0 for an empty file. Its first run waits for
/// the second to begin, so two runs hold a key each before asking the other's.
struct Chain;

impl Query for Chain {
    const KIND: QueryKind = QueryKind(0x2f43);
    const NAME: &'static str = "chain";
    const PRODUCER: ContentId = ContentId([0x43; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        match MET.fetch_add(1, Ordering::SeqCst) {
            0 => {
                let event = Event::new();
                let waiter = event.waiter();
                *MEETING.lock() = Some(event);
                waiter.wait().expect("a wait on an event closes no cycle");
            }
            1 => MEETING.lock().take().expect("the first run left its event").fire(),
            _ => {}
        }
        let name = match cx.file(key)? {
            Some(file) => String::from_utf8(file.bytes.to_vec()).expect("a name"),
            None => String::new(),
        };
        if name.is_empty() {
            return Ok(Num(0));
        }
        Ok(Num(1 + cx.get::<Chain>(&name)?))
    }
}

fn setup() -> (Arc<Database>, Arc<MemTree>) {
    let tree = Arc::new(MemTree::default());
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: None, tags: 1 << 40 });
    db.register::<Len>();
    db.register::<Gate>();
    db.register::<AsksGate>();
    db.register::<Chain>();
    (Arc::new(db), tree)
}

fn view(db: &Database, tree: &MemTree, files: &[(&str, &str)]) -> ViewId {
    let files: Vec<(&str, &[u8])> =
        files.iter().map(|(path, bytes)| (*path, bytes.as_bytes())).collect();
    db.open(View { root: tree.root(&files) })
}

/// Every scenario that holds on the seed, by name.
pub const ON_THE_SEED: &[(&str, fn())] = &[
    (
        "a_query_waiting_on_the_one_worker_frees_it_for_other_jobs",
        a_query_waiting_on_the_one_worker_frees_it_for_other_jobs,
    ),
    (
        "a_wait_that_would_close_a_cycle_across_views_is_cancelled",
        a_wait_that_would_close_a_cycle_across_views_is_cancelled,
    ),
    (
        "a_query_asked_again_at_once_after_giving_way_ends",
        a_query_asked_again_at_once_after_giving_way_ends,
    ),
];

/// A query waiting inside its run parks its job and frees the pool's one worker: another job's
/// query that asks its key waits on its flight, parked too, and a third job's query runs
/// meanwhile, sees both parked and lets the first go; both waiting queries then get the value.
pub fn a_query_waiting_on_the_one_worker_frees_it_for_other_jobs() {
    type Ended = Lock<(Vec<(&'static str, Result<u64, DbError>)>, Option<Event>)>;
    /// Notes what a job's query came to; the third to end fires the scenario's own wait.
    fn end(ended: &Ended, job: &'static str, value: Result<u64, DbError>) {
        let mut ended = ended.lock();
        ended.0.push((job, value));
        if ended.0.len() == 3 {
            ended.1.take().expect("the scenario waits").fire();
        }
    }
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    let pool = Arc::new(Pool::driven(&Topology::flat(1)).expect("a driven pool"));
    let gate = Event::new();
    *GATE.lock() = Some(gate.waiter());
    let all = Event::new();
    let all_ended = all.waiter();
    let ended: Arc<Ended> = Arc::new(Lock::new("query.test.driven.ended", (Vec::new(), Some(all))));
    let parked = Arc::new(AtomicUsize::new(usize::MAX));
    let job = {
        let (db, pool, ended, parked) =
            (Arc::clone(&db), Arc::clone(&pool), Arc::clone(&ended), Arc::clone(&parked));
        move || {
            {
                let (db, pool_, ended) = (Arc::clone(&db), Arc::clone(&pool), Arc::clone(&ended));
                pool.submit(
                    None,
                    move || {
                        {
                            let (db, pool__, ended) =
                                (Arc::clone(&db), Arc::clone(&pool_), Arc::clone(&ended));
                            pool_.submit(
                                None,
                                move || {
                                    let len = db.pin().get::<Len>(one, &"a".to_owned());
                                    parked.store(pool__.parked(), Ordering::SeqCst);
                                    end(&ended, "len", len);
                                    gate.fire();
                                },
                                |it| it.expect("the len job does not panic"),
                            );
                        }
                        end(&ended, "asks gate", db.pin().get::<AsksGate>(one, &()));
                    },
                    |it| it.expect("the asking job does not panic"),
                );
            }
            end(&ended, "gate", db.pin().get::<Gate>(one, &()));
        }
    };
    pool.submit(None, job, |it| it.expect("the gate job does not panic"));
    all_ended.wait().expect("a wait on an event closes no cycle");
    assert_eq!(
        parked.load(Ordering::SeqCst),
        2,
        "the gate's and its asker's jobs were parked while len ran"
    );
    assert_eq!(ended.lock().0, [("len", Ok(3)), ("gate", Ok(5)), ("asks gate", Ok(6))]);
}

/// Two jobs ask, in two views, keys whose runs each ask the other's: the wait that would close
/// the cycle is cancelled rather than both waiting forever, and of the cycle exactly one gives
/// way. As in a loop of [`blaze_graph::par`], what gave way is asked again once the others are
/// done, and gets its value. ([`a_query_asked_again_at_once_after_giving_way_ends`] asks again
/// at once instead.)
pub fn a_wait_that_would_close_a_cycle_across_views_is_cancelled() {
    let (db, tree) = setup();
    MET.store(0, Ordering::SeqCst);
    let x = view(&db, &tree, &[("a", "b"), ("b", "")]);
    let y = view(&db, &tree, &[("a", ""), ("b", "a")]);
    let pool = Pool::driven(&Topology::flat(1)).expect("a driven pool");
    let ask = |view: ViewId, name: &str| match db.pin().get::<Chain>(view, &name.to_owned()) {
        Ok(n) => Some(n),
        Err(DbError::Cancelled) => None,
        Err(error) => panic!("chain of {name}: {error}"),
    };
    let (mut in_x, mut in_y) = (None, None);
    pool.scope(|scope| {
        let (ask, in_x, in_y) = (&ask, &mut in_x, &mut in_y);
        scope.submit(None, move || ask(x, "a"), |it| *in_x = Some(it.expect("x does not panic")));
        scope.submit(None, move || ask(y, "b"), |it| *in_y = Some(it.expect("y does not panic")));
    });
    let (in_x, in_y) = (in_x.expect("x ran"), in_y.expect("y ran"));
    assert!(MET.load(Ordering::SeqCst) >= 2, "both runs met");
    assert_eq!(
        usize::from(in_x.is_none()) + usize::from(in_y.is_none()),
        1,
        "exactly one gave way: x {in_x:?}, y {in_y:?}"
    );
    let in_x = in_x.or_else(|| ask(x, "a"));
    let in_y = in_y.or_else(|| ask(y, "b"));
    assert_eq!((in_x, in_y), (Some(1), Some(1)));
}

/// How many times a job of [`a_query_asked_again_at_once_after_giving_way_ends`] asks before
/// it stops and says it never ended: the deadline of a livelock, which gives way this often
/// within milliseconds.
const ASKS: u32 = 1000;

/// The cycle of [`a_wait_that_would_close_a_cycle_across_views_is_cancelled`], each job asking
/// again at once whenever it gave way, as a caller that loops on [`DbError::Cancelled`] does.
/// On the one worker the job that gave way used to claim its key again before the job it gave
/// way to resumed, and the two gave way in turn forever. It now waits for the run it gave way to
/// to end first: exactly one gives way, once, and both get their value.
pub fn a_query_asked_again_at_once_after_giving_way_ends() {
    let (db, tree) = setup();
    MET.store(0, Ordering::SeqCst);
    let x = view(&db, &tree, &[("a", "b"), ("b", "")]);
    let y = view(&db, &tree, &[("a", ""), ("b", "a")]);
    let pool = Pool::driven(&Topology::flat(1)).expect("a driven pool");
    let ask_until_done = |view: ViewId, name: &str| -> Result<(u64, u32), u32> {
        for gave_way in 0..ASKS {
            match db.pin().get::<Chain>(view, &name.to_owned()) {
                Ok(n) => return Ok((n, gave_way)),
                Err(DbError::Cancelled) => {}
                Err(error) => panic!("chain of {name}: {error}"),
            }
        }
        Err(ASKS)
    };
    let (mut in_x, mut in_y) = (None, None);
    pool.scope(|scope| {
        let (ask, in_x, in_y) = (&ask_until_done, &mut in_x, &mut in_y);
        scope.submit(None, move || ask(x, "a"), |it| *in_x = Some(it.expect("x does not panic")));
        scope.submit(None, move || ask(y, "b"), |it| *in_y = Some(it.expect("y does not panic")));
    });
    let (in_x, in_y) = (in_x.expect("x ran"), in_y.expect("y ran"));
    assert!(MET.load(Ordering::SeqCst) >= 2, "both runs met");
    let (Ok((x_value, x_gave)), Ok((y_value, y_gave))) = (in_x, in_y) else {
        panic!(
            "a job never ended: it gave way {ASKS} times, asking again at once (x {in_x:?}, y {in_y:?})"
        );
    };
    assert_eq!((x_value, y_value), (1, 1), "each view's chain");
    assert_eq!(x_gave + y_gave, 1, "exactly one gave way, once: x {x_gave}, y {y_gave}");
}
