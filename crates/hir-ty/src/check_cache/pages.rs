//! A crate's bodies as one store value read an entry at a time (docs/v2/plan/design/restore.md,
//! "Bodies, inference and MIR as records"): the container. What an entry holds is its writer's
//! ([`super::runtime`]: a MIR owner's runtime body); here is only how one is found.
//!
//! | Segment | Holds | Read when |
//! |---|---|---|
//! | header ([`HEADER`] bytes) | the layout, how many buckets and entries, the value's length | the crate's value is opened |
//! | buckets ([`BUCKET`] bytes each) | [`WAYS`] slots: a tag of the entry's key, where the entry is, how long | an entry is looked up |
//! | entries | each its 32-byte key, then its bytes, at a multiple of [`ALIGN`] | that entry is looked up |
//!
//! A key is 32 bytes of a content hash ([`blaze_ids::Hasher`]), so its bytes are uniform: its
//! first eight choose one bucket, its next eight another, and the eight after are its tag. The
//! writer puts an entry in the emptier of its two buckets (the first on a tie), in the order of
//! the keys, and doubles the buckets until every entry has a slot: the bytes are a function of
//! the entries alone. A lookup reads at most the two buckets and the entries whose tag is the
//! key's (one, but for a tag met twice): at most two reads of 64 bytes and the entry's own
//! bytes, for a crate of any size.
//!
//! The bytes come through [`PageSource`], which holds them only while a read runs. Nothing
//! here is a database's id. A value of another layout, or cut short, is refused by name.

use blaze_format::{ALIGN, Aligned};
use hir_def::nameres::frozen::pages::PageSource;

type R<T = ()> = Result<T, String>;

/// An entry's key: 32 bytes of a content hash, the same in every database.
pub(crate) type Key = [u8; 32];

/// The first bytes of the value.
const MAGIC: [u8; 8] = *b"blzbody\0";

/// The layout of the value: changes with any change to the segments. A value of another is
/// refused naming both.
pub(crate) const LAYOUT: u32 = 1;

/// The header: magic, layout, buckets, entries, reserved, the value's length.
const HEADER: usize = 8 + 4 + 4 + 4 + 4 + 8;

/// How many slots a bucket has.
const WAYS: usize = 4;

/// A slot: tag, where the entry is in units of [`ALIGN`], its length (key included).
const SLOT: usize = 8 + 4 + 4;

const BUCKET: usize = WAYS * SLOT;

/// The fewest buckets a value has.
const MIN_BUCKETS: u32 = 2;

/// The most buckets a writer tries: past it the entries do not fit by a bug in their keys
/// (keys that are not uniform), never by their number, and the value is refused.
const MAX_BUCKETS: u32 = 1 << 28;

fn word(key: &Key, at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&key[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// The tag of `key` in its slot: never 0, which is an empty slot.
fn tag(key: &Key) -> u64 {
    word(key, 16) | 1
}

/// The two buckets of `key` among `buckets`, a power of two.
fn homes(key: &Key, buckets: u32) -> [u32; 2] {
    let mask = u64::from(buckets - 1);
    [(word(key, 0) & mask) as u32, (word(key, 8) & mask) as u32]
}

/// The value of `entries`, each a key and its bytes: sorted by key here, so the order they
/// are given in changes no byte. Refused when two entries have one key, when an entry or the
/// value is too long for a slot to name, and when the keys do not spread.
pub(crate) fn build(mut entries: Vec<(Key, Vec<u8>)>) -> R<Vec<u8>> {
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        // alloc: the error that fails the value
        return Err("two entries of a crate's bodies have one key".to_owned());
    }
    let count = u32::try_from(entries.len()).map_err(|_| "2^32 entries or more")?;
    // Half full at the start: two entries a bucket of four.
    let mut buckets = (count / 2).max(MIN_BUCKETS).next_power_of_two();
    let placed = loop {
        match place(&entries, buckets) {
            Some(placed) => break placed,
            None if buckets >= MAX_BUCKETS => {
                // alloc: the error that fails the value
                return Err(format!(
                    "{count} entries do not fit {MAX_BUCKETS} buckets: their keys are not uniform"
                ));
            }
            None => buckets *= 2,
        }
    };
    let table = HEADER + buckets as usize * BUCKET;
    let mut at = table.next_multiple_of(ALIGN);
    // alloc: the value, once per crate at store time
    let mut out = vec![0u8; at];
    // alloc: where each entry is, once per crate at store time
    let mut places = Vec::with_capacity(entries.len());
    for (key, bytes) in &entries {
        let len = key.len() + bytes.len();
        let unit = u32::try_from(at / ALIGN).map_err(|_| "a value of 64 GiB or more")?;
        let len32 = u32::try_from(len).map_err(|_| "an entry of 4 GiB or more")?;
        out.extend_from_slice(key);
        out.extend_from_slice(bytes);
        places.push((unit, len32));
        at = (at + len).next_multiple_of(ALIGN);
        out.resize(at, 0);
    }
    for (entry, &(bucket, way)) in placed.iter().enumerate() {
        let slot = HEADER + bucket as usize * BUCKET + way as usize * SLOT;
        let (unit, len) = places[entry];
        out[slot..slot + 8].copy_from_slice(&tag(&entries[entry].0).to_le_bytes());
        out[slot + 8..slot + 12].copy_from_slice(&unit.to_le_bytes());
        out[slot + 12..slot + 16].copy_from_slice(&len.to_le_bytes());
    }
    let size = out.len() as u64;
    out[0..8].copy_from_slice(&MAGIC);
    out[8..12].copy_from_slice(&LAYOUT.to_le_bytes());
    out[12..16].copy_from_slice(&buckets.to_le_bytes());
    out[16..20].copy_from_slice(&count.to_le_bytes());
    out[24..32].copy_from_slice(&size.to_le_bytes());
    Ok(out)
}

/// Each entry's bucket and slot among `buckets`; `None` when one has no slot in either of
/// its buckets.
fn place(entries: &[(Key, Vec<u8>)], buckets: u32) -> Option<Vec<(u32, u8)>> {
    // alloc: how full each bucket is, once per try at store time
    let mut filled = vec![0u8; buckets as usize];
    // alloc: each entry's place, once per try at store time
    let mut placed = Vec::with_capacity(entries.len());
    for (key, _) in entries {
        let [first, second] = homes(key, buckets);
        let bucket = match filled[second as usize] < filled[first as usize] {
            true => second,
            false => first,
        };
        let way = filled[bucket as usize];
        if usize::from(way) == WAYS {
            return None;
        }
        filled[bucket as usize] = way + 1;
        placed.push((bucket, way));
    }
    Some(placed)
}

/// A crate's bodies value, opened: its header read and checked, nothing else.
pub(crate) struct Pages<S> {
    source: S,
    buckets: u32,
    entries: u32,
    size: u64,
}

impl<S> std::fmt::Debug for Pages<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pages").field("entries", &self.entries).field("size", &self.size).finish()
    }
}

/// One read of `source`, its bytes copied out: a read holds its pages only while it runs.
fn read_into<S: PageSource + ?Sized>(source: &S, at: u64, out: &mut [u8]) -> R {
    let mut done = false;
    source.read(at, out.len() as u64, &mut |bytes| {
        if bytes.len() == out.len() {
            out.copy_from_slice(bytes);
            done = true;
        }
    })?;
    match done {
        true => Ok(()),
        // alloc: the reason of a refusal
        false => Err(format!("a read of {} bytes at {at} gave another length", out.len())),
    }
}

impl<S> Pages<S>
where
    S: std::ops::Deref,
    S::Target: PageSource,
{
    /// `source` as a crate's bodies value: its magic, layout, bucket count and length checked.
    /// A value of another layout is refused naming it and this one.
    pub(crate) fn open(source: S) -> R<Pages<S>> {
        let size = source.size();
        if size < HEADER as u64 {
            // alloc: the reason of a refusal
            return Err(format!("a bodies value of {size} bytes, shorter than its header"));
        }
        let mut header = [0u8; HEADER];
        read_into(&*source, 0, &mut header)?;
        if header[0..8] != MAGIC {
            // alloc: the reason of a refusal
            return Err("not a bodies value: its first bytes are another record's".to_owned());
        }
        let u32_at = |at: usize| {
            u32::from_le_bytes(header[at..at + 4].try_into().expect("four bytes"))
        };
        let layout = u32_at(8);
        if layout != LAYOUT {
            // alloc: the reason of a refusal
            return Err(format!(
                "a bodies value of layout {layout}; this blaze reads layout {LAYOUT}"
            ));
        }
        let (buckets, entries, reserved) = (u32_at(12), u32_at(16), u32_at(20));
        let said = u64::from_le_bytes(header[24..32].try_into().expect("eight bytes"));
        let table = HEADER as u64 + u64::from(buckets) * BUCKET as u64;
        let whole = buckets.is_power_of_two()
            && (MIN_BUCKETS..=MAX_BUCKETS).contains(&buckets)
            && reserved == 0
            && said == size
            && table <= size
            && u64::from(entries) <= u64::from(buckets) * WAYS as u64;
        if !whole {
            // alloc: the reason of a refusal
            return Err(format!(
                "a bodies value whose header does not hold: {buckets} buckets, {entries} \
                 entries, {said} bytes said and {size} there"
            ));
        }
        Ok(Pages { source, buckets, entries, size })
    }

    /// How many entries the value has.
    #[cfg(test)]
    pub(crate) fn len(&self) -> u32 {
        self.entries
    }

    /// The bytes of the entry whose key is `key`, aligned for `blaze_format` and held only by
    /// the caller; `None` when the value has none. A slot that names bytes outside the value,
    /// or a read that fails, is an error.
    pub(crate) fn find(&self, key: &Key) -> R<Option<Aligned>> {
        let [first, second] = homes(key, self.buckets);
        let tag = tag(key);
        for bucket in [Some(first), (second != first).then_some(second)].into_iter().flatten() {
            let mut slots = [0u8; BUCKET];
            read_into(&*self.source, HEADER as u64 + u64::from(bucket) * BUCKET as u64, &mut slots)?;
            for slot in slots.chunks_exact(SLOT) {
                let found = u64::from_le_bytes(slot[0..8].try_into().expect("eight bytes"));
                if found != tag {
                    continue;
                }
                let unit = u32::from_le_bytes(slot[8..12].try_into().expect("four bytes"));
                let len = u32::from_le_bytes(slot[12..16].try_into().expect("four bytes"));
                let at = u64::from(unit) * ALIGN as u64;
                let inside = len as usize >= key.len()
                    && at >= HEADER as u64 + u64::from(self.buckets) * BUCKET as u64
                    && at + u64::from(len) <= self.size;
                if !inside {
                    // alloc: the reason of a refusal
                    return Err(format!(
                        "a slot of bucket {bucket} names {len} bytes at {at}, outside the value"
                    ));
                }
                let mut entry = None;
                self.source.read(at, u64::from(len), &mut |bytes| {
                    // The key first: an entry of another key with this tag is passed over
                    // without a copy.
                    if bytes.len() == len as usize && bytes[..key.len()] == key[..] {
                        entry = Some(Aligned::copy_from(&bytes[key.len()..]));
                    }
                })?;
                if entry.is_some() {
                    return Ok(entry);
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use blaze_ids::Hasher;
    use hir_def::nameres::frozen::pages::Whole;

    use super::*;

    fn key(n: u32) -> Key {
        Hasher::new("blaze test bodies pages key").update(&n.to_le_bytes()).finish().0
    }

    fn bytes_of(n: u32) -> Vec<u8> {
        (0..(n % 97) + 1).map(|at| (n.wrapping_mul(31).wrapping_add(at)) as u8).collect()
    }

    fn entries(count: u32) -> Vec<(Key, Vec<u8>)> {
        (0..count).map(|n| (key(n), bytes_of(n))).collect()
    }

    fn whole(bytes: &[u8]) -> Box<Whole> {
        Box::new(Whole(Arc::new(Aligned::copy_from(bytes))))
    }

    /// A source that notes the 4 KiB pages each read touches.
    struct Counting {
        inner: Whole,
        pages: Mutex<Vec<u64>>,
    }

    impl PageSource for Counting {
        fn size(&self) -> u64 {
            self.inner.size()
        }

        // dyn: as `PageSource::read`
        fn read(&self, at: u64, len: u64, with: &mut dyn FnMut(&[u8])) -> Result<(), String> {
            let mut pages = self.pages.lock().unwrap();
            pages.extend(at / 4096..=(at + len.max(1) - 1) / 4096);
            drop(pages);
            self.inner.read(at, len, with)
        }
    }

    /// The round trip: every entry given is found with its bytes, and a key not given is not.
    #[test]
    fn every_entry_written_is_found_with_its_bytes_and_no_other_key_is() {
        for count in [0, 1, 2, 7, 500, 20_000] {
            let value = build(entries(count)).unwrap();
            let pages = Pages::open(whole(&value)).unwrap();
            assert_eq!(pages.len(), count);
            for n in 0..count {
                let found = pages.find(&key(n)).unwrap().unwrap_or_else(|| panic!("entry {n}"));
                assert_eq!(&found[..], &bytes_of(n)[..], "entry {n} of {count}");
            }
            for n in count..count + 50 {
                assert!(pages.find(&key(n)).unwrap().is_none(), "key {n} was never written");
            }
        }
    }

    /// The control of the round trip: a slot whose tag is damaged loses its entry, so the
    /// test above would see an entry left out of its bucket.
    #[test]
    fn the_oracle_catches_an_entry_left_out_of_its_bucket() {
        let count = 500;
        let mut value = build(entries(count)).unwrap();
        let buckets = u32::from_le_bytes(value[12..16].try_into().unwrap()) as usize;
        let slot = (0..buckets * WAYS)
            .map(|n| HEADER + n * SLOT)
            .find(|&at| value[at..at + 8] != [0; 8])
            .unwrap();
        value[slot + 1] ^= 0x40;
        let pages = Pages::open(whole(&value)).unwrap();
        let lost = (0..count).filter(|&n| pages.find(&key(n)).unwrap().is_none()).count();
        assert_eq!(lost, 1);
    }

    /// Deterministic bytes: the order the entries are given in changes none.
    #[test]
    fn the_same_entries_in_any_order_are_the_same_bytes() {
        let forward = entries(3_000);
        let mut backward = forward.clone(); // clone: the test's second order
        backward.reverse();
        let value = build(forward).unwrap();
        assert_eq!(value, build(backward).unwrap());
        let mut other = entries(3_000);
        other[17].1.push(1);
        assert_ne!(value, build(other).unwrap(), "another entry's bytes are other bytes");
    }

    /// A lookup touches a bounded number of pages whatever the crate's size.
    #[test]
    fn a_lookup_reads_a_bounded_number_of_pages() {
        for count in [40, 40_000] {
            let value = build(entries(count)).unwrap();
            let source = Arc::new(Counting {
                inner: Whole(Arc::new(Aligned::copy_from(&value))),
                pages: Mutex::new(Vec::new()),
            });
            let pages = Pages::open(Arc::clone(&source)).unwrap(); // clone: an `Arc`
            for n in (0..count).step_by(7) {
                source.pages.lock().unwrap().clear();
                let found = pages.find(&key(n)).unwrap().unwrap();
                let mut touched = source.pages.lock().unwrap().clone(); // clone: the test's count
                touched.sort_unstable();
                touched.dedup();
                // Two buckets (each within one page or across one boundary) and an entry of
                // under 4 KiB: at most six pages.
                let bound = 2 * 2 + (32 + found.len()).div_ceil(4096) + 1;
                assert!(touched.len() <= bound, "{} pages for entry {n} of {count}", touched.len());
            }
        }
    }

    /// The control of the bound: reading every entry touches pages by the value's size.
    #[test]
    fn reading_every_entry_reads_more_pages_than_one() {
        let count = 40_000;
        let value = build(entries(count)).unwrap();
        let source = Arc::new(Counting {
            inner: Whole(Arc::new(Aligned::copy_from(&value))),
            pages: Mutex::new(Vec::new()),
        });
        let pages = Pages::open(Arc::clone(&source)).unwrap(); // clone: an `Arc`
        for n in 0..count {
            pages.find(&key(n)).unwrap().unwrap();
        }
        let mut touched = source.pages.lock().unwrap().clone(); // clone: the test's count
        touched.sort_unstable();
        touched.dedup();
        assert!(touched.len() as u64 > value.len() as u64 / 4096 / 2);
    }

    #[test]
    fn a_previous_or_foreign_layout_is_refused_by_name() {
        let value = build(entries(10)).unwrap();
        let mut previous = value.clone(); // clone: the test's damaged copy
        previous[8..12].copy_from_slice(&(LAYOUT + 1).to_le_bytes());
        let why = Pages::open(whole(&previous)).unwrap_err();
        assert!(why.contains(&format!("layout {}", LAYOUT + 1)), "{why}");
        assert!(why.contains(&format!("reads layout {LAYOUT}")), "{why}");

        let mut foreign = value.clone(); // clone: as above
        foreign[0] ^= 1;
        assert!(Pages::open(whole(&foreign)).unwrap_err().contains("another record"));

        let cut = &value[..value.len() - ALIGN];
        assert!(Pages::open(whole(cut)).unwrap_err().contains("does not hold"));
        assert!(Pages::open(whole(&value[..HEADER - 1])).unwrap_err().contains("shorter"));
    }

    #[test]
    fn two_entries_of_one_key_are_refused() {
        let mut twice = entries(5);
        twice.push((key(2), vec![9]));
        assert!(build(twice).unwrap_err().contains("one key"));
    }
}
