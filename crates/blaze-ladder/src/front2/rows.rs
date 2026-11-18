//! [`Rows`]: the heads store's tables (`Store::fns`, `adts`, `impls`, ..): keyed rows that are
//! written once, never moved and never dropped before the table, inserted through `&self`.
//!
//! Why (lane VD, 2026-11-17): a body's check borrows its answers from the store for the whole
//! check (`World::fn_sig` hands out `&f.inputs`, `World::adt` the variants, ..). For the check
//! to make a head it meets on the spot instead of ending the round (a round re-runs the body's
//! whole inference), a row filed during the check must not move a row an earlier answer still
//! points into. A `HashMap<K, V>` moves its values when it grows; these tables keep each value
//! in an [`AppendVec`] (address space reserved up front, pages committed as it grows, never
//! moved) and the map holds only the row's index.
//!
//! What makes it sound, with no `unsafe` here: every value is a published element of the
//! `AppendVec`, which is written once, never written again through `&self`, and dropped only
//! when the table is (blaze-mem append.rs states and checks that). A reader's `&V` is to that
//! element, never to the index map's buckets, so neither a later insert (which may grow the
//! map), a replacement (a new row; the index moves to it) nor a removal (the key leaves the
//! index; the row stays) can move or free what an answer holds. The index map is behind a
//! `RefCell` borrowed only inside each method, never across a return.
//!
//! Iteration is in the order the live rows were filed: every caller sorts (rule 25), as it
//! did over the map's order.

use core::cell::{OnceCell, RefCell};
use core::hash::Hash;
use std::alloc::Global;

use blaze_hash::HashMap;
use blaze_mem::{AppendVec, Tag};

/// The most rows one table holds: address space only (pages are committed as rows are filed).
/// A table past it stops naming its tag, as memory that cannot be had does.
const MOST_ROWS: usize = 1 << 22;

/// Keyed rows, filed through `&self`, each at an address that holds until the table drops.
pub struct Rows<K, V> {
    /// Every row ever filed, with its key; reserved at the first insert (a store whose table
    /// stays empty maps nothing).
    rows: OnceCell<AppendVec<(K, V)>>,
    /// Each live key's row.
    at: RefCell<HashMap<K, u32, Global>>,
}

impl<K: Copy + Eq + Hash, V> Rows<K, V> {
    /// An empty table; its index in `alloc` (the rows are in their own reserved range).
    pub fn new_in(alloc: Global) -> Rows<K, V> {
        Rows { rows: OnceCell::new(), at: RefCell::new(HashMap::new_in(alloc)) }
    }

    fn filed(&self) -> &[(K, V)] {
        self.rows.get().map_or(&[], AppendVec::as_slice)
    }

    fn row(&self, i: u32) -> Option<&(K, V)> {
        self.filed().get(i as usize)
    }

    /// The live row of `k`.
    pub fn get(&self, k: &K) -> Option<&V> {
        let i = *self.at.borrow().get(k)?;
        self.row(i).map(|(_, v)| v)
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.at.borrow().contains_key(k)
    }

    /// Files `v` as `k`'s row: a new row, so every reference to the row it replaces (returned)
    /// still reads what it read.
    pub fn insert(&self, k: K, v: V) -> Option<&V> {
        let rows = self.rows.get_or_init(|| match AppendVec::new(MOST_ROWS, Tag::Infer) {
            Ok(rows) => rows,
            Err(error) => panic!("the heads store's rows could not be reserved: {error}"),
        });
        let i = rows.append((k, v));
        let old = self.at.borrow_mut().insert(k, i)?;
        self.row(old).map(|(_, v)| v)
    }

    /// Files `v` as `k`'s row unless `k` has one already, which stays (a map's
    /// `entry(k).or_insert(v)`); returns the live row.
    pub fn insert_absent(&self, k: K, v: V) -> Option<&V> {
        if !self.contains_key(&k) {
            self.insert(k, v);
        }
        self.get(&k)
    }

    /// Takes `k` out of the index: its row stays where it is (an answer may hold it), and is
    /// returned.
    pub fn remove(&self, k: &K) -> Option<&V> {
        let old = self.at.borrow_mut().remove(k)?;
        self.row(old).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.at.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The live rows, in the order filed; rows filed while iterating are not seen.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.filed().iter().enumerate().filter(move |(i, (k, _))| self.at.borrow().get(k) == Some(&(*i as u32))).map(|(_, (k, v))| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> + '_ {
        self.iter().map(|(_, v)| v)
    }
}

/// `rows[&k]`, as a map's: the caller has checked `k` is filed; a key that is not stops naming it.
impl<K: Copy + Eq + Hash + core::fmt::Debug, V> core::ops::Index<&K> for Rows<K, V> {
    type Output = V;

    #[track_caller]
    fn index(&self, k: &K) -> &V {
        match self.get(k) {
            Some(v) => v,
            None => panic!("no row is filed for {k:?}"),
        }
    }
}

impl<K: Copy + Eq + Hash + core::fmt::Debug, V: core::fmt::Debug> core::fmt::Debug for Rows<K, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Copy + Eq + Hash, V> Default for Rows<K, V> {
    fn default() -> Rows<K, V> {
        Rows::new_in(Global)
    }
}

/// The most entries one [`StableColumn`] holds: address space only.
const MOST_ENTRIES: usize = 1 << 26;

/// A type table's column (blaze-typeck `Stable`, the `&self` form of `PortableTys`): an
/// [`AppendVec`] reserved at its first entry (a body's table that interns nothing maps
/// nothing), so nothing it holds moves while a view of it is held. Past [`MOST_ENTRIES`], or
/// memory that cannot be had, is `Full`.
pub struct StableColumn<T> {
    cells: OnceCell<AppendVec<T>>,
}

impl<T> StableColumn<T> {
    pub const fn new() -> StableColumn<T> {
        StableColumn { cells: OnceCell::new() }
    }
}

impl<T> Default for StableColumn<T> {
    fn default() -> StableColumn<T> {
        StableColumn::new()
    }
}

impl<T: Copy> blaze_typeck::Stable<T> for StableColumn<T> {
    fn extend(&self, values: &[T]) -> Result<u32, blaze_typeck::Full> {
        let cells = match self.cells.get() {
            Some(c) => c,
            None => {
                let made = AppendVec::new(MOST_ENTRIES, Tag::Infer).map_err(|_| blaze_typeck::Full)?;
                self.cells.get_or_init(|| made)
            }
        };
        cells.try_extend_from_slice(values).map_err(|_| blaze_typeck::Full)
    }

    fn as_slice(&self) -> &[T] {
        self.cells.get().map_or(&[], AppendVec::as_slice)
    }
}

/// A type table over [`StableColumn`]s in `alloc` (blaze-typeck `PortableTys::new_stable`):
/// interned through `&self` too.
pub fn stable_tys(alloc: blaze_own::Alloc<'_>) -> Result<blaze_typeck::PortableTys<'_>, blaze_typeck::CheckError> {
    let nodes: blaze_own::Box<dyn blaze_typeck::Stable<blaze_typeck::PNode>, blaze_own::Alloc<'_>> = blaze_own::Box::new_in(StableColumn::new(), alloc);
    let lists: blaze_own::Box<dyn blaze_typeck::Stable<blaze_typeck::PTy>, blaze_own::Alloc<'_>> = blaze_own::Box::new_in(StableColumn::new(), alloc);
    blaze_typeck::PortableTys::new_stable(alloc, nodes, lists)
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    /// A type table over stable columns interns as one over owned columns does (equal ids,
    /// equal tables), through `&self`, while a view taken first still reads what it read; the
    /// owned form refuses a `&self` intern by type of error. The control: the owned form's
    /// `Vec` moves its nodes when it grows.
    #[test]
    fn a_stable_type_table_interns_alike_through_a_shared_borrow() {
        use blaze_typeck::{CheckError, PNode, PTy, PortableTys};
        let g = blaze_own::Alloc::global();
        let stable = stable_tys(g).expect("the stable table");
        let mut owned = PortableTys::new_in(g);
        let unit = PNode::Tuple(blaze_typeck::List::EMPTY);
        let first = stable.intern_shared(unit).expect("interned");
        assert_eq!(owned.intern(unit).expect("interned"), first);
        let held = stable.view();
        let held_len = held.len();
        let mut last = first;
        for i in 0..50_000u32 {
            let list = stable.intern_list_shared(&[last, first]).expect("a list");
            assert_eq!(owned.intern_list(&[last, first]).expect("a list"), list);
            let n = PNode::Tuple(list);
            last = stable.intern_shared(n).expect("interned");
            assert_eq!(owned.intern(n).expect("interned"), last);
            assert_eq!(stable.intern_shared(n).expect("again"), last, "hash-consed at {i}");
        }
        assert!(stable == owned, "equal tables");
        // The view taken before 100,000 interns: its length and node as they were.
        assert_eq!(held.len(), held_len);
        assert_eq!(held.node(first).ok(), Some(unit));
        assert_eq!(stable.view().len(), 50_001);
        assert!(matches!(owned.intern_shared(unit), Err(CheckError::Malformed(blaze_typeck::Malformed::SharedIntern))));
        let _: PTy = last;
    }

    /// A LIVE view (blaze-typeck `PTys`, lane VH): a view of a stable table taken, then 1,000
    /// types and their lists interned through `&self` (what an import during a body's check
    /// does): (a) each is read through the OLD view, equal to a fresh view, and through a
    /// seeded body's view (`OwnTys::in_base`) taken before them; (b) their ids are dense and
    /// unique; (c) the owned form reads the same nodes and lists through the same accessors.
    /// The control (d): a view of the same snapshot WITHOUT the column (`PTys::flat`) refuses a
    /// late id by type of error (`Malformed::TypeOrder`; a late list: `Malformed::List`).
    #[test]
    fn a_view_taken_before_an_import_reads_it() {
        use blaze_typeck::{CheckError, List, Malformed, OwnTys, PNode, PTy, PTys, PortableTys};
        let g = blaze_own::Alloc::global();
        let stable = stable_tys(g).expect("the stable table");
        let mut owned = PortableTys::new_in(g);
        let unit = PNode::Tuple(List::EMPTY);
        let first = stable.intern_shared(unit).expect("interned");
        assert_eq!(owned.intern(unit).expect("interned"), first);
        let old = stable.view();
        let old_len = old.len();
        let seeded = OwnTys::in_base(g);
        let body_view = seeded.view(&stable).expect("a seeded body's view");
        let mut made: Vec<(PTy, List), Global> = Vec::new_in(Global);
        let mut last = first;
        for _ in 0..1_000u32 {
            let list = stable.intern_list_shared(&[last, first]).expect("a list");
            let t = stable.intern_shared(PNode::Tuple(list)).expect("interned");
            assert_eq!(owned.intern_list(&[last, first]).expect("a list"), list);
            assert_eq!(owned.intern(PNode::Tuple(list)).expect("interned"), t);
            made.push((t, list));
            last = t;
        }
        // (b) Dense and unique: the k-th type made is `PTy(k + 1)`, after `()`.
        for (k, &(t, _)) in made.iter().enumerate() {
            assert_eq!(t.0 as usize, k + 1, "ids dense and unique");
        }
        let fresh = stable.view();
        let owned_view = owned.view();
        assert_eq!(old.len(), old_len, "the old view's snapshot is as it was");
        assert_eq!(old.len_now(), fresh.len(), "the old view's column as it stands");
        for &(t, list) in made.iter() {
            assert_eq!(old.node(t).ok(), Some(PNode::Tuple(list)), "(a) read through the old view");
            assert_eq!(old.node(t).ok(), fresh.node(t).ok());
            assert_eq!(body_view.node(t).ok(), fresh.node(t).ok(), "(a) through the seeded body's view");
            assert_eq!(old.list(list).ok(), fresh.list(list).ok());
            assert_eq!(body_view.list(list).ok(), fresh.list(list).ok());
            for i in list.range() {
                assert_eq!(old.list_slot(i), fresh.list_slot(i));
            }
            // (c) The owned form, through the same accessors.
            assert_eq!(owned_view.node(t).ok(), Some(PNode::Tuple(list)));
            assert_eq!(owned_view.list(list).ok(), fresh.list(list).ok());
        }
        // (d) The control: the old snapshot without the column.
        let mut nodes = Vec::new_in(Global);
        nodes.extend(old.nodes().copied());
        let mut lists = Vec::new_in(Global);
        lists.extend(old.lists().copied());
        let snapshot = PTys::flat(&nodes, &lists);
        assert_eq!(snapshot.node(first).ok(), Some(unit));
        assert!(matches!(snapshot.node(made[0].0), Err(CheckError::Malformed(Malformed::TypeOrder { .. }))), "a late id past a snapshot is refused");
        assert!(matches!(snapshot.list(made[0].1), Err(CheckError::Malformed(Malformed::List { .. }))), "a late list past a snapshot is refused");
    }

    /// A row read before many more are filed (past the first commit, and past the index map's
    /// growth), replaced and removed, is still at its address with its value: the property the
    /// on-the-spot head rests on. The control: the same through a `Vec` moves the row.
    #[test]
    fn a_row_never_moves() {
        let rows: Rows<u64, Vec<u64, Global>> = Rows::new_in(Global);
        let mut first = Vec::new_in(Global);
        first.extend_from_slice(&[7, 8, 9]);
        rows.insert(1, first);
        let held: &Vec<u64, Global> = rows.get(&1).expect("filed");
        let at = held as *const Vec<u64, Global> as usize;
        let data = held.as_ptr() as usize;
        for k in 2..100_000u64 {
            let mut v = Vec::new_in(Global);
            v.push(k);
            rows.insert(k, v);
        }
        let mut again = Vec::new_in(Global);
        again.push(1);
        let replaced = rows.insert(1, again).expect("the old row");
        assert_eq!(replaced as *const _ as usize, at);
        assert_eq!(rows.get(&1).map(|v| &v[..]), Some(&[1u64][..]));
        assert!(rows.remove(&5).is_some());
        assert!(!rows.contains_key(&5));
        // The reference taken first reads its row unchanged, at the same address.
        assert_eq!(held as *const _ as usize, at);
        assert_eq!(held.as_ptr() as usize, data);
        assert_eq!(&held[..], &[7, 8, 9]);
        assert_eq!(rows.len(), 99_998);
        // Live rows only, in the order filed: the replaced row of 1 and the removed 5 are not.
        let mut keys = Vec::new_in(Global);
        keys.extend(rows.keys().copied().take(3));
        assert_eq!(&keys[..], &[2, 3, 4]);
        assert_eq!(rows.iter().count(), 99_998);
        // Control: a `Vec`'s element moves when it grows.
        let mut v: Vec<u64, Global> = Vec::with_capacity_in(1, Global);
        v.push(0);
        let before = v.as_ptr() as usize;
        v.extend(1..100_000u64);
        assert_ne!(v.as_ptr() as usize, before);
    }
}
