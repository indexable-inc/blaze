//! Union-find over inference variables as flat columns: `parent` and `rank` beside the value of
//! each variable, path halving on `find`, union by rank.
//!
//! The root a union keeps is `ena` 0.14's, which rustc uses, so `resolve` and diagnostics see
//! the same variable: the root of higher rank, and on equal ranks the second one, whose rank
//! grows by one; where the value asks for [`UnifyValue::OLDER_ROOT`] (rustc's `order_roots` of
//! `TyVidEqKey` and `ConstVidKey`), the older of the two whatever the ranks. Path halving and
//! `ena`'s full compression never change a root.
//!
//! A snapshot is a length mark plus a position in an undo log of `(index, old parent, old rank,
//! old value)` rows. Rollback replays the rows in reverse, then truncates the columns to the
//! mark; committing the outermost snapshot drops the rows. Only a change to a variable that
//! existed when the innermost open snapshot started is logged: every later one goes with the
//! truncation.

use std::fmt;
use std::marker::PhantomData;

use super::touched::Touched;

pub(crate) trait UnifyKey: Copy + Eq + fmt::Debug {
    fn index(self) -> u32;
    fn from_index(index: u32) -> Self;
}

pub(crate) trait UnifyValue: Copy + fmt::Debug {
    type Error;

    /// Whether a union keeps the older root, the lower index, whatever the ranks (rustc's
    /// `order_roots`): the variable inference made first, for a generic parameter, stays the
    /// one a diagnostic names, not one a solver response made later and equated with it.
    const OLDER_ROOT: bool = false;

    fn unify_values(a: &Self, b: &Self) -> Result<Self, Self::Error>;
}

/// The error of values that always unify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoError {}

#[derive(Clone, Copy, Debug)]
struct UndoRow<V> {
    value: V,
    index: u32,
    parent: u32,
    rank: u8,
}

/// Where a table stood when a snapshot started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UnionFindMark {
    len: u32,
    undo: u32,
    outer_floor: u32,
    touched: u32,
}

/// Per variable: 4 bytes of `parent` (all `find` reads), 1 of `rank`, and the value. `find` is
/// `O(log n)` worst case and near `O(1)` amortized; a union writes two undo rows in a snapshot.
pub(crate) struct UnionFind<K, V> {
    parent: Vec<u32>,
    /// At most `log2` of the number of variables, so below 32, unless `V::OLDER_ROOT`, where it
    /// saturates and decides nothing.
    rank: Vec<u8>,
    value: Vec<V>,
    undo: Vec<UndoRow<V>>,
    /// The number of variables when the innermost open snapshot started; zero outside snapshots.
    floor: u32,
    /// Grows on every write to a set or value and on every rollback that undoes something, and
    /// never shrinks, so an unchanged count means unchanged sets and values.
    changes: u64,
    /// The rows `set` wrote, for readers waiting on a variable (`fulfill::watch`).
    touched: Touched,
    key: PhantomData<fn(K) -> K>,
}

impl<K, V> Default for UnionFind<K, V> {
    fn default() -> Self {
        UnionFind {
            // alloc: empty `Vec`s do not allocate.
            parent: Vec::new(),
            // alloc: empty `Vec`s do not allocate.
            rank: Vec::new(),
            // alloc: empty `Vec`s do not allocate.
            value: Vec::new(),
            // alloc: empty `Vec`s do not allocate.
            undo: Vec::new(),
            floor: 0,
            changes: 0,
            touched: Touched::default(),
            key: PhantomData,
        }
    }
}

impl<K, V: Copy> Clone for UnionFind<K, V> {
    fn clone(&self) -> Self {
        UnionFind {
            // clone: only `InferCtxt::fork` copies a table.
            parent: self.parent.clone(),
            // clone: only `InferCtxt::fork` copies a table.
            rank: self.rank.clone(),
            // clone: only `InferCtxt::fork` copies a table.
            value: self.value.clone(),
            // clone: only `InferCtxt::fork` copies a table.
            undo: self.undo.clone(),
            floor: self.floor,
            changes: self.changes,
            // clone: only `InferCtxt::fork` copies a table.
            touched: self.touched.clone(),
            key: PhantomData,
        }
    }

    /// Into `self`'s columns, keeping their capacity: a warm fork copies without allocating.
    fn clone_from(&mut self, source: &Self) {
        self.parent.clone_from(&source.parent);
        self.rank.clone_from(&source.rank);
        self.value.clone_from(&source.value);
        self.undo.clone_from(&source.undo);
        self.floor = source.floor;
        self.changes = source.changes;
        self.touched.clone_from(&source.touched);
    }
}

impl<K, V> UnionFind<K, V> {
    /// Back to `default()`, its columns' capacity kept.
    pub(crate) fn empty(&mut self) {
        self.parent.clear();
        self.rank.clear();
        self.value.clear();
        self.undo.clear();
        self.floor = 0;
        self.changes = 0;
        self.touched.empty();
    }

    /// Bytes its columns hold room for.
    pub(crate) fn held(&self) -> usize {
        self.parent.capacity() * size_of::<u32>()
            + self.rank.capacity() * size_of::<u8>()
            + self.value.capacity() * size_of::<V>()
            + self.undo.capacity() * size_of::<UndoRow<V>>()
            + self.touched.held()
    }
}

impl<K: UnifyKey, V: fmt::Debug> fmt::Debug for UnionFind<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries((0..self.parent.len()).map(|at| {
                (K::from_index(at as u32), K::from_index(self.parent[at]), &self.value[at])
            }))
            .finish()
    }
}

impl<K: UnifyKey, V: UnifyValue> UnionFind<K, V> {
    /// The number of variables created so far.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.parent.len()
    }

    /// The count of changes to the sets and values; see the field.
    #[inline]
    pub(crate) fn changes(&self) -> u64 {
        self.changes
    }

    /// The rows written; see `infer::touched`.
    #[inline]
    pub(crate) fn touched(&self) -> &Touched {
        &self.touched
    }

    pub(crate) fn new_key(&mut self, value: V) -> K {
        let index = u32::try_from(self.parent.len())
            .ok()
            .filter(|&index| index < u32::MAX)
            .expect("more than u32::MAX - 1 inference variables of one kind");
        self.parent.push(index);
        self.rank.push(0);
        self.value.push(value);
        K::from_index(index)
    }

    /// The root of `key`'s set.
    #[inline]
    pub(crate) fn find(&mut self, key: K) -> K {
        K::from_index(self.root(key.index()))
    }

    /// The value of `key`'s set.
    pub(crate) fn probe_value(&mut self, key: K) -> V {
        self.inlined_probe_value(key)
    }

    #[inline(always)]
    pub(crate) fn inlined_probe_value(&mut self, key: K) -> V {
        let root = self.root(key.index());
        self.value[root as usize]
    }

    /// The root of `key`'s set and the set's value, from one walk.
    #[inline(always)]
    pub(crate) fn inlined_probe_key_value(&mut self, key: K) -> (K, V) {
        let root = self.root(key.index());
        (K::from_index(root), self.value[root as usize])
    }

    /// The value of `key` if it is a root, without following or changing any parent.
    #[inline]
    pub(crate) fn try_probe_value(&self, key: K) -> Option<&V> {
        let at = key.index();
        (self.parent[at as usize] == at).then(|| &self.value[at as usize])
    }

    /// The root of every set whose value satisfies `keep`, in index order: one pass over the
    /// columns, as a root is its own parent.
    pub(crate) fn roots_where(&self, keep: impl Fn(V) -> bool) -> Vec<K> {
        self.parent
            .iter()
            .zip(&self.value)
            .enumerate()
            .filter(|&(at, (&parent, &value))| parent == at as u32 && keep(value))
            .map(|(at, _)| K::from_index(at as u32))
            // alloc: once per body, for fallback.
            .collect()
    }

    /// Merges the sets of `a` and `b`; when their values do not unify, changes nothing.
    pub(crate) fn unify_var_var(&mut self, a: K, b: K) -> Result<(), V::Error> {
        let root_a = self.root(a.index());
        let root_b = self.root(b.index());
        if root_a == root_b {
            return Ok(());
        }
        let combined =
            V::unify_values(&self.value[root_a as usize], &self.value[root_b as usize])?;
        self.unify_roots(root_a, root_b, combined);
        Ok(())
    }

    /// Unifies the value of `a`'s set with `value`; when they do not unify, changes nothing.
    pub(crate) fn unify_var_value(&mut self, a: K, value: V) -> Result<(), V::Error> {
        let root = self.root(a.index());
        let value = V::unify_values(&self.value[root as usize], &value)?;
        let at = root as usize;
        self.set(root, self.parent[at], self.rank[at], value);
        Ok(())
    }

    /// Makes every variable its own set again, with the value `value` gives it.
    pub(crate) fn reset_unifications(&mut self, mut value: impl FnMut(K) -> V) {
        for at in 0..self.parent.len() as u32 {
            self.set(at, at, 0, value(K::from_index(at)));
        }
    }

    pub(crate) fn start_snapshot(&mut self) -> UnionFindMark {
        let len = self.parent.len() as u32;
        let mark = UnionFindMark {
            len,
            undo: self.undo.len() as u32,
            outer_floor: self.floor,
            touched: self.touched.mark(),
        };
        self.floor = len;
        mark
    }

    pub(crate) fn rollback_to(&mut self, mark: UnionFindMark) {
        assert!(self.parent.len() >= mark.len as usize, "rolled back past a truncation");
        if self.undo.len() > mark.undo as usize || self.parent.len() > mark.len as usize {
            self.changes += 1;
        }
        while self.undo.len() > mark.undo as usize {
            let UndoRow { value, index, parent, rank } = self.undo.pop().unwrap();
            let at = index as usize;
            self.parent[at] = parent;
            self.rank[at] = rank;
            self.value[at] = value;
        }
        let len = mark.len as usize;
        self.parent.truncate(len);
        self.rank.truncate(len);
        self.value.truncate(len);
        self.floor = mark.outer_floor;
        self.touched.rollback_to(mark.touched);
    }

    pub(crate) fn commit(&mut self, mark: UnionFindMark) {
        self.floor = mark.outer_floor;
        self.touched.commit();
        // Any rollback still possible truncates to no more than the floor, and every row is
        // of a variable at or above it.
        if self.floor == 0 {
            self.undo.clear();
        }
    }

    /// Path halving only on variables at or above the floor, which a rollback truncates, so
    /// `find` never writes an undo row. Below the floor a path stays as it is: union by rank
    /// keeps it at most `log2(len)` long (an `OLDER_ROOT` table at most `len`, halved outside
    /// snapshots), and halving never changes a root.
    #[inline(always)]
    fn root(&mut self, index: u32) -> u32 {
        let mut at = index;
        loop {
            let parent = self.parent[at as usize];
            if parent == at {
                return at;
            }
            let grandparent = self.parent[parent as usize];
            if grandparent != parent && at >= self.floor {
                self.parent[at as usize] = grandparent;
            }
            at = grandparent;
        }
    }

    fn unify_roots(&mut self, root_a: u32, root_b: u32, combined: V) {
        let rank_a = self.rank[root_a as usize];
        let rank_b = self.rank[root_b as usize];
        let (new_rank, old_root, new_root) = if V::OLDER_ROOT {
            // `ena`'s rank for a root `order_roots` chose; it decides nothing in such a table.
            let (new_root, old_root, rank_new, rank_old) = if root_a < root_b {
                (root_a, root_b, rank_a, rank_b)
            } else {
                (root_b, root_a, rank_b, rank_a)
            };
            let rank = if rank_new > rank_old { rank_new } else { rank_old.saturating_add(1) };
            (rank, old_root, new_root)
        } else if rank_a > rank_b {
            (rank_a, root_b, root_a)
        } else if rank_a < rank_b {
            (rank_b, root_a, root_b)
        } else {
            (rank_a + 1, root_a, root_b)
        };
        let old = old_root as usize;
        self.set(old_root, new_root, self.rank[old], self.value[old]);
        self.set(new_root, new_root, new_rank, combined);
    }

    #[inline(always)]
    fn set(&mut self, index: u32, parent: u32, rank: u8, value: V) {
        let at = index as usize;
        if index < self.floor {
            self.undo.push(UndoRow {
                value: self.value[at],
                index,
                parent: self.parent[at],
                rank: self.rank[at],
            });
        }
        self.parent[at] = parent;
        self.rank[at] = rank;
        self.value[at] = value;
        self.changes += 1;
        self.touched.push(index);
    }
}

impl<K: UnifyKey, V: UnifyValue<Error = NoError>> UnionFind<K, V> {
    pub(crate) fn union(&mut self, a: K, b: K) {
        match self.unify_var_var(a, b) {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }

    pub(crate) fn union_value(&mut self, a: K, value: V) {
        match self.unify_var_value(a, value) {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }
}

const _: () = assert!(size_of::<UnionFindMark>() == 16);
const _: () = assert!(size_of::<UndoRow<()>>() == 12);
const _: () = assert!(size_of::<UndoRow<rustc_type_ir::IntVarValue>>() == 12);
const _: () = assert!(size_of::<UndoRow<rustc_type_ir::FloatVarValue>>() == 12);

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Key(u32);

    impl UnifyKey for Key {
        fn index(self) -> u32 {
            self.0
        }
        fn from_index(index: u32) -> Self {
            Key(index)
        }
    }

    /// A type variable's value in miniature: known, or unknown in a universe.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Value {
        Known(u32),
        Unknown(u32),
    }

    impl UnifyValue for Value {
        type Error = ();

        fn unify_values(a: &Self, b: &Self) -> Result<Self, ()> {
            match (*a, *b) {
                (Value::Known(_), Value::Known(_)) => Err(()),
                (Value::Known(k), Value::Unknown(_)) | (Value::Unknown(_), Value::Known(k)) => {
                    Ok(Value::Known(k))
                }
                (Value::Unknown(a), Value::Unknown(b)) => Ok(Value::Unknown(a.min(b))),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Universe(u32);

    impl UnifyValue for Universe {
        type Error = NoError;

        fn unify_values(a: &Self, b: &Self) -> Result<Self, NoError> {
            Ok(Universe(a.0.min(b.0)))
        }
    }

    /// The same rules with no path compression and snapshots as whole copies.
    #[derive(Clone, Default)]
    struct Model {
        parent: Vec<u32>,
        rank: Vec<u32>,
        value: Vec<Value>,
    }

    impl Model {
        fn new_key(&mut self, value: Value) -> u32 {
            let at = self.parent.len() as u32;
            self.parent.push(at);
            self.rank.push(0);
            self.value.push(value);
            at
        }

        fn find(&self, mut at: u32) -> u32 {
            while self.parent[at as usize] != at {
                at = self.parent[at as usize];
            }
            at
        }

        fn unify_var_var(&mut self, a: u32, b: u32) -> Result<(), ()> {
            let (a, b) = (self.find(a), self.find(b));
            if a == b {
                return Ok(());
            }
            let value = Value::unify_values(&self.value[a as usize], &self.value[b as usize])?;
            let (rank_a, rank_b) = (self.rank[a as usize], self.rank[b as usize]);
            let (rank, child, root) = match rank_a.cmp(&rank_b) {
                std::cmp::Ordering::Greater => (rank_a, b, a),
                std::cmp::Ordering::Less => (rank_b, a, b),
                std::cmp::Ordering::Equal => (rank_a + 1, a, b),
            };
            self.parent[child as usize] = root;
            self.rank[root as usize] = rank;
            self.value[root as usize] = value;
            Ok(())
        }

        fn unify_var_value(&mut self, a: u32, value: Value) -> Result<(), ()> {
            let root = self.find(a) as usize;
            self.value[root] = Value::unify_values(&self.value[root], &value)?;
            Ok(())
        }

        /// What the first `n` variables observe: each one's set, named by its root, and value.
        fn observed(&self, n: usize) -> Vec<(u32, Value)> {
            (0..n as u32)
                .map(|at| {
                    let root = self.find(at);
                    (root, self.value[root as usize])
                })
                .collect()
        }
    }

    fn assert_same(table: &mut UnionFind<Key, Value>, model: &Model) {
        assert_eq!(table.len(), model.parent.len());
        for at in 0..model.parent.len() as u32 {
            let root = model.find(at);
            assert_eq!(table.find(Key(at)), Key(root), "root of {at}");
            assert_eq!(table.probe_value(Key(at)), model.value[root as usize], "value of {at}");
            assert_eq!(
                table.inlined_probe_key_value(Key(at)),
                (Key(root), model.value[root as usize]),
                "root and value of {at}"
            );
            let is_root = root == at;
            assert_eq!(table.try_probe_value(Key(at)).is_some(), is_root, "{at} is a root");
        }
        let roots: Vec<Key> = (0..model.parent.len() as u32)
            .filter(|&at| model.find(at) == at)
            .map(Key)
            .collect();
        assert_eq!(table.roots_where(|_| true), roots, "roots");
    }

    #[test]
    fn equal_ranks_keep_the_second_root() {
        let mut table = UnionFind::<Key, Universe>::default();
        let a = table.new_key(Universe(3));
        let b = table.new_key(Universe(1));
        let c = table.new_key(Universe(2));
        table.union(a, b);
        assert_eq!(table.find(a), b);
        assert_eq!(table.probe_value(a), Universe(1));
        // `b` has rank 1 and `c` rank 0: the higher rank stays the root, either way round.
        table.union(c, a);
        assert_eq!(table.find(c), b);
        let d = table.new_key(Universe(0));
        table.union(b, d);
        assert_eq!(table.find(d), b);
        assert_eq!(table.probe_value(c), Universe(0));
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Older(u32);

    impl UnifyValue for Older {
        type Error = NoError;
        const OLDER_ROOT: bool = true;

        fn unify_values(a: &Self, b: &Self) -> Result<Self, NoError> {
            Ok(Older(a.0.min(b.0)))
        }
    }

    /// rustc's `order_roots` of `TyVidEqKey`: the older root stays, whichever side and rank.
    #[test]
    fn an_older_root_table_keeps_the_older_variable() {
        let mut table = UnionFind::<Key, Older>::default();
        let keys: Vec<_> = (0..4).map(|u| table.new_key(Older(u + 10))).collect();
        table.union(keys[0], keys[1]);
        assert_eq!(table.find(keys[1]), keys[0], "equal ranks keep the first, not the second");
        table.union(keys[3], keys[2]);
        assert_eq!(table.find(keys[3]), keys[2], "the older on either side");
        let fresh = table.new_key(Older(0));
        table.union(fresh, keys[0]);
        assert_eq!(table.find(fresh), keys[0], "a singleton made later never becomes the root");
        table.union(keys[3], keys[1]);
        assert!(keys.iter().chain([&fresh]).all(|&key| table.find(key) == keys[0]));
        assert_eq!(table.probe_value(keys[2]), Older(0));
    }

    #[test]
    fn failed_unification_changes_nothing() {
        let mut table = UnionFind::<Key, Value>::default();
        let a = table.new_key(Value::Known(1));
        let b = table.new_key(Value::Known(2));
        assert_eq!(table.unify_var_var(a, b), Err(()));
        assert_eq!(table.find(a), a);
        assert_eq!(table.find(b), b);
        assert_eq!(table.unify_var_value(a, Value::Known(3)), Err(()));
        assert_eq!(table.probe_value(a), Value::Known(1));
    }

    #[test]
    fn rollback_of_nested_snapshots() {
        let mut table = UnionFind::<Key, Value>::default();
        let a = table.new_key(Value::Unknown(0));
        let b = table.new_key(Value::Unknown(1));
        let outer = table.start_snapshot();
        let c = table.new_key(Value::Unknown(2));
        table.unify_var_var(a, b).unwrap();
        let inner = table.start_snapshot();
        table.unify_var_var(b, c).unwrap();
        table.unify_var_value(c, Value::Known(7)).unwrap();
        assert_eq!(table.probe_value(a), Value::Known(7));
        table.rollback_to(inner);
        assert_eq!(table.probe_value(a), Value::Unknown(0));
        assert_eq!(table.find(c), c);
        assert_eq!(table.find(a), b);
        table.rollback_to(outer);
        assert_eq!(table.len(), 2);
        assert_eq!(table.find(a), a);
        assert_eq!(table.probe_value(b), Value::Unknown(1));
    }

    #[test]
    fn commit_keeps_changes_for_an_outer_rollback() {
        let mut table = UnionFind::<Key, Value>::default();
        let a = table.new_key(Value::Unknown(0));
        let b = table.new_key(Value::Unknown(0));
        let outer = table.start_snapshot();
        let inner = table.start_snapshot();
        table.unify_var_var(a, b).unwrap();
        table.commit(inner);
        assert_eq!(table.find(a), b);
        table.rollback_to(outer);
        assert_eq!(table.find(a), a);
        let root = table.start_snapshot();
        table.unify_var_var(a, b).unwrap();
        table.commit(root);
        assert!(table.undo.is_empty());
        assert_eq!(table.find(a), b);
    }

    #[test]
    fn variables_made_in_the_snapshot_are_not_logged() {
        let mut table = UnionFind::<Key, Value>::default();
        let snapshot = table.start_snapshot();
        let a = table.new_key(Value::Unknown(0));
        let b = table.new_key(Value::Unknown(0));
        table.unify_var_var(a, b).unwrap();
        assert!(table.undo.is_empty());
        table.rollback_to(snapshot);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn find_in_a_snapshot_writes_no_undo_rows() {
        let mut table = UnionFind::<Key, Universe>::default();
        let keys: Vec<_> = (0..8).map(|u| table.new_key(Universe(u))).collect();
        for pair in keys.chunks(2) {
            table.union(pair[0], pair[1]);
        }
        table.union(keys[1], keys[3]);
        table.union(keys[5], keys[7]);
        table.union(keys[3], keys[7]);
        let snapshot = table.start_snapshot();
        for &key in &keys {
            table.find(key);
        }
        assert!(table.undo.is_empty());
        table.commit(snapshot);
        let root = table.find(keys[0]);
        assert!(keys.iter().all(|&key| table.find(key) == root));
    }

    #[test]
    fn reset_unifications_splits_every_set() {
        let mut table = UnionFind::<Key, Value>::default();
        let keys: Vec<_> = (0..4).map(|u| table.new_key(Value::Unknown(u))).collect();
        table.unify_var_var(keys[0], keys[1]).unwrap();
        table.unify_var_var(keys[2], keys[1]).unwrap();
        table.reset_unifications(|key| Value::Unknown(key.0 + 10));
        for key in keys {
            assert_eq!(table.find(key), key);
            assert_eq!(table.probe_value(key), Value::Unknown(key.0 + 10));
        }
    }

    /// Random sequences of every operation, nested snapshots included, against `Model`.
    #[test]
    fn matches_the_model() {
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for _ in 0..200 {
            let mut table = UnionFind::<Key, Value>::default();
            let mut model = Model::default();
            let mut snapshots: Vec<(UnionFindMark, Model)> = Vec::new();
            for _ in 0..300 {
                let len = model.parent.len();
                let (model_before, changes_before) = (model.clone(), table.changes());
                match rng.below(10) {
                    0 | 1 => {
                        let value = if rng.below(4) == 0 {
                            Value::Known(rng.below(5) as u32)
                        } else {
                            Value::Unknown(rng.below(4) as u32)
                        };
                        assert_eq!(table.new_key(value), Key(model.new_key(value)));
                    }
                    2..=4 if len > 0 => {
                        let (a, b) = (rng.index(len) as u32, rng.index(len) as u32);
                        assert_eq!(
                            table.unify_var_var(Key(a), Key(b)),
                            model.unify_var_var(a, b)
                        );
                    }
                    5 if len > 0 => {
                        let a = rng.index(len) as u32;
                        let value = Value::Known(rng.below(5) as u32);
                        assert_eq!(
                            table.unify_var_value(Key(a), value),
                            model.unify_var_value(a, value)
                        );
                    }
                    6 if len > 0 => {
                        let a = rng.index(len) as u32;
                        assert_eq!(table.find(Key(a)), Key(model.find(a)));
                    }
                    7 => snapshots.push((table.start_snapshot(), model.clone())),
                    8 => {
                        if let Some((mark, before)) = snapshots.pop() {
                            table.rollback_to(mark);
                            model = before;
                        }
                    }
                    9 => {
                        if let Some((mark, _)) = snapshots.pop() {
                            table.commit(mark);
                        }
                    }
                    _ => {}
                }
                // A variable made since doesn't count: nothing that predates it can name it.
                let kept = model_before.parent.len().min(model.parent.len());
                let changed = model.parent.len() < model_before.parent.len()
                    || model.observed(kept) != model_before.observed(kept);
                assert!(table.changes() >= changes_before, "the count never shrinks");
                assert!(!changed || table.changes() > changes_before, "a change is counted");
                if rng.below(16) == 0 {
                    assert_same(&mut table, &model);
                }
            }
            while let Some((mark, before)) = snapshots.pop() {
                if rng.below(2) == 0 {
                    table.rollback_to(mark);
                    model = before;
                } else {
                    table.commit(mark);
                }
                assert_same(&mut table, &model);
            }
            assert!(table.undo.is_empty());
            assert_eq!(table.floor, 0);
        }
    }
}
