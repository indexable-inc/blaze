//! A [`Trace`] as a file (`blaze trace --out`, compared by the `blaze-v2-proof` binary): one
//! deterministic CBOR map of nine entries, a version and eight byte strings, one per column, each
//! element little-endian and of fixed width. Reading one is nine CBOR items and a length check per
//! column, where a derived encoding is an item per byte of every key and fingerprint (35 items a
//! key, 17 an edge).
//!
//! [`View`] reads the columns in place, the file mapped, after one check across cores that every
//! index is in bounds and the keys are sorted; [`Trace`]'s `Deserialize` copies them out of the
//! same view. Writing one fills a single column buffer eight times over, in the order of the
//! keys' encoding, so the encoder writes the map straight through and never sorts it.

use blaze_ids::{
    ContentId, Fingerprint,
    bits::{self, BitSet},
};
use blaze_query::{QueryKey, QueryKind};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeStruct as _};

use crate::{
    Trace,
    join::{self, Columns},
};

/// The schema of the file; 1 was the derived encoding, an item per byte.
pub const VERSION: u64 = 2;

/// A column's bytes, written as one CBOR byte string and read back borrowed.
#[derive(Clone, Copy)]
struct Bytes<'a>(&'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_bytes(self.0)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Bytes<'a> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Bytes<'a>, D::Error> {
        <&'a [u8]>::deserialize(from).map(Bytes)
    }
}

/// The file: `n` keys and `m` edges. Read by its derive; written by [`Trace`]'s `Serialize`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File<'a> {
    version: u64,
    /// `n` × 2 bytes: each key's kind.
    #[serde(borrow)]
    kinds: Bytes<'a>,
    /// `n` × 32 bytes: each key's input.
    #[serde(borrow)]
    inputs: Bytes<'a>,
    /// ⌈n / 64⌉ × 8 bytes each: [`Trace::has`] and [`Trace::ran`].
    #[serde(borrow)]
    has: Bytes<'a>,
    #[serde(borrow)]
    ran: Bytes<'a>,
    /// `n` × 16 bytes.
    #[serde(borrow)]
    fingerprints: Bytes<'a>,
    /// (`n` + 1) × 4 bytes, or none when there are no keys.
    #[serde(borrow)]
    edges_from: Bytes<'a>,
    /// `m` × 4 and `m` × 16 bytes.
    #[serde(borrow)]
    reads: Bytes<'a>,
    #[serde(borrow)]
    saw: Bytes<'a>,
}

/// A trace read in place: each accessor is one unaligned little-endian load, so the columns are
/// never copied, and a key is built from its 34 bytes only when it is compared or named.
#[derive(Clone, Copy)]
pub struct View<'a> {
    kinds: &'a [[u8; 2]],
    inputs: &'a [[u8; 32]],
    has: &'a [[u8; 8]],
    ran: &'a [[u8; 8]],
    fingerprints: &'a [[u8; 16]],
    edges_from: &'a [[u8; 4]],
    reads: &'a [[u8; 4]],
    saw: &'a [[u8; 16]],
}

fn exact<'a, const N: usize>(bytes: Bytes<'a>, what: &str) -> Result<&'a [[u8; N]], String> {
    let (whole, rest) = bytes.0.as_chunks::<N>();
    if rest.is_empty() {
        Ok(whole)
    } else {
        // alloc: error path, and the trace is refused.
        Err(format!("the trace's `{what}` has {} bytes, not a multiple of {N}", bytes.0.len()))
    }
}

/// Bit `i` of mapped little-endian words, which need not be aligned for a `BitSlice`.
fn bit(words: &[[u8; 8]], i: usize) -> bool {
    let (word, mask) = bits::locate(i);
    u64::from_le_bytes(words[word]) & mask != 0
}

impl<'a> View<'a> {
    /// The trace `bytes` encode, checked: every column as long as the keys say, edge ranges in
    /// order and ending at the last edge, every edge reading a key of the trace, the keys strictly
    /// ascending. Nothing past the check can index out of bounds or join out of order.
    pub fn parse(bytes: &'a [u8]) -> Result<View<'a>, String> {
        // alloc: error path, and the trace is refused.
        let file: File<'a> = blaze_protocol::cbor::from_slice(bytes).map_err(|e| format!("the trace: {e}"))?;
        View::of(file)
    }

    fn of(file: File<'a>) -> Result<View<'a>, String> {
        if file.version != VERSION {
            // alloc: error path, and the trace is refused.
            return Err(format!("a trace of version {}; this reads version {VERSION}", file.version));
        }
        let view = View {
            kinds: exact(file.kinds, "kinds")?,
            inputs: exact(file.inputs, "inputs")?,
            has: exact(file.has, "has")?,
            ran: exact(file.ran, "ran")?,
            fingerprints: exact(file.fingerprints, "fingerprints")?,
            edges_from: exact(file.edges_from, "edges_from")?,
            reads: exact(file.reads, "reads")?,
            saw: exact(file.saw, "saw")?,
        };
        view.check()?;
        Ok(view)
    }

    fn from(&self, i: usize) -> usize {
        u32::from_le_bytes(self.edges_from[i]) as usize
    }

    fn check(&self) -> Result<(), String> {
        let (n, m) = (self.kinds.len(), self.reads.len());
        let words = bits::words_for(n);
        let lengths = [
            ("inputs", self.inputs.len(), n),
            ("has", self.has.len(), words),
            ("ran", self.ran.len(), words),
            ("fingerprints", self.fingerprints.len(), n),
            ("saw", self.saw.len(), m),
        ];
        if let Some((what, got, want)) = lengths.into_iter().find(|&(_, got, want)| got != want) {
            // alloc: error path, and the trace is refused.
            return Err(format!("the trace has {n} keys and {m} edges, and {got} elements of `{what}`, not {want}"));
        }
        if n == 0 && self.edges_from.is_empty() {
            // alloc: error path, and the trace is refused.
            return if m == 0 { Ok(()) } else { Err(format!("the trace has no keys and {m} edges")) };
        }
        if self.edges_from.len() != n + 1 || self.from(0) != 0 || self.from(n) != m {
            // alloc: error path, and the trace is refused.
            return Err(format!("the trace's `edges_from` does not run from 0 to its {m} edges over its {n} keys"));
        }
        let mut unsorted = None;
        join::split(
            n,
            join::pieces(n),
            |range| range.into_iter().find(|&i| self.from(i) > self.from(i + 1) || (i > 0 && self.key(i - 1) >= self.key(i))),
            |piece| unsorted = unsorted.or(piece),
        );
        if let Some(i) = unsorted {
            // alloc: error path, and the trace is refused.
            return Err(format!("the trace's key {i} is not above the one before it, or its edges end before they start"));
        }
        let mut highest = None;
        join::split(
            m,
            join::pieces(m),
            |range| range.into_iter().map(|e| u32::from_le_bytes(self.reads[e])).max(),
            |piece| highest = highest.max(piece),
        );
        match highest {
            // alloc: error path, and the trace is refused.
            Some(read) if read as usize >= n => Err(format!("an edge of the trace reads key {read}; it has {n}")),
            _ => Ok(()),
        }
    }

    /// The trace as owned columns: one copy of each, each allocated once at its length.
    pub fn to_trace(&self) -> Trace {
        let words = |column: &[[u8; 8]]| -> BitSet<u32> {
            // alloc: the owned copy is the answer, once a trace.
            BitSet::from_words(column.iter().map(|w| u64::from_le_bytes(*w)).collect())
        };
        let fingerprints = |column: &[[u8; 16]]| -> Vec<Fingerprint> {
            // alloc: as above.
            column.iter().map(|f| Fingerprint(u128::from_le_bytes(*f))).collect()
        };
        // alloc: as above.
        let indices = |column: &[[u8; 4]]| -> Vec<u32> { column.iter().map(|w| u32::from_le_bytes(*w)).collect() };
        Trace {
            // alloc: as above.
            keys: (0..self.kinds.len()).map(|i| self.key(i)).collect(),
            has: words(self.has),
            ran: words(self.ran),
            fingerprints: fingerprints(self.fingerprints),
            edges_from: indices(self.edges_from),
            reads: indices(self.reads),
            saw: fingerprints(self.saw),
        }
    }
}

impl Columns for View<'_> {
    fn keys(&self) -> usize {
        self.kinds.len()
    }

    fn key(&self, i: usize) -> QueryKey {
        QueryKey { kind: QueryKind(u16::from_le_bytes(self.kinds[i])), input: ContentId(self.inputs[i]) }
    }

    fn has(&self, i: usize) -> bool {
        bit(self.has, i)
    }

    fn ran(&self, i: usize) -> bool {
        bit(self.ran, i)
    }

    fn fingerprint(&self, i: usize) -> Fingerprint {
        Fingerprint(u128::from_le_bytes(self.fingerprints[i]))
    }

    fn edges_of(&self, i: usize) -> std::ops::Range<usize> {
        self.from(i)..self.from(i + 1)
    }

    fn read(&self, e: usize) -> u32 {
        u32::from_le_bytes(self.reads[e])
    }

    fn saw(&self, e: usize) -> Fingerprint {
        Fingerprint(u128::from_le_bytes(self.saw[e]))
    }

    fn has_word(&self, w: usize) -> u64 {
        self.has.get(w).map_or(0, |word| u64::from_le_bytes(*word))
    }

    fn ran_word(&self, w: usize) -> u64 {
        self.ran.get(w).map_or(0, |word| u64::from_le_bytes(*word))
    }
}

/// `items` as `column`, each one's fixed-width little-endian bytes after the last's, written over
/// what `column` held.
fn fill<'c, T, const N: usize>(column: &'c mut Vec<u8>, items: &[T], bytes: impl Fn(&T) -> [u8; N]) -> Bytes<'c> {
    column.clear();
    for item in items {
        column.extend_from_slice(&bytes(item));
    }
    Bytes(column)
}

/// A bitset's words, for [`fill`].
fn words(set: &BitSet<u32>) -> &[u64] {
    set.words()
}

impl Serialize for Trace {
    /// [`File`]'s fields in the order of their keys' encoding (shorter first, then bytewise), the
    /// order the deterministic encoding gives them: the encoder writes each straight through, where
    /// fields in declaration order would be put in order at the end through a copy the size of the
    /// whole trace. Each column is filled into the one buffer, sized once for the longest, and
    /// written before the next is filled.
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        let (has, ran) = (words(&self.has), words(&self.ran));
        let longest = [
            2 * self.keys.len(),
            32 * self.keys.len(),
            8 * has.len(),
            8 * ran.len(),
            16 * self.fingerprints.len(),
            4 * self.edges_from.len(),
            4 * self.reads.len(),
            16 * self.saw.len(),
        ];
        // alloc: once a trace written, the one buffer every column is filled into.
        let mut column = Vec::with_capacity(longest.into_iter().max().unwrap_or(0));
        let mut file = to.serialize_struct("File", 9)?;
        file.serialize_field("has", &fill(&mut column, has, |w| w.to_le_bytes()))?;
        file.serialize_field("ran", &fill(&mut column, ran, |w| w.to_le_bytes()))?;
        file.serialize_field("saw", &fill(&mut column, &self.saw, |f| f.0.to_le_bytes()))?;
        file.serialize_field("kinds", &fill(&mut column, &self.keys, |k| k.kind.0.to_le_bytes()))?;
        file.serialize_field("reads", &fill(&mut column, &self.reads, |r| r.to_le_bytes()))?;
        file.serialize_field("inputs", &fill(&mut column, &self.keys, |k| k.input.0))?;
        file.serialize_field("version", &VERSION)?;
        file.serialize_field("edges_from", &fill(&mut column, &self.edges_from, |e| e.to_le_bytes()))?;
        file.serialize_field("fingerprints", &fill(&mut column, &self.fingerprints, |f| f.0.to_le_bytes()))?;
        file.end()
    }
}

impl<'de> Deserialize<'de> for Trace {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Trace, D::Error> {
        let file = File::deserialize(from)?;
        View::of(file).map(|view| view.to_trace()).map_err(de::Error::custom)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use blaze_ids::{ContentId, Fingerprint};
    use blaze_query::{Edge, QueryKey, QueryKind};

    use super::{Bytes, View};
    use crate::{Rng, Trace, TraceBuilder, join::Columns};

    /// A trace of `results` results over `keys` keys, each reading up to 6 of them, by `seed`.
    pub(crate) fn seeded(seed: u64, keys: u64, results: usize) -> Trace {
        let mut rng = Rng::new(seed);
        let key = |n: u64| QueryKey {
            kind: QueryKind(0x2000 + (n % 3) as u16),
            input: ContentId::of("blaze v2 proof wire test key 1", &n.to_le_bytes()),
        };
        let mut builder = TraceBuilder::default();
        for _ in 0..results {
            let edges: Vec<Edge> = (0..rng.below(7))
                .map(|_| Edge { read: key(rng.below(keys)), saw: Fingerprint(u128::from(rng.below(4))) })
                .collect();
            builder.insert(key(rng.below(keys)), Fingerprint(u128::from(rng.below(4))), &edges, rng.below(2) == 0);
        }
        builder.finish()
    }

    #[test]
    fn a_trace_round_trips_through_its_columns() {
        for seed in 0..40 {
            let trace = seeded(seed, 1 + seed * 7, (seed * 13) as usize);
            let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
            blaze_protocol::cbor::check(&bytes).unwrap();
            let back: Trace = blaze_protocol::cbor::from_slice(&bytes).unwrap();
            assert_eq!(back, trace, "seed {seed}");
            let view = View::parse(&bytes).unwrap();
            assert_eq!(view.to_trace(), trace, "seed {seed}");
        }
    }

    #[test]
    fn the_view_answers_what_the_trace_does() {
        let trace = seeded(5, 300, 900);
        let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
        let view = View::parse(&bytes).unwrap();
        assert_eq!(view.keys(), trace.keys());
        for i in 0..trace.keys() {
            assert_eq!(
                (view.key(i), view.has(i), view.ran(i), view.fingerprint(i), view.edges_of(i)),
                (trace.key(i), trace.has(i), trace.ran(i), trace.fingerprint(i), trace.edges_of(i))
            );
        }
        for e in 0..trace.reads.len() {
            assert_eq!((view.read(e), view.saw(e)), (trace.read(e), trace.saw(e)));
        }
        for w in 0..=blaze_ids::bits::words_for(trace.keys()) {
            assert_eq!((view.has_word(w), view.ran_word(w)), (trace.has_word(w), trace.ran_word(w)), "word {w}");
        }
    }

    /// The file as `Trace` wrote it before: [`super::File`]'s fields in declaration order, each
    /// column a buffer of its own, which the encoder puts in key order at the end.
    #[derive(serde::Serialize)]
    struct Declared<'a> {
        version: u64,
        kinds: Bytes<'a>,
        inputs: Bytes<'a>,
        has: Bytes<'a>,
        ran: Bytes<'a>,
        fingerprints: Bytes<'a>,
        edges_from: Bytes<'a>,
        reads: Bytes<'a>,
        saw: Bytes<'a>,
    }

    fn declared(trace: &Trace) -> (Vec<u8>, u64) {
        let le = |words: &[u64]| words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        let wide = |column: &[Fingerprint]| column.iter().flat_map(|f| f.0.to_le_bytes()).collect::<Vec<u8>>();
        let narrow = |column: &[u32]| column.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        let kinds: Vec<u8> = trace.keys.iter().flat_map(|k| k.kind.0.to_le_bytes()).collect();
        let inputs: Vec<u8> = trace.keys.iter().flat_map(|k| k.input.0).collect();
        let (has, ran) = (le(super::words(&trace.has)), le(super::words(&trace.ran)));
        let (fingerprints, saw) = (wide(&trace.fingerprints), wide(&trace.saw));
        let (edges_from, reads) = (narrow(&trace.edges_from), narrow(&trace.reads));
        let file = Declared {
            version: super::VERSION,
            kinds: Bytes(&kinds),
            inputs: Bytes(&inputs),
            has: Bytes(&has),
            ran: Bytes(&ran),
            fingerprints: Bytes(&fingerprints),
            edges_from: Bytes(&edges_from),
            reads: Bytes(&reads),
            saw: Bytes(&saw),
        };
        let (mut writer, mut out) = (blaze_protocol::cbor::Writer::new(), Vec::new());
        writer.append(&mut out, &file).unwrap();
        (out, writer.permuted())
    }

    #[test]
    fn a_trace_is_written_in_key_order_as_the_same_bytes() {
        for seed in 0..20 {
            let trace = seeded(seed, 1 + seed * 11, (seed * 17) as usize);
            for trace in [trace, Trace::default(), TraceBuilder::default().finish()] {
                let (want, permuted) = declared(&trace);
                assert_eq!(permuted, 1, "seed {seed}: the declaration order is not the keys'");
                let (mut writer, mut out) = (blaze_protocol::cbor::Writer::new(), Vec::new());
                writer.append(&mut out, &trace).unwrap();
                assert_eq!(out, want, "seed {seed}");
                assert_eq!(writer.permuted(), 0, "seed {seed}: the fields came out of key order");
            }
        }
    }

    /// Writing a trace costs its one column buffer; reading one in place costs nothing.
    #[test]
    fn a_trace_is_written_with_one_buffer_and_read_with_none() {
        let trace = seeded(12, 2000, 6000);
        let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
        let (mut writer, mut out) = (blaze_protocol::cbor::Writer::new(), Vec::with_capacity(bytes.len()));
        blaze_alloc_count::assert_at_most(1, "a trace written: its one column buffer", || {
            writer.append(&mut out, &trace).unwrap()
        });
        assert_eq!(out, bytes);
        View::parse(&bytes).unwrap();
        let view = blaze_alloc_count::assert_at_most(0, "a trace parsed and checked in place, one piece", || {
            View::parse(&bytes).unwrap()
        });
        assert_eq!(view.keys(), trace.keys());
        let owned = blaze_alloc_count::assert_at_most(8, "a trace copied out: one allocation a column", || {
            view.to_trace()
        });
        assert_eq!(owned, trace);
    }

    #[test]
    fn an_empty_trace_round_trips() {
        for trace in [Trace::default(), TraceBuilder::default().finish()] {
            let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
            let back: Trace = blaze_protocol::cbor::from_slice(&bytes).unwrap();
            assert_eq!(back, trace);
        }
    }

    #[test]
    fn a_trace_out_of_bounds_or_order_is_refused() {
        let good = seeded(9, 50, 200);
        let refused = |trace: &Trace, what: &str| {
            let bytes = blaze_protocol::cbor::to_vec(trace).unwrap();
            let said = View::parse(&bytes).err().unwrap_or_else(|| panic!("{what}: parsed"));
            assert!(said.contains(what), "{what}: {said}");
        };
        let mut reads = good.clone();
        reads.reads[0] = reads.keys.len() as u32;
        refused(&reads, "reads key");
        let mut order = good.clone();
        order.keys.swap(0, 1);
        refused(&order, "not above");
        let mut ends = good.clone();
        *ends.edges_from.last_mut().unwrap() += 1;
        refused(&ends, "edges_from");
        let mut short = good.clone();
        short.fingerprints.pop();
        refused(&short, "fingerprints");
        let mut backwards = good;
        let mid = backwards.edges_from.len() / 2;
        backwards.edges_from[mid] = backwards.edges_from[mid + 1] + 1;
        refused(&backwards, "end before");
    }
}
