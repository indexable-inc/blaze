//! Code records on the run path, keyed in full (docs/v2/plan/design/run.md, "End state",
//! "The code record"; pieces 3 and 7).
//!
//! ONE INSTANCE, ONE RECORD. [`make_entry`] is the unit the run's parent makes when a stub's
//! first call asks for a symbol it has no record of: the instance's built LIR emitted by
//! `blaze_emit::emit_record` with every outside reference named by symbol or C name
//! ([`InstanceNames`]: no slot number, no address, no `Via`), sealed into its one canonical form,
//! encoded, and wrapped in an ENTRY with what the record declares (each instance its targets
//! and vtables reach, by instance name, so the asker can make a callee from its symbol alone).
//! A function's address is its body's address through its import word; there is no slot stub
//! in this form.
//!
//! THE KEY ([`code_key`]) is computed from text before the work, from exactly these parts:
//!
//! 1. the instance's SYMBOL (symbols.rs: its item's key and its ground arguments);
//! 2. the digest of its CHECKED BODY's result ([`body_digest`]): the fn's tokens without
//!    trivia (a comment or a blank changes nothing) and the checker's read list, each question
//!    with the digest of the answer it got (a callee's signature, an ADT's definition, an impl
//!    bucket, a constant): a body edit changes its own digest and the digest of every body
//!    whose check read what moved, and no other;
//! 3. the instance's GROUND FACTS: what each projection of the fn's own types normalizes to at
//!    the instance (`blaze_build::instance_key_normalized` over the item's identity, never its
//!    text), so an edited `type Item` of an impl changes the instances that see it;
//! 4. the TARGET: the triple and the core's features the emitter reads (`Isa`);
//! 5. the TIER (`blaze_run::Tier`);
//! 6. the builder's SETTINGS that change code ([`Settings`]: overflow checks, debug
//!    assertions, the panic strategy);
//! 7. the TOOLCHAIN's identity (`blaze_toolchain::ToolchainId`);
//! 8. the FORM's version ([`ENTRY_FORM`] and blaze-code's `MAGIC`).
//!
//! Nothing of the machine: no path, time, address, host name, slot or ground-table index.
//!
//! NOT IN THE KEY, said rather than guessed (rule 3): what the BUILD reads beyond the checked
//! body (the layouts of the argument types' ADTs a generic instance lowers with, the values of
//! constants and the bytes of statics' initializers, vtable shapes) and what the LOWERING read
//! that the checker's list does not hold (front2 reads.rs `NotListed`: macro definitions, the
//! crate's edition). They are the record's READ LIST, checked against today's facts when a
//! record is found (run.md: "Key: symbol, body digest, target, tier, settings, toolchain; read
//! list for the rest"): piece 8 files and checks it. Until then a record found by this key is
//! stale after an edit of only those inputs.
//!
//! THE STORE is [`CodeStore`] (`get`, `put`): the run's parent implements it over the home's
//! packs; [`MemoryStore`] is the in-memory one the tests use. Two different values under one key
//! is a typed refusal ([`StoreError::TwoValues`], rule 21), never a second write.

use core::fmt;
use core::hash::{Hash, Hasher as _};
use std::alloc::Global;
use std::boxed::Box;

use blaze_build::Built;
use blaze_code::record::{CodeView, DecodeError, MAGIC, RecordError, Symbol};
use blaze_emit::{AddressWord, Isa, Named, Names, Os, Outside, RecordEmitError, emit_record_measured, no_address_word};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::Tier;
use blaze_toolchain::ToolchainId;
use blaze_typeck::{ItemKey, ReadList};
use parser::{Edition, LexedStr};

use crate::symbols::{SymbolError, tls_desc_symbol, tls_getter_symbol};

/// The version of the entry form and of what a key means: stepped when either changes, so an
/// old store's entries are never found (and never misread) by a new blaze.
pub const ENTRY_FORM: u32 = 1;

/// The first eight bytes of every entry.
pub const ENTRY_MAGIC: [u8; 8] = *b"bzcent01";

const KEY_DOMAIN: &str = "blaze-ladder code record key 1";
const BODY_DOMAIN: &str = "blaze-ladder checked body 1";

/// A code record's key in the store ([`code_key`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CodeKey(pub ContentId);

impl fmt::Display for CodeKey {
    /// The first 12 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// What a panic does: part of the key, since unwinding code carries landing pads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanicStrategy {
    Unwind,
    Abort,
}

/// The builder's settings that change code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settings {
    pub overflow_checks: bool,
    pub debug_assertions: bool,
    pub panic: PanicStrategy,
}

impl Settings {
    /// What the baseline tier builds today: wrapping arithmetic (blaze-build walk.rs `arith`:
    /// "overflow is not checked in this tier"), no debug assertions, and no unwinding (the
    /// emitter refuses `Invoke` and `Resume`). A record keyed by other settings is built by
    /// the same code today: over-keyed, never stale.
    pub const BASELINE: Settings = Settings { overflow_checks: false, debug_assertions: false, panic: PanicStrategy::Abort };
}

/// What every record of one run is built for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Builder {
    pub isa: Isa,
    pub tier: Tier,
    pub settings: Settings,
    pub toolchain: ToolchainId,
}

/// The target triple of the code `isa` describes (the emitter makes AArch64 code only).
pub fn triple(isa: Isa) -> &'static str {
    match isa.os {
        Os::MacOs => "aarch64-apple-darwin",
        Os::Linux => "aarch64-unknown-linux-gnu",
    }
}

/// The instance's own parts of its key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyParts {
    pub symbol: Symbol,
    /// [`body_digest`] of the fn whose body (or signature) the instance is built from.
    pub body: ContentId,
    /// The instance's ground facts (the module doc, part 3).
    pub ground: ContentId,
}

/// A bug the key tests plant, to show the oracle catches it (rule 23).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyPlant {
    None,
    /// The tier left out of the key: a fast and an optimized record would share it.
    WithoutTier,
}

/// The key of the code record of `parts` built by `builder`: the eight parts of the module doc,
/// each length-prefixed in that order.
pub fn code_key(parts: &KeyParts, builder: &Builder) -> CodeKey {
    code_key_planted(parts, builder, KeyPlant::None)
}

/// [`code_key`] with `plant` in it.
pub fn code_key_planted(parts: &KeyParts, builder: &Builder, plant: KeyPlant) -> CodeKey {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.part(&parts.symbol.0);
    h.id(&parts.body);
    h.id(&parts.ground);
    h.part(triple(builder.isa).as_bytes());
    h.part(&[u8::from(builder.isa.lse)]);
    if plant != KeyPlant::WithoutTier {
        let tier: u8 = match builder.tier {
            Tier::Fast => 0,
            Tier::Optimized => 1,
        };
        h.part(&[tier]);
    }
    let s = builder.settings;
    let panic: u8 = match s.panic {
        PanicStrategy::Unwind => 0,
        PanicStrategy::Abort => 1,
    };
    h.part(&[u8::from(s.overflow_checks), u8::from(s.debug_assertions), panic]);
    h.part(&builder.toolchain.0);
    h.part(&ENTRY_FORM.to_le_bytes());
    h.part(&MAGIC);
    CodeKey(h.finish())
}

/// A `core::hash::Hasher` that appends every integer little-endian at a fixed width (`usize`
/// and `isize` as 8 bytes), so a derived `Hash` writes the same bytes on every machine.
struct Fixed<'v, 'a>(&'v mut AVec<'a, u8>);

impl core::hash::Hasher for Fixed<'_, '_> {
    /// Never asked by a derived `Hash`; the bytes written are the value.
    fn finish(&self) -> u64 {
        self.0.len() as u64
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn write_u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn write_u16(&mut self, v: u16) {
        self.write(&v.to_le_bytes());
    }
    fn write_u32(&mut self, v: u32) {
        self.write(&v.to_le_bytes());
    }
    fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    fn write_u128(&mut self, v: u128) {
        self.write(&v.to_le_bytes());
    }
    fn write_usize(&mut self, v: usize) {
        self.write(&(v as u64).to_le_bytes());
    }
    fn write_i8(&mut self, v: i8) {
        self.write(&v.to_le_bytes());
    }
    fn write_i16(&mut self, v: i16) {
        self.write(&v.to_le_bytes());
    }
    fn write_i32(&mut self, v: i32) {
        self.write(&v.to_le_bytes());
    }
    fn write_i64(&mut self, v: i64) {
        self.write(&v.to_le_bytes());
    }
    fn write_i128(&mut self, v: i128) {
        self.write(&v.to_le_bytes());
    }
    fn write_isize(&mut self, v: isize) {
        self.write(&(v as i64).to_le_bytes());
    }
}

/// The digest of a fn's checked body (the module doc, part 2): `text` (the fn item's text) as
/// its tokens without whitespace or comments, each length-prefixed, then whether it was checked
/// and, if so, each question of its read list (sorted, as the checker keeps them) with its
/// answer's digest. A fn with no checked body (a shim built from a signature alone) digests its
/// text only. Tokens are lexed as edition 2024: which tokens are trivia does not depend on the
/// edition, and the edition itself is a lowering input (the module doc, "Not in the key").
pub fn body_digest(text: &str, reads: Option<&ReadList<'_>>, scratch: Alloc<'_>) -> ContentId {
    let mut h = Hasher::new(BODY_DOMAIN);
    let lexed = LexedStr::new(Edition::Edition2024, text);
    let mut tokens: u64 = 0;
    for i in 0..lexed.len() {
        if !lexed.kind(i).is_trivia() {
            h.part(lexed.text(i).as_bytes());
            tokens += 1;
        }
    }
    h.update(&tokens.to_le_bytes());
    match reads {
        None => {
            h.update(&[0]);
        }
        Some(list) => {
            h.update(&[1]);
            let questions = list.questions();
            h.update(&(questions.len() as u64).to_le_bytes());
            let mut bytes: AVec<'_, u8> = Vec::new_in(scratch);
            for q in questions {
                bytes.clear();
                q.hash(&mut Fixed(&mut bytes));
                h.part(&bytes);
                match list.hash_of(q) {
                    Some(answer) => h.update(&[1]).update(&answer.to_le_bytes()),
                    None => h.update(&[0]),
                };
            }
        }
    }
    h.finish()
}

/// How one function reference place is named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Callee<'n> {
    /// A program function instance, by its symbol.
    Symbol(Symbol),
    /// A C library function, by its name.
    C(&'n str),
}

/// The names of what one built instance references, by reference place (blaze-build's
/// `Built::refs`, `statics`, `vtables`, `thread_locals`): `blaze_emit::Names` over the run's
/// tables, replacing `Target::Slot` and the `via` table.
pub struct InstanceNames<'n> {
    pub funcs: &'n [Callee<'n>],
    pub statics: &'n [Symbol],
    pub vtables: &'n [Symbol],
    pub tls: &'n [Symbol],
}

impl Names for InstanceNames<'_> {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        let at = |place: u32| place as usize;
        match of {
            Outside::Func(place) => self.funcs.get(at(place)).map(|callee| match *callee {
                Callee::Symbol(symbol) => Named::Symbol(symbol),
                Callee::C(name) => Named::C(name),
            }),
            Outside::Static(place) => self.statics.get(at(place)).map(|&s| Named::Symbol(s)),
            Outside::Vtable(place) => self.vtables.get(at(place)).map(|&s| Named::Symbol(s)),
            Outside::Tls(place) => self.tls.get(at(place)).map(|&s| Named::Symbol(s)),
            Outside::TlsDesc => Some(Named::Symbol(tls_desc_symbol())),
            Outside::TlsGetter => Some(Named::Symbol(tls_getter_symbol())),
        }
    }
}

/// One instance an entry declares: the fn it is tabled as (whose body or signature builds it)
/// and its instance name (symbols.rs `encode_instance`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Declared<'b> {
    pub tabled: ItemKey,
    pub name: &'b [u8],
}

/// Why an instance has no code record.
#[derive(Debug)]
pub enum CodeRecordError {
    /// The emitter refused it.
    Emit(RecordEmitError),
    /// The record did not seal.
    Seal(RecordError),
    /// A reference has no symbol.
    Symbol(SymbolError),
    /// The store refused it.
    Store(StoreError),
    /// An entry found in the store does not read.
    Entry(EntryError),
    /// A found entry declares an instance tabled as `tabled`, which this program has no fn for.
    NoTabledFn { tabled: ItemKey },
}

impl fmt::Display for CodeRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodeRecordError::Emit(why) => why.fmt(f),
            CodeRecordError::Seal(why) => why.fmt(f),
            CodeRecordError::Symbol(why) => why.fmt(f),
            CodeRecordError::Store(why) => why.fmt(f),
            CodeRecordError::Entry(why) => why.fmt(f),
            CodeRecordError::NoTabledFn { tabled } => write!(f, "a stored code record declares an instance of fn {:#x}, which the program does not have: the key missed an input (rule 21)", tabled.0),
        }
    }
}

impl core::error::Error for CodeRecordError {}

/// One instance's entry: `built` (the instance's LIR and its reference tables) emitted for
/// `isa` with its references named by `names`, sealed and encoded, then `declared`. The unit
/// the parent makes for a stub's first call.
///
/// The form, little-endian: [`ENTRY_MAGIC`], [`ENTRY_FORM`] (4), the bytes of instructions at
/// the code's start (4), the record's length (4) and bytes (blaze-code `Sealed::encode`), the
/// count of declared instances (4), and each: its tabled fn's key (16), its name's length (4)
/// and bytes, sorted by (tabled, name) and each once.
pub fn make_entry<'a>(
    built: &Built<Global>,
    names: &InstanceNames<'_>,
    declared: &mut AVec<'_, Declared<'_>>,
    isa: Isa,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, CodeRecordError> {
    let (record, insts) = emit_record_measured(built.func.view(), names, isa, alloc).map_err(CodeRecordError::Emit)?;
    // No landing pads or unwind word yet: the emitter refuses `Invoke` and `Resume` (piece 11).
    let sealed = record.seal().map_err(CodeRecordError::Seal)?;
    let mut code: AVec<'_, u8> = Vec::new_in(alloc);
    sealed.encode(&mut code);
    declared.sort_unstable_by(|a, b| (a.tabled, a.name).cmp(&(b.tabled, b.name)));
    declared.dedup();
    let mut out: AVec<'a, u8> = Vec::new_in(alloc);
    out.extend_from_slice(&ENTRY_MAGIC);
    out.extend_from_slice(&ENTRY_FORM.to_le_bytes());
    out.extend_from_slice(&insts.to_le_bytes());
    let len = |n: usize, what: &'static str| u32::try_from(n).map_err(|_| CodeRecordError::Seal(RecordError::TooLarge { what }));
    out.extend_from_slice(&len(code.len(), "entry's record")?.to_le_bytes());
    out.extend_from_slice(&code);
    out.extend_from_slice(&len(declared.len(), "entry's declared instances")?.to_le_bytes());
    for d in declared.iter() {
        out.extend_from_slice(&d.tabled.0.to_le_bytes());
        out.extend_from_slice(&len(d.name.len(), "declared instance's name")?.to_le_bytes());
        out.extend_from_slice(d.name);
    }
    Ok(out)
}

/// Why an entry's bytes do not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryError {
    /// Not an entry: its first bytes are not [`ENTRY_MAGIC`].
    Magic,
    /// An entry of another form version (an old store): refused, never misread.
    Form { found: u32 },
    /// The entry ends inside a part (`what`).
    Truncated { what: &'static str },
    /// Bytes after the last declared instance.
    Trailing,
    /// The record inside does not read.
    Record(DecodeError),
    /// The instructions' length is past the record's code.
    Instructions(AddressWord),
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "a code record's entry (blaze-ladder code_records.rs `make_entry`)";
        match *self {
            EntryError::Magic => write!(f, "not {rule}: its first bytes are not the entry's magic"),
            EntryError::Form { found } => write!(f, "{rule} of form {found}, and this blaze reads form {ENTRY_FORM}: the store is from another blaze"),
            EntryError::Truncated { what } => write!(f, "{rule} ends inside its {what}"),
            EntryError::Trailing => write!(f, "{rule} has bytes after its last declared instance"),
            EntryError::Record(why) => write!(f, "{rule}'s record does not read: {why}"),
            EntryError::Instructions(why) => write!(f, "{rule}: {why}"),
        }
    }
}

impl core::error::Error for EntryError {}

/// An entry read in place.
#[derive(Clone, Copy)]
pub struct EntryView<'b> {
    pub insts: u32,
    pub record: CodeView<'b>,
    /// The declared instances' bytes (checked whole by [`EntryView::read`]).
    declared: &'b [u8],
    count: u32,
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Cursor<'b> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'b [u8], EntryError> {
        let end = self.at.checked_add(n).filter(|&end| end <= self.bytes.len()).ok_or(EntryError::Truncated { what })?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }
    fn u32(&mut self, what: &'static str) -> Result<u32, EntryError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn declared(&mut self) -> Result<Declared<'b>, EntryError> {
        let key = self.take(16, "declared instance's fn")?;
        let mut wide = [0u8; 16];
        wide.copy_from_slice(key);
        let len = self.u32("declared instance's name length")? as usize;
        let name = self.take(len, "declared instance's name")?;
        Ok(Declared { tabled: ItemKey(u128::from_le_bytes(wide)), name })
    }
}

impl<'b> EntryView<'b> {
    /// Reads `bytes` as an entry: the form, the record (`CodeView::read`), the instructions'
    /// length against its code, and every declared instance.
    pub fn read(bytes: &'b [u8]) -> Result<EntryView<'b>, EntryError> {
        let mut c = Cursor { bytes, at: 0 };
        if c.take(8, "magic")? != ENTRY_MAGIC {
            return Err(EntryError::Magic);
        }
        let form = c.u32("form")?;
        if form != ENTRY_FORM {
            return Err(EntryError::Form { found: form });
        }
        let insts = c.u32("instructions' length")?;
        let len = c.u32("record's length")? as usize;
        let record = CodeView::read(c.take(len, "record")?).map_err(EntryError::Record)?;
        if insts as usize > record.code().len() || insts % 4 != 0 {
            return Err(EntryError::Instructions(AddressWord::Instructions { insts, code: record.code().len() as u32 }));
        }
        let count = c.u32("count of declared instances")?;
        let start = c.at;
        for _ in 0..count {
            c.declared()?;
        }
        if c.at != bytes.len() {
            return Err(EntryError::Trailing);
        }
        Ok(EntryView { insts, record, declared: &bytes[start..], count })
    }

    /// The count of declared instances.
    pub fn declared_count(&self) -> u32 {
        self.count
    }

    /// Each declared instance, in the entry's order, passed to `each`.
    pub fn each_declared<E>(&self, mut each: impl FnMut(Declared<'b>) -> Result<(), E>, malformed: impl Fn(EntryError) -> E) -> Result<(), E> {
        let mut c = Cursor { bytes: self.declared, at: 0 };
        for _ in 0..self.count {
            each(c.declared().map_err(&malformed)?)?;
        }
        Ok(())
    }

    /// The record's instructions checked to hold no address word (blaze-emit
    /// `no_address_word`); the count of its sites.
    pub fn no_address_word(&self) -> Result<usize, AddressWord> {
        no_address_word(&self.record, self.insts)
    }
}

/// Why the store refused a write or a read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreError {
    /// A second, different value put under `key`: the key missed an input (rule 21).
    TwoValues { key: CodeKey },
    /// The store's own failure, named by its implementation (`what`) for `key`.
    Failed { key: CodeKey, what: &'static str },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            StoreError::TwoValues { key } => write!(f, "a second, different code record under key {key}: two values under one key means the key missed an input (rule 21)"),
            StoreError::Failed { key, what } => write!(f, "the code store failed for key {key}: {what}"),
        }
    }
}

impl core::error::Error for StoreError {}

/// Where code records are kept by key: the run's parent implements it over the home's packs.
pub trait CodeStore {
    /// The entry under `key`, if any.
    fn get(&self, key: CodeKey) -> Result<Option<&[u8]>, StoreError>;
    /// Writes `bytes` under `key` once; equal bytes again are no write; different bytes are
    /// [`StoreError::TwoValues`].
    fn put(&mut self, key: CodeKey, bytes: &[u8]) -> Result<(), StoreError>;
}

/// The in-memory [`CodeStore`]: entries sorted by key in the caller's allocator.
pub struct MemoryStore<'a> {
    rows: AVec<'a, (CodeKey, AVec<'a, u8>)>,
    alloc: Alloc<'a>,
    /// Writes made (equal rewrites are not counted).
    pub puts: usize,
}

impl<'a> MemoryStore<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> MemoryStore<'a> {
        MemoryStore { rows: Vec::new_in(alloc), alloc, puts: 0 }
    }

    /// The count of entries.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl CodeStore for MemoryStore<'_> {
    fn get(&self, key: CodeKey) -> Result<Option<&[u8]>, StoreError> {
        Ok(self.rows.binary_search_by(|row| row.0.cmp(&key)).ok().map(|at| &self.rows[at].1[..]))
    }

    fn put(&mut self, key: CodeKey, bytes: &[u8]) -> Result<(), StoreError> {
        match self.rows.binary_search_by(|row| row.0.cmp(&key)) {
            Ok(at) if self.rows[at].1[..] == *bytes => Ok(()),
            Ok(_) => Err(StoreError::TwoValues { key }),
            Err(at) => {
                let mut value = Vec::new_in(self.alloc);
                value.extend_from_slice(bytes);
                self.rows.insert_shift(at, (key, value));
                self.puts += 1;
                Ok(())
            }
        }
    }
}

/// One instance the record walk met.
#[derive(Clone, Debug)]
pub struct Met {
    pub symbol: Symbol,
    pub key: CodeKey,
    /// The instance's own parts of its key, kept to say which part moved when a key did.
    pub parts: KeyParts,
    /// The fn it is tabled as, as the program names it.
    pub function: Box<str>,
    /// Whether its entry was found in the store (else made and put).
    pub found: bool,
}

/// A record walk's state (run.rs `build_records`): the store, what the records are built for,
/// and what was met.
pub struct RecordRun<'s> {
    pub store: &'s mut dyn CodeStore,
    pub builder: Builder,
    /// Each instance met, in the walk's order.
    pub met: std::vec::Vec<Met>,
}

impl<'s> RecordRun<'s> {
    pub fn new(store: &'s mut dyn CodeStore, builder: Builder) -> RecordRun<'s> {
        RecordRun { store, builder, met: std::vec::Vec::new() }
    }

    /// Instances whose entries were made by this walk.
    pub fn made(&self) -> usize {
        self.met.iter().filter(|m| !m.found).count()
    }

    /// Instances whose entries were found.
    pub fn found(&self) -> usize {
        self.met.iter().filter(|m| m.found).count()
    }

    /// `built M of N: F found`, the line a run prints for its records.
    pub fn summary(&self) -> std::string::String {
        let (made, found, all) = (self.made(), self.found(), self.met.len());
        match made {
            0 => std::format!("built 0 of {all}: all found"),
            _ => std::format!("built {made} of {all}: {found} found"),
        }
    }
}
