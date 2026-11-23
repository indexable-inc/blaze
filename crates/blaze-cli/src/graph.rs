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
use blaze_own::{AVec, Alloc, Vec};
use blaze_unit_graph::{Ask, CfgAtom, NoResolver, Package, Records, Resolver, TargetDesc};
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::fetch::{FetchEnv, Lockfile};
use crate::work::{Did, Made, held_str};

const GRAPH_OUT_DOMAIN: &str = "blaze-cli unit graph out 1";

/// The finding number of a root's target the graph leaves out by its `required-features`
/// (blaze-unit-graph `LeftOut`): a note on the graph's record, counted as nothing (`Told`).
/// Its one argument is the whole sentence, naming the target and the features that are off.
pub(crate) const TARGET_SKIPPED: u16 = 46;

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
    text.into_bump_slice()
}

/// One checkout path package matched to its lock entry: a workspace member (`PathKind::Member`)
/// or a non-member path package (`PathKind::External`, from `[patch.*]` or excluded/transitive
/// path dependencies).
pub struct CheckoutDir {
    pub id: LockId,
    pub dir: blaze_unit_graph::Text<'static>,
    pub kind: blaze_world::PathKind,
}

/// The members (and non-member checkout path packages) as the layout read them, before the lock
/// is matched: each one's facts, its toolchain line dropped, and the workspace's resolver.
pub struct Members {
    packages: std::vec::Vec<Package<'static>, std::alloc::Global>,
    /// Each member's directory, anchor-relative (`""` for the root's own package), in
    /// `packages`' order: the names world reads each member's files under its own.
    dirs: Vec<blaze_unit_graph::Text<'static>, Alloc<'static>>,
    /// Non-member checkout path packages (`[patch.*]`, excluded/transitive path deps): each with
    /// its package facts, written version (if any), and checkout-relative directory.
    path_packages: Vec<(Package<'static>, Option<blaze_unit_graph::Text<'static>>, blaze_unit_graph::Text<'static>), Alloc<'static>>,
    resolver: Result<Resolver, NoResolver>,
    written: Option<std::string::String>,
}

impl Members {
    /// The facts of `units`' members and non-member path packages, on the process's `heap` (they
    /// live as long as the command).
    pub fn of(units: &layout::Units<'_>, heap: &'static blaze_own::Heap) -> Members {
        let mut packages = std::vec::Vec::with_capacity_in(units.members.len(), std::alloc::Global);
        let mut dirs = Vec::with_capacity_in(units.members.len(), heap.alloc());
        for member in units.members.iter() {
            let mut package = Package::from_member(member, heap.alloc());
            let blaze = member.edition.blaze.is_some();
            package.deps.retain(|dep| !toolchain_line(blaze, dep.key.as_str(), dep.package.as_str()));
            packages.push(package);
            dirs.push(blaze_unit_graph::Text::from_str_in(member.dir.as_str(), heap.alloc()));
        }
        let mut path_packages = Vec::with_capacity_in(units.path_packages.len(), heap.alloc());
        for pkg in units.path_packages.iter() {
            let mut package = Package::from_member(pkg, heap.alloc());
            let blaze = pkg.edition.blaze.is_some();
            package.deps.retain(|dep| !toolchain_line(blaze, dep.key.as_str(), dep.package.as_str()));
            let version = pkg.version.as_ref().map(|v| blaze_unit_graph::Text::from_str_in(v.as_str(), heap.alloc()));
            let dir = blaze_unit_graph::Text::from_str_in(pkg.dir.as_str(), heap.alloc());
            path_packages.push((package, version, dir));
        }
        let written = units.resolver.as_ref().map(|(text, _)| text.as_str());
        Members { packages, dirs, path_packages, resolver: Resolver::of(written, units.root_edition), written: written.map(std::string::String::from) }
    }

    /// Each member matched to its lock entry (a path entry of its name), and each non-member
    /// path package matched to its lock entry when the lock holds one (an unused `[patch]` entry
    /// is absent from `Cargo.lock` and skipped).
    pub fn locked(self, lock: &Lock<'_>) -> Result<Parts, Unlocked> {
        let resolver = self.resolver.map_err(|NoResolver| Unlocked::Resolver { written: self.written.unwrap_or_default() })?;
        let alloc = *self.dirs.allocator();
        let mut members = std::vec::Vec::with_capacity_in(self.packages.len(), std::alloc::Global);
        let mut path_packages = Vec::with_capacity_in(self.path_packages.len(), alloc);
        // On the allocator the members' directories were made on (`Members::of`'s heap).
        let mut dirs = Vec::with_capacity_in(self.packages.len() + self.path_packages.len(), alloc);
        let mut used: Vec<LockId, Alloc<'static>> = Vec::with_capacity_in(self.packages.len() + self.path_packages.len(), alloc);
        for (package, dir) in self.packages.into_iter().zip(self.dirs) {
            let mut ids = lock.named(package.name.as_str()).filter(|id| matches!(lock.package(*id).source, LockSource::Path));
            let found = (ids.next(), ids.next());
            drop(ids);
            match found {
                (Some(id), None) => {
                    used.push(id);
                    members.push((id, package));
                    dirs.push(CheckoutDir { id, dir, kind: blaze_world::PathKind::Member });
                }
                _ => return Err(Unlocked::Member { name: std::string::String::from(package.name.as_str()) }),
            }
        }
        for (package, version, dir) in self.path_packages {
            let mut matched = None;
            let mut count = 0usize;
            for id in lock.named(package.name.as_str()).filter(|id| matches!(lock.package(*id).source, LockSource::Path)) {
                if !used.contains(&id) {
                    matched = Some(id);
                    count += 1;
                }
            }
            if count > 1 {
                let want = version.as_deref().unwrap_or("0.0.0");
                matched = None;
                count = 0;
                let mut buf = blaze_unit_graph::Text::new_in(alloc);
                for id in lock.named(package.name.as_str()).filter(|id| matches!(lock.package(*id).source, LockSource::Path)) {
                    if used.contains(&id) {
                        continue;
                    }
                    buf.clear();
                    let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("{}", lock.package(id).version));
                    if buf.as_str() == want {
                        matched = Some(id);
                        count += 1;
                    }
                }
            }
            if let (Some(id), 1) = (matched, count) {
                used.push(id);
                path_packages.push((id, package));
                dirs.push(CheckoutDir { id, dir, kind: blaze_world::PathKind::External });
            }
        }
        Ok(Parts { members, path_packages, dirs, resolver })
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

/// The members (and matched non-member checkout path packages) matched to the lock, with the
/// resolver.
pub struct Parts {
    members: std::vec::Vec<(LockId, Package<'static>), std::alloc::Global>,
    path_packages: Vec<(LockId, Package<'static>), Alloc<'static>>,
    /// Each checkout path package's lock entry, directory (anchor-relative), and kind
    /// (workspace member or external path package).
    dirs: Vec<CheckoutDir, Alloc<'static>>,
    resolver: Resolver,
}

impl Parts {
    /// Each checkout path package's lock entry, directory (anchor-relative), and kind.
    pub fn member_dirs(&self) -> &[CheckoutDir] {
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
    /// Each checkout path package's lock entry, directory (anchor-relative), and kind: where
    /// the names world reads that package's files ([`Graph::member_dirs`]).
    dirs: Vec<CheckoutDir, Alloc<'static>>,
    /// The rustflags blaze does not apply, said on the graph's record ([`RUSTFLAGS_NOT_APPLIED`]).
    told: Option<&'f str>,
}

impl<'f> Graph<'f> {
    /// [`Graph::new`] for the targets [`target_of`] made: what of their rustflags blaze does not
    /// apply is said on the graph's record, never dropped (rule 3). Under no `--target` the host
    /// takes the target's flags, so the target's are said.
    pub fn of(parts: Parts, lockfile: &Lockfile<'f>, workspace_read: ContentId, target: Targeted, host: Targeted, tests: bool, heap: &'static blaze_own::Heap, frame: &'f Frame) -> Graph<'f> {
        let told = RustflagsTold(&target.flags);
        let told = told.any().then(|| held_str(bstd::mem::format_in!(frame, "{told}").as_str(), frame));
        let mut graph = Graph::new(parts, lockfile, workspace_read, target.desc, host.desc, tests, heap, frame);
        graph.told = told;
        graph
    }

    /// The job of `parts` over `lockfile`, for `target` and, for the host's units (build
    /// scripts), `host` (both from [`target_of`], each asked for its unit's kind); `workspace_read`
    /// is the units' source.
    pub fn new(parts: Parts, lockfile: &Lockfile<'f>, workspace_read: ContentId, target: TargetDesc<'static>, host: TargetDesc<'static>, tests: bool, heap: &'static blaze_own::Heap, frame: &'f Frame) -> Graph<'f> {
        let mut roots = Vec::with_capacity_in(parts.members.len(), heap.alloc());
        roots.extend(parts.members.iter().map(|(id, _)| *id));
        let ask = Ask { roots, features: Vec::new_in(heap.alloc()), all_features: false, no_default_features: false, tests };
        let members: &'f [(LockId, Package<'static>)] = std::boxed::Box::leak(std::boxed::Box::new_in(parts.members, frame));
        let path_packages: &'f [(LockId, Package<'static>)] = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(parts.path_packages, frame));
        let target: &'f TargetDesc<'static> = std::boxed::Box::leak(std::boxed::Box::new_in(target, frame));
        let host: &'f TargetDesc<'static> = std::boxed::Box::leak(std::boxed::Box::new_in(host, frame));
        let ask: &'f Ask<'static> = std::boxed::Box::leak(std::boxed::Box::new_in(ask, frame));
        let inputs = UnitGraphInputs { lock: lockfile.text, workspace_read, members, path_packages, resolver: parts.resolver, target, host, ask };
        Graph { source: unit_graph_task::source(&inputs), inputs, mapped: Vec::new_in(frame), dirs: parts.dirs, told: None }
    }

    /// Each checkout path package's lock entry, directory (anchor-relative), and kind.
    pub fn member_dirs(&self) -> &[CheckoutDir] {
        &self.dirs
    }

    #[cfg(test)]
    pub(crate) fn inputs(&self) -> &UnitGraphInputs<'f, 'static> {
        &self.inputs
    }

    /// The graph source is the standing proof's graph half. It is derived from the lock bytes,
    /// layout reads, target and host cfg, and the request ask by `unit_graph_task::source`.
    pub(crate) fn plan_source(&self) -> ContentId {
        self.source
    }
}

/// The finding number of the rustflags blaze does not apply (blaze-manifest rustflags.rs
/// `Chosen::not_applied`): a note on the graph's record, counted as nothing (`Told`). Its one
/// argument is the whole sentence: the flags and the source they came from.
pub(crate) const RUSTFLAGS_NOT_APPLIED: u16 = 47;

/// The checkout's cargo config, as cargo names it: `.cargo/config` is read over
/// `.cargo/config.toml` when both are there.
const CARGO_CONFIG: [&str; 2] = [".cargo/config", ".cargo/config.toml"];

/// A unit's target with the rustflags cargo gives it ([`target_of`]): its description, whose cfg
/// holds the flags' `--cfg`s (so its key moves with them, rule 21), and what was chosen.
pub struct Targeted {
    pub desc: TargetDesc<'static>,
    pub flags: blaze_manifest::rustflags::Chosen<'static>,
}

/// The `rustflags` of the checkout's cargo config (blaze-manifest rustflags.rs), each file read
/// a row of the layout (`rows`: the units' source, so an edit to one, or one made or removed,
/// lays the graph again): the config file cargo reads ([`CARGO_CONFIG`]), and its `include`s
/// (relative to `.cargo/`, merged under it in order; an absent one that is not `optional` is
/// refused). Not read yet, and so not applied: the config files of the folders above the
/// checkout and `$CARGO_HOME/config.toml`.
pub fn rustflags_config(
    cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, stamps: &impl crate::sweep::Stamps, rows: &mut crate::sweep::Rows<'_>, heap: &'static blaze_own::Heap, frame: &Frame,
) -> Result<blaze_manifest::rustflags::RustflagsConfig<'static>, CliError> {
    use blaze_manifest::rustflags::{RustflagsConfig, RustflagsError};
    // Kept for the command (the graph's inputs are made from it): on the process's heap.
    let alloc = heap.alloc();
    let refused = |why: &dyn core::fmt::Display| CliError::Program(crate::ManifestRefused::of(why, frame));
    let mut config = RustflagsConfig::new(alloc);
    let mut file = None;
    let mut text = bstd::mem::Bytes::new_in(frame, "a cargo config file");
    for path in CARGO_CONFIG {
        if crate::sweep::read_manifest(cx, root, stamps, path, rows, &mut text)? {
            file = Some(path);
            break;
        }
    }
    let Some(file) = file else { return Ok(config) };
    let text = core::str::from_utf8(text.as_slice()).map_err(|_| refused(&format_args!("{file}: not UTF-8")))?;
    for include in RustflagsConfig::includes(file, text, alloc).map_err(|error| refused(&error))?.iter() {
        let path = include.path.as_str();
        if path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err(refused(&format_args!("{file}: its `include` of `{path}` leaves `.cargo/`, which blaze does not read")));
        }
        let joined = bstd::mem::format_in!(frame, ".cargo/{path}");
        let mut included = bstd::mem::Bytes::new_in(frame, "an included cargo config file");
        if !crate::sweep::read_manifest(cx, root, stamps, joined.as_str(), rows, &mut included)? {
            match include.optional {
                true => continue,
                false => return Err(refused(&RustflagsError::IncludeMissing { file: blaze_own::String::from_str_in(file, alloc), path: blaze_own::String::from_str_in(path, alloc) })),
            }
        }
        let included = core::str::from_utf8(included.as_slice()).map_err(|_| refused(&format_args!("{}: not UTF-8", joined.as_str())))?;
        if !RustflagsConfig::includes(joined.as_str(), included, alloc).map_err(|error| refused(&error))?.is_empty() {
            return Err(refused(&RustflagsError::NestedInclude { file: blaze_own::String::from_str_in(joined.as_str(), alloc) }));
        }
        config.read(joined.as_str(), included).map_err(|error| refused(&error))?;
    }
    config.read(file, text).map_err(|error| refused(&error))?;
    Ok(config)
}

/// This build's target as the pinned toolchain states it, for a unit of `kind`, with the
/// rustflags cargo gives that unit. The toolchain's cfg atoms are read from its manifest on the
/// shelf under `env.home`; the unit's `cfg(panic)` is derived from the program's profile fact
/// `program` by blaze-world `panic::unit_panic` in place of the toolchain list's atom; and each
/// `--cfg` of the rustflags (blaze-manifest rustflags.rs `choose`, over the checkout's `config`
/// and the command's variables, `env.rustflags`) is an atom of the unit's cfg. blaze takes no
/// `--target`, so a build script's unit takes the flags as a library's does (cargo's rule
/// without `--target`; the rule's twin is tested in blaze-manifest rustflags_tests.rs).
pub fn target_of(
    cx: &Cx, env: &crate::fetch::FetchEnv<'_>, config: &blaze_manifest::rustflags::RustflagsConfig<'_>, program: blaze_manifest::layout::panic::ProfilePanic, kind: &blaze_unit_graph::UnitKind<'_>, frame: &Frame,
) -> Result<Targeted, CliError> {
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
    let refused = |why: &dyn core::fmt::Display| CliError::Program(crate::ManifestRefused::of(why, frame));
    // The build script's run target is this machine's (the unit graph's host today).
    let cuts = blaze_world::panic::Cuts::read(view, TRIPLE).map_err(|e| refused(&e))?;
    let panic = blaze_world::panic::unit_panic(program, blaze_world::Side::Target, kind, &cuts).map_err(|e| refused(&e))?;
    let atoms = view.cfg(TRIPLE).ok_or(CliError::Text { what: "the pinned toolchain was not cut for this machine's target" })?;
    let mut listed = Vec::new_in(frame);
    listed.extend(atoms);
    // What `rustc --print cfg` says for the target, which cargo matches `target.'cfg(..)'` against.
    let unit = blaze_manifest::rustflags::Unit { target_given: false, host_unit: matches!(kind, blaze_unit_graph::UnitKind::BuildScript | blaze_unit_graph::UnitKind::ProcMacro) };
    // Kept for the command (the graph's inputs): on the process's heap.
    let heap = env.heap.alloc();
    let flags = blaze_manifest::rustflags::choose(config, env.rustflags, TRIPLE, &listed, unit, heap).map_err(|error| refused(&error))?;
    let mut cfg = Vec::new_in(heap);
    for &(name, value) in listed.iter() {
        if name == blaze_world::panic::CFG_KEY {
            continue;
        }
        cfg.push(match value {
            Some(value) => CfgAtom::pair(name, value, heap),
            None => CfgAtom::flag(name, heap),
        });
    }
    cfg.push(CfgAtom::pair(blaze_world::panic::CFG_KEY, blaze_world::panic::cfg_value(panic), heap));
    for (name, value) in flags.cfg.iter() {
        cfg.push(match value {
            Some(value) => CfgAtom::pair(name.as_str(), value.as_str(), heap),
            None => CfgAtom::flag(name.as_str(), heap),
        });
    }
    Ok(Targeted { desc: TargetDesc::new(TRIPLE, cfg, heap), flags })
}

/// The note of the rustflags a unit takes that blaze does not apply ([`RUSTFLAGS_NOT_APPLIED`]),
/// and of `target.'cfg(..)'` tables that did not settle in cargo's two turns.
struct RustflagsTold<'c, 'a>(&'c blaze_manifest::rustflags::Chosen<'a>);

impl RustflagsTold<'_, '_> {
    /// Whether there is anything to say.
    fn any(&self) -> bool {
        !self.0.not_applied.is_empty() || !self.0.settled
    }
}

impl core::fmt::Display for RustflagsTold<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let flags = self.0;
        if !flags.not_applied.is_empty() {
            write!(f, "the rustflags of {} hold {} flag(s) blaze does not apply:", flags.from, flags.not_applied.len())?;
            for flag in flags.not_applied.iter() {
                write!(f, " `{}`", flag.as_str())?;
            }
            write!(f, "; their {} `--cfg`(s) are in every unit's cfg", flags.cfg.len())?;
        }
        if !flags.settled {
            let after = if flags.not_applied.is_empty() { "" } else { "; " };
            write!(f, "{after}the `target.'cfg(..)'` rustflags tables did not settle in cargo's two turns (cargo warns the same): the second turn's flags are taken")?;
        }
        Ok(())
    }
}

/// The records of the `Fetch` jobs the task was handed, mapped from `<home>/.blaze/v2/sources/` once a
/// command and read in place (never read whole).
struct Mapped<'s, 'f> {
    /// The subjects of the handed `Fetch` jobs that hold their sources, sorted.
    handed: &'s [ContentId],
    mapped: &'s mut Vec<(ContentId, &'f [u8]), &'f Frame>,
    /// Maps one record from the sources store (fetch.rs `Lockfile::map_source`).
    map: &'s mut dyn FnMut(&ContentId) -> Result<blaze_core::Mapping, CliError>,
    frame: &'f Frame,
    /// The first record that could not be mapped: the store's fault.
    error: &'s mut Option<CliError>,
}

impl<'f> Records<'f> for Mapped<'_, 'f> {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'f [u8]> {
        let key = source.key();
        self.handed.binary_search(&key).ok()?;
        let at = match self.mapped.binary_search_by(|(held, _)| held.cmp(&key)) {
            Ok(at) => return Some(self.mapped[at].1),
            Err(at) => at,
        };
        match (self.map)(&key) {
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
    // task wrote its unit's record: never the checkout. Mapped through the fetch's own
    // directory (fetch.rs `Lockfile::map_source`).
    let lock = lockfile.lock;
    let mut map = |key: &ContentId| lockfile.map_source(cx, env, key);
    let mut unmapped = None;
    let records = Mapped { handed: &handed, mapped: &mut graph.mapped, map: &mut map, frame, error: &mut unmapped };
    let unheld = Unheld { failed: &failed, refused: &refused };
    let entry = unit_graph_task::run(&graph.inputs, lock, records, PackView::open_source_put, unheld, env.heap.alloc());
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
            hand_graph(&mut record, made.record)?;
            record.out = ContentId::of(GRAPH_OUT_DOMAIN, record.payload_bytes()?);
            // One `MacroModule` part per proc-macro crate (macros.rs): the module lookups.
            match crate::macros::parts(graph.source, record.payload_bytes()?, env.heap.alloc()) {
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
            // A target left out by its `required-features` is said, never quietly dropped
            // (rule 3): turso's bench `logical_log_serialization` (`bench`), example
            // `sync_example` (`sync`).
            for left in made.left_out.iter() {
                let words = bstd::mem::format_in!(held, "{left}");
                record.find(TARGET_SKIPPED, (0, 0), &[words.as_str()]);
            }
            // The rustflags blaze does not apply, said (rule 3): graph.rs `Graph::of`.
            if let Some(told) = graph.told {
                record.find(RUSTFLAGS_NOT_APPLIED, (0, 0), &[told]);
            }
            Ok(Did::Done(record))
        }
        Err(error) => {
            let words = bstd::mem::format_in!(held, "the dependency graph: {error}");
            Ok(Did::FailedSaying(TaskFailure { name, why: Why::Refused { what: "the dependency graph could not be resolved", rule: "unit_graph_task" } }, held_str(words.as_str(), held)))
        }
    }
}

/// Hands the graph's record `bytes` to its task's record `record` where the resolution made
/// them, on the heap, moved, never copied into the task's frame (rules 11 and 51): a
/// workspace's graph (tempo's 1,072 crates) is then bounded by the record, not by the frame, as
/// `core`'s 76 MB scope payload met its 64 MiB frame (2026-11-16 23:11). Bytes made over any
/// allocator but the heap's handle are refused (rule 3), never copied to the heap.
fn hand_graph(record: &mut Made<'_>, bytes: AVec<'_, u8>) -> Result<(), CliError> {
    let Ok(bytes) = bytes.into_global() else {
        return Err(CliError::Text { what: "the dependency graph's record was not made on the heap, so it cannot be handed whole to its task's record (graph.rs `hand_graph`)" });
    };
    record.hand(bytes)
}

#[cfg(test)]
mod tests {
    /// A graph record larger than its task's frame is handed to the task's record, not copied:
    /// the record reads the graph's exact bytes at the address the resolution made them, the
    /// frame grew by 0, and bytes made over another allocator (here `Global` in an `Erased` of
    /// its own) are refused, never copied. CONTROL: the copy this replaced
    /// (`record.payload.extend_from_slice(&made.record)`) panics here, the frame refusing the
    /// bytes; a copy onto the heap instead fails the address check. Prints
    /// `graph record handed: 1048576 bytes, frame grew by 0`.
    #[test]
    fn a_graph_record_larger_than_the_task_frame_is_handed_not_copied() {
        use bstd::mem::{Frame, Tag};
        use std::alloc::Global;
        const FRAME: usize = 64 << 10;
        const LEN: usize = 1 << 20;
        let bytes: std::vec::Vec<u8> = (0..LEN).map(|i| (i * 31 % 251) as u8).collect();
        let heap = blaze_own::testing::heap();
        let graph = blaze_own::Vec::from_slice_in(&bytes, heap.alloc());
        let at = graph.as_ptr();
        let frame = Frame::with_reserve(FRAME, Tag::Other).expect("a test frame");
        assert!(LEN > frame.reserved(), "the record ({LEN} bytes) is larger than the frame ({} bytes)", frame.reserved());
        let name = blaze_drive::Name::unit_graph();
        let mut made = crate::work::Made::new_in(name, &frame);
        let before = frame.used();
        super::hand_graph(&mut made, graph).expect("the graph's record handed");
        let grew = frame.used() - before;
        assert!(made.payload_bytes().unwrap() == &bytes[..], "the record reads the graph's exact bytes");
        assert_eq!(made.payload_bytes().unwrap().as_ptr(), at, "the record reads the graph where it was made: it was copied");
        assert_eq!(grew, 0, "the task's frame grew by the graph's record");
        let other = blaze_own::Erased::new(Global);
        let mut refused = crate::work::Made::new_in(name, &frame);
        assert!(super::hand_graph(&mut refused, blaze_own::Vec::from_slice_in(&bytes[..16], other.handle())).is_err(), "bytes over another allocator were taken");
        assert!(refused.payload_bytes().unwrap().is_empty(), "a refused hand holds no payload");
        println!("graph record handed: {} bytes, frame grew by {grew}", made.payload_bytes().unwrap().len());
        // A released payload is refused, never read as empty bytes (rule 76).
        let freed = made.payload_bytes().unwrap().len();
        assert_eq!(made.release_handed(), freed, "the release frees the handed bytes");
        assert!(made.payload_bytes().is_err(), "a released payload is refused");
        assert_eq!(made.release_handed(), 0, "a second release frees nothing");
    }
}
