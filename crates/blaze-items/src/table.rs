//! Items' long-lived data as columns (docs/v2/plan/design/data.md): a crate's item hashes one
//! column per hash kind, and facts as sorted `(kind, subject, object)` triples.
//!
//! A column is read alone: finding a key reads the key column only, and a fact range scan
//! reads the kind and subject columns only. Hashes that only some items have (a layout, a
//! code hash) sit densely in their own column, found through a presence bitset by rank.
//!
//! Each column is allocated once at its final length. What they are built from (a crate's
//! rows, a fact set's split facts and dictionary) is the building task's, in its frame
//! (`blaze_mem::frame`), so building grows nothing by doubling.

use std::cmp::Ordering;

use base_db::{Db, Crate};
use blaze_ids::{ContentId, bits::Ranked};
use blaze_mem::{Frame, Tag, frame};
use blaze_query::QueryKind;
use hir_ty::next_solver::goal_store::{FactObject, FactRef};

use crate::{
    CodeHash, DeepHash, Fact, InterfaceHash, ItemHashes, ItemKey, ItemKind, LayoutHash, NameHash,
    SimplifiedType, Unhashable, hash, key, kinds,
};

const _: () = {
    assert!(size_of::<ItemKey>() == 32);
    assert!(size_of::<InterfaceHash>() == 32);
    assert!(size_of::<LayoutHash>() == 32);
    assert!(size_of::<CodeHash>() == 32);
    assert!(size_of::<DeepHash>() == 32);
    assert!(size_of::<ItemKind>() == 1);
};

/// The hashes of a set of items, sorted by key, one column per hash kind. The items that have
/// no hash are in a side table, by key with why.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct HashColumns {
    keys: Box<[ItemKey]>,
    kinds: Box<[ItemKind]>,
    interface: Box<[InterfaceHash]>,
    deep: Box<[DeepHash]>,
    has_layout: Ranked,
    layout: Box<[LayoutHash]>,
    has_code: Ranked,
    code: Box<[CodeHash]>,
    unhashable: Box<[(ItemKey, Unhashable)]>,
}

impl HashColumns {
    /// The columns of `rows`, which may come in any order and which it sorts; a key given
    /// twice is a bug in the caller.
    pub fn new(
        rows: &mut [(ItemKey, ItemHashes)],
        unhashable: &mut [(ItemKey, Unhashable)],
    ) -> HashColumns {
        rows.sort_unstable_by_key(|&(key, _)| key);
        unhashable.sort_unstable_by_key(|&(key, _)| key);
        for pair in rows.windows(2) {
            assert_ne!(pair[0].0, pair[1].0, "blaze-items: two rows of one key");
        }
        let has_layout = Ranked::from_bools(rows.iter().map(|(_, it)| it.layout.is_some()));
        let has_code = Ranked::from_bools(rows.iter().map(|(_, it)| it.code.is_some()));
        HashColumns {
            keys: column(rows.iter().map(|&(key, _)| key)),
            kinds: column(rows.iter().map(|(_, it)| it.kind)),
            interface: column(rows.iter().map(|(_, it)| it.interface)),
            deep: column(rows.iter().map(|(_, it)| it.deep)),
            layout: dense(has_layout.count(), rows.iter().filter_map(|(_, it)| it.layout)),
            has_layout,
            code: dense(has_code.count(), rows.iter().filter_map(|(_, it)| it.code)),
            has_code,
            unhashable: Box::from(&*unhashable),
        }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn keys(&self) -> &[ItemKey] {
        &self.keys
    }

    /// The row of `key`, if it has hashes.
    pub fn find(&self, key: ItemKey) -> Option<usize> {
        self.keys.binary_search(&key).ok()
    }

    pub fn get(&self, key: ItemKey) -> Option<ItemHashes> {
        self.find(key).map(|row| self.row(row))
    }

    #[track_caller]
    pub fn row(&self, row: usize) -> ItemHashes {
        ItemHashes {
            kind: self.kinds[row],
            interface: self.interface[row],
            layout: self.has_layout.place(row).map(|at| self.layout[at]),
            code: self.has_code.place(row).map(|at| self.code[at]),
            deep: self.deep[row],
        }
    }

    /// The items that have no hash, sorted by key.
    pub fn unhashable(&self) -> &[(ItemKey, Unhashable)] {
        &self.unhashable
    }

    /// The keys whose hashes differ from `old` to `self`: added, removed or changed, sorted;
    /// one merge over both key columns.
    pub fn changed_since(&self, old: &HashColumns) -> Vec<ItemKey> {
        let (mut a, mut b) = (0, 0);
        // alloc: the caller's list of what changed, once per build.
        let mut changed = Vec::new();
        while a < old.len() || b < self.len() {
            let order = match (old.keys.get(a), self.keys.get(b)) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => Ordering::Less,
                (None, _) => Ordering::Greater,
            };
            match order {
                Ordering::Less => {
                    changed.push(old.keys[a]);
                    a += 1;
                }
                Ordering::Greater => {
                    changed.push(self.keys[b]);
                    b += 1;
                }
                Ordering::Equal => {
                    if old.row(a) != self.row(b) {
                        changed.push(self.keys[b]);
                    }
                    a += 1;
                    b += 1;
                }
            }
        }
        changed
    }
}

/// A memo's column of `values`, allocated once at its final length.
pub(crate) fn column<T>(values: impl ExactSizeIterator<Item = T>) -> Box<[T]> {
    dense(values.len(), values)
}

/// `count` values, the column of the rows that have one.
fn dense<T>(count: usize, values: impl Iterator<Item = T>) -> Box<[T]> {
    // alloc: a memo's column, kept as long as the memo, at the length its rows give.
    let mut column = Vec::with_capacity(count);
    column.extend(values);
    debug_assert_eq!(column.len(), count);
    column.into_boxed_slice()
}

/// The hashes of every item of `krate` with an [`crate::ItemId`], bodies' items included, in
/// one pass: what the store keeps of a crate and diffs against the last build. Its rows are
/// gathered in the task's frame (the hash queries it runs take frames of their own).
#[blaze_query::tracked(returns(ref))]
pub fn crate_hashes(db: &Db, krate: Crate) -> HashColumns {
    frame::task(Tag::Other, |frame| {
        let items = crate_items_by_key(db, krate);
        let mut rows: Vec<(ItemKey, ItemHashes), &Frame> = Vec::with_capacity_in(items.len(), frame);
        let mut unhashable: Vec<(ItemKey, Unhashable), &Frame> = Vec::new_in(frame);
        for &(key, id) in items.iter() {
            match hash::hashes(db, id) {
                Ok(it) => rows.push((key, it)),
                Err(it) => unhashable.push((key, it)),
            }
        }
        // Already in key order: the sort in `new` is one pass that finds it so.
        HashColumns::new(&mut rows, &mut unhashable)
    })
}

/// Every item of `krate` with an [`crate::ItemId`] and its key, sorted by key. An edit that
/// adds, removes, renames or moves no item leaves it as it was, so [`crate_hashes`] rebuilt
/// after a body edit reads no key and sorts nothing.
#[blaze_query::tracked(returns(deref))]
fn crate_items_by_key(db: &Db, krate: Crate) -> Box<[(ItemKey, crate::ItemId)]> {
    let mut items: Vec<(ItemKey, crate::ItemId)> =
        key::crate_item_ids(db, krate).map(|id| (key::item_key(db, id), id)).collect(); // alloc: a memo's column.
    items.sort_unstable_by_key(|&(key, _)| key);
    items.into_boxed_slice()
}

const NO_OBJECT: u32 = u32::MAX;

/// One fact: its kind's offset from [`kinds::SIGNATURE`], a subject and an object, each a
/// position in the set's dictionary of ids, except a field's object, which is its position.
pub const FACT_ROW_BYTES: usize = size_of::<u8>() + 2 * size_of::<u32>();

const _: () = assert!(FACT_ROW_BYTES == 9);

/// The facts a query read, distinct and sorted by `(kind, subject, object)`, as three columns
/// over a sorted dictionary of the ids they name: every fact of one kind about one subject is
/// one range, found by binary search.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct FactSet {
    ids: Box<[ContentId]>,
    kinds: Box<[u8]>,
    subjects: Box<[u32]>,
    objects: Box<[u32]>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Object {
    None,
    Field(u32),
    Id(ContentId),
}

fn tag(kind: QueryKind) -> u8 {
    u8::try_from(kind.0 - kinds::SIGNATURE.0).expect("blaze-items: a fact kind past u8")
}

fn kind_of(tag: u8) -> QueryKind {
    QueryKind(kinds::SIGNATURE.0 + u16::from(tag))
}

fn split(fact: &Fact) -> (u8, ContentId, Object) {
    let tag = tag(fact.kind());
    match *fact {
        Fact::Signature(it)
        | Fact::Layout(it)
        | Fact::Code(it)
        | Fact::ConstValue(it)
        | Fact::Name(it)
        | Fact::Captures(it)
        | Fact::AutoTraits(it)
        | Fact::Coherence(it)
        | Fact::Specializes(it)
        | Fact::DropCheck(it)
        | Fact::Derive(it)
        | Fact::HiddenTypes(it) => (tag, it.0, Object::None),
        Fact::Field { ty, field } => (tag, ty.0, Object::Field(field)),
        Fact::Method { self_ty, name } => (tag, self_ty.0, Object::Id(name.0)),
        Fact::Implements { trait_, self_ty } | Fact::Vtable { trait_, self_ty } => {
            (tag, trait_.0, Object::Id(self_ty.0))
        }
        Fact::Resolves { scope, name } => (tag, scope.0, Object::Id(name.0)),
        Fact::Target(krate) | Fact::Crate(krate) => (tag, krate, Object::None),
    }
}

/// The fact `split` gives these parts of; `None` for a kind that is no fact's, or an object
/// that is not its kind's.
fn joined(tag: u8, subject: ContentId, object: Object) -> Option<Fact> {
    let item = ItemKey(subject);
    let alone = |fact: Fact| matches!(object, Object::None).then_some(fact);
    let id = match object {
        Object::Id(it) => Some(it),
        Object::None | Object::Field(_) => None,
    };
    Some(match kind_of(tag) {
        kinds::SIGNATURE => return alone(Fact::Signature(item)),
        kinds::LAYOUT => return alone(Fact::Layout(item)),
        kinds::FIELD => match object {
            Object::Field(field) => Fact::Field { ty: item, field },
            Object::None | Object::Id(_) => return None,
        },
        kinds::METHOD => Fact::Method { self_ty: SimplifiedType(subject), name: NameHash(id?) },
        kinds::IMPLEMENTS => Fact::Implements { trait_: item, self_ty: SimplifiedType(id?) },
        kinds::RESOLVES => Fact::Resolves { scope: item, name: NameHash(id?) },
        kinds::CODE => return alone(Fact::Code(item)),
        kinds::CONST_VALUE => return alone(Fact::ConstValue(item)),
        kinds::NAME => return alone(Fact::Name(item)),
        kinds::CAPTURES => return alone(Fact::Captures(item)),
        kinds::AUTO_TRAITS => return alone(Fact::AutoTraits(item)),
        kinds::COHERENCE => return alone(Fact::Coherence(item)),
        kinds::SPECIALIZES => return alone(Fact::Specializes(item)),
        kinds::DROP_CHECK => return alone(Fact::DropCheck(item)),
        kinds::VTABLE => Fact::Vtable { trait_: item, self_ty: SimplifiedType(id?) },
        kinds::DERIVE => return alone(Fact::Derive(item)),
        kinds::TARGET => return alone(Fact::Target(subject)),
        kinds::CRATE => return alone(Fact::Crate(subject)),
        kinds::HIDDEN_TYPES => return alone(Fact::HiddenTypes(item)),
        _ => return None,
    })
}

/// A fact of this set, which `split` wrote.
fn join(tag: u8, subject: ContentId, object: Object) -> Fact {
    joined(tag, subject, object)
        .unwrap_or_else(|| unreachable!("fact kind {tag} with {object:?} is no fact"))
}

/// `fact` as hir-ty's goal store keeps it ([`FactRef`]): its kind, subject and object as
/// [`split`] gives them.
pub(crate) fn fact_ref(fact: &Fact) -> FactRef {
    let (_, subject, object) = split(fact);
    let object = match object {
        Object::None => FactObject::None,
        Object::Field(field) => FactObject::Field(field),
        Object::Id(it) => FactObject::Id(it),
    };
    FactRef { kind: fact.kind(), subject, object }
}

/// The fact a [`FactRef`] read from the store names; `None` for a kind or an object that is no
/// fact's: a record is data, which this build may not have written.
pub(crate) fn fact_of_ref(fact: FactRef) -> Option<Fact> {
    let tag = u8::try_from(fact.kind.0.checked_sub(kinds::SIGNATURE.0)?).ok()?;
    let object = match fact.object {
        FactObject::None => Object::None,
        FactObject::Field(field) => Object::Field(field),
        FactObject::Id(it) => Object::Id(it),
    };
    joined(tag, fact.subject, object)
}

impl FromIterator<Fact> for FactSet {
    fn from_iter<I: IntoIterator<Item = Fact>>(facts: I) -> FactSet {
        frame::task(Tag::Other, |frame| {
            let mut rows: Vec<(u8, ContentId, Object), &Frame> = Vec::new_in(frame);
            rows.extend(facts.into_iter().map(|it| split(&it)));
            let mut ids: Vec<ContentId, &Frame> = Vec::with_capacity_in(rows.len() * 2, frame);
            for &(_, subject, object) in rows.iter() {
                ids.push(subject);
                if let Object::Id(it) = object {
                    ids.push(it);
                }
            }
            ids.sort_unstable();
            ids.dedup();
            assert!(
                ids.len() < NO_OBJECT as usize,
                "blaze-items: more than u32::MAX ids in a fact set"
            );
            let at =
                |id: ContentId| ids.binary_search(&id).expect("every id is in the dictionary") as u32;
            // Positions in a sorted dictionary order as the ids do, so sorting the encoded rows
            // sorts the facts by content.
            rows.sort_unstable();
            rows.dedup();
            let kinds = column(rows.iter().map(|&(tag, _, _)| tag));
            let subjects = column(rows.iter().map(|&(_, subject, _)| at(subject)));
            let objects = column(rows.iter().map(|&(_, _, object)| match object {
                Object::None => NO_OBJECT,
                Object::Field(field) => field,
                Object::Id(it) => at(it),
            }));
            FactSet { ids: Box::from(&ids[..]), kinds, subjects, objects }
        })
    }
}

impl FactSet {
    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    fn fact(&self, row: usize) -> Fact {
        let tag = self.kinds[row];
        let subject = self.ids[self.subjects[row] as usize];
        let object = match (kind_of(tag), self.objects[row]) {
            (kinds::FIELD, field) => Object::Field(field),
            (_, NO_OBJECT) => Object::None,
            (_, at) => Object::Id(self.ids[at as usize]),
        };
        join(tag, subject, object)
    }

    /// Every fact, sorted.
    pub fn iter(&self) -> impl Iterator<Item = Fact> + '_ {
        (0..self.len()).map(|row| self.fact(row))
    }

    /// The rows of facts of `kind` about `subject` (a fact's first id: the item, the
    /// simplified self type of a method, the trait of an impl question, the scope of a name, the
    /// crate of a target or of a crate fact).
    fn range(&self, kind: QueryKind, subject: ContentId) -> std::ops::Range<usize> {
        let Ok(subject) = self.ids.binary_search(&subject) else { return 0..0 };
        let at = (tag(kind), subject as u32);
        let start = self.partition(|row| row < at);
        let end = self.partition(|row| row <= at);
        start..end
    }

    fn partition(&self, before: impl Fn((u8, u32)) -> bool) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let mid = low + (high - low) / 2;
            if before((self.kinds[mid], self.subjects[mid])) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    /// Every fact of `kind` about `subject`: what to re-check when that subject changes.
    pub fn about(&self, kind: QueryKind, subject: ContentId) -> impl Iterator<Item = Fact> + '_ {
        self.range(kind, subject).map(|row| self.fact(row))
    }

    pub fn contains(&self, fact: &Fact) -> bool {
        let (_, subject, _) = split(fact);
        self.range(fact.kind(), subject).any(|row| self.fact(row) == *fact)
    }
}
