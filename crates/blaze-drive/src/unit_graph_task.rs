//! The `UnitGraph` task (docs/v2/plan/design/units.md, "The record"): which crates a request
//! compiles, and with which features and cfgs, as one record (blaze-unit-graph).
//!
//! Inputs, all known before the work: the lock's bytes, the workspace members' facts with the
//! digest of the `Units` record's out (what the workspace layout read, the toolchain's id with
//! it), the target and host descriptions and the ask. Their digest
//! ([`blaze_unit_graph::record::Source`]) is the task's source; the record is valid by its key.
//! Each dependency's manifests are read from its fetched sources' record through the
//! [`Records`] the driver passes (the records of the `Fetch` jobs it was handed, mapped); a
//! package whose record it was not handed is asked of `Fetch` ([`Entry::Need`]), so only what
//! the roots reach is ever fetched. One whose `Fetch` failed this command leaves no graph
//! ([`Entry::NotFetched`]); one whose sources are refused for good, when the graph reaches it,
//! leaves no graph either: [`UnitGraphTaskError::Missing`] names it with its finding.
//!
//! The record's bytes are a function of the graph alone: an edit to a member's manifest that
//! changes no resolution gives byte-equal bytes, so every task keyed by the record's digest is
//! cut off early.

use core::fmt;

use blaze_id::ContentId;
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AVec, Alloc};
use blaze_unit_graph::record::{self, NotItsCfg, Source};
use blaze_unit_graph::{Ask, LeftOut, Open, Package, Pass, Records, Resolution, Resolver, SourceError, StoreSources, TargetDesc, Text, UnitError};

/// The most passes of the resolution: each pass asks for the packages one level deeper, so the
/// count is the graph's depth (tempo: 9). Past it, [`UnitGraphTaskError::Passes`] (rule 18).
pub const MOST_PASSES: u32 = 256;

/// What the task is handed.
pub struct UnitGraphInputs<'i, 'a> {
    pub lock: &'i [u8],
    /// The digest of what the workspace's layout read (its manifests and listings, in order).
    pub workspace_read: ContentId,
    /// Each member's lock id and facts.
    pub members: &'i [(LockId, Package<'a>)],
    /// Each non-member checkout path package's lock id and facts (from `[patch.*]` or path
    /// dependencies outside `workspace.members`).
    pub path_packages: &'i [(LockId, Package<'a>)],
    pub resolver: Resolver,
    pub target: &'i TargetDesc<'a>,
    pub host: &'i TargetDesc<'a>,
    pub ask: &'i Ask<'a>,
}

/// The task's source: the digest of its inputs, computed before any work (rule 65).
pub fn source(inputs: &UnitGraphInputs<'_, '_>) -> ContentId {
    let mut s = Source::new();
    s.lock(inputs.lock);
    s.workspace(&inputs.workspace_read.0);
    // The toolchain's id is not added here: the `Units` record's out already carries it, and
    // `workspace_read` is that (one home per input, rule 30).
    s.platform("target", inputs.target).platform("host", inputs.host).ask(inputs.ask);
    ContentId(s.finish().0)
}

/// Why the task failed: each names the thing.
#[derive(Debug)]
pub enum UnitGraphTaskError<'a> {
    /// The lock's plan (blaze-fetch's) refused it.
    Source(SourceError<'a>),
    Resolve(UnitError<'a>),
    Cfg(NotItsCfg),
    Passes,
    /// The graph reached packages whose sources are refused for good (a checksum that is not
    /// the lock's, an archive that does not unpack, no manifest): no graph is made from the
    /// rest (blaze-unit-graph's `Pass::Partial`; states.md 35). Each `name version`, sorted
    /// by lock id, with the findings that say why, in the order asked.
    Missing { packages: AVec<'a, Text<'a>>, findings: AVec<'a, SourceError<'a>> },
}

impl fmt::Display for UnitGraphTaskError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitGraphTaskError::Source(e) => write!(f, "{e}"),
            UnitGraphTaskError::Resolve(e) => write!(f, "{e}"),
            UnitGraphTaskError::Cfg(e) => write!(f, "{e}"),
            UnitGraphTaskError::Passes => write!(f, "the unit graph did not settle in {MOST_PASSES} passes"),
            UnitGraphTaskError::Missing { packages, findings } => {
                write!(f, "the unit graph reached {} package(s) whose sources could not be read, so no graph is made (rule 3):", packages.len())?;
                for (i, package) in packages.iter().enumerate() {
                    write!(f, "{} {package}", if i == 0 { "" } else { "," })?;
                }
                for finding in findings.iter() {
                    write!(f, "; {finding}")?;
                }
                Ok(())
            }
        }
    }
}

/// What the task made: the graph's record bytes (blaze-unit-graph's `record` form), and one
/// finding a dependency whose sources could not be read (not fetched, refused, no manifest),
/// in the order asked: a package the graph never reached (a graph that reaches one is
/// [`UnitGraphTaskError::Missing`]); the task maps each finding to a finding record naming it.
pub struct UnitGraphMade<'a> {
    pub record: AVec<'a, u8>,
    pub findings: AVec<'a, SourceError<'a>>,
    /// The roots' targets left out by `required-features` (also in the record): the task
    /// says each as a note, never quietly (rule 3).
    pub left_out: AVec<'a, LeftOut<'a>>,
}

/// What one entry of the task answered (drive.md, "For a successor", step 1). The task holds
/// nothing between entries: each replays the resolution from the roots over the records it was
/// handed, so the reach is fetched level by level (tempo: 9 deep).
pub enum Entry<'a> {
    /// The graph, whole: every package it reached was read from its record.
    Made(UnitGraphMade<'a>),
    /// The `Fetch` subjects (record keys) of the packages this level reached whose records it
    /// was not handed, sorted, each once: the task is entered again once they settle.
    Need(AVec<'a, ContentId>),
    /// `count` packages it reached had a `Fetch` that failed this command (`Task::failed`).
    /// No record is kept: an incomplete graph must never stand by its key, so the next command
    /// asks again.
    NotFetched { count: u32 },
}

/// The `Fetch` jobs a task was handed that hold no sources, by subject, each sorted.
#[derive(Clone, Copy)]
pub struct Unheld<'s> {
    /// Failed this command, with no record (the network's or the store's fault): no graph.
    pub failed: &'s [ContentId],
    /// Refused for good, a record saying so (a checksum that is not the lock's, an archive
    /// that does not unpack): a graph that reaches it is refused naming it.
    pub refused: &'s [ContentId],
}

/// Runs one entry of the task over `lock` (the lock `inputs.lock` is the text of). `records`
/// holds the records of the `Fetch` jobs the task was handed; a package whose record it lacks
/// is asked for unless its subject is in `unheld`.
pub fn run<'b, 'a, R: Records<'b>>(
    inputs: &UnitGraphInputs<'_, 'a>,
    lock: &Lock<'a>,
    records: R,
    open: Open<'b>,
    unheld: Unheld<'_>,
    alloc: Alloc<'a>,
) -> Result<Entry<'a>, UnitGraphTaskError<'a>> {
    let mut findings = AVec::new_in(alloc);
    let mut sources = StoreSources::new(lock, records, open, alloc).map_err(UnitGraphTaskError::Source)?;
    let mut res = Resolution::new(lock, inputs.target, inputs.host, inputs.ask, inputs.resolver, alloc);
    for (is_member, (id, package)) in inputs
        .members
        .iter()
        .map(|entry| (true, entry))
        .chain(inputs.path_packages.iter().map(|entry| (false, entry)))
    {
        let package = package.clone();
        match is_member {
            true => res.member(*id, package),
            false => res.package(*id, Some(package)),
        }
        .map_err(UnitGraphTaskError::Resolve)?;
    }
    let mut not_fetched = 0u32;
    for _ in 0..MOST_PASSES {
        match res.pass().map_err(UnitGraphTaskError::Resolve)? {
            Pass::Done(graph) => {
                if not_fetched > 0 {
                    return Ok(Entry::NotFetched { count: not_fetched });
                }
                let record = record::encode(&graph, inputs.target, inputs.host, alloc).map_err(UnitGraphTaskError::Cfg)?;
                return Ok(Entry::Made(UnitGraphMade { record, findings, left_out: graph.left_out }));
            }
            Pass::Partial(partial) => {
                // A package not fetched this command is asked again by the next one.
                if not_fetched > 0 {
                    return Ok(Entry::NotFetched { count: not_fetched });
                }
                // Refused for good: no graph is made without them (states.md 35), each named.
                let mut packages = AVec::with_capacity_in(partial.missing().len(), alloc);
                for id in partial.missing() {
                    let locked = lock.package(*id);
                    let mut named = Text::new_in(alloc);
                    let _ = fmt::Write::write_fmt(&mut named, format_args!("{} {}", locked.name, locked.version));
                    packages.push(named);
                }
                return Err(UnitGraphTaskError::Missing { packages, findings });
            }
            Pass::Need(needs) => {
                let mut asks = AVec::new_in(alloc);
                for id in needs.iter() {
                    let package = match sources.package(*id) {
                        Ok(package) => Some(package),
                        Err(SourceError::NotFetched { key, .. }) if unheld.failed.binary_search(&ContentId(key)).is_ok() => {
                            not_fetched += 1;
                            None
                        }
                        Err(SourceError::NotFetched { key, .. }) if unheld.refused.binary_search(&ContentId(key)).is_err() => {
                            asks.push(ContentId(key));
                            continue;
                        }
                        Err(finding) => {
                            findings.push(finding);
                            None
                        }
                    };
                    res.package(*id, package).map_err(UnitGraphTaskError::Resolve)?;
                }
                if !asks.is_empty() {
                    // A git commit's record serves all its packages: one ask a record.
                    asks.sort_unstable();
                    asks.dedup();
                    return Ok(Entry::Need(asks));
                }
            }
        }
    }
    Err(UnitGraphTaskError::Passes)
}
