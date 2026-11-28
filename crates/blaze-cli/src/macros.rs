//! The `MacroModule` task: resolves and self-compiles a proc-macro crate's `wasm64` (`memory64`)
//! module (docs/v2/plan/design/proc-macros.md).
//!
//! The store is the user's shared store, `<home>/.blaze/v2/proc-macro/modules/<key>/`: a
//! module is keyed by its crate's identity alone (the lock's pin of its sources, its dependency
//! closure including path-dependency file digests, its features, and the toolchain), the same for
//! every project, so it lives beside the toolchain shelf, never under a project. On a cache miss,
//! `wasm64_compile::compile_proc_macro` self-compiles the proc-macro crate via
//! `blaze-lir -> wasm64-blaze-wasip1` (zero `rustc`) and atomically publishes the record before
//! `Stage::Names` invokes `ProcRunner`.
//!
//! The `UnitGraph` job names one `MacroModule` part per proc-macro unit ([`parts`]); its
//! source is the graph's source with the crate's name and version, so a lock or member edit
//! asks again and nothing else does. Present (or freshly self-compiled): a record whose out is
//! the module's wasm digest. Failed: a typed failure naming the crate and cause; the store's
//! listing is in the part's key ([`MODULE_LISTING`]), so the next command asks again once the
//! store moves.

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
// "2": proc-macro module identity bumped to form 5 (`blaze proc-macro module 5`), invalidating
// cached `MacroModule` task records that pointed at pre-v5 proc-macro modules.
// "3": proc-macro module identity bumped to form 6 (`blaze proc-macro module 6`) after
// post-expansion proc-macro `_DECLS` and `macros` export synthesis (#2746), invalidating
// cached `MacroModule` task records that pointed at pre-v6 proc-macro modules.
// "4": proc-macro module identity bumped to form 7 (`blaze proc-macro module 7`) after
// `blaze-build` `bind_let_place` peeled-reference deref fix, invalidating cached
// `MacroModule` task records that pointed at stale form-6 proc-macro modules.
// "5": proc-macro module identity bumped to form 8 (`blaze proc-macro module 8`:
// wasm64-blaze-wasip1 target-specific dependency resolution), invalidating cached
// `MacroModule` task records that pointed at pre-v8 proc-macro modules.
// "6": `MacroModule` source mixes in per-closure `macro_files::closure_digest`
// (`part_source_with_files`) so patched path deps reached by registry/git proc-macros
// invalidate only affected `MacroModule` tasks without polluting `units_source` with unrelated
// runtime patches.
// "7": proc-macro module identity bumped to form 9 (`blaze proc-macro module wasm64 9`) after
// `blaze-build` `ScopeEnd`/`TempScopeEnd` adjusted-repr fix, invalidating cached `MacroModule`
// task records that pointed at pre-v9 wasm64 proc-macro modules.
// "8": proc-macro module identity bumped to form 10 (`blaze proc-macro module wasm64 10`) after
// adding target-level `getrandom_backend="custom"` for `wasm64-blaze-wasip1` (Rule 21),
// invalidating cached `MacroModule` task records that pointed at pre-v10 wasm64 modules.
// "9": proc-macro module identity bumped to form 11 (`blaze proc-macro module wasm64 11`) after
// registering `blaze_wasm64_pm_shim` in `export_override_crates` so `libc::getenv` binds to
// `blaze_wasm64_pm_shim::getenv` -> `blaze_proc_macro_64::env_var` (Rule 21), invalidating
// cached `MacroModule` task records that pointed at stale form-10 wasm64 proc-macro modules.
const SOURCE_DOMAIN: &str = "blaze-cli macro crate source 9";
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

/// One resolved proc-macro crate identity plus the frozen dependency-probe identity digest
/// folded into it (when its `wasm64-blaze-wasip1` closure runs dependency build scripts), so
/// on-miss self-compilation can revalidate the live answerer before publishing.
pub struct ListedMacro {
    pub(crate) identity: MacroIdentity,
    pub(crate) dep_probes: Option<crate::wasm64_compile::Hex64>,
}

/// Per-macro identity or probe-setup refusal kept on that crate alone so one dependent
/// proc-macro's probe failure never aborts unrelated proc-macros in `macros::list` (Rule 20).
pub enum MacroListFailure {
    Unidentified(Unidentified),
    Probes(crate::scripts::ProbeSetupError),
}

/// The macro crates of this command's graph, each with its module identity or why it has
/// none: made once a command, at the first `MacroModule` task that runs.
pub type MacroCrates = std::vec::Vec<(ContentId, Result<ListedMacro, MacroListFailure>), Global>;

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
pub(crate) fn subject_of(name: &str, version: &str) -> ContentId {
    let mut text = std::vec::Vec::new_in(Global);
    text.extend_from_slice(name.as_bytes());
    text.push(b' ');
    text.extend_from_slice(version.as_bytes());
    ContentId::of(SUBJECT_DOMAIN, &text)
}

/// A macro crate's subject: its name and version.
pub(crate) fn subject(unit: &Unit<'_>) -> ContentId {
    subject_of(unit.name.as_str(), unit.version.as_str())
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

/// The `MacroModule` task source for `subject` over `graph_source` and `macro_files_digest`
/// (`macro_files::rows_digest`): covers both the unit graph and the post-graph `MacroFiles`
/// rows of `[patch.*]` path deps reached by actual proc-macro crates (rules 21 and 60).
pub(crate) fn part_source_with_files(graph_source: ContentId, macro_files_digest: ContentId, subject: ContentId) -> ContentId {
    let mut source = [0u8; 96];
    source[..32].copy_from_slice(&graph_source.0);
    source[32..64].copy_from_slice(&macro_files_digest.0);
    source[64..].copy_from_slice(&subject.0);
    ContentId::of(SOURCE_DOMAIN, &source)
}

/// Mixes the dependency build-script probe/pin digest (`probes_hex`) into a proc-macro
/// `MacroModule` task source so derived-fact pin or live C-probe answerer changes invalidate
/// the `MacroModule` task itself (rules 21 and 60).
pub(crate) fn part_source_with_dep_probes(source: ContentId, probes_hex: &str) -> ContentId {
    let mut buf = [0u8; 97];
    if let Some(dst) = buf.get_mut(..32) {
        dst.copy_from_slice(&source.0);
    }
    if let Some(tag) = buf.get_mut(32) {
        *tag = b'P';
    }
    let bytes = probes_hex.as_bytes();
    let take = bytes.len().min(64);
    if let (Some(dst), Some(src)) = (buf.get_mut(33..33 + take), bytes.get(..take)) {
        dst.copy_from_slice(src);
    }
    ContentId::of(SOURCE_DOMAIN, &buf)
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
    /// The command's derived-fact pin setting for dependency build scripts could not be opened.
    Probes(crate::scripts::ProbeSetupError),
}

impl core::fmt::Display for MacrosError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MacrosError::Graph(refused) => write!(f, "the unit graph record at byte {}: {}", refused.at, refused.why),
            MacrosError::NoLock => f.write_str("no lock was read this command"),
            MacrosError::Procs(error) => write!(f, "{error}"),
            MacrosError::Probes(error) => write!(f, "{error}"),
        }
    }
}

/// Every macro crate of the graph `sections` with its identity, over `lock`, the module
/// store `modules` (its git folders) and `place` (a member's folder). When `sections.wasm_graph`
/// is present, folds each proc-macro's resolved `wasm64-blaze-wasip1` dependency closure features
/// into its module identity (rule 21), and when its closure runs dependency build scripts folds
/// `probes`'s deterministic probe/pin identity digest as well. The decoded graph is the task's
/// scratch, made over `scratch`.
pub(crate) fn list(
    cx: &Cx,
    sections: crate::graph_payload::Sections<'_>,
    lock: &crate::fetch::Lockfile<'_>,
    modules: &std::path::Path,
    place: &Place<'_, '_>,
    probes: Option<crate::scripts::CommandProbes<'_>>,
    planned_probes: Option<&[(blaze_unit_graph::UnitId, crate::engine_scripts::PlannedProbe)]>,
    scratch: Alloc<'_>,
) -> Result<MacroCrates, MacrosError> {
    let decoded = blaze_unit_graph::record::decode(sections.graph, scratch).map_err(MacrosError::Graph)?;
    let wasm_decoded = if sections.wasm_graph.is_empty() {
        None
    } else {
        Some(blaze_unit_graph::record::decode(sections.wasm_graph, scratch).map_err(MacrosError::Graph)?)
    };
    let wasm_graph = wasm_decoded.as_ref().map(|d| &d.graph);
    let locked = LockedPackages::of(lock.lock);
    let folders = git_folders(modules).map_err(MacrosError::Procs)?;
    let mut crates = std::vec::Vec::new_in(Global);
    for (idx, unit) in decoded.graph.units.iter().enumerate() {
        if !matches!(unit.kind, UnitKind::ProcMacro) {
            continue;
        }
        let subj = subject(unit);
        // A member macro crate's identity is its files' digest, read from its folder; a path
        // package that is no member has no folder here and is named as not identified.
        let identity = identify(lock.lock, &locked, &folders, &decoded.graph, unit, |id| Ok(place.member_files(id)), scratch).map_err(MacrosError::Procs)?;
        let identity = match (wasm_graph, identity) {
            (Some(wg), Ok(id)) => Ok(id.with_wasm_features(lock.lock, wg, unit.package, |dep_id| place.member_files(dep_id), scratch)),
            (_, other) => other,
        };
        let planned_entry = u32::try_from(idx).ok().and_then(|uid| {
            let table = planned_probes?;
            let at = table.binary_search_by_key(&blaze_unit_graph::UnitId(uid), |(u, _)| *u).ok()?;
            table.get(at).map(|(_, entry)| entry)
        });
        let listed = match (identity, u32::try_from(idx).ok(), probes) {
            (Ok(id), Some(uid), Some(probes)) => {
                match crate::wasm64_compile::unit_has_dep_build_scripts(
                    &decoded.graph,
                    wasm_graph,
                    blaze_unit_graph::UnitId(uid),
                    true,
                    probes.shared(),
                ) {
                    Ok(true) => match planned_entry {
                        Some(crate::engine_scripts::PlannedProbe::Failed(err)) => {
                            Err(MacroListFailure::Probes(err.duplicate()))
                        }
                        Some(crate::engine_scripts::PlannedProbe::Digest(planned_hex)) => {
                            match probes.dep_probe_identity_hex(cx) {
                                Ok(live_hex) if live_hex == *planned_hex => Ok(ListedMacro {
                                    identity: id.with_dep_probes(planned_hex.as_str()),
                                    dep_probes: Some(*planned_hex),
                                }),
                                Ok(live_hex) => Err(MacroListFailure::Probes(
                                    crate::scripts::ProbeSetupError::from_str(
                                        crate::scripts::ProbeErrorKind::PlannedDrift,
                                        bstd::mem::format_in!(
                                            cx.frame(),
                                            "live C-probe or derived-fact pin identity changed after planning {} (planned {}, now {}); refusing to file drifted result under planned task key",
                                            unit.name.as_str(),
                                            planned_hex.as_str(),
                                            live_hex.as_str()
                                        )
                                        .as_str(),
                                        probes.shared(),
                                    ),
                                )),
                                Err(err) => Err(MacroListFailure::Probes(err)),
                            }
                        }
                        None if planned_probes.is_some() => Err(MacroListFailure::Probes(
                            crate::scripts::ProbeSetupError::from_str(
                                crate::scripts::ProbeErrorKind::PlannedDrift,
                                "proc-macro has dependency build scripts at lookup time without a planned probe identity",
                                probes.shared(),
                            ),
                        )),
                        None => match probes.dep_probe_identity_hex(cx) {
                            Ok(hex) => Ok(ListedMacro {
                                identity: id.with_dep_probes(hex.as_str()),
                                dep_probes: Some(hex),
                            }),
                            Err(err) => Err(MacroListFailure::Probes(err)),
                        },
                    },
                    Ok(false) => match planned_entry {
                        Some(crate::engine_scripts::PlannedProbe::Failed(err)) => {
                            Err(MacroListFailure::Probes(err.duplicate()))
                        }
                        Some(crate::engine_scripts::PlannedProbe::Digest(_)) => Err(MacroListFailure::Probes(
                            crate::scripts::ProbeSetupError::from_str(
                                crate::scripts::ProbeErrorKind::PlannedDrift,
                                "proc-macro was planned with a dependency-probe identity but has no dependency build scripts at lookup time",
                                probes.shared(),
                            ),
                        )),
                        None => Ok(ListedMacro { identity: id, dep_probes: None }),
                    },
                    Err(err) => Err(MacroListFailure::Probes(err)),
                }
            }
            (Ok(id), _, _) => Ok(ListedMacro { identity: id, dep_probes: None }),
            (Err(why), _, _) => Err(MacroListFailure::Unidentified(why)),
        };
        crates.push((subj, listed));
    }
    // Sort deterministically by `(subject, rank)` so duplicate proc-macro subjects never hide a
    // probe error or probe-dependent entry behind an unrelated feature-set duplicate when
    // `macros::run` looks up `name.subject` via `partition_point`.
    crates.sort_by_key(|(subject, res)| {
        let rank: u8 = match res {
            Err(MacroListFailure::Probes(_)) => 0,
            Ok(listed) if listed.dep_probes.is_some() => 1,
            Ok(_) => 2,
            Err(MacroListFailure::Unidentified(_)) => 3,
        };
        (*subject, rank)
    });
    Ok(crates)
}

/// One `MacroModule` task: asks for the graph's record, then looks its crate's module up.
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn run<'b>(
    cx: &Cx,
    store: &mut dyn Store<'_>,
    lock: Option<&crate::fetch::Lockfile<'_>>,
    crates: &mut Option<std::sync::Arc<MacroCrates>>,
    place: Place<'_, '_>,
    compile: Option<crate::wasm64_compile::CompileInputs<'_>>,
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
            Some(lock) => list(&cx, read.unit_graph(graph.name)?, lock, &modules, &place, compile.and_then(|c| c.probes), None, scratch.handle()),
            None => Err(MacrosError::NoLock),
        };
        match listed {
            Ok(listed) => *crates = Some(std::sync::Arc::new(listed)),
            Err(error) => {
                let words = bstd::mem::format_in!(held, "the proc-macro crates of the graph could not be listed: {error}");
                return Ok(Did::Hole(refused("the macro crates could not be listed"), held_str(words.as_str(), held)));
            }
        }
    }
    let crates = crates.as_ref().expect("listed above");
    let at = crates.partition_point(|(subject, _)| *subject < name.subject);
    let Some(entry) = crates.get(at).filter(|(subject, _)| *subject == name.subject) else {
        return Ok(Did::FailedSaying(refused("a macro crate the graph does not name"), "a `MacroModule` part names a crate this command's graph does not hold"));
    };
    let listed = match &entry.1 {
        Ok(listed) => listed,
        Err(MacroListFailure::Unidentified(why)) => {
            let words = bstd::mem::format_in!(held, "proc-macro module not looked up: {why}");
            return Ok(Did::FailedSaying(refused("a macro crate without a module identity"), held_str(words.as_str(), held)));
        }
        Err(MacroListFailure::Probes(why)) => {
            let words = bstd::mem::format_in!(held, "proc-macro dependency probe identity refused: {why}");
            return Ok(Did::Hole(refused("the proc-macro's dependency probe identity could not be verified"), held_str(words.as_str(), held)));
        }
    };
    let identity = &listed.identity;
    match identity.with_id(|id| find_module(&modules, id)) {
        Ok((_key, wasm_sha256)) => {
            let mut made = Made::new_in(name, held);
            made.out = ContentId::of(OUT_DOMAIN, wasm_sha256.as_bytes());
            if let Some(dep_probes) = listed.dep_probes {
                made.payload.extend_from_slice(dep_probes.as_str().as_bytes());
            }
            Ok(Did::Done(made))
        }
        Err(ModuleError::Missing { name: package, version, key }) => {
            if let Some(inputs) = compile {
                let _compile_cx = cx.span("macro module: self-compile proc-macro to wasm64");
                match crate::wasm64_compile::compile_proc_macro(&cx, &modules, inputs, identity, listed.dep_probes) {
                    Ok(wasm_sha256) => {
                        let mut made = Made::new_in(name, held);
                        made.out = ContentId::of(OUT_DOMAIN, wasm_sha256.as_bytes());
                        if let Some(dep_probes) = listed.dep_probes {
                            made.payload.extend_from_slice(dep_probes.as_str().as_bytes());
                        }
                        return Ok(Did::Done(made));
                    }
                    Err(why) => {
                        let words = bstd::mem::format_in!(
                            held,
                            "{package} {version}: wasm64 proc-macro self-compilation failed (key {key}): {why}"
                        );
                        if why.is_hole() {
                            return Ok(Did::Hole(
                                refused("the wasm64 sysroot/module store could not be read"),
                                held_str(words.as_str(), held),
                            ));
                        }
                        let what = if why.is_tier2() {
                            "proc-macro requires Tier-2 native/host features unsupported in wasm64"
                        } else {
                            "proc-macro wasm64 self-compilation failed"
                        };
                        return Ok(Did::FailedSaying(
                            refused(what),
                            held_str(words.as_str(), held),
                        ));
                    }
                }
            }
            let words = bstd::mem::format_in!(
                held,
                "{package} {version}: no compiled proc-macro module in {} (key {key}) and no on-disk checkout ({}) was available for wasm64 self-compilation (no rustc fallback, rules 1 & 3)",
                modules.join("proc-macro/modules").display(),
                place.checkout_shown(),
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
