//! What the resolver asks of the sources: cargo 1.99.0's (5f94df4789) `Registry` trait
//! (`src/workspace/registry.rs`), `QueryKind` (`src/sources/source.rs`) and `IndexSummary`
//! (`src/sources/registry/index/mod.rs`).

use blaze_hash::{FixedState, Map};
use std::alloc::Global;

use crate::dependency::Dependency;
use crate::error::ResolveError;
use crate::interning::InternedString;
use crate::package_id::PackageId;
use crate::source_id::SourceId;
use crate::summary::Summary;

type Result<T, E = ResolveError> = std::result::Result<T, E>;

/// How a query matches.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum QueryKind {
    /// Summaries `Dependency::matches`, yanked ones included and marked.
    Exact,
    /// Like `Exact`, with the versions a registry would reject (yanked, unsupported, invalid)
    /// returned for messages.
    RejectedVersions,
    /// Every summary of the name and of names that normalize alike (`-` and `_`), for "did
    /// you mean" messages.
    AlternativeNames,
    /// Matches in every way, with the name normalized as the source defines.
    Normalized,
}

use blaze_own::Share;

/// One version a source has for a name, and whether it may be chosen.
#[derive(Debug, Clone)]
pub enum IndexSummary {
    Candidate(Summary),
    /// Yanked in its registry: chosen only when a lock file or `[patch]` has it, or
    /// `cargo update --precise` asks for it.
    Yanked(Summary),
    /// Offline, and its `.crate` is not downloaded.
    Offline(Summary),
    /// From an index schema newer than this one reads (the version is the schema's).
    Unsupported(Summary, u32),
    /// A supported schema with an error in the entry.
    Invalid(Summary),
}

impl Share for IndexSummary {}

impl IndexSummary {
    pub fn summary(&self) -> &Summary {
        match self {
            IndexSummary::Candidate(sum)
            | IndexSummary::Yanked(sum)
            | IndexSummary::Offline(sum)
            | IndexSummary::Unsupported(sum, _)
            | IndexSummary::Invalid(sum) => sum,
        }
    }

    pub fn into_summary(self) -> Summary {
        match self {
            IndexSummary::Candidate(sum)
            | IndexSummary::Yanked(sum)
            | IndexSummary::Offline(sum)
            | IndexSummary::Unsupported(sum, _)
            | IndexSummary::Invalid(sum) => sum,
        }
    }

    pub fn map_summary(self, f: impl Fn(Summary) -> Summary) -> Self {
        match self {
            IndexSummary::Candidate(s) => IndexSummary::Candidate(f(s)),
            IndexSummary::Yanked(s) => IndexSummary::Yanked(f(s)),
            IndexSummary::Offline(s) => IndexSummary::Offline(f(s)),
            IndexSummary::Unsupported(s, v) => IndexSummary::Unsupported(f(s), v),
            IndexSummary::Invalid(s) => IndexSummary::Invalid(f(s)),
        }
    }

    pub fn package_id(&self) -> PackageId {
        self.summary().package_id()
    }

    pub fn is_yanked(&self) -> bool {
        matches!(self, IndexSummary::Yanked(..))
    }

    pub fn is_offline(&self) -> bool {
        matches!(self, IndexSummary::Offline(..))
    }
}

/// Whether a table held the answer to a query.
#[must_use]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lookup<T = ()> {
    Answered(T),
    /// The table does not hold it yet (an index file not fetched): the resolve machine
    /// ([`crate::resolving::Resolving`]) asks its driver for it as an
    /// [`Ask`](crate::resolving::Ask), and the round that met it is thrown away.
    Unknown,
}

/// The sources as the resolver sees them, with `[patch]`, `[replace]` and source replacement
/// already applied by whoever implements it: a synchronous table of what is known now.
///
/// It never waits. What it cannot answer from what it holds (an index file not fetched) it
/// answers [`Lookup::Unknown`]; the resolve machine collects every such query of a round into
/// one [`Effect::Query`](crate::resolving::Effect::Query), so a graph's index files are
/// fetched together, by the driver, and resolves again over the answers.
pub trait Registry {
    /// Calls `f` with each summary of `dep`'s name that matches it as `kind` says, or calls it
    /// never and answers [`Lookup::Unknown`].
    fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<Lookup>;

    fn query_vec(&self, dep: &Dependency, kind: QueryKind) -> Result<Lookup<Vec<IndexSummary>>> {
        let mut ret = Vec::new();
        Ok(match self.query(dep, kind, &mut |s| ret.push(s))? {
            Lookup::Answered(()) => Lookup::Answered(ret),
            Lookup::Unknown => Lookup::Unknown,
        })
    }

    /// How to name the source in messages.
    fn describe_source(&self, source: SourceId) -> String;

    /// Whether `[source]` replaces this source with another.
    fn is_replaced(&self, source: SourceId) -> bool;
}

impl<R: Registry> Registry for &R {
    fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<Lookup> {
        (**self).query(dep, kind, f)
    }

    fn describe_source(&self, source: SourceId) -> String {
        (**self).describe_source(source)
    }

    fn is_replaced(&self, source: SourceId) -> bool {
        (**self).is_replaced(source)
    }
}

/// A registry held in memory: the resolver's tests, and sources already read.
#[derive(Default, Debug, Clone)]
pub struct MemRegistry {
    packages: Map<InternedString, Vec<IndexSummary>, FixedState, Global>,
}

impl MemRegistry {
    pub fn new() -> MemRegistry {
        MemRegistry::default()
    }

    pub fn add(&mut self, summary: IndexSummary) {
        self.packages.entry(summary.package_id().name()).or_default().push(summary);
    }

    /// Calls `f` with each summary it holds that matches `dep` as `kind` says: a table in
    /// memory always answers.
    pub fn matching(&self, dep: &Dependency, kind: QueryKind, f: &mut impl FnMut(IndexSummary)) {
        let normalized = |s: &str| s.replace('-', "_");
        for (name, summaries) in &self.packages {
            let name_matches = match kind {
                QueryKind::AlternativeNames => normalized(name) == normalized(&dep.package_name()),
                _ => *name == dep.package_name(),
            };
            if !name_matches {
                continue;
            }
            for s in summaries {
                let matched = match kind {
                    QueryKind::Exact | QueryKind::RejectedVersions => dep.matches(s.summary()),
                    QueryKind::AlternativeNames | QueryKind::Normalized => true,
                };
                if !matched {
                    continue;
                }
                match s {
                    IndexSummary::Candidate(_) | IndexSummary::Yanked(_) => f(s.share()),
                    _ if kind == QueryKind::RejectedVersions => f(s.share()),
                    _ => {}
                }
            }
        }
    }
}

impl FromIterator<Summary> for MemRegistry {
    fn from_iter<I: IntoIterator<Item = Summary>>(iter: I) -> Self {
        let mut registry = MemRegistry::new();
        for summary in iter {
            registry.add(IndexSummary::Candidate(summary));
        }
        registry
    }
}

impl Registry for MemRegistry {
    fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<Lookup> {
        self.matching(dep, kind, f);
        Ok(Lookup::Answered(()))
    }

    fn describe_source(&self, source: SourceId) -> String {
        source.to_string()
    }

    fn is_replaced(&self, _source: SourceId) -> bool {
        false
    }
}

/// Asks `pinned` first when `dep` is exact and its source has no registry or its version is
/// locked. `Ok(true)` when that answered (or `dep` is a path), so the caller asks no further;
/// an unregistered source `pinned` does not hold is refused.
pub fn query_pinned(
    pinned: &MemRegistry,
    registered: bool,
    dep: &Dependency,
    kind: QueryKind,
    f: &mut impl FnMut(IndexSummary),
    describe: impl FnOnce(SourceId) -> String,
) -> Result<bool> {
    if kind != QueryKind::Exact || (registered && !dep.version_req().is_locked()) {
        return Ok(false);
    }
    let mut found = false;
    pinned.matching(dep, kind, &mut |it| {
        found = true;
        f(it)
    });
    if found || dep.source_id().is_path() {
        return Ok(true);
    }
    if !registered {
        return Err(ResolveError::NotPinned {
            dep: dep.package_name(),
            source: describe(dep.source_id()),
        });
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn summary(name: &str, version: &str) -> Summary {
        let id = PackageId::try_new(name, version, SourceId::crates_io()).unwrap();
        Summary::new(id, vec![], &BTreeMap::new(), None::<&str>, None).unwrap()
    }

    fn versions(r: &MemRegistry, dep: &Dependency, kind: QueryKind) -> Vec<String> {
        let Ok(Lookup::Answered(found)) = r.query_vec(dep, kind) else { panic!() };
        let mut found: Vec<_> = found
            .iter()
            .map(|s| {
                format!(
                    "{}{}",
                    s.package_id().version(),
                    if s.is_yanked() { " yanked" } else { "" }
                )
            })
            .collect();
        found.sort();
        found
    }

    #[test]
    fn exact_queries_mark_yanked() {
        let mut r = MemRegistry::new();
        r.add(IndexSummary::Candidate(summary("foo", "1.0.0")));
        r.add(IndexSummary::Yanked(summary("foo", "1.1.0")));
        r.add(IndexSummary::Invalid(summary("foo", "1.2.0")));
        r.add(IndexSummary::Candidate(summary("foo", "2.0.0")));
        r.add(IndexSummary::Candidate(summary("foo_bar", "1.0.0")));
        let dep = Dependency::parse("foo", Some("1"), SourceId::crates_io()).unwrap();
        assert_eq!(versions(&r, &dep, QueryKind::Exact), ["1.0.0", "1.1.0 yanked"]);
        assert_eq!(
            versions(&r, &dep, QueryKind::RejectedVersions),
            ["1.0.0", "1.1.0 yanked", "1.2.0"]
        );

        let dep = Dependency::parse("foo-bar", Some("1"), SourceId::crates_io()).unwrap();
        assert!(versions(&r, &dep, QueryKind::Exact).is_empty());
        assert_eq!(versions(&r, &dep, QueryKind::AlternativeNames), ["1.0.0"]);
    }

    /// A table that does not hold the index file yet, as one before its fetch.
    struct Unfetched;

    impl Registry for Unfetched {
        fn query(
            &self,
            _dep: &Dependency,
            _kind: QueryKind,
            _f: &mut impl FnMut(IndexSummary),
        ) -> Result<Lookup> {
            Ok(Lookup::Unknown)
        }

        fn describe_source(&self, source: SourceId) -> String {
            source.to_string()
        }

        fn is_replaced(&self, _source: SourceId) -> bool {
            false
        }
    }

    /// `query_vec` passes on "not known yet" and gives nothing.
    #[test]
    fn an_unknown_answer_gives_nothing() {
        let dep = Dependency::parse("foo", Some("1"), SourceId::crates_io()).unwrap();
        assert!(matches!(Unfetched.query_vec(&dep, QueryKind::Exact), Ok(Lookup::Unknown)));
        assert!(matches!((&Unfetched).query_vec(&dep, QueryKind::Exact), Ok(Lookup::Unknown)));
    }

    /// `query_pinned` of `dep` against `pinned`: its answer and what it gave `f`.
    fn pinned_answer(
        pinned: &MemRegistry,
        registered: bool,
        dep: &Dependency,
        kind: QueryKind,
    ) -> (Result<bool>, Vec<String>) {
        let mut given = Vec::new();
        let mut f = |it: IndexSummary| given.push(it.package_id().version().to_string());
        let describe = |it: SourceId| format!("the source `{it}`");
        let answer = query_pinned(pinned, registered, dep, kind, &mut f, describe);
        (answer, given)
    }

    #[test]
    fn the_lock_file_answers_first() {
        let pinned: MemRegistry = [summary("foo", "1.0.0")].into_iter().collect();
        let exact =
            |name: &str| Dependency::parse(name, Some("=1.0.0"), SourceId::crates_io()).unwrap();

        let (answer, given) = pinned_answer(&pinned, false, &exact("foo"), QueryKind::Exact);
        assert!(answer.unwrap(), "an unregistered exact ask it holds is answered");
        assert_eq!(given, ["1.0.0"]);

        let (answer, given) = pinned_answer(&pinned, false, &exact("bar"), QueryKind::Exact);
        assert_eq!(
            answer.unwrap_err().to_string(),
            format!(
                "`bar` of the source `{}` is not pinned by Cargo.lock, and blaze reads that source \
                 only at the versions Cargo.lock pins",
                SourceId::crates_io()
            )
        );
        assert!(given.is_empty());

        let unlocked = Dependency::parse("foo", Some("1"), SourceId::crates_io()).unwrap();
        let (answer, given) = pinned_answer(&pinned, true, &unlocked, QueryKind::Exact);
        assert!(!answer.unwrap(), "a registered unlocked ask goes to the registry");
        assert!(given.is_empty(), "and asks nothing pinned");

        let alternative = QueryKind::AlternativeNames;
        let (answer, given) = pinned_answer(&pinned, false, &exact("foo"), alternative);
        assert!(!answer.unwrap(), "only an exact ask is pinned");
        assert!(given.is_empty());
    }
}
