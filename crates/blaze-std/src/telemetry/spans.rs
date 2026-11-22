//! The span table: per span name path (at most [`SPAN_DEPTH`] names), the count, and the sum
//! and most of wall time and of each [`Brief`] field. Rows are inclusive: a parent's sums hold
//! its children's.
//!
//! Bounds, never a drop: a path deeper than [`SPAN_DEPTH`] is its depth-4 ancestor's path
//! marked `deeper` (one row for all that hangs below it); a path met when [`SPAN_ROWS`] rows are
//! taken is added to the `other` row. Rows are placed by the path's hash (FNV-1a over its
//! names, made one name at a time at each open) with linear probing; which paths hold rows
//! depends on the order they are first met, never on the hash.

use core::{alloc::Allocator, cmp::Ordering};

use blaze_own::Vec;

use super::{Brief, FNV_START, TelemetryError, fnv};

/// The most names a path keeps.
pub const SPAN_DEPTH: usize = 4;

/// The rows a worker's table holds (64 rows of 448 bytes: 28 KiB a worker, reserved only from
/// `Spans`).
pub const SPAN_ROWS: usize = 64;

/// A span's name path from the run's root.
#[derive(Clone, Copy, Debug)]
pub struct SpanPath {
    names: [&'static str; SPAN_DEPTH],
    depth: u8,
    deeper: bool,
    hash: u64,
}

impl SpanPath {
    /// The run's root: no name.
    pub const ROOT: SpanPath = SpanPath { names: [""; SPAN_DEPTH], depth: 0, deeper: false, hash: FNV_START };

    /// The path of a span `name` opened under `self`.
    #[inline]
    pub fn child(&self, name: &'static str) -> SpanPath {
        let mut path = *self;
        if usize::from(self.depth) < SPAN_DEPTH {
            path.names[usize::from(self.depth)] = name;
            path.depth += 1;
            path.hash = fnv(fnv(self.hash, name.as_bytes()), &[0xff]);
        } else if !self.deeper {
            path.deeper = true;
            path.hash = fnv(self.hash, &[0xfe]);
        }
        path
    }

    pub fn names(&self) -> &[&'static str] {
        &self.names[..usize::from(self.depth)]
    }

    /// Whether spans deeper than [`SPAN_DEPTH`] were folded into this path.
    pub fn deeper(&self) -> bool {
        self.deeper
    }

    pub fn depth(&self) -> usize {
        usize::from(self.depth)
    }

    /// The path's identity for the sampler and the report: its hash (equal paths, equal ids).
    pub fn id(&self) -> u64 {
        self.hash
    }

    /// The top-level name, `""` for the root.
    pub fn top(&self) -> &'static str {
        self.names[0]
    }

    /// The order rows are printed and encoded in: by names, then shallower first.
    pub fn order(&self, other: &SpanPath) -> Ordering {
        self.names().cmp(other.names()).then(self.deeper.cmp(&other.deeper))
    }

    /// `f` on this path and each of its ancestors but the root, deepest first: the rows a use
    /// under this path is charged to (rows are inclusive).
    pub fn each_up(&self, mut f: impl FnMut(&SpanPath)) {
        if self.depth == 0 {
            return;
        }
        f(self);
        let mut up = [SpanPath::ROOT; SPAN_DEPTH];
        let mut path = SpanPath::ROOT;
        for (at, name) in self.names().iter().enumerate() {
            path = path.child(name);
            up[at] = path;
        }
        // The path itself is `up[depth - 1]` unless it is marked `deeper`: then that is its
        // first ancestor.
        let own = usize::from(self.depth) - usize::from(!self.deeper);
        for ancestor in up[..own].iter().rev() {
            f(ancestor);
        }
    }

    pub fn same(&self, other: &SpanPath) -> bool {
        self.hash == other.hash && self.depth == other.depth && self.deeper == other.deeper && self.names() == other.names()
    }
}

/// An open span: what its close hands back. 272 bytes, held by the context that opened it.
#[derive(Clone, Copy, Debug)]
pub struct Open {
    pub(super) worker: u32,
    pub(super) path: SpanPath,
    pub(super) wall_ns: u64,
    pub(super) at: Brief,
}

impl Open {
    pub fn path(&self) -> &SpanPath {
        &self.path
    }
}

/// One path's row.
#[derive(Clone, Copy, Debug)]
pub struct SpanRow {
    pub path: SpanPath,
    pub count: u64,
    pub wall_ns: u64,
    pub wall_most: u64,
    pub sum: Brief,
    pub most: Brief,
}

impl SpanRow {
    pub const fn empty(path: SpanPath) -> SpanRow {
        SpanRow { path, count: 0, wall_ns: 0, wall_most: 0, sum: Brief::ZERO, most: Brief::ZERO }
    }

    #[inline]
    pub fn take(&mut self, wall_ns: u64, used: &Brief) {
        self.count += 1;
        self.wall_ns = self.wall_ns.wrapping_add(wall_ns);
        if wall_ns > self.wall_most {
            self.wall_most = wall_ns;
        }
        self.sum.plus(used);
        self.most.most(used);
    }

    pub fn join(&mut self, other: &SpanRow) {
        self.count = self.count.wrapping_add(other.count);
        self.wall_ns = self.wall_ns.wrapping_add(other.wall_ns);
        if other.wall_most > self.wall_most {
            self.wall_most = other.wall_most;
        }
        self.sum.plus(&other.sum);
        self.most.most(&other.most);
    }
}

/// A worker's table.
pub struct Spans<A: Allocator + Copy> {
    /// [`SPAN_ROWS`] slots (none below `Spans`).
    rows: Vec<Option<SpanRow>, A>,
    taken: usize,
    other: SpanRow,
}

impl<A: Allocator + Copy> Spans<A> {
    pub(super) fn new(keeps: bool, alloc: A) -> Result<Spans<A>, TelemetryError> {
        let slots = if keeps { SPAN_ROWS } else { 0 };
        let mut rows = Vec::try_with_capacity_in(slots, alloc)
            .map_err(|_| TelemetryError::Reserve { what: "the span table", bytes: slots * size_of::<Option<SpanRow>>() })?;
        for _ in 0..slots {
            rows.push(None);
        }
        Ok(Spans { rows, taken: 0, other: SpanRow::empty(SpanPath::ROOT) })
    }

    /// A span of `path` closed: one more close, its wall time and what it used.
    #[inline]
    pub(super) fn close(&mut self, path: &SpanPath, wall_ns: u64, used: Brief) {
        self.row_of(path, |row| row.take(wall_ns, &used));
    }

    /// A task that ran under `path` on this worker, not inside its opener's stretch (the
    /// module doc of [`super`], "Spans across tasks"): what it used, added to the row's sum
    /// with no close and no wall time.
    #[inline]
    pub(super) fn charge(&mut self, path: &SpanPath, used: Brief) {
        self.row_of(path, |row| row.sum.plus(&used));
    }

    /// `f` on `path`'s row, made if new; past the table's rows, on `other`.
    #[inline]
    fn row_of(&mut self, path: &SpanPath, f: impl FnOnce(&mut SpanRow)) {
        let slots = self.rows.len();
        if slots == 0 {
            return;
        }
        let mut at = (path.hash as usize) % slots;
        for _ in 0..slots {
            match &mut self.rows[at] {
                Some(row) if row.path.same(path) => {
                    f(row);
                    return;
                }
                Some(_) => at = (at + 1) % slots,
                None => {
                    if self.taken == slots {
                        break;
                    }
                    let mut row = SpanRow::empty(*path);
                    f(&mut row);
                    self.rows[at] = Some(row);
                    self.taken += 1;
                    return;
                }
            }
        }
        f(&mut self.other);
    }

    /// The rows held, in slot order.
    pub fn rows(&self) -> impl Iterator<Item = &SpanRow> {
        self.rows.iter().filter_map(Option::as_ref)
    }

    /// The paths met past the table's rows.
    pub fn other(&self) -> &SpanRow {
        &self.other
    }
}
