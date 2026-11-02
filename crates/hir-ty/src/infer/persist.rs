//! [`InferenceResult`] in blaze's format (docs/v2/plan/design/data.md). [`InferenceRecord`] is
//! its `'static` form: the result's columns as they are, in three flat arrays of `u64`, `u32` and
//! `u8` words, each type an index in a type table of its own (`next_solver::persist`), each
//! definition its id (the database that wrote a buffer is the one reading it, as
//! `crates/hir-def/src/hir/persist.rs` writes ids), and what few nodes have (diagnostics, the
//! closures' captures) as tagged `u32` words beside them. [`Persist`] writes the arrays and the
//! table's buffer into one buffer and reads them back in place;
//! [`InferenceRecordRef::to_result`] makes the result again, every column checked as it is
//! rebuilt.

use std::mem::offset_of;

use blaze_format::{
    ALIGN, Aligned, FormatError, Part, Persist, Placed, Record, SchemaHash, Slice, Writer, open_as,
    schema_of,
};
use blaze_hash::FixedState;
use blaze_mem::{Frame, scratch};
use hir_def::{
    AdtId, AssocItemId, ConstId, ConstParamId, EnumVariantId, FieldId, FunctionId, GenericDefId,
    GenericParamId, ImplId, LifetimeParamId, StructId, TraitId, TupleFieldId, TupleId, TypeAliasId,
    TypeOrConstParamId, TypeParamId, UnionId, VariantId,
    builtin_type::{BuiltinFloat, BuiltinInt, BuiltinType, BuiltinUint},
    hir::{
        ArithOp, BinaryOp, BindingId, ExprId, ExprOrPatId, ExprOrPatIdPacked, PatId, UnaryOp,
        UnsafeBinderCastKind,
    },
    resolver::{TypeNs, ValueNs},
};
use hir_expand::name::Name;
use intern::Symbol;
use la_arena::{Idx, RawIdx};
use rustc_abi::ExternAbi;
use rustc_ast_ir::Mutability;
use rustc_type_ir::{ClausePolarity, ClosureKind};

use crate::{
    Span,
    consteval::CreateConstError,
    db::{AnonConstId, InternedOpaqueTyId},
    infer::{
        Adjust, Adjustment, AllowTwoPhase, AutoBorrow, AutoBorrowMutability, CallArguments,
        CaptureInfo, CaptureSourceStack, CapturedPlace, ClosureData, ExplicitDropMethodUseKind,
        InferenceDiagnostic, InferenceResult, MinCaptures, OverloadedDeref, PatAdjust,
        PatAdjustment, PointerCast, ReturnKind, StructPathDef, StructPathFound, Unreachable,
        UnreachableAt, UpvarCapture,
        cast::{CastError, CastPointerKind, CastPointers},
        closure::analysis::{
            BorrowKind,
            expr_use_visitor::{FakeReadCause, Place, PlaceBase, Projection, ProjectionKind},
        },
        columns::{
            Csr, DenseKey, ExprOrPatColumn, ExprOrPatMap, ExprOrPatSet, FieldColumn, IdxSet,
            IndexColumn, ModeColumn, SortedMap, TyColumn,
        },
        prelude_lints::{EditionLint, ReceiverAdjustment},
    },
    lower::{
        diagnostics::IncorrectGenericsLenKind,
        persist::{Words, const_error_of, const_error_word, put_diagnostic, put_path_diagnostic},
    },
    method_resolution::CandidateId,
    next_solver::{
        AnyImplId, DbInterner, FnSig, FnSigKind, GenericArgs, Predicate, StoredFnSig,
        StoredGenericArg, StoredGenericArgs, StoredPredicate, StoredTraitRef, StoredTy, StoredTys,
        TraitRef, Ty, Tys,
        abi::Safety,
        infer::traits::ObligationCauseCode,
        persist::{TyTableReader, TyTableWriter, table_words},
    },
    solver_errors::{
        Ambiguity, AmbiguityCandidate, AmbiguityNote, NamedTerm, StoredGoal, TypeAnnotationNeeded,
        UnreportedSolverError,
    },
    solver_errors::{SolverDiagnostic, SolverDiagnosticKind, StoredTraitPredicate},
};

const _: () = assert!(align_of::<u128>() == ALIGN, "the type table is placed as `u128`s");

/// The schema of the type table's own buffer inside an inference record.
const TY_TABLE_SCHEMA: SchemaHash = schema_of(&[
    Part::Str("hir_ty::infer::persist::TyTable"),
    Part::U64(2),
    Part::Schema(crate::next_solver::persist::TABLE_SCHEMA),
]);

macro_rules! columns {
    (
        $(#[$meta:meta])* $name:ident, $len:ident {
            $( $(#[$column_meta:meta])* $column:ident ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug)]
        enum $name { $( $(#[$column_meta])* $column ),* }
        const $len: usize = [$($name::$column),*].len();
    };
}

columns! {
    /// The columns of `u64` words: ids, bitset words, and which keys a sparse column has
    /// a row or list for.
    C64, N64 {
        MethodPresent,
        VariantExprPresent,
        VariantPatPresent,
        AssocExprPresent,
        AssocPatPresent,
        ExprAdjustPresent,
        CoercionPresent,
        PatAdjustPresent,
        MethodFns,
        FieldVariants,
        VariantExprIds,
        VariantPatIds,
        AssocExprIds,
        AssocPatIds,
        OpaqueIds,
        MismatchedExprs,
        MismatchedPats,
        SkippedRefPats,
        CoercionCasts,
        ClosuresWithoutExpectation,
        AnonConsts,
        /// The trait of each `traits_in_scope_used` pair, its node in `TraitsUsedNodes`.
        TraitsUsedIds,
    }
}

columns! {
    /// The columns of `u32` words: where each key's type is, types as indices in the type table,
    /// where each CSR list ends, arena indices, and the side tables' words.
    C32, N32 {
        MethodArgs,
        FieldData,
        FieldLocals,
        /// `[tuple, index]` per tuple field.
        TupleFields,
        AssocExprArgs,
        AssocPatArgs,
        /// How many types each tuple of `tuple_field_access_types` has, its types in `TupleItems`.
        TupleLens,
        TupleItems,
        ExprTyAt,
        ExprTys,
        PatTyAt,
        PatTys,
        BindingTyAt,
        BindingTys,
        PlaceholderTyAt,
        PlaceholderTys,
        OpaqueTys,
        ExprAdjustEnds,
        ExprAdjustTargets,
        CoercionEnds,
        CoercionTargets,
        PatAdjustEnds,
        PatAdjustSources,
        /// `[asm, operand, type]` per `sym` operand.
        AsmSyms,
        /// [`UNREACHABLE_WORDS`] words per `unreachable` entry: `[kind, expr, index, after kind,
        /// after, note]`.
        Unreachables,
        /// `[kind, local]` per `traits_in_scope_used` pair, as a side table writes a node.
        TraitsUsedNodes,
        /// `[span kind, span local, predicate]` per goal stalled on a coroutine of the body.
        CoroutineStalled,
        ClosureExprs,
        /// The diagnostics, then the closures' data.
        Words,
    }
}

columns! {
    /// The columns of bytes: tags and codes, and the names diagnostics hold.
    C8, N8 {
        FieldTags,
        FieldKinds,
        VariantExprKinds,
        VariantPatKinds,
        AssocExprKinds,
        AssocPatKinds,
        ExprAdjustKinds,
        CoercionKinds,
        PatAdjustKinds,
        BindingModes,
        Text,
    }
}

blaze_format::record! {
    /// An inference result: its three arrays of words and where each column is in them
    /// (`[start, len]` in words), the type table's buffer and its root in it, the error type
    /// as an index in the table, `has_errors` as bit 0 of `flags`, and how many diagnostics
    /// the words start with.
    #[derive(Clone, Copy)]
    struct InferRoot: 1 {
        table_root: u64,
        u64s: Slice<u64>,
        u32s: Slice<u32>,
        bytes: Slice<u8>,
        table: Slice<u128>,
        spans64: [[u32; 2]; N64],
        spans32: [[u32; 2]; N32],
        spans8: [[u32; 2]; N8],
        error_ty: u32,
        flags: u32,
        diagnostics: u32,
        reserved: u32,
    }
}

const _: () = assert!(size_of::<InferRoot>() == 8 + 4 * 8 + (N64 + N32 + N8) * 8 + 4 * 4);

/// An [`InferenceResult`] in its `'static` form: what [`InferenceResult::to_record`] makes and
/// [`Persist`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceRecord {
    u64s: Vec<u64>,
    u32s: Vec<u32>,
    bytes: Vec<u8>,
    spans64: [[u32; 2]; N64],
    spans32: [[u32; 2]; N32],
    spans8: [[u32; 2]; N8],
    error_ty: u32,
    flags: u32,
    diagnostics: u32,
    /// The type table's buffer, header and all.
    table: Vec<u128>,
    table_root: u64,
}

/// An [`InferenceRecord`] read in place: what [`Persist::read`] gives, and
/// [`InferenceRecord::as_ref`]. Every column is inside its array.
#[derive(Debug, Clone, Copy)]
pub struct InferenceRecordRef<'a> {
    /// Where the record's root is in its buffer, for errors.
    at: u64,
    u64s: &'a [u64],
    u32s: &'a [u32],
    bytes: &'a [u8],
    spans64: &'a [[u32; 2]; N64],
    spans32: &'a [[u32; 2]; N32],
    spans8: &'a [[u32; 2]; N8],
    error_ty: u32,
    flags: u32,
    diagnostics: u32,
    table: TableView<'a>,
    table_root: u64,
}

/// Where a record's type table is read from.
#[derive(Debug, Clone, Copy)]
enum TableView<'a> {
    /// Its bytes in the buffer [`Persist::read`] was given, at the `u128` alignment they were
    /// written at, read in place.
    InPlace(&'a [u8]),
    /// The words of an [`InferenceRecord`] in memory, copied out by [`table_bytes`].
    Words(&'a [u128]),
}

/// The type table's buffer as the reader takes it, from [`table_words`]'s words: a copy, for
/// a record that was never written.
fn table_bytes(words: &[u128]) -> Aligned {
    scratch::with_bytes(|bytes| {
        bytes.reserve(words.len() * size_of::<u128>());
        words.iter().for_each(|word| bytes.extend_from_slice(&word.to_le_bytes()));
        Aligned::copy_from(bytes)
    })
}

/// The bytes of `table`, a slice [`Slice::follow`] gave out of `buffer`.
fn table_in<'b>(buffer: &'b [u8], table: &[u128]) -> &'b [u8] {
    if table.is_empty() {
        return &[];
    }
    let start = (table.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize);
    &buffer[start..][..size_of_val(table)]
}

/// An id's word: its bits, or inside `check_cache::relocating` its place in the table.
fn id_bits<T: crate::check_cache::Relocate>(id: T) -> u64 {
    crate::check_cache::id_bits(id)
}

/// [`id_bits`]' id back; `None` for index 0 or a place not in the table.
fn id_of<T: crate::check_cache::Relocate>(bits: u64) -> Option<T> {
    crate::check_cache::id_of(bits)
}

fn local<T>(id: Idx<T>) -> u32 {
    id.into_raw().into_u32()
}

fn local_of<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

/// The values of `items`, `len` of them unless one is an error: one allocation of exactly `len`,
/// none when it is 0. Every `len` here is bounded by the record (a column's length, or a count
/// [`Source::len`] checked against the words left).
fn exact<T>(
    len: usize,
    items: impl IntoIterator<Item = Result<T, FormatError>>,
) -> Result<Vec<T>, FormatError> {
    // alloc: a decoded column of the result, once at its length
    let mut values = Vec::with_capacity(len);
    for item in items {
        values.push(item?);
    }
    debug_assert_eq!(values.len(), len, "a column of another length than counted");
    Ok(values)
}

fn variant_key(variant: VariantId) -> (u8, u64) {
    match variant {
        VariantId::EnumVariantId(it) => (0, id_bits(it)),
        VariantId::StructId(it) => (1, id_bits(it)),
        VariantId::UnionId(it) => (2, id_bits(it)),
    }
}

fn variant_of(tag: u8, bits: u64) -> Option<VariantId> {
    Some(match tag {
        0 => id_of::<EnumVariantId>(bits)?.into(),
        1 => id_of::<StructId>(bits)?.into(),
        2 => id_of::<UnionId>(bits)?.into(),
        _ => return None,
    })
}

const BUILTIN_INTS: [BuiltinInt; 6] = {
    use BuiltinInt::*;
    [Isize, I8, I16, I32, I64, I128]
};
const BUILTIN_UINTS: [BuiltinUint; 6] = {
    use BuiltinUint::*;
    [Usize, U8, U16, U32, U64, U128]
};
const BUILTIN_FLOATS: [BuiltinFloat; 4] = {
    use BuiltinFloat::*;
    [F16, F32, F64, F128]
};

/// `char`, `bool`, `str`, then the ints, uints and floats in the orders above.
fn builtin_word(ty: BuiltinType) -> u32 {
    let at = |at: Option<usize>| at.expect("every builtin number type is in its list") as u32;
    match ty {
        BuiltinType::Char => 0,
        BuiltinType::Bool => 1,
        BuiltinType::Str => 2,
        BuiltinType::Int(it) => 3 + at(BUILTIN_INTS.iter().position(|&x| x == it)),
        BuiltinType::Uint(it) => 9 + at(BUILTIN_UINTS.iter().position(|&x| x == it)),
        BuiltinType::Float(it) => 15 + at(BUILTIN_FLOATS.iter().position(|&x| x == it)),
    }
}

fn builtin_of(word: u32) -> Option<BuiltinType> {
    let word = usize::try_from(word).ok()?;
    Some(match word {
        0 => BuiltinType::Char,
        1 => BuiltinType::Bool,
        2 => BuiltinType::Str,
        3..9 => BuiltinType::Int(BUILTIN_INTS[word - 3]),
        9..15 => BuiltinType::Uint(BUILTIN_UINTS[word - 9]),
        15..19 => BuiltinType::Float(BUILTIN_FLOATS[word - 15]),
        _ => return None,
    })
}

/// Every `StructPathDef`, its word being its index here.
const STRUCT_PATH_DEFS: [StructPathDef; 17] = [
    StructPathDef::Enum,
    StructPathDef::Struct,
    StructPathDef::Union,
    StructPathDef::Variant,
    StructPathDef::TypeAlias,
    StructPathDef::BuiltinType,
    StructPathDef::Module,
    StructPathDef::Trait,
    StructPathDef::TypeParam,
    StructPathDef::Function,
    StructPathDef::Const,
    StructPathDef::Static,
    StructPathDef::ConstParam,
    StructPathDef::Local,
    StructPathDef::AssocFn,
    StructPathDef::AssocConst,
    StructPathDef::SelfCtor,
];

fn struct_path_def_word(def: StructPathDef) -> u32 {
    STRUCT_PATH_DEFS.iter().position(|&it| it == def).expect("every struct path def is listed")
        as u32
}

fn struct_path_def_of(word: u32) -> Option<StructPathDef> {
    STRUCT_PATH_DEFS.get(usize::try_from(word).ok()?).copied()
}

fn candidate_key(candidate: CandidateId) -> (u8, u64) {
    match candidate {
        CandidateId::FunctionId(it) => (0, id_bits(it)),
        CandidateId::ConstId(it) => (1, id_bits(it)),
    }
}

fn candidate_of(tag: u8, bits: u64) -> Option<CandidateId> {
    Some(match tag {
        0 => id_of::<FunctionId>(bits)?.into(),
        1 => id_of::<ConstId>(bits)?.into(),
        _ => return None,
    })
}

/// Every [`Adjust`], by its code.
const ADJUSTS: [Adjust; 16] = [
    Adjust::NeverToAny,
    Adjust::Deref(None),
    Adjust::Deref(Some(OverloadedDeref(Mutability::Not))),
    Adjust::Deref(Some(OverloadedDeref(Mutability::Mut))),
    Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Not)),
    Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut {
        allow_two_phase_borrow: AllowTwoPhase::Yes,
    })),
    Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut {
        allow_two_phase_borrow: AllowTwoPhase::No,
    })),
    Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Not)),
    Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Mut)),
    Adjust::Pointer(PointerCast::ReifyFnPointer),
    Adjust::Pointer(PointerCast::UnsafeFnPointer),
    Adjust::Pointer(PointerCast::ClosureFnPointer(Safety::Safe)),
    Adjust::Pointer(PointerCast::ClosureFnPointer(Safety::Unsafe)),
    Adjust::Pointer(PointerCast::MutToConstPointer),
    Adjust::Pointer(PointerCast::ArrayToPointer),
    Adjust::Pointer(PointerCast::Unsize),
];

fn adjust_code(adjust: &Adjust) -> u8 {
    match *adjust {
        Adjust::NeverToAny => 0,
        Adjust::Deref(None) => 1,
        Adjust::Deref(Some(OverloadedDeref(Mutability::Not))) => 2,
        Adjust::Deref(Some(OverloadedDeref(Mutability::Mut))) => 3,
        Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Not)) => 4,
        Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut {
            allow_two_phase_borrow: AllowTwoPhase::Yes,
        })) => 5,
        Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut {
            allow_two_phase_borrow: AllowTwoPhase::No,
        })) => 6,
        Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Not)) => 7,
        Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Mut)) => 8,
        Adjust::Pointer(PointerCast::ReifyFnPointer) => 9,
        Adjust::Pointer(PointerCast::UnsafeFnPointer) => 10,
        Adjust::Pointer(PointerCast::ClosureFnPointer(Safety::Safe)) => 11,
        Adjust::Pointer(PointerCast::ClosureFnPointer(Safety::Unsafe)) => 12,
        Adjust::Pointer(PointerCast::MutToConstPointer) => 13,
        Adjust::Pointer(PointerCast::ArrayToPointer) => 14,
        Adjust::Pointer(PointerCast::Unsize) => 15,
    }
}

const CAST_ERRORS: [CastError; 17] = [
    CastError::Unknown,
    CastError::CastToBool,
    CastError::CastToChar,
    CastError::DifferingKinds,
    CastError::SizedUnsizedCast,
    CastError::IllegalCast,
    CastError::IntToWideCast,
    CastError::NeedDeref,
    CastError::NeedViaPtr,
    CastError::NeedViaThinPtr,
    CastError::NeedViaInt,
    CastError::NonScalar,
    CastError::PtrPtrAddingAutoTraits,
    CastError::UnknownCastPtrKind,
    CastError::UnknownExprPtrKind,
    CastError::CastEnumDrop,
    CastError::ForeignNonExhaustiveAdt,
];

fn cast_error_code(error: &CastError) -> u32 {
    match error {
        CastError::Unknown => 0,
        CastError::CastToBool => 1,
        CastError::CastToChar => 2,
        CastError::DifferingKinds => 3,
        CastError::SizedUnsizedCast => 4,
        CastError::IllegalCast => 5,
        CastError::IntToWideCast => 6,
        CastError::NeedDeref => 7,
        CastError::NeedViaPtr => 8,
        CastError::NeedViaThinPtr => 9,
        CastError::NeedViaInt => 10,
        CastError::NonScalar => 11,
        CastError::PtrPtrAddingAutoTraits => 12,
        CastError::UnknownCastPtrKind => 13,
        CastError::UnknownExprPtrKind => 14,
        CastError::CastEnumDrop => 15,
        CastError::ForeignNonExhaustiveAdt => 16,
    }
}

const UNREACHABLE_WORDS: usize = 6;

/// The notes an [`Unreachable`] has, by their code less one.
const UNREACHABLE_NOTES: [&str; 1] = [Unreachable::ALL_ARMS_DIVERGE];

/// `[kind, expr, index, after kind, after, note]`: the kind of `what` with its expression and
/// index (0 for a kind with none), `after` as 0 for an expression or 1 for a pattern, and the
/// note as 0 or 1 + its place in [`UNREACHABLE_NOTES`].
fn unreachable_words(it: &Unreachable) -> [u32; UNREACHABLE_WORDS] {
    let (kind, expr, index) = match it.what {
        UnreachableAt::Expr(expr) => (0, expr, 0),
        UnreachableAt::Call(expr) => (1, expr, 0),
        UnreachableAt::Stmt { block, index } => (2, block, index),
        UnreachableAt::Arm { match_expr, index } => (3, match_expr, index),
        UnreachableAt::IfBlock(expr) => (4, expr, 0),
        UnreachableAt::LetInit(expr) => (5, expr, 0),
    };
    let (after_kind, after) = match it.after {
        ExprOrPatId::ExprId(it) => (0, local(it)),
        ExprOrPatId::PatId(it) => (1, local(it)),
    };
    let note = it.note.map_or(0, |note| {
        let at = UNREACHABLE_NOTES.iter().position(|&it| it == note);
        1 + at.expect("an unreachable note of `UNREACHABLE_NOTES`") as u32
    });
    [kind, local(expr), index, after_kind, after, note]
}

fn unreachable_of(words: &[u32]) -> Option<Unreachable> {
    let &[kind, expr, index, after_kind, after, note] = words else { return None };
    let expr = local_of(expr);
    let what = match kind {
        0 if index == 0 => UnreachableAt::Expr(expr),
        1 if index == 0 => UnreachableAt::Call(expr),
        2 => UnreachableAt::Stmt { block: expr, index },
        3 => UnreachableAt::Arm { match_expr: expr, index },
        4 if index == 0 => UnreachableAt::IfBlock(expr),
        5 if index == 0 => UnreachableAt::LetInit(expr),
        _ => return None,
    };
    let after = match after_kind {
        0 => ExprOrPatId::ExprId(local_of(after)),
        1 => ExprOrPatId::PatId(local_of(after)),
        _ => return None,
    };
    let note = match note {
        0 => None,
        code => Some(*UNREACHABLE_NOTES.get(code as usize - 1)?),
    };
    Some(Unreachable { what, after, note })
}

/// `[kind, local]`: 0 for an expression or 1 for a pattern, as [`Sink::expr_or_pat`] writes it.
fn node_words(node: ExprOrPatId) -> [u32; 2] {
    match node {
        ExprOrPatId::ExprId(it) => [0, local(it)],
        ExprOrPatId::PatId(it) => [1, local(it)],
    }
}

fn node_of(words: &[u32]) -> Option<ExprOrPatId> {
    match *words {
        [0, it] => Some(ExprOrPatId::ExprId(local_of(it))),
        [1, it] => Some(ExprOrPatId::PatId(local_of(it))),
        _ => None,
    }
}

/// `[kind, local]` with the kinds [`Sink::span`] writes, `[4, 0]` for `Dummy`.
fn span_words(span: Span) -> [u32; 2] {
    match span {
        Span::ExprId(it) => [0, local(it)],
        Span::PatId(it) => [1, local(it)],
        Span::BindingId(it) => [2, local(it)],
        Span::TypeRefId(it) => [3, local(it)],
        Span::Dummy => [4, 0],
    }
}

fn span_of(words: &[u32]) -> Option<Span> {
    match *words {
        [0, it] => Some(Span::ExprId(local_of(it))),
        [1, it] => Some(Span::PatId(local_of(it))),
        [2, it] => Some(Span::BindingId(local_of(it))),
        [3, it] => Some(Span::TypeRefId(local_of(it))),
        [4, 0] => Some(Span::Dummy),
        _ => None,
    }
}

/// Where a record's types go as it is made: the type table, or in tests a list.
trait TyEncode<'db> {
    fn ty(&mut self, ty: Ty<'db>) -> u32;
    fn args(&mut self, args: GenericArgs<'db>) -> u32;
    /// The trait as two words of its id and its arguments.
    fn trait_ref(&mut self, trait_ref: &StoredTraitRef) -> [u32; 3];
    fn predicate(&mut self, predicate: Predicate<'db>) -> u32;
}

/// Where a record's types come back from.
trait TyDecode {
    fn ty(&self, at: u32) -> Result<StoredTy, FormatError>;
    fn args(&self, at: u32) -> Result<StoredGenericArgs, FormatError>;
    fn trait_ref(&self, words: [u32; 3]) -> Result<StoredTraitRef, FormatError>;
    fn predicate(&self, at: u32) -> Result<StoredPredicate, FormatError>;
}

struct TableEncode<'db> {
    interner: DbInterner<'db>,
    table: TyTableWriter<'db>,
}

impl<'db> TyEncode<'db> for TableEncode<'db> {
    fn ty(&mut self, ty: Ty<'db>) -> u32 {
        self.table.ty(ty)
    }

    fn args(&mut self, args: GenericArgs<'db>) -> u32 {
        self.table.args(args)
    }

    fn trait_ref(&mut self, trait_ref: &StoredTraitRef) -> [u32; 3] {
        let trait_ref = trait_ref.get(self.interner);
        let bits = id_bits(trait_ref.def_id.0);
        let args = trait_ref.args.store();
        [bits as u32, (bits >> 32) as u32, self.table.args(args.as_ref())]
    }

    fn predicate(&mut self, predicate: Predicate<'db>) -> u32 {
        self.table.predicate(predicate)
    }
}

struct TableDecode<'a, 'db> {
    interner: DbInterner<'db>,
    table: TyTableReader<'a, 'db>,
    /// Where the record's root is, for errors.
    at: u64,
}

impl TyDecode for TableDecode<'_, '_> {
    fn ty(&self, at: u32) -> Result<StoredTy, FormatError> {
        self.table.ty(at).map(Ty::store)
    }

    fn args(&self, at: u32) -> Result<StoredGenericArgs, FormatError> {
        self.table.args(at).map(GenericArgs::store)
    }

    fn trait_ref(&self, [low, high, args]: [u32; 3]) -> Result<StoredTraitRef, FormatError> {
        let id = id_of::<TraitId>(u64::from(low) | (u64::from(high) << 32))
            .ok_or(FormatError::Invalid { at: self.at, what: "a trait of id 0 or not in the table" })?;
        let args = self.table.args(args)?;
        Ok(StoredTraitRef::new(crate::check_cache::outside(|| {
            TraitRef::new_from_args(self.interner, id.into(), args)
        })))
    }

    fn predicate(&self, at: u32) -> Result<StoredPredicate, FormatError> {
        self.table.predicate(at).map(Predicate::store)
    }
}

impl<'db> InferenceResult<'db> {
    /// The result in its `'static` form, its types in a type table of its own.
    pub fn to_record(&self, interner: DbInterner<'db>) -> InferenceRecord {
        let mut tys = TableEncode { interner, table: TyTableWriter::new(interner) };
        let mut record = InferenceRecord::of(self, &mut tys);
        let mut out = Writer::new(TY_TABLE_SCHEMA);
        record.table_root = tys.table.write(&mut out);
        let bytes = out.finish(record.table_root);
        record.table = table_words(&bytes);
        record
    }
}

/// An [`InferenceRecord`]'s three arrays while [`InferenceRecord::of`] writes them: the thread's
/// scratch buffers, warm across records, each copied once at its length by [`Columns::finish`].
struct Columns {
    u64s: scratch::Lent<Vec<u64>>,
    u32s: scratch::Lent<Vec<u32>>,
    bytes: scratch::Lent<Vec<u8>>,
    spans64: [[u32; 2]; N64],
    spans32: [[u32; 2]; N32],
    spans8: [[u32; 2]; N8],
}

impl Columns {
    fn new() -> Columns {
        Columns {
            u64s: scratch::lend_u64s(),
            u32s: scratch::lend_u32s(),
            bytes: scratch::lend_bytes(),
            spans64: [[0; 2]; N64],
            spans32: [[0; 2]; N32],
            spans8: [[0; 2]; N8],
        }
    }

    /// The record of these arrays, with no type table yet.
    fn finish(self, error_ty: u32, flags: u32, diagnostics: u32) -> InferenceRecord {
        InferenceRecord {
            // alloc: the record's own arrays, each once at its length
            u64s: self.u64s.to_vec(),
            // alloc: as `u64s`
            u32s: self.u32s.to_vec(),
            // alloc: as `u64s`
            bytes: self.bytes.to_vec(),
            spans64: self.spans64,
            spans32: self.spans32,
            spans8: self.spans8,
            error_ty,
            flags,
            diagnostics,
            // alloc: empty, no allocation; `to_record` puts the table in
            table: Vec::new(),
            table_root: 0,
        }
    }

    fn span(start: usize, end: usize) -> [u32; 2] {
        let word = |it: usize| u32::try_from(it).expect("an inference column of 2^32 words");
        [word(start), word(end - start)]
    }

    fn put64(&mut self, column: C64, words: impl IntoIterator<Item = u64>) {
        let start = self.u64s.len();
        self.u64s.extend(words);
        self.spans64[column as usize] = Self::span(start, self.u64s.len());
    }

    fn put32(&mut self, column: C32, words: impl IntoIterator<Item = u32>) {
        let start = self.u32s.len();
        self.u32s.extend(words);
        self.spans32[column as usize] = Self::span(start, self.u32s.len());
    }

    fn put8(&mut self, column: C8, words: impl IntoIterator<Item = u8>) {
        let start = self.bytes.len();
        self.bytes.extend(words);
        self.spans8[column as usize] = Self::span(start, self.bytes.len());
    }

    fn put_variants<K: DenseKey>(
        &mut self,
        column: &IndexColumn<K, VariantId>,
        present: C64,
        kinds: C8,
        ids: C64,
    ) {
        self.put64(present, column.present().iter().copied());
        self.put8(kinds, column.rows().iter().map(|&it| variant_key(it).0));
        self.put64(ids, column.rows().iter().map(|&it| variant_key(it).1));
    }

    fn put_assocs<'db, K: DenseKey>(
        &mut self,
        tys: &mut impl TyEncode<'db>,
        column: &IndexColumn<K, (CandidateId, StoredGenericArgs)>,
        [present, ids]: [C64; 2],
        args: C32,
        kinds: C8,
    ) {
        self.put64(present, column.present().iter().copied());
        self.put8(kinds, column.rows().iter().map(|&(it, _)| candidate_key(it).0));
        self.put64(ids, column.rows().iter().map(|&(it, _)| candidate_key(it).1));
        self.put32(args, column.rows().iter().map(|(_, it)| tys.args(it.as_ref())));
    }

    fn put_tys<'db, K: DenseKey>(
        &mut self,
        tys: &mut impl TyEncode<'db>,
        column: &TyColumn<K>,
        [at, dictionary]: [C32; 2],
    ) {
        self.put32(at, column.at().iter().copied());
        self.put32(dictionary, column.tys().iter().map(|it| tys.ty(it.as_ref())));
    }

    fn put_lists<K: DenseKey, T>(&mut self, column: &Csr<K, T>, present: C64, ends: C32) {
        self.put64(present, column.present().iter().copied());
        self.put32(ends, column.ends().iter().copied());
    }

    fn put_adjustments<'db, K: DenseKey>(
        &mut self,
        tys: &mut impl TyEncode<'db>,
        column: &Csr<K, Adjustment>,
        present: C64,
        [ends, targets]: [C32; 2],
        kinds: C8,
    ) {
        self.put_lists(column, present, ends);
        self.put8(kinds, column.items().iter().map(|it| adjust_code(&it.kind)));
        self.put32(targets, column.items().iter().map(|it| tys.ty(it.target.as_ref())));
    }
}

impl InferenceRecord {
    /// `result` with its types written into `tys`.
    fn of<'db>(result: &InferenceResult<'db>, tys: &mut impl TyEncode<'db>) -> InferenceRecord {
        let InferenceResult {
            method_resolutions,
            field_resolutions,
            variant_resolutions,
            value_resolutions,
            qualifier_resolutions,
            unstable_candidates,
            assoc_resolutions,
            tuple_field_access_types,
            type_of_expr,
            type_of_pat,
            type_of_binding,
            type_of_type_placeholder,
            type_of_opaque,
            has_errors,
            diagnostics,
            unreachable,
            traits_in_scope_used,
            coroutine_stalled_predicates,
            named_pat_types,
            nodes_with_type_mismatches,
            error_ty,
            expr_adjustments,
            assignment_coercions,
            pat_adjustments,
            binding_modes,
            skipped_ref_pats,
            coercion_casts,
            closures_without_expectation,
            asm_sym_tys,
            closures_data,
            defined_anon_consts,
        } = result;
        let mut record = Columns::new();

        record.put64(C64::MethodPresent, method_resolutions.present().iter().copied());
        record.put64(C64::MethodFns, method_resolutions.rows().iter().map(|&(it, _)| id_bits(it)));
        record.put32(
            C32::MethodArgs,
            method_resolutions.rows().iter().map(|(_, it)| tys.args(it.as_ref())),
        );

        let fields = field_resolutions.fields();
        record.put8(C8::FieldTags, field_resolutions.tags().iter().copied());
        record.put32(C32::FieldData, field_resolutions.data().iter().copied());
        record.put8(C8::FieldKinds, fields.iter().map(|it| variant_key(it.parent).0));
        record.put64(C64::FieldVariants, fields.iter().map(|it| variant_key(it.parent).1));
        record.put32(C32::FieldLocals, fields.iter().map(|it| local(it.local_id)));
        record.put32(
            C32::TupleFields,
            field_resolutions.tuple_fields().iter().flat_map(|it| [it.tuple.0, it.index]),
        );

        let ExprOrPatColumn { exprs, pats } = variant_resolutions;
        record.put_variants(exprs, C64::VariantExprPresent, C8::VariantExprKinds, C64::VariantExprIds);
        record.put_variants(pats, C64::VariantPatPresent, C8::VariantPatKinds, C64::VariantPatIds);
        let ExprOrPatColumn { exprs, pats } = assoc_resolutions;
        let expr_columns = [C64::AssocExprPresent, C64::AssocExprIds];
        record.put_assocs(tys, exprs, expr_columns, C32::AssocExprArgs, C8::AssocExprKinds);
        let pat_columns = [C64::AssocPatPresent, C64::AssocPatIds];
        record.put_assocs(tys, pats, pat_columns, C32::AssocPatArgs, C8::AssocPatKinds);

        let tuple_len = |it: &StoredTys| u32::try_from(it.as_ref().len()).expect("a tuple of 2^32 types");
        record.put32(C32::TupleLens, tuple_field_access_types.iter().map(tuple_len));
        record.put32(
            C32::TupleItems,
            tuple_field_access_types.iter().flat_map(|it| it.as_ref().iter()).map(|it| tys.ty(it)),
        );

        record.put_tys(tys, type_of_expr, [C32::ExprTyAt, C32::ExprTys]);
        record.put_tys(tys, type_of_pat, [C32::PatTyAt, C32::PatTys]);
        record.put_tys(tys, type_of_binding, [C32::BindingTyAt, C32::BindingTys]);
        record.put_tys(tys, type_of_type_placeholder, [C32::PlaceholderTyAt, C32::PlaceholderTys]);
        record.put64(C64::OpaqueIds, type_of_opaque.keys().map(|&it| id_bits(it)));
        record.put32(C32::OpaqueTys, type_of_opaque.values().map(|it| tys.ty(it.as_ref())));

        let ExprOrPatSet { exprs, pats } = nodes_with_type_mismatches;
        record.put64(C64::MismatchedExprs, exprs.words().iter().copied());
        record.put64(C64::MismatchedPats, pats.words().iter().copied());
        record.put64(C64::SkippedRefPats, skipped_ref_pats.words().iter().copied());
        record.put64(C64::CoercionCasts, coercion_casts.words().iter().copied());
        record.put64(
            C64::ClosuresWithoutExpectation,
            closures_without_expectation.words().iter().copied(),
        );

        let expr_columns = [C32::ExprAdjustEnds, C32::ExprAdjustTargets];
        record.put_adjustments(tys, expr_adjustments, C64::ExprAdjustPresent, expr_columns, C8::ExprAdjustKinds);
        let coercion_columns = [C32::CoercionEnds, C32::CoercionTargets];
        record.put_adjustments(tys, assignment_coercions, C64::CoercionPresent, coercion_columns, C8::CoercionKinds);
        record.put_lists(pat_adjustments, C64::PatAdjustPresent, C32::PatAdjustEnds);
        record.put8(
            C8::PatAdjustKinds,
            pat_adjustments.items().iter().map(|it| match it.kind {
                PatAdjust::BuiltinDeref => 0,
                PatAdjust::OverloadedDeref => 1,
            }),
        );
        record.put32(
            C32::PatAdjustSources,
            pat_adjustments.items().iter().map(|it| tys.ty(it.source.as_ref())),
        );
        record.put8(C8::BindingModes, binding_modes.codes().iter().copied());

        record.put32(
            C32::AsmSyms,
            asm_sym_tys
                .iter()
                .flat_map(|(&(asm, operand), ty)| [local(asm), operand, tys.ty(ty.as_ref())]),
        );
        record.put32(C32::Unreachables, unreachable.iter().flat_map(unreachable_words));
        record.put32(C32::TraitsUsedNodes, traits_in_scope_used.iter().flat_map(|&(node, _)| node_words(node)));
        record.put64(C64::TraitsUsedIds, traits_in_scope_used.iter().map(|&(_, trait_)| id_bits(trait_)));
        record.put32(
            C32::CoroutineStalled,
            coroutine_stalled_predicates.iter().flat_map(|&(span, predicate)| {
                let [kind, at] = span_words(span);
                [kind, at, tys.predicate(predicate.as_ref())]
            }),
        );

        let mut words = Sink::new();
        diagnostics.iter().for_each(|it| words.diagnostic(tys, it));
        let diagnostic_count = u32::try_from(diagnostics.len()).expect("2^32 diagnostics");
        record.put32(C32::ClosureExprs, closures_data.keys().map(|&it| local(it)));
        closures_data.values().for_each(|it| words.closure(tys, it));
        words.len(value_resolutions.iter().count());
        value_resolutions.iter().for_each(|(node, &value)| {
            words.node(node);
            words.value_ns(value);
        });
        words.len(qualifier_resolutions.iter().count());
        qualifier_resolutions.iter().for_each(|(node, &(ty, segments))| {
            words.node(node);
            words.type_ns(ty);
            words.word(segments);
        });
        words.len(unstable_candidates.iter().count());
        unstable_candidates.iter().for_each(|(node, candidates)| {
            words.node(node);
            words.len(candidates.len());
            candidates.iter().for_each(|(candidate, feature)| {
                words.candidate(*candidate);
                words.symbol(feature);
            });
        });
        words.len(named_pat_types.len());
        named_pat_types.iter().for_each(|(pat, named)| {
            words.word(local(*pat));
            words.named_term(tys, named);
        });
        record.put32(C32::Words, words.words.iter().copied());
        record.put8(C8::Text, words.text.iter().copied());

        record.put64(C64::AnonConsts, defined_anon_consts.iter().map(|&it| id_bits(it)));
        let error_ty = tys.ty(error_ty.as_ref());
        record.finish(error_ty, u32::from(*has_errors), diagnostic_count)
    }

    pub fn as_ref(&self) -> InferenceRecordRef<'_> {
        InferenceRecordRef {
            at: 0,
            u64s: &self.u64s,
            u32s: &self.u32s,
            bytes: &self.bytes,
            spans64: &self.spans64,
            spans32: &self.spans32,
            spans8: &self.spans8,
            error_ty: self.error_ty,
            flags: self.flags,
            diagnostics: self.diagnostics,
            table: TableView::Words(&self.table),
            table_root: self.table_root,
        }
    }
}

impl Persist for InferenceRecord {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("InferenceRecord"),
        Part::U64(23),
        Part::Schema(<InferRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = InferenceRecordRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let u64s = Placed::push(out, &self.u64s);
        let u32s = Placed::push(out, &self.u32s);
        let bytes = Placed::push(out, &self.bytes);
        let table = Placed::push(out, &self.table);

        let at = out.next::<InferRoot>();
        let field = |offset: usize| at + offset as u64;
        let root = InferRoot {
            table_root: self.table_root,
            u64s: u64s.slice(field(offset_of!(InferRoot, u64s))),
            u32s: u32s.slice(field(offset_of!(InferRoot, u32s))),
            bytes: bytes.slice(field(offset_of!(InferRoot, bytes))),
            table: table.slice(field(offset_of!(InferRoot, table))),
            spans64: self.spans64,
            spans32: self.spans32,
            spans8: self.spans8,
            error_ty: self.error_ty,
            flags: self.flags,
            diagnostics: self.diagnostics,
            reserved: 0,
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<InferenceRecordRef<'_>, FormatError> {
        let root = open_as::<InferRoot>(buffer, Self::SCHEMA)?;
        let at = (root as *const InferRoot as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
        if root.reserved != 0 {
            return Err(FormatError::Invalid { at, what: "an inference record with a reserved word set" });
        }
        let record = InferenceRecordRef {
            at,
            u64s: root.u64s.follow(buffer)?,
            u32s: root.u32s.follow(buffer)?,
            bytes: root.bytes.follow(buffer)?,
            spans64: &root.spans64,
            spans32: &root.spans32,
            spans8: &root.spans8,
            error_ty: root.error_ty,
            flags: root.flags,
            diagnostics: root.diagnostics,
            table: TableView::InPlace(table_in(buffer, root.table.follow(buffer)?)),
            table_root: root.table_root,
        };
        record.check()?;
        Ok(record)
    }
}

impl<'a> InferenceRecordRef<'a> {
    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at, what }
    }

    fn ensure(&self, holds: bool, what: &'static str) -> Result<(), FormatError> {
        if holds { Ok(()) } else { Err(self.invalid(what)) }
    }

    /// That every column is inside its array: what the column accessors index without checking.
    fn check(&self) -> Result<(), FormatError> {
        let inside = |spans: &[[u32; 2]], len: usize| {
            spans.iter().all(|&[start, count]| start as usize + count as usize <= len)
        };
        self.ensure(
            inside(self.spans64, self.u64s.len())
                && inside(self.spans32, self.u32s.len())
                && inside(self.spans8, self.bytes.len()),
            "an inference column past the end of its array",
        )
    }

    fn u64s(&self, column: C64) -> &'a [u64] {
        let [start, len] = self.spans64[column as usize];
        &self.u64s[start as usize..][..len as usize]
    }

    fn u32s(&self, column: C32) -> &'a [u32] {
        let [start, len] = self.spans32[column as usize];
        &self.u32s[start as usize..][..len as usize]
    }

    fn u8s(&self, column: C8) -> &'a [u8] {
        let [start, len] = self.spans8[column as usize];
        &self.bytes[start as usize..][..len as usize]
    }

    fn id<T: crate::check_cache::Relocate>(&self, bits: u64) -> Result<T, FormatError> {
        id_of(bits).ok_or(self.invalid("an id of index 0 or not in the table"))
    }

    fn same_len(&self, lens: &[usize], what: &'static str) -> Result<(), FormatError> {
        self.ensure(lens.windows(2).all(|it| it[0] == it[1]), what)
    }

    fn index_column<K: DenseKey, V>(&self, present: C64, rows: Box<[V]>) -> Result<IndexColumn<K, V>, FormatError> {
        // alloc: the memo's column, exact size, once per record read
        IndexColumn::from_parts(self.u64s(present).to_vec(), rows).map_err(|what| self.invalid(what))
    }

    fn variants<K: DenseKey>(&self, at: C64, kinds: C8, ids: C64) -> Result<IndexColumn<K, VariantId>, FormatError> {
        let (kinds, ids) = (self.u8s(kinds), self.u64s(ids));
        self.same_len(&[kinds.len(), ids.len()], "variant columns of different lengths")?;
        let rows = exact(
            kinds.len(),
            kinds
                .iter()
                .zip(ids)
                .map(|(&kind, &bits)| variant_of(kind, bits).ok_or(self.invalid("a variant of no kind or id 0"))),
        )?
        .into_boxed_slice();
        self.index_column(at, rows)
    }

    fn assocs<K: DenseKey>(
        &self,
        tys: &impl TyDecode,
        [at, ids]: [C64; 2],
        args: C32,
        kinds: C8,
    ) -> Result<IndexColumn<K, (CandidateId, StoredGenericArgs)>, FormatError> {
        let (kinds, ids, args) = (self.u8s(kinds), self.u64s(ids), self.u32s(args));
        self.same_len(&[kinds.len(), ids.len(), args.len()], "item columns of different lengths")?;
        let rows = exact(
            kinds.len(),
            kinds.iter().zip(ids).zip(args).map(|((&kind, &bits), &args)| {
                let candidate = candidate_of(kind, bits).ok_or(self.invalid("an item of no kind or id 0"))?;
                Ok((candidate, tys.args(args)?))
            }),
        )?
        .into_boxed_slice();
        self.index_column(at, rows)
    }

    fn tys(&self, tys: &impl TyDecode, column: C32) -> Result<Box<[StoredTy]>, FormatError> {
        let column = self.u32s(column);
        Ok(exact(column.len(), column.iter().map(|&it| tys.ty(it)))?.into_boxed_slice())
    }

    fn ty_column<K: DenseKey>(&self, tys: &impl TyDecode, [at, dictionary]: [C32; 2]) -> Result<TyColumn<K>, FormatError> {
        TyColumn::from_parts(self.u32s(at).into(), self.tys(tys, dictionary)?).map_err(|what| self.invalid(what))
    }

    fn idx_set<K: DenseKey>(&self, column: C64) -> Result<IdxSet<K>, FormatError> {
        // alloc: the memo's column, exact size, once per record read
        IdxSet::from_words(self.u64s(column).to_vec()).map_err(|what| self.invalid(what))
    }

    fn lists<K: DenseKey, T>(&self, present: C64, ends: C32, items: Box<[T]>) -> Result<Csr<K, T>, FormatError> {
        // alloc: the memo's column, exact size, once per record read
        Csr::from_parts(self.u64s(present).to_vec(), self.u32s(ends).into(), items).map_err(|what| self.invalid(what))
    }

    fn adjustments<K: DenseKey>(
        &self,
        tys: &impl TyDecode,
        present: C64,
        [ends, targets]: [C32; 2],
        kinds: C8,
    ) -> Result<Csr<K, Adjustment>, FormatError> {
        let (kinds, targets) = (self.u8s(kinds), self.u32s(targets));
        self.same_len(&[kinds.len(), targets.len()], "adjustment columns of different lengths")?;
        let items = exact(
            kinds.len(),
            kinds.iter().zip(targets).map(|(&kind, &target)| {
                let kind = ADJUSTS.get(kind as usize).cloned().ok_or(self.invalid("an adjustment of no kind"))?;
                Ok(Adjustment { kind, target: tys.ty(target)? })
            }),
        )?
        .into_boxed_slice();
        self.lists(present, ends, items)
    }

    /// The result again, its types read from the record's type table.
    pub fn to_result<'db>(&self, interner: DbInterner<'db>) -> Result<InferenceResult<'db>, FormatError> {
        let copy;
        let bytes: &[u8] = match self.table {
            TableView::InPlace(bytes) => bytes,
            TableView::Words(words) => {
                copy = table_bytes(words);
                &copy
            }
        };
        let table = TyTableReader::open(interner, bytes, self.table_root)?;
        self.decode(&TableDecode { interner, table, at: self.at })
    }

    /// [`Self::decode_in`] in a task of its own: decoding runs outside any query (R1).
    fn decode<'db>(&self, tys: &impl TyDecode) -> Result<InferenceResult<'db>, FormatError> {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| self.decode_in(tys, frame))
    }

    /// The result of this record, its temporaries in `frame`.
    fn decode_in<'db>(
        &self,
        tys: &impl TyDecode,
        frame: &Frame,
    ) -> Result<InferenceResult<'db>, FormatError> {
        let (fns, args) = (self.u64s(C64::MethodFns), self.u32s(C32::MethodArgs));
        self.same_len(&[fns.len(), args.len()], "method columns of different lengths")?;
        let rows = exact(
            fns.len(),
            fns.iter().zip(args).map(|(&bits, &args)| Ok((self.id::<FunctionId>(bits)?, tys.args(args)?))),
        )?
        .into_boxed_slice();
        let method_resolutions = self.index_column(C64::MethodPresent, rows)?;

        let tuple_field_access_types = {
            let (lens, items) = (self.u32s(C32::TupleLens), self.tys(tys, C32::TupleItems)?);
            let total = lens.iter().try_fold(0usize, |sum, &len| sum.checked_add(len as usize));
            self.ensure(total == Some(items.len()), "tuple lengths that do not add up to their types")?;
            let mut start = 0;
            let mut buffer = Vec::new_in(frame);
            lens.iter()
                .map(|&len| {
                    buffer.clear();
                    buffer.extend(items[start..][..len as usize].iter().map(StoredTy::as_ref));
                    start += len as usize;
                    Tys::new_from_slice(&buffer).store()
                })
                .collect::<Box<[StoredTys]>>()
        };

        let field_resolutions = {
            let (kinds, variants, locals) =
                (self.u8s(C8::FieldKinds), self.u64s(C64::FieldVariants), self.u32s(C32::FieldLocals));
            self.same_len(&[kinds.len(), variants.len(), locals.len()], "field columns of different lengths")?;
            let fields = exact(
                kinds.len(),
                kinds.iter().zip(variants).zip(locals).map(|((&kind, &bits), &local)| {
                    let parent = variant_of(kind, bits).ok_or(self.invalid("a field of a variant of no kind"))?;
                    Ok(FieldId { parent, local_id: local_of(local) })
                }),
            )?
            .into_boxed_slice();
            let words = self.u32s(C32::TupleFields);
            self.ensure(words.len() % 2 == 0, "a tuple field column of an odd number of words")?;
            let tuple_fields = exact(
                words.len() / 2,
                words.chunks_exact(2).map(|it| {
                    let tuple = tuple_field_access_types.get(it[0] as usize);
                    self.ensure(
                        tuple.is_some_and(|tuple| (it[1] as usize) < tuple.as_ref().len()),
                        "a tuple field past its tuple",
                    )?;
                    Ok(TupleFieldId { tuple: TupleId(it[0]), index: it[1] })
                }),
            )?
            .into_boxed_slice();
            let (tags, data) = (self.u8s(C8::FieldTags), self.u32s(C32::FieldData));
            FieldColumn::from_parts(tags.into(), data.into(), fields, tuple_fields)
                .map_err(|what| self.invalid(what))?
        };

        let variant_resolutions = ExprOrPatColumn {
            exprs: self.variants(C64::VariantExprPresent, C8::VariantExprKinds, C64::VariantExprIds)?,
            pats: self.variants(C64::VariantPatPresent, C8::VariantPatKinds, C64::VariantPatIds)?,
        };
        let assoc_resolutions = ExprOrPatColumn {
            exprs: self.assocs(tys, [C64::AssocExprPresent, C64::AssocExprIds], C32::AssocExprArgs, C8::AssocExprKinds)?,
            pats: self.assocs(tys, [C64::AssocPatPresent, C64::AssocPatIds], C32::AssocPatArgs, C8::AssocPatKinds)?,
        };

        let type_of_opaque = {
            let (ids, opaque_tys) = (self.u64s(C64::OpaqueIds), self.u32s(C32::OpaqueTys));
            self.same_len(&[ids.len(), opaque_tys.len()], "opaque columns of different lengths")?;
            let mut entries = exact(
                ids.len(),
                ids.iter()
                    .zip(opaque_tys)
                    .map(|(&bits, &ty)| Ok((self.id::<InternedOpaqueTyId<'db>>(bits)?, tys.ty(ty)?))),
            )?;
            if crate::check_cache::in_scope() {
                entries.sort_unstable_by_key(|&(id, _)| id);
            }
            SortedMap::from_sorted(entries).map_err(|what| self.invalid(what))?
        };

        let pat_adjustments = {
            let (kinds, sources) = (self.u8s(C8::PatAdjustKinds), self.u32s(C32::PatAdjustSources));
            self.same_len(&[kinds.len(), sources.len()], "pattern adjustment columns of different lengths")?;
            let items = exact(
                kinds.len(),
                kinds.iter().zip(sources).map(|(&kind, &source)| {
                    let kind = match kind {
                        0 => PatAdjust::BuiltinDeref,
                        1 => PatAdjust::OverloadedDeref,
                        _ => return Err(self.invalid("a pattern adjustment of no kind")),
                    };
                    Ok(PatAdjustment { kind, source: tys.ty(source)? })
                }),
            )?
            .into_boxed_slice();
            self.lists(C64::PatAdjustPresent, C32::PatAdjustEnds, items)?
        };

        let asm_sym_tys = {
            let words = self.u32s(C32::AsmSyms);
            self.ensure(words.len() % 3 == 0, "a sym operand column of a partial operand")?;
            let entries = exact(
                words.len() / 3,
                words.chunks_exact(3).map(|it| Ok(((local_of(it[0]), it[1]), tys.ty(it[2])?))),
            )?;
            SortedMap::from_sorted(entries).map_err(|what| self.invalid(what))?
        };

        let unreachable = {
            let words = self.u32s(C32::Unreachables);
            self.ensure(words.len() % UNREACHABLE_WORDS == 0, "an unreachable column of a partial entry")?;
            exact(
                words.len() / UNREACHABLE_WORDS,
                words
                    .chunks_exact(UNREACHABLE_WORDS)
                    .map(|it| unreachable_of(it).ok_or(self.invalid("an unreachable entry of no kind"))),
            )?
            .into_boxed_slice()
        };

        let coroutine_stalled_predicates = {
            let words = self.u32s(C32::CoroutineStalled);
            self.ensure(words.len() % 3 == 0, "a stalled goal column of a partial goal")?;
            exact(
                words.len() / 3,
                words.chunks_exact(3).map(|it| {
                    let span = span_of(&it[..2]).ok_or(self.invalid("a stalled goal at a span of no kind"))?;
                    Ok((span, tys.predicate(it[2])?))
                }),
            )?
            .into_boxed_slice()
        };

        let traits_in_scope_used = {
            let (nodes, ids) = (self.u32s(C32::TraitsUsedNodes), self.u64s(C64::TraitsUsedIds));
            self.same_len(&[nodes.len(), 2 * ids.len()], "trait use columns of different lengths")?;
            exact(
                ids.len(),
                nodes.chunks_exact(2).zip(ids).map(|(node, &bits)| {
                    let node = node_of(node).ok_or(self.invalid("a trait use at a node of no kind"))?;
                    Ok((node, self.id::<TraitId>(bits)?))
                }),
            )?
            .into_boxed_slice()
        };

        let side = self.u32s(C32::Words);
        let mut words = Source { words: side, next: 0, text: self.u8s(C8::Text), at: self.at, frame };
        let count = self.diagnostics as usize;
        self.ensure(count <= side.len(), "more diagnostics than side table words")?;
        let diagnostics = exact(count, (0..count).map(|_| words.diagnostic(tys)))?.into_boxed_slice();
        let closures_data = {
            let exprs = self.u32s(C32::ClosureExprs);
            let entries =
                exact(exprs.len(), exprs.iter().map(|&expr| Ok((local_of(expr), words.closure(tys, frame)?))))?;
            SortedMap::from_sorted(entries).map_err(|what| self.invalid(what))?
        };
        let value_resolutions = {
            let mut map = ExprOrPatMap::new_in(frame);
            for _ in 0..words.len()? {
                let node = words.node()?;
                let value = words.value_ns()?;
                self.ensure(map.insert(node, value).is_none(), "a node with two value path resolutions")?;
            }
            map.freeze()
        };
        let qualifier_resolutions = {
            let mut map = ExprOrPatMap::new_in(frame);
            for _ in 0..words.len()? {
                let node = words.node()?;
                let qualifier = (words.type_ns()?, words.word()?);
                self.ensure(map.insert(node, qualifier).is_none(), "a node with two qualifier resolutions")?;
            }
            map.freeze()
        };
        let unstable_candidates = {
            let mut map = ExprOrPatMap::new_in(frame);
            for _ in 0..words.len()? {
                let node = words.node()?;
                let len = words.len()?;
                self.ensure(len != 0, "a node that passed over no unstable items")?;
                let candidates =
                    exact(len, (0..len).map(|_| Ok((words.candidate()?, words.symbol()?))))?.into_boxed_slice();
                self.ensure(map.insert(node, candidates).is_none(), "a node with two unstable item lists")?;
            }
            map.freeze()
        };
        let named_pat_types = {
            let len = words.len()?;
            let entries = exact(
                len,
                (0..len).map(|_| -> Result<(PatId, NamedTerm), FormatError> {
                    Ok((local_of(words.word()?), words.named_term(tys)?))
                }),
            )?;
            self.ensure(entries.is_sorted_by(|a, b| a.0 < b.0), "named pattern types out of order")?;
            entries.into_boxed_slice()
        };
        self.ensure(words.is_done(), "words after the last named pattern type")?;

        let anon_consts = self.u64s(C64::AnonConsts);
        let defined_anon_consts =
            exact(anon_consts.len(), anon_consts.iter().map(|&bits| self.id::<AnonConstId<'db>>(bits)))?
                .into_boxed_slice();
        self.ensure(self.flags <= 1, "an inference record with unknown flags")?;

        Ok(InferenceResult {
            method_resolutions,
            field_resolutions,
            variant_resolutions,
            value_resolutions,
            qualifier_resolutions,
            unstable_candidates,
            assoc_resolutions,
            tuple_field_access_types,
            type_of_expr: self.ty_column(tys, [C32::ExprTyAt, C32::ExprTys])?,
            type_of_pat: self.ty_column(tys, [C32::PatTyAt, C32::PatTys])?,
            type_of_binding: self.ty_column(tys, [C32::BindingTyAt, C32::BindingTys])?,
            type_of_type_placeholder: self.ty_column(tys, [C32::PlaceholderTyAt, C32::PlaceholderTys])?,
            type_of_opaque,
            has_errors: self.flags & 1 != 0,
            diagnostics,
            unreachable,
            traits_in_scope_used,
            coroutine_stalled_predicates,
            named_pat_types,
            nodes_with_type_mismatches: ExprOrPatSet {
                exprs: self.idx_set(C64::MismatchedExprs)?,
                pats: self.idx_set(C64::MismatchedPats)?,
            },
            error_ty: tys.ty(self.error_ty)?,
            expr_adjustments: self.adjustments(
                tys,
                C64::ExprAdjustPresent,
                [C32::ExprAdjustEnds, C32::ExprAdjustTargets],
                C8::ExprAdjustKinds,
            )?,
            assignment_coercions: self.adjustments(
                tys,
                C64::CoercionPresent,
                [C32::CoercionEnds, C32::CoercionTargets],
                C8::CoercionKinds,
            )?,
            pat_adjustments,
            binding_modes: ModeColumn::from_codes(self.u8s(C8::BindingModes).into())
                .map_err(|what| self.invalid(what))?,
            skipped_ref_pats: self.idx_set(C64::SkippedRefPats)?,
            coercion_casts: self.idx_set(C64::CoercionCasts)?,
            closures_without_expectation: self.idx_set(C64::ClosuresWithoutExpectation)?,
            asm_sym_tys,
            closures_data,
            defined_anon_consts,
        })
    }
}

/// The side tables' words being written, and the text of the names in them: the thread's
/// scratch buffers, which [`InferenceRecord::of`] copies into the record's arrays.
struct Sink {
    words: scratch::Lent<Vec<u32>>,
    text: scratch::Lent<Vec<u8>>,
}

impl Sink {
    fn new() -> Sink {
        Sink { words: scratch::lend_u32s(), text: scratch::lend_bytes() }
    }

    fn word(&mut self, word: u32) {
        self.words.push(word);
    }

    fn u64(&mut self, word: u64) {
        self.words.extend([word as u32, (word >> 32) as u32]);
    }

    fn usize(&mut self, word: usize) {
        self.u64(word as u64);
    }

    fn bool(&mut self, value: bool) {
        self.word(u32::from(value));
    }

    fn len(&mut self, len: usize) {
        self.word(u32::try_from(len).expect("a side table list of 2^32 entries"));
    }

    fn local<T>(&mut self, id: Idx<T>) {
        self.word(local(id));
    }

    fn id<T: crate::check_cache::Relocate>(&mut self, id: T) {
        self.u64(id_bits(id));
    }

    fn ty<'db>(&mut self, tys: &mut impl TyEncode<'db>, ty: &StoredTy) {
        let at = tys.ty(ty.as_ref());
        self.word(at);
    }

    fn opt_ty<'db>(&mut self, tys: &mut impl TyEncode<'db>, ty: Option<&StoredTy>) {
        match ty {
            None => self.word(0),
            Some(ty) => {
                self.word(1);
                self.ty(tys, ty);
            }
        }
    }

    fn name(&mut self, name: &Name) {
        self.symbol(name.symbol());
    }

    fn symbol(&mut self, symbol: &Symbol) {
        let text = symbol.as_str().as_bytes();
        self.len(self.text.len());
        self.len(text.len());
        self.text.extend_from_slice(text);
    }

    fn candidate(&mut self, candidate: CandidateId) {
        let (tag, bits) = candidate_key(candidate);
        self.word(u32::from(tag));
        self.u64(bits);
    }

    fn node(&mut self, node: ExprOrPatIdPacked) {
        self.expr_or_pat(node.unpack());
    }

    fn expr_or_pat(&mut self, node: ExprOrPatId) {
        match node {
            ExprOrPatId::ExprId(it) => self.words.extend([0, local(it)]),
            ExprOrPatId::PatId(it) => self.words.extend([1, local(it)]),
        }
    }

    fn opt_expr(&mut self, expr: Option<ExprId>) {
        match expr {
            None => self.word(0),
            Some(it) => self.words.extend([1, local(it)]),
        }
    }

    fn variant(&mut self, variant: VariantId) {
        let (kind, bits) = variant_key(variant);
        self.word(u32::from(kind));
        self.u64(bits);
    }

    fn generic_def(&mut self, def: GenericDefId) {
        let (tag, bits) = match def {
            GenericDefId::AdtId(AdtId::StructId(it)) => (0, id_bits(it)),
            GenericDefId::AdtId(AdtId::EnumId(it)) => (1, id_bits(it)),
            GenericDefId::AdtId(AdtId::UnionId(it)) => (2, id_bits(it)),
            GenericDefId::ConstId(it) => (3, id_bits(it)),
            GenericDefId::FunctionId(it) => (4, id_bits(it)),
            GenericDefId::ImplId(it) => (5, id_bits(it)),
            GenericDefId::StaticId(it) => (6, id_bits(it)),
            GenericDefId::TraitId(it) => (7, id_bits(it)),
            GenericDefId::TypeAliasId(it) => (8, id_bits(it)),
        };
        self.word(tag);
        self.u64(bits);
    }

    fn cast_pointer_kind(&mut self, kind: Option<CastPointerKind>) {
        match kind {
            None => self.word(0),
            Some(CastPointerKind::Thin) => self.word(1),
            Some(CastPointerKind::VTable(principal)) => {
                self.word(2);
                self.bool(principal.is_some());
                if let Some(principal) = principal {
                    self.id(principal);
                }
            }
            Some(CastPointerKind::Length) => self.word(3),
            Some(CastPointerKind::OfAlias) => self.word(4),
            Some(CastPointerKind::OfParam(param)) => {
                self.word(5);
                self.generic_param(param.into());
            }
        }
    }

    fn generic_param(&mut self, param: GenericParamId) {
        let (tag, parent, local) = match param {
            GenericParamId::TypeParamId(it) => (0, it.parent(), local(it.local_id())),
            GenericParamId::ConstParamId(it) => (1, it.parent(), local(it.local_id())),
            GenericParamId::LifetimeParamId(it) => (2, it.parent, local(it.local_id)),
        };
        self.word(tag);
        self.generic_def(parent);
        self.word(local);
    }

    fn value_ns(&mut self, value: ValueNs) {
        match value {
            ValueNs::ImplSelf(it) => {
                self.word(0);
                self.id(it);
            }
            ValueNs::LocalBinding(it) => {
                self.word(1);
                self.local(it);
            }
            ValueNs::FunctionId(it) => {
                self.word(2);
                self.id(it);
            }
            ValueNs::ConstId(it) => {
                self.word(3);
                self.id(it);
            }
            ValueNs::StaticId(it) => {
                self.word(4);
                self.id(it);
            }
            ValueNs::StructId(it) => {
                self.word(5);
                self.id(it);
            }
            ValueNs::EnumVariantId(it) => {
                self.word(6);
                self.id(it);
            }
            ValueNs::GenericParam(it) => {
                self.word(7);
                self.generic_def(it.parent());
                self.word(local(it.local_id()));
            }
        }
    }

    fn type_ns(&mut self, ty: TypeNs) {
        match ty {
            TypeNs::SelfType(it) => {
                self.word(0);
                self.id(it);
            }
            TypeNs::GenericParam(it) => {
                self.word(1);
                self.generic_def(it.parent());
                self.word(local(it.local_id()));
            }
            TypeNs::AdtId(it) => {
                self.word(2);
                self.generic_def(it.into());
            }
            TypeNs::AdtSelfType(it) => {
                self.word(3);
                self.generic_def(it.into());
            }
            TypeNs::EnumVariantId(it) => {
                self.word(4);
                self.id(it);
            }
            TypeNs::TypeAliasId(it) => {
                self.word(5);
                self.id(it);
            }
            TypeNs::BuiltinType(it) => {
                self.word(6);
                self.word(builtin_word(it));
            }
            TypeNs::TraitId(it) => {
                self.word(7);
                self.id(it);
            }
            // hir-def's `resolve_path_in_value_ns` stops at a type, never at a module.
            TypeNs::ModuleId(_) => unreachable!("a value path's qualifier resolved to a module"),
        }
    }

    fn generics_len_kind(&mut self, kind: IncorrectGenericsLenKind) {
        self.word(match kind {
            IncorrectGenericsLenKind::Lifetimes => 0,
            IncorrectGenericsLenKind::TypesAndConsts => 1,
        });
    }

    fn span(&mut self, span: Span) {
        match span {
            Span::ExprId(it) => self.words.extend([0, local(it)]),
            Span::PatId(it) => self.words.extend([1, local(it)]),
            Span::BindingId(it) => self.words.extend([2, local(it)]),
            Span::TypeRefId(it) => self.words.extend([3, local(it)]),
            Span::Dummy => self.word(4),
        }
    }

    fn closure_kind(&mut self, kind: ClosureKind) {
        self.word(match kind {
            ClosureKind::Fn => 0,
            ClosureKind::FnMut => 1,
            ClosureKind::FnOnce => 2,
        });
    }

    fn trait_predicate<'db>(&mut self, tys: &mut impl TyEncode<'db>, predicate: &StoredTraitPredicate) {
        let StoredTraitPredicate { trait_ref, polarity } = predicate;
        let words = tys.trait_ref(trait_ref);
        self.words.extend(words);
        self.word(match polarity {
            ClausePolarity::Positive => 0,
            ClausePolarity::Negative => 1,
        });
    }

    fn call_arguments<'db>(&mut self, tys: &mut impl TyEncode<'db>, args: &CallArguments) {
        let CallArguments {
            callee,
            method,
            is_method,
            c_variadic,
            formal_inputs,
            expected_inputs,
            provided,
            compatible_ends,
            compatible,
        } = args;
        self.ty(tys, callee);
        match method {
            None => self.word(0),
            Some(method) => {
                self.word(1);
                self.id(*method);
            }
        }
        self.bool(*is_method);
        self.bool(*c_variadic);
        for list in [formal_inputs, expected_inputs, provided] {
            self.len(list.len());
            list.iter().for_each(|it| self.ty(tys, it));
        }
        self.words.extend(compatible_ends.iter().copied());
        self.len(compatible.len());
        self.words.extend(compatible.iter().copied());
    }

    fn polarity(&mut self, polarity: ClausePolarity) {
        self.word(match polarity {
            ClausePolarity::Positive => 0,
            ClausePolarity::Negative => 1,
        });
    }

    fn generic_arg<'db>(&mut self, tys: &mut impl TyEncode<'db>, arg: &StoredGenericArg) {
        let at = tys.args(GenericArgs::new_from_slice(&[arg.as_ref()]));
        self.word(at);
    }

    fn args<'db>(&mut self, tys: &mut impl TyEncode<'db>, args: &StoredGenericArgs) {
        let at = tys.args(args.as_ref());
        self.word(at);
    }

    fn goal<'db>(&mut self, tys: &mut impl TyEncode<'db>, goal: &StoredGoal) {
        match goal {
            StoredGoal::Trait { trait_, args, polarity } => {
                self.word(0);
                self.id(*trait_);
                self.args(tys, args);
                self.polarity(*polarity);
            }
            StoredGoal::HostEffect { trait_, args, maybe_const } => {
                self.word(1);
                self.id(*trait_);
                self.args(tys, args);
                self.bool(*maybe_const);
            }
            StoredGoal::Projection { alias, term } => {
                self.word(2);
                self.generic_arg(tys, alias);
                self.generic_arg(tys, term);
            }
            StoredGoal::NormalizesTo { alias, term } => {
                self.word(3);
                self.generic_arg(tys, alias);
                self.generic_arg(tys, term);
            }
            StoredGoal::WellFormed { term } => {
                self.word(4);
                self.generic_arg(tys, term);
            }
            StoredGoal::Subtype { a, b } => {
                self.word(5);
                self.ty(tys, a);
                self.ty(tys, b);
            }
            StoredGoal::Coerce { a, b } => {
                self.word(6);
                self.ty(tys, a);
                self.ty(tys, b);
            }
            StoredGoal::ConstEvaluatable { ct } => {
                self.word(7);
                self.generic_arg(tys, ct);
            }
            StoredGoal::ConstArgHasType { ct, ty } => {
                self.word(8);
                self.generic_arg(tys, ct);
                self.ty(tys, ty);
            }
            StoredGoal::ConstEquate { a, b } => {
                self.word(9);
                self.generic_arg(tys, a);
                self.generic_arg(tys, b);
            }
            StoredGoal::DynCompatible { trait_ } => {
                self.word(10);
                self.id(*trait_);
            }
            StoredGoal::UnstableFeature { feature } => {
                self.word(11);
                // clone: error path, the feature of a reported goal
                self.name(&Name::new_symbol_root(feature.clone()));
            }
            StoredGoal::Outlives { a, b } => {
                self.word(12);
                self.generic_arg(tys, a);
                self.generic_arg(tys, b);
            }
            StoredGoal::Ambiguous => self.word(13),
        }
    }

    /// A [`NamedTerm`]: its term as a one-argument list, then each parameter's span and id.
    fn named_term<'db>(&mut self, tys: &mut impl TyEncode<'db>, named: &NamedTerm) {
        let at = tys.args(GenericArgs::new_from_slice(&[named.term.as_ref()]));
        self.word(at);
        self.len(named.params.len());
        for &(span, param) in &named.params {
            self.span(span);
            self.generic_param(param);
        }
    }

    fn ambiguity_note<'db>(&mut self, tys: &mut impl TyEncode<'db>, note: &AmbiguityNote) {
        self.goal(tys, &note.goal);
        self.len(note.candidates.len());
        for candidate in &note.candidates {
            match candidate {
                AmbiguityCandidate::Impl(AnyImplId::ImplId(it)) => {
                    self.word(0);
                    self.id(*it);
                }
                AmbiguityCandidate::Impl(AnyImplId::BuiltinDeriveImplId(it)) => {
                    self.word(1);
                    self.id(*it);
                }
                AmbiguityCandidate::ParamEnv { trait_, args } => {
                    self.word(2);
                    self.id(*trait_);
                    self.args(tys, args);
                }
            }
        }
    }

    /// A lowering diagnostic in `lower::persist`'s words, after their count, so that
    /// [`Source::lowering_words`] reads exactly them.
    fn lowering_words(&mut self, put: impl FnOnce(&mut Vec<u32>)) {
        let at = self.words.len();
        self.word(0);
        put(&mut self.words);
        self.words[at] = u32::try_from(self.words.len() - at - 1).expect("a diagnostic of 2^32 words");
    }

    /// A diagnostic: its kind's place in [`InferenceDiagnostic`]'s declaration, then its fields
    /// in theirs.
    fn diagnostic<'db>(&mut self, tys: &mut impl TyEncode<'db>, diagnostic: &InferenceDiagnostic) {
        use InferenceDiagnostic as D;
        match diagnostic {
            D::NoSuchField { field, private, variant } => {
                self.word(0);
                self.node(*field);
                match private {
                    None => self.word(0),
                    Some(it) => self.words.extend([1, local(*it)]),
                }
                self.variant(*variant);
            }
            D::MismatchedArrayPatLen { pat, expected, found, has_rest } => {
                self.word(1);
                self.local(*pat);
                self.u64(*expected);
                self.u64(*found);
                self.bool(*has_rest);
            }
            D::ArrayPatternWithoutFixedLength { pat } => self.words.extend([2, local(*pat)]),
            D::ExpectedArrayOrSlicePat { pat, found } => {
                self.words.extend([3, local(*pat)]);
                self.ty(tys, found);
            }
            D::InvalidRangePatType { pat, lhs, rhs } => {
                self.words.extend([4, local(*pat)]);
                for side in [lhs, rhs] {
                    self.opt_ty(tys, side.as_ref().map(|(ty, _)| ty));
                    if let Some((_, fail)) = side {
                        self.bool(*fail);
                    }
                }
            }
            D::DuplicateField { field, variant } => {
                self.word(5);
                self.node(*field);
                self.variant(*variant);
            }
            D::PrivateField { expr, field } => {
                self.words.extend([6, local(*expr)]);
                self.variant(field.parent);
                self.local(field.local_id);
            }
            D::PrivateAssocItem { id, item } => {
                self.word(7);
                self.node(*id);
                let (kind, bits) = match *item {
                    AssocItemId::FunctionId(it) => (0, id_bits(it)),
                    AssocItemId::ConstId(it) => (1, id_bits(it)),
                    AssocItemId::TypeAliasId(it) => (2, id_bits(it)),
                };
                self.word(kind);
                self.u64(bits);
            }
            D::UnresolvedField { expr, receiver, name, method_with_same_name_exists } => {
                self.words.extend([8, local(*expr)]);
                self.ty(tys, receiver);
                self.name(name);
                self.bool(*method_with_same_name_exists);
            }
            D::UnresolvedMethodCall {
                expr,
                receiver,
                name,
                field_with_same_name,
                assoc_func_with_same_name,
            } => {
                self.words.extend([9, local(*expr)]);
                self.ty(tys, receiver);
                self.name(name);
                self.opt_ty(tys, field_with_same_name.as_ref());
                match assoc_func_with_same_name {
                    None => self.word(0),
                    Some(it) => {
                        self.word(1);
                        self.id(*it);
                    }
                }
            }
            D::UnresolvedAssocItem { id, ty, name } => {
                self.word(10);
                self.node(*id);
                self.ty(tys, ty);
                self.name(name);
            }
            D::UnresolvedIdent { id } => {
                self.word(11);
                self.node(*id);
            }
            D::UnresolvedPath { id, segment } => {
                self.word(70);
                self.node(*id);
                self.word(*segment);
            }
            D::BreakOutsideOfLoop { expr, is_break, bad_value_break } => {
                self.words.extend([12, local(*expr)]);
                self.bool(*is_break);
                self.bool(*bad_value_break);
            }
            D::UnlabeledJumpInLabeledBlock { expr, is_break } => {
                self.words.extend([50, local(*expr)]);
                self.bool(*is_break);
            }
            D::AsyncCoroutinesNotSupported { expr } => self.words.extend([51, local(*expr)]),
            D::NonExhaustiveRecordExpr { expr } => self.words.extend([13, local(*expr)]),
            D::NonExhaustiveRecordPat { pat, variant } => {
                self.words.extend([14, local(*pat)]);
                self.variant(*variant);
            }
            D::UnionPatMustHaveExactlyOneField { pat } => self.words.extend([15, local(*pat)]),
            D::UnionPatHasRest { pat } => self.words.extend([16, local(*pat)]),
            D::FunctionalRecordUpdateOnNonStruct { base_expr } => {
                self.words.extend([17, local(*base_expr)])
            }
            D::MismatchedArgCount { call_expr, expected, found, is_fn_trait_call, args } => {
                self.words.extend([18, local(*call_expr)]);
                self.usize(*expected);
                self.usize(*found);
                self.bool(*is_fn_trait_call);
                self.call_arguments(tys, args);
            }
            D::IncorrectArguments { call_expr, args } => {
                self.words.extend([52, local(*call_expr)]);
                self.call_arguments(tys, args);
            }
            D::MismatchedTupleStructPatArgCount { pat, expected, found, variant, had_err } => {
                self.words.extend([19, local(*pat)]);
                self.usize(*expected);
                self.usize(*found);
                self.variant(*variant);
                self.bool(*had_err);
            }
            D::ExpectedFunction { call_expr, found } => {
                self.words.extend([20, local(*call_expr)]);
                self.ty(tys, found);
            }
            D::CannotBeDereferenced { expr, found, at_error } => {
                self.words.extend([21, local(*expr)]);
                self.ty(tys, found);
                self.opt_ty(tys, at_error.as_ref());
            }
            D::UnaryOperatorCannotBeApplied { expr, op, found, at_error, unmet } => {
                let op = match op {
                    UnaryOp::Deref => 0,
                    UnaryOp::Not => 1,
                    UnaryOp::Neg => 2,
                };
                self.words.extend([22, local(*expr), op]);
                self.ty(tys, found);
                self.opt_ty(tys, at_error.as_ref());
                self.len(unmet.len());
                unmet.iter().for_each(|it| self.goal(tys, it));
            }
            D::BinaryOperatorCannotBeApplied { expr, op, lhs, rhs, at_error, unmet, output } => {
                self.words.extend([23, local(*expr), op.code()]);
                self.ty(tys, lhs);
                self.ty(tys, rhs);
                match at_error.as_deref() {
                    None => self.word(0),
                    Some((lhs, rhs)) => {
                        self.word(1);
                        self.ty(tys, lhs);
                        self.ty(tys, rhs);
                    }
                }
                self.len(unmet.len());
                unmet.iter().for_each(|it| self.goal(tys, it));
                self.opt_ty(tys, output.as_ref());
            }
            D::MutRefInImmRefPat { pat } => self.words.extend([24, local(*pat)]),
            D::CannotImplicitlyDerefTraitObject { pat, found } => {
                self.words.extend([25, local(*pat)]);
                self.ty(tys, found);
            }
            D::CannotIndexInto { expr, found, at_error } => {
                self.words.extend([26, local(*expr)]);
                self.ty(tys, found);
                self.opt_ty(tys, at_error.as_ref());
            }
            D::TypedHole { expr, expected } => {
                self.words.extend([27, local(*expr)]);
                self.ty(tys, expected);
            }
            D::CastToUnsized { expr, cast_ty } => {
                self.words.extend([28, local(*expr)]);
                self.ty(tys, cast_ty);
            }
            D::InvalidCast { expr, error, expr_ty, cast_ty, pointers } => {
                self.words.extend([29, local(*expr), cast_error_code(error)]);
                self.ty(tys, expr_ty);
                self.ty(tys, cast_ty);
                self.bool(pointers.is_some());
                if let Some(pointers) = pointers {
                    self.cast_pointer_kind(pointers.expr);
                    self.cast_pointer_kind(pointers.cast);
                    self.len(pointers.added_auto_traits.len());
                    for &trait_ in &pointers.added_auto_traits {
                        self.id(trait_);
                    }
                }
            }
            D::TyDiagnostic { diag } => {
                self.word(30);
                self.lowering_words(|words| put_diagnostic(words, diag));
            }
            D::PathDiagnostic { node, diag } => {
                self.word(31);
                self.node(*node);
                self.lowering_words(|words| put_path_diagnostic(words, diag));
            }
            D::MethodCallIncorrectGenericsLen { expr, provided_count, expected_count, kind, def } => {
                self.words.extend([32, local(*expr), *provided_count, *expected_count]);
                self.generics_len_kind(*kind);
                self.generic_def(*def);
            }
            D::MethodCallIllegalSizedBound { call_expr, method, bound, needs_mut, candidates } => {
                self.words.extend([33, local(*call_expr)]);
                self.id(*method);
                self.bool(bound.is_some());
                if let Some(bound) = bound {
                    self.generic_def(*bound);
                }
                self.bool(*needs_mut);
                self.len(candidates.len());
                for &trait_ in candidates {
                    self.id(trait_);
                }
            }
            D::EditionDependentMethodCall {
                call_expr,
                receiver,
                lint,
                method,
                inherent,
                autoderefs,
                autoref,
            } => {
                let lint = match lint {
                    EditionLint::Rust2021PreludeCollisions => 0,
                    EditionLint::Rust2024PreludeCollisions => 1,
                    EditionLint::ArrayIntoIter => 2,
                    EditionLint::BoxedSliceIntoIter => 3,
                };
                self.words.extend([61, local(*call_expr), local(*receiver), lint]);
                self.id(*method);
                self.bool(*inherent);
                self.word(*autoderefs);
                match autoref {
                    None => self.word(0),
                    Some(ReceiverAdjustment::Autoref(mutability)) => {
                        self.word(1);
                        self.mutability(*mutability);
                    }
                    Some(ReceiverAdjustment::ToConstPtr) => self.word(2),
                }
            }
            D::FullyQualifiedCallFrom2018 { node, item, self_ty } => {
                self.word(62);
                self.node(*node);
                self.candidate(*item);
                self.ty(tys, self_ty);
            }
            D::MethodCallIncorrectGenericsOrder { expr, param_id, arg_idx, has_self_arg } => {
                self.words.extend([34, local(*expr)]);
                self.generic_param(*param_id);
                self.word(*arg_idx);
                self.bool(*has_self_arg);
            }
            D::MethodCallLateBoundLifetimeArgs { expr, def } => {
                self.words.extend([60, local(*expr)]);
                self.generic_def(*def);
            }
            D::InvalidLhsOfAssignment { lhs, assign, op } => {
                let op = op.map_or(0, |op| 1 + op.code());
                self.words.extend([35, local(*lhs), local(*assign), op]);
            }
            D::TypeMustBeKnown { at_point, top_term, kind, ambiguity, origin, named, cause } => {
                self.word(36);
                self.span(*at_point);
                match top_term {
                    None => self.word(0),
                    Some(term) => {
                        let at = tys.args(GenericArgs::new_from_slice(&[term.as_ref()]));
                        self.words.extend([1, at]);
                    }
                }
                self.word(match kind {
                    TypeAnnotationNeeded::E0282 => 0,
                    TypeAnnotationNeeded::E0283 => 1,
                    TypeAnnotationNeeded::E0284 => 2,
                });
                match ambiguity {
                    None => self.word(0),
                    Some(ambiguity) => {
                        self.word(1);
                        self.ambiguity_note(tys, &ambiguity.primary);
                        self.len(ambiguity.related.len());
                        ambiguity.related.iter().for_each(|it| self.ambiguity_note(tys, it));
                    }
                }
                self.bool(origin.is_some());
                if let Some((span, param)) = origin {
                    self.span(*span);
                    self.generic_param(*param);
                }
                match named {
                    None => self.word(0),
                    Some(named) => {
                        self.word(1);
                        self.named_term(tys, named);
                    }
                }
                self.cause_code(*cause);
            }
            D::UnionExprMustHaveExactlyOneField { expr } => self.words.extend([37, local(*expr)]),
            D::TypeMismatch { node, expected, found, at_error, cause } => {
                self.word(38);
                self.node(*node);
                self.ty(tys, expected);
                self.ty(tys, found);
                match at_error.as_deref() {
                    None => self.word(0),
                    Some((expected, found)) => {
                        self.word(1);
                        self.ty(tys, expected);
                        self.ty(tys, found);
                    }
                }
                self.cause_code(*cause);
            }
            D::SolverDiagnostic(SolverDiagnostic { span, cause, kind }) => {
                self.word(39);
                self.span(*span);
                self.cause_code(*cause);
                match kind {
                    SolverDiagnosticKind::TraitUnimplemented {
                        trait_predicate,
                        parent_trait_predicates,
                        parent_impls,
                    } => {
                        self.word(0);
                        self.trait_predicate(tys, trait_predicate);
                        self.len(parent_trait_predicates.len());
                        parent_trait_predicates.iter().for_each(|it| self.trait_predicate(tys, it));
                        debug_assert_eq!(parent_impls.len(), parent_trait_predicates.len());
                        for &it in parent_impls {
                            self.optional_impl(it.map(|(impl_, _)| impl_));
                            if let Some((_, bound_index)) = it {
                                self.word(bound_index);
                            }
                        }
                    }
                    SolverDiagnosticKind::ClosureKindMismatch { expected, found, is_async, closure } => {
                        self.word(1);
                        self.closure_kind(*expected);
                        self.closure_kind(*found);
                        self.bool(*is_async);
                        self.ty(tys, closure);
                    }
                    SolverDiagnosticKind::GoalUnmet { goal, non_const_impl } => {
                        self.word(2);
                        self.goal(tys, goal);
                        self.optional_impl(*non_const_impl);
                    }
                    SolverDiagnosticKind::Overflow { goal, suggest_increasing_limit } => {
                        self.word(3);
                        self.goal(tys, goal);
                        self.bool(*suggest_increasing_limit);
                    }
                    SolverDiagnosticKind::ProjectionMismatch { goal, normalized, impl_ } => {
                        self.word(4);
                        self.goal(tys, goal);
                        self.bool(normalized.is_some());
                        if let Some(normalized) = normalized {
                            self.generic_arg(tys, normalized);
                        }
                        self.optional_impl(*impl_);
                    }
                    SolverDiagnosticKind::TypesDiffer { expected, found } => {
                        self.word(5);
                        self.ty(tys, expected);
                        self.ty(tys, found);
                    }
                    SolverDiagnosticKind::ConstArgHasWrongType { ct, ct_ty, expected_ty } => {
                        self.word(6);
                        self.generic_arg(tys, ct);
                        self.ty(tys, ct_ty);
                        self.ty(tys, expected_ty);
                    }
                    SolverDiagnosticKind::HigherRankedOutlives => self.word(7),
                    SolverDiagnosticKind::Unreported { code } => self.words.extend([
                        8,
                        match code {
                            UnreportedSolverError::Cycle => 0,
                            UnreportedSolverError::SignatureMismatch => 1,
                            UnreportedSolverError::TraitDynIncompatible => 2,
                            UnreportedSolverError::NotConstEvaluatable => 3,
                            UnreportedSolverError::SelectionOverflow => 4,
                            UnreportedSolverError::ConstEquate => 5,
                        },
                    ]),
                }
            }
            D::ExplicitDropMethodUse { kind } => {
                self.word(40);
                match kind {
                    ExplicitDropMethodUseKind::MethodCall(expr) => self.words.extend([0, local(*expr)]),
                    ExplicitDropMethodUseKind::Path(node) => {
                        self.word(1);
                        self.node(*node);
                    }
                }
            }
            D::MutableRefBinding { pat } => self.words.extend([41, local(*pat)]),
            D::YieldOutsideCoroutine { expr } => self.words.extend([42, local(*expr)]),
            D::ReturnOutsideFunction { expr, kind } => {
                let kind = match kind {
                    ReturnKind::ReturnExpr => 0,
                    ReturnKind::BecomeExpr => 1,
                };
                self.words.extend([43, local(*expr), kind]);
            }
            D::UnsafeBinderCastOfNonBinder { expr, kind, found } => {
                let kind = match kind {
                    UnsafeBinderCastKind::Wrap => 0,
                    UnsafeBinderCastKind::Unwrap => 1,
                };
                self.words.extend([45, local(*expr), kind]);
                self.ty(tys, found);
            }
            D::RecordMissingFields { record, variant, missed_fields } => {
                self.word(44);
                self.expr_or_pat(*record);
                self.variant(*variant);
                self.len(missed_fields.len());
                missed_fields.iter().for_each(|&it| self.local(it));
            }
            D::InvalidConstArg { site, reason } => {
                self.word(46);
                self.span(*site);
                self.word(const_error_word(reason));
                if let CreateConstError::TypeMismatch { actual } = reason {
                    self.ty(tys, actual);
                }
            }
            D::NotAStructPath { node, found, ty } => {
                self.word(47);
                self.node(*node);
                match *found {
                    StructPathFound::Ty => self.word(0),
                    StructPathFound::Def(def) => self.words.extend([1, struct_path_def_word(def)]),
                    StructPathFound::NoVariant { segment } => self.words.extend([2, segment]),
                    StructPathFound::AssocType { segment } => self.words.extend([3, segment]),
                    StructPathFound::AfterVariant { segment } => self.words.extend([4, segment]),
                    StructPathFound::TypeRelative(def) => {
                        self.words.extend([5, struct_path_def_word(def)])
                    }
                }
                self.opt_ty(tys, ty.as_ref());
            }
            D::AutoderefRecursionLimit { span, ty, suggested_limit } => {
                self.word(48);
                self.span(*span);
                self.ty(tys, ty);
                self.word(*suggested_limit);
            }
            D::MethodOnUnknownRawPointee { span } => {
                self.word(49);
                self.span(*span);
            }
            D::TyvarBehindRawPointer { span } => {
                self.word(56);
                self.span(*span);
            }
            D::InheritedRefMutabilityMismatch { pat } => self.words.extend([53, local(*pat)]),
            D::FnTraitArgsNotTuple { call_expr } => self.words.extend([54, local(*call_expr)]),
            D::SelfCtorNotTupleOrUnit { expr, self_ty } => {
                self.words.extend([55, local(*expr)]);
                self.ty(tys, self_ty);
            }
        }
    }

    fn mutability(&mut self, mutability: Mutability) {
        self.word(match mutability {
            Mutability::Not => 0,
            Mutability::Mut => 1,
        });
    }

    fn cause_code(&mut self, code: ObligationCauseCode) {
        match code {
            ObligationCauseCode::Misc => self.word(0),
            ObligationCauseCode::WhereClauseInExpr { item, bound_index, arg_index } => {
                self.word(1);
                self.generic_def(item);
                self.word(bound_index);
                match arg_index {
                    None => self.word(0),
                    Some(arg_index) => {
                        self.word(1);
                        self.word(arg_index);
                    }
                }
            }
            ObligationCauseCode::SizedArgumentType => self.word(2),
            ObligationCauseCode::BlockTailExpression => self.word(3),
            ObligationCauseCode::ReturnValue => self.word(4),
            ObligationCauseCode::IfExpression => self.word(5),
            ObligationCauseCode::MatchExpressionArm { prior_arms } => {
                self.word(6);
                self.word(prior_arms);
            }
            ObligationCauseCode::LetType => self.word(7),
            ObligationCauseCode::SizedReturnType => self.word(8),
            ObligationCauseCode::IfExpressionWithNoElse => self.word(9),
            ObligationCauseCode::ReturnNoExpression => self.word(10),
        }
    }

    fn optional_impl(&mut self, impl_: Option<AnyImplId>) {
        match impl_ {
            None => self.word(0),
            Some(AnyImplId::ImplId(it)) => {
                self.word(1);
                self.id(it);
            }
            Some(AnyImplId::BuiltinDeriveImplId(it)) => {
                self.word(2);
                self.id(it);
            }
        }
    }

    fn place<'db>(&mut self, tys: &mut impl TyEncode<'db>, place: &Place) {
        let Place { base_ty, base, projections } = place;
        self.ty(tys, base_ty);
        match *base {
            PlaceBase::Rvalue => self.word(0),
            PlaceBase::StaticItem => self.word(1),
            PlaceBase::Local(binding) => self.words.extend([2, local(binding)]),
            PlaceBase::Upvar { closure, var_id } => {
                self.words.extend([3, local(closure), local(var_id)])
            }
        }
        self.len(projections.len());
        for Projection { ty, kind } in projections {
            self.ty(tys, ty);
            match *kind {
                ProjectionKind::Deref => self.word(0),
                ProjectionKind::Field { field_idx, variant_idx } => {
                    self.words.extend([1, field_idx, variant_idx])
                }
                ProjectionKind::Index => self.word(2),
                ProjectionKind::Subslice => self.word(3),
                ProjectionKind::UnwrapUnsafeBinder => self.word(4),
            }
        }
    }

    fn sources(&mut self, sources: &[CaptureSourceStack]) {
        self.len(sources.len());
        for stack in sources {
            self.len(stack.len());
            stack.iter().for_each(|&it| self.node(it));
        }
    }

    /// A closure's data: its captures by root variable in their order, its fake reads, and its
    /// liberated signature as its types and `FnSigKind`'s flags and splatted argument.
    fn closure<'db>(&mut self, tys: &mut impl TyEncode<'db>, closure: &ClosureData) {
        let ClosureData { min_captures, fake_reads, liberated_sig, kind_origin } = closure;
        self.len(min_captures.len());
        for (binding, captures) in min_captures {
            self.local(binding);
            self.len(captures.len());
            for CapturedPlace { place, info, mutability } in captures {
                self.place(tys, place);
                let CaptureInfo { sources, capture_kind } = info;
                self.sources(sources);
                self.word(match capture_kind {
                    UpvarCapture::ByRef(BorrowKind::Immutable) => 0,
                    UpvarCapture::ByRef(BorrowKind::UniqueImmutable) => 1,
                    UpvarCapture::ByRef(BorrowKind::Mutable) => 2,
                    UpvarCapture::ByUse => 3,
                    UpvarCapture::ByValue => 4,
                });
                self.mutability(*mutability);
            }
        }
        self.len(fake_reads.len());
        for (place, cause, sources) in fake_reads {
            self.place(tys, place);
            match *cause {
                FakeReadCause::ForMatchGuard => self.word(0),
                FakeReadCause::ForMatchedPlace(expr) => {
                    self.word(1);
                    self.opt_expr(expr);
                }
                FakeReadCause::ForGuardBinding => self.word(2),
                FakeReadCause::ForLet(expr) => {
                    self.word(3);
                    self.opt_expr(expr);
                }
                FakeReadCause::ForIndex => self.word(4),
            }
            self.sources(sources);
        }
        let sig = liberated_sig.get();
        self.len(sig.inputs_and_output.len());
        for ty in sig.inputs_and_output.iter() {
            // `get` ties the types to the borrow of `closure`, shorter than the table's `'db`.
            let ty = ty.store();
            self.ty(tys, &ty);
        }
        let kind = sig.fn_sig_kind;
        self.word(
            u32::from(kind.abi().as_packed())
                | u32::from(kind.is_safe()) << 6
                | u32::from(kind.c_variadic()) << 7
                | u32::from(kind.splatted().unwrap_or(FnSigKind::NO_SPLATTED_ARG_INDEX)) << 8,
        );
        match kind_origin {
            None => self.word(0),
            Some((node, place)) => {
                self.word(1);
                self.node(*node);
                self.place(tys, place);
            }
        }
    }
}

/// The side tables' words being read.
struct Source<'a> {
    words: &'a [u32],
    next: usize,
    text: &'a [u8],
    /// Where the record's root is, for errors.
    at: u64,
    /// The decode's frame, for the lists of types it interns.
    frame: &'a Frame,
}

impl Source<'_> {
    fn is_done(&self) -> bool {
        self.next == self.words.len()
    }

    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at, what }
    }

    fn word(&mut self) -> Result<u32, FormatError> {
        let word = *self.words.get(self.next).ok_or(self.invalid("a side table past its words"))?;
        self.next += 1;
        Ok(word)
    }

    fn u64(&mut self) -> Result<u64, FormatError> {
        let low = self.word()?;
        Ok(u64::from(low) | (u64::from(self.word()?) << 32))
    }

    fn usize(&mut self) -> Result<usize, FormatError> {
        let word = self.u64()?;
        usize::try_from(word).map_err(|_| self.invalid("a count past the address space"))
    }

    fn bool(&mut self) -> Result<bool, FormatError> {
        match self.word()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(self.invalid("a bool that is neither 0 nor 1")),
        }
    }

    fn local<T>(&mut self) -> Result<Idx<T>, FormatError> {
        Ok(local_of(self.word()?))
    }

    fn id<T: crate::check_cache::Relocate>(&mut self) -> Result<T, FormatError> {
        let bits = self.u64()?;
        id_of(bits).ok_or(self.invalid("an id of index 0 or not in the table"))
    }

    /// A list's length, which is at most the words left: each entry is at least a word.
    fn len(&mut self) -> Result<usize, FormatError> {
        let len = self.word()? as usize;
        match len <= self.words.len() - self.next {
            true => Ok(len),
            false => Err(self.invalid("a list longer than the words left")),
        }
    }

    fn ty(&mut self, tys: &impl TyDecode) -> Result<StoredTy, FormatError> {
        tys.ty(self.word()?)
    }

    fn tys(&mut self, tys: &impl TyDecode) -> Result<StoredTys, FormatError> {
        let len = self.len()?;
        let mut list = Vec::with_capacity_in(len, self.frame);
        for _ in 0..len {
            list.push(self.ty(tys)?.as_ref());
        }
        Ok(Tys::new_from_slice(&list).store())
    }

    fn opt_ty(&mut self, tys: &impl TyDecode) -> Result<Option<StoredTy>, FormatError> {
        match self.word()? {
            0 => Ok(None),
            1 => self.ty(tys).map(Some),
            _ => Err(self.invalid("an optional type of no kind")),
        }
    }

    fn name(&mut self) -> Result<Name, FormatError> {
        self.symbol().map(Name::new_symbol_root)
    }

    fn symbol(&mut self) -> Result<Symbol, FormatError> {
        let (start, len) = (self.word()? as usize, self.word()? as usize);
        let bytes = self.text.get(start..).and_then(|it| it.get(..len)).ok_or(self.invalid("a name past the text"))?;
        let text = std::str::from_utf8(bytes).map_err(|_| self.invalid("a name that is not UTF-8"))?;
        Ok(Symbol::intern(text))
    }

    fn candidate(&mut self) -> Result<CandidateId, FormatError> {
        let tag = self.word()?;
        let bits = self.u64()?;
        u8::try_from(tag)
            .ok()
            .and_then(|tag| candidate_of(tag, bits))
            .ok_or(self.invalid("an item of no kind or id 0"))
    }

    fn expr_or_pat(&mut self) -> Result<ExprOrPatId, FormatError> {
        Ok(match self.word()? {
            0 => ExprOrPatId::ExprId(self.local()?),
            1 => ExprOrPatId::PatId(self.local()?),
            _ => return Err(self.invalid("a node that is neither an expression nor a pattern")),
        })
    }

    fn node(&mut self) -> Result<ExprOrPatIdPacked, FormatError> {
        Ok(match self.expr_or_pat()? {
            ExprOrPatId::ExprId(it) => it.into(),
            ExprOrPatId::PatId(it) => it.into(),
        })
    }

    fn opt_expr(&mut self) -> Result<Option<ExprId>, FormatError> {
        match self.word()? {
            0 => Ok(None),
            1 => self.local().map(Some),
            _ => Err(self.invalid("an optional expression of no kind")),
        }
    }

    fn variant(&mut self) -> Result<VariantId, FormatError> {
        let kind = self.word()?;
        let bits = self.u64()?;
        u8::try_from(kind)
            .ok()
            .and_then(|kind| variant_of(kind, bits))
            .ok_or(self.invalid("a variant of no kind or id 0"))
    }

    fn generic_def(&mut self) -> Result<GenericDefId, FormatError> {
        Ok(match self.word()? {
            0 => AdtId::StructId(self.id()?).into(),
            1 => AdtId::EnumId(self.id()?).into(),
            2 => AdtId::UnionId(self.id()?).into(),
            3 => GenericDefId::ConstId(self.id()?),
            4 => GenericDefId::FunctionId(self.id()?),
            5 => GenericDefId::ImplId(self.id()?),
            6 => GenericDefId::StaticId(self.id()?),
            7 => GenericDefId::TraitId(self.id()?),
            8 => GenericDefId::TypeAliasId(self.id()?),
            _ => return Err(self.invalid("a generic definition of no kind")),
        })
    }

    fn cast_pointer_kind(&mut self) -> Result<Option<CastPointerKind>, FormatError> {
        Ok(Some(match self.word()? {
            0 => return Ok(None),
            1 => CastPointerKind::Thin,
            2 => CastPointerKind::VTable(if self.bool()? { Some(self.id()?) } else { None }),
            3 => CastPointerKind::Length,
            4 => CastPointerKind::OfAlias,
            5 => match self.generic_param()? {
                GenericParamId::TypeParamId(param) => CastPointerKind::OfParam(param),
                _ => return Err(self.invalid("a pointer kind of a parameter that is not a type")),
            },
            _ => return Err(self.invalid("a pointer kind of no kind")),
        }))
    }

    fn generic_param(&mut self) -> Result<GenericParamId, FormatError> {
        let tag = self.word()?;
        let parent = self.generic_def()?;
        let local = self.word()?;
        let type_or_const = TypeOrConstParamId { parent, local_id: local_of(local) };
        Ok(match tag {
            0 => TypeParamId::from_unchecked(type_or_const).into(),
            1 => ConstParamId::from_unchecked(type_or_const).into(),
            2 => LifetimeParamId { parent, local_id: local_of(local) }.into(),
            _ => return Err(self.invalid("a generic parameter of no kind")),
        })
    }

    fn value_ns(&mut self) -> Result<ValueNs, FormatError> {
        Ok(match self.word()? {
            0 => ValueNs::ImplSelf(self.id()?),
            1 => ValueNs::LocalBinding(self.local()?),
            2 => ValueNs::FunctionId(self.id()?),
            3 => ValueNs::ConstId(self.id()?),
            4 => ValueNs::StaticId(self.id()?),
            5 => ValueNs::StructId(self.id()?),
            6 => ValueNs::EnumVariantId(self.id()?),
            7 => {
                let parent = self.generic_def()?;
                let local_id = self.local()?;
                ValueNs::GenericParam(ConstParamId::from_unchecked(TypeOrConstParamId { parent, local_id }))
            }
            _ => return Err(self.invalid("a value path resolution of no kind")),
        })
    }

    fn adt(&mut self) -> Result<AdtId, FormatError> {
        match self.generic_def()? {
            GenericDefId::AdtId(it) => Ok(it),
            _ => Err(self.invalid("an ADT that is no ADT")),
        }
    }

    fn type_ns(&mut self) -> Result<TypeNs, FormatError> {
        Ok(match self.word()? {
            0 => TypeNs::SelfType(self.id()?),
            1 => {
                let parent = self.generic_def()?;
                let local_id = self.local()?;
                TypeNs::GenericParam(TypeParamId::from_unchecked(TypeOrConstParamId { parent, local_id }))
            }
            2 => TypeNs::AdtId(self.adt()?),
            3 => TypeNs::AdtSelfType(self.adt()?),
            4 => TypeNs::EnumVariantId(self.id()?),
            5 => TypeNs::TypeAliasId(self.id()?),
            6 => {
                let word = self.word()?;
                TypeNs::BuiltinType(builtin_of(word).ok_or(self.invalid("a builtin type of no kind"))?)
            }
            7 => TypeNs::TraitId(self.id()?),
            _ => return Err(self.invalid("a qualifier resolution of no kind")),
        })
    }

    fn generics_len_kind(&mut self) -> Result<IncorrectGenericsLenKind, FormatError> {
        Ok(match self.word()? {
            0 => IncorrectGenericsLenKind::Lifetimes,
            1 => IncorrectGenericsLenKind::TypesAndConsts,
            _ => return Err(self.invalid("a generics count of no kind")),
        })
    }

    fn span(&mut self) -> Result<Span, FormatError> {
        Ok(match self.word()? {
            0 => Span::ExprId(self.local()?),
            1 => Span::PatId(self.local()?),
            2 => Span::BindingId(self.local()?),
            3 => Span::TypeRefId(self.local()?),
            4 => Span::Dummy,
            _ => return Err(self.invalid("a span of no kind")),
        })
    }

    fn closure_kind(&mut self) -> Result<ClosureKind, FormatError> {
        Ok(match self.word()? {
            0 => ClosureKind::Fn,
            1 => ClosureKind::FnMut,
            2 => ClosureKind::FnOnce,
            _ => return Err(self.invalid("a closure kind of no kind")),
        })
    }

    fn trait_predicate(&mut self, tys: &impl TyDecode) -> Result<StoredTraitPredicate, FormatError> {
        let words = [self.word()?, self.word()?, self.word()?];
        let trait_ref = tys.trait_ref(words)?;
        let polarity = match self.word()? {
            0 => ClausePolarity::Positive,
            1 => ClausePolarity::Negative,
            _ => return Err(self.invalid("a predicate of no polarity")),
        };
        Ok(StoredTraitPredicate { trait_ref, polarity })
    }

    fn polarity(&mut self) -> Result<ClausePolarity, FormatError> {
        Ok(match self.word()? {
            0 => ClausePolarity::Positive,
            1 => ClausePolarity::Negative,
            _ => return Err(self.invalid("a goal of no polarity")),
        })
    }

    fn generic_arg(&mut self, tys: &impl TyDecode) -> Result<StoredGenericArg, FormatError> {
        let args = tys.args(self.word()?)?;
        match args.as_ref().as_slice() {
            [arg] => Ok(arg.store()),
            _ => Err(self.invalid("a generic argument that is not one")),
        }
    }

    fn args(&mut self, tys: &impl TyDecode) -> Result<StoredGenericArgs, FormatError> {
        tys.args(self.word()?)
    }

    fn call_arguments(&mut self, tys: &impl TyDecode) -> Result<Box<CallArguments>, FormatError> {
        let callee = self.ty(tys)?;
        let method = match self.word()? {
            0 => None,
            1 => Some(self.id()?),
            _ => return Err(self.invalid("an optional method of no kind")),
        };
        let is_method = self.bool()?;
        let c_variadic = self.bool()?;
        let mut tys_list = || -> Result<Box<[StoredTy]>, FormatError> {
            let len = self.len()?;
            Ok(exact(len, (0..len).map(|_| self.ty(tys)))?.into_boxed_slice())
        };
        let formal_inputs = tys_list()?;
        let expected_inputs = tys_list()?;
        let provided = tys_list()?;
        let compatible_ends: Box<[u32]> =
            exact(provided.len(), (0..provided.len()).map(|_| self.word()))?.into_boxed_slice();
        let len = self.len()?;
        let compatible: Box<[u32]> = exact(len, (0..len).map(|_| self.word()))?.into_boxed_slice();
        if formal_inputs.len() != expected_inputs.len()
            || compatible_ends.windows(2).any(|it| it[0] > it[1])
            || compatible_ends.last().map_or(0, |&it| it as usize) != compatible.len()
            || compatible.iter().any(|&it| it as usize >= expected_inputs.len())
        {
            return Err(self.invalid("call arguments of no shape"));
        }
        // alloc: error path, a persisted diagnostic's arguments
        Ok(Box::new(CallArguments {
            callee,
            method,
            is_method,
            c_variadic,
            formal_inputs,
            expected_inputs,
            provided,
            compatible_ends,
            compatible,
        }))
    }

    fn goal(&mut self, tys: &impl TyDecode) -> Result<StoredGoal, FormatError> {
        Ok(match self.word()? {
            0 => StoredGoal::Trait { trait_: self.id()?, args: self.args(tys)?, polarity: self.polarity()? },
            1 => StoredGoal::HostEffect { trait_: self.id()?, args: self.args(tys)?, maybe_const: self.bool()? },
            2 => StoredGoal::Projection { alias: self.generic_arg(tys)?, term: self.generic_arg(tys)? },
            3 => StoredGoal::NormalizesTo { alias: self.generic_arg(tys)?, term: self.generic_arg(tys)? },
            4 => StoredGoal::WellFormed { term: self.generic_arg(tys)? },
            5 => StoredGoal::Subtype { a: self.ty(tys)?, b: self.ty(tys)? },
            6 => StoredGoal::Coerce { a: self.ty(tys)?, b: self.ty(tys)? },
            7 => StoredGoal::ConstEvaluatable { ct: self.generic_arg(tys)? },
            8 => StoredGoal::ConstArgHasType { ct: self.generic_arg(tys)?, ty: self.ty(tys)? },
            9 => StoredGoal::ConstEquate { a: self.generic_arg(tys)?, b: self.generic_arg(tys)? },
            10 => StoredGoal::DynCompatible { trait_: self.id()? },
            // clone: error path, the feature of a reported goal
            11 => StoredGoal::UnstableFeature { feature: self.name()?.symbol().clone() },
            12 => StoredGoal::Outlives { a: self.generic_arg(tys)?, b: self.generic_arg(tys)? },
            13 => StoredGoal::Ambiguous,
            _ => return Err(self.invalid("a goal of no kind")),
        })
    }

    /// What [`Sink::named_term`] wrote.
    fn named_term(&mut self, tys: &impl TyDecode) -> Result<NamedTerm, FormatError> {
        let args = tys.args(self.word()?)?;
        let term = match args.as_ref().as_slice() {
            [term] => term.store(),
            _ => return Err(self.invalid("a named term that is not one argument")),
        };
        let len = self.len()?;
        // alloc: error path, a persisted named term's parameters
        let params = exact(len, (0..len).map(|_| Ok((self.span()?, self.generic_param()?))))?;
        Ok(NamedTerm { term, params })
    }

    fn ambiguity_note(&mut self, tys: &impl TyDecode) -> Result<AmbiguityNote, FormatError> {
        let goal = self.goal(tys)?;
        let len = self.len()?;
        let candidates = exact(
            len,
            (0..len).map(|_| {
                Ok(match self.word()? {
                    0 => AmbiguityCandidate::Impl(AnyImplId::ImplId(self.id()?)),
                    1 => AmbiguityCandidate::Impl(AnyImplId::BuiltinDeriveImplId(self.id()?)),
                    2 => AmbiguityCandidate::ParamEnv { trait_: self.id()?, args: self.args(tys)? },
                    _ => return Err(self.invalid("an ambiguity candidate of no kind")),
                })
            }),
        )?;
        Ok(AmbiguityNote { goal, candidates })
    }

    /// What [`Sink::lowering_words`] wrote: `read` must take every one of its words.
    fn lowering_words<T>(
        &mut self,
        read: impl FnOnce(&mut Words<'_>) -> Result<T, FormatError>,
    ) -> Result<T, FormatError> {
        let len = self.len()?;
        let mut words = Words::new(&self.words[self.next..self.next + len], self.at);
        let value = read(&mut words)?;
        if !words.is_done() {
            return Err(self.invalid("a lowering diagnostic with words left over"));
        }
        self.next += len;
        Ok(value)
    }

    fn diagnostic(&mut self, tys: &impl TyDecode) -> Result<InferenceDiagnostic, FormatError> {
        use InferenceDiagnostic as D;
        Ok(match self.word()? {
            0 => D::NoSuchField {
                field: self.node()?,
                private: match self.word()? {
                    0 => None,
                    1 => Some(self.local()?),
                    _ => return Err(self.invalid("an optional field of no kind")),
                },
                variant: self.variant()?,
            },
            1 => D::MismatchedArrayPatLen {
                pat: self.local()?,
                expected: self.u64()?,
                found: self.u64()?,
                has_rest: self.bool()?,
            },
            2 => D::ArrayPatternWithoutFixedLength { pat: self.local()? },
            3 => D::ExpectedArrayOrSlicePat { pat: self.local()?, found: self.ty(tys)? },
            4 => {
                let pat = self.local()?;
                let mut side = || -> Result<_, FormatError> {
                    Ok(match self.opt_ty(tys)? {
                        Some(ty) => Some((ty, self.bool()?)),
                        None => None,
                    })
                };
                let lhs = side()?;
                let rhs = side()?;
                D::InvalidRangePatType { pat, lhs, rhs }
            }
            5 => D::DuplicateField { field: self.node()?, variant: self.variant()? },
            6 => D::PrivateField {
                expr: self.local()?,
                field: FieldId { parent: self.variant()?, local_id: self.local()? },
            },
            7 => {
                let id = self.node()?;
                let item = match self.word()? {
                    0 => AssocItemId::FunctionId(self.id()?),
                    1 => AssocItemId::ConstId(self.id()?),
                    2 => AssocItemId::TypeAliasId(self.id()?),
                    _ => return Err(self.invalid("an associated item of no kind")),
                };
                D::PrivateAssocItem { id, item }
            }
            8 => D::UnresolvedField {
                expr: self.local()?,
                receiver: self.ty(tys)?,
                name: self.name()?,
                method_with_same_name_exists: self.bool()?,
            },
            9 => D::UnresolvedMethodCall {
                expr: self.local()?,
                receiver: self.ty(tys)?,
                name: self.name()?,
                field_with_same_name: self.opt_ty(tys)?,
                assoc_func_with_same_name: match self.word()? {
                    0 => None,
                    1 => Some(self.id()?),
                    _ => return Err(self.invalid("an optional function of no kind")),
                },
            },
            10 => D::UnresolvedAssocItem {
                id: self.node()?,
                ty: self.ty(tys)?,
                name: self.name()?,
            },
            11 => D::UnresolvedIdent { id: self.node()? },
            70 => D::UnresolvedPath { id: self.node()?, segment: self.word()? },
            12 => D::BreakOutsideOfLoop {
                expr: self.local()?,
                is_break: self.bool()?,
                bad_value_break: self.bool()?,
            },
            50 => D::UnlabeledJumpInLabeledBlock { expr: self.local()?, is_break: self.bool()? },
            51 => D::AsyncCoroutinesNotSupported { expr: self.local()? },
            13 => D::NonExhaustiveRecordExpr { expr: self.local()? },
            14 => D::NonExhaustiveRecordPat { pat: self.local()?, variant: self.variant()? },
            15 => D::UnionPatMustHaveExactlyOneField { pat: self.local()? },
            16 => D::UnionPatHasRest { pat: self.local()? },
            17 => D::FunctionalRecordUpdateOnNonStruct { base_expr: self.local()? },
            18 => D::MismatchedArgCount {
                call_expr: self.local()?,
                expected: self.usize()?,
                found: self.usize()?,
                is_fn_trait_call: self.bool()?,
                args: self.call_arguments(tys)?,
            },
            52 => D::IncorrectArguments {
                call_expr: self.local()?,
                args: self.call_arguments(tys)?,
            },
            19 => D::MismatchedTupleStructPatArgCount {
                pat: self.local()?,
                expected: self.usize()?,
                found: self.usize()?,
                variant: self.variant()?,
                had_err: self.bool()?,
            },
            20 => D::ExpectedFunction { call_expr: self.local()?, found: self.ty(tys)? },
            21 => D::CannotBeDereferenced {
                expr: self.local()?,
                found: self.ty(tys)?,
                at_error: self.opt_ty(tys)?,
            },
            22 => D::UnaryOperatorCannotBeApplied {
                expr: self.local()?,
                op: match self.word()? {
                    0 => UnaryOp::Deref,
                    1 => UnaryOp::Not,
                    2 => UnaryOp::Neg,
                    _ => return Err(self.invalid("a unary operator of no kind")),
                },
                found: self.ty(tys)?,
                at_error: self.opt_ty(tys)?,
                unmet: {
                    let len = self.len()?;
                    exact(len, (0..len).map(|_| self.goal(tys)))?
                },
            },
            23 => D::BinaryOperatorCannotBeApplied {
                expr: self.local()?,
                op: {
                    let code = self.word()?;
                    BinaryOp::from_code(code).ok_or(self.invalid("a binary operator of no kind"))?
                },
                lhs: self.ty(tys)?,
                rhs: self.ty(tys)?,
                at_error: match self.word()? {
                    0 => None,
                    // alloc: error path, a persisted diagnostic's types
                    1 => Some(Box::new((self.ty(tys)?, self.ty(tys)?))),
                    _ => return Err(self.invalid("optional types at an error of no kind")),
                },
                unmet: {
                    let len = self.len()?;
                    exact(len, (0..len).map(|_| self.goal(tys)))?
                },
                output: self.opt_ty(tys)?,
            },
            24 => D::MutRefInImmRefPat { pat: self.local()? },
            25 => D::CannotImplicitlyDerefTraitObject { pat: self.local()?, found: self.ty(tys)? },
            26 => D::CannotIndexInto {
                expr: self.local()?,
                found: self.ty(tys)?,
                at_error: self.opt_ty(tys)?,
            },
            27 => D::TypedHole { expr: self.local()?, expected: self.ty(tys)? },
            28 => D::CastToUnsized { expr: self.local()?, cast_ty: self.ty(tys)? },
            29 => D::InvalidCast {
                expr: self.local()?,
                error: {
                    let code = self.word()?;
                    CAST_ERRORS.get(code as usize).cloned().ok_or(self.invalid("a cast error of no kind"))?
                },
                expr_ty: self.ty(tys)?,
                cast_ty: self.ty(tys)?,
                pointers: if self.bool()? {
                    // alloc: error path, a persisted diagnostic's pointers
                    Some(Box::new(CastPointers {
                        expr: self.cast_pointer_kind()?,
                        cast: self.cast_pointer_kind()?,
                        added_auto_traits: {
                            let len = self.len()?;
                            exact(len, (0..len).map(|_| self.id()))?.into_boxed_slice()
                        },
                    }))
                } else {
                    None
                },
            },
            30 => D::TyDiagnostic { diag: self.lowering_words(|words| words.diagnostic())? },
            31 => D::PathDiagnostic {
                node: self.node()?,
                diag: self.lowering_words(|words| words.path_diagnostic())?,
            },
            32 => D::MethodCallIncorrectGenericsLen {
                expr: self.local()?,
                provided_count: self.word()?,
                expected_count: self.word()?,
                kind: self.generics_len_kind()?,
                def: self.generic_def()?,
            },
            33 => D::MethodCallIllegalSizedBound {
                call_expr: self.local()?,
                method: self.id()?,
                bound: if self.bool()? { Some(self.generic_def()?) } else { None },
                needs_mut: self.bool()?,
                candidates: {
                    let len = self.len()?;
                    exact(len, (0..len).map(|_| self.id()))?.into_boxed_slice()
                },
            },
            61 => D::EditionDependentMethodCall {
                call_expr: self.local()?,
                receiver: self.local()?,
                lint: match self.word()? {
                    0 => EditionLint::Rust2021PreludeCollisions,
                    1 => EditionLint::Rust2024PreludeCollisions,
                    2 => EditionLint::ArrayIntoIter,
                    3 => EditionLint::BoxedSliceIntoIter,
                    _ => return Err(self.invalid("an edition lint of no kind")),
                },
                method: self.id()?,
                inherent: self.bool()?,
                autoderefs: self.word()?,
                autoref: match self.word()? {
                    0 => None,
                    1 => Some(ReceiverAdjustment::Autoref(self.mutability()?)),
                    2 => Some(ReceiverAdjustment::ToConstPtr),
                    _ => return Err(self.invalid("a receiver adjustment of no kind")),
                },
            },
            62 => D::FullyQualifiedCallFrom2018 {
                node: self.node()?,
                item: self.candidate()?,
                self_ty: self.ty(tys)?,
            },
            34 => D::MethodCallIncorrectGenericsOrder {
                expr: self.local()?,
                param_id: self.generic_param()?,
                arg_idx: self.word()?,
                has_self_arg: self.bool()?,
            },
            60 => D::MethodCallLateBoundLifetimeArgs {
                expr: self.local()?,
                def: self.generic_def()?,
            },
            35 => D::InvalidLhsOfAssignment {
                lhs: self.local()?,
                assign: self.local()?,
                op: match self.word()? {
                    0 => None,
                    code @ 1..11 => Some(ArithOp::ALL[code as usize - 1]),
                    _ => return Err(self.invalid("an assignment operator of no kind")),
                },
            },
            36 => D::TypeMustBeKnown {
                at_point: self.span()?,
                top_term: match self.word()? {
                    0 => None,
                    1 => {
                        let args = tys.args(self.word()?)?;
                        match args.as_ref().as_slice() {
                            [term] => Some(term.store()),
                            _ => return Err(self.invalid("a term that is not one argument")),
                        }
                    }
                    _ => return Err(self.invalid("an optional term of no kind")),
                },
                kind: match self.word()? {
                    0 => TypeAnnotationNeeded::E0282,
                    1 => TypeAnnotationNeeded::E0283,
                    2 => TypeAnnotationNeeded::E0284,
                    _ => return Err(self.invalid("a type annotation error of no code")),
                },
                ambiguity: match self.word()? {
                    0 => None,
                    1 => {
                        let primary = self.ambiguity_note(tys)?;
                        let len = self.len()?;
                        let related = exact(len, (0..len).map(|_| self.ambiguity_note(tys)))?;
                        // alloc: error path, a persisted diagnostic's ambiguity
                        Some(Box::new(Ambiguity { primary, related }))
                    }
                    _ => return Err(self.invalid("an optional ambiguity of no kind")),
                },
                origin: if self.bool()? { Some((self.span()?, self.generic_param()?)) } else { None },
                named: match self.word()? {
                    0 => None,
                    // alloc: error path, a persisted diagnostic's named term
                    1 => Some(Box::new(self.named_term(tys)?)),
                    _ => return Err(self.invalid("an optional named term of no kind")),
                },
                cause: self.cause_code()?,
            },
            37 => D::UnionExprMustHaveExactlyOneField { expr: self.local()? },
            38 => D::TypeMismatch {
                node: self.node()?,
                expected: self.ty(tys)?,
                found: self.ty(tys)?,
                at_error: match self.word()? {
                    0 => None,
                    // alloc: error path, a persisted diagnostic's types
                    1 => Some(Box::new((self.ty(tys)?, self.ty(tys)?))),
                    _ => return Err(self.invalid("optional types at an error of no kind")),
                },
                cause: self.cause_code()?,
            },
            39 => {
                let span = self.span()?;
                let cause = self.cause_code()?;
                let kind = match self.word()? {
                    0 => {
                        let trait_predicate = self.trait_predicate(tys)?;
                        let len = self.len()?;
                        let parent_trait_predicates = exact(len, (0..len).map(|_| self.trait_predicate(tys)))?;
                        let parent_impls = exact(
                            len,
                            (0..len).map(|_| match self.optional_impl()? {
                                Some(impl_) => Ok(Some((impl_, self.word()?))),
                                None => Ok(None),
                            }),
                        )?;
                        SolverDiagnosticKind::TraitUnimplemented {
                            trait_predicate,
                            parent_trait_predicates,
                            parent_impls,
                        }
                    }
                    1 => SolverDiagnosticKind::ClosureKindMismatch {
                        expected: self.closure_kind()?,
                        found: self.closure_kind()?,
                        is_async: self.bool()?,
                        closure: self.ty(tys)?,
                    },
                    2 => SolverDiagnosticKind::GoalUnmet {
                        goal: self.goal(tys)?,
                        non_const_impl: self.optional_impl()?,
                    },
                    3 => SolverDiagnosticKind::Overflow {
                        goal: self.goal(tys)?,
                        suggest_increasing_limit: self.bool()?,
                    },
                    4 => SolverDiagnosticKind::ProjectionMismatch {
                        goal: self.goal(tys)?,
                        normalized: if self.bool()? { Some(self.generic_arg(tys)?) } else { None },
                        impl_: self.optional_impl()?,
                    },
                    5 => SolverDiagnosticKind::TypesDiffer { expected: self.ty(tys)?, found: self.ty(tys)? },
                    6 => SolverDiagnosticKind::ConstArgHasWrongType {
                        ct: self.generic_arg(tys)?,
                        ct_ty: self.ty(tys)?,
                        expected_ty: self.ty(tys)?,
                    },
                    7 => SolverDiagnosticKind::HigherRankedOutlives,
                    8 => SolverDiagnosticKind::Unreported {
                        code: match self.word()? {
                            0 => UnreportedSolverError::Cycle,
                            1 => UnreportedSolverError::SignatureMismatch,
                            2 => UnreportedSolverError::TraitDynIncompatible,
                            3 => UnreportedSolverError::NotConstEvaluatable,
                            4 => UnreportedSolverError::SelectionOverflow,
                            5 => UnreportedSolverError::ConstEquate,
                            _ => return Err(self.invalid("an unreported solver error of no kind")),
                        },
                    },
                    _ => return Err(self.invalid("a solver diagnostic of no kind")),
                };
                D::SolverDiagnostic(SolverDiagnostic { span, cause, kind })
            }
            40 => D::ExplicitDropMethodUse {
                kind: match self.word()? {
                    0 => ExplicitDropMethodUseKind::MethodCall(self.local()?),
                    1 => ExplicitDropMethodUseKind::Path(self.node()?),
                    _ => return Err(self.invalid("an explicit drop of no kind")),
                },
            },
            41 => D::MutableRefBinding { pat: self.local()? },
            42 => D::YieldOutsideCoroutine { expr: self.local()? },
            43 => D::ReturnOutsideFunction {
                expr: self.local()?,
                kind: match self.word()? {
                    0 => ReturnKind::ReturnExpr,
                    1 => ReturnKind::BecomeExpr,
                    _ => return Err(self.invalid("a return of no kind")),
                },
            },
            45 => D::UnsafeBinderCastOfNonBinder {
                expr: self.local()?,
                kind: match self.word()? {
                    0 => UnsafeBinderCastKind::Wrap,
                    1 => UnsafeBinderCastKind::Unwrap,
                    _ => return Err(self.invalid("an unsafe binder cast of no kind")),
                },
                found: self.ty(tys)?,
            },
            44 => {
                let record = self.expr_or_pat()?;
                let variant = self.variant()?;
                let len = self.len()?;
                let missed_fields = exact(len, (0..len).map(|_| self.local()))?;
                D::RecordMissingFields { record, variant, missed_fields }
            }
            46 => D::InvalidConstArg {
                site: self.span()?,
                reason: match self.word()? {
                    9 => CreateConstError::TypeMismatch { actual: self.ty(tys)? },
                    word => match const_error_of(word) {
                        Some(reason) => reason,
                        None => return Err(self.invalid("an invalid const argument of no reason")),
                    },
                },
            },
            47 => {
                let node = self.node()?;
                let found = match self.word()? {
                    0 => StructPathFound::Ty,
                    1 => match struct_path_def_of(self.word()?) {
                        Some(def) => StructPathFound::Def(def),
                        None => return Err(self.invalid("a struct path naming no kind of item")),
                    },
                    2 => StructPathFound::NoVariant { segment: self.word()? },
                    3 => StructPathFound::AssocType { segment: self.word()? },
                    4 => StructPathFound::AfterVariant { segment: self.word()? },
                    5 => match struct_path_def_of(self.word()?) {
                        Some(def) => StructPathFound::TypeRelative(def),
                        None => return Err(self.invalid("a struct path naming no kind of item")),
                    },
                    _ => return Err(self.invalid("a struct path that found nothing")),
                };
                let ty = self.opt_ty(tys)?;
                if ty.is_some() != (found == StructPathFound::Ty) {
                    return Err(self.invalid("a struct path's type without a type found"));
                }
                D::NotAStructPath { node, found, ty }
            }
            48 => D::AutoderefRecursionLimit {
                span: self.span()?,
                ty: self.ty(tys)?,
                suggested_limit: self.word()?,
            },
            49 => D::MethodOnUnknownRawPointee { span: self.span()? },
            53 => D::InheritedRefMutabilityMismatch { pat: self.local()? },
            54 => D::FnTraitArgsNotTuple { call_expr: self.local()? },
            55 => D::SelfCtorNotTupleOrUnit { expr: self.local()?, self_ty: self.ty(tys)? },
            56 => D::TyvarBehindRawPointer { span: self.span()? },
            _ => return Err(self.invalid("an inference diagnostic of no kind")),
        })
    }

    fn mutability(&mut self) -> Result<Mutability, FormatError> {
        match self.word()? {
            0 => Ok(Mutability::Not),
            1 => Ok(Mutability::Mut),
            _ => Err(self.invalid("a mutability of no kind")),
        }
    }

    fn cause_code(&mut self) -> Result<ObligationCauseCode, FormatError> {
        Ok(match self.word()? {
            0 => ObligationCauseCode::Misc,
            1 => ObligationCauseCode::WhereClauseInExpr {
                item: self.generic_def()?,
                bound_index: self.word()?,
                arg_index: match self.word()? {
                    0 => None,
                    1 => Some(self.word()?),
                    _ => return Err(self.invalid("an optional argument index of no kind")),
                },
            },
            2 => ObligationCauseCode::SizedArgumentType,
            3 => ObligationCauseCode::BlockTailExpression,
            4 => ObligationCauseCode::ReturnValue,
            5 => ObligationCauseCode::IfExpression,
            6 => ObligationCauseCode::MatchExpressionArm { prior_arms: self.word()? },
            7 => ObligationCauseCode::LetType,
            8 => ObligationCauseCode::SizedReturnType,
            9 => ObligationCauseCode::IfExpressionWithNoElse,
            10 => ObligationCauseCode::ReturnNoExpression,
            _ => return Err(self.invalid("an obligation cause of no kind")),
        })
    }

    fn optional_impl(&mut self) -> Result<Option<AnyImplId>, FormatError> {
        Ok(match self.word()? {
            0 => None,
            1 => Some(AnyImplId::ImplId(self.id()?)),
            2 => Some(AnyImplId::BuiltinDeriveImplId(self.id()?)),
            _ => return Err(self.invalid("an optional impl of no kind")),
        })
    }

    fn place(&mut self, tys: &impl TyDecode) -> Result<Place, FormatError> {
        let base_ty = self.ty(tys)?;
        let base = match self.word()? {
            0 => PlaceBase::Rvalue,
            1 => PlaceBase::StaticItem,
            2 => PlaceBase::Local(self.local()?),
            3 => PlaceBase::Upvar { closure: self.local()?, var_id: self.local()? },
            _ => return Err(self.invalid("a place base of no kind")),
        };
        let len = self.len()?;
        // alloc: the decoded place's projections, once at their number
        let mut projections = Vec::with_capacity(len);
        for _ in 0..len {
            let ty = self.ty(tys)?;
            let kind = match self.word()? {
                0 => ProjectionKind::Deref,
                1 => ProjectionKind::Field { field_idx: self.word()?, variant_idx: self.word()? },
                2 => ProjectionKind::Index,
                3 => ProjectionKind::Subslice,
                4 => ProjectionKind::UnwrapUnsafeBinder,
                _ => return Err(self.invalid("a projection of no kind")),
            };
            projections.push(Projection { ty, kind });
        }
        Ok(Place { base_ty, base, projections: projections.into_boxed_slice() })
    }

    fn sources(&mut self) -> Result<Box<[CaptureSourceStack]>, FormatError> {
        let len = self.len()?;
        // alloc: the decoded sources, once at their number
        let mut sources = Vec::with_capacity(len);
        for _ in 0..len {
            let len = self.len()?;
            if len == 0 {
                return Err(self.invalid("a capture source stack without a source"));
            }
            sources.push(CaptureSourceStack::try_from_fn(len, || self.node())?);
        }
        Ok(sources.into_boxed_slice())
    }

    fn closure(&mut self, tys: &impl TyDecode, frame: &Frame) -> Result<ClosureData, FormatError> {
        let len = self.len()?;
        // alloc: the decoded roots, once at their number
        let mut roots = Vec::with_capacity(len);
        let mut seen = hashbrown::HashSet::with_capacity_and_hasher_in(len, FixedState, frame);
        let mut captures = Vec::new_in(frame);
        for _ in 0..len {
            let binding: BindingId = self.local()?;
            if !seen.insert(binding) {
                return Err(self.invalid("a closure that captures a variable twice"));
            }
            let len = self.len()?;
            captures.reserve(len);
            for _ in 0..len {
                let place = self.place(tys)?;
                let sources = self.sources()?;
                let capture_kind = match self.word()? {
                    0 => UpvarCapture::ByRef(BorrowKind::Immutable),
                    1 => UpvarCapture::ByRef(BorrowKind::UniqueImmutable),
                    2 => UpvarCapture::ByRef(BorrowKind::Mutable),
                    3 => UpvarCapture::ByUse,
                    4 => UpvarCapture::ByValue,
                    _ => return Err(self.invalid("a capture of no kind")),
                };
                let info = CaptureInfo { sources, capture_kind };
                captures.push(CapturedPlace { place, info, mutability: self.mutability()? });
            }
            let end = u32::try_from(captures.len())
                .map_err(|_| self.invalid("a closure with 2^32 captures or more"))?;
            roots.push((binding, end));
        }
        // alloc: the decoded captures, once at their number (a drain is `TrustedLen`)
        let min_captures = MinCaptures::from_parts(roots.into_boxed_slice(), captures.drain(..).collect());
        let len = self.len()?;
        // alloc: the decoded fake reads, once at their number
        let mut fake_reads = Vec::with_capacity(len);
        for _ in 0..len {
            let place = self.place(tys)?;
            let cause = match self.word()? {
                0 => FakeReadCause::ForMatchGuard,
                1 => FakeReadCause::ForMatchedPlace(self.opt_expr()?),
                2 => FakeReadCause::ForGuardBinding,
                3 => FakeReadCause::ForLet(self.opt_expr()?),
                4 => FakeReadCause::ForIndex,
                _ => return Err(self.invalid("a fake read of no cause")),
            };
            fake_reads.push((place, cause, self.sources()?));
        }
        let fake_reads = fake_reads.into_boxed_slice();
        let inputs_and_output = self.tys(tys)?;
        let flags = self.word()?;
        let abi = flags & 0b11_1111;
        if flags >> 16 != 0 || abi as usize >= ExternAbi::ALL_VARIANTS.len() {
            return Err(self.invalid("a signature of no ABI"));
        }
        let safety = if flags & (1 << 6) != 0 { Safety::Safe } else { Safety::Unsafe };
        let splatted = (flags >> 8) as u8;
        let splatted = (splatted != FnSigKind::NO_SPLATTED_ARG_INDEX).then_some(splatted);
        let Some(inputs_len) = inputs_and_output.as_ref().len().checked_sub(1) else {
            return Err(self.invalid("a signature with no output"));
        };
        let fn_sig_kind = FnSigKind::new(
            ExternAbi::from_packed(abi as u8),
            safety,
            flags & (1 << 7) != 0,
            splatted,
            inputs_len,
        )
        .map_err(|_| self.invalid("a signature splatting an argument it does not have"))?;
        let liberated_sig =
            StoredFnSig::new(FnSig { inputs_and_output: inputs_and_output.as_ref(), fn_sig_kind });
        let kind_origin = match self.word()? {
            0 => None,
            1 => Some((self.node()?, self.place(tys)?)),
            _ => return Err(self.invalid("an optional closure kind origin of no kind")),
        };
        Ok(ClosureData { min_captures, fake_reads, liberated_sig, kind_origin })
    }
}

#[cfg(test)]
mod tests {
    use blaze_format::to_bytes;
    use blaze_query::db::FromId;
    use either::Either;
    use hir_def::{
        BuiltinDeriveImplId, ImplId, StaticId, TypeAliasId,
        hir::{BindingId, PatId},
    };
    use la_arena::ArenaMap;

    use super::*;
    use crate::{
        infer::{BindingMode, ByRef, columns::CsrBuilder},
        lower::diagnostics::{
            GenericArgsProhibitedReason, PathGenericsSource, PathLoweringDiagnostic,
            TyLoweringDiagnostic,
        },
        next_solver::{
            Const, ConstExt, ConstKind, GenericArg, StoredGenericArg, TyKind, default_types,
        },
    };
    use rustc_type_ir::{InferConst, InferTy};

    /// Types and arguments as a list, without a database: what the round trip of everything but
    /// the type table is tested with. A trait reference needs an interner, so the tests hold none.
    #[derive(Default)]
    struct Model {
        tys: Vec<StoredTy>,
        args: Vec<StoredGenericArgs>,
        predicates: Vec<StoredPredicate>,
    }

    impl<'db> TyEncode<'db> for Model {
        fn ty(&mut self, ty: Ty<'db>) -> u32 {
            self.tys.push(ty.store());
            self.tys.len() as u32 - 1
        }

        fn args(&mut self, args: GenericArgs<'db>) -> u32 {
            self.args.push(args.store());
            self.args.len() as u32 - 1
        }

        fn trait_ref(&mut self, _: &StoredTraitRef) -> [u32; 3] {
            panic!("the tests hold no trait reference")
        }

        fn predicate(&mut self, predicate: Predicate<'db>) -> u32 {
            self.predicates.push(predicate.store());
            self.predicates.len() as u32 - 1
        }
    }

    impl TyDecode for Model {
        fn ty(&self, at: u32) -> Result<StoredTy, FormatError> {
            self.tys.get(at as usize).cloned().ok_or(FormatError::Invalid { at: 0, what: "no such type" })
        }

        fn args(&self, at: u32) -> Result<StoredGenericArgs, FormatError> {
            self.args.get(at as usize).cloned().ok_or(FormatError::Invalid { at: 0, what: "no such arguments" })
        }

        fn trait_ref(&self, _: [u32; 3]) -> Result<StoredTraitRef, FormatError> {
            Err(FormatError::Invalid { at: 0, what: "no trait references" })
        }

        fn predicate(&self, at: u32) -> Result<StoredPredicate, FormatError> {
            self.predicates.get(at as usize).copied().ok_or(FormatError::Invalid { at: 0, what: "no such predicate" })
        }
    }

    fn id(index: u32) -> blaze_query::db::Id {
        blaze_query::db::Id::from_u32(index + (3 << 20))
    }

    fn expr(index: u32) -> ExprId {
        local_of(index)
    }

    fn pat(index: u32) -> PatId {
        local_of(index)
    }

    fn binding(index: u32) -> BindingId {
        local_of(index)
    }

    fn place(base: PlaceBase) -> Place {
        let types = &default_types().types;
        Place {
            base_ty: types.i32.store(),
            base,
            projections: Box::new([
                Projection { ty: types.u8.store(), kind: ProjectionKind::Field { field_idx: 1, variant_idx: 0 } },
                Projection { ty: types.bool.store(), kind: ProjectionKind::Deref },
            ]),
        }
    }

    fn sources(lens: &[u32]) -> Box<[CaptureSourceStack]> {
        lens.iter()
            .map(|&len| {
                let ids: Vec<ExprOrPatIdPacked> = std::iter::once(expr(1).into())
                    .chain((1..len).map(|it| if it % 2 == 0 { expr(it).into() } else { pat(it).into() }))
                    .collect();
                CaptureSourceStack::from_slice(&ids)
            })
            .collect()
    }

    fn every_diagnostic() -> Vec<InferenceDiagnostic> {
        use InferenceDiagnostic as D;
        let types = &default_types().types;
        let regions = &default_types().regions;
        let region = |it: crate::next_solver::Region<'static>| GenericArg::from(it).store();
        let ty = |it: Ty<'static>| it.store();
        let variant: VariantId = StructId::from_id(id(4)).into();
        let enum_variant: VariantId = EnumVariantId::from_id(id(5)).into();
        let def: GenericDefId = ImplId::from_id(id(9)).into();
        let causes = cause_codes();
        let param = TypeOrConstParamId { parent: def, local_id: local_of(2) };
        let segment = PathGenericsSource::Segment(1);
        let assoc = PathGenericsSource::AssocType { segment: 2, assoc_type: 3 };
        let trait_ = TraitId::from_id(id(11));
        let args = GenericArgs::new_from_slice(&[GenericArg::from(types.u8), GenericArg::from(types.bool)]).store();
        let goals = [
            StoredGoal::Trait { trait_, args: args.clone(), polarity: ClausePolarity::Negative },
            StoredGoal::HostEffect { trait_, args: args.clone(), maybe_const: true },
            StoredGoal::Projection { alias: arg_of(types.u8), term: arg_of(types.i32) },
            StoredGoal::NormalizesTo { alias: arg_of(types.bool), term: arg_of(types.unit) },
            StoredGoal::WellFormed { term: arg_of(types.never) },
            StoredGoal::Subtype { a: ty(types.u8), b: ty(types.i32) },
            StoredGoal::Coerce { a: ty(types.bool), b: ty(types.unit) },
            StoredGoal::ConstEvaluatable { ct: arg_of(types.u8) },
            StoredGoal::ConstArgHasType { ct: arg_of(types.i32), ty: ty(types.u8) },
            StoredGoal::ConstEquate { a: arg_of(types.bool), b: arg_of(types.u8) },
            StoredGoal::DynCompatible { trait_ },
            StoredGoal::UnstableFeature { feature: Symbol::intern("blaze_feature") },
            StoredGoal::Outlives { a: region(regions.statik), b: region(regions.erased) },
            StoredGoal::Outlives { a: arg_of(types.u8), b: region(regions.statik) },
            StoredGoal::Ambiguous,
        ];
        let mut all = vec![
            D::NoSuchField { field: expr(1).into(), private: Some(local_of(3)), variant },
            D::NoSuchField { field: pat(2).into(), private: None, variant: enum_variant },
            D::MismatchedArrayPatLen { pat: pat(1), expected: 1 << 40, found: 3, has_rest: true },
            D::ArrayPatternWithoutFixedLength { pat: pat(2) },
            D::ExpectedArrayOrSlicePat { pat: pat(3), found: ty(types.u8) },
            D::InvalidRangePatType { pat: pat(4), lhs: None, rhs: None },
            D::InvalidRangePatType {
                pat: pat(4),
                lhs: Some((ty(types.str), true)),
                rhs: Some((Ty::new_without_interner(TyKind::Infer(InferTy::FreshIntTy(0))).store(), false)),
            },
            D::DuplicateField { field: pat(5).into(), variant },
            D::PrivateField { expr: expr(2), field: FieldId { parent: enum_variant, local_id: local_of(7) } },
            D::PrivateAssocItem { id: expr(3).into(), item: AssocItemId::TypeAliasId(TypeAliasId::from_id(id(6))) },
            D::UnresolvedField {
                expr: expr(4),
                receiver: ty(types.bool),
                name: Name::new_root("label"),
                method_with_same_name_exists: true,
            },
            D::UnresolvedMethodCall {
                expr: expr(5),
                receiver: ty(types.i32),
                name: Name::new_root("frobnicate"),
                field_with_same_name: Some(ty(types.unit)),
                assoc_func_with_same_name: Some(FunctionId::from_id(id(7))),
            },
            D::UnresolvedMethodCall {
                expr: expr(6),
                receiver: ty(types.i32),
                name: Name::new_root(""),
                field_with_same_name: None,
                assoc_func_with_same_name: None,
            },
            D::UnresolvedAssocItem {
                id: pat(6).into(),
                ty: ty(types.u8),
                name: Name::new_root("MISSING"),
            },
            D::UnresolvedIdent { id: expr(7).into() },
            D::UnresolvedPath { id: expr(7).into(), segment: 0 },
            D::UnresolvedPath { id: pat(6).into(), segment: 2 },
            D::NotAStructPath {
                node: expr(7).into(),
                found: StructPathFound::Ty,
                ty: Some(ty(types.never)),
            },
            D::NotAStructPath {
                node: pat(6).into(),
                found: StructPathFound::Def(StructPathDef::Local),
                ty: None,
            },
            D::NotAStructPath {
                node: pat(5).into(),
                found: StructPathFound::AfterVariant { segment: 2 },
                ty: None,
            },
            D::AutoderefRecursionLimit {
                span: Span::ExprId(expr(7)),
                ty: ty(types.never),
                suggested_limit: 256,
            },
            D::MethodOnUnknownRawPointee { span: Span::PatId(pat(4)) },
            D::TyvarBehindRawPointer { span: Span::ExprId(expr(5)) },
            D::InheritedRefMutabilityMismatch { pat: pat(3) },
            D::FnTraitArgsNotTuple { call_expr: expr(9) },
            D::SelfCtorNotTupleOrUnit { expr: expr(2), self_ty: ty(types.never) },
            D::NotAStructPath {
                node: pat(2).into(),
                found: StructPathFound::TypeRelative(StructPathDef::SelfCtor),
                ty: None,
            },
            D::BreakOutsideOfLoop { expr: expr(8), is_break: false, bad_value_break: true },
            D::UnlabeledJumpInLabeledBlock { expr: expr(8), is_break: true },
            D::UnlabeledJumpInLabeledBlock { expr: expr(9), is_break: false },
            D::AsyncCoroutinesNotSupported { expr: expr(10) },
            D::NonExhaustiveRecordExpr { expr: expr(9) },
            D::NonExhaustiveRecordPat { pat: pat(7), variant },
            D::UnionPatMustHaveExactlyOneField { pat: pat(8) },
            D::UnionPatHasRest { pat: pat(9) },
            D::FunctionalRecordUpdateOnNonStruct { base_expr: expr(10) },
            D::MismatchedArgCount {
                call_expr: expr(11),
                expected: 2,
                found: 5,
                is_fn_trait_call: true,
                args: Box::new(CallArguments {
                    callee: ty(types.unit),
                    method: None,
                    is_method: false,
                    c_variadic: true,
                    formal_inputs: [ty(types.u8), ty(types.bool)].into(),
                    expected_inputs: [ty(types.u8), ty(types.error)].into(),
                    provided: [types.u8, types.i32, types.bool, types.u8, types.u8].map(ty).into(),
                    compatible_ends: [1, 1, 2, 3, 3].into(),
                    compatible: [0, 1, 0].into(),
                }),
            },
            D::IncorrectArguments {
                call_expr: expr(12),
                args: Box::new(CallArguments {
                    callee: Ty::new_without_interner(TyKind::Infer(InferTy::FreshIntTy(0))).store(),
                    method: Some(FunctionId::from_id(id(12))),
                    is_method: true,
                    c_variadic: false,
                    formal_inputs: Box::default(),
                    expected_inputs: Box::default(),
                    provided: Box::default(),
                    compatible_ends: Box::default(),
                    compatible: Box::default(),
                }),
            },
            D::MismatchedTupleStructPatArgCount { pat: pat(10), expected: 1, found: 0, variant, had_err: false },
            D::MismatchedTupleStructPatArgCount {
                pat: pat(10),
                expected: 2,
                found: 3,
                variant: enum_variant,
                had_err: true,
            },
            D::ExpectedFunction { call_expr: expr(12), found: ty(types.never) },
            D::CannotBeDereferenced { expr: expr(13), found: ty(types.u8), at_error: None },
            D::CannotBeDereferenced {
                expr: expr(13),
                found: ty(types.i32),
                at_error: Some(Ty::new_without_interner(TyKind::Infer(InferTy::FreshIntTy(0))).store()),
            },
            D::MutRefInImmRefPat { pat: pat(11) },
            D::CannotImplicitlyDerefTraitObject { pat: pat(12), found: ty(types.bool) },
            D::CannotIndexInto { expr: expr(14), found: ty(types.i32), at_error: None },
            D::CannotIndexInto {
                expr: expr(14),
                found: ty(types.f64),
                at_error: Some(Ty::new_without_interner(TyKind::Infer(InferTy::FreshFloatTy(0))).store()),
            },
            D::TypedHole { expr: expr(15), expected: ty(types.unit) },
            D::CastToUnsized { expr: expr(16), cast_ty: ty(types.u8) },
            D::TyDiagnostic {
                diag: TyLoweringDiagnostic::PathDiagnostic {
                    source: local_of(4),
                    diag: PathLoweringDiagnostic::IncorrectGenericsOrder {
                        generics_source: assoc,
                        param_id: ConstParamId::from_unchecked(param).into(),
                        arg_idx: 1,
                        has_self_arg: true,
                    },
                },
            },
            D::TyDiagnostic {
                diag: TyLoweringDiagnostic::InferVarsNotAllowed { sources: Box::new([Span::Dummy]) },
            },
            D::MethodCallIncorrectGenericsLen {
                expr: expr(17),
                provided_count: 3,
                expected_count: 1,
                kind: IncorrectGenericsLenKind::Lifetimes,
                def: AdtId::UnionId(UnionId::from_id(id(10))).into(),
            },
            D::MethodCallIllegalSizedBound {
                call_expr: expr(18),
                method: FunctionId::from_id(id(40)),
                bound: Some(TraitId::from_id(id(41)).into()),
                needs_mut: true,
                candidates: Box::new([TraitId::from_id(id(42)), TraitId::from_id(id(43))]),
            },
            D::MethodCallIllegalSizedBound {
                call_expr: expr(19),
                method: FunctionId::from_id(id(40)),
                bound: None,
                needs_mut: false,
                candidates: Box::new([]),
            },
            D::EditionDependentMethodCall {
                call_expr: expr(18),
                receiver: expr(19),
                lint: EditionLint::Rust2021PreludeCollisions,
                method: FunctionId::from_id(id(40)),
                inherent: false,
                autoderefs: 0,
                autoref: None,
            },
            D::EditionDependentMethodCall {
                call_expr: expr(19),
                receiver: expr(20),
                lint: EditionLint::Rust2024PreludeCollisions,
                method: FunctionId::from_id(id(41)),
                inherent: true,
                autoderefs: 2,
                autoref: Some(ReceiverAdjustment::Autoref(Mutability::Mut)),
            },
            D::EditionDependentMethodCall {
                call_expr: expr(20),
                receiver: expr(21),
                lint: EditionLint::ArrayIntoIter,
                method: FunctionId::from_id(id(40)),
                inherent: true,
                autoderefs: 1,
                autoref: Some(ReceiverAdjustment::Autoref(Mutability::Not)),
            },
            D::EditionDependentMethodCall {
                call_expr: expr(21),
                receiver: expr(18),
                lint: EditionLint::BoxedSliceIntoIter,
                method: FunctionId::from_id(id(41)),
                inherent: false,
                autoderefs: 0,
                autoref: Some(ReceiverAdjustment::ToConstPtr),
            },
            D::FullyQualifiedCallFrom2018 {
                node: expr(18).into(),
                item: FunctionId::from_id(id(42)).into(),
                self_ty: ty(types.u8),
            },
            D::FullyQualifiedCallFrom2018 {
                node: pat(4).into(),
                item: ConstId::from_id(id(43)).into(),
                self_ty: ty(types.bool),
            },
            D::MethodCallIncorrectGenericsOrder {
                expr: expr(19),
                param_id: LifetimeParamId { parent: def, local_id: local_of(0) }.into(),
                arg_idx: 0,
                has_self_arg: false,
            },
            D::MethodCallLateBoundLifetimeArgs { expr: expr(19), def },
            D::InvalidLhsOfAssignment { lhs: expr(20), assign: expr(21), op: None },
            D::TypeMustBeKnown {
                at_point: Span::BindingId(binding(3)),
                top_term: Some(arg_of(types.u8)),
                kind: TypeAnnotationNeeded::E0282,
                ambiguity: None,
                origin: Some((Span::ExprId(expr(31)), TypeParamId::from_unchecked(param).into())),
                named: Some(Box::new(NamedTerm {
                    term: GenericArg::from(Ty::new_without_interner(TyKind::Infer(InferTy::FreshTy(1))))
                        .store(),
                    params: vec![
                        (Span::Dummy, ConstParamId::from_unchecked(param).into()),
                        (Span::ExprId(expr(31)), TypeParamId::from_unchecked(param).into()),
                    ],
                })),
                cause: causes[0],
            },
            D::TypeMustBeKnown {
                at_point: Span::PatId(pat(1)),
                top_term: None,
                kind: TypeAnnotationNeeded::E0284,
                ambiguity: Some(Box::new(Ambiguity {
                    primary: AmbiguityNote { goal: goals[2].clone(), candidates: Vec::new() },
                    related: Vec::new(),
                })),
                origin: Some((Span::Dummy, ConstParamId::from_unchecked(param).into())),
                named: None,
                cause: causes[2],
            },
            D::TypeMustBeKnown {
                at_point: Span::ExprId(expr(30)),
                top_term: Some(arg_of(types.bool)),
                kind: TypeAnnotationNeeded::E0283,
                ambiguity: Some(Box::new(Ambiguity {
                    primary: AmbiguityNote {
                        goal: goals[0].clone(),
                        candidates: vec![
                            AmbiguityCandidate::Impl(AnyImplId::ImplId(ImplId::from_id(id(13)))),
                            AmbiguityCandidate::Impl(AnyImplId::BuiltinDeriveImplId(
                                BuiltinDeriveImplId::from_id(id(14)),
                            )),
                            AmbiguityCandidate::ParamEnv { trait_, args: args.clone() },
                        ],
                    },
                    related: goals
                        .iter()
                        .map(|goal| AmbiguityNote { goal: goal.clone(), candidates: Vec::new() })
                        .collect(),
                })),
                origin: None,
                named: Some(Box::new(NamedTerm {
                    term: GenericArg::from(Const::new_without_interner(ConstKind::Infer(
                        InferConst::Fresh(0),
                    )))
                    .store(),
                    params: vec![(Span::PatId(pat(2)), ConstParamId::from_unchecked(param).into())],
                })),
                cause: causes[1],
            },
            D::UnionExprMustHaveExactlyOneField { expr: expr(21) },
            D::TypeMismatch {
                node: pat(13).into(),
                expected: ty(types.i32),
                found: ty(types.bool),
                at_error: None,
                cause: causes[0],
            },
            D::TypeMismatch {
                node: expr(35).into(),
                expected: ty(types.i32),
                found: ty(types.unit),
                at_error: Some(Box::new((
                    Ty::new_without_interner(TyKind::Infer(InferTy::FreshIntTy(0))).store(),
                    Ty::new_without_interner(TyKind::Infer(InferTy::FreshFloatTy(0))).store(),
                ))),
                cause: causes[7],
            },
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::ExprId(expr(22)),
                cause: causes[1],
                kind: SolverDiagnosticKind::ClosureKindMismatch {
                    expected: ClosureKind::Fn,
                    found: ClosureKind::FnOnce,
                    is_async: true,
                    closure: ty(types.u8),
                },
            }),
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::Dummy,
                cause: causes[2],
                kind: SolverDiagnosticKind::Overflow { goal: goals[3].clone(), suggest_increasing_limit: true },
            }),
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::ExprId(expr(31)),
                cause: causes[3],
                kind: SolverDiagnosticKind::ProjectionMismatch {
                    goal: goals[2].clone(),
                    normalized: Some(arg_of(types.bool)),
                    impl_: Some(AnyImplId::ImplId(ImplId::from_id(id(44)))),
                },
            }),
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::ExprId(expr(31)),
                cause: causes[0],
                kind: SolverDiagnosticKind::ProjectionMismatch {
                    goal: goals[2].clone(),
                    normalized: None,
                    impl_: None,
                },
            }),
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::PatId(pat(17)),
                cause: causes[4],
                kind: SolverDiagnosticKind::TypesDiffer { expected: ty(types.u8), found: ty(types.i32) },
            }),
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::ExprId(expr(32)),
                cause: causes[5],
                kind: SolverDiagnosticKind::ConstArgHasWrongType {
                    ct: arg_of(types.u8),
                    ct_ty: ty(types.bool),
                    expected_ty: ty(types.unit),
                },
            }),
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::TypeRefId(local_of(5)),
                cause: causes[6],
                kind: SolverDiagnosticKind::HigherRankedOutlives,
            }),
            D::ExplicitDropMethodUse { kind: ExplicitDropMethodUseKind::MethodCall(expr(23)) },
            D::ExplicitDropMethodUse { kind: ExplicitDropMethodUseKind::Path(pat(14).into()) },
            D::MutableRefBinding { pat: pat(15) },
            D::YieldOutsideCoroutine { expr: expr(24) },
            D::ReturnOutsideFunction { expr: expr(25), kind: ReturnKind::BecomeExpr },
            D::UnsafeBinderCastOfNonBinder {
                expr: expr(25),
                kind: UnsafeBinderCastKind::Wrap,
                found: ty(types.u8),
            },
            D::UnsafeBinderCastOfNonBinder {
                expr: expr(26),
                kind: UnsafeBinderCastKind::Unwrap,
                found: ty(types.never),
            },
            D::RecordMissingFields {
                record: ExprOrPatId::PatId(pat(16)),
                variant,
                missed_fields: vec![local_of(0), local_of(2)],
            },
        ];
        let reasons = [
            CreateConstError::UsedForbiddenParam,
            CreateConstError::LocalBinding,
            CreateConstError::Static,
            CreateConstError::ImplSelf,
            CreateConstError::NeedsAnonConst,
            CreateConstError::DoesNotResolve,
            CreateConstError::ConstHasGenerics,
            CreateConstError::UnderscoreExpr,
            CreateConstError::AnonConstInterningDisabled,
            CreateConstError::TypeMismatch { actual: ty(types.bool) },
            CreateConstError::TargetLayoutNotAvailable,
        ];
        all.extend(reasons.into_iter().enumerate().map(|(n, reason)| D::InvalidConstArg {
            site: if n % 2 == 0 { Span::ExprId(expr(34)) } else { Span::TypeRefId(local_of(6)) },
            reason,
        }));
        let unary = [UnaryOp::Deref, UnaryOp::Not, UnaryOp::Neg];
        all.extend(unary.into_iter().enumerate().map(|(n, op)| D::UnaryOperatorCannotBeApplied {
            expr: expr(26),
            op,
            found: ty(types.u8),
            at_error: (n == 1).then(|| Ty::new_without_interner(TyKind::Infer(InferTy::FreshIntTy(0))).store()),
            unmet: goals.iter().take(n).cloned().collect(),
        }));
        all.extend(goals.iter().enumerate().map(|(n, goal)| {
            let non_const_impl = match n % 3 {
                0 => None,
                1 => Some(AnyImplId::ImplId(ImplId::from_id(id(42)))),
                _ => Some(AnyImplId::BuiltinDeriveImplId(BuiltinDeriveImplId::from_id(id(43)))),
            };
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::ExprId(expr(33)),
                cause: causes[8],
                kind: SolverDiagnosticKind::GoalUnmet { goal: goal.clone(), non_const_impl },
            })
        }));
        let unreported = [
            UnreportedSolverError::Cycle,
            UnreportedSolverError::SignatureMismatch,
            UnreportedSolverError::TraitDynIncompatible,
            UnreportedSolverError::NotConstEvaluatable,
            UnreportedSolverError::SelectionOverflow,
            UnreportedSolverError::ConstEquate,
        ];
        all.extend(unreported.map(|code| {
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::ExprId(expr(34)),
                cause: causes[2],
                kind: SolverDiagnosticKind::Unreported { code },
            })
        }));
        all.extend((0..29).map(|code| D::BinaryOperatorCannotBeApplied {
            expr: expr(27),
            op: BinaryOp::from_code(code).unwrap(),
            lhs: ty(types.i32),
            rhs: ty(types.u8),
            at_error: (code % 4 == 1).then(|| {
                Box::new((
                    Ty::new_without_interner(TyKind::Infer(InferTy::FreshIntTy(0))).store(),
                    ty(types.u8),
                ))
            }),
            unmet: if code % 2 == 0 { Vec::new() } else { goals.to_vec() },
            output: (code % 3 == 0).then(|| ty(types.bool)),
        }));
        all.extend(ArithOp::ALL.map(|op| D::InvalidLhsOfAssignment { lhs: expr(20), assign: expr(22), op: Some(op) }));
        all.extend(CAST_ERRORS.iter().map(|error| D::InvalidCast {
            expr: expr(28),
            error: error.clone(),
            expr_ty: ty(types.i32),
            cast_ty: ty(types.bool),
            pointers: None,
        }));
        let pointer_kinds = [
            None,
            Some(CastPointerKind::Thin),
            Some(CastPointerKind::VTable(None)),
            Some(CastPointerKind::VTable(Some(trait_))),
            Some(CastPointerKind::Length),
            Some(CastPointerKind::OfAlias),
            Some(CastPointerKind::OfParam(TypeParamId::from_unchecked(param))),
        ];
        all.extend(pointer_kinds.iter().enumerate().map(|(i, &kind)| D::InvalidCast {
            expr: expr(28),
            error: CastError::DifferingKinds,
            expr_ty: ty(types.i32),
            cast_ty: ty(types.bool),
            pointers: Some(Box::new(CastPointers {
                expr: kind,
                cast: pointer_kinds[(i + 1) % pointer_kinds.len()],
                added_auto_traits: if i == 0 { Box::new([]) } else { Box::new([trait_, trait_]) },
            })),
        }));
        let paths = [
            PathLoweringDiagnostic::GenericArgsProhibited {
                segments: Box::new([4]),
                reason: GenericArgsProhibitedReason::Static,
            },
            PathLoweringDiagnostic::ParenthesizedGenericArgsWithoutFnTrait { segment: 5 },
            PathLoweringDiagnostic::IncorrectGenericsLen {
                generics_source: segment,
                provided_count: 3,
                expected_count: 1,
                kind: IncorrectGenericsLenKind::TypesAndConsts,
                def,
            },
            PathLoweringDiagnostic::IncorrectGenericsOrder {
                generics_source: segment,
                param_id: TypeParamId::from_unchecked(param).into(),
                arg_idx: 2,
                has_self_arg: false,
            },
            PathLoweringDiagnostic::ElidedLifetimesInPath { generics_source: assoc, def, expected_count: 2, hard_error: true },
            PathLoweringDiagnostic::ElisionFailure { generics_source: segment, def, expected_count: 1 },
            PathLoweringDiagnostic::MissingLifetime { generics_source: assoc, def, expected_count: 3 },
            PathLoweringDiagnostic::GenericDefaultRefersToSelf { segment: 0 },
        ];
        all.extend(paths.into_iter().map(|diag| D::PathDiagnostic { node: expr(29).into(), diag }));
        all
    }

    fn arg_of(ty: Ty<'static>) -> StoredGenericArg {
        GenericArg::from(ty).store()
    }

    fn closure() -> ClosureData {
        let types = &default_types().types;
        let min_captures = crate::infer::MinCaptures::from_parts(
            Box::new([(binding(4), 2), (binding(1), 2)]),
            Box::new([
                CapturedPlace {
                    place: place(PlaceBase::Upvar { closure: expr(3), var_id: binding(4) }),
                    info: CaptureInfo { sources: sources(&[1, 2, 5]), capture_kind: UpvarCapture::ByRef(BorrowKind::UniqueImmutable) },
                    mutability: Mutability::Mut,
                },
                CapturedPlace {
                    place: place(PlaceBase::Local(binding(4))),
                    info: CaptureInfo { sources: sources(&[3]), capture_kind: UpvarCapture::ByValue },
                    mutability: Mutability::Not,
                },
            ]),
        );
        ClosureData {
            min_captures,
            fake_reads: Box::new([
                (place(PlaceBase::Rvalue), FakeReadCause::ForMatchedPlace(Some(expr(2))), sources(&[1])),
                (place(PlaceBase::StaticItem), FakeReadCause::ForLet(None), sources(&[])),
                (place(PlaceBase::Local(binding(0))), FakeReadCause::ForIndex, sources(&[4, 1])),
            ]),
            liberated_sig: StoredFnSig::new(FnSig {
                inputs_and_output: Tys::new_from_slice(&[types.i32, types.u8, types.bool]),
                fn_sig_kind: FnSigKind::new(ExternAbi::RustCall, Safety::Safe, false, Some(1), 2)
                    .expect("two inputs, so index 1 is in bounds"),
            }),
            kind_origin: Some((pat(6).into(), place(PlaceBase::Upvar { closure: expr(3), var_id: binding(4) }))),
        }
    }

    /// A result with something in every column.
    fn result() -> InferenceResult<'static> {
        blaze_mem::frame::task(crate::db::INFER_TAG, result_in)
    }

    /// [`result`], its builders in `frame`.
    fn result_in(frame: &Frame) -> InferenceResult<'static> {
        let types = &default_types().types;
        let args = |tys: &[Ty<'static>]| {
            let args: Vec<_> = tys.iter().map(|&it| GenericArg::from(it)).collect();
            GenericArgs::new_from_slice(&args).store()
        };
        let mut type_of_expr = ArenaMap::default();
        for (at, ty) in [(0, types.i32), (2, types.u8), (3, types.i32), (7, types.bool)] {
            type_of_expr.insert(expr(at), ty.store());
        }
        let mut expr_adjustments = CsrBuilder::new_in(frame);
        expr_adjustments.insert(expr(1), ADJUSTS.iter().map(|kind| Adjustment { kind: kind.clone(), target: types.u8.store() }));
        expr_adjustments.ensure(expr(4));
        let mut assignment_coercions = CsrBuilder::new_in(frame);
        assignment_coercions.insert(expr(6), [Adjustment { kind: Adjust::NeverToAny, target: types.unit.store() }]);
        let mut pat_adjustments = CsrBuilder::new_in(frame);
        pat_adjustments.push(pat(2), PatAdjustment { kind: PatAdjust::OverloadedDeref, source: types.i32.store() });
        pat_adjustments.push(pat(2), PatAdjustment { kind: PatAdjust::BuiltinDeref, source: types.u8.store() });
        let mut mismatches = ExprOrPatSet::default();
        mismatches.insert(expr(70).into());
        mismatches.insert(pat(3).into());
        InferenceResult {
            method_resolutions: IndexColumn::from_entries([
                (expr(3), (FunctionId::from_id(id(20)), args(&[types.i32]))),
                (expr(9), (FunctionId::from_id(id(21)), args(&[]))),
            ]),
            field_resolutions: FieldColumn::from_entries([
                (expr(2), Either::Left(FieldId { parent: StructId::from_id(id(22)).into(), local_id: local_of(1) })),
                (expr(5), Either::Right(TupleFieldId { tuple: TupleId(1), index: 1 })),
                (expr(8), Either::Right(TupleFieldId { tuple: TupleId(0), index: 0 })),
            ]),
            variant_resolutions: ExprOrPatColumn {
                exprs: IndexColumn::from_entries([(expr(4), UnionId::from_id(id(23)).into())]),
                pats: IndexColumn::from_entries([(pat(1), EnumVariantId::from_id(id(24)).into())]),
            },
            value_resolutions: {
                let param = TypeOrConstParamId { parent: FunctionId::from_id(id(33)).into(), local_id: local_of(1) };
                let mut values = ExprOrPatMap::new_in(frame);
                values.insert(expr(12).into(), ValueNs::ImplSelf(ImplId::from_id(id(34))));
                values.insert(expr(13).into(), ValueNs::LocalBinding(binding(2)));
                values.insert(expr(14).into(), ValueNs::FunctionId(FunctionId::from_id(id(35))));
                values.insert(expr(15).into(), ValueNs::ConstId(ConstId::from_id(id(36))));
                values.insert(expr(16).into(), ValueNs::StaticId(StaticId::from_id(id(37))));
                values.insert(pat(5).into(), ValueNs::StructId(StructId::from_id(id(38))));
                values.insert(pat(6).into(), ValueNs::EnumVariantId(EnumVariantId::from_id(id(39))));
                values.insert(expr(17).into(), ValueNs::GenericParam(ConstParamId::from_unchecked(param)));
                values.freeze()
            },
            qualifier_resolutions: {
                let param = TypeOrConstParamId { parent: FunctionId::from_id(id(33)).into(), local_id: local_of(0) };
                let enum_ = AdtId::EnumId(hir_def::EnumId::from_id(id(49)));
                let mut qualifiers = ExprOrPatMap::new_in(frame);
                qualifiers.insert(expr(14).into(), (TypeNs::AdtId(StructId::from_id(id(38)).into()), 1));
                qualifiers.insert(expr(15).into(), (TypeNs::GenericParam(TypeParamId::from_unchecked(param)), 1));
                qualifiers.insert(expr(16).into(), (TypeNs::SelfType(ImplId::from_id(id(34))), 1));
                qualifiers.insert(expr(17).into(), (TypeNs::AdtSelfType(enum_), 1));
                qualifiers.insert(expr(18).into(), (TypeNs::TraitId(TraitId::from_id(id(41))), 2));
                qualifiers.insert(expr(19).into(), (TypeNs::TypeAliasId(TypeAliasId::from_id(id(6))), 3));
                qualifiers.insert(expr(20).into(), (TypeNs::BuiltinType(BuiltinType::Uint(BuiltinUint::U128)), 1));
                qualifiers.insert(expr(21).into(), (TypeNs::BuiltinType(BuiltinType::Char), 1));
                qualifiers.insert(pat(6).into(), (TypeNs::EnumVariantId(EnumVariantId::from_id(id(39))), 0));
                qualifiers.freeze()
            },
            unstable_candidates: {
                let mut candidates = ExprOrPatMap::new_in(frame);
                let call: Box<[(CandidateId, Symbol)]> = Box::new([
                    (CandidateId::FunctionId(FunctionId::from_id(id(44))), Symbol::intern("iter_flatten")),
                    (CandidateId::ConstId(ConstId::from_id(id(45))), Symbol::intern("int_roundings")),
                ]);
                let path: Box<[(CandidateId, Symbol)]> =
                    Box::new([(CandidateId::ConstId(ConstId::from_id(id(46))), Symbol::intern("f16"))]);
                candidates.insert(expr(12).into(), call);
                candidates.insert(pat(6).into(), path);
                candidates.freeze()
            },
            assoc_resolutions: ExprOrPatColumn {
                exprs: IndexColumn::from_entries([(expr(6), (ConstId::from_id(id(25)).into(), args(&[types.u8])))]),
                pats: IndexColumn::from_entries([(pat(0), (FunctionId::from_id(id(26)).into(), args(&[])))]),
            },
            tuple_field_access_types: Box::new([
                Tys::new_from_slice(&[types.i32]).store(),
                Tys::new_from_slice(&[types.u8, types.bool]).store(),
                Tys::new_from_slice(&[]).store(),
            ]),
            type_of_expr: TyColumn::from_entries(type_of_expr),
            type_of_pat: TyColumn::from_entries([(pat(1), types.bool.store()), (pat(3), types.bool.store())]),
            type_of_binding: TyColumn::from_entries([(binding(0), types.unit.store())]),
            type_of_type_placeholder: TyColumn::from_entries([(local_of(2), types.never.store())]),
            type_of_opaque: SortedMap::from_entries([
                (InternedOpaqueTyId::from_id(id(30)), types.i32.store()),
                (InternedOpaqueTyId::from_id(id(27)), types.u8.store()),
            ]),
            has_errors: true,
            diagnostics: every_diagnostic().into_boxed_slice(),
            unreachable: every_unreachable().into_boxed_slice(),
            traits_in_scope_used: Box::new([
                (ExprOrPatId::ExprId(expr(12)), TraitId::from_id(id(47))),
                (ExprOrPatId::PatId(pat(6)), TraitId::from_id(id(48))),
                (ExprOrPatId::ExprId(expr(12)), TraitId::from_id(id(48))),
            ]),
            // A predicate needs an interner, so the column is only tested empty here.
            coroutine_stalled_predicates: Box::new([]),
            named_pat_types: Box::new([
                (
                    pat(3),
                    NamedTerm {
                        term: GenericArg::from(Ty::new_without_interner(TyKind::Infer(InferTy::FreshTy(0))))
                            .store(),
                        params: vec![(
                            Span::PatId(pat(3)),
                            TypeParamId::from_unchecked(TypeOrConstParamId {
                                parent: FunctionId::from_id(id(33)).into(),
                                local_id: local_of(1),
                            })
                            .into(),
                        )],
                    },
                ),
                (pat(9), NamedTerm { term: arg_of(types.u8), params: vec![] }),
            ]),
            nodes_with_type_mismatches: mismatches,
            error_ty: types.unit.store(),
            expr_adjustments: expr_adjustments.freeze(),
            assignment_coercions: assignment_coercions.freeze(),
            pat_adjustments: pat_adjustments.freeze(),
            binding_modes: ModeColumn::from_entries([
                (pat(0), BindingMode(ByRef::Yes(Mutability::Mut), Mutability::Not)),
                (pat(4), BindingMode(ByRef::No, Mutability::Mut)),
            ]),
            skipped_ref_pats: [pat(130)].into_iter().collect(),
            coercion_casts: [expr(1), expr(64)].into_iter().collect(),
            closures_without_expectation: [expr(3)].into_iter().collect(),
            asm_sym_tys: SortedMap::from_entries([((expr(9), 2), types.i32.store()), ((expr(9), 0), types.u8.store())]),
            closures_data: SortedMap::from_entries([(expr(3), closure()), (expr(11), ClosureData {
                min_captures: Default::default(),
                fake_reads: Box::new([]),
                liberated_sig: StoredFnSig::new(FnSig {
                    inputs_and_output: Tys::new_from_slice(&[types.unit]),
                    fn_sig_kind: FnSigKind::new(ExternAbi::C { unwind: true }, Safety::Unsafe, true, None, 0)
                        .expect("no splatted argument is always valid"),
                }),
                kind_origin: None,
            })]),
            defined_anon_consts: Box::new([AnonConstId::from_id(id(31)), AnonConstId::from_id(id(32))]),
        }
    }

    fn round_trip(result: &InferenceResult<'static>) -> InferenceResult<'static> {
        let mut model = Model::default();
        let record = InferenceRecord::of(result, &mut model);
        let bytes = blaze_format::Aligned::copy_from(&to_bytes(&record));
        let read = InferenceRecord::read(&bytes).unwrap();
        let back: InferenceResult<'static> = read.decode(&model).unwrap();
        let in_memory: InferenceResult<'static> = record.as_ref().decode(&model).unwrap();
        assert_eq!(in_memory, back);
        back
    }

    #[test]
    fn a_result_with_every_column_round_trips() {
        let result = result();
        let back = round_trip(&result);
        assert_eq!(back, result);
        assert_eq!(content_hash(&back), content_hash(&result));
        assert_eq!(back.diagnostics.len(), every_diagnostic().len());
        assert_eq!(back.unreachable.len(), every_unreachable().len());
        assert_eq!(back.traits_in_scope_used(), result.traits_in_scope_used(), "in their order");
    }

    #[test]
    fn a_stalled_goal_span_round_trips_and_one_of_no_kind_is_refused() {
        let binding = BindingId::from_raw(RawIdx::from_u32(3));
        let type_ref = hir_def::type_ref::TypeRefId::from_raw(RawIdx::from_u32(9));
        for span in [
            Span::ExprId(expr(5)),
            Span::PatId(pat(7)),
            Span::BindingId(binding),
            Span::TypeRefId(type_ref),
            Span::Dummy,
        ] {
            assert_eq!(span_of(&span_words(span)), Some(span));
        }
        assert_eq!(span_of(&[4, 1]), None, "a dummy span holds no index");
        assert_eq!(span_of(&[5, 0]), None, "no sixth kind");
    }

    #[test]
    fn a_trait_use_at_a_node_of_no_kind_is_refused() {
        assert_eq!(node_of(&[2, 0]), None, "no third kind");
        assert_eq!(node_of(&[0]), None, "a partial node");
        for node in [ExprOrPatId::ExprId(expr(5)), ExprOrPatId::PatId(pat(7))] {
            assert_eq!(node_of(&node_words(node)), Some(node));
        }
    }

    /// One entry of each kind, after an expression and after a pattern, with and without a
    /// note.
    fn every_unreachable() -> Vec<Unreachable> {
        let after = ExprOrPatId::ExprId(expr(4));
        let note = Some(Unreachable::ALL_ARMS_DIVERGE);
        [
            UnreachableAt::Expr(expr(1)),
            UnreachableAt::Call(expr(2)),
            UnreachableAt::Stmt { block: expr(3), index: 2 },
            UnreachableAt::Arm { match_expr: expr(5), index: 1 },
            UnreachableAt::IfBlock(expr(6)),
            UnreachableAt::LetInit(expr(7)),
        ]
        .into_iter()
        .map(|what| Unreachable { what, after, note: None })
        .chain([
            Unreachable { what: UnreachableAt::Expr(expr(8)), after: ExprOrPatId::PatId(pat(2)), note: None },
            Unreachable { what: UnreachableAt::Stmt { block: expr(3), index: 0 }, after, note },
        ])
        .collect()
    }

    #[test]
    fn an_unreachable_entry_of_no_kind_is_refused() {
        assert_eq!(unreachable_of(&[6, 1, 0, 0, 1, 0]), None, "no seventh kind");
        assert_eq!(unreachable_of(&[0, 1, 3, 0, 1, 0]), None, "an expression has no index");
        assert_eq!(unreachable_of(&[0, 1, 0, 2, 1, 0]), None, "after neither");
        assert_eq!(unreachable_of(&[0, 1, 0, 0, 1, 2]), None, "no second note");
        for it in every_unreachable() {
            assert_eq!(unreachable_of(&unreachable_words(&it)), Some(it));
        }
    }

    #[test]
    fn an_empty_result_round_trips() {
        let result = InferenceResult::new(default_types().types.unit);
        assert_eq!(round_trip(&result), result);
    }

    fn content_hash(result: &InferenceResult<'_>) -> blaze_ids::ContentId {
        blaze_ids::Hasher::new("hir-ty test").std(result).finish()
    }

    #[test]
    fn the_type_table_comes_back_byte_for_byte() {
        let bytes: Vec<u8> = (0..3 * ALIGN).map(|it| (it * 37 % 251) as u8).collect();
        let words = table_words(&bytes);
        assert_eq!(words.len(), 3);
        assert_eq!(words[0] & 0xff, u128::from(bytes[0]));
        let back = table_bytes(&words);
        assert_eq!(&back[..], &bytes[..]);
        assert_eq!(back.as_ptr().align_offset(ALIGN), 0);
    }

    #[test]
    fn a_written_type_table_is_read_in_place_as_the_bytes_of_its_copy() {
        let mut model = Model::default();
        let mut record = InferenceRecord::of(&result(), &mut model);
        let table: Vec<u8> = (0..5 * ALIGN).map(|it| (it * 53 % 241) as u8).collect();
        record.table = table_words(&table);
        let buffer = blaze_format::Aligned::copy_from(&to_bytes(&record));
        let read = InferenceRecord::read(&buffer).unwrap();
        let TableView::InPlace(view) = read.table else { panic!("a read record's table is a view") };
        assert_eq!(view, &table_bytes(&record.table)[..]);
        assert_eq!(view.as_ptr().align_offset(ALIGN), 0);
        let inside = buffer.as_ptr() as usize..buffer.as_ptr() as usize + buffer.len();
        assert!(inside.contains(&(view.as_ptr() as usize)), "the view is into the buffer, not a copy");

        record.table = Vec::new();
        let buffer = blaze_format::Aligned::copy_from(&to_bytes(&record));
        let read = InferenceRecord::read(&buffer).unwrap();
        assert!(matches!(read.table, TableView::InPlace(&[])));
    }

    #[test]
    fn every_code_names_its_own_kind() {
        for (code, adjust) in ADJUSTS.iter().enumerate() {
            assert_eq!(adjust_code(adjust) as usize, code);
        }
        for code in 0..29 {
            assert_eq!(BinaryOp::from_code(code).unwrap().code(), code);
        }
        assert!(BinaryOp::from_code(29).is_none());
        for (code, error) in CAST_ERRORS.iter().enumerate() {
            assert_eq!(cast_error_code(error) as usize, code);
        }
    }

    /// A trait reference needs an interner, so no `TraitUnimplemented` is in the fixture; its
    /// parent impls round trip here.
    #[test]
    fn every_cause_and_parent_impl_round_trips() {
        let causes = cause_codes();
        let impls = [
            None,
            Some(AnyImplId::ImplId(ImplId::from_id(id(40)))),
            Some(AnyImplId::BuiltinDeriveImplId(BuiltinDeriveImplId::from_id(id(41)))),
        ];
        let mut sink = Sink::new();
        causes.iter().for_each(|&it| sink.cause_code(it));
        impls.iter().for_each(|&it| sink.optional_impl(it));
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let mut source = Source { words: &sink.words, next: 0, text: &sink.text, at: 0, frame };
            let read_causes = causes.iter().map(|_| source.cause_code().unwrap()).collect::<Vec<_>>();
            let read_impls = impls.iter().map(|_| source.optional_impl().unwrap()).collect::<Vec<_>>();
            assert_eq!(read_causes, causes);
            assert_eq!(read_impls, impls);
            assert!(source.is_done());
        });
    }

    fn cause_codes() -> [ObligationCauseCode; 12] {
        let item: GenericDefId = ImplId::from_id(id(9)).into();
        [
            ObligationCauseCode::Misc,
            ObligationCauseCode::WhereClauseInExpr { item, bound_index: 3, arg_index: None },
            ObligationCauseCode::WhereClauseInExpr { item, bound_index: 0, arg_index: Some(2) },
            ObligationCauseCode::SizedArgumentType,
            ObligationCauseCode::BlockTailExpression,
            ObligationCauseCode::ReturnValue,
            ObligationCauseCode::IfExpression,
            ObligationCauseCode::MatchExpressionArm { prior_arms: 4 },
            ObligationCauseCode::LetType,
            ObligationCauseCode::SizedReturnType,
            ObligationCauseCode::IfExpressionWithNoElse,
            ObligationCauseCode::ReturnNoExpression,
        ]
    }

    #[test]
    fn truncated_words_are_refused_not_misread() {
        let mut model = Model::default();
        let mut sink = Sink::new();
        let diagnostics = every_diagnostic();
        diagnostics.iter().for_each(|it| sink.diagnostic(&mut model, it));
        sink.closure(&mut model, &closure());
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            for len in 0..sink.words.len() {
                let mut source =
                    Source { words: &sink.words[..len], next: 0, text: &sink.text, at: 0, frame };
                let read = (0..diagnostics.len())
                    .map(|_| source.diagnostic(&model))
                    .collect::<Result<Vec<_>, _>>()
                    .and_then(|_| source.closure(&model, frame));
                assert!(read.is_err(), "{len} of {} words read as a whole", sink.words.len());
            }
        });
    }

    #[test]
    fn a_corrupt_record_is_an_error() {
        let mut model = Model::default();
        let record = InferenceRecord::of(&result(), &mut model);
        let mut past = record.clone();
        past.spans32[C32::Words as usize][1] += 1;
        let bytes = blaze_format::Aligned::copy_from(&to_bytes(&past));
        assert!(InferenceRecord::read(&bytes).is_err());

        let mut misordered = record.clone();
        let [start, len] = misordered.spans32[C32::ExprTyAt as usize];
        assert!(len >= 3);
        misordered.u32s.swap(start as usize, start as usize + 2);
        assert!(!decodes(&misordered, &model));

        let mut flagged = record.clone();
        flagged.flags = 2;
        assert!(!decodes(&flagged, &model));

        let mut no_kind = record.clone();
        let [start, len] = no_kind.spans8[C8::ExprAdjustKinds as usize];
        assert!(len >= 1);
        no_kind.bytes[start as usize] = ADJUSTS.len() as u8;
        assert!(!decodes(&no_kind, &model));

        for present in [C64::MethodPresent, C64::ExprAdjustPresent] {
            let mut dropped = record.clone();
            let [start, len] = dropped.spans64[present as usize];
            assert!(len >= 1);
            let word = &mut dropped.u64s[(start + len - 1) as usize];
            *word &= *word - 1;
            assert!(!decodes(&dropped, &model), "{present:?} with a key fewer than its rows");
        }

        assert!(decodes(&record, &model));
    }

    #[test]
    fn every_builtin_type_has_a_word_of_its_own() {
        let all = BuiltinType::all_builtin_types().map(|(_, ty)| ty);
        let words = all.map(builtin_word);
        for (ty, word) in all.into_iter().zip(words) {
            assert_eq!(builtin_of(word), Some(ty), "{ty:?}");
        }
        assert_eq!(words, std::array::from_fn(|at| at as u32), "the words are 0..19 in order");
        assert_eq!(builtin_of(19), None);
    }

    #[test]
    fn a_record_is_made_at_its_length_and_again_the_same() {
        let mut model = Model::default();
        let record = InferenceRecord::of(&result(), &mut model);
        assert_eq!(record.u64s.capacity(), record.u64s.len());
        assert_eq!(record.u32s.capacity(), record.u32s.len());
        assert_eq!(record.bytes.capacity(), record.bytes.len());
        let mut again = Model::default();
        assert_eq!(InferenceRecord::of(&result(), &mut again), record);
    }

    #[test]
    fn a_diagnostic_count_past_the_words_is_refused() {
        let mut model = Model::default();
        let mut record = InferenceRecord::of(&result(), &mut model);
        record.diagnostics = u32::MAX;
        assert!(!decodes(&record, &model));
    }

    fn decodes(record: &InferenceRecord, model: &Model) -> bool {
        let result: Result<InferenceResult<'static>, _> = record.as_ref().decode(model);
        result.is_ok()
    }
}
