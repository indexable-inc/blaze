//! The table ([`super::Flights`]) against the design it replaced ([`super::sharded::Sharded`]):
//! the same steps by the same tasks give the same answers. And the microbench of the two.

use std::{
    cell::Cell,
    io::Write as _,
    panic::AssertUnwindSafe,
    thread::Scope,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use hegel::{TestCase, generators as gs};

use super::{Flights, sharded::Sharded};
use crate::{Cancelled, Key};

const ACTORS: usize = 3;
const KEYS: usize = 3;
const LONG: Duration = Duration::from_secs(30);

fn key(n: usize) -> Key {
    let mut key = [0x5a; 32];
    key[0] = n as u8;
    Key(key)
}

fn below(tc: &TestCase, n: usize) -> usize {
    tc.draw(gs::integers::<u32>().max_value(n as u32 - 1)) as usize
}

// dyn: a hold of either design, let go once by the actor that holds it
type Release<'a> = Box<dyn FnOnce(Option<u32>) + 'a>;

/// What the property and the bench ask of a design.
trait Design: Sync {
    fn ask(&self, key: Key, value: u32) -> Result<u32, Cancelled>;
    fn if_idle(&self, key: Key, value: u32) -> Option<Result<u32, Cancelled>>;
    fn hold(&self, key: Key) -> Option<Release<'_>>;
    fn running(&self) -> usize;
    fn waiters(&self, key: Key) -> usize;
}

impl Design for Flights<u32> {
    fn ask(&self, key: Key, value: u32) -> Result<u32, Cancelled> {
        self.get_or_run(key, || Ok(value))
    }

    fn if_idle(&self, key: Key, value: u32) -> Option<Result<u32, Cancelled>> {
        self.run_if_idle(key, || Ok(value))
    }

    fn hold(&self, key: Key) -> Option<Release<'_>> {
        let held = Flights::hold(self, key)?;
        Some(Box::new(move |value| match value {
            Some(value) => held.end(&value),
            None => drop(held),
        }))
    }

    fn running(&self) -> usize {
        Flights::running(self)
    }

    fn waiters(&self, key: Key) -> usize {
        self.runs.waiters(&key)
    }
}

impl Design for Sharded<u32> {
    fn ask(&self, key: Key, value: u32) -> Result<u32, Cancelled> {
        self.get_or_run(key, || Ok(value))
    }

    fn if_idle(&self, key: Key, value: u32) -> Option<Result<u32, Cancelled>> {
        self.run_if_idle(key, || Ok(value))
    }

    fn hold(&self, key: Key) -> Option<Release<'_>> {
        Sharded::hold(self, key)?;
        let held = ShardedHeld { design: self, key, ended: false };
        Some(Box::new(move |value| held.end(value)))
    }

    fn running(&self) -> usize {
        Sharded::running(self)
    }

    fn waiters(&self, key: Key) -> usize {
        Sharded::waiters(self, &key)
    }
}

/// A run of the sharded design held by an actor: dropped unreleased (an actor let go when the
/// property failed), it ends as a cancel, as the table's own hold does, so its waiters wake.
struct ShardedHeld<'a> {
    design: &'a Sharded<u32>,
    key: Key,
    ended: bool,
}

impl ShardedHeld<'_> {
    fn end(mut self, value: Option<u32>) {
        self.ended = true;
        self.design.end(&self.key, value.as_ref());
    }
}

impl Drop for ShardedHeld<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.design.end(&self.key, None);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Do {
    Ask(usize, u32),
    IfIdle(usize, u32),
    Hold(usize),
    Release(usize, Option<u32>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Heard {
    Asked(Result<u32, Cancelled>),
    /// An ask woken by a cancel, which asked again: it ran the key or was given the run of
    /// another waiter woken with it, whichever asked first.
    AskedAfterCancel,
    IfIdle(Option<Result<u32, Cancelled>>),
    Held(bool),
    Released,
}

/// A design's actors: each a thread, its own job of the scheduler, doing what it is told in
/// order and saying what came of it.
struct World<'d> {
    name: &'static str,
    design: &'d dyn Design,
    to: Vec<Sender<Do>>,
    heard: Vec<Receiver<Heard>>,
    /// What each actor was last told.
    told: Cell<[Option<Do>; ACTORS]>,
    /// Each actor's last ask, which a holder told to let go may still be parked in.
    asked: Cell<[Option<Do>; ACTORS]>,
}

/// Says on `left` that an actor's thread ended, however it ended.
struct Gone(Sender<(&'static str, usize)>, &'static str, usize);

impl Drop for Gone {
    fn drop(&mut self) {
        _ = self.0.send((self.1, self.2));
    }
}

impl<'d> World<'d> {
    fn new<'s>(
        scope: &'s Scope<'s, 'd>,
        name: &'static str,
        design: &'d (dyn Design + 'd),
        left: &Sender<(&'static str, usize)>,
    ) -> World<'d> {
        let (mut to_all, mut heard) = (Vec::new(), Vec::new());
        for actor in 0..ACTORS {
            let (to, todo) = unbounded::<Do>();
            let (say, said) = unbounded::<Heard>();
            let gone = Gone(left.clone(), name, actor);
            scope.spawn(move || {
                let _gone = gone;
                _ = crate::sched::job_here();
                let mut held: Vec<Option<Release<'d>>> = (0..KEYS).map(|_| None).collect();
                for step in todo {
                    let heard = match step {
                        Do::Ask(at, value) => Heard::Asked(design.ask(key(at), value)),
                        Do::IfIdle(at, value) => Heard::IfIdle(design.if_idle(key(at), value)),
                        Do::Hold(at) => {
                            let hold = design.hold(key(at));
                            let got = hold.is_some();
                            if got {
                                held[at] = hold;
                            }
                            Heard::Held(got)
                        }
                        Do::Release(at, value) => {
                            held[at].take().expect("the actor holds the key")(value);
                            Heard::Released
                        }
                    };
                    if say.send(heard).is_err() {
                        break;
                    }
                }
            });
            to_all.push(to);
            heard.push(said);
        }
        World { name, design, to: to_all, heard, told: Cell::new([None; ACTORS]), asked: Cell::new([None; ACTORS]) }
    }

    fn tell(&self, actor: usize, todo: Do) {
        let mut told = self.told.get();
        told[actor] = Some(todo);
        self.told.set(told);
        if let Do::Ask(..) = todo {
            let mut asked = self.asked.get();
            asked[actor] = Some(todo);
            self.asked.set(asked);
        }
        self.to[actor].send(todo).unwrap();
    }

    fn hear(&self, actor: usize) -> Heard {
        self.heard[actor].recv_timeout(LONG).unwrap_or_else(|_| {
            hung(format!(
                "{} actor {actor} said nothing in {LONG:?}; last told {:?}, its last ask {:?}",
                self.name,
                self.told.get()[actor],
                self.asked.get()[actor]
            ))
        })
    }

    /// Waits until `count` waiters are linked to `at`'s run, or `actor` said something.
    fn linked_or_heard(&self, at: usize, count: usize, actor: usize) -> Option<Heard> {
        let until = Instant::now() + LONG;
        loop {
            if let Ok(heard) = self.heard[actor].try_recv() {
                return Some(heard);
            }
            if self.design.waiters(key(at)) == count {
                return None;
            }
            if Instant::now() >= until {
                hung(format!("{} actor {actor}'s ask of key {at} neither linked nor ended in {LONG:?}", self.name));
            }
            std::thread::yield_now();
        }
    }
}

/// What each actor is doing, as the steps so far say.
#[derive(Default)]
struct Expect {
    holder: [Option<usize>; KEYS],
    /// The key each actor's ask waits for.
    waits: [Option<usize>; ACTORS],
}

impl Expect {
    /// Whether `actor`'s wait for `at` would close a cycle: who runs `at` waits, through runs
    /// held and waits, for `actor`.
    fn closes_cycle(&self, actor: usize, at: usize) -> bool {
        let mut runner = self.holder[at];
        for _ in 0..=ACTORS {
            match runner {
                Some(it) if it == actor => return true,
                Some(it) => runner = self.waits[it].and_then(|at| self.holder[at]),
                None => return false,
            }
        }
        false
    }

    fn waiting_on(&self, at: usize) -> usize {
        self.waits.iter().filter(|it| **it == Some(at)).count()
    }
}

/// Random steps by three tasks over three keys (asks, runs if idle, holds and their ends or
/// cancels), each given to both designs: every answer, every woken ask's and every count of
/// runs under way is the same.
#[hegel::test(test_cases = 64)]
fn the_table_answers_as_the_sharded_design_did(tc: TestCase) {
    let table = Flights::<u32>::new();
    let sharded = Sharded::<u32>::new();
    let (left, gone) = unbounded();
    let watch = Watch::start();
    std::thread::scope(|scope| {
        let worlds = [World::new(scope, "table", &table, &left), World::new(scope, "sharded", &sharded, &left)];
        let checked = std::panic::catch_unwind(AssertUnwindSafe(|| check(&tc, &worlds, &watch)));
        watch.at(format_args!("joining the actors after the case ({})", if checked.is_ok() { "passed" } else { "failed" }));
        let Err(failed) = checked else { return };
        let told = worlds.each_ref().map(|world| (world.name, world.told.get()));
        drop(worlds);
        let mut running: Vec<(&str, usize)> =
            told.iter().flat_map(|(name, _)| (0..ACTORS).map(move |actor| (*name, actor))).collect();
        let until = Instant::now() + LONG;
        while !running.is_empty() {
            let Ok(ended) = gone.recv_deadline(until) else {
                let why = failed
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| failed.downcast_ref::<&str>().copied())
                    .unwrap_or("a panic with no message");
                let stuck: Vec<_> = running
                    .iter()
                    .map(|(name, actor)| {
                        let last = told.iter().find(|(it, _)| it == name).and_then(|(_, told)| told[*actor]);
                        format!("{name} actor {actor}, last told {last:?}")
                    })
                    .collect();
                // stderr itself, not eprintln: the harness keeps a test's eprintln until it ends,
                // and an abort ends nothing.
                _ = writeln!(
                    std::io::stderr(),
                    "the oracle failed ({why}), and {LONG:?} after its actors were let go these never \
                     finished, a run that never ended or a wake that was lost: {}",
                    stuck.join("; ")
                );
                std::process::abort();
            };
            running.retain(|it| *it != ended);
        }
        std::panic::resume_unwind(failed);
    });
    watch.end();
}

/// Longer than any case's own bounds add up to (a hear's [`LONG`], then the drain's [`LONG`]
/// before it aborts): a case still running past it is blocked where no bound reaches.
const CASE: Duration = Duration::from_secs(90);

/// The whole run's bound once a case hung: hegel replays a hung case while it shrinks, each
/// replay a [`LONG`] of its own, so no one case passes [`CASE`]. Under hegel's own 300 s shrink
/// budget, and far over a passing run's second.
const RUN: Duration = Duration::from_secs(200);

/// Where the running case is, as [`Watch::at`] last named it.
static PLACE: parking_lot::Mutex<String> = parking_lot::const_mutex(String::new());

/// The first wait of the run that timed out ([`hung`]).
static HUNG: parking_lot::Mutex<Option<String>> = parking_lot::const_mutex(None);

/// A bounded wait of the case timed out: said on stderr at once (hegel keeps a case's panic
/// until it ends shrinking) and kept as the run's first hang, then the case fails with it.
fn hung(why: String) -> ! {
    let mut first = HUNG.lock();
    if first.is_none() {
        _ = writeln!(std::io::stderr(), "the oracle hung: {why}");
        *first = Some(why.clone());
    }
    drop(first);
    panic!("{why}")
}

/// Says `why` on stderr itself (an abort ends nothing the harness keeps), with a sample of the
/// process where `sample` exists, and aborts.
fn sample_and_abort(why: &str) -> ! {
    let pid = std::process::id();
    let sampled = std::env::temp_dir().join(format!("blaze-oracle-hang-{pid}.txt"));
    let took = std::process::Command::new("/usr/bin/sample")
        .args([pid.to_string().as_str(), "3", "-file"])
        .arg(&sampled)
        .status()
        .map_or_else(|why| format!("no sample ({why})"), |_| format!("sampled to {}", sampled.display()));
    _ = writeln!(std::io::stderr(), "{why}; {took}");
    std::process::abort()
}

/// A case's deadline, watched from a thread of its own: past [`CASE`] it says where the case
/// was (the step and the call [`Watch::at`] last named), samples the process, and aborts, so a
/// hang fails naming itself rather than holding the run. The first case also starts the run's
/// watch: past [`RUN`], if a wait has timed out ([`hung`]), it aborts naming that wait.
struct Watch {
    ended: Option<Sender<()>>,
}

impl Watch {
    fn start() -> Watch {
        static RUN_WATCH: std::sync::Once = std::sync::Once::new();
        RUN_WATCH.call_once(|| {
            std::thread::spawn(|| {
                let (_never, quiet) = unbounded::<()>();
                _ = quiet.recv_timeout(RUN);
                let Some(first) = HUNG.lock().clone() else { return };
                let at = PLACE.lock().clone();
                sample_and_abort(&format!(
                    "the oracle ran past {RUN:?} while hegel replayed a hung case: first {first}; now at {at}"
                ));
            });
        });
        *PLACE.lock() = String::from("starting the actors");
        let (ended, watched) = unbounded::<()>();
        std::thread::spawn(move || {
            if watched.recv_timeout(CASE) != Err(crossbeam_channel::RecvTimeoutError::Timeout) {
                return;
            }
            let at = PLACE.lock().clone();
            sample_and_abort(&format!("the oracle's case ran past {CASE:?}, blocked where no bound of its own reaches: at {at}"));
        });
        Watch { ended: Some(ended) }
    }

    /// Where the case is now, for the message if it hangs here.
    fn at(&self, now: std::fmt::Arguments<'_>) {
        let mut at = PLACE.lock();
        at.clear();
        _ = std::fmt::Write::write_fmt(&mut *at, now);
    }

    fn end(mut self) {
        self.ended.take();
    }
}

fn check(tc: &TestCase, worlds: &[World<'_>; 2], watch: &Watch) {
    let mut expect = Expect::default();
    let steps = 1 + below(tc, 40);
    for step in 0..steps {
        let idle: Vec<usize> = (0..ACTORS).filter(|it| expect.waits[*it].is_none()).collect();
        let actor = idle[below(tc, idle.len())];
        let at = below(tc, KEYS);
        let value = 1 + step as u32;
        let todo = match (below(tc, 4), expect.holder[at] == Some(actor)) {
            (_, true) => Do::Release(at, tc.draw(gs::booleans()).then_some(value)),
            (0, false) => Do::Ask(at, value),
            (1, false) => Do::IfIdle(at, value),
            _ => Do::Hold(at),
        };
        let mut heard = Vec::new();
        for world in worlds {
            watch.at(format_args!("step {step} of {steps}, {}: told actor {actor} {todo:?}, hearing it or counting its ask's link", world.name));
            world.tell(actor, todo);
            let said = match todo {
                Do::Ask(at, _) if expect.holder[at].is_some() && !expect.closes_cycle(actor, at) => {
                    world.linked_or_heard(at, expect.waiting_on(at) + 1, actor)
                }
                _ => Some(world.hear(actor)),
            };
            let mut woken = Vec::new();
            if let Do::Release(at, value) = todo {
                for other in (0..ACTORS).filter(|it| expect.waits[*it] == Some(at)) {
                    watch.at(format_args!("step {step} of {steps}, {}: {todo:?} by actor {actor}, hearing woken actor {other}", world.name));
                    let said = match world.hear(other) {
                        Heard::Asked(Ok(_)) if value.is_none() => Heard::AskedAfterCancel,
                        it => it,
                    };
                    woken.push((other, said));
                }
            }
            watch.at(format_args!("step {step} of {steps}, {}: after {todo:?} by actor {actor}, counting the runs", world.name));
            heard.push((said, woken, world.design.running()));
        }
        assert_eq!(heard[0], heard[1], "step {step}: {todo:?}");
        let (said, woken, _) = &heard[0];
        match (todo, said) {
            (Do::Ask(at, _), None) => expect.waits[actor] = Some(at),
            (Do::Hold(at), Some(Heard::Held(true))) => expect.holder[at] = Some(actor),
            (Do::Release(at, _), _) => {
                expect.holder[at] = None;
                for (other, _) in woken {
                    expect.waits[*other] = None;
                }
            }
            _ => {}
        }
    }
    // A holder whose own ask waits hears nothing until that ask ends, so each key let go is
    // one held by an actor that waits for none; with no cycle among the waits, one always is.
    while let Some(at) = (0..KEYS).find(|&at| expect.holder[at].is_some_and(|holder| expect.waits[holder].is_none())) {
        let holder = expect.holder[at].take().expect("a held key");
        for world in worlds {
            watch.at(format_args!("the case's end, {}: actor {holder} lets go of key {at}", world.name));
            world.tell(holder, Do::Release(at, None));
            assert_eq!(world.hear(holder), Heard::Released);
            for other in (0..ACTORS).filter(|it| expect.waits[*it] == Some(at)) {
                world.hear(other);
            }
        }
        for waits in &mut expect.waits {
            if *waits == Some(at) {
                *waits = None;
            }
        }
    }
    assert_eq!(expect.holder, [None; KEYS], "the case's end: every key held is let go");
    for world in worlds {
        watch.at(format_args!("the case's end, {}: counting the runs left", world.name));
        assert_eq!(world.design.running(), 0);
    }
}

/// The microbench of the brief (docs/v2/plan/lanes/concurrency/flights-table.md): runs claimed and
/// ended with nobody waiting on one thread, 18 threads asking one hot key, and 18 threads on
/// keys of their own; the table against the sharded design, in nanoseconds a run.
#[test]
#[ignore = "bench: run with --ignored and read the table it prints"]
fn bench_flights_against_sharded() {
    const RUNS: u32 = 1 << 20;
    const THREADS: u32 = 18;
    fn per_run(threads: u32, runs: u32, each: impl Fn(u32, u32) + Sync) -> f64 {
        let began = Instant::now();
        std::thread::scope(|scope| {
            for thread in 0..threads {
                let each = &each;
                scope.spawn(move || {
                    _ = crate::sched::job_here();
                    for run in 0..runs {
                        each(thread, run);
                    }
                });
            }
        });
        began.elapsed().as_nanos() as f64 / f64::from(threads * runs)
    }
    fn numbered(n: u32) -> Key {
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(&n.to_le_bytes());
        key[4..8].copy_from_slice(&n.wrapping_mul(0x9e37_79b9).to_le_bytes());
        Key(key)
    }
    fn rows(design: &dyn Design) -> [f64; 3] {
        let alone = per_run(1, RUNS, |_, run| _ = design.ask(numbered(run % 4096), run));
        let hot = per_run(THREADS, RUNS / 16, |_, run| _ = design.ask(numbered(u32::MAX), run));
        let own = per_run(THREADS, RUNS / 16, |thread, run| _ = design.ask(numbered(thread << 24 | run % 4096), run));
        [alone, hot, own]
    }
    let table = rows(&Flights::<u32>::new());
    let sharded = rows(&Sharded::<u32>::new());
    eprintln!("| ns a run | table | sharded |\n|---|---|---|");
    for (at, name) in ["uncontended claim and end", "18 threads, one hot key", "18 threads, keys of their own"].iter().enumerate() {
        eprintln!("| {name} | {:.1} | {:.1} |", table[at], sharded[at]);
    }
}
