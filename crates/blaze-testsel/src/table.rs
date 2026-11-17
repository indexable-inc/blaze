//! A test binary's functions as selection sees them (docs/v2/plan/design/compile-time.md,
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
//! A [`Set`] is what a run's function asks are made from: the versions of the logged functions
//! a test executed (or reaches), sorted by identity, with the unlogged hash. [`Image::state`]
//! names the whole binary's functions as one program state, and [`Image::changed_since`] gives
//! the functions two states differ in (what a selection's `Changes` effect asks).

use std::{alloc::Allocator, fmt};

use blaze_ids::{ContentId, Hasher};

use crate::{
    ask::UNLOGGED,
    record::{Record, Table},
};

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
    /// The table lists more slots than its jump table has: `given`, of `slots`.
    PastTable { given: u32, slots: u32 },
    /// A record of another binary: the jump table it names is not this binary's. Never a
    /// test's doing: blaze ran a binary, or read a region, other than the one it holds the
    /// table of, so it stops rather than key a pass by another binary's slots.
    OtherBinary { record: Table, table: Table },
    /// A record hits a slot that names no function: one an incremental relink emptied
    /// (codegen's `ExecTable::EMPTY`), or one the link has not given out.
    EmptySlot { slot: u32 },
}

/// `table` as an error names it: its lineage in hex, its generation and its slot count.
struct Named<'a>(&'a Table);

impl fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("jump table ")?;
        for byte in &self.0.lineage {
            write!(f, "{byte:02x}")?;
        }
        write!(f, " generation {} of {} slots", self.0.generation, self.0.slots)
    }
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::NoFunction { slot, function } => {
                write!(
                    f,
                    "slot {slot} names function {function}, which the binary's table does not have"
                )
            }
            TableError::TwoSlots { function } => {
                write!(f, "function {function} of the binary's table has two slots")
            }
            TableError::Twice(id) => {
                write!(f, "two functions of the binary's table are function {}", id.0)
            }
            TableError::NotInTable(at) => {
                write!(f, "a test reaches function {at}, which the binary's table does not have")
            }
            TableError::PastTable { given, slots } => {
                write!(f, "the binary's table lists {given} slots of a jump table of {slots}")
            }
            TableError::OtherBinary { record, table } => {
                write!(
                    f,
                    "the execution record is of {}, and the binary's table of {}: blaze read the record \
                     of another binary than the one it ran (rule keys-cover-every-input: a record is \
                     read only against the table of the binary it is of)",
                    Named(record),
                    Named(table)
                )
            }
            TableError::EmptySlot { slot } => {
                write!(
                    f,
                    "the execution record hits slot {slot}, which names no function in the binary"
                )
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
                    other if fns[other as usize].id == version.id => {
                        return Err(TableError::Twice(version.id));
                    }
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
    /// Each slot's function: slot `n` is the jump table's `n`. As many as the link gave out,
    /// at most `table.slots`: a slot past them names no function.
    slot_fn: Vec<u32>,
    /// The jump table the slots are of: what a record must name to be read here.
    table: Table,
    /// Whether each function has a slot, by table index.
    logged: Vec<bool>,
    unlogged: ContentId,
    /// [`Image::state`].
    state: ContentId,
    lookup: Lookup,
    /// Table indices in identity order, which is [`Version`]'s, as identities are distinct.
    order: Vec<u32>,
    /// Each table index's place in `order`.
    rank: Vec<u32>,
}

pub const UNLOGGED_DOMAIN: &str = "blaze v2 testsel unlogged 1";
/// The domain of [`Image::state`].
pub const STATE_DOMAIN: &str = "blaze v2 testsel program state 1";

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
    let mut keyed: Vec<(u64, u32)> =
        fns.iter().enumerate().map(|(at, it)| (prefix(&it.id), at as u32)).collect();
    blaze_ids::radix::sort_pairs(&mut keyed);
    let mut start = 0;
    while start < keyed.len() {
        let mut end = start + 1;
        while end < keyed.len() && keyed[end].0 == keyed[start].0 {
            end += 1;
        }
        if end - start > 1 {
            keyed[start..end]
                .sort_unstable_by(|a, b| fns[a.1 as usize].id.cmp(&fns[b.1 as usize].id));
        }
        start = end;
    }
    // alloc: once per binary, the order kept
    keyed.into_iter().map(|it| it.1).collect()
}

impl Image {
    /// The table of a binary: its functions, each slot's function (of the slots its link gave
    /// out), `beside`, the content of the code it runs that is in no table (the premapped
    /// dependency image; zero for none), and `table`, the jump table its link made, as the
    /// binary's runtime names it.
    pub fn new(
        fns: Vec<Version>,
        slot_fn: Vec<u32>,
        beside: ContentId,
        table: Table,
    ) -> Result<Image, TableError> {
        if slot_fn.len() as u64 > u64::from(table.slots) {
            return Err(TableError::PastTable { given: slot_fn.len() as u32, slots: table.slots });
        }
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
        let mut hasher = Hasher::new(STATE_DOMAIN);
        hasher.id(&unlogged).update(&(fns.len() as u64).to_le_bytes());
        filled = 0;
        for &at in &order {
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
        let state = hasher.finish();
        Ok(Image { fns, slot_fn, table, logged, unlogged, state, lookup, order, rank })
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

    /// How many slots the link gave out: those the table names a function or an emptied slot
    /// for; the jump table has [`Image::table`]'s count.
    pub fn slots(&self) -> u32 {
        self.slot_fn.len() as u32
    }

    /// The jump table the slots are of.
    pub fn table(&self) -> Table {
        self.table
    }

    /// The bound code of the function `id` in this binary; `None` when it has none.
    pub fn bound_of(&self, id: &FnId) -> Option<Bound> {
        self.lookup.get(&self.fns, id).map(|at| self.fns[at as usize].bound)
    }

    /// The function of table index `at`.
    pub fn function(&self, at: u32) -> Option<&Version> {
        self.fns.get(at as usize)
    }

    /// The set a whole record of the slots of `table` says the test executed.
    ///
    /// The invariant: a record is read only against the table of the binary it is of, by
    /// identity: `table` (what the binary's runtime named) is this image's [`Image::table`]
    /// (what its link made), lineage, generation and slot count alike, or the record is
    /// refused ([`TableError::OtherBinary`]) before any slot of it is read. A slot count alone
    /// is no identity: every generation of an image has one capacity, and a count of slots
    /// given out is not what the runtime knows.
    pub fn executed(&self, table: Table, hits: &[u32]) -> Result<Set, TableError> {
        if table != self.table {
            return Err(TableError::OtherBinary { record: table, table: self.table });
        }
        // alloc: once per kept run
        let mut ranks = Vec::with_capacity(hits.len());
        for &hit in hits {
            // A slot past those given out names no function, as an emptied one does (and one
            // past the table, which no whole record holds: `Record::of_hits`).
            let function = self.slot_fn.get(hit as usize).copied().unwrap_or(NONE);
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
            Record::Complete { table, hits } => Some(self.executed(*table, hits)),
            Record::Incomplete { .. } => None,
        }
    }

    /// The program state this binary is: every function's version, in identity order, and the
    /// unlogged hash. What a suite's index head holds its rows at
    /// ([`crate::machine::Start::program`]).
    pub fn state(&self) -> ContentId {
        self.state
    }

    /// Into `out` (cleared first): every function whose bound code is not what it was in
    /// `before` (gone and new ones too), in identity order, then [`UNLOGGED`] when the
    /// unlogged hash moved. One merge of both identity orders: O(functions), nothing hashed.
    pub fn changed_since<A: Allocator>(&self, before: &Image, out: &mut Vec<FnId, A>) {
        out.clear();
        let (mut i, mut j) = (0, 0);
        loop {
            let now = self.order.get(i).map(|&at| &self.fns[at as usize]);
            let then = before.order.get(j).map(|&at| &before.fns[at as usize]);
            match (now, then) {
                (Some(now), Some(then)) if now.id == then.id => {
                    if now.bound != then.bound {
                        out.push(now.id);
                    }
                    i += 1;
                    j += 1;
                }
                (Some(now), Some(then)) if now.id < then.id => {
                    out.push(now.id);
                    i += 1;
                }
                (Some(_), Some(then)) | (None, Some(then)) => {
                    out.push(then.id);
                    j += 1;
                }
                (Some(now), None) => {
                    out.push(now.id);
                    i += 1;
                }
                (None, None) => break,
            }
        }
        if self.unlogged != before.unlogged {
            out.push(UNLOGGED);
        }
    }
}

/// The versions a run executed or reaches, sorted by identity, each once, and the unlogged hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Set {
    pub unlogged: ContentId,
    pub fns: Vec<Version>,
}

impl Set {
    pub fn of(unlogged: ContentId, mut fns: Vec<Version>) -> Set {
        fns.sort_unstable();
        fns.dedup();
        Set { unlogged, fns }
    }
}

#[cfg(test)]
pub(crate) mod tests;
