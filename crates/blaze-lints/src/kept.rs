//! What a lint query's value holds in place of owned memory, where that is a flat conversion.
//! A memo value owns nothing to drop unless its query is `owns` (docs/v2/plan/design/
//! front-end-on-memo.md, B, "Values that own memory"): it is freed with its arena, running no
//! destructor. So a list is a slice kept in the run's arena (`blaze_memo::keep_iter`), a map
//! is its rows sorted by the key's hash and searched ([`Map`]), a set is a map to `()`
//! ([`Set`]), and text is a kept `&'db str`. A value that is a real tree (diagnostics, nested
//! lists, summaries) keeps its heap type and its query is `owns`.

use std::{
    alloc::Global,
    fmt,
    hash::{BuildHasher, Hash},
};

use base_db::Db;
use blaze_hash::FixedState;
use blaze_memo::Database as _;

/// The rows of `values`, kept in the current run's arena.
pub(crate) fn slice<'db, T: Send + Sync + 'db>(
    db: &'db Db,
    values: impl IntoIterator<Item = T>,
) -> &'db [T] {
    blaze_memo::keep_iter(db.memo(), values)
}

/// `text` kept in the current run's arena.
pub(crate) fn text<'db>(db: &'db Db, text: &str) -> &'db str {
    blaze_memo::keep_str(db.memo(), text)
}

/// A map kept in a query's arena: its rows sorted by the key's hash ([`FixedState`], the same
/// in every process) and searched by it, then by the key. What a `HashMap` of the value was:
/// a lookup's answer is the same, and its iteration order is the hash's, as that one's was.
pub(crate) struct Map<'db, K: 'db, V: 'db> {
    rows: &'db [(u64, K, V)],
}

/// A set kept in a query's arena: a [`Map`] to `()`.
pub(crate) type Set<'db, K> = Map<'db, K, ()>;

fn hash_of<K: Hash + ?Sized>(key: &K) -> u64 {
    FixedState.hash_one(key)
}

impl<'db, K: Hash + Eq + Send + Sync + 'db, V: Send + Sync + 'db> Map<'db, K, V> {
    /// The map of `rows`, kept in the current run's arena. A key given twice keeps its last
    /// value, as `HashMap::insert` does.
    pub(crate) fn keep(db: &'db Db, rows: impl IntoIterator<Item = (K, V)>) -> Map<'db, K, V> {
        let mut sorted: Vec<(u64, K, V), Global> = Vec::new_in(Global);
        sorted.extend(rows.into_iter().map(|(key, value)| (hash_of(&key), key, value)));
        // Stable: of a key given twice, the later row is last among its hash's.
        sorted.sort_by_key(|row| row.0);
        let mut deduped: Vec<(u64, K, V), Global> = Vec::new_in(Global);
        for row in sorted {
            let start = deduped.partition_point(|it| it.0 < row.0);
            match deduped[start..].iter().position(|it| it.1 == row.1) {
                Some(at) => deduped[start + at] = row,
                None => deduped.push(row),
            }
        }
        Map { rows: blaze_memo::keep_iter(db.memo(), deduped) }
    }

    /// The rows of `key`'s hash.
    fn rows_of(&self, hash: u64) -> &'db [(u64, K, V)] {
        let start = self.rows.partition_point(|it| it.0 < hash);
        let end = start + self.rows[start..].partition_point(|it| it.0 == hash);
        &self.rows[start..end]
    }

    pub(crate) fn get(&self, key: &K) -> Option<&'db V> {
        self.rows_of(hash_of(key)).iter().find(|it| it.1 == *key).map(|it| &it.2)
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }
}

impl<'db, K: Hash + Eq + Send + Sync + 'db> Map<'db, K, ()> {
    /// The set of `keys`, kept in the current run's arena.
    pub(crate) fn keep_set(db: &'db Db, keys: impl IntoIterator<Item = K>) -> Set<'db, K> {
        Map::keep(db, keys.into_iter().map(|key| (key, ())))
    }

    pub(crate) fn contains(&self, key: &K) -> bool {
        self.contains_key(key)
    }
}

impl<'db, K: 'db, V: 'db> Map<'db, K, V> {
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Every row, in the order of the keys' hashes.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&'db K, &'db V)> + 'db {
        self.rows.iter().map(|it| (&it.1, &it.2))
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &'db K> + 'db {
        self.rows.iter().map(|it| &it.1)
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &'db V> + 'db {
        self.rows.iter().map(|it| &it.2)
    }
}

impl<K, V> Clone for Map<'_, K, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> Copy for Map<'_, K, V> {}

impl<K, V> Default for Map<'_, K, V> {
    fn default() -> Self {
        Map { rows: &[] }
    }
}

impl<K: PartialEq, V: PartialEq> PartialEq for Map<'_, K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows
    }
}

impl<K: Eq, V: Eq> Eq for Map<'_, K, V> {}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Map<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.rows.iter().map(|it| (&it.1, &it.2))).finish()
    }
}
