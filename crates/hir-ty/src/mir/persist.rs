//! Built MIR in blaze's format (docs/v2/plan/design/item-hash.md section 7): a [`MirBody`] as
//! one `blaze_format` buffer, which the item store files under its body's identity, so a later
//! build, branch or machine reads the body instead of lowering it again.
//!
//! The body is a column of `u32` words in prefix form: each value's first word is its tag, and
//! each list starts with its length; [`read`] checks every tag, length, local and block as it
//! rebuilds the body. Types, constants and generic arguments are places in the buffer's own
//! type table (`next_solver::persist`); an allocation (a constant operand's value) is
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
        InlineAsmRegOrRegClass, Literal, Pat,
    },
};
use la_arena::{Arena, ArenaMap, Idx, RawIdx};
use rustc_type_ir::{IntTy, UintTy, inherent::IntoKind as _};
use smallvec::SmallVec;

use super::{
    AggregateKind, AsmOperand, AssertKind, BasicBlock, BasicBlockId, BinOp, BorrowKind, CastKind,
    CoroutineLocals, FieldIndex, InlineAsm, Inlined, InlinedBinding, InlinedBody, InlinedCallee,
    InlinedPromoted, InlinedScope, InlinedSpan, Local, LocalId, MirBody, MirPhase, MirSpan,
    MutBorrowKind, Operand, OperandKind, Place, PlaceElem, Projection, ProjectionElem, Rvalue,
    Statement, StatementKind, StoredPlace, SwitchTargets, Terminator, TerminatorKind, UnOp,
    UnreachableAfterCall,
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
const WORDS_VERSION: u64 = 2;

/// The literals of a body that a location macro expanded to, sorted, with the store they are
/// expressions of: the constants a body's MIR makes of them are written as slots and filled
/// from the store on reading.
#[derive(Clone, Copy)]
pub(crate) struct Locations<'a> {
    pub(crate) store: &'a ExpressionStore,
    pub(crate) exprs: &'a [ExprId],
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
            Pat::Range { start, end, .. } => start.is_some_and(|it| self.has(it)) || end.is_some_and(|it| self.has(it)),
            _ => false,
        });
        if in_pattern {
            return Some("a location macro in a pattern");
        }
        let in_asm = self.store.exprs().any(|(_, expr)| match expr {
            Expr::InlineAsm(asm) => {
                asm.operands.iter().any(|(_, operand)| matches!(operand, HirAsmOperand::Const(it) if self.has(*it)))
            }
            _ => false,
        });
        in_asm.then_some("a location macro as an `asm!` constant")
    }

    /// The constant of type `ty` that MIR building makes of the literal `expr` as the store
    /// has it now, by its rule (`lower_literal_to_operand`) for the literals location macros
    /// expand to; `None` for another.
    fn constant<'db>(&self, expr: ExprId, ty: Ty<'db>) -> Option<StoredAllocation> {
        let Expr::Literal(literal) = self.store.expr(expr) else { return None };
        let word = super::target::word();
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
            Literal::Uint(value, _) => (Box::from(&value.to_le_bytes()[..int_size(ty, word)?]), MemoryMap::Empty),
            Literal::Int(value, _) => (Box::from(&value.to_le_bytes()[..int_size(ty, word)?]), MemoryMap::Empty),
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
]);

/// `body` as a buffer of its own, the constants of `locations` as slots. Refused, with why,
/// only for what has no word: a lint's description lowering did not make, a location literal
/// used with no operand of its own, and a location literal's constant that is not what its literal makes.
pub(crate) fn write<'db>(
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
        alloc_places: HashMap::default(),
        refused: None,
        locations,
    };
    encode.body(body)?;
    let Encode { out: _, tys, words, text, allocs, alloc_places: _, refused, locations: _ } = encode;
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
/// in place, then every word as it is read. A buffer that does not read is an error, never a
/// panic and never another body. Slots are filled from `locations`; a slot it does not list, or
/// whose expression is no longer such a literal, is an error.
pub(crate) fn read<'db>(
    db: &'db Db,
    buffer: &[u8],
    locations: Option<Locations<'_>>,
) -> Result<MirBody<'db>, FormatError> {
    let root = open_as::<MirRoot>(buffer, SCHEMA)?;
    let tys = TyTableReader::open(DbInterner::new_no_crate(db), buffer, root.tys)?;
    let allocs = root.allocs.follow(buffer)?;
    let mut decode = Decode {
        words: root.words.follow(buffer)?,
        at: 0,
        text: root.text.follow(buffer)?,
        tys,
        allocs,
        // alloc: one slot per allocation the body holds, once per body read
        made: vec![None; allocs.len()],
        buffer,
        locals: 0,
        blocks: 0,
        locations,
    };
    let body = decode.body()?;
    match decode.at == decode.words.len() {
        true => Ok(body),
        false => Err(decode.invalid("words left over after the body")),
    }
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
    alloc_places: HashMap<Allocation<'db>, u32>,
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
            Some(CoroutineLocals { poll, context }) => {
                self.word(1);
                self.local(*poll);
                self.local(*context);
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
        upvars: &HashMap<BindingId, Vec<(LocalId, closure_analysis::Place)>>,
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

    fn inlined(&mut self, inlined: &Inlined<'db>) {
        let Inlined { callees, scopes, spans, bindings, promoted } = inlined;
        self.count(callees.len());
        for InlinedCallee { body, args, written_checked } in callees {
            match *body {
                InlinedBody::Fn(it) => {
                    self.word(0);
                    self.id(it);
                }
                InlinedBody::Closure(it) => {
                    self.word(1);
                    self.id(it);
                }
            }
            self.args(args);
            self.bool(*written_checked);
        }
        self.count(scopes.len());
        for &InlinedScope { callee, call_site } in scopes {
            self.word(callee);
            self.span(call_site);
        }
        self.count(spans.len());
        for &InlinedSpan { scope, span } in spans {
            self.word(scope);
            self.span(span);
        }
        self.count(bindings.len());
        for &InlinedBinding { scope, binding, local } in bindings {
            self.word(scope);
            self.word(binding.into_raw().into_u32());
            self.local(local);
        }
        self.count(promoted.len());
        for &InlinedPromoted { local, callee, expr } in promoted {
            self.local(local);
            self.word(callee);
            self.word(expr.into_raw().into_u32());
        }
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
            MirSpan::Inlined(n) => {
                self.word(6);
                self.word(n);
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
                        self.refused.get_or_insert("a location literal's constant that is not what its literal makes");
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
        }
    }
}

struct Decode<'a, 'db> {
    words: &'a [u32],
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
    locations: Option<Locations<'a>>,
}

type R<T> = Result<T, FormatError>;

fn idx<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

impl<'a, 'db> Decode<'a, 'db> {
    /// An error at the word being read.
    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at as u64, what }
    }

    #[inline]
    fn word(&mut self) -> R<u32> {
        match self.words.get(self.at) {
            Some(&word) => {
                self.at += 1;
                Ok(word)
            }
            None => Err(self.invalid("a MIR body cut short")),
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

    /// The length of a list whose items are a word or more each: never more than the words
    /// left, so a list is allocated only as long as the buffer can fill.
    fn count(&mut self) -> R<usize> {
        let count = self.word()? as usize;
        match count <= self.words.len() - self.at {
            true => Ok(count),
            false => Err(self.invalid("a list longer than the words left")),
        }
    }

    fn id<T: Relocate>(&mut self) -> R<T> {
        let bits = self.u64()?;
        id_of(bits).ok_or_else(|| self.invalid("an id the record's table has no entry of its kind at"))
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
        let rel = allocs.get(index).ok_or_else(|| self.invalid("an allocation past the body's"))?;
        let record = rel.follow(buffer)?.ok_or_else(|| self.invalid("an allocation of no record"))?;
        let made = read_allocation(record, buffer, &self.tys)?.store();
        self.made[index] = Some(made);
        Ok(made)
    }

    fn text(&mut self) -> R<&'a str> {
        let start = self.word()? as usize;
        let len = self.word()? as usize;
        let text: &'a [u8] = self.text;
        let bytes = start
            .checked_add(len)
            .and_then(|end| text.get(start..end))
            .ok_or_else(|| self.invalid("text past the body's"))?;
        std::str::from_utf8(bytes).map_err(|_| self.invalid("text that is not UTF-8"))
    }

    fn local(&mut self) -> R<LocalId> {
        let raw = self.word()?;
        match raw < self.locals {
            true => Ok(idx(raw)),
            false => Err(self.invalid("a local past the body's")),
        }
    }

    fn block(&mut self) -> R<BasicBlockId> {
        let raw = self.word()?;
        match raw < self.blocks {
            true => Ok(idx(raw)),
            false => Err(self.invalid("a block past the body's")),
        }
    }

    fn opt_block(&mut self) -> R<Option<BasicBlockId>> {
        match self.word()? {
            0 => Ok(None),
            raw if raw - 1 < self.blocks => Ok(Some(idx(raw - 1))),
            _ => Err(self.invalid("a block past the body's")),
        }
    }

    fn locals(&mut self) -> R<Vec<LocalId>> {
        let len = self.count()?;
        // alloc: the body's own list, at its length
        let mut locals = Vec::with_capacity(len);
        for _ in 0..len {
            locals.push(self.local()?);
        }
        Ok(locals)
    }

    /// A list of `len` items, each read by `item`.
    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> R<T>) -> R<Vec<T>> {
        let len = self.count()?;
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
        let local_count = self.count()?;
        let mut locals = Arena::with_capacity(local_count);
        for _ in 0..local_count {
            locals.alloc(Local { ty: self.ty()? });
        }
        self.locals = local_count as u32;
        let block_count = self.count()?;
        self.blocks = block_count as u32;
        let start_block = self.block()?;
        let mut basic_blocks = Arena::with_capacity(block_count);
        for _ in 0..block_count {
            let block = self.basic_block()?;
            basic_blocks.alloc(block);
        }
        let binding_count = self.count()?;
        let mut binding_locals = ArenaMap::with_capacity(binding_count);
        let mut last = None;
        for _ in 0..binding_count {
            let binding = self.word()?;
            if last.is_some_and(|last| binding <= last) {
                return Err(self.invalid("bindings out of order"));
            }
            last = Some(binding);
            binding_locals.insert(idx(binding), self.local()?);
        }
        let upvar_locals = self.upvar_locals()?;
        let param_locals = self.locals()?;
        let promoted_locals = self.list(|it| Ok((it.local()?, idx(it.word()?))))?;
        let closures = self.list(|it| it.id())?;
        let coroutine = match self.word()? {
            0 => None,
            1 => Some(CoroutineLocals { poll: self.local()?, context: self.local()? }),
            _ => return Err(self.invalid("a coroutine's locals of no kind")),
        };
        let unreached = self.list(|it| it.block())?;
        let scope_ends = self.locals()?;
        let upvars_in_place = self.list(|it| Ok((it.local()?, it.word()?)))?;
        let panic_unwinds =
            self.list(|it| Ok((it.block()?, it.word()? as usize, it.block()?)))?;
        let promoted_constants = self.locals()?;
        let unreachable_after_calls = self.list(|it| {
            let call = it.span()?;
            let next = it.span()?;
            let descr = match it.word()? {
                0 => "definition",
                1 => "expression",
                _ => return Err(it.invalid("an unreachable call's description of no kind")),
            };
            Ok(UnreachableAfterCall { call, next, descr, ty: it.ty()? })
        })?;
        let guard_refs = self.list(|it| Ok((it.local()?, idx(it.word()?))))?;
        let capture_copies = self.list(|it| Ok((it.local()?, FieldIndex(it.word()?))))?;
        let inlined = self.inlined()?;
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
    ) -> R<HashMap<BindingId, Vec<(LocalId, closure_analysis::Place)>>> {
        let len = self.count()?;
        // alloc: the body's own map, at its length
        let mut upvars = HashMap::with_capacity_and_hasher(len, Default::default());
        let mut last = None;
        for _ in 0..len {
            let binding = self.word()?;
            if last.is_some_and(|last| binding <= last) {
                return Err(self.invalid("captured bindings out of order"));
            }
            last = Some(binding);
            let places = self.list(|it| Ok((it.local()?, it.capture_place()?)))?;
            upvars.insert(idx(binding), places);
        }
        Ok(upvars)
    }

    fn capture_place(&mut self) -> R<closure_analysis::Place> {
        use closure_analysis::{PlaceBase, Projection, ProjectionKind};
        let base_ty = self.ty()?;
        let base = match self.word()? {
            0 => PlaceBase::Rvalue,
            1 => PlaceBase::StaticItem,
            2 => PlaceBase::Local(idx(self.word()?)),
            3 => PlaceBase::Upvar { closure: idx(self.word()?), var_id: idx(self.word()?) },
            _ => return Err(self.invalid("a captured place's base of no kind")),
        };
        let projections = self.list(|it| {
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

    fn inlined(&mut self) -> R<Inlined<'db>> {
        let callees = self.list(|it| {
            let body = match it.word()? {
                0 => InlinedBody::Fn(it.id()?),
                1 => InlinedBody::Closure(it.id()?),
                _ => return Err(it.invalid("an inlined body of no kind")),
            };
            Ok(InlinedCallee { body, args: it.args()?, written_checked: it.bool()? })
        })?;
        let scopes = self.list(|it| Ok(InlinedScope { callee: it.word()?, call_site: it.span()? }))?;
        let spans = self.list(|it| Ok(InlinedSpan { scope: it.word()?, span: it.span()? }))?;
        let bindings = self.list(|it| {
            Ok(InlinedBinding { scope: it.word()?, binding: idx(it.word()?), local: it.local()? })
        })?;
        let promoted = self.list(|it| {
            Ok(InlinedPromoted { local: it.local()?, callee: it.word()?, expr: idx(it.word()?) })
        })?;
        Ok(Inlined { callees, scopes, spans, bindings, promoted })
    }

    fn basic_block(&mut self) -> R<BasicBlock> {
        let is_cleanup = self.bool()?;
        let statements = self.list(|it| {
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

    fn span(&mut self) -> R<MirSpan> {
        Ok(match self.word()? {
            0 => MirSpan::ExprId(idx(self.word()?)),
            1 => MirSpan::ExprEnd(idx(self.word()?)),
            2 => MirSpan::PatId(idx(self.word()?)),
            3 => MirSpan::BindingId(idx(self.word()?)),
            4 => MirSpan::SelfParam,
            5 => MirSpan::Unknown,
            6 => MirSpan::Inlined(self.word()?),
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
        let len = self.count()?;
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
        let locations = self.locations.ok_or_else(|| self.invalid("a location slot in a body read with none"))?;
        if !locations.has(expr) {
            return Err(self.invalid("a location slot of an expression that is no location literal now"));
        }
        locations.constant(expr, ty.as_ref()).ok_or_else(|| self.invalid("a location slot whose literal makes no constant now"))
    }

    fn opt_operand(&mut self) -> R<Option<Operand>> {
        match self.word()? {
            0 => Ok(None),
            1 => self.operand().map(Some),
            _ => Err(self.invalid("an optional operand of no kind")),
        }
    }

    fn operands(&mut self) -> R<Box<[Operand]>> {
        self.list(|it| it.operand()).map(Vec::into_boxed_slice)
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
                        let local_id: LocalFieldId = idx(self.word()?);
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
        let template = self.list(|it| {
            Ok(match it.word()? {
                0 => AsmPiece::Lit(Box::from(it.text()?)),
                1 => {
                    let index = it.word()? as usize;
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
        let operands = self.list(|it| {
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
                let len = self.count()?;
                let mut values = SmallVec::with_capacity(len);
                for _ in 0..len {
                    values.push(self.u128()?);
                }
                let len = self.count()?;
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
            _ => return Err(self.invalid("a terminator of no kind")),
        })
    }
}
