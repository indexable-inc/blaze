//! The park of one thread on a word and its wake from any other, a sans-IO core: the word's
//! states and the order of its atomics, with the kernel's wait and wake handed in ([`Futex`]).
//! On Linux the kernel is the thread's wait ring (`IORING_OP_FUTEX_WAIT` with the deadline in
//! its registered wait region, `IORING_OP_FUTEX_WAKE`); on macOS the wait on an address
//! (`os_sync_wait_on_address_with_deadline`, `wait/macos.rs`); elsewhere std's thread park.
//!
//! That no unpark is lost is proved here for every interleaving of a park and an unpark
//! ([`no_unpark_is_lost`], `crates/blaze-core/lean/BlazeCore/Wait/Parking.lean`): the two run
//! as two threads of [`crate::world::threads`], one atomic operation at a time by a schedule,
//! on that world's futex. The DST in `wait/tests.rs` runs these functions against a simulated
//! futex whose unparks come whole between two of the park's atomics ([`Futex::switch`]), with
//! a clock that moves and latencies (that a park ends by its deadline, and promptly), and
//! through the direct driver; both plant a lost wakeup ([`ParkPlant::CheckThenStore`]).
//!
//! The word is a token, as std's park is: [`unpark`] leaves [`NOTIFIED`], which the next
//! [`park`] takes and returns at once; a park waits while the word is [`PARKED`]. Only one
//! thread parks on a word; any thread unparks it.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::time::Duration;

use super::{
    Clock, Deadline, WaitError, Waited,
    retry::{Next, Retry},
};
use crate::{
    atomic::{AtomicU32, Ordering},
    world::threads::{self, Ended, Step, Woke},
};

/// No token and no one parked.
pub(crate) const EMPTY: u32 = 0;
/// An unpark's token, not yet taken.
pub(crate) const NOTIFIED: u32 = 1;
/// Its thread waits in the kernel, or is about to.
pub(crate) const PARKED: u32 = u32::MAX;

/// The kernel a park waits in.
pub(crate) trait Futex {
    /// Before the parker says it is [`PARKED`]: what a wake needs to find it (std's park
    /// records the thread). Nothing on a futex.
    fn prepare(&self, word: &AtomicU32) {
        _ = word;
    }

    /// Blocks while `word` holds `expected`, until a wake of it or `deadline` on `clock`
    /// (forever for `None`): `Ready` on a wake, a word that no longer held `expected`, or a
    /// return with nothing (the parker looks at the word again); `Deadline` once it passed.
    fn wait<C: Clock>(
        &self,
        what: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError>;

    /// Wakes the thread waiting on `word`, if one is.
    fn wake(&self, word: &AtomicU32);

    /// A point between two atomics of the park or the unpark where another thread may run:
    /// nothing outside the DST, which runs its unparks here.
    fn switch(&self) {}
}

/// A safeguard of the park the DST removes, to see that it catches the removal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ParkPlant {
    None,
    /// The park looks for a token, then stores [`PARKED`] over whatever came since, in place
    /// of one compare-and-swap: an unpark between the look and the store leaves its token
    /// overwritten and wakes no one, the classic lost wakeup.
    #[cfg(test)]
    CheckThenStore,
}

/// Parks on `word` until an unpark's token (`Ready`) or `deadline` on `clock` (`Deadline`;
/// forever for `None`). A token left before the park is taken at once.
pub(crate) fn park<K: Futex, C: Clock>(
    word: &AtomicU32,
    kernel: &K,
    what: &'static str,
    deadline: Option<Deadline>,
    clock: &C,
    plant: ParkPlant,
) -> Result<Waited<()>, WaitError> {
    kernel.switch();
    if word.compare_exchange(NOTIFIED, EMPTY, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        return Ok(Waited::Ready(()));
    }
    kernel.prepare(word);
    kernel.switch();
    if plant == ParkPlant::None {
        // The only other state is a token that came since the look above: taken.
        if word.compare_exchange(EMPTY, PARKED, Ordering::AcqRel, Ordering::Acquire).is_err() {
            kernel.switch();
            word.store(EMPTY, Ordering::Relaxed);
            return Ok(Waited::Ready(()));
        }
    } else {
        if word.load(Ordering::Acquire) == NOTIFIED {
            word.store(EMPTY, Ordering::Relaxed);
            return Ok(Waited::Ready(()));
        }
        kernel.switch();
        word.store(PARKED, Ordering::Release);
    }
    let mut retry = Retry::new(what, deadline, clock.now());
    loop {
        kernel.switch();
        let waited = match kernel.wait(what, word, PARKED, deadline, clock) {
            Ok(waited) => waited,
            // Not parked any more, whatever the wait says; a token that came is still one.
            Err(error) => {
                return match word.swap(EMPTY, Ordering::Acquire) {
                    NOTIFIED => Ok(Waited::Ready(())),
                    _ => Err(error),
                };
            }
        };
        kernel.switch();
        if word.compare_exchange(NOTIFIED, EMPTY, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return Ok(Waited::Ready(()));
        }
        let passed = match waited {
            Waited::Deadline => true,
            // Woken with no token: a spurious return, looked at against the deadline and the
            // fuel of a clock that does not move.
            Waited::Ready(()) => match retry.after(clock.now()) {
                Ok(Next::Deadline) => true,
                Ok(Next::Again) => false,
                Err(error) => {
                    return match word.swap(EMPTY, Ordering::Acquire) {
                        NOTIFIED => Ok(Waited::Ready(())),
                        _ => Err(error),
                    };
                }
            },
        };
        if passed {
            kernel.switch();
            return Ok(match word.swap(EMPTY, Ordering::Acquire) {
                NOTIFIED => Waited::Ready(()),
                _ => Waited::Deadline,
            });
        }
    }
}

/// Leaves a token on `word` and wakes its parker if it is parked.
pub(crate) fn unpark<K: Futex>(word: &AtomicU32, kernel: &K) {
    kernel.switch();
    if word.swap(NOTIFIED, Ordering::Release) == PARKED {
        kernel.switch();
        kernel.wake(word);
    }
}

/// The kernel of [`no_unpark_is_lost`]: the futex of the world threads share
/// ([`threads::wait`], [`threads::wake`]). A wait with a deadline may be told its time ran
/// out at any step; one with none never is.
struct Shared;

impl Futex for Shared {
    fn prepare(&self, _: &AtomicU32) {}

    fn wait<C: Clock>(
        &self,
        _: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        _: &C,
    ) -> Result<Waited<()>, WaitError> {
        Ok(match threads::wait(word, expected, deadline.is_some()) {
            Woke::Woken | Woke::Changed | Woke::Spurious => Waited::Ready(()),
            Woke::TimedOut => Waited::Deadline,
        })
    }

    fn wake(&self, word: &AtomicU32) {
        _ = threads::wake(word);
    }

    fn switch(&self) {}
}

/// The clock of [`no_unpark_is_lost`] and of `reaping::a_reaped_child_is_reported`: one
/// reading, whenever it is read.
pub(crate) struct Still(pub(crate) Duration);

impl Clock for Still {
    fn now(&self) -> Duration {
        self.0
    }
}

/// Whether a park (with `plant`) and an unpark of one empty word, run as two threads by
/// `schedule` with the clock at `now`, end as they must:
///
/// - the unpark returned: the park is not asleep with no wake coming (no unpark is lost);
/// - both returned: the park took the token and the word is empty; or it left without it,
///   at its deadline or because its clock stood still, before the token came, and the token
///   is still there for the next park;
/// - a park says its deadline passed only where it has one.
///
/// A schedule that ends before a thread does says nothing of that thread.
fn parks(plant: ParkPlant, schedule: &[Step], now: Duration, deadline: Option<Deadline>) -> bool {
    let word = AtomicU32::new(EMPTY);
    let (parked, unparked) = threads::two(
        schedule,
        || park(&word, &Shared, "a park", deadline, &Still(now), plant),
        || unpark(&word, &Shared),
    );
    let last = word.load(Ordering::SeqCst);
    let both = match unparked {
        Ended::Done(()) => true,
        Ended::Running | Ended::Asleep => false,
    };
    match parked {
        Ended::Asleep => !both,
        Ended::Running => true,
        Ended::Done(Ok(Waited::Ready(()))) => !both || last == EMPTY,
        Ended::Done(Ok(Waited::Deadline)) => deadline.is_some() && (!both || last == NOTIFIED),
        Ended::Done(Err(_)) => !both || last == NOTIFIED,
    }
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/Wait/Parking.lean): no unpark is
/// lost, on any interleaving. For every schedule of a park and an
/// unpark of one word as two threads, one atomic operation at a time, every choice of the
/// kernel's (a wait returned for nothing, or told its time ran out where the park has a
/// deadline), every reading of a clock that stands still and every deadline or none: once the
/// unpark has returned the park is never asleep with no wake coming; where both returned, the
/// token was taken once and the word is empty, or the park had left before the token came and
/// it is there for the next; and a park with no deadline never says one passed ([`parks`]).
/// The simulation test beside it (`wait/tests.rs`, `no_unpark_is_lost_under_interruption`)
/// runs each unpark whole between two of the park's atomics; here a park's step may come
/// between the unpark's swap and its wake, and the proof is of every such run: the park's
/// eight kinds of program point and the unpark's three, 26 reachable states, each thread's
/// step with any choice of the kernel's keeps them. That test stays for what this does not
/// state: that a park ends by its deadline and promptly after its wake, on a clock that moves.
/// Its control is `the_oracle_catches_a_lost_wakeup_in_a_schedule`.
#[cfg_attr(not(test), expect(dead_code, reason = "a theorem function: `blaze prove` reads it"))]
#[blaze::theorem]
pub(crate) fn no_unpark_is_lost(schedule: Vec<Step>, now: Duration, deadline: Option<Duration>) -> bool {
    let deadline = match deadline {
        Some(at) => Some(Deadline::at(at)),
        None => None,
    };
    parks(ParkPlant::None, &schedule, now, deadline)
}

/// The kernel of the park's theorems about time: the futex of the world threads share, with
/// its deadline on that world's clock ([`threads::wait_until`]): the kernel says the time ran
/// out only at or after the deadline, and a waiter does not sleep past it.
struct Timed;

impl Futex for Timed {
    fn prepare(&self, _: &AtomicU32) {}

    fn wait<C: Clock>(
        &self,
        _: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        _: &C,
    ) -> Result<Waited<()>, WaitError> {
        let until = match deadline {
            Some(deadline) => Some(deadline.get()),
            None => None,
        };
        Ok(match threads::wait_until(word, expected, until) {
            Woke::Woken | Woke::Changed | Woke::Spurious => Waited::Ready(()),
            Woke::TimedOut => Waited::Deadline,
        })
    }

    fn wake(&self, word: &AtomicU32) {
        _ = threads::wake(word);
    }

    fn switch(&self) {}
}

/// The clock of the park's theorems about time: the world's, each reading one of the run's
/// operations ([`threads::now`]).
struct Shown;

impl Clock for Shown {
    fn now(&self) -> Duration {
        threads::now()
    }
}

/// How many of its own steps a park left alone takes at most before it has returned or
/// sleeps: its two compares, the reading it begins its retry with, the wait, the compare
/// after it and the swap it leaves with are six.
const ALONE: usize = 8;

/// Whether a park and an unpark of one empty word, on `kernel` and the world's clock reading
/// `readings`, run as two threads by `schedule`, keep the park's deadline:
///
/// - a park that says its deadline passed has one, and the clock is at or past it;
/// - with the clock at or past its deadline when the schedule ends, the park is not asleep.
fn keeps<K: Futex + Sync>(kernel: &K, schedule: &[Step], readings: &[Duration], deadline: Option<Deadline>) -> bool {
    let word = AtomicU32::new(EMPTY);
    threads::clock(readings);
    let (parked, _) = threads::two(
        schedule,
        || park(&word, kernel, "a park", deadline, &Shown, ParkPlant::None),
        || unpark(&word, kernel),
    );
    let end = threads::time();
    let passed = match deadline {
        Some(deadline) => deadline.passed(end),
        None => false,
    };
    match parked {
        Ended::Asleep => !passed,
        Ended::Done(Ok(Waited::Deadline)) => passed,
        Ended::Done(Ok(Waited::Ready(()))) | Ended::Done(Err(_)) | Ended::Running => true,
    }
}

/// Whether a park of a word (holding a token where `token`) and an unpark, run by `schedule`
/// and then left to the park alone for [`ALONE`] steps with the kernel quiet (pick 0: no
/// return for nothing, no timeout it is not owed), end with the park returned or asleep:
/// never still running. A schedule with no room for those steps (a `Vec` holds `usize::MAX`
/// at most) says nothing.
fn settles<K: Futex + Sync>(
    kernel: &K,
    schedule: Vec<Step>,
    readings: &[Duration],
    deadline: Option<Deadline>,
    token: bool,
) -> bool {
    if schedule.len() > usize::MAX - ALONE {
        return true;
    }
    let mut steps = schedule;
    let mut alone = 0;
    while alone < ALONE {
        steps.push(Step { second: false, pick: 0 });
        alone += 1;
    }
    let word = AtomicU32::new(match token {
        true => NOTIFIED,
        false => EMPTY,
    });
    threads::clock(readings);
    let (parked, _) = threads::two(
        &steps,
        || park(&word, kernel, "a park", deadline, &Shown, ParkPlant::None),
        || unpark(&word, kernel),
    );
    match parked {
        Ended::Running => false,
        Ended::Done(_) | Ended::Asleep => true,
    }
}

/// The invariant: a park keeps its deadline. For every schedule of a park and an unpark of
/// one word, every script of the clock's readings, every choice of the kernel's and every
/// deadline or none, on a kernel whose wait has the deadline ([`Timed`]) and the world's
/// clock: a park that says its deadline passed has one and the clock is at or past it (never
/// early); and with the clock at or past the deadline no schedule ends with the park asleep
/// ([`keeps`]). Run on drawn schedules and readings below; not yet proved. Its controls are
/// `the_oracle_catches_a_deadline_said_early` and
/// `the_oracle_catches_a_park_asleep_past_its_deadline`.
// when its proof (crates/blaze-core/lean/BlazeCore/Wait/ParkingTime.lean) lands: "(proved: that
// file)" in the first line above.
#[cfg_attr(not(test), expect(dead_code, reason = "a theorem function: `blaze prove` reads it"))]
#[blaze::theorem]
pub(crate) fn a_park_keeps_its_deadline(
    schedule: Vec<Step>,
    readings: Vec<Duration>,
    deadline: Option<Duration>,
) -> bool {
    let deadline = match deadline {
        Some(at) => Some(Deadline::at(at)),
        None => None,
    };
    keeps(&Timed, &schedule, &readings, deadline)
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/Wait/ParkingTime.lean): a park left
/// alone returns or sleeps. From every state a schedule of a park and an unpark reaches (the
/// word empty or holding a token, any script of readings, any deadline), [`ALONE`] more steps
/// of the park with the kernel quiet leave it returned or asleep ([`settles`]): it does not
/// spin. The proof's count of the longest such run is six: two compares, the reading, a wait
/// told at once that its time ran out, the compare after it and the swap it leaves with. With
/// [`no_unpark_is_lost`] (not asleep once the unpark returned) and
/// [`a_park_keeps_its_deadline`] (not asleep past its deadline), that is the simulation test's
/// "promptly" and "by its deadline", in steps. Run on drawn schedules below too. Its control
/// is `the_oracle_catches_a_park_that_spins`.
#[cfg_attr(not(test), expect(dead_code, reason = "a theorem function: `blaze prove` reads it"))]
#[blaze::theorem]
pub(crate) fn a_park_left_alone_returns_or_sleeps(
    schedule: Vec<Step>,
    readings: Vec<Duration>,
    deadline: Option<Duration>,
    token: bool,
) -> bool {
    let deadline = match deadline {
        Some(at) => Some(Deadline::at(at)),
        None => None,
    };
    settles(&Timed, schedule, &readings, deadline, token)
}

#[cfg(test)]
mod theorems {
    use std::alloc::Global;

    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::*;

    fn ns(n: u64) -> Duration {
        Duration::from_nanos(n)
    }

    /// The parker goes, with the world's pick.
    fn p(pick: u8) -> Step {
        Step { second: false, pick }
    }

    /// The unparker goes.
    fn u() -> Step {
        Step { second: true, pick: 0 }
    }

    fn steps(steps: &[Step]) -> Vec<Step> {
        let mut out = Vec::new_in(Global);
        out.extend_from_slice(steps);
        out
    }

    /// A schedule drawn: up to 32 steps, each thread about as often, picks 0 to 2.
    fn drawn(tape: &Tape) -> Vec<Step> {
        let mut schedule = Vec::new_in(Global);
        for _ in 0..tape.int_in(0..=32u64, "steps") {
            schedule.push(Step { second: tape.coin("the unparker goes"), pick: tape.int_in(0..=2u8, "pick") });
        }
        schedule
    }

    /// The same run as [`parks`], to see how it ended.
    fn ran(
        plant: ParkPlant,
        schedule: &[Step],
        now: Duration,
        deadline: Option<Deadline>,
    ) -> (Ended<Result<Waited<()>, WaitError>>, Ended<()>, u32) {
        let word = AtomicU32::new(EMPTY);
        let (parked, unparked) = threads::two(
            schedule,
            || park(&word, &Shared, "a park", deadline, &Still(now), plant),
            || unpark(&word, &Shared),
        );
        (parked, unparked, word.load(Ordering::SeqCst))
    }

    fn drawn_parks(tape: &Tape, plant: ParkPlant) -> Result<(), Broke> {
        let schedule = drawn(tape);
        let now = ns(tape.int_in(0..=10u64, "now"));
        let deadline = tape.option("a deadline", |tape| Deadline::at(ns(tape.int_in(0..=20u64, "deadline"))));
        tape.note(format_args!("schedule {schedule:?} now {now:?} deadline {deadline:?}"));
        let (parked, unparked, last) = ran(plant, &schedule, now, deadline);
        match (&parked, &unparked) {
            (Ended::Done(Ok(Waited::Ready(()))), Ended::Done(())) => tape.reached("a park that took the token"),
            (Ended::Done(Ok(Waited::Deadline)), Ended::Done(())) => tape.reached("a park that left at its deadline"),
            (Ended::Asleep, _) => tape.reached("a park asleep when the schedule ended"),
            _ => {}
        }
        match parks(plant, &schedule, now, deadline) {
            true => Ok(()),
            false => Err(Broke::Oracle(format!(
                "an unpark was lost, or a token: the park ended {parked:?}, the unpark {unparked:?}, the word {last:#x}"
            ))),
        }
    }

    /// The theorem's function holds where it is run: on drawn schedules of the real park and
    /// unpark, reached with a park that took its token, one that left at its deadline and one
    /// still asleep when the schedule ended.
    #[test]
    fn no_unpark_is_lost_where_it_is_run() {
        let config = Config {
            must_reach: &[
                "a park that took the token",
                "a park that left at its deadline",
                "a park asleep when the schedule ended",
            ],
            ..Config::cases(2_000)
        };
        check(config, |tape| {
            let schedule = drawn(tape);
            let now = ns(tape.int_in(0..=10u64, "now"));
            let deadline = tape.option("a deadline", |tape| ns(tape.int_in(0..=20u64, "deadline")));
            let (parked, unparked, last) = ran(ParkPlant::None, &schedule, now, deadline.map(Deadline::at));
            match (&parked, &unparked) {
                (Ended::Done(Ok(Waited::Ready(()))), Ended::Done(())) => tape.reached("a park that took the token"),
                (Ended::Done(Ok(Waited::Deadline)), Ended::Done(())) => {
                    tape.reached("a park that left at its deadline");
                }
                (Ended::Asleep, _) => tape.reached("a park asleep when the schedule ended"),
                _ => {}
            }
            match no_unpark_is_lost(schedule.clone(), now, deadline) {
                true => Ok(()),
                false => Err(Broke::Oracle(format!(
                    "schedule {schedule:?} now {now:?} deadline {deadline:?}: the park ended {parked:?}, the unpark \
                     {unparked:?}, the word {last:#x}"
                ))),
            }
        });
    }

    /// The planted bug, on the schedule that shows it: the park fails its first compare and
    /// loads an empty word; the unpark swaps its token in, sees no one parked and returns;
    /// the park stores `PARKED` over the token and waits, with no wake coming. With the
    /// compare-and-swap the park has, the same schedule ends with the token taken.
    #[test]
    fn the_oracle_catches_a_lost_wakeup_in_a_schedule() {
        let schedule = steps(&[p(0), p(0), u(), p(0), p(0)]);
        let (parked, unparked, last) = ran(ParkPlant::CheckThenStore, &schedule, ns(0), None);
        assert!(matches!((parked, unparked, last), (Ended::Asleep, Ended::Done(()), PARKED)));
        assert!(!parks(ParkPlant::CheckThenStore, &schedule, ns(0), None));
        let (parked, unparked, last) = ran(ParkPlant::None, &schedule, ns(0), None);
        assert!(matches!((parked, unparked, last), (Ended::Done(Ok(Waited::Ready(()))), Ended::Running, EMPTY)), "{last:#x}");
        assert!(parks(ParkPlant::None, &schedule, ns(0), None));
    }

    /// And drawn schedules find the plant with no help.
    #[test]
    fn the_oracle_catches_a_lost_wakeup_in_a_drawn_schedule() {
        control(Config::cases(512), ParkPlant::None, ParkPlant::CheckThenStore, "an unpark was lost", drawn_parks);
    }

    // --- The park's deadline and its promptness ------------------------------------------

    /// A kernel that says a wait's time ran out though the clock has not reached its
    /// deadline: the futex with no clock ([`threads::wait`], whose timeout is the kernel's
    /// whim).
    struct Early;

    impl Futex for Early {
        fn prepare(&self, _: &AtomicU32) {}

        fn wait<C: Clock>(
            &self,
            _: &'static str,
            word: &AtomicU32,
            expected: u32,
            deadline: Option<Deadline>,
            _: &C,
        ) -> Result<Waited<()>, WaitError> {
            Ok(match threads::wait(word, expected, deadline.is_some()) {
                Woke::Woken | Woke::Changed | Woke::Spurious => Waited::Ready(()),
                Woke::TimedOut => Waited::Deadline,
            })
        }

        fn wake(&self, word: &AtomicU32) {
            _ = threads::wake(word);
        }

        fn switch(&self) {}
    }

    /// A kernel whose wait is handed no deadline: a park on it sleeps past its own.
    struct Untimed;

    impl Futex for Untimed {
        fn prepare(&self, _: &AtomicU32) {}

        fn wait<C: Clock>(
            &self,
            _: &'static str,
            word: &AtomicU32,
            expected: u32,
            _: Option<Deadline>,
            _: &C,
        ) -> Result<Waited<()>, WaitError> {
            Ok(match threads::wait_until(word, expected, None) {
                Woke::Woken | Woke::Changed | Woke::Spurious => Waited::Ready(()),
                Woke::TimedOut => Waited::Deadline,
            })
        }

        fn wake(&self, word: &AtomicU32) {
            _ = threads::wake(word);
        }

        fn switch(&self) {}
    }

    /// A kernel whose wait never blocks: a park on it spins.
    struct Busy;

    impl Futex for Busy {
        fn prepare(&self, _: &AtomicU32) {}

        fn wait<C: Clock>(
            &self,
            _: &'static str,
            _: &AtomicU32,
            _: u32,
            _: Option<Deadline>,
            _: &C,
        ) -> Result<Waited<()>, WaitError> {
            Ok(Waited::Ready(()))
        }

        fn wake(&self, word: &AtomicU32) {
            _ = threads::wake(word);
        }

        fn switch(&self) {}
    }

    /// The clock's script, drawn: up to six readings of up to 30 ns.
    fn readings(tape: &Tape) -> Vec<Duration> {
        let mut readings = Vec::new_in(Global);
        for _ in 0..tape.int_in(0..=6u64, "readings") {
            readings.push(ns(tape.int_in(0..=30u64, "reading")));
        }
        readings
    }

    /// The theorems' functions about time hold where they are run: on drawn schedules,
    /// readings and deadlines of the real park and unpark, reached with a park that left at
    /// its deadline and one asleep before it.
    #[test]
    fn a_park_keeps_its_deadline_where_it_is_run() {
        let config = Config {
            must_reach: &["a park that left at its deadline", "a park asleep before its deadline"],
            ..Config::cases(2_000)
        };
        check(config, |tape| {
            let schedule = drawn(tape);
            let readings = readings(tape);
            let deadline = tape.option("a deadline", |tape| ns(tape.int_in(0..=20u64, "deadline")));
            let token = tape.coin("a token waiting");
            // The same run, to see how it ended.
            let word = AtomicU32::new(EMPTY);
            threads::clock(&readings);
            let (parked, _) = threads::two(
                &schedule,
                || park(&word, &Timed, "a park", deadline.map(Deadline::at), &Shown, ParkPlant::None),
                || unpark(&word, &Timed),
            );
            let end = threads::time();
            match &parked {
                Ended::Done(Ok(Waited::Deadline)) => tape.reached("a park that left at its deadline"),
                Ended::Asleep if deadline.is_some() => tape.reached("a park asleep before its deadline"),
                _ => {}
            }
            let said = |what: &str| {
                Err(Broke::Oracle(format!(
                    "{what}: schedule {schedule:?} readings {readings:?} deadline {deadline:?} token {token}: the park \
                     ended {parked:?} with the clock at {end:?}"
                )))
            };
            if !a_park_keeps_its_deadline(schedule.clone(), readings.clone(), deadline) {
                return said("a park did not keep its deadline");
            }
            if !a_park_left_alone_returns_or_sleeps(schedule.clone(), readings.clone(), deadline, token) {
                return said("a park left alone was still running");
            }
            Ok(())
        });
    }

    /// The planted kernel that says the time ran out early: the park sleeps, is told its time
    /// ran out with the clock at 0 and a deadline of 10, and says its deadline passed.
    #[test]
    fn the_oracle_catches_a_deadline_said_early() {
        let schedule = steps(&[p(0), p(0), p(0), p(0), p(2), p(0), p(0)]);
        let deadline = Some(Deadline::at(ns(10)));
        assert!(!keeps(&Early, &schedule, &[], deadline));
        assert!(keeps(&Timed, &schedule, &[], deadline));
    }

    /// The planted kernel whose wait has no deadline: the park reads the clock at 12, past
    /// its deadline of 10, and sleeps all the same.
    #[test]
    fn the_oracle_catches_a_park_asleep_past_its_deadline() {
        let schedule = steps(&[p(0), p(0), p(0), p(0)]);
        let deadline = Some(Deadline::at(ns(10)));
        let readings = [ns(12)];
        assert!(!keeps(&Untimed, &schedule, &readings, deadline));
        assert!(keeps(&Timed, &schedule, &readings, deadline));
    }

    /// The planted kernel that never blocks: the park compares, reads the clock and waits
    /// again without end, and eight steps alone leave it running.
    #[test]
    fn the_oracle_catches_a_park_that_spins() {
        assert!(!settles(&Busy, steps(&[]), &[], None, false));
        assert!(settles(&Timed, steps(&[]), &[], None, false));
        // With a token waiting it returns at its first step on any kernel.
        assert!(settles(&Busy, steps(&[]), &[], None, true));
    }
}
