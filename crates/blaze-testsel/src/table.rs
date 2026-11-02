//! A test binary's functions as selection sees them (docs/v2/plan/lanes/fastcompile/select.md,
//! "Keys"), made from what codegen says of the binary.
//!
//! Each function is a [`Version`]: its identity ([`FnId`], the same across edits of its body)
//! and its bound code ([`Bound`]: its code hash and what each of its refs binds to). A code
//! hash alone names each callee by its interface, so a caller edited to call another function
//! of the same signature keeps its code hash; its bound code does not. Functions called
//! through a jump-table slot are logged when they run; every other function (one called only
//! directly, the premapped dependencies) is in one hash, [`Image::unlogged`], which every set
//! carries: a change to any of them reruns every test.
//!
//! A [`Set`] is what a pass is kept under: the versions of the logged functions a test
//! executed (or reaches), sorted by identity, with the unlogged hash. It is stored by content;
//! [`SetView`] reads it in place.

use std::fmt;

use blaze_cas::Key;
use blaze_ids::{ContentId, Hasher};

use crate::record::Record;

/// A function by what it is across versions: its item and, for an instance, its type
/// arguments by identity (docs/v2/plan/design/compile-time.md, idea 15). A rename or a move
/// makes another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FnId(pub ContentId);

/// A function's code as it runs: its code hash, each function or method it refers to by its
/// [`FnId`], each constant and static it refers to by the deep hash of its value, for an
/// instance the facts of its type arguments its code reads, and the positions it names (the
/// content of each span table its code, its data and its debug lines name), as a test sees a
/// `line!()`, a panic's `Location` and a backtrace's lines only by running the code that names
/// them. A line added above a function moves it: its code hash stays and its bound does not.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Bound(pub ContentId);

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Version {
    pub id: FnId,
    pub bound: Bound,
}

const _: () = assert!(size_of::<Version>() == 64);

/// What [`Image::new`] refuses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TableError {
    /// A slot names a function the table does not have.
    NoFunction { slot: u32, function: u32 },
    /// Two slots name one function.
    TwoSlots { function: u32 },
    /// Two functions of the table have one identity.
    Twice(FnId),
    /// A test reaches a function the table does not have.
    NotInTable(u32),
    /// A record of another binary: its slot count is not the table's.
    OtherBinary { record: u32, table: u32 },
    /// A record hits a slot an incremental relink emptied (codegen's `ExecTable::EMPTY`).
    EmptySlot { slot: u32 },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::NoFunction { slot, function } => {
                write!(f, "slot {slot} names function {function}, which the binary's table does not have")
            }
            TableError::TwoSlots { function } => write!(f, "function {function} of the binary's table has two slots"),
            TableError::Twice(id) => write!(f, "two functions of the binary's table are function {}", id.0),
            TableError::NotInTable(at) => write!(f, "a test reaches function {at}, which the binary's table does not have"),
            TableError::OtherBinary { record, table } => {
                write!(f, "the execution record has {record} slots and the binary {table}: it is of another binary")
            }
            TableError::EmptySlot { slot } => {
                write!(f, "the execution record hits slot {slot}, which names no function in the binary")
            }
        }
    }
}

/// No function: an empty place of [`Lookup`], a slot of no function.
const NONE: u32 = u32::MAX;

/// Identity to table index, open-addressed by the identity's first eight bytes, which are a
/// BLAKE3 output's and so uniform: one probe, nothing hashed again.
struct Lookup {
    mask: usize,
    places: Vec<u32>,
}

impl Lookup {
    fn home(id: &FnId) -> usize {
        let b = &id.0.0;
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as usize
    }

    fn new(fns: &[Version]) -> Result<Lookup, TableError> {
        let size = (fns.len() * 2).next_power_of_two().max(8);
        // alloc: once per binary
        let mut lookup = Lookup { mask: size - 1, places: vec![NONE; size] };
        for (at, version) in fns.iter().enumerate() {
            let mut place = Lookup::home(&version.id) & lookup.mask;
            loop {
                match lookup.places[place] {
                    NONE => {
                        lookup.places[place] = at as u32;
                        break;
                    }
                    other if fns[other as usize].id == version.id => return Err(TableError::Twice(version.id)),
                    _ => place = (place + 1) & lookup.mask,
                }
            }
        }
        Ok(lookup)
    }

    fn get(&self, fns: &[Version], id: &FnId) -> Option<u32> {
        let mut place = Lookup::home(id) & self.mask;
        loop {
            match self.places[place] {
                NONE => return None,
                at if fns[at as usize].id == *id => return Some(at),
                _ => place = (place + 1) & self.mask,
            }
        }
    }
}

/// A test binary's functions.
pub struct Image {
    /// By the table's index, codegen's order.
    fns: Vec<Version>,
    /// Each slot's function: slot `n` is the jump table's `n`.
    slot_fn: Vec<u32>,
    /// Whether each function has a slot, by table index.
    logged: Vec<bool>,
    unlogged: ContentId,
    lookup: Lookup,
    /// Table indices in identity order, which is [`Version`]'s, as identities are distinct.
    order: Vec<u32>,
    /// Each table index's place in `order`.
    rank: Vec<u32>,
}

pub const UNLOGGED_DOMAIN: &str = "blaze v2 testsel unlogged 1";

/// Unlogged versions hashed per call: whole BLAKE3 chunks, which the hasher compresses many at
/// once with SIMD, where a call per 32 bytes compresses one block at a time.
const BATCH: usize = 256;

/// An identity's first eight bytes as an integer of their order: big-endian.
fn prefix(id: &FnId) -> u64 {
    let b = &id.0.0;
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// The table indices of `fns`, whose identities are distinct, in identity order: the table's
/// own when it is in that order already (one compare a function), else a radix sort by the
/// first eight bytes, then each run of one prefix (one in 2^64 for two BLAKE3 outputs) by the
/// whole identity.
fn by_identity(fns: &[Version]) -> Vec<u32> {
    if fns.windows(2).all(|it| it[0].id < it[1].id) {
        // alloc: once per binary, the order kept
        return (0..fns.len() as u32).collect();
    }
    // alloc: once per binary, the sort's keys
    let mut keyed: Vec<(u64, u32)> = fns.iter().enumerate().map(|(at, it)| (prefix(&it.id), at as u32)).collect();
    blaze_ids::radix::sort_pairs(&mut keyed);
    let mut start = 0;
    while start < keyed.len() {
        let mut end = start + 1;
        while end < keyed.len() && keyed[end].0 == keyed[start].0 {
            end += 1;
        }
        if end - start > 1 {
            keyed[start..end].sort_unstable_by(|a, b| fns[a.1 as usize].id.cmp(&fns[b.1 as usize].id));
        }
        start = end;
    }
    // alloc: once per binary, the order kept
    keyed.into_iter().map(|it| it.1).collect()
}

impl Image {
    /// The table of a binary: its functions, each slot's function, and `beside`, the content
    /// of the code it runs that is in no table (the premapped dependency image; zero for none).
    pub fn new(fns: Vec<Version>, slot_fn: Vec<u32>, beside: ContentId) -> Result<Image, TableError> {
        // alloc: once per binary
        let mut logged = vec![false; fns.len()];
        for (slot, &function) in slot_fn.iter().enumerate() {
            if function == NONE {
                continue;
            }
            let Some(it) = logged.get_mut(function as usize) else {
                return Err(TableError::NoFunction { slot: slot as u32, function });
            };
            if *it {
                return Err(TableError::TwoSlots { function });
            }
            *it = true;
        }
        let lookup = Lookup::new(&fns)?;
        let order = by_identity(&fns);
        // alloc: once per binary
        let mut rank = vec![0u32; fns.len()];
        for (place, &at) in order.iter().enumerate() {
            rank[at as usize] = place as u32;
        }
        let outside = logged.iter().filter(|it| !**it).count();
        let mut hasher = Hasher::new(UNLOGGED_DOMAIN);
        hasher.id(&beside).update(&(outside as u64).to_le_bytes());
        let mut batch = [0u8; 64 * BATCH];
        let mut filled = 0;
        for &at in &order {
            if logged[at as usize] {
                continue;
            }
            let version = &fns[at as usize];
            batch[filled..filled + 32].copy_from_slice(&version.id.0.0);
            batch[filled + 32..filled + 64].copy_from_slice(&version.bound.0.0);
            filled += 64;
            if filled == batch.len() {
                hasher.update(&batch);
                filled = 0;
            }
        }
        hasher.update(&batch[..filled]);
        let unlogged = hasher.finish();
        Ok(Image { fns, slot_fn, logged, unlogged, lookup, order, rank })
    }

    /// How many functions the binary's table has.
    pub fn len(&self) -> usize {
        self.fns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fns.is_empty()
    }

    /// The set of the functions of `ranks` (places in identity order, any order, repeats
    /// allowed), each once, in identity order: a bitset over every rank when they are many
    /// against the table, else a radix sort, whichever touches less.
    fn set_of_ranks(&self, mut ranks: Vec<u32>) -> Set {
        let words = self.fns.len().div_ceil(64);
        // alloc: the set the caller keeps, once per kept run
        let mut fns = Vec::with_capacity(ranks.len());
        if ranks.len() * 4 >= words {
            // alloc: once per kept run, a bit per function
            let mut bits = vec![0u64; words];
            for &rank in &ranks {
                bits[rank as usize / 64] |= 1u64 << (rank % 64);
            }
            for (word, &set) in bits.iter().enumerate() {
                let mut set = set;
                while set != 0 {
                    let rank = word * 64 + set.trailing_zeros() as usize;
                    fns.push(self.fns[self.order[rank] as usize]);
                    set &= set - 1;
                }
            }
        } else {
            blaze_ids::radix::sort(&mut ranks);
            ranks.dedup();
            fns.extend(ranks.iter().map(|&rank| self.fns[self.order[rank as usize] as usize]));
        }
        Set { unlogged: self.unlogged, fns }
    }

    /// The hash of every function called not through a slot, and of the code in no table.
    pub fn unlogged(&self) -> ContentId {
        self.unlogged
    }

    pub fn slots(&self) -> u32 {
        self.slot_fn.len() as u32
    }

    /// The bound code of the function `id` in this binary; `None` when it has none.
    pub fn bound_of(&self, id: &FnId) -> Option<Bound> {
        self.lookup.get(&self.fns, id).map(|at| self.fns[at as usize].bound)
    }

    /// The function of table index `at`.
    pub fn function(&self, at: u32) -> Option<&Version> {
        self.fns.get(at as usize)
    }

    /// The set a whole record says the test executed.
    pub fn executed(&self, slots: u32, hits: &[u32]) -> Result<Set, TableError> {
        if slots != self.slots() {
            return Err(TableError::OtherBinary { record: slots, table: self.slots() });
        }
        // alloc: once per kept run
        let mut ranks = Vec::with_capacity(hits.len());
        for &hit in hits {
            let Some(&function) = self.slot_fn.get(hit as usize) else {
                return Err(TableError::OtherBinary { record: slots, table: self.slots() });
            };
            if function == NONE {
                return Err(TableError::EmptySlot { slot: hit });
            }
            ranks.push(self.rank[function as usize]);
        }
        Ok(self.set_of_ranks(ranks))
    }

    /// The set of what codegen's walk says a test reaches (`reached`, by table index): its
    /// logged functions; the rest are in [`Image::unlogged`].
    pub fn reached(&self, reached: &[u32]) -> Result<Set, TableError> {
        // alloc: once per kept run
        let mut ranks = Vec::with_capacity(reached.len());
        for &at in reached {
            match self.logged.get(at as usize) {
                None => return Err(TableError::NotInTable(at)),
                Some(true) => ranks.push(self.rank[at as usize]),
                Some(false) => {}
            }
        }
        Ok(self.set_of_ranks(ranks))
    }

    /// The set a record keys a pass by: what it executed when whole.
    pub fn of_record(&self, record: &Record) -> Option<Result<Set, TableError>> {
        match record {
            Record::Complete { slots, hits } => Some(self.executed(*slots, hits)),
            Record::Incomplete { .. } => None,
        }
    }

    /// Whether every function of `set` is in this binary as it was: `None` when so, or what
    /// is not.
    pub fn changed(&self, set: &SetView<'_>) -> Option<Change> {
        if set.unlogged() != self.unlogged {
            return Some(Change::Unlogged);
        }
        for version in set.versions() {
            match self.bound_of(&version.id) {
                None => return Some(Change::Gone(version.id)),
                Some(bound) if bound != version.bound => return Some(Change::Changed(version.id)),
                Some(_) => {}
            }
        }
        None
    }
}

/// What of a set is not as it was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// This function's bound code changed.
    Changed(FnId),
    /// This function is no longer in the binary.
    Gone(FnId),
    /// Code called not through a slot changed.
    Unlogged,
}

/// The versions a pass is kept under, sorted by identity, each once, and the unlogged hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Set {
    pub unlogged: ContentId,
    pub fns: Vec<Version>,
}

const SET_MAGIC: &[u8; 16] = b"blaze v2 fnset 1";
const SET_HEAD: usize = 16 + 32 + 8;
pub const SET_DOMAIN: &str = "blaze v2 testsel set 1";

impl Set {
    pub fn of(unlogged: ContentId, mut fns: Vec<Version>) -> Set {
        fns.sort_unstable();
        fns.dedup();
        Set { unlogged, fns }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SET_HEAD + 64 * self.fns.len());
        out.extend_from_slice(SET_MAGIC);
        out.extend_from_slice(&self.unlogged.0);
        out.extend_from_slice(&(self.fns.len() as u64).to_le_bytes());
        for version in &self.fns {
            out.extend_from_slice(&version.id.0.0);
            out.extend_from_slice(&version.bound.0.0);
        }
        out
    }

    /// The store's key of an encoded set: its content.
    pub fn key_of(encoded: &[u8]) -> Key {
        Key(ContentId::of(SET_DOMAIN, encoded))
    }
}

/// An encoded [`Set`], read in place.
#[derive(Clone, Copy, Debug)]
pub struct SetView<'a> {
    bytes: &'a [u8],
}

impl<'a> SetView<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<SetView<'a>, String> {
        if bytes.len() < SET_HEAD || bytes[..16] != *SET_MAGIC {
            return Err("not a set of functions".to_owned()); // alloc: error path
        }
        let count = u64::from_le_bytes(bytes[48..56].try_into().expect("8 bytes"));
        if (bytes.len() - SET_HEAD) as u64 != count.saturating_mul(64) {
            // alloc: error path
            return Err("a set of functions holds another number of them than it says".to_owned());
        }
        Ok(SetView { bytes })
    }

    pub fn unlogged(&self) -> ContentId {
        ContentId(self.bytes[16..48].try_into().expect("32 bytes"))
    }

    pub fn len(&self) -> usize {
        (self.bytes.len() - SET_HEAD) / 64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn versions(&self) -> impl Iterator<Item = Version> + 'a {
        let bytes: &'a [u8] = self.bytes;
        bytes[SET_HEAD..].chunks_exact(64).map(|it| Version {
            id: FnId(ContentId(it[..32].try_into().expect("32 bytes"))),
            bound: Bound(ContentId(it[32..].try_into().expect("32 bytes"))),
        })
    }

    pub fn to_set(&self) -> Set {
        // alloc: a set to own, made only when one is asked for
        Set { unlogged: self.unlogged(), fns: self.versions().collect() }
    }
}

#[cfg(test)]
pub(crate) mod tests;
