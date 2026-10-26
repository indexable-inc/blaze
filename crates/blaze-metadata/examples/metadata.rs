//! `cargo metadata --format-version 1` made by blaze-metadata, for harness/own_metadata.py,
//! which compares it with cargo's.
//!
//! usage: metadata [--manifest-path P] [--filter-platform TRIPLE --cfg-file F] [--features F]
//!                 [--all-features] [--no-default-features] [--offline]
//!                 [--build [-p NAME]... [--workspace] [--exclude NAME]... [--dev-units]
//!                  [--programs [--bin NAME | --example NAME]]]
//!
//! `--cfg-file` is what `rustc --print cfg --target TRIPLE` prints. With `--build`, the graph is
//! that of `cargo build` with the same flags; with `--programs` too, what it prints is the
//! programs that build links instead, a JSON list.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use blaze_metadata::{Asked, Build, TargetPlatform, Targets, metadata, programs};
use blaze_resolve::Cfg;

fn main() -> Result<()> {
    let mut manifest_path = None;
    let mut triple = None;
    let mut cfg_file = None;
    let mut features = Vec::new();
    let (mut all_features, mut no_default_features, mut offline) = (false, false, false);
    let (mut build, mut workspace, mut dev_units) = (false, false, false);
    let (mut packages, mut excludes) = (Vec::new(), Vec::new());
    let (mut listed, mut bin, mut example) = (false, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} takes a value"));
        match arg.as_str() {
            "--manifest-path" => manifest_path = Some(PathBuf::from(value()?)),
            "--filter-platform" => triple = Some(value()?),
            "--cfg-file" => cfg_file = Some(value()?),
            "--features" => features.push(value()?),
            "-p" | "--package" => packages.push(value()?),
            "--exclude" => excludes.push(value()?),
            "--all-features" => all_features = true,
            "--no-default-features" => no_default_features = true,
            "--offline" => offline = true,
            "--build" => build = true,
            "--workspace" => workspace = true,
            "--dev-units" => dev_units = true,
            "--programs" => listed = true,
            "--bin" => bin = Some(value()?),
            "--example" => example = Some(value()?),
            _ => bail!("unknown argument {arg}"),
        }
    }
    let cfg: Vec<Cfg> = match &cfg_file {
        Some(file) => std::fs::read_to_string(file)?
            .lines()
            .filter(|it| !it.is_empty())
            .map(|it| Cfg::from_str(it).with_context(|| format!("cfg {it}")))
            .collect::<Result<_>>()?,
        None => Vec::new(),
    };
    if triple.is_some() != cfg_file.is_some() {
        bail!("--filter-platform and --cfg-file go together");
    }
    let cwd = std::env::current_dir()?;
    let asked = Asked {
        cwd: &cwd,
        env: std::env::vars().collect(),
        manifest_path: manifest_path.as_deref(),
        config: &[],
        features: &features,
        all_features,
        no_default_features,
        build: build.then_some(Build {
            packages: &packages,
            workspace,
            excludes: &excludes,
            dev_units,
        }),
        platform: triple.as_deref().map(|triple| TargetPlatform { triple, cfg: &cfg }),
        offline,
        store: None,
    };
    if listed {
        let targets = Targets { bin: bin.as_deref(), example: example.as_deref() };
        println!("{}", serde_json::to_string(&programs(&asked, targets)?.1)?);
    } else {
        println!("{}", metadata(&asked)?);
    }
    Ok(())
}
