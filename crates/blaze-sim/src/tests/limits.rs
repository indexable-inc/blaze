//! The worked example of D165 (limits.rs, control.rs, kind.rs): a bounded log whose cap is a
//! field of its `Limits`, drawn by the simulation down to its smallest legal value; typed reach
//! points that say the run hit the cap; plants passed through a type parameter; and every plant
//! with its control. Also: a cap that no case reaches fails the run by the point's name, and a
//! draw that never varies fails a simulation by its label.

#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the verdict it judged as it is"
)]

use core::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::check::{check_given, search};
use crate::control::{NoPlant, Planted, controls};
use crate::kind::{Kind, simulate};
use crate::limits::{Limits, Point};
use crate::observe::ObserveMode;
use crate::search::{Broken, Config, Given, Verdict};
use crate::tape::Tape;

// The component, as it would be written in its own crate: its caps are one struct with the
// shipped values as a constant, taken as a value; its plants come through one type parameter.

/// The log's caps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LogLimits {
    /// The most records the log holds; at least 1.
    most_records: usize,
}

impl LogLimits {
    /// What the log ships with.
    const SHIPPED: LogLimits = LogLimits { most_records: 16 };
    /// The least legal value of each cap.
    const SMALLEST: LogLimits = LogLimits { most_records: 1 };
}

/// Room for the shipped cap and the one record past it that the plant `OffByOne` takes.
const HELD: usize = LogLimits::SHIPPED.most_records + 1;

/// A push the log refused: it was at its cap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Full;

/// A bounded log, a sans-IO core: `S` is its plant seam ([`NoPlant`] where it ships).
struct Log<S> {
    limits: LogLimits,
    held: [u64; HELD],
    len: usize,
    plant: S,
}

impl<S: Planted<LogPlant>> Log<S> {
    fn new(limits: LogLimits, plant: S) -> Log<S> {
        Log { limits, held: [0; HELD], len: 0, plant }
    }

    /// Takes `value`, unless the log is at its cap.
    fn push(&mut self, value: u64) -> Result<(), Full> {
        let cap = if self.plant.has(LogPlant::OffByOne) {
            self.limits.most_records + 1
        } else {
            self.limits.most_records
        };
        if self.len >= cap {
            if self.plant.has(LogPlant::ForgetsOnFull) {
                self.len -= 1;
            }
            return Err(Full);
        }
        self.held[self.len] = value;
        self.len += 1;
        Ok(())
    }

    fn len(&self) -> usize {
        self.len
    }

    /// The newest record held.
    fn last(&self) -> Option<u64> {
        self.len.checked_sub(1).map(|at| self.held[at])
    }
}

// Its simulation: the limits drawn, the reach points, the oracle's invariants, the plants.

impl Limits for LogLimits {
    const SHIPPED: LogLimits = LogLimits::SHIPPED;

    fn draw(tape: &Tape) -> LogLimits {
        LogLimits {
            most_records: tape.limit(
                LogLimits::SHIPPED.most_records,
                LogLimits::SMALLEST.most_records,
                "most records",
            ),
        }
    }
}

points! {
    /// Where the log's simulation has to get.
    enum LogPoint {
        /// A push was refused: the log was at its drawn cap.
        Full,
        /// The case ran with the shipped cap.
        AtShippedCap,
    }
}

/// The invariants the log's oracle checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LogBroken {
    /// The log holds more than its cap.
    PastCap { len: usize, cap: usize },
    /// The log holds other than the records it took.
    Lost { took: usize, holds: usize },
}

impl fmt::Display for LogBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogBroken::PastCap { len, cap } => write!(f, "the log holds {len} records, past its cap of {cap}"),
            LogBroken::Lost { took, holds } => write!(f, "the log took {took} records and holds {holds}"),
        }
    }
}

plants! {
    /// The bugs planted in the log.
    enum LogPlant breaks LogBroken {
        /// The log as it is.
        None,
        /// The log holds one record past its cap.
        OffByOne => LogBroken::PastCap { .. },
        /// A refused push drops the newest record.
        ForgetsOnFull => LogBroken::Lost { .. },
    }
}

/// One case: the limits drawn, then up to 40 pushes, the oracle checking after each.
fn log_run(tape: &Tape, plant: impl Planted<LogPlant>) -> Result<(), Broken<LogBroken>> {
    let limits: LogLimits = tape.limits();
    if limits == LogLimits::SHIPPED {
        tape.reach(LogPoint::AtShippedCap);
    }
    let mut log = Log::new(limits, plant);
    let mut took = 0;
    let pushes = tape.int_in(0..=40u32, "pushes");
    for at in 0..pushes {
        match log.push(u64::from(at)) {
            Ok(()) => took += 1,
            Err(Full) => tape.reach(LogPoint::Full),
        }
        let (len, cap) = (log.len(), limits.most_records);
        if len > 0 && log.last() > Some(u64::from(at)) {
            return Err(Broken::Harness(format!("the log's newest record is past push {at}")));
        }
        if len > cap {
            return Err(Broken::Oracle(LogBroken::PastCap { len, cap }));
        }
        if len != took {
            return Err(Broken::Oracle(LogBroken::Lost { took, holds: len }));
        }
    }
    Ok(())
}

/// The cases of the log's simulation, and every point of `LogPoint` required.
const LOG_CONFIG: Config = Config::cases(256).must_reach_points(<LogPoint as Point>::IDS);

kind_test! {
    /// The log as shipped holds at most its drawn cap and loses nothing it took, and the run gets
    /// to the cap and to the shipped cap. A simulation: a draw that never varied fails it.
    simulation fn the_log_holds_at_most_its_drawn_cap() {
        simulate(LOG_CONFIG, |tape| log_run(tape, NoPlant));
    }
}

kind_test! {
    /// Every plant of `LogPlant` has its control, by the invariant it names.
    simulation fn every_log_plant_is_caught_by_its_invariant() {
        controls(LOG_CONFIG.observe(ObserveMode::Enforce), |tape, plant: LogPlant| log_run(tape, plant));
    }
}

kind_test! {
    /// A test declared of a kind runs as `<name>::kind_<kind>`, which is what a runner tiers by.
    unit fn a_kind_test_is_named_by_its_kind() {
        let named = crate::check::with_test_name(|test| test.map(str::to_owned));
        if let Some(test) = named {
            assert!(test.ends_with("a_kind_test_is_named_by_its_kind::kind_unit"), "{test}");
            assert_eq!(Kind::of_test(&test), Some(Kind::Unit));
        }
        assert_eq!(Kind::of_test("blaze_cas::tests::big::kind_scale"), Some(Kind::Scale));
        assert_eq!(Kind::of_test("blaze_cas::tests::kind_scale_x"), None);
        assert_eq!(Kind::of_test("plain_test_name"), None);
    }
}

/// A limit drawn between its smallest legal value and its shipped one: calm is the smallest, it
/// never leaves the range, and over 256 seeds it is the smallest, the next value and the shipped
/// value each more than once, the smallest most often.
#[test]
fn a_limit_is_drawn_to_its_boundaries_and_sometimes_shipped() {
    assert_eq!(Tape::calm().limit(16usize, 1, "cap"), 1, "calm is the smallest legal value");
    let mut smallest = 0;
    let mut next = 0;
    let mut shipped = 0;
    for seed in 0..256 {
        let cap = Tape::seeded(seed).limit(16usize, 1, "cap");
        assert!((1..=16).contains(&cap), "seed {seed} drew {cap}");
        match cap {
            1 => smallest += 1,
            2 => next += 1,
            16 => shipped += 1,
            _ => {}
        }
    }
    assert!(smallest > next && next > 1 && shipped > 1, "{smallest} {next} {shipped}");
    assert_eq!(Tape::seeded(3).limit(7u32, 7, "fixed"), 7, "a cap with one legal value is fixed");
}

/// A cap no case reaches: the pushes are drawn below 4 and the cap is the shipped 16, so no push
/// is ever refused, and the run fails naming the point instead of passing untested.
#[test]
fn a_cap_no_case_reaches_fails_the_run_by_the_points_name() {
    const FULL: &[crate::limits::PointId] = &[LogPoint::Full.id()];
    let verdict = search(Config::cases(64).must_reach_points(FULL), |tape| {
        let mut log = Log::new(LogLimits::SHIPPED, NoPlant);
        for at in 0..tape.below(4, "pushes") {
            if log.push(at) == Err(Full) {
                tape.reach(LogPoint::Full);
            }
        }
    });
    let Verdict::Unreached { missing, .. } = verdict else {
        panic!("a cap no case reached passed: {verdict:?}")
    };
    assert_eq!(missing.as_slice(), &[LogPoint::Full.id().path()]);
    assert!(
        LogPoint::Full.id().path().ends_with("tests::limits::LogPoint::Full"),
        "{}",
        LogPoint::Full.id().path()
    );
}

/// The same run with the cap drawn reaches it: the limit, not the size of the input, is what
/// gets a case to the boundary.
#[test]
fn a_drawn_cap_is_reached_with_the_same_small_input() {
    const FULL: &[crate::limits::PointId] = &[LogPoint::Full.id()];
    let verdict = search(Config::cases(64).must_reach_points(FULL), |tape| {
        let limits: LogLimits = tape.limits();
        let mut log = Log::new(limits, NoPlant);
        for at in 0..tape.below(4, "pushes") {
            if log.push(at) == Err(Full) {
                tape.reach(LogPoint::Full);
            }
        }
    });
    assert!(matches!(verdict, Verdict::Passed(_)), "{verdict:?}");
}

/// Under `ObserveMode::Enforce` a draw that took one value in every passing case fails the run,
/// naming its label; under `Observe` the same run passes.
#[test]
fn a_draw_that_never_varied_fails_an_enforced_run_by_its_label() {
    let pinned = |tape: &Tape| {
        let _cap = tape.limit(16u32, 1, "cap");
        tape.assume(tape.below(2, "pinned") == 1);
    };
    check_given(Config::cases(200), Given::NOTHING, pinned);
    let payload = catch_unwind(AssertUnwindSafe(|| {
        check_given(Config::cases(200).observe(ObserveMode::Enforce), Given::NOTHING, pinned);
    }))
    .expect_err("a draw that never varied fails an enforced run");
    let said = payload.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(said.contains("under `ObserveMode::Enforce`"), "{said}");
    assert!(said.contains("  `pinned` never varied ("), "{said}");
    assert!(!said.contains("`cap` never varied"), "{said}");
}

/// `search` returns a verdict and judges no observation, so it refuses `Enforce` by name rather
/// than ignore it.
#[test]
fn search_refuses_enforce_by_name() {
    let payload = catch_unwind(|| search(Config::cases(1).observe(ObserveMode::Enforce), |tape| {
        let _ = tape.below(2, "n");
    }))
    .expect_err("`search` refuses `Enforce`");
    let said = payload.downcast_ref::<&str>().map(|said| (*said).to_owned()).unwrap_or_default();
    assert!(said.contains("`ObserveMode::Enforce` is judged by the driver"), "{said}");
}
