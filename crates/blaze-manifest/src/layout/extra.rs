//! A package's integration tests, examples and benches: the targets the manifest format finds
//! in `tests/`, `examples/` and `benches/`, and the `[[test]]`, `[[example]]` and `[[bench]]`
//! tables that add or rename them. Read by the format's documented rules (the Cargo Book,
//! "Cargo Targets"), quoted at each branch; nothing here runs cargo or opens a file: each
//! directory is one listing asked of the caller ([`super::Need::Listing`]), one level, its
//! entries in name-byte order (as [`super::Layout::listing`] keeps them). No walk of the
//! checkout: a target directory's subdirectories are listed once each, for their `main.rs`.

use core::fmt;

use blaze_own::{Alloc, String, Vec};
use edition::PackageEdition;

use super::raw::{RawExtra, RawPackage};
use super::{Found, LayoutError, Passing, Span, Text, file, join, within};

/// Which of the three kinds a target is; the order is the order targets are listed in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ExtraKind {
    /// An integration test: "When you run `cargo test`, Cargo will compile each of these files
    /// as a separate crate, and execute them."
    Test,
    Example,
    Bench,
}

impl ExtraKind {
    pub const ALL: [ExtraKind; 3] = [ExtraKind::Test, ExtraKind::Example, ExtraKind::Bench];

    /// The package directory its targets are found in.
    pub fn dir(self) -> &'static str {
        match self {
            ExtraKind::Test => "tests",
            ExtraKind::Example => "examples",
            ExtraKind::Bench => "benches",
        }
    }

    /// The table that writes one (`[[test]]`).
    pub fn table(self) -> &'static str {
        match self {
            ExtraKind::Test => "test",
            ExtraKind::Example => "example",
            ExtraKind::Bench => "bench",
        }
    }

    /// The `[package]` key that turns its discovery off.
    pub fn auto_key(self) -> &'static str {
        match self {
            ExtraKind::Test => "autotests",
            ExtraKind::Example => "autoexamples",
            ExtraKind::Bench => "autobenches",
        }
    }

    /// `test` when not written: "The `test` field indicates whether or not the target is tested
    /// by default by `cargo test`. The default is `true` for lib, bins, and tests."
    pub fn tested_by_default(self) -> bool {
        self == ExtraKind::Test
    }
}

/// An integration test, example or bench target.
#[derive(Clone)]
pub struct ExtraTarget<'a> {
    pub kind: ExtraKind,
    pub name: Text<'a>,
    /// The root file, anchor-relative; always under the package's directory.
    pub root: Text<'a>,
    /// Its `[[test]]` (`[[example]]`, `[[bench]]`) table's line, or for one found by its file
    /// the `[package]` header.
    pub span: Span,
    /// Found by its file, not written.
    pub found: bool,
    /// `harness`, default `true`: "The `harness` field indicates that the `--test` flag will be
    /// passed to `rustc` [..]. The default is `true` for all targets." Carried as data: the
    /// runner decides what a harness-less target is run as.
    pub harness: bool,
    /// `test`, by [`ExtraKind::tested_by_default`] when not written.
    pub test: bool,
    /// `required-features`, as written: "The `required-features` field specifies which features
    /// the target needs in order to be built." Carried as data, not judged here.
    pub required_features: Vec<Text<'a>, Alloc<'a>>,
}

/// A member's integration test, example or bench refused by the rules: a finding on its
/// manifest's line ([`Member::refused_targets`]), not a [`LayoutError`], so one bad target
/// leaves the rest of the workspace read and checked. The target is not in
/// [`Member::extras`] and so is never a unit.
///
/// [`Member::refused_targets`]: super::Member::refused_targets
/// [`Member::extras`]: super::Member::extras
#[derive(Clone)]
pub struct RefusedTarget<'a> {
    pub kind: ExtraKind,
    /// Its name; empty for a table with no `name`.
    pub target: Text<'a>,
    /// Its table's line, or for a found one the `[package]` header.
    pub at: Span,
    pub why: TargetRefused<'a>,
}

impl<'a> RefusedTarget<'a> {
    /// The same refusal as an error of the package `package`, for a caller that stops on it.
    pub fn error(&self, package: &str, alloc: Alloc<'a>) -> LayoutError<'a> {
        LayoutError::Target {
            package: String::from_str_in(package, alloc),
            kind: self.kind,
            target: self.target.clone_in(alloc),
            at: self.at,
            why: self.why.clone_in(alloc),
        }
    }
}

/// Why a workspace member's target of the three kinds is refused ([`RefusedTarget`]). Never
/// found for a layout read without `Layout::with_test_targets` (a dependency's), which finds
/// no such targets.
#[derive(Clone)]
pub enum TargetRefused<'a> {
    /// A table with no `name`: "This is required for all targets except `[lib]`."
    NoName,
    /// A `path` that leaves the package's directory (or the anchor): the path as written. blaze
    /// holds a target's root relative to its package, in the unit's key.
    OutsidePackage { path: Text<'a> },
    /// No file where the target's root must be: the written `path`, or with none written the
    /// inferred `<dir>/<name>.rs` ("If not specified, the inferred path is used based on the
    /// target name."); anchor-relative.
    NoFile { path: Text<'a> },
    /// A second target of this kind and name; the first's line.
    Duplicate { first: Span },
}

impl<'a> TargetRefused<'a> {
    fn clone_in(&self, alloc: Alloc<'a>) -> TargetRefused<'a> {
        match self {
            TargetRefused::NoName => TargetRefused::NoName,
            TargetRefused::OutsidePackage { path } => TargetRefused::OutsidePackage { path: path.clone_in(alloc) },
            TargetRefused::NoFile { path } => TargetRefused::NoFile { path: path.clone_in(alloc) },
            TargetRefused::Duplicate { first } => TargetRefused::Duplicate { first: *first },
        }
    }
}

impl fmt::Display for TargetRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetRefused::NoName => f.write_str("the table has no `name` (\"This is required for all targets except `[lib]`.\")"),
            TargetRefused::OutsidePackage { path } => {
                write!(f, "its `path` `{}` leaves the package's directory, where blaze reads every target's root", path.as_str())
            }
            TargetRefused::NoFile { path } => write!(f, "no file `{}` for its root", path.as_str()),
            TargetRefused::Duplicate { first } => write!(
                f,
                "a second target of this kind and name (the first at manifest {} bytes {}..{})",
                first.manifest.0, first.start, first.end
            ),
        }
    }
}

impl fmt::Debug for TargetRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// `autotests` (`autoexamples`, `autobenches`) as written.
fn auto_written(package: &RawPackage<'_>, kind: ExtraKind) -> Option<bool> {
    match kind {
        ExtraKind::Test => package.autotests,
        ExtraKind::Example => package.autoexamples,
        ExtraKind::Bench => package.autobenches,
    }
}

impl<'l, 'a> Passing<'l, 'a> {
    /// The package's integration tests, examples and benches, sorted by kind, then name bytes;
    /// `None` while a listing is still to come (every listing it can see it needs is asked in
    /// the same pass).
    ///
    /// The rules, each from the Cargo Book, "Cargo Targets":
    /// - found by file: `tests/<name>.rs` and `tests/<name>/main.rs` (likewise `examples/`,
    ///   `benches/`): "If a binary, example, bench, or integration test consists of multiple
    ///   source files, place a `main.rs` file along with the extra modules within a subdirectory
    ///   of the `src/bin`, `examples`, `benches`, or `tests` directory. The name of the
    ///   executable will be the directory name." Hidden entries are skipped, as for binaries.
    /// - discovery off: "Setting the keys `autolib`, `autobins`, `autoexamples`, `autotests`, or
    ///   `autobenches` to `false` in the `[package]` section will disable auto-discovery of the
    ///   corresponding target type." and "For packages with the 2015 edition, the default for
    ///   auto-discovery is `false` if at least one target is manually defined in `Cargo.toml`.
    ///   Beginning with the 2018 edition, the default is always `true`."
    /// - a written table adds a target or renames a found one: a found target whose name or
    ///   root a written one has is not added again (as for binaries).
    ///
    /// A target refused by these rules is left out and returned as a [`RefusedTarget`] (a
    /// finding on its manifest's line), never a [`LayoutError`]: one bad `[[example]]` must not
    /// hide the rest of the workspace from `check`.
    #[allow(clippy::type_complexity)]
    pub(super) fn extras(
        &mut self,
        found: &Found<'l, 'a>,
        edition: PackageEdition,
    ) -> Option<(Vec<ExtraTarget<'a>, Alloc<'a>>, Vec<RefusedTarget<'a>, Alloc<'a>>)> {
        let alloc = self.alloc();
        let package = found.package;
        let mut out: Vec<ExtraTarget<'a>, Alloc<'a>> = Vec::new_in(alloc);
        let mut refused: Vec<RefusedTarget<'a>, Alloc<'a>> = Vec::new_in(alloc);
        let mut pending = false;
        for kind in ExtraKind::ALL {
            let any_written = package.extras.iter().any(|raw| raw.kind == kind);
            let auto = auto_written(package, kind).unwrap_or(!(edition.syntax == edition::Edition::Edition2015 && any_written));
            // The found files are needed even with discovery off: a written table with no
            // `path` takes the found file of its name.
            let Some(inferred) = self.inferred(&found.dir, kind) else {
                pending = true;
                continue;
            };
            let start = out.len();
            for raw in package.extras.iter().filter(|raw| raw.kind == kind) {
                match self.written(found, raw, &inferred) {
                    Ok(target) => out.push(target),
                    Err(refusal) => refused.push(refusal),
                }
            }
            if auto {
                for (name, root) in inferred.iter() {
                    let taken = out[start..].iter().any(|target| target.name == *name || target.root == *root);
                    if !taken {
                        out.push(ExtraTarget {
                            kind,
                            name: name.clone_in(alloc),
                            root: root.clone_in(alloc),
                            span: package.span,
                            found: true,
                            harness: true,
                            test: kind.tested_by_default(),
                            required_features: Vec::new_in(alloc),
                        });
                    }
                }
            }
            // One name per kind: two written tables, or `x.rs` beside `x/main.rs`. The first
            // stays; each later one of its name is refused and left out.
            let mut at = start + 1;
            while at < out.len() {
                match out[start..at].iter().find(|other| other.name == out[at].name).map(|first| first.span) {
                    Some(first) => {
                        let second = out.remove(at);
                        refused.push(RefusedTarget { kind, target: second.name, at: second.span, why: TargetRefused::Duplicate { first } });
                    }
                    None => at += 1,
                }
            }
        }
        if pending {
            return None;
        }
        out.sort_by(|a, b| (a.kind, a.name.as_str()).cmp(&(b.kind, b.name.as_str())));
        refused.sort_by(|a, b| (a.at.manifest, a.at.start, a.kind, a.target.as_str()).cmp(&(b.at.manifest, b.at.start, b.kind, b.target.as_str())));
        Some((out, refused))
    }

    /// The targets of `kind` found by their files under the package directory `dir`, each
    /// (name, anchor-relative root) in name-byte order; `None` while a listing is still to come.
    fn inferred(&mut self, dir: &str, kind: ExtraKind) -> Option<Vec<(Text<'a>, Text<'a>), Alloc<'a>>> {
        let alloc = self.alloc();
        let base = file(dir, kind.dir(), alloc);
        let mut inferred: Vec<(Text<'a>, Text<'a>), Alloc<'a>> = Vec::new_in(alloc);
        let entries = match self.list(&base)? {
            None => return Some(inferred),
            Some(entries) => entries,
        };
        let mut pending = false;
        for (name, is_dir) in entries {
            if name.starts_with('.') {
                continue;
            }
            if !*is_dir {
                if let Some(stem) = name.strip_suffix(".rs") {
                    inferred.push((String::from_str_in(stem, alloc), file(&base, name, alloc)));
                }
                continue;
            }
            let sub = file(&base, name, alloc);
            match self.list(&sub) {
                None => pending = true,
                Some(sub_entries) => {
                    if sub_entries.is_some_and(|sub_entries| sub_entries.iter().any(|(n, d)| n.as_str() == "main.rs" && !*d)) {
                        inferred.push((name.clone_in(alloc), file(&sub, "main.rs", alloc)));
                    }
                }
            }
        }
        match pending {
            true => None,
            false => Some(inferred),
        }
    }

    /// A `[[test]]` (`[[example]]`, `[[bench]]`) table as a target: "The `path` field specifies
    /// where the source for the crate is located, relative to the `Cargo.toml` file. If not
    /// specified, the inferred path is used based on the target name." A written path is the
    /// root as written (no listing asked; its reader looks at it once); a target these rules
    /// refuse is the `Err`.
    fn written(
        &self,
        found: &Found<'l, 'a>,
        raw: &RawExtra<'a>,
        inferred: &[(Text<'a>, Text<'a>)],
    ) -> Result<ExtraTarget<'a>, RefusedTarget<'a>> {
        let alloc = self.alloc();
        let dir = found.dir.as_str();
        let refuse = |target: &str, why: TargetRefused<'a>| {
            Err(RefusedTarget { kind: raw.kind, target: String::from_str_in(target, alloc), at: raw.span, why })
        };
        let Some(name) = &raw.name else { return refuse("", TargetRefused::NoName) };
        // Only a workspace member's targets are read here (`Layout::with_test_targets`): a
        // dependency is published without these folders as a matter of course, and its tests,
        // examples and benches are never built, so its tables are read for shape only.
        let root = match &raw.path {
            None => match inferred.iter().find(|(n, _)| n == name) {
                Some((_, root)) => root.clone_in(alloc),
                None => {
                    let mut path = file(&file(dir, raw.kind.dir(), alloc), name, alloc);
                    path.push_str(".rs");
                    return refuse(name, TargetRefused::NoFile { path });
                }
            },
            // A written path is the root, exactly that file relative to the manifest's directory,
            // wherever it is under the package (cargo's rule), as a `[lib]` or `[[bin]]` path is
            // (layout/mod.rs `rooted`): no listing is asked to look for it. Its one look is its
            // reader's, the crate root's read (an absent root is named there, its row absent, so
            // the file made later moves it). Until 2026-11-20 the parent's listing was asked and
            // the file looked for in it, but the command's listings hold only the names a
            // discovery reads (blaze-cli sweep.rs `wanted`: `lib.rs`, `main.rs`, `build.rs`,
            // `*.rs` under `tests/`), so `tests/integration/mod.rs` or a package's own
            // `regression_tests.rs` was refused "no file" while there (turso: eight tests).
            Some(path) => match join(dir, path, alloc) {
                Some(root) if within(&root, dir) && root.as_str() != dir => root,
                _ => return refuse(name, TargetRefused::OutsidePackage { path: path.clone_in(alloc) }),
            },
        };
        Ok(ExtraTarget {
            kind: raw.kind,
            name: name.clone_in(alloc),
            root,
            span: raw.span,
            found: false,
            harness: raw.harness.unwrap_or(true),
            test: raw.test.unwrap_or(raw.kind.tested_by_default()),
            required_features: raw.required_features.clone_in(alloc),
        })
    }
}
