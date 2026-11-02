//! [`Mapped`]: an interner of `[T]` values of a plain record whose `u32` IDs mean the same in
//! every process that restores one image, as [`crate::Symbol`]'s do: what an `Interned<T>`
//! becomes when a persisted value names it (docs/v2/plan/design/restore.md, "IDs are stable
//! across restarts"). A value restored from the image is read in place; one made by this
//! process sits in the table's overlay. Neither is freed while the process runs; the image's
//! GC ([`Mapped::write`]) keeps the ones the other sections still name, each at its own ID.
//!
//! A value refers to another (a path to its segments' symbols, a type to its parts) by ID, so
//! it is plain bytes and needs no fixing up when it is mapped.

use std::{fmt, hash, marker::PhantomData, num::NonZeroU32, sync::Arc, sync::OnceLock};

use blaze_format::{Record, SchemaHash};
use blaze_ids::Hasher;
use blaze_image::{Builder, Image, SectionId, Stable, bytes_of, cast};

use crate::table::{Jobs, LAYOUT, Live, MAX_ALIGN, ROOT, RestoreError, Shape, Table};

/// The interner of one kind of value: a `static`, its section [`SectionId::interned`]`(tag)`.
pub struct Mapped<T> {
    tag: u16,
    table: OnceLock<Table>,
    values: PhantomData<fn() -> T>,
}

/// A value of a [`Mapped`]: equal exactly when the values are.
pub struct Id<T> {
    id: NonZeroU32,
    values: PhantomData<fn() -> T>,
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Id<T> {
        *self
    }
}

impl<T> Copy for Id<T> {}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Id<T>) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for Id<T> {}

impl<T> hash::Hash for Id<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<T> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.id)
    }
}

impl<T> Id<T> {
    /// The ID a persisted value stores.
    #[inline]
    pub fn id(self) -> u32 {
        self.id.get()
    }
}

impl<T: Record> Mapped<T> {
    pub const fn new(tag: u16) -> Mapped<T> {
        const { assert!(T::PLAIN, "a mapped value is a plain record, read in place") };
        const { assert!(size_of::<T>() > 0 && align_of::<T>() <= MAX_ALIGN) };
        Mapped { tag, table: OnceLock::new(), values: PhantomData }
    }

    fn shape() -> Shape {
        Shape {
            statics: 0,
            static_bytes: |_| &[],
            key: crate::order_key,
            align: align_of::<T>(),
            size: size_of::<T>(),
            text: false,
        }
    }

    #[inline]
    fn table(&'static self) -> &'static Table {
        self.table.get_or_init(|| Table::new(Self::shape()))
    }

    /// Where the image holds this interner's values.
    pub const fn section(&self) -> SectionId {
        SectionId::interned(self.tag)
    }

    /// The section's schema: the table's layout and `T`'s.
    pub fn schema(&self) -> SchemaHash {
        let mut hasher = Hasher::new("blaze intern mapped");
        hasher.update(LAYOUT.as_bytes()).update(&ROOT.0.0).update(&T::SCHEMA.0.0);
        SchemaHash(hasher.finish())
    }

    /// The ID of `values`, made if it has none.
    pub fn intern(&'static self, values: &[T]) -> Id<T> {
        Self::wrap(self.table().intern(bytes_of(values)))
    }

    /// The ID of `values`, if one was made or restored.
    pub fn find(&'static self, values: &[T]) -> Option<Id<T>> {
        self.table().find(bytes_of(values)).map(Self::wrap)
    }

    /// The ID of a stored `id`, `None` when it names no value in this process.
    pub fn from_id(&'static self, id: u32) -> Option<Id<T>> {
        self.table().get(id)?;
        NonZeroU32::new(id).map(|id| Id { id, values: PhantomData })
    }

    /// The values of `id`, read in place.
    #[inline]
    pub fn get(&'static self, id: Id<T>) -> &'static [T] {
        let bytes = self.table().bytes(id.id());
        cast(bytes).expect("a table keeps each value aligned and a whole number of records")
    }

    /// Makes the values of `image`'s section this interner's, read in place. Must run before
    /// any value is interned; the image stays mapped until the process exits. On an error no
    /// value was restored, so no section naming these IDs may be used.
    pub fn restore<S: Stable>(&'static self, image: Arc<Image<S>>, jobs: Jobs<'_>) -> Result<(), RestoreError> {
        // alloc: once a restore, so the image outlives every ID read from it
        let image: &'static Arc<Image<S>> = Box::leak(Box::new(image));
        Table::restore_into(&self.table, Self::shape(), image, self.section(), self.schema(), jobs)
    }

    /// Writes the section into `builder`: every value `live` marks at its ID, every other ID
    /// free for the next process.
    pub fn write(&'static self, live: &Live, builder: &mut Builder) {
        let mut out = builder.section(self.section(), self.schema());
        self.table().write(live, &mut out);
    }

    fn wrap(id: u32) -> Id<T> {
        Id { id: NonZeroU32::new(id).expect("the table never gives ID 0"), values: PhantomData }
    }
}

impl Live {
    /// Keeps `id`'s value in the next image of its [`Mapped`].
    #[inline]
    pub fn mark_value<T>(&mut self, id: Id<T>) {
        self.mark_id(id.id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_of<T: Record>(mapped: &'static Mapped<T>, live: &Live) -> Arc<Image<Vec<u8>>> {
        let mut builder = Builder::new();
        mapped.write(live, &mut builder);
        Arc::new(Image::open(builder.finish().unwrap()).unwrap())
    }

    #[test]
    fn values_keep_their_ids_across_an_image() {
        static PATHS: Mapped<u32> = Mapped::new(1);
        static RESTORED: Mapped<u32> = Mapped::new(1);
        let values: Vec<Vec<u32>> = (0..500u32).map(|n| (0..n % 7).map(|k| n * 31 + k).collect()).collect();
        let ids: Vec<Id<u32>> = values.iter().map(|it| PATHS.intern(it)).collect();
        for (value, &id) in values.iter().zip(&ids) {
            assert_eq!(PATHS.get(id), &value[..]);
            assert_eq!(PATHS.intern(value), id);
        }
        let mut live = Live::new();
        ids.iter().step_by(3).for_each(|&id| live.mark_value(id));
        RESTORED.restore(image_of(&PATHS, &live), &crate::serial).unwrap();
        for (value, &id) in values.iter().zip(&ids) {
            match live.contains(id.id()) {
                true => {
                    assert_eq!(RESTORED.get(RESTORED.from_id(id.id()).unwrap()), &value[..]);
                    assert_eq!(RESTORED.find(value), Some(id));
                }
                false => assert_eq!(RESTORED.from_id(id.id()), None),
            }
        }
        let new = RESTORED.intern(&[u32::MAX; 3]);
        assert!(!live.contains(new.id()), "a new value never takes a live ID");
        assert_eq!(RESTORED.restore(image_of(&PATHS, &live), &crate::serial), Err(RestoreError::Started { section: RESTORED.section() }));
    }

    #[test]
    fn values_are_read_aligned() {
        static WIDE: Mapped<u128> = Mapped::new(2);
        static RESTORED: Mapped<u128> = Mapped::new(2);
        let ids: Vec<Id<u128>> = (0..64u128).map(|n| WIDE.intern(&[n << 100, n])).collect();
        let mut live = Live::new();
        ids.iter().for_each(|&id| live.mark_value(id));
        RESTORED.restore(image_of(&WIDE, &live), &crate::serial).unwrap();
        for (n, &id) in ids.iter().enumerate() {
            let value = RESTORED.get(id);
            assert!((value.as_ptr() as usize).is_multiple_of(align_of::<u128>()));
            assert_eq!(value, &[(n as u128) << 100, n as u128]);
            assert!((WIDE.get(id).as_ptr() as usize).is_multiple_of(align_of::<u128>()));
        }
    }

    #[test]
    fn a_section_of_another_type_is_refused() {
        static WORDS: Mapped<u32> = Mapped::new(3);
        static SIGNED: Mapped<i32> = Mapped::new(3);
        WORDS.intern(&[1, 2, 3]);
        let refused = SIGNED.restore(image_of(&WORDS, &Live::new()), &crate::serial);
        assert!(matches!(refused, Err(RestoreError::Refused(blaze_image::Refused::Schema { .. }))));
        assert_eq!(SIGNED.section(), SectionId::interned(3));
    }
}
