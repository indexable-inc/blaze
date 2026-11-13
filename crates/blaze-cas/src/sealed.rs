//! Item sets, the sealed form (docs/v2/plan/design/store.md, "Item sets: one value an item").
//!
//! A sealed set is one store value that holds many items: a header, a directory and each
//! item's key and bytes end to end. It is for the small values (16 to 200 bytes) of a crate
//! that is not edited under its version (the standard library, a registry crate) and for a
//! live set that has been quiet: a record each costs several times such a value (a header,
//! an index row, a key in a leaf), a place in a sealed set costs a directory slot, the key
//! and padding. To the index it is one row, to the collector one record, to a pin one key.
//!
//! An item is found by its own key: the directory is open addressing on the key's first
//! eight bytes, each slot the item's offset, length and four more bytes of its key, so a
//! lookup reads a run of slots and then the one item whose key it checks whole. Both are
//! read a part at a time ([`crate::Reader::part`]): the pages the slots and the item are on,
//! never the value.
//!
//! A sealed set is keyed by its own bytes ([`key_of`]), so two with the same items are one
//! record and a sealed set never changes under its key. What says which sealed sets a set
//! has is its root ([`crate::sets::Listing::Root`]).
//!
//! The layout (version 1; every number little-endian):
//!
//! ```text
//! 0   magic "BLZSLD\0" and the layout's number
//! 8   count of items, u32
//! 12  count of slots, u32
//! 16  the set, 32 bytes
//! 48  the slots, 16 bytes each: tag u32 (the key's bytes 8..12), length u32, offset u64
//!     (0: an empty slot)
//!     the items, each at an offset that is a multiple of 8: its key (32 bytes), its bytes,
//!     zeros to the next multiple of 8
//! ```
//!
//! The invariant: [`find`] of a key in the bytes [`encode`] made of some items gives that
//! key's bytes when it is one of them and `None` when it is not, reading at most
//! [`MAX_RUNS`] runs of slots and one item. Tested in `tests::sealed`.

use blaze_ids::ContentId;

use crate::Key;

/// The first bytes of a sealed set: its layout's number is the last.
pub const MAGIC: [u8; 8] = *b"BLZSLD\x00\x01";
const DOMAIN: &str = "blaze v2 store sealed set 1";
/// The bytes before the slots.
pub const FIXED: usize = 8 + 4 + 4 + 32;
/// A slot's bytes.
pub const SLOT: usize = 16;
/// The slots read at once: one part of the value, nearly always on one page.
pub const RUN: u32 = 8;
/// A lookup reads at most this many runs of slots. [`encode`] refuses a set an item of
/// which would be further from its place than these reach (it adds slots until none is), so
/// a lookup that reads them all without an answer is of a value [`encode`] did not make.
pub const MAX_RUNS: u32 = 64;

/// The key a sealed set of these bytes is kept under.
pub fn key_of(bytes: &[u8]) -> Key {
    Key(ContentId::of(DOMAIN, bytes))
}

/// A sealed set's first bytes, read: all a lookup needs to keep of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub count: u32,
    pub slots: u32,
    pub set: ContentId,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

impl Header {
    /// The header in a sealed set's first [`FIXED`] bytes, or why they are none. A sealed
    /// set of another layout is refused by its number, never read as empty.
    pub fn decode(bytes: &[u8]) -> Result<Header, String> {
        if bytes.len() < FIXED {
            // alloc: error path
            return Err(format!("{} bytes, under a sealed set's header", bytes.len()));
        }
        if bytes[..7] != MAGIC[..7] {
            return Err("not a sealed set: its first bytes are not a sealed set's".into()); // alloc: error path
        }
        if bytes[7] != MAGIC[7] {
            // alloc: error path
            return Err(format!(
                "a sealed set of layout {}, and this blaze reads layout {}",
                bytes[7], MAGIC[7]
            ));
        }
        let (count, slots) = (u32_at(bytes, 8), u32_at(bytes, 12));
        if slots <= count {
            // alloc: error path
            return Err(format!("a sealed set of {count} items in {slots} slots"));
        }
        Ok(Header { count, slots, set: ContentId(bytes[16..48].try_into().expect("32 bytes")) })
    }

    /// Where the items begin.
    pub fn items_from(&self) -> u64 {
        FIXED as u64 + self.slots as u64 * SLOT as u64
    }
}

/// The slot `key` is looked for at first.
fn home(key: &Key, slots: u32) -> u32 {
    (u64_at(&key.0.0, 0) % slots as u64) as u32
}

fn tag(key: &Key) -> u32 {
    u32_at(&key.0.0, 8)
}

fn padded(len: u64) -> u64 {
    len.div_ceil(8) * 8
}

/// The slots a set of `count` items has: a load of 3/4, where a lookup that finds its item
/// reads two or three slots and one that does not reads eight or nine.
pub fn slots_for(count: u32) -> u32 {
    (count as u64 * 4).div_ceil(3).max(count as u64 + 1).min(u32::MAX as u64) as u32
}

/// Places every item in `slots` slots (each slot: the item's number plus one, 0 empty), or
/// `None` when an item would be further from its home than a lookup reads.
fn place(items: &[(Key, &[u8])], slots: u32) -> Option<Vec<u32>> {
    // alloc: once a set sealed, 4 bytes a slot
    let mut table = vec![0u32; slots as usize];
    let reach = MAX_RUNS * RUN;
    for (n, (key, _)) in items.iter().enumerate() {
        let from = home(key, slots);
        let free = (0..reach.min(slots)).map(|step| (from + step) % slots).find(|at| table[*at as usize] == 0)?;
        table[free as usize] = n as u32 + 1;
    }
    Some(table)
}

/// A sealed set's bytes: `items` ascending by key, without repeats. The same items give the
/// same bytes. Refused, naming what: an item over 4 GiB, more items than a `u32` counts.
pub fn encode(set: &ContentId, items: &[(Key, &[u8])]) -> Result<Vec<u8>, String> {
    debug_assert!(items.windows(2).all(|it| it[0].0 < it[1].0), "a sealed set's keys ascend");
    let count = u32::try_from(items.len())
        // alloc: error path
        .map_err(|_| format!("a sealed set of {} items: more than it counts", items.len()))?;
    if count == u32::MAX {
        return Err("a sealed set of 2^32 - 1 items: more than it has slots for".into()); // alloc: error path
    }
    // More slots until every item is within a lookup's reach of its home: at a load of 3/4
    // the first count is enough but for keys an adversary chose.
    let mut slots = slots_for(count);
    let table = loop {
        match place(items, slots) {
            Some(table) => break table,
            None if slots > u32::MAX / 2 => {
                // alloc: error path
                return Err(format!("a sealed set of {count} items whose keys do not spread"));
            }
            None => slots *= 2,
        }
    };
    let items_from = FIXED as u64 + slots as u64 * SLOT as u64;
    let total = items_from + items.iter().map(|it| padded(32 + it.1.len() as u64)).sum::<u64>();
    // alloc: once a set sealed, its bytes
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&slots.to_le_bytes());
    out.extend_from_slice(&set.0);
    // alloc: once a set sealed, 8 bytes an item
    let mut offsets = Vec::with_capacity(items.len());
    let mut at = items_from;
    for (key, bytes) in items {
        if u32::try_from(bytes.len()).is_err() {
            // alloc: error path
            return Err(format!("the item {key:?} is {} bytes: over a sealed set's 4 GiB", bytes.len()));
        }
        offsets.push(at);
        at += padded(32 + bytes.len() as u64);
    }
    for slot in &table {
        match slot.checked_sub(1) {
            None => out.extend_from_slice(&[0; SLOT]),
            Some(n) => {
                let (key, bytes) = &items[n as usize];
                out.extend_from_slice(&tag(key).to_le_bytes());
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(&offsets[n as usize].to_le_bytes());
            }
        }
    }
    for (key, bytes) in items {
        out.extend_from_slice(&key.0.0);
        out.extend_from_slice(bytes);
        out.resize(padded(out.len() as u64) as usize, 0);
    }
    debug_assert_eq!(out.len() as u64, total);
    Ok(out)
}

/// Where a sealed set's bytes are read from, a part at a time.
pub trait Parts {
    /// The `len` bytes at `at`, or why they could not be read (a part past the end
    /// included).
    fn part(&self, at: u64, len: u64) -> Result<&[u8], String>;
}

impl Parts for [u8] {
    fn part(&self, at: u64, len: u64) -> Result<&[u8], String> {
        at.checked_add(len)
            .and_then(|end| self.get(usize::try_from(at).ok()?..usize::try_from(end).ok()?))
            // alloc: error path
            .ok_or_else(|| format!("bytes {at}+{len} were asked of its {} bytes", self.len()))
    }
}

/// What a lookup read: for the tests of its bound, and for whoever measures it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Probe {
    /// Parts read (runs of slots, then items whose tag matched).
    pub parts: u32,
    /// Their bytes.
    pub bytes: u64,
}

/// The bytes of `key` in the sealed set `from` holds, whose header is `header`: `None` when
/// it is not one of its items. Reads runs of [`RUN`] slots from the key's home until an
/// empty slot or the item, at most [`MAX_RUNS`] of them. An error names what in the value
/// is not a sealed set's: an offset outside the items, an item that runs past the end.
pub fn find<'a, P: Parts + ?Sized>(
    from: &'a P,
    header: &Header,
    key: &Key,
    probe: &mut Probe,
) -> Result<Option<&'a [u8]>, String> {
    let slots = header.slots;
    let mut at = home(key, slots);
    let mut left = slots.min(MAX_RUNS * RUN);
    while left > 0 {
        // A run ends at the last slot: the next begins at the first.
        let run = RUN.min(left).min(slots - at);
        let bytes = from.part(FIXED as u64 + at as u64 * SLOT as u64, run as u64 * SLOT as u64)?;
        probe.parts += 1;
        probe.bytes += bytes.len() as u64;
        for slot in bytes.chunks_exact(SLOT) {
            let (slot_tag, len, offset) = (u32_at(slot, 0), u32_at(slot, 4) as u64, u64_at(slot, 8));
            if offset == 0 {
                return Ok(None);
            }
            if slot_tag != tag(key) {
                continue;
            }
            if offset < header.items_from() || offset % 8 != 0 {
                // alloc: error path
                return Err(format!("a slot of the sealed set names offset {offset}, not an item's"));
            }
            let item = from.part(offset, 32 + len)?;
            probe.parts += 1;
            probe.bytes += item.len() as u64;
            if item[..32] == key.0.0 {
                return Ok(Some(&item[32..]));
            }
        }
        left -= run;
        at = (at + run) % slots;
    }
    Ok(None)
}

/// Every item of a sealed set read whole, ascending by key, with every byte accounted for:
/// the header, each slot naming one item and each item named by one slot, the items end to
/// end with zero padding and nothing after. For a compaction that merges it, for
/// `blaze store verify`, and for the tests.
pub fn decode(bytes: &[u8]) -> Result<(Header, Vec<(Key, &[u8])>), String> {
    let header = Header::decode(bytes)?;
    let items_from = header.items_from();
    if (bytes.len() as u64) < items_from {
        // alloc: error path
        return Err(format!("a sealed set of {} slots in {} bytes", header.slots, bytes.len()));
    }
    // alloc: once a sealed set read whole
    let mut items: Vec<(u64, Key, &[u8])> = Vec::with_capacity(header.count as usize);
    for slot in bytes[FIXED..items_from as usize].chunks_exact(SLOT) {
        let (slot_tag, len, offset) = (u32_at(slot, 0), u32_at(slot, 4) as u64, u64_at(slot, 8));
        if offset == 0 {
            if slot_tag != 0 || len != 0 {
                return Err("an empty slot of the sealed set is not zeros".into()); // alloc: error path
            }
            continue;
        }
        let item = bytes.part(offset, 32 + len)?;
        if offset < items_from || offset % 8 != 0 {
            // alloc: error path
            return Err(format!("a slot of the sealed set names offset {offset}, not an item's"));
        }
        let key = Key(ContentId(item[..32].try_into().expect("32 bytes")));
        if tag(&key) != slot_tag {
            // alloc: error path
            return Err(format!("the slot of the item {key:?} has another key's tag"));
        }
        items.push((offset, key, &item[32..]));
    }
    if items.len() != header.count as usize {
        // alloc: error path
        return Err(format!("a sealed set of {} items whose slots name {}", header.count, items.len()));
    }
    items.sort_unstable_by_key(|it| it.0);
    let mut at = items_from;
    for (offset, key, item) in &items {
        if *offset != at {
            // alloc: error path
            return Err(format!("the item {key:?} is at {offset}, where the one before ends at {at}"));
        }
        let end = at + 32 + item.len() as u64;
        at = padded(end);
        if bytes.len() as u64 >= at && bytes[end as usize..at as usize].iter().any(|it| *it != 0) {
            // alloc: error path
            return Err(format!("the padding after the item {key:?} is not zeros"));
        }
    }
    if at != bytes.len() as u64 {
        // alloc: error path
        return Err(format!("a sealed set of {} bytes whose items end at {at}", bytes.len()));
    }
    if !items.windows(2).all(|it| it[0].1 < it[1].1) {
        return Err("a sealed set whose items do not ascend by key".into()); // alloc: error path
    }
    // alloc: once a sealed set read whole
    Ok((header, items.into_iter().map(|(_, key, item)| (key, item)).collect()))
}
