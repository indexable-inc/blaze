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
//! impl's). The lifetime parts are in the form too (head_record.rs form 10: `FnLife`,
//! `AdtLife`, `ContainerLife`, each made or refused by name, made at [`export`] by lifetimes.rs
//! `Store::make_lives`): the borrow check's `BorrowHead`/`BorrowAdt` facts read the head cell
//! (tables/task.rs `fact_read`), so an edit of a lifetime bound alone moves that cell's meaning
//! and the `Signature` out with it (head_tests.rs `a_lifetime_bound_alone_moves_the_head`).

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, ABSENT, Key, Kind, Name, Tool};
use blaze_engine::CellId;
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
use crate::front2::types::Why;
use crate::front2::head_record::{self, Head, HeadAt, HeadRecordError, HeadRef};
use crate::front2::heads::{HeadError, SeedFailed, Seeded, Store, Want};
use crate::front2::keys::{Keys, MemberKind, checker_of, member_key_of};
use crate::front2::project::head_digest::{digest_of_heads, want_heads};
use crate::front2::source::NamesSource;

const MAGIC: [u8; 4] = *b"BZFZ";
/// Raised on any change of the form; head_record.rs's own version is inside each record.
/// 2: the records are head_record.rs form 9 (the store's side tables).
/// 3: the records are form 10 (every lifetime part, made or refused).
/// 4: an enum's cell holds its tuple variants' ctor heads (head_digest.rs `want_heads`).
/// 5: the records are form 12 (a refused type alias is a record, `K_ALIAS_REFUSED`).
/// 6: the records are form 13 (a member refused alone is a record, `K_FN_REFUSED`), and a
/// trait's members cell holds them (head_digest.rs `want_heads`).
pub const VERSION: u32 = 6;
/// How many bytes [`want_bytes`] writes: a cell's name in the form's header, and in a head
/// task's payload (blaze-cli engine_heads.rs `heads_of`).
pub const WANT_BYTES: usize = 1 + 32 + 16;
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
    /// Cell `want` is not frozen again: it failed before, as cell `failed`'s freeze did (the
    /// first asker was handed that failure).
    Again { want: Want, failed: Want },
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
            FreezeError::Again { want, failed } => write!(f, "frozen head of {want:?}: not made again, as cell {failed:?} failed when first frozen"),
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
    /// The constants' values its store held when made, by key (each a `Why::NeedConst` key):
    /// a superset of the values its heads hold (one the evaluator met on the way is in it
    /// too). Read by value ([`const_value_cell`]): the record holds the value, not the
    /// constant's text, so an edit that keeps the value keeps the record.
    pub consts: AVec<'a, (LibKey, u128)>,
    /// The wants its store made by lowering (heads.rs `Store::lowered`): over a store seeded
    /// from frozen heads ([`freeze_seeded`]), only the item's own.
    pub lowered: u32,
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
    want: Want,
    source: &mut H,
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
    let mut asked: AVec<'a, LibKey> = AVec::new_in(alloc);
    for _ in 0..=MOST_FREEZE_CONSTS {
        let c = match ensure_in(names, want, want, source, &mut store, &mut keys, &mut needed, scratch) {
            Ok(()) => return finish(names, &keys, &mut store, want, &needed, alloc),
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
        if names.text_of(c).is_some() && !store.made(Want::Fn(c)) {
            // Its key's origin, which the evaluator lowers its body by (keys.rs `library_in`;
            // `Front2::new` files every item's in a body's keys, a freeze's start empty).
            keys.item(c).map_err(|clash| FreezeError::Head { want, error: HeadError::Key(clash) })?;
            match ensure_in(names, want, Want::Fn(c), source, &mut store, &mut keys, &mut needed, scratch) {
                Ok(()) => {}
                Err(Need::Fail(e)) => return Err(e),
                Err(Need::Const(of)) => return Err(FreezeError::Const { want, c, error: ConstRefused::TypeNeedsConst { of } }),
            }
        }
        match source.eval_const(&mut store, &mut keys, c) {
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
    Ok(Frozen { bytes, reads, consts, lowered: store.lowered })
}

/// The seed of a store making cell `own` (`None`: a body's store, which makes no cell): need
/// `need`'s cell filed from `source`'s frozen record, or (its own cell, or one `source` is
/// making) lowered there. The seed a body's store passes to heads.rs `Store::ensure_seeded`,
/// as [`freeze_seeded`] does (stash the error, hand back `SeedFailed`).
pub fn seed<'n, N: NamesSource<'n>, H: HeadSource>(names: N, own: Option<Want>, need: Want, source: &mut H, store: &Store<'n>, keys: &Keys, scratch: Alloc<'static>) -> Result<Seeded, FreezeError> {
    let cell = cell_of(names, need)?;
    if Some(cell) == own || source.open(cell) {
        return Ok(Seeded::Lower);
    }
    let frozen = source.frozen(cell)?;
    let members = members_of(names, cell, scratch);
    let mut text_of = |at: HeadAt| fn_text(names, cell, &members, at);
    let heads = import(&frozen, &mut text_of, &store.tys, scratch, scratch)?;
    file_keys(cell, &members, &heads, keys)?;
    store.file_heads(cell, heads).map_err(|error| FreezeError::Head { want: cell, error })?;
    Ok(Seeded::Filed)
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
            return names.text_of(lib).map(|t| t.text);
        }
    }
    members.iter().find(|m| m.key == k && m.kind != MemberKind::Type).map(|m| m.text)
}

/// The job's key origins a seeded cell's heads name, filed as its lowering files them
/// (heads.rs `keys.member`): a struct's or union's constructor; a trait's fns, associated
/// types and constants; an impl's fns and constants. A member the heads do not name (one its
/// crate's cfg leaves out) is not filed.
fn file_keys(cell: Want, members: &[MemberText<'_>], heads: &[(HeadAt, Head<'_>)], keys: &Keys) -> Result<(), FreezeError> {
    let container = checker_of(cell.key());
    let clash = |error| FreezeError::Head { want: cell, error: HeadError::Key(error) };
    for (_, head) in heads.iter() {
        match head {
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
    /// `task_key(tool, name, digest of the item's whole text)`: before the work.
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
    let text = names.text_of(want.key()).map(|t| t.text);
    let from = match text {
        Some(t) => key::digest(HEAD_SOURCE, t.as_bytes()),
        None => key::digest(HEAD_NO_TEXT, &want_bytes(want)),
    };
    let key = key::task_key(tool, id.of, &from);
    let made = match text {
        Some(_) => head_task_made(source, want, id, alloc),
        None => Err(FreezeError::NoText { want }),
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

    fn open(&mut self, _want: Want) -> bool {
        false
    }
}

/// Heads each frozen in a store seeded from the others ([`freeze_seeded`], this source the
/// seed's), kept by cell: freezing N items lowers each item once.
pub struct SeededHeads<'n, 'a, N> {
    names: N,
    sized: Option<LibKey>,
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
    /// Each cell whose freeze failed, with the cell whose failure it was (itself, or a need of
    /// it): asked again, it is refused by name ([`FreezeError::Again`]), never frozen twice.
    failed: AVec<'a, (Want, Want)>,
    /// Every freeze begun, made or given up for a need frozen first (`attempts - cells` were
    /// begun again).
    pub attempts: u32,
    /// What evaluates a constant a head's type holds ([`ConstSource::eval_const`]).
    consts: Option<&'a mut (dyn ConstValues<'n> + 'a)>,
    /// The wants all its freezes made by lowering (heads.rs `Store::lowered`).
    pub lowered: u32,
    /// The needs met of a cell up the stack ([`HeadSource::open`]), each lowered where met.
    pub reentered: u32,
    /// The frozen heads known before this source freezes a cell itself (the request's, seeds.rs
    /// `RequestHeads`): a cell it knows is copied in, never frozen again.
    known: Option<&'a (dyn KnownHeads + 'a)>,
    /// For each kept cell (at its index in `kept`), whether it was copied from `known`.
    from_known: AVec<'a, bool>,
    /// The cells copied from `known`.
    pub taken: u32,
    alloc: Alloc<'a>,
}

/// Frozen heads a [`SeededHeads`] takes before it freezes a cell itself: the request's table
/// and what other workers froze (seeds.rs `RequestSeed`). The bytes handed back are the cell's
/// frozen head as [`freeze_seeded`] would make it: a freeze is a function of its cell and the
/// names, so a known head and one made here are equal bytes.
pub trait KnownHeads {
    /// Cell `cell`'s frozen bytes (the `BZFZ` form), if known.
    fn known(&self, cell: Want) -> Option<&[u8]>;
}

/// What a [`SeededHeads`] is made over: [`freeze`]'s inputs and the constant evaluator.
pub struct SeededHeadsOpts<'n, 'a, N> {
    pub names: N,
    pub sized: Option<LibKey>,
    pub frame: &'n blaze_mem::Frame,
    pub scratch: Alloc<'static>,
    pub alloc: Alloc<'a>,
    /// `None`: a head whose type holds a constant is refused ([`ConstRefused::NoEvaluator`]).
    pub consts: Option<&'a mut (dyn ConstValues<'n> + 'a)>,
    /// The heads known before a freeze ([`KnownHeads`]); `None`: every cell is frozen here. A
    /// cell taken from it keeps no reads ([`HeadCells::cell`] answers it with none), so a head
    /// task, whose read list is its source's, takes none.
    pub known: Option<&'a (dyn KnownHeads + 'a)>,
}

impl<'n, 'a, N: NamesSource<'n>> SeededHeads<'n, 'a, N> {
    pub fn new(o: SeededHeadsOpts<'n, 'a, N>) -> SeededHeads<'n, 'a, N> {
        let kept = Kept { cells: AVec::new_in(o.alloc) };
        SeededHeads {
            names: o.names,
            sized: o.sized,
            frame: o.frame,
            scratch: o.scratch,
            kept,
            reads: AVec::new_in(o.alloc),
            values: AVec::new_in(o.alloc),
            making: AVec::new_in(o.alloc),
            failed: AVec::new_in(o.alloc),
            attempts: 0,
            consts: o.consts,
            lowered: 0,
            reentered: 0,
            known: o.known,
            from_known: AVec::new_in(o.alloc),
            taken: 0,
            alloc: o.alloc,
        }
    }

    /// The cells frozen so far, in the order they were finished.
    pub fn cells(&self) -> impl Iterator<Item = Want> + '_ {
        self.kept.cells.iter().map(|(w, _)| *w)
    }

    /// How many cells it keeps (frozen here or taken from its known heads).
    pub fn kept_len(&self) -> usize {
        self.kept.cells.len()
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
        if let Some(at) = self.kept.cells.iter().position(|(w, _)| *w == want) {
            return Ok(at);
        }
        if let Some(&(_, failed)) = self.failed.iter().find(|(w, _)| *w == want) {
            return Err(FreezeError::Again { want, failed });
        }
        // Known already (the request's table, another worker's freeze): copied in, so its
        // bytes are read like a cell frozen here, and never frozen again.
        if let Some(bytes) = self.known.and_then(|known| known.known(want)) {
            let mut copy = RecordBytes::with_capacity_in(bytes.len(), self.alloc);
            copy.extend_from_slice(bytes);
            self.kept.cells.push((want, copy));
            self.reads.push(AVec::new_in(self.alloc));
            self.values.push(AVec::new_in(self.alloc));
            self.from_known.push(true);
            self.taken += 1;
            return Ok(self.kept.cells.len() - 1);
        }
        if !self.making.is_empty() {
            return Err(FreezeError::Unfrozen { need: want });
        }
        self.making.push(want);
        let driven = self.drive(want);
        self.making.clear();
        driven?;
        // The driver returns once `want`, the stack's bottom, is kept.
        self.kept.cells.iter().position(|(w, _)| *w == want).ok_or(FreezeError::Unfrozen { need: want })
    }

    /// Freeze the cells on `making`, innermost first, until it is empty: a freeze that meets a
    /// need not yet frozen pushes it and is begun again once it is kept. Rule 18: each attempt
    /// keeps a cell or pushes one, the stack at most [`MOST_MAKING`] deep, the attempts at most
    /// [`MOST_ATTEMPTS`]. A failure fails every cell on the stack (each needs the one above it),
    /// kept so none is frozen again, and is handed to the asker as it was.
    fn drive(&mut self, want: Want) -> Result<(), FreezeError> {
        let (names, sized, frame, scratch, alloc) = (self.names, self.sized, self.frame, self.scratch, self.alloc);
        for _ in 0..MOST_ATTEMPTS {
            let Some(&top) = self.making.last() else { return Ok(()) };
            self.attempts += 1;
            let reentered = self.reentered;
            match freeze_seeded(names, sized, top, self, frame, scratch, alloc) {
                Ok(f) => {
                    self.making.pop();
                    self.lowered += f.lowered;
                    self.kept.cells.push((top, f.bytes));
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
                }
                Err(e) => {
                    let making = core::mem::replace(&mut self.making, AVec::new_in(alloc));
                    self.failed.extend(making.iter().map(|w| (*w, top)));
                    return Err(e);
                }
            }
        }
        Err(FreezeError::Steps { want, most: MOST_ATTEMPTS })
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
