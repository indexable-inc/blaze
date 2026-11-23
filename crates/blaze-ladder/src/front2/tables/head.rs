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
//! impl's). A head's regions are in the form (head_record.rs form 22, D148), with their own
//! digest in each entry beside the reader digest. The other lifetime parts are in it too
//! (head_record.rs form 10: `AdtLife`, `ContainerLife`, each made or refused by name, made at [`export`] by lifetimes.rs
//! `Store::make_lives`): the borrow check's `BorrowHead`/`BorrowAdt` facts read the head cell
//! (tables/task.rs `fact_read`), so an edit of a lifetime bound alone moves that cell's meaning
//! and the `Signature` out with it (head_tests.rs `a_lifetime_bound_alone_moves_the_head`).
//!
//! ## A cell that does not freeze (lane HF1)
//!
//! A [`SeededHeads`] keeps each failed cell's OWN failure ([`SeededHeads::failure`]): a cell that
//! failed by itself keeps its typed reason; a cell that needed one that did not freeze keeps that
//! DIRECT need, the first of its own demand order not frozen; each keeps the heads its last
//! attempt read, and its names reads are its own (its owner entered for every attempt). Every ask
//! of a failed cell, the first too, is answered [`FreezeError::Again`] naming that cell or its
//! need. So a failure's answer, reason and reads are a function of the cell, whatever order a job
//! or two workers ask in (rule 25; failed_once_tests.rs, with the old rule as its control).

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, ABSENT, Key, Kind, Name, Tool};
use blaze_engine::CellId;
use blaze_hash::{FixedState, Map, MapOpts};
use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_surface::{ItemKind, RecordBytes};
use blaze_typeck::{ItemKey, PortableTys};

use blaze_lower::scoped::Prepares;

use crate::front2::by_origin;
use crate::front2::check::{BuildOrder, Records, eval_len};
use crate::front2::consts::{ConstFail, Consts};
use crate::front2::fill::FillError;
use crate::front2::index::Index;
use crate::front2::receiver::Pointers;
use crate::front2::types::Why;
use crate::front2::head_record::{self, Head, HeadAt, HeadRecordError, HeadRef};
use crate::front2::heads::{HeadError, SeedFailed, Seeded, Store, Want};
use crate::front2::keys::{Keys, MemberKind, checker_of, member_key_of};
use crate::front2::project::head_digest::{digest_of_heads, want_heads};
use crate::front2::source::NamesSource;
use crate::front2::tables::owned_reads::ReadOwners;

const MAGIC: [u8; 4] = *b"BZFZ";
/// Raised on any change of the form; head_record.rs's own version is inside each record.
/// 2: the records are head_record.rs form 9 (the store's side tables).
/// 3: the records are form 10 (every lifetime part, made or refused).
/// 4: an enum's cell holds its tuple variants' ctor heads (head_digest.rs `want_heads`).
/// 5: the records are form 12 (a refused type alias is a record, `K_ALIAS_REFUSED`).
/// 6: the records are form 13 (a member refused alone is a record, `K_FN_REFUSED`), and a
/// trait's members cell holds them (head_digest.rs `want_heads`).
/// 7: the records are form 18 (opaque type terms name their defining heads).
/// 8: the records are form 19 (coherence marks, lifetime arity and field region positions).
/// 9: the records are form 20 (trait heads identify lifetime parameters for call selection).
/// 10: the records are form 22 (a head's regions section, D148), and each entry holds the
/// head's regions digest after its reader digest (head_record.rs `regions_digest`).
/// 11: each entry ends with its head's [`RecordedRegions`] byte: a head of a kind that holds
/// regions is recorded laid or refused by name (D148), never with neither, and every store
/// that files it files the same answer (before, a soft refusal, or regions its cell's freeze
/// never made, reached another store as no entry at all: `UnreadWhy::NoHead`).
/// 12: the [`RecordedRegions`] byte is followed by the refusal's detail
/// ([`head_record::Unlaid::detail`]: a `NotLaid` refusal's typed cause and the signature part
/// it was met in), so a reader in another store is told the class the making store was; the
/// detail moves the refused regions digest too.
pub const VERSION: u32 = 12;
/// How many bytes [`want_bytes`] writes: a cell's name in the form's header, and in a head
/// task's payload (blaze-cli engine_heads.rs `heads_of`).
pub const WANT_BYTES: usize = 1 + 32 + 16;
const AT_BYTES: usize = 1 + 32;
const HEADER: usize = 4 + 4 + WANT_BYTES + 4;
/// A directory entry: the head's place, its reader digest, its regions digest, its record's
/// offset and length, its [`RecordedRegions`] bytes.
const ENTRY: usize = AT_BYTES + 32 + 32 + 4 + 4 + RECORDED;
/// A [`RecordedRegions`]'s bytes: its kind byte, then a refusal's detail.
const RECORDED: usize = 1 + head_record::Unlaid::DETAIL;

/// What a frozen head's entry says of its regions (form 11, D148): its kind holds none, they
/// are laid (its record holds the section), or they are refused by kind. One byte; never
/// "absent" for a kind that holds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordedRegions {
    None,
    Laid,
    Refused(head_record::Unlaid),
}

impl RecordedRegions {
    /// What head `at` of `store`, borrowed as `head`, is recorded with: laid when its regions
    /// are filed; refused by the store's kind when it refused them softly; refused as
    /// [`head_record::Unlaid::NotMade`] when the freeze made the head and never its regions.
    pub fn of(store: &Store<'_>, at: HeadAt, head: HeadRef<'_, '_>) -> RecordedRegions {
        if !head.holds_regions() {
            return RecordedRegions::None;
        }
        if head.regions().is_some() {
            return RecordedRegions::Laid;
        }
        RecordedRegions::Refused(store.regions.unlaid_of(head_record::regions_key(at)).unwrap_or(head_record::Unlaid::NotMade))
    }

    /// Its bytes in an entry (form 12): the kind byte, then a refusal's detail (zeros else).
    fn bytes(self) -> [u8; RECORDED] {
        let mut b = [0u8; RECORDED];
        match self {
            RecordedRegions::None => b[0] = 0,
            RecordedRegions::Laid => b[0] = 1,
            RecordedRegions::Refused(u) => {
                b[0] = 2 + u.byte();
                b[1..].copy_from_slice(&u.detail());
            }
        }
        b
    }

    /// What bytes `b` record, or `None` for bytes no answer writes (canonical: no detail
    /// beside `None` or `Laid`).
    fn of_bytes(b: [u8; RECORDED]) -> Option<RecordedRegions> {
        let mut detail = [0u8; head_record::Unlaid::DETAIL];
        detail.copy_from_slice(&b[1..]);
        match b[0] {
            0 | 1 if detail != [0; head_record::Unlaid::DETAIL] => None,
            0 => Some(RecordedRegions::None),
            1 => Some(RecordedRegions::Laid),
            k => head_record::Unlaid::of_bytes(k - 2, detail).map(RecordedRegions::Refused),
        }
    }
}

#[cfg(test)]
#[path = "adt_facts_tests.rs"]
mod adt_facts_tests;

#[cfg(test)]
#[path = "fn_qualifier_tests.rs"]
mod fn_qualifier_tests;

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
    /// type holds a constant only the const evaluator answers (an array length), met by
    /// [`freeze`], which has no evaluator ([`freeze_seeded`] asks its source's: [`Const`]).
    ///
    /// [`Const`]: FreezeError::Const
    Head { want: Want, error: HeadError },
    /// The head of `want` needs constant `c`'s value (`Why::NeedConst`), which its source's
    /// evaluator did not give ([`ConstSource::eval_const`]).
    Const { want: Want, c: LibKey, error: ConstRefused },
    /// Head `at` of `want` was not written as a record.
    Record { want: Want, at: HeadAt, error: HeadRecordError },
    /// A field past what the form holds.
    TooBig { want: Want, what: &'static str },
    /// The bytes a source handed back do not read as a frozen head.
    Read { want: Want, error: FrozenError },
    /// The names library has no text of cell `want`'s item: the head task has no key from text.
    NoText { want: Want },
    /// Cell `need` is asked inside another cell's freeze before it is frozen: the driver
    /// ([`SeededHeads`]) freezes it first and begins the asking freeze again. Only a source
    /// that drives its freezes by an explicit stack hands it back, and only to that driver.
    Unfrozen { need: Want },
    /// One ask needed a chain of cells, each needing the next, deeper than [`MOST_MAKING`]:
    /// its outermost cell and its innermost four.
    TooDeep { depth: usize, outermost: Want, innermost: [Want; 4] },
    /// One ask took more than `most` freeze attempts ([`SeededHeads`]'s driver).
    Steps { want: Want, most: usize },
    /// Cell `want` did not freeze, and every ask of it, the first too, is answered this (lane HF1:
    /// the answer is a function of the cell, never of who asked first). `failed == want`: its own
    /// freeze failed, its typed reason kept by its source ([`SeededHeads::failure`],
    /// [`CellFailure::Own`]). Else it needed cell `failed`, its DIRECT need (the first that did not
    /// freeze in its own demand order), which did not freeze ([`CellFailure::Needs`]).
    Again { want: Want, failed: Want },
    /// Cell `want` is not the asker's to freeze, and its knower holds no bytes of it
    /// ([`Known::Refused`]): a member crate's head a body asked that its head task did not
    /// freeze for the body's unit.
    NotTheirs { want: Want, why: NotTheirs },
}

impl core::fmt::Display for FreezeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FreezeError::NotACell { want, cell } => write!(f, "frozen head: {want:?} is no item's whole want; its item's is {cell:?}"),
            FreezeError::Fill { want, error } => write!(f, "frozen head of {want:?}: {error:?}"),
            FreezeError::NoWant { key, error } => write!(f, "head cell of key {key:?}: no want makes it: {error:?}"),
            FreezeError::Head { want, error } => write!(f, "frozen head of {want:?}: {error}"),
            FreezeError::Const { want, c, error } => write!(f, "frozen head of {want:?}: the value of constant {c:?}: {error}"),
            FreezeError::Record { want, at, error } => write!(f, "frozen head of {want:?}, head {at:?}: {error}"),
            FreezeError::TooBig { want, what } => write!(f, "frozen head of {want:?}: {what} past what the form holds"),
            FreezeError::Read { want, error } => write!(f, "frozen head of {want:?}: {error}"),
            FreezeError::NoText { want } => write!(f, "head task of {want:?}: the names library has no text of its item"),
            FreezeError::Unfrozen { need } => write!(f, "frozen head: cell {need:?} was asked inside another cell's freeze before it was frozen (front2 tables/head.rs `SeededHeads`)"),
            FreezeError::TooDeep { depth, outermost, innermost } => {
                write!(f, "frozen head: a chain of {depth} cells, each needing the next, is deeper than {MOST_MAKING}: from {outermost:?} to {innermost:?}")
            }
            FreezeError::Steps { want, most } => write!(f, "frozen head of {want:?}: more than {most} freeze attempts"),
            FreezeError::Again { want, failed } if want == failed => write!(f, "frozen head of {want:?}: its freeze failed (its own reason is kept by its source: front2 tables/head.rs `SeededHeads::failure`)"),
            FreezeError::Again { want, failed } => write!(f, "frozen head of {want:?}: not made, as cell {failed:?} it needs did not freeze"),
            FreezeError::NotTheirs { why, .. } => write!(f, "{}", why.said()),
        }
    }
}

/// Why a freeze did not get a constant's value ([`ConstSource::eval_const`]).
#[derive(Debug)]
pub enum ConstRefused {
    /// The source has no constant evaluator ([`SeededHeadsOpts::consts`] `None`).
    NoEvaluator,
    /// The evaluator refused it (check.rs `eval_len`).
    Eval(ConstFail),
    /// It was evaluated, and the head asked for it again: its value was not filed.
    NotFiled,
    /// The head asked for more than [`MOST_FREEZE_CONSTS`] constants.
    TooMany { most: usize },
    /// The constant's own head (its type) needs constant `of`'s value first.
    TypeNeedsConst { of: LibKey },
}

impl core::fmt::Display for ConstRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConstRefused::NoEvaluator => write!(f, "the freeze has no constant evaluator"),
            ConstRefused::Eval(e) => write!(f, "{e}"),
            ConstRefused::NotFiled => write!(f, "evaluated, and asked for again: its value was not filed"),
            ConstRefused::TooMany { most } => write!(f, "the head asks for more than {most} constants"),
            ConstRefused::TypeNeedsConst { of } => write!(f, "its own type needs the value of constant {of:?} first"),
        }
    }
}

/// How many constants one freeze evaluates at most (rule 18).
pub const MOST_FREEZE_CONSTS: usize = 256;

/// What evaluates a constant a frozen head's type holds (an array length naming a constant,
/// an anonymous constant `[u8; 1 << 2]`): the body side's evaluator behind one seam.
pub trait ConstValues<'n> {
    /// Evaluates constant `c` (a `Why::NeedConst`'s key: a constant item's, or the need key of
    /// an anonymous constant asked in `store`) and files its bits in `store.maps.const_bits`,
    /// as check.rs `eval_len` does in a body's store.
    fn eval(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: LibKey) -> Result<(), ConstFail>;
}

/// The store path's own evaluator (check.rs `eval_len` over a `Records` of the freeze's store),
/// with a fresh `Consts` for each constant: what it checks and records is of that store's
/// table, never carried into another store.
pub struct StoreConsts<'r, 'n, S> {
    pub names: S,
    pub index: &'r Index<'n>,
    pub prepared: &'r Prepares<'n>,
}

impl<'n, S: NamesSource<'n>> ConstValues<'n> for StoreConsts<'_, 'n, S> {
    fn eval(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: LibKey) -> Result<(), ConstFail> {
        let mut consts = Consts::default();
        let mut buckets = None;
        let mut r = Records { names: self.names, keys, index: self.index, store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, self.prepared)) };
        eval_len(&mut r, c)
    }
}

/// The engine's id of constant `c`'s VALUE, read by a frozen head whose type holds it: a part
/// of `c`'s own `Signature` other than the whole head (part 0, [`cell_id`]), so an edit that
/// moves the value moves the read and one that keeps it (a comment) does not. Its digest is
/// [`const_value_digest`] of the bits.
pub fn const_value_cell(c: LibKey) -> CellId {
    CellId { of: signature_name(checker_of(c).0.to_be_bytes()), cell: Cell(CONST_VALUE_PART) }
}

/// The part of a constant's `Signature` its value is ([`const_value_cell`]).
const CONST_VALUE_PART: [u8; 16] = *b"constant value\0\0";

/// The digest a reader of [`const_value_cell`] records: of the value's bits alone.
pub fn const_value_digest(bits: u128) -> ContentId {
    ContentId::of("blaze frozen head: a constant's value", &bits.to_le_bytes())
}

/// Why bytes do not read as a frozen head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrozenError {
    /// Not `BZFZ`.
    NotThisRecord,
    /// Another version of the form: rebuilt, never migrated.
    Version { found: u32 },
    /// The record under the requested cell belongs to another item.
    WrongCell { found: Want },
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
            FrozenError::WrongCell { found } => write!(f, "frozen head: the supplied record belongs to {found:?}"),
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
pub fn export<'n, 'a, N: NamesSource<'n>>(names: N, keys: &Keys, store: &mut Store<'n>, want: Want, plant: Plant, alloc: Alloc<'a>) -> Result<RecordBytes<'a>, FreezeError> {
    let heads = want_heads(store, want, alloc);
    // Each head's lifetime part (head_record.rs form 10), made or refused, before it is read.
    store.make_lives(names, keys, &heads);
    let store: &Store<'n> = store;
    let count = u32::try_from(heads.len()).map_err(|_| FreezeError::TooBig { want, what: "the head count" })?;
    let mut dir = RecordBytes::new_in(alloc);
    let mut payload = RecordBytes::new_in(alloc);
    for at in heads.iter().copied() {
        let rec = |error| FreezeError::Record { want, at, error };
        let head = head_record::from_store(store, at).map_err(rec)?;
        let origin = |key| crate::front2::project::head_digest::opaque_origin(names, keys, store, key);
        let bytes = head_record::encode_with_origins(head, store.tys.view(), alloc, &origin).map_err(rec)?;
        let meaning = head_record::reader_digest_with_origins(head, store.tys.view(), alloc, &origin).map_err(rec)?;
        // Beside it, the digest a lifetime-only edit moves (form 22, D148): the borrow
        // verdict's key; type checks read `meaning`.
        let regions = head_record::regions_digest_with_origins(head, store.tys.view(), alloc, &origin).map_err(rec)?;
        // A refusal's kind and detail are part of what a region reader reads
        // (`UnreadWhy::Refused`): they move the regions digest too (forms 11, 12).
        let recorded = RecordedRegions::of(store, at, head);
        let regions = match recorded {
            RecordedRegions::Refused(_) => {
                let mut input = [0u8; 32 + RECORDED];
                input[..32].copy_from_slice(&regions.0);
                input[32..].copy_from_slice(&recorded.bytes());
                ContentId::of("blaze-ladder frozen head refused regions 2", &input)
            }
            RecordedRegions::None | RecordedRegions::Laid => regions,
        };
        let offset = u32::try_from(payload.len()).map_err(|_| FreezeError::TooBig { want, what: "the records" })?;
        payload.extend_from_slice(&bytes);
        if let (Plant::JobLocalId, HeadRef::Fn { head, .. }) = (plant, head) {
            payload.extend_from_slice(&head.output.0.to_le_bytes());
        }
        let len = u32::try_from(payload.len()).map_err(|_| FreezeError::TooBig { want, what: "the records" })? - offset;
        dir.extend_from_slice(&at_bytes(at));
        dir.extend_from_slice(&meaning.0);
        dir.extend_from_slice(&regions.0);
        dir.extend_from_slice(&offset.to_le_bytes());
        dir.extend_from_slice(&len.to_le_bytes());
        dir.extend_from_slice(&recorded.bytes());
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
    /// The constants' values its store held when made, by key (each a `Why::NeedConst` key):
    /// a superset of the values its heads hold (one the evaluator met on the way is in it
    /// too). Read by value ([`const_value_cell`]): the record holds the value, not the
    /// constant's text, so an edit that keeps the value keeps the record.
    pub consts: AVec<'a, (LibKey, u128)>,
    /// The wants its store made by lowering (heads.rs `Store::lowered`) outside a constant's
    /// evaluation: over a store seeded from frozen heads ([`freeze_seeded`]), only the item's
    /// own.
    pub lowered: u32,
    /// The wants the CONSTANT EVALUATOR made by lowering in its store ([`ConstSource::eval_const`]):
    /// its `Records` make the heads an evaluation needs by an unseeded `Store::ensure`, so a
    /// constant ITEM an anonymous constant names (`[u8; N]`: the anon const's body reads `N`)
    /// has its head lowered again here though `N`'s own cell froze it (owned_reads_tests.rs
    /// `one_source_lowers_each_head_and_evaluates_each_constant_once`, lane H7: the 31st of 30).
    /// Counted apart, so the freeze's floor is stated exactly; seeding the evaluator's store is
    /// front2 check.rs `Records`' piece (the report's NOT DONE).
    pub eval_lowered: u32,
}

/// The item whose whole want is `want`, frozen: a pure function of the item and the names. It
/// is lowered in a scratch store of its own (`frame` and `scratch` hold that store's lowering
/// and table, dropped here), so nothing another item made is in it; `sized` is the program's
/// `Sized` (front2 index.rs `lang("sized")`), as every store is made with; `pointers` the
/// program's receiver pointer types (front2 receiver.rs `Pointers::find`), so a receiver such as
/// `self: Pin<&mut Self>` is decided, never a gap (lane H9, after FL2's finding: with
/// `Pointers::NOT_LOOKED_UP` every struct receiver was a gap and `Future`'s and `Stream`'s
/// dyn-compatibility verdicts were `Gap`).
pub fn freeze<'n, 'a, N: NamesSource<'n>>(
    names: N,
    sized: Option<LibKey>,
    pointers: Pointers,
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
    store.pointers = pointers;
    let mut keys = Keys::new();
    let mut needed: AVec<'a, Want> = AVec::new_in(alloc);
    store.ensure_noting(names, &mut keys, want, &mut |w| needed.push(w)).map_err(|error| FreezeError::Head { want, error })?;
    finish(names, &keys, &mut store, want, &needed, alloc)
}

/// The item whose whole want is `want`, frozen as [`freeze`] freezes it, in a store SEEDED
/// from frozen heads: each head its making needs from another cell is read from `source`'s
/// frozen record of that cell ([`import`] into the store's table, filed by heads.rs
/// `Store::file_heads`), never lowered again; only the item's own wants are lowered (and a
/// cell `source` is making up its stack, [`HeadSource::open`]: two items needing each other's
/// heads). Its bytes equal [`freeze`]'s (head_tests.rs `a_seeded_freeze_lowers_each_item_once`).
///
/// A head whose type holds a constant (`Why::NeedConst`) has it evaluated by `source`
/// ([`ConstSource::eval_const`], filed in the store) and is made again, as check.rs `make`
/// does for a body's store; each constant once, at most [`MOST_FREEZE_CONSTS`].
pub fn freeze_seeded<'n, 'a, N: NamesSource<'n>, H: ConstSource<'n>>(
    names: N,
    sized: Option<LibKey>,
    pointers: Pointers,
    want: Want,
    source: &mut H,
    frame: &'n blaze_mem::Frame,
    scratch: Alloc<'static>,
    alloc: Alloc<'a>,
) -> Result<Frozen<'a>, FreezeError> {
    let mut needed: AVec<'a, Want> = AVec::new_in(alloc);
    freeze_noting(names, sized, pointers, want, source, frame, scratch, alloc, &mut needed)
}

/// [`freeze_seeded`], each need its making met noted in `needed` as it is met (before its seed),
/// whether the freeze ends made or failed: what a FAILED cell read of other heads, up to and
/// with the need whose seed failed ([`SeededHeads::failure`]).
#[allow(clippy::too_many_arguments)]
fn freeze_noting<'n, 'a, N: NamesSource<'n>, H: ConstSource<'n>>(
    names: N,
    sized: Option<LibKey>,
    pointers: Pointers,
    want: Want,
    source: &mut H,
    frame: &'n blaze_mem::Frame,
    scratch: Alloc<'static>,
    alloc: Alloc<'a>,
    needed: &mut AVec<'a, Want>,
) -> Result<Frozen<'a>, FreezeError> {
    let cell = cell_of(names, want)?;
    if cell != want {
        return Err(FreezeError::NotACell { want, cell });
    }
    // Boxed, made in a frame of its own: this frame stays on the stack under every head the
    // freeze makes (blaze-int notes t29 overflow-bt4.txt: 62,096 bytes, a debug frame giving
    // the store's value and its temporary a slot each).
    let mut store = store_boxed(sized, pointers, frame, scratch);
    let mut keys = Keys::new();
    let mut asked: AVec<'a, LibKey> = AVec::new_in(alloc);
    // What the constant evaluator lowered in this store ([`Frozen::eval_lowered`]).
    let mut by_eval = 0u32;
    for _ in 0..=MOST_FREEZE_CONSTS {
        let c = match ensure_in(names, want, want, source, &mut *store, &mut keys, needed, scratch) {
            Ok(()) => {
                let mut frozen = finish(names, &keys, &mut *store, want, &needed[..], alloc)?;
                frozen.lowered = frozen.lowered.saturating_sub(by_eval);
                frozen.eval_lowered = by_eval;
                return Ok(frozen);
            }
            Err(Need::Fail(e)) => return Err(e),
            Err(Need::Const(c)) => c,
        };
        if asked.contains(&c) {
            return Err(FreezeError::Const { want, c, error: ConstRefused::NotFiled });
        }
        if asked.len() == MOST_FREEZE_CONSTS {
            break;
        }
        asked.push(c);
        // A constant ITEM's value is evaluated from its head (its type) and its body: its head
        // is made first, a need of this cell like any other (seeded from its frozen record),
        // so its type is in the read list beside its value. An anonymous constant (no text in
        // the names library) is filed by the evaluator from the ask in this store.
        if names.seeded_text_of(c).is_some() && !store.made(Want::Fn(c)) {
            // Its key's origin, which the evaluator lowers its body by (keys.rs `library_in`;
            // `Front2::new` files every item's in a body's keys, a freeze's start empty).
            keys.item(c).map_err(|clash| FreezeError::Head { want, error: HeadError::Key(clash) })?;
            match ensure_in(names, want, Want::Fn(c), source, &mut *store, &mut keys, needed, scratch) {
                Ok(()) => {}
                Err(Need::Fail(e)) => return Err(e),
                Err(Need::Const(of)) => return Err(FreezeError::Const { want, c, error: ConstRefused::TypeNeedsConst { of } }),
            }
        }
        let before = store.lowered;
        let evaluated = source.eval_const(&mut *store, &mut keys, c);
        by_eval = by_eval.saturating_add(store.lowered.saturating_sub(before));
        match evaluated {
            Ok(()) => {}
            // In a trait's members it is the asking member's gap, as an anonymous constant's
            // refusal is (`Maps::anon_failed`; types.rs `const_value_bits`): the member is
            // refused by name with this reason (head_record.rs `FnRefused`), the others stand.
            Err(error) if matches!(want, Want::Members(_)) => {
                store.maps.anon_failed.insert(c, std::format!("{error}").into_boxed_str());
            }
            Err(error) => return Err(FreezeError::Const { want, c, error }),
        }
    }
    let c = asked.last().copied().unwrap_or(want.key());
    Err(FreezeError::Const { want, c, error: ConstRefused::TooMany { most: MOST_FREEZE_CONSTS } })
}

/// A freeze's store, made in this frame and boxed in `scratch` (its own allocator): the
/// caller's frame holds the box's two words, never the store ([`freeze_noting`]).
#[inline(never)]
fn store_boxed<'n>(sized: Option<LibKey>, pointers: Pointers, frame: &'n blaze_mem::Frame, scratch: Alloc<'static>) -> blaze_own::Box<Store<'n>, Alloc<'static>> {
    let mut store = blaze_own::Box::new_in(Store::new(sized, frame, scratch), scratch);
    store.pointers = pointers;
    store
}

const _: () = assert!(core::mem::size_of::<blaze_own::Box<Store<'static>, Alloc<'static>>>() == 2 * core::mem::size_of::<usize>());

/// Why [`ensure_in`] stopped: a constant's value to evaluate, or the freeze's failure.
enum Need {
    Const(LibKey),
    Fail(FreezeError),
}

/// `target` made in `store` (cell `own`'s freeze), each need seeded from `source`.
#[allow(clippy::too_many_arguments)]
fn ensure_in<'n, 'a, N: NamesSource<'n>, H: HeadSource>(
    names: N,
    own: Want,
    target: Want,
    source: &mut H,
    store: &mut Store<'n>,
    keys: &mut Keys,
    needed: &mut AVec<'a, Want>,
    scratch: Alloc<'static>,
) -> Result<(), Need> {
    let mut failed: Option<FreezeError> = None;
    let made = store.ensure_seeded(names, keys, target, &mut |w| needed.push(w), &mut |store, keys, need| {
        seed(names, Some(own), need, source, store, keys, scratch).map_err(|e| {
            failed = Some(e);
            SeedFailed
        })
    });
    match (made, failed) {
        (Ok(()), _) => Ok(()),
        (Err(HeadError::Seed { .. }), Some(e)) => Err(Need::Fail(e)),
        (Err(HeadError::Item(_, Why::NeedConst(c, _))), _) => Err(Need::Const(c)),
        (Err(error), _) => Err(Need::Fail(FreezeError::Head { want: own, error })),
    }
}

/// What [`freeze`] and [`freeze_seeded`] hand back once `want` is made in `store`.
fn finish<'n, 'a, N: NamesSource<'n>>(names: N, keys: &Keys, store: &mut Store<'n>, want: Want, needed: &[Want], alloc: Alloc<'a>) -> Result<Frozen<'a>, FreezeError> {
    let bytes = export(names, keys, store, want, Plant::None, alloc)?;
    let mut reads: AVec<'a, Want> = AVec::new_in(alloc);
    for w in needed.iter().copied() {
        let c = cell_of(names, w)?;
        if c != want {
            reads.push(c);
        }
    }
    reads.sort_unstable_by_key(|w| want_bytes(*w));
    reads.dedup();
    let mut consts: AVec<'a, (LibKey, u128)> = AVec::with_capacity_in(store.maps.const_bits.len(), alloc);
    consts.extend(store.maps.const_bits.iter().map(|(k, b)| (*k, *b)));
    consts.sort_unstable_by_key(|(k, _)| *k);
    Ok(Frozen { bytes, reads, consts, lowered: store.lowered, eval_lowered: 0 })
}

/// The seed of a store making cell `own` (`None`: a body's store, which makes no cell): need
/// `need`'s cell filed from `source`'s frozen record. A freezer lowering its own cell can
/// also lower a need that re-enters the freeze stack. A body requires the completed record.
/// The seed a body's store passes to heads.rs `Store::ensure_seeded`,
/// as [`freeze_seeded`] does (stash the error, hand back `SeedFailed`).
pub fn seed<'n, N: NamesSource<'n>, H: HeadSource>(names: N, own: Option<Want>, need: Want, source: &mut H, store: &Store<'n>, keys: &Keys, scratch: Alloc<'static>) -> Result<Seeded, FreezeError> {
    let cell = cell_of(names, need)?;
    if Some(cell) == own {
        return Ok(Seeded::Lower);
    }
    if source.open(cell) {
        return match own {
            Some(_) => Ok(Seeded::Lower),
            None => Err(FreezeError::Unfrozen { need: cell }),
        };
    }
    file_frozen(names, cell, source.frozen(cell)?, store, keys, scratch)?;
    Ok(Seeded::Filed)
}

/// Import an already-frozen cell. A Bodies task has the record its Heads task published and
/// can only file it or report a typed error. The expected cell is checked before filing keys.
pub fn file_frozen<'n, N: NamesSource<'n>>(names: N, cell: Want, frozen: FrozenHead<'_>, store: &Store<'n>, keys: &Keys, scratch: Alloc<'static>) -> Result<(), FreezeError> {
    if frozen.want() != cell {
        return Err(FreezeError::Read { want: cell, error: FrozenError::WrongCell { found: frozen.want() } });
    }
    let members = members_of(names, cell, scratch);
    let mut text_of = |at: HeadAt| fn_text(names, cell, &members, at);
    // Where the store's type table stands before the import: the nodes past it are this
    // record's ([`file_named_items`]).
    let tys_before = store.tys.view().len_now();
    let heads = match crate::front2::project::head_digest::import_with_opaque_origins_boxed(names, keys, store, &frozen, &mut text_of, scratch, scratch) {
        Ok(heads) => heads,
        // A record of another form (an old home): its regions read as `RecordForm` while the
        // error goes up, reported, and the cell is made again from text. Never migrated.
        Err(e) => {
            if let FreezeError::Record { at, error, .. } = &e {
                if let Some(found) = head_record::form_of(error) {
                    store.regions.file_form(head_record::regions_key(*at), found);
                }
            }
            return Err(e);
        }
    };
    file_frozen_finish(names, cell, store, keys, &frozen, &members, heads, tys_before)
}

#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "outlined tail of file_frozen to keep stack frame small")]
fn file_frozen_finish<'n, N: NamesSource<'n>>(
    names: N,
    cell: Want,
    store: &Store<'n>,
    keys: &Keys,
    frozen: &FrozenHead<'_>,
    members: &[MemberText<'_>],
    heads: AVec<'static, (HeadAt, blaze_own::Box<Head<'n>, Alloc<'static>>)>,
    tys_before: usize,
) -> Result<(), FreezeError> {
    file_keys(cell, members, &heads, keys)?;
    file_named_items(names, cell, store, keys, tys_before)?;
    // Each head's recorded regions (form 11) against its record: laid exactly when the record
    // holds the section, and none only for a kind that holds none.
    for ((at, head), e) in heads.iter().zip(frozen.heads()) {
        let h = (**head).as_ref();
        let fits = match e.recorded {
            RecordedRegions::None => !h.holds_regions(),
            RecordedRegions::Laid => h.regions().is_some(),
            RecordedRegions::Refused(_) => h.holds_regions() && h.regions().is_none(),
        };
        if !fits {
            return Err(FreezeError::Record { want: cell, at: *at, error: HeadRecordError::Shape { what: "a head's recorded regions disagree with its record (D148, form 11)" } });
        }
    }
    store.file_heads(cell, heads.into_iter().map(|(at, h)| (at, blaze_own::Box::into_inner(h)))).map_err(|error| FreezeError::Head { want: cell, error })?;
    // The same step files each soft refusal into the store's regions: a reader is told its
    // kind by name, as in the store that made it, never `NoHead`.
    for e in frozen.heads() {
        if let RecordedRegions::Refused(kind) = e.recorded {
            store.regions.file_unlaid(head_record::regions_key(e.at), kind, kind.name());
        }
    }
    Ok(())
}

/// One member of a cell's container as the names library lists it (a macro call left out):
/// its key by the kind its head files it as, its kind, name and text.
struct MemberText<'n> {
    key: ItemKey,
    kind: MemberKind,
    name: &'n str,
    text: &'n str,
}

/// The members of `cell`'s item (a trait's or an impl's; none for another item).
fn members_of<'n, N: NamesSource<'n>>(names: N, cell: Want, alloc: Alloc<'static>) -> AVec<'static, MemberText<'n>> {
    let mut out: AVec<'static, MemberText<'n>> = AVec::new_in(alloc);
    if !matches!(cell, Want::Members(_) | Want::Impl(_) | Want::MemberFn(_, _)) {
        return out;
    }
    let container = checker_of(cell.key());
    names.each_member(cell.key(), &mut |m| {
        let kind = match m.kind {
            ItemKind::Function => MemberKind::Fn,
            ItemKind::Const => MemberKind::Const,
            ItemKind::TypeAlias => MemberKind::Type,
            _ => return,
        };
        let Some(name) = m.name else { return };
        out.push(MemberText { key: member_key_of(container, kind, name), kind, name, text: m.text });
    });
    out
}

/// The text fn head `at` of `cell`'s record was made from (head_record.rs checks it by its
/// digest): an item fn's, const's or static's own text; a member fn's or constant's as the
/// names library lists it. `None` (refused by the decoder as `NoText`) for a text the head
/// made itself: an ADT's field default's (heads.rs `Store::adt`, `const _: T = v;`).
fn fn_text<'n, N: NamesSource<'n>>(names: N, cell: Want, members: &[MemberText<'n>], at: HeadAt) -> Option<&'n str> {
    let HeadAt::Fn(k) = at else { return None };
    if let Want::Fn(lib) = cell {
        if checker_of(lib) == k {
            return names.seeded_text_of(lib).map(|t| t.text);
        }
    }
    members.iter().find(|m| m.key == k && m.kind != MemberKind::Type).map(|m| m.text)
}

/// A function pointer's code identity consumes the target's definition even when
/// evaluation never calls it. A seeded member borrows its text from the head, so
/// fetching the string alone would bypass the names view. `source_member` resolves
/// cfg alternatives by the text the seeded head actually carries.
pub(crate) fn member_definition<'n, N: NamesSource<'n>>(names: N, keys: &Keys, item: ItemKey, head: &crate::front2::heads::FnHead<'n>) -> &'n str {
    if matches!(keys.origin(item), Some(crate::front2::keys::Origin::Member { kind: MemberKind::Fn | MemberKind::Const, .. })) {
        match keys.source_member(names, item, head.text) {
            Some((_, member)) => names.read_member_body(member.key, member.written_by),
            None => names.gap("the source member of a function pointer's code identity", Some(head.lib)),
        }
    }
    head.text
}

/// The job's key origins a seeded cell's heads name, filed as its lowering files them
/// (heads.rs `keys.member`): a struct's or union's constructor; a trait's fns, associated
/// types and constants; an impl's fns and constants. A member the heads do not name (one its
/// crate's cfg leaves out) is not filed.
/// The library key of each ADT and fn an imported record's types name (the store's type nodes
/// past `from`, which the import added), filed in the body's `keys` as lowering the head would
/// have filed it when it resolved the path that names it. Found through the IMPORT's names
/// (the seed's), so the body's read list gets no `Names(Beginning)` row per item named: the
/// key's library key is a function of the head's record, whose fact the body reads already
/// (its bytes name the item by checker key; a second key of those 16 bytes is refused as
/// `KeysBeginAlike` wherever it is met). Without it a body over frozen heads asked
/// `Keys::library_in` of each such item through its own recorded view (buckets.rs
/// `coherence_wants` and `inherent_crates`, `facts.library`): 41 reads more than the lowering
/// store on a 40-link method chain (front2 tables/enter_once_tests.rs, 2026-11-21).
fn file_named_items<'n, N: NamesSource<'n>>(names: N, cell: Want, store: &Store<'n>, keys: &Keys, from: usize) -> Result<(), FreezeError> {
    let view = store.tys.view();
    for node in view.nodes().skip(from) {
        let k = match *node {
            blaze_typeck::PNode::Adt(k, _) | blaze_typeck::PNode::FnDef(k, _) => k,
            _ => continue,
        };
        if keys.origin(k).is_some() {
            continue;
        }
        // A key no item row begins with (a body-local item, a member) stays unfiled: its
        // origin is the body's, filed where it is lowered.
        let Some(lib) = names.key_beginning(k.0.to_be_bytes()) else { continue };
        keys.item(lib).map_err(|error| FreezeError::Head { want: cell, error: HeadError::Key(error) })?;
    }
    Ok(())
}

fn file_keys(cell: Want, members: &[MemberText<'_>], heads: &[(HeadAt, blaze_own::Box<Head<'_>, Alloc<'static>>)], keys: &Keys) -> Result<(), FreezeError> {
    let container = checker_of(cell.key());
    let clash = |error| FreezeError::Head { want: cell, error: HeadError::Key(error) };
    // The cell's own library key (e.g. an impl or trait container whose members are filed
    // below): without it, asking `fn_sig` of an imported member looks up the container's
    // library key through the body's recorded `NamesSource::key_beginning`.
    keys.item(cell.key()).map_err(clash)?;
    for (_, head) in heads.iter() {
        match &**head {
            Head::Adt { row, .. } if row.kind != blaze_typeck::AdtKind::Enum => {
                keys.member(container, MemberKind::Ctor, "").map_err(clash)?;
            }
            Head::Trait { head, extra, .. } => {
                for m in members.iter() {
                    let named = match m.kind {
                        MemberKind::Fn => head.fns.iter().any(|(_, k)| *k == m.key),
                        MemberKind::Type => extra.as_ref().is_some_and(|x| x.assoc.iter().any(|a| a.item == m.key)),
                        MemberKind::Const => extra.as_ref().is_some_and(|x| x.consts.iter().any(|(_, k)| *k == m.key)),
                        MemberKind::Ctor | MemberKind::Anon => false,
                    };
                    if named {
                        keys.member(container, m.kind, m.name).map_err(clash)?;
                    }
                }
            }
            Head::Impl { head, .. } => {
                for m in members.iter() {
                    let named = match m.kind {
                        MemberKind::Fn => head.fns.iter().any(|(_, k)| *k == m.key),
                        MemberKind::Const => head.consts.iter().any(|(_, k)| *k == m.key),
                        MemberKind::Type | MemberKind::Ctor | MemberKind::Anon => false,
                    };
                    if named {
                        keys.member(container, m.kind, m.name).map_err(clash)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
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
    /// Its regions digest (head_record.rs `regions_digest`): moved by a lifetime-only edit too.
    pub regions: ContentId,
    /// Its head record (`BZHD`), read by head_record.rs `decode`.
    pub record: &'b [u8],
    /// What it is recorded with of its regions (form 11).
    pub recorded: RecordedRegions,
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
            let (at_b, _, _, offset, len) = frozen.entry_raw(i);
            at_of(&at_b)?;
            let state = frozen.state_raw(i);
            RecordedRegions::of_bytes(state).ok_or(FrozenError::Byte { what: "a head's recorded regions (its kind byte, or the detail after it)", byte: state[0] })?;
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
    fn entry_raw(&self, i: u32) -> ([u8; AT_BYTES], [u8; 32], [u8; 32], u32, u32) {
        let at = HEADER + i as usize * ENTRY;
        let e = &self.bytes[at..at + ENTRY];
        let mut at_b = [0; AT_BYTES];
        at_b.copy_from_slice(&e[..AT_BYTES]);
        let mut meaning = [0; 32];
        meaning.copy_from_slice(&e[AT_BYTES..AT_BYTES + 32]);
        let mut regions = [0; 32];
        regions.copy_from_slice(&e[AT_BYTES + 32..AT_BYTES + 64]);
        let o = &e[AT_BYTES + 64..];
        (at_b, meaning, regions, u32::from_le_bytes([o[0], o[1], o[2], o[3]]), u32::from_le_bytes([o[4], o[5], o[6], o[7]]))
    }

    /// Entry `i`'s [`RecordedRegions`] bytes (its last); `i` below the count.
    fn state_raw(&self, i: u32) -> [u8; RECORDED] {
        let at = HEADER + i as usize * ENTRY + ENTRY - RECORDED;
        let mut b = [0u8; RECORDED];
        b.copy_from_slice(&self.bytes[at..at + RECORDED]);
        b
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
            let (at_b, meaning, regions, offset, len) = self.entry_raw(i);
            // Checked by `read`: the tag and the range.
            let at = at_of(&at_b).unwrap_or(HeadAt::Fn(ItemKey(0)));
            let from = payload + offset as usize;
            // Checked by `read` too.
            let recorded = RecordedRegions::of_bytes(self.state_raw(i)).unwrap_or(RecordedRegions::None);
            FrozenEntry { at, meaning: ContentId(meaning), regions: ContentId(regions), record: &self.bytes[from..from + len as usize], recorded }
        })
    }

    /// What the item's heads say of their lifetimes: their regions digests by the same formula
    /// as [`FrozenHead::meaning`]. A lifetime-only edit moves it and not `meaning`; the borrow
    /// verdict of a reader keys on it.
    pub fn regions(&self) -> ContentId {
        digest_of_heads(self.count as usize, self.heads().map(|e| e.regions))
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
/// `text_of` hands (the body has it: it lowers bodies from it). Through a shared borrow of
/// `tys` (head_record.rs `decode`): a table over stable columns, read by a live view while
/// this imports into it.
pub fn import<'n, 'a>(
    frozen: &FrozenHead<'_>,
    text_of: &mut dyn FnMut(HeadAt) -> Option<&'n str>,
    tys: &PortableTys<'_>,
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
/// - [`FrozenHeads`]: each cell frozen alone ([`freeze`]);
/// - [`SeededHeads`]: each cell frozen in a store seeded from the others ([`freeze_seeded`]).
pub trait HeadSource {
    /// The frozen head of cell `want` ([`cell_of`]), made on first ask.
    fn frozen(&mut self, want: Want) -> Result<FrozenHead<'_>, FreezeError>;
    /// Whether cell `want` is being made by this source now (up the stack of a
    /// [`freeze_seeded`] that asked it): its record cannot be asked for, so a seeded store
    /// lowers what it needs of it (a source may count each such answer).
    fn open(&mut self, want: Want) -> bool;
}

/// A [`HeadSource`] a [`freeze_seeded`] asks for the value of a constant a head's type holds.
pub trait ConstSource<'n>: HeadSource {
    /// Constant `c` evaluated and its bits filed in `store` ([`ConstValues::eval`]).
    fn eval_const(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: LibKey) -> Result<(), ConstRefused>;
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

/// The domain of a head task's source: its item's whole text.
const HEAD_SOURCE: &str = "blaze head task source: item text";
/// The domain of a head task's source when its item has no text (the task fails `NoText`).
const HEAD_NO_TEXT: &str = "blaze head task source: no text, by cell";

/// One head task's result, in the shape of blaze-engine `Done` (engine.rs): a failed head is a
/// result too, carrying its typed reason, its `out` [`ABSENT`] and its `defines` and `reads`
/// empty (invariant 7: its row is the failure).
#[derive(Debug)]
pub struct HeadDone<'a> {
    /// The cell's [`signature_name`] ([`cell_id`]).
    pub name: Name,
    /// The CLI's planned key, or the standalone task's key from the item's text.
    pub key: Key,
    /// The frozen head's [`FrozenHead::meaning`]: what a reader of the cell records.
    pub out: ContentId,
    /// The frozen head's bytes (the `BZFZ` form), empty when failed.
    pub bytes: RecordBytes<'a>,
    /// The one cell it defines (part 0, [`cell_id`]) and its digest, `out`.
    pub defines: AVec<'a, (Cell, ContentId)>,
    /// Each other head cell its making read and the meaning seen there ([`reads_seen`]),
    /// sorted by cell id. The names library's answers are not here (module docs, `Frozen::reads`).
    pub reads: AVec<'a, (CellId, ContentId)>,
    pub failed: Option<FreezeError>,
}

/// The head task of cell `want` (a [`cell_of`] want): its key from its item's text and the
/// caller's per-kind `tool`, then its frozen head from `source` (made there on first ask, so a
/// cell is lowered once whatever order the tasks run in, its needs read from the source's kept
/// records). A pure function of the names, the cell and what `source` answers.
pub fn head_task<'n, 'a, N: NamesSource<'n>, H: HeadCells>(names: N, source: &mut H, want: Want, tool: Tool, alloc: Alloc<'a>) -> HeadDone<'a> {
    let id = cell_id(want);
    let text = names.seeded_text_of(want.key()).map(|t| t.text);
    let from = match text {
        Some(t) => key::digest(HEAD_SOURCE, t.as_bytes()),
        None => key::digest(HEAD_NO_TEXT, &want_bytes(want)),
    };
    let key = key::task_key(tool, id.of, &from);
    head_result(source, want, id, key, text.is_some(), alloc)
}

/// A planned CLI task already has the source key from its physical file's outline.
/// Checking for the row here lets the producer report a missing definition on either path.
pub fn head_task_with_key<'n, 'a, N: NamesSource<'n>, H: HeadCells>(names: N, source: &mut H, want: Want, key: Key, alloc: Alloc<'a>) -> HeadDone<'a> {
    head_result(source, want, cell_id(want), key, names.seeded_text_of(want.key()).is_some(), alloc)
}

fn head_result<'a, H: HeadCells>(source: &mut H, want: Want, id: CellId, key: Key, has_text: bool, alloc: Alloc<'a>) -> HeadDone<'a> {
    let made = if has_text {
        head_task_made(source, want, id, alloc)
    } else {
        Err(FreezeError::NoText { want })
    };
    match made {
        Ok((out, bytes, defines, reads)) => HeadDone { name: id.of, key, out, bytes, defines, reads, failed: None },
        Err(e) => HeadDone {
            name: id.of,
            key,
            out: ABSENT,
            bytes: RecordBytes::new_in(alloc),
            defines: AVec::new_in(alloc),
            reads: AVec::new_in(alloc),
            failed: Some(e),
        },
    }
}

/// What [`head_task`] made of `want`: its meaning, bytes, defines and sorted reads.
#[allow(clippy::type_complexity)]
fn head_task_made<'a, H: HeadCells>(
    source: &mut H,
    want: Want,
    id: CellId,
    alloc: Alloc<'a>,
) -> Result<(ContentId, RecordBytes<'a>, AVec<'a, (Cell, ContentId)>, AVec<'a, (CellId, ContentId)>), FreezeError> {
    let (frozen, read) = source.cell(want)?;
    let out = frozen.meaning();
    let mut bytes = RecordBytes::with_capacity_in(frozen.bytes().len(), alloc);
    bytes.extend_from_slice(frozen.bytes());
    let mut wants: AVec<'a, Want> = AVec::with_capacity_in(read.heads.len(), alloc);
    wants.extend_from_slice(read.heads);
    let mut values: AVec<'a, (CellId, ContentId)> = AVec::with_capacity_in(read.consts.len(), alloc);
    values.extend(read.consts.iter().map(|(c, bits)| (const_value_cell(*c), const_value_digest(*bits))));
    let seen = reads_seen(&wants, source, alloc)?;
    let mut reads: AVec<'a, (CellId, ContentId)> = AVec::with_capacity_in(seen.len() + values.len(), alloc);
    reads.extend(seen.iter().map(|(w, c)| (cell_id(*w), *c)));
    reads.extend_from_slice(&values);
    reads.sort_unstable();
    reads.dedup();
    let mut defines: AVec<'a, (Cell, ContentId)> = AVec::with_capacity_in(1, alloc);
    defines.push((id.cell, out));
    Ok((out, bytes, defines, reads))
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

    fn open(&mut self, _want: Want) -> bool {
        false
    }
}

/// Heads each frozen alone ([`freeze`]), kept by cell.
pub struct FrozenHeads<'n, 'a, N> {
    names: N,
    sized: Option<LibKey>,
    pointers: Pointers,
    frame: &'n blaze_mem::Frame,
    scratch: Alloc<'static>,
    kept: Kept<'a>,
    alloc: Alloc<'a>,
}

/// What a [`FrozenHeads`] is made over ([`freeze`]'s inputs).
pub struct FrozenHeadsOpts<'n, 'a, N> {
    pub names: N,
    pub sized: Option<LibKey>,
    /// The program's receiver pointer types ([`freeze`]).
    pub pointers: Pointers,
    pub frame: &'n blaze_mem::Frame,
    pub scratch: Alloc<'static>,
    pub alloc: Alloc<'a>,
}

impl<'n, 'a, N: NamesSource<'n>> FrozenHeads<'n, 'a, N> {
    pub fn new(o: FrozenHeadsOpts<'n, 'a, N>) -> FrozenHeads<'n, 'a, N> {
        FrozenHeads { names: o.names, sized: o.sized, pointers: o.pointers, frame: o.frame, scratch: o.scratch, kept: Kept { cells: AVec::new_in(o.alloc) }, alloc: o.alloc }
    }
}

impl<'n, N: NamesSource<'n>> HeadSource for FrozenHeads<'n, '_, N> {
    fn frozen(&mut self, want: Want) -> Result<FrozenHead<'_>, FreezeError> {
        let (names, sized, pointers, frame, scratch, alloc) = (self.names, self.sized, self.pointers, self.frame, self.scratch, self.alloc);
        self.kept.get_or_make(want, || Ok(freeze(names, sized, pointers, want, frame, scratch, alloc)?.bytes))
    }

    fn open(&mut self, _want: Want) -> bool {
        false
    }
}

/// Heads each frozen in a store seeded from the others ([`freeze_seeded`], this source the
/// seed's), kept by cell: freezing N items lowers each item once.
pub struct SeededHeads<'n, 'a, N> {
    names: N,
    sized: Option<LibKey>,
    pointers: Pointers,
    frame: &'n blaze_mem::Frame,
    scratch: Alloc<'static>,
    kept: Kept<'a>,
    /// Each kept cell's [`Frozen::reads`], at its index in `kept`.
    reads: AVec<'a, AVec<'a, Want>>,
    /// Each kept cell's [`Frozen::consts`], at its index in `kept`.
    values: AVec<'a, AVec<'a, (LibKey, u128)>>,
    /// The cells being frozen now, innermost last: the driver's explicit stack ([`MOST_MAKING`]
    /// deep at most), each cell frozen after the needs it met unfrozen.
    making: AVec<'a, Want>,
    /// The needs each cell on `making` noted in its last attempt (at its index there): what it
    /// read of other heads when it is given up for a need that fails.
    making_noted: AVec<'a, AVec<'a, Want>>,
    /// Each cell whose freeze failed, in the order they failed: why (its own reason, its direct
    /// need, a refusal), and the cells of the heads its last attempt read (sorted by their bytes,
    /// each once, its own left out). Asked again it is refused by that, never frozen twice.
    failed: AVec<'a, (Want, Unmade, AVec<'a, Want>)>,
    /// The own reason of each cell that failed by itself ([`Unmade::Own`] indexes it).
    own: AVec<'a, FreezeError>,
    /// The failure rule, or a test's planted bug ([`FailPlant`]).
    plant: FailPlant,
    /// Every freeze begun, made or given up for a need frozen first (`attempts - cells` were
    /// begun again).
    pub attempts: u32,
    /// What evaluates a constant a head's type holds ([`ConstSource::eval_const`]).
    consts: Option<&'a mut (dyn ConstValues<'n> + 'a)>,
    /// The wants all its freezes made by lowering (heads.rs `Store::lowered`), outside constant
    /// evaluation ([`Frozen::lowered`]).
    pub lowered: u32,
    /// The wants its constant evaluator made by lowering ([`Frozen::eval_lowered`]).
    pub eval_lowered: u32,
    /// Each kept cell's own [`Frozen::lowered`], at its index in `kept` (0 for one taken from
    /// `known`): which cell a count past its floor is (owned_reads_tests.rs prints it).
    lowered_each: AVec<'a, u32>,
    /// The needs met of a cell up the stack ([`HeadSource::open`]), each lowered where met.
    pub reentered: u32,
    /// The frozen heads known before this source freezes a cell itself (the request's, seeds.rs
    /// `RequestHeads`): a cell it knows is copied in, never frozen again.
    known: Option<&'a (dyn KnownHeads + 'a)>,
    /// For each kept cell (at its index in `kept`), whether it was copied from `known`.
    from_known: AVec<'a, bool>,
    /// The cells copied from `known`.
    pub taken: u32,
    /// Told each cell's freeze as it begins and ends ([`SeededHeadsOpts::owners`]).
    owners: Option<&'a (dyn ReadOwners + 'a)>,
    /// Each kept cell's index in `kept`, by its bytes ([`want_bytes`]): an ask is one probe, not
    /// a walk of every cell kept so far (a job of a large member keeps tens of thousands, and
    /// each ask walked them: quadratic in the job, lane H8).
    at: Map<[u8; WANT_BYTES], usize, FixedState, Alloc<'a>>,
    /// Each failed cell's index in `failed`, by its bytes: the lookup of `failed`, which keeps
    /// the order.
    failed_at: Map<[u8; WANT_BYTES], usize, FixedState, Alloc<'a>>,
    alloc: Alloc<'a>,
}

/// Why a [`SeededHeads`] cell was not made, kept for every ask of it (lane HF1: a function of
/// the cell alone, whatever order its source was asked in).
#[derive(Clone, Copy, Debug)]
enum Unmade {
    /// Its OWN freeze failed: the reason at this index of `own`. Asked, it is
    /// [`FreezeError::Again`] naming itself.
    Own(usize),
    /// It needed this cell, its DIRECT need (the need its last attempt met first that did not
    /// freeze), which did not freeze. Asked, it is [`FreezeError::Again`] naming that need.
    Needs(Want),
    /// A need of it was not the asker's to freeze for an engine's lack ([`NotTheirs::OtherUnits`],
    /// [`NotTheirs::NoCell`]): asked, it is that same typed refusal ([`FreezeError::NotTheirs`]),
    /// never `Again`, so every body that meets it is told the refusal's own words and class
    /// (blaze-cli engine_seeds.rs). A need its knower holds as FAILED ([`NotTheirs::Failed`]) is
    /// [`Unmade::Needs`], as one failed here is.
    Refused { want: Want, why: NotTheirs },
}

impl Unmade {
    /// The error an ask of cell `want` is handed: the first ask's and every later one's.
    fn again(self, want: Want) -> FreezeError {
        match self {
            Unmade::Own(_) => FreezeError::Again { want, failed: want },
            Unmade::Needs(failed) => FreezeError::Again { want, failed },
            Unmade::Refused { want, why } => FreezeError::NotTheirs { want, why },
        }
    }
}

/// Why a cell of a [`SeededHeads`] did not freeze, as its source keeps it ([`SeededHeads::failure`]):
/// a function of the cell, its names and the heads it read, never of the order its source was
/// asked in (lane HF1; blaze-cli engine_heads_once.rs publishes it in the request's table).
#[derive(Clone, Copy, Debug)]
pub enum CellFailure<'s> {
    /// Its own freeze failed, for this typed reason.
    Own(&'s FreezeError),
    /// It needed this cell, its direct need, which did not freeze.
    Needs(Want),
    /// A need of it, `want`, was refused as not its source's to freeze ([`Known::Refused`]).
    Refused { want: Want, why: NotTheirs },
}

/// The failure rule of a [`SeededHeads`], or a test's planted bug (failed_once_tests.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailPlant {
    /// Each cell keeps its own failure, every ask answered by it ([`Unmade`]).
    Rule,
    /// PLANTED BUG, for tests only: the rule before lane HF1. The ask on which a freeze fails is
    /// handed the error of the cell that failed first (the top of the stack), and every cell on
    /// the stack keeps that first failed cell as its need, none its own reason: so a cell's
    /// answer and its failure depend on whether it, or a cell needing it, was asked first.
    FirstFailedNamed,
}

/// Frozen heads a [`SeededHeads`] takes before it freezes a cell itself: the request's table
/// and what other workers froze (seeds.rs `RequestSeed`). The bytes handed back are the cell's
/// frozen head as [`freeze_seeded`] would make it: a freeze is a function of its cell and the
/// names, so a known head and one made here are equal bytes.
pub trait KnownHeads {
    /// Cell `cell`'s frozen bytes (the `BZFZ` form) when known; else whether the asker may
    /// freeze it itself, or must not (a head only its owner's task freezes).
    fn known(&self, cell: Want) -> Known<'_>;
}

/// What [`KnownHeads::known`] answers of one cell.
#[derive(Clone, Copy, Debug)]
pub enum Known<'k> {
    /// Its frozen bytes (the `BZFZ` form).
    Bytes(&'k [u8]),
    /// Not known: the asker freezes it (a dependency's head: one unit, frozen once a machine).
    Freeze,
    /// Not known, and not the asker's to freeze: refused, typed ([`FreezeError::NotTheirs`]).
    Refused(NotTheirs),
}

/// Why a cell is not the asker's to freeze (blaze-cli engine_seeds.rs `StretchHeads`: a body
/// never freezes a member crate's head; it is its head task's run for the body's unit, or the
/// body is refused). `said`: the head by path and the reason, in the knower's words (rule 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotTheirs {
    /// Its head task froze it under other units than the asker's only.
    OtherUnits { said: &'static str },
    /// Its head task defines no cell of it this request (not planned, no such cell, or no task
    /// covers it): which, in `said`. The engine's own inconsistency, never the project's.
    NoCell { said: &'static str },
    /// Its head task ran for the asker's unit and could not freeze it: `said` quotes that
    /// task's own reason (blaze-cli engine_head_notes.rs `Failed`). The project's head, not
    /// the engine: the asker is not checked, as a head it froze itself and could not make
    /// (blaze-cli engine_bodies.rs `unmade_head`), the head reported once, at its task. Also a
    /// cell the Heads stage's table holds as FAILED (blaze-cli engine_heads_once.rs `HeadsOnce`,
    /// lane HF1): some freeze of it this request failed, `said` its own reason's words, so no
    /// worker freezes it again; a cell that needs it fails naming it ([`CellFailure::Needs`]).
    Failed { said: &'static str },
}

impl NotTheirs {
    pub fn said(self) -> &'static str {
        match self {
            NotTheirs::OtherUnits { said } | NotTheirs::NoCell { said } | NotTheirs::Failed { said } => said,
        }
    }
}

/// What a [`SeededHeads`] is made over: [`freeze`]'s inputs and the constant evaluator.
pub struct SeededHeadsOpts<'n, 'a, N> {
    pub names: N,
    pub sized: Option<LibKey>,
    /// The program's receiver pointer types ([`freeze`]; front2 receiver.rs `Pointers::find`,
    /// found once a job by the caller, as project.rs `ProjectWorld::new` finds them once).
    pub pointers: Pointers,
    pub frame: &'n blaze_mem::Frame,
    pub scratch: Alloc<'static>,
    pub alloc: Alloc<'a>,
    /// `None`: a head whose type holds a constant is refused ([`ConstRefused::NoEvaluator`]).
    pub consts: Option<&'a mut (dyn ConstValues<'n> + 'a)>,
    /// The heads known before a freeze ([`KnownHeads`]); `None`: every cell is frozen here. A
    /// cell taken from it keeps no reads ([`HeadCells::cell`] answers it with none), so a head
    /// task, whose read list is its source's, takes none.
    pub known: Option<&'a (dyn KnownHeads + 'a)>,
    /// Who the names reads made now belong to (front2 tables/owned_reads.rs): each attempt of
    /// a cell's freeze is entered as that cell's and left after, so a recorder with owners keeps
    /// every cell's reads apart and one source serves many head tasks. `None`: no one is told
    /// (a source a task, whose recorder is the task's).
    pub owners: Option<&'a (dyn ReadOwners + 'a)>,
}

impl<'n, 'a, N: NamesSource<'n>> SeededHeads<'n, 'a, N> {
    pub fn new(o: SeededHeadsOpts<'n, 'a, N>) -> SeededHeads<'n, 'a, N> {
        let kept = Kept { cells: AVec::new_in(o.alloc) };
        SeededHeads {
            names: o.names,
            sized: o.sized,
            pointers: o.pointers,
            frame: o.frame,
            scratch: o.scratch,
            kept,
            reads: AVec::new_in(o.alloc),
            values: AVec::new_in(o.alloc),
            making: AVec::new_in(o.alloc),
            making_noted: AVec::new_in(o.alloc),
            failed: AVec::new_in(o.alloc),
            own: AVec::new_in(o.alloc),
            plant: FailPlant::Rule,
            attempts: 0,
            consts: o.consts,
            lowered: 0,
            eval_lowered: 0,
            lowered_each: AVec::new_in(o.alloc),
            reentered: 0,
            known: o.known,
            from_known: AVec::new_in(o.alloc),
            taken: 0,
            owners: o.owners,
            at: Map::new(MapOpts { hasher: FixedState, alloc: o.alloc, capacity: None }),
            failed_at: Map::new(MapOpts { hasher: FixedState, alloc: o.alloc, capacity: None }),
            alloc: o.alloc,
        }
    }

    /// Cell `want` kept at the end of `kept` (its bytes pushed there just now): indexed.
    fn index_last(&mut self, want: Want) {
        self.at.insert(want_bytes(want), self.kept.cells.len() - 1);
    }

    /// The cells frozen so far, in the order they were finished.
    pub fn cells(&self) -> impl Iterator<Item = Want> + '_ {
        self.kept.cells.iter().map(|(w, _)| *w)
    }

    /// Each kept cell with the wants its freeze lowered (outside constant evaluation), in the
    /// order finished; 0 for a cell taken from the known heads.
    pub fn lowered_each(&self) -> impl Iterator<Item = (Want, u32)> + '_ {
        self.kept.cells.iter().zip(self.lowered_each.iter()).map(|((w, _), n)| (*w, *n))
    }

    /// How many cells it keeps (frozen here or taken from its known heads).
    pub fn kept_len(&self) -> usize {
        self.kept.cells.len()
    }

    /// How many cells' freezes failed until now (a mark for [`SeededHeads::failed_since`]).
    pub fn failed_len(&self) -> usize {
        self.failed.len()
    }

    /// The cell the last failure recorded from mark `from` on is about: one that failed by
    /// itself (the top of its stack, `drive`, recorded last of its ask), else the need it names.
    /// `None`: no freeze failed since `from`. Every failure since a mark, each with its own
    /// reason, is [`SeededHeads::failures_from`].
    pub fn failed_since(&self, from: usize) -> Option<Want> {
        self.failed.get(from..)?.last().map(|(w, unmade, _)| match unmade {
            Unmade::Own(_) => *w,
            Unmade::Needs(need) => *need,
            Unmade::Refused { want, .. } => *want,
        })
    }

    /// Cell `failed`'s kept failure as the source answers it.
    fn failure_at(&self, at: usize) -> Option<(Want, CellFailure<'_>, &[Want])> {
        let (w, unmade, heads) = self.failed.get(at)?;
        let failure = match unmade {
            Unmade::Own(i) => CellFailure::Own(self.own.get(*i)?),
            Unmade::Needs(need) => CellFailure::Needs(*need),
            Unmade::Refused { want, why } => CellFailure::Refused { want: *want, why: *why },
        };
        Some((*w, failure, &heads[..]))
    }

    /// Why cell `want` did not freeze, and the cells of the heads its last attempt read (sorted
    /// by their bytes, each once, its own left out; with its direct need for a
    /// [`CellFailure::Needs`]); `None` when it froze or was never asked. A function of the cell
    /// alone (module docs of blaze-cli engine_heads_once.rs; failed_once_tests.rs): its own
    /// reads are its source's owners' (front2 tables/owned_reads.rs), entered as it.
    pub fn failure(&self, want: Want) -> Option<(CellFailure<'_>, &[Want])> {
        let at = *self.failed_at.get(&want_bytes(want))?;
        self.failure_at(at).map(|(_, failure, heads)| (failure, heads))
    }

    /// Each cell that failed from mark `from` on ([`SeededHeads::failed_len`]), in the order
    /// recorded, with its failure and the heads it read ([`SeededHeads::failure`]): what a caller
    /// publishes after an ask (blaze-cli engine_heads_once.rs `publish_since`; seeds.rs
    /// `RequestSeed::seed` hands each own reason to the request).
    pub fn failures_from(&self, from: usize) -> impl Iterator<Item = (Want, CellFailure<'_>, &[Want])> + '_ {
        (from..self.failed.len()).filter_map(move |at| self.failure_at(at))
    }

    /// From now on, failures follow `plant` (the failure test's planted bug, failed_once_tests.rs).
    #[cfg(test)]
    pub(crate) fn plant_failures(&mut self, plant: FailPlant) {
        self.plant = plant;
    }

    /// Each cell kept from index `from` on that this source froze itself (not one taken from
    /// its known heads), with its frozen bytes, in the order finished: what a caller publishes
    /// after an ask (seeds.rs `RequestSeed`).
    pub fn frozen_from(&self, from: usize) -> impl Iterator<Item = (Want, &[u8])> + '_ {
        self.kept.cells.iter().zip(self.from_known.iter()).skip(from).filter(|(_, known)| !**known).map(|((w, bytes), _)| (*w, &bytes[..]))
    }

    /// Cell `want`'s index in `kept`, frozen on first ask ([`freeze_seeded`], this source the
    /// seed's) and kept with its reads. A failed cell is refused again by name, never frozen
    /// twice. Asked inside a freeze (`seed` -> [`HeadSource::frozen`]) for a cell not yet
    /// frozen, it answers [`FreezeError::Unfrozen`] and the driver freezes that cell first: no
    /// freeze runs inside another, so the stack is `making` and not the thread's.
    fn made(&mut self, want: Want) -> Result<usize, FreezeError> {
        let key = want_bytes(want);
        if let Some(&at) = self.at.get(&key) {
            return Ok(at);
        }
        if let Some(&at) = self.failed_at.get(&key) {
            let unmade = self.failed[at].1;
            return Err(unmade.again(want));
        }
        // Known already (the request's table, another worker's freeze): copied in, so its
        // bytes are read like a cell frozen here, and never frozen again.
        // Not the asker's to freeze: refused by the knower's words, never frozen here.
        let bytes = match self.known.map(|known| known.known(want)) {
            Some(Known::Bytes(bytes)) => Some(bytes),
            Some(Known::Refused(why)) => return Err(FreezeError::NotTheirs { want, why }),
            Some(Known::Freeze) | None => None,
        };
        if let Some(bytes) = bytes {
            let mut copy = RecordBytes::with_capacity_in(bytes.len(), self.alloc);
            copy.extend_from_slice(bytes);
            self.kept.cells.push((want, copy));
            self.index_last(want);
            self.reads.push(AVec::new_in(self.alloc));
            self.values.push(AVec::new_in(self.alloc));
            self.lowered_each.push(0);
            self.from_known.push(true);
            self.taken += 1;
            return Ok(self.kept.cells.len() - 1);
        }
        if !self.making.is_empty() {
            return Err(FreezeError::Unfrozen { need: want });
        }
        self.making.push(want);
        self.making_noted.push(AVec::new_in(self.alloc));
        let driven = self.drive(want);
        self.making.clear();
        self.making_noted.clear();
        driven?;
        // The driver returns once `want`, the stack's bottom, is kept.
        self.at.get(&key).copied().ok_or(FreezeError::Unfrozen { need: want })
    }

    /// Freeze the cells on `making`, innermost first, until it is empty: a freeze that meets a
    /// need not yet frozen pushes it and is begun again once it is kept. Rule 18: each attempt
    /// keeps a cell or pushes one, the stack at most [`MOST_MAKING`] deep, the attempts at most
    /// [`MOST_ATTEMPTS`]. A failure fails every cell on the stack (each needs the one above it),
    /// each kept with ITS OWN failure ([`Unmade`]): the top its own reason, each below it the
    /// cell above it, its direct need. The asker is answered its own cell's kept failure, as every
    /// later ask is (lane HF1).
    fn drive(&mut self, want: Want) -> Result<(), FreezeError> {
        let (names, sized, pointers, frame, scratch, alloc, owners) = (self.names, self.sized, self.pointers, self.frame, self.scratch, self.alloc, self.owners);
        for _ in 0..MOST_ATTEMPTS {
            let Some(&top) = self.making.last() else { return Ok(()) };
            self.attempts += 1;
            let reentered = self.reentered;
            // This attempt's reads are `top`'s (front2 tables/owned_reads.rs): its lookups, the
            // ones that find and seed its needs, and its constants' evaluation.
            if let Some(owners) = owners {
                owners.enter(top);
            }
            let mut noted: AVec<'a, Want> = AVec::new_in(alloc);
            let attempt = freeze_noting(names, sized, pointers, top, self, frame, scratch, alloc, &mut noted);
            if let Some(owners) = owners {
                owners.leave();
            }
            // What this attempt met of other heads: `top`'s, should it be given up for good.
            if let Some(last) = self.making_noted.last_mut() {
                *last = noted;
            }
            match attempt {
                Ok(f) => {
                    self.making.pop();
                    self.making_noted.pop();
                    self.lowered += f.lowered;
                    self.eval_lowered += f.eval_lowered;
                    self.lowered_each.push(f.lowered);
                    self.kept.cells.push((top, f.bytes));
                    self.index_last(top);
                    self.reads.push(f.reads);
                    self.values.push(f.consts);
                    self.from_known.push(false);
                }
                Err(FreezeError::Unfrozen { need }) => {
                    // The attempt given up counts its re-entries when it is made again.
                    self.reentered = reentered;
                    if self.making.len() == MOST_MAKING {
                        let m = &self.making[..];
                        let innermost = [m[m.len() - 4], m[m.len() - 3], m[m.len() - 2], m[m.len() - 1]];
                        return Err(FreezeError::TooDeep { depth: m.len(), outermost: m[0], innermost });
                    }
                    self.making.push(need);
                    self.making_noted.push(AVec::new_in(alloc));
                }
                Err(e) => {
                    let making = core::mem::replace(&mut self.making, AVec::new_in(alloc));
                    let noted = core::mem::replace(&mut self.making_noted, AVec::new_in(alloc));
                    return Err(self.fail(top, e, &making, &noted));
                }
            }
        }
        Err(FreezeError::Steps { want, most: MOST_ATTEMPTS })
    }

    /// The freeze of `top`, the innermost cell of `making` (the stack, its bottom the ask's
    /// cell; `noted` each one's last attempt's needs), failed with `e`: every cell on the stack
    /// kept failed, and the error the ask is answered.
    ///
    /// Each keeps ITS OWN failure (lane HF1), so its answer, its failure and its reads are a
    /// function of the cell, never of the order its source was asked in:
    /// - `top` failed by itself (its reason kept, [`CellFailure::Own`]), or because the need its
    ///   attempt met had not frozen: a need failed BEFORE (its seed answered `Again`) or one its
    ///   knower holds as failed ([`NotTheirs::Failed`]): [`CellFailure::Needs`] that need. Either
    ///   way it is the first need of its own demand order that did not freeze: the needs before it
    ///   froze, which is a fact of their cells, not of who asked first.
    /// - each cell below it needs the cell above it, the need its last attempt was given up for:
    ///   its first need not frozen in its own demand order, which then failed.
    /// - a need refused for an engine's lack ([`NotTheirs::OtherUnits`], [`NotTheirs::NoCell`]):
    ///   every cell on the stack keeps that refusal, as before (blaze-cli engine_seeds.rs).
    ///
    /// The heads each cell read are its last attempt's noted needs, as cells, under its own owner
    /// ([`SeededHeadsOpts::owners`]): the lookups that find a need's cell are the cell's reads, as
    /// they are in the attempt itself (`seed`), so no read is offered with no owner. A need whose
    /// cell cannot be told is no head read: the failure that met it is that `cell_of`'s, and its
    /// names reads are the cell's own.
    fn fail(&mut self, top: Want, e: FreezeError, making: &[Want], noted: &[AVec<'a, Want>]) -> FreezeError {
        let (names, alloc, owners) = (self.names, self.alloc, self.owners);
        let heads_of = |cell: Want, at: usize| -> AVec<'a, Want> {
            let mut out: AVec<'a, Want> = AVec::new_in(alloc);
            let Some(needs) = noted.get(at) else { return out };
            if let Some(owners) = owners {
                owners.enter(cell);
            }
            for w in needs.iter().copied() {
                if let Ok(c) = cell_of(names, w)
                    && c != cell
                {
                    out.push(c);
                }
            }
            if let Some(owners) = owners {
                owners.leave();
            }
            out.sort_unstable_by_key(|w| want_bytes(*w));
            out.dedup();
            out
        };
        // PLANTED BUG (tests only): the rule before lane HF1, every cell naming the first failed.
        if self.plant == FailPlant::FirstFailedNamed {
            let first = match &e {
                FreezeError::Again { failed, .. } => *failed,
                FreezeError::NotTheirs { want, .. } => *want,
                _ => top,
            };
            for (at, w) in making.iter().copied().enumerate() {
                let heads = heads_of(w, at);
                self.failed_at.insert(want_bytes(w), self.failed.len());
                self.failed.push((w, Unmade::Needs(first), heads));
            }
            return e;
        }
        let top_unmade = match &e {
            FreezeError::NotTheirs { want: need, why: NotTheirs::Failed { .. } } => Unmade::Needs(*need),
            FreezeError::NotTheirs { want, why } => Unmade::Refused { want: *want, why: *why },
            FreezeError::Again { want: need, .. } => Unmade::Needs(*need),
            _ => Unmade::Own(self.own.len()),
        };
        let mut asked: Option<Unmade> = None;
        for (at, w) in making.iter().copied().enumerate() {
            let unmade = match (making.get(at + 1), top_unmade) {
                (None, _) => top_unmade,
                (Some(_), Unmade::Refused { .. }) => top_unmade,
                (Some(above), Unmade::Own(_) | Unmade::Needs(_)) => Unmade::Needs(*above),
            };
            asked.get_or_insert(unmade);
            let heads = heads_of(w, at);
            self.failed_at.insert(want_bytes(w), self.failed.len());
            self.failed.push((w, unmade, heads));
        }
        let answer = match asked {
            Some(unmade) => unmade.again(making[0]),
            // No cell on the stack (the driver always has one): `top` alone.
            None => top_unmade.again(top),
        };
        if matches!(top_unmade, Unmade::Own(_)) {
            self.own.push(e);
        }
        answer
    }
}

/// The deepest chain of cells one ask of a [`SeededHeads`] freezes, each before the cell that
/// needs it.
pub const MOST_MAKING: usize = 1 << 12;
/// The most freeze attempts one ask of a [`SeededHeads`] makes: each keeps a cell (at most
/// seeds.rs `MOST_CELLS`) or pushes one onto the stack, so twice that bounds it.
pub const MOST_ATTEMPTS: usize = 2 << 20;

impl<'n, N: NamesSource<'n>> ConstSource<'n> for SeededHeads<'n, '_, N> {
    fn eval_const(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: LibKey) -> Result<(), ConstRefused> {
        match self.consts.as_deref_mut() {
            Some(values) => values.eval(store, keys, c).map_err(ConstRefused::Eval),
            None => Err(ConstRefused::NoEvaluator),
        }
    }
}

/// What a [`HeadCells`] source read to make one cell: other heads' cells
/// ([`Frozen::reads`]) and constants' values ([`Frozen::consts`]).
pub struct CellReads<'s> {
    pub heads: &'s [Want],
    pub consts: &'s [(LibKey, u128)],
}

/// A [`HeadSource`] that keeps what each cell's freeze read: the source of [`head_task`].
pub trait HeadCells: HeadSource {
    /// Cell `want` frozen (made on its first ask, once) and what its making read.
    fn cell(&mut self, want: Want) -> Result<(FrozenHead<'_>, CellReads<'_>), FreezeError>;
}

impl<'n, N: NamesSource<'n>> HeadCells for SeededHeads<'n, '_, N> {
    fn cell(&mut self, want: Want) -> Result<(FrozenHead<'_>, CellReads<'_>), FreezeError> {
        let at = self.made(want)?;
        let (_, bytes) = &self.kept.cells[at];
        let frozen = FrozenHead::read(bytes).map_err(|error| FreezeError::Read { want, error })?;
        Ok((frozen, CellReads { heads: &self.reads[at][..], consts: &self.values[at][..] }))
    }
}

impl<'n, N: NamesSource<'n>> HeadSource for SeededHeads<'n, '_, N> {
    fn frozen(&mut self, want: Want) -> Result<FrozenHead<'_>, FreezeError> {
        let at = self.made(want)?;
        let (_, bytes) = &self.kept.cells[at];
        FrozenHead::read(bytes).map_err(|error| FreezeError::Read { want, error })
    }

    fn open(&mut self, want: Want) -> bool {
        let open = self.making.contains(&want);
        self.reentered += u32::from(open);
        open
    }
}
