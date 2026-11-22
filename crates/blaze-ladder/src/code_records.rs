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
//! 5. the TIER whose emitter made the code (`blaze_tier::TierName`, [`EMITTING`]: today the
//!    Reference emitter's, whatever tier the command asked for);
//! 6. the builder's SETTINGS that change code ([`Settings`]: overflow checks, debug
//!    assertions, the panic strategy);
//! 7. the TOOLCHAIN's identity (`blaze_toolchain::ToolchainId`);
//! 8. the FORM's version ([`ENTRY_FORM`] and blaze-code's `MAGIC`).
//!
//! Then the code builder's identity (D105 `ProducerId<CodeBuilder>`) and the program
//! package's edition ([`Builder`]).
//!
//! Nothing of the machine: no path, time, address, host name, slot or ground-table index.
//!
//! NOT IN THE KEY: what the BUILD reads beyond the checked body (the layouts of the ADTs an
//! instance lowers with, constant values, the impl a call resolves to, vtable shapes). That is
//! the entry's READ LIST ([`reads`]): rows of (question, answer digest), recorded around the
//! world the builder is handed and checked against today's facts when the entry is found
//! (run.md: "Key: symbol, body digest, target, tier, settings, toolchain; read list for the
//! rest"). What neither holds is said in [`reads`]' module doc (macro definitions, the editions
//! of the program's other crates, lang items and attributes of workspace crates).
//!
//! THE STORE is [`CodeStore`] (`get`, `put`): the run's parent implements it over the home's
//! packs; [`MemoryStore`] is the in-memory one the tests use. Two different values under one key
//! is a typed refusal ([`StoreError::TwoValues`], rule 21), never a second write.

use core::fmt;
use std::alloc::Global;
use std::boxed::Box;

use blaze_build::tls;
use blaze_code::record::{CodeView, DecodeError, MAGIC, RecordError, Symbol, TrapSource};
use blaze_emit::{AddressWord, Isa, Named, Names, Os, Outside, RecordEmitError, emit_sota_record_measured, no_address_word};
use blaze_ids::producer::{CodeBuilder, ProducerId};
use blaze_ids::{ContentId, Hasher};
use blaze_lir::Verified;
use blaze_own::{AVec, Alloc, Vec};
pub use blaze_tier::TierName;
use blaze_toolchain::ToolchainId;
use edition::PackageEdition;
use blaze_typeck::ItemKey;

use crate::front2::body_reads::ReadsDigest;
use parser::{Edition, LexedStr};

use crate::symbols::{SymbolError, static_symbol, tls_desc_symbol, tls_getter_symbol, tls_symbol};

#[path = "code_reads.rs"]
pub mod reads;

use reads::ReadList;

/// A static's entry (its datum record and what its words reach): run.md, "Statics in the run".
#[path = "static_entry.rs"]
pub mod statics;

/// The version of the entry form and of what a key means: stepped when either changes, so an
/// old store's entries are never found (and never misread) by a new blaze. 4: an entry declares
/// the statics its record names (by item key), and a static has an entry of its own
/// ([`statics`]). 5: an entry also declares the thread-locals its record names (by item key),
/// and a thread-local's template has an entry of its own (`statics::make_template_entry`).
/// 6: a code entry carries the datum of each vtable its record names ([`VtableDatum`]).
/// 7: wide pointers cross calls in two registers and transparent values use their field's ABI.
/// 8: the instance names an entry declares are symbols.rs' form 2 (a shared forest, each
/// distinct type once), never form 1's trees: an entry of 7 is refused by its form, never
/// listed by `body_ids::each_item` as a forest.
pub const ENTRY_FORM: u32 = 8;

/// One vtable a code record names, as the entry carries it (run.md, "Statics in the run": a
/// vtable is data the child places before the record, as a static's datum is): its symbol
/// (symbols.rs `vtable_symbol`, from the trait reference and the concrete type) and its datum
/// (blaze-code datum.rs, read-only): the header words (the drop glue's entry, or 0, the size,
/// the alignment), then each method's entry, each fn word a `Space::Func` target by the
/// instance's symbol and each supertrait's table a `Space::Vtable` target. The datum names
/// instances by symbol, never by code, so an edit of a method's body moves that method's record
/// alone; the datum's bytes are content-named in the records folder, so every record naming one
/// vtable files it once.
#[derive(Clone, Copy, Debug)]
pub struct VtableDatum<'d> {
    pub symbol: Symbol,
    pub datum: &'d [u8],
}

/// The first eight bytes of every entry.
pub const ENTRY_MAGIC: [u8; 8] = *b"bzcent01";

const KEY_DOMAIN: &str = "blaze-ladder code record key 2";
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
    /// The profile's `overflow-checks` (D101): the walk of a record run builds with exactly
    /// this (run.rs `build_records`), so two profiles never share a record.
    pub overflow_checks: blaze_build::OverflowChecks,
    pub debug_assertions: bool,
    pub panic: PanicStrategy,
}

impl Settings {
    /// What the baseline tier builds at `overflow_checks` with the program's panic strategy
    /// `panic` (UNWIND.md 3.1; no default: the caller names it), no debug assertions. On the
    /// record path this IS where the program's strategy lives: the walk and the program's
    /// inputs are made from it (run.rs `build_records_new`). Debug assertions decide what
    /// core's `intrinsics::ub_checks()` is in the code built (blaze-build `UbChecks`), so they
    /// are in the key for that.
    pub const fn baseline(overflow_checks: blaze_build::OverflowChecks, panic: blaze_build::Panic) -> Settings {
        Settings { overflow_checks, debug_assertions: false, panic: crate::panic_strategy::strategy(panic) }
    }
}

/// The tier whose emitter makes every entry: [`make_entry`] calls the Reference emitter
/// (`blaze_emit::emit_record_measured`) and no other, so this is the one tier a [`Builder`]
/// may name; a run whose builder names another is refused before its first entry is made
/// ([`Builder::emits`]).
pub const EMITTING: TierName = TierName::Reference;

/// What every record of one run is built for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Builder {
    pub isa: Isa,
    /// The tier whose emitter makes the code: the byte the key holds is the emitter that ran,
    /// never the tier the command asked for ([`EMITTING`]).
    pub tier: TierName,
    pub settings: Settings,
    pub toolchain: ToolchainId,
    /// The code builder's own identity (D105 `Row::CodeBuilder`): the digest of the code that
    /// builds and emits the entry, so a blaze whose builder changed finds none of an older one's.
    pub producer: ProducerId<CodeBuilder>,
    /// The program package's edition (its manifest's `edition`, read once by
    /// `PackageEdition::of_manifest`): the lowering reads it beyond the checker's list (front2
    /// reads.rs `NotListed`). The editions of the run's other crates are not in it: the front
    /// end gives no instance its own crate's edition (said in [`code_key`]'s doc).
    pub edition: PackageEdition,
}

/// A builder naming a tier whose emitter [`make_entry`] does not call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotEmitting {
    pub named: TierName,
}

impl fmt::Display for NotEmitting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the code builder names the {} tier, but every entry is made by the {} emitter: a key naming {} would hold code it did not make", self.named.as_str(), EMITTING.as_str(), self.named.as_str())
    }
}

impl Builder {
    /// The builder, refused when its tier is not the one whose emitter makes entries.
    pub fn emits(self) -> Result<Builder, NotEmitting> {
        if self.tier == EMITTING { Ok(self) } else { Err(NotEmitting { named: self.tier }) }
    }
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
    /// The overflow setting left out of the key: a dev and a release record would share it.
    WithoutOverflow,
    /// The code builder's identity left out: two blazes' builders would share a key.
    WithoutProducer,
    /// The edition left out: a 2021 and a 2024 program's record would share a key.
    WithoutEdition,
}

/// The key of the code record of `parts` built by `builder`: the eight parts of the module doc,
/// then the code builder's identity and the program package's edition, each length-prefixed
/// in that order. The tier byte is `TierName`'s own (`Reference` 0, `Fast` 1, `Optimized` 2):
/// the emitter that made the code. The edition of a crate other than the program's is not in
/// it (the front end names no instance's crate edition): an edition edit of a dependency
/// alone moves no key, a gap the read list does not close either.
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
        h.part(&[builder.tier as u8]);
    }
    let s = builder.settings;
    let panic: u8 = match s.panic {
        PanicStrategy::Unwind => 0,
        PanicStrategy::Abort => 1,
    };
    let overflow = s.overflow_checks.on() && plant != KeyPlant::WithoutOverflow;
    h.part(&[u8::from(overflow), u8::from(s.debug_assertions), panic]);
    h.part(&builder.toolchain.0);
    if plant != KeyPlant::WithoutProducer {
        h.id(builder.producer.id());
    }
    if plant != KeyPlant::WithoutEdition {
        let blaze = match builder.edition.blaze {
            None => 0,
            Some(edition) => 1 + edition as u8,
        };
        h.part(&[builder.edition.syntax as u8, blaze]);
    }
    h.part(&ENTRY_FORM.to_le_bytes());
    h.part(&MAGIC);
    CodeKey(h.finish())
}

/// A `core::hash::Hasher` that appends every integer little-endian at a fixed width (`usize`
/// and `isize` as 8 bytes), so a derived `Hash` writes the same bytes on every machine.
pub(crate) struct Fixed<'v, 'a>(pub(crate) &'v mut AVec<'a, u8>);

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
/// and, if so, the digest of its read list ([`ReadsDigest`]: each question, sorted as the
/// checker keeps them, with its answer's hash), filed when the body was checked (front2
/// body_reads.rs). A fn with no checked body (a shim built from a signature alone) digests its
/// text only. Tokens are lexed as edition 2024: which tokens are trivia does not depend on the
/// edition, and the edition itself is a lowering input (the module doc, "Not in the key").
/// [`ENTRY_FORM`] 2: the list enters as its digest.
pub fn body_digest(text: &str, reads: Option<ReadsDigest>) -> ContentId {
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
        None => h.update(&[0]),
        Some(list) => h.update(&[1]).update(&list.0.0),
    };
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
    /// The statics, by item key: each is named by its symbol (symbols.rs `static_symbol`) and
    /// declared by the entry, so the run's parent can send it before the record.
    pub statics: &'n [ItemKey],
    /// The statics among `statics` an `extern` block declares, each with its C symbol (its
    /// `#[link_name]`, else its name): its place is that symbol (bound as a foreign fn is,
    /// blaze-emit record.rs `target_of`), and the entry does not declare it, since it has no
    /// record of its own (the Reference, "External blocks", "Statics": no initializer).
    pub c_statics: &'n [(ItemKey, &'n str)],
    pub vtables: &'n [Symbol],
    /// The thread-locals, by item key: each is named by its symbol (symbols.rs `tls_symbol`)
    /// and declared by the entry, so the run's parent can send its template before the record.
    pub tls: &'n [ItemKey],
}

impl Names for InstanceNames<'_> {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        let at = |place: u32| place as usize;
        match of {
            Outside::Func(place) => self.funcs.get(at(place)).map(|callee| match *callee {
                Callee::Symbol(symbol) => Named::Symbol(symbol),
                Callee::C(name) => Named::C(name),
            }),
            Outside::Static(place) => self.statics.get(at(place)).map(|&s| match self.c_statics.iter().find(|(k, _)| *k == s) {
                Some(&(_, c)) => Named::C(c),
                None => Named::Symbol(static_symbol(s)),
            }),
            Outside::Vtable(place) => self.vtables.get(at(place)).map(|&s| Named::Symbol(s)),
            Outside::Tls(place) => self.tls.get(at(place)).map(|&s| Named::Symbol(tls_symbol(s))),
            Outside::TlsDesc => Some(Named::Symbol(tls_desc_symbol())),
            Outside::TlsGetter => Some(Named::Symbol(tls_getter_symbol())),
        }
    }

    /// None: an instance's routines are bound after its own references (front2/builtins_shim.rs
    /// `Bound::bind` appends them to `funcs`) and handed beside these names as `Routines`;
    /// [`Routed`] puts the two together for a tier behind the seam.
    fn routines(&self) -> blaze_tier::RoutinePlaces {
        blaze_tier::RoutinePlaces::NONE
    }
}

/// An instance's names with the routine table its `funcs` holds the routines at: what a tier
/// behind the seam (`blaze_tier::Tier::emit`) is handed, so the routine table is the seam's input
/// (D197 root 6, piece (a)). [`make_entry`] calls the Reference emitter directly today
/// ([`EMITTING`]) and takes the table as `Routines`; it hands a `Routed` once the run places the
/// seam's record form (piece (b)).
pub struct Routed<'n> {
    pub names: &'n InstanceNames<'n>,
    pub routines: blaze_tier::RoutinePlaces,
}

impl Names for Routed<'_> {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        self.names.name(of)
    }

    fn routines(&self) -> blaze_tier::RoutinePlaces {
        self.routines
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
    /// A fn with a checked body whose read-list digest the front end did not file
    /// ([`crate::seam::FrontEnd::body_reads`]).
    NoReads { item: ItemKey },
    /// The thread-local getter (blaze-build `tls::index::getter`) did not build.
    Getter(blaze_lir::BuildError),
    /// The thread-local ender (blaze-build `tls::index::ender`) did not build.
    Ender(blaze_lir::BuildError),
    /// The C entry shim (blaze-build `start::c_main_shim`, run/special.rs) did not build.
    Shim(blaze_build::BuildError),
    /// An `f128` operation needs a soft-float routine the program's compiler_builtins does
    /// not give (front2/builtins_shim.rs).
    Routine(crate::front2::builtins_shim::BuiltinsRefused),
    /// The build's read list could not be made, or a found entry's could not be asked again.
    Reads(reads::ReadsError),
    /// The run's builder names a tier whose emitter does not make entries.
    NotEmitting(NotEmitting),
    /// A vtable the record names did not make a datum (blaze-code datum.rs names the rule).
    VtableDatum(blaze_code::datum::DatumError),
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
            CodeRecordError::NoReads { item } => write!(f, "fn {:#x} has a checked body but no read-list digest was filed for it (front2 body_reads.rs): its record's key would miss what its check read", item.0),
            CodeRecordError::Getter(why) => write!(f, "the thread-local getter did not build: {why}"),
            CodeRecordError::Ender(why) => write!(f, "the thread-local ender did not build: {why}"),
            CodeRecordError::Shim(why) => write!(f, "the C entry shim did not build: {why}"),
            CodeRecordError::Routine(why) => why.fmt(f),
            CodeRecordError::Reads(why) => why.fmt(f),
            CodeRecordError::NotEmitting(why) => why.fmt(f),
            CodeRecordError::VtableDatum(why) => write!(f, "a vtable the record names did not make its datum (code_records.rs `VtableDatum`): {why}"),
        }
    }
}

impl core::error::Error for CodeRecordError {}

/// One instance's entry: `func` (an instance's built LIR, blaze-build `Built::func`, or the
/// thread-local getter's) emitted for `isa` with its references named by `names` (from the
/// instance's reference tables), sealed and encoded, then `declared`. The unit the parent
/// makes for a stub's first call.
///
/// The form, little-endian: [`ENTRY_MAGIC`], [`ENTRY_FORM`] (4), the bytes of instructions at
/// the code's start (4), the record's length (4) and bytes (blaze-code `Sealed::encode`), the
/// count of declared instances (4), and each: its tabled fn's key (16), its name's length (4)
/// and bytes, sorted by (tabled, name) and each once; the count of declared statics (4) and
/// each one's item key (16), ascending and each once: every static `names` holds (form 4: the
/// parent sends each before the record, run.md "Statics in the run"); the declared
/// thread-locals in the same form (form 5: every thread-local `names` holds, whose template the
/// parent sends before the record); the vtables' data (form 6: [`make_entry_with_vtables`]);
/// then the count of read-list rows (4) and
/// each: its question's length (4) and bytes, and its answer's digest (32), sorted by question
/// and each once ([`reads`]). An entry made here reads nothing beyond its key: no row.
pub fn make_entry<'a, 'd>(
    func: Verified<'_>,
    names: &InstanceNames<'_>,
    routines: crate::front2::builtins_shim::Routines<'d>,
    declared: &mut AVec<'_, Declared<'d>>,
    isa: Isa,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, CodeRecordError> {
    make_entry_reading(func, names, routines, declared, &ReadList::empty(), isa, alloc)
}

/// [`make_entry`] with the build's read list `reads` ([`reads::ReadLog::seal`]).
pub fn make_entry_reading<'a, 'd>(
    func: Verified<'_>,
    names: &InstanceNames<'_>,
    routines: crate::front2::builtins_shim::Routines<'d>,
    declared: &mut AVec<'_, Declared<'d>>,
    reads: &ReadList,
    isa: Isa,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, CodeRecordError> {
    make_entry_with_vtables(func, names, &mut Vec::new_in(alloc), routines, declared, reads, &[], isa, alloc)
}

/// [`make_entry_reading`] carrying `vtables`, the datum of each vtable the record names (form 6:
/// after the thread-locals, their count (4), then each: its symbol (32), its datum's length (4)
/// and bytes, ascending by symbol and each once). The run's parent sends each before the
/// record (blaze-cli run_parent.rs `Lazy::resolve`). `traps` is the build's trap table
/// (blaze-build `Built::traps`): the record's trap rows, what a fault's report names the
/// construct an `unreachable` ends by.
#[expect(clippy::too_many_arguments, reason = "the entry's parts, each its own input")]
pub fn make_entry_with_vtables<'a, 'd>(
    func: Verified<'_>,
    names: &InstanceNames<'_>,
    vtables: &mut AVec<'_, VtableDatum<'_>>,
    routines: crate::front2::builtins_shim::Routines<'d>,
    declared: &mut AVec<'_, Declared<'d>>,
    reads: &ReadList,
    traps: &[TrapSource],
    isa: Isa,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, CodeRecordError> {
    let (record, insts, called) =
        emit_sota_record_measured(func, names, routines.places(), isa, traps, alloc).map_err(|why| routines.refusal(why))?;
    // Which soft-float routines the record calls is the emitter's answer (only legalize maps an
    // `f128` operation to its routine): exactly those are declared, as any instance it reaches.
    for r in called.iter() {
        declared.push(routines.declared(r)?);
    }
    // The record form carries landing pads and the unwind word (blaze-emit pass/unwind.rs).
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
    // A foreign static has no entry to send (its place is its C symbol): not declared.
    let mut own: AVec<'_, ItemKey> = Vec::with_capacity_in(names.statics.len(), alloc);
    own.extend(names.statics.iter().copied().filter(|s| !names.c_statics.iter().any(|(k, _)| k == s)));
    put_statics(&own, &mut out, alloc).map_err(|what| CodeRecordError::Seal(RecordError::TooLarge { what }))?;
    // Form 5: the thread-locals, in the same keys form (each one's template is sent before the
    // record, as a static is).
    put_statics(names.tls, &mut out, alloc).map_err(|what| CodeRecordError::Seal(RecordError::TooLarge { what }))?;
    // Form 6: the vtables' data, ascending by symbol, each once (one vtable named at two sites
    // is one datum: its symbol is its whole identity).
    vtables.sort_unstable_by(|a, b| a.symbol.0.cmp(&b.symbol.0));
    vtables.dedup_by(|a, b| a.symbol == b.symbol);
    out.extend_from_slice(&len(vtables.len(), "entry's vtables")?.to_le_bytes());
    for v in vtables.iter() {
        out.extend_from_slice(&v.symbol.0);
        out.extend_from_slice(&len(v.datum.len(), "vtable's datum")?.to_le_bytes());
        out.extend_from_slice(v.datum);
    }
    out.extend_from_slice(&len(reads.len(), "entry's read list")?.to_le_bytes());
    for (question, answer) in reads.rows() {
        out.extend_from_slice(&len(question.len(), "read-list question")?.to_le_bytes());
        out.extend_from_slice(question);
        out.extend_from_slice(&answer.0);
    }
    Ok(out)
}

/// The declared statics block onto `out`: their count (4), then each item key (16), ascending
/// and each once (a record may name one static at two places). `Err` names what is too large.
pub(crate) fn put_statics(statics: &[ItemKey], out: &mut AVec<'_, u8>, alloc: Alloc<'_>) -> Result<(), &'static str> {
    let mut sorted: AVec<'_, ItemKey> = Vec::with_capacity_in(statics.len(), alloc);
    sorted.extend_from_slice(statics);
    sorted.sort_unstable();
    sorted.dedup();
    let count = u32::try_from(sorted.len()).map_err(|_| "entry's declared statics")?;
    out.extend_from_slice(&count.to_le_bytes());
    for item in sorted.iter() {
        out.extend_from_slice(&item.0.to_le_bytes());
    }
    Ok(())
}

const GETTER_DOMAIN: &str = "blaze-ladder thread-local getter body 2";

/// How a walk's [`Met`] names the thread-local getter.
pub const GETTER_NAME: &str = "the thread-local getter";

/// The thread-local getter's own key parts. One function a program, made by blaze-build's
/// `tls::index::getter` from no text: its body digest is that generator's form (the domain,
/// stepped when `tls::index::getter` changes) and the C functions it calls
/// (`tls::index::GETTER_IMPORTS`); it has no ground facts. Every record that touches a
/// thread-local calls it by [`tls_getter_symbol`], so it is a program part the record walk
/// makes once (run.rs `build_from`), not an instance a caller declares. Its twin for the key's
/// destructor is `crate::tls_index::ender_parts`.
pub fn getter_parts() -> KeyParts {
    let mut body = Hasher::new(GETTER_DOMAIN);
    for name in tls::index::GETTER_IMPORTS {
        body.part(name.as_bytes());
    }
    let mut ground = Hasher::new(GETTER_DOMAIN);
    ground.part(b"no ground facts");
    KeyParts { symbol: tls_getter_symbol(), body: body.finish(), ground: ground.finish() }
}

/// The thread-local getter's entry: its LIR emitted with its calls named as the C functions
/// they are, declaring no instance.
pub fn make_getter_entry<'a>(isa: Isa, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, CodeRecordError> {
    let func = tls::index::getter(Global).map_err(CodeRecordError::Getter)?;
    let funcs = tls::index::GETTER_IMPORTS.map(Callee::C);
    let names = InstanceNames { funcs: &funcs, statics: &[], c_statics: &[], vtables: &[], tls: &[] };
    let mut declared: AVec<'_, Declared<'_>> = Vec::new_in(alloc);
    make_entry(func.verified(), &names, crate::front2::builtins_shim::Routines::NONE, &mut declared, isa, alloc)
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
    /// Bytes after the last read-list row.
    Trailing,
    /// Read-list rows not sorted by question, or one question twice.
    ReadsOrder,
    /// The record inside does not read.
    Record(DecodeError),
    /// The instructions' length is past the record's code.
    Instructions(AddressWord),
    /// The declared statics are not ascending, or one comes twice.
    StaticsOrder,
    /// A static's entry holds a datum that does not read (blaze-code datum.rs).
    Datum(blaze_code::datum::DatumError),
    /// The entry's vtables are not ascending by symbol, or one comes twice.
    VtablesOrder,
    /// A vtable's datum in a code entry does not read (blaze-code datum.rs).
    VtableDatum(blaze_code::datum::DatumError),
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "a code record's entry (blaze-ladder code_records.rs `make_entry`)";
        match *self {
            EntryError::Magic => write!(f, "not {rule}: its first bytes are not the entry's magic"),
            EntryError::Form { found } => write!(f, "{rule} of form {found}, and this blaze reads form {ENTRY_FORM}: the store is from another blaze"),
            EntryError::Truncated { what } => write!(f, "{rule} ends inside its {what}"),
            EntryError::Trailing => write!(f, "{rule} has bytes after its last read-list row"),
            EntryError::ReadsOrder => write!(f, "{rule}'s read list is not sorted by question, each once"),
            EntryError::Record(why) => write!(f, "{rule}'s record does not read: {why}"),
            EntryError::Instructions(why) => write!(f, "{rule}: {why}"),
            EntryError::StaticsOrder => write!(f, "{rule}'s declared statics are not ascending by item key, each once"),
            EntryError::Datum(why) => write!(f, "a static's entry (code_records.rs `statics::make_static_entry`) holds a datum that does not read: {why}"),
            EntryError::VtablesOrder => write!(f, "{rule}'s vtables are not ascending by symbol, each once"),
            EntryError::VtableDatum(why) => write!(f, "{rule} holds a vtable's datum that does not read: {why}"),
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
    /// The declared statics' item keys, 16 bytes each (checked ascending by [`EntryView::read`]).
    statics: &'b [u8],
    /// The declared thread-locals' item keys, in the statics' form.
    thread_locals: &'b [u8],
    /// The vtables' rows' bytes (checked whole, in order, each datum read, by [`EntryView::read`]).
    vtables: &'b [u8],
    vtables_count: u32,
    /// The read list's rows' bytes (checked whole and in order by [`EntryView::read`]).
    reads: &'b [u8],
    reads_count: u32,
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

/// A 16-byte little-endian item key.
fn key_of(bytes: &[u8]) -> u128 {
    let mut wide = [0u8; 16];
    wide.copy_from_slice(bytes);
    u128::from_le_bytes(wide)
}

/// Each item key of a checked statics block, in order.
fn each_key(block: &[u8], mut each: impl FnMut(ItemKey)) {
    for key in block.chunks_exact(16) {
        each(ItemKey(key_of(key)));
    }
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
        let key = key_of(self.take(16, "declared instance's fn")?);
        let len = self.u32("declared instance's name length")? as usize;
        let name = self.take(len, "declared instance's name")?;
        Ok(Declared { tabled: ItemKey(key), name })
    }
    /// The declared instances' block: the count, then each row checked; (its bytes, count).
    fn declared_block(&mut self) -> Result<(&'b [u8], u32), EntryError> {
        let count = self.u32("count of declared instances")?;
        let start = self.at;
        for _ in 0..count {
            self.declared()?;
        }
        Ok((&self.bytes[start..self.at], count))
    }
    /// The declared statics' block ([`put_statics`]): the count, then the keys, checked
    /// ascending and each once; their bytes.
    fn statics_block(&mut self) -> Result<&'b [u8], EntryError> {
        let count = self.u32("count of declared statics")? as usize;
        let keys = self.take(count.checked_mul(16).ok_or(EntryError::Truncated { what: "declared statics" })?, "declared statics")?;
        let mut last: Option<u128> = None;
        for key in keys.chunks_exact(16) {
            let key = key_of(key);
            if last.is_some_and(|before| before >= key) {
                return Err(EntryError::StaticsOrder);
            }
            last = Some(key);
        }
        Ok(keys)
    }
    /// One vtable row: its symbol and its datum, read (`DatumView::read`).
    fn vtable(&mut self) -> Result<(Symbol, blaze_code::datum::DatumView<'b>), EntryError> {
        let mut symbol = [0u8; 32];
        symbol.copy_from_slice(self.take(32, "vtable's symbol")?);
        let len = self.u32("vtable's datum length")? as usize;
        let datum = blaze_code::datum::DatumView::read(self.take(len, "vtable's datum")?).map_err(EntryError::VtableDatum)?;
        Ok((Symbol(symbol), datum))
    }
    fn read_row(&mut self) -> Result<(&'b [u8], ContentId), EntryError> {
        let len = self.u32("read-list question's length")? as usize;
        let question = self.take(len, "read-list question")?;
        let mut answer = [0u8; 32];
        answer.copy_from_slice(self.take(32, "read-list answer")?);
        Ok((question, ContentId(answer)))
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
        let (declared, count) = c.declared_block()?;
        let statics = c.statics_block()?;
        let thread_locals = c.statics_block()?;
        let vtables_count = c.u32("count of vtables")?;
        let vtables_start = c.at;
        let mut last_vtable: Option<[u8; 32]> = None;
        for _ in 0..vtables_count {
            let (symbol, _) = c.vtable()?;
            if last_vtable.is_some_and(|before| before >= symbol.0) {
                return Err(EntryError::VtablesOrder);
            }
            last_vtable = Some(symbol.0);
        }
        let vtables = &bytes[vtables_start..c.at];
        let reads_count = c.u32("count of read-list rows")?;
        let reads_start = c.at;
        let mut last: Option<&[u8]> = None;
        for _ in 0..reads_count {
            let (question, _) = c.read_row()?;
            if last.is_some_and(|before| before >= question) {
                return Err(EntryError::ReadsOrder);
            }
            last = Some(question);
        }
        if c.at != bytes.len() {
            return Err(EntryError::Trailing);
        }
        Ok(EntryView { insts, record, declared, count, statics, thread_locals, vtables, vtables_count, reads: &bytes[reads_start..], reads_count })
    }

    /// Each vtable the entry carries: its symbol and its datum, ascending by symbol.
    pub fn each_vtable<E>(&self, mut each: impl FnMut(Symbol, blaze_code::datum::DatumView<'b>) -> Result<(), E>, malformed: impl Fn(EntryError) -> E) -> Result<(), E> {
        let mut c = Cursor { bytes: self.vtables, at: 0 };
        for _ in 0..self.vtables_count {
            let (symbol, datum) = c.vtable().map_err(&malformed)?;
            each(symbol, datum)?;
        }
        Ok(())
    }

    /// Each static the record names, by item key, ascending.
    pub fn each_static(&self, each: impl FnMut(ItemKey)) {
        each_key(self.statics, each);
    }

    /// Each thread-local the record names, by item key, ascending.
    pub fn each_thread_local(&self, each: impl FnMut(ItemKey)) {
        each_key(self.thread_locals, each);
    }

    /// The count of read-list rows.
    pub fn reads_count(&self) -> u32 {
        self.reads_count
    }

    /// Each read-list row (question, answer digest), in the entry's order.
    pub fn reads(&self) -> impl Iterator<Item = Result<(&'b [u8], ContentId), EntryError>> + 'b {
        let mut c = Cursor { bytes: self.reads, at: 0 };
        (0..self.reads_count).map(move |_| c.read_row())
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
///
/// Two forms. A PLAIN entry is put under its key once ([`CodeStore::put`]): the thread-local
/// getter and ender, made from no text, read nothing beyond their keys and are never stale. An
/// instance's entry has a read list and may be stale after an edit its key does not cover: it
/// is put under its CONTENT key ([`content_key`]), and its code key is a POINTER naming that
/// content key ([`CodeStore::point`]), replaced when the entry is built again. Two different
/// values under one plain key stay [`StoreError::TwoValues`], the alarm that a key missed an
/// input (rule 21).
///
/// Every write says its [`CodeClass`]: which root keeps it after the command (blaze-cli
/// store/roots.rs), so std's compiled code outlives any one program's root.
pub trait CodeStore {
    /// The entry under `key`, if any.
    fn get(&self, key: CodeKey) -> Result<Option<&[u8]>, StoreError>;
    /// Writes `bytes` under `key` once, as a record of `class`; equal bytes again are no write;
    /// different bytes are [`StoreError::TwoValues`].
    fn put(&mut self, class: CodeClass, key: CodeKey, bytes: &[u8]) -> Result<(), StoreError>;
    /// The content key the pointer `key` names, if any.
    fn pointed(&self, key: CodeKey) -> Result<Option<CodeKey>, StoreError>;
    /// The pointer `key` names `content` from now on, whatever it named before; both of `class`.
    fn point(&mut self, class: CodeClass, key: CodeKey, content: CodeKey) -> Result<(), StoreError>;
}

/// The lifetime class of a code record: what keeps it after the command that made it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeClass {
    /// Made from the toolchain alone (every item of its instance name is of a toolchain crate,
    /// or it is made from no text at all: the thread-local getter and ender): kept by the
    /// toolchain's root, whichever program reached it.
    Toolchain,
    /// Anything else: kept by the program root of the run that reached it.
    Program,
}

const CONTENT_DOMAIN: &str = "blaze-ladder code entry content 1";

/// The key an instance's entry is put under: the digest of its bytes, so two entries never
/// share one.
pub fn content_key(entry: &[u8]) -> CodeKey {
    CodeKey(ContentId::of(CONTENT_DOMAIN, entry))
}

/// The entry `key` names: through its pointer when it is one, else the plain entry under it.
/// A pointer naming no entry is the store's failure, named.
pub fn entry_of(store: &dyn CodeStore, key: CodeKey) -> Result<Option<&[u8]>, StoreError> {
    match store.pointed(key)? {
        Some(content) => match store.get(content)? {
            Some(bytes) => Ok(Some(bytes)),
            None => Err(StoreError::Failed { key, what: "a code key's pointer names a content key with no entry" }),
        },
        None => store.get(key),
    }
}

/// The in-memory [`CodeStore`]: entries and pointers sorted by key in the caller's allocator.
pub struct MemoryStore<'a> {
    rows: AVec<'a, (CodeKey, AVec<'a, u8>)>,
    pointers: AVec<'a, (CodeKey, CodeKey)>,
    alloc: Alloc<'a>,
    /// Writes made (equal rewrites are not counted).
    pub puts: usize,
}

impl<'a> MemoryStore<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> MemoryStore<'a> {
        MemoryStore { rows: Vec::new_in(alloc), pointers: Vec::new_in(alloc), alloc, puts: 0 }
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

    /// The class says nothing to a store kept in memory: everything it holds goes at its drop.
    fn put(&mut self, _class: CodeClass, key: CodeKey, bytes: &[u8]) -> Result<(), StoreError> {
        match self.rows.binary_search_by(|row| row.0.cmp(&key)) {
            Ok(at) if self.rows[at].1[..] == *bytes => Ok(()),
            Ok(_) => Err(StoreError::TwoValues { key }),
            Err(at) => {
                let mut value = Vec::new_in(self.alloc);
                value.extend_from_slice(bytes);
                self.rows.insert_shift(at, (key, value));
                Ok(())
            }
        }
    }

    fn pointed(&self, key: CodeKey) -> Result<Option<CodeKey>, StoreError> {
        Ok(self.pointers.binary_search_by(|row| row.0.cmp(&key)).ok().map(|at| self.pointers[at].1))
    }

    fn point(&mut self, _class: CodeClass, key: CodeKey, content: CodeKey) -> Result<(), StoreError> {
        match self.pointers.binary_search_by(|row| row.0.cmp(&key)) {
            Ok(at) if self.pointers[at].1 == content => {}
            Ok(at) => {
                self.pointers[at].1 = content;
                self.puts += 1;
            }
            Err(at) => {
                self.pointers.insert_shift(at, (key, content));
                self.puts += 1;
            }
        }
        Ok(())
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

/// One instance a census walk hands over ([`LirSeen::seen`]): its LIR exactly as
/// [`make_entry`] would get it, the names of what it references, the soft-float routines bound
/// after its own references (as [`make_entry`]'s `routines`), and the declaration of every
/// target its code can name ([`instance_declares`]), so a receiver that emits the tier record
/// form (`blaze_tier::record::MAGIC`) declares its targets as the run path will (`blaze_tier::record::declare`
/// before the seal, D106). The declarations are the receiver's to keep.
pub struct SeenInstance<'s> {
    /// As the program names it.
    pub function: &'s str,
    pub symbol: Symbol,
    pub func: Verified<'s>,
    pub names: &'s InstanceNames<'s>,
    pub routines: blaze_tier::RoutinePlaces,
    pub declares: crate::declared_names::DeclaredNames<'static>,
}

/// Every target one instance's code can name, declared (D106, blaze-tier record/declared.rs):
/// the instances it reaches (`reached`, the rows [`make_entry`] gets as `declared`); its
/// statics, a foreign one excepted (a C name, declared by no row); its vtables (`vtables`, the
/// build's, each by its principal and `symbols::vtable_trees`, as `names.vtables` names them);
/// its thread-locals and the program's thread-local runtime; and every soft-float routine
/// `routines` binds. A row whose target the record does not name is never written
/// (`blaze_tier::record::declare` takes only its targets' rows), so declaring all of them is
/// exact, not a guess at what the emitter calls.
pub fn instance_declares(
    reached: &[Declared<'_>],
    names: &InstanceNames<'_>,
    vtables: &[blaze_build::vtable::VtableRef],
    ground: blaze_typeck::PTys<'_>,
    routines: &crate::front2::builtins_shim::Routines<'_>,
    alloc: Alloc<'static>,
) -> Result<crate::declared_names::DeclaredNames<'static>, CodeRecordError> {
    let large = |_| CodeRecordError::Seal(RecordError::TooLarge { what: "an instance's declared names" });
    let mut d = crate::declared_names::DeclaredNames::new_in(alloc);
    for row in reached {
        d.instance(row.tabled, row.name).map_err(large)?;
    }
    for r in blaze_lir::legalize::SoftFloat::ALL {
        if routines.places().place(r).is_some() {
            let row = routines.declared(r)?;
            d.instance(row.tabled, row.name).map_err(large)?;
        }
    }
    for &s in names.statics.iter().filter(|s| !names.c_statics.iter().any(|(k, _)| k == *s)) {
        d.static_item(s).map_err(large)?;
    }
    let mut trees: AVec<'_, u8> = Vec::new_in(alloc);
    for &table in vtables {
        trees.clear();
        crate::symbols::vtable_trees(table, ground, alloc, &mut trees).map_err(CodeRecordError::Symbol)?;
        d.vtable(table.principal, &trees, alloc).map_err(large)?;
    }
    for &t in names.tls {
        d.thread_local(t).map_err(large)?;
    }
    d.tls_runtime().map_err(large)?;
    d.finish();
    Ok(d)
}

/// What a census walk ([`RecordRun::census`]) hands each instance it builds to, in the walk's
/// order, IN PLACE of making its record: the instance's LIR exactly as [`make_entry`] would get
/// it, and the names of what it references. The census emits it itself (examples/fast_census.rs:
/// every tier over every reached body); the walk puts nothing in the store for it.
pub trait LirSeen {
    /// One instance, as [`SeenInstance`] says.
    fn seen(&mut self, instance: SeenInstance<'_>);
}

/// A record walk's state (run.rs `build_records`): the store, what the records are built for,
/// and what was met.
pub struct RecordRun<'s> {
    pub store: &'s mut dyn CodeStore,
    pub builder: Builder,
    /// Each instance met, in the walk's order.
    pub met: std::vec::Vec<Met>,
    /// A census walk's receiver ([`LirSeen`]): `None` on the run path, which makes every record.
    pub seen: Option<&'s mut dyn LirSeen>,
    /// What the instance being built read beyond its key ([`reads::Recorded`]), cleared before
    /// each build and sealed into its entry.
    pub reads: reads::ReadLog,
    /// Entries found and then built again because a read-list row answered otherwise.
    pub stale: usize,
}

impl<'s> RecordRun<'s> {
    /// A walk over `store` for `builder`, refused when the builder names a tier whose emitter
    /// does not make entries ([`Builder::emits`]).
    pub fn new(store: &'s mut dyn CodeStore, builder: Builder) -> Result<RecordRun<'s>, NotEmitting> {
        RecordRun::planted(store, builder, reads::ReadPlant::None)
    }

    /// [`RecordRun::new`] with `plant` in its recorder.
    pub fn planted(store: &'s mut dyn CodeStore, builder: Builder, plant: reads::ReadPlant) -> Result<RecordRun<'s>, NotEmitting> {
        Ok(RecordRun { store, builder: builder.emits()?, met: std::vec::Vec::new(), seen: None, reads: reads::ReadLog::new(plant), stale: 0 })
    }

    /// A census walk: as [`RecordRun::new`], but each instance built is handed to `seen` and no
    /// record is made for it (the thread-local getter and ender, made from no text, still are).
    pub fn census(store: &'s mut dyn CodeStore, builder: Builder, seen: &'s mut dyn LirSeen) -> Result<RecordRun<'s>, NotEmitting> {
        Ok(RecordRun { store, builder: builder.emits()?, met: std::vec::Vec::new(), seen: Some(seen), reads: reads::ReadLog::new(reads::ReadPlant::None), stale: 0 })
    }

    /// The special record `which` (run/special.rs: the shim, the getter, the ender): found in
    /// the store, or made and put, by the one find-or-make (`special::make_special`); met under
    /// its name. `alloc` is the caller's scratch for the entry until it is put.
    pub fn special(&mut self, which: crate::run::special::Special<'_>, alloc: Alloc<'_>) -> Result<(), CodeRecordError> {
        let made = crate::run::special::make_special(which, &self.builder, &mut *self.store, alloc)?;
        self.met.push(Met { symbol: made.symbol, key: made.key, parts: made.parts, function: Box::from(which.name()), found: made.found });
        Ok(())
    }

    /// The thread-local getter's entry ([`getter_parts`]): found in the store, or made and put;
    /// met as [`GETTER_NAME`]. `alloc` is the caller's scratch for the entry until it is put.
    pub fn getter(&mut self, alloc: Alloc<'_>) -> Result<(), CodeRecordError> {
        self.special(crate::run::special::Special::Getter, alloc)
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
