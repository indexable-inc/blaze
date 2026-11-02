//! Things related to storage opaques in the infer context of the next-trait-solver.

use std::ops::Deref;

use tracing::instrument;

use super::OpaqueHiddenType;
use crate::next_solver::{FxIndexMap, OpaqueTypeKey, Ty};

#[derive(Default, Debug)]
pub(crate) struct OpaqueTypeStorage<'db> {
    opaque_types: FxIndexMap<OpaqueTypeKey<'db>, OpaqueHiddenType<'db>>,
    duplicate_entries: Vec<(OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)>,
    /// Each registration in the open snapshots, with the hidden type it replaced.
    undo: Vec<(OpaqueTypeKey<'db>, Option<OpaqueHiddenType<'db>>)>,
    /// Grows on every registration, duplicate, take and rollback that undoes something, and
    /// never shrinks, so an unchanged count means unchanged opaque types.
    changes: u64,
}

/// Where the opaque types stood when a snapshot started.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OpaqueTypeMark {
    undo: u32,
    duplicate_entries: u32,
}

/// The number of entries in the opaque type storage at a given point.
///
/// Used to check that we haven't added any new opaque types after checking
/// the opaque types currently in the storage.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpaqueTypeStorageEntries {
    opaque_types: usize,
    duplicate_entries: usize,
}

impl rustc_type_ir::inherent::OpaqueTypeStorageEntries for OpaqueTypeStorageEntries {
    fn needs_reevaluation(self, canonicalized: usize) -> bool {
        self.opaque_types != canonicalized
    }
}

impl Clone for OpaqueTypeStorage<'_> {
    fn clone(&self) -> Self {
        OpaqueTypeStorage {
            // clone: only `InferCtxt::fork` copies the opaque types.
            opaque_types: self.opaque_types.clone(),
            // clone: only `InferCtxt::fork` copies the opaque types.
            duplicate_entries: self.duplicate_entries.clone(),
            // clone: only `InferCtxt::fork` copies the opaque types.
            undo: self.undo.clone(),
            changes: self.changes,
        }
    }

    /// Into `self`'s tables, keeping their capacity: a warm fork copies without allocating.
    fn clone_from(&mut self, source: &Self) {
        self.opaque_types.clone_from(&source.opaque_types);
        self.duplicate_entries.clone_from(&source.duplicate_entries);
        self.undo.clone_from(&source.undo);
        self.changes = source.changes;
    }
}

impl<'db> OpaqueTypeStorage<'db> {
    /// Back to `default()`, its capacity kept.
    pub(crate) fn empty(&mut self) {
        self.opaque_types.clear();
        self.duplicate_entries.clear();
        self.undo.clear();
        self.changes = 0;
    }

    /// Bytes its tables hold room for.
    pub(crate) fn held(&self) -> usize {
        // An index map keeps its entries in a `Vec` and their positions in a table of `usize`s.
        self.opaque_types.capacity()
            * (size_of::<(OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)>() + size_of::<usize>())
            + self.duplicate_entries.capacity()
                * size_of::<(OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)>()
            + self.undo.capacity()
                * size_of::<(OpaqueTypeKey<'db>, Option<OpaqueHiddenType<'db>>)>()
    }

    #[instrument(level = "debug")]
    pub(crate) fn remove(&mut self, key: OpaqueTypeKey<'db>, prev: Option<OpaqueHiddenType<'db>>) {
        if let Some(prev) = prev {
            *self.opaque_types.get_mut(&key).unwrap() = prev;
        } else {
            // FIXME(#120456) - is `swap_remove` correct?
            match self.opaque_types.swap_remove(&key) {
                None => {
                    panic!("reverted opaque type inference that was never registered: {key:?}")
                }
                Some(_) => {}
            }
        }
    }

    pub(crate) fn start_snapshot(&self) -> OpaqueTypeMark {
        OpaqueTypeMark {
            undo: self.undo.len() as u32,
            duplicate_entries: self.duplicate_entries.len() as u32,
        }
    }

    /// Undoes the registrations since `mark` in reverse, so each `swap_remove` removes the
    /// last entry and the order of the others is kept.
    pub(crate) fn rollback_to(&mut self, mark: OpaqueTypeMark) {
        if self.undo.len() > mark.undo as usize
            || self.duplicate_entries.len() > mark.duplicate_entries as usize
        {
            self.changes += 1;
        }
        while self.undo.len() > mark.undo as usize {
            let (key, prev) = self.undo.pop().unwrap();
            self.remove(key, prev);
        }
        let duplicates = mark.duplicate_entries as usize;
        assert!(self.duplicate_entries.len() >= duplicates, "duplicate opaque types were lost");
        self.duplicate_entries.truncate(duplicates);
    }

    /// `outermost`: no snapshot is open after this one.
    pub(crate) fn commit(&mut self, outermost: bool) {
        if outermost {
            self.undo.clear();
        }
    }

    /// Whether an opaque type was registered since `mark`.
    pub(crate) fn registered_since(&self, mark: &OpaqueTypeMark) -> bool {
        self.undo.len() > mark.undo as usize
    }

    /// The count of changes to the opaque types; see the field.
    #[inline]
    pub(crate) fn changes(&self) -> u64 {
        self.changes
    }

    pub(crate) fn is_empty(&self) -> bool {
        let OpaqueTypeStorage { opaque_types, duplicate_entries, undo: _, changes: _ } = self;
        opaque_types.is_empty() && duplicate_entries.is_empty()
    }

    pub(crate) fn take_opaque_types(
        &mut self,
    ) -> impl IntoIterator<Item = (OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)> + use<'db> {
        let OpaqueTypeStorage { opaque_types, duplicate_entries, undo: _, changes } = self;
        *changes += 1;
        std::mem::take(opaque_types).into_iter().chain(std::mem::take(duplicate_entries))
    }

    pub(crate) fn num_entries(&self) -> OpaqueTypeStorageEntries {
        OpaqueTypeStorageEntries {
            opaque_types: self.opaque_types.len(),
            duplicate_entries: self.duplicate_entries.len(),
        }
    }

    pub(crate) fn opaque_types_added_since(
        &self,
        prev_entries: OpaqueTypeStorageEntries,
    ) -> impl Iterator<Item = (OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)> {
        self.opaque_types
            .iter()
            .skip(prev_entries.opaque_types)
            .map(|(k, v)| (*k, *v))
            .chain(self.duplicate_entries.iter().skip(prev_entries.duplicate_entries).copied())
    }

    /// Only returns the opaque types from the lookup table. These are used
    /// when normalizing opaque types and have a unique key.
    ///
    /// Outside of canonicalization one should generally use `iter_opaque_types`
    /// to also consider duplicate entries.
    pub(crate) fn iter_lookup_table(
        &self,
    ) -> impl Iterator<Item = (OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)> {
        self.opaque_types.iter().map(|(k, v)| (*k, *v))
    }

    /// Only returns the opaque types which are stored in `duplicate_entries`.
    ///
    /// These have to considered when checking all opaque type uses but are e.g.
    /// irrelevant for canonical inputs as nested queries never meaningfully
    /// accesses them.
    pub(crate) fn iter_duplicate_entries(
        &self,
    ) -> impl Iterator<Item = (OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)> {
        self.duplicate_entries.iter().copied()
    }

    pub(crate) fn iter_opaque_types(
        &self,
    ) -> impl Iterator<Item = (OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)> {
        let OpaqueTypeStorage { opaque_types, duplicate_entries, undo: _, changes: _ } = self;
        opaque_types.iter().map(|(k, v)| (*k, *v)).chain(duplicate_entries.iter().copied())
    }

    #[inline]
    pub(crate) fn table(&mut self, in_snapshot: bool) -> OpaqueTypeTable<'_, 'db> {
        OpaqueTypeTable { storage: self, in_snapshot }
    }
}

pub(crate) struct OpaqueTypeTable<'a, 'db> {
    storage: &'a mut OpaqueTypeStorage<'db>,

    in_snapshot: bool,
}
impl<'db> Deref for OpaqueTypeTable<'_, 'db> {
    type Target = OpaqueTypeStorage<'db>;
    fn deref(&self) -> &Self::Target {
        self.storage
    }
}

impl<'a, 'db> OpaqueTypeTable<'a, 'db> {
    #[instrument(skip(self), level = "debug")]
    pub(crate) fn register(
        &mut self,
        key: OpaqueTypeKey<'db>,
        hidden_type: OpaqueHiddenType<'db>,
    ) -> Option<Ty<'db>> {
        self.storage.changes += 1;
        if let Some(entry) = self.storage.opaque_types.get_mut(&key) {
            let prev = std::mem::replace(entry, hidden_type);
            if self.in_snapshot {
                self.storage.undo.push((key, Some(prev)));
            }
            return Some(prev.ty);
        }
        self.storage.opaque_types.insert(key, hidden_type);
        if self.in_snapshot {
            self.storage.undo.push((key, None));
        }
        None
    }

    pub(crate) fn add_duplicate(
        &mut self,
        key: OpaqueTypeKey<'db>,
        hidden_type: OpaqueHiddenType<'db>,
    ) {
        self.storage.changes += 1;
        self.storage.duplicate_entries.push((key, hidden_type));
    }
}
