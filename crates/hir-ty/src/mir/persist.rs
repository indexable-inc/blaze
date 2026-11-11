//! Built MIR in blaze's format (docs/v2/plan/design/item-hash.md section 7): a [`MirBody`] as
//! one `blaze_format` buffer, which the item store files under its body's identity, so a later
//! build, branch or machine reads the body instead of lowering it again.
//!
//! The body is a column of `u32` words in prefix form: each value's first word is its tag, and
//! each list starts with its length; [`read`] checks every tag, length and index (a local, a
//! block, an id of the store, a reference into the body's own tables) as it rebuilds the body.
//! Types, constants and generic arguments are places in the buffer's own type table
//! (`next_solver::persist`); an allocation (a constant operand's value) is
//! `consteval`'s record, written once however often the body uses it; text (an `asm!`
//! template's literals, register names) is one byte column. Locals, blocks, fields and spans
//! are the body's own indices: a span is an `ExprId`, `PatId` or `BindingId` of the body's
//! store, never a position in a file, so moving the body in its file changes no byte.
//!
//! Every id (the owner, a static, a variant, a closure, each definition a type names) is
//! written through `check_cache::id_bits`: in the item store's relocating scope that is its
//! place in the record's id table, which the store writes as item keys, so two databases that
//! built the same body write the same bytes. Outside a scope it is the id's own bits, read
//! back only by the database that wrote them.
//!
//! A constant a location macro made (`line!()`, `column!()`, `file!()`, `module_path!()`, a
//! `concat!` of one) is written as a slot, its literal's expression and its type, never its
//! value: reading fills it from that literal as the body is now ([`Locations`]), so moving or
//! renaming the code reads as the MIR building it again gives. `#[track_caller]` and panic
//! locations are not constants of built MIR: they are read off the spans where used.

#[cfg(test)]
mod tests;

use std::mem::offset_of;

use base_db::Db;
use blaze_format::{
    FormatError, Part, Placed, Record, Rel, SchemaHash, Slice, Writer, open_as, schema_of,
};
use blaze_hash::HashMap;
use hir_def::{
    DefWithBodyId, FieldId, LocalFieldId, VariantId,
    expr_store::ExpressionStore,
    hir::{
        AsmOperand as HirAsmOperand, AsmOptions, AsmPiece, BindingId, Expr, ExprId,
        InlineAsmRegOrRegClass, Literal, Pat, PatId,
    },
};
use la_arena::{Arena, ArenaMap, Idx, RawIdx};
use rustc_type_ir::{IntTy, UintTy, UniverseIndex, inherent::IntoKind as _};
use smallvec::SmallVec;
use std::alloc::Global;

use super::{
    AggregateKind, AsmOperand, AssertKind, BasicBlock, BasicBlockId, BinOp, BorrowKind, CastKind,
    ComptimeBinder, CoroutineLocals, FieldIndex, InlineAsm, Inlined, Local, LocalId, MirBody,
    MirPhase, MirSpan, MutBorrowKind, Operand, OperandKind, Place, PlaceElem, Projection,
    ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, SwitchTargets, Terminator,
    TerminatorKind, UnOp, UnreachableAfterCall,
};
use crate::{
    InferBodyId, MemoryMap,
    check_cache::{Relocate, id_bits, id_of},
    closure_analysis,
    consteval::{AllocationRec, read_allocation, write_allocation},
    infer::PointerCast,
    intrinsic::Intrinsic,
    next_solver::{
        Allocation, AllocationData, ConstExt as _, DbInterner, StoredAllocation, StoredConst,
        StoredGenericArgs, StoredTy, Ty, TyKind,
        abi::Safety,
        persist::{TABLE_SCHEMA, TyTableReader, TyTableWriter},
    },
};

/// The version of the words [`write`] writes: changes with any change to what a word means.
/// 5: no inlined tables (only built MIR is kept, which has none).
/// 6: a coroutine's locals carry the span of its body.
const WORDS_VERSION: u64 = 6;

/// The literals of a body that a location macro expanded to, sorted, with the store they are
/// expressions of: the constants a body's MIR makes of them are written as slots and filled
/// from the store on reading.
#[derive(Clone, Copy)]
pub(crate) struct Locations<'a> {
    pub(crate) store: &'a ExpressionStore,
    pub(crate) exprs: &'a [ExprId],
    /// How many bytes an address of the body's target takes.
    pub(crate) word: usize,
}

impl Locations<'_> {
    fn has(&self, expr: ExprId) -> bool {
        self.exprs.binary_search(&expr).is_ok()
    }

    /// Why the body's MIR cannot hold its location literals as slots: one used where lowering
    /// makes a value of it with no operand of its own (a pattern, an `asm!` constant).
    fn unslotted(&self) -> Option<&'static str> {
        if self.exprs.is_empty() {
            return None;
        }
        let in_pattern = self.store.pats().any(|(_, pat)| match pat {
            Pat::Lit(expr) => self.has(expr),
            Pat::Range { start, end, .. } => {
                start.is_some_and(|it| self.has(it)) || end.is_some_and(|it| self.has(it))
            }
            _ => false,
        });
        if in_pattern {
            return Some("a location macro in a pattern");
        }
        let in_asm = self.store.exprs().any(|(_, expr)| match expr {
            Expr::InlineAsm(asm) => asm
                .operands
                .iter()
                .any(|(_, operand)| matches!(operand, HirAsmOperand::Const(it) if self.has(*it))),
            _ => false,
        });
        in_asm.then_some("a location macro as an `asm!` constant")
    }

    /// The constant of type `ty` that MIR building makes of the literal `expr` as the store
    /// has it now, by its rule (`lower_literal_to_operand`) for the literals location macros
    /// expand to; `None` for another.
    fn constant<'db>(&self, expr: ExprId, ty: Ty<'db>) -> Option<StoredAllocation> {
        let Expr::Literal(literal) = self.store.expr(expr) else { return None };
        let word = self.word;
        let (memory, memory_map): (Box<[u8]>, MemoryMap<'db>) = match *literal {
            Literal::String(ref text) => {
                let text = text.as_str();
                // A reference to the text's bytes, whose address the memory map gives.
                // alloc: the constant and its bytes, once per location literal written or read
                let mut fat = Vec::with_capacity(2 * word);
                fat.extend_from_slice(&0usize.to_le_bytes()[..word]);
                fat.extend_from_slice(&text.len().to_le_bytes()[..word]);
                (fat.into_boxed_slice(), MemoryMap::simple(Box::from(text.as_bytes())))
            }
            Literal::Uint(value, _) => {
                (Box::from(&value.to_le_bytes()[..int_size(ty, word)?]), MemoryMap::Empty)
            }
            Literal::Int(value, _) => {
                (Box::from(&value.to_le_bytes()[..int_size(ty, word)?]), MemoryMap::Empty)
            }
            _ => return None,
        };
        Some(Allocation::new(AllocationData { ty, memory, memory_map }).store())
    }
}

/// The size of the integer type `ty` on a target of `word`-byte addresses.
fn int_size(ty: Ty<'_>, word: usize) -> Option<usize> {
    Some(match ty.kind() {
        TyKind::Uint(UintTy::U8) | TyKind::Int(IntTy::I8) => 1,
        TyKind::Uint(UintTy::U16) | TyKind::Int(IntTy::I16) => 2,
        TyKind::Uint(UintTy::U32) | TyKind::Int(IntTy::I32) => 4,
        TyKind::Uint(UintTy::U64) | TyKind::Int(IntTy::I64) => 8,
        TyKind::Uint(UintTy::U128) | TyKind::Int(IntTy::I128) => 16,
        TyKind::Uint(UintTy::Usize) | TyKind::Int(IntTy::Isize) => word,
        _ => return None,
    })
}

blaze_format::record! {
    /// A MIR body: where its type table is, its words, its text, and each allocation its
    /// constant operands hold, by the index the words name it with.
    #[derive(Clone, Copy)]
    struct MirRoot: 1 {
        tys: u64,
        words: Slice<u32>,
        text: Slice<u8>,
        allocs: Slice<Rel<AllocationRec>>,
    }
}

const _: () = assert!(size_of::<MirRoot>() == 32);

/// The schema of a MIR buffer and of everything in it: a body written by another encoding
/// reads as a schema mismatch, never as another body.
pub(crate) const SCHEMA: SchemaHash = schema_of(&[
    Part::Str("hir_ty::mir::persist::MirBody"),
    Part::U64(WORDS_VERSION),
    Part::Schema(<MirRoot as Record>::SCHEMA),
    Part::Schema(TABLE_SCHEMA),
    Part::Schema(<AllocationRec as Record>::SCHEMA),
    // A call's intrinsic is stored by its index.
    Part::Schema(Intrinsic::SCHEMA),
]);

/// Why [`write`] refused a body.
#[derive(Debug)]
pub(crate) enum WriteError {
    /// What of the body has no word, or is never kept: a lint's description lowering did not
    /// make, a location literal used with no operand of its own or whose constant is not what
    /// its literal makes, anything of optimized MIR (an inlined table or span, a specialized
    /// call, an allocation moved into the frame).
    Unwritable(&'static str),
    /// The buffer written does not read back as the body: a lowering or writer bug, the
    /// build's error when the body is offered (`body_store::fail`), never a body left unstored.
    /// Checked in debug builds, against `locations`' store.
    Unreadable(ReadBack),
}

/// How a buffer [`write`] wrote failed to read back.
#[derive(Debug)]
pub(crate) enum ReadBack {
    /// [`read`] refuses it: the body breaks one of `read`'s rules.
    Refused(FormatError),
    /// It reads as another body: writer and reader skew that keeps every index in range. The
    /// first field of [`MirBody`] that differs.
    Differs(&'static str),
}

impl std::fmt::Display for ReadBack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadBack::Refused(why) => write!(f, "a body written that does not read back: {why}"),
            ReadBack::Differs(what) => {
                write!(f, "a body written that reads back as another: its {what} differ")
            }
        }
    }
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Unwritable(why) => f.write_str(why),
            WriteError::Unreadable(why) => why.fmt(f),
        }
    }
}

/// `body` as a buffer of its own, the constants of `locations` as slots. Refused, with why,
/// for what has no word ([`WriteError::Unwritable`]) and, in debug builds with `locations`
/// given, for a buffer [`read`] refuses or reads as another body ([`WriteError::Unreadable`]):
/// `read`'s rules are checked once, in `read`, and `write` holds to them by reading its own
/// buffer back and comparing it with `body`.
pub(crate) fn write<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    locations: Option<Locations<'_>>,
) -> Result<Vec<u8>, WriteError> {
    let buffer = write_unchecked(db, body, locations).map_err(WriteError::Unwritable)?;
    if cfg!(debug_assertions)
        && let Some(locations) = locations
    {
        let read = read(db, &blaze_format::Aligned::copy_from(&buffer), locations)
            .map_err(|why| WriteError::Unreadable(ReadBack::Refused(why)))?;
        if let Some(what) = first_difference(body, &read) {
            return Err(WriteError::Unreadable(ReadBack::Differs(what)));
        }
    }
    Ok(buffer)
}

/// The first field of `read` that is not `written`'s, by name; `None` when they are equal.
fn first_difference(written: &MirBody<'_>, read: &MirBody<'_>) -> Option<&'static str> {
    let MirBody {
        basic_blocks,
        locals,
        start_block,
        owner,
        binding_locals,
        upvar_locals,
        param_locals,
        promoted_locals,
        closures,
        coroutine,
        unreached,
        scope_ends,
        inherits_overflow_checks,
        phase,
        upvars_in_place,
        panic_unwinds,
        promoted_constants,
        unreachable_after_calls,
        guard_refs,
        capture_copies,
        inlined,
    } = written;
    [
        ("basic blocks", *basic_blocks == read.basic_blocks),
        ("locals", *locals == read.locals),
        ("start blocks", *start_block == read.start_block),
        ("owners", *owner == read.owner),
        ("binding locals", *binding_locals == read.binding_locals),
        ("upvar locals", *upvar_locals == read.upvar_locals),
        ("parameter locals", *param_locals == read.param_locals),
        ("promoted locals", *promoted_locals == read.promoted_locals),
        ("closures", *closures == read.closures),
        ("coroutine locals", *coroutine == read.coroutine),
        ("unreached blocks", *unreached == read.unreached),
        ("scope ends", *scope_ends == read.scope_ends),
        ("overflow check inheritances", *inherits_overflow_checks == read.inherits_overflow_checks),
        ("phases", *phase == read.phase),
        ("upvars in place", *upvars_in_place == read.upvars_in_place),
        ("panic unwinds", *panic_unwinds == read.panic_unwinds),
        ("promoted constants", *promoted_constants == read.promoted_constants),
        ("unreachable-after calls", *unreachable_after_calls == read.unreachable_after_calls),
        ("guard references", *guard_refs == read.guard_refs),
        ("capture copies", *capture_copies == read.capture_copies),
        ("inlined tables", *inlined == read.inlined),
    ]
    .into_iter()
    .find_map(|(what, same)| (!same).then_some(what))
}

/// [`write`] without reading the buffer back.
fn write_unchecked<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    locations: Option<Locations<'_>>,
) -> Result<Vec<u8>, &'static str> {
    if let Some(why) = locations.and_then(|it| it.unslotted()) {
        return Err(why);
    }
    let mut out = Writer::new(SCHEMA);
    let mut encode = Encode {
        out: &mut out,
        tys: TyTableWriter::new(DbInterner::new_no_crate(db)),
        // alloc: the record's columns, once per body written at store time
        words: Vec::with_capacity(words_hint(body)),
        // alloc: as above
        text: Vec::new(),
        // alloc: as above
        allocs: Vec::new(),
        alloc_places: HashMap::new_in(Global),
        refused: None,
        locations,
    };
    encode.body(body)?;
    let Encode { out: _, tys, words, text, allocs, alloc_places: _, refused, locations: _ } =
        encode;
    if let Some(why) = refused {
        return Err(why);
    }
    let tys = tys.write(&mut out);
    let words = Placed::push(&mut out, &words);
    let text = Placed::push(&mut out, &text);
    let rels_at = out.next::<Rel<AllocationRec>>();
    let rel_size = size_of::<Rel<AllocationRec>>() as u64;
    let allocs = Placed::push_each(
        &mut out,
        allocs.len(),
        allocs.iter().enumerate().map(|(n, &target)| {
            Rel::<AllocationRec>::between(rels_at + n as u64 * rel_size, target)
        }),
    );
    debug_assert!(allocs.len == 0 || allocs.at == rels_at);
    let at = out.next::<MirRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = MirRoot {
        tys,
        words: words.slice(field(offset_of!(MirRoot, words))),
        text: text.slice(field(offset_of!(MirRoot, text))),
        allocs: allocs.slice(field(offset_of!(MirRoot, allocs))),
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    Ok(out.finish(at))
}

/// The body [`write`] wrote into `buffer`, made in `db`: its header, schema and columns checked
/// in place, then every word as it is read. Input this read can bound and that is out of range
/// is an error, never a panic, and no length or index it holds is trusted to size an
/// allocation: a list is at most as long as the words left can hold. Every index is checked
/// against the length of the table it indexes: an expression, pattern or binding against
/// `locations`' store (the store the body's spans and bindings index), a union's field against
/// the union's fields, the body's references into its own tables (a statement of a block, an
/// `asm!` operand) against those tables as read, a switch's targets against its values, and a
/// `for const` binder against the universes. Built MIR has no inlined code, so an inlined span
/// is refused. Slots are filled from `locations`; a slot it does not list, or whose expression
/// is no longer such a literal, is an error.
///
/// What this read does not catch: damage that keeps every index in range reads as another
/// body (another local, block, operator or field). The store rules that out, checking each
/// value against the content hash it was put with (`ImageStore::get`,
/// hir-def/src/deps/serve.rs); this read's checks guard against writer and reader skew and
/// crafted values. Field indexes of place projections (`ProjectionElem::Field`, a captured
/// place's `ProjectionKind::Field`) are relative to a type and are not checked here: that needs
/// the place types, which `Mode::Verify` gets by building the body again.
pub(crate) fn read<'db>(
    db: &'db Db,
    buffer: &[u8],
    locations: Locations<'_>,
) -> Result<MirBody<'db>, FormatError> {
    let root = open_as::<MirRoot>(buffer, SCHEMA)?;
    let tys = TyTableReader::open(DbInterner::new_no_crate(db), buffer, root.tys)?;
    let allocs = root.allocs.follow(buffer)?;
    let words = root.words.follow(buffer)?;
    let store = locations.store;
    let mut decode = Decode {
        db,
        words,
        words_at: (words.as_ptr().addr() - buffer.as_ptr().addr()) as u64,
        at: 0,
        text: root.text.follow(buffer)?,
        tys,
        allocs,
        // alloc: one slot per allocation the body holds, once per body read
        made: vec![None; allocs.len()],
        buffer,
        locals: 0,
        blocks: 0,
        exprs: table_len(store.expr_count()),
        pats: table_len(store.pat_count()),
        bindings: table_len(store.bindings().count()),
        locations,
    };
    let body = decode.body()?;
    match decode.at == decode.words.len() {
        true => Ok(body),
        false => Err(FormatError::Invalid {
            at: decode.byte(decode.at),
            what: "words left over after the body",
        }),
    }
}

/// A table's length as the `u32` its indices are: a table of `2^32` rows or more (none fits
/// in memory) bounds nothing a word can name, so it saturates rather than wrap.
fn table_len(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// About how many words `body` writes, so the column grows once or not at all.
fn words_hint(body: &MirBody<'_>) -> usize {
    let statements: usize = body.basic_blocks.values().map(|it| it.statements.len()).sum();
    64 + body.locals.len() + 12 * body.basic_blocks.len() + 12 * statements
}

struct Encode<'w, 'db> {
    out: &'w mut Writer,
    tys: TyTableWriter<'db>,
    words: Vec<u32>,
    text: Vec<u8>,
    /// Where each allocation's record is, by the index the words name it with.
    allocs: Vec<u64>,
    alloc_places: HashMap<Allocation<'db>, u32, Global>,
    /// Why the body has no stored form, found where a word could not say so.
    refused: Option<&'static str>,
    locations: Option<Locations<'w>>,
}

type W<T = ()> = Result<T, &'static str>;

impl<'db> Encode<'_, 'db> {
    #[inline]
    fn word(&mut self, word: u32) {
        self.words.push(word);
    }

    #[inline]
    fn bool(&mut self, value: bool) {
        self.word(u32::from(value));
    }

    fn u64(&mut self, value: u64) {
        self.words.extend([value as u32, (value >> 32) as u32]);
    }

    fn u128(&mut self, value: u128) {
        self.u64(value as u64);
        self.u64((value >> 64) as u64);
    }

    #[inline]
    fn count(&mut self, len: usize) {
        self.word(u32::try_from(len).expect("a MIR list of fewer than 2^32 items"));
    }

    #[inline]
    fn id<T: Relocate>(&mut self, id: T) {
        self.u64(id_bits(id));
    }

    #[inline]
    fn ty(&mut self, ty: &StoredTy) {
        let place = self.tys.ty(ty.as_ref());
        self.word(place);
    }

    fn konst(&mut self, konst: &StoredConst) {
        let place = self.tys.konst(konst.as_ref());
        self.word(place);
    }

    fn args(&mut self, args: &StoredGenericArgs) {
        let place = self.tys.args(args.as_ref());
        self.word(place);
    }

    fn allocation(&mut self, allocation: &StoredAllocation) {
        let allocation = allocation.as_ref();
        let index = match self.alloc_places.get(&allocation) {
            Some(&index) => index,
            None => {
                let at = write_allocation(self.out, &mut self.tys, allocation);
                let index = u32::try_from(self.allocs.len()).expect("fewer than 2^32 allocations");
                self.allocs.push(at);
                self.alloc_places.insert(allocation, index);
                index
            }
        };
        self.word(index);
    }

    fn text(&mut self, text: &str) {
        let start = u32::try_from(self.text.len()).expect("a body's text under 4 GiB");
        self.word(start);
        self.count(text.len());
        self.text.extend_from_slice(text.as_bytes());
    }

    #[inline]
    fn local(&mut self, local: LocalId) {
        self.word(local.into_raw().into_u32());
    }

    #[inline]
    fn block(&mut self, block: BasicBlockId) {
        self.word(block.into_raw().into_u32());
    }

    fn opt_block(&mut self, block: Option<BasicBlockId>) {
        self.word(block.map_or(0, |it| it.into_raw().into_u32() + 1));
    }

    fn locals(&mut self, locals: &[LocalId]) {
        self.count(locals.len());
        locals.iter().for_each(|&it| self.local(it));
    }

    fn body(&mut self, body: &MirBody<'db>) -> W {
        let MirBody {
            basic_blocks,
            locals,
            start_block,
            owner,
            binding_locals,
            upvar_locals,
            param_locals,
            promoted_locals,
            closures,
            coroutine,
            unreached,
            scope_ends,
            inherits_overflow_checks,
            phase,
            upvars_in_place,
            panic_unwinds,
            promoted_constants,
            unreachable_after_calls,
            guard_refs,
            capture_copies,
            inlined,
        } = body;
        self.owner(*owner);
        self.word(match phase {
            MirPhase::Built => 0,
            MirPhase::Runtime => 1,
        });
        self.bool(*inherits_overflow_checks);
        self.count(locals.len());
        for local in locals.values() {
            self.ty(&local.ty);
        }
        self.count(basic_blocks.len());
        self.block(*start_block);
        for block in basic_blocks.values() {
            self.basic_block(block);
        }
        // An `ArenaMap` built by inserts only: no slot after the last binding.
        self.count(binding_locals.values().count());
        for (binding, &local) in binding_locals.iter() {
            self.word(binding.into_raw().into_u32());
            self.local(local);
        }
        self.upvar_locals(upvar_locals);
        self.locals(param_locals);
        self.count(promoted_locals.len());
        for &(local, expr) in promoted_locals {
            self.local(local);
            self.word(expr.into_raw().into_u32());
        }
        self.count(closures.len());
        closures.iter().for_each(|&it| self.id(it));
        match coroutine {
            None => self.word(0),
            Some(CoroutineLocals { poll, context, span }) => {
                self.word(1);
                self.local(*poll);
                self.local(*context);
                self.span(*span);
            }
        }
        self.count(unreached.len());
        unreached.iter().for_each(|&it| self.block(it));
        self.locals(scope_ends);
        self.count(upvars_in_place.len());
        for &(local, field) in upvars_in_place {
            self.local(local);
            self.word(field);
        }
        self.count(panic_unwinds.len());
        for &(block, statement, cleanup) in panic_unwinds {
            self.block(block);
            self.count(statement);
            self.block(cleanup);
        }
        self.locals(promoted_constants);
        self.count(unreachable_after_calls.len());
        for UnreachableAfterCall { call, next, descr, ty } in unreachable_after_calls {
            self.span(*call);
            self.span(*next);
            self.word(match *descr {
                "definition" => 0,
                "expression" => 1,
                _ => return Err("an unreachable call's description that is neither of lowering's"),
            });
            self.ty(ty);
        }
        self.count(guard_refs.len());
        for &(local, binding) in guard_refs {
            self.local(local);
            self.word(binding.into_raw().into_u32());
        }
        self.count(capture_copies.len());
        for &(local, FieldIndex(field)) in capture_copies {
            self.local(local);
            self.word(field);
        }
        self.inlined(inlined);
        Ok(())
    }

    fn owner(&mut self, owner: InferBodyId<'db>) {
        match owner {
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => {
                self.word(0);
                self.id(it);
            }
            InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(it)) => {
                self.word(1);
                self.id(it);
            }
            InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it)) => {
                self.word(2);
                self.id(it);
            }
            InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(it)) => {
                self.word(3);
                self.id(it);
            }
            InferBodyId::AnonConstId(it) => {
                self.word(4);
                self.id(it);
            }
        }
    }

    /// Sorted by binding, so equal maps write equal words whatever order they iterate in.
    fn upvar_locals(
        &mut self,
        upvars: &HashMap<BindingId, Vec<(LocalId, closure_analysis::Place)>, Global>,
    ) {
        self.count(upvars.len());
        blaze_mem::scratch::with_u32s(|keys| {
            keys.extend(upvars.keys().map(|it| it.into_raw().into_u32()));
            keys.sort_unstable();
            for &key in keys.iter() {
                let places = &upvars[&BindingId::from_raw(RawIdx::from_u32(key))];
                self.word(key);
                self.count(places.len());
                for (local, place) in places {
                    self.local(*local);
                    self.capture_place(place);
                }
            }
        });
    }

    fn capture_place(&mut self, place: &closure_analysis::Place) {
        use closure_analysis::{PlaceBase, ProjectionKind};
        let closure_analysis::Place { base_ty, base, projections } = place;
        self.ty(base_ty);
        match *base {
            PlaceBase::Rvalue => self.word(0),
            PlaceBase::StaticItem => self.word(1),
            PlaceBase::Local(binding) => {
                self.word(2);
                self.word(binding.into_raw().into_u32());
            }
            PlaceBase::Upvar { closure, var_id } => {
                self.word(3);
                self.word(closure.into_raw().into_u32());
                self.word(var_id.into_raw().into_u32());
            }
        }
        self.count(projections.len());
        for projection in projections {
            self.ty(&projection.ty);
            match projection.kind {
                ProjectionKind::Deref => self.word(0),
                ProjectionKind::Field { field_idx, variant_idx } => {
                    self.word(1);
                    self.word(field_idx);
                    self.word(variant_idx);
                }
                ProjectionKind::Index => self.word(2),
                ProjectionKind::Subslice => self.word(3),
                ProjectionKind::UnwrapUnsafeBinder => self.word(4),
            }
        }
    }

    /// Nothing: only built MIR is kept, and a built body has no inlined code, specialized
    /// calls or allocations moved into its frame. A body with any is optimized MIR, refused.
    fn inlined(&mut self, inlined: &Inlined<'db>) {
        let Inlined {
            callees,
            scopes,
            spans,
            bindings,
            promoted,
            devirtualized,
            specialized,
            moved_to_frame,
            relied,
        } = inlined;
        let why = if !callees.is_empty() {
            "a body with inlined callees, which only optimized MIR has"
        } else if !scopes.is_empty() {
            "a body with inlined scopes, which only optimized MIR has"
        } else if !spans.is_empty() {
            "a body with inlined spans, which only optimized MIR has"
        } else if !bindings.is_empty() {
            "a body with inlined bindings, which only optimized MIR has"
        } else if !promoted.is_empty() {
            "a body with inlined promoted locals, which only optimized MIR has"
        } else if !devirtualized.is_empty() {
            "a body with devirtualized calls, which only optimized MIR has"
        } else if !specialized.is_empty() {
            "a body with specialized calls, which only optimized MIR has"
        } else if *moved_to_frame != 0 {
            "a body with allocations moved into its frame, which only optimized MIR has"
        } else if !relied.is_empty() {
            "a body that relied on facts, which only optimized MIR has"
        } else {
            return;
        };
        self.refused.get_or_insert(why);
    }

    fn basic_block(&mut self, block: &BasicBlock) {
        let BasicBlock { statements, terminator, is_cleanup } = block;
        self.bool(*is_cleanup);
        self.count(statements.len());
        for Statement { kind, span } in statements {
            self.statement(kind);
            self.span(*span);
        }
        match terminator {
            None => self.word(0),
            Some(Terminator { span, kind }) => {
                self.word(1);
                self.span(*span);
                self.terminator(kind);
            }
        }
    }

    /// A span is an index of the body's store: a span holding a position in a file has no
    /// word, so such a variant fails to compile here rather than reach the bytes.
    fn span(&mut self, span: MirSpan) {
        match span {
            MirSpan::ExprId(it) => {
                self.word(0);
                self.word(it.into_raw().into_u32());
            }
            MirSpan::ExprEnd(it) => {
                self.word(1);
                self.word(it.into_raw().into_u32());
            }
            MirSpan::PatId(it) => {
                self.word(2);
                self.word(it.into_raw().into_u32());
            }
            MirSpan::BindingId(it) => {
                self.word(3);
                self.word(it.into_raw().into_u32());
            }
            MirSpan::SelfParam => self.word(4),
            MirSpan::Unknown => self.word(5),
            MirSpan::Inlined(_) => {
                self.refused.get_or_insert("an inlined span, which only optimized MIR has");
            }
        }
    }

    fn variant(&mut self, variant: VariantId) {
        match variant {
            VariantId::EnumVariantId(it) => {
                self.word(0);
                self.id(it);
            }
            VariantId::StructId(it) => {
                self.word(1);
                self.id(it);
            }
            VariantId::UnionId(it) => {
                self.word(2);
                self.id(it);
            }
        }
    }

    fn place(&mut self, place: &StoredPlace) {
        self.local(place.local);
        let projection = place.projection.as_slice();
        self.count(projection.len());
        for elem in projection {
            match *elem {
                ProjectionElem::Deref => self.word(0),
                ProjectionElem::Field(FieldIndex(field)) => {
                    self.word(1);
                    self.word(field);
                }
                ProjectionElem::Index(local) => {
                    self.word(2);
                    self.local(local);
                }
                ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                    self.word(3);
                    self.u64(offset);
                    self.u64(min_length);
                    self.bool(from_end);
                }
                ProjectionElem::Subslice { from, to } => {
                    self.word(4);
                    self.u64(from);
                    self.u64(to);
                }
                ProjectionElem::Downcast(variant) => {
                    self.word(5);
                    self.variant(variant);
                }
                ProjectionElem::ReflectedField(ComptimeBinder(universe)) => {
                    self.word(6);
                    self.word(universe);
                }
            }
        }
    }

    fn opt_place(&mut self, place: &Option<StoredPlace>) {
        match place {
            None => self.word(0),
            Some(place) => {
                self.word(1);
                self.place(place);
            }
        }
    }

    fn operand(&mut self, operand: &Operand) {
        let Operand { kind, span } = operand;
        match span {
            None => self.word(0),
            Some(span) => {
                self.word(1);
                self.span(*span);
            }
        }
        match kind {
            OperandKind::Copy(place) => {
                self.word(0);
                self.place(place);
            }
            OperandKind::Move(place) => {
                self.word(1);
                self.place(place);
            }
            OperandKind::Constant { konst, ty } => {
                self.word(2);
                self.konst(konst);
                self.ty(ty);
            }
            OperandKind::Allocation { allocation } => match (*span, self.locations) {
                (Some(MirSpan::ExprId(expr)), Some(locations)) if locations.has(expr) => {
                    let ty = allocation.as_ref().ty;
                    if locations.constant(expr, ty) != Some(*allocation) {
                        self.refused.get_or_insert(
                            "a location literal's constant that is not what its literal makes",
                        );
                    }
                    self.word(5);
                    self.ty(&ty.store());
                }
                _ => {
                    self.word(3);
                    self.allocation(allocation);
                }
            },
            OperandKind::Static(it) => {
                self.word(4);
                self.id(*it);
            }
        }
    }

    fn opt_operand(&mut self, operand: &Option<Operand>) {
        match operand {
            None => self.word(0),
            Some(operand) => {
                self.word(1);
                self.operand(operand);
            }
        }
    }

    fn operands(&mut self, operands: &[Operand]) {
        self.count(operands.len());
        operands.iter().for_each(|it| self.operand(it));
    }

    fn bin_op(&mut self, op: &BinOp) {
        self.word(match op {
            BinOp::Add => 0,
            BinOp::Sub => 1,
            BinOp::Mul => 2,
            BinOp::Div => 3,
            BinOp::Rem => 4,
            BinOp::BitXor => 5,
            BinOp::BitAnd => 6,
            BinOp::BitOr => 7,
            BinOp::Shl => 8,
            BinOp::Shr => 9,
            BinOp::Eq => 10,
            BinOp::Lt => 11,
            BinOp::Le => 12,
            BinOp::Ne => 13,
            BinOp::Ge => 14,
            BinOp::Gt => 15,
            BinOp::Offset => 16,
            BinOp::AddWithOverflow => 17,
            BinOp::SubWithOverflow => 18,
            BinOp::MulWithOverflow => 19,
        });
    }

    fn borrow_kind(&mut self, kind: BorrowKind) {
        self.word(match kind {
            BorrowKind::Shared => 0,
            BorrowKind::Shallow => 1,
            BorrowKind::Mut { kind: MutBorrowKind::ClosureCapture } => 2,
            BorrowKind::Mut { kind: MutBorrowKind::Default } => 3,
            BorrowKind::Mut { kind: MutBorrowKind::TwoPhasedBorrow } => 4,
        });
    }

    fn cast_kind(&mut self, kind: &CastKind) {
        match kind {
            CastKind::PointerExposeAddress => self.word(0),
            CastKind::PointerFromExposedAddress => self.word(1),
            CastKind::PtrToPtr => self.word(2),
            CastKind::PointerCoercion(cast) => {
                self.word(3);
                self.word(match cast {
                    PointerCast::ReifyFnPointer => 0,
                    PointerCast::UnsafeFnPointer => 1,
                    PointerCast::ClosureFnPointer(Safety::Unsafe) => 2,
                    PointerCast::ClosureFnPointer(Safety::Safe) => 3,
                    PointerCast::MutToConstPointer => 4,
                    PointerCast::ArrayToPointer => 5,
                    PointerCast::Unsize => 6,
                });
            }
            CastKind::DynStar => self.word(4),
            CastKind::IntToInt => self.word(5),
            CastKind::FloatToInt => self.word(6),
            CastKind::FloatToFloat => self.word(7),
            CastKind::IntToFloat => self.word(8),
            CastKind::FnPtrToPtr => self.word(9),
            CastKind::Transmute => self.word(10),
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue) {
        match rvalue {
            Rvalue::Use(operand) => {
                self.word(0);
                self.operand(operand);
            }
            Rvalue::Repeat(operand, len) => {
                self.word(1);
                self.operand(operand);
                self.konst(len);
            }
            Rvalue::Ref(kind, place) => {
                self.word(2);
                self.borrow_kind(*kind);
                self.place(place);
            }
            Rvalue::Len(place) => {
                self.word(3);
                self.place(place);
            }
            Rvalue::Cast(kind, operand, ty) => {
                self.word(4);
                self.cast_kind(kind);
                self.operand(operand);
                self.ty(ty);
            }
            Rvalue::BinaryOp(op, a, b) => {
                self.word(5);
                self.bin_op(op);
                self.operand(a);
                self.operand(b);
            }
            Rvalue::CheckedBinaryOp(op, a, b) => {
                self.word(6);
                self.bin_op(op);
                self.operand(a);
                self.operand(b);
            }
            Rvalue::UnaryOp(op, operand) => {
                self.word(7);
                self.word(match op {
                    UnOp::Not => 0,
                    UnOp::Neg => 1,
                });
                self.operand(operand);
            }
            Rvalue::Discriminant(place) => {
                self.word(8);
                self.place(place);
            }
            Rvalue::Aggregate(kind, operands) => {
                self.word(9);
                match kind {
                    AggregateKind::Array(ty) => {
                        self.word(0);
                        self.ty(ty);
                    }
                    AggregateKind::Tuple(ty) => {
                        self.word(1);
                        self.ty(ty);
                    }
                    AggregateKind::Adt(variant, args) => {
                        self.word(2);
                        self.variant(*variant);
                        self.args(args);
                    }
                    AggregateKind::Union(union, FieldId { parent, local_id }) => {
                        self.word(3);
                        self.id(*union);
                        self.variant(*parent);
                        self.word(local_id.into_raw().into_u32());
                    }
                    AggregateKind::Closure(ty) => {
                        self.word(4);
                        self.ty(ty);
                    }
                }
                self.operands(operands);
            }
            Rvalue::CopyForDeref(place) => {
                self.word(10);
                self.place(place);
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    fn statement(&mut self, kind: &StatementKind) {
        match kind {
            StatementKind::Assign(place, rvalue) => {
                self.word(0);
                self.place(place);
                self.rvalue(rvalue);
            }
            StatementKind::FakeRead(place) => {
                self.word(1);
                self.place(place);
            }
            StatementKind::Deinit(place) => {
                self.word(2);
                self.place(place);
            }
            StatementKind::StorageLive(local) => {
                self.word(3);
                self.local(*local);
            }
            StatementKind::StorageDead(local) => {
                self.word(4);
                self.local(*local);
            }
            StatementKind::InlineAsm(asm) => {
                self.word(5);
                self.inline_asm(asm);
            }
            StatementKind::Nop => self.word(6),
        }
    }

    fn inline_asm(&mut self, asm: &InlineAsm) {
        let InlineAsm { template, operands, options, taken, naked, clobber_abi } = asm;
        self.count(template.len());
        for piece in template {
            match piece {
                AsmPiece::Lit(text) => {
                    self.word(0);
                    self.text(text);
                }
                AsmPiece::Operand { index, modifier } => {
                    self.word(1);
                    self.count(*index);
                    self.word(modifier.map_or(0, |it| u32::from(it) + 1));
                }
            }
        }
        self.count(operands.len());
        for operand in operands {
            match operand {
                AsmOperand::Reg { reg, input, output, written } => {
                    self.word(0);
                    match reg {
                        InlineAsmRegOrRegClass::Reg(name) => {
                            self.word(0);
                            self.text(name.as_str());
                        }
                        InlineAsmRegOrRegClass::RegClass(name) => {
                            self.word(1);
                            self.text(name.as_str());
                        }
                    }
                    self.opt_operand(input);
                    self.opt_place(output);
                    self.bool(*written);
                }
                AsmOperand::Const(operand) => {
                    self.word(1);
                    self.operand(operand);
                }
                AsmOperand::Label => self.word(2),
                AsmOperand::Sym(operand) => {
                    self.word(3);
                    self.operand(operand);
                }
            }
        }
        self.word(u32::from(options.bits()));
        self.opt_place(taken);
        self.bool(*naked);
        self.bool(*clobber_abi);
    }

    fn assert_kind(&mut self, msg: &AssertKind) {
        match msg {
            AssertKind::BoundsCheck { len, index } => {
                self.word(0);
                self.operand(len);
                self.operand(index);
            }
            AssertKind::Overflow(op, a, b) => {
                self.word(1);
                self.bin_op(op);
                self.operand(a);
                self.operand(b);
            }
            AssertKind::OverflowNeg(a) => {
                self.word(2);
                self.operand(a);
            }
            AssertKind::DivisionByZero(a) => {
                self.word(3);
                self.operand(a);
            }
            AssertKind::RemainderByZero(a) => {
                self.word(4);
                self.operand(a);
            }
            AssertKind::MisalignedPointerDereference { required, found } => {
                self.word(5);
                self.operand(required);
                self.operand(found);
            }
            AssertKind::NullPointerDereference => self.word(6),
            AssertKind::NullReferenceConstructed => self.word(7),
            AssertKind::InvalidEnumConstruction(a) => {
                self.word(8);
                self.operand(a);
            }
        }
    }

    fn terminator(&mut self, kind: &TerminatorKind) {
        match kind {
            TerminatorKind::Goto { target } => {
                self.word(0);
                self.block(*target);
            }
            TerminatorKind::SwitchInt { discr, targets: SwitchTargets { values, targets } } => {
                self.word(1);
                self.operand(discr);
                self.count(values.len());
                values.iter().for_each(|&it| self.u128(it));
                self.count(targets.len());
                targets.iter().for_each(|&it| self.block(it));
            }
            TerminatorKind::UnwindResume => self.word(2),
            TerminatorKind::Abort => self.word(3),
            TerminatorKind::Return => self.word(4),
            TerminatorKind::Unreachable => self.word(5),
            TerminatorKind::Drop { place, target, unwind } => {
                self.word(6);
                self.place(place);
                self.block(*target);
                self.opt_block(*unwind);
            }
            TerminatorKind::DropAndReplace { place, value, target, unwind } => {
                self.word(7);
                self.place(place);
                self.operand(value);
                self.block(*target);
                self.opt_block(*unwind);
            }
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
                cleanup,
                from_hir_call,
                intrinsic,
            } => {
                self.word(8);
                self.operand(func);
                self.operands(args);
                self.place(destination);
                self.opt_block(*target);
                self.opt_block(*cleanup);
                self.bool(*from_hir_call);
                self.word(intrinsic.map_or(0, |it| u32::from(it.index()) + 1));
            }
            TerminatorKind::TailCall { func, args, fn_span } => {
                self.word(9);
                self.operand(func);
                self.operands(args);
                self.span(*fn_span);
            }
            TerminatorKind::Assert { cond, expected, msg, target, cleanup } => {
                self.word(10);
                self.operand(cond);
                self.bool(*expected);
                self.assert_kind(msg);
                self.block(*target);
                self.opt_block(*cleanup);
            }
            TerminatorKind::Yield { value, resume, resume_arg, drop } => {
                self.word(11);
                self.operand(value);
                self.block(*resume);
                self.place(resume_arg);
                self.opt_block(*drop);
            }
            TerminatorKind::CoroutineDrop => self.word(12),
            TerminatorKind::FalseEdge { real_target, imaginary_target } => {
                self.word(13);
                self.block(*real_target);
                self.block(*imaginary_target);
            }
            TerminatorKind::FalseUnwind { real_target, unwind } => {
                self.word(14);
                self.block(*real_target);
                self.opt_block(*unwind);
            }
            TerminatorKind::ComptimeFor {
                binder: ComptimeBinder(universe),
                domain,
                body,
                after,
            } => {
                self.word(15);
                self.word(*universe);
                self.ty(domain);
                self.block(*body);
                self.block(*after);
            }
            TerminatorKind::ComptimeNext { head } => {
                self.word(16);
                self.block(*head);
            }
        }
    }
}

struct Decode<'a, 'db> {
    db: &'db Db,
    words: &'a [u32],
    /// Where the words column starts in the buffer, so an error names the byte of its word.
    words_at: u64,
    /// The next word to read.
    at: usize,
    text: &'a [u8],
    tys: TyTableReader<'a, 'db>,
    allocs: &'a [Rel<AllocationRec>],
    /// Each allocation once it is read, by its index.
    made: Vec<Option<StoredAllocation>>,
    buffer: &'a [u8],
    /// How many locals and blocks the body has: every one the words name is checked against
    /// them, so the body read never indexes past its arenas.
    locals: u32,
    blocks: u32,
    /// How many expressions, patterns and bindings the body's store has: every one the
    /// words name is checked against them, so the body read never indexes past the store (a
    /// binding the body maps to a local is also a slot of a dense map up to it, so one past
    /// them is refused before it sizes that map).
    exprs: u32,
    pats: u32,
    bindings: u32,
    locations: Locations<'a>,
}

type R<T> = Result<T, FormatError>;

fn idx<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

impl<'a, 'db> Decode<'a, 'db> {
    /// Where the words column's word `word` is in the buffer, in bytes.
    fn byte(&self, word: usize) -> u64 {
        self.words_at + 4 * word as u64
    }

    /// An error at the word just read, by its byte in the buffer.
    fn invalid(&self, what: &'static str) -> FormatError {
        self.invalid_at(self.at.saturating_sub(1), what)
    }

    /// An error at the word `word` (a value of several words, by its first), by its byte in
    /// the buffer.
    fn invalid_at(&self, word: usize, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.byte(word), what }
    }

    #[inline]
    fn word(&mut self) -> R<u32> {
        match self.words.get(self.at) {
            Some(&word) => {
                self.at += 1;
                Ok(word)
            }
            None => Err(self.invalid_at(self.at, "a MIR body cut short")),
        }
    }

    fn bool(&mut self) -> R<bool> {
        match self.word()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(self.invalid("a bool that is neither 0 nor 1")),
        }
    }

    fn u64(&mut self) -> R<u64> {
        let low = self.word()?;
        let high = self.word()?;
        Ok(u64::from(low) | (u64::from(high) << 32))
    }

    fn u128(&mut self) -> R<u128> {
        let low = self.u64()?;
        let high = self.u64()?;
        Ok(u128::from(low) | (u128::from(high) << 64))
    }

    /// An error at the word just read, by its byte in the buffer: `what`, `found`, is past
    /// its bound `bound`.
    fn past(
        &self,
        what: &'static str,
        found: impl Into<u64>,
        bound: impl Into<u64>,
    ) -> FormatError {
        self.past_at(self.at.saturating_sub(1), what, found, bound)
    }

    /// [`Self::past`] of the word `word`.
    fn past_at(
        &self,
        word: usize,
        what: &'static str,
        found: impl Into<u64>,
        bound: impl Into<u64>,
    ) -> FormatError {
        FormatError::Bound { at: self.byte(word), what, found: found.into(), bound: bound.into() }
    }

    /// The length of the list `what` names, whose items are a word or more each: never more
    /// than the words left, so a list is allocated only as long as the buffer can fill.
    fn count(&mut self, what: &'static str) -> R<usize> {
        self.count_of(what, 1)
    }

    /// The length of the list `what` names, whose items are `words` words each at least: never
    /// more items than the words left can hold.
    fn count_of(&mut self, what: &'static str, words: usize) -> R<usize> {
        let count = self.word()?;
        let fits = (self.words.len() - self.at) / words;
        match count as usize <= fits {
            true => Ok(count as usize),
            false => Err(self.past(what, count, fits as u64)),
        }
    }

    /// An index of a table of `len` rows, `what` naming the field and the table.
    fn below<T>(&mut self, what: &'static str, len: u32) -> R<Idx<T>> {
        match self.word()? {
            raw if raw < len => Ok(idx(raw)),
            raw => Err(self.past(what, raw, len)),
        }
    }

    /// An expression of the body's store.
    fn expr(&mut self, what: &'static str) -> R<ExprId> {
        self.below(what, self.exprs)
    }

    /// A pattern of the body's store.
    fn pat(&mut self, what: &'static str) -> R<PatId> {
        self.below(what, self.pats)
    }

    /// A binding of the body's store.
    fn binding(&mut self, what: &'static str) -> R<BindingId> {
        self.below(what, self.bindings)
    }

    /// A `for const` loop's binder, `what` naming the field: a universe index, at most
    /// [`UniverseIndex::MAX_AS_U32`] (`ComptimeBinder::ty` makes the universe of it, which
    /// asserts that).
    fn binder(&mut self, what: &'static str) -> R<ComptimeBinder> {
        let universes = u64::from(UniverseIndex::MAX_AS_U32) + 1;
        match self.word()? {
            raw if u64::from(raw) < universes => Ok(ComptimeBinder(raw)),
            raw => Err(self.past(what, raw, universes)),
        }
    }

    /// An index of one of the body's own tables already read, of `len` rows.
    fn own(&mut self, what: &'static str, len: usize) -> R<u32> {
        let raw = self.word()?;
        match (raw as usize) < len {
            true => Ok(raw),
            false => Err(self.past(what, raw, len as u64)),
        }
    }

    fn id<T: Relocate>(&mut self) -> R<T> {
        let start = self.at;
        let bits = self.u64()?;
        id_of(bits).map_err(|refused| refused.at(self.byte(start)))
    }

    fn ty(&mut self) -> R<StoredTy> {
        let place = self.word()?;
        Ok(self.tys.ty(place)?.store())
    }

    fn konst(&mut self) -> R<StoredConst> {
        let place = self.word()?;
        Ok(self.tys.konst(place)?.store())
    }

    fn args(&mut self) -> R<StoredGenericArgs> {
        let place = self.word()?;
        Ok(self.tys.args(place)?.store())
    }

    fn allocation(&mut self) -> R<StoredAllocation> {
        let index = self.word()? as usize;
        if let Some(Some(made)) = self.made.get(index) {
            return Ok(*made);
        }
        let (allocs, buffer): (&'a [Rel<AllocationRec>], &'a [u8]) = (self.allocs, self.buffer);
        let rel = allocs.get(index).ok_or_else(|| {
            self.past(
                "an allocation operand's index, below the body's allocation count",
                index as u64,
                allocs.len() as u64,
            )
        })?;
        let record =
            rel.follow(buffer)?.ok_or_else(|| self.invalid("an allocation of no record"))?;
        let made = read_allocation(record, buffer, &self.tys)?.store();
        self.made[index] = Some(made);
        Ok(made)
    }

    fn text(&mut self) -> R<&'a str> {
        let word = self.at;
        let start = self.word()? as usize;
        let len = self.word()? as usize;
        let text: &'a [u8] = self.text;
        let bytes = start
            .checked_add(len)
            .and_then(|end| text.get(start..end))
            .ok_or_else(|| self.invalid_at(word, "text past the body's"))?;
        std::str::from_utf8(bytes).map_err(|_| self.invalid_at(word, "text that is not UTF-8"))
    }

    fn local(&mut self) -> R<LocalId> {
        let raw = self.word()?;
        match raw < self.locals {
            true => Ok(idx(raw)),
            false => Err(self.past("a local, below the body's local count", raw, self.locals)),
        }
    }

    fn block(&mut self) -> R<BasicBlockId> {
        let raw = self.word()?;
        match raw < self.blocks {
            true => Ok(idx(raw)),
            false => Err(self.past("a block, below the body's block count", raw, self.blocks)),
        }
    }

    fn opt_block(&mut self) -> R<Option<BasicBlockId>> {
        match self.word()? {
            0 => Ok(None),
            raw if raw - 1 < self.blocks => Ok(Some(idx(raw - 1))),
            raw => Err(self.past(
                "an optional block, below the body's block count",
                raw - 1,
                self.blocks,
            )),
        }
    }

    fn locals(&mut self, what: &'static str) -> R<Vec<LocalId>> {
        let len = self.count(what)?;
        // alloc: the body's own list, at its length
        let mut locals = Vec::with_capacity(len);
        for _ in 0..len {
            locals.push(self.local()?);
        }
        Ok(locals)
    }

    /// The list `what` names, each item read by `item`.
    fn list<T>(&mut self, what: &'static str, item: impl FnMut(&mut Self) -> R<T>) -> R<Vec<T>> {
        self.list_of(what, 1, item)
    }

    /// The list `what` names, of items `words` words each at least, each read by `item`.
    fn list_of<T>(
        &mut self,
        what: &'static str,
        words: usize,
        mut item: impl FnMut(&mut Self) -> R<T>,
    ) -> R<Vec<T>> {
        let len = self.count_of(what, words)?;
        // alloc: the body's own list, at its length
        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            items.push(item(self)?);
        }
        Ok(items)
    }

    fn body(&mut self) -> R<MirBody<'db>> {
        let owner = self.owner()?;
        let phase = match self.word()? {
            0 => MirPhase::Built,
            1 => MirPhase::Runtime,
            _ => return Err(self.invalid("a MIR phase of no kind")),
        };
        let inherits_overflow_checks = self.bool()?;
        let local_count = self.count("the body's locals")?;
        let mut locals = Arena::with_capacity(local_count);
        for _ in 0..local_count {
            locals.alloc(Local { ty: self.ty()? });
        }
        self.locals = local_count as u32;
        // A block is three words at least: its cleanup flag, its statements' count, its
        // terminator's tag.
        let block_count = self.count_of("the body's blocks", 3)?;
        self.blocks = block_count as u32;
        let start_block = self.block()?;
        let mut basic_blocks = Arena::with_capacity(block_count);
        for _ in 0..block_count {
            let block = self.basic_block()?;
            basic_blocks.alloc(block);
        }
        let binding_count = self.count_of("the body's binding locals", 2)?;
        let mut binding_locals = ArenaMap::with_capacity(binding_count);
        let mut last = None;
        for _ in 0..binding_count {
            let binding = self.word()?;
            // The map is dense up to its last binding: one past the store's would size it by
            // a damaged word, up to 2^32 slots.
            if binding >= self.bindings {
                return Err(self.past(
                    "a binding mapped to a local, below the body's store's binding count",
                    binding,
                    self.bindings,
                ));
            }
            if last.is_some_and(|last| binding <= last) {
                return Err(self.invalid("bindings out of order"));
            }
            last = Some(binding);
            binding_locals.insert(idx(binding), self.local()?);
        }
        let upvar_locals = self.upvar_locals()?;
        let param_locals = self.locals("the body's parameter locals")?;
        let promoted_locals = self.list_of("the body's promoted locals", 2, |it| {
            Ok((
                it.local()?,
                it.expr("a promoted local's expression, below the store's expression count")?,
            ))
        })?;
        let closures = self.list_of("the body's closures", 2, |it| it.id())?;
        let coroutine = match self.word()? {
            0 => None,
            1 => Some(CoroutineLocals {
                poll: self.local()?,
                context: self.local()?,
                span: self.span()?,
            }),
            _ => return Err(self.invalid("a coroutine's locals of no kind")),
        };
        let unreached = self.list("the body's unreached blocks", |it| it.block())?;
        let scope_ends = self.locals("the body's scope ends")?;
        let upvars_in_place =
            self.list_of("the body's upvars in place", 2, |it| Ok((it.local()?, it.word()?)))?;
        let panic_unwinds = self.list_of("the body's panic unwinds", 3, |it| {
            let block = it.block()?;
            let statements = basic_blocks[block].statements.len();
            let statement = it
                .own("a panic unwind's statement, below its block's statement count", statements)?;
            Ok((block, statement as usize, it.block()?))
        })?;
        let promoted_constants = self.locals("the body's promoted constants")?;
        // Two spans of a word at least, a description, a type.
        let unreachable_after_calls = self.list_of("the body's unreachable calls", 4, |it| {
            let call = it.span()?;
            let next = it.span()?;
            let descr = match it.word()? {
                0 => "definition",
                1 => "expression",
                _ => return Err(it.invalid("an unreachable call's description of no kind")),
            };
            Ok(UnreachableAfterCall { call, next, descr, ty: it.ty()? })
        })?;
        let guard_refs = self.list_of("the body's guard references", 2, |it| {
            Ok((
                it.local()?,
                it.binding("a guard reference's binding, below the store's binding count")?,
            ))
        })?;
        let capture_copies = self.list_of("the body's capture copies", 2, |it| {
            Ok((it.local()?, FieldIndex(it.word()?)))
        })?;
        // Built MIR has no inlined code (`Encode::inlined`).
        let inlined = Inlined::default();
        Ok(MirBody {
            basic_blocks,
            locals,
            start_block,
            owner,
            binding_locals,
            upvar_locals,
            param_locals,
            promoted_locals,
            closures,
            coroutine,
            unreached,
            scope_ends,
            inherits_overflow_checks,
            phase,
            upvars_in_place,
            panic_unwinds,
            promoted_constants,
            unreachable_after_calls,
            guard_refs,
            capture_copies,
            inlined,
        })
    }

    fn owner(&mut self) -> R<InferBodyId<'db>> {
        Ok(match self.word()? {
            0 => InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(self.id()?)),
            1 => InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(self.id()?)),
            2 => InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(self.id()?)),
            3 => InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(self.id()?)),
            4 => InferBodyId::AnonConstId(self.id()?),
            _ => return Err(self.invalid("a body owner of no kind")),
        })
    }

    fn upvar_locals(
        &mut self,
    ) -> R<HashMap<BindingId, Vec<(LocalId, closure_analysis::Place)>, Global>> {
        // A captured binding and its places' count.
        let len = self.count_of("the body's captured bindings", 2)?;
        // alloc: the body's own map, at its length
        let mut upvars = HashMap::with_capacity_in(len, Global);
        let mut last = None;
        for _ in 0..len {
            let binding = self.binding("a captured binding, below the store's binding count")?;
            let raw = binding.into_raw().into_u32();
            if last.is_some_and(|last| raw <= last) {
                return Err(self.invalid("captured bindings out of order"));
            }
            last = Some(raw);
            // A local, then a place: its base type, its base's tag, its projections' count.
            let places = self.list_of("a captured binding's places", 4, |it| {
                Ok((it.local()?, it.capture_place()?))
            })?;
            upvars.insert(binding, places);
        }
        Ok(upvars)
    }

    fn capture_place(&mut self) -> R<closure_analysis::Place> {
        use closure_analysis::{PlaceBase, Projection, ProjectionKind};
        let base_ty = self.ty()?;
        let base = match self.word()? {
            0 => PlaceBase::Rvalue,
            1 => PlaceBase::StaticItem,
            2 => PlaceBase::Local(
                self.binding("a captured place's binding, below the store's binding count")?,
            ),
            3 => PlaceBase::Upvar {
                closure: self
                    .expr("a captured upvar's closure, below the store's expression count")?,
                var_id: self
                    .binding("a captured upvar's binding, below the store's binding count")?,
            },
            _ => return Err(self.invalid("a captured place's base of no kind")),
        };
        let projections = self.list_of("a captured place's projections", 2, |it| {
            let ty = it.ty()?;
            let kind = match it.word()? {
                0 => ProjectionKind::Deref,
                1 => ProjectionKind::Field { field_idx: it.word()?, variant_idx: it.word()? },
                2 => ProjectionKind::Index,
                3 => ProjectionKind::Subslice,
                4 => ProjectionKind::UnwrapUnsafeBinder,
                _ => return Err(it.invalid("a captured place's projection of no kind")),
            };
            Ok(Projection { ty, kind })
        })?;
        Ok(closure_analysis::Place { base_ty, base, projections: projections.into_boxed_slice() })
    }

    fn basic_block(&mut self) -> R<BasicBlock> {
        let is_cleanup = self.bool()?;
        // A statement's tag and a span of a word at least.
        let statements = self.list_of("a block's statements", 2, |it| {
            let kind = it.statement()?;
            Ok(Statement { kind, span: it.span()? })
        })?;
        let terminator = match self.word()? {
            0 => None,
            1 => {
                let span = self.span()?;
                Some(Terminator { span, kind: self.terminator()? })
            }
            _ => return Err(self.invalid("a terminator of no kind")),
        };
        Ok(BasicBlock { statements, terminator, is_cleanup })
    }

    /// A span of the body, of its store: never an inlined span, which only optimized MIR has.
    fn span(&mut self) -> R<MirSpan> {
        Ok(match self.word()? {
            0 => MirSpan::ExprId(
                self.expr("a span's expression, below the store's expression count")?,
            ),
            1 => MirSpan::ExprEnd(
                self.expr("a span's expression end, below the store's expression count")?,
            ),
            2 => MirSpan::PatId(self.pat("a span's pattern, below the store's pattern count")?),
            3 => MirSpan::BindingId(
                self.binding("a span's binding, below the store's binding count")?,
            ),
            4 => MirSpan::SelfParam,
            5 => MirSpan::Unknown,
            6 => {
                return Err(
                    self.invalid("an inlined span, which only optimized MIR has and is never kept")
                );
            }
            _ => return Err(self.invalid("a span of no kind")),
        })
    }

    fn variant(&mut self) -> R<VariantId> {
        Ok(match self.word()? {
            0 => VariantId::EnumVariantId(self.id()?),
            1 => VariantId::StructId(self.id()?),
            2 => VariantId::UnionId(self.id()?),
            _ => return Err(self.invalid("a variant of no kind")),
        })
    }

    fn place(&mut self) -> R<StoredPlace> {
        let local = self.local()?;
        let len = self.count("a place's projections")?;
        let mut elems: SmallVec<[PlaceElem; 8]> = SmallVec::with_capacity(len);
        for _ in 0..len {
            elems.push(match self.word()? {
                0 => ProjectionElem::Deref,
                1 => ProjectionElem::Field(FieldIndex(self.word()?)),
                2 => ProjectionElem::Index(self.local()?),
                3 => ProjectionElem::ConstantIndex {
                    offset: self.u64()?,
                    min_length: self.u64()?,
                    from_end: self.bool()?,
                },
                4 => ProjectionElem::Subslice { from: self.u64()?, to: self.u64()? },
                5 => ProjectionElem::Downcast(self.variant()?),
                6 => {
                    let what = "a reflected field's binder, below the universe count";
                    ProjectionElem::ReflectedField(self.binder(what)?)
                }
                _ => return Err(self.invalid("a projection of no kind")),
            });
        }
        Ok(Place { local, projection: Projection::new_from_slice(&elems) }.store())
    }

    fn opt_place(&mut self) -> R<Option<StoredPlace>> {
        match self.word()? {
            0 => Ok(None),
            1 => self.place().map(Some),
            _ => Err(self.invalid("an optional place of no kind")),
        }
    }

    fn operand(&mut self) -> R<Operand> {
        let span = match self.word()? {
            0 => None,
            1 => Some(self.span()?),
            _ => return Err(self.invalid("an operand's span of no kind")),
        };
        let kind = match self.word()? {
            0 => OperandKind::Copy(self.place()?),
            1 => OperandKind::Move(self.place()?),
            2 => OperandKind::Constant { konst: self.konst()?, ty: self.ty()? },
            3 => OperandKind::Allocation { allocation: self.allocation()? },
            4 => OperandKind::Static(self.id()?),
            5 => OperandKind::Allocation { allocation: self.slot(span)? },
            _ => return Err(self.invalid("an operand of no kind")),
        };
        Ok(Operand { kind, span })
    }

    /// The constant of the location literal `span` is, of the type the next word names, as
    /// its literal makes it now.
    fn slot(&mut self, span: Option<MirSpan>) -> R<StoredAllocation> {
        let ty = self.ty()?;
        let Some(MirSpan::ExprId(expr)) = span else {
            return Err(self.invalid("a location slot with no expression"));
        };
        let locations = self.locations;
        if !locations.has(expr) {
            return Err(
                self.invalid("a location slot of an expression that is no location literal now")
            );
        }
        locations
            .constant(expr, ty.as_ref())
            .ok_or_else(|| self.invalid("a location slot whose literal makes no constant now"))
    }

    fn opt_operand(&mut self) -> R<Option<Operand>> {
        match self.word()? {
            0 => Ok(None),
            1 => self.operand().map(Some),
            _ => Err(self.invalid("an optional operand of no kind")),
        }
    }

    fn operands(&mut self) -> R<Box<[Operand]>> {
        // An operand is three words at least: its span's tag, its kind's tag, one more.
        self.list_of("a list of operands", 3, |it| it.operand()).map(Vec::into_boxed_slice)
    }

    fn bin_op(&mut self) -> R<BinOp> {
        Ok(match self.word()? {
            0 => BinOp::Add,
            1 => BinOp::Sub,
            2 => BinOp::Mul,
            3 => BinOp::Div,
            4 => BinOp::Rem,
            5 => BinOp::BitXor,
            6 => BinOp::BitAnd,
            7 => BinOp::BitOr,
            8 => BinOp::Shl,
            9 => BinOp::Shr,
            10 => BinOp::Eq,
            11 => BinOp::Lt,
            12 => BinOp::Le,
            13 => BinOp::Ne,
            14 => BinOp::Ge,
            15 => BinOp::Gt,
            16 => BinOp::Offset,
            17 => BinOp::AddWithOverflow,
            18 => BinOp::SubWithOverflow,
            19 => BinOp::MulWithOverflow,
            _ => return Err(self.invalid("a binary operator of no kind")),
        })
    }

    fn borrow_kind(&mut self) -> R<BorrowKind> {
        Ok(match self.word()? {
            0 => BorrowKind::Shared,
            1 => BorrowKind::Shallow,
            2 => BorrowKind::Mut { kind: MutBorrowKind::ClosureCapture },
            3 => BorrowKind::Mut { kind: MutBorrowKind::Default },
            4 => BorrowKind::Mut { kind: MutBorrowKind::TwoPhasedBorrow },
            _ => return Err(self.invalid("a borrow of no kind")),
        })
    }

    fn cast_kind(&mut self) -> R<CastKind> {
        Ok(match self.word()? {
            0 => CastKind::PointerExposeAddress,
            1 => CastKind::PointerFromExposedAddress,
            2 => CastKind::PtrToPtr,
            3 => CastKind::PointerCoercion(match self.word()? {
                0 => PointerCast::ReifyFnPointer,
                1 => PointerCast::UnsafeFnPointer,
                2 => PointerCast::ClosureFnPointer(Safety::Unsafe),
                3 => PointerCast::ClosureFnPointer(Safety::Safe),
                4 => PointerCast::MutToConstPointer,
                5 => PointerCast::ArrayToPointer,
                6 => PointerCast::Unsize,
                _ => return Err(self.invalid("a pointer coercion of no kind")),
            }),
            4 => CastKind::DynStar,
            5 => CastKind::IntToInt,
            6 => CastKind::FloatToInt,
            7 => CastKind::FloatToFloat,
            8 => CastKind::IntToFloat,
            9 => CastKind::FnPtrToPtr,
            10 => CastKind::Transmute,
            _ => return Err(self.invalid("a cast of no kind")),
        })
    }

    fn rvalue(&mut self) -> R<Rvalue> {
        Ok(match self.word()? {
            0 => Rvalue::Use(self.operand()?),
            1 => Rvalue::Repeat(self.operand()?, self.konst()?),
            2 => Rvalue::Ref(self.borrow_kind()?, self.place()?),
            3 => Rvalue::Len(self.place()?),
            4 => Rvalue::Cast(self.cast_kind()?, self.operand()?, self.ty()?),
            5 => Rvalue::BinaryOp(self.bin_op()?, self.operand()?, self.operand()?),
            6 => Rvalue::CheckedBinaryOp(self.bin_op()?, self.operand()?, self.operand()?),
            7 => {
                let op = match self.word()? {
                    0 => UnOp::Not,
                    1 => UnOp::Neg,
                    _ => return Err(self.invalid("a unary operator of no kind")),
                };
                Rvalue::UnaryOp(op, self.operand()?)
            }
            8 => Rvalue::Discriminant(self.place()?),
            9 => {
                let kind = match self.word()? {
                    0 => AggregateKind::Array(self.ty()?),
                    1 => AggregateKind::Tuple(self.ty()?),
                    2 => AggregateKind::Adt(self.variant()?, self.args()?),
                    3 => {
                        let union = self.id()?;
                        let parent = self.variant()?;
                        match parent {
                            VariantId::UnionId(it) if it == union => {}
                            VariantId::UnionId(_) => {
                                return Err(
                                    self.invalid("a union aggregate's field of another union")
                                );
                            }
                            VariantId::StructId(_) => {
                                let what = "a union aggregate's field of a struct, not its union";
                                return Err(self.invalid(what));
                            }
                            VariantId::EnumVariantId(_) => {
                                let what =
                                    "a union aggregate's field of an enum variant, not its union";
                                return Err(self.invalid(what));
                            }
                        }
                        // A query, outside the relocating scope (`ids::relocating`'s contract):
                        // the union's fields are hir-def's and name no id of the record.
                        let fields =
                            crate::check_cache::outside(|| parent.fields(self.db).fields().len());
                        let fields = table_len(fields);
                        let local_id: LocalFieldId = self.below(
                            "a union aggregate's field, below its union's field count",
                            fields,
                        )?;
                        AggregateKind::Union(union, FieldId { parent, local_id })
                    }
                    4 => AggregateKind::Closure(self.ty()?),
                    _ => return Err(self.invalid("an aggregate of no kind")),
                };
                Rvalue::Aggregate(kind, self.operands()?)
            }
            10 => Rvalue::CopyForDeref(self.place()?),
            _ => return Err(self.invalid("an rvalue of no kind")),
        })
    }

    fn statement(&mut self) -> R<StatementKind> {
        Ok(match self.word()? {
            0 => StatementKind::Assign(self.place()?, self.rvalue()?),
            1 => StatementKind::FakeRead(self.place()?),
            2 => StatementKind::Deinit(self.place()?),
            3 => StatementKind::StorageLive(self.local()?),
            4 => StatementKind::StorageDead(self.local()?),
            // alloc: the body's own `asm!` block
            5 => StatementKind::InlineAsm(Box::new(self.inline_asm()?)),
            6 => StatementKind::Nop,
            _ => return Err(self.invalid("a statement of no kind")),
        })
    }

    fn inline_asm(&mut self) -> R<InlineAsm> {
        // The largest operand index of the template and the word it is at: the operands it
        // indexes come next.
        let mut most: Option<(u32, usize)> = None;
        // A piece is three words: its tag and two.
        let template = self.list_of("an `asm!` template", 3, |it| {
            Ok(match it.word()? {
                0 => AsmPiece::Lit(Box::from(it.text()?)),
                1 => {
                    let index = it.word()?;
                    if most.is_none_or(|(most, _)| index > most) {
                        most = Some((index, it.at - 1));
                    }
                    let index = index as usize;
                    let modifier = match it.word()? {
                        0 => None,
                        code => Some(
                            char::from_u32(code - 1)
                                .ok_or_else(|| it.invalid("a modifier that is no char"))?,
                        ),
                    };
                    AsmPiece::Operand { index, modifier }
                }
                _ => return Err(it.invalid("a template piece of no kind")),
            })
        })?;
        let operands = self.list("an `asm!` block's operands", |it| {
            Ok(match it.word()? {
                0 => {
                    let reg = match it.word()? {
                        0 => InlineAsmRegOrRegClass::Reg(intern::Symbol::intern(it.text()?)),
                        1 => InlineAsmRegOrRegClass::RegClass(intern::Symbol::intern(it.text()?)),
                        _ => return Err(it.invalid("a register of no kind")),
                    };
                    AsmOperand::Reg {
                        reg,
                        input: it.opt_operand()?,
                        output: it.opt_place()?,
                        written: it.bool()?,
                    }
                }
                1 => AsmOperand::Const(it.operand()?),
                2 => AsmOperand::Label,
                3 => AsmOperand::Sym(it.operand()?),
                _ => return Err(it.invalid("an `asm!` operand of no kind")),
            })
        })?;
        if let Some((index, word)) = most
            && index as usize >= operands.len()
        {
            let what = "an `asm!` template's operand, below the block's operand count";
            return Err(self.past_at(word, what, index, operands.len() as u64));
        }
        let options = u16::try_from(self.word()?)
            .ok()
            .and_then(AsmOptions::from_bits)
            .ok_or_else(|| self.invalid("`asm!` options of no kind"))?;
        Ok(InlineAsm {
            template: template.into_boxed_slice(),
            operands: operands.into_boxed_slice(),
            options,
            taken: self.opt_place()?,
            naked: self.bool()?,
            clobber_abi: self.bool()?,
        })
    }

    fn assert_kind(&mut self) -> R<AssertKind> {
        Ok(match self.word()? {
            0 => AssertKind::BoundsCheck { len: self.operand()?, index: self.operand()? },
            1 => AssertKind::Overflow(self.bin_op()?, self.operand()?, self.operand()?),
            2 => AssertKind::OverflowNeg(self.operand()?),
            3 => AssertKind::DivisionByZero(self.operand()?),
            4 => AssertKind::RemainderByZero(self.operand()?),
            5 => AssertKind::MisalignedPointerDereference {
                required: self.operand()?,
                found: self.operand()?,
            },
            6 => AssertKind::NullPointerDereference,
            7 => AssertKind::NullReferenceConstructed,
            8 => AssertKind::InvalidEnumConstruction(self.operand()?),
            _ => return Err(self.invalid("an assertion of no kind")),
        })
    }

    fn terminator(&mut self) -> R<TerminatorKind> {
        Ok(match self.word()? {
            0 => TerminatorKind::Goto { target: self.block()? },
            1 => {
                let discr = self.operand()?;
                // A value is a `u128`, four words.
                let len = self.count_of("a switch's values", 4)?;
                let mut values = SmallVec::with_capacity(len);
                for _ in 0..len {
                    values.push(self.u128()?);
                }
                // `SwitchTargets::new` makes one target a value and the last for the rest.
                let len = self.count("a switch's targets")?;
                if len != values.len() + 1 {
                    let what = "a switch's targets, one more than its values";
                    return Err(self.past(what, len as u64, values.len() as u64 + 1));
                }
                let mut targets = SmallVec::with_capacity(len);
                for _ in 0..len {
                    targets.push(self.block()?);
                }
                TerminatorKind::SwitchInt { discr, targets: SwitchTargets { values, targets } }
            }
            2 => TerminatorKind::UnwindResume,
            3 => TerminatorKind::Abort,
            4 => TerminatorKind::Return,
            5 => TerminatorKind::Unreachable,
            6 => TerminatorKind::Drop {
                place: self.place()?,
                target: self.block()?,
                unwind: self.opt_block()?,
            },
            7 => TerminatorKind::DropAndReplace {
                place: self.place()?,
                value: self.operand()?,
                target: self.block()?,
                unwind: self.opt_block()?,
            },
            8 => TerminatorKind::Call {
                func: self.operand()?,
                args: self.operands()?,
                destination: self.place()?,
                target: self.opt_block()?,
                cleanup: self.opt_block()?,
                from_hir_call: self.bool()?,
                intrinsic: match self.word()? {
                    0 => None,
                    code => Some(
                        u16::try_from(code - 1)
                            .ok()
                            .and_then(Intrinsic::from_index)
                            .ok_or_else(|| self.invalid("an intrinsic of no kind"))?,
                    ),
                },
            },
            9 => TerminatorKind::TailCall {
                func: self.operand()?,
                args: self.operands()?,
                fn_span: self.span()?,
            },
            10 => TerminatorKind::Assert {
                cond: self.operand()?,
                expected: self.bool()?,
                msg: self.assert_kind()?,
                target: self.block()?,
                cleanup: self.opt_block()?,
            },
            11 => TerminatorKind::Yield {
                value: self.operand()?,
                resume: self.block()?,
                resume_arg: self.place()?,
                drop: self.opt_block()?,
            },
            12 => TerminatorKind::CoroutineDrop,
            13 => TerminatorKind::FalseEdge {
                real_target: self.block()?,
                imaginary_target: self.block()?,
            },
            14 => TerminatorKind::FalseUnwind {
                real_target: self.block()?,
                unwind: self.opt_block()?,
            },
            15 => TerminatorKind::ComptimeFor {
                binder: self.binder("a `for const` loop's binder, below the universe count")?,
                domain: self.ty()?,
                body: self.block()?,
                after: self.block()?,
            },
            16 => TerminatorKind::ComptimeNext { head: self.block()? },
            _ => return Err(self.invalid("a terminator of no kind")),
        })
    }
}
