//! One manifest's text to what the layout needs of it, read once when the text is handed in:
//! `[workspace]` (members, exclude, the package fields and dependencies it offers) and
//! `[package]` (name, edition, targets, dependencies, features), every piece with the byte
//! range of its line. Nothing here knows another manifest or a file.

use blaze_own::{Alloc, String, Vec};
use blaze_toml::{Document, Item, Kind, Limits, Origin};
use edition::PackageEdition;

use super::meta::{AUTHORS, META_KEYS, README};
use super::{DepKind, ExtraKind, Feature, LayoutError, ManifestId, Span, Text};

type It<'d, 't, 'a> = Item<'d, 't, Alloc<'a>>;

/// What a manifest's `[workspace]` says.
pub(super) struct RawWorkspace<'a> {
    /// `members`, each with the range of its entry; `None` when the key is absent.
    pub members: Option<Vec<(Text<'a>, Span), Alloc<'a>>>,
    pub exclude: Vec<Text<'a>, Alloc<'a>>,
    /// `workspace.package.edition`, read.
    pub edition: Option<PackageEdition>,
    /// `workspace.dependencies`, each by its key.
    pub dependencies: Vec<RawDep<'a>, Alloc<'a>>,
    /// `workspace.package.version`.
    pub version: Option<Text<'a>>,
    /// `workspace.package`'s metadata keys ([`META_KEYS`]' order), each in the form a crate
    /// reads it ([`Reader::meta_value`]); `None` where the root does not offer the key.
    pub meta: [Option<Text<'a>>; 8],
    /// `workspace.resolver`, as written, with its line.
    pub resolver: Option<(Text<'a>, Span)>,
}

/// A package's metadata key ([`META_KEYS`]) as written.
pub(super) enum RawMeta<'a> {
    Absent,
    /// The value in the form a crate reads it ([`Reader::meta_value`]).
    Written(Text<'a>),
    /// `key.workspace = true`: the line that asks.
    Inherited(Span),
}

/// A package's `build` key as written.
pub(super) enum RawBuild<'a> {
    /// No key, or `build = true`: `build.rs` in the package's directory when it is there.
    Auto,
    /// `build = false`.
    None,
    /// `build = "path"`, with its line.
    Path(Text<'a>, Span),
}

/// A package's `version` as written.
pub(super) enum RawVersion<'a> {
    Absent,
    Written(Text<'a>),
    /// `version.workspace = true`: the line that asks.
    Inherited(Span),
}

/// A package's `edition` as written.
pub(super) enum RawEdition {
    Absent,
    Written(PackageEdition, Span),
    /// `edition.workspace = true`: the line that asks.
    Inherited(Span),
}

/// A `[lib]` or `[[bin]]` table.
pub(super) struct RawTarget<'a> {
    pub name: Option<Text<'a>>,
    pub path: Option<Text<'a>>,
    pub proc_macro: bool,
    /// `required-features`; empty when not written (the Cargo Book, "Cargo Targets", "The
    /// `required-features` field": "specifies which features the target needs in order to be
    /// built"; it has an effect on `[[bin]]`, `[[example]]`, `[[test]]` and `[[bench]]`).
    pub required_features: Vec<Text<'a>, Alloc<'a>>,
    pub span: Span,
}

/// A `[[test]]`, `[[example]]` or `[[bench]]` table, as written.
pub(super) struct RawExtra<'a> {
    pub kind: ExtraKind,
    pub name: Option<Text<'a>>,
    pub path: Option<Text<'a>>,
    pub harness: Option<bool>,
    pub test: Option<bool>,
    /// `required-features`; empty when not written.
    pub required_features: Vec<Text<'a>, Alloc<'a>>,
    pub span: Span,
}

/// What a manifest's `[package]` says.
pub(super) struct RawPackage<'a> {
    pub span: Span,
    pub name: Text<'a>,
    pub edition: RawEdition,
    /// `package.workspace`: the root's directory, relative to this one.
    pub workspace: Option<(Text<'a>, Span)>,
    pub autolib: bool,
    pub autobins: Option<bool>,
    pub lib: Option<RawTarget<'a>>,
    pub bins: Vec<RawTarget<'a>, Alloc<'a>>,
    /// `autotests`, `autoexamples`, `autobenches` as written.
    pub autotests: Option<bool>,
    pub autoexamples: Option<bool>,
    pub autobenches: Option<bool>,
    /// The `[[test]]`, `[[example]]` and `[[bench]]` tables, in that order, each in written order.
    pub extras: Vec<RawExtra<'a>, Alloc<'a>>,
    pub dependencies: Vec<RawDep<'a>, Alloc<'a>>,
    pub features: Vec<Feature<'a>, Alloc<'a>>,
    pub build: RawBuild<'a>,
    pub links: Option<Text<'a>>,
    pub version: RawVersion<'a>,
    /// The metadata keys, in [`META_KEYS`]' order.
    pub meta: [RawMeta<'a>; 8],
    /// `package.resolver`, as written, with its line.
    pub resolver: Option<(Text<'a>, Span)>,
}

/// A git dependency's reference.
#[derive(Clone)]
pub enum GitRef<'a> {
    DefaultBranch,
    Branch(Text<'a>),
    Tag(Text<'a>),
    Rev(Text<'a>),
}

impl<'a> GitRef<'a> {
    pub(super) fn clone_in(&self, alloc: Alloc<'a>) -> GitRef<'a> {
        match self {
            GitRef::DefaultBranch => GitRef::DefaultBranch,
            GitRef::Branch(s) => GitRef::Branch(s.clone_in(alloc)),
            GitRef::Tag(s) => GitRef::Tag(s.clone_in(alloc)),
            GitRef::Rev(s) => GitRef::Rev(s.clone_in(alloc)),
        }
    }
}

/// One dependency as written.
pub(super) struct RawDep<'a> {
    pub key: Text<'a>,
    pub kind: DepKind,
    pub target: Option<Text<'a>>,
    pub package: Option<Text<'a>>,
    pub version: Option<Text<'a>>,
    pub path: Option<Text<'a>>,
    pub git: Option<(Text<'a>, GitRef<'a>)>,
    pub registry: Option<Text<'a>>,
    pub workspace: bool,
    pub optional: bool,
    pub default_features: Option<bool>,
    pub features: Vec<Text<'a>, Alloc<'a>>,
    pub span: Span,
}

/// A manifest read.
pub(super) struct Raw<'a> {
    pub workspace: Option<RawWorkspace<'a>>,
    pub package: Option<RawPackage<'a>>,
    /// Every `[patch.<source>]` entry in this manifest as `(source_key, dep)` (only the workspace
    /// root's is read for non-member path packages).
    pub patches: Vec<(Text<'a>, RawDep<'a>), Alloc<'a>>,
}

/// Reads `text`, the manifest `id`, whose path is `path` (for the error).
pub(super) fn read<'a>(text: &str, id: ManifestId, path: &str, alloc: Alloc<'a>) -> Result<Raw<'a>, LayoutError<'a>> {
    let doc = match Document::parse_in(text, Limits::CARGO, alloc) {
        Ok(doc) => doc,
        Err(error) => return Err(LayoutError::Toml { manifest: String::from_str_in(path, alloc), error }),
    };
    let r = Reader { text, id, alloc };
    let root = doc.root();
    let workspace = match root.get("workspace") {
        Some(ws) => Some(r.workspace(ws)?),
        None => None,
    };
    let package = match root.get("package") {
        Some(package) => Some(r.package(root, package)?),
        None => None,
    };
    let mut patches = Vec::new_in(alloc);
    if let Some(patch) = root.get("patch") {
        r.table(patch, "patch")?;
        for source in patch.children() {
            let source_key = source.key();
            let mut deps = Vec::new_in(alloc);
            r.deps(source, "patch", DepKind::Normal, None, &mut deps)?;
            for dep in deps {
                patches.push((String::from_str_in(source_key, alloc), dep));
            }
        }
    }
    Ok(Raw { workspace, package, patches })
}

struct Reader<'s, 'a> {
    text: &'s str,
    id: ManifestId,
    alloc: Alloc<'a>,
}

impl<'s, 'a> Reader<'s, 'a> {
    /// The range of `item`'s line: from the start of the line its key (or header) starts on,
    /// to the end of its value (or header); a table made by dotted keys ends at its last
    /// value's end.
    fn line(&self, item: It<'_, '_, 'a>) -> Span {
        let from = match item.key_span().is_empty() {
            true => item.span().start,
            false => item.key_span().start.min(item.span().start),
        };
        let start = self.text.as_bytes()[..from].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let mut end = item.span().end;
        if item.kind() == Kind::Table && item.origin() == Origin::Dotted {
            for child in item.children() {
                end = end.max(child.span().end);
            }
        }
        Span::new(self.id, start, end)
    }

    fn expected(&self, item: It<'_, '_, 'a>, key: &'static str, kind: &'static str) -> LayoutError<'a> {
        LayoutError::Expected { at: self.line(item), key, kind }
    }

    fn text(&self, item: It<'_, '_, 'a>, key: &'static str) -> Result<Text<'a>, LayoutError<'a>> {
        match item.as_str() {
            Some(text) => Ok(String::from_str_in(text, self.alloc)),
            None => Err(self.expected(item, key, "a string")),
        }
    }

    fn opt_text(&self, table: It<'_, '_, 'a>, key: &'static str) -> Result<Option<Text<'a>>, LayoutError<'a>> {
        match table.get(key) {
            Some(item) => self.text(item, key).map(Some),
            None => Ok(None),
        }
    }

    fn opt_bool(&self, table: It<'_, '_, 'a>, key: &'static str) -> Result<Option<bool>, LayoutError<'a>> {
        match table.get(key) {
            Some(item) => match item.as_bool() {
                Some(b) => Ok(Some(b)),
                None => Err(self.expected(item, key, "a bool")),
            },
            None => Ok(None),
        }
    }

    fn texts(&self, item: It<'_, '_, 'a>, key: &'static str) -> Result<Vec<Text<'a>, Alloc<'a>>, LayoutError<'a>> {
        if item.kind() != Kind::Array {
            return Err(self.expected(item, key, "an array of strings"));
        }
        let mut out = Vec::new_in(self.alloc);
        for entry in item.children() {
            out.push(self.text(entry, key)?);
        }
        Ok(out)
    }

    fn table(&self, item: It<'_, '_, 'a>, key: &'static str) -> Result<(), LayoutError<'a>> {
        match item.kind() {
            Kind::Table => Ok(()),
            _ => Err(self.expected(item, key, "a table")),
        }
    }

    fn workspace(&self, ws: It<'_, '_, 'a>) -> Result<RawWorkspace<'a>, LayoutError<'a>> {
        self.table(ws, "workspace")?;
        let members = match ws.get("members") {
            Some(members) => {
                if members.kind() != Kind::Array {
                    return Err(self.expected(members, "workspace.members", "an array of strings"));
                }
                let mut out = Vec::new_in(self.alloc);
                for entry in members.children() {
                    out.push((self.text(entry, "workspace.members")?, self.line(entry)));
                }
                Some(out)
            }
            None => None,
        };
        let exclude = match ws.get("exclude") {
            Some(exclude) => self.texts(exclude, "workspace.exclude")?,
            None => Vec::new_in(self.alloc),
        };
        let mut edition = None;
        let mut version = None;
        let mut meta: [Option<Text<'a>>; 8] = core::array::from_fn(|_| None);
        if let Some(package) = ws.get("package") {
            self.table(package, "workspace.package")?;
            if let Some(item) = package.get("edition") {
                edition = Some(self.edition_value(item)?.0);
            }
            version = self.opt_text(package, "version")?;
            for (at, &key) in META_KEYS.iter().enumerate() {
                if let Some(item) = package.get(key) {
                    meta[at] = Some(self.meta_value(item, at)?);
                }
            }
        }
        let mut dependencies = Vec::new_in(self.alloc);
        if let Some(deps) = ws.get("dependencies") {
            self.deps(deps, "workspace.dependencies", DepKind::Normal, None, &mut dependencies)?;
        }
        let resolver = self.resolver(ws, "workspace.resolver")?;
        Ok(RawWorkspace { members, exclude, edition, dependencies, version, meta, resolver })
    }

    /// The metadata key `META_KEYS[at]`'s value `item` in the form a crate reads it (meta.rs):
    /// `authors` an array of strings joined with `:`; `readme` a path, `true` (`README.md`) or
    /// `false` (`""`); every other key a string.
    fn meta_value(&self, item: It<'_, '_, 'a>, at: usize) -> Result<Text<'a>, LayoutError<'a>> {
        let key = META_KEYS[at];
        if at == AUTHORS {
            let authors = self.texts(item, key)?;
            let mut joined = String::new_in(self.alloc);
            for (i, author) in authors.iter().enumerate() {
                if i > 0 {
                    joined.push(':');
                }
                joined.push_str(author);
            }
            return Ok(joined);
        }
        if at == README
            && let Some(on) = item.as_bool()
        {
            return Ok(String::from_str_in(if on { "README.md" } else { "" }, self.alloc));
        }
        self.text(item, key)
    }

    fn resolver(&self, table: It<'_, '_, 'a>, key: &'static str) -> Result<Option<(Text<'a>, Span)>, LayoutError<'a>> {
        match table.get("resolver") {
            Some(item) => Ok(Some((self.text(item, key)?, self.line(item)))),
            None => Ok(None),
        }
    }

    fn edition_value(&self, item: It<'_, '_, 'a>) -> Result<(PackageEdition, Span), LayoutError<'a>> {
        let span = self.line(item);
        let Some(written) = item.as_str() else { return Err(self.expected(item, "edition", "a string")) };
        match PackageEdition::of_manifest(written) {
            Ok(edition) => Ok((edition, span)),
            Err(why) => Err(LayoutError::Edition { at: span, why }),
        }
    }

    /// `true` when `item` is `{ workspace = true }` (or `x.workspace = true`).
    fn inherits(&self, item: It<'_, '_, 'a>, key: &'static str) -> Result<bool, LayoutError<'a>> {
        if item.kind() != Kind::Table {
            return Ok(false);
        }
        match self.opt_bool(item, "workspace")? {
            Some(true) => Ok(true),
            _ => Err(self.expected(item, key, "a value or `{ workspace = true }`")),
        }
    }

    fn package(&self, root: It<'_, '_, 'a>, package: It<'_, '_, 'a>) -> Result<RawPackage<'a>, LayoutError<'a>> {
        self.table(package, "package")?;
        let span = self.line(package);
        let name = match package.get("name") {
            Some(name) => self.text(name, "package.name")?,
            None => return Err(LayoutError::NoPackageName { at: span }),
        };
        let edition = match package.get("edition") {
            None => RawEdition::Absent,
            Some(item) => match self.inherits(item, "package.edition")? {
                true => RawEdition::Inherited(self.line(item)),
                false => {
                    let (edition, at) = self.edition_value(item)?;
                    RawEdition::Written(edition, at)
                }
            },
        };
        let workspace = match package.get("workspace") {
            Some(item) => Some((self.text(item, "package.workspace")?, self.line(item))),
            None => None,
        };
        let autolib = self.opt_bool(package, "autolib")?.unwrap_or(true);
        let autobins = self.opt_bool(package, "autobins")?;
        let lib = match root.get("lib") {
            Some(lib) => Some(self.target(lib, "lib")?),
            None => None,
        };
        let mut bins = Vec::new_in(self.alloc);
        if let Some(list) = root.get("bin") {
            if list.kind() != Kind::Array {
                return Err(self.expected(list, "bin", "an array of tables (`[[bin]]`)"));
            }
            for bin in list.children() {
                bins.push(self.target(bin, "bin")?);
            }
        }
        let autotests = self.opt_bool(package, "autotests")?;
        let autoexamples = self.opt_bool(package, "autoexamples")?;
        let autobenches = self.opt_bool(package, "autobenches")?;
        let mut extras = Vec::new_in(self.alloc);
        for kind in ExtraKind::ALL {
            let key = kind.table();
            if let Some(list) = root.get(key) {
                if list.kind() != Kind::Array {
                    return Err(self.expected(list, key, "an array of tables (`[[test]]`, `[[example]]`, `[[bench]]`)"));
                }
                for table in list.children() {
                    extras.push(self.extra(table, key, kind)?);
                }
            }
        }
        let underscores_refused = match &edition {
            RawEdition::Written(edition, _) => edition.syntax >= edition::Edition::Edition2024,
            RawEdition::Absent | RawEdition::Inherited(_) => false,
        };
        let mut dependencies = Vec::new_in(self.alloc);
        self.dep_tables(root, None, underscores_refused, &mut dependencies)?;
        if let Some(targets) = root.get("target") {
            self.table(targets, "target")?;
            for cfg in targets.children() {
                self.table(cfg, "target")?;
                let target = String::from_str_in(cfg.key(), self.alloc);
                self.dep_tables(cfg, Some(&target), underscores_refused, &mut dependencies)?;
            }
        }
        let mut features = Vec::new_in(self.alloc);
        if let Some(table) = root.get("features") {
            self.table(table, "features")?;
            for feature in table.children() {
                features.push(Feature {
                    name: String::from_str_in(feature.key(), self.alloc),
                    enables: self.texts(feature, "features")?,
                    span: self.line(feature),
                });
            }
        }
        let build = match package.get("build") {
            None => RawBuild::Auto,
            Some(item) => match (item.as_bool(), item.as_str()) {
                (Some(true), _) => RawBuild::Auto,
                (Some(false), _) => RawBuild::None,
                (None, Some(path)) => RawBuild::Path(String::from_str_in(path, self.alloc), self.line(item)),
                (None, None) => return Err(self.expected(item, "package.build", "a path or a bool")),
            },
        };
        let links = self.opt_text(package, "links")?;
        let version = match package.get("version") {
            None => RawVersion::Absent,
            Some(item) => match self.inherits(item, "package.version")? {
                true => RawVersion::Inherited(self.line(item)),
                false => RawVersion::Written(self.text(item, "package.version")?),
            },
        };
        let mut meta: [RawMeta<'a>; 8] = core::array::from_fn(|_| RawMeta::Absent);
        for (at, &key) in META_KEYS.iter().enumerate() {
            if let Some(item) = package.get(key) {
                meta[at] = match self.inherits(item, key)? {
                    true => RawMeta::Inherited(self.line(item)),
                    false => RawMeta::Written(self.meta_value(item, at)?),
                };
            }
        }
        let resolver = self.resolver(package, "package.resolver")?;
        Ok(RawPackage {
            span,
            name,
            edition,
            workspace,
            autolib,
            autobins,
            lib,
            bins,
            autotests,
            autoexamples,
            autobenches,
            extras,
            dependencies,
            features,
            build,
            links,
            version,
            meta,
            resolver,
        })
    }

    /// A `[[test]]`, `[[example]]` or `[[bench]]` table: `name`, `path`, `harness`, `test` and
    /// `required-features` (the Cargo Book, "Cargo Targets", "Configuring a target").
    fn extra(&self, table: It<'_, '_, 'a>, key: &'static str, kind: ExtraKind) -> Result<RawExtra<'a>, LayoutError<'a>> {
        self.table(table, key)?;
        let required_features = match table.get("required-features") {
            Some(item) => self.texts(item, "required-features")?,
            None => Vec::new_in(self.alloc),
        };
        Ok(RawExtra {
            kind,
            name: self.opt_text(table, "name")?,
            path: self.opt_text(table, "path")?,
            harness: self.opt_bool(table, "harness")?,
            test: self.opt_bool(table, "test")?,
            required_features,
            span: self.line(table),
        })
    }

    fn target(&self, table: It<'_, '_, 'a>, key: &'static str) -> Result<RawTarget<'a>, LayoutError<'a>> {
        self.table(table, key)?;
        let proc_macro = match self.opt_bool(table, "proc-macro")? {
            Some(it) => it,
            None => self.opt_bool(table, "proc_macro")?.unwrap_or(false),
        };
        let required_features = match table.get("required-features") {
            Some(item) => self.texts(item, "required-features")?,
            None => Vec::new_in(self.alloc),
        };
        Ok(RawTarget {
            name: self.opt_text(table, "name")?,
            path: self.opt_text(table, "path")?,
            proc_macro,
            required_features,
            span: self.line(table),
        })
    }

    /// The three dependency tables of `table` (the root, or a `target.<cfg>`).
    fn dep_tables(
        &self,
        table: It<'_, '_, 'a>,
        target: Option<&Text<'a>>,
        underscores_refused: bool,
        out: &mut Vec<RawDep<'a>, Alloc<'a>>,
    ) -> Result<(), LayoutError<'a>> {
        const TABLES: [(&str, &str, DepKind); 3] = [
            ("dependencies", "dependencies", DepKind::Normal),
            ("dev-dependencies", "dev_dependencies", DepKind::Dev),
            ("build-dependencies", "build_dependencies", DepKind::Build),
        ];
        for (dashed, underscored, kind) in TABLES {
            if let Some(deps) = table.get(dashed) {
                self.deps(deps, dashed, kind, target, out)?;
            }
            if dashed != underscored
                && let Some(deps) = table.get(underscored)
            {
                // cargo reads the underscored spelling before the 2024 edition and refuses it
                // from 2024 on (`[dev_dependencies]`).
                if underscores_refused {
                    return Err(LayoutError::Underscored { at: self.line(deps) });
                }
                self.deps(deps, dashed, kind, target, out)?;
            }
        }
        Ok(())
    }

    fn deps(
        &self,
        deps: It<'_, '_, 'a>,
        key: &'static str,
        kind: DepKind,
        target: Option<&Text<'a>>,
        out: &mut Vec<RawDep<'a>, Alloc<'a>>,
    ) -> Result<(), LayoutError<'a>> {
        self.table(deps, key)?;
        for dep in deps.children() {
            out.push(self.dep(dep, key, kind, target)?);
        }
        Ok(())
    }

    fn dep(
        &self,
        dep: It<'_, '_, 'a>,
        key: &'static str,
        kind: DepKind,
        target: Option<&Text<'a>>,
    ) -> Result<RawDep<'a>, LayoutError<'a>> {
        let mut raw = RawDep {
            key: String::from_str_in(dep.key(), self.alloc),
            kind,
            target: target.cloned(),
            package: None,
            version: None,
            path: None,
            git: None,
            registry: None,
            workspace: false,
            optional: false,
            default_features: None,
            features: Vec::new_in(self.alloc),
            span: self.line(dep),
        };
        if let Some(version) = dep.as_str() {
            raw.version = Some(String::from_str_in(version, self.alloc));
            return Ok(raw);
        }
        if dep.kind() != Kind::Table {
            return Err(self.expected(dep, key, "a version string or a table"));
        }
        raw.package = self.opt_text(dep, "package")?;
        raw.version = self.opt_text(dep, "version")?;
        raw.path = self.opt_text(dep, "path")?;
        raw.registry = self.opt_text(dep, "registry")?;
        raw.workspace = self.opt_bool(dep, "workspace")?.unwrap_or(false);
        raw.optional = self.opt_bool(dep, "optional")?.unwrap_or(false);
        raw.default_features = match self.opt_bool(dep, "default-features")? {
            Some(it) => Some(it),
            None => self.opt_bool(dep, "default_features")?,
        };
        if let Some(features) = dep.get("features") {
            raw.features = self.texts(features, key)?;
        }
        if let Some(url) = self.opt_text(dep, "git")? {
            let reference = match (self.opt_text(dep, "branch")?, self.opt_text(dep, "tag")?, self.opt_text(dep, "rev")?) {
                (None, None, None) => GitRef::DefaultBranch,
                (Some(branch), None, None) => GitRef::Branch(branch),
                (None, Some(tag), None) => GitRef::Tag(tag),
                (None, None, Some(rev)) => GitRef::Rev(rev),
                _ => return Err(LayoutError::GitReferences { at: raw.span }),
            };
            raw.git = Some((url, reference));
        }
        if raw.git.is_some() && raw.path.is_some() {
            return Err(LayoutError::GitAndPath { at: raw.span });
        }
        Ok(raw)
    }
}
