//! blaze-resolve's `Registry` over the registries' indexes as the view mounts them, as cargo
//! 1.99 answers the resolver: `RegistrySource::query` and `RegistryIndex::query_inner`
//! (`src/sources/registry/{mod.rs,index/mod.rs}`) behind `[source]` replacement, with what the
//! lock file pins answered from the packages read (v1's `Pinned` in
//! `crates/blaze-metadata/src/lib.rs` and `IndexRegistry` in `crates/blaze-registry/src/query.rs`).
//! An index file no mount covers is answered as absent and kept as a fetch: the resolve that
//! read it is thrown away, the file fetched, and the resolve run again.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::bail;
use blaze_hash::HashMap;
use blaze_query::{Cancelled, Cx};
use blaze_resolve::{
    Dependency, IndexSummary, MemRegistry, OptVersionReq, QueryKind, Registry, SourceId,
    query_pinned,
};

use super::{IndexFile, Sources};
use crate::index::{Lines, index_summary};
use crate::query::Fetch;

const CANCELLED: &str = "the load was cancelled";

/// The registries' indexes for what the lock file does not pin (cargo's `PackageRegistry`
/// sources): a `blaze_resolve::Registry` whose exact queries of pinned packages answer from
/// `pinned` (every package read: members, path packages, locked ones), and the rest from the
/// sparse-index files mounted where cargo keeps them.
pub(crate) struct Index<'s, 'c, 'db> {
    sources: &'s Sources,
    cx: RefCell<&'c mut Cx<'db>>,
    pinned: MemRegistry,
    /// Each index file read, by its registry and the name asked for.
    files: RefCell<HashMap<SourceId, HashMap<String, Rc<Lines>>>>,
    needs: RefCell<Vec<Fetch>>,
    cancelled: Cell<bool>,
}

impl<'s, 'c, 'db> Index<'s, 'c, 'db> {
    pub(crate) fn new(sources: &'s Sources, cx: &'c mut Cx<'db>, pinned: MemRegistry) -> Self {
        Index {
            sources,
            cx: RefCell::new(cx),
            pinned,
            files: RefCell::default(),
            needs: RefCell::default(),
            cancelled: Cell::new(false),
        }
    }

    /// After the resolve: the index files it needed that no mount covers (none offline), or
    /// `Err(Cancelled)` when a read was cancelled (then the resolve's answer is thrown away).
    pub(crate) fn finish(self) -> Result<Vec<Fetch>, Cancelled> {
        if self.cancelled.get() {
            return Err(Cancelled);
        }
        Ok(self.needs.into_inner())
    }

    /// A read through the view, which once cancelled fails every read after it.
    fn read<T>(&self, read: impl FnOnce(&mut Cx<'db>) -> Result<T, Cancelled>) -> anyhow::Result<T> {
        if self.cancelled.get() {
            bail!(CANCELLED);
        }
        let mut cx = self.cx.borrow_mut();
        match read(&mut **cx) {
            Ok(it) => Ok(it),
            Err(Cancelled) => {
                self.cancelled.set(true);
                bail!(CANCELLED)
            }
        }
    }

    /// The lines of `name`'s index file in the registry `from`, read once.
    fn lines(&self, from: SourceId, name: &str) -> anyhow::Result<Rc<Lines>> {
        if let Some(lines) = self.files.borrow().get(&from).and_then(|it| it.get(name)) {
            return Ok(lines.clone());
        }
        let lines = match self.read(|cx| self.sources.index_file(cx, from, name))? {
            IndexFile::Here(bytes) => Lines::new(bytes),
            IndexFile::Absent => Lines::empty(),
            IndexFile::Needs(fetch) => {
                let mut needs = self.needs.borrow_mut();
                if !needs.iter().any(|it| it.at == fetch.at) {
                    needs.push(fetch);
                }
                Lines::empty()
            }
            IndexFile::Failed(error) => return Err(error),
        };
        let lines = Rc::new(lines);
        self.files.borrow_mut().entry(from).or_default().insert(name.to_owned(), lines.clone());
        Ok(lines)
    }

    /// cargo's `RegistryIndex::query_inner`: each version of `name` that `req` matches, with
    /// `source` as its source. Offline, only those whose package is fetched, when there are
    /// any; when there are none, all of them, so that a dependency nothing builds still
    /// resolves and one that is built fails to download.
    fn query_index(
        &self,
        from: SourceId,
        name: &str,
        req: &OptVersionReq,
        source: SourceId,
        f: &mut impl FnMut(IndexSummary),
    ) -> anyhow::Result<()> {
        let lines = self.lines(from, name)?;
        let lines: Vec<_> = lines.matching(|version| req.matches(version)).collect();
        if self.sources.offline().is_some() {
            let mut called = false;
            for line in &lines {
                let entry = &line.entry;
                let fetched =
                    self.read(|cx| self.sources.is_downloaded(cx, from, &entry.name, &entry.version))?;
                if fetched && let Some(summary) = index_summary(line, source) {
                    called = true;
                    f(summary);
                }
            }
            if called {
                return Ok(());
            }
        }
        for line in lines {
            if let Some(summary) = index_summary(line, source) {
                f(summary);
            }
        }
        Ok(())
    }

    /// cargo's `RegistrySource::query`: the versions of the registry's package `dep` asks
    /// for, as `kind` matches them, with `source` (the source `dep` names, which `from` may
    /// replace) as their source.
    fn query_registry(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        source: SourceId,
        from: SourceId,
        f: &mut impl FnMut(IndexSummary),
    ) -> anyhow::Result<()> {
        let name = dep.package_name();
        let mut req = dep.version_req().clone();
        if let Some((_, to)) =
            source.precise_registry_version(name.as_str()).filter(|(from, _)| req.matches(from))
        {
            req.precise_to(to);
        }
        let mut called = false;
        self.query_index(from, name.as_str(), &req, source, &mut |s| {
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
                IndexSummary::Unsupported(..) | IndexSummary::Invalid(_) | IndexSummary::Offline(_) => {}
            }
        })?;
        if called || !matches!(kind, QueryKind::AlternativeNames | QueryKind::Normalized) {
            return Ok(());
        }
        // A name spelled with `-` for `_` or `_` for `-`, which the resolver turns into "a
        // similar crate exists".
        for other in [name.as_str().replace('-', "_"), name.as_str().replace('_', "-")] {
            if other == name.as_str() {
                continue;
            }
            self.query_index(from, &other, &req, source, &mut |s| {
                if !s.is_yanked() || kind == QueryKind::AlternativeNames {
                    f(s);
                }
            })?;
        }
        Ok(())
    }
}

impl Registry for Index<'_, '_, '_> {
    async fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut impl FnMut(IndexSummary),
    ) -> anyhow::Result<()> {
        let source = dep.source_id();
        let registry = self.sources.registry_of(source)?;
        let describe = |it: SourceId| self.describe_source(it);
        if query_pinned(&self.pinned, registry.is_some(), dep, kind, f, describe).await? {
            return Ok(());
        }
        match registry {
            Some(from) => self.query_registry(dep, kind, source, from, f),
            None => self.pinned.query(dep, kind, f).await,
        }
    }

    fn describe_source(&self, source: SourceId) -> String {
        self.sources.describe(source)
    }

    fn is_replaced(&self, source: SourceId) -> bool {
        self.sources.is_replaced(source)
    }
}
