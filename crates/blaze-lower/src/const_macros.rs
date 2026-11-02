//! Const macros (docs/future/const-macros.md, behind the experiment `const-macros`): the file a
//! package's `[package.metadata.blaze] const-macros` names is a crate of the front end of its
//! own, a proc macro's to the def map, which no unit compiles: its macros are its functions,
//! run by the const evaluator ([`hir_ty_mir::const_macro`]).
//!
//! There is one such crate a package and platform, rooted at the file. Its dependencies are the
//! package's library's (a binary's, else a test's, when the build has no library of it for that
//! platform), never one of the package's own crates, then the standard library's `proc_macro`
//! and blaze's copy of `const-ast`; its edition and cfgs are that unit's. Every crate of the
//! package but its build script depends on it by the file's stem and has
//! `cfg(blaze_const_macros)`, which rustc never sets.

#![forbid(unsafe_code)]

use base_db::{
    AbsPathBuf, BuiltDependency, Crate, CrateDisplayName, CrateName, CrateOrigin, CrateType,
    CrateTypes, CrateWorkspaceData, Db, Env, FileId,
};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_query::db::Durability;
use blaze_units::{
    query::{PackagePlan, Units},
    schema::{Mode, Unit},
    stdlib::target::{self, CfgArgs},
};
use cfg::{CfgAtom, CfgDiff, CfgOptions};
use hir_expand::proc_macro::{ProcMacro, ProcMacros};
use hir_ty_mir::const_macro::{CONST_AST, CONST_AST_NAME};
use intern::Symbol;
use span::Edition;
use triomphe::Arc;

use super::{LowerError, Lowering, crates, files};

/// The cfg every crate of a package with const macros has.
pub(crate) const CFG: &str = "blaze_const_macros";

/// The macro crate of package `package` (an index of `Units::packages`) for one platform.
pub(crate) struct Group {
    pub(crate) package: usize,
    /// The file, absolute, as blaze-units names it.
    pub(crate) file: String,
    /// The unit whose dependencies, cfgs and edition the macro crate has.
    pub(crate) anchor: usize,
    /// The anchor's dependencies but build script runs and the package's own units, in its
    /// order.
    pub(crate) deps: Vec<usize>,
    /// The standard library's `proc_macro` and `core` for the platform.
    pub(crate) proc_macro: usize,
    pub(crate) core: usize,
    pub(crate) krate: Option<Crate>,
}

/// The build's macro crates, and which units depend on each.
pub(crate) struct Plan {
    /// By unit: the group whose macro crate it depends on.
    pub(crate) group_of: Vec<Option<usize>>,
    pub(crate) groups: Vec<Group>,
    /// By unit: the units its crate is made after besides its dependencies, so that its
    /// group's macro crate is made before it (`crates::topological`).
    pub(crate) after: Vec<Vec<usize>>,
    /// By platform (none: the host): the crate of `const-ast`, once made.
    pub(crate) const_ast: HashMap<Option<String>, Crate>,
}

impl Plan {
    pub(crate) fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// A macro crate as made, which [`Loaded::reload`] keeps up with its file.
pub(crate) struct Loaded {
    pub(crate) krate: Crate,
    pub(crate) root: FileId,
    pub(crate) edition: Edition,
    /// The content of the root file its macros were listed from.
    pub(crate) content: ContentId,
    /// The unit it was made for, for errors.
    pub(crate) what: String,
}

/// The macro crates `units` need: one for each package with const macros and platform a unit of
/// it compiles for but its build script.
pub(crate) fn plan(units: &Units, package_of: &HashMap<&str, usize>, whats: &[String]) -> Result<Plan, LowerError> {
    let all = &units.graph.units;
    let mut plan = Plan {
        group_of: vec![None; all.len()],
        groups: Vec::new(),
        after: vec![Vec::new(); all.len()],
        const_ast: HashMap::default(),
    };
    let mut by: HashMap<(usize, Option<&str>), usize> = HashMap::default();
    for (index, unit) in all.iter().enumerate() {
        if unit.mode == Mode::RunCustomBuild || unit.target.is_custom_build() {
            continue;
        }
        let Some(&package) = package_of.get(unit.pkg_id.as_str()) else { continue };
        let Some(file) = &units.packages[package].const_macros else { continue };
        let key = (package, unit.platform.as_deref());
        let group = match by.get(&key) {
            Some(&group) => group,
            None => {
                let group = plan.groups.len();
                plan.groups.push(group_of(units, package, file, unit.platform.as_deref(), whats)?);
                by.insert(key, group);
                group
            }
        };
        plan.group_of[index] = Some(group);
        let group = &plan.groups[group];
        plan.after[index].extend(group.deps.iter().copied().chain([group.proc_macro, group.core]));
    }
    Ok(plan)
}

fn group_of(
    units: &Units,
    package: usize,
    file: &str,
    platform: Option<&str>,
    whats: &[String],
) -> Result<Group, LowerError> {
    let all = &units.graph.units;
    let plan = &units.packages[package];
    let id = plan.id.as_str();
    let compiles = |index: usize, unit: &Unit| {
        unit.platform.as_deref() == platform
            && unit.mode != Mode::RunCustomBuild
            && !unit.target.is_custom_build()
            && !crates::is_test(unit, &units.plans[index])
    };
    // The library, else any other target built, else a test: the first by that order.
    let anchor = all
        .iter()
        .enumerate()
        .filter(|(_, unit)| unit.pkg_id == id && unit.platform.as_deref() == platform)
        .filter(|(_, unit)| unit.mode != Mode::RunCustomBuild && !unit.target.is_custom_build())
        .min_by_key(|&(index, unit)| (crates::is_test(unit, &units.plans[index]), !unit.target.is_lib(), index))
        .map(|(index, _)| index);
    let Some(anchor) = anchor else {
        return Err(LowerError::at(format!("the build compiles no crate of `{}` its const macros are of", plan.name), file));
    };
    let deps = all[anchor]
        .dependencies
        .iter()
        .map(|dep| dep.index)
        .filter(|&on| all.get(on).is_some_and(|it| it.mode != Mode::RunCustomBuild && it.pkg_id != id))
        .collect();
    let standard = |name: &str| {
        all.iter()
            .enumerate()
            .find(|&(index, unit)| unit.is_std && unit.target.name == name && compiles(index, unit))
            .map(|(index, _)| index)
            .ok_or_else(|| {
                LowerError::at(
                    format!(
                        "the const macros of {} need the standard library's `{name}`, which the build \
                         has none of for {}",
                        whats[anchor],
                        platform.unwrap_or("the host")
                    ),
                    file,
                )
            })
    };
    Ok(Group {
        package,
        file: file.to_owned(),
        anchor,
        deps,
        proc_macro: standard("proc_macro")?,
        core: standard("core")?,
        krate: None,
    })
}

/// Where the front end has blaze's copy of `const-ast`: beside the standard library's sources,
/// in no mount, its files given from [`CONST_AST`].
pub(crate) fn const_ast_dir(library: &str) -> String {
    let parent = files::parent(library.trim_end_matches('/'));
    match parent {
        "/" => "/blaze/const-ast".to_owned(),
        parent => format!("{parent}/blaze/const-ast"),
    }
}

/// [`CONST_AST`]'s files under `dir`.
pub(crate) fn const_ast_files(dir: &str) -> Vec<files::Loaded> {
    CONST_AST
        .iter()
        .map(|(name, text)| files::Loaded {
            path: format!("{dir}/{name}"),
            id: ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes()),
            bytes: blaze_vfs::Bytes::from(std::sync::Arc::<[u8]>::from(text.as_bytes())),
        })
        .collect()
}

/// What making a group's crates reads.
pub(crate) struct Cx<'a> {
    pub(crate) units: &'a Units,
    pub(crate) whats: &'a [String],
    pub(crate) lowerings: &'a [Lowering],
    pub(crate) workspace: Arc<CrateWorkspaceData>,
    pub(crate) triple: &'a str,
    pub(crate) host_cpu: Option<&'a target::HostCpu>,
    /// The macro file's crate root, if the package's files have it.
    pub(crate) root: Option<FileId>,
    /// The root file of `const-ast` for the platform.
    pub(crate) const_ast_root: Option<FileId>,
    pub(crate) const_ast_dir: &'a str,
    pub(crate) proc_macro_cwd: AbsPathBuf,
    pub(crate) durability: Durability,
}

/// The macro crate of `group`, made with the crate of `const-ast` for its platform if that is
/// not made yet: each made pushed to `all`, dependencies first. Its macros are listed from its
/// file's text.
pub(crate) fn make(
    db: &mut Db,
    plan: &mut Plan,
    group: usize,
    cx: Cx<'_>,
    all: &mut Vec<Crate>,
) -> Result<(Loaded, Vec<ProcMacro>), LowerError> {
    let Group { package, anchor, proc_macro, core, .. } = plan.groups[group];
    let file = plan.groups[group].file.clone();
    let file = file.as_str();
    let deps = plan.groups[group].deps.clone();
    let units = cx.units;
    let unit = &units.graph.units[anchor];
    let what = &cx.whats[anchor];
    let package_plan: &PackagePlan = &units.packages[package];
    let Some(root) = cx.root else {
        return Err(LowerError::at(
            format!(
                "`const-macros` under [package.metadata.blaze] in {} names `{file}`, which does not exist",
                package_plan.manifest_path
            ),
            file,
        ));
    };
    let name = |text: &str| CrateName::new(text).expect("a crate's name");
    let made = |at: usize| {
        cx.lowerings[at].krate.ok_or_else(|| {
            LowerError::at(format!("the const macros of {what} depend on {}, which has no crate", cx.whats[at]), file)
        })
    };

    let platform = unit.platform.clone();
    let const_ast = match plan.const_ast.get(&platform) {
        Some(&krate) => krate,
        None => {
            let Some(const_ast_root) = cx.const_ast_root else {
                return Err(LowerError::at("blaze's copy of `const-ast` is not among the front end's files", cx.const_ast_dir));
            };
            let core = BuiltDependency::with_prelude(name("core"), made(core)?, true, true);
            let data = base_db::BuiltCrateData {
                root_file_id: const_ast_root,
                edition: Edition::Edition2024,
                dependencies: vec![core],
                origin: CrateOrigin::Library { repo: None, name: Symbol::intern(CONST_AST_NAME) },
                crate_attrs: Box::new([]),
                target_kind: base_db::TargetKind::Lib { is_proc_macro: false },
                crate_types: CrateTypes::of(&[CrateType::Rlib]),
                proc_macro_cwd: Arc::new(abs(cx.const_ast_dir)?),
            };
            let extra = base_db::ExtraCrateData {
                version: None,
                display_name: Some(CrateDisplayName::from_canonical_name(CONST_AST_NAME)),
                potential_cfg_options: None,
            };
            let mut cfg = CfgOptions::default();
            for it in target::target_cfg(cx.triple, &CfgArgs::default()).map_err(|why| LowerError::at(why, cx.triple))? {
                cfg.extend([crates::atom(&it).map_err(|why| LowerError::at(why, cx.triple))?]);
            }
            let krate = Crate::create(db, data, extra, cx.workspace.clone(), cfg, Env::default(), Durability::HIGH);
            all.push(krate);
            plan.const_ast.insert(platform, krate);
            krate
        }
    };

    let plan_of = &units.plans[anchor];
    let mut dependencies = Vec::with_capacity(deps.len() + 2);
    for dep in unit.dependencies.iter().filter(|dep| deps.contains(&dep.index)) {
        let on = &units.graph.units[dep.index];
        dependencies.push(crates::dependency(unit, on, dep, made(dep.index)?, what)?);
    }
    if !dependencies.iter().any(|it| it.name.as_str() == "proc_macro") {
        dependencies.push(BuiltDependency::with_prelude(name("proc_macro"), made(proc_macro)?, true, true));
    }
    dependencies.push(BuiltDependency::with_prelude(name(CONST_AST_NAME), const_ast, true, false));

    let edition = crates::edition(unit, what)?;
    let mut cfg = crates::cfg(unit, plan_of, cx.triple, cx.host_cpu, what)?;
    cfg.apply_diff(CfgDiff::new(
        vec![CfgAtom::Flag(Symbol::intern("proc_macro")), CfgAtom::Flag(Symbol::intern(CFG))],
        vec![CfgAtom::Flag(Symbol::intern("test"))],
    ));
    let stem = stem(file);
    let data = base_db::BuiltCrateData {
        root_file_id: root,
        edition,
        dependencies,
        origin: crates::origin(package_plan, unit),
        crate_attrs: Box::new([]),
        target_kind: base_db::TargetKind::Lib { is_proc_macro: true },
        crate_types: CrateTypes::of(&[CrateType::ProcMacro]),
        proc_macro_cwd: Arc::new(cx.proc_macro_cwd),
    };
    let extra = base_db::ExtraCrateData {
        version: Some(package_plan.version.clone()),
        display_name: Some(CrateDisplayName::from_canonical_name(stem)),
        potential_cfg_options: None,
    };
    let env: Env = plan_of.env.iter().cloned().collect();
    let krate = Crate::create(db, data, extra, cx.workspace, cfg, env, cx.durability);
    all.push(krate);
    plan.groups[group].krate = Some(krate);
    let db: &Db = db;
    let file_text = db.file_text(root);
    let macros = listed(root, &**file_text.text(db), edition);
    let content = file_text.content(db);
    let loaded = Loaded { krate, root, edition, content, what: what.clone() };
    Ok((loaded, macros))
}

/// The name the package's crates depend on the macro crate by: its file's stem, dashes as
/// underscores.
pub(crate) fn dependency_name(file: &str) -> Result<CrateName, LowerError> {
    let stem = stem(file);
    let is_name = stem.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && stem.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-');
    match is_name {
        true => Ok(CrateName::normalize_dashes(stem)),
        false => Err(LowerError::at(format!("the const macros' file `{file}` has no crate's name for a stem"), file)),
    }
}

fn stem(file: &str) -> &str {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.strip_suffix(".rs").unwrap_or(name)
}

/// The macros of the macro crate whose root is `root`, in the order [`super::Lowered::macros`]
/// sorts a proc macro's.
fn listed(root: FileId, text: &str, edition: Edition) -> Vec<ProcMacro> {
    let mut macros = hir_ty_mir::const_macro::load(root, text, edition);
    macros.sort_unstable_by(|a, b| a.name.cmp_order(&b.name).then_with(|| a.kind.cmp(&b.kind)));
    macros
}

/// Sets each of `made`'s macros among the database's proc macros.
pub(crate) fn set_macros(db: &mut Db, made: Vec<(Crate, Vec<ProcMacro>)>) {
    if made.is_empty() {
        return;
    }
    ProcMacros::give(db, made.into_iter().map(|(krate, macros)| (krate, Ok(macros))));
}

impl Loaded {
    /// Its macros listed again if its file changed since; none when it did not. An error when
    /// the file is gone from the front end.
    pub(crate) fn reload(&mut self, db: &Db, paths: &std::collections::HashMap<FileId, String>) -> Result<Option<Vec<ProcMacro>>, LowerError> {
        if !paths.contains_key(&self.root) {
            return Err(LowerError::at("the file of the const macros is gone from its package's files", self.what.clone()));
        }
        let input = db.file_text(self.root);
        let content = input.content(db);
        if content == self.content {
            return Ok(None);
        }
        self.content = content;
        Ok(Some(listed(self.root, &**input.text(db), self.edition)))
    }
}

fn abs(path: &str) -> Result<AbsPathBuf, LowerError> {
    AbsPathBuf::try_from(path).map_err(|_| LowerError::at(format!("`{path}` is not an absolute path"), path))
}
