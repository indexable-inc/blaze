//! Which copy of an instance a call reaches in a program whose crates do not all check
//! overflow alike.
//!
//! rustc compiles a function with the flags of the crate that compiles it, and which crate
//! that is it decides as it goes: a crate built without optimisation calls the copy of a
//! generic instance that a crate it depends on has made, and a crate built with it compiles a
//! small function of another crate itself. Neither is in the source. Both are in the symbols
//! of what rustc built, where a copy ends in the crate that made it, so that is what is read.

use base_db::Crate;
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};

use super::{SymbolResolver, crate_checks_overflow};
use hir_ty::db::HirDatabase;

/// The crates of a program that differ in `-C overflow-checks`.
pub(super) struct Mixed {
    /// The crates cargo builds, each with whether it checks.
    crates: Vec<(Crate, bool)>,
    /// What each of `crates` depends on, itself included.
    sees: FxHashMap<Crate, FxHashSet<Crate>>,
    /// The crates of the program by rustc's `StableCrateId`, the standard library's too.
    by_id: FxHashMap<u64, Crate>,
    /// What the sources and the flags of the crates come to, when every crate has an
    /// identity: an entry of the store is of one such program only, as what its calls
    /// reach is not in its name.
    pub(super) key: Option<[u8; 32]>,
}

/// A copy of an instance in what rustc built: the crate that made it, and whether it checks.
pub(super) type Holder = (Crate, bool);

impl Mixed {
    /// `None` for a program of `built` whose crates all check alike: every copy of an
    /// instance behaves as every other, and none of this is asked.
    pub(super) fn of(
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        built: &FxHashSet<Crate>,
    ) -> Option<Mixed> {
        let program = super::linked_crates(db, symbols, built.iter().copied());
        let crates: Vec<(Crate, bool)> = program
            .iter()
            .filter_map(|&krate| Some((krate, crate_checks_overflow(db, krate)?)))
            .collect();
        if crates.iter().all(|it| it.1 == crates[0].1) {
            return None;
        }
        let identity = |&(krate, checks): &(Crate, bool)| {
            let id = match symbols.content_id(krate) {
                Some(content) => content.0.to_vec(),
                None => symbols.stable_crate_id(krate)?.to_le_bytes().to_vec(),
            };
            Some((id, checks))
        };
        let key = crates.iter().map(identity).collect::<Option<Vec<_>>>().map(|mut ids| {
            ids.sort_unstable();
            let mut hasher = Sha256::new();
            for (id, checks) in ids {
                hasher.update(id);
                hasher.update([u8::from(checks)]);
            }
            hasher.finalize().into()
        });
        let sees = crates
            .iter()
            .map(|&(krate, _)| (krate, krate.transitive_deps(db).into_iter().collect()))
            .collect();
        let by_id = program
            .iter()
            .filter_map(|&krate| Some((symbols.stable_crate_id(krate)?, krate)))
            .collect();
        Some(Mixed { crates, sees, by_id, key })
    }

    /// The crate that made the copy whose symbol ends in `id`. The standard library is built
    /// without checks.
    pub(super) fn holder(&self, db: &dyn HirDatabase, id: u64) -> Option<Holder> {
        let krate = *self.by_id.get(&id)?;
        Some((krate, crate_checks_overflow(db, krate).unwrap_or(false)))
    }

    /// Whether the copy of a generic instance that a crate built with `asking` calls checks,
    /// given the copies rustc made. A crate with a copy calls its own. One without calls a
    /// copy of a crate it depends on, and one that depends on none has made no call of the
    /// instance that rustc compiled: it would compile its own.
    ///
    /// `None` when the crates built with `asking` do not all reach copies that check alike:
    /// which of them asks is not known here.
    pub(super) fn reached(&self, asking: bool, holders: &[Holder]) -> Option<bool> {
        let mut reached = None;
        for (krate, _) in self.crates.iter().filter(|it| it.1 == asking) {
            let sees = &self.sees[krate];
            let own = holders.iter().any(|it| it.0 == *krate);
            let mut seen = holders.iter().filter(|it| sees.contains(&it.0)).map(|it| it.1);
            let checks = match (own, seen.next()) {
                (true, _) => asking,
                (false, None) => continue,
                (false, Some(first)) if seen.all(|it| it == first) => first,
                (false, Some(_)) => return None,
            };
            if reached.is_some_and(|it| it != checks) {
                return None;
            }
            reached = Some(checks);
        }
        Some(reached.unwrap_or(asking))
    }

    /// As [`Mixed::reached`] for an instance whose copies have no name to be found by, from
    /// the crates its name `mentions`: only a crate that depends on all of them can have made
    /// a copy, and of a function only one of `makers`. Either may be unknown, not both.
    /// `None` when a crate built with `asking` could reach a copy of a crate that checks
    /// otherwise, as whether there is one is not known.
    ///
    /// The standard library's copies are left out: see `BUILDMODEL.md`.
    pub(super) fn reachable(
        &self,
        asking: bool,
        mentions: Option<&FxHashSet<Crate>>,
        makers: Option<&[Holder]>,
    ) -> Option<bool> {
        if mentions.is_none() && makers.is_none() {
            return None;
        }
        let able = |krate: &Crate| {
            mentions.is_none_or(|mentions| mentions.iter().all(|it| self.sees[krate].contains(it)))
                && makers.is_none_or(|makers| makers.iter().any(|it| it.0 == *krate))
        };
        let able: Vec<Holder> = self.crates.iter().copied().filter(|it| able(&it.0)).collect();
        let sees = |asker: &Holder, it: &Holder| self.sees[&asker.0].contains(&it.0);
        let alike =
            |asker: &Holder| able.iter().filter(|it| sees(asker, it)).all(|it| it.1 == asking);
        self.crates.iter().filter(|it| it.1 == asking).all(alike).then_some(asking)
    }
}
