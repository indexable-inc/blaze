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
//! ([`Stepped::Unkeyed`]) and its counts; the driver opens a span for each. Every other instance is
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
//! THE READ LIST (F20): what the build of an instance reads beyond its key (a dependency's
//! layouts, constants, statics, callee ABIs, vtable shapes, projections, drop impls, resolved
//! calls, C symbols: code_reads.rs) is not in the key. The make records it at the one world the
//! builders take (run/make_one.rs `Asking`), the entry carries it, and an entry found under its
//! key is taken only after the maker asks every row again today ([`Make::verify`]). Code
//! records are no engine rows with cells (they live in the [`CodeStore`]), so nothing dirties
//! them forward: the check is here, at lookup. A row answered otherwise makes the instance
//! again; the new entry goes under its CONTENT key and the instance's key points to it
//! (`CodeStore::point`), so a key never holds two values (rule 21) and a found entry is read
//! through the pointer.
//!
//! OPEN, said rather than guessed: a static's entry (its initializer's reads) carries no read
//! list yet; closure facts read from an owner other than the instance's fn are in neither
//! (code_reads.rs "FORWARDED, NOT RECORDED"). A coroutine state an entry read is laid again
//! by the verifier and its rows asked of that lay (code_reads.rs `ReadKind::Laid`,
//! `CoroutineState`).

use core::fmt;

use blaze_code::record::{MAGIC, Symbol};
use blaze_ids::producer::{CodeBuilder, ProducerId};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, Vec};
pub use blaze_run::parent::Read;
use blaze_typeck::ItemKey;

use crate::code_records::reads::Verdict;
use crate::code_records::statics::{STATIC_ENTRY_MAGIC, StaticEntryView};
use crate::code_records::{Builder, CodeClass, CodeKey, CodeStore, ENTRY_FORM, EntryError, EntryView, PanicStrategy, StoreError, triple};
use crate::symbols::{fn_symbol_of_name, static_symbol, tls_symbol};

/// Version 3 (F20): an entry found under its key is taken only when its read list holds today,
/// and a key names its entry through a pointer to the entry's content key. Every key of version
/// 2 named an entry that carried no read list (run/make_one.rs passed `ReadList::empty()`), so
/// none of them is read again.
const KEY_DOMAIN: &str = "blaze-ladder code record key 3";

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
    /// The class of the crate that defines `item` (code_records.rs [`CodeClass`]): an
    /// instance is the toolchain's when every item of its name is. A seam that tells no
    /// toolchain crate from another answers `Program` for all: its code is then kept by the
    /// program's root alone, which only shortens its life (the command's seam, blaze-cli
    /// run_ids.rs, tells them).
    fn class(&self, item: ItemKey) -> CodeClass {
        let _ = item;
        CodeClass::Program
    }
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

/// Computes the content-addressed [`CodeKey`] of a previously walked instance `(symbol, tabled, name)`
/// against today's [`BodyIds`], [`Builder`], and [`ProducerId<CodeBuilder>`], without compiling it.
/// Returns `None` if `tabled` has no valid body record today, any read moved, or any item in `name`
/// lacks a crate fact. Uses the exact same inputs as `Walk::start` (rule 30).
pub fn instance_key(
    symbol: Symbol,
    tabled: ItemKey,
    name: &[u8],
    ids: &dyn BodyIds,
    builder: &Builder,
    producer: &ProducerId<CodeBuilder>,
    alloc: Alloc<'_>,
) -> Option<CodeKey> {
    let body = ids.body_check(tabled)?;
    if body.reads.is_empty() {
        return None;
    }
    for read in body.reads {
        if ids.today(&read.fact) != Some(read.digest) {
            return None;
        }
    }
    let mut items: AVec<'_, ItemKey> = Vec::new_in(alloc);
    each_item(name, |item| items.push(item)).ok()?;
    items.sort_unstable();
    items.dedup();
    let mut facts: AVec<'_, Read> = Vec::with_capacity_in(items.len(), alloc);
    for &item in items.iter() {
        facts.push(ids.crate_fact(item)?);
    }
    facts.sort_unstable();
    facts.dedup();
    Some(record_key(symbol, &body, facts.as_slice(), builder, producer))
}

/// Why an instance name's items cannot be listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameError {
    /// The name ends inside a part, at byte `at`.
    Truncated { at: usize },
    /// A tag symbols.rs never writes, at byte `at`.
    Tag { tag: u8, at: usize },
    /// A reference, at byte `at`, to a node not written before it.
    Ref { at: usize },
    /// Bytes past the name's last part, from byte `at`.
    Trailing { at: usize },
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "an instance name is its item's key, a count, the distinct nodes of its arguments and a reference per argument (blaze-ladder symbols.rs `encode_instance`)";
        match *self {
            NameError::Truncated { at } => write!(f, "an instance name ends inside a part at byte {at}: {rule}"),
            NameError::Tag { tag, at } => write!(f, "an instance name holds the tag {tag} at byte {at}, which the form never writes: {rule}"),
            NameError::Ref { at } => write!(f, "an instance name refers, at byte {at}, to a node not written before it: {rule}"),
            NameError::Trailing { at } => write!(f, "an instance name has bytes past its end, from byte {at}: {rule}"),
        }
    }
}

impl core::error::Error for NameError {}

/// Each item an instance name holds, in the name's order, WITHOUT a ground table: the item
/// itself, then per DISTINCT node the key its tag carries (symbols.rs' table: 12 ADT, 13 fn
/// item, 17 a `dyn`'s principal, 18 a `dyn`'s projection's trait, 19 auto trait, 20 closure's
/// owner, 23 coroutine's owner). A node is written once however often the arguments reach it,
/// so the calls are linear in the name's bytes; one item may still come more than once (`Vec`
/// at two arguments is two nodes). The nodes are a flat stream, each a tag, its payload, a
/// child count and its children's references, so no tree is rebuilt; every reference is
/// checked to point before its node.
pub fn each_item(name: &[u8], mut each: impl FnMut(ItemKey)) -> Result<(), NameError> {
    let key_at = |at: usize| -> Result<ItemKey, NameError> {
        let bytes = name.get(at..at + 16).ok_or(NameError::Truncated { at })?;
        let mut wide = [0u8; 16];
        wide.copy_from_slice(bytes);
        Ok(ItemKey(u128::from_le_bytes(wide)))
    };
    let u32_at = |at: usize| -> Result<u32, NameError> {
        let bytes = name.get(at..at + 4).ok_or(NameError::Truncated { at })?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let head = key_at(0)?;
    if head != blaze_build::REIFY_SHIM_ITEM {
        each(head);
    }
    // The item's key (16) and the count of arguments (4); at no argument, nothing more.
    let args = u32_at(16)? as usize;
    let mut at = 20;
    if args == 0 {
        return if name.len() == at { Ok(()) } else { Err(NameError::Trailing { at }) };
    }
    let nodes = u32_at(at)? as usize;
    at += 4;
    for index in 0..nodes {
        let tag = *name.get(at).ok_or(NameError::Truncated { at })?;
        // (the payload's bytes, whether it starts with an item's key)
        let (payload, keyed) = match tag {
            0 | 1 | 5 | 6 | 7 | 10 | 11 | 15 | 16 | 26 => (0, false),
            2 | 3 | 4 | 8 | 9 | 21 => (1, false),
            12 | 13 | 17 | 19 | 24 => (16, true),
            14 => (16, false),
            18 => (24, true),
            20 | 23 => (20, true),
            22 => (4, false),
            25 => (8, false),
            _ => return Err(NameError::Tag { tag, at }),
        };
        if keyed {
            each(key_at(at + 1)?);
        }
        // The tag, the payload, the child count, then each child's reference.
        let kids = u32_at(at + 1 + payload)? as usize;
        at += 1 + payload + 4;
        if kids > (name.len() - at) / 4 {
            return Err(NameError::Truncated { at });
        }
        for _ in 0..kids {
            if u32_at(at)? as usize >= index {
                return Err(NameError::Ref { at });
            }
            at += 4;
        }
    }
    // A reference per argument.
    if args > name.len().saturating_sub(at) / 4 {
        return Err(NameError::Truncated { at });
    }
    for _ in 0..args {
        if u32_at(at)? as usize >= nodes {
            return Err(NameError::Ref { at });
        }
        at += 4;
    }
    if at != name.len() {
        return Err(NameError::Trailing { at });
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
    /// The rows of `entry`'s read list (code_reads.rs): a found entry with any is taken only
    /// when [`Make::verify`] says it holds. A form whose entries hold no read list (the walk's
    /// model forms) has none to say: 0.
    fn reads(&self, entry: &[u8]) -> Result<u32, EntryError> {
        let _ = entry;
        Ok(0)
    }
}

/// The run path's entry form: a code record's (code_records.rs `make_entry`, read by
/// `EntryView`) or a static's (code_records.rs `statics::make_static_entry`, read by
/// `StaticEntryView`), told by the magic.
pub struct EntryForm;

impl Form for EntryForm {
    fn declared(&self, entry: &[u8], out: &mut DeclaredList<'_>) -> Result<(), EntryError> {
        if entry.starts_with(&STATIC_ENTRY_MAGIC[..]) {
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

    /// A code entry's read-list rows (code_records.rs `EntryView::reads_count`); a static's
    /// entry carries none (the module doc's OPEN).
    fn reads(&self, entry: &[u8]) -> Result<u32, EntryError> {
        if entry.starts_with(&STATIC_ENTRY_MAGIC[..]) {
            return Ok(0);
        }
        Ok(EntryView::read(entry)?.reads_count())
    }
}

/// The on-demand side: the front end and the builder, made on first need. One call checks,
/// lowers, builds, emits and seals ONE instance, or makes ONE static's datum; nothing else of
/// the program is touched.
///
/// Every make takes the row's [`Make::Grant`] (D163, rules 74 and 76): the proof that the row's
/// body may run, made by the gate and handed down unread by anything between. The production
/// maker's grant is blaze-cli run_ids.rs `Runnable`, so nothing reaches the make path without the
/// gate having let it; a walk with no gate ([`Walk::run`], the fixtures) makes a [`Fixture`]
/// grant, and only a maker whose grant is [`Fixture`] takes one.
pub trait Make {
    type Error;
    /// What the make path accepts as leave to make a row (the trait doc).
    type Grant: Grant;
    /// Appends to `entry` the entry of the instance `name`, tabled as the fn `tabled`, under
    /// `grant`, its fn's proof.
    fn make(&mut self, grant: &Self::Grant, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), Self::Error>;
    /// Appends to `entry` the entry of static `item` (code_records.rs `statics`): its
    /// initializer evaluated, its value made a datum.
    fn make_static(&mut self, grant: &Self::Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error>;
    /// Appends to `entry` the TEMPLATE entry of thread-local `item` (code_records.rs
    /// `statics::make_template_entry`): its initial value made a read-only datum.
    fn make_thread_local(&mut self, grant: &Self::Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error>;
    /// Whether `entry`, found under the key of the instance `name` tabled as `tabled`, still
    /// holds: every row of its read list asked again today (code_reads.rs `verify_today`).
    /// Asked only of an entry with rows ([`Form::reads`]), with no grant: nothing is made.
    /// `Ok(None)`: this maker asks no read list again, and the walk refuses the entry by name
    /// ([`WalkError::Unverified`]); it is never taken unasked (rule 3).
    fn verify(&mut self, tabled: ItemKey, name: &[u8], entry: &[u8]) -> Result<Option<Verdict>, Self::Error> {
        let _ = (tabled, name, entry);
        Ok(None)
    }
}

/// A proof a make takes ([`Make::Grant`]). It is held, not read: the make path's signature is
/// the check (rule 76), and which proof a maker takes is its type (`Maker<'_, G>`): blaze-cli
/// run_ids.rs `Runnable` for the run, [`Fixture`] for a walk with no gate. Implemented by those
/// two only.
pub trait Grant {}

/// The grant of a walk with no gate ([`Walk::run`], make_one.rs `walk_records`: the fixtures
/// and the examples). Made only in this module; a maker taking it is its own instantiation
/// (`Maker<'_, Fixture>`), never the production maker's.
///
/// ```compile_fail
/// // Outside body_ids.rs no fixture grant is made by hand: its field is private.
/// let forged = blaze_ladder::body_ids::Fixture(());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixture(());

impl Grant for Fixture {}

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

/// Where one instance's walk is: waiting, or done, with what was done.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    /// Not reached by the loop yet.
    Waiting,
    /// Begun ([`Walk::begin`]): keyed and not found, its entry being made by whoever holds its
    /// [`Pending`]; neither stepped again nor begun again until [`Walk::finish`] or
    /// [`Walk::abandon`] takes the pending.
    Making,
    /// Stepped ([`Walk::one`]): what was done for it, its key with it when it has one.
    Done(Stepped),
}

/// A row begun ([`Walk::begin`]) whose entry is to be made: what the make is ([`Pending::order`])
/// and what [`Walk::finish`] needs after it (the key, or why there is none; the class the put
/// files it under). Its fields are private and its only maker is `begin`, so a finish is
/// always of a row this walk keyed and did not find (rule 76); not `Copy` or `Clone`, so one
/// begun row is finished once (D194).
#[must_use = "a begun row is finished (`Walk::finish`) or abandoned (`Walk::abandon`)"]
#[derive(Debug, PartialEq, Eq)]
pub struct Pending {
    row: u32,
    order: Order,
    keyed: Result<CodeKey, Unkeyed>,
    class: CodeClass,
}

impl Pending {
    /// The row being made.
    pub fn row(&self) -> usize {
        self.row as usize
    }

    /// The make, apart from the walk: what a worker runs ([`Order::make`]).
    pub fn order(&self) -> Order {
        self.order
    }

    /// The key the made entry will be put under; `None` for an unkeyed instance (made, never
    /// put: its make is its own task, D194).
    pub fn key(&self) -> Option<CodeKey> {
        self.keyed.ok()
    }
}

/// One make, apart from the walk ([`Pending::order`]): the row's tabled fn and kind. With the
/// row's name (its walk's [`Walk::name`], copied by the caller) it is all a worker needs to make
/// the entry, so the make runs off the walk's thread (D194) and the walk stays the relay's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Order {
    pub tabled: ItemKey,
    pub kind: RowKind,
}

impl Order {
    /// The entry of this order's row appended to `entry` by `make`, under `grant` (the row's
    /// proof, unread here). `name` is the row's instance name (ignored for a static or a
    /// thread-local, whose item is `tabled`).
    pub fn make<M: Make>(self, make: &mut M, grant: &M::Grant, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), WalkError<M::Error>> {
        match self.kind {
            RowKind::Instance => make.make(grant, self.tabled, name, entry),
            RowKind::Static => make.make_static(grant, self.tabled, entry),
            RowKind::ThreadLocal => make.make_thread_local(grant, self.tabled, entry),
        }
        .map_err(|why| WalkError::Make { tabled: self.tabled, why })
    }
}

/// What [`Walk::begin`] answered: the row stepped (found under its key, or done before), or
/// begun, its entry to be made ([`Pending`]).
#[derive(Debug)]
pub enum Begun<'w> {
    Stepped(One<'w>),
    Make(Pending),
}

/// What one step did with its instance, each answer carrying what it holds: a found or made
/// entry is under its key, so the key is in the variant, never an `Option` beside it (a keyed
/// answer with no key cannot be written).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stepped {
    /// Its entry was in the store under its key, its read list holding today: nothing checked,
    /// lowered or built. The key is the one the entry's bytes are under (`CodeStore::get`): its
    /// content key, which the instance's key points to.
    Found(CodeKey),
    /// Made and put under its content key, which the instance's key now points to; the content
    /// key is held.
    Made(CodeKey),
    /// Made and not put: why it has no key.
    Unkeyed(Unkeyed),
}

impl Stepped {
    /// The key the entry is under; `None` for one made without a key.
    pub fn key(self) -> Option<CodeKey> {
        match self {
            Stepped::Found(key) | Stepped::Made(key) => Some(key),
            Stepped::Unkeyed(_) => None,
        }
    }
}

/// Why the walk stopped.
#[derive(Debug)]
pub enum WalkError<E> {
    /// The command has no fn for `tabled`, which an entry (or the caller) names.
    NoBody { tabled: ItemKey },
    /// The row of an instance of `tabled` was stepped or begun while it was being made
    /// ([`How::Making`]): its [`Pending`] is held elsewhere, and only `finish` or `abandon` takes
    /// it (D194: one make per row).
    Making { tabled: ItemKey },
    /// A [`Pending`] of an instance of `tabled` handed to [`Walk::finish`] or [`Walk::abandon`]
    /// of a walk whose row it names is not being made for that fn: another walk's, or finished
    /// already (D194).
    NotBegun { tabled: ItemKey },
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
    /// The entry of an instance of `tabled` was found with `rows` read-list rows, and the
    /// walk's maker asks no read list again ([`Make::verify`]): refused, never taken unasked.
    Unverified { tabled: ItemKey, rows: u32 },
    /// The maker could not ask the found entry's read list of an instance of `tabled` again.
    Verify { tabled: ItemKey, why: E },
    /// More instances than `most` (rule 18).
    Cap { most: usize },
    /// An instance name of `bytes` bytes, past `most` ([`NAME_MOST_BYTES`] and the forest's
    /// header), declared by the instance of `declarer` (`None`: a root), whose name's first
    /// items are `heads` (rule 18). symbols.rs refuses a name past its cap while it is made, so
    /// this is a name from a stored entry.
    NameTooLarge { tabled: ItemKey, declarer: Option<ItemKey>, bytes: usize, most: usize, heads: [Option<ItemKey>; 3] },
    /// The walk's arena of names would pass `u32::MAX` bytes (its offsets are 4 bytes) at the
    /// instance of `tabled`: `bytes` held, `adding` more (rule 18).
    ArenaFull { tabled: ItemKey, bytes: usize, adding: usize },
    /// rustc's "reached the recursion limit while instantiating": the instance of `item`
    /// (tabled `tabled`) has `depth` instances of `item` above it on the chain of instances that
    /// first declared it, past `limit` (the crate's `#![recursion_limit]`, 128 when unset);
    /// `declarer` is the tabled fn of the instance that declared it.
    RecursionLimit { tabled: ItemKey, item: ItemKey, declarer: Option<ItemKey>, depth: u32, limit: u32 },
}

impl<E: fmt::Display> fmt::Display for WalkError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::NoBody { tabled } => write!(f, "the record walk reached fn {:#x}, and the command's body ids hold no such fn (blaze-ladder body_ids.rs `BodyIds::body_check`)", tabled.0),
            WalkError::Making { tabled } => write!(
                f,
                "the record walk's row of an instance of fn {:#x} was stepped while its entry was being made (body_ids.rs `Walk::begin`: one make per row until `finish` or `abandon`, D194)",
                tabled.0
            ),
            WalkError::NotBegun { tabled } => write!(
                f,
                "a made entry of an instance of fn {:#x} was handed back to a record walk whose row is not being made for it: another walk's, or finished already (body_ids.rs `Walk::finish`, D194)",
                tabled.0
            ),
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
            WalkError::Unverified { tabled, rows } => write!(
                f,
                "the stored entry of an instance of fn {:#x} has {rows} read-list rows, and this walk's maker asks none again (body_ids.rs `Make::verify`): it is not taken unasked (rule 3)",
                tabled.0
            ),
            WalkError::Verify { tabled, why } => write!(f, "the read list of the stored entry of an instance of fn {:#x} was not asked again: {why}", tabled.0),
            WalkError::Cap { most } => write!(f, "the record walk met more than {most} instances (its cap, rule 18)"),
            WalkError::NameTooLarge { tabled, declarer, bytes, most, heads } => {
                write!(f, "an instance of fn {:#x}", tabled.0)?;
                if let Some(d) = declarer {
                    write!(f, ", declared by an instance of fn {:#x},", d.0)?;
                }
                write!(f, " has a name of {bytes} bytes, past the walk's cap of {most} (body_ids.rs `Walk::reach`, rule 18); its name's first items:")?;
                heads.iter().flatten().try_for_each(|h| write!(f, " {:#x}", h.0))
            }
            WalkError::ArenaFull { tabled, bytes, adding } => write!(
                f,
                "the record walk's names hold {bytes} bytes, and the name of an instance of fn {:#x} adds {adding}: past 4 GiB, its offsets' width (body_ids.rs `Walk::reach`, rule 18)",
                tabled.0
            ),
            WalkError::RecursionLimit { tabled, item, declarer, depth, limit } => {
                write!(f, "reached the recursion limit while instantiating fn {:#x}", item.0)?;
                if *tabled != *item {
                    write!(f, " (tabled under fn {:#x})", tabled.0)?;
                }
                if let Some(d) = declarer {
                    write!(f, ", declared by an instance of fn {:#x}", d.0)?;
                }
                write!(
                    f,
                    ": {depth} instances of it above it on the chain that declared it, past the limit {limit} (`#![recursion_limit]`, 128 when unset; rustc's \"reached the recursion limit while instantiating\")"
                )
            }
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
    /// The longest instance name reached, in bytes.
    pub longest_name: usize,
    /// The most distinct items one name stepped with a key holds.
    pub most_name_items: usize,
    /// The bytes of every name reached (the walk's arena).
    pub arena: usize,
    /// Found entries whose read list was asked again and held (counted in `found`).
    pub verified: usize,
    /// Found entries whose read list answered otherwise today: made again (counted in `made`).
    pub stale_reads: usize,
    /// The first found entry whose read list did not hold: its fn and the verdict (the row's
    /// kind and item). What a test prints when a walk it expects to find everything makes one.
    pub first_moved: Option<(ItemKey, Verdict)>,
}

impl Counts {
    /// Calls of the maker: one per instance not found.
    pub fn make_calls(&self) -> usize {
        self.made + self.no_record + self.stale
    }
}

impl fmt::Display for Counts {
    /// `code records: reached R, found F (read lists held V), made M (read lists moved L),
    /// unkeyed U (no record N, stale S); names: longest B bytes, most items I, arena A bytes`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "code records: reached {}, found {} (read lists held {}), made {} (read lists moved {}), unkeyed {} (no record {}, stale {}); names: longest {} bytes, most items {}, arena {} bytes",
            self.reached,
            self.found,
            self.verified,
            self.made,
            self.stale_reads,
            self.no_record + self.stale,
            self.no_record,
            self.stale,
            self.longest_name,
            self.most_name_items,
            self.arena
        )
    }
}

/// The record walk's table (the module doc, "The table"): columns, row `n` the `n`th instance
/// reached.
pub struct Walk<'a> {
    alloc: Alloc<'a>,
    symbols: AVec<'a, Symbol>,
    /// A fn instance's tabled fn; a static's own item.
    tabled: AVec<'a, ItemKey>,
    kinds: AVec<'a, RowKind>,
    /// Each name's (start, length) in `arena`.
    names: AVec<'a, (u32, u32)>,
    /// Each row's state, its key in it once stepped with one ([`Stepped`]): the one home of a
    /// row's key.
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
    /// Each row's declarer: the row whose step first reached it ([`NO_ROW`] for a root). The
    /// rows by their declarers are a tree: the chain an error names, and the recursion depth.
    declarer: AVec<'a, u32>,
    /// Each fn instance row's recursion depth: 1 + the depth of the nearest row on its declarer
    /// chain that instantiates the same item (0 when none does); 0 for a static's row.
    depth: AVec<'a, u32>,
    /// The deepest an item may instantiate itself down one declarer chain: rustc's
    /// `recursion_limit` ([`DEFAULT_RECURSION_LIMIT`] until [`Walk::set_recursion_limit`]).
    recursion_limit: u32,
    /// The most distinct items of one name stepped with a key ([`Counts::most_name_items`]).
    most_name_items: usize,
    /// [`Counts::verified`] and [`Counts::stale_reads`].
    verified: usize,
    stale_reads: usize,
    /// [`Counts::first_moved`].
    first_moved: Option<(ItemKey, Verdict)>,
}

/// A row index that is no row: a root's declarer.
const NO_ROW: u32 = u32::MAX;

/// rustc's default `recursion_limit` (the Reference, "Limits"): 128.
pub const DEFAULT_RECURSION_LIMIT: u32 = 128;

/// The longest instance name the walk files (rule 18): a forest at its cap
/// ([`crate::symbols::NAME_MOST_BYTES`]) and its header (item key, argument count, node count)
/// and argument references, the arguments bounded by the cap's nodes.
pub const WALK_MOST_NAME: usize = crate::symbols::NAME_MOST_BYTES + 24 + 4 * crate::symbols::NAME_MOST_NODES as usize;

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

    /// The entry bytes the maker made in the last [`Walk::one`], when its row's [`One::stepped`]
    /// is [`Stepped::Made`] or [`Stepped::Unkeyed`] (an unkeyed instance is not put: these bytes
    /// are the only copy of its record, and the caller serves them). Empty after a step that
    /// made nothing ([`Stepped::Found`], or a row already done); a made entry may itself be
    /// empty, so the caller tells the cases by `stepped`, never by the length.
    pub fn made(&self) -> &[u8] {
        &self.made
    }
}

/// What one step ([`Walk::one`]) holds for its row.
#[derive(Clone, Copy, Debug)]
pub struct One<'w> {
    /// What was done for the row, its key in it when it has one (a step's row is never
    /// waiting, by this type).
    pub stepped: Stepped,
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
            tabled: Vec::new_in(alloc),
            kinds: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            how: Vec::new_in(alloc),
            arena: Vec::new_in(alloc),
            declares: Vec::new_in(alloc),
            edges: Vec::new_in(alloc),
            slots: Vec::new_in(alloc),
            declarer: Vec::new_in(alloc),
            depth: Vec::new_in(alloc),
            recursion_limit: DEFAULT_RECURSION_LIMIT,
            most_name_items: 0,
            verified: 0,
            stale_reads: 0,
            first_moved: None,
        }
    }

    /// The crate's `#![recursion_limit]` (rustc's limit on how deep one item instantiates
    /// itself down a chain of calls), for the rows reached from now on.
    pub fn set_recursion_limit(&mut self, limit: u32) {
        self.recursion_limit = limit;
    }

    /// Row `row`'s declarer: the row whose step first reached it; `None` for a root.
    pub fn declarer(&self, row: usize) -> Option<usize> {
        match self.declarer[row] {
            NO_ROW => None,
            d => Some(d as usize),
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
        self.reach(entry.0, entry.1, RowKind::Instance, most, NO_ROW)
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

    /// Row `row`'s key; `None` for an instance made without one, or not stepped yet.
    pub fn key(&self, row: usize) -> Option<CodeKey> {
        match self.how[row] {
            How::Done(stepped) => stepped.key(),
            How::Waiting | How::Making => None,
        }
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
        let longest_name = self.names.iter().map(|&(_, len)| len as usize).max().unwrap_or(0);
        let mut c = Counts {
            reached: self.len(),
            longest_name,
            most_name_items: self.most_name_items,
            arena: self.arena.len(),
            verified: self.verified,
            stale_reads: self.stale_reads,
            first_moved: self.first_moved,
            ..Counts::default()
        };
        for how in self.how.iter() {
            match how {
                How::Waiting | How::Making => {}
                How::Done(Stepped::Found(_)) => c.found += 1,
                How::Done(Stepped::Made(_)) => c.made += 1,
                How::Done(Stepped::Unkeyed(Unkeyed::NoRecord)) => c.no_record += 1,
                How::Done(Stepped::Unkeyed(Unkeyed::Stale { .. })) => c.stale += 1,
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

    /// The item a fn instance row instantiates: its name's first 16 bytes (rustc's `def_id`).
    fn item_of(name: &[u8]) -> Option<ItemKey> {
        let mut wide = [0u8; 16];
        wide.copy_from_slice(name.get(..16)?);
        Some(ItemKey(u128::from_le_bytes(wide)))
    }

    /// The recursion depth of a new fn instance row of `item` whose declarer is `declarer`, as
    /// rustc's collector counts it (`check_recursion_limit`): 1 + the depth of the nearest row up
    /// the declarer chain that instantiates `item`, or 0 when none does. The climb is as long as
    /// the chain (a declarer is always an older row, so it ends).
    fn depth_of(&self, item: ItemKey, declarer: u32) -> u32 {
        let mut at = declarer;
        while at != NO_ROW {
            let row = at as usize;
            if self.kinds[row] == RowKind::Instance && Walk::item_of(self.name(row)) == Some(item) {
                return self.depth[row] + 1;
            }
            at = self.declarer[row];
        }
        0
    }

    /// The instance `name` tabled as `tabled` (or the static `tabled`, named by [`static_name`]),
    /// reached from row `declarer` ([`NO_ROW`] for a root): a new row, or the one it has.
    fn reach<E>(&mut self, tabled: ItemKey, name: &[u8], kind: RowKind, most: usize, declarer: u32) -> Result<usize, WalkError<E>> {
        let declarer_fn = (declarer != NO_ROW).then(|| self.tabled[declarer as usize]);
        if name.len() > WALK_MOST_NAME {
            let mut heads = [None; 3];
            let mut n = 0;
            // The first items, as far as the name lists; a name that does not list still names
            // its tabled fn and declarer, so its listing error is not the one reported.
            let _ = each_item(name, |item| {
                if n < heads.len() {
                    heads[n] = Some(item);
                    n += 1;
                }
            });
            return Err(WalkError::NameTooLarge { tabled, declarer: declarer_fn, bytes: name.len(), most: WALK_MOST_NAME, heads });
        }
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
            Err(_) if self.arena.len() + name.len() > u32::MAX as usize => Err(WalkError::ArenaFull { tabled, bytes: self.arena.len(), adding: name.len() }),
            Err(at) => {
                let depth = match (kind, Walk::item_of(name)) {
                    (RowKind::Instance, Some(item)) => {
                        let depth = self.depth_of(item, declarer);
                        if depth > self.recursion_limit {
                            return Err(WalkError::RecursionLimit { tabled, item, declarer: declarer_fn, depth, limit: self.recursion_limit });
                        }
                        depth
                    }
                    _ => 0,
                };
                let row = self.symbols.len();
                self.declarer.push(declarer);
                self.depth.push(depth);
                self.slots[at] = row as u32 + 1;
                self.symbols.push(symbol);
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
    ///
    /// A walk with no gate: its maker takes the [`Fixture`] grant, so a maker whose grant is the
    /// gate's proof cannot be driven by it (blaze-cli run_parent.rs steps under the gate).
    pub fn run<M: Make<Grant = Fixture>>(
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
        let grant = Fixture(());
        while at < self.len() {
            if self.how[at] == How::Waiting {
                self.one(at, ids, store, builder, producer, make, &grant, form, most, &mut scratch)?;
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
    ///
    /// `grant` is the row's proof ([`Make::Grant`]), handed to the make unread: the caller made
    /// it from this row (blaze-cli run_parent.rs `Cleared`), so nothing here compares it.
    pub fn one<'w, M: Make>(
        &'w mut self,
        row: usize,
        ids: &dyn BodyIds,
        store: &mut dyn CodeStore,
        builder: &Builder,
        producer: &ProducerId<CodeBuilder>,
        make: &mut M,
        grant: &M::Grant,
        form: &dyn Form,
        most: usize,
        scratch: &mut Scratch<'a>,
    ) -> Result<One<'w>, WalkError<M::Error>> {
        scratch.made.clear();
        // The same three parts the pool drives apart (D194: begin and finish on the relay, the
        // make on a worker), here in one call: the seam test `split_steps_equal_one` holds the
        // two equal (rule 78).
        let pending = match self.start(row, ids, store, builder, producer, make, form, most, scratch)? {
            Began::Stepped(stepped) => return Ok(One { stepped, declared: self.declared(row) }),
            Began::Make(pending) => pending,
        };
        let Scratch { name, made, declared, .. } = scratch;
        if let Err(why) = pending.order.make(make, grant, name.as_slice(), made) {
            self.how[pending.row()] = How::Waiting;
            return Err(why);
        }
        let stepped = self.end(pending, made.as_slice(), store, form, most, declared)?;
        Ok(One { stepped, declared: self.declared(row) })
    }

    /// The first part of [`Walk::one`] for row `row`, on the walk's thread: the instance keyed
    /// from text and today's facts, looked up in `store`; found, its declared rows are reached
    /// and the row is done ([`Begun::Stepped`]); not found (or unkeyed), the row is
    /// [`How::Making`] and its make is handed out ([`Begun::Make`]): the caller makes the entry
    /// (anywhere: [`Order::make`] with the row's name, [`Walk::name`]) and hands it to
    /// [`Walk::finish`], or gives the row back by [`Walk::abandon`]. A row done before answers
    /// what it holds, with no store or seam call; a row being made is refused
    /// ([`WalkError::Making`]). On an error the row stays [`How::Waiting`]. [`Scratch::made`] is
    /// not touched. A found entry with a read list is taken only when `verify`'s
    /// [`Make::verify`] says it holds; nothing is made here.
    pub fn begin<'w, M: Make>(
        &'w mut self,
        row: usize,
        ids: &dyn BodyIds,
        store: &mut dyn CodeStore,
        builder: &Builder,
        producer: &ProducerId<CodeBuilder>,
        verify: &mut M,
        form: &dyn Form,
        most: usize,
        scratch: &mut Scratch<'a>,
    ) -> Result<Begun<'w>, WalkError<M::Error>> {
        Ok(match self.start(row, ids, store, builder, producer, verify, form, most, scratch)? {
            Began::Stepped(stepped) => Begun::Stepped(One { stepped, declared: self.declared(row) }),
            Began::Make(pending) => Begun::Make(pending),
        })
    }

    /// The last part of [`Walk::one`], on the walk's thread: `made` is the entry made for
    /// `pending`'s row ([`Order::make`]); it is put under the row's key when it has one, and
    /// the rows it declares are reached, in its order, with nothing done for them. On an error
    /// the row is [`How::Waiting`] again (a row reached before the error stays reached), as
    /// `one`'s.
    pub fn finish<'w, E>(&'w mut self, pending: Pending, made: &[u8], store: &mut dyn CodeStore, form: &dyn Form, most: usize, scratch: &mut Scratch<'a>) -> Result<One<'w>, WalkError<E>> {
        let row = pending.row();
        let stepped = self.end(pending, made, store, form, most, &mut scratch.declared)?;
        Ok(One { stepped, declared: self.declared(row) })
    }

    /// `pending`'s row given back with nothing made (its make failed, or the run stopped): the
    /// row is [`How::Waiting`] again, as after a refused `one`.
    pub fn abandon<E>(&mut self, pending: Pending) -> Result<(), WalkError<E>> {
        let row = self.begun(&pending)?;
        self.how[row] = How::Waiting;
        Ok(())
    }

    /// The entry `key` names and the key its bytes are under: its pointer's content key, or
    /// `key` itself when it holds a plain entry (code_records.rs `entry_of`, which this is with
    /// the content key kept). A pointer to no entry is the store's failure, named.
    fn found(store: &dyn CodeStore, key: CodeKey) -> Result<Option<(CodeKey, &[u8])>, StoreError> {
        let under = store.pointed(key)?.unwrap_or(key);
        match store.get(under)? {
            Some(bytes) => Ok(Some((under, bytes))),
            None if under != key => Err(StoreError::Failed { key, what: "a code key's pointer names a content key with no entry" }),
            None => Ok(None),
        }
    }

    /// `pending`'s row, when this walk began it for that fn and it is being made.
    fn begun<E>(&self, pending: &Pending) -> Result<usize, WalkError<E>> {
        let row = pending.row();
        let tabled = pending.order.tabled;
        match (self.how.get(row), self.tabled.get(row)) {
            (Some(How::Making), Some(&at)) if at == tabled => Ok(row),
            _ => Err(WalkError::NotBegun { tabled }),
        }
    }

    /// [`Walk::begin`]'s work, its answer holding no borrow of the walk (so `one` may go on
    /// to the make after it).
    #[allow(clippy::too_many_arguments)]
    fn start<M: Make>(
        &mut self,
        row: usize,
        ids: &dyn BodyIds,
        store: &mut dyn CodeStore,
        builder: &Builder,
        producer: &ProducerId<CodeBuilder>,
        verify: &mut M,
        form: &dyn Form,
        most: usize,
        scratch: &mut Scratch<'a>,
    ) -> Result<Began, WalkError<M::Error>> {
        let Scratch { name, made: _, facts, items, declared } = scratch;
        match self.how[row] {
            How::Done(stepped) => return Ok(Began::Stepped(stepped)),
            How::Making => return Err(WalkError::Making { tabled: self.tabled[row] }),
            How::Waiting => {}
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
        // The instance's key, or why it has none: one value, so a keyed step always has its key.
        let keyed: Result<CodeKey, Unkeyed> = match unkeyed {
            Some(why) => Err(why),
            None => {
                // The name's items, each once, before any fact is looked up: a name lists an
                // item once per distinct node (symbols.rs' forest), so `items` is at most the
                // name's bytes / 21 long, and `facts` is as long as the distinct items.
                items.clear();
                each_item(name.as_slice(), |item| items.push(item)).map_err(|why| WalkError::Name { tabled, why })?;
                items.sort_unstable();
                items.dedup();
                self.most_name_items = self.most_name_items.max(items.len());
                facts.clear();
                for &item in items.iter() {
                    facts.push(ids.crate_fact(item).ok_or(WalkError::NoCrateFact { tabled, item })?);
                }
                facts.sort_unstable();
                facts.dedup();
                Ok(record_key(self.symbols[row], &body, facts.as_slice(), builder, producer))
            }
        };
        // Found: through the key's pointer to its content key (a key of an older walk may hold
        // its entry itself), and taken only when its read list holds today (the module doc,
        // "The read list"). A list that moved: made again, as a missing entry is.
        if let Ok(key) = keyed
            && let Some((under, bytes)) = Walk::found(&*store, key).map_err(WalkError::Store)?
        {
            let rows = form.reads(bytes).map_err(|why| WalkError::Entry { tabled, found: true, why })?;
            let holds = match rows {
                0 => true,
                rows => match verify.verify(tabled, name.as_slice(), bytes).map_err(|why| WalkError::Verify { tabled, why })? {
                    None => return Err(WalkError::Unverified { tabled, rows }),
                    Some(Verdict::Holds) => {
                        self.verified += 1;
                        true
                    }
                    // `Needs` is answered inside the verifier (make_one.rs `verify_instance`);
                    // one handed back is a need it could not make: built again.
                    Some(moved @ (Verdict::Stale { .. } | Verdict::Needs { .. })) => {
                        self.stale_reads += 1;
                        self.first_moved.get_or_insert((tabled, moved));
                        false
                    }
                },
            };
            if holds {
                form.declared(bytes, declared).map_err(|why| WalkError::Entry { tabled, found: true, why })?;
                let stepped = Stepped::Found(under);
                self.reach_declared(row, stepped, most, declared)?;
                return Ok(Began::Stepped(stepped));
            }
        }
        // The toolchain's when every item of its name is (`items` is the name's, filled when it
        // was keyed above), else the program's; an unkeyed entry is never put, so its class is
        // never read.
        let toolchain = keyed.is_ok() && !items.is_empty() && items.iter().all(|&item| ids.class(item) == CodeClass::Toolchain);
        let class = if toolchain { CodeClass::Toolchain } else { CodeClass::Program };
        self.how[row] = How::Making;
        Ok(Began::Make(Pending { row: row as u32, order: Order { tabled, kind: self.kinds[row] }, keyed, class }))
    }

    /// [`Walk::finish`]'s work: `made` read for its declared rows, put when keyed, the rows
    /// reached; on an error the row is waiting again.
    fn end<E>(&mut self, pending: Pending, made: &[u8], store: &mut dyn CodeStore, form: &dyn Form, most: usize, declared: &mut DeclaredList<'a>) -> Result<Stepped, WalkError<E>> {
        let row = self.begun(&pending)?;
        let Pending { order, keyed, class, .. } = pending;
        let tabled = order.tabled;
        declared.clear();
        // Put under its content key, and the instance's key pointed to it: an entry made again
        // because its read list moved has other bytes under the same instance key, which a put
        // under that key would refuse as two values (rule 21).
        let content = crate::code_records::content_key(made);
        let done = form
            .declared(made, declared)
            .map_err(|why| WalkError::Entry { tabled, found: false, why })
            .and_then(|()| match keyed {
                Ok(key) => store.put(class, content, made).and_then(|()| store.point(class, key, content)).map_err(WalkError::Store),
                Err(_) => Ok(()),
            })
            .and_then(|()| {
                let stepped = match keyed {
                    Ok(_) => Stepped::Made(content),
                    Err(why) => Stepped::Unkeyed(why),
                };
                self.reach_declared(row, stepped, most, declared).map(|()| stepped)
            });
        if done.is_err() {
            self.how[row] = How::Waiting;
        }
        done
    }

    /// Row `row` done as `stepped`: the rows `declared` names reached, in the entry's order; on
    /// a refusal the row's edges so far are dropped and its `How` is left as it was.
    fn reach_declared<E>(&mut self, row: usize, stepped: Stepped, most: usize, declared: &DeclaredList<'_>) -> Result<(), WalkError<E>> {
        let start = self.edges.len();
        let (instances, statics) = (declared.len(), declared.statics().len());
        for i in 0..instances + statics + declared.thread_locals().len() {
            let by = row as u32;
            let reached = if i < instances {
                let (callee, callee_name) = declared.row(i);
                self.reach(callee, callee_name, RowKind::Instance, most, by)
            } else if i < instances + statics {
                let item = declared.statics()[i - instances];
                self.reach(item, &static_name(item), RowKind::Static, most, by)
            } else {
                let item = declared.thread_locals()[i - instances - statics];
                self.reach(item, &static_name(item), RowKind::ThreadLocal, most, by)
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
        self.how[row] = How::Done(stepped);
        Ok(())
    }
}

/// [`Walk::start`]'s answer: [`Begun`] with no borrow of the walk.
enum Began {
    Stepped(Stepped),
    Make(Pending),
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
            // One argument, one node (the ADT, no children), the argument's reference to it.
            Some(adt) => {
                out.extend_from_slice(&1u32.to_le_bytes());
                out.extend_from_slice(&1u32.to_le_bytes());
                out.push(12);
                out.extend_from_slice(&adt.to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
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

    /// The model's grant: these tests drive [`Walk::one`] with no gate.
    const GRANT: Fixture = Fixture(());

    impl Make for Model<'_> {
        type Error = NameError;
        type Grant = Fixture;
        fn make(&mut self, _grant: &Fixture, tabled: ItemKey, _name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
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

        fn make_static(&mut self, _grant: &Fixture, _item: ItemKey, _entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            panic!("the five-instance model declares no static");
        }

        fn make_thread_local(&mut self, _grant: &Fixture, _item: ItemKey, _entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
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
        type Grant = Fixture;
        fn make(&mut self, _grant: &Fixture, tabled: ItemKey, _name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            assert_eq!(tabled.0, MAIN, "the statics model has one fn");
            self.fns += 1;
            entry.extend_from_slice(&S.to_le_bytes());
            entry.extend_from_slice(&u32::MAX.to_le_bytes());
            Ok(())
        }

        fn make_static(&mut self, _grant: &Fixture, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            self.statics.push(item);
            if item.0 == S {
                entry.extend_from_slice(&T.to_le_bytes());
                entry.extend_from_slice(&u32::MAX.to_le_bytes());
            }
            Ok(())
        }

        fn make_thread_local(&mut self, _grant: &Fixture, _item: ItemKey, _entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
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
        // The node's tag is at byte 24 (key 16, argument count 4, node count 4).
        name[24] = 99;
        assert_eq!(each_item(&name, |_| {}), Err(NameError::Tag { tag: 99, at: 24 }));
        name[24] = 12;
        // The argument's reference past the one node: refused, never read past.
        let last = name.len() - 4;
        name[last] = 1;
        assert_eq!(each_item(&name, |_| {}), Err(NameError::Ref { at: last }));
    }

    /// The polymorphic recursion model: `fn r<T>() { r::<(T,)>() }` (rustc: "reached the
    /// recursion limit while instantiating `r::<(((...,),),)>`"). `R` at depth `d` is named by
    /// `T` = `bool` in `d` one-tuples: in the forest, `d + 1` nodes.
    const R: u128 = 0xC001;

    /// The name of `r` at depth `depth` (symbols.rs' form, written by hand).
    fn r_name(depth: u32, out: &mut AVec<'_, u8>) {
        out.extend_from_slice(&R.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(depth + 1).to_le_bytes());
        // `bool`, no children.
        out.push(0);
        out.extend_from_slice(&0u32.to_le_bytes());
        // Each one-tuple of the node before it.
        for k in 1..=depth {
            out.push(7);
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(&(k - 1).to_le_bytes());
        }
        out.extend_from_slice(&depth.to_le_bytes());
    }

    /// The model's maker: `r` at depth `d` declares `r` at `d + 1`, while `d + 1 < stop`.
    struct RecModel<'a> {
        alloc: Alloc<'a>,
        stop: u32,
    }

    impl Make for RecModel<'_> {
        type Error = NameError;
        type Grant = Fixture;
        fn make(&mut self, _grant: &Fixture, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            assert_eq!(tabled.0, R, "the recursion model has one fn");
            let depth = u32::from_le_bytes([name[20], name[21], name[22], name[23]]) - 1;
            if depth + 1 < self.stop {
                let mut callee: AVec<'_, u8> = Vec::new_in(self.alloc);
                r_name(depth + 1, &mut callee);
                entry.extend_from_slice(&R.to_le_bytes());
                entry.extend_from_slice(&(callee.len() as u32).to_le_bytes());
                entry.extend_from_slice(&callee);
            }
            Ok(())
        }

        fn make_static(&mut self, _grant: &Fixture, _item: ItemKey, _entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            panic!("the recursion model declares no static");
        }

        fn make_thread_local(&mut self, _grant: &Fixture, _item: ItemKey, _entry: &mut AVec<'_, u8>) -> Result<(), NameError> {
            panic!("the recursion model declares no thread-local");
        }
    }

    /// (c) Polymorphic recursion stops at the recursion limit with a typed error, rustc's
    /// words, the item and its declarer named: 128 by default, so `r` at depths 0..=128 are
    /// rows and depth 129 is refused (rustc's collector: a def's depth is the count of its
    /// instances above it, refused past the limit). CONTROLS: a raised limit (`#![recursion_limit
    /// = "200"]`) moves the stop to 201; a recursion that ends at depth 128 walks whole.
    #[test]
    fn polymorphic_recursion_stops_at_the_recursion_limit() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let ids = StaticIds { text: [(R, 0), (S, 0), (T, 0)] };
        let producer = model_producer();
        let walk_to = |limit: Option<u32>, stop: u32| {
            let mut store = MemoryStore::new_in(alloc);
            let mut walk = Walk::new_in(alloc);
            if let Some(limit) = limit {
                walk.set_recursion_limit(limit);
            }
            let mut model = RecModel { alloc, stop };
            let mut root: AVec<'_, u8> = Vec::new_in(alloc);
            r_name(0, &mut root);
            let ran = walk.run((ItemKey(R), &root), &ids, &mut store, &builder(), &producer, &mut model, &Toy, 1 << 16);
            (ran, walk.len())
        };
        let (ran, rows) = walk_to(None, u32::MAX);
        match ran {
            Err(WalkError::RecursionLimit { tabled, item, declarer, depth, limit }) => {
                assert_eq!((tabled, item, declarer, depth, limit), (ItemKey(R), ItemKey(R), Some(ItemKey(R)), 129, 128));
            }
            other => panic!("expected the recursion limit, got {other:?}"),
        }
        assert_eq!(rows, 129, "depths 0..=128 are rows");
        let shown = std::format!("{}", WalkError::<NameError>::RecursionLimit { tabled: ItemKey(R), item: ItemKey(R), declarer: Some(ItemKey(R)), depth: 129, limit: 128 });
        assert!(shown.starts_with("reached the recursion limit while instantiating fn 0xc001"), "{shown}");
        let (ran, rows) = walk_to(Some(200), u32::MAX);
        assert!(matches!(ran, Err(WalkError::RecursionLimit { depth: 201, limit: 200, .. })), "control: the crate's limit is the one read: {ran:?}");
        assert_eq!(rows, 201);
        let (ran, rows) = walk_to(None, 129);
        assert!(ran.is_ok(), "control: a recursion that ends at depth 128 walks whole: {ran:?}");
        assert_eq!(rows, 129);
        std::println!("names: r::<(T,)> recursion refused at depth 129 of limit 128, 201 of 200; ends at 128 whole");
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
        let one = walk.one(root, &ids, &mut store, &builder, &producer, &mut model, &GRANT, &Toy, 64, &mut scratch).expect("main is made");
        let declared = <[u32; 3]>::try_from(one.declared).expect("main declares three instances");
        let Stepped::Made(key) = one.stepped else { panic!("main was not made under a key: {:?}", one.stepped) };
        assert_eq!(walk.key(root), Some(key));
        assert_eq!(walk.how(root), How::Done(Stepped::Made(key)), "the row holds what the step answered");
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

    /// D194's seam (rule 78): the walk driven apart, as the code pool drives it (every waiting
    /// row begun before any is finished, each make run apart from the walk by its own maker,
    /// the makes handed back last first), equals `Walk::one` in walk order: the same rows,
    /// each with the same `How` and key, the same declared rows in the entry's order, the same
    /// entry bytes in the store under each key, and as many makes. Keyed (every body has its
    /// record) and unkeyed (none has: each make is its own task, nothing put). Controls: a row
    /// being made is not begun again (`Making`); a pending handed to a walk that did not begin
    /// it is refused (`NotBegun`), never finished there.
    /// Prints "split_steps_equal_one: keyed 5 rows 5 makes; unkeyed 5 rows 5 makes".
    #[test]
    fn split_steps_equal_one() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let builder = builder();
        let producer = model_producer();
        let mut entry: AVec<'_, u8> = Vec::new_in(alloc);
        name_of(MAIN, None, &mut entry);
        let mut printed = std::string::String::new();
        for recorded in [true, false] {
            let mut ids = Ids::new([0, 0]);
            ids.recorded = recorded;

            let mut serial_store = MemoryStore::new_in(alloc);
            let mut serial_model = Model { alloc, calls: 0 };
            let mut serial = Walk::new_in(alloc);
            serial.run((ItemKey(MAIN), &entry), &ids, &mut serial_store, &builder, &producer, &mut serial_model, &Toy, 64).expect("the serial walk");

            let mut split_store = MemoryStore::new_in(alloc);
            let mut split_model = Model { alloc, calls: 0 };
            let mut split = Walk::new_in(alloc);
            let mut scratch = Scratch::new_in(alloc);
            split.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
            loop {
                let mut wave: std::vec::Vec<(Pending, AVec<'_, u8>)> = std::vec::Vec::new();
                for row in 0..split.len() {
                    if split.how(row) != How::Waiting {
                        continue;
                    }
                    let begun = split.begin(row, &ids, &mut split_store, &builder, &producer, &mut split_model, &Toy, 64, &mut scratch).expect("begun");
                    let pending = match begun {
                        Begun::Stepped(_) => continue,
                        Begun::Make(pending) => pending,
                    };
                    assert_eq!(split.how(row), How::Making);
                    let name: std::vec::Vec<u8> = split.name(row).to_vec();
                    let mut made: AVec<'_, u8> = Vec::new_in(alloc);
                    pending.order().make(&mut split_model, &GRANT, &name, &mut made).expect("made apart from the walk");
                    wave.push((pending, made));
                }
                let Some((first, _)) = wave.first() else { break };
                let again = split.begin(first.row(), &ids, &mut split_store, &builder, &producer, &mut split_model, &Toy, 64, &mut scratch);
                assert!(matches!(again, Err(WalkError::Making { .. })), "control: a row being made is begun again: {again:?}");
                while let Some((pending, made)) = wave.pop() {
                    split.finish::<NameError>(pending, &made, &mut split_store, &Toy, 64, &mut scratch).expect("finished");
                }
            }

            assert_eq!((split.len(), split_model.calls, split_store.puts), (serial.len(), serial_model.calls, serial_store.puts));
            let declared = |walk: &Walk<'_>, row: usize| -> std::vec::Vec<Symbol> { walk.declared(row).iter().map(|&c| walk.symbol(c as usize)).collect() };
            for row in 0..serial.len() {
                let other = split.row_of(&serial.symbol(row)).expect("the split walk reached every row the serial one did");
                assert_eq!(split.how(other), serial.how(row), "row {row}");
                assert_eq!(declared(&split, other), declared(&serial, row), "row {row}: the entry's order");
                match serial.key(row) {
                    Some(key) => assert_eq!(split_store.get(key).expect("read"), serial_store.get(key).expect("read"), "row {row}: one entry under one key"),
                    None => assert!(!recorded, "row {row}: a keyed model made an unkeyed row"),
                }
            }

            // Control: a pending of another walk's row is refused here, and this walk is
            // unchanged by it.
            let mut stranger = Walk::new_in(alloc);
            stranger.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
            let mut empty = MemoryStore::new_in(alloc);
            let Begun::Make(foreign) = stranger.begin(0, &ids, &mut empty, &builder, &producer, &mut split_model, &Toy, 64, &mut scratch).expect("begun") else {
                panic!("an empty store found main");
            };
            let before = split.how(0);
            let refused = split.finish::<NameError>(foreign, &[], &mut split_store, &Toy, 64, &mut scratch).map(|one| one.stepped);
            assert!(matches!(refused, Err(WalkError::NotBegun { tabled: ItemKey(MAIN) })), "control: {refused:?}");
            assert_eq!(split.how(0), before);
            printed.push_str(&std::format!("{} {} rows {} makes; ", if recorded { "keyed" } else { "unkeyed" }, split.len(), split_model.calls));
        }
        println!("split_steps_equal_one: {}", printed.trim_end_matches("; "));
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
        let one = lazy.one(main_row, &ids, &mut lazy_store, &builder, &producer, &mut lazy_model, &GRANT, &Toy, 64, &mut scratch).expect("main is made");
        assert_eq!(one.declared[2], 0, "main's step names plain's old row");
        let mut asked = 1;
        while let Some(row) = last_waiting(&lazy) {
            lazy.one(row, &ids, &mut lazy_store, &builder, &producer, &mut lazy_model, &GRANT, &Toy, 64, &mut scratch).expect("the row is done");
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
        let first = walk.one(root, &ids, &mut store, &builder, &producer, &mut model, &GRANT, &Toy, 64, &mut scratch).expect("main is made");
        let first = (first.stepped, <[u32; 3]>::try_from(first.declared).expect("three callees"));
        let second = walk.one(root, &ids, &mut store, &builder, &producer, &mut model, &GRANT, &Toy, 64, &mut scratch).expect("main is held");
        let second = (second.stepped, <[u32; 3]>::try_from(second.declared).expect("three callees"));
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
            let one = walk.one(row, &ids, &mut store, &builder, &producer, &mut model, &GRANT, &Toy, 64, &mut scratch).expect("the row is found");
            assert!(matches!(one.stepped, Stepped::Found(_)), "a row over a full store was not found: {:?}", one.stepped);
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
        Model { alloc, calls: 0 }.make(&Fixture(()), ItemKey(MAIN), &entry, &mut expected).expect("main's entry");
        assert!(!expected.is_empty(), "main's entry declares its callees");

        let mut dropped = Ids::new([0, 0]);
        dropped.recorded = false;
        let mut store = MemoryStore::new_in(alloc);
        let mut model = Model { alloc, calls: 0 };
        let mut walk = Walk::new_in(alloc);
        let mut scratch = Scratch::new_in(alloc);
        let root = walk.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
        let one = walk.one(root, &dropped, &mut store, &builder, &producer, &mut model, &GRANT, &Toy, 64, &mut scratch).expect("main is made");
        assert_eq!((one.stepped, one.stepped.key(), one.declared.len()), (Stepped::Unkeyed(Unkeyed::NoRecord), None, 3));
        assert_eq!(scratch.made(), expected.as_slice());
        assert_eq!((store.len(), store.puts, model.calls), (0, 0, 1));

        let ids = Ids::new([0, 0]);
        let mut keyed_store = MemoryStore::new_in(alloc);
        let mut keyed = Walk::new_in(alloc);
        let root = keyed.root::<NameError>((ItemKey(MAIN), &entry), 64).expect("main is reached");
        let one = keyed.one(root, &ids, &mut keyed_store, &builder, &producer, &mut model, &GRANT, &Toy, 64, &mut scratch).expect("main is made");
        let Stepped::Made(key) = one.stepped else { panic!("main was not made under a key: {:?}", one.stepped) };
        assert_eq!(keyed_store.get(key).expect("a memory store reads"), Some(scratch.made()));
        assert_eq!(scratch.made(), expected.as_slice());
    }
}
