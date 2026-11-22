//! R6's and R8's errors as the cell core keeps them until the writer ([`super::write`]) puts them
//! in the module's scope: an early resolution (an import's or a macro call's first segment, a
//! macro-written root `extern crate`) that the fully expanded crate makes ambiguous.
//!
//! The Reference (`names.resolution.expansion`): a name "must resolve to the same definition
//! regardless of the order in which macros are expanded and imports are resolved";
//! `...ambiguity.more-expanded-vs-outer` (a macro-expanded name beside an outer scope's),
//! `...ambiguity.glob-vs-expanded` and `...ambiguity.glob-vs-outer`. R8 (blaze's D56, rustc's
//! "macro-expanded `extern crate` items cannot shadow names passed with `--extern`"): a
//! macro-written root `extern crate X as n;` beside a dependency's `n` is an error; it is
//! written as the same ambiguity, the dependency's name against the expanded one.

use blaze_surface::{AmbiguityKind, ScopeError, Site, Unresolved};

use crate::{Def, Namespace};

/// One early resolution an expansion made ambiguous: the first segment `name` in `ns`, why
/// (`kind`), the definition the early resolution took or the outer scope holds (`first`) and the
/// other candidate (`second`); `None`: the speculation's candidate is gone when the run ends
/// and no second definition is there to name (written as the first segment failing, ambiguous).
///
/// Ordered so that when one item meets two such errors (two expansions, two orders of work) the
/// core keeps the least: what is written is a function of the program, not of the order the
/// work finished in (rule 25).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct EarlyAmbiguity<N> {
    pub(crate) ns: Namespace,
    pub(crate) name: N,
    pub(crate) kind: AmbiguityKind,
    pub(crate) first: Def,
    pub(crate) second: Option<Def>,
}

impl<N: Copy> EarlyAmbiguity<N> {
    /// The error at `site`, a use or a call whose path is `path` (its first segment is the
    /// ambiguous one): `Ambiguous` naming both definitions, or, with no second one, the first
    /// segment failing as ambiguous.
    pub(crate) fn error<P>(&self, site: Site, path: P) -> ScopeError<N, P> {
        match (self.second, site) {
            (Some(second), _) => ScopeError::Ambiguous { site, ns: self.ns, name: self.name, kind: self.kind, first: self.first, second },
            (None, Site::Import(site)) => ScopeError::Import { site, path, segment: 0, why: Unresolved::Ambiguous(self.kind) },
            (None, Site::Call(call)) => ScopeError::Macro { call, path, segment: 0, why: Unresolved::Ambiguous(self.kind) },
        }
    }
}
