//! Asks blaze-resolve's `Registry` over crates.io's index for every registry package of a
//! `Cargo.lock`, all at once as the resolver asks or `--one-at-a-time`, into a `CARGO_HOME`,
//! and prints how many versions it was offered and how long it took.
//!
//!     cargo run -p blaze-registry --example query -- <Cargo.lock> <CARGO_HOME> [--one-at-a-time]

#![feature(allocator_api)]

use std::{alloc::Global, path::PathBuf, sync::Arc, time::Instant};

use anyhow::{Context as _, bail};
use blaze_registry::{Context, IndexRegistry, SourceConfig, Sources, http::HyperHttp};
use blaze_resolve::{Dependency, MemRegistry, QueryKind, SourceId};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(lock), Some(home)) = (args.next(), args.next()) else {
        bail!("usage: query <Cargo.lock> <CARGO_HOME> [--one-at-a-time]");
    };
    let one_at_a_time = args.next().as_deref() == Some("--one-at-a-time");
    let context = Context::new(PathBuf::from(home), HyperHttp::shared()?);
    let sources = Sources::new(&SourceConfig::default(), Arc::new(context))?;
    let registry = IndexRegistry::new(&sources, MemRegistry::default());

    let text = std::fs::read_to_string(&lock).with_context(|| format!("reading {lock}"))?;
    let lock: toml::Table = toml::from_str(&text)?;
    let mut names: Vec<&str> = lock
        .get("package")
        .and_then(|it| it.as_array())
        .into_iter()
        .flatten()
        .filter(|it| {
            it.get("source")
                .and_then(|it| it.as_str())
                .is_some_and(|it| it.starts_with("registry+"))
        })
        .filter_map(|it| it.get("name").and_then(|it| it.as_str()))
        .collect();
    names.sort_unstable();
    names.dedup();
    let deps = names
        .iter()
        .map(|name| Dependency::parse(*name, None, SourceId::crates_io()))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let registry = &registry;
    let started = Instant::now();
    // Each ask answered as a resolve's step loop answers it: read now, blocking its thread,
    // `AT_ONCE` threads at a time unless `--one-at-a-time`.
    let at_once = if one_at_a_time { 1 } else { AT_ONCE };
    let chunk = deps.len().div_ceil(at_once).max(1);
    let offered = std::thread::scope(|scope| -> anyhow::Result<usize> {
        let workers: Vec<_> = deps
            .chunks(chunk)
            .map(|deps| {
                scope.spawn(move || -> anyhow::Result<usize> {
                    let mut count = 0;
                    for dep in deps {
                        count += registry.fetch_summaries(dep, QueryKind::Exact, Global)?.len();
                    }
                    Ok(count)
                })
            })
            .collect();
        let mut versions = 0;
        for worker in workers {
            versions += worker.join().map_err(|_| anyhow::anyhow!("a query's thread panicked"))??;
        }
        Ok(versions)
    })?;
    println!(
        "{} packages, {offered} versions, {:.2}s",
        names.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// How many asks are read at once: about as many as the HTTP client keeps in flight.
const AT_ONCE: usize = 16;
