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
//! What the engine core would need for the plan to be handed over in one piece: a way to grow
//! the plan after a merge (`Engine::grow(&Edit)`, refusing a task of a phase not after the
//! merged one), so a stage's tasks are planned from the merged records of the stage before
//! instead of by the planner ahead of the first phase.

use blaze_drive::{Condition, Input, Key, Kind, Name, Task, TaskFailure};
use blaze_ids::ContentId;
use blaze_own::Vec;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;

use crate::fetch::FetchEnv;
use crate::names::Names;
use crate::units::Laid;
use crate::work::{Did, Made};
use crate::{CliError, engine_check::EngineCheckError};

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

/// The stage's records, found forward from the layout: the units, the fetches level by level,
/// the graph, and the names world made from it.
#[expect(clippy::too_many_arguments, reason = "the planner's edge: each argument one handle or fact of it")]
pub(crate) fn discover<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn crate::store::Store<'_>,
    laid: &mut Laid<'f>,
    units: ContentId,
    tools: blaze_engine::Tools,
    env: &FetchEnv<'_>,
    frame: &'f Frame,
) -> Result<Discovered<'f>, CliError> {
    let cx = cx.span("engine: the units stage found");
    let alloc = env.heap.alloc();
    let mut records: Vec<Published<'f>, &'f Frame> = Vec::new_in(frame);
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
    if let Some((lockfile, graph)) = laid.lock.as_mut().zip(laid.graph.as_mut()) {
        let name = Name::unit_graph();
        let key = tools.key(name, &graph.source);
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
                let made = published(fetch, crate::fetch::run(&cx, lockfile, env, fetch, frame)?)?;
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
    Ok(Discovered { published: records, names })
}
