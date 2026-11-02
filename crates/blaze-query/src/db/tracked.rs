//! Tracked structs: an interned identity `(creator, occurrence, identity fields)` and, beside
//! it, the `#[tracked]` fields, each with a node whose color is its stamp. A rerun of the
//! creator that makes the same identity again finds the same ID and rewrites the fields in
//! place, bumping each changed field's stamp.
//!
//! The one unsafe concern is the fields' cell, written while `&Storage` is shared. Sound by
//! read order: a reader reaches a tracked struct's ID only through a value that depends on its
//! creator, which it brings up to the view (rerunning it, so rewriting the fields) before it
//! reads that value; so no reference to the fields of this view is alive while the creator's
//! run (the one writer, in its flight) rewrites them, and earlier views' ended under `&mut`.
//! A fixpoint rewrites them between rounds, when every frame of the last round returned.

use std::{cell::UnsafeCell, hash::Hash};

use super::{
    DatabaseKeyIndex, Id, MemoryUsage, function,
    ingredient::{Ingredient, Kind, Vtable},
    input::check_owner,
    interned::{self, hash_of, intern_keyed},
    local,
    storage::{ALL, Durability, FIELD, Storage, color, node_key, or},
};

/// One tracked struct: its untracked fields are its identity, its `#[tracked]` ones are
/// numbered 0..TRACKED_FIELDS.
pub trait Tracked: 'static {
    type Identity: Eq + Hash + Send + Sync + 'static;
    type Fields: Send + Sync + 'static;
    const NAME: &'static str;
    const FILE: &'static str;
    const TRACKED_FIELDS: u32;
    /// Each `#[tracked]` field's name, in their numbers' order: `TRACKED_FIELDS` of them.
    const TRACKED_FIELD_NAMES: &'static [&'static str];
    fn ingredient() -> &'static Ingredient;
    /// `false` for a `#[no_eq]` field.
    fn equal_field(field: u32, old: &Self::Fields, new: &Self::Fields) -> bool;
}

pub(crate) struct TrackedRow<T: Tracked> {
    creator: DatabaseKeyIndex,
    occurrence: u32,
    identity: T::Identity,
    /// The node of tracked field 0; field `f`'s is `first + f`.
    first: u32,
    fields: UnsafeCell<T::Fields>,
}

// SAFETY: `fields` is written only by the creator's run, when no reference to it lives (the
// module's read-order rule); everything else is immutable once interned.
unsafe impl<T: Tracked> Sync for TrackedRow<T> {}

impl Vtable {
    pub const fn tracked<T: Tracked>() -> Vtable {
        Vtable {
            name: T::NAME,
            file: T::FILE,
            kind: Kind::Tracked,
            fields: T::TRACKED_FIELD_NAMES,
            row_size: 0,
            ids_of: None,
            made: None,
            memory: memory::<T>,
            verify: None,
            reset: None,
            sweep: None,
            relocate: None,
            restamp: None,
            drop: drop_table::<T>,
        }
    }
}

/// The tracked struct of `identity` the running query makes (its `occurrence`-th of that
/// identity), with `fields`: new, or the same ID as its last run's, its fields rewritten.
///
/// # Panics
///
/// Outside a tracked function.
pub fn tracked_new<T: Tracked>(storage: &Storage, identity: T::Identity, fields: T::Fields) -> Id {
    let index = T::ingredient().index();
    let creator = function::running(storage)
        .unwrap_or_else(|| panic!("{} made outside a tracked function", T::NAME));
    let identity_hash = local::Local::identity_hash(&identity);
    let (occurrence, durability, summary) = local::with(|local| {
        let occurrence = local.occurrence(index, identity_hash).expect("a running query");
        let frame = local.recording(storage).expect("a running query");
        frame.unshareable = true;
        let (durability, untracked, start, replayed) =
            (frame.durability, frame.untracked, frame.edges_start as usize, frame.summary);
        let summary = if untracked {
            ALL
        } else {
            or(replayed, storage.summary_of_edges(&local.edges[start..]))
        };
        (occurrence, Durability::from_bits(durability), summary)
    });
    let hash = hash_of(&(creator, occurrence, identity_hash));
    let view = storage.view_bits();
    let (id, found) = intern_keyed::<TrackedRow<T>, _>(
        storage,
        T::ingredient(),
        hash,
        (identity, fields),
        |row, (identity, _)| {
            row.creator == creator && row.occurrence == occurrence && row.identity == *identity
        },
        |(identity, fields)| TrackedRow {
            creator,
            occurrence,
            identity,
            first: storage.new_nodes(T::TRACKED_FIELDS.max(1)),
            fields: UnsafeCell::new(fields),
        },
    );
    let row = interned::fields_of::<TrackedRow<T>>(storage, index, id).expect("just interned");
    match found.map(|(_, fields)| fields) {
        None => {
            for field in 0..T::TRACKED_FIELDS {
                let node = row.first + field;
                storage.set_key(node, node_key(index, id, field));
                storage.set_summary(node, summary);
                storage.repaint(node, color(FIELD | u32::from(durability.0), view));
            }
        }
        Some(new) => {
            // SAFETY: the creator's run is the one writer, and no reference to the fields
            // lives (the module's read-order rule).
            let old = unsafe { &mut *row.fields.get() };
            for field in 0..T::TRACKED_FIELDS {
                let node = row.first + field;
                storage.set_summary(node, summary);
                let was = Durability((((storage.color_of(node) >> 32) as u32) & !FIELD) as u8);
                if was != durability || !T::equal_field(field, old, &new) {
                    storage.repaint(node, color(FIELD | u32::from(durability.0), view));
                }
            }
            *old = new;
        }
    }
    id
}

fn row<'s, T: Tracked>(storage: &'s Storage, id: Id) -> &'s TrackedRow<T> {
    let index = T::ingredient().index();
    check_owner(storage, id, index, T::NAME);
    interned::fields_of::<TrackedRow<T>>(storage, index, id)
        .unwrap_or_else(|| panic!("{}({id:?}) was never made", T::NAME))
}

/// The identity (untracked fields) of tracked struct `id`: no edge.
pub fn tracked_identity<'s, T: Tracked>(storage: &'s Storage, id: Id) -> &'s T::Identity {
    &row::<T>(storage, id).identity
}

/// The tracked fields of `id`, recording a read of field `field`.
pub fn tracked_fields<'s, T: Tracked>(storage: &'s Storage, id: Id, field: u32) -> &'s T::Fields {
    assert!(field < T::TRACKED_FIELDS, "{} has no tracked field {field}", T::NAME);
    let row = row::<T>(storage, id);
    let node = row.first + field;
    let seen = storage.color_of(node);
    let durability = ((seen >> 32) as u32 & !FIELD) as u8;
    local::with(|local| local.read(storage, node, durability, seen as u32));
    // SAFETY: the reader reached `id` through its creator, current in this view (the module's
    // read-order rule): its run will not rewrite the fields while `'s` lives.
    unsafe { &*row.fields.get() }
}

fn memory<T: Tracked>(storage: &Storage, index: u32) -> MemoryUsage {
    interned::memory_of::<TrackedRow<T>>(storage, index, T::NAME)
}

fn drop_table<T: Tracked>(storage: &mut Storage, index: u32) {
    interned::drop_rows::<TrackedRow<T>>(storage, index);
}
