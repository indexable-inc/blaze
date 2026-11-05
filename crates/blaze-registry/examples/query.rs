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
    task::{Context as TaskContext, Poll, Wake, Waker},
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
        queries.into_iter().try_for_each(block_on)?;
    } else {
        join(queries.collect()).into_iter().collect::<anyhow::Result<()>>()?;
    }
    let versions: usize = offered.iter().sum();
    println!(
        "{} packages, {versions} versions, {:.2}s",
        names.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future + Unpin>(future: F) -> F::Output {
    join(vec![future]).pop().unwrap()
}

/// Each of `futures` run to its end on this thread, as blaze-resolve's resolver runs its
/// queries.
fn join<F: Future + Unpin>(mut futures: Vec<F>) -> Vec<F::Output> {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = TaskContext::from_waker(&waker);
    let mut done: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    let mut left = futures.len();
    while left > 0 {
        for (future, out) in futures.iter_mut().zip(&mut done) {
            if out.is_none()
                && let Poll::Ready(it) = Pin::new(future).poll(&mut cx)
            {
                *out = Some(it);
                left -= 1;
            }
        }
        if left > 0 {
            std::thread::park();
        }
    }
    done.into_iter().map(|it| it.expect("every future ended")).collect()
}
