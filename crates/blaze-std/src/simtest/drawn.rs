//! The drawn half of `#[bstd::test]` (D109): a test whose parameters include values DRAWN from
//! a tape, run by blaze's own engine (blaze-sim's [`Search`]: a case is a function of one tape
//! of choices, shrunk and replayed) inside the test's simulated worlds (granted.rs).
//!
//! - **A case is a world seed and a tape.** For world seed `s` the search runs the cases of
//!   tapes seeded `s ^ TAPE_SALT`, `+ 1`, .. (`cases = N` in the attribute, [`CASES`] by
//!   default): a stream apart from the world's own, so a tape's choices never move the world's.
//!   A test whose capabilities meet no fault (only `cx` and drawn values, or `Out`, `Args`,
//!   `Env`) runs its cases under ONE world seed: the world adds nothing a second seed changes.
//! - **Drawn once, before the first start.** The first start draws from the case's tape and
//!   its record fixes the choices; every restart (after a power cut, a give-up under faults)
//!   draws again from THAT record replayed, in its own start's frame, so it gets the same
//!   values; a restart whose draws take other choices is a failure named as such
//!   ([`DrawnWorld::run`]). The planted bug its test catches draws again from the live tape.
//! - **A failure shrinks the tape, the world's seed fixed**: blaze-sim's shrinker through
//!   [`Search`], each candidate run as a whole world under the same seed. Two failures are the
//!   same when their first lines are equal with the numbers taken out ([`origin`]): a panic's
//!   message names its check and not its values' digits. (A panic's `Location` would be the
//!   better origin; bstd catches panics with no hook that keeps it: the next piece.)
//! - **The report** is granted.rs's block with the shrunk VALUE of each drawn parameter (its
//!   `Debug` form, at most `SHOWN_MOST` characters), the tape as blaze-sim's text, and ONE
//!   replay line holding the seed and the tape; then the line to keep.
//! - **Kept tapes**: with `kept` in the attribute, the crate's `sim-kept.tapes` lines kept for
//!   `<test>@<seed>` are replayed FIRST, each under its seed, before any search; a line kept
//!   under other primitives is not replayed and is said by name ([`KeptRefusal`]).
//!
//! The seed, the count and the tape a person gives come in as arguments (granted.rs reads
//! them from the test entry's environment capability): nothing here reads the machine.

use alloc::string::String as Text;
use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use blaze_core::sim::{
    Sim,
    seeds::{self, Ended, Given as SeedsGiven, Replay, Run, World},
};
use blaze_own::{String, Vec};
use blaze_sim::{
    Config, Failure, Origin, Record, Tail, Tape, Verdict,
    kept::{self, KeptRefusal},
    search::{Next, Search},
};
use blaze_text::Writes as _;

use super::{
    Outcome,
    granted::{Laid, Test, boot, laid, report_drawn, spec_of, start},
    listed,
    param::{DrawnReport, Draws},
};
use crate::{
    grant::{Given, LaunchAlloc, Manifest, Refused, Spec},
    io::Io,
};

/// What a world seed is XORed with to seed its first case's tape: a stream apart from the
/// world's (the digits of no other constant of the tree).
pub const TAPE_SALT: u64 = 0x5d1c_7a9e_3b48_f061;

/// Cases per world seed when the attribute says no `cases = N`: each case is a whole world
/// run, restarts and all.
pub const CASES: u32 = 64;

/// The most `cases = N` may ask.
pub const CASES_MOST: u32 = 4096;

/// Runs the shrinker may spend on one failure: each is a whole world run.
pub const SHRINK_RUNS: u32 = 300;

/// `cases = N`, checked as the crate compiles.
pub const fn cases(n: u32) -> u32 {
    if n == 0 {
        panic!("bstd::test: `cases = 0` runs nothing; give 1 or more");
    }
    if n > CASES_MOST {
        panic!("bstd::test: `cases = N` above 4096: each case is a whole world run; split the property");
    }
    n
}

/// How a restart draws (the planted bug its test catches draws again from the live tape).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Again {
    /// The first start's record, replayed: the same values.
    Replay,
    #[cfg(test)]
    Live,
}

/// One case's draws, shared by its starts.
struct Case<'c> {
    /// The case's tape, live at the first start.
    tape: &'c Tape,
    /// Each restart's replay of the first start's choices.
    replay: Tape,
    /// What the first start drew.
    record: RefCell<Record>,
    values: RefCell<alloc::vec::Vec<u64>>,
    starts: Cell<u32>,
    shown: Option<&'c RefCell<String<LaunchAlloc>>>,
    again: Again,
}

/// A test's world for one seed, with one case's draws.
pub(crate) struct DrawnWorld<'p, B> {
    io: Io,
    spec: &'p Spec,
    manifest: &'static Manifest,
    body: &'p B,
    alloc: LaunchAlloc,
    case: &'p Case<'p>,
}

impl<B, R> World for DrawnWorld<'_, B>
where
    B: Fn(&mut Given<'_>, &Draws<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    fn run(&mut self) -> Ended {
        let case = self.case;
        let n = case.starts.get() + 1;
        case.starts.set(n);
        let restart_live = match case.again {
            Again::Replay => false,
            #[cfg(test)]
            Again::Live => true,
        };
        if n == 1 || restart_live {
            let draws = Draws::of(case.tape, case.shown.filter(|_| n == 1));
            let ended = start(&self.io, self.spec, self.manifest, self.alloc, &|given: &mut Given<'_>| (self.body)(given, &draws));
            if n == 1 {
                let mut record = case.record.borrow_mut();
                case.tape.finish(&mut record);
                let mut values = case.values.borrow_mut();
                values.clear();
                values.extend(record.choices.iter().map(|choice| choice.value));
            }
            return ended;
        }
        let values = case.values.borrow();
        case.replay.reset(&values, Tail::Calm);
        let draws = Draws::of(&case.replay, None);
        let ended = start(&self.io, self.spec, self.manifest, self.alloc, &|given: &mut Given<'_>| (self.body)(given, &draws));
        let mut again = Record::new();
        case.replay.finish(&mut again);
        if again.unused > 0 || again.clamped > 0 || again.choices.len() != values.len() {
            // alloc: a failure's message.
            return Ended::Panicked(alloc::format!(
                "bstd::test: start {n} drew {} choices where the first start drew {} ({} left undrawn, {} lowered): a drawn parameter's draw is not a function of its tape",
                again.choices.len(),
                values.len(),
                again.unused,
                again.clamped
            ));
        }
        ended
    }

    fn crash(&mut self) {
        self.io.crash();
    }

    fn with_sim<T>(&mut self, f: impl FnOnce(&mut Sim) -> T) -> T {
        match self.io.with_sim(f) {
            Some(it) => it,
            None => unreachable!("a bstd::test's world is Io::simulated's: simulated"),
        }
    }
}

/// What one test's drawn run needs, made once.
struct Runner<'p, B> {
    test: &'p Test,
    spec: Spec,
    laid: Vec<Laid, LaunchAlloc>,
    body: &'p B,
    alloc: LaunchAlloc,
    again: Again,
}

impl<B, R> Runner<'_, B>
where
    B: Fn(&mut Given<'_>, &Draws<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    /// One case: world seed `seed`, its draws from `tape` as it stands; the run, what the first
    /// start drew, and the last world (for what `out` and `err` wrote).
    fn case(&self, seed: u64, tape: &Tape, shown: Option<&RefCell<String<LaunchAlloc>>>) -> (Run, Record, Option<Io>) {
        let case = Case {
            tape,
            replay: Tape::new(&[], Tail::Calm, Tape::MOST_CHOICES),
            record: RefCell::new(Record::new()),
            values: RefCell::new(alloc::vec::Vec::new()),
            starts: Cell::new(0),
            shown,
            again: self.again,
        };
        let mut last: Option<Io> = None;
        let run = seeds::one(seed, &self.test.planned.seeds, &mut |seed: u64| {
            let io = boot(seed, self.test.setup, &self.laid);
            // clone: the world's handle, kept for what `out` and `err` wrote.
            last = Some(io.clone());
            DrawnWorld { io, spec: &self.spec, manifest: self.test.manifest, body: self.body, alloc: self.alloc, case: &case }
        });
        (run, case.record.into_inner(), last)
    }

    /// The search of world seed `seed`: its cases, and on a failure the shrunk tape, as
    /// blaze-sim's verdict, and the world effects its runs asked for.
    fn search(&self, seed: u64, given: Option<&[u64]>) -> (Verdict, u64) {
        let cases = self.test.setup.cases.unwrap_or(CASES);
        let given = blaze_sim::Given { seed: None, seeds: None, tape: given.map(<[u64]>::to_vec) };
        let config = Config { cases, first: seed ^ TAPE_SALT, shrink_runs: SHRINK_RUNS, ..Config::DEFAULT }.given(given);
        let mut search = Search::new(config);
        let tape = Tape::new(&[], Tail::Calm, Tape::MOST_CHOICES);
        let mut effects = 0;
        loop {
            match search.next() {
                Next::Run(proposal) => tape.reset(proposal.prefix, proposal.tail),
                Next::Done(verdict) => return (verdict.clone(), effects),
            }
            let (run, record, _) = self.case(seed, &tape, None);
            effects += run.counts.effects;
            search.ran(&record, ended_of(&run));
        }
    }

    /// The failing case `values` of world seed `seed` run twice more, its values shown: the
    /// report's block, and the keep line. `(at, of)` is which world seed it was.
    fn report(&self, seed: u64, values: &[u64], (at, of): (u32, u32), how: &str, out: &mut String<LaunchAlloc>) {
        let listed = listed(self.test.module, self.test.name);
        let shown = RefCell::new(String::new_in(self.alloc));
        shown.borrow_mut().str(how);
        let tape = Tape::replay(values);
        let (run, _, world) = self.case(seed, &tape, Some(&shown));
        tape.reset(values, Tail::Calm);
        let (again, _, _) = self.case(seed, &tape, None);
        let replay = replayed(&run, &again);
        let tape_text = blaze_sim::text::print(values);
        let shown = shown.into_inner();
        let drawn = DrawnReport { values: shown.as_str(), tape: tape_text.as_str() };
        report_drawn(self.test, &listed, &run, (at, of), &replay, world.as_ref(), Some(&drawn), out);
        let why = match &run.verdict {
            Err(broke) => broke.why.as_str(),
            Ok(()) => "it passed when run for the report",
        };
        out.str("\n").str(kept::KEEP);
        let mut path = String::new_in(self.alloc);
        path.str(&listed).str("@").int(seed);
        match kept::write_line(out, path.as_str(), kept::PRIMITIVES, values, kept::DATE_TO_FILL, why) {
            Ok(()) => {
                out.str("\n  add it to ").str(kept::FILE_NAME).str(" beside the Cargo.toml of ").str(self.test.package);
                out.str(", with today's date for ").str(kept::DATE_TO_FILL).str(", and write `kept` in the test's attribute");
            }
            Err(_) => {
                out.str("none: the test path `").str(path.as_str()).str("` holds white space or `#`");
            }
        }
    }
}

/// How a case ended, as the search sees it.
fn ended_of(run: &Run) -> blaze_sim::Ended {
    match &run.verdict {
        Ok(()) => blaze_sim::Ended::Passed,
        // alloc: a failure's text and origin, the forms blaze-sim's `Failure` holds.
        Err(broke) => blaze_sim::Ended::Failed(Failure { origin: origin(&broke.why), text: broke.why.clone() }),
    }
}

/// Which failure a run's `why` is: its first line with each run of digits written `#`, so a
/// shrunk case whose values differ is the same failure, and another check is not.
pub(crate) fn origin(why: &str) -> Origin {
    let mut at = Text::new();
    let mut digits = false;
    for c in why.lines().next().unwrap_or_default().chars() {
        match c.is_ascii_digit() {
            true if digits => {}
            true => {
                digits = true;
                at.push('#');
            }
            false => {
                digits = false;
                at.push(c);
            }
        }
    }
    Origin::Panic { at }
}

/// Whether the report's two runs of one case were the same run, byte for byte.
fn replayed(first: &Run, second: &Run) -> Replay {
    if first.verdict == second.verdict && first.trace == second.trace {
        return Replay::Same;
    }
    let (mut a, mut b) = (first.trace.lines(), second.trace.lines());
    let mut line = 1;
    loop {
        match (a.next(), b.next()) {
            (x, y) if x == y && x.is_some() => line += 1,
            (x, y) => {
                // alloc: once per failure, the lines that differ.
                let said = |it: Option<&str>| Text::from(it.unwrap_or("(the trace ended; the verdicts differ)"));
                return Replay::Diverged { line, first: said(x), second: said(y) };
            }
        }
    }
}

/// What the person gave for a drawn test, beside the world's seed and count.
pub struct DrawnGiven<'g> {
    pub world: SeedsGiven,
    /// `BLAZE_SIM_TAPE`'s text: one case, run alone under the given seed, not shrunk.
    pub tape: Option<&'g str>,
}

/// `test`'s `body` with its drawn parameters: kept tapes first (with `kept`), then each world
/// seed's search; the first failure's report block, or the seeds that passed.
pub fn run<B, R>(test: &Test, given: DrawnGiven<'_>, kept: Option<kept::File>, alloc: LaunchAlloc, body: &B) -> Result<u32, String<LaunchAlloc>>
where
    B: Fn(&mut Given<'_>, &Draws<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    run_with(test, given, kept, alloc, body, Again::Replay)
}

pub(crate) fn run_with<B, R>(
    test: &Test,
    given: DrawnGiven<'_>,
    kept: Option<kept::File>,
    alloc: LaunchAlloc,
    body: &B,
    again: Again,
) -> Result<u32, String<LaunchAlloc>>
where
    B: Fn(&mut Given<'_>, &Draws<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    let mut out = String::new_in(alloc);
    let (spec, laid) = match spec_of(test, alloc).and_then(|spec| laid(test.setup, alloc).map(|laid| (spec, laid))) {
        Ok(set_up) => set_up,
        Err(why) => {
            out.str("bstd::test: `").str(test.name).str("` could not be set up: ");
            why.line(&mut out);
            return Err(out);
        }
    };
    let runner = Runner { test, spec, laid, body, alloc, again };
    let listed = listed(test.module, test.name);
    // A test no fault reaches runs its cases under ONE world seed.
    let one_world = test.planned.faults == 0;
    let tape = match given.tape {
        None => None,
        Some(text) => match blaze_sim::text::parse(text) {
            Ok(tape) => Some(tape),
            Err(error) => {
                out.str("bstd::test: `").str(blaze_sim::search::TAPE).str("` is not a tape: ");
                let _ = write!(out, "{error}");
                return Err(out);
            }
        },
    };
    if tape.is_some() && given.world.seed.is_none() && !one_world {
        out.str("bstd::test: `").str(blaze_sim::search::TAPE).str("` is one case of ONE world seed: give `");
        out.str(seeds::SEED_VAR).str("` with it, as the replay line does");
        return Err(out);
    }
    let seeds = &test.planned.seeds;
    let (first, count) = match (given.world.seed, given.world.count) {
        (Some(seed), _) => (seed, 1),
        (None, _) if one_world => (seeds.first, 1),
        (None, count) => (seeds.first, count.unwrap_or(seeds.count)),
    };
    if let Some(file) = kept {
        replay_kept(&runner, &listed, file, &mut out)?;
    }
    let mut effects = 0;
    for at in 0..count {
        let seed = first.wrapping_add(u64::from(at));
        let (verdict, did) = runner.search(seed, tape.as_deref());
        effects += did;
        let mut values = alloc::vec::Vec::new();
        let how = match &verdict {
            Verdict::Passed(_) => continue,
            Verdict::Failed(failed) => {
                failed.least.values_into(&mut values);
                // alloc: once per failure, the line above the values.
                alloc::format!(
                    "drawn (case {} of {}, shrunk from {} to {} choices in {} runs):\n",
                    failed.case,
                    failed.cases,
                    failed.first_len,
                    values.len(),
                    failed.shrink_runs
                )
            }
            Verdict::NotAFunction { tape, .. } => {
                values.extend_from_slice(tape);
                Text::from("drawn: THE SHRUNK TAPE DOES NOT FAIL AGAIN: the case reads something outside its seed and tape\n")
            }
            Verdict::Misfit { clamped, unused, .. } => {
                out.str("bstd::test: the given tape does not fit the test's draws: ").int(*clamped).str(" values lowered, ");
                out.int(*unused).str(" never drawn");
                return Err(out);
            }
            Verdict::DrewNothing { .. } | Verdict::AllDiscarded { .. } | Verdict::Unreached { .. } => {
                out.str("bstd::test: `").str(&listed).str("` drew nothing from its tape in any case: its drawn parameters are all `()`");
                return Err(out);
            }
        };
        runner.report(seed, &values, (at + 1, count), &how, &mut out);
        return Err(out);
    }
    if !one_world && effects == 0 {
        out.str("bstd::test: `").str(&listed).str("` asked the simulation for nothing in any case: its faults tested nothing");
        return Err(out);
    }
    Ok(count)
}

/// The crate's kept lines for this test (`<test>@<seed>`), each run alone under its seed, in
/// file order; a line of another primitives version is said by name in the report of a
/// failure, and not replayed.
fn replay_kept<B, R>(runner: &Runner<'_, B>, listed: &str, file: kept::File, out: &mut String<LaunchAlloc>) -> Result<(), String<LaunchAlloc>>
where
    B: Fn(&mut Given<'_>, &Draws<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    let mut refused = alloc::vec::Vec::new();
    for line in file.lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                let _ = write!(out, "{error}");
                return Err(core::mem::replace(out, String::new_in(runner.alloc)));
            }
        };
        let Some((test, seed)) = line.test.rsplit_once('@') else { continue };
        if test != listed {
            continue;
        }
        let Ok(seed) = seed.parse::<u64>() else {
            out.str("bstd::test: line ").int(line.number).str(" of `").str(file.krate).str("`'s ").str(kept::FILE_NAME);
            out.str(" is kept for `").str(line.test).str("`, whose seed after `@` is not a number");
            return Err(core::mem::replace(out, String::new_in(runner.alloc)));
        };
        if let Some(refusal) = KeptRefusal::of(file, &line) {
            refused.push(refusal);
            continue;
        }
        let values = match blaze_sim::text::parse(line.tape) {
            Ok(values) => values,
            Err(error) => {
                let _ = write!(out, "bstd::test: line {} of `{}`'s {}: {error}", line.number, file.krate, kept::FILE_NAME);
                return Err(core::mem::replace(out, String::new_in(runner.alloc)));
            }
        };
        let tape = Tape::replay(&values);
        let (run, _, _) = runner.case(seed, &tape, None);
        if run.verdict.is_err() {
            for refusal in &refused {
                let _ = writeln!(out, "{refusal}");
            }
            out.str("kept tape replayed first: line ").int(line.number).str(" of `").str(file.krate).str("`'s ").str(kept::FILE_NAME).str("\n");
            runner.report(seed, &values, (1, 1), "drawn (a kept tape):\n", out);
            return Err(core::mem::replace(out, String::new_in(runner.alloc)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
