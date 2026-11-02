//! The flags cargo gives rustc: a platform's rustflags (`target_info.rs`'s `TargetInfo::new`
//! and `extra_args`), the `--cfg`s (`features_args`), the `--check-cfg`s (`check_cfg_args`) and
//! `[lints]` (`parser/mod.rs`'s `lints_to_rustflags`), and blaze's own lints of
//! `[workspace.metadata.blaze.lints]`.

use std::cmp::Reverse;
use std::path::Path;

use blaze_lint_defs::Named;
use blaze_manifest::Config;
use blaze_manifest::config::{CompileKind, Flags, PathAndArgs};
use blaze_manifest::files::Files;
use blaze_manifest::manifest::Manifest;
use blaze_resolve::Cfg;
use cargo_util_schemas::manifest::{TomlLintLevel, TomlManifest};

use crate::load::load_error;
use crate::query::LoadError;
use crate::stdlib::target::{self as platform, CfgArgs};

fn error(what: String) -> LoadError {
    LoadError { what, at: None }
}

/// Whether `blaze::{lint}` is a lint or group of the compile's registry, which has blaze-lints'
/// as well as the front end's: a level set for any other name would set nothing.
fn is_blazes(lint: &str) -> bool {
    matches!(blaze_lint_defs::named(&format!("blaze::{lint}")), Named::Lint(_) | Named::Group(_))
}

/// What cargo's `TargetInfo` has of one kind of unit.
pub(super) struct KindInfo {
    /// The triple, the host's for the host kind.
    pub triple: String,
    /// cargo's `TargetInfo::rustflags`: every unit of the kind has them (`Unit::rustflags`).
    pub rustflags: Vec<String>,
    /// `rustc --print cfg` of the kind with them, as lines.
    pub cfg_lines: Vec<String>,
    pub cfgs: Vec<Cfg>,
}

/// cargo's `TargetInfo::new` for `kind` (`None` the host) in a build for `target`: the
/// rustflags of `extra_args`, then once more with the cfg they give for `[target.'cfg(..)']`,
/// up to the second turn as cargo stops.
pub(super) fn kind_info<F: Files>(config: &Config<F>, host: &str, target: Option<&str>, kind: Option<&str>) -> Result<KindInfo, LoadError> {
    let requested = [match target {
        Some(triple) => CompileKind::Target(triple.to_owned()),
        None => CompileKind::Host,
    }];
    let compile_kind = match kind {
        Some(triple) => CompileKind::Target(triple.to_owned()),
        None => CompileKind::Host,
    };
    let triple = kind.unwrap_or(host);
    let extra = |cfg: Option<&[Cfg]>| {
        config.extra_args(&requested, host, cfg, &compile_kind, Flags::Rust).map_err(|e| load_error(&e, None))
    };
    let mut rustflags = extra(None)?;
    let mut turn = 0;
    loop {
        let args = CfgArgs::from_flags(&rustflags).map_err(error)?;
        let cfg_lines = platform::target_cfg(triple, &CfgArgs { proc_macro: true, ..args }).map_err(error)?;
        let cfgs = cfg_lines
            .iter()
            .map(|line| line.parse::<Cfg>().map_err(|e| error(format!("the cfg `{line}` of `{triple}`: {e}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let next = extra(Some(&cfgs))?;
        if next != rustflags && turn == 0 {
            turn += 1;
            rustflags = next;
            continue;
        }
        return Ok(KindInfo { triple: triple.to_owned(), rustflags, cfg_lines, cfgs });
    }
}

/// What `cargo doc` would read that `blaze doc` does not honor yet, refused naming it: rustdoc's
/// flags of each kind (`extra_args` with `Flags::Rustdoc`; blaze-doc makes the pages, and no
/// rustdoc runs), and under `--open` the browser of `doc.browser` (`CARGO_DOC_BROWSER`), which
/// cargo takes over `$BROWSER`.
pub(super) fn doc_unhonored<F: Files>(
    config: &Config<F>,
    host: &str,
    target: Option<&str>,
    kinds: &[KindInfo],
    open: bool,
) -> Result<(), LoadError> {
    let requested = [match target {
        Some(triple) => CompileKind::Target(triple.to_owned()),
        None => CompileKind::Host,
    }];
    for (at, info) in kinds.iter().enumerate() {
        let kind = match at {
            0 => CompileKind::Host,
            _ => CompileKind::Target(info.triple.clone()),
        };
        let rustdocflags = config
            .extra_args(&requested, host, Some(&info.cfgs), &kind, Flags::Rustdoc)
            .map_err(|e| load_error(&e, None))?;
        if !rustdocflags.is_empty() {
            return Err(error(format!(
                "blaze doc makes the pages itself and takes none of rustdoc's flags yet: `{}` \
                 (`RUSTDOCFLAGS`, `build.rustdocflags` or `target.{}.rustdocflags`)",
                rustdocflags.join(" "),
                info.triple
            )));
        }
    }
    if open {
        let browser: Option<PathAndArgs> = config.get("doc.browser").map_err(|e| load_error(&e, None))?;
        if browser.is_some() {
            return Err(error(
                "blaze doc --open opens the pages with `$BROWSER` or the system's opener, and does \
                 not read `doc.browser` (`CARGO_DOC_BROWSER`) yet"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

/// Each `--cfg` of `flags`, as `--cfg x` or `--cfg=x`.
fn push_cfgs(flags: &[String], into: &mut Vec<String>) {
    let mut at = 0;
    while at < flags.len() {
        let flag = &flags[at];
        if flag == "--cfg" {
            if let Some(value) = flags.get(at + 1) {
                into.push(value.clone());
            }
            at += 2;
            continue;
        }
        if let Some(value) = flag.strip_prefix("--cfg=") {
            into.push(value.to_owned());
        }
        at += 1;
    }
}

/// The `--cfg`s of a compile in cargo's order: the profile's rustflags' (`build_base_args`),
/// the features (`features_args`), the kind's rustflags' (`prepare_rustc`).
pub(super) fn cfgs(profile_rustflags: &[String], features: &[String], rustflags: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(features.len());
    push_cfgs(profile_rustflags, &mut out);
    out.extend(features.iter().map(|feature| format!("feature=\"{feature}\"")));
    push_cfgs(rustflags, &mut out);
    out
}

/// A unit's rustflags in cargo's order: its profile's `rustflags` (`-Z profile-rustflags`),
/// then the kind's `rest`. A profile `-C panic` that says the profile's `panic`
/// (library/Cargo.toml's `panic_abort`, built to abort) is the profile's own flag and goes; one
/// that says another strategy is refused, naming `package`, since blaze compiles with the
/// profile's `panic` and rustc's last `-C panic` would win.
pub(super) fn unit_rustflags(flags: &[String], panic: &str, package: &str, rest: &[String]) -> Result<Vec<String>, LoadError> {
    // alloc: per unit, its flags (`UnitPlan` owns them)
    let mut out = Vec::with_capacity(flags.len() + rest.len());
    let mut at = 0;
    while at < flags.len() {
        let flag = flags[at].as_str();
        let (option, words) = match flag {
            "-C" | "--codegen" => (flags.get(at + 1).map(String::as_str), 2),
            _ => (flag.strip_prefix("--codegen=").or_else(|| flag.strip_prefix("-C")), 1),
        };
        match option.and_then(|it| it.strip_prefix("panic=")) {
            Some(strategy) if strategy == panic => {}
            Some(strategy) => {
                return Err(error(format!(
                    "`{package}`'s profile rustflags say `-C panic={strategy}` and its profile \
                     `panic = \"{panic}\"`: blaze compiles with the profile's `panic`; set it in \
                     `[profile.*]` instead"
                )));
            }
            // clone: each plan owns its flags
            None => out.extend(flags[at..flags.len().min(at + words)].iter().cloned()),
        }
        at += words;
    }
    // clone: each plan owns its flags
    out.extend(rest.iter().cloned());
    Ok(out)
}

/// cargo's `check_cfg_args`: what follows each `--check-cfg`, the package's features (its
/// summary's, the implicit ones of optional dependencies too) sorted by name.
pub(super) fn check_cfg(manifest: &Manifest) -> Vec<String> {
    check_cfg_of(manifest.features.keys().map(|it| it.as_str()))
}

pub(super) fn check_cfg_of<'a>(features: impl ExactSizeIterator<Item = &'a str>) -> Vec<String> {
    let mut feature = String::with_capacity(features.len() * 7 + 25);
    feature.push_str("cfg(feature, values(");
    for (at, name) in features.enumerate() {
        if at != 0 {
            feature.push_str(", ");
        }
        feature.push('"');
        feature.push_str(name);
        feature.push('"');
    }
    feature.push_str("))");
    vec!["cfg(docsrs,test)".to_owned(), feature]
}

/// The table of the workspace's root manifest that sets the levels of blaze's own lints for
/// every member. cargo hands `[lints]` to rustc, which stops at a tool it does not know
/// (E0602), so a `blaze` table there breaks every cargo build; cargo hands rustc no metadata.
pub(crate) const WORKSPACE_LINTS: &str = "[workspace.metadata.blaze.lints]";

/// The level flags of [`WORKSPACE_LINTS`] in `metadata`, the root manifest's
/// `[workspace.metadata]` (at `manifest`): `hot_alloc = "warn"` is `--warn=blaze::hot_alloc`,
/// by name. A level that is not `allow`, `warn`, `deny` or `forbid`, a name with a path, or one
/// that is no lint or group of blaze's, is an error.
pub(super) fn workspace_lints(metadata: Option<&toml::Value>, manifest: &Path) -> Result<Vec<String>, LoadError> {
    let Some(table) = metadata.and_then(|it| it.get("blaze")).and_then(|it| it.get("lints")) else {
        return Ok(Vec::new());
    };
    let wrong = |what: String| LoadError {
        what: format!("{WORKSPACE_LINTS} in {}: {what}", manifest.display()),
        at: Some(manifest.display().to_string()),
    };
    let table = table.as_table().ok_or_else(|| wrong("is a table of lint names to levels".to_owned()))?;
    let mut flags = Vec::with_capacity(table.len());
    for (lint, level) in table {
        if lint.contains(':') {
            return Err(wrong(format!("`{lint}` is named without its tool: these are blaze's own lints")));
        }
        if !is_blazes(lint) {
            return Err(wrong(format!("`{lint}` is no lint or group of blaze's (`blaze::{lint}`)")));
        }
        let flag = match level.as_str() {
            Some("forbid") => "--forbid",
            Some("deny") => "--deny",
            Some("warn") => "--warn",
            Some("allow") => "--allow",
            _ => return Err(wrong(format!("the level of `{lint}` is `allow`, `warn`, `deny` or `forbid`"))),
        };
        flags.push(format!("{flag}=blaze::{lint}"));
    }
    flags.sort_by(|a, b| a.split_once('=').map(|it| it.1).cmp(&b.split_once('=').map(|it| it.1)));
    Ok(flags)
}

/// cargo's `lints_to_rustflags` of the package's `[lints]` (inherited already): the level
/// flags sorted by priority, then by name backwards, then by flag; and what follows each
/// `--check-cfg` of `[lints.rust.unexpected_cfgs] check-cfg`.
pub(super) fn lints(manifest: &Manifest) -> Result<(Vec<String>, Vec<String>), LoadError> {
    lints_of(&manifest.package_id.name, &manifest.normalized_toml)
}

pub(super) fn lints_of(name: &str, toml: &TomlManifest) -> Result<(Vec<String>, Vec<String>), LoadError> {
    let lints = toml
        .normalized_lints()
        .map_err(|_| error(format!("the `[lints]` of `{name}` were not inherited from the workspace")))?;
    let Some(lints) = lints else { return Ok((Vec::new(), Vec::new())) };
    let mut flags: Vec<(i8, Reverse<&str>, String)> = Vec::new();
    for (tool, tool_lints) in lints {
        if tool == "cargo" {
            continue;
        }
        for (lint, config) in tool_lints {
            if tool == blaze_lint_defs::BLAZE_TOOL && !is_blazes(lint) {
                return Err(error(format!(
                    "the `[lints.blaze]` of `{name}` set `{lint}`, which is no lint or group of blaze's (`blaze::{lint}`)"
                )));
            }
            let flag = match config.level() {
                TomlLintLevel::Forbid => "--forbid",
                TomlLintLevel::Deny => "--deny",
                TomlLintLevel::Warn => "--warn",
                TomlLintLevel::Allow => "--allow",
            };
            let option = if tool == "rust" { format!("{flag}={lint}") } else { format!("{flag}={tool}::{lint}") };
            flags.push((config.priority(), Reverse(lint.as_str()), option));
        }
    }
    flags.sort();
    let flags = flags.into_iter().map(|(_, _, option)| option).collect();
    let mut check_cfg = Vec::new();
    let custom = lints
        .get("rust")
        .and_then(|rust| rust.get("unexpected_cfgs"))
        .and_then(|lint| lint.config())
        .and_then(|config| config.get("check-cfg"));
    if let Some(value) = custom {
        let list = toml::Value::try_into::<Vec<String>>(value.clone())
            .map_err(|_| error("`lints.rust.unexpected_cfgs.check-cfg` must be a list of string".to_owned()))?;
        check_cfg.extend(list);
    }
    Ok((flags, check_cfg))
}
