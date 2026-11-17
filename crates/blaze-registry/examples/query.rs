//! Asks blaze-resolve's `Registry` over crates.io's index for every registry package of a
//! `Cargo.lock`, all at once as the resolver asks or `--one-at-a-time`, into a `CARGO_HOME`,
//! and prints how many versions it was offered and how long it took.
//!
//!     cargo run -p blaze-registry --example query -- <Cargo.lock> <CARGO_HOME> [--one-at-a-time]

use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::Instant,
};

use anyhow::{Context as _, bail};
use blaze_registry::{Context, IndexRegistry, SourceConfig, Sources, http::HyperHttp};
use blaze_resolve::{Dependency, MemRegistry, QueryKind, Registry, SourceId};

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
    let mut offered = vec![0; deps.len()];
    let queries = deps.iter().zip(&mut offered).map(|(dep, count)| {
        Box::pin(async move { registry.query(dep, QueryKind::Exact, &mut |_| *count += 1).await })
    });
    if one_at_a_time {
        for query in queries {
            block_on(query)??;
        }
    } else {
        block_on(join(queries.collect()))?.into_iter().collect::<anyhow::Result<()>>()?;
    }
    let versions: usize = offered.iter().sum();
    println!(
        "{} packages, {versions} versions, {:.2}s",
        names.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// `future` to its end on this thread through blaze-core's one `block_on`, with no bound of
/// its own: every request ends by the HTTP client's deadlines, as the wait's name says.
fn block_on<F: Future>(future: F) -> anyhow::Result<F::Output> {
    let what = "crates.io's index files (bounded by the HTTP client's timeouts)";
    Ok(blaze_core::wait::block_on(what, None, future)?)
}

/// Each of `futures` polled together to its end, as blaze-resolve's resolver runs its queries.
async fn join<F: Future + Unpin>(mut futures: Vec<F>) -> Vec<F::Output> {
    let mut done: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut left = 0;
        for (future, out) in futures.iter_mut().zip(&mut done) {
            if out.is_none() {
                match Pin::new(future).poll(cx) {
                    Poll::Ready(it) => *out = Some(it),
                    Poll::Pending => left += 1,
                }
            }
        }
        if left == 0 { Poll::Ready(()) } else { Poll::Pending }
    })
    .await;
    done.into_iter().map(|it| it.expect("every future ended")).collect()
}
