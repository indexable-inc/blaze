//! `blaze run` on records (decision D106): a code record is FOUND by a key made from text and
//! today's facts BEFORE any check, and a found record is followed to the instances it declares
//! with nothing checked, lowered or named.
//!
//! THE SEAM ([`BodyIds`], agreed with the command's lane, PIPELINE runner 4): the command hands
//! over, per fn, its BodyCheck identity (the drive's task key for that body, made from the
//! item's text and the text around the bodies of its file) and the read list its BodyCheck
//! record holds, and answers what a fact is today. The ladder holds no drive type: a fact is 32
//! opaque bytes ([`Read`], the row a code record's read section holds).
//!
//! THE KEY ([`record_key`]) of one instance, from exactly:
//!
//! 1. the instance's SYMBOL (the hash of its instance name: its item's key and its ground
//!    arguments, symbols.rs);
//! 2. its fn's BODYCHECK KEY (text);
//! 3. that record's READ LIST, each fact with the digest it has today: what the check read
//!    (crate grain today: each crate scope it asked, with that scope's exports);
//! 4. the INSTANCE'S FACTS: for every item its name holds (an ADT, a fn item, a `dyn`'s traits,
//!    a closure's or a coroutine's owner), the crate fact of that item's crate
//!    ([`BodyIds::crate_fact`]): the layouts and the impls picked at the instance, which no
//!    body's check read;
//! 5. the target, the tier, the settings, the toolchain and the forms, as code_records.rs
//!    `code_key` writes them.
//!
//! This is the drive's value key (blaze-drive key.rs `value_key`: a task key and the digest each
//! read saw) for a code record: "valid by its key, or else by its read list checked against
//! today's facts" in one lookup. It replaces `code_records::body_digest(text, checker reads)`,
//! whose reads exist only AFTER the check, and the ground part of `KeyParts`, which needed the
//! checked body's types.
//!
//! WHEN A BODY HAS NO KEY (never a guess, never a silent whole rebuild: rule 3): the command
//! holds no BodyCheck record for it ([`Unkeyed::NoRecord`]), or the record it holds read a fact
//! that has moved ([`Unkeyed::Stale`]: the check would read other things today, so its list is
//! not today's). That ONE instance is made and NOT put, and the walk says so in its row
//! ([`How::Unkeyed`]) and its counts; the driver opens a span for each. Every other instance is
//! still found.
//!
//! THE WALK ([`Walk`]) is sans-IO: it asks the seam, the store ([`CodeStore`]), the maker
//! ([`Make`]: checks, lowers, builds, emits and seals ONE instance) and the entry's form
//! ([`Form`]) and holds only its table.
//!
//! THE TABLE is one copy, in the caller's allocator, as columns: per instance its symbol (32
//! bytes), key (32), tabled fn (16), name range (8) and outcome (1): 89 bytes a row, the names
//! in one arena. A found instance has no other content: no body, no checked tables. (Before:
//! front2 program.rs `Tabled`, about 6 KB a row with a `Body` and a `Checked` inline, the same
//! rows in three vectors.)
//!
//! OPEN, said rather than guessed: (a) a crate's fact must cover that crate's exports AND its
//! dependencies' (an argument type's field of a third crate): asked of the command's lane;
//! until it does, a record is stale after an edit of only such a field's type. (b) What the
//! build reads of a constant's value or a static's initializer is in no fact yet (code_records.rs
//! "NOT IN THE KEY" still holds for those two).

use core::fmt;

use blaze_code::record::{MAGIC, Symbol};
use blaze_ids::producer::{CodeBuilder, ProducerId};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, Vec};
pub use blaze_run::parent::Read;
use blaze_run::Tier;
use blaze_typeck::ItemKey;

use crate::code_records::{Builder, CodeKey, CodeStore, ENTRY_FORM, EntryError, EntryView, PanicStrategy, StoreError, triple};
use crate::symbols::fn_symbol_of_name;

const KEY_DOMAIN: &str = "blaze-ladder code record key 2";

/// One fn's BodyCheck identity, as the command read it.
#[derive(Clone, Copy, Debug)]
pub struct BodyId<'r> {
    /// The drive's BodyCheck task key for the body: from text, no check run.
    pub key: ContentId,
    /// The read list the BodyCheck record holds, sorted by `fact`, unique; EMPTY when the
    /// command holds no record for this body.
    pub reads: &'r [Read],
}

/// What the command hands the run path (the module doc, "The seam").
pub trait BodyIds {
    /// The fn whose checker key is `item`; `None`: the command has no such fn.
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>>;
    /// The digest `fact` has today; `None`: absent today.
    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]>;
    /// The crate-grain fact of the crate that defines `item`, with today's digest.
    fn crate_fact(&self, item: ItemKey) -> Option<Read>;
}

/// The key of the code record of the instance `symbol` (the module doc, "The key"). `body.reads`
/// are today's (the caller checked each against [`BodyIds::today`]); `facts` are sorted by fact
/// and unique.
/// `producer` is the code builder's identity (D105): code made by another builder is another
/// key, never read as current.
pub fn record_key(symbol: Symbol, body: &BodyId<'_>, facts: &[Read], builder: &Builder, producer: &ProducerId<CodeBuilder>) -> CodeKey {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.part(&symbol.0);
    h.id(&body.key);
    for list in [body.reads, facts] {
        h.update(&(list.len() as u64).to_le_bytes());
        for read in list {
            h.update(&read.fact);
            h.update(&read.digest);
        }
    }
    h.part(triple(builder.isa).as_bytes());
    h.part(&[u8::from(builder.isa.lse)]);
    let tier: u8 = match builder.tier {
        Tier::Fast => 0,
        Tier::Optimized => 1,
    };
    h.part(&[tier]);
    let s = builder.settings;
    let panic: u8 = match s.panic {
        PanicStrategy::Unwind => 0,
        PanicStrategy::Abort => 1,
    };
    h.part(&[u8::from(s.overflow_checks.on()), u8::from(s.debug_assertions), panic]);
    h.part(&builder.toolchain.0);
    h.part(&ENTRY_FORM.to_le_bytes());
    h.part(&MAGIC);
    h.id(producer.id());
    CodeKey(h.finish())
}

/// Why an instance name's items cannot be listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameError {
    /// The name ends inside a part, at byte `at`.
    Truncated { at: usize },
    /// A tag symbols.rs never writes, at byte `at`.
    Tag { tag: u8, at: usize },
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "an instance name is its item's key, a count and each argument's nodes (blaze-ladder symbols.rs `encode_instance`)";
        match *self {
            NameError::Truncated { at } => write!(f, "an instance name ends inside a part at byte {at}: {rule}"),
            NameError::Tag { tag, at } => write!(f, "an instance name holds the tag {tag} at byte {at}, which the form never writes: {rule}"),
        }
    }
}

impl core::error::Error for NameError {}

/// Each item an instance name holds, in the name's order (an item may come twice), WITHOUT a
/// ground table: the item itself, then per node the key its tag carries (symbols.rs' table:
/// 12 ADT, 13 fn item, 17 a `dyn`'s principal, 18 a `dyn`'s projection's trait, 19 auto trait,
/// 20 closure's owner, 23 coroutine's owner). The nodes are a flat pre-order stream, each a tag,
/// its payload and a child count, so no tree is rebuilt.
pub fn each_item(name: &[u8], mut each: impl FnMut(ItemKey)) -> Result<(), NameError> {
    let key_at = |at: usize| -> Result<ItemKey, NameError> {
        let bytes = name.get(at..at + 16).ok_or(NameError::Truncated { at })?;
        let mut wide = [0u8; 16];
        wide.copy_from_slice(bytes);
        Ok(ItemKey(u128::from_le_bytes(wide)))
    };
    each(key_at(0)?);
    // The item's key (16) and the count of arguments (4).
    let mut at = 20;
    if name.len() < at {
        return Err(NameError::Truncated { at: name.len() });
    }
    while at < name.len() {
        let tag = name[at];
        // (the payload's bytes, whether it starts with an item's key)
        let (payload, keyed) = match tag {
            0 | 1 | 5 | 6 | 7 | 10 | 11 | 15 | 16 => (0, false),
            2 | 3 | 4 | 8 | 9 | 21 => (1, false),
            12 | 13 | 17 | 19 => (16, true),
            14 => (16, false),
            18 => (24, true),
            20 | 23 => (20, true),
            22 => (4, false),
            _ => return Err(NameError::Tag { tag, at }),
        };
        if keyed {
            each(key_at(at + 1)?);
        }
        // The tag, the payload, the child count.
        let end = at + 1 + payload + 4;
        if end > name.len() {
            return Err(NameError::Truncated { at });
        }
        at = end;
    }
    Ok(())
}

/// The instances one entry declares, copied out of its bytes: (tabled fn, name) rows over one
/// text.
pub struct DeclaredList<'a> {
    rows: AVec<'a, (ItemKey, u32, u32)>,
    text: AVec<'a, u8>,
}

impl<'a> DeclaredList<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> DeclaredList<'a> {
        DeclaredList { rows: Vec::new_in(alloc), text: Vec::new_in(alloc) }
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.text.clear();
    }

    pub fn push(&mut self, tabled: ItemKey, name: &[u8]) {
        let at = self.text.len() as u32;
        self.text.extend_from_slice(name);
        self.rows.push((tabled, at, name.len() as u32));
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Row `i`: its tabled fn and its instance name.
    pub fn row(&self, i: usize) -> (ItemKey, &[u8]) {
        let (tabled, at, len) = self.rows[i];
        (tabled, &self.text[at as usize..(at + len) as usize])
    }
}

/// How an entry's bytes say what it declares: one path for a found entry and a made one.
pub trait Form {
    /// Appends each instance `entry` declares to `out`, in the entry's order.
    fn declared(&self, entry: &[u8], out: &mut DeclaredList<'_>) -> Result<(), EntryError>;
}

/// The run path's entry form (code_records.rs `make_entry`, read by `EntryView`).
pub struct EntryForm;

impl Form for EntryForm {
    fn declared(&self, entry: &[u8], out: &mut DeclaredList<'_>) -> Result<(), EntryError> {
        let view = EntryView::read(entry)?;
        view.each_declared(
            |d| {
                out.push(d.tabled, d.name);
                Ok(())
            },
            |e| e,
        )
    }
}

/// The on-demand side: the front end and the builder, made on first need. One call checks,
/// lowers, builds, emits and seals ONE instance; nothing else of the program is touched.
pub trait Make {
    type Error;
    /// Appends to `entry` the entry of the instance `name`, tabled as the fn `tabled`.
    fn make(&mut self, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), Self::Error>;
}

/// Why an instance was made without a key (the module doc).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unkeyed {
    /// The command holds no BodyCheck record for its fn.
    NoRecord,
    /// The BodyCheck record read `fact`, which is another digest (or absent) today.
    Stale { fact: [u8; 32] },
}

/// What the walk did with one instance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    /// Not reached by the loop yet.
    Waiting,
    /// Its entry was in the store under its key: nothing checked, lowered or built.
    Found,
    /// Made and put under its key.
    Made,
    /// Made and not put.
    Unkeyed(Unkeyed),
}

/// Why the walk stopped.
#[derive(Debug)]
pub enum WalkError<E> {
    /// The command has no fn for `tabled`, which an entry (or the caller) names.
    NoBody { tabled: ItemKey },
    /// The command has no crate fact for `item`, which the instance of `tabled` names.
    NoCrateFact { tabled: ItemKey, item: ItemKey },
    /// An instance's name does not list.
    Name { tabled: ItemKey, why: NameError },
    /// One symbol declared as tabled under two fns: an entry's declaration is wrong.
    TwoTabled { symbol: Symbol, first: ItemKey, second: ItemKey },
    /// The store refused a read or a write.
    Store(StoreError),
    /// An entry (found, or just made) does not read.
    Entry { tabled: ItemKey, found: bool, why: EntryError },
    /// The maker refused the instance of `tabled`.
    Make { tabled: ItemKey, why: E },
    /// More instances than `most` (rule 18).
    Cap { most: usize },
}

impl<E: fmt::Display> fmt::Display for WalkError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::NoBody { tabled } => write!(f, "the record walk reached fn {:#x}, and the command's body ids hold no such fn (blaze-ladder body_ids.rs `BodyIds::body_check`)", tabled.0),
            WalkError::NoCrateFact { tabled, item } => write!(f, "an instance of fn {:#x} names item {:#x}, whose crate has no fact (body_ids.rs `BodyIds::crate_fact`): its record's key would miss that crate (rule 21)", tabled.0, item.0),
            WalkError::Name { tabled, why } => write!(f, "an instance of fn {:#x}: {why}", tabled.0),
            WalkError::TwoTabled { symbol, first, second } => {
                write!(f, "the instance of symbol ")?;
                symbol.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))?;
                write!(f, " is declared as tabled under fn {:#x} and under fn {:#x}: one instance is built from one fn (body_ids.rs `Walk`)", first.0, second.0)
            }
            WalkError::Store(why) => why.fmt(f),
            WalkError::Entry { tabled, found: true, why } => write!(f, "the stored entry of an instance of fn {:#x}: {why}", tabled.0),
            WalkError::Entry { tabled, found: false, why } => write!(f, "the entry just made for an instance of fn {:#x}: {why}", tabled.0),
            WalkError::Make { tabled, why } => write!(f, "the instance of fn {:#x} was not made: {why}", tabled.0),
            WalkError::Cap { most } => write!(f, "the record walk met more than {most} instances (its cap, rule 18)"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> core::error::Error for WalkError<E> {}

/// A walk's counts: the line a run prints (`summary`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Instances reached.
    pub reached: usize,
    pub found: usize,
    /// Made and put.
    pub made: usize,
    /// Made and not put: no BodyCheck record.
    pub no_record: usize,
    /// Made and not put: a BodyCheck record whose read moved.
    pub stale: usize,
}

impl Counts {
    /// Calls of the maker: one per instance not found.
    pub fn make_calls(&self) -> usize {
        self.made + self.no_record + self.stale
    }
}

impl fmt::Display for Counts {
    /// `code records: reached R, found F, made M, unkeyed U (no record N, stale S)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "code records: reached {}, found {}, made {}, unkeyed {} (no record {}, stale {})",
            self.reached,
            self.found,
            self.made,
            self.no_record + self.stale,
            self.no_record,
            self.stale
        )
    }
}

/// The record walk's table (the module doc, "The table"): columns, row `n` the `n`th instance
/// reached.
pub struct Walk<'a> {
    alloc: Alloc<'a>,
    symbols: AVec<'a, Symbol>,
    /// All zero for an instance with no key ([`How::Unkeyed`]) or not reached yet.
    keys: AVec<'a, CodeKey>,
    tabled: AVec<'a, ItemKey>,
    /// Each name's (start, length) in `arena`.
    names: AVec<'a, (u32, u32)>,
    how: AVec<'a, How>,
    arena: AVec<'a, u8>,
    /// Open addressing over `symbols` by a symbol's first eight bytes (a symbol is a content
    /// digest: its bytes are the hash, so the table has no seed); 0 is empty, else the row + 1.
    /// A power of two long, at most half full.
    slots: AVec<'a, u32>,
}

const NO_KEY: CodeKey = CodeKey(ContentId([0; 32]));

impl<'a> Walk<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Walk<'a> {
        Walk {
            alloc,
            symbols: Vec::new_in(alloc),
            keys: Vec::new_in(alloc),
            tabled: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            how: Vec::new_in(alloc),
            arena: Vec::new_in(alloc),
            slots: Vec::new_in(alloc),
        }
    }

    /// Instances reached.
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    pub fn symbol(&self, row: usize) -> Symbol {
        self.symbols[row]
    }

    /// Row `row`'s key; `None` for an instance made without one.
    pub fn key(&self, row: usize) -> Option<CodeKey> {
        Some(self.keys[row]).filter(|key| *key != NO_KEY)
    }

    pub fn tabled(&self, row: usize) -> ItemKey {
        self.tabled[row]
    }

    pub fn name(&self, row: usize) -> &[u8] {
        let (at, len) = self.names[row];
        &self.arena[at as usize..(at + len) as usize]
    }

    pub fn how(&self, row: usize) -> How {
        self.how[row]
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts { reached: self.len(), ..Counts::default() };
        for how in self.how.iter() {
            match how {
                How::Waiting => {}
                How::Found => c.found += 1,
                How::Made => c.made += 1,
                How::Unkeyed(Unkeyed::NoRecord) => c.no_record += 1,
                How::Unkeyed(Unkeyed::Stale { .. }) => c.stale += 1,
            }
        }
        c
    }

    fn slot_of(symbol: &Symbol, mask: usize) -> usize {
        let mut first = [0u8; 8];
        first.copy_from_slice(&symbol.0[..8]);
        (u64::from_le_bytes(first) as usize) & mask
    }

    /// The row of `symbol`, or the empty slot it would take.
    fn find(&self, symbol: &Symbol) -> Result<usize, usize> {
        let mask = self.slots.len() - 1;
        let mut at = Walk::slot_of(symbol, mask);
        loop {
            match self.slots[at] {
                0 => return Err(at),
                n if self.symbols[n as usize - 1] == *symbol => return Ok(n as usize - 1),
                _ => at = (at + 1) & mask,
            }
        }
    }

    /// The slots doubled (16 at first) and every row filed again.
    fn grow(&mut self) {
        let len = (self.slots.len() * 2).max(16);
        let mut slots: AVec<'a, u32> = Vec::with_capacity_in(len, self.alloc);
        slots.resize(len, 0);
        self.slots = slots;
        for row in 0..self.symbols.len() {
            let symbol = self.symbols[row];
            if let Err(at) = self.find(&symbol) {
                self.slots[at] = row as u32 + 1;
            }
        }
    }

    /// The instance `name` tabled as `tabled`, reached: a new row, or the one it has.
    fn reach<E>(&mut self, tabled: ItemKey, name: &[u8], most: usize) -> Result<usize, WalkError<E>> {
        if (self.symbols.len() + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let symbol = fn_symbol_of_name(name);
        match self.find(&symbol) {
            Ok(row) if self.tabled[row] == tabled => Ok(row),
            Ok(row) => Err(WalkError::TwoTabled { symbol, first: self.tabled[row], second: tabled }),
            Err(_) if self.symbols.len() >= most => Err(WalkError::Cap { most }),
            Err(at) => {
                let row = self.symbols.len();
                self.slots[at] = row as u32 + 1;
                self.symbols.push(symbol);
                self.keys.push(NO_KEY);
                self.tabled.push(tabled);
                self.names.push((self.arena.len() as u32, name.len() as u32));
                self.arena.extend_from_slice(name);
                self.how.push(How::Waiting);
                Ok(row)
            }
        }
    }

    /// The walk from the instance `entry` (its tabled fn and its instance name; called again
    /// with another root, it goes on from what it holds): each instance keyed before any work,
    /// found in `store` or made by `make` and put, and what its entry declares reached in the
    /// entry's order. At most `most` instances.
    pub fn run<M: Make>(
        &mut self,
        entry: (ItemKey, &[u8]),
        ids: &dyn BodyIds,
        store: &mut dyn CodeStore,
        builder: &Builder,
        producer: &ProducerId<CodeBuilder>,
        make: &mut M,
        form: &dyn Form,
        most: usize,
    ) -> Result<(), WalkError<M::Error>> {
        let mut at = self.reach(entry.0, entry.1, most)?;
        let mut name: AVec<'a, u8> = Vec::new_in(self.alloc);
        let mut made: AVec<'a, u8> = Vec::new_in(self.alloc);
        let mut facts: AVec<'a, Read> = Vec::new_in(self.alloc);
        let mut items: AVec<'a, ItemKey> = Vec::new_in(self.alloc);
        let mut declared = DeclaredList::new_in(self.alloc);
        while at < self.len() {
            let row = at;
            at += 1;
            if self.how[row] != How::Waiting {
                continue;
            }
            let tabled = self.tabled[row];
            name.clear();
            name.extend_from_slice(self.name(row));
            let body = ids.body_check(tabled).ok_or(WalkError::NoBody { tabled })?;
            // Whether the instance has a key: its fn's BodyCheck record is there and every read
            // of it is today's.
            let mut unkeyed = if body.reads.is_empty() { Some(Unkeyed::NoRecord) } else { None };
            for read in body.reads {
                if unkeyed.is_none() && ids.today(&read.fact) != Some(read.digest) {
                    unkeyed = Some(Unkeyed::Stale { fact: read.fact });
                }
            }
            declared.clear();
            let mut found = false;
            let mut key = NO_KEY;
            if unkeyed.is_none() {
                items.clear();
                each_item(&name, |item| items.push(item)).map_err(|why| WalkError::Name { tabled, why })?;
                facts.clear();
                for &item in items.iter() {
                    facts.push(ids.crate_fact(item).ok_or(WalkError::NoCrateFact { tabled, item })?);
                }
                facts.sort_unstable();
                facts.dedup();
                key = record_key(self.symbols[row], &body, &facts, builder, producer);
                if let Some(bytes) = store.get(key).map_err(WalkError::Store)? {
                    form.declared(bytes, &mut declared).map_err(|why| WalkError::Entry { tabled, found: true, why })?;
                    found = true;
                }
            }
            if !found {
                made.clear();
                make.make(tabled, &name, &mut made).map_err(|why| WalkError::Make { tabled, why })?;
                form.declared(&made, &mut declared).map_err(|why| WalkError::Entry { tabled, found: false, why })?;
                if unkeyed.is_none() {
                    store.put(key, &made).map_err(WalkError::Store)?;
                }
            }
            self.keys[row] = key;
            self.how[row] = match (found, unkeyed) {
                (true, _) => How::Found,
                (false, None) => How::Made,
                (false, Some(why)) => How::Unkeyed(why),
            };
            for i in 0..declared.len() {
                let (callee, callee_name) = declared.row(i);
                self.reach(callee, callee_name, most)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The walk over a model of a two-crate program (rule 8, each claim with its control). The
    //! model: crate `app` (`main`, `helper`, the struct `App`) and crate `lib` (`twice<T>`,
    //! `plain`, `leaf`); `main` calls `helper`, `twice::<App>` and `plain`; `twice` and `plain`
    //! call `leaf`. Five instances. An entry here is its declared rows alone ([`Toy`]).

    use super::*;
    use crate::code_records::{MemoryStore, Settings};
    use blaze_emit::{Isa, Os};
    use blaze_toolchain::ToolchainId;

    const MAIN: u128 = 0xA001;
    const HELPER: u128 = 0xA002;
    const APP: u128 = 0xA0FF;
    const TWICE: u128 = 0xB001;
    const PLAIN: u128 = 0xB002;
    const LEAF: u128 = 0xB003;

    /// Each fn's callees: (fn, its one ADT argument).
    const CALLS: [(u128, &[(u128, Option<u128>)]); 5] =
        [(MAIN, &[(HELPER, None), (TWICE, Some(APP)), (PLAIN, None)]), (HELPER, &[]), (TWICE, &[(LEAF, None)]), (PLAIN, &[(LEAF, None)]), (LEAF, &[])];

    fn name_of(item: u128, arg: Option<u128>, out: &mut AVec<'_, u8>) {
        out.extend_from_slice(&item.to_le_bytes());
        match arg {
            None => out.extend_from_slice(&0u32.to_le_bytes()),
            Some(adt) => {
                out.extend_from_slice(&1u32.to_le_bytes());
                out.push(12);
                out.extend_from_slice(&adt.to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
            }
        }
    }

    /// An entry that is its declared rows: per row the tabled fn (16), the name's length (4)
    /// and bytes.
    struct Toy;

    impl Form for Toy {
        fn declared(&self, entry: &[u8], out: &mut DeclaredList<'_>) -> Result<(), EntryError> {
            let mut at = 0;
            while at < entry.len() {
                let mut wide = [0u8; 16];
                wide.copy_from_slice(&entry[at..at + 16]);
                let len = u32::from_le_bytes([entry[at + 16], entry[at + 17], entry[at + 18], entry[at + 19]]) as usize;
                out.push(ItemKey(u128::from_le_bytes(wide)), &entry[at + 20..at + 20 + len]);
                at += 20 + len;
            }
            Ok(())
        }
    }

    /// The maker: counts its calls (the model's `check_body` counter).
    struct Model {
        alloc: Alloc<'static>,
        calls: usize,
    }

    impl Make for Model {
        type Error = NameError;
        fn make(&mut self, tabled: ItemKey, _name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            self.calls += 1;
            let (_, callees) = CALLS.iter().find(|(f, _)| *f == tabled.0).ok_or(NameError::Tag { tag: 255, at: 0 })?;
            let mut name: AVec<'_, u8> = Vec::new_in(self.alloc);
            for &(callee, arg) in callees.iter() {
                name.clear();
                name_of(callee, arg, &mut name);
                entry.extend_from_slice(&callee.to_le_bytes());
                entry.extend_from_slice(&(name.len() as u32).to_le_bytes());
                entry.extend_from_slice(&name);
            }
            Ok(())
        }
    }

    /// The crate of a model item: `app`'s keys start 0xA0, `lib`'s 0xB0.
    fn crate_fact_name(item: u128) -> [u8; 32] {
        ContentId::of("d106 test crate", &[(item >> 8) as u8]).0
    }

    /// The command's side: each fn's text version, each crate's exports version, and the
    /// version of `lib`'s exports that `PLAIN`'s BodyCheck record saw (the stale case).
    struct Ids {
        /// (fn, the version of its text).
        text: [(u128, u8); 5],
        /// The exports version of `app` and of `lib`, today.
        exports: [u8; 2],
        /// Whether a body has a BodyCheck record.
        recorded: bool,
        /// The reads of every fn's record, made by `reads_of` at construction.
        reads: [[Read; 2]; 5],
    }

    fn exports_digest(version: u8) -> [u8; 32] {
        ContentId::of("d106 test exports", &[version]).0
    }

    impl Ids {
        /// Every body's record read both crates' scopes, except `helper`'s, which read `app`'s
        /// alone (its row holds that read twice: the walk takes the list as given, and the
        /// model keeps one row shape).
        fn new(exports: [u8; 2]) -> Ids {
            let app = Read { fact: crate_fact_name(MAIN), digest: exports_digest(exports[0]) };
            let lib = Read { fact: crate_fact_name(LEAF), digest: exports_digest(exports[1]) };
            let mut both = [app, lib];
            both.sort_unstable();
            Ids { text: [(MAIN, 0), (HELPER, 0), (TWICE, 0), (PLAIN, 0), (LEAF, 0)], exports, recorded: true, reads: [both, [app, app], both, both, both] }
        }

        fn row(&self, item: u128) -> Option<usize> {
            self.text.iter().position(|(f, _)| *f == item)
        }
    }

    impl BodyIds for Ids {
        fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
            let row = self.row(item.0)?;
            let mut text = [0u8; 17];
            text[..16].copy_from_slice(&item.0.to_le_bytes());
            text[16] = self.text[row].1;
            let reads: &[Read] = match (self.recorded, item.0 == HELPER) {
                (false, _) => &[],
                (true, true) => &self.reads[row][..1],
                (true, false) => &self.reads[row],
            };
            Some(BodyId { key: ContentId::of("d106 test body", &text), reads })
        }

        fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
            if *fact == crate_fact_name(MAIN) {
                Some(exports_digest(self.exports[0]))
            } else if *fact == crate_fact_name(LEAF) {
                Some(exports_digest(self.exports[1]))
            } else {
                None
            }
        }

        fn crate_fact(&self, item: ItemKey) -> Option<Read> {
            let fact = crate_fact_name(item.0);
            self.today(&fact).map(|digest| Read { fact, digest })
        }
    }

    fn builder() -> Builder {
        Builder {
            isa: Isa { os: Os::MacOs, lse: true },
            tier: Tier::Fast,
            settings: Settings::baseline(blaze_build::OverflowChecks::On),
            toolchain: ToolchainId([7; 32]),
        }
    }

    /// One run of the model program over `store`: its counts and the maker's calls.
    fn run_once(ids: &Ids, store: &mut MemoryStore<'_>) -> (Counts, usize) {
        let alloc = blaze_own::testing::heap().alloc();
        let mut walk = Walk::new_in(alloc);
        let mut model = Model { alloc, calls: 0 };
        let mut entry: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(MAIN, None, &mut entry);
        let producer = blaze_ids::producer::Producers::for_tests(b"d106 walk model").get::<CodeBuilder>();
        walk.run((ItemKey(MAIN), &entry), ids, store, &builder(), &producer, &mut model, &Toy, 64).expect("the model program walks");
        (walk.counts(), model.calls)
    }

    /// The second run finds every record and makes nothing; the control (no BodyCheck records:
    /// the seam's answers dropped) makes every reached instance again over the SAME full store.
    /// Prints "d106 twice: first found 0 made 5 calls 5; second found 5 made 0 calls 0; control
    /// found 0 unkeyed 5 calls 5".
    #[test]
    fn a_second_run_finds_every_record_and_checks_no_body() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut store = MemoryStore::new_in(alloc);
        let ids = Ids::new([0, 0]);
        let (first, first_calls) = run_once(&ids, &mut store);
        let (second, second_calls) = run_once(&ids, &mut store);
        let mut dropped = Ids::new([0, 0]);
        dropped.recorded = false;
        let (control, control_calls) = run_once(&dropped, &mut store);
        std::println!(
            "d106 twice: first found {} made {} calls {}; second found {} made {} calls {}; control found {} unkeyed {} calls {}",
            first.found,
            first.made,
            first_calls,
            second.found,
            second.made,
            second_calls,
            control.found,
            control.no_record,
            control_calls
        );
        assert_eq!((first.reached, first.found, first.made, first_calls), (5, 0, 5, 5));
        assert_eq!((second.reached, second.found, second.made, second_calls), (5, 5, 0, 0));
        assert_eq!((control.reached, control.found, control.no_record, control_calls), (5, 0, 5, 5));
        assert_eq!(store.puts, 5, "the control put nothing");
    }

    /// One body's text edited between runs (`leaf`'s BodyCheck key moves; no export moves):
    /// exactly `leaf`'s one instance is made; its callers' records are found, since a caller
    /// reads its callee's head (in the crate's exports), never its body. Then `lib`'s exports
    /// move (a head edit): every body whose check read `lib`'s scope is made again (`main`,
    /// `twice`, `plain`, `leaf`) and `helper`, which read `app` alone, is found.
    /// Prints "d106 edit: body edit found 4 made 1 calls 1; head edit found 1 made 4 calls 4".
    #[test]
    fn an_edited_body_is_the_one_instance_made_again() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut store = MemoryStore::new_in(alloc);
        let mut ids = Ids::new([0, 0]);
        run_once(&ids, &mut store);
        ids.text[4].1 = 1;
        let (body, body_calls) = run_once(&ids, &mut store);
        let mut moved = Ids::new([0, 1]);
        moved.text[4].1 = 1;
        let (head, head_calls) = run_once(&moved, &mut store);
        std::println!("d106 edit: body edit found {} made {} calls {}; head edit found {} made {} calls {}", body.found, body.made, body_calls, head.found, head.made, head_calls);
        assert_eq!((body.found, body.made, body_calls), (4, 1, 1));
        assert_eq!((head.found, head.made, head_calls), (1, 4, 4));
    }

    /// A BodyCheck record whose read moved is not today's list: its instance is made, said
    /// ([`Unkeyed::Stale`] naming the fact), and NOT put; the rest are found. `plain`'s record
    /// here still holds `lib`'s old exports while the others' were made again.
    /// Prints "d106 stale: found 1 made 3 stale 1 puts +3".
    #[test]
    fn a_stale_body_check_is_made_and_never_put() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut store = MemoryStore::new_in(alloc);
        run_once(&Ids::new([0, 0]), &mut store);
        let before = store.puts;
        let old = Ids::new([0, 0]);
        let mut ids = Ids::new([0, 1]);
        ids.reads[3] = old.reads[3];
        let (counts, calls) = run_once(&ids, &mut store);
        std::println!("d106 stale: found {} made {} stale {} puts +{}", counts.found, counts.made, counts.stale, store.puts - before);
        assert_eq!((counts.found, counts.made, counts.stale, calls), (1, 3, 1, 4));
        assert_eq!(store.puts - before, 3);
    }

    /// The items of a name are listed without a ground table, and a tag the form never writes
    /// is refused by its byte.
    #[test]
    fn a_name_lists_its_items() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut name: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(TWICE, Some(APP), &mut name);
        let mut items: AVec<'_, ItemKey> = Vec::new_in(alloc);
        each_item(&name, |item| items.push(item)).expect("a well-formed name");
        assert_eq!(items.as_slice(), &[ItemKey(TWICE), ItemKey(APP)]);
        name[20] = 99;
        assert_eq!(each_item(&name, |_| {}), Err(NameError::Tag { tag: 99, at: 20 }));
    }
}
