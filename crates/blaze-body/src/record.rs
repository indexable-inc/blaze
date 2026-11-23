//! The body's record: one blaze-format buffer whose root holds a slice per table, so a body is
//! read in place ([`open`] gives the same [`BodyRef`] a body in memory gives) and written once
//! ([`encode`]). The rows are [`crate::code`]'s; what they mean is fixed by [`VERSION`].
//!
//! Opening checks the buffer's layout once (blaze-format's verification: the root and every
//! slice in bounds and aligned) and the version; what the rows say is [`crate::verify`]'s.
#![blaze::sans_io]

use blaze_format_core::{FormatError, Own, Placed, Record, Slice, Unfit, Writer, WriterOpts, open_as};
use blaze_own::Vec;
use core::alloc::Allocator;
use core::mem::offset_of;

use crate::body::{BodyRef, Span, Table};
use crate::code::{
    BlockWords, CalleeWords, CaseWords, ConstWords, LoanWords, LocalWords, OperandWords, PairWords,
    PlaceWords, StmtWords, SubsetWords, TyWords,
};

/// What the rows of a body record mean: raised whenever a number in [`crate::code`] changes
/// meaning. A record of another version is refused, and the body lowered again (rule 26).
///
/// 2: the root gains `ty_base` and `list_base` after `reserved`, and the overlay's two slices
/// (`tys`, `ty_lists`) after `known`; the statement tags 24 (`SET_DISCRIMINANT`) and 25
/// (`DEINIT`) are new.
/// 3: the type tag 31 (`FOREIGN`, an extern type: `PNode::Foreign`) is new.
pub const VERSION: u32 = 3;

blaze_format_core::record! {
    /// A body record's root: the version, the overlay's bases, then one slice per table of
    /// [`crate::body::Body`].
    #[derive(Clone, Copy)]
    pub struct BodyRoot: 1 {
        pub version: u32,
        pub reserved: u32,
        pub ty_base: u32,
        pub list_base: u32,
        pub locals: Slice<LocalWords>,
        pub local_places: Slice<u32>,
        pub places: Slice<PlaceWords>,
        pub consts: Slice<ConstWords>,
        pub data: Slice<u8>,
        pub callees: Slice<CalleeWords>,
        pub stmts: Slice<StmtWords>,
        pub spans: Slice<Span>,
        pub stmt_block: Slice<u32>,
        pub blocks: Slice<BlockWords>,
        pub operands: Slice<OperandWords>,
        pub cases: Slice<CaseWords>,
        pub loans: Slice<LoanWords>,
        pub subsets: Slice<SubsetWords>,
        pub placeholder: Slice<u8>,
        pub local_origins: Slice<PairWords>,
        pub known: Slice<PairWords>,
        pub tys: Slice<TyWords>,
        pub ty_lists: Slice<u32>,
    }
}

/// Why a body could not be written as a record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EncodeError {
    /// A table's slice cannot be referred to from the root: the buffer passed the `i32`
    /// offset's reach (2 GiB).
    TooFar { table: Table, unfit: Unfit },
}

/// Why a buffer is not a body record of this version.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OpenError {
    /// Not a body record: blaze-format's check of the layout refused it.
    Format(FormatError),
    /// A body record of another version.
    Version { found: u32, want: u32 },
}

impl From<FormatError> for OpenError {
    fn from(e: FormatError) -> OpenError {
        OpenError::Format(e)
    }
}

/// Writes `body` as a record in `alloc`. Returns an empty buffer if the record would pass 2 GiB;
/// [`try_encode`] refuses that with a typed error.
pub fn encode<A: Allocator>(body: &BodyRef<'_>, alloc: A) -> Vec<u8, A> {
    let mut out = Writer::new(WriterOpts { schema: <BodyRoot as Record>::SCHEMA, alloc: Own(alloc), capacity: None });
    match write_root(&mut out, body) {
        Ok(at) => out.finish(at),
        Err(_) => {
            let at = out.push(&0u8);
            let mut bytes = out.finish(at);
            bytes.clear();
            bytes
        }
    }
}

/// Writes `body` as a record in `alloc`, or says which table's slice the root cannot reach.
/// Equal bodies give equal bytes (rule 25): the buffer is the tables in a fixed order, nothing
/// else.
pub fn try_encode<A: Allocator>(body: &BodyRef<'_>, alloc: A) -> Result<Vec<u8, A>, EncodeError> {
    let mut out = Writer::new(WriterOpts { schema: <BodyRoot as Record>::SCHEMA, alloc: Own(alloc), capacity: None });
    let at = write_root(&mut out, body)?;
    Ok(out.finish(at))
}

/// Writes `body`'s tables and then its root into `out`, and says where the root is: the
/// body record's own buffer ([`try_encode`]) or a record that holds a body (kept.rs's
/// `KeptRoot`, whose `body` slice is this root). The bytes from the first table to the root's
/// end depend on `body` and on where `out` stood, nothing else (rule 25).
pub(crate) fn write_root<A: Allocator>(out: &mut Writer<Own<A>>, body: &BodyRef<'_>) -> Result<u64, EncodeError> {
    let locals = Placed::push(out,body.locals);
    let local_places = Placed::push(out,body.local_places);
    let places = Placed::push(out,body.places);
    let consts = Placed::push(out,body.consts);
    let data = Placed::push(out,body.data);
    let callees = Placed::push(out,body.callees);
    let stmts = Placed::push(out,body.stmts);
    let spans = Placed::push(out,body.spans);
    let stmt_block = Placed::push(out,body.stmt_block);
    let blocks = Placed::push(out,body.blocks);
    let operands = Placed::push(out,body.operands);
    let cases = Placed::push(out,body.cases);
    let loans = Placed::push(out,body.loans);
    let subsets = Placed::push(out,body.subsets);
    let placeholder = Placed::push(out,body.placeholder);
    let local_origins = Placed::push(out,body.local_origins);
    let known = Placed::push(out,body.known);
    let tys = Placed::push(out,body.tys);
    let ty_lists = Placed::push(out,body.ty_lists);
    let at = out.next::<BodyRoot>();
    let root = BodyRoot {
        version: VERSION,
        reserved: 0,
        ty_base: body.ty_base,
        list_base: body.list_base,
        locals: reach(locals, at, offset_of!(BodyRoot, locals), Table::Locals)?,
        local_places: reach(local_places, at, offset_of!(BodyRoot, local_places), Table::LocalPlaces)?,
        places: reach(places, at, offset_of!(BodyRoot, places), Table::Places)?,
        consts: reach(consts, at, offset_of!(BodyRoot, consts), Table::Consts)?,
        data: reach(data, at, offset_of!(BodyRoot, data), Table::Data)?,
        callees: reach(callees, at, offset_of!(BodyRoot, callees), Table::Callees)?,
        stmts: reach(stmts, at, offset_of!(BodyRoot, stmts), Table::Stmts)?,
        spans: reach(spans, at, offset_of!(BodyRoot, spans), Table::Spans)?,
        stmt_block: reach(stmt_block, at, offset_of!(BodyRoot, stmt_block), Table::StmtBlocks)?,
        blocks: reach(blocks, at, offset_of!(BodyRoot, blocks), Table::Blocks)?,
        operands: reach(operands, at, offset_of!(BodyRoot, operands), Table::Operands)?,
        cases: reach(cases, at, offset_of!(BodyRoot, cases), Table::Cases)?,
        loans: reach(loans, at, offset_of!(BodyRoot, loans), Table::Loans)?,
        subsets: reach(subsets, at, offset_of!(BodyRoot, subsets), Table::Subsets)?,
        placeholder: reach(placeholder, at, offset_of!(BodyRoot, placeholder), Table::Origins)?,
        local_origins: reach(local_origins, at, offset_of!(BodyRoot, local_origins), Table::LocalOrigins)?,
        known: reach(known, at, offset_of!(BodyRoot, known), Table::Known)?,
        tys: reach(tys, at, offset_of!(BodyRoot, tys), Table::Types)?,
        ty_lists: reach(ty_lists, at, offset_of!(BodyRoot, ty_lists), Table::TyLists)?,
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at);
    Ok(at)
}

/// The root's slice of column `p` for the field at `offset` in the root written at `at`.
fn reach<T>(p: Placed, at: u64, offset: usize, table: Table) -> Result<Slice<T>, EncodeError> {
    p.slice_as::<T, i32>(at + offset as u64).map_err(|unfit| EncodeError::TooFar { table, unfit })
}

/// Opens a body record in place: its layout checked once, each table a slice of `buffer`.
/// Run [`crate::verify::verify_ref`] on the result before reading it as a body.
pub fn open(buffer: &[u8]) -> Result<BodyRef<'_>, OpenError> {
    let root = open_as::<BodyRoot>(buffer, <BodyRoot as Record>::SCHEMA)?;
    body_of(root, buffer)
}

/// The body whose root is `root`, a record of `buffer` already checked by blaze-format (its
/// own buffer's root, or the `body` slice of a kept record): the version, then each table a
/// slice of `buffer`.
pub(crate) fn body_of<'b>(root: &'b BodyRoot, buffer: &'b [u8]) -> Result<BodyRef<'b>, OpenError> {
    if root.version != VERSION {
        return Err(OpenError::Version { found: root.version, want: VERSION });
    }
    Ok(BodyRef {
        locals: root.locals.follow(buffer)?,
        local_places: root.local_places.follow(buffer)?,
        places: root.places.follow(buffer)?,
        consts: root.consts.follow(buffer)?,
        data: root.data.follow(buffer)?,
        callees: root.callees.follow(buffer)?,
        stmts: root.stmts.follow(buffer)?,
        spans: root.spans.follow(buffer)?,
        stmt_block: root.stmt_block.follow(buffer)?,
        blocks: root.blocks.follow(buffer)?,
        operands: root.operands.follow(buffer)?,
        cases: root.cases.follow(buffer)?,
        loans: root.loans.follow(buffer)?,
        subsets: root.subsets.follow(buffer)?,
        placeholder: root.placeholder.follow(buffer)?,
        local_origins: root.local_origins.follow(buffer)?,
        known: root.known.follow(buffer)?,
        ty_base: root.ty_base,
        list_base: root.list_base,
        tys: root.tys.follow(buffer)?,
        ty_lists: root.ty_lists.follow(buffer)?,
    })
}
