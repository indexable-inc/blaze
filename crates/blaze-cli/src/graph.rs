//! The `UnitGraph` task's work (blaze_drive::unit_graph_task; drive.md, "For a successor",
//! step 1): one job a command, made when the layout and a lock were read this command.
//!
//! - **Its inputs** are known before the work: the lock's text, what the layout read (the
//!   units' source, which holds the members' manifests), the members' facts, the target's cfg
//!   from the pinned toolchain's manifest on the shelf (never a table here), and the ask. Their
//!   digest is the job's source, so a repeat stands on the graph's record and asks nothing.
//! - **Each entry** replays the resolution from the roots over the records of the `Fetch` jobs it
//!   was handed, MAPPED and read in place: a manifest is read before its dependencies are known,
//!   so the reach is fetched level by level, and a lock entry nothing reaches is never fetched
//!   nor reported. A level's asks are wanted of the fetch machine at once
//!   ([`Lockfile::want_ahead`]), so their fetches overlap.
//! - **A blaze-edition member's** `bstd` (or `blaze-std`) line is dropped here: the toolchain
//!   gives it (`blaze_unit_graph::implicit`), with `std`, `core` and `alloc`; none is fetched.
//! - **A `Fetch` that failed** this command leaves no graph record (an incomplete graph must never
//!   stand by its key), so the next command asks again; one refused for good is a record, and
//!   its package is unavailable in a graph that is kept.

use blaze_drive::unit_graph_task::{self, Entry, UnitGraphInputs, Unheld};
use blaze_drive::{Kind, Locate, Name, Part, Task, TaskFailure, Want, Why};
use blaze_fetch::SourceId;
use blaze_fetch::pack::PackView;
use blaze_ids::ContentId;
use blaze_manifest::layout;
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{Alloc, Vec};
use blaze_unit_graph::{Ask, CfgAtom, NoResolver, Package, Records, Resolver, TargetDesc};
use bstd::cx::Cx;
use bstd::fs::Fs;
use bstd::mem::Frame;

use crate::CliError;
use crate::fetch::{FetchEnv, Lockfile};
use crate::work::{Did, Made, held_str};

const GRAPH_OUT_DOMAIN: &str = "blaze-cli unit graph out 1";

/// This build's own target: the one its checks are for.
pub(crate) const TRIPLE: &str = if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
    "aarch64-apple-darwin"
} else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
    "aarch64-unknown-linux-gnu"
} else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
    "x86_64-unknown-linux-gnu"
} else {
    ""
};

/// Whether a member's dependency line names the toolchain's crate, which blaze gives with no
/// line: `bstd` (or the package `blaze-std`) in a blaze-edition member (toolchain.md, section
/// 4). The one home of the rule: the units' finding 9 and the graph's members both ask it.
pub fn toolchain_line(blaze_edition: bool, name: &str, package: &str) -> bool {
    blaze_edition && (name == "bstd" || package == "blaze-std")
}

/// Whether any member names a dependency outside the workspace and the toolchain: with none,
/// the lock is not read and there is no graph to walk (units.rs `lay`).
pub fn reaches_outside(units: &layout::Units<'_>) -> bool {
    units.members.iter().any(|member| {
        let blaze = member.edition.blaze.is_some();
        member.dependencies.iter().any(|dep| !matches!(dep.source, layout::Source::Member(_)) && !toolchain_line(blaze, dep.name.as_str(), dep.package.as_str()))
    })
}

/// The lock a workspace that reaches nothing outside stands on: its members alone, each a path
/// package with its member dependencies, in name order, as cargo writes it. Made from the
/// manifests (already the layout's reads), so no file is read: a stale `Cargo.lock` beside such a
/// workspace is never one, and the world (names.rs) is the toolchain's crates and the members.
pub fn members_lock<'f>(units: &layout::Units<'_>, frame: &'f Frame) -> &'f [u8] {
    let mut order: Vec<usize, &Frame> = Vec::with_capacity_in(units.members.len(), frame);
    order.extend(0..units.members.len());
    order.sort_unstable_by(|a, b| units.members[*a].name.as_str().cmp(units.members[*b].name.as_str()));
    let mut text: Vec<u8, &'f Frame> = Vec::new_in(frame);
    text.extend_from_slice(b"version = 4\n");
    for at in order.iter() {
        let member = &units.members[*at];
        text.extend_from_slice(b"\n[[package]]\nname = \"");
        text.extend_from_slice(member.name.as_str().as_bytes());
        text.extend_from_slice(b"\"\nversion = \"");
        text.extend_from_slice(member.version.as_ref().map_or("0.0.0", |v| v.as_str()).as_bytes());
        text.extend_from_slice(b"\"\n");
        let mut deps: Vec<&str, &Frame> = Vec::new_in(frame);
        for dep in member.dependencies.iter() {
            if let layout::Source::Member(index) = dep.source {
                deps.push(units.members[index].name.as_str());
            }
        }
        deps.sort_unstable();
        deps.dedup();
        if !deps.is_empty() {
            text.extend_from_slice(b"dependencies = [\n");
            for dep in deps.iter() {
                text.extend_from_slice(b" \"");
                text.extend_from_slice(dep.as_bytes());
                text.extend_from_slice(b"\",\n");
            }
            text.extend_from_slice(b"]\n");
        }
    }
    text.leak()
}

/// The members as the layout read them, before the lock is matched: each one's facts, its
/// toolchain line dropped, and the workspace's resolver.
pub struct Members {
    packages: std::vec::Vec<Package<'static>, std::alloc::Global>,
    /// Each member's directory, anchor-relative (`""` for the root's own package), in
    /// `packages`' order: the names world reads each member's files under its own.
    dirs: Vec<blaze_unit_graph::Text<'static>, Alloc<'static>>,
    resolver: Result<Resolver, NoResolver>,
    written: Option<std::string::String>,
}

impl Members {
    /// The facts of `units`' members, on the global heap (they live as long as the command).
    pub fn of(units: &layout::Units<'_>) -> Members {
        let mut packages = std::vec::Vec::with_capacity_in(units.members.len(), std::alloc::Global);
        let mut dirs = Vec::with_capacity_in(units.members.len(), Alloc::global());
        for member in units.members.iter() {
            let mut package = Package::from_member(member, Alloc::global());
            let blaze = member.edition.blaze.is_some();
            package.deps.retain(|dep| !toolchain_line(blaze, dep.key.as_str(), dep.package.as_str()));
            packages.push(package);
            dirs.push(blaze_unit_graph::Text::from_str_in(member.dir.as_str(), Alloc::global()));
        }
        let written = units.resolver.as_ref().map(|(text, _)| text.as_str());
        Members { packages, dirs, resolver: Resolver::of(written, units.root_edition), written: written.map(std::string::String::from) }
    }

    /// Each member matched to its lock entry (a path entry of its name).
    pub fn locked(self, lock: &Lock<'_>) -> Result<Parts, Unlocked> {
        let resolver = self.resolver.map_err(|NoResolver| Unlocked::Resolver { written: self.written.unwrap_or_default() })?;
        let mut members = std::vec::Vec::with_capacity_in(self.packages.len(), std::alloc::Global);
        let mut dirs = Vec::with_capacity_in(self.packages.len(), Alloc::global());
        for (package, dir) in self.packages.into_iter().zip(self.dirs) {
            let mut ids = lock.named(package.name.as_str()).filter(|id| matches!(lock.package(*id).source, LockSource::Path));
            let found = (ids.next(), ids.next());
            drop(ids);
            match found {
                (Some(id), None) => {
                    members.push((id, package));
                    dirs.push((id, dir));
                }
                _ => return Err(Unlocked::Member { name: std::string::String::from(package.name.as_str()) }),
            }
        }
        Ok(Parts { members, dirs, resolver })
    }
}

/// Why the graph has no job this command: a finding on the root manifest (`LOCK_UNREAD`).
#[derive(Debug)]
pub enum Unlocked {
    /// The workspace's `resolver` is none cargo knows.
    Resolver { written: std::string::String },
    /// A member with no path entry of its name in `Cargo.lock`, or more than one.
    Member { name: std::string::String },
}

impl core::fmt::Display for Unlocked {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unlocked::Resolver { written } => write!(f, "`resolver = \"{written}\"` is no resolver cargo knows (1, 2 or 3)"),
            Unlocked::Member { name } => write!(f, "the member `{name}` has no one path entry in Cargo.lock: run `cargo update -w` to write it"),
        }
    }
}

/// The members matched to the lock, with the resolver.
pub struct Parts {
    members: std::vec::Vec<(LockId, Package<'static>), std::alloc::Global>,
    /// Each member's lock entry and its directory, anchor-relative, in `members`' order.
    dirs: Vec<(LockId, blaze_unit_graph::Text<'static>), Alloc<'static>>,
    resolver: Resolver,
}

impl Parts {
    /// Each member's lock entry and its directory, anchor-relative, in the layout's order.
    pub fn member_dirs(&self) -> &[(LockId, blaze_unit_graph::Text<'static>)] {
        &self.dirs
    }
}

/// The job's inputs and what its entries mapped, for the command.
pub struct Graph<'f> {
    /// The job's source (`unit_graph_task::source`), handed in `Start`.
    pub source: ContentId,
    inputs: UnitGraphInputs<'f, 'static>,
    /// The records mapped so far, by key, sorted: a re-entry maps nothing again.
    mapped: Vec<(ContentId, &'f [u8]), &'f Frame>,
    /// Each member's lock entry and its directory, anchor-relative (`""` for the root's own
    /// package): where the names world reads that member's files ([`Graph::member_dirs`]).
    dirs: Vec<(LockId, blaze_unit_graph::Text<'static>), Alloc<'static>>,
}

impl<'f> Graph<'f> {
    /// The job of `parts` over `lockfile`, for `target`; `workspace_read` is the units' source.
    pub fn new(parts: Parts, lockfile: &Lockfile<'f>, workspace_read: ContentId, target: TargetDesc<'static>, tests: bool, frame: &'f Frame) -> Graph<'f> {
        let mut roots = Vec::with_capacity_in(parts.members.len(), Alloc::global());
        roots.extend(parts.members.iter().map(|(id, _)| *id));
        let ask = Ask { roots, features: Vec::new_in(Alloc::global()), all_features: false, no_default_features: false, tests };
        let members: &'f [(LockId, Package<'static>)] = std::boxed::Box::leak(std::boxed::Box::new_in(parts.members, frame));
        let target: &'f TargetDesc<'static> = std::boxed::Box::leak(std::boxed::Box::new_in(target, frame));
        let ask: &'f Ask<'static> = std::boxed::Box::leak(std::boxed::Box::new_in(ask, frame));
        let inputs = UnitGraphInputs { lock: lockfile.text, workspace_read, members, resolver: parts.resolver, target, host: target, ask };
        Graph { source: unit_graph_task::source(&inputs), inputs, mapped: Vec::new_in(frame), dirs: parts.dirs }
    }

    /// Each member's lock entry and its directory, anchor-relative, in the layout's order.
    pub fn member_dirs(&self) -> &[(LockId, blaze_unit_graph::Text<'static>)] {
        &self.dirs
    }
}

/// This build's target as the pinned toolchain states it: its cfg atoms, read from the
/// toolchain's manifest on the shelf under `home`, reached from the start folder `start`.
pub fn target(cx: &Cx, env: &crate::fetch::FetchEnv<'_>, frame: &Frame) -> Result<TargetDesc<'static>, CliError> {
    if TRIPLE.is_empty() {
        return Err(CliError::Text { what: "this build's machine has no target triple the toolchain is cut for" });
    }
    let Some(home) = env.home else {
        return Err(CliError::Text { what: "`HOME` is not set: the toolchain's shelf, which states the target's cfg, cannot be found" });
    };
    let mut shelf = crate::toolchain::HomeShelf::open(cx, &env.fs, home, frame).map_err(CliError::Shelf)?;
    let id = env.toolchain;
    let manifest = blaze_toolchain::shelf::open_manifest(&mut shelf, id).map_err(CliError::Toolchain)?;
    let view = blaze_toolchain::ToolchainView::open(manifest.as_ref(), id)
        .map_err(|_| CliError::Text { what: "the pinned toolchain's manifest is refused by its reader" })?;
    let atoms = view.cfg(TRIPLE).ok_or(CliError::Text { what: "the pinned toolchain was not cut for this machine's target" })?;
    let mut cfg = Vec::new_in(Alloc::global());
    for (name, value) in atoms {
        cfg.push(match value {
            Some(value) => CfgAtom::pair(name, value, Alloc::global()),
            None => CfgAtom::flag(name, Alloc::global()),
        });
    }
    Ok(TargetDesc::new(TRIPLE, cfg, Alloc::global()))
}

/// The records of the `Fetch` jobs the task was handed, mapped from `<home>/.blaze/v2/sources/` once a
/// command and read in place (never read whole).
struct Mapped<'s, 'f> {
    /// The subjects of the handed `Fetch` jobs that hold their sources, sorted.
    handed: &'s [ContentId],
    mapped: &'s mut Vec<(ContentId, &'f [u8]), &'f Frame>,
    /// The records' folder as the machine names it (`fetch::sources_native`).
    folder: &'s str,
    frame: &'f Frame,
    /// The first record that could not be mapped: the store's fault.
    error: &'s mut Option<std::io::Error>,
}

impl<'f> Records<'f> for Mapped<'_, 'f> {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'f [u8]> {
        let key = source.key();
        self.handed.binary_search(&key).ok()?;
        let at = match self.mapped.binary_search_by(|(held, _)| held.cmp(&key)) {
            Ok(at) => return Some(self.mapped[at].1),
            Err(at) => at,
        };
        let hex = crate::fetch::key_hex(&key);
        let mut path = std::string::String::with_capacity(self.folder.len() + 65);
        path.push_str(self.folder);
        path.push('/');
        path.extend(hex.iter().map(|digit| char::from(*digit)));
        match blaze_core::Mapping::map_file(std::path::Path::new(&path)) {
            Ok(mapping) => {
                // Kept for the command: the bytes are read in place by every later entry.
                let mapping: &'f blaze_core::Mapping = std::boxed::Box::leak(std::boxed::Box::new_in(mapping, self.frame));
                self.mapped.insert(at, (key, mapping.bytes()));
                Some(mapping.bytes())
            }
            Err(error) => {
                self.error.get_or_insert(error);
                None
            }
        }
    }
}

/// One entry of the `UnitGraph` task.
pub(crate) fn run<'f, 'b>(cx: &Cx, lockfile: &mut Lockfile<'f>, graph: &mut Graph<'f>, task: &Task<'_>, env: &FetchEnv<'_>, frame: &'f Frame, held: &'b Frame) -> Result<Did<'b>, CliError> {
    let _cx = cx.span("unit graph: one level of the dependencies");
    let name = task.name;
    // A `Fetch` record holds its sources when its out is its subject (fetch_task::made); one
    // refused for good says so with another out.
    let mut handed = Vec::new_in(held);
    let mut refused = Vec::new_in(held);
    for input in task.inputs.iter().filter(|input| input.name.kind == Kind::Fetch) {
        match input.out == input.name.subject {
            true => handed.push(input.name.subject),
            false => refused.push(input.name.subject),
        }
    }
    handed.sort_unstable();
    refused.sort_unstable();
    let mut failed = Vec::new_in(held);
    failed.extend(task.failed.iter().filter(|need| need.kind == Kind::Fetch).map(|need| need.subject));
    failed.sort_unstable();
    // The machine's sources store beneath the home (fetch.rs `SOURCES`), where every `Fetch`
    // task wrote its unit's record: never the checkout.
    let Some(folder) = crate::fetch::sources_native(env, held) else {
        return Err(CliError::Text { what: "HOME is not set: the sources store (fetch.rs `SOURCES`) is beneath it" });
    };
    let mut unmapped = None;
    let records = Mapped { handed: &handed, mapped: &mut graph.mapped, folder: folder.as_str(), frame, error: &mut unmapped };
    let unheld = Unheld { failed: &failed, refused: &refused };
    let entry = unit_graph_task::run(&graph.inputs, lockfile.lock, records, PackView::open_source_put, unheld, Alloc::global());
    if let Some(error) = unmapped {
        let words = bstd::mem::format_in!(held, "a fetched source record could not be mapped: {error}");
        return Ok(Did::Hole(
            TaskFailure { name, why: Why::Refused { what: "the dependency graph: a source record could not be mapped (the store's fault); the next command asks again", rule: "graph.rs" } },
            held_str(words.as_str(), held),
        ));
    }
    match entry {
        Ok(Entry::Need(subjects)) => {
            lockfile.want_ahead(&subjects, env);
            let mut wants = Vec::with_capacity_in(subjects.len(), held);
            for subject in subjects.iter() {
                let fetch = Name::new(Kind::Fetch, *subject);
                wants.push(Want { name: fetch, home: fetch });
            }
            Ok(Did::Need(wants))
        }
        Ok(Entry::NotFetched { count }) => {
            let words = bstd::mem::format_in!(held, "the dependency graph is not kept: {count} of the packages it reaches were not fetched this command; the next command asks again");
            Ok(Did::Hole(TaskFailure { name, why: Why::Refused { what: "the dependency graph: sources not fetched", rule: "unit_graph_task" } }, held_str(words.as_str(), held)))
        }
        Ok(Entry::Made(made)) => {
            let mut record = Made::new_in(name, held);
            record.out = ContentId::of(GRAPH_OUT_DOMAIN, &made.record);
            record.payload.extend_from_slice(&made.record);
            // One `MacroModule` part per proc-macro crate (macros.rs): the module lookups.
            match crate::macros::parts(graph.source, &made.record) {
                Ok(parts) => record.parts.extend(parts.iter().map(|(name, source)| Part { name: *name, locate: Locate::Source(*source), place: 0 })),
                Err(refused) => {
                    let words = bstd::mem::format_in!(held, "the dependency graph's own record does not decode at byte {}: {}", refused.at, refused.why);
                    return Ok(Did::FailedSaying(TaskFailure { name, why: Why::Refused { what: "the dependency graph's record", rule: "graph.rs" } }, held_str(words.as_str(), held)));
                }
            }
            // The `Script` parts are named with the `CrateScope` parts, a crate of the world each
            // (work.rs, the `UnitGraph` arm; ruling D115 (2)).
            for finding in made.findings.iter() {
                let words = bstd::mem::format_in!(held, "{finding}");
                record.find(crate::fetch::SOURCE_REFUSED, (0, 0), &[words.as_str()]);
            }
            Ok(Did::Done(record))
        }
        Err(error) => {
            let words = bstd::mem::format_in!(held, "the dependency graph: {error}");
            Ok(Did::FailedSaying(TaskFailure { name, why: Why::Refused { what: "the dependency graph could not be resolved", rule: "unit_graph_task" } }, held_str(words.as_str(), held)))
        }
    }
}
