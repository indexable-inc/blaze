//! What the reach walk's instance questions (front2/reach/instances.rs) read of the impl index
//! to ask a trait's questions only at the shapes one of its impls can answer: the filed
//! `(trait, shape)` buckets, the shapes holding an impl whose trait path was not read, and the
//! impls filed under no shape. Read only; nothing here files or answers.

use blaze_id::ItemKey as LibKey;
use blaze_typeck::{ItemKey, Shape};

use super::BucketIndex;

impl BucketIndex {
    /// How many `(trait, shape)` buckets are filed: an index a caller builds from
    /// [`Buckets::each_trait_bucket`] is whole while this is unchanged (over [`Buckets::new`]
    /// every impl is filed when the buckets are made; over [`Buckets::lazy`] a question's
    /// [`Buckets::admit`] files more, which is why the caller compares).
    pub(crate) fn trait_buckets(&self) -> usize {
        self.by_shape.len()
    }

    /// Calls `each` with every filed `(trait, shape)` bucket's key, in the map's order (the
    /// caller sorts what it keeps).
    pub(crate) fn each_trait_bucket(&self, mut each: impl FnMut(ItemKey, Shape)) {
        for &(trait_, shape) in self.by_shape.keys() {
            each(trait_, shape);
        }
    }

    /// Calls `each` with every shape holding a trait impl whose trait path was not read
    /// (`unread_by_shape`): a candidate of EVERY trait's question at that shape, so each such
    /// shape is one of every trait's candidate shapes.
    pub(crate) fn each_unread_shape(&self, mut each: impl FnMut(Shape)) {
        for &shape in self.unread_by_shape.keys() {
            each(shape);
        }
    }

    /// The impls filed under no shape that are not blankets by their header's tokens
    /// (`unread_any`, `unread_blind`): a candidate of every question, lowered by the first
    /// question that admits them; once lowered, its header says its trait and self type.
    pub(crate) fn shapeless_unread(&self) -> impl Iterator<Item = LibKey> + '_ {
        self.unread_any.iter().chain(self.unread_blind.iter()).copied()
    }
}
