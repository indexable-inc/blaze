//! A frozen head (engine piece E2a): the heads one item makes, as an immutable record with its
//! own type tables, items named by key, made by a pure function of the item ([`freeze`]) and
//! read in place by any worker ([`FrozenHead::read`]) or into a body's own type table
//! ([`import`]).
//!
//! ## What a head is, and what of it is shared
//!
//! A head is what a body reads of another item without reading its body (heads.rs): a fn's
//! signature (`FnHead`: inputs, output, generics, bounds, `Self`), an ADT's row (`AdtRow`: its
//! variants' fields, repr, defaults), a constructor's signature (`CtorSig`), a trait's head and
//! members (`TraitHead`, `TraitExtra`), an impl's header (`ImplHead`), an alias, a trait alias,
//! an `impl Trait` return's bounds. Each is filed in the job's `Store`, every type in it a `PTy`:
//! an index into the store's ONE `PortableTys`, whose numbering is the order the job interned
//! terms in. That index is the only part of a head that depends on what the job made before;
//! keys are content-derived (keys.rs), names are hashes of their text (`Name::of`).
//!
//! ## The form
//!
//! Each head is head_record.rs's record (`BZHD`): its terms in a table of its own, numbered by
//! the head's own walk (first use), keys and names as written. This container lists the heads
//! one item's want makes ([`want_heads`]: the item's own first, then the heads made with it,
//! each group by key), so its bytes are those of the item alone, whatever a store made before:
//!
//! ```text
//! "BZFZ" | version u32 | want (tag u8, library key [32], member key [16])
//!        | count u32
//!        | count x (head: tag u8, key [32] | reader digest [32] | offset u32 | len u32)
//!        | the records, in order, back to back
//! ```
//!
//! The reader digest is head_record.rs `reader_digest` (what the head MEANS, its fn text slot
//! zeroed); [`FrozenHead::meaning`] combines them by the formula a `Signature`'s out uses
//! (head_digest.rs `digest_of_heads`), so a frozen head's meaning IS its cell's digest.
//!
//! ## The cell
//!
//! One frozen head per item: the want that makes the item's whole record ([`cell_of`]: an
//! ADT's definition, a trait's members, an impl's header, a fn's signature, an alias's head, an
//! impl fn's own signature). A read of a part of it (a trait's head alone, an ADT's generics)
//! is a read of the whole cell: coarser than the parts, never wrong.
//!
//! The store's side tables a later head or a body reads are in each record (head_record.rs form
//! 9): an ADT's fieldless-enum count, a trait's associated types' parameter counts, outlives
//! clauses and const parameters, an impl's members' frame (whose const parameters are the
//! impl's). Not in the form (left out by name, never guessed): the lifetime parts (`FnLife`,
//! `AdtLife`, `ContainerLife`). No body reads them from a store: the borrow check reads each
//! callee's lifetimes from its TEXT (blaze-flow-lower `read_signature`) and files what it read
//! as a `FactKind::BorrowHead` row (borrow_reads.rs), so an edit of a lifetime bound alone
//! moves that row though not the `Signature` out (head_tests.rs
//! `a_frozen_head_carries_the_store_s_side_tables` shows the form holds none). In the engine a
//! body's borrow check must therefore record the callee's text cell, or the lifetime part joins
//! this form.

use blaze_drive::cells::Cell;
use blaze_drive::key::{Kind, Name};
use blaze_engine::CellId;
use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_surface::RecordBytes;
use blaze_typeck::{ItemKey, PortableTys};

use crate::front2::by_origin;
use crate::front2::fill::FillError;
use crate::front2::head_record::{self, Head, HeadAt, HeadRecordError, HeadRef};
use crate::front2::heads::{HeadError, Store, Want};
use crate::front2::keys::{Keys, checker_of};
use crate::front2::project::head_digest::{digest_of_heads, want_heads};
use crate::front2::source::NamesSource;

const MAGIC: [u8; 4] = *b"BZFZ";
/// Raised on any change of the form; head_record.rs's own version is inside each record.
/// 2: the records are head_record.rs form 9 (the store's side tables).
pub const VERSION: u32 = 2;
const WANT_BYTES: usize = 1 + 32 + 16;
const AT_BYTES: usize = 1 + 32;
const HEADER: usize = 4 + 4 + WANT_BYTES + 4;
const ENTRY: usize = AT_BYTES + 32 + 4 + 4;

/// Why a frozen head was not made.
#[derive(Debug)]
pub enum FreezeError {
    /// `want` is no item's whole want: its cell is `cell` ([`cell_of`]).
    NotACell { want: Want, cell: Want },
    /// What item `want` names could not be told (the names library's row).
    Fill { want: Want, error: FillError },
    /// No want makes checker key `key` (an anonymous constant's, or no item's).
    NoWant { key: ItemKey, error: FillError },
    /// The head could not be made. A `HeadError::Item(_, Why::NeedConst(..))` is a head whose
    /// type holds a constant only the const evaluator answers (an array length): the freeze has
    /// no evaluator (check.rs `make` drives one), so it is refused here, by name.
    Head { want: Want, error: HeadError },
    /// Head `at` of `want` was not written as a record.
    Record { want: Want, at: HeadAt, error: HeadRecordError },
    /// A field past what the form holds.
    TooBig { want: Want, what: &'static str },
    /// The bytes a source handed back do not read as a frozen head.
    Read { want: Want, error: FrozenError },
}

impl core::fmt::Display for FreezeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FreezeError::NotACell { want, cell } => write!(f, "frozen head: {want:?} is no item's whole want; its item's is {cell:?}"),
            FreezeError::Fill { want, error } => write!(f, "frozen head of {want:?}: {error:?}"),
            FreezeError::NoWant { key, error } => write!(f, "head cell of key {key:?}: no want makes it: {error:?}"),
            FreezeError::Head { want, error } => write!(f, "frozen head of {want:?}: {error}"),
            FreezeError::Record { want, at, error } => write!(f, "frozen head of {want:?}, head {at:?}: {error}"),
            FreezeError::TooBig { want, what } => write!(f, "frozen head of {want:?}: {what} past what the form holds"),
            FreezeError::Read { want, error } => write!(f, "frozen head of {want:?}: {error}"),
        }
    }
}

/// Why bytes do not read as a frozen head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrozenError {
    /// Not `BZFZ`.
    NotThisRecord,
    /// Another version of the form: rebuilt, never migrated.
    Version { found: u32 },
    /// The bytes end inside `what`.
    Short { what: &'static str },
    /// A tag that is no value of `what`.
    Byte { what: &'static str, byte: u8 },
    /// Head `entry`'s record does not start where the one before it ended.
    Order { entry: u32 },
    /// Bytes after the last head's record.
    Trailing { at: usize },
}

impl core::fmt::Display for FrozenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrozenError::NotThisRecord => write!(f, "frozen head: not a frozen head (no `BZFZ`)"),
            FrozenError::Version { found } => write!(f, "frozen head: version {found}, this reader is {VERSION}: rebuild it"),
            FrozenError::Short { what } => write!(f, "frozen head: the bytes end inside {what}"),
            FrozenError::Byte { what, byte } => write!(f, "frozen head: {byte:#04x} is no value of {what}"),
            FrozenError::Order { entry } => write!(f, "frozen head: head {entry}'s record does not start where the one before it ended"),
            FrozenError::Trailing { at } => write!(f, "frozen head: bytes after the last head's record (at byte {at})"),
        }
    }
}

/// The engine's name of the head cell of the item whose checker key is `item` (big-endian, as
/// a body's fact names it): the `Signature` whose subject is the key padded to 32 bytes. The
/// one home of that form (tables/task.rs `cell_id` calls it).
pub fn signature_name(item: [u8; 16]) -> Name {
    let mut subject = [0u8; 32];
    subject[..16].copy_from_slice(&item);
    Name::new(Kind::Signature, ContentId(subject))
}

/// The engine's id of head cell `cell` (a [`cell_of`] want): its item's [`signature_name`]
/// (`f` for an impl fn's own signature, the item's checker key otherwise), one part meaning
/// the whole cell (part 0; splitting a cell into parts waits for a measurement that asks).
pub fn cell_id(cell: Want) -> CellId {
    let item = match cell {
        Want::MemberFn(_, f) => f,
        w => checker_of(w.key()),
    };
    CellId { of: signature_name(item.0.to_be_bytes()), cell: Cell([0; 16]) }
}

/// The head cell a body's fact about checker key `key` reads: the want that makes `key`
/// (by_origin.rs `key_want`: a constructor's is its ADT's, a trait member's its trait's, an
/// impl constant's its impl's), then its [`cell_of`]. A fact's key is not always its cell's
/// item: the cell's is what the read must name.
pub fn fact_cell<'n, N: NamesSource<'n>>(names: N, keys: &Keys, key: ItemKey) -> Result<Want, FreezeError> {
    let fill = |error| FreezeError::NoWant { key, error };
    let kind = by_origin::kind_of_key(names, keys, key).map_err(fill)?;
    cell_of(names, by_origin::key_want(names, keys, key, kind).map_err(fill)?)
}

/// The cell `want` is read by: the want that makes its item's whole record (by_origin.rs
/// `item_want`). A trait's head is its members' cell, an ADT's generics its definition's; an
/// impl's fn and an item declared in a body keep their own.
pub fn cell_of<'n, N: NamesSource<'n>>(names: N, want: Want) -> Result<Want, FreezeError> {
    match want {
        Want::Head(lib) | Want::Adt(lib) | Want::Members(lib) => {
            let fill = |error| FreezeError::Fill { want, error };
            by_origin::item_want(names, by_origin::kind_of_lib(names, lib).map_err(fill)?).map_err(fill)
        }
        Want::Impl(_) | Want::Fn(_) | Want::MemberFn(..) => Ok(want),
    }
}

/// `want`'s bytes in the form's header, and the order cells are sorted by.
pub fn want_bytes(want: Want) -> [u8; WANT_BYTES] {
    let (tag, lib, member) = match want {
        Want::Head(k) => (1, k, None),
        Want::Adt(k) => (2, k, None),
        Want::Impl(k) => (3, k, None),
        Want::Fn(k) => (4, k, None),
        Want::Members(k) => (5, k, None),
        Want::MemberFn(k, f) => (6, k, Some(f)),
    };
    let mut out = [0; WANT_BYTES];
    out[0] = tag;
    out[1..33].copy_from_slice(&lib.0.0);
    if let Some(f) = member {
        out[33..].copy_from_slice(&f.0.to_le_bytes());
    }
    out
}

fn want_of(b: &[u8; WANT_BYTES]) -> Result<Want, FrozenError> {
    let mut lib = [0; 32];
    lib.copy_from_slice(&b[1..33]);
    let lib = LibKey(ContentId(lib));
    let mut member = [0; 16];
    member.copy_from_slice(&b[33..]);
    Ok(match b[0] {
        1 => Want::Head(lib),
        2 => Want::Adt(lib),
        3 => Want::Impl(lib),
        4 => Want::Fn(lib),
        5 => Want::Members(lib),
        6 => Want::MemberFn(lib, ItemKey(u128::from_le_bytes(member))),
        byte => return Err(FrozenError::Byte { what: "the want's kind", byte }),
    })
}

fn at_bytes(at: HeadAt) -> [u8; AT_BYTES] {
    let mut out = [0; AT_BYTES];
    let (tag, key): (u8, KeyOf) = match at {
        HeadAt::Fn(k) => (1, KeyOf::Checker(k)),
        HeadAt::Adt(k) => (2, KeyOf::Lib(k)),
        HeadAt::Ctor(k) => (3, KeyOf::Checker(k)),
        HeadAt::Trait(k) => (4, KeyOf::Lib(k)),
        HeadAt::Impl(k) => (5, KeyOf::Checker(k)),
        HeadAt::Alias(k) => (6, KeyOf::Lib(k)),
        HeadAt::TraitAlias(k) => (7, KeyOf::Lib(k)),
        HeadAt::Opaque(k) => (8, KeyOf::Checker(k)),
    };
    out[0] = tag;
    match key {
        KeyOf::Lib(k) => out[1..].copy_from_slice(&k.0.0),
        KeyOf::Checker(k) => out[1..17].copy_from_slice(&k.0.to_le_bytes()),
    }
    out
}

/// A head's key as [`at_bytes`] writes it: a library key whole, a checker key in 16 bytes.
enum KeyOf {
    Lib(LibKey),
    Checker(ItemKey),
}

fn at_of(b: &[u8; AT_BYTES]) -> Result<HeadAt, FrozenError> {
    let mut lib = [0; 32];
    lib.copy_from_slice(&b[1..]);
    let lib = LibKey(ContentId(lib));
    let mut ck = [0; 16];
    ck.copy_from_slice(&b[1..17]);
    let ck = ItemKey(u128::from_le_bytes(ck));
    let checker_only = b[17..].iter().all(|x| *x == 0);
    let at = match b[0] {
        1 => HeadAt::Fn(ck),
        2 => HeadAt::Adt(lib),
        3 => HeadAt::Ctor(ck),
        4 => HeadAt::Trait(lib),
        5 => HeadAt::Impl(ck),
        6 => HeadAt::Alias(lib),
        7 => HeadAt::TraitAlias(lib),
        8 => HeadAt::Opaque(ck),
        byte => return Err(FrozenError::Byte { what: "a head's kind", byte }),
    };
    if matches!(at, HeadAt::Fn(_) | HeadAt::Ctor(_) | HeadAt::Impl(_) | HeadAt::Opaque(_)) && !checker_only {
        return Err(FrozenError::Byte { what: "a checker key's padding", byte: b[17..].iter().copied().find(|x| *x != 0).unwrap_or(0) });
    }
    Ok(at)
}

/// What [`export`] writes beside the form: nothing, or (a test's planted bug only) a job-local
/// id, the store's index of each fn's output, after its record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The bug the history test must catch: a `PTy` index of the job's own table exported.
    JobLocalId,
}

/// The heads `want` made in `store`, frozen ([`want_heads`]; see the module docs). `want` must
/// be made in `store`. The bytes are a function of the heads alone: two stores that made the
/// same item give equal bytes whatever else each made before.
pub fn export<'n, 'a, N: NamesSource<'n>>(names: N, keys: &Keys, store: &Store<'_>, want: Want, plant: Plant, alloc: Alloc<'a>) -> Result<RecordBytes<'a>, FreezeError> {
    let heads = want_heads(names, keys, store, want, alloc);
    let count = u32::try_from(heads.len()).map_err(|_| FreezeError::TooBig { want, what: "the head count" })?;
    let mut dir = RecordBytes::new_in(alloc);
    let mut payload = RecordBytes::new_in(alloc);
    for at in heads.iter().copied() {
        let rec = |error| FreezeError::Record { want, at, error };
        let head = head_record::from_store(store, at, None, None).map_err(rec)?;
        let bytes = head_record::encode(head, store.tys.view(), alloc).map_err(rec)?;
        let meaning = head_record::reader_digest(head, store.tys.view(), alloc).map_err(rec)?;
        let offset = u32::try_from(payload.len()).map_err(|_| FreezeError::TooBig { want, what: "the records" })?;
        payload.extend_from_slice(&bytes);
        if let (Plant::JobLocalId, HeadRef::Fn { head, .. }) = (plant, head) {
            payload.extend_from_slice(&head.output.0.to_le_bytes());
        }
        let len = u32::try_from(payload.len()).map_err(|_| FreezeError::TooBig { want, what: "the records" })? - offset;
        dir.extend_from_slice(&at_bytes(at));
        dir.extend_from_slice(&meaning.0);
        dir.extend_from_slice(&offset.to_le_bytes());
        dir.extend_from_slice(&len.to_le_bytes());
    }
    let mut out = RecordBytes::with_capacity_in(HEADER + dir.len() + payload.len(), alloc);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&want_bytes(want));
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&dir);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// One item frozen: its bytes and the cells its making read.
pub struct Frozen<'a> {
    pub bytes: RecordBytes<'a>,
    /// Every other cell the item's heads were made from (each [`cell_of`] a want the making
    /// needed, sorted by [`want_bytes`], the item's own left out): a superset of what they
    /// read, since a need's own needs are in it too. The digest seen for each is its cell's
    /// [`FrozenHead::meaning`] ([`reads_seen`]). What the names library answered (paths walked,
    /// a module's items) is not here: those reads are the names cells' (CELL's `CellSink`).
    pub reads: AVec<'a, Want>,
}

/// The item whose whole want is `want`, frozen: a pure function of the item and the names. It
/// is lowered in a scratch store of its own (`frame` and `scratch` hold that store's lowering
/// and table, dropped here), so nothing another item made is in it; `sized` is the program's
/// `Sized` (front2 index.rs `lang("sized")`), as every store is made with.
pub fn freeze<'n, 'a, N: NamesSource<'n>>(
    names: N,
    sized: Option<LibKey>,
    want: Want,
    frame: &'n blaze_mem::Frame,
    scratch: Alloc<'static>,
    alloc: Alloc<'a>,
) -> Result<Frozen<'a>, FreezeError> {
    let cell = cell_of(names, want)?;
    if cell != want {
        return Err(FreezeError::NotACell { want, cell });
    }
    let mut store = Store::new(sized, frame, scratch);
    let mut keys = Keys::new();
    let mut needed: AVec<'a, Want> = AVec::new_in(alloc);
    store.ensure_noting(names, &mut keys, want, &mut |w| needed.push(w)).map_err(|error| FreezeError::Head { want, error })?;
    let bytes = export(names, &keys, &store, want, Plant::None, alloc)?;
    let mut reads: AVec<'a, Want> = AVec::new_in(alloc);
    for w in needed.iter().copied() {
        let c = cell_of(names, w)?;
        if c != want {
            reads.push(c);
        }
    }
    reads.sort_unstable_by_key(|w| want_bytes(*w));
    reads.dedup();
    Ok(Frozen { bytes, reads })
}

/// A frozen head read in place: checked once ([`FrozenHead::read`]), then its heads borrowed.
#[derive(Clone, Copy)]
pub struct FrozenHead<'b> {
    bytes: &'b [u8],
    want: Want,
    count: u32,
}

/// One head of a frozen head, borrowed from it.
#[derive(Clone, Copy)]
pub struct FrozenEntry<'b> {
    pub at: HeadAt,
    /// Its reader digest (head_record.rs `reader_digest`).
    pub meaning: ContentId,
    /// Its head record (`BZHD`), read by head_record.rs `decode`.
    pub record: &'b [u8],
}

impl<'b> FrozenHead<'b> {
    /// `bytes` checked as a frozen head: its header, every entry's tag, and the records back
    /// to back to the end. Nothing is copied.
    pub fn read(bytes: &'b [u8]) -> Result<FrozenHead<'b>, FrozenError> {
        let header: &[u8; HEADER] = bytes.get(..HEADER).and_then(|h| h.try_into().ok()).ok_or(FrozenError::Short { what: "the header" })?;
        if header[..4] != MAGIC {
            return Err(FrozenError::NotThisRecord);
        }
        let found = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if found != VERSION {
            return Err(FrozenError::Version { found });
        }
        let want_b: &[u8; WANT_BYTES] = header[8..8 + WANT_BYTES].try_into().map_err(|_| FrozenError::Short { what: "the want" })?;
        let want = want_of(want_b)?;
        let c = &header[8 + WANT_BYTES..];
        let count = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        let dir_end = usize::try_from(count).ok().and_then(|n| n.checked_mul(ENTRY)).and_then(|n| n.checked_add(HEADER)).ok_or(FrozenError::Short { what: "the directory" })?;
        if dir_end > bytes.len() {
            return Err(FrozenError::Short { what: "the directory" });
        }
        let frozen = FrozenHead { bytes, want, count };
        let mut next = 0u32;
        for i in 0..count {
            let (at_b, _, offset, len) = frozen.entry_raw(i);
            at_of(&at_b)?;
            if offset != next {
                return Err(FrozenError::Order { entry: i });
            }
            next = offset.checked_add(len).ok_or(FrozenError::Short { what: "a head's record" })?;
        }
        let end = usize::try_from(next).ok().and_then(|n| n.checked_add(dir_end)).ok_or(FrozenError::Short { what: "the records" })?;
        if end > bytes.len() {
            return Err(FrozenError::Short { what: "the records" });
        }
        if end < bytes.len() {
            return Err(FrozenError::Trailing { at: end });
        }
        Ok(frozen)
    }

    /// Entry `i`'s fields; `i` below the count [`FrozenHead::read`] checked.
    fn entry_raw(&self, i: u32) -> ([u8; AT_BYTES], [u8; 32], u32, u32) {
        let at = HEADER + i as usize * ENTRY;
        let e = &self.bytes[at..at + ENTRY];
        let mut at_b = [0; AT_BYTES];
        at_b.copy_from_slice(&e[..AT_BYTES]);
        let mut meaning = [0; 32];
        meaning.copy_from_slice(&e[AT_BYTES..AT_BYTES + 32]);
        let o = &e[AT_BYTES + 32..];
        (at_b, meaning, u32::from_le_bytes([o[0], o[1], o[2], o[3]]), u32::from_le_bytes([o[4], o[5], o[6], o[7]]))
    }

    /// The want (the item's cell) it is the frozen head of.
    pub fn want(&self) -> Want {
        self.want
    }

    /// Its bytes, as written.
    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }

    /// Its heads in order.
    pub fn heads(&self) -> impl Iterator<Item = FrozenEntry<'b>> + '_ {
        let payload = HEADER + self.count as usize * ENTRY;
        (0..self.count).map(move |i| {
            let (at_b, meaning, offset, len) = self.entry_raw(i);
            // Checked by `read`: the tag and the range.
            let at = at_of(&at_b).unwrap_or(HeadAt::Fn(ItemKey(0)));
            let from = payload + offset as usize;
            FrozenEntry { at, meaning: ContentId(meaning), record: &self.bytes[from..from + len as usize] }
        })
    }

    /// What the item's heads mean: their reader digests by the `Signature` out's formula
    /// (head_digest.rs `digest_of_heads`), so equal to `want_digest` of the same item made in
    /// any store. Its cell's digest: what a reader of the cell records.
    pub fn meaning(&self) -> ContentId {
        digest_of_heads(self.count as usize, self.heads().map(|e| e.meaning))
    }
}

/// The heads of `frozen` read into `tys`, a body's own type table: each record's terms
/// interned there in one pass (head_record.rs `decode`, linear in the record), so equal
/// structure is an equal `PTy` of `tys`. A fn head is checked against its item's text, which
/// `text_of` hands (the body has it: it lowers bodies from it).
pub fn import<'n, 'a>(
    frozen: &FrozenHead<'_>,
    text_of: &mut dyn FnMut(HeadAt) -> Option<&'n str>,
    tys: &mut PortableTys<'_>,
    alloc: Alloc<'static>,
    out: Alloc<'a>,
) -> Result<AVec<'a, (HeadAt, Head<'n>)>, FreezeError> {
    let want = frozen.want();
    let mut heads: AVec<'a, (HeadAt, Head<'n>)> = AVec::new_in(out);
    for e in frozen.heads() {
        let text = match e.at {
            HeadAt::Fn(_) => text_of(e.at),
            _ => None,
        };
        let head = head_record::decode(e.record, text, tys, alloc).map_err(|error| FreezeError::Record { want, at: e.at, error })?;
        heads.push((e.at, head));
    }
    Ok(heads)
}

/// Where a body's view reads heads from (lane VW, front2/tables/view.rs): by cell, as a frozen
/// head. A strategy is a type parameter at this one seam (rule 78), each tested equal
/// (head_tests.rs `the_two_sources_answer_alike`):
/// - [`StoreHeads`]: today's per-job store, each cell made in it and exported;
/// - [`FrozenHeads`]: each cell frozen alone ([`freeze`]).
pub trait HeadSource {
    /// The frozen head of cell `want` ([`cell_of`]), made on first ask.
    fn frozen(&mut self, want: Want) -> Result<FrozenHead<'_>, FreezeError>;
}

/// The digest seen for each cell of `reads` ([`Frozen::reads`]), from `source`: a head task's
/// read list (blaze-engine `Done::reads`, each cell named there as a `CellId`).
pub fn reads_seen<'a, H: HeadSource>(reads: &[Want], source: &mut H, alloc: Alloc<'a>) -> Result<AVec<'a, (Want, ContentId)>, FreezeError> {
    let mut out: AVec<'a, (Want, ContentId)> = AVec::with_capacity_in(reads.len(), alloc);
    for w in reads.iter().copied() {
        out.push((w, source.frozen(w)?.meaning()));
    }
    Ok(out)
}

/// The kept frozen heads of one source, by cell, in the order first asked.
struct Kept<'a> {
    cells: AVec<'a, (Want, RecordBytes<'a>)>,
}

impl<'a> Kept<'a> {
    fn get_or_make(&mut self, want: Want, make: impl FnOnce() -> Result<RecordBytes<'a>, FreezeError>) -> Result<FrozenHead<'_>, FreezeError> {
        let at = match self.cells.iter().position(|(w, _)| *w == want) {
            Some(at) => at,
            None => {
                let bytes = make()?;
                self.cells.push((want, bytes));
                self.cells.len() - 1
            }
        };
        let (_, bytes) = &self.cells[at];
        FrozenHead::read(bytes).map_err(|error| FreezeError::Read { want, error })
    }
}

/// Heads from today's per-job [`Store`]: each cell ensured there and exported.
pub struct StoreHeads<'s, 'n, 'a, N> {
    names: N,
    keys: &'s mut Keys,
    store: &'s mut Store<'n>,
    kept: Kept<'a>,
    alloc: Alloc<'a>,
}

/// What a [`StoreHeads`] is made over.
pub struct StoreHeadsOpts<'s, 'n, 'a, N> {
    pub names: N,
    pub keys: &'s mut Keys,
    pub store: &'s mut Store<'n>,
    pub alloc: Alloc<'a>,
}

impl<'s, 'n, 'a, N: NamesSource<'n>> StoreHeads<'s, 'n, 'a, N> {
    pub fn new(o: StoreHeadsOpts<'s, 'n, 'a, N>) -> StoreHeads<'s, 'n, 'a, N> {
        StoreHeads { names: o.names, keys: o.keys, store: o.store, kept: Kept { cells: AVec::new_in(o.alloc) }, alloc: o.alloc }
    }
}

impl<'n, N: NamesSource<'n>> HeadSource for StoreHeads<'_, 'n, '_, N> {
    fn frozen(&mut self, want: Want) -> Result<FrozenHead<'_>, FreezeError> {
        let (names, keys, store, alloc) = (self.names, &mut *self.keys, &mut *self.store, self.alloc);
        self.kept.get_or_make(want, || {
            store.ensure(names, keys, want).map_err(|error| FreezeError::Head { want, error })?;
            export(names, keys, store, want, Plant::None, alloc)
        })
    }
}

/// Heads each frozen alone ([`freeze`]), kept by cell.
pub struct FrozenHeads<'n, 'a, N> {
    names: N,
    sized: Option<LibKey>,
    frame: &'n blaze_mem::Frame,
    scratch: Alloc<'static>,
    kept: Kept<'a>,
    alloc: Alloc<'a>,
}

/// What a [`FrozenHeads`] is made over ([`freeze`]'s inputs).
pub struct FrozenHeadsOpts<'n, 'a, N> {
    pub names: N,
    pub sized: Option<LibKey>,
    pub frame: &'n blaze_mem::Frame,
    pub scratch: Alloc<'static>,
    pub alloc: Alloc<'a>,
}

impl<'n, 'a, N: NamesSource<'n>> FrozenHeads<'n, 'a, N> {
    pub fn new(o: FrozenHeadsOpts<'n, 'a, N>) -> FrozenHeads<'n, 'a, N> {
        FrozenHeads { names: o.names, sized: o.sized, frame: o.frame, scratch: o.scratch, kept: Kept { cells: AVec::new_in(o.alloc) }, alloc: o.alloc }
    }
}

impl<'n, N: NamesSource<'n>> HeadSource for FrozenHeads<'n, '_, N> {
    fn frozen(&mut self, want: Want) -> Result<FrozenHead<'_>, FreezeError> {
        let (names, sized, frame, scratch, alloc) = (self.names, self.sized, self.frame, self.scratch, self.alloc);
        self.kept.get_or_make(want, || Ok(freeze(names, sized, want, frame, scratch, alloc)?.bytes))
    }
}
