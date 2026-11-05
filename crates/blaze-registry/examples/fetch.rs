//! Fetches every registry package of a `Cargo.lock` into a `CARGO_HOME`, as `cargo fetch` would
//! with that lock file, for `harness/fetchdiff.py` to compare with cargo's.
//!
//!     cargo run -p blaze-registry --example fetch -- <Cargo.lock> <CARGO_HOME>
//!         [--config <config.toml>]... [--store <dir>] [--offline] [--roots]
//!
//! `--config` files are read for `[source]` and `[registries]` in the order given, a later one
//! over an earlier one, which is a sketch of what blaze-manifest reads. A registry that requires
//! authentication gets its token from the configuration cargo would read from the lock file's
//! directory with that `CARGO_HOME` and the `--config` files. `--roots` prints where each
//! package is.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, bail};
use blaze_registry::{
    Context, Locked, Package, SourceConfig, SourceDef, Sources,
    auth::{Auth, Stderr},
    download::From,
    http::HyperHttp,
};
use blaze_source::SourceId;
use semver::Version;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(lock), Some(home)) = (args.next(), args.next()) else {
        bail!(
            "usage: fetch <Cargo.lock> <CARGO_HOME> [--config <file>]... [--store <dir>] [--offline] [--roots]"
        );
    };
    let mut context = Context::new(PathBuf::from(&home), HyperHttp::shared()?);
    let mut config = SourceConfig::default();
    let mut files = Vec::new();
    let mut print_roots = false;
    let mut driven: Option<&'static mut blaze_cas::Driven> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--offline" => context.net.offline = true,
            "--roots" => print_roots = true,
            "--config" => {
                let file = args.next().context("--config needs a file")?;
                read_config(Path::new(&file), &mut config)?;
                files.push(file);
            }
            "--store" => {
                let dir = PathBuf::from(args.next().context("--store needs a directory")?);
                let opened = blaze_cas::Driven::open(&dir).map_err(anyhow::Error::msg)?;
                let opened = Box::leak(Box::new(opened));
                context.store = Some(Box::leak(Box::new(opened.handle())));
                driven = Some(opened);
            }
            other => bail!("unknown argument `{other}`"),
        }
    }
    let cwd = std::path::absolute(&lock)?.parent().context("a lock file's directory")?.to_owned();
    let env = std::env::vars().filter(|(k, _)| k != "CARGO_HOME");
    let env = env.chain([("CARGO_HOME".to_owned(), home.clone())]);
    let settings = blaze_manifest::Config::load(&cwd, env, &files)?;
    context.auth = Some(Arc::new(Auth::new(settings, Box::new(Stderr))));
    let sources = Sources::new(&config, Arc::new(context))?;
    let text = std::fs::read_to_string(&lock).with_context(|| format!("reading {lock}"))?;
    let lock: toml::Table = toml::from_str(&text)?;
    let metadata = lock.get("metadata").and_then(|it| it.as_table());
    let mut locked = Vec::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for package in lock.get("package").and_then(|it| it.as_array()).into_iter().flatten() {
        let field = |name: &str| package.get(name).and_then(|it| it.as_str());
        let (Some(name), Some(version)) = (field("name"), field("version")) else { continue };
        let Some(source) = field("source") else { continue };
        let v1 = format!("checksum {name} {version} ({source})");
        let checksum = field("checksum")
            .or_else(|| metadata.and_then(|it| it.get(&v1)).and_then(|it| it.as_str()));
        let source = SourceId::from_url(source)?;
        let mut package = Package {
            name: name.to_owned(),
            version: Version::parse(version)?,
            checksum: checksum.unwrap_or_default().to_owned(),
        };
        let from = sources.map().replacement(source)?;
        // A lock file of cargo before 1.53 may name no checksum: the index's.
        if checksum.is_none() && from.is_remote_registry() {
            package.checksum = sources.registry(from)?.checksum(&package.name, &package.version)?;
        }
        locked.push(Locked { source, package });
    }
    let fetched = sources.fetch(&locked)?;
    for (_, from) in &fetched {
        let from = match from {
            From::Unpacked => "unpacked",
            From::Store => "store",
            From::CargoCache => "cargo-cache",
            From::Network => "network",
            From::Vendored => "vendored",
            From::Git => "git",
        };
        *counts.entry(from).or_default() += 1;
    }
    if print_roots {
        for (it, (root, _)) in locked.iter().zip(&fetched) {
            println!("{} {}", it.package, root.display());
        }
    }
    println!("{}", serde_json::to_string(&counts)?);
    if let Some(driven) = driven {
        driven.flush().map_err(anyhow::Error::msg).context("the store")?;
    }
    Ok(())
}

/// `[source]` and `[registries]` of a cargo configuration file, its paths relative to the
/// directory holding the `.cargo` directory it is in.
fn read_config(file: &Path, config: &mut SourceConfig) -> anyhow::Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    let base = file.parent().and_then(Path::parent).unwrap_or(Path::new("/"));
    let string = |table: &toml::Table, key: &str| {
        table.get(key).and_then(|it| it.as_str()).map(str::to_owned)
    };
    if let Some(sources) = table.get("source").and_then(|it| it.as_table()) {
        for (name, def) in sources {
            let def = def.as_table().with_context(|| format!("source.{name} is a table"))?;
            let def = SourceDef {
                replace_with: string(def, "replace-with"),
                directory: string(def, "directory").map(|it| base.join(it)),
                registry: string(def, "registry"),
                local_registry: string(def, "local-registry").map(|it| base.join(it)),
                git: string(def, "git"),
                branch: string(def, "branch"),
                tag: string(def, "tag"),
                rev: string(def, "rev"),
                definition: file.display().to_string(),
            };
            config.sources.retain(|(it, _)| it != name);
            config.sources.push((name.clone(), def));
        }
    }
    if let Some(registries) = table.get("registries").and_then(|it| it.as_table()) {
        for (name, def) in registries {
            let Some(def) = def.as_table() else { continue };
            if let Some(index) = string(def, "index") {
                config.registries.insert(name.clone(), index);
            }
            if name == "crates-io" {
                config.crates_io_git = string(def, "protocol").as_deref() == Some("git");
            }
        }
    }
    Ok(())
}
