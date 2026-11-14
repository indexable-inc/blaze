//! The driver: runs a [`Search`] over a body, catches what the body panics with, and says the
//! verdict (docs/v2/plan/design/sim.md, "The driver").
//!
//! [`search`] reads nothing from outside: its config is all. [`check`] is the one place
//! blaze-sim reads what the person gave (`BLAZE_SIM_SEED`, `BLAZE_SIM_SEEDS`,
//! `BLAZE_SIM_TAPE`), prints a pass and panics with [`report`] on anything else.
//!
//! A panic inside a body is the case's failure, not noise: a panic hook, installed once for the
//! process, keeps the place and message of a panic on a thread that is inside a `search` and
//! prints nothing; on any other thread, or outside a search, it calls the hook that was there
//! before.

use std::alloc::Global;
use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe, Location, PanicHookInfo};
use std::sync::Once;

#[cfg(test)]
use crate::search::Plant;
use crate::search::{
    Broke, Config, Ended, Failed, Failure, Given, GivenError, Next, Origin, SEED, SEEDS, Search,
    TAPE, Verdict,
};
use crate::tape::{Record, Tail, Tape};
use crate::text;

#[cfg(test)]
mod tests;

/// What a case's body returns.
pub trait Outcome {
    fn ended(self) -> Ended;
}

impl Outcome for () {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn ended(self) -> Ended {
        Ended::Passed
    }
}

impl Outcome for Result<(), Broke> {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn ended(self) -> Ended {
        match self {
            Ok(()) => Ended::Passed,
            Err(Broke::Oracle(text)) => Ended::Failed(Failure { origin: Origin::Oracle, text }),
            Err(Broke::Harness(text)) => Ended::Failed(Failure { origin: Origin::Harness, text }),
        }
    }
}

impl Outcome for Result<(), String> {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn ended(self) -> Ended {
        match self {
            Ok(()) => Ended::Passed,
            Err(text) => Ended::Failed(Failure { origin: Origin::Error, text }),
        }
    }
}

/// A panic the hook kept: its `file:line:column` and its message.
struct Kept {
    at: String,
    message: String,
}

/// The thread-locals the panic hook and `search` share.
mod local {
    #![expect(
        blaze::global_state,
        blaze::std_use,
        reason = "the panic hook is process-wide and finds the search it serves through the panicking thread: how deep in searches the thread is, and the panic it kept"
    )]
    #![expect(
        blaze::process_static,
        reason = "the kept panic is one per thread, and the search that ran the case takes it before its next run: it holds at most one panic's message and place"
    )]

    use std::cell::Cell;

    use super::Kept;

    thread_local! {
        /// How many `search`es this thread is inside.
        pub(super) static DEPTH: Cell<u32> = const { Cell::new(0) };
        /// The last panic the hook kept on this thread.
        pub(super) static KEPT: Cell<Option<Kept>> = const { Cell::new(None) };
    }
}

use local::{DEPTH, KEPT};

/// Installs the hook once for the process.
#[expect(
    blaze::std_use,
    reason = "a panic hook is one per process: the process's one `Once` says whether it is installed"
)]
#[expect(
    blaze::effect_breadth,
    reason = "std's `Once::call_once` alone brings IO, blocking, locking, allocating and panicking: the function that installs the process's one hook once has all five, however it is split"
)]
fn install_hook() {
    #[expect(
        blaze::global_state,
        blaze::std_use,
        reason = "a panic hook is one per process: it is installed once, over the hook that was there before"
    )]
    static INSTALL: Once = Once::new();
    INSTALL.call_once(replace_hook);
}

/// Puts blaze-sim's hook over the one the process had, which it keeps to hand panics on to.
#[expect(
    blaze::effect_breadth,
    reason = "std's `take_hook` alone brings IO, blocking, locking, allocating and panicking: a function that takes the hook there was has all five, however it is split"
)]
fn replace_hook() {
    let before = panic::take_hook();
    let hook = Box::new_in(move |info: &PanicHookInfo<'_>| on_panic(&before, info), Global);
    panic::set_hook(hook);
}

/// What the hook does with a panic: keeps it when this thread is inside a search, else hands it
/// to `before`, the hook that was there before.
#[expect(
    blaze::std_use,
    reason = "the hook is process-wide and finds the search it serves through the panicking thread's depth"
)]
#[expect(
    blaze::dropped_error,
    reason = "the hook can run while a thread is torn down, when its thread-locals are gone and so is any search on it: no depth to read is depth 0"
)]
fn on_panic(before: &(dyn Fn(&PanicHookInfo<'_>) + Sync + Send), info: &PanicHookInfo<'_>) {
    let inside = DEPTH.try_with(Cell::get).unwrap_or(0) > 0;
    if inside {
        keep(info);
    } else {
        before(info);
    }
}

/// Keeps a panic's place and message for the search on this thread.
#[expect(
    blaze::std_use,
    reason = "the hook hands a panic to the search on its thread through that thread's kept panic"
)]
fn keep(info: &PanicHookInfo<'_>) {
    let at = match info.location() {
        Some(at) => format!("{}:{}:{}", at.file(), at.line(), at.column()),
        None => "a place the panic did not give".to_owned(),
    };
    let message = match info.payload_as_str() {
        Some(text) => text.to_owned(),
        None => "a panic whose payload is not text".to_owned(),
    };
    // Teardown has no search to keep it for.
    let _gone = KEPT.try_with(|kept| kept.set(Some(Kept { at, message })));
}

/// Raises this thread's depth while a search runs; puts it back when dropped, a panic out of the
/// search among the ways.
struct Inside {
    /// The thread's depth outside this search, which the drop puts back.
    outer: u32,
}

impl Inside {
    #[expect(
        blaze::std_use,
        reason = "the depth is the thread's, so the process-wide hook can tell a panic inside a search from one outside"
    )]
    fn enter() -> Inside {
        let outer = DEPTH.get();
        DEPTH.set(outer + 1);
        Inside { outer }
    }
}

impl Drop for Inside {
    #[blaze::noalloc]
    #[blaze::nonblocking]
    #[expect(
        blaze::std_use,
        reason = "the depth is the thread's, so the process-wide hook can tell a panic inside a search from one outside"
    )]
    fn drop(&mut self) {
        DEPTH.set(self.outer);
    }
}

/// Runs the search over `body` and gives its verdict. Reads nothing from outside: `config` is all.
pub fn search<O: Outcome>(config: Config, body: impl FnMut(&Tape) -> O) -> Verdict {
    drive(Search::new(config), body)
}

/// `search` with a bug planted in the search, for a control.
#[cfg(test)]
pub(crate) fn search_planted<O: Outcome>(
    config: Config,
    plant: Plant,
    body: impl FnMut(&Tape) -> O,
) -> Verdict {
    drive(Search::planted(config, plant), body)
}

/// Runs `search`'s proposals on one tape and one record, kept across cases.
#[expect(
    blaze::std_use,
    reason = "the hook hands a body's panic to its search through this thread's kept panic: emptied before each run, taken after it"
)]
fn drive<O: Outcome>(mut search: Search, mut body: impl FnMut(&Tape) -> O) -> Verdict {
    install_hook();
    let _inside = Inside::enter();
    let tape = Tape::new(&[], Tail::Calm, search.most_choices());
    let mut record = Record::new();
    let most_runs = search.most_runs();
    for _ in 0..=most_runs {
        let proposal = match search.next() {
            Next::Done(
                verdict @ (Verdict::Passed(_)
                | Verdict::Failed(_)
                | Verdict::NotAFunction { .. }
                | Verdict::DrewNothing { .. }
                | Verdict::AllDiscarded { .. }
                | Verdict::Unreached { .. }
                | Verdict::Misfit { .. }),
            ) => return verdict.clone(),
            Next::Run(proposal) => proposal,
        };
        tape.reset(proposal.prefix, proposal.tail);
        KEPT.take();
        let result = panic::catch_unwind(AssertUnwindSafe(|| body(&tape)));
        let kept = KEPT.take();
        tape.finish(&mut record);
        let ended = decide(&record, result, kept);
        search.ran(&record, ended);
    }
    panic!(
        "blaze-sim: the search proposed more than its {most_runs} runs (its cases, shrink runs and final run)"
    )
}

/// How a case ended: discarded, then overran, then a panic, then what the body returned.
fn decide<O: Outcome>(
    record: &Record,
    result: std::thread::Result<O>,
    kept: Option<Kept>,
) -> Ended {
    if record.discarded {
        return Ended::Discarded;
    }
    if record.overran {
        let text = match kept {
            Some(kept) => kept.message,
            None => "the case drew more than its bound of choices".to_owned(),
        };
        return Ended::Failed(Failure { origin: Origin::Overran, text });
    }
    match result {
        Ok(outcome) => outcome.ended(),
        Err(payload) => {
            let (at, text) = match kept {
                Some(kept) => (kept.at, kept.message),
                // Another hook took over after blaze-sim's was installed: the payload is all there is.
                None => {
                    let text =
                        match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
                            (Some(text), _) => (*text).to_owned(),
                            (None, Some(text)) => text.clone(),
                            (None, None) => "a panic whose payload is not text".to_owned(),
                        };
                    ("a place unknown: another panic hook replaced blaze-sim's".to_owned(), text)
                }
            };
            Ended::Failed(Failure { origin: Origin::Panic { at }, text })
        }
    }
}

/// `search` with what the person gave in the environment, then: prints one line for a pass,
/// panics with `report` for anything else.
/// What the person gave in the environment replaces what `config` was given; when they gave
/// nothing, `config`'s own stands.
#[track_caller]
pub fn check<O: Outcome>(config: Config, body: impl FnMut(&Tape) -> O) {
    let given = given_by_the_person();
    let given = if given == Given::NOTHING { config.given.clone() } else { given };
    check_given(config, given, body);
}

/// `check` with `given` in place of what the environment says: runs the search with
/// `config.given(given)`, prints one line for a pass, panics with `report` for anything else.
/// Reads nothing from outside.
#[track_caller]
pub fn check_given<O: Outcome>(config: Config, given: Given, body: impl FnMut(&Tape) -> O) {
    let at = Location::caller();
    let verdict = search(config.given(given), body);
    match &verdict {
        Verdict::Passed(passed) => {
            say_passed(&with_test_name(|test| place(at, test)), passed.cases, passed.choices);
        }
        Verdict::Failed(_)
        | Verdict::NotAFunction { .. }
        | Verdict::DrewNothing { .. }
        | Verdict::AllDiscarded { .. }
        | Verdict::Unreached { .. }
        | Verdict::Misfit { .. } => panic!("{}", report_here(&verdict, at)),
    }
}

/// The name Rust's runtime gives the main thread: a thread's name, but no test's.
const MAIN_THREAD: &str = "main";

/// Runs `then` with the name of the test on this thread: the thread's name, unless the thread
/// has none or is the main thread.
#[expect(
    blaze::std_use,
    reason = "the test harness names the thread it runs a test on after the test: the thread's name is the one place the test's name is"
)]
fn with_test_name<R>(then: impl FnOnce(Option<&str>) -> R) -> R {
    let thread = std::thread::current();
    then(thread.name().filter(|name| *name != MAIN_THREAD))
}

/// `report` of `verdict`, from the check at `at`, named after the test on this thread.
pub(crate) fn report_here(verdict: &Verdict, at: &Location<'_>) -> String {
    with_test_name(|test| report(verdict, at, test))
}

/// The three settings, read once, as a `Given`; a refused one panics with why.
#[expect(
    clippy::disallowed_methods,
    reason = "the driver is the entry point: the one place blaze-sim reads what the person gave"
)]
#[expect(
    blaze::std_use,
    reason = "the driver is the entry point: the one place blaze-sim reads what the person gave, from the test process's environment"
)]
#[expect(
    blaze::error_debug,
    reason = "a setting that is not Unicode has no `Display`: `Debug`'s escaped form is the one that shows its bytes as they are"
)]
fn given_by_the_person() -> Given {
    let read = |setting: &'static str| match std::env::var(setting) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(value)) => {
            panic!("blaze-sim: `{setting}` is not Unicode ({value:?}): give it as decimal text")
        }
    };
    let (seed, seeds, tape) = (read(SEED), read(SEEDS), read(TAPE));
    match Given::parse(seed.as_deref(), seeds.as_deref(), tape.as_deref()) {
        Ok(given) => given,
        Err(
            error @ (GivenError::NotANumber { .. }
            | GivenError::Tape { .. }
            | GivenError::TapeWith { .. }
            | GivenError::ZeroSeeds
            | GivenError::SeedWithSeeds),
        ) => panic!("blaze-sim: {error}"),
    }
}

/// Prints a check's pass line.
#[expect(clippy::print_stdout, reason = "a check's pass line goes to the test's own output")]
#[expect(
    blaze::std_use,
    reason = "a check's pass line goes to the test's own output, which the test harness captures through `println!` alone"
)]
#[expect(
    blaze::effect_breadth,
    reason = "one `println!`: std's `_print` brings IO, blocking, locking and allocating and its arguments' formatting panicking, so any function that prints a line has all five"
)]
fn say_passed(place: &str, cases: u32, choices: u64) {
    println!("blaze-sim: {place}: {cases} cases, {choices} choices");
}

/// The test's name, or else `file:line` of the check.
fn place(at: &Location<'_>, test: Option<&str>) -> String {
    match test {
        Some(test) => test.to_owned(),
        None => format!("{}:{}", at.file(), at.line()),
    }
}

/// What a verdict says to a person. `test` is the test's name when known.
#[blaze::pure]
#[blaze::nonblocking]
pub fn report(verdict: &Verdict, at: &Location<'_>, test: Option<&str>) -> String {
    let place = place(at, test);
    match verdict {
        Verdict::Failed(failed) => report_failed(failed, &place),
        Verdict::Passed(passed) => {
            format!(
                "blaze-sim: `{place}` passed: {} cases ({} discarded), {} choices",
                passed.cases, passed.discarded, passed.choices
            )
        }
        Verdict::NotAFunction {
            seed,
            tape,
            first,
            again: again @ (Ended::Passed | Ended::Failed(_) | Ended::Discarded),
        } => report_not_a_function(&place, *seed, tape, first, again),
        Verdict::DrewNothing { cases } => format!(
            "blaze-sim: `{place}`: its {cases} cases drew no choice, so it is no property test: draw what varies from the tape"
        ),
        Verdict::AllDiscarded { cases } => format!(
            "blaze-sim: `{place}`: all {cases} cases were discarded, so nothing was tested: discard less, or draw what the case assumes"
        ),
        Verdict::Unreached { cases, missing } => report_unreached(&place, *cases, missing),
        Verdict::Misfit {
            clamped,
            unused,
            ended: how @ (Ended::Passed | Ended::Failed(_) | Ended::Discarded),
        } => format!(
            "blaze-sim: `{place}`: the given tape does not fit the case: {clamped} values lowered to fit their draws, {unused} never drawn; the case {}\n\
             a tape replays only the case it was recorded from: give the one the failure printed",
            ended(how)
        ),
    }
}

/// The report of a body that is not a function of its tape: its least tape failed with `first`,
/// and run once more it ended `again`.
fn report_not_a_function(
    place: &str,
    seed: Option<u64>,
    tape: &[u64],
    first: &Failure,
    again: &Ended,
) -> String {
    let tape = text::print(tape);
    let found = match seed {
        Some(seed) => format!(" {}", as_found(seed)),
        None => String::new(),
    };
    format!(
        "blaze-sim: `{place}` is not a function of its tape: its least tape failed with `{}` ({}), and run once more it {}\n\
         it reads something outside its tape (a counter, a clock, a global); the tape: {TAPE}='{tape}'{found}",
        first.text,
        origin(&first.origin),
        ended(again),
    )
}

/// The report of a search whose passing cases never reached the points `missing`.
fn report_unreached(place: &str, cases: u32, missing: &[&'static str]) -> String {
    let mut points = String::new();
    for (index, point) in missing.iter().enumerate() {
        if index > 0 {
            points.push_str(", ");
        }
        points.push('`');
        points.push_str(point);
        points.push('`');
    }
    format!("blaze-sim: `{place}`: in {cases} cases no passing case reached {points}")
}

/// The report of a failure: what failed, the least tape, its choices that are not calm, the
/// notes, and the line that replays it.
fn report_failed(failed: &Failed, place: &str) -> String {
    let mut values = Vec::new_in(Global);
    failed.least.values_into(&mut values);
    let tape = text::print(&values);
    let mut out = failed_what(failed, place);
    failed_tape(&mut out, failed, &tape);
    failed_not_calm(&mut out, &failed.least);
    failed_notes(&mut out, &failed.least);
    failed_replay(&mut out, &tape, failed.seed);
    out
}

/// The report's first two lines: what failed, at which seed or on the given tape, and where the
/// failure came from.
fn failed_what(failed: &Failed, place: &str) -> String {
    let mut out = match failed.seed {
        Some(seed) => format!(
            "blaze-sim: `{place}` failed at seed {seed} (case {} of {}): {}\n",
            failed.case, failed.cases, failed.failure.text
        ),
        None => format!("blaze-sim: `{place}` failed on the given tape: {}\n", failed.failure.text),
    };
    match &failed.failure.origin {
        Origin::Panic { at } => out.push_str(&format!("  at {at}\n")),
        other @ (Origin::Oracle | Origin::Harness | Origin::Error | Origin::Overran) => {
            out.push_str(&format!("  from {}\n", origin(other)));
        }
    }
    out
}

/// The report's tape lines: how many choices the least tape has, how it was found, and the tape
/// as text.
fn failed_tape(out: &mut String, failed: &Failed, tape: &str) {
    match failed.seed {
        Some(_) => out.push_str(&format!(
            "least tape: {} choices, from {}, in {} shrink runs\n",
            failed.least.choices.len(),
            failed.first_len,
            failed.shrink_runs
        )),
        None => out.push_str(&format!("tape: {} choices, as given\n", failed.least.choices.len())),
    }
    match tape.is_empty() {
        true => out.push_str("  (empty: every choice calm)\n"),
        false => out.push_str(&format!("  {tape}\n")),
    }
}

/// The report's line of the least tape's choices that are not calm, at most `NOT_CALM_SHOWN`.
fn failed_not_calm(out: &mut String, least: &Record) {
    out.push_str("not calm:");
    let mut shown = 0;
    for choice in least.choices.iter().filter(|choice| choice.value != 0) {
        if shown == NOT_CALM_SHOWN {
            out.push_str(" | ..");
            break;
        }
        out.push_str(match shown {
            0 => " ",
            _ => " | ",
        });
        out.push_str(&format!("{} = {} of 0..={}", choice.label, choice.value, choice.most));
        shown += 1;
    }
    if shown == 0 {
        out.push_str(" none");
    }
    out.push('\n');
}

/// The report's notes of the least tape, when it has any.
fn failed_notes(out: &mut String, least: &Record) {
    if !least.notes.is_empty() {
        out.push_str("notes:\n");
        for note in &least.notes {
            out.push_str(&format!("  [{}] {}\n", note.at, note.text));
        }
    }
}

/// The report's last line: the setting that replays the least tape, and the seed it was found at.
fn failed_replay(out: &mut String, tape: &str, seed: Option<u64>) {
    out.push_str(&format!("replay: {TAPE}='{tape}'"));
    if let Some(seed) = seed {
        out.push_str("   ");
        out.push_str(&as_found(seed));
    }
}

/// The words that give the seed a failing case was found at, as the setting that finds it again.
fn as_found(seed: u64) -> String {
    format!("(the case as found: {SEED}={seed})")
}

/// The most choices that are not calm a report lists.
const NOT_CALM_SHOWN: usize = 32;

/// Where a failure came from, in words.
fn origin(origin: &Origin) -> String {
    match origin {
        Origin::Panic { at } => format!("a panic at {at}"),
        Origin::Oracle => "the oracle (`Broke::Oracle`)".to_owned(),
        Origin::Harness => "the harness itself (`Broke::Harness`)".to_owned(),
        Origin::Error => "an `Err` the body returned".to_owned(),
        Origin::Overran => "the bound on a case's choices".to_owned(),
    }
}

/// The word that says a case passed, following "it".
const SAID_PASSED: &str = "passed";

/// How a case ended, in words that follow "it".
fn ended(ended: &Ended) -> String {
    match ended {
        Ended::Passed => SAID_PASSED.to_owned(),
        Ended::Discarded => "was discarded".to_owned(),
        Ended::Failed(failure) => {
            format!("failed with `{}` ({})", failure.text, origin(&failure.origin))
        }
    }
}

/// How many searches this thread is inside.
#[cfg(test)]
pub(crate) fn depth() -> u32 {
    DEPTH.get()
}

/// The panic the hook kept on this thread, taken.
#[cfg(test)]
pub(crate) fn take_kept() -> Option<(String, String)> {
    KEPT.take().map(|kept| (kept.at, kept.message))
}
