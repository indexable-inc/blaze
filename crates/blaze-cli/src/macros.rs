//! The `MacroModule` task: whether the module store holds a proc-macro crate's compiled
//! module (docs/v2/plan/design/proc-macros.md). A lookup only: the module is never compiled
//! here and nothing is run; the sandbox runtime is not needed for it.
//!
//! The store is the user's shared store, `<home>/.blaze/v2/proc-macro/modules/<key>/`: a
//! module is keyed by its crate's identity alone (the lock's pin of its sources, its dependency
//! closure, its features, the toolchain), the same for every project, so it lives beside the
//! toolchain shelf, never under a project. The home is the passed value main.rs reads at the
//! edge. The bootstrap tool (`toolrun.sh proc_macro_modules`) fills it; later CI's cache does.
//!
//! The `UnitGraph` job names one `MacroModule` part per proc-macro unit ([`parts`]); its
//! source is the graph's source with the crate's name and version, so a lock or member edit
//! asks again and nothing else does. Present: a record whose out is the module's wasm digest.
//! Absent: one filed failure naming the crate and the bootstrap command; the store's listing is
//! in the part's key ([`MODULE_LISTING`]), so the next command asks again once the store moves.

use std::alloc::Global;

use blaze_drive::{Input, Kind, Name, Task, TaskFailure, Want, Why};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_manifest::lock::LockId;
use blaze_proc_macro::module::{LockedPackages, ModuleError, find_module};
use blaze_unit_graph::{Unit, UnitKind};
use blaze_world::procs::{MacroIdentity, Unidentified, git_folders, identify};
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::store::Store;
use crate::work::{Did, Made, held_str};

const SUBJECT_DOMAIN: &str = "blaze-cli macro crate 1";
const SOURCE_DOMAIN: &str = "blaze-cli macro crate source 1";
const OUT_DOMAIN: &str = "blaze-cli macro module out 1";
/// The rule a refusal of this task names.
const RULE: &str = "macros.rs";
/// The module store's folder under blaze's folder in the home (`<home>/.blaze`): its listing
/// (one folder a module, named by the module's key, renamed into place whole by the bootstrap
/// tool) is a `HomeListing` row of the leaf table, read with the layout (main.rs). It is in the
/// units' source, so in the dependency graph's source and in each `MacroModule` part's: a part's
/// key covers which modules exist, and a module the store lacks is a filed failure, asked again
/// when the listing moves and never before.
pub(crate) const MODULE_LISTING: &str = "v2/proc-macro/modules";

/// The macro crates of this command's graph, each with its module identity or why it has
/// none: made once a command, at the first `MacroModule` task that runs.
pub type MacroCrates = std::vec::Vec<(ContentId, Result<MacroIdentity, Unidentified>), Global>;

/// Where a command's macro crates are: the home (the module store is under it), the checkout
/// relative to where blaze was started ("" for there: `FetchEnv::checkout`), each workspace
/// member's lock entry and folder, relative to the checkout (`names::member_dirs`; `None` when
/// the members were not matched to the lock this command), and the command's leaf rows, which
/// hold each member macro crate's files digest (macro_files.rs).
pub(crate) struct Place<'p, 'r> {
    pub home: Option<&'p str>,
    pub checkout: &'p str,
    pub members: Option<&'p [crate::graph::CheckoutDir]>,
    pub rows: &'p crate::sweep::Rows<'r>,
}

impl Place<'_, '_> {
    /// The files digest of the checkout path package `id` (hex, `blaze_proc_macro::module::files_digest`)
    /// as its `MacroFiles` row holds it: what a path macro crate's module key holds, by
    /// package-relative paths and bytes (rule 25), and what the lookup's own key holds through
    /// the units' source (rule 21); `None` for a package with no checkout folder or no row
    /// read (then `PathWithoutFolder`, by name).
    pub(crate) fn member_files(&self, id: LockId) -> Option<std::string::String> {
        let entry = self.members?.iter().find(|entry| entry.id == id)?;
        crate::macro_files::member_hex(self.rows, entry.dir.as_str())
    }

    /// The checkout as the bootstrap tool's `checkout` input takes it.
    fn checkout_shown(&self) -> &str {
        if self.checkout.is_empty() { "." } else { self.checkout }
    }
}

/// A macro crate's subject: its name and version.
pub(crate) fn subject(unit: &Unit<'_>) -> ContentId {
    let mut text = std::vec::Vec::new_in(Global);
    text.extend_from_slice(unit.name.as_str().as_bytes());
    text.push(b' ');
    text.extend_from_slice(unit.version.as_str().as_bytes());
    ContentId::of(SUBJECT_DOMAIN, &text)
}

/// The `MacroModule` parts of a graph record `graph_record` made over the graph source
/// `graph_source`: one per proc-macro unit, by subject, sorted and without repeats. The decoded
/// graph is scratch, made over `scratch` and dropped here.
pub(crate) fn parts(graph_source: ContentId, graph_record: &[u8], scratch: Alloc<'_>) -> Result<std::vec::Vec<(Name, ContentId), Global>, blaze_unit_graph::record::RecordRefused> {
    let decoded = blaze_unit_graph::record::decode(graph_record, scratch)?;
    let made = parts_of(graph_source, &decoded.graph, scratch);
    let mut parts = std::vec::Vec::with_capacity_in(made.len(), Global);
    parts.extend_from_slice(&made);
    Ok(parts)
}

/// The `MacroModule` parts of the decoded graph `graph` over the graph source `graph_source`
/// ([`parts`]'s one maker): for a reader that already holds the decoded graph (engine_units.rs
/// discover, over the names world's), so the graph section is decoded once a command.
pub(crate) fn parts_of<A: core::alloc::Allocator>(graph_source: ContentId, graph: &blaze_unit_graph::UnitGraph<'_>, alloc: A) -> blaze_own::Vec<(Name, ContentId), A> {
    parts_of_subjects(graph_source, macro_subjects(graph), alloc)
}

/// Each proc-macro unit's subject ([`subject`]), in the graph's order (repeats kept).
pub(crate) fn macro_subjects<'g>(graph: &'g blaze_unit_graph::UnitGraph<'_>) -> impl Iterator<Item = ContentId> + 'g {
    graph.units.iter().filter(|unit| matches!(unit.kind, UnitKind::ProcMacro)).map(subject)
}

/// The `MacroModule` parts of the proc-macro crates `subjects` over the graph source
/// `graph_source`: [`parts_of`] without the graph, for a reader of the crate list's macro table
/// (crate_list.rs version 6, engine_units.rs `read_back`: no graph decoded under a standing
/// graph). Sorted by subject, each once.
pub(crate) fn parts_of_subjects<A: core::alloc::Allocator>(graph_source: ContentId, subjects: impl Iterator<Item = ContentId>, alloc: A) -> blaze_own::Vec<(Name, ContentId), A> {
    let mut parts = blaze_own::Vec::new_in(alloc);
    for subject in subjects {
        let mut source = [0u8; 64];
        source[..32].copy_from_slice(&graph_source.0);
        source[32..].copy_from_slice(&subject.0);
        parts.push((Name::new(Kind::MacroModule, subject), ContentId::of(SOURCE_DOMAIN, &source)));
    }
    parts.sort_unstable_by_key(|(name, _)| name.subject);
    parts.dedup_by_key(|(name, _)| name.subject);
    parts
}

/// Why the macro crates of a command could not be listed.
#[derive(Debug)]
pub enum MacrosError {
    /// The graph record the task was handed does not decode.
    Graph(blaze_unit_graph::record::RecordRefused),
    /// No lock was read this command (a graph needs one, so the layout is the tool's fault).
    NoLock,
    /// The module store could not be listed, or a path crate's files read.
    Procs(blaze_world::procs::ProcsError),
}

impl core::fmt::Display for MacrosError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MacrosError::Graph(refused) => write!(f, "the unit graph record at byte {}: {}", refused.at, refused.why),
            MacrosError::NoLock => f.write_str("no lock was read this command"),
            MacrosError::Procs(error) => write!(f, "{error}"),
        }
    }
}

/// Every macro crate of the graph `graph_record` with its identity, over `lock`, the module
/// store `modules` (its git folders) and `place` (a member's folder). The decoded graph is the
/// task's scratch, made over `scratch`.
fn list(graph_record: &[u8], lock: &crate::fetch::Lockfile<'_>, modules: &std::path::Path, place: &Place<'_, '_>, scratch: Alloc<'_>) -> Result<MacroCrates, MacrosError> {
    let decoded = blaze_unit_graph::record::decode(graph_record, scratch).map_err(MacrosError::Graph)?;
    let locked = LockedPackages::of(lock.lock);
    let folders = git_folders(modules).map_err(MacrosError::Procs)?;
    let mut crates = std::vec::Vec::new_in(Global);
    for unit in decoded.graph.units.iter().filter(|unit| matches!(unit.kind, UnitKind::ProcMacro)) {
        // A member macro crate's identity is its files' digest, read from its folder; a path
        // package that is no member has no folder here and is named as not identified.
        let identity = identify(lock.lock, &locked, &folders, unit, |id| Ok(place.member_files(id))).map_err(MacrosError::Procs)?;
        crates.push((subject(unit), identity));
    }
    crates.sort_unstable_by_key(|(subject, _)| *subject);
    Ok(crates)
}

/// One `MacroModule` task: asks for the graph's record, then looks its crate's module up.
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn run<'b>(
    cx: &Cx,
    store: &mut dyn Store<'_>,
    lock: Option<&crate::fetch::Lockfile<'_>>,
    crates: &mut Option<MacroCrates>,
    place: Place<'_, '_>,
    task: &Task<'_>,
    frame: &Frame,
    held: &'b Frame,
) -> Result<Did<'b>, CliError> {
    let cx = cx.span("macro module: look the module up");
    let name = task.name;
    let refused = |what: &'static str| TaskFailure { name, why: Why::Refused { what, rule: RULE } };
    let Some(home) = place.home else {
        return Ok(Did::Hole(refused("no home: the module store is under it"), "the proc-macro module store is under the home, and HOME is not set"));
    };
    let modules = std::path::Path::new(home).join(".blaze/v2");
    if crates.is_none() {
        let Some(graph) = task.inputs.iter().find(|input: &&Input| input.name == Name::unit_graph()) else {
            let mut wants = blaze_own::Vec::with_capacity_in(1, held);
            wants.push(Want { name: Name::unit_graph(), home: Name::unit_graph() });
            return Ok(Did::Need(wants));
        };
        let read = crate::work::payload(&cx, store, graph, frame)?;
        // The task's frame carries no header: erased here, for this task's scratch.
        let scratch = blaze_own::Erased::new(frame);
        let listed = match lock {
            Some(lock) => list(read.unit_graph(graph.name)?.graph, lock, &modules, &place, scratch.handle()),
            None => Err(MacrosError::NoLock),
        };
        match listed {
            Ok(listed) => *crates = Some(listed),
            Err(error) => {
                let words = bstd::mem::format_in!(held, "the proc-macro crates of the graph could not be listed: {error}");
                return Ok(Did::Hole(refused("the macro crates could not be listed"), held_str(words.as_str(), held)));
            }
        }
    }
    let crates = crates.as_ref().expect("listed above");
    let Ok(at) = crates.binary_search_by_key(&name.subject, |(subject, _)| *subject) else {
        return Ok(Did::FailedSaying(refused("a macro crate the graph does not name"), "a `MacroModule` part names a crate this command's graph does not hold"));
    };
    let identity = match &crates[at].1 {
        Ok(identity) => identity,
        Err(why) => {
            let words = bstd::mem::format_in!(held, "proc-macro module not looked up: {why}");
            return Ok(Did::FailedSaying(refused("a macro crate without a module identity"), held_str(words.as_str(), held)));
        }
    };
    match identity.with_id(|id| find_module(&modules, id)) {
        Ok((_key, wasm_sha256)) => {
            let mut made = Made::new_in(name, held);
            made.out = ContentId::of(OUT_DOMAIN, wasm_sha256.as_bytes());
            Ok(Did::Done(made))
        }
        Err(ModuleError::Missing { name: package, version, key }) => {
            let features = identity.with_id(|id| match id.features.is_empty() {
                true => std::string::String::new(),
                false => std::format!("\"{}\"", id.features.join("\", \"")),
            });
            let words = bstd::mem::format_in!(
                held,
                "{package} {version}: no compiled proc-macro module in {} (key {key}); build it with `toolrun.sh proc_macro_modules` (input: {{\"checkout\": \"{}\", \"store\": \"{}\", \"crates\": [{{\"name\": \"{package}\", \"version\": \"{version}\", \"features\": [{features}]}}]}})",
                modules.join("proc-macro/modules").display(),
                place.checkout_shown(),
                modules.display(),
            );
            // Filed: the store's listing is in this part's key ([`MODULE_LISTING`]), and a
            // member's files are too (its `MacroFiles` row), so the failure stands until a
            // module comes or goes, or the member's files move.
            Ok(Did::FailedSaying(refused("a proc-macro module the store lacks"), held_str(words.as_str(), held)))
        }
        Err(error) => {
            let words = bstd::mem::format_in!(held, "the proc-macro module store: {error}");
            Ok(Did::Hole(refused("the module store could not be read"), held_str(words.as_str(), held)))
        }
    }
}
