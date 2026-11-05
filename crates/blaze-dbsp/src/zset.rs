use crate::Weight;

/// A Z-set: (value, weight) consolidated (sorted by value, no zero weights).
///
/// [`ZSet::push`] appends; a push in value order onto a consolidated set keeps it consolidated,
/// any other push leaves it to [`ZSet::consolidate`]. Reading an unconsolidated set
/// ([`ZSet::iter`], [`ZSet::len`], [`ZSet::is_empty`]) panics: its rows would not be the set.
#[derive(Debug)]
pub struct ZSet<T> {
    rows: Vec<(T, Weight)>,
    consolidated: bool,
}

impl<T: Ord + Clone> Default for ZSet<T> {
    fn default() -> Self {
        ZSet::new()
    }
}

impl<T: Ord + Clone> ZSet<T> {
    pub fn new() -> Self {
        ZSet { rows: Vec::new(), consolidated: true } // alloc: setup: an empty buffer, grown once and kept
    }

    pub fn with_capacity(capacity: usize) -> Self {
        ZSet { rows: Vec::with_capacity(capacity), consolidated: true }
    }

    pub fn push(&mut self, t: T, w: Weight) {
        if w == 0 {
            return;
        }
        if self.consolidated
            && let Some((last, _)) = self.rows.last()
            && *last >= t
        {
            self.consolidated = false;
        }
        self.rows.push((t, w));
    }

    /// Sorts by value, sums the weights of equal values and drops the zeros: `O(n log n)`,
    /// `O(n)` on rows already in order (pdqsort's run detection), nothing on a consolidated set.
    pub fn consolidate(&mut self) {
        if self.consolidated {
            return;
        }
        self.rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut out = 0;
        for at in 0..self.rows.len() {
            if out > 0 && self.rows[out - 1].0 == self.rows[at].0 {
                let w = self.rows[at].1;
                let sum = &mut self.rows[out - 1].1;
                *sum = sum.checked_add(w).expect("a Z-set weight overflows `i32`");
            } else {
                self.rows.swap(out, at);
                out += 1;
            }
        }
        self.rows.truncate(out);
        self.rows.retain(|row| row.1 != 0);
        self.consolidated = true;
    }

    pub fn iter(&self) -> impl Iterator<Item = (&T, Weight)> {
        self.read().iter().map(|(t, w)| (t, *w))
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Empties the set, keeping its buffer.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.consolidated = true;
    }

    /// The weight of `t`: a binary search, 0 when absent.
    pub fn weight(&self, t: &T) -> Weight {
        let rows = self.read();
        match rows.binary_search_by(|row| row.0.cmp(t)) {
            Ok(at) => rows[at].1,
            Err(_) => 0,
        }
    }

    /// The consolidated rows.
    pub fn as_slice(&self) -> &[(T, Weight)] {
        self.read()
    }

    /// Every weight negated: the retraction of this set.
    pub fn negate(&mut self) {
        for row in &mut self.rows {
            row.1 = row.1.checked_neg().expect("a Z-set weight overflows `i32`");
        }
    }

    /// Adds `other`'s rows to this set's (DBSP's `+`); consolidate after.
    pub fn add(&mut self, other: &ZSet<T>) {
        for (t, w) in other.iter() {
            // clone: the values of the other set's rows, which it keeps
            self.push(t.clone(), w);
        }
    }

    fn read(&self) -> &[(T, Weight)] {
        assert!(self.consolidated, "a `ZSet` read before `consolidate`");
        &self.rows
    }
}
