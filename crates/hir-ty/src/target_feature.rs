//! Stuff for handling `#[target_feature]` (needed for unsafe check).
//!
//! The features themselves, rustc's tables and what each implies are the `target-features`
//! crate's, below `cfg`; this is what a crate's cfg and a function's attributes enable.

use base_db::{Db, Crate};
use blaze_mem::Frame;
use hir_def::attrs::AttrFlags;
use hir_def::{FunctionId, HasModule};
use intern::Symbol;

pub use target_features::{TargetArch, TargetFeature, TargetFeatureSet};

#[cfg(test)]
mod tests;

/// The arch of `krate`'s target, from its `target_arch` cfg: once per crate.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn crate_target_arch(db: &Db, krate: Crate) -> TargetArch {
    krate
        .cfg_options(db)
        .get_cfg_values("target_arch")
        .next()
        .map_or(TargetArch::Other, |it| TargetArch::from_target_arch(it.as_str()))
}

/// The target features of a function: those of rustc's tables as bits, and any other name
/// `#[target_feature(enable = "…")]` gives (which implies nothing) as its symbol.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TargetFeatures<'db> {
    known: TargetFeatureSet,
    /// Sorted by bytes; almost always empty.
    unknown: &'db [Symbol],
}

impl<'db> TargetFeatures<'db> {
    /// The features of `owner`'s attributes and what they imply on its crate's target.
    pub fn from_fn(db: &'db Db, owner: FunctionId) -> Self {
        let declared = TargetFeatures::from_fn_no_implications(db, owner);
        if declared.known.is_empty() {
            return declared;
        }
        let arch = crate_target_arch(db, owner.krate(db));
        Self { known: declared.known.implied_in(arch), ..declared }
    }

    /// The features the attributes name, without those they imply.
    pub fn from_fn_no_implications(db: &'db Db, owner: FunctionId) -> Self {
        if !AttrFlags::query(db, owner.into()).contains(AttrFlags::HAS_TARGET_FEATURE) {
            return Self::default();
        }
        Self::of(declared_target_features(db, owner))
    }

    fn of(declared: &'db Declared) -> Self {
        Self { known: declared.known, unknown: &declared.unknown }
    }

    /// Whether `feature` is enabled, by the attributes or (for [`Self::from_fn`]) by a feature
    /// they enable.
    #[inline]
    pub fn enables(&self, feature: TargetFeature) -> bool {
        self.known.contains(feature)
    }

    /// [`Self::enables`] of a feature by name, which may be one no table has.
    pub fn enables_name(&self, name: &str) -> bool {
        match TargetFeature::from_name(name) {
            Some(feature) => self.known.contains(feature),
            None => self.unknown.iter().any(|it| it.as_str() == name),
        }
    }

    /// Whether every feature of `other` is enabled here.
    pub fn is_superset(&self, other: &TargetFeatures<'_>) -> bool {
        self.known.is_superset(&other.known)
            && other.unknown.iter().all(|it| self.unknown.contains(it))
    }

    /// The features of rustc's tables.
    #[inline]
    pub fn known(&self) -> TargetFeatureSet {
        self.known
    }
}

/// What a function's `#[target_feature(enable = "…")]` attributes name.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
struct Declared {
    known: TargetFeatureSet,
    unknown: Box<[Symbol]>,
}

impl Declared {
    fn of<'a>(frame: &Frame, names: impl IntoIterator<Item = &'a Symbol>) -> Self {
        let mut known = TargetFeatureSet::EMPTY;
        let mut unknown = Vec::new_in(frame);
        for name in names {
            match TargetFeature::from_name(name.as_str()) {
                Some(feature) => known.insert(feature),
                None => unknown.push(name),
            }
        }
        unknown.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        unknown.dedup();
        // alloc: the memo's own list, at its size; none when every name is known
        // clone: the memo keeps its own symbols
        let unknown = unknown.iter().map(|&name| name.clone()).collect();
        Self { known, unknown }
    }
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn declared_target_features(db: &Db, owner: FunctionId) -> Declared {
    blaze_mem::frame::current(|frame| Declared::of(frame, AttrFlags::target_features(db, owner)))
}

/// The target features of rustc's tables that `krate`'s cfg has (`cfg(target_feature = "…")`),
/// without what they imply: `CfgOptions::check_atom` of each, once per crate.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn crate_target_features(db: &Db, krate: Crate) -> TargetFeatureSet {
    let values = krate.cfg_options(db).get_cfg_values("target_feature");
    // alloc: none, the set is bits by value
    values.filter_map(|it| TargetFeature::from_name(it.as_str())).collect()
}
