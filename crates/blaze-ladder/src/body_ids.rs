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
//! ([`Form`]) and holds only its table. Its one step is [`Walk::one`]: ONE row keyed, found or
//! made, and what its entry declares reached as rows with nothing done for them (D124: the
//! `blaze run` parent answers one `Need` at a time with it and never walks ahead of what is
//! asked). [`Walk::run`], the walk to a fixpoint, is [`Walk::root`] then `one` over the rows in
//! order.
//!
//! THE TABLE is one copy, in the caller's allocator, as columns: per instance its symbol (32
//! bytes), key (32), tabled fn (16), name range (8), outcome (1) and the range of the rows its
//! entry declares (8): 97 bytes a row, the names in one arena, the declared rows in another (4
//! bytes each). A found instance has no other content: no body, no checked tables. (Before:
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
use blaze_typeck::ItemKey;

use crate::code_records::statics::{STATIC_ENTRY_MAGIC, StaticEntryView};
use crate::code_records::{Builder, CodeKey, CodeStore, ENTRY_FORM, EntryError, EntryView, PanicStrategy, StoreError, triple};
use crate::symbols::{fn_symbol_of_name, static_symbol, tls_symbol};

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
    h.part(&[builder.tier as u8]);
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
/// text; and the statics and thread-locals it declares, by item key.
pub struct DeclaredList<'a> {
    rows: AVec<'a, (ItemKey, u32, u32)>,
    text: AVec<'a, u8>,
    statics: AVec<'a, ItemKey>,
    thread_locals: AVec<'a, ItemKey>,
}

impl<'a> DeclaredList<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> DeclaredList<'a> {
        DeclaredList { rows: Vec::new_in(alloc), text: Vec::new_in(alloc), statics: Vec::new_in(alloc), thread_locals: Vec::new_in(alloc) }
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.text.clear();
        self.statics.clear();
        self.thread_locals.clear();
    }

    /// A thread-local the entry declares (its record names it): its template is a row.
    pub fn push_thread_local(&mut self, item: ItemKey) {
        self.thread_locals.push(item);
    }

    /// The declared thread-locals, in the entry's order.
    pub fn thread_locals(&self) -> &[ItemKey] {
        &self.thread_locals
    }

    pub fn push(&mut self, tabled: ItemKey, name: &[u8]) {
        let at = self.text.len() as u32;
        self.text.extend_from_slice(name);
        self.rows.push((tabled, at, name.len() as u32));
    }

    /// A static the entry declares (its record names it).
    pub fn push_static(&mut self, item: ItemKey) {
        self.statics.push(item);
    }

    /// The declared statics, in the entry's order.
    pub fn statics(&self) -> &[ItemKey] {
        &self.statics
    }

    /// The count of declared instances (the statics are [`DeclaredList::statics`]).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether it declares no instance, no static and no thread-local.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.statics.is_empty() && self.thread_locals.is_empty()
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

/// The run path's entry form: a code record's (code_records.rs `make_entry`, read by
/// `EntryView`) or a static's (code_records.rs `statics::make_static_entry`, read by
/// `StaticEntryView`), told by the magic.
pub struct EntryForm;

impl Form for EntryForm {
    fn declared(&self, entry: &[u8], out: &mut DeclaredList<'_>) -> Result<(), EntryError> {
        if entry.get(..8) == Some(&STATIC_ENTRY_MAGIC[..]) {
            let view = StaticEntryView::read(entry)?;
            view.each_declared(
                |d| {
                    out.push(d.tabled, d.name);
                    Ok(())
                },
                |e| e,
            )?;
            view.each_static(|item| out.push_static(item));
            return Ok(());
        }
        let view = EntryView::read(entry)?;
        view.each_declared(
            |d| {
                out.push(d.tabled, d.name);
                Ok(())
            },
            |e| e,
        )?;
        view.each_static(|item| out.push_static(item));
        view.each_thread_local(|item| out.push_thread_local(item));
        Ok(())
    }
}

/// The on-demand side: the front end and the builder, made on first need. One call checks,
/// lowers, builds, emits and seals ONE instance, or makes ONE static's datum; nothing else of
/// the program is touched.
pub trait Make {
    type Error;
    /// Appends to `entry` the entry of the instance `name`, tabled as the fn `tabled`.
    fn make(&mut self, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), Self::Error>;
    /// Appends to `entry` the entry of static `item` (code_records.rs `statics`): its
    /// initializer evaluated, its value made a datum.
    fn make_static(&mut self, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error>;
    /// Appends to `entry` the TEMPLATE entry of thread-local `item` (code_records.rs
    /// `statics::make_template_entry`): its initial value made a read-only datum.
    fn make_thread_local(&mut self, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error>;
}

/// What a walk row is: a fn instance (its code record), a static (its datum) or a thread-local
/// (its template's datum, under symbols.rs `tls_symbol`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Instance,
    Static,
    ThreadLocal,
}

/// A static's row name: its item's key and no arguments (an instance name's form, so
/// [`each_item`] lists the item and the key takes its crate's fact).
fn static_name(item: ItemKey) -> [u8; 20] {
    let mut name = [0u8; 20];
    name[..16].copy_from_slice(&item.0.to_le_bytes());
    name
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
    /// A fn instance's tabled fn; a static's own item.
    tabled: AVec<'a, ItemKey>,
    kinds: AVec<'a, RowKind>,
    /// Each name's (start, length) in `arena`.
    names: AVec<'a, (u32, u32)>,
    how: AVec<'a, How>,
    arena: AVec<'a, u8>,
    /// Each row's (start, length) in `edges`: the rows its entry declares, set when the row
    /// stops being [`How::Waiting`] (empty until then).
    declares: AVec<'a, (u32, u32)>,
    /// The declared rows of every row done, in each entry's order.
    edges: AVec<'a, u32>,
    /// Open addressing over `symbols` by a symbol's first eight bytes (a symbol is a content
    /// digest: its bytes are the hash, so the table has no seed); 0 is empty, else the row + 1.
    /// A power of two long, at most half full.
    slots: AVec<'a, u32>,
}

const NO_KEY: CodeKey = CodeKey(ContentId([0; 32]));

/// The buffers one step of the walk ([`Walk::one`]) fills, made once by the caller and reused
/// by every step, so a step allocates nothing once they have grown.
pub struct Scratch<'a> {
    /// The instance name of the row being done (copied out of the walk's arena).
    name: AVec<'a, u8>,
    /// The entry the maker made in the last step; empty when that step made nothing.
    made: AVec<'a, u8>,
    facts: AVec<'a, Read>,
    items: AVec<'a, ItemKey>,
    declared: DeclaredList<'a>,
}

impl<'a> Scratch<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Scratch<'a> {
        Scratch { name: Vec::new_in(alloc), made: Vec::new_in(alloc), facts: Vec::new_in(alloc), items: Vec::new_in(alloc), declared: DeclaredList::new_in(alloc) }
    }

    /// The entry bytes the maker made in the last [`Walk::one`], when its row's [`One::how`] is
    /// [`How::Made`] or [`How::Unkeyed`] (an unkeyed instance is not put: these bytes are the
    /// only copy of its record, and the caller serves them). Empty after a step that made
    /// nothing ([`How::Found`], or a row already done); a made entry may itself be empty, so
    /// the caller tells the cases by `how`, never by the length.
    pub fn made(&self) -> &[u8] {
        &self.made
    }
}

/// What one step ([`Walk::one`]) holds for its row.
#[derive(Clone, Copy, Debug)]
pub struct One<'w> {
    /// What was done for the row: never [`How::Waiting`].
    pub how: How,
    /// The row's key; `None` for an instance made without one ([`How::Unkeyed`]).
    pub key: Option<CodeKey>,
    /// The rows its entry declares, in the entry's order (an instance declared twice comes
    /// twice). A row may be old (reached before, perhaps already done) or new
    /// ([`How::Waiting`], nothing done for it).
    pub declared: &'w [u32],
}

impl<'a> Walk<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Walk<'a> {
        Walk {
            alloc,
            symbols: Vec::new_in(alloc),
            keys: Vec::new_in(alloc),
            tabled: Vec::new_in(alloc),
            kinds: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            how: Vec::new_in(alloc),
            arena: Vec::new_in(alloc),
            declares: Vec::new_in(alloc),
            edges: Vec::new_in(alloc),
            slots: Vec::new_in(alloc),
        }
    }

    /// The rows row `row`'s entry declares, in the entry's order; empty while it is
    /// [`How::Waiting`].
    pub fn declared(&self, row: usize) -> &[u32] {
        let (at, len) = self.declares[row];
        &self.edges[at as usize..(at + len) as usize]
    }

    /// The row of the instance `symbol`, if the walk has reached it.
    pub fn row_of(&self, symbol: &Symbol) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        self.find(symbol).ok()
    }

    /// The instance `entry` (its tabled fn and its instance name) reached as a root: its row,
    /// new and [`How::Waiting`] with nothing done for it, or the row it has. At most `most`
    /// instances in the table.
    pub fn root<E>(&mut self, entry: (ItemKey, &[u8]), most: usize) -> Result<usize, WalkError<E>> {
        self.reach(entry.0, entry.1, RowKind::Instance, most)
    }

    /// Whether row `row` is a fn instance or a static.
    pub fn kind(&self, row: usize) -> RowKind {
        self.kinds[row]
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

    /// The instance `name` tabled as `tabled` (or the static `tabled`, named by [`static_name`]),
    /// reached: a new row, or the one it has.
    fn reach<E>(&mut self, tabled: ItemKey, name: &[u8], kind: RowKind, most: usize) -> Result<usize, WalkError<E>> {
        if (self.symbols.len() + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let symbol = match kind {
            RowKind::Instance => fn_symbol_of_name(name),
            RowKind::Static => static_symbol(tabled),
            RowKind::ThreadLocal => tls_symbol(tabled),
        };
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
                self.kinds.push(kind);
                self.names.push((self.arena.len() as u32, name.len() as u32));
                self.arena.extend_from_slice(name);
                self.how.push(How::Waiting);
                self.declares.push((0, 0));
                Ok(row)
            }
        }
    }

    /// The walk from the instance `entry` (its tabled fn and its instance name; called again
    /// with another root, it goes on from what it holds): each instance keyed before any work,
    /// found in `store` or made by `make` and put, and what its entry declares reached in the
    /// entry's order. At most `most` instances. [`Walk::root`], then [`Walk::one`] over the
    /// rows from the root's in order until none is [`How::Waiting`].
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
        let mut at = self.root(entry, most)?;
        let mut scratch = Scratch::new_in(self.alloc);
        while at < self.len() {
            if self.how[at] == How::Waiting {
                self.one(at, ids, store, builder, producer, make, form, most, &mut scratch)?;
            }
            at += 1;
        }
        Ok(())
    }

    /// ONE step of the walk, for row `row` alone (a row this walk gave: [`Walk::root`],
    /// [`Walk::row_of`] or a step's [`One::declared`]): the instance keyed from text and today's
    /// facts before any work, its entry found in `store` or made by `make` (and put when keyed),
    /// and each instance the entry declares reached as a row with nothing done for it. No other
    /// row is keyed, looked up or made.
    ///
    /// Idempotent: a row that is not [`How::Waiting`] answers what it holds, with no store, seam
    /// or maker call. After the step, [`Scratch::made`] holds the entry bytes when the
    /// instance was made. At most `most` instances in the table. On an error the row stays
    /// [`How::Waiting`] (a row its entry reached before the error stays reached).
    pub fn one<'w, M: Make>(
        &'w mut self,
        row: usize,
        ids: &dyn BodyIds,
        store: &mut dyn CodeStore,
        builder: &Builder,
        producer: &ProducerId<CodeBuilder>,
        make: &mut M,
        form: &dyn Form,
        most: usize,
        scratch: &mut Scratch<'a>,
    ) -> Result<One<'w>, WalkError<M::Error>> {
        let Scratch { name, made, facts, items, declared } = scratch;
        made.clear();
        if self.how[row] != How::Waiting {
            return Ok(One { how: self.how[row], key: self.key(row), declared: self.declared(row) });
        }
        let tabled = self.tabled[row];
        name.clear();
        name.extend_from_slice(self.name(row));
        let body = ids.body_check(tabled).ok_or(WalkError::NoBody { tabled })?;
        // Whether the instance has a key: its fn's BodyCheck record is there and every read of
        // it is today's.
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
            each_item(name.as_slice(), |item| items.push(item)).map_err(|why| WalkError::Name { tabled, why })?;
            facts.clear();
            for &item in items.iter() {
                facts.push(ids.crate_fact(item).ok_or(WalkError::NoCrateFact { tabled, item })?);
            }
            facts.sort_unstable();
            facts.dedup();
            key = record_key(self.symbols[row], &body, facts.as_slice(), builder, producer);
            if let Some(bytes) = store.get(key).map_err(WalkError::Store)? {
                form.declared(bytes, declared).map_err(|why| WalkError::Entry { tabled, found: true, why })?;
                found = true;
            }
        }
        if !found {
            match self.kinds[row] {
                RowKind::Instance => make.make(tabled, name.as_slice(), made),
                RowKind::Static => make.make_static(tabled, made),
                RowKind::ThreadLocal => make.make_thread_local(tabled, made),
            }
            .map_err(|why| WalkError::Make { tabled, why })?;
            form.declared(made.as_slice(), declared).map_err(|why| WalkError::Entry { tabled, found: false, why })?;
            if unkeyed.is_none() {
                store.put(key, made.as_slice()).map_err(WalkError::Store)?;
            }
        }
        // The declared rows, reached in the entry's order; on a refusal the row's edges so far
        // are dropped and the row stays waiting.
        let start = self.edges.len();
        let (instances, statics) = (declared.len(), declared.statics().len());
        for i in 0..instances + statics + declared.thread_locals().len() {
            let reached = if i < instances {
                let (callee, callee_name) = declared.row(i);
                self.reach(callee, callee_name, RowKind::Instance, most)
            } else if i < instances + statics {
                let item = declared.statics()[i - instances];
                self.reach(item, &static_name(item), RowKind::Static, most)
            } else {
                let item = declared.thread_locals()[i - instances - statics];
                self.reach(item, &static_name(item), RowKind::ThreadLocal, most)
            };
            match reached {
                Ok(callee_row) => self.edges.push(callee_row as u32),
                Err(why) => {
                    self.edges.truncate(start);
                    return Err(why);
                }
            }
        }
        self.declares[row] = (start as u32, (self.edges.len() - start) as u32);
        self.keys[row] = key;
        let how = match (found, unkeyed) {
            (true, _) => How::Found,
            (false, None) => How::Made,
            (false, Some(why)) => How::Unkeyed(why),
        };
        self.how[row] = how;
        Ok(One { how, key: self.key(row), declared: self.declared(row) })
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
                let len = u32::from_le_bytes([entry[at + 16], entry[at + 17], entry[at + 18], entry[at + 19]]);
                // The model's static row: its item, the length `u32::MAX`, no name.
                if len == u32::MAX {
                    out.push_static(ItemKey(u128::from_le_bytes(wide)));
                    at += 20;
                    continue;
                }
                let len = len as usize;
                out.push(ItemKey(u128::from_le_bytes(wide)), &entry[at + 20..at + 20 + len]);
                at += 20 + len;
            }
            Ok(())
        }
    }

    /// The maker: counts its calls (the model's `check_body` counter).
    struct Model<'a> {
        alloc: Alloc<'a>,
        calls: usize,
    }

    impl Make for Model<'_> {
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

        fn make_static(&mut self, _item: ItemKey, _entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            panic!("the five-instance model declares no static");
        }

        fn make_thread_local(&mut self, _item: ItemKey, _entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            panic!("the five-instance model declares no thread-local");
        }
    }

    /// The statics model: `main` (crate `app`) names static `S` (crate `app`), whose value
    /// points at static `T` (crate `lib`). The maker counts its calls of each kind.
    struct StaticModel<'a> {
        fns: usize,
        statics: AVec<'a, ItemKey>,
    }

    const S: u128 = 0xA0E1;
    const T: u128 = 0xB0E2;

    impl Make for StaticModel<'_> {
        type Error = NameError;
        fn make(&mut self, tabled: ItemKey, _name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            assert_eq!(tabled.0, MAIN, "the statics model has one fn");
            self.fns += 1;
            entry.extend_from_slice(&S.to_le_bytes());
            entry.extend_from_slice(&u32::MAX.to_le_bytes());
            Ok(())
        }

        fn make_static(&mut self, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            self.statics.push(item);
            if item.0 == S {
                entry.extend_from_slice(&T.to_le_bytes());
                entry.extend_from_slice(&u32::MAX.to_le_bytes());
            }
            Ok(())
        }

        fn make_thread_local(&mut self, _item: ItemKey, _entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            panic!("the statics model declares no thread-local");
        }
    }

    /// The statics model's command side: each item's text version, one read of `app`'s scope.
    struct StaticIds {
        text: [(u128, u8); 3],
    }

    impl BodyIds for StaticIds {
        fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
            let &(_, version) = self.text.iter().find(|(k, _)| *k == item.0)?;
            let mut text = [0u8; 17];
            text[..16].copy_from_slice(&item.0.to_le_bytes());
            text[16] = version;
            Some(BodyId { key: ContentId::of("d106 test body", &text), reads: core::slice::from_ref(&STATIC_READ) })
        }

        /// Every fact of the model is unmoved: its digest is zero.
        fn today(&self, _fact: &[u8; 32]) -> Option<[u8; 32]> {
            Some([0; 32])
        }

        fn crate_fact(&self, item: ItemKey) -> Option<Read> {
            Some(Read { fact: crate_fact_name(item.0), digest: [0; 32] })
        }
    }

    /// `app`'s scope, unmoved (the one read each body of the statics model holds).
    const STATIC_READ: Read = Read { fact: [0xA0; 32], digest: [0; 32] };

    /// A static is a walk row of its own: keyed from its symbol (symbols.rs `static_symbol`),
    /// its initializer's BodyCheck key and its crate's fact; found on the second run; and an
    /// edit of its initializer makes that static's datum alone, never the code that names it
    /// (code names the static's word, not its value). Controls: an edit of `main` makes `main`
    /// alone; the static's symbol is not the fn symbol of its row name.
    /// Prints "d106 statics: first made 1+2; second found 3; S edited made S; main edited made main".
    #[test]
    fn a_static_is_a_row_keyed_from_its_initializer() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut store = MemoryStore::new_in(alloc);
        let producer = blaze_ids::producer::Producers::for_tests(b"d106 walk model").get::<CodeBuilder>();
        let run = |ids: &StaticIds, store: &mut MemoryStore<'_>| {
            let mut walk = Walk::new_in(alloc);
            let mut model = StaticModel { fns: 0, statics: Vec::new_in(alloc) };
            let mut entry: AVec<'_, u8> = Vec::new_in(alloc);
            name_of(MAIN, None, &mut entry);
            walk.run((ItemKey(MAIN), &entry), ids, store, &builder(), &producer, &mut model, &Toy, 64).expect("the statics model walks");
            let kinds: std::vec::Vec<(RowKind, Symbol)> = (0..walk.len()).map(|r| (walk.kind(r), walk.symbol(r))).collect();
            (walk.counts(), model.fns, model.statics.iter().map(|k| k.0).collect::<std::vec::Vec<_>>(), kinds)
        };
        let mut ids = StaticIds { text: [(MAIN, 0), (S, 0), (T, 0)] };
        let (first, fns, statics, kinds) = run(&ids, &mut store);
        assert_eq!((first.reached, first.made, fns), (3, 3, 1), "{first:?}");
        assert_eq!(statics, [S, T], "each static made once, in the walk's order");
        assert_eq!(kinds[1], (RowKind::Static, static_symbol(ItemKey(S))), "S's row is a static's, by its symbol");
        assert_eq!(kinds[2], (RowKind::Static, static_symbol(ItemKey(T))));
        assert_ne!(kinds[1].1, fn_symbol_of_name(&static_name(ItemKey(S))), "a static's symbol is not its row name's fn symbol");
        let (second, fns, statics, _) = run(&ids, &mut store);
        assert_eq!((second.found, second.made, fns, statics.len()), (3, 0, 0, 0), "{second:?}");
        ids.text[1].1 = 1;
        let (edited, fns, statics, _) = run(&ids, &mut store);
        assert_eq!((edited.found, edited.made, fns), (2, 1, 0), "{edited:?}");
        assert_eq!(statics, [S], "S's initializer edited: S's datum alone is made");
        ids.text[0].1 = 1;
        let (control, fns, statics, _) = run(&ids, &mut store);
        assert_eq!((control.found, control.made, fns, statics.len()), (2, 1, 1, 0), "main edited: main alone");
        std::println!("d106 statics: first made 1+2; second found 3; S edited made S; main edited made main");
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
            tier: crate::code_records::TierName::Reference,
            settings: Settings::baseline(blaze_build::OverflowChecks::On, blaze_build::Panic::Unwind),
            toolchain: ToolchainId([7; 32]),
            producer: blaze_ids::producer::Producers::for_tests(b"body ids test builder").get::<blaze_ids::producer::CodeBuilder>(),
            edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
        }
    }

    /// One run of the model program over `store`: its counts and the maker's calls.
    fn run_once(ids: &Ids, store: &mut MemoryStore<'_>, alloc: blaze_own::Alloc<'_>) -> (Counts, usize) {
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
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut store = MemoryStore::new_in(alloc);
        let ids = Ids::new([0, 0]);
        let (first, first_calls) = run_once(&ids, &mut store, alloc);
        let (second, second_calls) = run_once(&ids, &mut store, alloc);
        let mut dropped = Ids::new([0, 0]);
        dropped.recorded = false;
        let (control, control_calls) = run_once(&dropped, &mut store, alloc);
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
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut store = MemoryStore::new_in(alloc);
        let mut ids = Ids::new([0, 0]);
        run_once(&ids, &mut store, alloc);
        ids.text[4].1 = 1;
        let (body, body_calls) = run_once(&ids, &mut store, alloc);
        let mut moved = Ids::new([0, 1]);
        moved.text[4].1 = 1;
        let (head, head_calls) = run_once(&moved, &mut store, alloc);
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
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut store = MemoryStore::new_in(alloc);
        run_once(&Ids::new([0, 0]), &mut store, alloc);
        let before = store.puts;
        let old = Ids::new([0, 0]);
        let mut ids = Ids::new([0, 1]);
        ids.reads[3] = old.reads[3];
        let (counts, calls) = run_once(&ids, &mut store, alloc);
        std::println!("d106 stale: found {} made {} stale {} puts +{}", counts.found, counts.made, counts.stale, store.puts - before);
        assert_eq!((counts.found, counts.made, counts.stale, calls), (1, 3, 1, 4));
        assert_eq!(store.puts - before, 3);
    }

    /// The items of a name are listed without a ground table, and a tag the form never writes
    /// is refused by its byte.
    #[test]
    fn a_name_lists_its_items() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut name: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(TWICE, Some(APP), &mut name);
        let mut items: AVec<'_, ItemKey> = Vec::new_in(alloc);
        each_item(&name, |item| items.push(item)).expect("a well-formed name");
        assert_eq!(items.as_slice(), &[ItemKey(TWICE), ItemKey(APP)]);
        name[20] = 99;
        assert_eq!(each_item(&name, |_| {}), Err(NameError::Tag { tag: 99, at: 20 }));
    }

    fn model_producer() -> ProducerId<CodeBuilder> {
        blaze_ids::producer::Producers::for_tests(b"d106 walk model").get::<CodeBuilder>()
    }

    /// The rows not yet done, last first: the order the lazy tests ask in.
    fn last_waiting(walk: &Walk<'_>) -> Option<usize> {
        (0..walk.len()).rev().find(|&row| walk.how(row) == How::Waiting)
    }

    /// D124: one step for `main` alone makes `main` once and reaches its three callees as rows
    /// with nothing done for them: `helper`, `twice::<App>` and `plain` are `Waiting`, the maker
    /// was called once, the store holds one entry, and `leaf` (only their callee) is not in the
    /// table at all. The control is `root` alone: a row, no maker call, no put.
    #[test]
    fn one_step_makes_its_row_and_nothing_ahead() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut store = MemoryStore::new_in(alloc);
        let ids = Ids::new([0, 0]);
        let builder = builder();
        let producer = model_producer();
        let mut model = Model { alloc, calls: 0 };
        let mut entry: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(MAIN, None, &mut entry);
        let mut walk = Walk::new_in(alloc);
        let mut scratch = Scratch::new_in(alloc);
        let root = walk.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
        assert_eq!((root, walk.len(), walk.how(root), model.calls, store.puts), (0, 1, How::Waiting, 0, 0), "a root is a row and nothing else");
        let one = walk.one(root, &ids, &mut store, &builder, &producer, &mut model, &Toy, 64, &mut scratch).expect("main is made");
        let (how, key) = (one.how, one.key);
        let declared = <[u32; 3]>::try_from(one.declared).expect("main declares three instances");
        assert_eq!(how, How::Made);
        assert_eq!(walk.key(root), key);
        assert!(key.is_some(), "main is keyed");
        assert_eq!(declared.map(|row| walk.tabled(row as usize).0), [HELPER, TWICE, PLAIN], "the entry's order");
        let mut twice: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(TWICE, Some(APP), &mut twice);
        assert_eq!(walk.name(declared[1] as usize), twice.as_slice(), "the second is twice::<App>");
        assert!(declared.iter().all(|&row| walk.how(row as usize) == How::Waiting && walk.key(row as usize).is_none()));
        assert_eq!((walk.len(), model.calls, store.puts, store.len()), (4, 1, 1, 1));
        let mut leaf: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(LEAF, None, &mut leaf);
        assert_eq!(walk.row_of(&fn_symbol_of_name(&leaf)), None, "leaf is not reached: nothing walked ahead");
        assert_eq!(walk.row_of(&fn_symbol_of_name(&twice)), Some(declared[1] as usize));
    }

    /// Asking the rows one at a time in another order, from two roots (`plain` first, then
    /// `main`; then always the last waiting row) ends with the table `run` from `main` makes:
    /// the same five symbols, each with the same tabled fn, name, key and how, each declaring
    /// the same symbols in the same order, the same maker calls, and a store with the same
    /// entries under the same keys. Row numbers differ (`plain` is row 0 here), and `main`'s
    /// step names `plain` as an old row.
    #[test]
    fn rows_asked_in_any_order_end_where_run_ends() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let ids = Ids::new([0, 0]);
        let builder = builder();
        let producer = model_producer();
        let mut main: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(MAIN, None, &mut main);
        let mut plain: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(PLAIN, None, &mut plain);

        let mut eager_store = MemoryStore::new_in(alloc);
        let mut eager_model = Model { alloc, calls: 0 };
        let mut eager = Walk::new_in(alloc);
        eager.run((ItemKey(MAIN), &main), &ids, &mut eager_store, &builder, &producer, &mut eager_model, &Toy, 64).expect("the model program walks");

        let mut lazy_store = MemoryStore::new_in(alloc);
        let mut lazy_model = Model { alloc, calls: 0 };
        let mut lazy = Walk::new_in(alloc);
        let mut scratch = Scratch::new_in(alloc);
        assert_eq!(lazy.root::<NameError>((ItemKey(PLAIN), &plain), 64).expect("plain is reached"), 0);
        let main_row = lazy.root::<NameError>((ItemKey(MAIN), &main), 64).expect("main is reached");
        assert_eq!(main_row, 1);
        let one = lazy.one(main_row, &ids, &mut lazy_store, &builder, &producer, &mut lazy_model, &Toy, 64, &mut scratch).expect("main is made");
        assert_eq!(one.declared[2], 0, "main's step names plain's old row");
        let mut asked = 1;
        while let Some(row) = last_waiting(&lazy) {
            lazy.one(row, &ids, &mut lazy_store, &builder, &producer, &mut lazy_model, &Toy, 64, &mut scratch).expect("the row is done");
            asked += 1;
            assert!(asked <= 5, "five instances take five steps");
        }
        assert_eq!(asked, 5);
        assert_eq!((lazy.len(), lazy_model.calls), (eager.len(), eager_model.calls));
        assert_eq!(lazy.counts(), eager.counts());
        for row in 0..eager.len() {
            let symbol = eager.symbol(row);
            let other = lazy.row_of(&symbol).expect("every instance run reached is reached");
            assert_eq!((lazy.tabled(other), lazy.name(other), lazy.key(other), lazy.how(other)), (eager.tabled(row), eager.name(row), eager.key(row), eager.how(row)));
            let eager_declared = eager.declared(row).iter().map(|&r| eager.symbol(r as usize));
            let lazy_declared = lazy.declared(other).iter().map(|&r| lazy.symbol(r as usize));
            assert!(eager_declared.eq(lazy_declared), "the same callees in the same order");
            let key = eager.key(row).expect("every model instance is keyed");
            assert_eq!(lazy_store.get(key).expect("a memory store reads"), eager_store.get(key).expect("a memory store reads"));
        }
        assert_eq!((lazy_store.len(), lazy_store.puts), (eager_store.len(), eager_store.puts));
    }

    /// A step asked twice is one maker call and one put: the second answers the same how, key
    /// and declared rows, touches neither the maker nor the store, and leaves no made bytes in
    /// the scratch (it made nothing).
    #[test]
    fn a_row_asked_twice_is_made_once() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut store = MemoryStore::new_in(alloc);
        let ids = Ids::new([0, 0]);
        let builder = builder();
        let producer = model_producer();
        let mut model = Model { alloc, calls: 0 };
        let mut entry: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(MAIN, None, &mut entry);
        let mut walk = Walk::new_in(alloc);
        let mut scratch = Scratch::new_in(alloc);
        let root = walk.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
        let first = walk.one(root, &ids, &mut store, &builder, &producer, &mut model, &Toy, 64, &mut scratch).expect("main is made");
        let first = (first.how, first.key, <[u32; 3]>::try_from(first.declared).expect("three callees"));
        let second = walk.one(root, &ids, &mut store, &builder, &producer, &mut model, &Toy, 64, &mut scratch).expect("main is held");
        let second = (second.how, second.key, <[u32; 3]>::try_from(second.declared).expect("three callees"));
        assert_eq!(first, second);
        assert_eq!((model.calls, store.puts, walk.len()), (1, 1, 4));
        assert!(scratch.made().is_empty(), "the second step made nothing");
        // A second root call for the same instance is its row, not a new one.
        assert_eq!(walk.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached"), root);
    }

    /// Over a store that already holds every entry (a `run` before), asking each row one at a
    /// time makes nothing: every step is `Found`, the maker is never called, nothing is put,
    /// and the scratch holds no made bytes after any step.
    #[test]
    fn rows_over_a_full_store_make_nothing() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut store = MemoryStore::new_in(alloc);
        let ids = Ids::new([0, 0]);
        let (filled, _) = run_once(&ids, &mut store, alloc);
        assert_eq!((filled.made, store.puts), (5, 5));
        let builder = builder();
        let producer = model_producer();
        let mut model = Model { alloc, calls: 0 };
        let mut entry: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(MAIN, None, &mut entry);
        let mut walk = Walk::new_in(alloc);
        let mut scratch = Scratch::new_in(alloc);
        walk.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
        let mut asked = 0;
        while let Some(row) = last_waiting(&walk) {
            let one = walk.one(row, &ids, &mut store, &builder, &producer, &mut model, &Toy, 64, &mut scratch).expect("the row is found");
            assert_eq!(one.how, How::Found);
            assert!(scratch.made().is_empty(), "a found row makes no bytes");
            asked += 1;
            assert!(asked <= 5, "five instances take five steps");
        }
        assert_eq!((asked, walk.len(), walk.counts().found, model.calls, store.puts), (5, 5, 5, 0, 5));
    }

    /// An instance without a key (no BodyCheck record) is made and not put, and its entry is
    /// readable from the scratch: the bytes equal what the maker makes for `main`, and the
    /// store holds nothing. The control, keyed over another store: the scratch's bytes are the
    /// entry the store now holds under the row's key.
    #[test]
    fn an_unkeyed_instance_is_served_from_the_scratch() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let builder = builder();
        let producer = model_producer();
        let mut entry: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(MAIN, None, &mut entry);
        let mut expected: AVec<'_, u8> = Vec::new_in(alloc);
        Model { alloc, calls: 0 }.make(ItemKey(MAIN), &entry, &mut expected).expect("main's entry");
        assert!(!expected.is_empty(), "main's entry declares its callees");

        let mut dropped = Ids::new([0, 0]);
        dropped.recorded = false;
        let mut store = MemoryStore::new_in(alloc);
        let mut model = Model { alloc, calls: 0 };
        let mut walk = Walk::new_in(alloc);
        let mut scratch = Scratch::new_in(alloc);
        let root = walk.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
        let one = walk.one(root, &dropped, &mut store, &builder, &producer, &mut model, &Toy, 64, &mut scratch).expect("main is made");
        assert_eq!((one.how, one.key, one.declared.len()), (How::Unkeyed(Unkeyed::NoRecord), None, 3));
        assert_eq!(scratch.made(), expected.as_slice());
        assert_eq!((store.len(), store.puts, model.calls), (0, 0, 1));

        let ids = Ids::new([0, 0]);
        let mut keyed_store = MemoryStore::new_in(alloc);
        let mut keyed = Walk::new_in(alloc);
        let root = keyed.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
        let one = keyed.one(root, &ids, &mut keyed_store, &builder, &producer, &mut model, &Toy, 64, &mut scratch).expect("main is made");
        assert_eq!(one.how, How::Made);
        let key = one.key.expect("main is keyed");
        assert_eq!(keyed_store.get(key).expect("a memory store reads"), Some(scratch.made()));
        assert_eq!(scratch.made(), expected.as_slice());
    }
}
