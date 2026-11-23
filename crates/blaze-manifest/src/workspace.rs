//! A workspace as cargo 1.99 finds it (`src/workspace/workspace.rs` there): its root, found by
//! walking up from the current manifest; its members, from `members` globs and path
//! dependencies, less `exclude`; its default members; and cargo's checks that they agree.

use std::fmt;
use std::path::{Path, PathBuf};

use blaze_core::home::NoHome;
use blaze_data::Value;
use blaze_hash::{FixedState, Map, MapOpts, Set, SetOpts};
use std::alloc::Global;

use crate::error::{self, ManifestError, ResultExt};
use crate::features::{Edition, Features, ResolveBehavior};
use crate::files::Files;
use crate::manifest::{
    Context, EitherManifest, Manifest, WorkspaceConfig, WorkspaceRootConfig, read_manifest,
};
use crate::paths::{ancestors, normalize_path, relative_to};
use crate::schema::TomlProfiles;
use crate::{snippet, source};

/// What cargo refuses of a workspace's shape, in its words.
#[derive(Debug)]
pub enum WorkspaceError {
    /// A manifest path that is not absolute.
    NotAbsolute { manifest: PathBuf },
    /// No variable says where blaze keeps the workspace's build directory.
    NoBuildHome { workspace: PathBuf, why: NoHome },
    /// The root found is a member's manifest, not a root's.
    InferredNotRoot { root: PathBuf },
    /// A `default-members` entry that is not a member.
    DefaultNotMember { path: PathBuf, glob: Option<String>, root: PathBuf },
    DuplicateName { name: String, first: PathBuf, second: PathBuf },
    /// `package.workspace` points to a manifest with no `[workspace]`.
    RootNotWorkspace { current: PathBuf, root: PathBuf },
    MultipleRoots { roots: Vec<PathBuf> },
    WrongWorkspace { member: PathBuf, expected: PathBuf, actual: PathBuf },
    NotBelowRoot { member: PathBuf, root: PathBuf },
    /// The current package believes it is in a workspace that does not list it; `help` is
    /// cargo's line of what may fix it.
    NotInMembers { current: PathBuf, root: PathBuf, help: String },
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceError::NotAbsolute { manifest } => write!(
                f,
                "manifest_path:{manifest:?} is not an absolute path. Please provide an absolute \
                 path."
            ),
            WorkspaceError::NoBuildHome { workspace, why } => write!(
                f,
                "no BLAZE_HOME or XDG_CACHE_HOME says where blaze keeps the build directory of \
                 the workspace at {}, and {why}",
                workspace.display()
            ),
            WorkspaceError::InferredNotRoot { root } => {
                write!(f, "root of a workspace inferred but wasn't a root: {}", root.display())
            }
            WorkspaceError::DefaultNotMember { path, glob, root } => {
                write!(f, "package `{}` is listed in default-members", path.display())?;
                if let Some(glob) = glob {
                    write!(f, " via `{glob}`")?;
                }
                write!(f, " but is not a member\nfor workspace at `{}`.", root.display())
            }
            WorkspaceError::DuplicateName { name, first, second } => write!(
                f,
                "two packages named `{name}` in this workspace:\n- {}\n- {}",
                first.display(),
                second.display()
            ),
            WorkspaceError::RootNotWorkspace { current, root } => write!(
                f,
                "`package.workspace` configuration points to a crate \
                 which is not configured with [workspace]: \n\
                 configuration at: {}\n\
                 points to: {}",
                current.display(),
                root.display()
            ),
            WorkspaceError::MultipleRoots { roots } => {
                f.write_str("multiple workspace roots found in the same workspace:")?;
                for root in roots {
                    write!(f, "\n  {}", root.display())?;
                }
                Ok(())
            }
            WorkspaceError::WrongWorkspace { member, expected, actual } => write!(
                f,
                "package `{}` is a member of the wrong workspace\n\
                 expected: {}\n\
                 actual:   {}",
                member.display(),
                expected.display(),
                actual.display()
            ),
            WorkspaceError::NotBelowRoot { member, root } => write!(
                f,
                "workspace member `{}` is not hierarchically below the workspace root `{}`",
                member.display(),
                root.display()
            ),
            WorkspaceError::NotInMembers { current, root, help } => write!(
                f,
                "current package believes it's in a workspace when it's not:\n\
                 current:   {}\n\
                 workspace: {}\n\n{help}\n\
                 Alternatively, to keep it out of the workspace, add the package \
                 to the `workspace.exclude` array, or add an empty `[workspace]` \
                 table to the package's manifest.",
                current.display(),
                root.display(),
            ),
        }
    }
}

impl core::error::Error for WorkspaceError {}

/// The manifest of the workspace root above `manifest_path`, if one claims it.
pub(crate) fn find_workspace_root<F: Files>(
    manifest_path: &Path,
    ctx: &Context<F>,
) -> Result<Option<PathBuf>, ManifestError> {
    find_workspace_root_with_loader(manifest_path, ctx, |self_path| {
        let source_id = source::for_manifest_path(self_path)?;
        let manifest = read_manifest(self_path, source_id, ctx)?;
        Ok(manifest.workspace_config().get_ws_root(self_path, manifest_path))
    })
}

fn find_workspace_root_with_loader<F: Files>(
    manifest_path: &Path,
    ctx: &Context<F>,
    mut loader: impl FnMut(&Path) -> Result<Option<PathBuf>, ManifestError>,
) -> Result<Option<PathBuf>, ManifestError> {
    {
        let roots = ctx.ws_roots.borrow();
        for current in manifest_path.ancestors().skip(1) {
            if let Some(ws_config) = roots.get(current)
                && !ws_config.is_excluded(manifest_path)
            {
                return Ok(Some(current.join("Cargo.toml")));
            }
        }
    }

    for ances_manifest_path in find_root_iter(manifest_path, ctx) {
        let ws_root_path =
            loader(&ances_manifest_path).context(|| error::Context::WorkspaceSearch {
                manifest: manifest_path.to_owned(),
                potential: ances_manifest_path.clone(),
            })?;
        if let Some(ws_root_path) = ws_root_path {
            return Ok(Some(ws_root_path));
        }
    }
    Ok(None)
}

/// The `Cargo.toml` files above `manifest_path`, nearest first, stopping at `target/package`
/// and after `CARGO_HOME`.
fn find_root_iter<'a, F: Files>(
    manifest_path: &'a Path,
    ctx: &'a Context<F>,
) -> impl Iterator<Item = PathBuf> + 'a {
    let home = ctx.config.home();
    let mut last: Option<&Path> = None;
    ancestors(manifest_path, None)
        .skip(2)
        .map(move |curr| {
            let window = (curr, last);
            last = Some(curr);
            window
        })
        .take_while(|(curr, _)| !curr.ends_with("target/package"))
        .take_while(move |(_, last)| last.is_none_or(|last| last != home))
        .map(|(curr, _)| curr.join("Cargo.toml"))
        .filter(move |path| ctx.files().exists(path))
}

pub struct Workspace<'c, F: Files> {
    ctx: &'c Context<F>,
    current_manifest: PathBuf,
    packages: Map<PathBuf, EitherManifest, FixedState, Global>,
    root_manifest: Option<PathBuf>,
    target_dir: Option<PathBuf>,
    build_dir: Option<PathBuf>,
    /// In the order cargo finds them, which is the order `cargo metadata` prints them.
    members: Vec<PathBuf>,
    member_set: Set<PathBuf, FixedState, Global>,
    default_members: Vec<PathBuf>,
    resolve_behavior: ResolveBehavior,
    custom_metadata: Option<Value<Global>>,
    warnings: Vec<String>,
}

impl<'c, F: Files> Workspace<'c, F> {
    pub fn new(
        manifest_path: &Path,
        ctx: &'c Context<F>,
    ) -> Result<Workspace<'c, F>, ManifestError> {
        let mut ws = Workspace {
            ctx,
            current_manifest: manifest_path.to_owned(),
            packages: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
            root_manifest: None,
            target_dir: None,
            build_dir: None,
            members: Vec::new(),
            member_set: Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None }),
            default_members: Vec::new(),
            resolve_behavior: ResolveBehavior::V1,
            custom_metadata: None,
            warnings: Vec::new(),
        };
        // `has_root`: on Windows the daemon names `C:\a` as `/C:/a`, which has no prefix.
        if !manifest_path.has_root() {
            return Err(WorkspaceError::NotAbsolute { manifest: manifest_path.to_owned() }.into());
        }
        ws.root_manifest = ws.find_root(manifest_path)?;
        ws.target_dir = ctx.config.target_dir()?;
        ws.build_dir = ctx.config.build_dir(ws.root_manifest())?;
        ws.custom_metadata = ws.load_workspace_config()?.and_then(|cfg| cfg.custom_metadata);
        ws.find_members()?;
        ws.set_resolve_behavior();
        ws.validate()?;
        Ok(ws)
    }

    fn load(&mut self, manifest_path: &Path) -> Result<&EitherManifest, ManifestError> {
        if !self.packages.contains_key(manifest_path) {
            let source_id = source::for_manifest_path(manifest_path)?;
            let manifest = read_manifest(manifest_path, source_id, self.ctx)?;
            self.packages.insert(manifest_path.to_owned(), manifest);
        }
        Ok(&self.packages[manifest_path])
    }

    fn get(&self, manifest_path: &Path) -> &EitherManifest {
        &self.packages[manifest_path]
    }

    pub fn root_manifest(&self) -> &Path {
        self.root_manifest.as_deref().unwrap_or(&self.current_manifest)
    }

    pub fn root(&self) -> &Path {
        self.root_manifest().parent().unwrap()
    }

    pub fn current_manifest(&self) -> &Path {
        &self.current_manifest
    }

    pub fn is_virtual(&self) -> bool {
        matches!(self.get(&self.current_manifest), EitherManifest::Virtual(_))
    }

    /// The target directory `CARGO_TARGET_DIR` or `build.target-dir` names, if one does.
    pub fn explicit_target_dir(&self) -> Option<&Path> {
        self.target_dir.as_deref()
    }

    /// The directory `build.build-dir` names, if it does.
    pub fn explicit_build_dir(&self) -> Option<&Path> {
        self.build_dir.as_deref()
    }

    /// The target directory: [`Workspace::explicit_target_dir`], else the workspace's
    /// directory under blaze's root ([`crate::home::build_dir`]), never one in the checkout.
    /// The root is read from the configuration's variables; with none to say it this is an
    /// error naming the workspace, never a directory guessed.
    pub fn target_dir(&self) -> Result<PathBuf, ManifestError> {
        if let Some(dir) = &self.target_dir {
            return Ok(dir.clone());
        }
        let config = &self.ctx.config;
        let root = crate::home::root(
            |name| config.env(name).map(std::ffi::OsString::from),
            || config.user().0.clone(),
        );
        let root = root.map_err(|why| WorkspaceError::NoBuildHome {
            workspace: self.root().to_owned(),
            why,
        })?;
        Ok(crate::home::build_dir(&crate::home::builds(&root), self.root()))
    }

    /// `build.build-dir`, else [`Workspace::target_dir`].
    pub fn build_dir(&self) -> Result<PathBuf, ManifestError> {
        match &self.build_dir {
            Some(dir) => Ok(dir.clone()),
            None => self.target_dir(),
        }
    }

    pub fn custom_metadata(&self) -> Option<&Value<Global>> {
        self.custom_metadata.as_ref()
    }

    pub fn resolve_behavior(&self) -> ResolveBehavior {
        self.resolve_behavior
    }

    /// The root manifest's `[profile]` tables, the only ones cargo reads.
    pub fn profiles(&self) -> Option<&TomlProfiles> {
        match self.get(self.root_manifest()) {
            EitherManifest::Real(m) => m.normalized_toml.profile.as_ref(),
            EitherManifest::Virtual(vm) => vm.normalized_toml.profile.as_ref(),
        }
    }

    /// The root manifest's `cargo-features`.
    pub fn unstable_features(&self) -> &Features {
        match self.get(self.root_manifest()) {
            EitherManifest::Real(m) => &m.unstable_features,
            EitherManifest::Virtual(vm) => &vm.unstable_features,
        }
    }

    pub fn is_member(&self, manifest_path: &Path) -> bool {
        self.member_set.contains(manifest_path)
    }

    /// The member packages with their manifest paths, in cargo's order.
    pub fn members(&self) -> impl Iterator<Item = (&Path, &Manifest)> {
        self.members.iter().filter_map(|path| match self.get(path) {
            EitherManifest::Real(m) => Some((path.as_path(), &**m)),
            EitherManifest::Virtual(_) => None,
        })
    }

    pub fn default_members(&self) -> impl Iterator<Item = (&Path, &Manifest)> {
        self.default_members.iter().filter_map(|path| match self.get(path) {
            EitherManifest::Real(m) => Some((path.as_path(), &**m)),
            EitherManifest::Virtual(_) => None,
        })
    }

    /// The warnings cargo prints of the workspace as a whole, before those of its manifests.
    pub fn workspace_warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The warnings of every manifest read, by manifest path relative to the root as cargo
    /// prints them: a virtual root's first, then each package's. cargo takes the packages in
    /// the order of a `HashMap`; here they are in path order.
    pub fn warnings(&self) -> impl Iterator<Item = (&Path, &str)> {
        let root = self.root_manifest();
        let virtual_root = match self.packages.get(root) {
            Some(EitherManifest::Virtual(vm)) => Some((root, &vm.warnings)),
            _ => None,
        };
        let mut packages: Vec<(&Path, &Vec<String>)> = self
            .packages
            .iter()
            .filter_map(|(path, m)| match m {
                EitherManifest::Real(m) => Some((path.as_path(), &m.warnings)),
                EitherManifest::Virtual(_) => None,
            })
            .collect();
        packages.sort_by_key(|(path, _)| *path);
        let dir = self.root();
        virtual_root.into_iter().chain(packages).flat_map(move |(path, warnings)| {
            let rel = path.strip_prefix(dir).unwrap_or(path);
            warnings.iter().map(move |w| (rel, w.as_str()))
        })
    }

    fn load_workspace_config(&mut self) -> Result<Option<WorkspaceRootConfig>, ManifestError> {
        let Some(root_path) = self.root_manifest.clone() else {
            return Ok(None);
        };
        match self.load(&root_path)?.workspace_config() {
            WorkspaceConfig::Root(root_config) => Ok(Some((**root_config).clone())),
            _ => Err(WorkspaceError::InferredNotRoot { root: root_path }.into()),
        }
    }

    fn find_root(&mut self, manifest_path: &Path) -> Result<Option<PathBuf>, ManifestError> {
        let current = self.load(manifest_path)?;
        if let Some(root_path) =
            current.workspace_config().get_ws_root(manifest_path, manifest_path)
        {
            return Ok(Some(root_path));
        }
        let ctx = self.ctx;
        find_workspace_root_with_loader(manifest_path, ctx, |self_path| {
            Ok(self.load(self_path)?.workspace_config().get_ws_root(self_path, manifest_path))
        })
    }

    fn insert_member(&mut self, path: PathBuf) {
        if self.member_set.insert(path.clone()) {
            self.members.push(path);
        }
    }

    fn find_members(&mut self) -> Result<(), ManifestError> {
        let Some(workspace_config) = self.load_workspace_config()? else {
            let current = self.current_manifest.clone();
            self.insert_member(current.clone());
            self.default_members.push(current);
            return Ok(());
        };
        let root_manifest_path = self.root_manifest.clone().unwrap();

        let members_paths = workspace_config.members_paths(
            self.ctx.files(),
            workspace_config.members.as_deref().unwrap_or_default(),
        )?;
        let default_members_paths = if root_manifest_path == self.current_manifest {
            match &workspace_config.default_members {
                Some(default) => Some(workspace_config.members_paths(self.ctx.files(), default)?),
                None => None,
            }
        } else {
            None
        };

        for (path, glob) in &members_paths {
            self.find_path_deps(&path.join("Cargo.toml"), &root_manifest_path, false).context(
                || error::Context::WorkspaceMember {
                    path: path.clone(),
                    glob: glob.map(str::to_owned),
                    root: root_manifest_path.clone(),
                },
            )?;
        }

        self.find_path_deps(&root_manifest_path, &root_manifest_path, false)?;

        if let Some(default) = default_members_paths {
            for (path, default_member_glob) in default {
                let normalized_path = normalize_path(&path);
                let manifest_path = normalized_path.join("Cargo.toml");
                if !self.member_set.contains(&manifest_path) {
                    let exclude = members_paths.iter().any(|(m, _)| *m == normalized_path)
                        && workspace_config.is_excluded(&normalized_path);
                    if exclude {
                        continue;
                    }
                    return Err(WorkspaceError::DefaultNotMember {
                        path,
                        glob: default_member_glob.map(str::to_owned),
                        root: root_manifest_path,
                    }
                    .into());
                }
                self.default_members.push(manifest_path)
            }
        } else if self.is_virtual() {
            self.default_members = self.members.clone();
        } else {
            self.default_members.push(self.current_manifest.clone())
        }
        Ok(())
    }

    fn find_path_deps(
        &mut self,
        manifest_path: &Path,
        root_manifest: &Path,
        is_path_dep: bool,
    ) -> Result<(), ManifestError> {
        let manifest_path = normalize_path(manifest_path);
        if self.member_set.contains(&manifest_path) {
            return Ok(());
        }
        if is_path_dep && !manifest_path.parent().unwrap().starts_with(self.root()) {
            let root = self.find_root(&manifest_path)?;
            if root != self.root_manifest {
                return Ok(());
            }
        }

        if let WorkspaceConfig::Root(root_config) = self.load(root_manifest)?.workspace_config()
            && root_config.is_excluded(&manifest_path)
        {
            return Ok(());
        }

        self.insert_member(manifest_path.clone());

        let candidates = match self.load(&manifest_path)? {
            EitherManifest::Real(pkg) => pkg
                .dependencies
                .iter()
                .filter_map(|d| {
                    Some((d.source_id.local_path()?.join("Cargo.toml"), d.name.clone()))
                })
                .collect::<Vec<_>>(),
            EitherManifest::Virtual(_) => return Ok(()),
        };
        for (path, name) in candidates {
            self.find_path_deps(&path, root_manifest, true)
                .context(|| error::Context::DependencyManifest { name: name.to_string() })?;
        }
        Ok(())
    }

    fn set_resolve_behavior(&mut self) {
        self.resolve_behavior = match self.get(self.root_manifest()) {
            EitherManifest::Real(m) => {
                m.resolve_behavior.unwrap_or_else(|| m.edition.default_resolve_behavior())
            }
            EitherManifest::Virtual(vm) => vm.resolve_behavior.unwrap_or(ResolveBehavior::V1),
        };
    }

    fn validate(&mut self) -> Result<(), ManifestError> {
        if self.root_manifest.is_none() {
            return Ok(());
        }
        self.validate_unique_names()?;
        self.validate_workspace_roots()?;
        self.validate_members()?;
        self.error_if_manifest_not_in_members()?;
        self.validate_manifest();
        Ok(())
    }

    fn validate_unique_names(&self) -> Result<(), WorkspaceError> {
        let mut names = Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None });
        for member in &self.members {
            let name = match self.get(member) {
                EitherManifest::Real(m) => &m.package_id.name,
                EitherManifest::Virtual(_) => continue,
            };
            if let Some(prev) = names.insert(name, member) {
                return Err(WorkspaceError::DuplicateName {
                    name: name.to_string(),
                    first: prev.clone(),
                    second: member.clone(),
                });
            }
        }
        Ok(())
    }

    fn validate_workspace_roots(&self) -> Result<(), WorkspaceError> {
        let roots: Vec<PathBuf> = self
            .members
            .iter()
            .filter(|member| {
                matches!(self.get(member).workspace_config(), WorkspaceConfig::Root(_))
            })
            .map(|member| member.parent().unwrap().to_path_buf())
            .collect();
        match roots.len() {
            1 => Ok(()),
            0 => Err(WorkspaceError::RootNotWorkspace {
                current: self.current_manifest.clone(),
                root: self.root_manifest.clone().unwrap(),
            }),
            _ => Err(WorkspaceError::MultipleRoots { roots }),
        }
    }

    fn validate_members(&mut self) -> Result<(), ManifestError> {
        for member in self.members.clone() {
            let root = self.find_root(&member)?;
            if root == self.root_manifest {
                continue;
            }
            let expected = self.root_manifest.clone().unwrap();
            return Err(match root {
                Some(actual) => WorkspaceError::WrongWorkspace { member, expected, actual },
                None => WorkspaceError::NotBelowRoot { member, root: expected },
            }
            .into());
        }
        Ok(())
    }

    fn error_if_manifest_not_in_members(&mut self) -> Result<(), WorkspaceError> {
        if self.member_set.contains(&self.current_manifest) {
            return Ok(());
        }
        let root = self.root_manifest.as_ref().unwrap();
        let root_dir = normalize_path(root.parent().unwrap());
        let current_dir = normalize_path(self.current_manifest.parent().unwrap());
        let members_msg = match relative_to(&current_dir, &root_dir) {
            Some(rel) => format!(
                "this may be fixable by adding `{}` to the \
                 `workspace.members` array of the manifest \
                 located at: {}",
                rel.display(),
                root.display()
            ),
            None => format!(
                "this may be fixable by adding a member to \
                 the `workspace.members` array of the \
                 manifest located at: {}",
                root.display()
            ),
        };
        let extra = match self.get(root) {
            EitherManifest::Virtual(_) => members_msg,
            EitherManifest::Real(m) => {
                let has_members_list = match &m.workspace {
                    WorkspaceConfig::Root(root_config) => root_config.has_members_list(),
                    WorkspaceConfig::Member { .. } => unreachable!(),
                };
                if !has_members_list {
                    format!(
                        "this may be fixable by ensuring that this \
                         crate is depended on by the workspace \
                         root: {}",
                        root.display()
                    )
                } else {
                    members_msg
                }
            }
        };
        Err(WorkspaceError::NotInMembers {
            current: self.current_manifest.clone(),
            root: root.clone(),
            help: extra,
        })
    }

    /// cargo's warnings for settings only the root may have.
    fn validate_manifest(&mut self) {
        let Some(root_manifest) = self.root_manifest.as_ref() else { return };
        let resolve_behavior = self.resolve_behavior;
        let mut max_member_edition = None;
        for path in &self.members {
            if path == root_manifest {
                continue;
            }
            let Some(EitherManifest::Real(m)) = self.packages.get(path) else { continue };
            let warnings = &mut self.warnings;
            let mut warn = |what: &str| {
                warnings.push(format!(
                    "{what} for the non root package will be ignored, \
                     specify {what} at the workspace root:\n\
                     package:   {}\n\
                     workspace: {}",
                    path.display(),
                    root_manifest.display(),
                ))
            };
            if m.normalized_toml.has_profiles() {
                warn("profiles");
            }
            if !m.replace.is_empty() {
                warn("replace");
            }
            if !m.patch.is_empty() {
                warn("patch");
            }
            if m.resolve_behavior.is_some_and(|b| b != resolve_behavior) {
                warn("resolver");
            }
            if m.edition >= Edition::Edition2021 {
                max_member_edition = max_member_edition.max(Some(m.edition));
            }
        }
        if let Some(EitherManifest::Virtual(vm)) = self.packages.get(root_manifest)
            && vm.resolve_behavior.is_none()
            && let Some(edition) = max_member_edition
        {
            let resolver = edition.default_resolve_behavior().to_manifest();
            let title = format!(
                "virtual workspace defaulting to `resolver = \"1\"` despite one or more workspace members being on edition {edition} which implies `resolver = \"{resolver}\"`"
            );
            let to_use = format!(
                "to use the edition {edition} resolver, specify `workspace.resolver = \"{resolver}\"` in the workspace root's manifest"
            );
            let rendered = snippet::with_notes(
                snippet::Level::Warning,
                &title,
                &[
                    (snippet::Level::Note, "to keep the current resolver, specify `workspace.resolver = \"1\"` in the workspace root's manifest"),
                    (snippet::Level::Note, &to_use),
                    (snippet::Level::Note, "for more details see https://doc.rust-lang.org/cargo/reference/resolver.html#resolver-versions"),
                ],
            );
            self.warnings.push(rendered.strip_prefix("warning: ").unwrap_or(&rendered).to_owned());
        }
    }
}
