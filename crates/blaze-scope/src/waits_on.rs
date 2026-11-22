//! Unknown, not absent: a `use` or macro path of a crate that did not resolve because a scope it
//! looks a name up in is a failed dependency's, or takes names from one, waits on that crate.
//! The name may be one the failed crate would have given, so it is not "not found" (rules 3 and
//! 19): the reporter counts it under that crate's failure, which is reported once at its cause
//! (plan row A8), as a path written straight into a failed crate already is.
//!
//! A failed dependency is handed to its dependents as an [`error_record`]: an empty root that a
//! glob imports nothing from and a path finds nothing in. turso, 2026-11-21 (c1647: 58 such
//! errors): `turso_core` had no names (its build script failed), so `sync/engine/src/alloc.rs`'s
//! `pub use turso_core::alloc::*;` failed, and each `use crate::alloc::TursoAllocExt;` of the
//! crate said "no `TursoAllocExt` in `crate::alloc`", a false error naming nothing.
//!
//! What waits, inside the crate (its own modules' scopes are here, its dependencies' are not):
//! - a module of a failed crate (`use failed as f;` then `f::X`);
//! - a module with a glob edge to a module of a failed crate (`pub use failed::*;`), or to a
//!   module of this crate that waits (`pub use crate::alloc::*;`);
//! - a module whose glob import failed for a reason that itself waits (`pub use
//!   failed::alloc::*;`): every name in it;
//! - a module whose named import failed so: that one name (`pub use failed::Thing;` makes
//!   `Thing` unknown there, `Other` still absent).
//!
//! A name absent from a module of a crate that resolved stays an error, here or across crates.
//! Not covered: a dependency that re-exports a failed crate's names (`rusqlite`'s `pub use
//! libsqlite3_sys::ErrorCode;`): its exports record leaves out what failed in it, so a
//! dependent cannot tell; that needs the exports record to say which of its modules wait.
//!
//! [`error_record`]: crate::crate_scope::error_record

use blaze_hash::HashMap;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CrateRootBuilder, DefKind, Namespace, Path, PathKind, Rename, ScopeBuilder, ScopeError, Unresolved, UseForm, UseSite};

use crate::names::{self, PathError, VariantHit, Why, World, Written};
use crate::resolve::CrateNames;

/// The most modules one lookup walks through glob edges (rule 18): past it the walk stops and
/// the path's error stands.
const MOST_GLOB_STEPS: usize = 1 << 12;

/// A dependency that failed this command: handed as an error record
/// ([`crate::crate_scope::error_record_crate`] reads `key` and `name` from it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FailedDep<'a> {
    /// What the crate calls it (its extern prelude name), or `None` for one reached only
    /// through another dependency.
    pub called: Option<&'a str>,
    /// Its own name: what a path behind it waits on.
    pub name: &'a str,
    pub key: CrateItemKey,
}

/// The crate's modules and names that wait on a failed dependency, found once per crate
/// ([`WaitsOn::of`]) and asked per error ([`WaitsOn::waits_on`]).
pub struct WaitsOn<'c, 'a> {
    names: &'c CrateNames<'a>,
    failed: &'c [FailedDep<'c>],
    /// A module every name of which waits (a glob of it failed so), and on which crate.
    whole: HashMap<ItemKey, &'c str, Alloc<'c>>,
    /// One name of a module that waits (a named import of it failed so).
    named: HashMap<(ItemKey, &'a str), &'c str, Alloc<'c>>,
    alloc: Alloc<'c>,
}

impl<'c, 'a: 'c> WaitsOn<'c, 'a> {
    /// What waits in crate `names`, whose failed dependencies are `failed`. Each failed import
    /// whose cause waits marks its module (a glob) or its name (a named import), until no more
    /// is marked: a mark only ever adds, and of two crates a mark could name the lesser name is
    /// kept, so the marks are the same in any map order (rule 25).
    pub fn of(names: &'c CrateNames<'a>, failed: &'c [FailedDep<'c>], alloc: Alloc<'c>) -> WaitsOn<'c, 'a> {
        let mut w = WaitsOn { names, failed, whole: HashMap::new_in(alloc), named: HashMap::new_in(alloc), alloc };
        if failed.is_empty() {
            return w;
        }
        // Each pass marks at least one more or ends; there are no more marks than errors.
        let most = names.done.values().map(|r| r.scope.errors.len()).sum::<usize>() + 1;
        for _ in 0..most {
            let mut grew = false;
            for (&module, r) in names.done.iter() {
                for error in r.scope.errors.iter() {
                    let ScopeError::Import { site, path, segment, why } = error else { continue };
                    let Some(krate) = w.cause(module, path, *segment as usize, *why) else { continue };
                    match w.form(module, *site) {
                        Some(UseForm::Glob) => grew |= keep_least(&mut w.whole, module, krate),
                        Some(UseForm::Named { rename, .. }) => {
                            let bound = match rename {
                                Rename::Same => path.segments.last().copied(),
                                Rename::To(name) => Some(name),
                                Rename::Underscore => None,
                            };
                            if let Some(bound) = bound {
                                grew |= keep_least(&mut w.named, (module, bound), krate);
                            }
                        }
                        // A `use` of a fragment this solve does not hold: no mark.
                        None => {}
                    }
                }
            }
            if !grew {
                break;
            }
        }
        w
    }

    /// The failed crate `error`, a scope error of `module`, waits on; `None`: the error stands
    /// as the resolver gave it.
    pub fn waits_on(&self, module: ItemKey, error: &ScopeError<&'a str, Path<'a, &'a str>>) -> Option<&'c str> {
        match error {
            ScopeError::Import { path, segment, why, .. } | ScopeError::Macro { path, segment, why, .. } => self.cause(module, path, *segment as usize, *why),
            _ => None,
        }
    }

    /// Why `path`, written in `module`, failing at `segment` for `why`, is unknown: the failed
    /// crate the scope it looked in waits on.
    fn cause(&self, module: ItemKey, path: &Path<'a, &'a str>, segment: usize, why: Unresolved) -> Option<&'c str> {
        if why != Unresolved::NotFound {
            return None;
        }
        let segments = &path.segments[..];
        let name = *segments.get(segment)?;
        // A path written straight into a failed crate (`failed::x`), as the reporter has always
        // counted it.
        if segment > 0 && matches!(path.kind, PathKind::Plain | PathKind::Abs) {
            if let Some(dep) = self.failed.iter().find(|dep| dep.called == Some(segments[0])) {
                return Some(dep.name);
            }
        }
        let searched = if segment == 0 {
            match path.kind {
                PathKind::Plain | PathKind::SelfMod => module,
                PathKind::Crate => self.names.root,
                _ => return None,
            }
        } else {
            let prefix = Written { kind: path.kind, segments: &segments[..segment] };
            match names::path(&OneCrate(self.names), module, Namespace::Types, prefix, self.alloc) {
                Ok(found) if found.consumed == segment => {
                    if let Some(dep) = self.failed_of(found.def.key) {
                        return Some(dep.name);
                    }
                    if found.def.kind != DefKind::Module {
                        return None;
                    }
                    found.def.key
                }
                // The prefix ended at a definition with no names under it: no scope was searched.
                Ok(_) => return None,
                // The walk met a scope this crate does not hold: a failed crate's, or another
                // dependency's, which may well hold the name.
                Err(PathError { why: Why::NotRead { module: unheld, .. }, .. }) => return self.failed_of(unheld).map(|dep| dep.name),
                Err(_) => return None,
            }
        };
        self.module_waits(searched, name)
    }

    /// Whether `name` looked up in `module` waits: the module, or one its globs reach in this
    /// crate, is a failed crate's or is marked. The least crate name of all that are found.
    fn module_waits(&self, module: ItemKey, name: &'a str) -> Option<&'c str> {
        let mut stack = AVec::new_in(self.alloc);
        let mut seen: AVec<'_, ItemKey> = AVec::new_in(self.alloc);
        stack.push(module);
        let mut found: Option<&'c str> = None;
        while let Some(m) = stack.pop() {
            if seen.contains(&m) {
                continue;
            }
            if seen.len() == MOST_GLOB_STEPS {
                return None;
            }
            seen.push(m);
            let here = self.failed_of(m).map(|dep| dep.name).or_else(|| self.whole.get(&m).copied()).or_else(|| self.named.get(&(m, name)).copied());
            if let Some(krate) = here {
                found = Some(found.map_or(krate, |least| least.min(krate)));
                continue;
            }
            if let Some(r) = self.names.done.get(&m) {
                stack.extend(r.scope.globs.iter().map(|edge| edge.source.key));
            }
        }
        found
    }

    /// The failed dependency `key` is an item of.
    fn failed_of(&self, key: ItemKey) -> Option<&'c FailedDep<'c>> {
        self.failed.iter().find(|dep| key.0.0[..8] == dep.key.prefix()[..])
    }

    /// The form of the `use` at `site` of `module`: a glob or a named import.
    fn form(&self, module: ItemKey, site: UseSite) -> Option<UseForm<&'a str>> {
        let uses = match site.expansion {
            None => &self.names.decls.get(&module)?.1.uses,
            Some(call) => &self.names.expansions.get(&call)?.decls.uses,
        };
        uses.get(site.index as usize).map(|it| it.leaf)
    }
}

/// `krate` as what `key` waits on, unless a lesser name is kept already; whether it changed.
fn keep_least<'s, K: Eq + core::hash::Hash>(marks: &mut HashMap<K, &'s str, Alloc<'_>>, key: K, krate: &'s str) -> bool {
    match marks.get(&key) {
        Some(kept) if *kept <= krate => false,
        _ => {
            marks.insert(key, krate);
            true
        }
    }
}

/// One crate's finished scopes as a [`World`]: a path walks its own modules; a scope of any
/// other crate is not held ([`Why::NotRead`]), which [`WaitsOn::cause`] reads.
struct OneCrate<'c, 'a>(&'c CrateNames<'a>);

impl OneCrate<'_, '_> {
    fn own(&self, key: ItemKey) -> bool {
        key.0.0[..8] == self.0.key.prefix()[..]
    }
}

impl<'c, 'a: 'c> World<&'a str> for OneCrate<'c, 'a> {
    type Scope<'w>
        = &'w ScopeBuilder<'w, &'a str>
    where
        Self: 'w;
    type Root<'w>
        = &'w CrateRootBuilder<'w, &'a str>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<&ScopeBuilder<'_, &'a str>> {
        self.0.done.get(&module).map(|r| &r.scope)
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.0.chains.get(&module).map(|c| c.as_slice())
    }

    fn root_of(&self, key: ItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        self.own(key).then_some(&self.0.crate_root)
    }

    fn root(&self, krate: CrateItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        (krate.prefix() == self.0.key.prefix()).then_some(&self.0.crate_root)
    }

    /// Not asked of the enums: a prefix through an enum is not a module's scope.
    fn variant(&self, _: ItemKey, _: &'a str) -> Option<Option<VariantHit>> {
        None
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        let Some(chain) = self.0.chains.get(&module) else { return false };
        chain.iter().any(|m| self.0.info(*m).is_some_and(|info| info.no_implicit_prelude))
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.0.anonymous(module)
    }
}
