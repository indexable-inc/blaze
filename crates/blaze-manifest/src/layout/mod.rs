//! A workspace's layout from manifest texts, sans-IO: which packages are its members, each
//! one's library and binary targets with their root files, its dependencies (in the
//! workspace, by path, from a registry or from git), its features and its edition, every one
//! with the byte range of its line in its manifest.
//!
//! The reader opens no file and knows no absolute path. Every path is relative to an anchor
//! the caller chose (a checkout's root, say), `/`-separated, with no `.` or `..` left; `""` is
//! the anchor. The reader says what it needs as typed requests ([`Need`]): a manifest's text,
//! or a directory's entries. The caller answers each (in any order, in parallel if it likes)
//! and asks again; one [`Layout::pass`] asks for everything it can see it needs, so the number
//! of passes is the depth of the search (the root above, the members' globs, path
//! dependencies, the target directories), not the number of files.
//!
//! ```text
//! let mut layout = Layout::new("crates/a", alloc);
//! let units = loop {
//!     match layout.pass()? {
//!         Pass::Done(units) => break units,
//!         Pass::Need(needs) => for need in needs.iter() {
//!             match need {
//!                 Need::Manifest(path) => layout.manifest(path, read(path))?, // Some(text) or None
//!                 Need::Listing(dir) => layout.listing(dir, list(dir)),      // Some(entries) or None
//!             }
//!         },
//!     }
//! };
//! ```
//!
//! What it reads as cargo does (the pinned nightly's `src/workspace` and `util/toml/targets`):
//! the root is the start's manifest when it has `[workspace]`, else the one
//! `package.workspace` names, else the nearest one above (up to the anchor) with
//! `[workspace]` whose `exclude` does not leave the start out; members are `members`' globs
//! (only directories match, in name order; a match with no manifest is an error) in order,
//! each followed by its path dependencies under the root's directory, depth first, then the
//! root's own package, all but those `exclude` leaves out; `edition` and dependencies may be
//! inherited with `workspace = true`. The library is `[lib]`, else `src/lib.rs` when it
//! exists; binaries are the `[[bin]]` tables and, unless `autobins = false` (or, in the 2015
//! edition, unless any `[[bin]]` is written), `src/main.rs` (named as the package),
//! `src/bin/<name>.rs` and `src/bin/<name>/main.rs`.
//!
//! Integration tests, examples and benches are read in `extra.rs` ([`ExtraTarget`]: `tests/`,
//! `examples/`, `benches/` and their tables).
//!
//! Not read here, and so never wrong here:
//! `default-members`; a path dependency outside the root's directory is not a member (cargo
//! would look for that directory's own root). The caller stops the search above at the anchor:
//! cargo stops after `CARGO_HOME` and at `target/package`, which the caller chooses the anchor
//! by.

mod extra;
pub mod meta;
#[cfg(test)]
mod meta_tests;
pub mod panic;
pub mod profile;
mod raw;
#[cfg(test)]
mod tests;

use core::fmt;

use blaze_own::{Alloc, SortedMap, String, Vec};
use edition::{NoEdition, PackageEdition};

use raw::{Raw, RawBuild, RawDep, RawEdition, RawMeta, RawPackage, RawTarget, RawVersion, RawWorkspace};

pub use extra::{ExtraKind, ExtraTarget, RefusedTarget, TargetRefused};
pub use meta::{META_KEYS, PackageMeta, PackageMetaVars};
pub use raw::GitRef;

/// Text in the caller's allocator.
pub type Text<'a> = String<Alloc<'a>>;

/// A manifest read, by its index in [`Units::manifests`] (and the order the reader was handed
/// them in).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ManifestId(pub u32);

/// A byte range in one manifest: a line, from its start to the end of the value (or header)
/// on it, so a finding can be placed there.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Span {
    pub manifest: ManifestId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    fn new(manifest: ManifestId, start: usize, end: usize) -> Span {
        // A manifest past `u32::MAX` bytes is refused by the TOML reader's limit.
        let narrow = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Span { manifest, start: narrow(start), end: narrow(end) }
    }

    pub fn range(self) -> core::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// Which table a dependency is in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DepKind {
    Normal,
    Dev,
    Build,
}

/// A request the reader makes of its caller.
#[derive(Clone)]
pub enum Need<'a> {
    /// The text of the manifest at this path (`dir/Cargo.toml`): answer with
    /// [`Layout::manifest`], `None` when there is no such file.
    Manifest(Text<'a>),
    /// The entries of the directory at this path: answer with [`Layout::listing`], `None` when
    /// there is no such directory.
    Listing(Text<'a>),
}

/// One entry of a directory listing.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'e> {
    pub name: &'e str,
    pub is_dir: bool,
}

/// What a pass came to.
pub enum Pass<'a> {
    /// The answers it lacks; every one of them, as far as it could see.
    Need(Vec<Need<'a>, Alloc<'a>>),
    Done(Units<'a>),
}

/// A crate target: its name, its root file (anchor-relative), and its line: the `[lib]` or
/// `[[bin]]` table, or for a target found by its file the `[package]` header (`found`).
#[derive(Clone)]
pub struct Target<'a> {
    pub name: Text<'a>,
    pub root: Text<'a>,
    pub span: Span,
    /// Found by its file (`src/lib.rs`, `src/main.rs`, `src/bin/..`), not written.
    pub found: bool,
    pub proc_macro: bool,
    /// A `[[bin]]`'s `required-features`, as written; empty for every other target (a library
    /// or build script has none). The Cargo Book, "Cargo Targets", "The `required-features`
    /// field": "The `required-features` field specifies which features the target needs in
    /// order to be built. If any of the required features are not enabled, the target will be
    /// skipped." Judged where features are known (blaze-unit-graph).
    pub required_features: Vec<Text<'a>, Alloc<'a>>,
}

/// Where a dependency comes from.
#[derive(Clone)]
pub enum Source<'a> {
    /// A member of this workspace: its index in [`Units::members`].
    Member(usize),
    /// A directory by path, outside the workspace's members (anchor-relative).
    Path(Text<'a>),
    /// A registry: the requirement (`None` for none written, which cargo reads as `*`), and the
    /// registry when it is not crates.io.
    Registry { req: Option<Text<'a>>, registry: Option<Text<'a>> },
    Git { url: Text<'a>, reference: GitRef<'a> },
    /// A path dependency whose directory is absolute or leaves the anchor: the path as
    /// written (relative to the manifest's directory, or for an inherited one the root's). The
    /// reader asks for nothing there and it is never a member; the caller decides what it is.
    Outside(Text<'a>),
}

/// A dependency of a member.
#[derive(Clone)]
pub struct Dependency<'a> {
    /// The name the code calls it by (the key).
    pub name: Text<'a>,
    /// The package's own name (`package = ".."`, else the key).
    pub package: Text<'a>,
    pub kind: DepKind,
    /// `[target.<this>.dependencies]`.
    pub target: Option<Text<'a>>,
    pub source: Source<'a>,
    pub optional: bool,
    pub default_features: bool,
    pub features: Vec<Text<'a>, Alloc<'a>>,
    /// The member's own line, also for an inherited one.
    pub span: Span,
}

/// A feature: its name, what it turns on as written, and its line.
#[derive(Clone)]
pub struct Feature<'a> {
    pub name: Text<'a>,
    pub enables: Vec<Text<'a>, Alloc<'a>>,
    pub span: Span,
}

impl<'a> Feature<'a> {
    fn clone_in(&self, alloc: Alloc<'a>) -> Feature<'a> {
        Feature { name: self.name.clone_in(alloc), enables: self.enables.clone_in(alloc), span: self.span }
    }
}

/// A member package.
pub struct Member<'a> {
    /// Its directory, anchor-relative.
    pub dir: Text<'a>,
    pub manifest: ManifestId,
    pub name: Text<'a>,
    /// The `[package]` header's line.
    pub span: Span,
    pub edition: PackageEdition,
    /// The `edition` line, absent when none is written (2015).
    pub edition_span: Option<Span>,
    pub lib: Option<Target<'a>>,
    pub bins: Vec<Target<'a>, Alloc<'a>>,
    /// Integration tests, examples and benches, sorted by kind, then name bytes; empty unless
    /// read with [`Layout::with_test_targets`].
    pub extras: Vec<ExtraTarget<'a>, Alloc<'a>>,
    /// Integration tests, examples and benches the rules refuse (a missing file, a path out of
    /// the package, no name, a second of one name): findings on this manifest, left out of
    /// `extras`; sorted by line. Empty unless read with [`Layout::with_test_targets`].
    pub refused_targets: Vec<RefusedTarget<'a>, Alloc<'a>>,
    pub dependencies: Vec<Dependency<'a>, Alloc<'a>>,
    /// Every feature, the implicit ones of optional dependencies included (an optional
    /// dependency no feature names as `dep:<name>`, in every edition).
    pub features: Vec<Feature<'a>, Alloc<'a>>,
    /// The build script: `package.build = "path"`, else `build.rs` in the package's directory
    /// when it is there (a found one is on the `[package]` line), none for `build = false`.
    pub build: Option<Target<'a>>,
    /// `package.links`.
    pub links: Option<Text<'a>>,
    /// `package.version`, the root's `workspace.package.version` when inherited; `None` when
    /// none is written (cargo reads that as `0.0.0`).
    pub version: Option<Text<'a>>,
    /// The `[package]` metadata cargo gives each crate of the package as `CARGO_PKG_*`
    /// (meta.rs), an inherited key the root's `[workspace.package]` value.
    pub meta: PackageMeta<'a>,
    /// The `[patch.<source>]` source table name(s) that directly named this path package in the
    /// workspace root manifest (e.g. `"crates-io"` or a git URL); empty for ordinary members and
    /// unpatched path dependencies.
    pub patched_from: Vec<Text<'a>, Alloc<'a>>,
}

/// A workspace's layout.
pub struct Units<'a> {
    /// Every manifest read, by [`ManifestId`]: its anchor-relative path.
    pub manifests: Vec<Text<'a>, Alloc<'a>>,
    /// The root's manifest; the start's own when it is in no workspace.
    pub root: ManifestId,
    /// In cargo's order (`cargo metadata`'s `workspace_members`).
    pub members: Vec<Member<'a>, Alloc<'a>>,
    /// Non-member path packages in the checkout discovered from the root's `[patch.*]` tables,
    /// excluded path dependencies of workspace members, and their transitive normal/build path
    /// dependencies, sorted by directory; empty unless read with [`Layout::with_path_packages`].
    pub path_packages: Vec<Member<'a>, Alloc<'a>>,
    /// The resolver as written: the root's `workspace.resolver`, else the root package's
    /// `package.resolver`; `None` when neither is written (the default is by edition, which
    /// the feature rules decide, not the reader).
    pub resolver: Option<(Text<'a>, Span)>,
    /// The root's own package's edition when the root is a package; `None` for a virtual root.
    pub root_edition: Option<PackageEdition>,
}

/// Why no layout: each names the place.
pub enum LayoutError<'a> {
    /// The manifest is no TOML.
    Toml { manifest: Text<'a>, error: blaze_toml::Error },
    /// A value of the wrong kind.
    Expected { at: Span, key: &'static str, kind: &'static str },
    /// No manifest where one must be: the start's, a member's, a path dependency's in the
    /// workspace, or the root `package.workspace` names.
    NoManifest { path: Text<'a> },
    /// `[package]` with no `name`.
    NoPackageName { at: Span },
    /// A member's manifest with no `[package]`.
    NotAPackage { manifest: Text<'a> },
    /// The manifest `package.workspace` names has no `[workspace]`.
    NotARoot { at: Span },
    Edition { at: Span, why: NoEdition },
    /// `workspace = true` for something the root's `[workspace]` does not offer.
    NotInherited { at: Span, key: &'static str },
    /// A path that leaves the anchor, or is absolute, where one cannot: a `members` glob, a
    /// target's `path`, `package.workspace`. (A path dependency there is [`Source::Outside`].)
    OutsideAnchor { at: Span },
    /// A `members` glob that is no pattern.
    Pattern { at: Span, error: blaze_glob::PatternError },
    /// A `**` in a `members` glob: not read here.
    RecursiveGlob { at: Span },
    /// The start is under the root and not one of its members.
    NotAMember { start: Text<'a>, root: Text<'a> },
    /// A `[[bin]]` with no `path` whose file is in none of the places cargo looks, or a `[lib]`
    /// with no `path` and no `src/lib.rs`, or a target with no name to take.
    NoTargetFile { at: Span },
    /// Two members of one name.
    DuplicateName { first: Span, second: Span },
    /// More than one of `branch`, `tag`, `rev`.
    GitReferences { at: Span },
    GitAndPath { at: Span },
    /// `[dev_dependencies]` and `[build_dependencies]` from the 2024 edition on.
    Underscored { at: Span },
    /// An integration test, example or bench refused: the package, the kind, the target (empty
    /// for a table with no name), its line, and the rule.
    Target { package: Text<'a>, kind: ExtraKind, target: Text<'a>, at: Span, why: TargetRefused<'a> },
}

impl fmt::Display for LayoutError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = |f: &mut fmt::Formatter<'_>, span: &Span| write!(f, "manifest {} bytes {}..{}: ", span.manifest.0, span.start, span.end);
        match self {
            LayoutError::Toml { manifest, error } => write!(f, "{}: {error}", manifest.as_str()),
            LayoutError::Expected { at: span, key, kind } => {
                at(f, span)?;
                write!(f, "`{key}` must be {kind}")
            }
            LayoutError::NoManifest { path } => write!(f, "{}: no such manifest", path.as_str()),
            LayoutError::NoPackageName { at: span } => {
                at(f, span)?;
                f.write_str("`[package]` has no `name`")
            }
            LayoutError::NotAPackage { manifest } => write!(f, "{}: a member's manifest has no `[package]`", manifest.as_str()),
            LayoutError::NotARoot { at: span } => {
                at(f, span)?;
                f.write_str("`package.workspace` names a manifest with no `[workspace]`")
            }
            LayoutError::Edition { at: span, why } => {
                at(f, span)?;
                write!(f, "the edition is {why}")
            }
            LayoutError::NotInherited { at: span, key } => {
                at(f, span)?;
                write!(f, "`{key}` is inherited with `workspace = true`, and the root's `[workspace]` does not offer it")
            }
            LayoutError::OutsideAnchor { at: span } => {
                at(f, span)?;
                f.write_str("the path leaves the directory the workspace is read under, or is absolute")
            }
            LayoutError::Pattern { at: span, error } => {
                at(f, span)?;
                write!(f, "{error}")
            }
            LayoutError::RecursiveGlob { at: span } => {
                at(f, span)?;
                f.write_str("`**` in `workspace.members` is not read by blaze")
            }
            LayoutError::NotAMember { start, root } => write!(
                f,
                "{}: the package is under the workspace root {} and not one of its members",
                start.as_str(),
                root.as_str()
            ),
            LayoutError::NoTargetFile { at: span } => {
                at(f, span)?;
                f.write_str("no root file for this target where cargo looks")
            }
            LayoutError::DuplicateName { first, second } => {
                at(f, second)?;
                write!(f, "a second member of the name the member at manifest {} has", first.manifest.0)
            }
            LayoutError::GitReferences { at: span } => {
                at(f, span)?;
                f.write_str("only one of `branch`, `tag` or `rev` may be given")
            }
            LayoutError::GitAndPath { at: span } => {
                at(f, span)?;
                f.write_str("a dependency is either by `git` or by `path`")
            }
            LayoutError::Underscored { at: span } => {
                at(f, span)?;
                f.write_str("the underscored table name is refused from the 2024 edition on; use the dashed one")
            }
            LayoutError::Target { package, kind, target, at: span, why } => {
                at(f, span)?;
                write!(f, "package `{}`: {} target `{}`: {why}", package.as_str(), kind.table(), target.as_str())
            }
        }
    }
}

impl fmt::Debug for LayoutError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl core::error::Error for LayoutError<'_> {}

/// An answer to a manifest request.
enum Answer<'a> {
    Missing,
    Read(Raw<'a>),
}

/// The reader: the answers so far.
pub struct Layout<'a> {
    alloc: Alloc<'a>,
    start: Text<'a>,
    /// Manifests answered, in the order answered (their ids).
    manifests: Vec<(Text<'a>, Answer<'a>), Alloc<'a>>,
    /// Path to its index in `manifests`.
    by_path: SortedMap<Text<'a>, u32, Alloc<'a>>,
    /// Directory to its entries, sorted by name (`None`: no such directory).
    listings: SortedMap<Text<'a>, Option<Vec<(Text<'a>, bool), Alloc<'a>>>, Alloc<'a>>,
    /// Whether the members' integration tests, examples and benches are read
    /// ([`Layout::with_test_targets`]); off by default.
    test_targets: bool,
    /// Whether non-member path packages (`[patch.*]`, excluded member path dependencies, and
    /// their transitive normal/build path dependencies) are read ([`Layout::with_path_packages`]);
    /// off by default.
    path_packages: bool,
}

impl<'a> Layout<'a> {
    /// The same reader, also finding and checking each member's integration tests, examples and
    /// benches ([`Member::extras`]). Only for the workspace the user builds: a dependency is
    /// published without `tests/`, `examples/` and `benches/` as a matter of course and its
    /// targets of those kinds are never built, so a dependency's layout is read with [`Layout::new`]
    /// alone, which finds none of them and reads their tables for shape only.
    pub fn with_test_targets(mut self) -> Layout<'a> {
        self.test_targets = true;
        self
    }

    /// The same reader, also reading non-member path packages reached from the root manifest's
    /// `[patch.*]` tables, excluded `path` dependencies of workspace members, and their transitive
    /// normal/build `path` dependencies ([`Units::path_packages`]). Only for the workspace the user
    /// builds: cargo ignores `[patch]` in fetched dependencies.
    pub fn with_path_packages(mut self) -> Layout<'a> {
        self.path_packages = true;
        self
    }

    /// A reader of the workspace of the package in `start` (anchor-relative; normalized here).
    /// A `start` that leaves the anchor is refused.
    pub fn new(start: &str, alloc: Alloc<'a>) -> Result<Layout<'a>, LayoutError<'a>> {
        let start = match join("", start, alloc) {
            Some(start) => start,
            None => return Err(LayoutError::NoManifest { path: String::from_str_in(start, alloc) }),
        };
        Ok(Layout {
            alloc,
            start,
            manifests: Vec::new_in(alloc),
            by_path: SortedMap::with_capacity_in(0, alloc),
            listings: SortedMap::with_capacity_in(0, alloc),
            test_targets: false,
            path_packages: false,
        })
    }

    /// The text of the manifest at `path` (as a [`Need::Manifest`] named it), `None` when there
    /// is none; read now, so a manifest that is no TOML is refused here.
    pub fn manifest(&mut self, path: &str, text: Option<&str>) -> Result<(), LayoutError<'a>> {
        if self.by_path.contains_key(path) {
            return Ok(());
        }
        let id = ManifestId(u32::try_from(self.manifests.len()).unwrap_or(u32::MAX));
        let answer = match text {
            Some(text) => Answer::Read(raw::read(text, id, path, self.alloc)?),
            None => Answer::Missing,
        };
        let path = String::from_str_in(path, self.alloc);
        self.by_path.insert(path.clone_in(self.alloc), id.0);
        self.manifests.push((path, answer));
        Ok(())
    }

    /// The entries of the directory `dir` (as a [`Need::Listing`] named it), `None` when there
    /// is no such directory.
    pub fn listing(&mut self, dir: &str, entries: Option<&[Entry<'_>]>) {
        let entries = entries.map(|entries| {
            let mut out = Vec::with_capacity_in(entries.len(), self.alloc);
            for entry in entries {
                out.push((String::from_str_in(entry.name, self.alloc), entry.is_dir));
            }
            // glob reads a directory's entries in name order; so do members and binaries here.
            out.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            out
        });
        self.listings.insert(String::from_str_in(dir, self.alloc), entries);
    }

    /// Everything the answers so far decide: the layout, or every request the reader can see
    /// it needs.
    pub fn pass(&self) -> Result<Pass<'a>, LayoutError<'a>> {
        let mut p = Passing { layout: self, needs: Vec::new_in(self.alloc) };
        let units = p.run()?;
        match (units, p.needs.is_empty()) {
            (Some(units), true) => Ok(Pass::Done(units)),
            (_, _) => Ok(Pass::Need(p.needs)),
        }
    }
}

/// A manifest looked up: not asked yet, missing, or read.
enum Look<'l, 'a> {
    Pending,
    Missing,
    Read(ManifestId, &'l Raw<'a>),
}

/// One pass: the answers, and the requests it makes.
struct Passing<'l, 'a> {
    layout: &'l Layout<'a>,
    needs: Vec<Need<'a>, Alloc<'a>>,
}

/// Whether a package read into a [`Member`] is a workspace member or a non-member path package.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MemberRole {
    Workspace,
    PathPackage,
}

/// A member found: its directory, and the manifest read.
struct Found<'l, 'a> {
    dir: Text<'a>,
    id: ManifestId,
    package: &'l RawPackage<'a>,
}

impl<'l, 'a> Passing<'l, 'a> {
    fn alloc(&self) -> Alloc<'a> {
        self.layout.alloc
    }

    fn look(&mut self, manifest: &str) -> Look<'l, 'a> {
        let layout = self.layout;
        match layout.by_path.get(manifest) {
            Some(&id) => match &layout.manifests[id as usize].1 {
                Answer::Missing => Look::Missing,
                Answer::Read(raw) => Look::Read(ManifestId(id), raw),
            },
            None => {
                let already = self.needs.iter().any(|need| matches!(need, Need::Manifest(path) if path.as_str() == manifest));
                if !already {
                    self.needs.push(Need::Manifest(String::from_str_in(manifest, self.alloc())));
                }
                Look::Pending
            }
        }
    }

    /// The entries of `dir`: `None` when not answered yet (asked now), `Some(None)` for no such
    /// directory.
    fn list(&mut self, dir: &str) -> Option<Option<&'l [(Text<'a>, bool)]>> {
        let layout = self.layout;
        match layout.listings.get(dir) {
            Some(entries) => Some(entries.as_ref().map(|it| it.as_slice())),
            None => {
                let already = self.needs.iter().any(|need| matches!(need, Need::Listing(path) if path.as_str() == dir));
                if !already {
                    self.needs.push(Need::Listing(String::from_str_in(dir, self.alloc())));
                }
                None
            }
        }
    }

    /// Asks for `dir/src` and `dir/src/bin` with the package's manifest, so its targets are
    /// known a pass sooner; a directory that is no package is answered `None` at little cost.
    fn prefetch(&mut self, dir: &str, role: MemberRole) {
        let alloc = self.alloc();
        let src = file(dir, "src", alloc);
        let bin = file(&src, "bin", alloc);
        // The package's own directory: whether `build.rs` is there.
        let _ = self.list(dir);
        let _ = self.list(&src);
        let _ = self.list(&bin);
        // `tests/`, `examples/`, `benches/`: the integration tests, examples and benches, when
        // they are read at all (workspace members only).
        if role == MemberRole::Workspace && self.layout.test_targets {
            for kind in ExtraKind::ALL {
                let _ = self.list(&file(dir, kind.dir(), alloc));
            }
        }
    }

    fn manifest_of(&self, dir: &str) -> Text<'a> {
        let mut path = String::from_str_in(dir, self.alloc());
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str("Cargo.toml");
        path
    }

    fn missing(&self, path: &str) -> LayoutError<'a> {
        LayoutError::NoManifest { path: String::from_str_in(path, self.alloc()) }
    }

    /// `None` when some answer is still to come.
    fn run(&mut self) -> Result<Option<Units<'a>>, LayoutError<'a>> {
        let alloc = self.alloc();
        let start = self.layout.start.clone_in(alloc);
        let start_manifest = self.manifest_of(&start);
        self.prefetch(&start, MemberRole::Workspace);
        let (start_id, start_raw) = match self.look(&start_manifest) {
            Look::Pending => return Ok(None),
            Look::Missing => return Err(self.missing(&start_manifest)),
            Look::Read(id, raw) => (id, raw),
        };
        let Some((root_dir, root_id, root_ws)) = self.root(&start, start_id, start_raw)? else {
            return Ok(None);
        };
        let mut members: Vec<Found<'l, 'a>, Alloc<'a>> = Vec::new_in(alloc);
        let mut complete = true;
        match root_ws {
            None => match &start_raw.package {
                Some(package) => members.push(Found { dir: start.clone_in(alloc), id: start_id, package }),
                None => return Err(LayoutError::NotAPackage { manifest: start_manifest }),
            },
            Some(ws) => {
                let mut dirs: Vec<Text<'a>, Alloc<'a>> = Vec::new_in(alloc);
                if let Some(globs) = &ws.members {
                    for (glob, span) in globs.iter() {
                        complete &= self.expand(&root_dir, glob, *span, &mut dirs)?;
                    }
                }
                if !complete {
                    return Ok(None);
                }
                let mut visit = Vec::new_in(alloc);
                for dir in dirs.iter().rev() {
                    visit.push((dir.clone_in(alloc), false));
                }
                let mut seen: SortedMap<Text<'a>, (), Alloc<'a>> = SortedMap::with_capacity_in(0, alloc);
                // The members, then the root's own package, each depth first through its path
                // dependencies.
                for round in 0..2 {
                    if round == 1 {
                        visit.push((root_dir.clone_in(alloc), false));
                    }
                    while let Some((dir, is_path_dep)) = visit.pop() {
                        if seen.contains_key(dir.as_str()) {
                            continue;
                        }
                        // cargo's `find_path_deps`: `exclude` leaves out a glob's match as well
                        // as a path dependency; a path dependency outside the root's directory
                        // is not a member.
                        if (is_path_dep && !within(&dir, &root_dir)) || excluded(ws, &root_dir, &dir) {
                            continue;
                        }
                        let manifest = self.manifest_of(&dir);
                        self.prefetch(&dir, MemberRole::Workspace);
                        let (id, raw) = match self.look(&manifest) {
                            Look::Pending => {
                                complete = false;
                                continue;
                            }
                            Look::Missing => return Err(self.missing(&manifest)),
                            Look::Read(id, raw) => (id, raw),
                        };
                        seen.insert(dir.clone_in(alloc), ());
                        let Some(package) = &raw.package else {
                            if dir == root_dir {
                                continue;
                            }
                            return Err(LayoutError::NotAPackage { manifest });
                        };
                        for dep in package.dependencies.iter().rev() {
                            if let Some((path, _)) = dep_path(dep, Some(ws), &root_dir, &dir, alloc)? {
                                visit.push((path, true));
                            }
                        }
                        members.push(Found { dir, id, package });
                    }
                }
                if !complete {
                    return Ok(None);
                }
                if !seen.contains_key(start.as_str()) {
                    return Err(LayoutError::NotAMember { start: start_manifest, root: self.manifest_of(&root_dir) });
                }
            }
        }
        // Names are one per member.
        for (i, member) in members.iter().enumerate() {
            if let Some(first) = members[..i].iter().find(|other| other.package.name == member.package.name) {
                return Err(LayoutError::DuplicateName { first: first.package.span, second: member.package.span });
            }
        }
        let root_raw = match &self.layout.manifests[root_id.0 as usize].1 {
            Answer::Read(raw) => Some(raw),
            Answer::Missing => None,
        };
        // Non-member path packages: `[patch.*]` entries in the root manifest, excluded path
        // dependencies of workspace members, and their transitive normal/build path dependencies.
        let mut path_found: Vec<(Found<'l, 'a>, Option<&'l RawWorkspace<'a>>, Text<'a>), Alloc<'a>> = Vec::new_in(alloc);
        let mut patch_sources: SortedMap<Text<'a>, Vec<Text<'a>, Alloc<'a>>, Alloc<'a>> = SortedMap::new_in(alloc);
        if self.layout.path_packages {
            // `seen_paths` maps dir -> `required` (`true` once visited as a required path or
            // successfully read, `false` if skipped while missing as an optional transitive patch
            // dependency so a later required edge still reports `NoManifest`).
            let mut seen_paths: SortedMap<Text<'a>, bool, Alloc<'a>> = SortedMap::with_capacity_in(members.len(), alloc);
            for member in members.iter() {
                seen_paths.insert(member.dir.clone_in(alloc), true);
            }
            // `(dir, optional)`: direct member path deps and direct `[patch]` entries are required
            // (`optional = false`); transitive path deps of `[patch]` entries are `optional = true`
            // so an unused patch with an absent transitive path dependency does not break layout.
            let mut visit: Vec<(Text<'a>, bool), Alloc<'a>> = Vec::new_in(alloc);
            if let Some(root_raw) = root_raw {
                for (source_key, patch) in root_raw.patches.iter() {
                    if let Some(path) = &patch.path
                        && let Some(joined) = join(&root_dir, path, alloc)
                    {
                        let entry = patch_sources.entry(joined.clone_in(alloc)).or_insert_with(|| Vec::new_in(alloc));
                        if !entry.iter().any(|existing| existing.as_str() == source_key.as_str()) {
                            entry.push(source_key.clone_in(alloc));
                        }
                        visit.push((joined, false));
                    }
                }
            }
            for member in members.iter() {
                for dep in member.package.dependencies.iter() {
                    if let Some((joined, _)) = dep_path(dep, root_ws, &root_dir, &member.dir, alloc)? {
                        visit.push((joined, false));
                    }
                }
            }
            let mut at = 0usize;
            while let Some((dir, optional)) = visit.get(at) {
                let dir = dir.clone_in(alloc);
                let optional = *optional;
                at += 1;
                if let Some(&was_required) = seen_paths.get(dir.as_str())
                    && (was_required || optional)
                {
                    continue;
                }
                let manifest = self.manifest_of(&dir);
                self.prefetch(&dir, MemberRole::PathPackage);
                let (id, raw) = match self.look(&manifest) {
                    Look::Pending => {
                        complete = false;
                        continue;
                    }
                    Look::Missing => {
                        if optional {
                            seen_paths.insert(dir, false);
                            continue;
                        }
                        return Err(self.missing(&manifest));
                    }
                    Look::Read(id, raw) => (id, raw),
                };
                let Some(package) = &raw.package else {
                    if optional {
                        seen_paths.insert(dir, false);
                        continue;
                    }
                    return Err(LayoutError::NotAPackage { manifest });
                };
                let Some((pkg_root_dir, _, pkg_ws)) = self.root(&dir, id, raw)? else {
                    complete = false;
                    continue;
                };
                seen_paths.insert(dir.clone_in(alloc), true);
                let child_optional = optional || patch_sources.contains_key(dir.as_str());
                for dep in package.dependencies.iter().filter(|dep| dep.kind != DepKind::Dev) {
                    if let Some((dep_dir, _)) = dep_path(dep, pkg_ws, &pkg_root_dir, &dir, alloc)?
                        && !seen_paths.get(dep_dir.as_str()).is_some_and(|&was_req| was_req || child_optional)
                    {
                        visit.push((dep_dir, child_optional));
                    }
                }
                path_found.push((Found { dir, id, package }, pkg_ws, pkg_root_dir));
            }
        }
        let mut out = Vec::with_capacity_in(members.len(), alloc);
        for found in members.iter() {
            match self.member(found, &members, root_ws, &root_dir, MemberRole::Workspace)? {
                Some(mut member) => {
                    if let Some(sources) = patch_sources.get(member.dir.as_str()) {
                        for src in sources.iter() {
                            member.patched_from.push(src.clone_in(alloc));
                        }
                    }
                    out.push(member);
                }
                None => complete = false,
            }
        }
        let mut path_packages = Vec::with_capacity_in(path_found.len(), alloc);
        for (found, pkg_ws, pkg_root_dir) in path_found.iter() {
            match self.member(found, &members, *pkg_ws, pkg_root_dir, MemberRole::PathPackage)? {
                Some(mut pkg) => {
                    if let Some(sources) = patch_sources.get(pkg.dir.as_str()) {
                        for src in sources.iter() {
                            pkg.patched_from.push(src.clone_in(alloc));
                        }
                    }
                    path_packages.push(pkg);
                }
                None => complete = false,
            }
        }
        if !complete {
            return Ok(None);
        }
        path_packages.sort_unstable_by(|a, b| a.dir.as_str().cmp(b.dir.as_str()));
        let mut manifests = Vec::with_capacity_in(self.layout.manifests.len(), alloc);
        for (path, _) in self.layout.manifests.iter() {
            manifests.push(path.clone_in(alloc));
        }
        let root_package = root_raw.and_then(|raw| raw.package.as_ref());
        let resolver = match (root_ws.and_then(|ws| ws.resolver.as_ref()), root_package.and_then(|p| p.resolver.as_ref())) {
            (Some(written), _) | (None, Some(written)) => Some((written.0.clone_in(alloc), written.1)),
            (None, None) => None,
        };
        let root_edition = out.iter().find(|member| member.manifest == root_id).map(|member| member.edition);
        Ok(Some(Units { manifests, root: root_id, members: out, path_packages, resolver, root_edition }))
    }

    /// The root: its directory, manifest and `[workspace]` (`None` for a start in no
    /// workspace); `Ok(None)` while an answer is still to come.
    #[allow(clippy::type_complexity)]
    fn root(
        &mut self,
        start: &Text<'a>,
        start_id: ManifestId,
        start_raw: &'l Raw<'a>,
    ) -> Result<Option<(Text<'a>, ManifestId, Option<&'l RawWorkspace<'a>>)>, LayoutError<'a>> {
        let alloc = self.alloc();
        if let Some(ws) = &start_raw.workspace {
            return Ok(Some((start.clone_in(alloc), start_id, Some(ws))));
        }
        if let Some(package) = &start_raw.package
            && let Some((rel, span)) = &package.workspace
        {
            let Some(dir) = join(start, rel, alloc) else { return Err(LayoutError::OutsideAnchor { at: *span }) };
            let manifest = self.manifest_of(&dir);
            return match self.look(&manifest) {
                Look::Pending => Ok(None),
                Look::Missing => Err(self.missing(&manifest)),
                Look::Read(id, raw) => match &raw.workspace {
                    Some(ws) => Ok(Some((dir, id, Some(ws)))),
                    None => Err(LayoutError::NotARoot { at: *span }),
                },
            };
        }
        // Every directory above, nearest first, asked at once.
        let mut found = None;
        let mut waiting = false;
        let mut dir = start.clone_in(alloc);
        while let Some(up) = parent(&dir, alloc) {
            dir = up;
            let manifest = self.manifest_of(&dir);
            match self.look(&manifest) {
                Look::Pending => waiting = true,
                Look::Missing => {}
                Look::Read(id, raw) => {
                    if found.is_none()
                        && !waiting
                        && let Some(ws) = &raw.workspace
                        && !excluded(ws, &dir, start)
                    {
                        found = Some((dir.clone_in(alloc), id, Some(ws)));
                    }
                }
            }
        }
        match (found, waiting) {
            (Some(found), _) => Ok(Some(found)),
            (None, true) => Ok(None),
            (None, false) => Ok(Some((start.clone_in(alloc), start_id, None))),
        }
    }

    /// The directories `glob` (relative to `root`) names, pushed onto `out`; `false` while a
    /// listing is still to come.
    fn expand(&mut self, root: &str, glob: &str, span: Span, out: &mut Vec<Text<'a>, Alloc<'a>>) -> Result<bool, LayoutError<'a>> {
        let alloc = self.alloc();
        let pattern = match blaze_glob::Pattern::new(glob) {
            Ok(pattern) => pattern,
            Err(error) => return Err(LayoutError::Pattern { at: span, error }),
        };
        if !pattern.has_metachars() {
            match join(root, glob, alloc) {
                Some(dir) => out.push(dir),
                None => return Err(LayoutError::OutsideAnchor { at: span }),
            }
            return Ok(true);
        }
        // Component by component: a literal one is joined, a pattern one matched against the
        // listing of the directories so far.
        let mut dirs = Vec::new_in(alloc);
        dirs.push(String::from_str_in(root, alloc));
        let mut complete = true;
        let parts: Vec<&str, Alloc<'a>> = {
            let mut parts = Vec::new_in(alloc);
            for part in glob.split('/') {
                if !part.is_empty() && part != "." {
                    parts.push(part);
                }
            }
            parts
        };
        for part in parts.iter() {
            let mut next = Vec::new_in(alloc);
            if *part == "**" {
                return Err(LayoutError::RecursiveGlob { at: span });
            }
            let component = match blaze_glob::Pattern::new(part) {
                Ok(component) => component,
                Err(error) => return Err(LayoutError::Pattern { at: span, error }),
            };
            for dir in dirs.iter() {
                if !component.has_metachars() {
                    match join(dir, part, alloc) {
                        Some(joined) => next.push(joined),
                        None => return Err(LayoutError::OutsideAnchor { at: span }),
                    }
                    continue;
                }
                match self.list(dir) {
                    None => complete = false,
                    Some(None) => {}
                    Some(Some(entries)) => {
                        for (name, is_dir) in entries {
                            if *is_dir && component.matches(name) {
                                match join(dir, name, alloc) {
                                    Some(joined) => next.push(joined),
                                    None => return Err(LayoutError::OutsideAnchor { at: span }),
                                }
                            }
                        }
                    }
                }
            }
            dirs = next;
        }
        if !complete {
            return Ok(false);
        }
        for dir in dirs.iter() {
            out.push(dir.clone_in(alloc));
        }
        Ok(true)
    }

    /// The member `found` with its targets and dependencies; `None` while a listing is still
    /// to come.
    fn member(
        &mut self,
        found: &Found<'l, 'a>,
        members: &[Found<'l, 'a>],
        ws: Option<&'l RawWorkspace<'a>>,
        root_dir: &str,
        role: MemberRole,
    ) -> Result<Option<Member<'a>>, LayoutError<'a>> {
        let alloc = self.alloc();
        let package = found.package;
        let (edition, edition_span) = match &package.edition {
            RawEdition::Absent => (PackageEdition::DEFAULT, None),
            RawEdition::Written(edition, span) => (*edition, Some(*span)),
            RawEdition::Inherited(span) => match ws.and_then(|ws| ws.edition) {
                Some(edition) => (edition, Some(*span)),
                None => return Err(LayoutError::NotInherited { at: *span, key: "edition" }),
            },
        };
        // Both asked before either waits, so their listings come in one pass.
        let targets = self.targets(found, edition)?;
        let extras = match role == MemberRole::Workspace && self.layout.test_targets {
            true => self.extras(found, edition),
            false => Some((Vec::new_in(alloc), Vec::new_in(alloc))),
        };
        let (Some((lib, bins)), Some((extras, refused_targets))) = (targets, extras) else { return Ok(None) };
        let build = match &package.build {
            RawBuild::None => None,
            RawBuild::Path(path, span) => Some(Target {
                name: String::from_str_in("build-script-build", alloc),
                root: self.rooted(&found.dir, path, *span)?,
                span: *span,
                found: false,
                proc_macro: false,
                required_features: Vec::new_in(alloc),
            }),
            RawBuild::Auto => match self.list(&found.dir) {
                None => return Ok(None),
                Some(entries) => match entries.is_some_and(|entries| entries.iter().any(|(n, d)| n.as_str() == "build.rs" && !*d)) {
                    true => Some(Target {
                        name: String::from_str_in("build-script-build", alloc),
                        root: file(&found.dir, "build.rs", alloc),
                        span: package.span,
                        found: true,
                        proc_macro: false,
                        required_features: Vec::new_in(alloc),
                    }),
                    false => None,
                },
            },
        };
        let version = match &package.version {
            RawVersion::Absent => None,
            RawVersion::Written(version) => Some(version.clone_in(alloc)),
            RawVersion::Inherited(span) => match ws.and_then(|ws| ws.version.as_ref()) {
                Some(version) => Some(version.clone_in(alloc)),
                None => return Err(LayoutError::NotInherited { at: *span, key: "version" }),
            },
        };
        // The metadata, as cargo's normalization makes it (meta.rs).
        let mut values: [Text<'a>; 8] = core::array::from_fn(|_| String::new_in(alloc));
        for (at, raw) in package.meta.iter().enumerate() {
            values[at] = match raw {
                RawMeta::Written(value) => value.clone_in(alloc),
                RawMeta::Inherited(span) => match ws.and_then(|ws| ws.meta[at].as_ref()) {
                    None => return Err(LayoutError::NotInherited { at: *span, key: META_KEYS[at] }),
                    // A path the root states is the root's: made relative to this package's
                    // directory (cargo's `resolve_relative_path`).
                    Some(value) if (at == meta::README || at == meta::LICENSE_FILE) && !value.is_empty() => {
                        let from_anchor = self.rooted(root_dir, value, *span)?;
                        meta::relative_to(&from_anchor, &found.dir, alloc)
                    }
                    Some(value) => value.clone_in(alloc),
                },
                // Unstated: the first default readme that is a file in the package's directory.
                RawMeta::Absent if at == meta::README => match self.list(&found.dir) {
                    None => return Ok(None),
                    Some(entries) => {
                        let mut readme = "";
                        for name in meta::DEFAULT_READMES {
                            if entries.is_some_and(|entries| entries.iter().any(|(n, d)| n.as_str() == name && !*d)) {
                                readme = name;
                                break;
                            }
                        }
                        String::from_str_in(readme, alloc)
                    }
                },
                RawMeta::Absent => String::new_in(alloc),
            };
        }
        let meta = PackageMeta::new(values);
        let mut dependencies = Vec::with_capacity_in(package.dependencies.len(), alloc);
        for dep in package.dependencies.iter() {
            if role == MemberRole::PathPackage && dep.kind == DepKind::Dev {
                continue;
            }
            dependencies.push(self.dependency(dep, ws, root_dir, &found.dir, members)?);
        }
        // cargo's implicit features: an optional dependency no feature names as `dep:<name>`
        // is a feature of its own name that turns it on, placed on the dependency's line (the
        // Cargo Book, "Features", "Optional dependencies"). Every edition has them: the
        // proposed 2024 removal did not ship, and tempo (edition 2024) asks for one
        // (`tempo-dkg-onchain-artifacts`'s `commonware-consensus`).
        let mut features = Vec::with_capacity_in(package.features.len(), alloc);
        for feature in package.features.iter() {
            features.push(feature.clone_in(alloc));
        }
        for dep in dependencies.iter().filter(|dep| dep.optional) {
            let named = package.features.iter().any(|feature| {
                feature.enables.iter().any(|enables| enables.strip_prefix("dep:") == Some(dep.name.as_str()))
            });
            let taken = features.iter().any(|feature| feature.name == dep.name);
            if !named && !taken {
                let mut enables = Vec::with_capacity_in(1, alloc);
                let mut turn_on = String::from_str_in("dep:", alloc);
                turn_on.push_str(&dep.name);
                enables.push(turn_on);
                features.push(Feature { name: dep.name.clone_in(alloc), enables, span: dep.span });
            }
        }
        Ok(Some(Member {
            dir: found.dir.clone_in(alloc),
            manifest: found.id,
            name: package.name.clone_in(alloc),
            span: package.span,
            edition,
            edition_span,
            lib,
            bins,
            extras,
            refused_targets,
            dependencies,
            features,
            build,
            links: package.links.as_ref().map(|s| s.clone_in(alloc)),
            version,
            meta,
            patched_from: Vec::new_in(alloc),
        }))
    }

    /// The library and the binaries; `None` while a listing is still to come.
    #[allow(clippy::type_complexity)]
    fn targets(
        &mut self,
        found: &Found<'l, 'a>,
        edition: PackageEdition,
    ) -> Result<Option<(Option<Target<'a>>, Vec<Target<'a>, Alloc<'a>>)>, LayoutError<'a>> {
        let alloc = self.alloc();
        let package = found.package;
        let dir = &found.dir;
        let Some(src) = join(dir, "src", alloc) else { return Err(LayoutError::OutsideAnchor { at: package.span }) };
        let Some(src_entries) = self.list(&src) else { return Ok(None) };
        let has = |entries: Option<&[(Text<'a>, bool)]>, name: &str, want_dir: bool| {
            entries.is_some_and(|entries| entries.iter().any(|(n, d)| n.as_str() == name && *d == want_dir))
        };
        let autobins = package.autobins.unwrap_or(!(edition.syntax == edition::Edition::Edition2015 && !package.bins.is_empty()));
        // The binaries found by their files: `src/main.rs`, `src/bin/<name>.rs`,
        // `src/bin/<name>/main.rs`, each (name, root).
        let mut inferred: Vec<(Text<'a>, Text<'a>), Alloc<'a>> = Vec::new_in(alloc);
        if has(src_entries, "main.rs", false) {
            inferred.push((package.name.clone_in(alloc), file(&src, "main.rs", alloc)));
        }
        if has(src_entries, "bin", true) {
            let bin = file(&src, "bin", alloc);
            match self.list(&bin) {
                None => return Ok(None),
                Some(None) => {}
                Some(Some(entries)) => {
                    for (name, is_dir) in entries {
                        // cargo skips hidden entries (`src/bin/.x.rs`).
                        if name.starts_with('.') {
                            continue;
                        }
                        if !is_dir {
                            if let Some(stem) = name.strip_suffix(".rs") {
                                inferred.push((String::from_str_in(stem, alloc), file(&bin, name, alloc)));
                            }
                            continue;
                        }
                        let sub = file(&bin, name, alloc);
                        match self.list(&sub) {
                            None => return Ok(None),
                            Some(sub_entries) => {
                                if has(sub_entries, "main.rs", false) {
                                    inferred.push((name.clone_in(alloc), file(&sub, "main.rs", alloc)));
                                }
                            }
                        }
                    }
                }
            }
        }
        let lib = match &package.lib {
            Some(lib) => {
                let root = match &lib.path {
                    Some(path) => self.rooted(dir, path, lib.span)?,
                    None if has(src_entries, "lib.rs", false) => file(&src, "lib.rs", alloc),
                    None => return Err(LayoutError::NoTargetFile { at: lib.span }),
                };
                Some(Target {
                    name: lib_name(lib.name.as_ref(), &package.name, alloc),
                    root,
                    span: lib.span,
                    found: false,
                    proc_macro: lib.proc_macro,
                    required_features: Vec::new_in(alloc),
                })
            }
            None if package.autolib && has(src_entries, "lib.rs", false) => Some(Target {
                name: lib_name(None, &package.name, alloc),
                root: file(&src, "lib.rs", alloc),
                span: package.span,
                found: true,
                proc_macro: false,
                required_features: Vec::new_in(alloc),
            }),
            None => None,
        };
        let mut bins: Vec<Target<'a>, Alloc<'a>> = Vec::new_in(alloc);
        for bin in package.bins.iter() {
            bins.push(self.bin(bin, dir, &inferred)?);
        }
        if autobins {
            for (name, root) in inferred.iter() {
                let taken = bins.iter().any(|bin| bin.name == *name || bin.root == *root);
                if !taken {
                    // A binary found by its file has no table, so no `required-features`.
                    bins.push(Target {
                        name: name.clone_in(alloc),
                        root: root.clone_in(alloc),
                        span: package.span,
                        found: true,
                        proc_macro: false,
                        required_features: Vec::new_in(alloc),
                    });
                }
            }
        }
        Ok(Some((lib, bins)))
    }

    fn bin(
        &self,
        bin: &RawTarget<'a>,
        dir: &str,
        inferred: &[(Text<'a>, Text<'a>)],
    ) -> Result<Target<'a>, LayoutError<'a>> {
        let alloc = self.alloc();
        let Some(name) = &bin.name else { return Err(LayoutError::NoTargetFile { at: bin.span }) };
        let root = match &bin.path {
            Some(path) => self.rooted(dir, path, bin.span)?,
            None => match inferred.iter().find(|(n, _)| n == name) {
                Some((_, root)) => root.clone_in(alloc),
                None => return Err(LayoutError::NoTargetFile { at: bin.span }),
            },
        };
        Ok(Target {
            name: name.clone_in(alloc),
            root,
            span: bin.span,
            found: false,
            proc_macro: false,
            required_features: bin.required_features.clone_in(alloc),
        })
    }

    fn rooted(&self, dir: &str, path: &str, span: Span) -> Result<Text<'a>, LayoutError<'a>> {
        match join(dir, path, self.alloc()) {
            Some(root) => Ok(root),
            None => Err(LayoutError::OutsideAnchor { at: span }),
        }
    }

    fn dependency(
        &self,
        dep: &RawDep<'a>,
        ws: Option<&'l RawWorkspace<'a>>,
        root_dir: &str,
        dir: &str,
        members: &[Found<'l, 'a>],
    ) -> Result<Dependency<'a>, LayoutError<'a>> {
        let alloc = self.alloc();
        // An inherited one takes the root's entry, with this one's features added and its
        // `optional`.
        let (from, base) = match dep.workspace {
            false => (dep, dir),
            true => match ws.and_then(|ws| ws.dependencies.iter().find(|it| it.key == dep.key)) {
                Some(entry) => (entry, root_dir),
                None => return Err(LayoutError::NotInherited { at: dep.span, key: "dependencies" }),
            },
        };
        let source = match (&from.path, &from.git) {
            (Some(path), _) => {
                match join(base, path, alloc) {
                    Some(joined) => match members.iter().position(|member| member.dir == joined) {
                        Some(index) => Source::Member(index),
                        None => Source::Path(joined),
                    },
                    None => Source::Outside(path.clone_in(alloc)),
                }
            }
            (None, Some((url, reference))) => Source::Git { url: url.clone_in(alloc), reference: reference.clone_in(alloc) },
            (None, None) => Source::Registry {
                req: from.version.as_ref().map(|s| s.clone_in(alloc)),
                registry: from.registry.as_ref().map(|s| s.clone_in(alloc)),
            },
        };
        let mut features = from.features.clone_in(alloc);
        if dep.workspace {
            for feature in dep.features.iter() {
                features.push(feature.clone_in(alloc));
            }
        }
        let package = match &from.package {
            Some(package) => package.clone_in(alloc),
            None => dep.key.clone_in(alloc),
        };
        let default_features = match dep.workspace {
            true => from.default_features.unwrap_or(true),
            false => dep.default_features.unwrap_or(true),
        };
        Ok(Dependency {
            name: dep.key.clone_in(alloc),
            package,
            kind: dep.kind,
            target: dep.target.as_ref().map(|s| s.clone_in(alloc)),
            source,
            optional: dep.optional,
            default_features,
            features,
            span: dep.span,
        })
    }
}

/// The directory a path dependency names (with its line), joined to the directory it is
/// relative to: the member's, or for an inherited one the root's.
fn dep_path<'a>(
    dep: &RawDep<'a>,
    ws: Option<&RawWorkspace<'a>>,
    root_dir: &str,
    dir: &str,
    alloc: Alloc<'a>,
) -> Result<Option<(Text<'a>, Span)>, LayoutError<'a>> {
    let (path, base) = match dep.workspace {
        false => (dep.path.as_ref(), dir),
        true => match ws.and_then(|ws| ws.dependencies.iter().find(|it| it.key == dep.key)) {
            Some(entry) => (entry.path.as_ref(), root_dir),
            None => return Err(LayoutError::NotInherited { at: dep.span, key: "dependencies" }),
        },
    };
    match path {
        None => Ok(None),
        Some(path) => match join(base, path, alloc) {
            Some(joined) => Ok(Some((joined, dep.span))),
            // Absolute, or out of the anchor: never a member; [`Source::Outside`] says so.
            None => Ok(None),
        },
    }
}

/// The library's name: `[lib] name`, else the package's with `-` as `_`.
fn lib_name<'a>(written: Option<&Text<'a>>, package: &str, alloc: Alloc<'a>) -> Text<'a> {
    if let Some(name) = written {
        return name.clone_in(alloc);
    }
    let mut name = String::new_in(alloc);
    for ch in package.chars() {
        name.push(if ch == '-' { '_' } else { ch });
    }
    name
}

/// `dir/name`, for a name from a listing (one component).
fn file<'a>(dir: &str, name: &str, alloc: Alloc<'a>) -> Text<'a> {
    let mut out = String::from_str_in(dir, alloc);
    if !out.is_empty() {
        out.push('/');
    }
    out.push_str(name);
    out
}

/// `rel` joined to `dir` and normalized (`.` dropped, `..` taking a component off), as cargo
/// normalizes paths without reading the file system; `None` when it leaves the anchor or is
/// absolute.
fn join<'a>(dir: &str, rel: &str, alloc: Alloc<'a>) -> Option<Text<'a>> {
    if rel.starts_with('/') {
        return None;
    }
    let mut out = String::from_str_in(dir, alloc);
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let cut = out.as_str().rfind('/').unwrap_or(0);
                if out.is_empty() {
                    return None;
                }
                out.truncate(cut);
            }
            part => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(part);
            }
        }
    }
    Some(out)
}

/// The directory above `dir`; `None` for the anchor.
fn parent<'a>(dir: &str, alloc: Alloc<'a>) -> Option<Text<'a>> {
    if dir.is_empty() {
        return None;
    }
    Some(String::from_str_in(&dir[..dir.rfind('/').unwrap_or(0)], alloc))
}

/// Whether `path` is `dir` or under it.
fn within(path: &str, dir: &str) -> bool {
    dir.is_empty() || path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

/// cargo's `is_excluded`: under an `exclude` entry and under no `members` entry (each read as a
/// path, not a glob), both relative to the root `root`.
fn excluded(ws: &RawWorkspace<'_>, root: &str, dir: &str) -> bool {
    let under = |entry: &str| under_entry(root, entry, dir);
    let excluded = ws.exclude.iter().any(|entry| under(entry));
    let member = ws.members.as_ref().is_some_and(|members| members.iter().any(|(entry, _)| under(entry)));
    excluded && !member
}

/// Whether `dir` is under `root/entry`, comparing components, without allocating.
fn under_entry(root: &str, entry: &str, dir: &str) -> bool {
    // Walk `root/entry` component by component against `dir`'s, `.` skipped and `..` popping;
    // an entry that leaves the anchor covers nothing.
    let mut want: [&str; 64] = [""; 64];
    let mut len = 0usize;
    for part in root.split('/').chain(entry.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                if len == 0 {
                    return false;
                }
                len -= 1;
            }
            part => {
                if len == want.len() {
                    return false;
                }
                want[len] = part;
                len += 1;
            }
        }
    }
    let mut parts = dir.split('/').filter(|p| !p.is_empty());
    for expected in &want[..len] {
        if parts.next() != Some(*expected) {
            return false;
        }
    }
    true
}
