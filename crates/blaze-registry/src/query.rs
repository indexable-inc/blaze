//! blaze-resolve's [`Registry`] over the registries' indexes, as cargo 1.99 answers the
//! resolver: each line of an index file made a [`blaze_resolve::Summary`] by blaze-units'
//! [`index_summary`] (cargo's `index_package_to_summary` and `registry_dependency_into_dep` in
//! `src/sources/registry/index/mod.rs`), the versions a
//! dependency is offered as `RegistrySource::query` and `RegistryIndex::query_inner` offer them
//! (`src/sources/registry/mod.rs`), behind `[source]` replacement as `SourceConfigMap` and
//! `ReplacedSource` put it (`src/sources/{config.rs,replaced.rs}`). Sources that are not
//! registries (paths, git checkouts, vendored directories) are answered by another registry.

use std::alloc::Allocator;
use std::fmt;

use anyhow::Result;
use blaze_resolve::{
    Dependency, IndexSummary, Lookup, OptVersionReq, QueryKind, Registry as Resolver,
    ResolveError, SourceId,
};

use crate::{
    Registry, Sources,
    index::{IndexEntry, IndexLine},
};

pub use blaze_units::index::index_summary;

/// How a query reads the index files it needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reads {
    /// Only those this update has read already: one not read is [`Lookup::Unknown`], so the
    /// resolver's step never waits for the network ([`IndexRegistry`]'s `Registry`).
    Held,
    /// Each read now, blocking this thread ([`IndexRegistry::fetch_summaries`]).
    Blocking,
}

/// A source the resolver asked about that answers neither from an index nor from what is not
/// a registry: `others` said it does not know, which a fetch cannot change.
#[derive(Debug)]
pub struct NoAnswer {
    pub dep: String,
    pub source: String,
}

impl fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` from {}: not a registry's index, and what answers for other sources does not know it",
            self.dep, self.source
        )
    }
}

impl std::error::Error for NoAnswer {}

impl Registry {
    /// `name`'s versions as `reads` says: `None` when held reads only and this update has not
    /// read the file.
    fn index_file(&self, name: &str, reads: Reads) -> Result<Option<std::sync::Arc<crate::index::Summaries>>> {
        match reads {
            Reads::Held => Ok(self.held(name)),
            Reads::Blocking => self.summaries(name).map(Some),
        }
    }

    /// cargo's `RegistryIndex::query_inner`: each version of `name` that `req` matches, with
    /// `source` as its source. Offline, only those whose `.crate` is on disk, when there are
    /// any; when there are none, all of them, so that a dependency nothing builds still
    /// resolves and one that is built fails to download.
    fn query_index(
        &self,
        name: &str,
        req: &OptVersionReq,
        source: SourceId,
        reads: Reads,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<Lookup> {
        let Some(summaries) = self.index_file(name, reads)? else { return Ok(Lookup::Unknown) };
        let lines: Vec<IndexLine> = summaries.select(|version| req.matches(version)).collect();
        if self.context().net.offline {
            let mut called = false;
            for line in &lines {
                let IndexEntry { name, version, checksum, .. } = &line.entry;
                if self.has_crate(name, version, checksum)
                    && let Some(summary) = index_summary(line, source)
                {
                    called = true;
                    f(summary);
                }
            }
            if called {
                return Ok(Lookup::Answered(()));
            }
        }
        lines.iter().filter_map(|line| index_summary(line, source)).for_each(f);
        Ok(Lookup::Answered(()))
    }

    /// cargo's `RegistrySource::query`: the versions of the registry's package `dep` asks
    /// for, as `kind` matches them, with `source` (the source `dep` names, which this registry
    /// may replace) as their source; [`Lookup::Unknown`] as soon as a file it needs is not
    /// held (with [`Reads::Held`]), what `f` was given by then to be thrown away.
    fn query_dependency(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        source: SourceId,
        reads: Reads,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<Lookup> {
        let name = dep.package_name();
        let mut req = dep.version_req().clone();
        if let Some((_, to)) =
            source.precise_registry_version(&name).filter(|(from, _)| req.matches(from))
        {
            req.precise_to(to);
        }
        let mut called = false;
        // A dependency of the lock file is looked for in the index as it is on disk first,
        // and the index is updated only when it is not there.
        if kind == QueryKind::Exact && req.is_locked() && !self.is_updated() {
            let read = self.query_index(&name, &req, source, reads, &mut |s| {
                if matches!(s, IndexSummary::Candidate(_) | IndexSummary::Yanked(_))
                    && dep.matches(s.summary())
                {
                    called = true;
                    f(s);
                }
            })?;
            if read == Lookup::Unknown {
                return Ok(Lookup::Unknown);
            }
            if called {
                return Ok(Lookup::Answered(()));
            }
            self.invalidate();
        }
        let read = self.query_index(&name, &req, source, reads, &mut |s| {
            let matched = match kind {
                QueryKind::Exact | QueryKind::RejectedVersions => dep.matches(s.summary()),
                QueryKind::AlternativeNames | QueryKind::Normalized => true,
            };
            if !matched {
                return;
            }
            match s {
                s if kind == QueryKind::RejectedVersions => {
                    called = true;
                    f(s);
                }
                s @ (IndexSummary::Candidate(_) | IndexSummary::Yanked(_)) => {
                    called = true;
                    f(s);
                }
                IndexSummary::Unsupported(..)
                | IndexSummary::Invalid(_)
                | IndexSummary::Offline(_) => {}
            }
        })?;
        if read == Lookup::Unknown
            || called
            || !matches!(kind, QueryKind::AlternativeNames | QueryKind::Normalized)
        {
            return Ok(read);
        }
        // A name spelled with `-` for `_` or `_` for `-`, which the resolver turns into "a
        // similar crate exists".
        for other in [name.replace('-', "_"), name.replace('_', "-")] {
            if other == *name {
                continue;
            }
            let read = self.query_index(&other, &req, source, reads, &mut |s| {
                if !s.is_yanked() || kind == QueryKind::AlternativeNames {
                    f(s);
                }
            })?;
            if read == Lookup::Unknown {
                return Ok(Lookup::Unknown);
            }
        }
        Ok(Lookup::Answered(()))
    }
}

/// blaze-resolve's [`Registry`](Resolver) over the registries of `sources`, each after its
/// `[source]` replacement, and over `others` for what is not a registry or is replaced by a
/// vendored directory: cargo's `PackageRegistry` over its `SourceMap`, without `[patch]` and
/// the lock file's locks, which blaze-resolve's `PackageRegistry` puts around it.
pub struct IndexRegistry<'a, R> {
    sources: &'a Sources,
    others: R,
}

impl<'a, R: Resolver> IndexRegistry<'a, R> {
    pub fn new(sources: &'a Sources, others: R) -> IndexRegistry<'a, R> {
        IndexRegistry { sources, others }
    }

    /// What answers for the sources that are not registries.
    pub fn others(&self) -> &R {
        &self.others
    }

    /// The source `id`'s packages come from, when it is a registry's index.
    pub fn registry_of(&self, id: SourceId) -> Result<Option<SourceId>> {
        self.sources.map().registry_of(id)
    }

    /// The query `dep`, `kind` (a `Resolving` machine's ask), read as `reads` says.
    fn query_as(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        reads: Reads,
        f: &mut impl FnMut(IndexSummary),
    ) -> std::result::Result<Lookup, ResolveError> {
        let id = dep.source_id();
        let Some(from) = self.registry_of(id).map_err(registry_error)? else {
            return self.others.query(dep, kind, f);
        };
        let registry = self.sources.registry(from).map_err(registry_error)?;
        // A source asked for at no version the lock file pins is updated, once, as cargo's
        // `PackageRegistry::load` invalidates it.
        if !id.has_locked_precise() && !registry.is_updated() {
            registry.invalidate();
        }
        registry.query_dependency(dep, kind, id, reads, f).map_err(registry_error)
    }

    /// The answer to a `Resolving` machine's ask: the summaries of `dep` as `kind` matches
    /// them, each index file read now on this thread (a request when the cache is not good),
    /// so that the machine's next query of it is answered from what this read holds. Callers
    /// with many asks run one per thread.
    pub fn fetch_summaries<A: Allocator>(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        alloc: A,
    ) -> std::result::Result<Vec<IndexSummary, A>, ResolveError> {
        let mut found = Vec::new_in(alloc);
        match self.query_as(dep, kind, Reads::Blocking, &mut |s| found.push(s))? {
            Lookup::Answered(()) => Ok(found),
            // A blocking read never leaves a file unread, so only `others` says this.
            Lookup::Unknown => Err(registry_error(
                NoAnswer { dep: dep.package_name().to_string(), source: self.describe_source(dep.source_id()) }
                    .into(),
            )),
        }
    }
}

/// The index's own failure (a read, a request, an unknown `[source]`) as the resolver's
/// error: the trait's error type, which boxes an implementor's failures in `Registry`.
fn registry_error(error: anyhow::Error) -> ResolveError {
    ResolveError::Registry(error.into())
}

impl<R: Resolver> Resolver for IndexRegistry<'_, R> {
    /// Answered from the index files this update holds, [`Lookup::Unknown`] for one it does
    /// not: the network is asked only by [`IndexRegistry::fetch_summaries`].
    fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut impl FnMut(IndexSummary),
    ) -> std::result::Result<Lookup, ResolveError> {
        self.query_as(dep, kind, Reads::Held, f)
    }

    /// As cargo's `ReplacedSource::describe`: the source read, and the one it replaces unless
    /// that is crates.io read over its sparse index.
    fn describe_source(&self, source: SourceId) -> String {
        self.sources.map().describe(source)
    }

    fn is_replaced(&self, source: SourceId) -> bool {
        self.sources.map().is_replaced(source)
    }
}
