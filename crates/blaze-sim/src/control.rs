//! Planted-bug controls (rule dst-not-review; D165, "a control is typed"): a test's oracle is
//! trusted only once it has been seen to catch a bug planted on purpose, and a clean run of the
//! same code has been seen to pass.
//!
//! # The typed form
//!
//! - The oracle returns `Result<(), Broken<E>>` ([`Broken`]): `E` is the component's own typed
//!   error, or a small invariant enum the test defines, and its `Display` is only what a person
//!   reads.
//! - The plants are the variants of ONE enum the test module declares with
//!   [`plants!`](crate::plants), each with the pattern over `E` it must trip:
//!   `OffByOne => LogBroken::PastCap { .. }`. The macro writes the [`Plant`] impl, so a variant
//!   without its pattern does not compile and every variant is in `Plant::ALL`.
//! - [`controls`] takes the whole enum: one clean run (`Plant::NONE`), then one planted run per
//!   variant of `ALL`. Adding a variant adds its control; a hand-written `Plant` impl whose `ALL`
//!   misses a variant fails the test naming the enum (the variant count is the compiler's).
//! - The code under test takes its plant as a PARAMETER, a type at one seam (rule 78): generic
//!   over [`Planted<P>`], called with [`NoPlant`] where it ships (which compiles every plant
//!   check to `false`) and with the plant itself under a control. Never a thread-local.
//!
//! # Outcomes
//!
//! A control that does not pass ends in one of four [`Refusal`]s, each a line naming the plant,
//! tagged so a runner tells them apart by text (`blaze-sim control <tag>: control <plant>: ..`),
//! and the test unwinds with a [`ControlsFailed`] payload, so code in the process tells them apart
//! by type (`catch_unwind` then `downcast_ref::<ControlsFailed>()`):
//!
//! - [`Refusal::Blocked`]: the clean run fails, so the control proves nothing about its plant
//!   (`control <plant>: the clean run fails, so this control proves nothing: ..`).
//! - [`Refusal::Missed`]: the planted run passed (`the planted bug was not caught in N cases`).
//! - [`Refusal::OtherInvariant`]: the planted run failed, but not by the invariant named
//!   (`caught, but by <other>, not <expected>`): another invariant, a panic, the harness.
//! - [`Refusal::Unjudged`]: the planted run ended neither failed nor passed (nothing drawn, all
//!   discarded, a required point unreached).

use core::alloc::Allocator;
use core::cell::Cell;
use core::fmt::{self, Debug, Display};
use std::panic::Location;

use crate::check::{
    enforced, observed_lines, place, report_here, say_observed, search_observing, with_test_name,
};
use crate::observe::{Caught, plant_lines_into};
use crate::search::{Broke, Broken, Config, Failure, Origin, Verdict};
use crate::tape::Tape;

// The seam, in the base crate (blaze_own::planted's docs): blaze-sim's paths keep working.
pub use blaze_own::planted::{NoPlant, Planted, Plants};

/// Why a control did not pass. Each has its own message and its own tag ([`Refusal::tag`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The clean run failed: the control could not judge its plant, and says so, not "missed".
    Blocked,
    /// The planted run passed every case: the oracle did not see the plant.
    Missed,
    /// The planted run failed, but by another invariant than the one the plant names (or by a
    /// panic, the harness, an overrun): the oracle saw something, not what was planted.
    OtherInvariant,
    /// The planted run ended neither failed nor passed: it judged nothing.
    Unjudged,
}

impl Refusal {
    /// The word a runner matches in the line `blaze-sim control <tag>: ..`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn tag(self) -> &'static str {
        match self {
            Refusal::Blocked => "blocked",
            Refusal::Missed => "missed",
            Refusal::OtherInvariant => "other-invariant",
            Refusal::Unjudged => "unjudged",
        }
    }
}

/// The most plants one enum may have (rule bounded-progress: a table has a cap).
pub const MOST_PLANTS: usize = 64;

/// The payload a failed control unwinds with, after printing [`ControlsFailed::message`] on
/// standard error: the refusal of each plant by its index in `Plant::ALL` (index 0 for the
/// one plant of the deprecated [`control`]), `None` for a plant that was caught.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ControlsFailed {
    /// What was printed: one tagged line per plant refused, then (when the clean run failed) its
    /// report.
    pub message: String,
    refusals: [Option<Refusal>; MOST_PLANTS],
    plants: usize,
}

impl ControlsFailed {
    fn new(plants: usize) -> ControlsFailed {
        assert!(
            plants <= MOST_PLANTS,
            "blaze-sim: {plants} plants in one control, past the {MOST_PLANTS} a control holds"
        );
        ControlsFailed { message: String::new(), refusals: [None; MOST_PLANTS], plants }
    }

    /// How many plants the control judged.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn plants(&self) -> usize {
        self.plants
    }

    /// The refusal of the plant at `at` in `Plant::ALL`; `None` when it was caught (or past the
    /// plants judged).
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn refusal(&self, at: usize) -> Option<Refusal> {
        if at < self.plants { self.refusals[at] } else { None }
    }

    /// Every plant was blocked by the clean run: the test proved nothing about any plant, which
    /// is not the same as an oracle that missed one.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn blocked(&self) -> bool {
        self.plants > 0
            && self.refusals[..self.plants].iter().all(|refusal| *refusal == Some(Refusal::Blocked))
    }

    /// Some plant was refused.
    fn any(&self) -> bool {
        self.refusals[..self.plants].iter().any(Option::is_some)
    }

    /// Refuses the plant at `at`, named `plant`, for `refusal`, saying `why`.
    fn refuse(&mut self, at: usize, plant: &str, refusal: Refusal, why: &str) {
        self.refusals[at] = Some(refusal);
        self.message.push_str(&format!("blaze-sim control {}: control {plant}: {why}\n", refusal.tag()));
    }

    /// Refuses every plant of `names` as blocked by the clean run's failure `clean`, then ends
    /// the message with the clean run's report, once.
    fn block_all<'n>(&mut self, names: impl Iterator<Item = &'n str>, clean: &CleanFailed) {
        let first = clean.report.lines().next().unwrap_or("");
        let why = format!("the clean run fails, so this control proves nothing: {first}");
        for (at, plant) in names.enumerate() {
            self.refuse(at, plant, Refusal::Blocked, &why);
        }
        self.message.push_str("the clean run's report:\n");
        self.message.push_str(&clean.report);
        self.message.push('\n');
    }
}

impl Display for ControlsFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The invariant a plant must trip, as a predicate over the oracle's error type `E`, and its name
/// for messages (the pattern's text, from [`plants!`](crate::plants)). Never words of a message.
pub struct Expect<E> {
    name: &'static str,
    holds: fn(&E) -> bool,
}

impl<E> Clone for Expect<E> {
    fn clone(&self) -> Expect<E> {
        *self
    }
}

impl<E> Copy for Expect<E> {}

impl<E> Debug for Expect<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Expect").field("name", &self.name).finish_non_exhaustive()
    }
}

impl<E> Expect<E> {
    /// The invariant named `name` that a broken value `E` trips when `holds` says so.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(name: &'static str, holds: fn(&E) -> bool) -> Expect<E> {
        Expect { name, holds }
    }

    /// The invariant's name.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Whether `broken` is this invariant.
    pub fn holds(self, broken: &E) -> bool {
        (self.holds)(broken)
    }
}

/// The bugs a test plants in one component: an enum with a `None` variant, the code as it is,
/// and one variant per plant, each naming the invariant it must trip. Write it with
/// [`plants!`](crate::plants), which writes this impl from the enum; written by hand, a variant
/// left out of `ALL` fails [`controls`] by the enum's name.
///
/// The seam shipped code takes a plant through, [`Planted`] and [`NoPlant`], and the marker
/// [`Plants`] this trait extends, live in blaze-own (`blaze_own::planted`, re-exported here and as
/// `blaze_core::planted`), since production crates cannot depend on blaze-sim.
///
/// `E`, the oracle's error type the planted runs are judged by, is a parameter of the trait and
/// not an associated type, so a test's private error type can name a shipped public enum's
/// plants: an impl's visibility is the least of the types in its header, so
/// `impl Plant<PrivateBroken> for PubPlant` is private to the test, where an associated
/// `type Error = PrivateBroken` in `impl Plant for PubPlant` leaks the private type (E0446,
/// blaze-comptime's, blaze-roots' and blaze-own's tests, 2026-11-20).
pub trait Plant<E>: Plants + Debug {
    /// The code as it is: the clean run's plant.
    const NONE: Self;
    /// Every plant, `NONE` excepted, each once.
    const ALL: &'static [Self];
    /// The variant's name, for messages.
    fn name(self) -> &'static str;
    /// The invariant this plant must trip. Asked of the plants of `ALL` alone.
    fn breaks(self) -> Expect<E>;
}


/// Declares a component's plants as an enum and implements [`Plant`] for it: the first variant
/// is `None` (the code as it is), and each other variant names, after `=>`, the pattern over the
/// oracle's error type it must trip. The enum derives `Clone, Copy, PartialEq, Eq, Debug`.
///
/// ```ignore
/// blaze_sim::plants! {
///     /// Bugs planted in the log.
///     pub(crate) enum LogPlant breaks LogBroken {
///         /// The log as it is.
///         None,
///         /// The log holds one record past its cap.
///         OffByOne => LogBroken::PastCap { .. },
///         /// A refused push drops the newest record.
///         ForgetsOnFull => LogBroken::Lost { .. },
///     }
/// }
/// ```
///
/// # An enum the crate already ships
///
/// A crate whose shipped code takes `impl Planted<XPlant>` declares `XPlant` there (it
/// implements `blaze_own::planted::Plants`, needing no blaze-sim), and its unit tests then write
/// `plants! { impl XPlant breaks XBroken { None, A => .., B => .. } }`: the same body without
/// `enum` and a visibility. Only `Plant` is implemented, over the shipped enum, so one enum
/// serves both and no second one is mapped onto it. The matches over it are exhaustive: a
/// variant shipped without a pattern here, or a pattern for one that is not shipped, fails to
/// compile. One limit, from the orphan rule: it works in the crate's own unit tests
/// (`src/tests.rs`), not in an integration test under `tests/`. The test's error type may be
/// private to the test module: the impl is `Plant<XBroken>`, private with it (see [`Plant`]).
///
/// ```ignore
/// // src/plant.rs, shipped:
/// #[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// pub enum LogPlant { None, OffByOne }
/// impl blaze_own::planted::Plants for LogPlant {}
///
/// // src/tests.rs:
/// blaze_sim::plants! {
///     impl LogPlant breaks LogBroken {
///         None,
///         OffByOne => LogBroken::PastCap { .. },
///     }
/// }
/// ```
#[macro_export]
macro_rules! plants {
    (
        $(#[$meta:meta])*
        impl $name:ident breaks $error:ty {
            $(#[$none_meta:meta])*
            None,
            $( $(#[$vmeta:meta])* $variant:ident => $pattern:pat ),+ $(,)?
        }
    ) => {
        impl $crate::Plant<$error> for $name {
            const NONE: $name = $name::None;
            const ALL: &'static [$name] = &[$($name::$variant),+];

            fn name(self) -> &'static str {
                match self {
                    $name::None => "None",
                    $( $name::$variant => stringify!($variant), )+
                }
            }

            fn breaks(self) -> $crate::Expect<$error> {
                match self {
                    $name::None => panic!(concat!(
                        "blaze-sim: `", stringify!($name), "::None` is the code as it is and breaks no invariant: a control asks `breaks` of the plants of `ALL` alone"
                    )),
                    $( $name::$variant => $crate::Expect::new(
                        stringify!($pattern),
                        |broken: &$error| matches!(broken, $pattern),
                    ), )+
                }
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident breaks $error:ty {
            $(#[$none_meta:meta])*
            None,
            $( $(#[$vmeta:meta])* $variant:ident => $pattern:pat ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        $vis enum $name {
            $(#[$none_meta])*
            None,
            $( $(#[$vmeta])* $variant, )+
        }

        impl $crate::Plants for $name {}

        // The impl form's `impl`, written out again: a rule of this macro calling another through
        // `$crate::plants!` is refused where the macro is used inside blaze-sim itself
        // (`macro_expanded_macro_exports_accessed_by_absolute_paths`: the crate-root tool
        // attribute makes every exported macro of the crate count as macro-expanded; annoyance
        // crate-root-tool-attribute-hides-macros), and a helper macro would be reached by the
        // same path. Keep the two copies equal.
        impl $crate::Plant<$error> for $name {
            const NONE: $name = $name::None;
            const ALL: &'static [$name] = &[$($name::$variant),+];

            fn name(self) -> &'static str {
                match self {
                    $name::None => "None",
                    $( $name::$variant => stringify!($variant), )+
                }
            }

            fn breaks(self) -> $crate::Expect<$error> {
                match self {
                    $name::None => panic!(concat!(
                        "blaze-sim: `", stringify!($name), "::None` is the code as it is and breaks no invariant: a control asks `breaks` of the plants of `ALL` alone"
                    )),
                    $( $name::$variant => $crate::Expect::new(
                        stringify!($pattern),
                        |broken: &$error| matches!(broken, $pattern),
                    ), )+
                }
            }
        }
    };
}

/// The controls of every plant of `P` (D165): the code as it is (`run(tape, P::NONE)`) passes
/// every case of `config`, and each plant of `P::ALL` is caught, unshrunk, in some case of
/// `config`, as `Broken::Oracle(e)` with `e` the invariant the plant names (`Plant::breaks`). A
/// planted case that trips another invariant is counted, the first one kept, and the run goes on;
/// a planted case that panics or fails the harness ends that plant's run as caught by the wrong
/// thing. Every plant is judged before the test fails, so one run names every plant refused.
///
/// Under `ObserveMode::Enforce` the clean run is also failed by a draw that never varied, which
/// blocks every plant (the clean run did not exercise the code).
///
/// Prints its observe block (observe.rs) on standard error: what the clean run never varied, then
/// the case each plant was caught at. On a refusal it prints one tagged line per plant refused
/// and unwinds with [`ControlsFailed`] (module docs). Panics, naming the enum, when `P::ALL` is
/// empty, lists `NONE` or a plant twice, has more than [`MOST_PLANTS`], or leaves a variant out.
#[track_caller]
pub fn controls<E: Display, P: Plant<E>>(config: Config, run: impl Fn(&Tape, P) -> Result<(), Broken<E>>) {
    let at = Location::caller();
    let mut observed = observed_lines();
    let judged = controls_judged(at, &config, &run, &mut observed);
    say_observed(at, &observed);
    if let Err(failed) = judged {
        fail(failed);
    }
}

/// `controls` from the place `at`, appending its observe lines to `out` and giving the refusals
/// back in place of printing and unwinding. Panics as `controls` does on a malformed enum.
pub(crate) fn controls_judged<E: Display, P: Plant<E>, A: Allocator>(
    at: &Location<'_>,
    config: &Config,
    run: &impl Fn(&Tape, P) -> Result<(), Broken<E>>,
    out: &mut blaze_own::String<A>,
) -> Result<(), ControlsFailed> {
    let plants = listed::<E, P>();
    let mut failed = ControlsFailed::new(plants.len());
    let clean = judge_clean(at, config, |tape| untyped(run(tape, P::NONE)), out);
    if let Err(clean) = clean {
        failed.block_all(plants.iter().map(|plant| plant.name()), &clean);
        return Err(failed);
    }
    for (index, &plant) in plants.iter().enumerate() {
        let expect = plant.breaks();
        let expected = format!("`{}`", expect.name());
        let judged = judge_planted(at, config, &expected, |tape| match run(tape, plant) {
            Ok(()) => Case::Passed,
            Err(Broken::Oracle(broken)) if expect.holds(&broken) => Case::Caught(broken.to_string()),
            Err(Broken::Oracle(broken)) => Case::Other(format!("`{broken}`")),
            Err(Broken::Harness(why)) => Case::Harness(why),
        });
        match judged {
            Ok(caught) => plant_lines_into(plant.name(), caught, out),
            Err(refused) => failed.refuse(index, plant.name(), refused.refusal, &refused.why),
        }
    }
    if failed.any() { Err(failed) } else { Ok(()) }
}

/// `P::ALL`, checked: not empty, at most [`MOST_PLANTS`], without `NONE`, each plant once, and
/// every variant but `NONE` in it (by the compiler's count of the enum's variants). Panics naming
/// the enum otherwise.
fn listed<E, P: Plant<E>>() -> &'static [P] {
    let all = P::ALL;
    let enum_name = core::any::type_name::<P>();
    assert!(
        !all.is_empty(),
        "blaze-sim: the plant enum `{enum_name}` lists no plant in `ALL`: a control of no plant proves nothing"
    );
    assert!(
        all.len() <= MOST_PLANTS,
        "blaze-sim: the plant enum `{enum_name}` lists {} plants, past the {MOST_PLANTS} a control holds",
        all.len()
    );
    for (at, plant) in all.iter().enumerate() {
        assert!(
            *plant != P::NONE,
            "blaze-sim: the plant enum `{enum_name}` lists its `NONE` (`{}`) in `ALL`: the code as it is is the clean run, not a plant",
            plant.name()
        );
        assert!(
            !all[..at].contains(plant),
            "blaze-sim: the plant enum `{enum_name}` lists `{}` twice in `ALL`",
            plant.name()
        );
    }
    let variants = core::mem::variant_count::<P>();
    assert!(
        variants == all.len() + 1,
        "blaze-sim: the plant enum `{enum_name}` has {variants} variants, but `ALL` lists {} plants and `NONE` is one more: a variant left out of `ALL` is a plant no control proves (declare the enum with `blaze_sim::plants!`, which lists every variant)",
        all.len()
    );
    all
}

/// A typed outcome as the untyped one the search runs on: the oracle's finding as its `Display`.
fn untyped<E: Display>(ran: Result<(), Broken<E>>) -> Result<(), Broke> {
    match ran {
        Ok(()) => Ok(()),
        Err(Broken::Oracle(broken)) => Err(Broke::Oracle(broken.to_string())),
        Err(Broken::Harness(why)) => Err(Broke::Harness(why)),
    }
}

/// How the clean run failed: its report, for the message.
struct CleanFailed {
    report: String,
}

/// The clean run over `config`, its observe lines appended to `out`: `Ok` when it passed (and,
/// under `ObserveMode::Enforce`, nothing it drew never varied).
fn judge_clean<A: Allocator>(
    at: &Location<'_>,
    config: &Config,
    clean: impl FnMut(&Tape) -> Result<(), Broke>,
    out: &mut blaze_own::String<A>,
) -> Result<(), CleanFailed> {
    let (verdict, searched) = search_observing(config.clone(), clean);
    searched.tally().lines_into(out);
    if !matches!(verdict, Verdict::Passed(_)) {
        return Err(CleanFailed { report: report_here(&verdict, at) });
    }
    let here = with_test_name(|test| place(at, test));
    match enforced(config.observe, &verdict, searched.tally(), &here) {
        Some(report) => Err(CleanFailed { report }),
        None => Ok(()),
    }
}

/// How one planted case ended, as a control reads it.
enum Case {
    Passed,
    /// The oracle tripped the invariant the plant names: its text.
    Caught(String),
    /// The oracle tripped another invariant: its text, quoted.
    Other(String),
    /// The harness's own fault: its text.
    Harness(String),
}

/// Why a plant was refused: the refusal and the words after `control <plant>: `.
struct Refused {
    refusal: Refusal,
    why: String,
}

/// The planted run over `config`'s cases, unshrunk, until a case is `Case::Caught` (the catch) or
/// fails another way; `expected` names the invariant for the messages.
fn judge_planted(
    at: &Location<'_>,
    config: &Config,
    expected: &str,
    planted: impl Fn(&Tape) -> Case,
) -> Result<Caught, Refused> {
    // Oracle findings of other invariants: how many, and the first one's text.
    let others = Cell::new(0u32);
    let first_other: Cell<Option<String>> = Cell::new(None);
    let (verdict, _) = search_observing(config.clone().shrink_runs(0), |tape| match planted(tape) {
        Case::Passed => Ok(()),
        Case::Caught(text) => Err(Broke::Oracle(text)),
        Case::Other(text) => {
            others.set(others.get() + 1);
            let first = first_other.take();
            first_other.set(Some(first.unwrap_or(text)));
            Ok(())
        }
        Case::Harness(text) => Err(Broke::Harness(text)),
    });
    match &verdict {
        Verdict::Failed(failed) if failed.failure.origin == Origin::Oracle => {
            Ok(Caught { case: failed.case, cases: failed.cases })
        }
        Verdict::Failed(failed) => Err(Refused {
            refusal: Refusal::OtherInvariant,
            why: format!("caught, but by {}, not {expected}", by_what(&failed.failure)),
        }),
        Verdict::Passed(passed) => match first_other.take() {
            Some(first) => Err(Refused {
                refusal: Refusal::OtherInvariant,
                why: format!(
                    "caught, but by {first}, not {expected} ({} of {} cases tripped another invariant, none {expected})",
                    others.get(),
                    passed.cases
                ),
            }),
            None => Err(Refused {
                refusal: Refusal::Missed,
                why: format!("the planted bug was not caught in {} cases", passed.cases),
            }),
        },
        Verdict::NotAFunction { .. }
        | Verdict::DrewNothing { .. }
        | Verdict::AllDiscarded { .. }
        | Verdict::Unreached { .. }
        | Verdict::Misfit { .. } => Err(Refused {
            refusal: Refusal::Unjudged,
            why: format!(
                "the planted run ended neither caught nor passed, so it judged nothing:\n{}",
                report_here(&verdict, at)
            ),
        }),
    }
}

/// What a planted case failed by when it was not the oracle, in words that follow "by".
fn by_what(failure: &Failure) -> String {
    match &failure.origin {
        Origin::Panic { at } => format!("a panic at {at}: `{}`", failure.text),
        Origin::Harness => format!("the harness: `{}`", failure.text),
        Origin::Error => format!("an `Err` the body returned: `{}`", failure.text),
        Origin::Overran => format!("the bound on a case's choices: `{}`", failure.text),
        Origin::Oracle => format!("`{}`", failure.text),
    }
}

/// Prints `failed`'s tagged lines on standard error and unwinds with it as the payload. The
/// lines are printed here because a payload that is not text is not printed by the panic hook;
/// `resume_unwind` runs no hook, so nothing prints twice.
#[expect(clippy::print_stderr, reason = "a failed control's lines go to the test's standard error")]
#[expect(
    blaze::std_use,
    reason = "a failed control's lines go to the test's own standard error, which the test harness captures through `eprint!` alone, and the typed payload leaves through `resume_unwind`"
)]
#[expect(
    blaze::effect_breadth,
    reason = "one `eprint!` and one `resume_unwind`: std's printing brings IO, blocking, locking and allocating, and the unwind panicking"
)]
fn fail(failed: ControlsFailed) -> ! {
    eprint!("{}", failed.message);
    std::panic::resume_unwind(Box::new(failed))
}

/// A planted-bug control by text. Deprecated (D165): an oracle's finding is matched here by
/// `says` being a substring of `Broke::Oracle`'s text, so rewording a message changes what the
/// control proves and two invariants whose texts overlap satisfy each other. Use [`controls`]
/// over a [`plants!`](crate::plants) enum and an oracle returning `Broken<E>`.
///
/// The check named by `says` catches `plant` in some case, and the code as it is (`clean`)
/// passes every case: the clean run, `run(tape, clean)` over `config`, has to pass; the planted
/// run goes through the cases of `config`, unshrunk, until one fails as `Broke::Oracle(why)` with
/// `why` containing `says`. A case the oracle fails with other words is counted, the first kept,
/// and the run goes on; a case that fails any other way (a panic, `Broke::Harness`, an overrun)
/// ends it as caught by the wrong thing. Prints and unwinds as [`controls`] does, the plant named
/// by its `Debug` form at index 0 of the payload.
#[track_caller]
pub fn control<P: Copy + Debug>(
    config: Config,
    clean: P,
    plant: P,
    says: &str,
    run: impl Fn(&Tape, P) -> Result<(), Broke>,
) {
    let at = Location::caller();
    let mut observed = observed_lines();
    let judged = control_judged(at, config, clean, plant, says, run, &mut observed);
    say_observed(at, &observed);
    if let Err(failed) = judged {
        fail(failed);
    }
}

/// `control` from the place `at`, appending the lines of its observe block (observe.rs) to `out`
/// in place of printing them. Prints and unwinds on a refusal, as `control` does.
pub(crate) fn control_observed<P: Copy + Debug, A: Allocator>(
    at: &Location<'_>,
    config: Config,
    clean: P,
    plant: P,
    says: &str,
    run: impl Fn(&Tape, P) -> Result<(), Broke>,
    out: &mut blaze_own::String<A>,
) {
    if let Err(failed) = control_judged(at, config, clean, plant, says, run, out) {
        fail(failed);
    }
}

/// `control_observed`, giving the refusal back in place of printing and unwinding.
#[expect(
    blaze::error_debug,
    reason = "a legacy plant is a value of the caller's own type, which the signature asks only to be `Debug`: its `Debug` form is the name the messages give it"
)]
pub(crate) fn control_judged<P: Copy + Debug, A: Allocator>(
    at: &Location<'_>,
    config: Config,
    clean: P,
    plant: P,
    says: &str,
    run: impl Fn(&Tape, P) -> Result<(), Broke>,
    out: &mut blaze_own::String<A>,
) -> Result<(), ControlsFailed> {
    let name = format!("{plant:?}");
    let mut failed = ControlsFailed::new(1);
    if let Err(clean_failed) = judge_clean(at, &config, |tape| run(tape, clean), out) {
        failed.block_all(core::iter::once(name.as_str()), &clean_failed);
        return Err(failed);
    }
    let expected = format!("`{says}`");
    let judged = judge_planted(at, &config, &expected, |tape| match run(tape, plant) {
        Ok(()) => Case::Passed,
        Err(Broke::Oracle(why)) if why.contains(says) => Case::Caught(why),
        Err(Broke::Oracle(why)) => Case::Other(format!("`{why}`")),
        Err(Broke::Harness(why)) => Case::Harness(why),
    });
    match judged {
        Ok(caught) => {
            plant_lines_into(&name, caught, out);
            Ok(())
        }
        Err(refused) => {
            failed.refuse(0, &name, refused.refusal, &refused.why);
            Err(failed)
        }
    }
}

// Declared after `plants!` so the tests name it by textual scope: `crate::plants!` is refused
// inside the crate ("macro-expanded `macro_export` macros ... cannot be referred to by absolute
// paths"), as kept.rs records for `sim_kept!`.
#[cfg(test)]
mod tests;
