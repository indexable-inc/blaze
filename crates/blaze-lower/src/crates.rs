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
    schema::{Mode, Unit, UnitDep},
    stdlib::target::{self, CfgArgs, HostCpu},
};
use cfg::{CfgAtom, CfgOptions};
use intern::Symbol;
use span::Edition;
use triomphe::Arc;

use super::{LowerError, files};

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
    let platform = unit.platform.as_deref().unwrap_or("the host");
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

/// The unit's root file, absolute and `/`-separated, as the mounts name paths.
pub(crate) fn src_path(unit: &Unit, what: &str) -> Result<String, LowerError> {
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

/// blaze's rustc's version (`1.101.0-nightly`), which the front end gates nightly behavior on.
pub(crate) fn toolchain() -> Result<Version, LowerError> {
    let version = target::rustc_version();
    let number = version.split(' ').next().unwrap_or(version);
    Version::parse(number)
        .map_err(|error| LowerError::new(format!("blaze's rustc version `{version}` is not a version: {error}")))
}

/// What every crate built for `triple` shares: its target's data layout (blaze's table), the
/// toolchain and the const-eval budget.
pub(crate) fn workspace_data(
    triple: &str,
    toolchain: &Version,
    const_eval: ConstEvalBudget,
) -> Result<Arc<CrateWorkspaceData>, LowerError> {
    let data_layout = target::data_layout(triple).map_err(|what| LowerError::at(what, triple))?;
    let has_thumb_interworking =
        target::has_thumb_interworking(triple).map_err(|what| LowerError::at(what, triple))?;
    let arch = match triple.split('-').next() {
        Some("wasm32") => Arch::Wasm32,
        Some("wasm64") => Arch::Wasm64,
        _ => Arch::Other,
    };
    Ok(Arc::new(CrateWorkspaceData {
        target: Ok(TargetData {
            tuple: Symbol::intern(triple),
            data_layout: data_layout.into_boxed_str(),
            arch,
            has_thumb_interworking,
        }),
        toolchain: Some(toolchain.clone()),
        const_eval,
    }))
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
    let mut flags: Vec<String> = Vec::with_capacity(10 + 2 * plan.cfgs.len() + plan.rustflags.len());
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
    Ok(options)
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
        return Err(format!("`--cfg {cfg}`: a cfg's value is a string in quotes, as `{name}=\"value\"`"));
    };
    let value = unescape(inner).ok_or_else(|| format!("`--cfg {cfg}`: the value has an escape rustc does not take"))?;
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
pub(crate) fn topological(units: &Units, whats: &[String], after: &[Vec<usize>]) -> Result<Vec<usize>, LowerError> {
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
                    let cycle: Vec<&str> =
                        stack[from..].iter().map(|it| whats[it.0].as_str()).chain([whats[on].as_str()]).collect();
                    return Err(LowerError::at(
                        format!("the units depend on each other in a cycle: {}", cycle.join(" -> ")),
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
        LowerError::at(format!("{what} has edition `{}`, which blaze does not know", unit.target.edition), what)
    })
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
pub(crate) fn crate_types(unit: &Unit, plan: &UnitPlan, what: &str) -> Result<CrateTypes, LowerError> {
    if is_test(unit, plan) {
        return Ok(CrateTypes::of(&[CrateType::Bin]));
    }
    let named = |name: &String| {
        CrateType::ALL.into_iter().find(|it| it.as_str() == name).ok_or_else(|| {
            LowerError::at(format!("{what} has the crate type `{name}`, which blaze does not know"), what)
        })
    };
    let types = unit.target.crate_types.iter().map(named).collect::<Result<Vec<_>, _>>()?;
    Ok(CrateTypes::of(&types))
}

/// What the crate is: a `--test` crate is a test (a bench's a bench), whatever its target.
pub(crate) fn target_kind(unit: &Unit, plan: &UnitPlan, what: &str) -> Result<TargetKind, LowerError> {
    let target = &unit.target;
    let has = |kind: &str| target.kind.iter().any(|it| it == kind);
    if is_test(unit, plan) {
        return Ok(if has("bench") { TargetKind::Bench } else { TargetKind::Test });
    }
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
pub(crate) fn root<T: blaze_query::Tree>(units: &Units, tree: &files::Mounts<'_, T>) -> Result<Root, LowerError> {
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
            let table: toml::Table =
                text.parse().map_err(|error| LowerError::at(format!("the manifest: {error}"), manifest.clone()))?;
            if let Some(workspace) = table.get("workspace") {
                let excluded = workspace
                    .get("exclude")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_str)
                    .any(|it| files::rest(own, &files::normalize(&format!("{dir}/{it}"))).is_some());
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
    let Some(value) = workspace.get("metadata").and_then(|it| it.get("blaze")).and_then(|it| it.get(key)) else {
        return Ok(budget);
    };
    let Some(settings) = value.as_table() else {
        return Err(LowerError::at(
            format!("`{key}` under `[workspace.metadata.blaze]` is a table: `{key} = {{ steps = N, memory = BYTES }}`"),
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
    const LONG: [&str; 6] = ["--allow", "--warn", "--deny", "--forbid", "--force-warn", "--cap-lints"];
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
