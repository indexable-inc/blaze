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

use crate::control::Broke;
#[cfg(test)]
use crate::search::Plant;
use crate::search::{Config, Ended, Failed, Failure, Given, Next, Origin, SEED, SEEDS, Search, TAPE, Verdict};
use crate::tape::{Record, Tail, Tape};
use crate::text;

#[cfg(test)]
mod tests;

/// What a case's body returns.
pub trait Outcome {
    fn ended(self) -> Ended;
}

impl Outcome for () {
    fn ended(self) -> Ended {
        Ended::Passed
    }
}

impl Outcome for Result<(), Broke> {
    fn ended(self) -> Ended {
        match self {
            Ok(()) => Ended::Passed,
            Err(Broke::Oracle(text)) => Ended::Failed(Failure { origin: Origin::Oracle, text }),
            Err(Broke::Harness(text)) => Ended::Failed(Failure { origin: Origin::Harness, text }),
        }
    }
}

impl Outcome for Result<(), String> {
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
        blaze::process_static,
        reason = "the panic hook is process-wide and finds the search it serves through the panicking thread: how deep in searches the thread is, and the panic it kept"
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
fn install_hook() {
    #[expect(
        blaze::process_static,
        reason = "a panic hook is one per process: it is installed once, over the hook that was there before"
    )]
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let before = panic::take_hook();
        let hook = Box::new_in(
            move |info: &PanicHookInfo<'_>| {
                // At thread teardown the depth is gone, and so is any search on the thread.
                let inside = DEPTH.try_with(Cell::get).unwrap_or(0) > 0;
                if inside {
                    keep(info);
                } else {
                    before(info);
                }
            },
            Global,
        );
        panic::set_hook(hook);
    });
}

/// Keeps a panic's place and message for the search on this thread.
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

/// Raises this thread's depth while a search runs; lowers it when dropped, a panic out of the
/// search among the ways.
struct Inside;

impl Inside {
    fn enter() -> Inside {
        DEPTH.set(DEPTH.get() + 1);
        Inside
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        DEPTH.set(DEPTH.get() - 1);
    }
}

/// Runs the search over `body` and gives its verdict. Reads nothing from outside: `config` is all.
pub fn search<O: Outcome>(config: Config, body: impl FnMut(&Tape) -> O) -> Verdict {
    drive(Search::new(config), body)
}

/// `search` with a bug planted in the search, for a control.
#[cfg(test)]
pub(crate) fn search_planted<O: Outcome>(config: Config, plant: Plant, body: impl FnMut(&Tape) -> O) -> Verdict {
    drive(Search::planted(config, plant), body)
}

/// Runs `search`'s proposals on one tape and one record, kept across cases.
fn drive<O: Outcome>(mut search: Search, mut body: impl FnMut(&Tape) -> O) -> Verdict {
    install_hook();
    let _inside = Inside::enter();
    let tape = Tape::new(&[], Tail::Calm, search.most_choices());
    let mut record = Record::new();
    let most_runs = search.most_runs();
    for _ in 0..=most_runs {
        let proposal = match search.next() {
            Next::Done(verdict) => return verdict.clone(),
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
    panic!("blaze-sim: the search proposed more than its {most_runs} runs (its cases, shrink runs and final run)")
}

/// How a case ended: discarded, then overran, then a panic, then what the body returned.
fn decide<O: Outcome>(record: &Record, result: std::thread::Result<O>, kept: Option<Kept>) -> Ended {
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
                    let text = match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
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
    let thread = std::thread::current();
    let test = thread.name().filter(|name| *name != "main");
    match &verdict {
        Verdict::Passed(passed) => say_passed(&place(at, test), passed.cases, passed.choices),
        _ => panic!("{}", report(&verdict, at, test)),
    }
}

/// The three settings, read once, as a `Given`; a refused one panics with why.
#[expect(
    clippy::disallowed_methods,
    reason = "the driver is the entry point: the one place blaze-sim reads what the person gave"
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
        Err(error) => panic!("blaze-sim: {error}"),
    }
}

#[expect(clippy::print_stdout, reason = "a check's pass line goes to the test's own output")]
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
pub fn report(verdict: &Verdict, at: &Location<'_>, test: Option<&str>) -> String {
    let place = place(at, test);
    match verdict {
        Verdict::Failed(failed) => report_failed(failed, &place),
        Verdict::Passed(passed) => format!(
            "blaze-sim: `{place}` passed: {} cases ({} discarded), {} choices",
            passed.cases, passed.discarded, passed.choices
        ),
        Verdict::NotAFunction { seed, tape, first, again } => {
            let tape = text::print(tape);
            let found = match seed {
                Some(seed) => format!(" (the case as found: {SEED}={seed})"),
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
        Verdict::DrewNothing { cases } => format!(
            "blaze-sim: `{place}`: its {cases} cases drew no choice, so it is no property test: draw what varies from the tape"
        ),
        Verdict::AllDiscarded { cases } => format!(
            "blaze-sim: `{place}`: all {cases} cases were discarded, so nothing was tested: discard less, or draw what the case assumes"
        ),
        Verdict::Unreached { cases, missing } => {
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
        Verdict::Misfit { clamped, unused, ended: how } => format!(
            "blaze-sim: `{place}`: the given tape does not fit the case: {clamped} values lowered to fit their draws, {unused} never drawn; the case {}\n\
             a tape replays only the case it was recorded from: give the one the failure printed",
            ended(how)
        ),
    }
}

/// The report of a failure: what failed, the least tape, its choices that are not calm, the
/// notes, and the line that replays it.
fn report_failed(failed: &Failed, place: &str) -> String {
    let mut values = Vec::new_in(Global);
    failed.least.values_into(&mut values);
    let tape = text::print(&values);
    let mut out = match failed.seed {
        Some(seed) => format!(
            "blaze-sim: `{place}` failed at seed {seed} (case {} of {}): {}\n",
            failed.case, failed.cases, failed.failure.text
        ),
        None => format!("blaze-sim: `{place}` failed on the given tape: {}\n", failed.failure.text),
    };
    match &failed.failure.origin {
        Origin::Panic { at } => out.push_str(&format!("  at {at}\n")),
        other => out.push_str(&format!("  from {}\n", origin(other))),
    }
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
    out.push_str("not calm:");
    let mut shown = 0;
    for choice in failed.least.choices.iter().filter(|choice| choice.value != 0) {
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
    if !failed.least.notes.is_empty() {
        out.push_str("notes:\n");
        for note in &failed.least.notes {
            out.push_str(&format!("  [{}] {}\n", note.at, note.text));
        }
    }
    out.push_str(&format!("replay: {TAPE}='{tape}'"));
    if let Some(seed) = failed.seed {
        out.push_str(&format!("   (the case as found: {SEED}={seed})"));
    }
    out
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

/// How a case ended, in words that follow "it".
fn ended(ended: &Ended) -> String {
    match ended {
        Ended::Passed => "passed".to_owned(),
        Ended::Discarded => "was discarded".to_owned(),
        Ended::Failed(failure) => format!("failed with `{}` ({})", failure.text, origin(&failure.origin)),
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
