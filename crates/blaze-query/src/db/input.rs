//! Input structs: a row of fields per ID, each field with its own node, whose color is its
//! stamp (the view it last changed in) and durability, always verified. Made by `&` (a
//! singleton, whose absence a query can read, by `&mut`), set only by `&mut` (a setter opens a
//! view of the field's kind).

use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    sync::atomic::{AtomicU32, Ordering},
};

use blaze_mem::Tag;

use super::{
    Id, MemoryUsage, counts,
    ingredient::{Ingredient, Kind, Vtable},
    local,
    row::{Pages, layout},
    seal::SealGroup,
    storage::{
        Durability, FIELD, Ids, LOOSE, Storage, Table, View, color, input_bits, node_key, or,
    },
};

/// One input struct. Its fields are numbered 0..FIELDS; each has its own stamp and node.
pub trait Input: 'static {
    type Fields: Send + Sync + 'static;
    const NAME: &'static str;
    const FILE: &'static str;
    const FIELDS: u32;
    /// Each field's name, in field order: `FIELDS` of them.
    const FIELD_NAMES: &'static [&'static str];
    /// Made once, by `&mut` ([`input_new_singleton`]); a second panics naming `NAME`.
    const SINGLETON: bool;
    fn ingredient() -> &'static Ingredient;
}

/// What the setter builder is: `with_durability`, then `to`.
pub trait Setter: Sized {
    type FieldTy;
    fn with_durability(self, durability: Durability) -> Self;
    /// Sets the field, returning its old value.
    fn to(self, value: Self::FieldTy) -> Self::FieldTy;
}

/// One input's row. The fields are written once by `input_new` before `made` is stored (with
/// release), then only under `&mut` of the storage.
struct InputRow<F> {
    made: AtomicU32,
    /// The node of field 0; field `f`'s is `first + f`.
    first: AtomicU32,
    fields: UnsafeCell<MaybeUninit<F>>,
}

pub(crate) struct InputTable<I: Input> {
    pages: Pages,
    ids: Ids,
    /// A singleton's ID and field nodes, reserved at the first `try_get` or `new`: before it
    /// is made, its field 0's node is what a read of its absence depends on.
    /// Its bits, 0 until reserved.
    singleton: AtomicU32,
    /// Held while the singleton is reserved, so it is reserved once.
    #[expect(clippy::disallowed_types, reason = "held once, while a singleton is reserved")]
    reserving: blaze_core::lock::Lock<()>,
    _of: PhantomData<fn() -> I>,
}

impl<I: Input> Table for InputTable<I> {
    #[expect(clippy::disallowed_types, reason = "makes the locks its fields allow")]
    fn new(_: &Storage, _: u32) -> InputTable<I> {
        InputTable {
            pages: Pages::new(Tag::Query),
            ids: Ids::new(Tag::Query),
            singleton: AtomicU32::new(0),
            reserving: blaze_core::lock::Lock::new("query.input.reserving", ()),
            _of: PhantomData,
        }
    }

    fn pages(&self) -> &Pages {
        &self.pages
    }
}

type RowOf<I> = InputRow<<I as Input>::Fields>;

impl Vtable {
    pub const fn input<I: Input>() -> Vtable {
        Vtable {
            name: I::NAME,
            file: I::FILE,
            kind: Kind::Input,
            fields: I::FIELD_NAMES,
            row_size: size_of::<RowOf<I>>(),
            ids_of: None,
            made: None,
            memory: memory::<I>,
            used: None,
            values: None,
            verify: None,
            db: None,
            reset: None,
            weighed: None,
            explicit: None,
            unread: None,
            relocate: None,
            restamp: None,
            write_key: None,
            drop: drop_table::<I>,
        }
    }
}

/// Panics unless `id`'s page is ingredient `index`'s: IDs come from persisted `u32`s, and a
/// row is never read as another type's.
#[inline]
#[track_caller]
pub(crate) fn check_owner(storage: &Storage, id: Id, index: u32, name: &str) {
    let owner = storage.owner(id);
    if owner != Some(index) {
        owner_mismatch(storage, id, owner, name);
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn owner_mismatch(storage: &Storage, id: Id, owner: Option<u32>, name: &str) -> ! {
    let owner = owner.map_or("no ingredient", |it| storage.ingredient_name(it));
    panic!("{id:?} is a row of {owner}, read as {name}")
}

fn row<'s, I: Input>(storage: &'s Storage, id: Id) -> &'s RowOf<I> {
    let index = I::ingredient().index();
    check_owner(storage, id, index, I::NAME);
    let row = storage
        .table::<InputTable<I>>(index)
        .and_then(|table| table.pages.row::<RowOf<I>>(id))
        .unwrap_or_else(|| panic!("{}({id:?}) was never made", I::NAME));
    // Acquire: the fields `input_new` wrote before `made`.
    assert!(row.made.load(Ordering::Acquire) != 0, "{}({id:?}) was never made", I::NAME);
    row
}

/// A new input with `fields`, field `f` of durability `durability[f]`.
pub fn input_new<I: Input, const N: usize>(
    storage: &Storage,
    fields: I::Fields,
    durability: [Durability; N],
) -> Id {
    assert_eq!(
        N,
        I::FIELDS as usize,
        "{} has {} fields, given {N} durabilities",
        I::NAME,
        I::FIELDS
    );
    assert!(
        !I::SINGLETON,
        "{} is a singleton: it is made by `&mut` (input_new_singleton)",
        I::NAME
    );
    let (table, index) = storage.table_or_make::<InputTable<I>>(I::ingredient());
    let id = table.ids.next(storage, index, &table.pages, &storage.heap, layout::<RowOf<I>>());
    let row = table.pages.row::<RowOf<I>>(id).expect("`Ids::next` made the row's page");
    let first = storage.new_nodes(I::FIELDS.max(1));
    let view = storage.view_bits();
    for (field, durability) in durability.into_iter().enumerate() {
        let node = first + field as u32;
        storage.set_key(node, node_key(index, id, field as u32));
        // Of no seal group until its host says which ([`input_seal_group`]).
        storage.set_summary(node, or(input_bits(node), LOOSE));
        storage.repaint(node, color(FIELD | u32::from(durability.0), view));
    }
    // SAFETY: the ID is fresh (`Ids::next` gives each once), so this task is the row's one
    // writer, and no reader reads it before `made` is stored below.
    unsafe { (*row.fields.get()).write(fields) };
    row.first.store(first, Ordering::Relaxed);
    // Release: the fields and nodes are written before a reader sees the row made.
    row.made.store(1, Ordering::Release);
    id
}

/// The fields of input `id`, recording no edge (the macro's getter calls [`input_read`]).
pub fn input_fields<'s, I: Input>(storage: &'s Storage, id: Id) -> &'s I::Fields {
    let row = row::<I>(storage, id);
    // SAFETY: `row` checked `made` with acquire, so the fields are written; they change only
    // under `&mut` of the storage, which `'s` excludes.
    unsafe { (*row.fields.get()).assume_init_ref() }
}

/// Records a read of `field` of input `id` into the running query.
#[inline]
pub fn input_read<I: Input>(storage: &Storage, id: Id, field: u32) {
    debug_assert!(field < I::FIELDS);
    let node = row::<I>(storage, id).first.load(Ordering::Relaxed) + field;
    let seen = storage.color_of(node);
    let durability = ((seen >> 32) as u32 & !FIELD) as u8;
    let note = local::with(|local| local.read(storage, node, durability, seen as u32));
    super::noted(storage, note);
}

/// Whether a field of input `id` was written, or the input made, in a view after `since`, a
/// view the caller read from [`Storage::view`]: each field's node holds the view it last
/// changed in. For a host that keeps something made from one input's fields alone (a crate's
/// sources, say) and asks, a view later, whether to make it again, with no row verified and
/// no edge walked: a load a field. Exact, or `true` where the storage can no longer say (its
/// views were renumbered since `since`). Records no read: for a caller outside any query.
pub fn input_written_since<I: Input>(storage: &Storage, id: Id, since: View) -> bool {
    let Some(stamp) = storage.stamp_of(since) else { return true };
    let first = row::<I>(storage, id).first.load(Ordering::Relaxed);
    // The low word of a field node's color is the view it last changed in (`color`).
    (0..I::FIELDS.max(1)).any(|field| storage.color_of(first + field) as u32 > stamp)
}

/// Says every field of input `id` is an input of seal group `group` ([`super::seal`]): a write
/// of one opens that group and no other, found from the field with no walk of rows. For the
/// host that makes the input, before a query reads it (a row that read it before stays as it
/// was: of no group, verified row by row). An input of another group moves: it is removed
/// from that group and added to this one (`Storage::seal_input`).
pub fn input_seal_group<I: Input>(storage: &mut Storage, id: Id, group: SealGroup) {
    let first = row::<I>(storage, id).first.load(Ordering::Relaxed);
    for field in 0..I::FIELDS.max(1) {
        storage.seal_input(first + field, group, I::NAME);
    }
}

/// [`input_seal_group`] for the singleton `I`, made or not: its absence is read as its field
/// 0, which is the group's input too.
pub fn input_singleton_seal_group<I: Input>(storage: &mut Storage, group: SealGroup) {
    let (table, id) = reserved::<I>(storage);
    let first = table
        .pages
        .row::<RowOf<I>>(id)
        .expect("the reserved row's page")
        .first
        .load(Ordering::Relaxed);
    for field in 0..I::FIELDS.max(1) {
        storage.seal_input(first + field, group, I::NAME);
    }
}

#[cfg(test)]
pub(crate) fn field_node<I: Input>(storage: &Storage, id: Id, field: u32) -> u32 {
    row::<I>(storage, id).first.load(Ordering::Relaxed) + field
}

/// Singleton `I`'s table and ID, reserving the ID and its field nodes on first use: unmade,
/// each field node stamped 0 (never changed) with durability `HIGH`.
///
/// Why `HIGH`. A query that asks for a singleton nobody made reads its absence, and a row is
/// as durable as the least durable thing it read. At `LOW` every row that asked for an
/// unmade singleton was `LOW` itself, the standard library's rows among them (measured
/// 2026-11-11 on a hello-world: `ServedBodies` and `FrozenImpls` were never made, and each
/// body edit walked the edges of 1,091,174 rows, `interface_of` alone 428,529, that a write
/// of a workspace file cannot change). An absence changes only by the singleton being made,
/// which opens a view of kind `HIGH` ([`input_new_singleton`]), so every row that read the
/// absence is looked at again then, whatever its durability.
fn reserved<I: Input>(storage: &Storage) -> (&InputTable<I>, Id) {
    let (table, index) = storage.table_or_make::<InputTable<I>>(I::ingredient());
    // Acquire: the row and field nodes `reserve` made before it stored the ID.
    let id = match table.singleton.load(Ordering::Acquire) {
        0 => reserve::<I>(storage, table, index),
        bits => Id::from_u32(bits),
    };
    (table, id)
}

#[cold]
#[inline(never)]
fn reserve<I: Input>(storage: &Storage, table: &InputTable<I>, index: u32) -> Id {
    let _one = table.reserving.lock();
    if let bits @ 1.. = table.singleton.load(Ordering::Acquire) {
        return Id::from_u32(bits);
    }
    let id = table.ids.next(storage, index, &table.pages, &storage.heap, layout::<RowOf<I>>());
    let row = table.pages.row::<RowOf<I>>(id).expect("`Ids::next` made the row's page");
    let first = storage.new_nodes(I::FIELDS.max(1));
    for field in 0..I::FIELDS.max(1) {
        let node = first + field;
        storage.set_key(node, node_key(index, id, field));
        // Of no seal group until its host says which ([`input_seal_group`]).
        storage.set_summary(node, or(input_bits(node), LOOSE));
        storage.repaint(node, color(FIELD | u32::from(Durability::HIGH.0), 0));
    }
    row.first.store(first, Ordering::Relaxed);
    // Release: a reader that loads the ID sees its row and field nodes.
    table.singleton.store(id.as_u32(), Ordering::Release);
    id
}

/// The singleton's ID, if made. Its absence is a read of its field 0's node, which making it
/// changes ([`input_new_singleton`]).
pub fn input_singleton<I: Input>(storage: &Storage) -> Option<Id> {
    let (table, id) = reserved::<I>(storage);
    let row = table.pages.row::<RowOf<I>>(id).expect("the reserved row's page");
    // Acquire: the fields `input_new_singleton` wrote before `made`.
    if row.made.load(Ordering::Acquire) != 0 {
        return Some(id);
    }
    let node = row.first.load(Ordering::Relaxed);
    let seen = storage.color_of(node);
    let durability = ((seen >> 32) as u32 & !FIELD) as u8;
    let note = local::with(|local| local.read(storage, node, durability, seen as u32));
    super::noted(storage, note);
    None
}

/// Makes singleton `I` with `fields`, field `f` of durability `durability[f]`: a write, whose
/// view repaints its field nodes, so every query that read its absence runs again.
pub fn input_new_singleton<I: Input, const N: usize>(
    storage: &mut Storage,
    fields: I::Fields,
    durability: [Durability; N],
) -> Id {
    assert!(I::SINGLETON, "{} is not a singleton", I::NAME);
    assert_eq!(
        N,
        I::FIELDS as usize,
        "{} has {} fields, given {N} durabilities",
        I::NAME,
        I::FIELDS
    );
    let (table, id) = reserved::<I>(storage);
    let row = table.pages.row::<RowOf<I>>(id).expect("the reserved row's page");
    assert!(
        row.made.load(Ordering::Relaxed) == 0,
        "{} is a singleton, made a second time",
        I::NAME
    );
    let first = row.first.load(Ordering::Relaxed);
    let end = first + I::FIELDS.max(1);
    let written = (first..end).fold([0; 2], |acc, node| or(acc, input_bits(node)));
    // Of kind `HIGH`: its absence was read at `HIGH` ([`reserved`]), so making it is a change
    // no row's durability excuses it from looking at.
    let view = storage.new_view(Durability::HIGH, written);
    for (at, node) in (first..end).enumerate() {
        let kind = durability.get(at).copied().unwrap_or(Durability::LOW);
        storage.repaint(node, color(FIELD | u32::from(kind.0), view));
        storage.note_written(node);
    }
    let (table, _) = reserved::<I>(storage);
    let row = table.pages.row::<RowOf<I>>(id).expect("the reserved row's page");
    // SAFETY: `&mut` of the storage, so no reader; the row is unmade, so its fields were
    // never written.
    unsafe { (*row.fields.get()).write(fields) };
    // Release: the fields and nodes are written before a reader sees the row made.
    row.made.store(1, Ordering::Release);
    id
}

/// Sets one field of an input: `with_durability`, then `to`.
pub struct InputSetter<'s, I: Input, T> {
    storage: &'s mut Storage,
    id: Id,
    field: u32,
    durability: Option<Durability>,
    at: fn(&mut I::Fields) -> &mut T,
}

/// The setter of `field` of input `id`, which `at` finds in its fields.
pub fn input_set<'s, I: Input, T: PartialEq>(
    storage: &'s mut Storage,
    id: Id,
    field: u32,
    at: fn(&mut I::Fields) -> &mut T,
) -> InputSetter<'s, I, T> {
    assert!(field < I::FIELDS, "{} has no field {field}", I::NAME);
    InputSetter { storage, id, field, durability: None, at }
}

impl<I: Input, T: PartialEq> Setter for InputSetter<'_, I, T> {
    type FieldTy = T;

    fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = Some(durability);
        self
    }

    /// The invariant: a set of the value the field holds, at the durability it has, writes
    /// nothing: no view opens, no row verifies again, nothing is counted as written. So a
    /// host that gives an input again what it gave before (a request's environment, a crate's
    /// proc macros loaded again, a build script's run applied again) costs nothing, whatever
    /// the database holds: a request that changes nothing writes no input. Until 2026-11-12
    /// every set opened a view of the field's durability, and each caller had to compare
    /// first; the one that did not (`ProcMacros::give`, at `HIGH`) made a build of reth with
    /// nothing changed verify 23 million rows.
    fn to(self, value: T) -> T {
        let node = row::<I>(self.storage, self.id).first.load(Ordering::Relaxed) + self.field;
        let old =
            Durability(((*self.storage.node_mut(node).color_mut() >> 32) as u32 & !FIELD) as u8);
        let durability = self.durability.unwrap_or(old);
        if durability == old && !self.storage.planted_equal_sets_write() {
            let row = row::<I>(self.storage, self.id);
            // SAFETY: `&mut` of the storage: no reference to the fields lives, and nobody
            // else writes them.
            let fields = unsafe { (*row.fields.get()).assume_init_mut() };
            let held = (self.at)(fields);
            if *held == value {
                // The field takes the value given and the one it held is returned, as a
                // write does (a caller that takes a value out to edit it in place and set
                // it back gets the very one it set): equal, so nothing that read it differs.
                return std::mem::replace(held, value);
            }
        }
        let view = self.storage.new_view(old.max(durability), input_bits(node));
        let row = row::<I>(self.storage, self.id);
        // SAFETY: `&mut` of the storage: no reference to the fields lives, and nobody else
        // writes them.
        let fields = unsafe { (*row.fields.get()).assume_init_mut() };
        let old_value = std::mem::replace((self.at)(fields), value);
        *self.storage.node_mut(node).color_mut() = color(FIELD | u32::from(durability.0), view);
        self.storage.note_written(node);
        // Counted per input, so a build's `query_counts` name what it set: the cause of every
        // run after it (harness/incremental.py reads them).
        counts::count(I::ingredient().index(), counts::WRITTEN);
        old_value
    }
}

fn memory<I: Input>(storage: &Storage, index: u32) -> MemoryUsage {
    let rows = storage.table::<InputTable<I>>(index).map_or(0, |table| u64::from(table.ids.len()));
    // The fields are in place in the rows; what they hold beside is not counted.
    let row_bytes = rows * size_of::<RowOf<I>>() as u64;
    MemoryUsage { name: I::NAME, rows, used: rows, row_bytes, value_bytes: 0 }
}

fn drop_table<I: Input>(storage: &mut Storage, index: u32) {
    let Some(table) = storage.take_table::<InputTable<I>>(index) else { return };
    for id in table.ids.all() {
        if let Some(row) = table.pages.row::<RowOf<I>>(id)
            && row.made.load(Ordering::Acquire) != 0
        {
            // SAFETY: the storage is being dropped: nothing reads the fields.
            unsafe { (*row.fields.get()).assume_init_drop() };
        }
    }
}
