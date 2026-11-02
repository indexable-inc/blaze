//! Input structs: a row of fields per ID, each field with its own node, whose color is its
//! stamp (the view it last changed in) and durability, always verified. Made by `&` (a
//! singleton, whose absence a query can read, by `&mut`), set only by `&mut` (a setter opens a
//! view of the field's kind).

use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_mem::Tag;

use super::{
    Id, MemoryUsage,
    ingredient::{Ingredient, Kind, Vtable},
    local,
    row::{Pages, layout},
    storage::{Durability, FIELD, Ids, Storage, Table, color, input_bits, node_key, or},
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
    singleton: OnceLock<Id>,
    _of: PhantomData<fn() -> I>,
}

impl<I: Input> Table for InputTable<I> {
    fn new(_: &Storage, _: u32) -> InputTable<I> {
        InputTable {
            pages: Pages::new(Tag::Query),
            ids: Ids::new(Tag::Query),
            singleton: OnceLock::new(),
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
            verify: None,
            reset: None,
            sweep: None,
            relocate: None,
            restamp: None,
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
        storage.set_summary(node, input_bits(node));
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
    local::with(|local| local.read(storage, node, durability, seen as u32));
}

#[cfg(test)]
pub(crate) fn field_node<I: Input>(storage: &Storage, id: Id, field: u32) -> u32 {
    row::<I>(storage, id).first.load(Ordering::Relaxed) + field
}

/// Singleton `I`'s table and ID, reserving the ID and its field nodes on first use: unmade,
/// each field node stamped 0 (never changed) with durability `LOW`.
fn reserved<I: Input>(storage: &Storage) -> (&InputTable<I>, Id) {
    let (table, index) = storage.table_or_make::<InputTable<I>>(I::ingredient());
    let id = *table.singleton.get_or_init(|| {
        let id = table.ids.next(storage, index, &table.pages, &storage.heap, layout::<RowOf<I>>());
        let row = table.pages.row::<RowOf<I>>(id).expect("`Ids::next` made the row's page");
        let first = storage.new_nodes(I::FIELDS.max(1));
        for field in 0..I::FIELDS.max(1) {
            let node = first + field;
            storage.set_key(node, node_key(index, id, field));
            storage.set_summary(node, input_bits(node));
            storage.repaint(node, color(FIELD | u32::from(Durability::LOW.0), 0));
        }
        row.first.store(first, Ordering::Relaxed);
        id
    });
    (table, id)
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
    local::with(|local| local.read(storage, node, durability, seen as u32));
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
    let view = storage.new_view(Durability::LOW, written);
    for (at, node) in (first..end).enumerate() {
        let kind = durability.get(at).copied().unwrap_or(Durability::LOW);
        storage.repaint(node, color(FIELD | u32::from(kind.0), view));
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
pub fn input_set<'s, I: Input, T>(
    storage: &'s mut Storage,
    id: Id,
    field: u32,
    at: fn(&mut I::Fields) -> &mut T,
) -> InputSetter<'s, I, T> {
    assert!(field < I::FIELDS, "{} has no field {field}", I::NAME);
    InputSetter { storage, id, field, durability: None, at }
}

impl<I: Input, T> Setter for InputSetter<'_, I, T> {
    type FieldTy = T;

    fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = Some(durability);
        self
    }

    fn to(self, value: T) -> T {
        let node = row::<I>(self.storage, self.id).first.load(Ordering::Relaxed) + self.field;
        let old = Durability(((self.storage.color_of(node) >> 32) as u32 & !FIELD) as u8);
        let durability = self.durability.unwrap_or(old);
        let view = self.storage.new_view(old.max(durability), input_bits(node));
        let row = row::<I>(self.storage, self.id);
        // SAFETY: `&mut` of the storage: no reference to the fields lives, and nobody else
        // writes them.
        let fields = unsafe { (*row.fields.get()).assume_init_mut() };
        let old_value = std::mem::replace((self.at)(fields), value);
        self.storage.repaint(node, color(FIELD | u32::from(durability.0), view));
        old_value
    }
}

fn memory<I: Input>(storage: &Storage, index: u32) -> MemoryUsage {
    let rows = storage.table::<InputTable<I>>(index).map_or(0, |table| u64::from(table.ids.len()));
    MemoryUsage {
        name: I::NAME,
        rows,
        row_bytes: rows * size_of::<RowOf<I>>() as u64,
        value_bytes: rows * size_of::<I::Fields>() as u64,
    }
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
