//! What cargo hands rustc to learn of a target, and `cargo rustc --print`, as the pinned
//! nightly's cargo does them (`ops/cargo_compile/mod.rs` `print`, `compile_kind.rs`
//! `from_requested_targets`, `build_context/target_info.rs` `TargetInfo::new`), with rustc's
//! answers the caller's: this module runs nothing.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context as _, bail};
use cargo_platform::Cfg;

use super::Config;
use crate::files::Files;
use super::target::{CompileKind, Flags};

impl<F: Files> Config<F> {
    /// The rustc cargo runs, as `GlobalContext::get_tool`: `RUSTC` (against the current
    /// directory when it has a separator), else `build.rustc`. `None` is cargo's default,
    /// the toolchain's own rustc.
    pub fn rustc_program(&self) -> anyhow::Result<Option<PathBuf>> {
        if let Some(path) = self.env("RUSTC") {
            let relative = path.contains('/') || path.contains('\\');
            return Ok(Some(if relative { self.cwd().join(path) } else { PathBuf::from(path) }));
        }
        Ok(self.build_config()?.rustc.as_ref().map(|it| it.resolve_program(self.cwd())))
    }

    /// The kinds a command with `--target` `targets` builds for, as cargo's
    /// `CompileKind::from_requested_targets`: those, else `build.target`, else the host; each
    /// once, in order of their names.
    pub fn requested_kinds(&self, targets: &[String]) -> anyhow::Result<Vec<CompileKind>> {
        let dedup = |targets: &[String]| -> anyhow::Result<Vec<CompileKind>> {
            let mut kinds = BTreeSet::new();
            for target in targets {
                if target.is_empty() {
                    bail!("target was empty");
                }
                if target.ends_with(".json") {
                    bail!("blaze does not read a target spec file yet: `{target}`");
                }
                kinds.insert(target.clone());
            }
            Ok(kinds.into_iter().map(CompileKind::Target).collect())
        };
        if !targets.is_empty() {
            return dedup(targets);
        }
        match &self.build_config()?.target {
            None => Ok(vec![CompileKind::Host]),
            Some(target) => dedup(&target.values(self.cwd())),
        }
    }

    /// The flags every rustc of a unit of `kind` gets in a build for `requested`, and the
    /// target's `cfg` under them, as cargo's `TargetInfo::new` finds them: `rustflags` without
    /// the `cfg` tables first, then again with the `cfg` rustc prints under those, at most
    /// twice, warning when they still differ. `cfg` is rustc's answer for those flags. `rustc`
    /// answers the arguments given as the rustc cargo runs would, `Err` when it fails.
    pub fn target_rustflags(
        &self,
        requested: &[CompileKind],
        host: &str,
        kind: &CompileKind,
        rustc: &mut impl FnMut(&[String]) -> anyhow::Result<String>,
    ) -> anyhow::Result<(Vec<String>, Vec<Cfg>)> {
        let mut rustflags = self.extra_args(requested, host, None, kind, Flags::Rust)?;
        let mut turn = 0;
        loop {
            let output = rustc(&cfg_args(&rustflags, kind))?;
            let cfg = parse_cfg(&output)?;
            let new_flags = self.extra_args(requested, host, Some(&cfg), kind, Flags::Rust)?;
            let fixed = new_flags == rustflags;
            if !fixed && turn == 0 {
                turn += 1;
                rustflags = new_flags;
                continue;
            }
            if !fixed {
                self.warn(
                    "non-trivial mutual dependency between target-specific configuration and \
                     RUSTFLAGS"
                        .to_owned(),
                );
            }
            return Ok((rustflags, cfg));
        }
    }

    /// What `cargo rustc --print <what>` with `--target` `targets` and `-- <rustc_args>` prints:
    /// for each kind asked for, rustc's answer to that kind's flags, `rustc_args`, the target
    /// and `--print <what>`, an empty line between kinds.
    pub fn rustc_print(
        &self,
        targets: &[String],
        what: &str,
        rustc_args: &[String],
        rustc: &mut impl FnMut(&[String]) -> anyhow::Result<String>,
    ) -> anyhow::Result<String> {
        let verbose = rustc(&["-vV".to_owned()])?;
        let host = verbose
            .lines()
            .find_map(|it| it.strip_prefix("host: "))
            .with_context(|| {
                format!("`rustc -vV` didn't have a line for `host:`, got:\n{verbose}")
            })?
            .to_owned();
        let requested = self.requested_kinds(targets)?;
        let mut out = String::new();
        for (index, kind) in requested.iter().enumerate() {
            if index != 0 {
                out.push('\n');
            }
            let (mut args, _) = self.target_rustflags(&requested, &host, kind, rustc)?;
            args.extend(rustc_args.iter().cloned());
            add_target_arg(kind, &mut args);
            args.extend(["--print".to_owned(), what.to_owned()]);
            out += &rustc(&args)?;
        }
        Ok(out)
    }
}

/// `--target` for a target, nothing for the host, as `CompileKind::add_target_arg`.
pub fn add_target_arg(kind: &CompileKind, args: &mut Vec<String>) {
    if let CompileKind::Target(target) = kind {
        args.extend(["--target".to_owned(), target.clone()]);
    }
}

/// The question whose `cfg` cargo's probe of a target reads: rustc with `rustflags` and the
/// target. cargo asks for more at once (file names, sysroot, split debuginfo), whose answers
/// do not change the `cfg`.
pub fn cfg_args(rustflags: &[String], kind: &CompileKind) -> Vec<String> {
    let mut args = rustflags.to_vec();
    add_target_arg(kind, &mut args);
    args.extend(["--print".to_owned(), "cfg".to_owned()]);
    args
}

/// The `cfg` of rustc's `--print cfg`, as cargo keeps it: without `proc_macro`, which a probe
/// that names the proc-macro crate type has.
pub fn parse_cfg(output: &str) -> anyhow::Result<Vec<Cfg>> {
    let cfg = output
        .lines()
        .map(Cfg::from_str)
        .filter(|it| !matches!(it, Ok(Cfg::Name(name)) if name.as_str() == "proc_macro"));
    cfg.collect::<Result<Vec<_>, _>>().with_context(|| {
        format!("failed to parse the cfg from `rustc --print=cfg`, got:\n{output}")
    })
}
