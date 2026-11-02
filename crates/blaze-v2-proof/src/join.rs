//! [`under`] and [`over`] across cores, on owned [`Trace`]s or on [`View`]s of mapped files read in
//! place. The keys of one trace are split into one range per core; each range finds where it
//! starts in the other trace with one gallop and walks both in order from there, so a range costs
//! its keys' bytes streamed plus one probe per edge, and the ranges share nothing. Keys are
//! content hashes, so equal key ranges carry about equal edges. The pieces are joined in key
//! order and sorted as the serial join sorts, so the answer is the same bytes whatever the core
//! count: `crate::under` and `crate::over` are the reference the tests hold these to.
//!
//! [`View`]: crate::wire::View

use std::{ops::Range, sync::OnceLock, thread};

use blaze_ids::{Fingerprint, bits::BitSet};
use blaze_query::QueryKey;

use crate::{Stale, Trace, Work};

/// A trace's columns, read by index: implemented by [`Trace`] and by [`crate::wire::View`], and
/// monomorphized for each, so nothing here is dispatched at run time.
pub trait Columns: Sync {
    fn keys(&self) -> usize;
    fn key(&self, i: usize) -> QueryKey;
    fn has(&self, i: usize) -> bool;
    fn ran(&self, i: usize) -> bool;
    fn fingerprint(&self, i: usize) -> Fingerprint;
    /// Key `i`'s edges, as indices into [`Columns::read`] and [`Columns::saw`].
    fn edges_of(&self, i: usize) -> Range<usize>;
    fn read(&self, e: usize) -> u32;
    fn saw(&self, e: usize) -> Fingerprint;
    /// Word `w` of the `has` and `ran` bitsets (keys `64 w..64 w + 64`), so a walk over the keys a
    /// build ran reads one word for 64 keys; 0 past the last word.
    fn has_word(&self, w: usize) -> u64;
    fn ran_word(&self, w: usize) -> u64;
}

impl Columns for Trace {
    fn keys(&self) -> usize {
        self.keys.len()
    }

    fn key(&self, i: usize) -> QueryKey {
        self.keys[i]
    }

    fn has(&self, i: usize) -> bool {
        self.has.contains(i as u32)
    }

    fn ran(&self, i: usize) -> bool {
        self.ran.contains(i as u32)
    }

    fn fingerprint(&self, i: usize) -> Fingerprint {
        self.fingerprints[i]
    }

    fn edges_of(&self, i: usize) -> Range<usize> {
        self.edges_from[i] as usize..self.edges_from[i + 1] as usize
    }

    fn read(&self, e: usize) -> u32 {
        self.reads[e]
    }

    fn saw(&self, e: usize) -> Fingerprint {
        self.saw[e]
    }

    fn has_word(&self, w: usize) -> u64 {
        self.has.words().get(w).copied().unwrap_or(0)
    }

    fn ran_word(&self, w: usize) -> u64 {
        self.ran.words().get(w).copied().unwrap_or(0)
    }
}

/// The indices of `c`'s keys that this build ran and that have a result, ascending: one word of
/// each bitset per 64 keys and a `trailing_zeros` per member, rather than two bit tests per key.
/// Bits past the last key (a file may set them) are not members.
pub(crate) fn ran_results<C: Columns>(c: &C) -> impl Iterator<Item = usize> + '_ {
    let n = c.keys();
    (0..blaze_ids::bits::words_for(n)).flat_map(move |w| {
        let tail = if (w + 1) * 64 > n { (1u64 << (n % 64)) - 1 } else { !0 };
        let mut live = c.ran_word(w) & c.has_word(w) & tail;
        std::iter::from_fn(move || {
            (live != 0).then(|| {
                let i = w * 64 + live.trailing_zeros() as usize;
                live &= live - 1;
                i
            })
        })
    })
}

/// The keys of `kind`, a contiguous run since keys sort by kind first: two binary searches.
pub(crate) fn of_kind<C: Columns>(c: &C, kind: blaze_query::QueryKind) -> Range<usize> {
    fn first<C: Columns>(c: &C, mut lo: usize, before: impl Fn(QueryKey) -> bool) -> usize {
        let mut hi = c.keys();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if before(c.key(mid)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
    let start = first(c, 0, |k| k.kind < kind);
    start..first(c, start, |k| k.kind == kind)
}

/// Below this many elements a piece is not worth a thread (its spawn, about 10 µs, is the time of
/// that many keys joined).
const LEAST_PIECE: usize = 1 << 14;

/// The cores this process may run on, asked once: on Linux each ask reads the cgroup's quota
/// files, a few syscalls and allocations.
pub(crate) fn cores() -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    *CORES.get_or_init(|| thread::available_parallelism().map_or(1, |n| n.get()))
}

/// How many pieces to split `len` elements into: one per core, none smaller than [`LEAST_PIECE`].
pub(crate) fn pieces(len: usize) -> usize {
    (len / LEAST_PIECE).clamp(1, cores())
}

/// `work` over `0..len` cut into `pieces` consecutive ranges, the first on this thread and the
/// rest each on a scoped thread of its own, each answer handed to `take` in range order. One
/// piece runs on this thread with nothing allocated.
pub(crate) fn split<T: Send>(
    len: usize,
    pieces: usize,
    work: impl Fn(Range<usize>) -> T + Sync,
    mut take: impl FnMut(T),
) {
    let pieces = pieces.clamp(1, len.max(1));
    let range = |p: usize| p * len / pieces..(p + 1) * len / pieces;
    if pieces == 1 {
        return take(work(0..len));
    }
    thread::scope(|scope| {
        let work = &work;
        // alloc: one handle a core, once a join that spawns its threads (each spawn allocates too).
        let others: Vec<_> = (1..pieces).map(|p| scope.spawn(move || work(range(p)))).collect();
        take(work(range(0)));
        for other in others {
            take(other.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)));
        }
    });
}

/// `piece` added to the end of `all`, which takes it whole when it is the first.
fn append<T>(all: &mut Vec<T>, mut piece: Vec<T>) {
    if all.is_empty() {
        *all = piece;
    } else {
        all.append(&mut piece);
    }
}

pub(crate) fn result<C: Columns>(c: &C, i: usize) -> Option<Fingerprint> {
    c.has(i).then(|| c.fingerprint(i))
}

/// The first index at or after `from` whose key is not below `key`: `crate::seek` on columns.
pub(crate) fn seek<C: Columns>(c: &C, from: usize, key: &QueryKey) -> usize {
    let n = c.keys();
    let (mut lo, mut step) = (from, 1);
    while lo + step <= n && c.key(lo + step - 1) < *key {
        lo += step;
        step *= 2;
    }
    let mut hi = (lo + step).min(n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if c.key(mid) < *key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

pub(crate) fn index_from<C: Columns>(c: &C, cursor: &mut usize, key: &QueryKey) -> Option<usize> {
    *cursor = seek(c, *cursor, key);
    (*cursor < c.keys() && c.key(*cursor) == *key).then_some(*cursor)
}

/// Where the piece starting at key `start` of `of` begins in `other`.
fn start_in<A: Columns, B: Columns>(other: &A, of: &B, start: usize) -> usize {
    if start == 0 { 0 } else { seek(other, 0, &of.key(start)) }
}

pub(crate) fn same_edges<A: Columns, B: Columns>(a: &A, i: usize, b: &B, j: usize) -> bool {
    let (x, y) = (a.edges_of(i), b.edges_of(j));
    x.len() == y.len()
        && x.zip(y).all(|(ea, eb)| a.saw(ea) == b.saw(eb) && a.key(a.read(ea) as usize) == b.key(b.read(eb) as usize))
}

/// The keys reachable from `roots`, the roots included; `None`, every result, when there are no
/// roots.
fn reachable<C: Columns>(c: &C, roots: &[QueryKey]) -> Option<BitSet<u32>> {
    if roots.is_empty() {
        return None;
    }
    let mut seen = BitSet::new(c.keys());
    let mut next: Vec<usize> = roots
        .iter()
        .filter_map(|k| {
            let at = seek(c, 0, k);
            (at < c.keys() && c.key(at) == *k).then_some(at)
        })
        // alloc: once a join given roots, the stack of the walk; none without roots.
        .collect();
    while let Some(i) = next.pop() {
        if seen.insert(i as u32) {
            next.extend(c.edges_of(i).map(|e| c.read(e) as usize));
        }
    }
    Some(seen)
}

/// [`crate::under`] across cores.
pub fn under<A: Columns, B: Columns>(incremental: &A, scratch: &B, roots: &[QueryKey]) -> Vec<Stale> {
    under_in(incremental, scratch, roots, pieces(scratch.keys()))
}

fn under_in<A: Columns, B: Columns>(incremental: &A, scratch: &B, roots: &[QueryKey], pieces: usize) -> Vec<Stale> {
    let reached = reachable(scratch, roots);
    // alloc: the answer, which stays unallocated while nothing is stale.
    let mut stale = Vec::new();
    let work = |range: Range<usize>| {
        // alloc: as above, this piece's.
        let mut stale = Vec::new();
        let mut cursor = start_in(incremental, scratch, range.start);
        for i in range {
            let reaches = reached.as_ref().is_none_or(|r| r.contains(i as u32));
            let Some(theirs) = result(scratch, i).filter(|_| reaches) else { continue };
            let query = scratch.key(i);
            let found = index_from(incremental, &mut cursor, &query);
            let Some((j, ours)) = found.and_then(|j| Some((j, result(incremental, j)?))) else {
                stale.push(Stale::Absent { query });
                continue;
            };
            if !incremental.ran(j) {
                let missed =
                    incremental.edges_of(j).find(|&e| result(incremental, incremental.read(e) as usize) != Some(incremental.saw(e)));
                if let Some(e) = missed {
                    let read = incremental.read(e) as usize;
                    let (saw, now) = (incremental.saw(e), result(incremental, read));
                    stale.push(Stale::Unverified { query, read: incremental.key(read), saw, now });
                    continue;
                }
            }
            if ours == theirs {
                continue;
            }
            stale.push(if same_edges(incremental, j, scratch, i) {
                Stale::UnrecordedRead { query, incremental: ours, scratch: theirs }
            } else {
                Stale::Downstream { query, incremental: ours, scratch: theirs }
            });
        }
        stale
    };
    split(scratch.keys(), pieces, work, |piece| append(&mut stale, piece));
    // A query is stale at most once (one answer a key, the keys distinct), so no two have one
    // sort key and the unstable sort, which needs no buffer, orders them as the stable one did.
    stale.sort_unstable_by_key(|s| (s.rank(), s.query()));
    stale
}

/// [`crate::over`] across cores.
pub fn over<A: Columns, B: Columns>(before: &A, after: &B) -> (Work, Vec<QueryKey>) {
    over_in(before, after, pieces(after.keys()))
}

fn over_in<A: Columns, B: Columns>(before: &A, after: &B, pieces: usize) -> (Work, Vec<QueryKey>) {
    let mut work = Work::default();
    // alloc: the answer, which stays unallocated while nothing reran for nothing.
    let mut needless = Vec::new();
    let found = |range: Range<usize>| {
        let mut work = Work::default();
        // alloc: as above, this piece's.
        let mut needless = Vec::new();
        let mut cursor = start_in(before, after, range.start);
        for i in range {
            let Some(now) = result(after, i).filter(|_| after.ran(i)) else { continue };
            work.ran += 1;
            let key = after.key(i);
            match index_from(before, &mut cursor, &key).and_then(|j| Some((j, result(before, j)?))) {
                None => work.new += 1,
                Some((j, was)) if was == now && same_edges(before, j, after, i) => {
                    work.needless += 1;
                    needless.push(key);
                }
                Some((_, was)) if was == now => work.cut_off += 1,
                Some(_) => {}
            }
        }
        (work, needless)
    };
    split(after.keys(), pieces, found, |(piece, keys)| {
        work.ran += piece.ran;
        work.needless += piece.needless;
        work.cut_off += piece.cut_off;
        work.new += piece.new;
        append(&mut needless, keys);
    });
    (work, needless)
}

#[cfg(test)]
mod tests {
    use blaze_ids::{ContentId, Fingerprint};
    use blaze_query::{Edge, QueryKey, QueryKind};

    use super::{over_in, under_in};
    use crate::{Trace, TraceBuilder, Work, wire::View, wire::tests::seeded};

    /// `n` results, each reading up to four earlier ones and seeing what they are, none of them
    /// run: joined with itself, nothing is stale and nothing reran.
    fn settled(seed: u64, n: u64) -> Trace {
        let mut rng = crate::Rng::new(seed);
        let key = |i: u64| QueryKey {
            kind: QueryKind(0x2000 + (i % 5) as u16),
            input: ContentId::of("blaze v2 proof join test key 1", &i.to_le_bytes()),
        };
        let fingerprint = |i: u64| Fingerprint(u128::from(i) * 7 + 1);
        let mut builder = TraceBuilder::default();
        let mut edges = Vec::new();
        for i in 0..n {
            edges.clear();
            for _ in 0..rng.below(5).min(i) {
                let read = rng.below(i);
                edges.push(Edge { read: key(read), saw: fingerprint(read) });
            }
            builder.insert(key(i), fingerprint(i), &edges, false);
        }
        builder.finish()
    }

    /// The oracle's steady state, a build that agrees with the one from scratch and reran
    /// nothing, joined on one piece, owned or in place: nothing allocated, as the answers are
    /// empty.
    #[test]
    fn a_join_of_traces_that_agree_allocates_nothing() {
        let trace = settled(8, 3000);
        let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
        let view = View::parse(&bytes).unwrap();
        assert!(crate::under(&trace, &trace, &[]).is_empty(), "the settled trace agrees with itself");
        for _ in 0..2 {
            let stale = blaze_alloc_count::assert_at_most(0, "under, a trace and itself, owned", || {
                under_in(&trace, &trace, &[], 1)
            });
            assert!(stale.is_empty());
            let stale = blaze_alloc_count::assert_at_most(0, "under, a trace and itself, in place", || {
                under_in(&view, &view, &[], 1)
            });
            assert!(stale.is_empty());
            let (work, needless) =
                blaze_alloc_count::assert_at_most(0, "over, nothing ran, owned", || over_in(&trace, &trace, 1));
            assert_eq!((work, needless.len()), (Work::default(), 0));
            let (work, needless) =
                blaze_alloc_count::assert_at_most(0, "over, nothing ran, in place", || over_in(&view, &view, 1));
            assert_eq!((work, needless.len()), (Work::default(), 0));
        }
    }

    /// A trace of the same keys as `base` with some results changed, kept or dropped by `seed`,
    /// so `under` and `over` against `base` find every kind of answer.
    fn edited(base: &Trace, seed: u64) -> Trace {
        let mut rng = crate::Rng::new(seed);
        let mut trace = base.clone();
        for i in 0..trace.keys.len() as u32 {
            match rng.below(8) {
                0 => trace.fingerprints[i as usize] = Fingerprint(trace.fingerprints[i as usize].0 + 1),
                1 => trace.ran.set(i, !trace.ran.contains(i)),
                2 if trace.has.contains(i) => {
                    trace.has.remove(i);
                }
                _ => {}
            }
        }
        for saw in trace.saw.iter_mut() {
            if rng.below(10) == 0 {
                *saw = Fingerprint(saw.0 + 1);
            }
        }
        trace
    }

    #[test]
    fn under_and_over_across_pieces_answer_as_the_serial_join() {
        for seed in 0..12 {
            let scratch = seeded(seed, 40 + seed * 31, 60 + seed as usize * 50);
            let incremental = edited(&scratch, seed + 100);
            let other = seeded(seed + 1000, 40 + seed * 31, 60 + seed as usize * 50);
            let bytes = [&incremental, &scratch, &other].map(|t| blaze_protocol::cbor::to_vec(t).unwrap());
            let [inc_view, scratch_view, other_view] = [0, 1, 2].map(|k| View::parse(&bytes[k]).unwrap());
            let roots: Vec<QueryKey> = scratch.keys.iter().step_by(5).copied().collect();
            for roots in [&[][..], &roots[..2.min(roots.len())]] {
                for (inc, scr) in [(&incremental, &scratch), (&other, &scratch), (&scratch, &other)] {
                    let want = crate::under(inc, scr, roots);
                    for pieces in 1..=7 {
                        assert_eq!(under_in(inc, scr, roots, pieces), want, "seed {seed}, {pieces} pieces");
                    }
                }
                let want = crate::under(&incremental, &scratch, roots);
                for pieces in 1..=7 {
                    assert_eq!(under_in(&inc_view, &scratch_view, roots, pieces), want, "seed {seed} in place");
                }
            }
            for (before, after) in [(&scratch, &incremental), (&other, &incremental), (&incremental, &scratch)] {
                let want = crate::over(before, after);
                for pieces in 1..=7 {
                    assert_eq!(over_in(before, after, pieces), want, "seed {seed}, {pieces} pieces");
                }
            }
            let want = crate::over(&other, &incremental);
            for pieces in 1..=7 {
                assert_eq!(over_in(&other_view, &inc_view, pieces), want, "seed {seed} in place");
            }
        }
    }
}
