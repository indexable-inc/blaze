//! What `check --no-daemon` says: per unit, the tasks it ran and the records it ended with;
//! the time of each phase; the outcome, and each failure by name with its unit and item. Plain
//! text into any [`fmt::Write`]; the command line's driver puts it on the terminal.
//!
//! The seam for real diagnostics is [`diagnostics`]: handed a body record's bytes and the
//! line table of its file, it is to write each diagnostic at its line and column. What it needs
//! and does not have yet is said on it.
//!
//! The invariant: every unit, failure and phase the summary holds is printed, in the graph's
//! order; nothing is left out because it is zero but a unit that ran nothing and holds no
//! record, which is one line of totals. Floor: one line per unit that did anything, written
//! once; nothing is allocated but the writer's own.

#![blaze::sans_io]

use std::{alloc::Allocator, fmt, time::Duration};

use blaze_ids::ContentId;
use blaze_keyed::{
    driver::{Failure, Filed, ItemPlace, NO_UNIT, Outcome, Ran, TaskFailed},
    record::StableId,
};

use crate::{
    CommandError,
    graph::Graph,
    run::{Summary, named},
};

/// The summary of `summary` over `graph`, with `before` the phases the command line ran before
/// [`crate::run::check`] (the graph's load, the front end's making), into `out`.
pub fn summary<A: Allocator + Clone, W: fmt::Write>(
    graph: &Graph<A>,
    summary: &Summary<A>,
    before: &[(&str, Duration)],
    out: &mut W,
) -> fmt::Result {
    let mut quiet = 0usize;
    for (at, (unit, done)) in graph.units.iter().zip(summary.units.iter()).enumerate() {
        let ran = done.keyed + done.surfaces + done.bodies;
        let held = u32::from(done.manifest) + done.surface + done.body;
        let told = summary.told.iter().filter(|(of, _)| *of as usize == at);
        let failed = summary.failures.iter().filter(|why| unit_of(why).is_some_and(|of| of as usize == at));
        if ran == 0 && held == 0 && done.failed == 0 && told.clone().next().is_none() && failed.clone().next().is_none() {
            quiet += 1;
            continue;
        }
        writeln!(
            out,
            "  {}: ran {} key, {} surface, {} body{}; holds {} manifest, {} surface, {} body",
            named(unit),
            done.keyed,
            done.surfaces,
            done.bodies,
            Failed(done.failed),
            u32::from(done.manifest),
            done.surface,
            done.body,
        )?;
        // Every failure of the unit, then what its refused tasks said, under the unit.
        for why in failed {
            write!(out, "    ")?;
            failure(summary, why, out)?;
        }
        for (_, line) in told {
            writeln!(out, "    {line}")?;
        }
    }
    // A line about a unit the graph has not (a front end's bug) is still printed, by number.
    for (of, line) in summary.told.iter().filter(|(of, _)| *of as usize >= graph.units.len()) {
        writeln!(out, "  unit {of}: {line}")?;
    }
    // A failure of no unit (the store, the certificate, a race), of a program's plan, code or
    // link task (recorded at `NO_UNIT`: what it was of is told under the program's unit, by
    // the task, `program::caught`), or of a unit the graph has not.
    for why in summary.failures.iter() {
        match unit_of(why) {
            Some(of) if (of as usize) < graph.units.len() => continue,
            Some(NO_UNIT) => write!(out, "  a program's task (told above, under its unit): ")?,
            Some(of) => write!(out, "  unit {of}: ")?,
            None => write!(out, "  ")?,
        }
        failure(summary, why, out)?;
    }
    if quiet > 0 {
        writeln!(out, "  {quiet} units ran nothing and were not opened: the certificate or their marks stood")?;
    }
    let sweep = &summary.sweep;
    writeln!(
        out,
        "  sweep: {} stats, {} listings, {} probes, {} racy, {} files hashed; {} reads",
        sweep.stated, sweep.listed, sweep.probed, sweep.racy, sweep.hashed, summary.reads
    )?;
    write!(out, "  time:")?;
    for (name, took) in before {
        write!(out, " {name} {}", Ms(*took))?;
    }
    let phases = &summary.phases;
    writeln!(
        out,
        " sweep {} plan {} look {} gather {} host {} keep {}",
        Ms(phases.sweep),
        Ms(phases.plan),
        Ms(phases.look),
        Ms(phases.gather),
        Ms(phases.host),
        Ms(phases.keep)
    )?;
    let load = &graph.load;
    writeln!(
        out,
        "  load: resolve {} units {} scripts {} keys {}",
        Ms(load.resolve),
        Ms(load.units),
        Ms(load.scripts),
        Ms(load.keys)
    )?;
    match summary.gathered {
        true => writeln!(out, "  gathered every unit's text: the certificate's look missed")?,
        false => writeln!(out, "  gathered nothing: the certificate's look held")?,
    }
    writeln!(out, "  store: {} gets, {} memo hits", summary.asks.gets, summary.asks.memo_hits)?;
    outcome(summary, out)?;
    if let Some(error) = &summary.store_error {
        writeln!(out, "  the store said: {error}")?;
    }
    Ok(())
}

/// `error`, a unit named by its crate and package where it names one by index.
pub fn error<A: Allocator + Clone, W: fmt::Write>(graph: &Graph<A>, error: &CommandError, out: &mut W) -> fmt::Result {
    match error {
        CommandError::Unit { unit, rule } => writeln!(out, "{}: {rule}", unit_name(graph, *unit)),
        other => writeln!(out, "{other}"),
    }
}

/// Whether the outcome is a success the command exits 0 on.
pub fn succeeded(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Certified { .. } | Outcome::Built { .. })
}

fn outcome<A: Allocator + Clone, W: fmt::Write>(summary: &Summary<A>, out: &mut W) -> fmt::Result {
    match &summary.outcome {
        Outcome::Certified { .. } => writeln!(out, "nothing changed: the certificate held, no task ran"),
        Outcome::Built { ran, filed } => {
            write!(out, "checked: ")?;
            totals(ran, out)?;
            match filed {
                Filed::Certificate => writeln!(out, "; certificate filed"),
                Filed::Full => writeln!(out, "; no certificate filed: every shape slot of this base is taken, the next command settles each unit by its mark"),
                Filed::Audited => writeln!(out, "; audited, nothing filed"),
            }
        }
        Outcome::Failed { ran, why } => {
            write!(out, "failed after ")?;
            totals(ran, out)?;
            // Every failure is printed above, under its unit; the outcome's `why` is the first.
            match summary.failures.len() {
                0 => {
                    write!(out, "; ")?;
                    failure(summary, why, out)
                }
                n => writeln!(out, "; {n} failures, each printed above under its unit"),
            }
        }
        Outcome::Stalled { ran } => {
            write!(out, "stalled (a bug of the driver: nothing ready and nothing asked) after ")?;
            totals(ran, out)?;
            writeln!(out)
        }
    }
}

fn totals<W: fmt::Write>(ran: &Ran, out: &mut W) -> fmt::Result {
    write!(
        out,
        "{} keyed, {} surfaces, {} bodies run; {} records found, {} units by their mark, {} manifests stood, {} filed",
        ran.keyed, ran.surfaces, ran.bodies, ran.found, ran.marked, ran.stood, ran.manifests
    )
}

/// Unit `at` of the graph, by name; an index the graph has not, by number.
fn unit_name<A: Allocator + Clone>(graph: &Graph<A>, at: u32) -> UnitName<'_, A> {
    UnitName { graph, at }
}

struct UnitName<'g, A: Allocator + Clone> {
    graph: &'g Graph<A>,
    at: u32,
}

impl<A: Allocator + Clone> fmt::Display for UnitName<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.graph.units.get(self.at as usize) {
            Some(unit) => write!(f, "{}", named(unit)),
            None => write!(f, "unit {}", self.at),
        }
    }
}

/// The unit a failure is about, `None` for one of the request (the store, the certificate, a
/// race, the protocol, the audit, a key's two values).
fn unit_of(why: &Failure) -> Option<u32> {
    match *why {
        Failure::Task { unit, .. }
        | Failure::MadeCycle { unit, .. }
        | Failure::NotMade { unit, .. }
        | Failure::MakerNotKeyed { unit, .. }
        | Failure::TwoDigests { unit, .. } => Some(unit),
        Failure::TwoValues { .. }
        | Failure::Store(_)
        | Failure::Record { .. }
        | Failure::Outside(_)
        | Failure::Raced { .. }
        | Failure::Audit { .. }
        | Failure::Misused(_) => None,
    }
}

/// One failure, one line, without its unit: the caller prints it under the unit
/// ([`unit_of`]) or before it.
fn failure<A: Allocator + Clone, W: fmt::Write>(summary: &Summary<A>, why: &Failure, out: &mut W) -> fmt::Result {
    match why {
        Failure::Task { tag, unit, failed } => {
            write!(out, "a task of kind {} ", tag.0)?;
            task_failed(summary, *unit, failed, out)
        }
        Failure::TwoValues { tag, key } => writeln!(out, "two values filed at one key of kind {} ({}): a key misses an input", tag.0, Hex(key)),
        Failure::Store(fault) => writeln!(out, "the store: {}", fault.what),
        Failure::Record { tag, key, why } => writeln!(out, "the record of kind {} at {} is not what its key says: {why}", tag.0, Hex(key)),
        Failure::Outside(error) => writeln!(out, "the certificate's reads: {error}"),
        Failure::Raced { at } => writeln!(out, "read {at} of the request moved while it ran: nothing certified; run again"),
        Failure::MadeCycle { items, .. } => {
            write!(out, "made facts did not settle, a cycle through")?;
            for item in items {
                write!(out, " {}", Item(item))?;
            }
            writeln!(out)
        }
        Failure::NotMade { item, kind, id, .. } => {
            writeln!(out, "item {} holds fact {} of kind {kind}, which it may not make", Item(item), Hex(id))
        }
        Failure::MakerNotKeyed { item, .. } => writeln!(
            out,
            "its surface names item {} as the body that makes a demanded fact, and its key task gave no such item",
            Item(item)
        ),
        Failure::TwoDigests { id, .. } => writeln!(out, "fact {} was answered two ways in one request", Hex(id)),
        Failure::Audit { tag, address, first } => {
            writeln!(out, "the audit: the record of kind {} at {} made again differs first at {first:?}", tag.0, Hex(address))
        }
        Failure::Misused(what) => writeln!(out, "the host broke the driver's protocol: {what}"),
    }
}

/// A task of unit `unit`'s failure: the rule, for a refused fact its query kind and subject,
/// for two items under one key both at `file:line` ([`Summary::place`]), for a body not given
/// its item.
fn task_failed<A: Allocator + Clone, W: fmt::Write>(summary: &Summary<A>, unit: u32, failed: &TaskFailed, out: &mut W) -> fmt::Result {
    let what = failed.what();
    match failed {
        TaskFailed::Panicked { what } => writeln!(out, "panicked: {what}"),
        TaskFailed::Refused { what } => writeln!(out, "was refused: {what}"),
        TaskFailed::RefusedFact { what, kind, subject } => {
            writeln!(out, "asked a fact no rule answers: {what} (query kind {kind}, subject {})", Hex(subject))
        }
        TaskFailed::TwoItems { first, second } => writeln!(
            out,
            "found two items under one key, at {} and at {}: {what}",
            Place { summary, unit, at: *first },
            Place { summary, unit, at: *second }
        ),
        TaskFailed::NoBody { item } => writeln!(out, "read the MIR of {}, a body it was not given: {what}", Item(item)),
    }
}

/// An item place of a unit as [`Summary::places`] holds it (`path:line`); a place it does not
/// hold (a summary not made by [`crate::run::check`]) by its file index and row, which still
/// name the item.
struct Place<'a, A: Allocator + Clone> {
    summary: &'a Summary<A>,
    unit: u32,
    at: ItemPlace,
}

impl<A: Allocator + Clone> fmt::Display for Place<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.summary.place(self.unit, self.at) {
            Some(shown) => f.write_str(shown),
            None => write!(f, "file {} of the unit, item row {}", self.at.file, self.at.row),
        }
    }
}

/// `, N failed`, or nothing for none.
struct Failed(u32);

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0 => Ok(()),
            n => write!(f, ", {n} failed"),
        }
    }
}

struct Ms(Duration);

impl fmt::Display for Ms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:03}ms", self.0.as_millis(), self.0.as_micros() % 1000)
    }
}

/// An id by its first eight bytes, as a person compares two.
struct Hex<'a>(&'a ContentId);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0.0[..8] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// An item by its stable id: the front end names it (lane K2b's `front::Command` maps an id to
/// its path; until it is handed here, the id).
struct Item<'a>(&'a StableId);

impl fmt::Display for Item<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "item:{}", Hex(&self.0.0))
    }
}

// --- the seam for real diagnostics ----------------------------------------------------------------

/// A file's line table: the byte offset each line starts at, the first `0`, ascending (the
/// line table blaze-unitkey's file image holds).
#[derive(Clone, Copy, Debug)]
pub struct Lines<'a> {
    pub starts: &'a [u32],
}

impl Lines<'_> {
    /// The line and column (both from 1, the column in bytes) of byte `offset`.
    pub fn at(&self, offset: u32) -> (u32, u32) {
        let line = self.starts.partition_point(|start| *start <= offset).max(1);
        let start = self.starts.get(line - 1).copied().unwrap_or(0);
        (u32::try_from(line).unwrap_or(u32::MAX), offset - start + 1)
    }
}

/// Why [`diagnostics`] wrote nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unwritten {
    /// A body record's payload holds no diagnostics yet: blaze-front's body task writes its
    /// verdict, not the item-relative diagnostics host.md names.
    NoDiagnosticsInPayload,
}

impl fmt::Display for Unwritten {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unwritten::NoDiagnosticsInPayload => f.write_str(
                "a body record's payload carries no diagnostics yet (blaze-front's body task writes none): nothing to print at a line",
            ),
        }
    }
}

/// The diagnostics of one body, each at its line and column of `path`, into `out`.
///
/// What it needs and does not have: (1) a layout of diagnostics in a body record's payload
/// (severity, a code, a message, a span relative to the item's start), which blaze-front's
/// body task does not write; (2) the item's start offset in its file, which blaze-unitkey's
/// file image holds per item and a `Body` record does not name, so the caller must hand it
/// with the file's [`Lines`]; (3) the file's path, which the graph's unit and the image's file
/// order give. Until (1) exists it writes nothing and says so: never an empty success.
pub fn diagnostics<W: fmt::Write>(body: &[u8], item_start: u32, lines: Lines<'_>, path: &str, out: &mut W) -> Result<usize, Unwritten> {
    let _ = (body, item_start, lines, path, out);
    Err(Unwritten::NoDiagnosticsInPayload)
}
