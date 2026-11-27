//! The units task (drive.md, "Units"): the workspace's layout, read once per request by
//! blaze-manifest's sans-IO `Layout` ([`lay`]) when what it read last moved (or on a cold run, or
//! when a units record must be made), each manifest it asks for and each directory it lists a
//! row of the leaf table (sweep.rs). The root's units list each member's manifest as a part at
//! `Locate::Source`, the digest of that member's share (its parts and findings), so an edit of
//! one member's manifest re-makes that member's units and no other's; a member's units list its
//! crate roots (`Locate::File` in the package's edition, each with its path) and the units of
//! each workspace member it depends on (normal and build dependencies, at the root's
//! `Locate::Source` for that member: `run`'s closure follows them, blaze-drive run_scope.rs),
//! and each dependency not built yet is a finding on its key. A manifest the layout refuses is a finding
//! on the line it names, never a refusal of the whole workspace.

use blaze_drive::{Locate, Name, Part, Task};
use blaze_ids::ContentId;
use blaze_manifest::layout::{self, Entry, Layout, LayoutError, Need, Pass, Source, Span};
use blaze_own::{Alloc, Texts, Vec};
use blaze_surface::Edition;
use blaze_unit_graph::{StdIs, UnitKind, implicit};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};

use crate::CliError;
use crate::sweep::{self, Rows, Stamps};
use crate::work::{Did, Made, parts_out};

/// The finding number of what the layout refuses in a manifest, on the line it names. Its one
/// argument is the refusal.
pub const UNREAD: u16 = 3;
/// The finding number of a blaze-edition manifest's dependency line naming the toolchain's
/// bstd (`bstd`, or the package `blaze-std`): blaze gives it with no line (toolchain.md,
/// section 4). Its range is the whole line with its newline, which its fix deletes; its one
/// argument is the name.
pub const TOOLCHAIN_NAMED: u16 = 9;
/// The finding number of a member's integration test, example or bench the layout refused
/// (blaze-manifest `Member::refused_targets`): on its table's line, the target left out and the
/// rest of the workspace checked. Its arguments: the kind's table name (`test`, `example`,
/// `bench`), the target's name, the package's name and the rule that refused it.
pub const TARGET_REFUSED: u16 = 15;
/// The finding number of an internal workspace member whose `std` is Rust's `std` while
/// `[workspace.metadata.blaze] std = "bstd"` requires bstd. Its arguments: the member's package
/// name and its current Rust edition (`2015`, `2018`, `2021`, `2024`).
pub const STD_NOT_BSTD: u16 = 49;
/// The workspace's lock, at its root.
const LOCK: &str = "Cargo.lock";

/// 5: includes the workspace `[workspace.metadata.blaze] std` policy flag (`require_bstd`) for
/// the member (rule 21); 4: a member's units list the units of each workspace member it depends
/// on (`member_units`).
const UNITS_OUT_DOMAIN: &str = "blaze-cli units out 5";
const MEMBER_SOURCE_DOMAIN: &str = "blaze-cli member source 1";
/// Passes the layout may ask for: the depth of its search (root, globs, path dependencies,
/// target directories), far under this on any workspace (rule 18).
const MOST_PASSES: usize = 64;

/// The layout's answer, as the units records it makes: the root's, and each member's by its
/// manifest's units name.
pub struct Laid<'f> {
    pub root: Made<'f>,
    pub members: Vec<(Name, Made<'f>), &'f Frame>,
    /// Each non-member checkout path package's crate root path (checkout-relative) and syntax
    /// edition byte, from `[patch.*]` and excluded/transitive path dependencies: used to outline
    /// reached non-member checkout crates as sweep leaves without checking them as members.
    pub path_roots: Vec<(&'f str, u8), &'f Frame>,
    /// The workspace's `Cargo.lock` planned (fetch.rs): `None` when there is none, or when it
    /// is refused (then a [`crate::fetch::LOCK_UNREAD`] finding on the root's units).
    pub lock: Option<crate::fetch::Lockfile<'f>>,
    /// The members matched to the lock (graph.rs): `None` with no lock, or when the match is
    /// refused (then a [`crate::fetch::LOCK_UNREAD`] finding on the root's units).
    pub parts: Option<crate::graph::Parts>,
    /// The `UnitGraph` job, made from `parts` once the units' source is known (main.rs).
    pub graph: Option<crate::graph::Graph<'f>>,
    /// The `rustflags` of the checkout's cargo config (graph.rs `rustflags_config`), each file a
    /// row the layout read: what `graph::target_of` gives the units' cfg.
    pub rustflags: blaze_manifest::rustflags::RustflagsConfig<'static>,
    /// Candidate declared dependencies of each workspace member, for unused-dependency detection
    /// (unused_deps.rs).
    pub(crate) member_deps: Vec<crate::unused_deps::MemberDeps<'f>, &'f Frame>,
}

impl Laid<'_> {
    /// Whether the command has a unit graph: the lock matched (`parts`), whether or not `main`
    /// made the graph's job from it yet. A layout read late (a task's, when nothing the layout
    /// read moved) has its `parts` and no `graph`: the graph's record is then found by the last
    /// answer's row, and its absence is no fact of the workspace (rule 26: the edited home and
    /// a fresh one give one answer).
    pub fn has_graph(&self) -> bool {
        self.graph.is_some() || self.parts.is_some()
    }

    /// The layout half of the standing graph proof. It includes every units record's output and
    /// name in layout order, so a removed member or a changed manifest cannot reuse an old plan
    /// directory under the same graph source.
    pub(crate) fn plan_source(&self) -> ContentId {
        let mut hash = blaze_ids::Hasher::new("blaze-cli standing plan layout 1");
        hash.update(&self.root.name.subject.0).update(&self.root.out.0);
        for (name, member) in self.members.iter() {
            hash.update(&name.subject.0).update(&member.out.0);
        }
        hash.finish()
    }
}

/// Read the workspace's layout from the checkout's root: every manifest and listing it asks for
/// is pushed onto `rows`.
pub fn lay<'f, 'l>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    rows: &mut Rows<'_>,
    // The toolchain the command opens, given by `main`: in the units' output (rule 75).
    toolchain: blaze_toolchain::ToolchainId,
    // The process's heap, handed from `main`: the lock and its plan are kept on it.
    heap: &'static blaze_own::Heap,
    frame: &'f Frame,
    alloc: Alloc<'l>,
) -> Result<Laid<'f>, CliError> {
    let cx = cx.span("units: the workspace's layout");
    // Each manifest answered, in the order answered (a `ManifestId` is its index): its path,
    // its text ("" when none is there) and whether one is.
    let mut paths: Texts<&Frame> = Texts::new_in(frame);
    let mut texts: Texts<&Frame> = Texts::new_in(frame);
    let mut present: Vec<bool, &Frame> = Vec::new_in(frame);
    let laid = (|| -> Result<Result<layout::Units<'l>, LayoutError<'l>>, CliError> {
        // The workspace's own members read their test, example and bench targets, and the
        // workspace's root `[patch.*]` and excluded/transitive path packages are discovered too;
        // a fetched dependency's layout never reads either.
        let mut layout = match Layout::new("", alloc) {
            Ok(layout) => layout.with_test_targets().with_path_packages(),
            Err(error) => return Ok(Err(error)),
        };
        for _ in 0..MOST_PASSES {
            let needs = match layout.pass() {
                Err(error) => return Ok(Err(error)),
                Ok(Pass::Done(units)) => return Ok(Ok(units)),
                Ok(Pass::Need(needs)) => needs,
            };
            for need in needs.iter() {
                match need {
                    Need::Manifest(path) => {
                        let path = path.as_str();
                        let mut text = Bytes::new_in(frame, "a manifest");
                        let found = sweep::read_manifest(&cx, root, stamps, path, rows, &mut text)?;
                        let text = match found {
                            true => Some(core::str::from_utf8(text.as_slice()).map_err(|_| CliError::Text { what: "a manifest is not UTF-8" })?),
                            false => None,
                        };
                        // A path asked again is answered once (the layout keeps the first).
                        if !(0..paths.len()).any(|i| paths.text(i) == path) {
                            paths.push(path);
                            texts.push(text.unwrap_or(""));
                            present.push(found);
                        }
                        if let Err(error) = layout.manifest(path, text) {
                            return Ok(Err(error));
                        }
                    }
                    Need::Listing(dir) => {
                        let dir = dir.as_str();
                        let listing = sweep::read_listing(&cx, root, stamps, dir, rows, frame)?;
                        let entries: Option<Vec<Entry<'_>, &Frame>> = listing.as_ref().map(|listing| {
                            let mut entries = Vec::with_capacity_in(listing.dirs.len(), frame);
                            entries.extend(listing.entries().map(|(name, is_dir)| Entry { name, is_dir }));
                            entries
                        });
                        layout.listing(dir, entries.as_deref());
                    }
                }
            }
        }
        Err(CliError::Text { what: "the workspace's layout asked for more than 64 passes" })
    })()?;
    let mut members = Vec::new_in(frame);
    let mut path_roots: Vec<(&'f str, u8), &'f Frame> = Vec::new_in(frame);
    let mut member_deps: Vec<crate::unused_deps::MemberDeps<'f>, &'f Frame> = Vec::new_in(frame);
    let mut root_made = Made::new_in(Name::units(), frame);
    let mut facts = None;
    match laid {
        Ok(units) => {
            member_deps = crate::unused_deps::extract_from_layout(&units, &paths, frame);
            let root_text = usize::try_from(units.root.0).ok().filter(|&id| id < texts.len()).map_or("", |id| texts.text(id));
            let std_policy = parse_std_policy(root_text, &mut root_made, frame);
            // With no dependency outside the workspace and the toolchain there is no graph:
            // the lock is not read (nothing in it can change the answer), so a stale one costs
            // nothing and a lock edit moves no key.
            // Then the members' own lock stands in (graph.rs `members_lock`): the graph and the
            // names world are the toolchain's crates and the members alone.
            let own = match crate::graph::reaches_outside(&units) {
                true => None,
                false => Some(crate::graph::members_lock(&units, frame)),
            };
            facts = Some((crate::graph::Members::of(&units, heap), own));
            // Outside dependencies are read by the unit-graph job (unit_graph_task), which
            // fetches what the roots reach: nothing is said of them here. A member's units list
            // the units of each member it depends on, at the source the root lists them by, so
            // they are made dependencies first ([`deps_first`]).
            let mut listed: Vec<Option<(Name, ContentId)>, &Frame> = Vec::with_capacity_in(units.members.len(), frame);
            listed.extend(units.members.iter().map(|_| None));
            let mut made_of: Vec<Option<Made<'f>>, &Frame> = Vec::with_capacity_in(units.members.len(), frame);
            made_of.extend(units.members.iter().map(|_| None));
            for k in deps_first(&units.members, frame).iter().copied() {
                let member = &units.members[k];
                let id = member.manifest.0 as usize;
                let require_bstd = std_policy.requires_bstd(member.name.as_str());
                let made = member_units(Name::manifest(paths.text(id).as_bytes()), member, texts.text(id), toolchain, require_bstd, (&units.members[..], &listed[..]), frame);
                listed[k] = Some((made.name, member_source(&made)));
                made_of[k] = Some(made);
            }
            // Collect all workspace members and path packages whose whole file tree enters a
            // proc-macro or build-script module key (rule 21): every proc-macro crate, every
            // transitive path dependency of a proc-macro crate, and every transitive path
            // build-dependency of a crate with a build script.
            let mut whole_dirs: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            let mut dep_stack: Vec<(bool, usize), &Frame> = Vec::new_in(frame);
            let mut seen_pkgs: std::collections::BTreeSet<(bool, usize)> = std::collections::BTreeSet::new();
            let push_pkg = |is_path: bool, idx: usize, seen: &mut std::collections::BTreeSet<(bool, usize)>, stack: &mut Vec<(bool, usize), &Frame>| {
                if seen.insert((is_path, idx)) {
                    stack.push((is_path, idx));
                }
            };
            let push_dep_target = |dep: &blaze_manifest::layout::Dependency<'_>, units: &layout::Units<'_>, seen: &mut std::collections::BTreeSet<(bool, usize)>, stack: &mut Vec<(bool, usize), &Frame>| {
                match &dep.source {
                    blaze_manifest::layout::Source::Member(idx) => push_pkg(false, *idx, seen, stack),
                    blaze_manifest::layout::Source::Path(dir) => {
                        if let Some(idx) = units.path_packages.iter().position(|p| p.dir.as_str() == dir.as_str()) {
                            push_pkg(true, idx, seen, stack);
                        } else if let Some(idx) = units.members.iter().position(|m| m.dir.as_str() == dir.as_str()) {
                            push_pkg(false, idx, seen, stack);
                        }
                    }
                    _ => {}
                }
            };
            for (idx, m) in units.members.iter().enumerate() {
                if m.lib.iter().any(|lib| lib.proc_macro) {
                    push_pkg(false, idx, &mut seen_pkgs, &mut dep_stack);
                }
                if m.build.is_some() {
                    for dep in m.dependencies.iter() {
                        if dep.kind == blaze_manifest::layout::DepKind::Build {
                            push_dep_target(dep, &units, &mut seen_pkgs, &mut dep_stack);
                        }
                    }
                }
            }
            for (idx, p) in units.path_packages.iter().enumerate() {
                if p.lib.iter().any(|lib| lib.proc_macro) {
                    push_pkg(true, idx, &mut seen_pkgs, &mut dep_stack);
                }
                if p.build.is_some() {
                    for dep in p.dependencies.iter() {
                        if dep.kind == blaze_manifest::layout::DepKind::Build {
                            push_dep_target(dep, &units, &mut seen_pkgs, &mut dep_stack);
                        }
                    }
                }
            }
            while let Some((is_path, idx)) = dep_stack.pop() {
                let pkg = if is_path { &units.path_packages[idx] } else { &units.members[idx] };
                whole_dirs.insert(pkg.dir.as_str());
                for dep in pkg.dependencies.iter() {
                    if matches!(dep.kind, blaze_manifest::layout::DepKind::Normal | blaze_manifest::layout::DepKind::Build) {
                        push_dep_target(dep, &units, &mut seen_pkgs, &mut dep_stack);
                    }
                }
            }
            for (member, made) in units.members.iter().zip(made_of.iter_mut()) {
                let Some(made) = made.take() else { return Err(CliError::Text { what: "a member's units were not made in the layout's dependency order (units.rs `deps_first`)" }) };
                root_made.parts.push(Part { name: made.name, locate: Locate::Source(member_source(&made)), place: 0 });
                members.push((made.name, made));
                // A member proc-macro crate (or transitive path dep of a proc-macro or build
                // script): its files enter the module key (macro_files.rs, rule 21), each file
                // and folder with its stamp, so the next sweep proves it unchanged by stats alone.
                if whole_dirs.contains(member.dir.as_str()) {
                    crate::macro_files::read_row(&cx, root, stamps, member.dir.as_str(), rows, frame)?;
                } else if let Some(build) = &member.build {
                    let rel = if member.dir.is_empty() {
                        build.root.as_str()
                    } else {
                        build.root.as_str().strip_prefix(member.dir.as_str()).and_then(|s| s.strip_prefix('/')).unwrap_or(build.root.as_str())
                    };
                    crate::macro_files::read_script_files(&cx, root, stamps, member.dir.as_str(), rel, rows, frame)?;
                }
            }
            // Non-member path packages: keep each target root's edition for the file walk and
            // track proc-macro, transitive path dep, and build-script source trees in the sweep
            // leaf table (rule 21).
            for pkg in units.path_packages.iter() {
                let edition = syntax_of(pkg.edition.syntax).byte();
                for target in pkg.lib.iter().chain(pkg.bins.iter()).chain(pkg.build.iter()) {
                    path_roots.push((frame.alloc_str(target.root.as_str()), edition));
                }
                if whole_dirs.contains(pkg.dir.as_str()) {
                    crate::macro_files::read_row(&cx, root, stamps, pkg.dir.as_str(), rows, frame)?;
                } else if let Some(build) = &pkg.build {
                    let rel = if pkg.dir.is_empty() {
                        build.root.as_str()
                    } else {
                        build.root.as_str().strip_prefix(pkg.dir.as_str()).and_then(|s| s.strip_prefix('/')).unwrap_or(build.root.as_str())
                    };
                    crate::macro_files::read_script_files(&cx, root, stamps, pkg.dir.as_str(), rel, rows, frame)?;
                }
            }
        }
        // A refusal is a finding on the line it names: the root's, or a member's whose own
        // units then hold it.
        Err(error) => {
            let (manifest, span) = place_of(&error);
            let message = refusal(&error, frame);
            let at = span.map_or((0, 0), |span| (span.start, span.end));
            match manifest.filter(|id| *id != 0 && (*id as usize) < present.len() && present[*id as usize]) {
                None => root_made.find(UNREAD, at, &[message.as_str()]),
                Some(id) => {
                    let name = Name::manifest(paths.text(id as usize).as_bytes());
                    let mut made = Made::new_in(name, frame);
                    made.find(UNREAD, at, &[message.as_str()]);
                    made.out = parts_out(&made.parts);
                    root_made.parts.push(Part { name, locate: Locate::Source(member_source(&made)), place: 0 });
                    members.push((name, made));
                }
            }
        }
    }
    // The lock, when a member reaches outside: one of the layout's reads, so an edit to it
    // moves the units' source.
    let mut text = Bytes::new_in(frame, "Cargo.lock");
    let lock_text: Option<&[u8]> = match &facts {
        None => None,
        Some((_, Some(own))) => {
            // The members' own lock names the workspace the save keeps its base by
            // (engine_answer.rs `BASES_ROOT`).
            rows.members_read(own, frame);
            Some(own)
        }
        Some((_, None)) => match sweep::read_manifest(&cx, root, stamps, LOCK, rows, &mut text)? {
            false => None,
            true => Some(frame.alloc_slice_copy(text.as_slice())),
        },
    };
    let lock = match lock_text {
        None => None,
        Some(lock_text) => match crate::fetch::Lockfile::read(lock_text, heap, frame) {
            Ok(lock) => Some(lock),
            Err(words) => {
                root_made.find(crate::fetch::LOCK_UNREAD, (0, 0), &[words.as_str()]);
                None
            }
        },
    };
    let parts = match (facts, &lock) {
        (Some((facts, _)), Some(lockfile)) => match facts.locked(lockfile.lock) {
            Ok(parts) => Some(parts),
            Err(unlocked) => {
                let words = bstd::mem::format_in!(frame, "{unlocked}");
                root_made.find(crate::fetch::LOCK_UNREAD, (0, 0), &[words.as_str()]);
                None
            }
        },
        _ => None,
    };
    // The cargo config's rustflags: its files are the layout's reads, so an edit to one moves the
    // units' source and lays the graph again (rule 21).
    let rustflags = crate::graph::rustflags_config(&cx, root, stamps, rows, heap, frame)?;
    root_made.parts.sort_unstable_by_key(|part| part.name);
    root_made.parts.dedup_by_key(|part| part.name);
    root_made.out = parts_out(&root_made.parts);
    Ok(Laid { root: root_made, members, path_roots, lock, parts, graph: None, rustflags, member_deps })
}

/// Where a refusal is: the manifest's id and the span, when it names one.
fn place_of(error: &LayoutError<'_>) -> (Option<u32>, Option<Span>) {
    let span = match error {
        LayoutError::Expected { at, .. }
        | LayoutError::NoPackageName { at }
        | LayoutError::NotARoot { at }
        | LayoutError::Edition { at, .. }
        | LayoutError::NotInherited { at, .. }
        | LayoutError::OutsideAnchor { at }
        | LayoutError::Pattern { at, .. }
        | LayoutError::RecursiveGlob { at }
        | LayoutError::NoTargetFile { at }
        | LayoutError::GitReferences { at }
        | LayoutError::GitAndPath { at }
        | LayoutError::Underscored { at }
        | LayoutError::Target { at, .. } => Some(*at),
        LayoutError::DuplicateName { second, .. } => Some(*second),
        LayoutError::Toml { .. } | LayoutError::NoManifest { .. } | LayoutError::NotAPackage { .. } | LayoutError::NotAMember { .. } => None,
    };
    (span.map(|span| span.manifest.0), span)
}

/// A refusal's words, without the reader's own `manifest N bytes a..b: ` place (the finding is
/// placed by its range).
fn refusal<'f>(error: &LayoutError<'_>, frame: &'f Frame) -> bstd::mem::String<&'f Frame> {
    let whole = bstd::mem::format_in!(frame, "{error}");
    let words = match whole.strip_prefix("manifest ").and_then(|rest| rest.split_once(": ")) {
        Some((_, words)) => words,
        None => whole.as_str(),
    };
    bstd::mem::String::from_str_in(words, frame)
}

/// The digest a member's part is found by in the root's record: its share of the layout.
fn member_source(made: &Made<'_>) -> ContentId {
    let mut h = blaze_ids::Hasher::new(MEMBER_SOURCE_DOMAIN);
    h.update(&made.out.0);
    for (rule, start, end, at, to) in made.findings.iter() {
        h.update(&rule.to_le_bytes()).update(&start.to_le_bytes()).update(&end.to_le_bytes()).part(&made.messages[*at..*to]);
    }
    for (name, at, to, module) in made.paths.iter() {
        h.update(&name.subject.0).part(&made.path_bytes[*at as usize..*to as usize]).update(&[u8::from(*module)]);
    }
    h.finish()
}

/// The workspace members a member depends on as its code is built: its normal and build
/// dependencies that are members ([`Source::Member`]), each by its index in the layout's
/// members, in manifest order. Never a dev-dependency: a program does not reach its packages'
/// tests' dependencies. Every one written is followed, an optional one and one under a
/// `[target.'cfg(..)']` table too: which of them the features and the target turn on is the
/// unit graph's to resolve, later, and a listing that holds one more member only checks more
/// (`run`'s closure is a superset of what the program reaches, never a subset: run_scope.rs).
fn member_deps<'m, 'a>(member: &'m layout::Member<'a>) -> impl Iterator<Item = (usize, &'m layout::Dependency<'a>)> + use<'m, 'a> {
    member.dependencies.iter().filter(|dep| dep.kind != layout::DepKind::Dev).filter_map(|dep| match dep.source {
        Source::Member(to) => Some((to, dep)),
        _ => None,
    })
}

/// The members' indices, each after every member it depends on ([`member_deps`]): a
/// depth-first walk in the layout's order, each member placed when its dependencies are, so the
/// order is the layout's alone (rule 25). A dependency back onto a member still being walked (a
/// cycle, which Rust's packages cannot have outside dev-dependencies) is not followed; that
/// member's units say so by a finding ([`member_units`]). COST: each member once, and each of
/// its dependencies found again by its place (`nth`), so the sum of each member's dependency
/// count squared: a few hundred steps on tempo's 60 members.
fn deps_first<'f>(members: &[layout::Member<'_>], frame: &'f Frame) -> Vec<usize, &'f Frame> {
    let mut order: Vec<usize, &'f Frame> = Vec::with_capacity_in(members.len(), frame);
    // 0: not met, 1: being walked, 2: placed.
    let mut state: Vec<u8, &Frame> = Vec::with_capacity_in(members.len(), frame);
    state.extend(members.iter().map(|_| 0u8));
    // Each member being walked, with how many of its dependencies were looked at.
    let mut stack: Vec<(usize, usize), &Frame> = Vec::new_in(frame);
    for start in 0..members.len() {
        if state[start] != 0 {
            continue;
        }
        state[start] = 1;
        stack.push((start, 0));
        while let Some((at, next)) = stack.last().copied() {
            match member_deps(&members[at]).nth(next) {
                Some((to, _)) => {
                    if let Some(top) = stack.last_mut() {
                        top.1 += 1;
                    }
                    if to < members.len() && state[to] == 0 {
                        state[to] = 1;
                        stack.push((to, 0));
                    }
                }
                None => {
                    stack.pop();
                    state[at] = 2;
                    order.push(at);
                }
            }
        }
    }
    order
}

/// Workspace-level standard library policy read from `[workspace.metadata.blaze]` in the root
/// `Cargo.toml`:
/// - `std = "bstd"` requires every internal workspace member (except pure proc-macro crates and
///   members named in `std-allow-rust`) to use a blaze edition (`std` is `bstd`).
/// - `std = "rust"` (or absent) preserves ordinary Rust workspaces.
struct StdPolicy<'f> {
    require_bstd: bool,
    allow_rust: Vec<&'f str, &'f Frame>,
}

impl<'f> StdPolicy<'f> {
    fn rust(frame: &'f Frame) -> StdPolicy<'f> {
        StdPolicy { require_bstd: false, allow_rust: Vec::new_in(frame) }
    }

    fn requires_bstd(&self, member_name: &str) -> bool {
        self.require_bstd && !self.allow_rust.iter().any(|allowed| *allowed == member_name)
    }
}

/// A TOML item's byte range narrowed to `(u32, u32)` for a finding's span.
fn span_u32(span: core::ops::Range<usize>) -> (u32, u32) {
    (u32::try_from(span.start).unwrap_or(u32::MAX), u32::try_from(span.end).unwrap_or(u32::MAX))
}

/// Parse `[workspace.metadata.blaze]` `std` and `std-allow-rust` from the root `Cargo.toml` text.
/// Any malformed value emits a hard [`UNREAD`] finding on `root_made` at the TOML item's byte
/// span (rules 3 and 19).
fn parse_std_policy<'f>(manifest: &str, root_made: &mut Made<'f>, frame: &'f Frame) -> StdPolicy<'f> {
    let doc = match blaze_toml::Document::parse_in(manifest, blaze_toml::Limits::CARGO, frame) {
        Ok(doc) => doc,
        Err(error) => {
            let words = bstd::mem::format_in!(frame, "the root `Cargo.toml` could not be read for `[workspace.metadata.blaze]`: {error}");
            let at = u32::try_from(error.at).unwrap_or(u32::MAX);
            root_made.find(UNREAD, (at, at), &[words.as_str()]);
            return StdPolicy::rust(frame);
        }
    };
    let Some(blaze) = doc.root().get("workspace").and_then(|it| it.get("metadata")).and_then(|it| it.get("blaze")) else {
        return StdPolicy::rust(frame);
    };
    if !blaze.is_table() {
        root_made.find(UNREAD, span_u32(blaze.span()), &["`[workspace.metadata.blaze]` is not a table"]);
        return StdPolicy::rust(frame);
    }
    let mut require_bstd = false;
    if let Some(item) = blaze.get("std") {
        let at = span_u32(item.span());
        match item.as_str() {
            Some("bstd") => require_bstd = true,
            Some("rust") => require_bstd = false,
            Some(other) => {
                let words = bstd::mem::format_in!(frame, "`[workspace.metadata.blaze] std` is `{other}`, which is not valid: name `\"bstd\"` or `\"rust\"`");
                root_made.find(UNREAD, at, &[words.as_str()]);
            }
            None => {
                root_made.find(UNREAD, at, &["`[workspace.metadata.blaze] std` is not a string: name `\"bstd\"` or `\"rust\"`"]);
            }
        }
    }
    let mut allow_rust = Vec::new_in(frame);
    if let Some(allow) = blaze.get("std-allow-rust") {
        if !allow.is_array() {
            root_made.find(
                UNREAD,
                span_u32(allow.span()),
                &["`[workspace.metadata.blaze] std-allow-rust` is not an array: write member package names, `std-allow-rust = [\"crate-a\"]`"],
            );
        } else {
            for entry in allow.children() {
                match entry.as_str() {
                    Some(name) => allow_rust.push(&*frame.alloc_str(name)),
                    None => {
                        root_made.find(
                            UNREAD,
                            span_u32(entry.span()),
                            &["an entry of `[workspace.metadata.blaze] std-allow-rust` is not a string: write a workspace member package name"],
                        );
                    }
                }
            }
        }
    }
    StdPolicy { require_bstd, allow_rust }
}

/// The manifest spelling of a Rust edition (`2015`, `2018`, `2021`, `2024`).
fn rust_edition_name(edition: edition::Edition) -> &'static str {
    match edition {
        edition::Edition::Edition2015 => "2015",
        edition::Edition::Edition2018 => "2018",
        edition::Edition::Edition2021 => "2021",
        edition::Edition::Edition2024 => "2024",
    }
}

/// A member's units: its crate roots, the units of each member it depends on, and a finding on
/// a blaze-edition line naming the toolchain's crate.
///
/// `(members, listed)`: the layout's members and, by the same index, each member's units name
/// and source once made ([`deps_first`] makes a member's dependencies first). A dependency's
/// part is at the source the root's units list it by (`member_source`), so both records name
/// one part one way; one not made yet is a member this one is being walked from, a cycle.
fn member_units<'f>(
    name: Name,
    member: &layout::Member<'_>,
    text: &str,
    toolchain: blaze_toolchain::ToolchainId,
    require_bstd: bool,
    (members, listed): (&[layout::Member<'_>], &[Option<(Name, ContentId)>]),
    frame: &'f Frame,
) -> Made<'f> {
    let mut made = Made::new_in(name, frame);
    let edition = syntax_of(member.edition.syntax).byte();
    // Every crate root the graph makes a unit of is a part: the library, the binaries, and the
    // integration tests, examples and benches the layout read (`extras`: a unit whose root no
    // units record lists is `DriveError::NoPart` at its first want).
    let roots = member.lib.iter().chain(member.bins.iter()).map(|target| &target.root).chain(member.extras.iter().map(|extra| &extra.root));
    for root in roots {
        let file = Name::file(root.as_bytes());
        if made.parts.iter().any(|part| part.name == file) {
            continue;
        }
        made.parts.push(Part { name: file, locate: Locate::File { edition }, place: 0 });
        made.path(file, root.as_bytes(), false);
    }
    // Each workspace member it depends on, once (a dependency written in two tables is one
    // part): `run`'s closure follows these from the program's package (blaze-drive run_scope.rs).
    for (to, dep) in member_deps(member) {
        match listed.get(to).copied().flatten() {
            Some((units, source)) => {
                if !made.parts.iter().any(|part| part.name == units) {
                    made.parts.push(Part { name: units, locate: Locate::Source(source), place: 0 });
                }
            }
            None => {
                let back = members.get(to).map_or("", |to| to.name.as_str());
                let words = bstd::mem::format_in!(frame, "the package `{}` depends on `{back}`, a workspace member that depends back on it: a cycle of dependencies (dev-dependencies aside)", member.name.as_str());
                made.find(UNREAD, (dep.span.start, dep.span.end), &[words.as_str()]);
            }
        }
    }
    made.parts.sort_unstable_by_key(|part| part.name);
    // cargo's rule (blaze-manifest manifest.rs `InvalidManifest::NoTargets`): only a package with
    // no target of any kind is an error, a build script not counted. A package of benches only
    // or tests only is a package (turso's `perf/query-batch`, `postgres/tests`); a target the
    // layout refused is a target written, its own finding below. The layout is read with test
    // targets (`lay`), so `extras` holds every test, example and bench the manifest has.
    if member.lib.is_none() && member.bins.is_empty() && member.extras.is_empty() && member.refused_targets.is_empty() {
        made.find(UNREAD, (member.span.start, member.span.end), &["the package has no target: no library, binary, test, example or bench"]);
    }
    // Each test, example or bench target the layout refused is a finding on its table's line
    // (rule 3: left out, never quietly): it is in no unit, and the member's other targets and
    // every other member are read as before. The layout keeps them in manifest-byte order.
    for refused in member.refused_targets.iter() {
        let why = bstd::mem::format_in!(frame, "{}", refused.why);
        made.find(
            TARGET_REFUSED,
            (refused.at.start, refused.at.end),
            &[refused.kind.table(), "\x1f", refused.target.as_str(), "\x1f", member.name.as_str(), "\x1f", why.as_str()],
        );
    }
    for dep in member.dependencies.iter() {
        if let Source::Member(_) = dep.source {
            continue;
        }
        if crate::graph::toolchain_line(member.edition.blaze.is_some(), dep.name.as_str(), dep.package.as_str()) {
            made.find(TOOLCHAIN_NAMED, whole_line(text, dep.span), &[dep.name.as_str()]);
        }
    }
    // Workspace-level `[workspace.metadata.blaze] std = "bstd"` gate: refuse any internal
    // workspace member whose `std` is Rust's `std`, except pure proc-macro crates (whose targets
    // are always host-side Rust/wasm units by `blaze-unit-graph::implicit`) and members listed in
    // `std-allow-rust`.
    let pure_proc_macro = member.lib.as_ref().is_some_and(|lib| lib.proc_macro) && member.bins.is_empty() && member.extras.is_empty();
    if require_bstd && !pure_proc_macro && implicit(member.edition, &UnitKind::Lib).std == StdIs::Rust {
        made.find(
            STD_NOT_BSTD,
            (member.span.start, member.span.end),
            &[member.name.as_str(), "\x1f", rust_edition_name(member.edition.syntax)],
        );
    }
    // What reads the units sees what each crate is given with nothing in the manifest, by the
    // one rule (blaze-unit-graph `implicit`), and from which toolchain: the toolchain's id is in
    // this record's output and in no other key (toolchain.md, section 1). The member's `std`
    // policy flag is hashed too so toggling `std = "bstd"` or `std-allow-rust` moves the output
    // (rule 21).
    let mut out = blaze_ids::Hasher::new(UNITS_OUT_DOMAIN);
    out.update(&parts_out(&made.parts).0).update(&toolchain.0).update(&[u8::from(require_bstd)]);
    // A binary, an integration test, an example and a bench are given what a library is.
    let like_lib = member.bins.len() + member.extras.len();
    let kinds = member.lib.iter().map(|lib| if lib.proc_macro { UnitKind::ProcMacro } else { UnitKind::Lib }).chain((0..like_lib).map(|_| UnitKind::Lib));
    for kind in kinds {
        let given = implicit(member.edition, &kind);
        let std = match given.std {
            StdIs::Rust => 0,
            StdIs::Bstd(blaze) => blaze as u8 + 1,
        };
        out.update(&[std, u8::from(given.proc_macro)]).update(given.prelude.module().as_bytes());
    }
    made.out = out.finish();
    made
}

/// The lines `span` is on, from the first's start through the last's newline (or the text's
/// end): what deleting the dependency removes.
fn whole_line(text: &str, span: Span) -> (u32, u32) {
    let bytes = text.as_bytes();
    let start = (span.start as usize).min(bytes.len());
    let end = (span.end as usize).clamp(start, bytes.len());
    let first = bytes[..start].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let last = bytes[end..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |at| end + at + 1);
    (first as u32, last as u32)
}

/// The outline's edition of a Rust edition (also what `blaze why` outlines a member's file
/// under, engine_units.rs `stored_editions`).
pub(crate) fn syntax_of(edition: edition::Edition) -> Edition {
    match edition {
        edition::Edition::Edition2015 => Edition::E2015,
        edition::Edition::Edition2018 => Edition::E2018,
        edition::Edition::Edition2021 => Edition::E2021,
        edition::Edition::Edition2024 => Edition::E2024,
    }
}

/// The units task `task`: the root's or a member's, from the request's layout, copied into
/// `frame` (the batch's); a frame that refuses the copy fails the task as a hole.
pub fn run<'f>(laid: &Laid<'_>, task: &Task<'_>, frame: &'f Frame) -> Result<Did<'f>, CliError> {
    let made = match laid.members.iter().find(|(name, _)| *name == task.name) {
        _ if task.name == Name::units() => &laid.root,
        Some((_, made)) => made,
        None => return Err(CliError::Text { what: "a units task of a manifest the workspace's layout does not list" }),
    };
    Ok(match made.clone_in(frame) {
        Ok(held) => Did::Done(held),
        Err(_) => crate::work::held_refused(task.name),
    })
}

impl<A: core::alloc::Allocator + Copy> crate::work::Record<A> {
    /// A finding of `rule` at `at`, its message the pieces in order.
    pub(crate) fn find(&mut self, rule: u16, at: (u32, u32), pieces: &[&str]) {
        let start = self.messages.len();
        for piece in pieces {
            self.messages.extend_from_slice(piece.as_bytes());
        }
        self.findings.push((rule, at.0, at.1, start, self.messages.len()));
    }
}

#[cfg(test)]
mod tests {
    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::path::Path;
    use bstd::test::BUDGET;

    use super::*;
    use crate::sweep::FromMeta;

    /// The owner's /Volumes/Projects/tmp/hello-world/Cargo.toml, as written.
    const HELLO: &str = "[package]\nname = \"hello-world\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nbstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }\n";

    /// A package in the blaze edition whose dependency is by absolute path: its crate root is a
    /// part in the 2024 syntax with its path, and the dependency a finding on its key.
    #[test]
    fn a_blaze_edition_package_with_an_outside_dependency_is_read_and_the_dependency_named_on_its_key() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        root.publish(&cx, &Path::new("Cargo.toml").expect("a path"), Content::bytes(HELLO.as_bytes()), Durable::No).expect("write");
        root.publish(&cx, &Path::new("src/main.rs").expect("a path"), Content::bytes(b"fn main() {}"), Durable::No).expect("write");
        // A stale lock, as the owner's hello-world has: with nothing outside to reach, unread.
        root.publish(&cx, &Path::new("Cargo.lock").expect("a path"), Content::bytes(b"version = 4\n"), Durable::No).expect("write");
        let erased = blaze_own::Erased::new(frame);
        let mut rows = Rows::new_in(frame);
        let laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), blaze_own::testing::heap(), frame, erased.handle()).expect("laid");
        assert_eq!(laid.root.findings.len(), 0, "{:?}", core::str::from_utf8(&laid.root.messages));
        // The members' own lock stands in for it (graph.rs `members_lock`): the graph and the
        // names world are made from the toolchain and the member alone.
        let lock = laid.lock.as_ref().expect("the members' own lock");
        assert_ne!(lock.text, b"version = 4\n", "the stale file is never the lock");
        assert!(laid.parts.is_some(), "the member is matched to its own lock");
        let [(name, member)] = laid.members.as_slice() else { panic!("one member: {}", laid.members.len()) };
        assert_eq!(*name, Name::manifest(b"Cargo.toml"));
        assert_eq!(member.parts.as_slice(), [Part { name: Name::file(b"src/main.rs"), locate: Locate::File { edition: Edition::E2024.byte() }, place: 0 }]);
        let [(rule, start, end, at, to)] = member.findings.as_slice() else { panic!("one finding") };
        // Under the blaze edition the line naming bstd is the toolchain's crate named again: the
        // finding is the whole line with its newline, which its fix deletes.
        let line = "bstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }\n";
        assert_eq!((*rule, &HELLO[*start as usize..*end as usize], &member.messages[*at..*to]), (TOOLCHAIN_NAMED, line, &b"bstd"[..]));
        let mut fixed = std::string::String::from(&HELLO[..*start as usize]);
        fixed.push_str(&HELLO[*end as usize..]);
        assert_eq!(fixed, "[package]\nname = \"hello-world\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\n");
        // What it read: the manifest, and the listing of `src`.
        let read: std::vec::Vec<&str> = rows.rows.iter().map(|leaf| rows.path(leaf)).collect();
        assert!(read.contains(&"Cargo.toml") && read.contains(&"src"), "{read:?}");
        assert!(!read.contains(&"Cargo.lock"), "{read:?}");
    }

    /// Control: the same line in a Rust-edition package is an ordinary dependency, not the
    /// toolchain's crate named again.
    #[test]
    fn a_rust_edition_package_naming_bstd_keeps_its_line() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        let manifest = HELLO.replace("blaze-2026.11", "2024");
        root.publish(&cx, &Path::new("Cargo.toml").expect("a path"), Content::bytes(manifest.as_bytes()), Durable::No).expect("write");
        root.publish(&cx, &Path::new("src/main.rs").expect("a path"), Content::bytes(b"fn main() {}"), Durable::No).expect("write");
        root.publish(&cx, &Path::new("Cargo.lock").expect("a path"), Content::bytes(b"version = 4\n"), Durable::No).expect("write");
        let erased = blaze_own::Erased::new(frame);
        let mut rows = Rows::new_in(frame);
        let laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), blaze_own::testing::heap(), frame, erased.handle()).expect("laid");
        let [(_, member)] = laid.members.as_slice() else { panic!("one member") };
        assert!(member.findings.is_empty());
        // An outside dependency is the unit-graph job's to read: nothing is said of it here, but
        // the lock is read (the control of the stale lock left unread above).
        assert!(laid.root.findings.iter().all(|finding| finding.0 == crate::fetch::LOCK_UNREAD), "{:?}", core::str::from_utf8(&laid.root.messages));
        let read: std::vec::Vec<&str> = rows.rows.iter().map(|leaf| rows.path(leaf)).collect();
        assert!(read.contains(&"Cargo.lock"), "{read:?}");
    }

    /// A workspace whose members name outside dependencies lays with no finding: reading them
    /// is the unit-graph job's (finding 10, `dependencies-not-read`, is retired).
    #[test]
    fn outside_dependencies_are_no_finding_of_the_layout() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        let put = |path: &str, text: &str| {
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
        };
        put("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n\n[workspace.dependencies]\nserde = \"1\"\n");
        put("a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\ntokio = \"1\"\nzstd = \"1\"\n");
        put("b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\nserde = \"1\"\nanyhow = \"1\"\nbytes = \"1\"\n");
        put("a/src/lib.rs", "");
        put("b/src/lib.rs", "");
        let erased = blaze_own::Erased::new(frame);
        let mut rows = Rows::new_in(frame);
        let laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), blaze_own::testing::heap(), frame, erased.handle()).expect("laid");
        assert!(laid.members.iter().all(|(_, member)| member.findings.is_empty()));
        assert!(laid.root.findings.is_empty(), "{:?}", core::str::from_utf8(&laid.root.messages));
    }

    /// The workspace laid from `files` (path, text), in a simulated tree.
    fn laid_of(files: &[(&str, &str)], check: impl FnOnce(&Laid<'_>)) {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        for (path, text) in files {
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
        }
        let erased = blaze_own::Erased::new(frame);
        let mut rows = Rows::new_in(frame);
        let laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), blaze_own::testing::heap(), frame, erased.handle()).expect("laid");
        check(&laid);
    }

    /// The `Units` parts of the member whose manifest is at `path`.
    fn member_parts(laid: &Laid<'_>, path: &str) -> std::vec::Vec<Name> {
        let (_, member) = laid.members.iter().find(|(name, _)| *name == Name::manifest(path.as_bytes())).expect("a member at the path");
        member.parts.iter().filter(|part| part.name.kind == blaze_drive::Kind::Units).map(|part| part.name).collect()
    }

    fn package(name: &str, tail: &str) -> std::string::String {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{tail}")
    }

    /// `run`'s closure (blaze-drive run_scope.rs) follows these parts: `app` depends on `lib_a`
    /// (a normal dependency), `lib_a` on `lib_b` (a build dependency), and `lib_c` is only
    /// `app`'s dev-dependency, which a program does not reach. Each listed member is at the
    /// source the root's units list it by, so the two records name one part one way.
    #[test]
    fn a_members_units_list_the_members_it_depends_on_and_no_dev_dependency() {
        let app = package("app", "\n[dependencies]\nlib_a = { path = \"../lib_a\" }\n\n[dev-dependencies]\nlib_c = { path = \"../lib_c\" }\n");
        let lib_a = package("lib_a", "\n[build-dependencies]\nlib_b = { path = \"../lib_b\" }\n");
        let (lib_b, lib_c) = (package("lib_b", ""), package("lib_c", ""));
        let files = [
            ("Cargo.toml", "[workspace]\nmembers = [\"app\", \"lib_a\", \"lib_b\", \"lib_c\"]\n"),
            ("app/Cargo.toml", app.as_str()),
            ("app/src/main.rs", "fn main() {}"),
            ("lib_a/Cargo.toml", lib_a.as_str()),
            ("lib_a/src/lib.rs", ""),
            ("lib_b/Cargo.toml", lib_b.as_str()),
            ("lib_b/src/lib.rs", ""),
            ("lib_c/Cargo.toml", lib_c.as_str()),
            ("lib_c/src/lib.rs", ""),
        ];
        laid_of(&files, |laid| {
            assert_eq!(member_parts(laid, "app/Cargo.toml"), [Name::manifest(b"lib_a/Cargo.toml")]);
            assert_eq!(member_parts(laid, "lib_a/Cargo.toml"), [Name::manifest(b"lib_b/Cargo.toml")]);
            assert_eq!(member_parts(laid, "lib_b/Cargo.toml"), std::vec::Vec::<Name>::new());
            assert_eq!(member_parts(laid, "lib_c/Cargo.toml"), std::vec::Vec::<Name>::new());
            for (_, member) in laid.members.iter() {
                assert!(member.findings.is_empty(), "{:?}", core::str::from_utf8(&member.messages));
                for part in member.parts.iter().filter(|part| part.name.kind == blaze_drive::Kind::Units) {
                    let listed = laid.root.parts.iter().find(|root| root.name == part.name).expect("the root lists every member");
                    assert_eq!(listed.locate, part.locate, "one part, one source");
                }
            }
        });
    }

    /// Control of the order: the same workspace with `lib_b` listed first and `app` last lays to
    /// the same member records (each member made after what it depends on, whatever the order).
    #[test]
    fn the_members_listing_does_not_depend_on_the_members_order() {
        let app = package("app", "\n[dependencies]\nlib_a = { path = \"../lib_a\" }\n");
        let lib_a = package("lib_a", "\n[dependencies]\nlib_b = { path = \"../lib_b\" }\n");
        let lib_b = package("lib_b", "");
        let tree = |members: &'static str| {
            let mut sources = std::vec::Vec::new();
            let files = [
                ("Cargo.toml", members),
                ("app/Cargo.toml", app.as_str()),
                ("app/src/main.rs", "fn main() {}"),
                ("lib_a/Cargo.toml", lib_a.as_str()),
                ("lib_a/src/lib.rs", ""),
                ("lib_b/Cargo.toml", lib_b.as_str()),
                ("lib_b/src/lib.rs", ""),
            ];
            laid_of(&files, |laid| {
                sources = laid.root.parts.iter().cloned().collect::<std::vec::Vec<Part>>();
            });
            sources
        };
        let forward = tree("[workspace]\nmembers = [\"app\", \"lib_a\", \"lib_b\"]\n");
        let backward = tree("[workspace]\nmembers = [\"lib_b\", \"lib_a\", \"app\"]\n");
        assert_eq!(forward, backward);
    }

    /// Two members that depend on each other (not a Rust workspace cargo builds) are a finding on
    /// the dependency line that closes the cycle, never a hang or a part left out in silence.
    #[test]
    fn a_cycle_of_members_is_a_finding_on_its_line() {
        let a = package("a", "\n[dependencies]\nb = { path = \"../b\" }\n");
        let b = package("b", "\n[dependencies]\na = { path = \"../a\" }\n");
        let files = [("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n"), ("a/Cargo.toml", a.as_str()), ("a/src/lib.rs", ""), ("b/Cargo.toml", b.as_str()), ("b/src/lib.rs", "")];
        laid_of(&files, |laid| {
            let findings: std::vec::Vec<&str> = laid
                .members
                .iter()
                .flat_map(|(_, member)| member.findings.iter().map(|(_, _, _, at, to)| core::str::from_utf8(&member.messages[*at..*to]).expect("words")))
                .collect();
            assert_eq!(findings.len(), 1, "{findings:?}");
            assert!(findings[0].contains("depends back on it"), "{findings:?}");
        });
    }

    /// What the layout of `files` gives a build script's unit from the cargo config's rustflags
    /// (no `--target`: the twin of a `--target` build is blaze-manifest rustflags_tests.rs), the
    /// units' source, and the config rows the layout read with whether each was there.
    #[allow(clippy::type_complexity, reason = "a test's three answers")]
    fn rustflags_laid(files: &[(&str, &str)]) -> Result<(std::vec::Vec<std::string::String>, ContentId, std::vec::Vec<(std::string::String, bool)>), std::string::String> {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        for (path, text) in files {
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
        }
        let erased = blaze_own::Erased::new(frame);
        let mut rows = Rows::new_in(frame);
        let laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), blaze_own::testing::heap(), frame, erased.handle()).map_err(|error| format!("{error:?}"))?;
        let unit = blaze_manifest::rustflags::Unit { target_given: false, host_unit: true };
        let atoms = [("unix", None), ("target_vendor", Some("apple"))];
        let chosen = blaze_manifest::rustflags::choose(&laid.rustflags, blaze_manifest::rustflags::RustflagsEnv::default(), "aarch64-apple-darwin", &atoms, unit, blaze_own::testing::heap().alloc()).map_err(|error| error.to_string())?;
        let cfg = chosen.cfg.iter().map(|(name, _)| name.as_str().to_owned()).collect();
        let source = sweep::units_source(rows.rows.iter().map(|leaf| (rows.path(leaf), leaf)), frame);
        let config = rows.rows.iter().filter(|leaf| rows.path(leaf).starts_with(".cargo/")).map(|leaf| (rows.path(leaf).to_owned(), !leaf.absent())).collect();
        Ok((cfg, source, config))
    }

    const MEMBER: [(&str, &str); 2] = [("Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"), ("src/lib.rs", "")];

    /// turso's `.cargo/config.toml` `--cfg=tokio_unstable` reaches the units' cfg (ORACLE); with
    /// no config nothing does (TWIN), and both config names are read absent, so making one lays
    /// the graph again; `--cfg=other` gives `other` (CONTROL, rule 23) and moves the units' source
    /// (the graph's key, rule 21).
    #[test]
    fn a_cargo_config_rustflags_cfg_is_a_read_of_the_layout_and_reaches_the_units_cfg() {
        let with = |flag: &str| {
            let config = format!("[build]\nrustflags = [\"--cfg={flag}\"]\n");
            let mut files = MEMBER.to_vec();
            files.push((".cargo/config.toml", config.leak()));
            rustflags_laid(&files).expect("laid")
        };
        let (cfg, tokio, rows) = with("tokio_unstable");
        assert_eq!(cfg, ["tokio_unstable"]);
        assert_eq!(rows, [(".cargo/config".to_owned(), false), (".cargo/config.toml".to_owned(), true)]);
        let (cfg, other, _) = with("other");
        assert_eq!(cfg, ["other"]);
        assert_ne!(tokio, other, "an edited rustflags moves the units' source");
        let (cfg, none, rows) = rustflags_laid(&MEMBER).expect("laid");
        assert!(cfg.is_empty(), "{cfg:?}");
        assert_eq!(rows, [(".cargo/config".to_owned(), false), (".cargo/config.toml".to_owned(), false)]);
        assert_ne!(none, tokio);
    }

    /// tempo's `.cargo/config.toml` includes an optional `orb.toml`: absent, it is a row read
    /// absent and nothing else; present, its flags merge under the including file's. A missing
    /// include that is not optional is refused by name.
    #[test]
    fn a_cargo_config_include_is_read_under_the_including_file() {
        let tempo = "include = [{ path = \"orb.toml\", optional = true }]\n\n[alias]\nx = \"run --bin x\"\n";
        let mut files = MEMBER.to_vec();
        files.push((".cargo/config.toml", tempo));
        let (cfg, _, rows) = rustflags_laid(&files).expect("tempo's config");
        assert!(cfg.is_empty(), "{cfg:?}");
        assert!(rows.contains(&(".cargo/orb.toml".to_owned(), false)), "{rows:?}");
        files.push((".cargo/orb.toml", "[build]\nrustflags = [\"--cfg=orb\"]\n"));
        let (cfg, _, _) = rustflags_laid(&files).expect("with orb.toml");
        assert_eq!(cfg, ["orb"]);
        let mut files = MEMBER.to_vec();
        files.push((".cargo/config.toml", "include = [\"gone.toml\"]\n"));
        let refused = rustflags_laid(&files).expect_err("a missing include");
        assert!(refused.contains("gone.toml"), "{refused}");
    }

    /// `[patch.crates-io]` and transitive path dependencies outside `[workspace.members]` are
    /// tracked as layout leaves (so editing them moves `units_source`), skipped cleanly when
    /// absent from `Cargo.lock`, classified as `PathKind::External`, and resolve `UnitGraph`
    /// without hitting `PathOutsideWorkspace`.
    #[test]
    fn patch_and_transitive_path_packages_track_leaves_and_resolve_unit_graph() {
        const ROOT_TOML: &str = "\
[workspace]
members = [\"crates/app\"]
exclude = [\"vendor/patched\", \"vendor/helper\", \"vendor/unused\"]

[patch.crates-io]
patched = { path = \"vendor/patched\" }
unused = { path = \"vendor/unused\" }
";
        const APP_TOML: &str = "\
[package]
name = \"app\"
version = \"0.1.0\"
edition = \"2024\"

[dependencies]
patched = \"1.0\"
";
        const HELPER_TOML: &str = "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        const UNUSED_TOML: &str = "[package]\nname = \"unused\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        const LOCK_TOML: &str = "\
version = 4

[[package]]
name = \"app\"
version = \"0.1.0\"
dependencies = [\"patched\"]

[[package]]
name = \"patched\"
version = \"1.0.0\"
dependencies = [\"helper\"]

[[package]]
name = \"helper\"
version = \"0.1.0\"
";
        let run_once = |patched_toml: &str| {
            let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
            let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
            let frame = cx.frame();
            let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
            let put = |path: &str, text: &str| {
                root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
            };
            put("Cargo.toml", ROOT_TOML);
            put("Cargo.lock", LOCK_TOML);
            put("crates/app/Cargo.toml", APP_TOML);
            put("crates/app/src/lib.rs", "pub fn f() {}");
            put("vendor/patched/Cargo.toml", patched_toml);
            put("vendor/patched/src/lib.rs", "pub fn p() {}");
            put("vendor/helper/Cargo.toml", HELPER_TOML);
            put("vendor/helper/src/lib.rs", "pub fn h() {}");
            put("vendor/unused/Cargo.toml", UNUSED_TOML);
            put("vendor/unused/src/lib.rs", "");
            let erased = blaze_own::Erased::new(frame);
            let mut rows = Rows::new_in(frame);
            let mut laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), blaze_own::testing::heap(), frame, erased.handle()).expect("laid");
            let source = sweep::units_source(rows.rows.iter().map(|leaf| (rows.path(leaf), leaf)), frame);
            let heap = blaze_own::testing::heap();
            let mut tracked = blaze_own::Vec::new_in(heap.alloc());
            tracked.extend(rows.rows.iter().filter(|l| !l.absent()).map(|l| blaze_own::String::from_str_in(rows.path(l), heap.alloc())));
            let mut path_roots = blaze_own::Vec::new_in(heap.alloc());
            path_roots.extend(laid.path_roots.iter().map(|(p, _)| blaze_own::String::from_str_in(p, heap.alloc())));
            let parts = laid.parts.take().expect("graph parts");
            let lock = laid.lock.as_ref().expect("lock");
            let desc = || blaze_unit_graph::TargetDesc {
                triple: blaze_unit_graph::Text::from_str_in("aarch64-apple-darwin", heap.alloc()),
                cfg: blaze_own::Vec::new_in(heap.alloc()),
            };
            let graph = crate::graph::Graph::new(parts, lock, source, desc(), desc(), false, heap, frame);
            let mut dirs = blaze_own::Vec::new_in(heap.alloc());
            dirs.extend(graph.member_dirs().iter().map(|d| (blaze_own::String::from_str_in(d.dir.as_str(), heap.alloc()), d.kind)));
            struct NoRecords;
            impl<'a> blaze_unit_graph::Records<'a> for NoRecords {
                fn record(&mut self, _: &blaze_fetch::SourceId<'_>) -> Option<&'a [u8]> {
                    None
                }
            }
            let unheld = blaze_drive::unit_graph_task::Unheld { failed: &[], refused: &[] };
            let resolved = match blaze_drive::unit_graph_task::run(graph.inputs(), lock.lock, NoRecords, blaze_fetch::pack::PackView::open_source_put, unheld, heap.alloc()) {
                Ok(blaze_drive::unit_graph_task::Entry::Made(made)) => {
                    let decoded = blaze_unit_graph::record::decode(made.record.as_slice(), heap.alloc()).expect("decode graph");
                    let mut names = blaze_own::Vec::new_in(heap.alloc());
                    names.extend(decoded.graph.units.iter().map(|u| blaze_own::String::from_str_in(u.name.as_str(), heap.alloc())));
                    Ok(names)
                }
                Ok(blaze_drive::unit_graph_task::Entry::Need(need)) => Err(format!("unexpected need: {}", need.len())),
                Ok(blaze_drive::unit_graph_task::Entry::NotFetched { count }) => Err(format!("not fetched: {count}")),
                Err(why) => Err(format!("graph failed: {why}")),
            };
            (source, tracked, path_roots, dirs, resolved)
        };
        let patched_a = "[package]\nname = \"patched\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\n[dependencies]\nhelper = { path = \"../helper\" }\n";
        let patched_b = "[package]\nname = \"patched\"\nversion = \"1.0.0\"\nedition = \"2021\"\ndescription = \"edited\"\n\n[dependencies]\nhelper = { path = \"../helper\" }\n";
        let (src_a, tracked, path_roots, dirs, resolved) = run_once(patched_a);
        assert!(tracked.iter().any(|p| p.as_str() == "vendor/patched/Cargo.toml"), "patched manifest tracked: {tracked:?}");
        assert!(tracked.iter().any(|p| p.as_str() == "vendor/helper/Cargo.toml"), "helper manifest tracked: {tracked:?}");
        assert!(path_roots.iter().any(|p| p.as_str() == "vendor/patched/src/lib.rs"), "patched root recorded: {path_roots:?}");
        assert!(path_roots.iter().any(|p| p.as_str() == "vendor/helper/src/lib.rs"), "helper root recorded: {path_roots:?}");
        assert!(dirs.iter().any(|(d, k)| d.as_str() == "crates/app" && *k == blaze_world::PathKind::Member), "dirs: {dirs:?}");
        assert!(dirs.iter().any(|(d, k)| d.as_str() == "vendor/patched" && *k == blaze_world::PathKind::External), "dirs: {dirs:?}");
        assert!(dirs.iter().any(|(d, k)| d.as_str() == "vendor/helper" && *k == blaze_world::PathKind::External), "dirs: {dirs:?}");
        assert!(!dirs.iter().any(|(d, _)| d.as_str() == "vendor/unused"), "unused patch must not be in locked dirs: {dirs:?}");
        let units = resolved.expect("unit graph must resolve cleanly");
        assert!(units.iter().any(|u| u.as_str() == "app"), "units: {units:?}");
        assert!(units.iter().any(|u| u.as_str() == "patched"), "units: {units:?}");
        assert!(units.iter().any(|u| u.as_str() == "helper"), "units: {units:?}");
        let (src_b, _, _, _, _) = run_once(patched_b);
        assert_ne!(src_a, src_b, "editing patched Cargo.toml must invalidate units_source");
    }

    #[test]
    fn std_bstd_refuses_rust_edition_workspace_member() {
        const ROOT_BSTD: &str = "[workspace]\nmembers = [\"app\"]\n\n[workspace.metadata.blaze]\nstd = \"bstd\"\n";
        const ROOT_RUST: &str = "[workspace]\nmembers = [\"app\"]\n\n[workspace.metadata.blaze]\nstd = \"rust\"\n";
        const ROOT_ABSENT: &str = "[workspace]\nmembers = [\"app\"]\n";
        const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

        let mut bstd_out = ContentId([0; 32]);
        let mut bstd_locate = Locate::Source(ContentId([0; 32]));
        laid_of(&[("Cargo.toml", ROOT_BSTD), ("app/Cargo.toml", APP), ("app/src/lib.rs", "")], |laid| {
            assert!(laid.root.findings.is_empty(), "{:?}", core::str::from_utf8(&laid.root.messages));
            let [(name, member)] = laid.members.as_slice() else { panic!("one member: {}", laid.members.len()) };
            assert_eq!(*name, Name::manifest(b"app/Cargo.toml"));
            let [(rule, start, end, at, to)] = member.findings.as_slice() else {
                panic!("expected one STD_NOT_BSTD finding, got {:?}", member.findings);
            };
            let start = usize::try_from(*start).expect("start fits usize");
            let end = usize::try_from(*end).expect("end fits usize");
            assert_eq!(*rule, STD_NOT_BSTD);
            assert_eq!(&APP[start..end], "[package]");
            assert_eq!(&member.messages[*at..*to], b"app\x1f2024");
            bstd_out = member.out;
            bstd_locate = laid.root.parts[0].locate;
        });

        laid_of(&[("Cargo.toml", ROOT_RUST), ("app/Cargo.toml", APP), ("app/src/lib.rs", "")], |laid| {
            assert!(laid.root.findings.is_empty(), "{:?}", core::str::from_utf8(&laid.root.messages));
            let [(_, member)] = laid.members.as_slice() else { panic!("one member") };
            assert!(member.findings.is_empty(), "{:?}", core::str::from_utf8(&member.messages));
            assert_ne!(member.out, bstd_out, "std policy flag must move member units out (rule 21)");
            assert_ne!(laid.root.parts[0].locate, bstd_locate, "member_source must move with policy and findings");
        });

        laid_of(&[("Cargo.toml", ROOT_ABSENT), ("app/Cargo.toml", APP), ("app/src/lib.rs", "")], |laid| {
            assert!(laid.root.findings.is_empty(), "{:?}", core::str::from_utf8(&laid.root.messages));
            let [(_, member)] = laid.members.as_slice() else { panic!("one member") };
            assert!(member.findings.is_empty(), "{:?}", core::str::from_utf8(&member.messages));
        });
    }

    #[test]
    fn std_bstd_accepts_blaze_edition_workspace_member() {
        const ROOT_BSTD: &str = "[workspace]\nmembers = [\"app\"]\n\n[workspace.metadata.blaze]\nstd = \"bstd\"\n";
        const ROOT_RUST: &str = "[workspace]\nmembers = [\"app\"]\n\n[workspace.metadata.blaze]\nstd = \"rust\"\n";
        const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n";

        let mut bstd_out = ContentId([0; 32]);
        laid_of(&[("Cargo.toml", ROOT_BSTD), ("app/Cargo.toml", APP), ("app/src/lib.rs", "")], |laid| {
            assert!(laid.root.findings.is_empty(), "{:?}", core::str::from_utf8(&laid.root.messages));
            let [(_, member)] = laid.members.as_slice() else { panic!("one member") };
            assert!(member.findings.is_empty(), "{:?}", core::str::from_utf8(&member.messages));
            bstd_out = member.out;
        });

        laid_of(&[("Cargo.toml", ROOT_RUST), ("app/Cargo.toml", APP), ("app/src/lib.rs", "")], |laid| {
            let [(_, member)] = laid.members.as_slice() else { panic!("one member") };
            assert!(member.findings.is_empty(), "{:?}", core::str::from_utf8(&member.messages));
            assert_ne!(member.out, bstd_out, "member units out must depend on std policy even with 0 findings (rule 21)");
        });
    }

    #[test]
    fn std_bstd_skips_pure_proc_macro_workspace_member() {
        const ROOT: &str = "[workspace]\nmembers = [\"macros\", \"mixed\"]\n\n[workspace.metadata.blaze]\nstd = \"bstd\"\n";
        const MACROS: &str = "[package]\nname = \"macros\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n";
        const MIXED: &str = "[package]\nname = \"mixed\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n";

        laid_of(
            &[
                ("Cargo.toml", ROOT),
                ("macros/Cargo.toml", MACROS),
                ("macros/src/lib.rs", ""),
                ("mixed/Cargo.toml", MIXED),
                ("mixed/src/lib.rs", ""),
                ("mixed/src/main.rs", "fn main() {}"),
            ],
            |laid| {
                assert!(laid.root.findings.is_empty(), "{:?}", core::str::from_utf8(&laid.root.messages));
                let (_, macros) = laid.members.iter().find(|(name, _)| *name == Name::manifest(b"macros/Cargo.toml")).expect("macros member");
                assert!(macros.findings.is_empty(), "pure proc-macro must be skipped: {:?}", core::str::from_utf8(&macros.messages));
                let (_, mixed) = laid.members.iter().find(|(name, _)| *name == Name::manifest(b"mixed/Cargo.toml")).expect("mixed member");
                let [(rule, _, _, at, to)] = mixed.findings.as_slice() else {
                    panic!("proc-macro with a bin target must be refused, got {:?}", mixed.findings);
                };
                assert_eq!(*rule, STD_NOT_BSTD);
                assert_eq!(&mixed.messages[*at..*to], b"mixed\x1f2024");
            },
        );
    }

    #[test]
    fn std_bstd_allow_rust_permits_listed_member_and_refuses_others() {
        const ROOT: &str = "[workspace]\nmembers = [\"legacy\", \"other\"]\n\n[workspace.metadata.blaze]\nstd = \"bstd\"\nstd-allow-rust = [\"legacy\"]\n";
        const LEGACY: &str = "[package]\nname = \"legacy\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
        const OTHER: &str = "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

        laid_of(
            &[
                ("Cargo.toml", ROOT),
                ("legacy/Cargo.toml", LEGACY),
                ("legacy/src/lib.rs", ""),
                ("other/Cargo.toml", OTHER),
                ("other/src/lib.rs", ""),
            ],
            |laid| {
                assert!(laid.root.findings.is_empty(), "{:?}", core::str::from_utf8(&laid.root.messages));
                let (_, legacy) = laid.members.iter().find(|(name, _)| *name == Name::manifest(b"legacy/Cargo.toml")).expect("legacy member");
                assert!(legacy.findings.is_empty(), "{:?}", core::str::from_utf8(&legacy.messages));
                let (_, other) = laid.members.iter().find(|(name, _)| *name == Name::manifest(b"other/Cargo.toml")).expect("other member");
                let [(rule, _, _, at, to)] = other.findings.as_slice() else {
                    panic!("expected one STD_NOT_BSTD finding on other, got {:?}", other.findings);
                };
                assert_eq!(*rule, STD_NOT_BSTD);
                assert_eq!(&other.messages[*at..*to], b"other\x1f2021");
            },
        );
    }

    #[test]
    fn std_policy_malformed_values_emit_unread_on_root_manifest() {
        const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n";
        let cases = [
            (
                "[workspace]\nmembers = [\"app\"]\n\n[workspace.metadata.blaze]\nstd = \"foo\"\n",
                "\"foo\"",
                "[workspace.metadata.blaze] std",
            ),
            (
                "[workspace]\nmembers = [\"app\"]\n\n[workspace.metadata.blaze]\nstd = 42\n",
                "42",
                "[workspace.metadata.blaze] std",
            ),
            (
                "[workspace]\nmembers = [\"app\"]\n\n[workspace.metadata.blaze]\nstd = \"bstd\"\nstd-allow-rust = 123\n",
                "123",
                "[workspace.metadata.blaze] std-allow-rust",
            ),
            (
                "[workspace]\nmembers = [\"app\"]\n\n[workspace.metadata.blaze]\nstd = \"bstd\"\nstd-allow-rust = [\"ok\", 99]\n",
                "99",
                "[workspace.metadata.blaze] std-allow-rust",
            ),
        ];

        for (root_toml, expected_slice, expected_key) in cases {
            laid_of(&[("Cargo.toml", root_toml), ("app/Cargo.toml", APP), ("app/src/lib.rs", "")], |laid| {
                let [(rule, start, end, at, to)] = laid.root.findings.as_slice() else {
                    panic!("expected one UNREAD finding for `{root_toml}`, got {:?}", laid.root.findings);
                };
                let start = usize::try_from(*start).expect("start fits usize");
                let end = usize::try_from(*end).expect("end fits usize");
                let words = core::str::from_utf8(&laid.root.messages[*at..*to]).expect("UTF-8 message");
                assert_eq!(*rule, UNREAD);
                assert_eq!(&root_toml[start..end], expected_slice);
                assert!(words.contains(expected_key), "finding message should name key `{expected_key}`: {words}");
            });
        }
    }

    #[test]
    fn std_not_bstd_wording_exists_and_renders_cleanly() {
        use blaze_report::frame::words::{Area, Kind, say, wording};

        let row = wording(Area::Cli, STD_NOT_BSTD).expect("STD_NOT_BSTD wording registered in WORDINGS");
        assert_eq!(row.name, "std-not-bstd");
        assert_eq!(row.kind, Kind::Error);
        assert_eq!(row.args, 2);

        let alloc = blaze_own::testing::heap().alloc();
        let mut headline = blaze_own::AString::new_in(alloc);
        let mut label = blaze_own::AString::new_in(alloc);
        let name = say(Area::Cli, STD_NOT_BSTD, &["legacy", "2024"], &mut headline, &mut label).expect("wording renders with 2 args");
        assert_eq!(name, "std-not-bstd");
        assert!(headline.as_str().contains("`legacy`"), "headline: {}", headline.as_str());
        assert!(headline.as_str().contains("`2024`"), "headline: {}", headline.as_str());
        assert!(headline.as_str().contains("bstd"), "headline: {}", headline.as_str());
        assert!(label.as_str().contains("blaze-2026.11"), "label: {}", label.as_str());
        assert!(label.as_str().contains("[workspace.package]"), "label: {}", label.as_str());
    }
}
