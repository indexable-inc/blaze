//! What each unit's crate is: its cfg (blaze's target table, never `rustc --print`), edition,
//! kind, origin and dependencies, as cargo passes them to rustc; and the workspace's root and
//! `[workspace.metadata.blaze]`, read from its manifest through the mounts.

#![forbid(unsafe_code)]

use std::borrow::Cow;

use base_db::{
    BuiltDependency, ConstEvalBudget, Crate, CrateName, CrateOrigin, CrateType, CrateTypes,
    CrateWorkspaceData, LangCrateOrigin, TargetKind, Version,
    target::{Arch, TargetData},
};
use blaze_units::{
    query::{PackagePlan, UnitPlan, Units},
    schema::{BuiltFor, Mode, Unit, UnitDep},
    stdlib::target::{self, CfgArgs, HostCpu},
};
use cfg::{CfgAtom, CfgOptions};
use intern::Symbol;
use span::Edition;
use triomphe::Arc;

use super::{LowerError, files};

/// The stable identities of the build-script runs a crate reads, each run by its run class's
/// ([`run_classes`]; `class_of` is a unit's class). A profile can split runs of one package even
/// for the same platform and features; their output directories are machine paths, so keys use
/// the runs' portable unit identities instead (#1610). Runs of one class are one run to the
/// front end, so units apart only in which run of a class they read keep one crate.
pub(crate) fn build_script_runs(
    units: &Units,
    unit: &Unit,
    class_of: impl Fn(usize) -> usize,
) -> Box<[String]> {
    unit.dependencies
        .iter()
        .filter(|dep| units.graph.units[dep.index].mode == Mode::RunCustomBuild)
        .map(|dep| units.plans[class_of(dep.index)].unit_id.clone()) // clone: one stable identity per run class read by a crate
        .collect() // alloc: crate graph input, kept for the front end's lifetime
}

/// `` `foo` (lib test for x86_64-unknown-linux-gnu) of foo v0.1.0 ``: a unit as an error names it.
pub(crate) fn describe(package: &PackagePlan, unit: &Unit) -> String {
    let mode = match unit.mode {
        Mode::Build => "",
        Mode::Test => " test",
        Mode::Check => " check",
        Mode::Doc => " doc",
        Mode::Doctest => " doctest",
        Mode::Docscrape => " docscrape",
        Mode::RunCustomBuild => " run",
    };
    let platform = unit.built_for();
    format!(
        "`{}` ({}{mode} for {platform}) of {} v{}",
        unit.target.name,
        unit.target.kind.join(", "),
        package.name,
        package.version
    )
}

/// Whether rustc compiles the unit with `--test`.
pub(crate) fn is_test(unit: &Unit, plan: &UnitPlan) -> bool {
    unit.mode == Mode::Test || plan.checked_as_test
}

/// Whether the package is a path package (a member, or one cargo names by its directory), as
/// opposed to a registry or git package.
pub(crate) fn is_path(package: &PackagePlan) -> bool {
    package.is_member || package.source.as_deref().is_some_and(|it| !it.contains("://"))
}

/// The unit's root file as cargo passes it to rustc: absolute and `/`-separated, its `.`s and
/// `..`s kept. cargo's `src_path` is the package's directory joined with `[lib] path` as
/// written, and rustc names the crate's files from it (`file!()`, panic and `#[track_caller]`
/// locations, debug info): a root of `path = "../../library/x/src/lib.rs"` names its module
/// `more` `crates/x/../../library/x/src/more.rs`. Only naming reads it; [`src_path`] finds the
/// file.
pub(crate) fn spelled_src_path(unit: &Unit, what: &str) -> Result<String, LowerError> {
    let Some(path) = unit.target.src_path.to_str() else {
        return Err(LowerError::at(
            format!("the root file of {what} is not a UTF-8 path"),
            unit.target.src_path.to_string_lossy().into_owned(),
        ));
    };
    Ok(match path.contains('\\') {
        true => path.replace('\\', "/"),
        false => path.to_owned(),
    })
}

/// [`spelled_src_path`] normal (`.` and `..` taken out), as the mounts and the source roots name
/// paths: as cargo spells it, a root outside the package keeps its `..`s, which by its bytes is
/// under the package's own directory and by its name in no source root. `..` is taken out by
/// the text, as the front end resolves `mod` and `#[path]` and as `files::follow` does.
pub(crate) fn src_path(unit: &Unit, what: &str) -> Result<String, LowerError> {
    Ok(files::normalize(&spelled_src_path(unit, what)?))
}

/// blaze's rustc's version (`1.101.0-nightly`), which the front end gates nightly behavior on.
pub(crate) fn toolchain() -> Result<Version, LowerError> {
    let version = target::rustc_version();
    let number = version.split(' ').next().unwrap_or(version);
    Version::parse(number).map_err(|error| {
        LowerError::new(format!("blaze's rustc version `{version}` is not a version: {error}"))
    })
}

/// What every crate built for `triple` shares: its target's data layout (blaze's table), the
/// toolchain and the const-eval budget.
pub(crate) fn workspace_data(
    triple: &str,
    toolchain: &Version,
    const_eval: ConstEvalBudget,
) -> Result<Arc<CrateWorkspaceData>, LowerError> {
    Ok(Arc::new(CrateWorkspaceData {
        target: Ok(target_data(triple)?),
        toolchain: Some(toolchain.clone()),
        const_eval,
    }))
}

/// The target data every crate built for `triple` is given, which the front end lays types out
/// by: its data layout from blaze's recorded spec, its arch and `has_thumb_interworking`. The
/// code generator's toolchain id hashes it (`hir_ty_codegen::v2::toolchain`), as the data is
/// past the code generator's packages.
pub fn target_data(triple: &str) -> Result<TargetData, LowerError> {
    let data_layout = target::data_layout(triple).map_err(|what| LowerError::at(what, triple))?;
    let has_thumb_interworking =
        target::has_thumb_interworking(triple).map_err(|what| LowerError::at(what, triple))?;
    let arch = match triple.split('-').next() {
        Some("wasm32") => Arch::Wasm32,
        Some("wasm64") => Arch::Wasm64,
        _ => Arch::Other,
    };
    Ok(TargetData {
        tuple: Symbol::intern(triple),
        data_layout: data_layout.into_boxed_str(),
        arch,
        has_thumb_interworking,
    })
}

/// The cfgs rustc compiles the unit with: the target's, with what the profile, the crate type,
/// `--test`, cargo's `--cfg`s and the rustflags decide, in the order cargo passes them (the
/// profile's `-C`s first, the rustflags last, so theirs win).
pub(crate) fn cfg(
    unit: &Unit,
    plan: &UnitPlan,
    triple: &str,
    host_cpu: Option<&HostCpu>,
    what: &str,
) -> Result<CfgOptions, LowerError> {
    let test = is_test(unit, plan);
    let profile = &unit.profile;
    let on = |yes: bool| if yes { "on" } else { "off" };
    let mut flags: Vec<String> =
        Vec::with_capacity(10 + 2 * plan.cfgs.len() + plan.rustflags.len());
    flags.extend(["-C".to_owned(), format!("opt-level={}", profile.opt_level)]);
    flags.extend(["-C".to_owned(), format!("debug-assertions={}", on(profile.debug_assertions))]);
    flags.extend(["-C".to_owned(), format!("overflow-checks={}", on(profile.overflow_checks))]);
    // cargo passes `-C panic` only for a strategy other than unwinding, which leaves a target
    // whose own strategy is `abort` at it.
    if profile.panic != "unwind" {
        flags.extend(["-C".to_owned(), format!("panic={}", profile.panic)]);
    }
    // cargo passes no `--crate-type` with `--test`.
    if unit.target.is_proc_macro() && !test {
        flags.extend(["--crate-type".to_owned(), "proc-macro".to_owned()]);
    }
    for cfg in &plan.cfgs {
        flags.extend(["--cfg".to_owned(), cfg.clone()]);
    }
    flags.extend(plan.rustflags.iter().cloned());

    let mut args = CfgArgs::from_flags(&flags).map_err(|why| LowerError::at(why, what))?;
    args.host_cpu = host_cpu.cloned();
    let cfgs = target::target_cfg(triple, &args).map_err(|why| LowerError::at(why, what))?;
    let mut options = CfgOptions::default();
    for cfg in &cfgs {
        options.extend([atom(cfg).map_err(|why| LowerError::at(why, what))?]);
    }
    if test {
        options.insert_atom(Symbol::intern("test"));
    }
    // rustdoc reads a library's examples from the library as `--cfg doc --cfg doctest` has it
    // (`rustdoc::doctest::run`); the examples themselves are built without either.
    if unit.mode == Mode::Doctest {
        options.insert_atom(Symbol::intern("doc"));
        options.insert_atom(Symbol::intern("doctest"));
    }
    // blaze's own atom, after the target's cfgs and rustc's flags, so `--print cfg` and build
    // scripts' `CARGO_CFG_*` never see it: MIR building reads it to check native contract
    // clauses (docs/v2/plan/design/contracts.md, part 1). The crate's cfg is an input of every
    // body lowering, so each key over a body covers it (rule 21).
    if profile.contract_checks {
        options.insert_atom(Symbol::intern(cfg::CONTRACT_CHECKS));
    }
    Ok(options)
}

/// The cfg keys a crate that links `proc_macro` is not told of the wasm proc macros' platform
/// ([`host_side`]).
const HOST_SIDE_UNTOLD: [&str; 2] = ["target_os", "target_family"];

/// `cfg`, of a crate built to run in the macro host ([`UnitPlan::macro_wasm`]) that links
/// `proc_macro` ([`links_proc_macro`]), without the platform's `target_os` and
/// `target_family`. rustc builds a proc macro and the crates it uses for the host, which has
/// neither `target_os = "wasi"` nor `target_family = "wasm"`, and crates gate their
/// `proc_macro` items on just that: synstructure 0.12 to 0.14 and syn 1 leave out
/// `extern crate proc_macro` (and synstructure its `decl_derive!`) under
/// `all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi"))`, so a proc
/// macro built with `synstructure::decl_derive` would not resolve (`superset-of-rust`). Every
/// other crate of the macro host keeps the platform's cfg whole: libc, getrandom and wasi pick
/// the WASI code the module's std runs on by it. The arch and pointer width stay, as they are
/// the code's. Not for the build script's `CARGO_CFG_*`, which describe the platform.
pub(crate) fn host_side(cfg: &mut CfgOptions) {
    let mut untold = Vec::new(); // alloc: once per macro-host crate, a handful of atoms
    for key in HOST_SIDE_UNTOLD {
        for value in cfg.get_cfg_values(key) {
            untold.push(CfgAtom::KeyValue { key: Symbol::intern(key), value: value.clone() });
        }
    }
    cfg.apply_diff(cfg::CfgDiff::new(Vec::new(), untold));
}

/// Whether a crate of the macro host links `proc_macro`, so is told of the host's side
/// ([`host_side`]): a proc macro, or a crate with an `extern crate proc_macro` item anywhere in
/// its root file, under any cfg (the cfg that would hide it is the one being decided). Read
/// from the root's syntax, so a comment, a string or a macro's body that only says it is not
/// taken for it, nor `extern crate proc_macro2`.
pub(crate) fn links_proc_macro(is_proc_macro: bool, root_text: &str, edition: Edition) -> bool {
    use syntax::{AstNode, ast};
    if is_proc_macro {
        return true;
    }
    // A cheap refusal first: most roots of the macro host never say it.
    if !root_text.contains("proc_macro") {
        return false;
    }
    let parse = ast::SourceFile::parse(root_text, edition);
    parse
        .tree()
        .syntax()
        .descendants()
        .filter_map(ast::ExternCrate::cast)
        .any(|item| item.name_ref().is_some_and(|name| name.text() == "proc_macro"))
}

/// A cfg as `--cfg` takes it: `name`, or `name="value"` with the value a string literal.
pub(crate) fn atom(cfg: &str) -> Result<CfgAtom, String> {
    let (name, value) = match cfg.split_once('=') {
        Some((name, value)) => (name.trim(), Some(value.trim())),
        None => (cfg.trim(), None),
    };
    let name = name.strip_prefix("r#").unwrap_or(name);
    let is_name = name.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !is_name {
        return Err(format!("`--cfg {cfg}`: `{name}` is not a cfg's name"));
    }
    let Some(value) = value else {
        return Ok(CfgAtom::Flag(Symbol::intern(name)));
    };
    let Some(inner) = value.strip_prefix('"').and_then(|it| it.strip_suffix('"')) else {
        return Err(format!(
            "`--cfg {cfg}`: a cfg's value is a string in quotes, as `{name}=\"value\"`"
        ));
    };
    let value = unescape(inner)
        .ok_or_else(|| format!("`--cfg {cfg}`: the value has an escape rustc does not take"))?;
    Ok(CfgAtom::KeyValue { key: Symbol::intern(name), value: Symbol::intern(&value) })
}

/// A string literal's text without its escapes; `None` for an escape a literal can't have.
fn unescape(text: &str) -> Option<Cow<'_, str>> {
    if !text.contains('\\') {
        return Some(Cow::Borrowed(text));
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '0' => out.push('\0'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'x' => {
                let hex: String = chars.by_ref().take(2).collect();
                let byte = u8::from_str_radix(&hex, 16).ok().filter(|it| it.is_ascii())?;
                out.push(byte as char);
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let hex: String = chars.by_ref().take_while(|it| *it != '}').collect();
                out.push(char::from_u32(u32::from_str_radix(&hex.replace('_', ""), 16).ok()?)?);
            }
            '\n' => {
                let rest = chars.as_str().trim_start();
                chars = rest.chars();
            }
            _ => return None,
        }
    }
    Some(Cow::Owned(out))
}

/// The units that compile (every mode but `RunCustomBuild`), each after the units it depends
/// on and those `after` has for it (by unit; a crate made with its crate depends on them): the
/// order their crates are made in. A cycle is an error naming it.
pub(crate) fn topological(
    units: &Units,
    whats: &[String],
    after: &[Vec<usize>],
) -> Result<Vec<usize>, LowerError> {
    const UNSEEN: u8 = 0;
    const OPEN: u8 = 1;
    const DONE: u8 = 2;
    let all = &units.graph.units;
    let mut state = vec![UNSEEN; all.len()];
    let mut order = Vec::with_capacity(all.len());
    // Each unit being walked, and the index of its next dependency.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for start in 0..all.len() {
        if state[start] != UNSEEN || all[start].mode == Mode::RunCustomBuild {
            continue;
        }
        state[start] = OPEN;
        stack.push((start, 0));
        while let Some(top) = stack.last_mut() {
            let (at, edge) = *top;
            let deps = &all[at].dependencies;
            let (on, named) = match deps.get(edge) {
                Some(dep) => (dep.index, dep.extern_crate_name.as_str()),
                None => match after.get(at).and_then(|it| it.get(edge - deps.len())) {
                    Some(&on) => (on, "const macros"),
                    None => {
                        stack.pop();
                        state[at] = DONE;
                        order.push(at);
                        continue;
                    }
                },
            };
            top.1 += 1;
            let Some(unit) = all.get(on) else {
                return Err(LowerError::at(
                    format!("a dependency `{named}` on unit {on}, which the build lacks"),
                    whats[at].clone(),
                ));
            };
            if unit.mode == Mode::RunCustomBuild {
                continue;
            }
            match state[on] {
                UNSEEN => {
                    state[on] = OPEN;
                    stack.push((on, 0));
                }
                OPEN => {
                    let from = stack.iter().position(|it| it.0 == on).unwrap_or(0);
                    let cycle: Vec<&str> = stack[from..]
                        .iter()
                        .map(|it| whats[it.0].as_str())
                        .chain([whats[on].as_str()])
                        .collect();
                    return Err(LowerError::at(
                        format!(
                            "the units depend on each other in a cycle: {}",
                            cycle.join(" -> ")
                        ),
                        whats[on].clone(),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(order)
}

/// `unit`'s dependency on `on` (whose crate is `krate`) by the name its code calls it: in the
/// extern prelude unless cargo passes it `noprelude` (build-std's standard library crates).
/// A proc macro's `proc_macro` is in it: cargo passes a proc-macro target `--extern
/// proc_macro` beside build-std's `noprelude` one, and rustc puts a crate in the extern prelude
/// when any `--extern` naming it is not `noprelude`.
pub(crate) fn dependency(
    unit: &Unit,
    on: &Unit,
    dep: &UnitDep,
    krate: Crate,
    what: &str,
) -> Result<BuiltDependency, LowerError> {
    let name = CrateName::new(&dep.extern_crate_name).map_err(|_| {
        LowerError::at(format!("`{}` is not a crate's name", dep.extern_crate_name), what)
    })?;
    let prelude = !dep.noprelude.unwrap_or(false)
        || (unit.target.is_proc_macro() && dep.extern_crate_name == "proc_macro");
    Ok(BuiltDependency::with_prelude(name, krate, prelude, on.is_std && !unit.is_std))
}

pub(crate) fn edition(unit: &Unit, what: &str) -> Result<Edition, LowerError> {
    unit.target.edition.parse::<Edition>().map_err(|_| {
        LowerError::at(
            format!("{what} has edition `{}`, which blaze does not know", unit.target.edition),
            what,
        )
    })
}

/// The front end's form of what a unit is built for ([`Unit::built_for`], the source of truth),
/// as cargo's units tell two builds of one package apart (the host, a target triple or the wasm
/// proc macros): two crates alike but for this are two, and their keys say so. The one
/// conversion from `blaze_units::schema::BuiltFor` to `base_db::BuiltFor`: every crate the
/// lowering makes is given it from here.
pub(crate) fn built_for(of: BuiltFor<'_>) -> base_db::BuiltFor {
    base_db::BuiltFor { platform: of.platform.map(Symbol::intern), macro_wasm: of.macro_wasm }
}

/// The variables of a build script's run that its dependents' profile decides beside
/// `OUT_DIR`: `OPT_LEVEL` and `DEBUG`, the profile's opt-level and debuginfo. A `cc`-based
/// script writes other objects for each, and may write other sources, cfgs or variables too.
const RUN_PROFILE_VARS: [&str; 2] = ["OPT_LEVEL", "DEBUG"];

/// The cfg atom of a crate whose unit depends on a build script's run of a run family of more
/// than one run class ([`run_classes`]): `blaze::build_script_profile = "OPT_LEVEL=0 DEBUG=false"`,
/// that run's [`RUN_PROFILE_VARS`]. Two such crates alike but for their run are two, and their
/// output may differ, so this tells their keys apart (`blaze_items::key::crate_key` hashes every
/// cfg atom; rule keys-cover-every-input), and every dependent's key after them by its
/// dependencies'. Not an identifier, so no `cfg(...)` of a program names it, and set after the
/// target's cfgs and rustc's flags, so neither `--print cfg` nor `CARGO_CFG_*` shows it.
pub(crate) const RUN_PROFILE: &str = "blaze::build_script_profile";

/// What makes two build script runs one run class, or one run family ([`run_classes`]):
/// everything of the run but the variables `ignored` names. Its dependencies are by their unit,
/// a run's by its class (or family).
#[derive(PartialEq, Eq, Hash)]
struct RunKey<'a> {
    pkg_id: &'a str,
    target: &'a str,
    src_path: &'a std::path::Path,
    built_for: BuiltFor<'a>,
    features: &'a [String],
    is_std: bool,
    deps: Vec<(usize, &'a str)>,
    env: Vec<(&'a str, &'a str)>,
    cfgs: &'a [String],
    rustflags: &'a [String],
    links: Option<&'a str>,
    unsandboxed: bool,
}

/// The build's runs of build scripts by what the front end can tell of them before they run.
pub(crate) struct RunClasses {
    /// Each unit's run class: for a build script's run, the first run (by index) of the runs of
    /// that build script apart only in their `OUT_DIR`, whose name cargo hashes from the
    /// profile (two profiles apart only in what the run never reads, as `codegen-units`, `lto`
    /// or `strip`). Their inputs are the same, so they are one run to the front end, read under
    /// the first's `OUT_DIR`, and the units apart only in which run of a class they depend on
    /// are one crate (`lower_writes`). Any other unit is its own.
    pub(crate) class: Vec<usize>,
    /// By unit, for a run: whether its run family (its runs apart only in `OUT_DIR` and
    /// [`RUN_PROFILE_VARS`]) has runs of another class. cargo runs a build script once per
    /// profile of the units depending on it, so a release build's host package reached both
    /// as a normal dependency (`profile.release`) and as a build dependency
    /// (`profile.release.build-override`, opt-level 0) has two runs that differ in `OPT_LEVEL`.
    /// What they give is known only once both ran, and may differ, so their dependents are two
    /// crates, each read from its own run as rustc reads them, and the crates of such a run's
    /// dependents carry [`RUN_PROFILE`].
    pub(crate) keyed: Vec<bool>,
}

/// The build's [`RunClasses`].
pub(crate) fn run_classes(units: &Units) -> RunClasses {
    let class = run_partition(units, &["OUT_DIR"]);
    let family = run_partition(units, &["OUT_DIR", RUN_PROFILE_VARS[0], RUN_PROFILE_VARS[1]]);
    // alloc: once a lowering, a flag per unit
    let mut keyed = vec![false; class.len()];
    // The class of each family's first run, and whether another class was seen in it.
    // alloc: once a lowering, an entry per family of runs
    let mut seen: blaze_hash::HashMap<usize, (usize, bool), std::alloc::Global> =
        blaze_hash::HashMap::new_in(std::alloc::Global);
    let runs = || (0..class.len()).filter(|&at| units.graph.units[at].mode == Mode::RunCustomBuild);
    for at in runs() {
        let entry = seen.entry(family[at]).or_insert((class[at], false));
        entry.1 |= entry.0 != class[at];
    }
    for at in runs() {
        keyed[at] = seen.get(&family[at]).is_some_and(|&(_, more)| more);
    }
    RunClasses { class, keyed }
}

/// Each unit's part of the build's runs by everything but the variables `ignored` names: for a
/// run, the first run (by index) of its part; any other unit is its own.
///
/// A run's dependencies count by their own part, so runs of a `links` package's dependents that
/// wait on two runs of one part are one part too: parts only merge from pass to pass, so the
/// passes settle within one per run (the longest chain of runs), and one more to see it.
fn run_partition(units: &Units, ignored: &[&str]) -> Vec<usize> {
    let all = &units.graph.units;
    let mut part: Vec<usize> = (0..all.len()).collect(); // alloc: once a lowering, one per unit
    let runs = all.iter().filter(|it| it.mode == Mode::RunCustomBuild).count();
    if runs < 2 {
        return part;
    }
    for _ in 0..=runs {
        // The first run of each part, once a pass of a lowering.
        let mut first: blaze_hash::HashMap<RunKey<'_>, usize, std::alloc::Global> =
            blaze_hash::HashMap::new_in(std::alloc::Global); // alloc: once a pass of a lowering, a key per run
        let mut moved = false;
        for (index, unit) in all.iter().enumerate() {
            if unit.mode != Mode::RunCustomBuild {
                continue;
            }
            let plan = &units.plans[index];
            let key = RunKey {
                pkg_id: &unit.pkg_id,
                target: &unit.target.name,
                src_path: &unit.target.src_path,
                built_for: unit.built_for(),
                features: &unit.features,
                is_std: unit.is_std,
                deps: unit
                    .dependencies
                    .iter()
                    .map(|dep| {
                        (
                            part.get(dep.index).copied().unwrap_or(dep.index),
                            dep.extern_crate_name.as_str(),
                        )
                    })
                    .collect(), // alloc: as `first`, one per run
                env: plan
                    .env
                    .iter()
                    .filter(|(name, _)| !ignored.contains(&name.as_str()))
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .collect(), // alloc: as `first`, one per run
                cfgs: &plan.cfgs,
                rustflags: &plan.rustflags,
                links: plan.links.as_deref(),
                unsandboxed: plan.unsandboxed,
            };
            let rep = *first.entry(key).or_insert(index);
            if part[index] != rep {
                part[index] = rep;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    part
}

/// The [`RUN_PROFILE`] atom of a crate whose unit depends on run `run`: each of the run's
/// [`RUN_PROFILE_VARS`] as `NAME=value`, or `NAME unset`.
pub(crate) fn run_profile_atom(units: &Units, run: usize) -> CfgAtom {
    let env = units.plans.get(run).map_or(&[][..], |it| it.env.as_slice());
    let value = RUN_PROFILE_VARS
        .iter()
        .map(|name| match env.iter().find(|(it, _)| it == name) {
            Some((_, value)) => format!("{name}={value}"), // alloc: once per crate of such a run, its atom
            None => format!("{name} unset"),               // alloc: as above
        })
        .collect::<Vec<_>>() // alloc: as above
        .join(" ");
    CfgAtom::KeyValue { key: Symbol::intern(RUN_PROFILE), value: Symbol::intern(&value) }
}

/// `unit`'s variables (`plan.env`) as its crate is made with them: an `OUT_DIR` that is one of
/// its runs' named by its run class's ([`run_classes`]), so units apart only in which run of a
/// class they depend on have one crate. A run's own `OUT_DIR` is in its plan.
pub(crate) fn class_env(
    units: &Units,
    unit: &Unit,
    env: &[(String, String)],
    class: &[usize],
) -> Vec<(String, String)> {
    let mut out = env.to_vec(); // alloc: once per unit lowered, its variables, which its crate and lowering keep
    for (name, value) in &mut out {
        if name != "OUT_DIR" {
            continue;
        }
        let run = unit.dependencies.iter().map(|dep| dep.index).find(|&at| {
            units.graph.units.get(at).is_some_and(|it| it.mode == Mode::RunCustomBuild)
                && units.plans.get(at).and_then(|it| it.out_dir.as_deref()) == Some(value.as_str())
        });
        if let Some(canonical) = run.and_then(|at| units.plans[class[at]].out_dir.as_ref()) {
            value.clone_from(canonical);
        }
    }
    out
}

/// `bytes` with each `from` in it written as `to`; borrowed when there is none.
pub(crate) fn replace_bytes<'a>(bytes: &'a [u8], from: &[u8], to: &[u8]) -> Cow<'a, [u8]> {
    let Some(&head) = from.first() else { return Cow::Borrowed(bytes) };
    let found = |at: usize| bytes[at] == head && bytes[at..].starts_with(from);
    let Some(mut at) = (0..bytes.len()).find(|&at| found(at)) else { return Cow::Borrowed(bytes) };
    let mut out = Vec::with_capacity(bytes.len()); // alloc: a run's file that names its `OUT_DIR`, once per apply
    let mut kept = 0;
    loop {
        out.extend_from_slice(&bytes[kept..at]);
        out.extend_from_slice(to);
        kept = at + from.len();
        match (kept..bytes.len()).find(|&at| found(at)) {
            Some(next) => at = next,
            None => break,
        }
    }
    out.extend_from_slice(&bytes[kept..]);
    Cow::Owned(out)
}

/// The standard library's crates are the language's, path packages local, the rest libraries.
pub(crate) fn origin(package: &PackagePlan, unit: &Unit) -> CrateOrigin {
    if unit.is_std {
        return CrateOrigin::Lang(LangCrateOrigin::from(package.name.as_str()));
    }
    match is_path(package) {
        true => CrateOrigin::Local { repo: None, name: Some(Symbol::intern(&package.name)) },
        false => CrateOrigin::Library { repo: None, name: Symbol::intern(&package.name) },
    }
}

/// The `--crate-type`s cargo passes for the unit, by rustc's names. A `--test` crate is an
/// executable whatever its target's types (rustc's `collect_crate_types`).
pub(crate) fn crate_types(
    unit: &Unit,
    plan: &UnitPlan,
    what: &str,
) -> Result<CrateTypes, LowerError> {
    if is_test(unit, plan) {
        return Ok(CrateTypes::of(&[CrateType::Bin]));
    }
    let named = |name: &String| {
        CrateType::ALL.into_iter().find(|it| it.as_str() == name).ok_or_else(|| {
            LowerError::at(
                format!("{what} has the crate type `{name}`, which blaze does not know"),
                what,
            )
        })
    };
    let types = unit.target.crate_types.iter().map(named).collect::<Result<Vec<_>, _>>()?;
    Ok(CrateTypes::of(&types))
}

/// What the crate is: a `--test` crate is a test (a bench's a bench), whatever its target.
pub(crate) fn target_kind(
    unit: &Unit,
    plan: &UnitPlan,
    what: &str,
) -> Result<TargetKind, LowerError> {
    if is_test(unit, plan) {
        let bench = unit.target.kind.iter().any(|it| it == "bench");
        return Ok(if bench { TargetKind::Bench } else { TargetKind::Test });
    }
    manifest_kind(unit, what)
}

/// The kind of the manifest's target the crate is built from, `--test` or not: with the
/// target's name, which of its package's targets it is (cargo's target identity), so an
/// example and an integration test of one name, both built with `--test` under
/// `--all-targets`, are two crates.
pub(crate) fn manifest_kind(unit: &Unit, what: &str) -> Result<TargetKind, LowerError> {
    let target = &unit.target;
    let has = |kind: &str| target.kind.iter().any(|it| it == kind);
    Ok(if target.is_custom_build() {
        TargetKind::BuildScript
    } else if target.is_proc_macro() {
        TargetKind::Lib { is_proc_macro: true }
    } else if target.is_lib() {
        TargetKind::Lib { is_proc_macro: false }
    } else if has("bin") {
        TargetKind::Bin
    } else if has("example") {
        TargetKind::Example
    } else if has("test") {
        TargetKind::Test
    } else if has("bench") {
        TargetKind::Bench
    } else {
        return Err(LowerError::at(format!("{what} is of no kind of target blaze knows"), what));
    })
}

/// Where a proc macro runs for the crate: the workspace's root for a member, its package's
/// directory otherwise, as cargo runs rustc.
pub(crate) fn proc_macro_cwd(root: &Root, package: &PackagePlan) -> String {
    match (&root.dir, package.is_member) {
        (Some(dir), true) => dir.clone(),
        _ => files::parent(&package.manifest_path).to_owned(),
    }
}

/// The workspace's root, as cargo finds it from a member, and what its manifest sets.
#[derive(PartialEq)]
pub(crate) struct Root {
    /// The directory of the root manifest; none for a build of no member.
    pub(crate) dir: Option<String>,
    /// `const-eval` under `[workspace.metadata.blaze]`.
    pub(crate) const_eval: ConstEvalBudget,
}

/// The first manifest with a `[workspace]` that does not exclude the first member, from that
/// member's directory up (cargo's `find_workspace_root`), else the member's own: read through
/// the mounts, stopping where they end.
pub(crate) fn root<T: blaze_query::Tree>(
    units: &Units,
    tree: &files::Mounts<'_, T>,
) -> Result<Root, LowerError> {
    let Some(member) = units.packages.iter().find(|it| it.is_member) else {
        return Ok(Root { dir: None, const_eval: ConstEvalBudget::default() });
    };
    let own = files::parent(&member.manifest_path);
    let mut dir = own;
    loop {
        let manifest = match dir {
            "/" => "/Cargo.toml".to_owned(),
            dir => format!("{dir}/Cargo.toml"),
        };
        if !tree.covers(&manifest) {
            break;
        }
        if let Some(bytes) = tree.read(&manifest)? {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| LowerError::at("the manifest is not UTF-8", manifest.clone()))?;
            let table: toml::Table = text.parse().map_err(|error| {
                LowerError::at(format!("the manifest: {error}"), manifest.clone())
            })?;
            if let Some(workspace) = table.get("workspace") {
                let excluded = workspace
                    .get("exclude")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_str)
                    .any(|it| {
                        files::rest(own, &files::normalize(&format!("{dir}/{it}"))).is_some()
                    });
                if !excluded {
                    let const_eval = const_eval(workspace, &manifest)?;
                    return Ok(Root { dir: Some(dir.to_owned()), const_eval });
                }
            }
        }
        let up = files::parent(dir);
        if up == dir {
            break;
        }
        dir = up;
    }
    Ok(Root { dir: Some(own.to_owned()), const_eval: ConstEvalBudget::default() })
}

/// `const-eval` under `[workspace.metadata.blaze]`: the default for what it leaves out, an
/// error naming the key for anything but a positive integer of a setting it has.
fn const_eval(workspace: &toml::Value, manifest: &str) -> Result<ConstEvalBudget, LowerError> {
    let mut budget = ConstEvalBudget::default();
    let key = ConstEvalBudget::KEY;
    let Some(value) =
        workspace.get("metadata").and_then(|it| it.get("blaze")).and_then(|it| it.get(key))
    else {
        return Ok(budget);
    };
    let Some(settings) = value.as_table() else {
        return Err(LowerError::at(
            format!(
                "`{key}` under `[workspace.metadata.blaze]` is a table: `{key} = {{ steps = N, memory = BYTES }}`"
            ),
            manifest,
        ));
    };
    for (name, value) in settings {
        let slot = match name.as_str() {
            "steps" => &mut budget.steps,
            "memory" => &mut budget.memory,
            _ => {
                return Err(LowerError::at(
                    format!("`{key}.{name}` is not a setting (known: `steps`, `memory`)"),
                    manifest,
                ));
            }
        };
        *slot = value.as_integer().filter(|it| *it > 0).map(|it| it as u64).ok_or_else(|| {
            LowerError::at(format!("`{key}.{name}` is a positive integer, not `{value}`"), manifest)
        })?;
    }
    Ok(budget)
}

/// The lint flags of `unit`'s rustc: cargo's `[lints]`, those of its rustflags, its
/// `--check-cfg`s, and cargo's `--cap-lints` for a package not from a path or the standard
/// library: `allow`, or `warn` under `-vv` (`extra_verbose`), which shows their warnings
/// (cargo's `compute_cap_lints`). The check and the front end's crate read this one list.
pub fn lint_flags(unit: &Unit, plan: &UnitPlan, local: bool, extra_verbose: bool) -> Vec<String> {
    const SHORT: [&str; 4] = ["-A", "-W", "-D", "-F"];
    const LONG: [&str; 6] =
        ["--allow", "--warn", "--deny", "--forbid", "--force-warn", "--cap-lints"];
    let mut out = plan.lints.clone(); // clone: the unit's own list, once per unit per build
    let mut flags = plan.rustflags.iter();
    while let Some(flag) = flags.next() {
        let flag = flag.as_str();
        if SHORT.contains(&flag) || LONG.contains(&flag) {
            out.push(flag.to_owned()); // alloc: `CommandLine::parse` takes `&[String]`; once per flag per unit per build
            if let Some(value) = flags.next() {
                out.push(value.clone()); // clone: as above
            }
        } else if SHORT.iter().any(|it| flag.starts_with(it))
            || LONG.iter().any(|it| flag.strip_prefix(it).is_some_and(|rest| rest.starts_with('=')))
        {
            out.push(flag.to_owned()); // alloc: as above
        }
    }
    for spec in &plan.check_cfg {
        out.push("--check-cfg".to_owned()); // alloc: as above
        out.push(spec.clone()); // clone: as above
    }
    if unit.is_std || !local {
        out.push("--cap-lints".to_owned()); // alloc: as above
        out.push(if extra_verbose { "warn" } else { "allow" }.to_owned()); // alloc: as above
    }
    out
}

/// Whether cargo shows a package's warnings: one from a path (`(/abs/path)`).
pub fn local(source: Option<&str>) -> bool {
    source.is_some_and(|it| {
        let path = it.trim_start_matches('(');
        path.starts_with('/') || path.get(1..3) == Some(":\\") || path.starts_with("\\\\")
    })
}
