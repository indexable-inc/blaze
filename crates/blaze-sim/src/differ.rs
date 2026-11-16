//! Differential tests (docs/v2/plan/design/test.md, "B. Differential tests", (i)): two or more
//! implementations of one thing run on the same input, drawn from one tape, and their answers
//! compared. It is the library form of rule 78 ("a choice between strategies is a type parameter
//! at one seam, each instantiation tested equal") and of the hand-written oracles that compared an
//! old and a new implementation on every input.
//!
//! - [`differ`]: two functions over one drawn input.
//! - [`differ_all`]: N functions, usually N instantiations of one generic function over its
//!   strategy type parameter ([`Side::of`]), each compared to the first.
//! - [`compare`]: the same run, reading nothing from outside and panicking on nothing, giving the
//!   search's [`Verdict`] and the run's [`Summary`].
//!
//! The judge is code, never a list in prose ([`Differ`]): `normalize` maps an answer to the form
//! compared, and `approved` lists the differences that are allowed, each with its reason and the
//! function that recognises it. An approved difference that matched none of a run's declared
//! cases is reported: it no longer describes anything, and is deleted or its inputs drawn.
//!
//! A side that panics gave an answer too, [`Answer::Panicked`]: a panic on one side and not the
//! other is a difference, shrunk and printed like any other, never the end of the test. Both
//! sides panicking on one input is agreement (both refused it), counted apart in the summary.
//!
//! A difference is a failure of the case from the oracle (`Broke::Oracle`), so the search shrinks
//! its tape to the least one that still differs and prints, through the check's own report, the
//! least tape, the input it draws and every compared side's answer:
//!
//! ```text
//! `new` differs from `old`
//!   input  100
//!   old    200
//!   new    201
//! ```

use core::any::Any;
use core::fmt::{self, Debug, Display};
use core::panic::{AssertUnwindSafe, Location};
// The one way to keep a side's panic as its answer: `core` has no unwinding catch.
use std::panic::catch_unwind;

use crate::check::{check, search};
use crate::search::{Broke, Config, Verdict};
use crate::tape::Tape;

#[cfg(test)]
mod tests;

/// The most sides one run compares (rule 18: a table has a cap).
pub const MOST_SIDES: usize = 16;

/// The most approved differences one judge lists (rule 18: a table has a cap).
pub const MOST_APPROVED: usize = 32;

/// One implementation compared: its name, as the difference prints it, and the function.
pub struct Side<'f, I, O> {
    pub name: &'static str,
    pub run: &'f dyn Fn(&I) -> O,
}

impl<'f, I, O> Side<'f, I, O> {
    /// The side `run`, printed as `name`.
    pub fn new(name: &'static str, run: &'f dyn Fn(&I) -> O) -> Side<'f, I, O> {
        Side { name, run }
    }

    /// The side `run`, an instantiation of a generic function at the strategy `S`, printed by
    /// `S`'s name (its path's last segment, with its own type arguments):
    /// `Side::of::<Linear>(&total::<Linear>)`.
    pub fn of<S: ?Sized>(run: &'f dyn Fn(&I) -> O) -> Side<'f, I, O> {
        Side { name: short_name(core::any::type_name::<S>()), run }
    }
}

/// A type's name without its path: `a::b::Linear<c::D>` is `Linear<c::D>`.
fn short_name(name: &'static str) -> &'static str {
    let head_end = match name.find('<') {
        Some(at) => at,
        None => name.len(),
    };
    let start = match name.get(..head_end).and_then(|head| head.rfind("::")) {
        Some(at) => at + 2,
        None => 0,
    };
    name.get(start..).unwrap_or(name)
}

/// What one side gave for one input, after the judge's `normalize`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer<O> {
    /// It returned this.
    Returned(O),
    /// It panicked with this message (the payload's text, or words saying it had none).
    Panicked { message: String },
}

impl<O: Debug> Display for Answer<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Answer::Returned(value) => write!(f, "{value:?}"),
            Answer::Panicked { message } => write!(f, "panicked: `{message}`"),
        }
    }
}

/// Two answers for one input, as an approval sees them: the first side's and another's.
pub struct Pair<'c, I, O> {
    pub input: &'c I,
    pub first: &'static str,
    pub first_answer: &'c Answer<O>,
    pub other: &'static str,
    pub other_answer: &'c Answer<O>,
}

/// A difference that is allowed: why, and the function that recognises it.
pub struct Approved<I, O> {
    /// Why it is allowed, with where it was decided: `"len 0 is an error now, #2210"`.
    pub reason: &'static str,
    /// Whether this pair's difference is the one allowed.
    pub applies: fn(&Pair<'_, I, O>) -> bool,
}

impl<I, O> Approved<I, O> {
    pub const fn new(reason: &'static str, applies: fn(&Pair<'_, I, O>) -> bool) -> Approved<I, O> {
        Approved { reason, applies }
    }
}

/// The judge, as code: how an answer is normalised before it is compared, and which differences
/// are allowed (the first entry that applies is the one counted).
pub struct Differ<'j, I, O> {
    /// The form an answer is compared and printed in.
    pub normalize: fn(O) -> O,
    /// The allowed differences, at most `MOST_APPROVED`.
    pub approved: &'j [Approved<I, O>],
}

impl<'j, I, O> Differ<'j, I, O> {
    /// Answers compared as they are; no difference allowed.
    pub fn plain() -> Differ<'j, I, O> {
        Differ { normalize: as_it_is::<O>, approved: &[] }
    }

    /// This judge, normalising each answer by `normalize`.
    pub fn normalize(self, normalize: fn(O) -> O) -> Differ<'j, I, O> {
        Differ { normalize, approved: self.approved }
    }

    /// This judge, allowing the differences `approved`.
    pub fn approved(self, approved: &'j [Approved<I, O>]) -> Differ<'j, I, O> {
        Differ { normalize: self.normalize, approved }
    }
}

/// The identity: `Differ::plain`'s normalisation.
fn as_it_is<O>(answer: O) -> O {
    answer
}

/// A comparison refused before anything ran, by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A comparison needs two sides at least.
    TooFewSides { given: usize },
    /// More sides than `MOST_SIDES` (rule 18).
    TooManySides { given: usize, most: usize },
    /// More approved differences than `MOST_APPROVED` (rule 18).
    TooManyApproved { given: usize, most: usize },
}

impl Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::TooFewSides { given } => write!(
                f,
                "a differential test compares two sides at least, and was given {given} (rule 78)"
            ),
            Refused::TooManySides { given, most } => write!(
                f,
                "a differential test compares at most {most} sides, and was given {given} (rule 18: a table has a cap)"
            ),
            Refused::TooManyApproved { given, most } => write!(
                f,
                "a judge lists at most {most} approved differences, and was given {given} (rule 18: a table has a cap)"
            ),
        }
    }
}

impl core::error::Error for Refused {}

/// What every case of a run came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tally {
    /// Cases entered, discarded ones among them.
    pub entered: u32,
    /// Cases on which every side agreed with the first.
    pub equal: u32,
    /// Cases with a difference, every one of them approved.
    pub approved: u32,
    /// Cases with a difference no entry approved (on a pass: none).
    pub differed: u32,
    /// Pairs that agreed by both panicking.
    pub panicked_alike: u32,
    /// Each approved difference's reason and how many pairs it allowed, in the judge's order.
    fired: [(&'static str, u32); MOST_APPROVED],
    /// How many entries of `fired` are the judge's.
    kinds: usize,
}

impl Tally {
    fn new<I, O>(approved: &[Approved<I, O>]) -> Tally {
        let mut fired = [("", 0); MOST_APPROVED];
        for (slot, entry) in fired.iter_mut().zip(approved) {
            slot.0 = entry.reason;
        }
        Tally {
            entered: 0,
            equal: 0,
            approved: 0,
            differed: 0,
            panicked_alike: 0,
            fired,
            kinds: approved.len().min(MOST_APPROVED),
        }
    }

    /// Each approved difference's reason and how many pairs it allowed, in the judge's order.
    pub fn fired(&self) -> &[(&'static str, u32)] {
        self.fired.get(..self.kinds).unwrap_or(&[])
    }

    /// Cases the draw discarded: entered and none of equal, approved or differing.
    pub fn discarded(&self) -> u32 {
        self.entered - self.equal - self.approved - self.differed
    }
}

/// What a passing run came to, and whether its approved differences were judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub tally: Tally,
    /// The cases the config declared.
    pub declared: u32,
    /// Whether the run passed and ran exactly its declared cases, so that an approved difference
    /// matching none of them is stale. A given tape, a given seed or another count of seeds is
    /// not the declared run: what it did not reach says nothing of the approvals.
    pub judged: bool,
}

impl Summary {
    fn new(tally: Tally, declared: u32, passed: bool) -> Summary {
        let judged = passed && tally.entered == declared;
        Summary { tally, declared, judged }
    }

    /// The approved differences that matched none of the declared cases, in the judge's order;
    /// none when the run was not judged.
    pub fn stale(&self) -> impl Iterator<Item = &'static str> + '_ {
        let judged = self.judged;
        self.tally
            .fired()
            .iter()
            .filter(move |(_, count)| judged && *count == 0)
            .map(|(reason, _)| *reason)
    }
}

/// `256 cases: 249 equal, 7 approved ("len 0 is an error now, #2210" 7), 0 discarded`.
impl Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tally = &self.tally;
        write!(f, "{} cases: {} equal, {} approved", tally.entered, tally.equal, tally.approved)?;
        for (index, (reason, count)) in tally.fired().iter().enumerate() {
            let open = if index == 0 { " (" } else { ", " };
            write!(f, "{open}\"{reason}\" {count}")?;
        }
        if tally.kinds > 0 {
            f.write_str(")")?;
        }
        write!(f, ", {} discarded", tally.discarded())?;
        if tally.panicked_alike > 0 {
            write!(f, "; {} pairs agreed by both panicking", tally.panicked_alike)?;
        }
        if !self.judged {
            write!(
                f,
                "; approvals not judged: {} cases ran, not the {} declared",
                tally.entered, self.declared
            )?;
        }
        Ok(())
    }
}

/// A run of [`compare`]: the search's verdict and what the cases came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compared {
    pub verdict: Verdict,
    pub summary: Summary,
}

/// Compares `sides` on the inputs `draw` makes, over the cases of `config`, each side to the
/// first. Reads nothing from outside and panics on nothing a side or the judge does not: the
/// verdict says whether a difference no approval allows was found (a failure from the oracle,
/// shrunk to its least tape, its text the difference print), and the summary what the cases
/// came to.
pub fn compare<I: Debug, O: Debug + PartialEq>(
    config: Config,
    mut draw: impl FnMut(&Tape) -> I,
    sides: &[Side<'_, I, O>],
    judge: &Differ<'_, I, O>,
) -> Result<Compared, Refused> {
    let (first, others) = shape(sides, judge)?;
    let declared = config.cases;
    let mut tally = Tally::new(judge.approved);
    let verdict =
        search(config, |tape| one_case(tape, &mut draw, first, others, judge, &mut tally));
    let passed = matches!(verdict, Verdict::Passed(_));
    Ok(Compared { verdict, summary: Summary::new(tally, declared, passed) })
}

/// `differ_all` of two sides: `a` is the first, the one `b` is compared to.
#[track_caller]
pub fn differ<I: Debug, O: Debug + PartialEq>(
    config: Config,
    draw: impl FnMut(&Tape) -> I,
    a: Side<'_, I, O>,
    b: Side<'_, I, O>,
    judge: Differ<'_, I, O>,
) {
    differ_all(config, draw, &[a, b], judge);
}

/// Compares `sides` as `compare` does, inside a `check`: what the person gave in the
/// environment (a seed, a count, a tape) is read, a difference no approval allows panics with
/// the check's report (the least tape, its input, every compared answer, the line that replays
/// it); a pass prints its line and then the summary's, and panics naming each approved
/// difference that matched none of the declared cases.
#[track_caller]
pub fn differ_all<I: Debug, O: Debug + PartialEq>(
    config: Config,
    mut draw: impl FnMut(&Tape) -> I,
    sides: &[Side<'_, I, O>],
    judge: Differ<'_, I, O>,
) {
    let at = Location::caller();
    let (first, others) = match shape(sides, &judge) {
        Ok(shape) => shape,
        Err(refused) => panic!("blaze-sim: differ at {}:{}: {refused}", at.file(), at.line()),
    };
    let declared = config.cases;
    let mut tally = Tally::new(judge.approved);
    // `check` returns only on a pass; anything else panicked with its report.
    check(config, |tape| one_case(tape, &mut draw, first, others, &judge, &mut tally));
    let summary = Summary::new(tally, declared, true);
    say_summary(at, &summary);
    if summary.stale().next().is_some() {
        panic!(
            "blaze-sim: differ at {}:{}: approved differences that matched none of the {declared} declared cases: {}; delete each, or draw the inputs it is for",
            at.file(),
            at.line(),
            Stale(&summary)
        );
    }
}

/// A summary's stale approvals, quoted and joined by commas: `"a", "b"`.
struct Stale<'s>(&'s Summary);

impl Display for Stale<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, reason) in self.0.stale().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "\"{reason}\"")?;
        }
        Ok(())
    }
}

/// Prints a passing run's summary.
#[expect(
    blaze::effect_breadth,
    reason = "one `println!`: std's `_print` brings IO, blocking, locking and allocating and its arguments' formatting panicking, so any function that prints a line has all five"
)]
fn say_summary(at: &Location<'_>, summary: &Summary) {
    println!("blaze-sim: differ at {}:{}: {summary}", at.file(), at.line());
}

/// The sides split into the first and the others, or the comparison refused.
fn shape<'s, 'f, I, O>(
    sides: &'s [Side<'f, I, O>],
    judge: &Differ<'_, I, O>,
) -> Result<(&'s Side<'f, I, O>, &'s [Side<'f, I, O>]), Refused> {
    if sides.len() > MOST_SIDES {
        return Err(Refused::TooManySides { given: sides.len(), most: MOST_SIDES });
    }
    if judge.approved.len() > MOST_APPROVED {
        return Err(Refused::TooManyApproved { given: judge.approved.len(), most: MOST_APPROVED });
    }
    match sides.split_first() {
        Some((first, others)) if !others.is_empty() => Ok((first, others)),
        Some(_) | None => Err(Refused::TooFewSides { given: sides.len() }),
    }
}

/// One case: draws the input, asks the first side, then each other side in turn; the first
/// difference no approval allows ends the case as the oracle's finding, its text the
/// difference print.
fn one_case<I: Debug, O: Debug + PartialEq>(
    tape: &Tape,
    draw: &mut impl FnMut(&Tape) -> I,
    first: &Side<'_, I, O>,
    others: &[Side<'_, I, O>],
    judge: &Differ<'_, I, O>,
    tally: &mut Tally,
) -> Result<(), Broke> {
    // Counted before the draw: a case the draw discards was entered too.
    tally.entered += 1;
    let input = draw(tape);
    let first_answer = answer(first, &input, judge.normalize);
    let mut any_approved = false;
    for other in others {
        let other_answer = answer(other, &input, judge.normalize);
        match (&first_answer, &other_answer) {
            (Answer::Returned(one), Answer::Returned(two)) if one == two => continue,
            (Answer::Panicked { .. }, Answer::Panicked { .. }) => {
                tally.panicked_alike += 1;
                continue;
            }
            (Answer::Returned(_) | Answer::Panicked { .. }, _) => {}
        }
        let pair = Pair {
            input: &input,
            first: first.name,
            first_answer: &first_answer,
            other: other.name,
            other_answer: &other_answer,
        };
        let allowed = judge.approved.iter().position(|entry| (entry.applies)(&pair));
        match allowed.and_then(|index| tally.fired.get_mut(index)) {
            Some((_, count)) => {
                *count += 1;
                any_approved = true;
            }
            None => {
                tally.differed += 1;
                return Err(difference(&pair));
            }
        }
    }
    if any_approved {
        tally.approved += 1;
    } else {
        tally.equal += 1;
    }
    Ok(())
}

/// What `side` gives for `input`, normalised; a panic is its answer too.
fn answer<I, O>(side: &Side<'_, I, O>, input: &I, normalize: fn(O) -> O) -> Answer<O> {
    match catch_unwind(AssertUnwindSafe(|| (side.run)(input))) {
        Ok(value) => Answer::Returned(normalize(value)),
        Err(payload) => panicked(&*payload),
    }
}

/// The answer of a side that panicked with `payload`: its text.
fn panicked<O>(payload: &(dyn Any + Send)) -> Answer<O> {
    let message = match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
        (Some(text), _) => (*text).to_owned(),
        (None, Some(text)) => text.clone(),
        (None, None) => "a panic whose payload is not text".to_owned(),
    };
    Answer::Panicked { message }
}

/// The oracle's finding for a difference no approval allows, its text the difference print:
/// which side differs from which, then the input and both answers, the labels padded to one
/// column.
fn difference<I: Debug, O: Debug>(pair: &Pair<'_, I, O>) -> Broke {
    let width = INPUT.len().max(pair.first.len()).max(pair.other.len());
    Broke::Oracle(format!(
        "`{other}` differs from `{first}`\n  {INPUT:width$}  {input:?}\n  {first:width$}  {first_answer}\n  {other:width$}  {other_answer}",
        other = pair.other,
        first = pair.first,
        input = pair.input,
        first_answer = pair.first_answer,
        other_answer = pair.other_answer,
    ))
}

/// The difference print's label of the input.
const INPUT: &str = "input";
