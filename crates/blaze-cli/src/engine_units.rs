//! The `Units` stage of `blaze engine-check` (engine.md E6, second step): the records every later
//! task's plan is made from, found forward before the first phase as the files are
//! (engine_check.rs `plan`), each published by its task, entered once.
//!
//! - **Units**: the root's and each member's units record, as the layout made them (units.rs
//!   `run`).
//! - **Fetch** and **UnitGraph**: the dependency graph is resolved one level of the lock at a
//!   time (graph.rs `run`): each level names the lock entries whose manifests it needs, which are
//!   fetched (fetch.rs `run`) and handed to the next level. That is discovery, the way the file
//!   walk is, so it runs here, in waves bounded by [`LEVELS`]; each `Fetch` and the graph are
//!   then one task each, published in wave 0 and 1 of the stage (the graph reads the fetches).
//! - The names world is made from the graph's record (names.rs `world`): it names every crate
//!   of the build, members, dependencies and the toolchain shelf's, each one `Names` task.
//!
//! THE GRAPH IS MADE ONCE AND READ BACK (plan row D2). The unit graph is a function of the
//! manifests, the lock, the toolchain and the ask; when none of them moved, the stage reads the
//! stored answer's rows and the graph's record ([`read_back`]): no fetch runs, no level of the
//! lock is walked, no crate list is written. What it checks, each through the one home that
//! already says it (rule 30):
//!
//! - every units record today's layout made (`units::lay`, whose reads are the sweep's rows:
//!   manifests, listings, the home's module listings) stands by its row's key and out. A units
//!   record's out holds the toolchain's id (units.rs `member_units`), so a moved pin is seen here;
//! - the graph's row stands by its key, `unit_graph_task::source` over the lock's bytes, the
//!   layout's reads (`sweep::units_source`), the target and host (cfg from the toolchain's
//!   manifest and the profile's panic) and the ask;
//! - every stored `Fetch` row stands ([`fetch_stands`]): its key, and for a held one its record
//!   still in the sources store (one stat; the bytes were checked once when fetched, rule 74).
//!
//! Anything else is a full discover, and the profile's phase line names why ([`UnitsCause`]). A
//! stored record that cannot be read back is a typed refusal ([`BackRefused`]), never a quiet
//! second route: the full discover runs and says that refusal is its cause (rule 3).
//!
//! What the engine core would need for the plan to be handed over in one piece: a way to grow
//! the plan after a merge (`Engine::grow(&Edit)`, refusing a task of a phase not after the
//! merged one), so a stage's tasks are planned from the merged records of the stage before
//! instead of by the planner ahead of the first phase.

use core::fmt;

use blaze_drive::{Condition, Input, Key, Kind, Locate, Name, Part, Task, TaskFailure};
use blaze_engine::{Row, SMap, Stage};
use blaze_ids::ContentId;
use blaze_own::Vec;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;

use crate::fetch::FetchEnv;
use crate::graph_payload::PayloadError;
use crate::names::Names;
use crate::units::Laid;
use crate::work::{Did, Made};
use crate::{CliError, engine_check::EngineCheckError};

/// What the stage did this request (`Checked.units`): the graph read back, or discovered, and
/// how many `Fetch` tasks ran. A stored fetch whose row stands is never run, in a discover too.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Units {
    /// 1 when the graph's record and its fetches' rows were read back.
    pub(crate) read_back: u32,
    /// 1 when the graph was resolved level by level from the lock.
    pub(crate) discovered: u32,
    /// The `Fetch` tasks run (fetch.rs `run`).
    pub(crate) fetches_run: u32,
}

/// The stored answer as `check` loaded it (engine_answer.rs `load_answer`): each row, and each
/// row's record key. Both empty for a fresh request or none stored.
pub(crate) struct Back<'b, 'a> {
    pub(crate) rows: &'b SMap<'a, Name, Row<'a>>,
    pub(crate) records: &'b SMap<'a, Name, Key>,
}

/// Why the graph was or was not read back: the profile's phase line says it ([`UnitsCause::phase`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnitsCause {
    /// Read back: nothing it is a function of moved.
    ReadBack,
    /// No stored answer: a fresh request, none stored, or one that did not read.
    NoAnswer,
    /// No lock, so no graph this request: only the units records.
    NoGraph,
    /// A units record is new, failed, or moved (a manifest, a listing, the toolchain's id).
    UnitsMoved { name: Name },
    /// The graph's key moved: the lock, the layout's reads, the target, the host or the ask.
    GraphMoved,
    /// The stored graph or a stored fetch failed: a hole is asked again by the next command.
    Failed { name: Name },
    /// A held fetch's record is gone from the sources store.
    SourceGone { name: Name },
    /// A fetch this build could not make (built without `fetch`): its read list (cargo's
    /// archive cache) is no stamp of the sweep's, so it is asked again each command.
    Unfetched { name: Name },
    /// The stored graph's world is not made again from its bytes (a shelf or source the world
    /// reads moved under the same key), or the record was sealed with no crate list: the finding
    /// of a world not made, which a units record does not keep, is made again.
    World,
    /// The stored record of a row that stands could not be read back (rule 3).
    Refused(BackRefused),
}

impl UnitsCause {
    /// The phase's name in the profile.
    pub(crate) fn phase(self) -> &'static str {
        match self {
            UnitsCause::ReadBack => "plan (the units read back: the graph's record, no fetch run, no graph walk)",
            UnitsCause::NoAnswer | UnitsCause::NoGraph => "plan (the units, the fetches and the graph)",
            UnitsCause::UnitsMoved { .. } => "plan (the units, the fetches and the graph: a units record moved)",
            UnitsCause::GraphMoved => "plan (the units, the fetches and the graph: the graph's key moved)",
            UnitsCause::Failed { .. } => "plan (the units, the fetches and the graph: a stored fetch or graph had failed)",
            UnitsCause::SourceGone { .. } => "plan (the units, the fetches and the graph: a fetched source record is gone)",
            UnitsCause::Unfetched { .. } => "plan (the units, the fetches and the graph: a fetch this build cannot make is asked again)",
            UnitsCause::World => "plan (the units, the fetches and the graph: the stored graph's world was not made again)",
            UnitsCause::Refused(_) => "plan (the units, the fetches and the graph: the stored graph record was refused)",
        }
    }
}

impl fmt::Display for UnitsCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitsCause::Refused(why) => write!(f, "{why}"),
            UnitsCause::UnitsMoved { name } | UnitsCause::Failed { name } | UnitsCause::SourceGone { name } | UnitsCause::Unfetched { name } => write!(f, "{} ({name:?})", self.phase()),
            _ => f.write_str(self.phase()),
        }
    }
}

/// Why the graph's stored record, under a row that stands, was not read back. Each names the
/// key the stored answer gave (rule 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackRefused {
    /// The stored answer names no record key for the graph's row (pages.rs `records`).
    NoKey,
    /// The store holds nothing at the key.
    Missing { key: Key },
    /// The bytes there are no whole drive record of the graph.
    Unreadable { key: Key, why: Option<blaze_drive::WireError> },
    /// The record's out is not the row's.
    Out { key: Key },
    /// The payload is not a sealed graph and crate list (graph_payload.rs).
    Payload { key: Key, why: PayloadError },
    /// The graph section does not decode (macros.rs `parts`).
    Graph { key: Key, why: blaze_unit_graph::record::RecordRefused },
}

impl fmt::Display for BackRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the stored unit graph was not read back (engine_units.rs `read_back`): ")?;
        match self {
            BackRefused::NoKey => f.write_str("the stored answer names no record for its row"),
            BackRefused::Missing { key } => write!(f, "the store holds no record at {key:?}"),
            BackRefused::Unreadable { key, why } => write!(f, "the bytes at {key:?} are no whole record of it ({why:?})"),
            BackRefused::Out { key } => write!(f, "the record at {key:?} has another out than its row"),
            BackRefused::Payload { key, why } => write!(f, "the record at {key:?}: {why}"),
            BackRefused::Graph { key, why } => write!(f, "the record at {key:?}: {why}"),
        }
    }
}

/// At most this many levels of the dependency graph (rule 18): a lock deeper than this is a
/// typed refusal, never a wait.
pub(crate) const LEVELS: u32 = 1 << 12;

/// Why a published task failed, with its own words when it said any.
pub(crate) struct Failure<'f> {
    pub(crate) failure: TaskFailure,
    pub(crate) words: Option<&'f str>,
}

/// One record the planner made, published by its task: its name, source, wave in the stage,
/// and the record or why it failed.
pub(crate) struct Published<'f> {
    pub(crate) name: Name,
    pub(crate) source: ContentId,
    pub(crate) wave: u16,
    pub(crate) made: Result<Made<'f>, Failure<'f>>,
}

/// What the stage found: every record, sorted by name, and the names world.
pub(crate) struct Discovered<'f> {
    pub(crate) published: Vec<Published<'f>, &'f Frame>,
    /// Made from the graph's record: `None` with no lock, or when it could not be made (the
    /// graph's record then holds the finding).
    pub(crate) names: Option<Names<'f>>,
    /// What the stage did, and why.
    pub(crate) units: Units,
    pub(crate) cause: UnitsCause,
}

impl Discovered<'_> {
    /// The record of `name`, if it was made.
    pub(crate) fn get(&self, name: Name) -> Option<&Published<'_>> {
        self.published.binary_search_by(|one| one.name.cmp(&name)).ok().map(|at| &self.published[at])
    }
}

/// A drive task for an old function: entered once (`entry` 0), under no condition, its inputs
/// handed whole.
pub(crate) fn drive_task<'a>(name: Name, key: Key, source: ContentId, inputs: blaze_own::Vec<Input, blaze_own::Alloc<'a>>, failed: blaze_own::Vec<Name, blaze_own::Alloc<'a>>, alloc: blaze_own::Alloc<'a>) -> Task<'a> {
    Task {
        name,
        base: name,
        condition: Condition::ANY,
        key,
        source,
        class: blaze_drive::Class::Foreground,
        entry: 0,
        home: name,
        place: 0,
        file: None,
        edition: 0,
        inputs,
        failed,
        members: blaze_own::Vec::new_in(alloc),
    }
}

/// What a wrapped function answered, as a published record: `Need` is the planner's bug, named
/// with the read it did not see (never a second entry).
pub(crate) fn published<'f>(name: Name, did: Did<'f>) -> Result<Result<Made<'f>, Failure<'f>>, CliError> {
    match did {
        Did::Done(made) => Ok(Ok(made)),
        Did::Failed(failure) => Ok(Err(Failure { failure, words: None })),
        Did::FailedSaying(failure, words) | Did::Hole(failure, words) => Ok(Err(Failure { failure, words: Some(words) })),
        Did::Need(wants) => Err(EngineCheckError::Unseen { task: name, read: wants.first().map_or(name, |want| want.name) }.into()),
    }
}

/// The sources store (fetch.rs `sources_dir`), opened at the first stored `Fetch` row looked at.
struct Sources {
    dir: Option<Dir<ReadWrite>>,
}

impl Sources {
    fn open(&mut self, cx: &Cx, env: &FetchEnv<'_>) -> Result<&Dir<ReadWrite>, CliError> {
        let dir = match self.dir.take() {
            Some(dir) => dir,
            None => crate::fetch::sources_dir(cx, env)?,
        };
        Ok(self.dir.insert(dir))
    }
}

/// The stored answer's `Fetch` rows, in name order: names sort by kind first, so they are one run
/// of the map, found in `log n` (never a walk of every row).
fn stored_fetches<'b, 'a>(rows: &'b SMap<'a, Name, Row<'a>>) -> impl Iterator<Item = (&'b Name, &'b Row<'a>)> + 'b {
    rows.range(Name::new(Kind::Fetch, ContentId([0; 32]))..).take_while(|(name, _)| name.kind == Kind::Fetch)
}

/// Whether the stored row of the `Fetch` task `name` stands, and so its out, without running it.
/// Its read list (rule 21): the lock entry's record key, which is its subject and its source
/// (fetch_task.rs `source`), in its key; for a held record, the record at that key in the sources
/// store, looked at by one stat (its bytes were checked once, when fetched or adopted, rule 74);
/// for a refusal, nothing more where the refusal is lasting (built with `fetch`). Built without
/// it the refusal also read cargo's archive cache, which no stamp of the sweep's covers: it never
/// stands, and is asked again each command.
fn fetch_stands(cx: &Cx, name: Name, row: &Row<'_>, tools: blaze_engine::Tools, sources: &mut Sources, env: &FetchEnv<'_>) -> Result<Result<ContentId, UnitsCause>, CliError> {
    if row.failed {
        return Ok(Err(UnitsCause::Failed { name }));
    }
    if row.phase.stage != Stage::Units || row.key != tools.key(name, &name.subject) {
        return Ok(Err(UnitsCause::GraphMoved));
    }
    match row.out == name.subject {
        true => match crate::fetch::holds(cx, sources.open(cx, env)?, &name.subject)? {
            true => Ok(Ok(row.out)),
            false => Ok(Err(UnitsCause::SourceGone { name })),
        },
        false if cfg!(feature = "fetch") => Ok(Ok(row.out)),
        false => Ok(Err(UnitsCause::Unfetched { name })),
    }
}

/// `None` when the stored graph stands with every input it is a function of: each units record
/// today's layout made (`records`) by its row's key and out, the graph's row by `graph_key`, every
/// stored fetch by [`fetch_stands`]. Else the first input that moved.
#[expect(clippy::too_many_arguments, reason = "the planner's edge: each argument one handle or fact of it")]
fn stood(cx: &Cx, records: &[Published<'_>], back: &Back<'_, '_>, units: ContentId, graph_key: Key, tools: blaze_engine::Tools, sources: &mut Sources, env: &FetchEnv<'_>) -> Result<Option<UnitsCause>, CliError> {
    if back.rows.is_empty() {
        return Ok(Some(UnitsCause::NoAnswer));
    }
    for one in records.iter() {
        let Ok(made) = &one.made else { return Ok(Some(UnitsCause::UnitsMoved { name: one.name })) };
        match back.rows.get(&one.name) {
            Some(row) if !row.failed && row.key == tools.key(one.name, &units) && row.out == made.out => {}
            _ => return Ok(Some(UnitsCause::UnitsMoved { name: one.name })),
        }
    }
    let name = Name::unit_graph();
    match back.rows.get(&name) {
        Some(row) if row.key != graph_key => return Ok(Some(UnitsCause::GraphMoved)),
        Some(row) if row.failed => return Ok(Some(UnitsCause::Failed { name })),
        Some(_) => {}
        None => return Ok(Some(UnitsCause::GraphMoved)),
    }
    for (fetch, row) in stored_fetches(back.rows) {
        if let Err(why) = fetch_stands(cx, *fetch, row, tools, sources, env)? {
            return Ok(Some(why));
        }
    }
    Ok(None)
}

/// The graph's stored record read back under its standing row (`stood` said so): its out and
/// payload as put, its `MacroModule` parts made again from its graph section (the store keeps a
/// units record's head without parts, engine_store.rs `put_all`), each stored fetch row as its
/// record, all pushed onto `records`; and the names world made from the graph section. Nothing is
/// fetched, no level of the lock walked, no crate list written. `Err` names why not (rule 3):
/// nothing is pushed then, and the caller discovers in full.
#[expect(clippy::too_many_arguments, reason = "the planner's edge: each argument one handle or fact of it")]
fn read_back<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn crate::store::Store<'_>,
    back: &Back<'_, '_>,
    lockfile: &crate::fetch::Lockfile<'f>,
    graph: &crate::graph::Graph<'f>,
    records: &mut Vec<Published<'f>, &'f Frame>,
    env: &FetchEnv<'_>,
    frame: &'f Frame,
) -> Result<Result<Option<Names<'f>>, UnitsCause>, CliError> {
    let _cx = cx.span("engine: the unit graph read back");
    let name = Name::unit_graph();
    let Some(row) = back.rows.get(&name) else { return Ok(Err(UnitsCause::GraphMoved)) };
    let refused = |why: BackRefused| -> Result<Result<Option<Names<'f>>, UnitsCause>, CliError> { Ok(Err(UnitsCause::Refused(why))) };
    let (key, view, sections) = match stored_graph(cx, store, back.records)? {
        Ok(found) => found,
        Err(why) => return refused(why),
    };
    if view.out != row.out {
        return refused(BackRefused::Out { key });
    }
    // The module lookups' parts, as graph.rs `run` names them from the same bytes.
    let parts = match crate::macros::parts(graph.source, sections.graph, env.heap.alloc()) {
        Ok(parts) => parts,
        Err(why) => return refused(BackRefused::Graph { key, why }),
    };
    // A record sealed with no list holds the finding of a world that could not be made, which a
    // units record keeps nowhere but its record: discovered in full, so the finding is made
    // again (rare: the names did not resolve).
    if sections.list.is_empty() {
        return Ok(Err(UnitsCause::World));
    }
    // The names world, from the graph section as the full discover makes it. A world that is
    // not made from bytes it was made from before is a moved input under one key: discovered in
    // full.
    let world = {
        let _cx = cx.span("names: make the world");
        crate::names::world(cx, root, sections.graph, Some(lockfile.lock), Some(graph.member_dirs()), env)
    };
    let Ok(made) = world else { return Ok(Err(UnitsCause::World)) };
    crate::names::tell_public(store, &made);
    for (fetch, row) in stored_fetches(back.rows) {
        let mut record = Made::new_in(*fetch, frame);
        record.out = row.out;
        records.push(Published { name: *fetch, source: fetch.subject, wave: 0, made: Ok(record) });
    }
    let mut record = Made::new_in(name, frame);
    record.out = view.out;
    // Its reads as its stored row holds them (none, for a graph made whole: `graph::run` records
    // none), so the row put again equals the row stored.
    record.reads.extend(row.reads.iter().map(|(id, out)| (id.of, *out)));
    record.parts.extend(parts.iter().map(|(name, source)| Part { name: *name, locate: Locate::Source(*source), place: 0 }));
    record.payload.extend_from_slice(view.payload);
    records.push(Published { name, source: graph.source, wave: 1, made: Ok(record) });
    Ok(Ok(Some(made)))
}

/// The stage's records: the units, then the graph read back when nothing it is a function of
/// moved ([`read_back`]), else found forward from the layout: the fetches level by level (each
/// stored fetch whose row stands handed as it was, not run), the graph, and the names world made
/// from it.
#[expect(clippy::too_many_arguments, reason = "the planner's edge: each argument one handle or fact of it")]
pub(crate) fn discover<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn crate::store::Store<'_>,
    laid: &mut Laid<'f>,
    units: ContentId,
    tools: blaze_engine::Tools,
    back: &Back<'_, '_>,
    env: &FetchEnv<'_>,
    frame: &'f Frame,
) -> Result<Discovered<'f>, CliError> {
    let cx = cx.span("engine: the units stage found");
    let alloc = env.heap.alloc();
    let mut records: Vec<Published<'f>, &'f Frame> = Vec::new_in(frame);
    let mut counts = Units::default();
    // The units records, the root's and each member's.
    let mut unit_names: Vec<Name, &Frame> = Vec::new_in(frame);
    unit_names.push(Name::units());
    unit_names.extend(laid.members.iter().map(|(name, _)| *name));
    for name in unit_names.iter() {
        let task = drive_task(*name, tools.key(*name, &units), units, Vec::new_in(alloc), Vec::new_in(alloc), alloc);
        let made = published(*name, crate::units::run(laid, &task, frame)?)?;
        records.push(Published { name: *name, source: units, wave: 0, made });
    }
    let mut names = None;
    let mut cause = UnitsCause::NoGraph;
    let mut sources = Sources { dir: None };
    if let Some((lockfile, graph)) = laid.lock.as_mut().zip(laid.graph.as_mut()) {
        let name = Name::unit_graph();
        let key = tools.key(name, &graph.source);
        cause = match stood(&cx, &records, back, units, key, tools, &mut sources, env)? {
            Some(why) => why,
            None => match read_back(&cx, root, store, back, lockfile, graph, &mut records, env, frame)? {
                Ok(world) => {
                    names = world;
                    UnitsCause::ReadBack
                }
                Err(why) => why,
            },
        };
        if cause == UnitsCause::ReadBack {
            counts.read_back = 1;
            records.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            return Ok(Discovered { published: records, names, units: counts, cause });
        }
        counts.discovered = 1;
        let mut inputs: Vec<Input, blaze_own::Alloc<'_>> = Vec::new_in(alloc);
        let mut failed: Vec<Name, blaze_own::Alloc<'_>> = Vec::new_in(alloc);
        let mut level = 0u32;
        let made = loop {
            level += 1;
            if level > LEVELS {
                return Err(EngineCheckError::Levels { levels: LEVELS }.into());
            }
            let task = drive_task(name, key, graph.source, inputs.clone(), failed.clone(), alloc);
            let did = crate::graph::run(&cx, lockfile, graph, &task, env, frame, frame)?;
            let wants = match did {
                Did::Need(wants) => wants,
                did => break published(name, did)?,
            };
            // One level's lock entries, each fetched and handed to the next level.
            for want in wants.iter() {
                let fetch = want.name;
                if fetch.kind != Kind::Fetch || inputs.iter().any(|input| input.name == fetch) || failed.contains(&fetch) {
                    return Err(EngineCheckError::Unseen { task: name, read: fetch }.into());
                }
                let fetch_key = tools.key(fetch, &fetch.subject);
                // A fetch whose stored row stands is handed as it was (rule 77): only a fetch
                // one of whose inputs moved, or one never made, runs.
                let stands = match back.rows.get(&fetch) {
                    Some(row) => fetch_stands(&cx, fetch, row, tools, &mut sources, env)?.ok(),
                    None => None,
                };
                let made = match stands {
                    Some(out) => {
                        let mut record = Made::new_in(fetch, frame);
                        record.out = out;
                        Ok(record)
                    }
                    None => {
                        counts.fetches_run += 1;
                        published(fetch, crate::fetch::run(&cx, lockfile, env, fetch, frame)?)?
                    }
                };
                match &made {
                    Ok(record) => inputs.push(Input { name: fetch, at: fetch_key, out: record.out }),
                    Err(_) => failed.push(fetch),
                }
                records.push(Published { name: fetch, source: fetch.subject, wave: 0, made });
            }
        };
        let made = match made {
            Ok(mut record) => {
                // The names world (work.rs's `UnitGraph` arm): every crate of the build.
                let world = {
                    let _cx = cx.span("names: make the world");
                    crate::names::world(&cx, root, record.payload_bytes()?, Some(lockfile.lock), Some(graph.member_dirs()), env)
                };
                // The record sealed with the crate list as work.rs's arm seals it: its readers
                // (a `Script` job, a macro module's lookup) open its two sections
                // (graph_payload.rs); unsealed, they were refused "no trailer mark".
                match world {
                    Ok(made) => match blaze_world::crate_list::encode(made.world(), &|at| made.script_name_of(at), env.heap.alloc()) {
                        Ok(list) => {
                            record.out = crate::work::seal_graph(&mut record, &list);
                            crate::names::tell_public(store, &made);
                            names = Some(made);
                            Ok(record)
                        }
                        Err(error) => {
                            let words = bstd::mem::format_in!(frame, "the crate list of the world could not be written: {error}");
                            let failure = TaskFailure { name, why: blaze_drive::Why::Refused { what: "the crate list", rule: "crate_list.rs" } };
                            Err(Failure { failure, words: Some(crate::work::held_str(words.as_str(), frame)) })
                        }
                    },
                    Err(error) => {
                        let words = bstd::mem::format_in!(frame, "names not resolved: {error}");
                        record.find(crate::fetch::SOURCE_REFUSED, (0, 0), &[words.as_str()]);
                        record.out = crate::work::seal_graph(&mut record, &[]);
                        Ok(record)
                    }
                }
            }
            Err(failure) => Err(failure),
        };
        records.push(Published { name, source: graph.source, wave: 1, made });
    }
    records.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(Discovered { published: records, names, units: counts, cause })
}

/// The graph's record the stored answer names (`records`, pages.rs), opened whole, and its two
/// sections: its key, its view and its sections, borrowed from the store's mapping. `Err` names
/// why not (rule 3).
type StoredGraph<'m> = (Key, blaze_drive::record::WholeView<'m>, crate::graph_payload::Sections<'m>);

fn stored_graph<'m>(cx: &Cx, store: &mut dyn crate::store::Store<'m>, records: &SMap<'_, Name, Key>) -> Result<Result<StoredGraph<'m>, BackRefused>, CliError> {
    let name = Name::unit_graph();
    let Some(&key) = records.get(&name) else { return Ok(Err(BackRefused::NoKey)) };
    let Some(held) = store.get(cx, key)? else { return Ok(Err(BackRefused::Missing { key })) };
    let view = match blaze_drive::record::open(held.bytes) {
        Ok(blaze_drive::record::Stored::Whole(view)) if view.name == name => view,
        Ok(_) => return Ok(Err(BackRefused::Unreadable { key, why: None })),
        Err(why) => return Ok(Err(BackRefused::Unreadable { key, why: Some(why) })),
    };
    match crate::graph_payload::sections(view.payload) {
        Ok(sections) => Ok(Ok((key, view, sections))),
        Err(why) => Ok(Err(BackRefused::Payload { key, why })),
    }
}

/// Each workspace member crate's root folder in the checkout (`a/src/`, checkout-relative, with
/// its `/`; empty for a crate whose root is at the checkout's top) and its package's edition,
/// from the stored unit graph: what `blaze why` outlines a member's file under (engine_why.rs),
/// never an edition chosen for it.
pub(crate) struct Editions<'f> {
    roots: Vec<(&'f str, blaze_surface::Edition), &'f Frame>,
}

impl Editions<'_> {
    /// The edition of the crate whose root folder is the deepest one holding `path`; `None`
    /// when no member crate's folder holds it.
    pub(crate) fn of(&self, path: &str) -> Option<blaze_surface::Edition> {
        self.roots.iter().filter(|(folder, _)| path.starts_with(folder)).max_by_key(|(folder, _)| folder.len()).map(|(_, edition)| *edition)
    }
}

/// The member crates' editions ([`Editions`]) from the stored answer's graph record
/// (`records`): each member row of its crate list (its root, `/0/<folder>/<file>`, and its
/// package's name and version) joined with the graph's units of that package, whose edition it
/// takes. A list with no rows (the world was not made) gives none. `Err` names why the record
/// was not read (rule 3).
pub(crate) fn stored_editions<'f>(cx: &Cx, store: &mut dyn crate::store::Store<'_>, records: &SMap<'_, Name, Key>, frame: &'f Frame) -> Result<Result<Editions<'f>, BackRefused>, CliError> {
    let (key, _, sections) = match stored_graph(cx, store, records)? {
        Ok(found) => found,
        Err(why) => return Ok(Err(why)),
    };
    let erased = blaze_own::Erased::new(frame);
    let decoded = match blaze_unit_graph::record::decode(sections.graph, erased.handle()) {
        Ok(decoded) => decoded,
        Err(why) => return Ok(Err(BackRefused::Graph { key, why })),
    };
    let mut roots: Vec<(&'f str, blaze_surface::Edition), &'f Frame> = Vec::new_in(frame);
    let list = match sections.crates() {
        Ok(Some(list)) => list,
        Ok(None) => return Ok(Ok(Editions { roots })),
        Err(why) => return Ok(Err(BackRefused::Payload { key, why })),
    };
    for r in 0..blaze_world::crate_list::CrateGraph::len(&list) {
        if !list.member(r) {
            continue;
        }
        let Some((package, version)) = list.env_facts(r).package else { continue };
        let Some(unit) = decoded.graph.units.iter().find(|unit| unit.name.as_str() == package && unit.version.as_str() == version) else { continue };
        // `/0/a/src/lib.rs`: the checkout's tree, then the file; its folder is the crate's.
        let Some(file) = list.root(r).strip_prefix("/0/") else { continue };
        let folder = match file.rsplit_once('/') {
            Some((folder, _)) => crate::work::held_str(bstd::mem::format_in!(frame, "{folder}/").as_str(), frame),
            None => "",
        };
        roots.push((folder, crate::units::syntax_of(unit.edition.syntax)));
    }
    Ok(Ok(Editions { roots }))
}
