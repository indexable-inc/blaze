//! blaze-resolve's [`Registry`] over the registries' indexes, as cargo 1.99 answers the
//! resolver: each line of an index file made a [`blaze_resolve::Summary`] by blaze-units'
//! [`index_summary`] (cargo's `index_package_to_summary` and `registry_dependency_into_dep` in
//! `src/sources/registry/index/mod.rs`), the versions a
//! dependency is offered as `RegistrySource::query` and `RegistryIndex::query_inner` offer them
//! (`src/sources/registry/mod.rs`), behind `[source]` replacement as `SourceConfigMap` and
//! `ReplacedSource` put it (`src/sources/{config.rs,replaced.rs}`). Sources that are not
//! registries (paths, git checkouts, vendored directories) are answered by another registry.

use anyhow::Result;
use blaze_resolve::{
    Dependency, IndexSummary, OptVersionReq, QueryKind, Registry as Resolver, SourceId,
};

use crate::{
    Registry, Sources,
    index::{IndexEntry, IndexLine},
};

pub use blaze_units::index::index_summary;

impl Registry {
    /// cargo's `RegistryIndex::query_inner`: each version of `name` that `req` matches, with
    /// `source` as its source. Offline, only those whose `.crate` is on disk, when there are
    /// any; when there are none, all of them, so that a dependency nothing builds still
    /// resolves and one that is built fails to download.
    async fn query_index(
        &self,
        name: &str,
        req: &OptVersionReq,
        source: SourceId,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<()> {
        let summaries = self.summaries(name).await?;
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
                return Ok(());
            }
        }
        lines.iter().filter_map(|line| index_summary(line, source)).for_each(f);
        Ok(())
    }

    /// cargo's `RegistrySource::query`: the versions of the registry's package `dep` asks
    /// for, as `kind` matches them, with `source` (the source `dep` names, which this registry
    /// may replace) as their source.
    pub async fn query_dependency(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        source: SourceId,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<()> {
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
            self.query_index(&name, &req, source, &mut |s| {
                if matches!(s, IndexSummary::Candidate(_) | IndexSummary::Yanked(_))
                    && dep.matches(s.summary())
                {
                    called = true;
                    f(s);
                }
            })
            .await?;
            if called {
                return Ok(());
            }
            self.invalidate();
        }
        self.query_index(&name, &req, source, &mut |s| {
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
        })
        .await?;
        if called || !matches!(kind, QueryKind::AlternativeNames | QueryKind::Normalized) {
            return Ok(());
        }
        // A name spelled with `-` for `_` or `_` for `-`, which the resolver turns into "a
        // similar crate exists".
        for other in [name.replace('-', "_"), name.replace('_', "-")] {
            if other == *name {
                continue;
            }
            self.query_index(&other, &req, source, &mut |s| {
                if !s.is_yanked() || kind == QueryKind::AlternativeNames {
                    f(s);
                }
            })
            .await?;
        }
        Ok(())
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
}

impl<R: Resolver> Resolver for IndexRegistry<'_, R> {
    async fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<()> {
        let id = dep.source_id();
        let Some(from) = self.registry_of(id)? else {
            return self.others.query(dep, kind, f).await;
        };
        let registry = self.sources.registry(from)?;
        // A source asked for at no version the lock file pins is updated, once, as cargo's
        // `PackageRegistry::load` invalidates it.
        if !id.has_locked_precise() && !registry.is_updated() {
            registry.invalidate();
        }
        registry.query_dependency(dep, kind, id, f).await
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
