//! [`ExpressionStore`], [`Body`] and their source maps as [`blaze_format`] buffers
//! (docs/v2/plan/lanes/format.md section 5), in the encoding of [`crate::hir::persist`].
//!
//! A store's expressions, patterns and statements are written as the columns they are in memory
//! ([`HirColumns`]): the kind bytes, the data words, `extra` and the list arrays copied as they
//! are, the arms and record fields as records of words (a field's name a [`Str`]), and the cold
//! side tables (names, literals, `offset_of!`, `asm!`, macro definitions) in the words, as
//! [`Code`] writes them. Type refs are written the same way, as the columns of [`TypeColumns`]:
//! kinds, data words, `extra` and the list array copied, the side tables (bounds, references,
//! arrays, fn pointers, type params) in the words. So are the store's paths ([`PathColumns`],
//! which expressions and patterns share with type refs): kinds, data words, `extra` and slots
//! copied, their tables (mod paths, lang items, generic argument rows, arguments, binding rows,
//! bounds, names, `use<..>` arguments) in the words. Bindings, labels
//! and lifetimes are one [`Node`] column per arena, their fields in the same words. Source maps
//! are buffers of their own, so that spans stay out of what is hashed ([`source_map`]).
//!
//! What is read from a buffer is checked before it is used: each kind and each column's ids
//! when the buffer is read; each id in a record or a side table, and the columns as a whole
//! ([`HirColumns::check`], [`TypeColumns::check`]), when it is decoded. An id past the arena it
//! indexes is a [`FormatError::Invalid`], never a later panic.

mod source_map;

use std::{
    cell::RefCell,
    mem::{offset_of, size_of},
};

use blaze_format::{
    FormatError, Part, Persist, Record, SchemaHash, Slice, Str, Writer, open_as, schema_of,
};
use hir_expand::{
    MacroCallId, MacroCallStyles, MacroDefId, MacroDefKind,
    builtin::{BuiltinAttrExpander, BuiltinDeriveExpander, BuiltinFnLikeExpander, EagerExpander},
    mod_path::ModPath,
    name::Name,
    proc_macro::{CustomProcMacroExpander, ProcMacroKind},
};
use intern::Symbol;
use la_arena::{Arena, Idx, RawIdx};
use rustc_abi::ExternAbi;
use blaze_hash::HashMap;
use blaze_query::db::{AsId, FromId};
use smallvec::SmallVec;
use span::Edition;

use crate::{
    AdtId, BlockId, ConstId, EnumId, EnumVariantId, FunctionId, GenericDefId, ImplId,
    LifetimeParamId, Macro2Id, MacroId, MacroRulesId, ProcMacroId, StaticId, StructId, TraitId,
    TypeAliasId, TypeOrConstParamId, TypeParamId, UnionId,
    builtin_type::{BuiltinFloat, BuiltinInt, BuiltinUint},
    deps::ImageBuf,
    expr_store::{
        Body, ExprRoot, ExpressionOnlyStore, ExpressionStore, HygieneId, MissingBodyItemKind,
        NO_OWNER, kinds_of,
        body::Param,
        path::{BindingRow, GenericArg, GenericArgsRow, Names, PathColumns, PathTag, UseArgs},
    },
    hir::{
        AsmOperand, AsmOptions, AsmPiece, Binding, BindingAnnotation, BindingId, BindingProblems,
        ExprId, ExprKind, ExprOrPatId, ExprOrPatIdPacked, FloatTypeWrapper, InlineAsm,
        InlineAsmKind, InlineAsmRegOrRegClass, Label, Literal, MatchArm, OffsetOf, PatId, PatKind,
        RecordFieldPat, RecordLitField,
        col::{Col, SlabLayout, SlabWriter},
        columns::{self, Frozen, HirColumns},
        persist::{
            Code, Column, Node, Placed, Read, Words, WordsRec, blank_segments, id_of_bits,
            lifetime_index, names_blank, syntax_context, with_slots,
        },
    },
    lang_item::LangItemTarget,
    type_ref::{
        self, ArrayType, ConstRef, FnType, LifetimeRef, LifetimeRefId, Mutability, PathId,
        RefType, TraitBoundModifier, TypeBound, TypeColumns, TypeRefId, UseArgRef,
    },
};

pub use self::source_map::{BodySourceMapRef, ExpressionStoreSourceMapRef};

blaze_format::record! {
    /// Two ids, or an id and what a side table holds for it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct IdPair: 1 {
        pub(crate) a: u32,
        pub(crate) b: u32,
    }
}

blaze_format::record! {
    /// An [`ExprRoot`]: its root, and where the expressions, patterns and bindings it holds end.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ExprRootRec: 1 {
        pub(crate) root: u32,
        pub(crate) exprs_end: u32,
        pub(crate) pats_end: u32,
        pub(crate) bindings_end: u32,
    }
}

blaze_format::record! {
    /// A [`MatchArm`], its guard the raw id plus one, 0 for none.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ArmRec: 1 {
        pub(crate) pat: u32,
        pub(crate) guard: u32,
        pub(crate) expr: u32,
    }
}

blaze_format::record! {
    /// A [`RecordLitField`] (`id` its expression) or a [`RecordFieldPat`] (`id` its pattern).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct FieldRec: 1 {
        pub(crate) name: Str,
        pub(crate) id: u32,
    }
}

blaze_format::record! {
    /// The root of an [`ExpressionStore`]'s buffer: [`HirColumns`] and [`TypeColumns`] as they
    /// are in memory, each id a raw `u32`, the side tables in the words from `side` and
    /// `ty_side` on.
    #[derive(Clone, Copy)]
    pub(crate) struct StoreRoot: 6 {
        pub(crate) expr_kind: Slice<u8>,
        pub(crate) expr_data: Slice<u32>,
        pub(crate) pat_kind: Slice<u8>,
        pub(crate) pat_data: Slice<u32>,
        pub(crate) extra: Slice<u32>,
        pub(crate) expr_lists: Slice<u32>,
        pub(crate) pat_lists: Slice<u32>,
        /// The statement rows, as [`HirColumns`] keeps them: a `let`'s words are in `extra`.
        pub(crate) stmt_kind: Slice<u8>,
        pub(crate) stmt_data: Slice<u32>,
        pub(crate) arms: Slice<ArmRec>,
        pub(crate) record_fields: Slice<FieldRec>,
        pub(crate) field_pats: Slice<FieldRec>,
        /// Each `Option<TypeRefId>` as the raw id plus one, 0 for `None`.
        pub(crate) type_lists: Slice<u32>,
        pub(crate) bindings: Slice<Node>,
        pub(crate) labels: Slice<Node>,
        pub(crate) ty_kind: Slice<u8>,
        pub(crate) ty_data: Slice<u32>,
        pub(crate) ty_extra: Slice<u32>,
        /// The tuples' element ids, raw.
        pub(crate) ty_lists: Slice<u32>,
        /// The paths' columns, as [`PathColumns`] keeps them; their tables are in the words.
        pub(crate) path_kind: Slice<u8>,
        pub(crate) path_data: Slice<u32>,
        pub(crate) path_extra: Slice<u32>,
        pub(crate) path_slots: Slice<u32>,
        pub(crate) lifetimes: Slice<Node>,
        /// The owner of each binding, or [`NO_OWNER`]; empty when the top level owns them all.
        pub(crate) binding_owners: Slice<u32>,
        /// Each block's id, as its bits.
        pub(crate) block_scopes: Slice<u64>,
        /// Each macro call's id, as its bits, in lowering order.
        pub(crate) macro_calls: Slice<u64>,
        /// The packed id and the syntax context, by packed id in increasing order.
        pub(crate) ident_hygiene: Slice<IdPair>,
        /// The block and its edition, by block in increasing order.
        pub(crate) block_editions: Slice<IdPair>,
        pub(crate) overflow_inheriting: Slice<u32>,
        pub(crate) expr_roots: Slice<ExprRootRec>,
        /// The type and the syntax context, by type in increasing order.
        pub(crate) type_hygiene: Slice<IdPair>,
        pub(crate) words: WordsRec,
        /// The word the side tables begin at: names, literals, `offset_of!`s, `asm!`s and macro
        /// definitions, each a count and its values.
        pub(crate) side: u32,
        /// The word the type side tables begin at: bounds, the paths' tables ([`TypeSide`]),
        /// references, arrays, fn pointers and type params, each a count and its values.
        pub(crate) ty_side: u32,
        /// [`HAS_EXPRS`] and [`HAS_TYPE_HYGIENE`].
        pub(crate) flags: u32,
    }
}

blaze_format::record! {
    /// The root of a [`Body`]'s buffer.
    #[derive(Clone, Copy)]
    pub(crate) struct BodyRoot: 3 {
        pub(crate) store: StoreRoot,
        /// Each parameter's formal and user-written pattern.
        pub(crate) params: Slice<IdPair>,
        /// The formal and user-written binding of `self`, when there is one.
        pub(crate) self_param: Slice<IdPair>,
    }
}

/// The store has an [`ExpressionOnlyStore`], or the source map an `ExpressionOnlySourceMap`.
const HAS_EXPRS: u32 = 1 << 0;
/// The store's `type_hygiene` is `Some`.
const HAS_TYPE_HYGIENE: u32 = 1 << 1;

macro_rules! kinds {
    ($($(#[$meta:meta])* $name:ident { $($variant:ident),* $(,)? })*) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum $name {
            $($variant),*
        }

        impl $name {
            const ALL: &[$name] = &[$($name::$variant),*];

            fn from_u32(kind: u32) -> Option<$name> {
                $name::ALL.get(kind as usize).copied()
            }
        }
    )*};
}

/// Asserts each variant of `ours` has the number of the same variant of `theirs`: a kind
/// read in place from a buffer is the byte of the column it was copied from.
macro_rules! same_numbers {
    ($($ours:ident = $theirs:path { $($variant:ident),* $(,)? })*) => {$(
        const _: () = {
            $(::core::assert!(
                $ours::$variant as u8 == <$theirs>::$variant as u8,
                ::core::concat!(
                    ::core::stringify!($ours), "::", ::core::stringify!($variant),
                    " is not the column's number",
                ),
            );)*
            ::core::assert!(
                $ours::ALL.len() == [$($ours::$variant),*].len(),
                ::core::concat!(::core::stringify!($ours), " is not listed whole"),
            );
        };
    )*};
}

kinds! {
    /// Which [`type_ref::TypeRef`] variant a type of a buffer is, read in place.
    TypeRefKind {
        Never, Placeholder, Tuple, Path, RawPtr, Reference, Array, Slice, Fn, ImplTrait, DynTrait,
        TypeParam, PatternType, Error, UnsafeBinder,
    }

    /// Which [`LifetimeRef`] variant a node of a buffer is, read in place.
    LifetimeRefKind {
        Named, Static, Placeholder, Param, Error,
    }

    /// Which [`crate::hir::Statement`] variant a statement row of a buffer is, read in place:
    /// `Item`'s two split.
    StatementKind {
        Let, Expr, MacroDef, Item,
    }
}

same_numbers! {
    TypeRefKind = type_ref::TypeRefKind {
        Never, Placeholder, Tuple, Path, RawPtr, Reference, Array, Slice, Fn, ImplTrait, DynTrait,
        TypeParam, PatternType, Error, UnsafeBinder,
    }
    StatementKind = columns::StatementKind {
        Let, Expr, MacroDef, Item,
    }
}

fn raw<T>(id: Idx<T>) -> u32 {
    id.into_raw().into_u32()
}

fn idx<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

fn bits(id: impl AsId) -> u64 {
    u64::from(id.as_id().as_u32())
}

/// Where `value`, which is in `buffer`, is from the buffer's start.
fn at_of<T>(value: &T, buffer: &[u8]) -> u64 {
    (value as *const T as usize).wrapping_sub(buffer.as_ptr() as usize) as u64
}

fn ensure<T>(ok: bool, value: &T, buffer: &[u8], what: &'static str) -> Result<(), FormatError> {
    match ok {
        true => Ok(()),
        false => Err(FormatError::Invalid { at: at_of(value, buffer), what }),
    }
}

/// Checks that `column` is in strictly increasing order of `key`.
fn ascending<T>(
    column: &[T],
    key: impl Fn(&T) -> u32,
    buffer: &[u8],
    what: &'static str,
) -> Result<(), FormatError> {
    column.windows(2).try_for_each(|pair| ensure(key(&pair[0]) < key(&pair[1]), &pair[1], buffer, what))
}

fn check_nodes(
    nodes: &[Node],
    kinds: usize,
    words: u32,
    buffer: &[u8],
    what: &'static str,
) -> Result<(), FormatError> {
    nodes.iter().try_for_each(|node| {
        ensure((node.kind as usize) < kinds, node, buffer, what)?;
        ensure(node.at <= words, node, buffer, "a node whose fields begin past the words")
    })
}

fn check_kinds(
    kinds: &[u8],
    count: usize,
    buffer: &[u8],
    what: &'static str,
) -> Result<(), FormatError> {
    match kinds.iter().find(|&&kind| kind as usize >= count) {
        Some(kind) => Err(FormatError::Invalid { at: at_of(kind, buffer), what }),
        None => Ok(()),
    }
}

fn hygiene_bits(hygiene: HygieneId) -> u32 {
    hygiene.0.into_u32()
}

fn hygiene_of(bits: u32) -> HygieneId {
    HygieneId(syntax_context(bits).expect("a syntax context of u32::MAX, refused when the buffer was read"))
}

fn packed_bits(id: ExprOrPatIdPacked) -> u32 {
    const PAT_BIT: u32 = 1 << (u32::BITS - 1);
    match id.unpack() {
        ExprOrPatId::ExprId(expr) => raw(expr),
        ExprOrPatId::PatId(pat) => raw(pat) | PAT_BIT,
    }
}

fn packed_of(bits: u32) -> ExprOrPatIdPacked {
    const PAT_BIT: u32 = 1 << (u32::BITS - 1);
    match bits & PAT_BIT {
        0 => {
            let expr: ExprId = idx(bits);
            ExprOrPatIdPacked::from(expr)
        }
        _ => {
            let pat: PatId = idx(bits & !PAT_BIT);
            ExprOrPatIdPacked::from(pat)
        }
    }
}

/// An optional id as a word: the raw id plus one, 0 for `None`.
fn opt_raw<T>(id: Option<Idx<T>>) -> u32 {
    id.map_or(0, |id| raw(id) + 1)
}

fn opt_idx<T>(word: u32) -> Option<Idx<T>> {
    word.checked_sub(1).map(idx)
}

fn edition_of(bits: u32) -> Option<Edition> {
    (bits <= Edition::LATEST as u32).then(|| Edition::from_u32(bits))
}

/// The words of `bytes`, four to a word, after their length.
fn put_bytes(w: &mut Words, bytes: &[u8]) {
    w.len(bytes.len());
    for chunk in bytes.chunks(4) {
        let mut word = [0; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        w.u32(u32::from_le_bytes(word));
    }
}

fn get_bytes(r: &mut Read<'_>) -> Result<Box<[u8]>, FormatError> {
    let len = r.u32()? as usize;
    // alloc: the decode's one copy; capacity isn't taken from the unchecked length.
    let mut bytes = Vec::new();
    for _ in 0..len.div_ceil(4) {
        bytes.extend_from_slice(&r.u32()?.to_le_bytes());
    }
    if bytes[len..].iter().any(|&byte| byte != 0) {
        return Err(r.invalid("bytes padded with other than zeros"));
    }
    bytes.truncate(len);
    Ok(bytes.into_boxed_slice())
}

/// The ids a store's nodes may name: what each of its arenas holds.
struct Ids<'s> {
    exprs: u32,
    pats: u32,
    types: &'s TypeColumns<Frozen>,
}

type Checked = Result<(), &'static str>;

fn below(id: u32, len: u32, what: &'static str) -> Checked {
    match id < len {
        true => Ok(()),
        false => Err(what),
    }
}

impl Ids<'_> {
    fn expr(&self, expr: ExprId) -> Checked {
        below(raw(expr), self.exprs, "an expression id past its arena")
    }

    fn expr_opt(&self, expr: Option<ExprId>) -> Checked {
        expr.map_or(Ok(()), |expr| self.expr(expr))
    }

    fn pat(&self, pat: PatId) -> Checked {
        below(raw(pat), self.pats, "a pattern id past its arena")
    }

    fn ty(&self, ty: TypeRefId) -> Checked {
        below(raw(ty), self.types.kind.len() as u32, "a type id past its arena")
    }

    fn ty_opt(&self, ty: Option<TypeRefId>) -> Checked {
        ty.map_or(Ok(()), |ty| self.ty(ty))
    }

    fn asm(&self, asm: &InlineAsm) -> Checked {
        asm.operands.iter().try_for_each(|(_, operand)| match operand {
            &AsmOperand::In { reg: _, expr }
            | &AsmOperand::InOut { reg: _, expr, late: _ }
            | &AsmOperand::Label(expr)
            | &AsmOperand::Const(expr) => self.expr(expr),
            &AsmOperand::Out { reg: _, expr, late: _ } => self.expr_opt(expr),
            &AsmOperand::SplitInOut { reg: _, in_expr, out_expr, late: _ } => {
                self.expr(in_expr)?;
                self.expr_opt(out_expr)
            }
            AsmOperand::Sym(_) => Ok(()),
        })
    }

    /// The ids the side tables' values hold, which [`HirColumns::check`] does not look into.
    fn side_tables(&self, side: &SideTables) -> Checked {
        side.offset_ofs.iter().try_for_each(|offset_of| self.ty(offset_of.container))?;
        side.asms.iter().try_for_each(|asm| self.asm(asm))
    }
}

/// Writes each value's fields and appends the node of each to `nodes`.
fn column<'v, T: 'v>(
    w: &mut Words,
    values: impl Iterator<Item = &'v T>,
    put: fn(&T, &mut Words) -> u32,
    nodes: &mut Vec<Node>,
) {
    nodes.extend(values.map(|value| {
        let at = w.at();
        let kind = put(value, w);
        Node { kind, at }
    }));
}

/// Writes `values`, each as `f` makes its record, as a column gathered in `buffer` (a kept
/// one, left holding them).
fn push_mapped<T, R: Record>(
    out: &mut Writer,
    buffer: &mut Vec<R>,
    values: &[T],
    f: impl FnMut(&T) -> R,
) -> Column {
    buffer.clear();
    buffer.extend(values.iter().map(f));
    Column::push(out, buffer)
}

/// The columns an encode gathers before it pushes them, kept per thread so that a warm encode
/// grows none of them.
#[derive(Default)]
struct EncodeScratch {
    nodes: Vec<Node>,
    u32s: Vec<u32>,
    u64s: Vec<u64>,
    pairs: Vec<IdPair>,
    arms: Vec<ArmRec>,
    roots: Vec<ExprRootRec>,
    fields: Vec<FieldRec>,
    names: FieldNames,
}

thread_local! {
    // alloc: empty, once per thread.
    static ENCODE: RefCell<EncodeScratch> = RefCell::new(EncodeScratch::default());
}

impl EncodeScratch {
    /// Runs `f` on this thread's scratch, taken out for the call: an encode nested in it (none
    /// is today) gets an empty set of its own instead of a second borrow.
    fn with<R>(f: impl FnOnce(&mut EncodeScratch) -> R) -> R {
        let taken = ENCODE.try_with(|it| std::mem::take(&mut *it.borrow_mut()));
        let mut scratch = taken.unwrap_or_default();
        let result = f(&mut scratch);
        // A thread that is ending frees it.
        let _exiting = ENCODE.try_with(move |it| *it.borrow_mut() = scratch);
        result
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        let EncodeScratch { nodes, u32s, u64s, pairs, arms, roots, fields, names } = self;
        nodes.capacity()
            + u32s.capacity()
            + u64s.capacity()
            + pairs.capacity()
            + arms.capacity()
            + roots.capacity()
            + fields.capacity()
            + names.text.capacity()
            + names.by_symbol.capacity()
    }
}

/// Reads the arena of `nodes`, the fields of each from its word.
fn arena<T, K>(
    nodes: &[Node],
    words: &Read<'_>,
    buffer: &[u8],
    kind: fn(u32) -> Option<K>,
    get: fn(K, &mut Read<'_>) -> Result<T, FormatError>,
) -> Result<Arena<T>, FormatError> {
    let mut arena = Arena::with_capacity(nodes.len());
    for node in nodes {
        let kind = kind(node.kind).ok_or(FormatError::Invalid {
            at: at_of(node, buffer),
            what: "a node kind out of range",
        })?;
        arena.alloc(get(kind, &mut words.from(node.at))?);
    }
    Ok(arena)
}

fn only_kind(kind: u32) -> Option<()> {
    (kind == 0).then_some(())
}

/// A column of `hir`, empty for a store without expressions.
fn slice<'c, T>(
    hir: Option<&'c HirColumns<Frozen>>,
    column: impl FnOnce(&'c HirColumns<Frozen>) -> &'c [T],
) -> &'c [T] {
    hir.map_or(&[][..], column)
}

/// Writes the literal table of `hir`: a count, then each literal, or for one an [`anonymous`]
/// scope's slots name, [`SLOT`] and its kind and type.
///
/// [`anonymous`]: crate::hir::persist::anonymous
fn put_literals(w: &mut Words, hir: Option<&HirColumns<Frozen>>) {
    let literals = slice(hir, |c| &c.literals[..]);
    let slotted = with_slots(|slots| match hir {
        Some(hir) if !slots.is_empty() => {
            let mut at: SmallVec<[u32; 4]> = slots
                .iter()
                .map(|&raw| ExprId::from_raw(RawIdx::from_u32(raw)))
                .filter(|&expr| hir.expr_kind_of(expr) == ExprKind::Literal)
                .map(|expr| hir.expr_data[expr.into_raw().into_u32() as usize])
                // alloc: the literal rows of a body's location macros, inline for a few
                .collect();
            at.sort_unstable();
            at
        }
        _ => SmallVec::new(),
    });
    if slotted.is_empty() {
        w.seq(literals.iter());
        return;
    }
    w.len(literals.len());
    for (at, literal) in (0u32..).zip(literals) {
        match slotted.binary_search(&at) {
            Ok(_) => put_slot(w, literal),
            Err(_) => literal.put(w),
        }
    }
}

/// The tag a slot is written with in place of a literal's: past every literal kind's.
const SLOT: u32 = 8;

/// A literal a location macro expanded to, as its slot: [`SLOT`], its kind, and its type where
/// it has one, never its value.
fn put_slot(w: &mut Words, literal: &Literal) {
    w.u32(SLOT);
    match literal {
        Literal::String(_) => w.u32(0),
        Literal::ByteString(_) => w.u32(1),
        Literal::CString(_) => w.u32(2),
        Literal::Char(_) => w.u32(3),
        Literal::Bool(_) => w.u32(4),
        Literal::Int(_, ty) => {
            w.u32(5);
            ty.put(w);
        }
        Literal::Uint(_, ty) => {
            w.u32(6);
            ty.put(w);
        }
        Literal::Float(_, ty) => {
            w.u32(7);
            ty.put(w);
        }
    }
}

/// The cold side tables of [`HirColumns`], read back from the words.
struct SideTables {
    names: Vec<Name>,
    literals: Vec<Literal>,
    offset_ofs: Vec<OffsetOf>,
    asms: Vec<InlineAsm>,
    macro_defs: Vec<MacroDefId>,
}

impl SideTables {
    /// Writes the side tables of `hir`, each a count and its values, and returns the word they
    /// begin at.
    fn put(w: &mut Words, hir: Option<&HirColumns<Frozen>>) -> u32 {
        let at = w.at();
        w.seq(slice(hir, |c| &c.names[..]).iter());
        put_literals(w, hir);
        w.seq(slice(hir, |c| &c.offset_ofs[..]).iter());
        w.seq(slice(hir, |c| &c.asms[..]).iter());
        w.seq(slice(hir, |c| &c.macro_defs[..]).iter());
        at
    }

    fn get(r: &mut Read<'_>) -> Result<SideTables, FormatError> {
        Ok(SideTables {
            names: r.seq::<Name, _>()?,
            literals: r.seq::<Literal, _>()?,
            offset_ofs: r.seq::<OffsetOf, _>()?,
            asms: r.seq::<InlineAsm, _>()?,
            macro_defs: r.seq::<MacroDefId, _>()?,
        })
    }

    fn is_empty(&self) -> bool {
        let SideTables { names, literals, offset_ofs, asms, macro_defs } = self;
        names.is_empty()
            && literals.is_empty()
            && offset_ofs.is_empty()
            && asms.is_empty()
            && macro_defs.is_empty()
    }
}

/// The side tables of [`TypeColumns`] and the tables of its [`PathColumns`], read back from the
/// words.
struct TypeSide {
    bounds: Vec<TypeBound>,
    mod_paths: Vec<ModPath>,
    lang_items: Vec<LangItemTarget>,
    generic_args: Vec<GenericArgsRow>,
    args: Vec<GenericArg>,
    bindings: Vec<BindingRow>,
    binding_bounds: Vec<TypeBound>,
    names: Vec<Name>,
    use_args: Vec<UseArgRef>,
    refs: Vec<RefType>,
    arrays: Vec<ArrayType>,
    fns: Vec<FnType>,
    type_params: Vec<TypeParamId>,
}

impl TypeSide {
    /// Writes the side tables of `types`, each a count and its values, and returns the word they
    /// begin at.
    fn put(w: &mut Words, types: &TypeColumns<Frozen>) -> u32 {
        let at = w.at();
        let paths = &types.paths;
        w.seq(types.bounds.iter());
        w.len(paths.mod_paths.len());
        for (at, path) in paths.mod_paths.iter().enumerate() {
            match blank_segments(at) {
                0 => path.put(w),
                blank => {
                    path.kind.put(w);
                    w.len(path.segments().len());
                    for (index, segment) in path.segments().iter().enumerate() {
                        match index < blank {
                            true => w.str(""),
                            false => segment.put(w),
                        }
                    }
                }
            }
        }
        w.seq(paths.lang_items.iter());
        w.seq(paths.generic_args.iter());
        w.seq(paths.args.iter());
        w.seq(paths.bindings.iter());
        w.seq(paths.bounds.iter());
        w.seq(paths.names.iter());
        w.seq(paths.use_args.iter());
        w.seq(types.refs.iter());
        w.seq(types.arrays.iter());
        w.seq(types.fns.iter());
        w.seq(types.type_params.iter());
        at
    }

    fn get(r: &mut Read<'_>) -> Result<TypeSide, FormatError> {
        Ok(TypeSide {
            bounds: r.seq::<TypeBound, _>()?,
            mod_paths: r.seq::<ModPath, _>()?,
            lang_items: r.seq::<LangItemTarget, _>()?,
            generic_args: r.seq::<GenericArgsRow, _>()?,
            args: r.seq::<GenericArg, _>()?,
            bindings: r.seq::<BindingRow, _>()?,
            binding_bounds: r.seq::<TypeBound, _>()?,
            names: r.seq::<Name, _>()?,
            use_args: r.seq::<UseArgRef, _>()?,
            refs: r.seq::<RefType, _>()?,
            arrays: r.seq::<ArrayType, _>()?,
            fns: r.seq::<FnType, _>()?,
            type_params: r.seq::<TypeParamId, _>()?,
        })
    }
}

/// The names of record fields and field patterns, each text once, for [`FieldRec`]s to point
/// at.
#[derive(Default)]
struct FieldNames {
    text: String,
    /// Each name's start in `text` and its length.
    by_symbol: HashMap<Symbol, (u32, u32)>,
}

impl FieldNames {
    fn clear(&mut self) {
        self.text.clear();
        self.by_symbol.clear();
    }

    fn add(&mut self, name: &Name) {
        let symbol = name.symbol();
        if self.by_symbol.contains_key(symbol) {
            return;
        }
        let start =
            u32::try_from(self.text.len()).expect("more than 4 GiB of field names in one buffer");
        let len = u32::try_from(symbol.as_str().len()).expect("a name of 4 GiB or more");
        self.text.push_str(symbol.as_str());
        self.by_symbol.insert(*symbol, (start, len));
    }

    /// Writes `fields`, each a name and an id, as a column of [`FieldRec`]s gathered in
    /// `records` (a kept buffer), their names in this text, which was written at `text_at`.
    fn column<'f>(
        &self,
        out: &mut Writer,
        text_at: u64,
        fields: impl Iterator<Item = (&'f Name, u32)>,
        records: &mut Vec<FieldRec>,
    ) -> Column {
        let at = out.next::<FieldRec>();
        records.clear();
        records.extend((fields.enumerate()).map(|(index, (name, id))| {
            let (start, len) = self.by_symbol[name.symbol()];
            let field_at = at + (index * size_of::<FieldRec>() + offset_of!(FieldRec, name)) as u64;
            let name = match len {
                0 => Str::EMPTY,
                len => Str::between(field_at, text_at + u64::from(start), len as usize),
            };
            FieldRec { name, id }
        }));
        let column = Column::push(out, records);
        debug_assert_eq!(column.at, at, "`Writer::next` is not where `push_slice` writes");
        column
    }
}

/// The name a [`FieldRec`] holds, which is in `buffer`.
fn field_name(field: &FieldRec, buffer: &[u8]) -> Result<Name, FormatError> {
    let text = field.name.as_str(buffer)?;
    ensure(!text.starts_with("r#"), field, buffer, "a name written with its `r#`")?;
    Ok(Name::new_symbol_root(Symbol::intern(text)))
}

/// Each value of `records`, which are in `buffer`, as `of` makes it and `check` accepts it.
fn decoded<R, T>(
    records: &[R],
    buffer: &[u8],
    of: impl Fn(&R) -> Result<T, FormatError>,
    check: impl Fn(&T) -> Checked,
) -> Result<Vec<T>, FormatError> {
    (records.iter())
        .map(|record| {
            let value = of(record)?;
            check(&value).map_err(|what| FormatError::Invalid { at: at_of(record, buffer), what })?;
            Ok(value)
        })
        // alloc: the decode's one copy of the column.
        .collect()
}

/// Where everything of a store but its root was written.
struct StoreColumns {
    words: Placed,
    side: u32,
    ty_side: u32,
    expr_kind: Column,
    expr_data: Column,
    pat_kind: Column,
    pat_data: Column,
    extra: Column,
    expr_lists: Column,
    pat_lists: Column,
    stmt_kind: Column,
    stmt_data: Column,
    arms: Column,
    record_fields: Column,
    field_pats: Column,
    type_lists: Column,
    bindings: Column,
    labels: Column,
    ty_kind: Column,
    ty_data: Column,
    ty_extra: Column,
    ty_lists: Column,
    path_kind: Column,
    path_data: Column,
    path_extra: Column,
    path_slots: Column,
    lifetimes: Column,
    binding_owners: Column,
    block_scopes: Column,
    macro_calls: Column,
    ident_hygiene: Column,
    block_editions: Column,
    overflow_inheriting: Column,
    expr_roots: Column,
    type_hygiene: Column,
    flags: u32,
}

impl StoreColumns {
    fn write(store: &ExpressionStore, out: &mut Writer) -> StoreColumns {
        EncodeScratch::with(|scratch| Self::write_in(store, out, scratch))
    }

    /// [`Self::write`]: each column gathered in `scratch` and pushed in turn, in the order of
    /// the buffer (the words, the field names and field columns, then the rest in field order).
    fn write_in(
        store: &ExpressionStore,
        out: &mut Writer,
        scratch: &mut EncodeScratch,
    ) -> StoreColumns {
        let ExpressionStore { expr_only, types, lifetimes, type_hygiene, macro_calls } = store;
        let expr_only = expr_only.as_deref();
        let hir = expr_only.map(|it| &it.columns);
        let EncodeScratch { nodes, u32s, u64s, pairs, arms: arm_recs, roots, fields, names } =
            scratch;
        let mut w = Words::new();
        let side = SideTables::put(&mut w, hir);
        let ty_side = TypeSide::put(&mut w, types);
        nodes.clear();
        let bindings = expr_only.into_iter().flat_map(|it| it.bindings.values());
        column(&mut w, bindings, put_binding, nodes);
        let bindings_end = nodes.len();
        let labels = expr_only.into_iter().flat_map(|it| it.labels.values());
        column(&mut w, labels, put_label, nodes);
        let labels_end = nodes.len();
        column(&mut w, lifetimes.values(), put_lifetime, nodes);
        let words = w.place(out);

        let (record_fields, field_pats) =
            (slice(hir, |c| &c.record_fields[..]), slice(hir, |c| &c.field_pats[..]));
        names.clear();
        record_fields.iter().for_each(|field| names.add(&field.name));
        field_pats.iter().for_each(|field| names.add(&field.name));
        let text_at = out.push_str(&names.text);
        let record_fields = names.column(
            out,
            text_at,
            record_fields.iter().map(|field| (&field.name, raw(field.expr))),
            fields,
        );
        let field_pats = names.column(
            out,
            text_at,
            field_pats.iter().map(|field| (&field.name, raw(field.pat))),
            fields,
        );

        let expr_kind = Column::push(out, slice(hir, |c| &c.expr_kind[..]));
        let expr_data = Column::push(out, slice(hir, |c| &c.expr_data[..]));
        let pat_kind = Column::push(out, slice(hir, |c| &c.pat_kind[..]));
        let pat_data = Column::push(out, slice(hir, |c| &c.pat_data[..]));
        let extra = Column::push(out, slice(hir, |c| &c.extra[..]));
        let expr_lists = push_mapped(out, u32s, slice(hir, |c| &c.expr_lists[..]), |&e| raw(e));
        let pat_lists = push_mapped(out, u32s, slice(hir, |c| &c.pat_lists[..]), |&p| raw(p));
        let stmt_kind = Column::push(out, slice(hir, |c| &c.stmt_kind[..]));
        let stmt_data = Column::push(out, slice(hir, |c| &c.stmt_data[..]));
        let arms = push_mapped(out, arm_recs, slice(hir, |c| &c.arms[..]), |arm| ArmRec {
            pat: raw(arm.pat),
            guard: opt_raw(arm.guard),
            expr: raw(arm.expr),
        });
        let type_lists =
            push_mapped(out, u32s, slice(hir, |c| &c.type_lists[..]), |&ty| opt_raw(ty));
        let bindings = Column::push(out, &nodes[..bindings_end]);
        let labels = Column::push(out, &nodes[bindings_end..labels_end]);
        let ty_kind = Column::push(out, &types.kind[..]);
        let ty_data = Column::push(out, &types.data[..]);
        let ty_extra = Column::push(out, &types.extra[..]);
        let ty_lists = push_mapped(out, u32s, &types.lists[..], |&ty| raw(ty));
        let path_kind = Column::push(out, &types.paths.kind[..]);
        let path_data = Column::push(out, &types.paths.data[..]);
        let path_extra = Column::push(out, &types.paths.extra[..]);
        let path_slots = Column::push(out, &types.paths.slots[..]);
        let lifetimes = Column::push(out, &nodes[labels_end..]);
        let owners = expr_only.map_or(&[][..], |it| &*it.binding_owners);
        let binding_owners = Column::push(out, owners);
        let blocks = expr_only.map_or(&[][..], |it| &*it.block_scopes);
        let block_scopes = push_mapped(out, u64s, blocks, |&block| bits(block));
        let macro_calls = push_mapped(out, u64s, &macro_calls[..], |&call| bits(call));
        let hygiene = expr_only.map_or(&[][..], |it| &*it.ident_hygiene);
        let ident_hygiene = push_mapped(out, pairs, hygiene, |&(id, hygiene)| IdPair {
            a: packed_bits(id),
            b: hygiene_bits(hygiene),
        });
        let editions = expr_only.map_or(&[][..], |it| &*it.block_editions);
        let block_editions = push_mapped(out, pairs, editions, |&(block, edition)| IdPair {
            a: raw(block),
            b: edition as u32,
        });
        let inheriting = expr_only.map_or(&[][..], |it| &*it.overflow_inheriting);
        let overflow_inheriting = push_mapped(out, u32s, inheriting, |&closure| raw(closure));
        let expr_roots = expr_only.map_or(&[][..], |it| &it.expr_roots[..]);
        let expr_roots = push_mapped(out, roots, expr_roots, |root| ExprRootRec {
            root: raw(root.root),
            exprs_end: raw(root.exprs_end),
            pats_end: raw(root.pats_end),
            bindings_end: raw(root.bindings_end),
        });
        let type_hygiene_pairs = type_hygiene.as_deref().unwrap_or(&[]);
        let type_hygiene_column =
            push_mapped(out, pairs, type_hygiene_pairs, |&(ty, hygiene)| IdPair {
                a: raw(ty),
                b: hygiene_bits(hygiene),
            });
        let flags = (if expr_only.is_some() { HAS_EXPRS } else { 0 })
            | (if type_hygiene.is_some() { HAS_TYPE_HYGIENE } else { 0 });
        StoreColumns {
            words,
            side,
            ty_side,
            expr_kind,
            expr_data,
            pat_kind,
            pat_data,
            extra,
            expr_lists,
            pat_lists,
            stmt_kind,
            stmt_data,
            arms,
            record_fields,
            field_pats,
            type_lists,
            bindings,
            labels,
            ty_kind,
            ty_data,
            ty_extra,
            ty_lists,
            path_kind,
            path_data,
            path_extra,
            path_slots,
            lifetimes,
            binding_owners,
            block_scopes,
            macro_calls,
            ident_hygiene,
            block_editions,
            overflow_inheriting,
            expr_roots,
            type_hygiene: type_hygiene_column,
            flags,
        }
    }

    /// The root of these columns, for one written at `at`.
    fn rec(&self, at: u64) -> StoreRoot {
        let field = |offset: usize| at + offset as u64;
        StoreRoot {
            expr_kind: self.expr_kind.slice(field(offset_of!(StoreRoot, expr_kind))),
            expr_data: self.expr_data.slice(field(offset_of!(StoreRoot, expr_data))),
            pat_kind: self.pat_kind.slice(field(offset_of!(StoreRoot, pat_kind))),
            pat_data: self.pat_data.slice(field(offset_of!(StoreRoot, pat_data))),
            extra: self.extra.slice(field(offset_of!(StoreRoot, extra))),
            expr_lists: self.expr_lists.slice(field(offset_of!(StoreRoot, expr_lists))),
            pat_lists: self.pat_lists.slice(field(offset_of!(StoreRoot, pat_lists))),
            stmt_kind: self.stmt_kind.slice(field(offset_of!(StoreRoot, stmt_kind))),
            stmt_data: self.stmt_data.slice(field(offset_of!(StoreRoot, stmt_data))),
            arms: self.arms.slice(field(offset_of!(StoreRoot, arms))),
            record_fields: self.record_fields.slice(field(offset_of!(StoreRoot, record_fields))),
            field_pats: self.field_pats.slice(field(offset_of!(StoreRoot, field_pats))),
            type_lists: self.type_lists.slice(field(offset_of!(StoreRoot, type_lists))),
            bindings: self.bindings.slice(field(offset_of!(StoreRoot, bindings))),
            labels: self.labels.slice(field(offset_of!(StoreRoot, labels))),
            ty_kind: self.ty_kind.slice(field(offset_of!(StoreRoot, ty_kind))),
            ty_data: self.ty_data.slice(field(offset_of!(StoreRoot, ty_data))),
            ty_extra: self.ty_extra.slice(field(offset_of!(StoreRoot, ty_extra))),
            ty_lists: self.ty_lists.slice(field(offset_of!(StoreRoot, ty_lists))),
            path_kind: self.path_kind.slice(field(offset_of!(StoreRoot, path_kind))),
            path_data: self.path_data.slice(field(offset_of!(StoreRoot, path_data))),
            path_extra: self.path_extra.slice(field(offset_of!(StoreRoot, path_extra))),
            path_slots: self.path_slots.slice(field(offset_of!(StoreRoot, path_slots))),
            lifetimes: self.lifetimes.slice(field(offset_of!(StoreRoot, lifetimes))),
            binding_owners: self.binding_owners.slice(field(offset_of!(StoreRoot, binding_owners))),
            block_scopes: self.block_scopes.slice(field(offset_of!(StoreRoot, block_scopes))),
            macro_calls: self.macro_calls.slice(field(offset_of!(StoreRoot, macro_calls))),
            ident_hygiene: self.ident_hygiene.slice(field(offset_of!(StoreRoot, ident_hygiene))),
            block_editions: self.block_editions.slice(field(offset_of!(StoreRoot, block_editions))),
            overflow_inheriting: (self.overflow_inheriting)
                .slice(field(offset_of!(StoreRoot, overflow_inheriting))),
            expr_roots: self.expr_roots.slice(field(offset_of!(StoreRoot, expr_roots))),
            type_hygiene: self.type_hygiene.slice(field(offset_of!(StoreRoot, type_hygiene))),
            words: self.words.rec(field(offset_of!(StoreRoot, words))),
            side: self.side,
            ty_side: self.ty_side,
            flags: self.flags,
        }
    }
}

/// An [`ExpressionStore`] read in place from its buffer.
#[derive(Clone, Copy)]
pub struct ExpressionStoreRef<'a> {
    buffer: &'a [u8],
    root: &'a StoreRoot,
    expr_kind: &'a [u8],
    expr_data: &'a [u32],
    pat_kind: &'a [u8],
    pat_data: &'a [u32],
    extra: &'a [u32],
    expr_lists: &'a [u32],
    pat_lists: &'a [u32],
    stmt_kind: &'a [u8],
    stmt_data: &'a [u32],
    arms: &'a [ArmRec],
    record_fields: &'a [FieldRec],
    field_pats: &'a [FieldRec],
    type_lists: &'a [u32],
    bindings: &'a [Node],
    labels: &'a [Node],
    ty_kind: &'a [u8],
    ty_data: &'a [u32],
    ty_extra: &'a [u32],
    ty_lists: &'a [u32],
    path_kind: &'a [u8],
    path_data: &'a [u32],
    path_extra: &'a [u32],
    path_slots: &'a [u32],
    lifetimes: &'a [Node],
    binding_owners: &'a [u32],
    block_scopes: &'a [u64],
    macro_calls: &'a [u64],
    ident_hygiene: &'a [IdPair],
    block_editions: &'a [IdPair],
    overflow_inheriting: &'a [u32],
    expr_roots: &'a [ExprRootRec],
    type_hygiene: &'a [IdPair],
}

impl<'a> ExpressionStoreRef<'a> {
    /// The store of `root`, in `buffer`, its columns checked.
    fn new(root: &'a StoreRoot, buffer: &'a [u8]) -> Result<ExpressionStoreRef<'a>, FormatError> {
        let store = ExpressionStoreRef {
            buffer,
            root,
            expr_kind: root.expr_kind.follow(buffer)?,
            expr_data: root.expr_data.follow(buffer)?,
            pat_kind: root.pat_kind.follow(buffer)?,
            pat_data: root.pat_data.follow(buffer)?,
            extra: root.extra.follow(buffer)?,
            expr_lists: root.expr_lists.follow(buffer)?,
            pat_lists: root.pat_lists.follow(buffer)?,
            stmt_kind: root.stmt_kind.follow(buffer)?,
            stmt_data: root.stmt_data.follow(buffer)?,
            arms: root.arms.follow(buffer)?,
            record_fields: root.record_fields.follow(buffer)?,
            field_pats: root.field_pats.follow(buffer)?,
            type_lists: root.type_lists.follow(buffer)?,
            bindings: root.bindings.follow(buffer)?,
            labels: root.labels.follow(buffer)?,
            ty_kind: root.ty_kind.follow(buffer)?,
            ty_data: root.ty_data.follow(buffer)?,
            ty_extra: root.ty_extra.follow(buffer)?,
            ty_lists: root.ty_lists.follow(buffer)?,
            path_kind: root.path_kind.follow(buffer)?,
            path_data: root.path_data.follow(buffer)?,
            path_extra: root.path_extra.follow(buffer)?,
            path_slots: root.path_slots.follow(buffer)?,
            lifetimes: root.lifetimes.follow(buffer)?,
            binding_owners: root.binding_owners.follow(buffer)?,
            block_scopes: root.block_scopes.follow(buffer)?,
            macro_calls: root.macro_calls.follow(buffer)?,
            ident_hygiene: root.ident_hygiene.follow(buffer)?,
            block_editions: root.block_editions.follow(buffer)?,
            overflow_inheriting: root.overflow_inheriting.follow(buffer)?,
            expr_roots: root.expr_roots.follow(buffer)?,
            type_hygiene: root.type_hygiene.follow(buffer)?,
        };
        store.check()?;
        Ok(store)
    }

    fn check(&self) -> Result<(), FormatError> {
        let (root, buffer) = (self.root, self.buffer);
        let flags = root.flags;
        ensure(flags & !(HAS_EXPRS | HAS_TYPE_HYGIENE) == 0, root, buffer, "store flags out of range")?;
        let words = root.words.words.len;
        let datas = self.expr_data.len() == self.expr_kind.len()
            && self.pat_data.len() == self.pat_kind.len()
            && self.stmt_data.len() == self.stmt_kind.len()
            && self.ty_data.len() == self.ty_kind.len()
            && self.path_data.len() == self.path_kind.len();
        ensure(datas, root, buffer, "a data column that is not one word for each node")?;
        check_kinds(self.expr_kind, ExprKind::ALL.len(), buffer, "an expression kind out of range")?;
        check_kinds(self.pat_kind, PatKind::ALL.len(), buffer, "a pattern kind out of range")?;
        check_kinds(
            self.stmt_kind,
            StatementKind::ALL.len(),
            buffer,
            "a statement kind out of range",
        )?;
        check_kinds(self.ty_kind, TypeRefKind::ALL.len(), buffer, "a type kind out of range")?;
        check_kinds(self.path_kind, PathTag::ALL.len(), buffer, "a path kind out of range")?;
        check_nodes(self.bindings, 1, words, buffer, "a binding kind out of range")?;
        check_nodes(self.labels, 1, words, buffer, "a label kind out of range")?;
        check_nodes(
            self.lifetimes,
            LifetimeRefKind::ALL.len(),
            words,
            buffer,
            "a lifetime kind out of range",
        )?;
        ensure(root.side <= words, root, buffer, "side tables that begin past the words")?;
        ensure(root.ty_side <= words, root, buffer, "type side tables that begin past the words")?;
        if flags & HAS_EXPRS == 0 {
            let empty = self.expr_kind.is_empty()
                && self.pat_kind.is_empty()
                && self.extra.is_empty()
                && self.expr_lists.is_empty()
                && self.pat_lists.is_empty()
                && self.stmt_kind.is_empty()
                && self.stmt_data.is_empty()
                && self.arms.is_empty()
                && self.record_fields.is_empty()
                && self.field_pats.is_empty()
                && self.type_lists.is_empty()
                && self.bindings.is_empty()
                && self.labels.is_empty()
                && self.binding_owners.is_empty()
                && self.block_scopes.is_empty()
                && self.ident_hygiene.is_empty()
                && self.block_editions.is_empty()
                && self.overflow_inheriting.is_empty()
                && self.expr_roots.is_empty();
            ensure(empty, root, buffer, "expression columns in a store without expressions")?;
        }
        if flags & HAS_TYPE_HYGIENE == 0 {
            ensure(self.type_hygiene.is_empty(), root, buffer, "type hygiene the store has not")?;
        }
        let (exprs, pats, bindings) =
            (self.expr_kind.len() as u32, self.pat_kind.len() as u32, self.bindings.len() as u32);
        let owners = self.binding_owners;
        ensure(
            owners.is_empty() || owners.len() == self.bindings.len(),
            root,
            buffer,
            "binding owners that are not one for each binding",
        )?;
        for owner in owners {
            ensure(*owner == NO_OWNER || *owner < exprs, owner, buffer, "a binding owner past the expressions")?;
        }
        for block in self.block_scopes {
            ensure(id_of_bits(*block).is_some(), block, buffer, "a block id of 0 or past 32 bits")?;
        }
        for call in self.macro_calls {
            ensure(id_of_bits(*call).is_some(), call, buffer, "a macro call id of 0 or past 32 bits")?;
        }
        ascending(self.ident_hygiene, |it| it.a, buffer, "ident hygiene out of order")?;
        for pair in self.ident_hygiene {
            let id = match packed_of(pair.a).unpack() {
                ExprOrPatId::ExprId(expr) => raw(expr) < exprs,
                ExprOrPatId::PatId(pat) => raw(pat) < pats,
            };
            ensure(id, pair, buffer, "ident hygiene of an id past its arena")?;
            ensure(pair.b != u32::MAX, pair, buffer, "a syntax context of u32::MAX")?;
        }
        ascending(self.block_editions, |it| it.a, buffer, "block editions out of order")?;
        for pair in self.block_editions {
            ensure(pair.a < exprs, pair, buffer, "the edition of a block past the expressions")?;
            ensure(edition_of(pair.b).is_some(), pair, buffer, "an edition out of range")?;
        }
        ascending(self.overflow_inheriting, |&it| it, buffer, "closures out of order")?;
        for closure in self.overflow_inheriting {
            ensure(*closure < exprs, closure, buffer, "a closure past the expressions")?;
        }
        for (at, root) in self.expr_roots.iter().enumerate() {
            let fits = root.root < exprs
                && root.exprs_end <= exprs
                && root.pats_end <= pats
                && root.bindings_end <= bindings;
            ensure(fits, root, buffer, "an expression root past its arenas")?;
            if let Some(previous) = at.checked_sub(1).map(|at| &self.expr_roots[at]) {
                let ordered = previous.exprs_end <= root.exprs_end
                    && previous.pats_end <= root.pats_end
                    && previous.bindings_end <= root.bindings_end;
                ensure(ordered, root, buffer, "expression roots out of order")?;
            }
        }
        ascending(self.type_hygiene, |it| it.a, buffer, "type hygiene out of order")?;
        for pair in self.type_hygiene {
            ensure(pair.a < self.ty_kind.len() as u32, pair, buffer, "the hygiene of a type past its arena")?;
            ensure(pair.b != u32::MAX, pair, buffer, "a syntax context of u32::MAX")?;
        }
        Ok(())
    }

    /// Whether the store holds expressions, patterns, bindings or labels.
    pub fn has_exprs(self) -> bool {
        self.root.flags & HAS_EXPRS != 0
    }

    pub fn exprs_len(self) -> usize {
        self.expr_kind.len()
    }

    pub fn pats_len(self) -> usize {
        self.pat_kind.len()
    }

    pub fn bindings_len(self) -> usize {
        self.bindings.len()
    }

    pub fn labels_len(self) -> usize {
        self.labels.len()
    }

    pub fn types_len(self) -> usize {
        self.ty_kind.len()
    }

    pub fn paths_len(self) -> usize {
        self.path_kind.len()
    }

    pub fn lifetimes_len(self) -> usize {
        self.lifetimes.len()
    }

    /// Which variant `expr` is, `None` past the arena.
    pub fn expr_kind(self, expr: ExprId) -> Option<ExprKind> {
        ExprKind::ALL.get(usize::from(*self.expr_kind.get(raw(expr) as usize)?)).copied()
    }

    pub fn pat_kind(self, pat: PatId) -> Option<PatKind> {
        PatKind::ALL.get(usize::from(*self.pat_kind.get(raw(pat) as usize)?)).copied()
    }

    pub fn statements_len(self) -> usize {
        self.stmt_kind.len()
    }

    /// Which variant the statement row `at` is, `None` past the rows.
    pub fn statement_kind(self, at: u32) -> Option<StatementKind> {
        StatementKind::from_u32(u32::from(*self.stmt_kind.get(at as usize)?))
    }

    pub fn type_kind(self, ty: TypeRefId) -> Option<TypeRefKind> {
        TypeRefKind::from_u32(u32::from(*self.ty_kind.get(raw(ty) as usize)?))
    }

    pub fn lifetime_kind(self, lifetime: LifetimeRefId) -> Option<LifetimeRefKind> {
        LifetimeRefKind::from_u32(self.lifetimes.get(raw(lifetime) as usize)?.kind)
    }

    /// As [`ExpressionStore::binding_owner`].
    pub fn binding_owner(self, binding: BindingId) -> Option<ExprId> {
        let owner = *self.binding_owners.get(raw(binding) as usize)?;
        (owner != NO_OWNER).then(|| idx(owner))
    }

    /// As [`ExpressionStore::expr_roots`].
    pub fn expr_roots(self) -> impl DoubleEndedIterator<Item = ExprId> + 'a {
        self.expr_roots.iter().map(|root| idx(root.root))
    }

    /// The blocks of the store that may hold items, as [`ExpressionStore::blocks`] walks them.
    pub fn block_scopes(self) -> impl Iterator<Item = BlockId> + 'a {
        (self.block_scopes.iter())
            .filter_map(|&block| id_of_bits(block))
            .map(<BlockId as FromId>::from_id)
    }

    /// As [`ExpressionStore::inherits_overflow_checks`].
    pub fn inherits_overflow_checks(self, closure: ExprId) -> bool {
        self.overflow_inheriting.binary_search(&raw(closure)).is_ok()
    }

    /// As [`ExpressionStore::block_edition`].
    pub fn block_edition(self, block: ExprId) -> Option<Edition> {
        let index = self.block_editions.binary_search_by_key(&raw(block), |it| it.a).ok()?;
        edition_of(self.block_editions[index].b)
    }

    /// The store, back as the value that was written.
    pub fn decode(self) -> Result<ExpressionStore, FormatError> {
        self.decode_with(None)
    }

    /// [`decode`](Self::decode) of a store whose buffer lies in `owner`'s bytes: each `Copy`
    /// column the buffer holds as the store does is read where it is, holding a count of
    /// `owner`, not copied. A column outside `owner` is an error, never a copy.
    pub fn decode_in(self, owner: &ImageBuf) -> Result<ExpressionStore, FormatError> {
        self.decode_with(Some(owner))
    }

    fn decode_with(self, image: Option<&ImageBuf>) -> Result<ExpressionStore, FormatError> {
        let buffer = self.buffer;
        let as_is = AsIs { image, root: self.root, buffer };
        let words = Read::new(&self.root.words, buffer);
        let lifetimes = arena(self.lifetimes, &words, buffer, LifetimeRefKind::from_u32, get_lifetime)?;
        let bindings = arena(self.bindings, &words, buffer, only_kind, get_binding)?;
        let labels = arena(self.labels, &words, buffer, only_kind, get_label)?;
        let side = SideTables::get(&mut words.from(self.root.side))?;
        let TypeSide {
            bounds,
            mod_paths,
            lang_items,
            generic_args: generic_arg_rows,
            args,
            bindings: binding_rows,
            binding_bounds,
            names: path_names,
            use_args,
            refs,
            arrays,
            fns,
            type_params,
        } = TypeSide::get(&mut words.from(self.root.ty_side))?;
        let paths = PathColumns {
            kind: self.path_kind.into(),
            data: self.path_data.into(),
            extra: self.path_extra.into(),
            mod_paths,
            lang_items,
            slots: self.path_slots.into(),
            generic_args: generic_arg_rows,
            args,
            bindings: binding_rows,
            bounds: binding_bounds,
            names: path_names,
            use_args,
        };
        let types = TypeColumns::<Frozen> {
            kind: as_is.col(self.ty_kind, |it| Col::from(it))?,
            data: as_is.col(self.ty_data, |it| Col::from(it))?,
            extra: as_is.col(self.ty_extra, |it| Col::from(it))?,
            // alloc: the decode's one copy of the column.
            lists: self.ty_lists.iter().map(|&ty| idx(ty)).collect(),
            bounds: bounds.into(),
            paths,
            refs: refs.into(),
            arrays: arrays.into(),
            fns: fns.into(),
            type_params: type_params.into(),
        };
        let (exprs, pats) = (self.expr_kind.len(), self.pat_kind.len());
        (types.check(exprs, pats, lifetimes.len()))
            .map_err(|what| FormatError::Invalid { at: at_of(self.root, buffer), what })?;
        let ids = Ids { exprs: exprs as u32, pats: pats as u32, types: &types };
        (ids.side_tables(&side))
            .map_err(|what| FormatError::Invalid { at: at_of(&self.root.words, buffer), what })?;
        if !self.has_exprs() {
            ensure(side.is_empty(), self.root, buffer, "side tables in a store without expressions")?;
        }

        let arms: Vec<MatchArm> = decoded(
            self.arms,
            buffer,
            |rec| Ok(MatchArm { pat: idx(rec.pat), guard: opt_idx(rec.guard), expr: idx(rec.expr) }),
            |arm| {
                ids.pat(arm.pat)?;
                ids.expr_opt(arm.guard)?;
                ids.expr(arm.expr)
            },
        )?;
        let record_fields: Vec<RecordLitField> = decoded(
            self.record_fields,
            buffer,
            |rec| Ok(RecordLitField { name: field_name(rec, buffer)?, expr: idx(rec.id) }),
            |field| ids.expr(field.expr),
        )?;
        let field_pats: Vec<RecordFieldPat> = decoded(
            self.field_pats,
            buffer,
            |rec| Ok(RecordFieldPat { name: field_name(rec, buffer)?, pat: idx(rec.id) }),
            |field| ids.pat(field.pat),
        )?;
        let mut expr_lists: Vec<ExprId> =
            decoded(self.expr_lists, buffer, |&word| Ok(idx(word)), |&expr| ids.expr(expr))?;
        let mut pat_lists: Vec<PatId> =
            decoded(self.pat_lists, buffer, |&word| Ok(idx(word)), |&pat| ids.pat(pat))?;
        let mut type_lists: Vec<Option<TypeRefId>> =
            decoded(self.type_lists, buffer, |&word| Ok(opt_idx(word)), |&ty| ids.ty_opt(ty))?;
        let SideTables { names, literals, offset_ofs, asms, mut macro_defs } = side;
        // The `Copy` columns in one slab, in `HirColumns::freeze_into`'s order; those the buffer
        // holds as they are take no room when they are read in the image.
        let mut layout = SlabLayout::default();
        layout.add::<MacroDefId>(macro_defs.len());
        layout.add::<Option<TypeRefId>>(type_lists.len());
        layout.add::<u32>(as_is.slab_len(self.expr_data.len()));
        layout.add::<u32>(as_is.slab_len(self.pat_data.len()));
        layout.add::<u32>(as_is.slab_len(self.extra.len()));
        layout.add::<u32>(as_is.slab_len(self.stmt_data.len()));
        layout.add::<ExprId>(expr_lists.len());
        layout.add::<PatId>(pat_lists.len());
        layout.add::<u8>(as_is.slab_len(self.expr_kind.len()));
        layout.add::<u8>(as_is.slab_len(self.pat_kind.len()));
        layout.add::<u8>(as_is.slab_len(self.stmt_kind.len()));
        let mut slab = SlabWriter::new(layout);
        let macro_defs = slab.take(&mut macro_defs);
        let type_lists = slab.take(&mut type_lists);
        let expr_data = as_is.col(self.expr_data, |it| slab.copy(it))?;
        let pat_data = as_is.col(self.pat_data, |it| slab.copy(it))?;
        let extra = as_is.col(self.extra, |it| slab.copy(it))?;
        let stmt_data = as_is.col(self.stmt_data, |it| slab.copy(it))?;
        let expr_lists = slab.take(&mut expr_lists);
        let pat_lists = slab.take(&mut pat_lists);
        let columns = HirColumns::<Frozen> {
            expr_kind: as_is.col(self.expr_kind, |it| slab.copy(it))?,
            expr_data,
            pat_kind: as_is.col(self.pat_kind, |it| slab.copy(it))?,
            pat_data,
            extra,
            expr_lists,
            pat_lists,
            stmt_kind: as_is.col(self.stmt_kind, |it| slab.copy(it))?,
            stmt_data,
            arms: arms.into(),
            record_fields: record_fields.into(),
            field_pats: field_pats.into(),
            type_lists,
            names: names.into(),
            literals: literals.into(),
            offset_ofs: offset_ofs.into(),
            asms: asms.into(),
            macro_defs,
        };
        (columns.check(bindings.len(), labels.len(), types.len(), &types.paths))
            .map_err(|what| FormatError::Invalid { at: at_of(self.root, buffer), what })?;

        let expr_only = match self.has_exprs() {
            false => None,
            // alloc: once per decoded store, as `finish` boxes it.
            true => Some(Box::new(ExpressionOnlyStore {
                kinds: kinds_of(&columns.expr_kind),
                columns,
                bindings,
                labels,
                binding_owners: as_is.col(self.binding_owners, |it| Col::from(it))?,
                // alloc: the decode's one copy of the column.
                block_scopes: self.block_scopes().collect(),
                ident_hygiene: (self.ident_hygiene.iter())
                    .map(|pair| (packed_of(pair.a), hygiene_of(pair.b)))
                    // alloc: the decode's one copy of the column.
                    .collect(),
                block_editions: (self.block_editions.iter())
                    .filter_map(|pair| Some((idx(pair.a), edition_of(pair.b)?)))
                    // alloc: the decode's one copy of the column.
                    .collect(),
                // alloc: the decode's one copy of the column.
                overflow_inheriting: self.overflow_inheriting.iter().map(|&it| idx(it)).collect(),
                expr_roots: (self.expr_roots.iter())
                    .map(|root| ExprRoot {
                        root: idx(root.root),
                        exprs_end: idx(root.exprs_end),
                        pats_end: idx(root.pats_end),
                        bindings_end: idx(root.bindings_end),
                    })
                    .collect::<SmallVec<_>>(),
            })),
        };
        let type_hygiene = (self.root.flags & HAS_TYPE_HYGIENE != 0).then(|| {
            // alloc: the decode's one copy of the column.
            (self.type_hygiene.iter()).map(|pair| (idx(pair.a), hygiene_of(pair.b))).collect()
        });
        let macro_calls = (self.macro_calls.iter())
            .map(|&call| {
                <MacroCallId as FromId>::from_id(id_of_bits(call).expect("checked when read"))
            })
            // alloc: the decode's one copy of the column.
            .collect();
        Ok(ExpressionStore { expr_only, types, lifetimes, type_hygiene, macro_calls })
    }
}

/// Where a decode puts a `Copy` column its buffer holds as the store does.
#[derive(Clone, Copy)]
struct AsIs<'a> {
    /// The image the buffer lies in, when it is read in place there.
    image: Option<&'a ImageBuf>,
    root: &'a StoreRoot,
    buffer: &'a [u8],
}

impl AsIs<'_> {
    /// The column of `elements`: read where it is in the image when there is one, else `copy`
    /// of it.
    fn col<T: Copy>(
        self,
        elements: &[T],
        copy: impl FnOnce(&[T]) -> Col<T>,
    ) -> Result<Col<T>, FormatError> {
        match self.image {
            None => Ok(copy(elements)),
            Some(image) => Col::borrowed(image, elements).ok_or(FormatError::Invalid {
                at: at_of(self.root, self.buffer),
                what: "a column outside its image",
            }),
        }
    }

    /// `len` when the column is copied into the slab, 0 when it is read in the image.
    fn slab_len(self, len: usize) -> usize {
        if self.image.is_some() { 0 } else { len }
    }
}

impl ExpressionStore {
    /// The warm and cold forms of expression store buffers, codec by column: what
    /// `blaze_cas::Store::encode_columns` is given for [`ExpressionStore`]'s schema.
    pub fn columns() -> blaze_format::tiers::BufferColumns {
        blaze_format::tiers::BufferColumns::of::<StoreRoot>(<ExpressionStore as Persist>::SCHEMA)
    }
}

impl Persist for ExpressionStore {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("ExpressionStore"),
        Part::U64(8),
        Part::Schema(<StoreRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = ExpressionStoreRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let columns = StoreColumns::write(self, out);
        let at = out.next::<StoreRoot>();
        let written = out.push(&columns.rec(at));
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<ExpressionStoreRef<'_>, FormatError> {
        let root = open_as::<StoreRoot>(buffer, Self::SCHEMA)?;
        ExpressionStoreRef::new(root, buffer)
    }
}

fn param_pair<Id>(param: &Param<Idx<Id>>) -> IdPair {
    IdPair { a: raw(param.formal), b: raw(param.user_written) }
}

fn param_of<Id>(pair: &IdPair) -> Param<Idx<Id>> {
    Param { formal: idx(pair.a), user_written: idx(pair.b) }
}

/// A [`Body`] read in place from its buffer.
#[derive(Clone, Copy)]
pub struct BodyRef<'a> {
    store: ExpressionStoreRef<'a>,
    params: &'a [IdPair],
    self_param: Option<&'a IdPair>,
}

impl<'a> BodyRef<'a> {
    pub fn store(self) -> ExpressionStoreRef<'a> {
        self.store
    }

    pub fn params_len(self) -> usize {
        self.params.len()
    }

    pub fn params(self) -> impl ExactSizeIterator<Item = Param<PatId>> + 'a {
        self.params.iter().map(param_of)
    }

    pub fn self_param(self) -> Option<Param<BindingId>> {
        self.self_param.map(param_of)
    }

    /// The body, back as the value that was written.
    pub fn decode(self) -> Result<Body, FormatError> {
        Ok(Body {
            store: self.store.decode()?,
            // alloc: the decode's one copy of the column.
            params: self.params().collect(),
            self_param: self.self_param(),
        })
    }
}

impl Body {
    /// As [`ExpressionStore::columns`], for [`Body`]'s schema.
    pub fn columns() -> blaze_format::tiers::BufferColumns {
        blaze_format::tiers::BufferColumns::of::<BodyRoot>(<Body as Persist>::SCHEMA)
    }
}

impl Persist for Body {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("Body"),
        Part::U64(8),
        Part::Schema(<BodyRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = BodyRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let Body { store, params, self_param } = self;
        let columns = StoreColumns::write(store, out);
        let params =
            EncodeScratch::with(|scratch| push_mapped(out, &mut scratch.pairs, params, param_pair));
        let self_param = self_param.as_ref().map(param_pair);
        let self_param = Column::push(out, self_param.as_slice());
        let at = out.next::<BodyRoot>();
        let field = |offset: usize| at + offset as u64;
        let root = BodyRoot {
            store: columns.rec(field(offset_of!(BodyRoot, store))),
            params: params.slice(field(offset_of!(BodyRoot, params))),
            self_param: self_param.slice(field(offset_of!(BodyRoot, self_param))),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<BodyRef<'_>, FormatError> {
        let root = open_as::<BodyRoot>(buffer, Self::SCHEMA)?;
        let store = ExpressionStoreRef::new(&root.store, buffer)?;
        let params = root.params.follow(buffer)?;
        for param in params {
            let pats = store.pat_kind.len() as u32;
            let fits = param.a < pats && param.b < pats;
            ensure(fits, param, buffer, "a parameter past the patterns")?;
        }
        let self_params = root.self_param.follow(buffer)?;
        ensure(self_params.len() <= 1, root, buffer, "more than one `self` parameter")?;
        let self_param = self_params.first();
        if let Some(param) = self_param {
            let fits = param.a < store.bindings.len() as u32 && param.b < store.bindings.len() as u32;
            ensure(fits, param, buffer, "a `self` parameter past the bindings")?;
        }
        Ok(BodyRef { store, params, self_param })
    }
}

fn put_binding(binding: &Binding, w: &mut Words) -> u32 {
    let Binding { name, mode, problems, hygiene } = binding;
    match names_blank() {
        true => w.str(""),
        false => w.put(name),
    }
    w.put(mode);
    w.put(problems);
    w.put(hygiene);
    0
}

fn get_binding((): (), r: &mut Read<'_>) -> Result<Binding, FormatError> {
    Ok(Binding { name: r.get()?, mode: r.get()?, problems: r.get()?, hygiene: r.get()? })
}

fn put_label(label: &Label, w: &mut Words) -> u32 {
    let Label { name } = label;
    match names_blank() {
        true => w.str(""),
        false => w.put(name),
    }
    0
}

fn get_label((): (), r: &mut Read<'_>) -> Result<Label, FormatError> {
    Ok(Label { name: r.get()? })
}

fn put_lifetime(lifetime: &LifetimeRef, w: &mut Words) -> u32 {
    let kind = match lifetime {
        LifetimeRef::Named(name) => {
            match lifetime_index(name) {
                Some(at) => {
                    w.str("");
                    w.u32(at);
                }
                None => w.put(name),
            }
            LifetimeRefKind::Named
        }
        LifetimeRef::Static => LifetimeRefKind::Static,
        LifetimeRef::Placeholder => LifetimeRefKind::Placeholder,
        LifetimeRef::Param(param) => {
            w.put(param);
            LifetimeRefKind::Param
        }
        LifetimeRef::Error => LifetimeRefKind::Error,
    };
    kind as u32
}

fn get_lifetime(kind: LifetimeRefKind, r: &mut Read<'_>) -> Result<LifetimeRef, FormatError> {
    Ok(match kind {
        LifetimeRefKind::Named => LifetimeRef::Named(r.get()?),
        LifetimeRefKind::Static => LifetimeRef::Static,
        LifetimeRefKind::Placeholder => LifetimeRef::Placeholder,
        LifetimeRefKind::Param => LifetimeRef::Param(r.get()?),
        LifetimeRefKind::Error => LifetimeRef::Error,
    })
}

/// `Code` for enums without fields: each variant its number.
macro_rules! unit_codes {
    ($($ty:ty { $($tag:literal => $variant:path),* $(,)? })*) => {$(
        impl Code for $ty {
            fn put(&self, w: &mut Words) {
                w.u32(match self { $($variant => $tag,)* });
            }

            fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
                Ok(match r.u32()? {
                    $($tag => $variant,)*
                    _ => return Err(r.invalid(concat!("a ", stringify!($ty), " out of range"))),
                })
            }
        }
    )*};
}

unit_codes! {
    Mutability { 0 => Mutability::Shared, 1 => Mutability::Mut }
    BindingAnnotation {
        0 => BindingAnnotation::Unannotated,
        1 => BindingAnnotation::Mutable,
        2 => BindingAnnotation::Ref,
        3 => BindingAnnotation::RefMut,
    }
    BindingProblems {
        0 => BindingProblems::BoundMoreThanOnce,
        1 => BindingProblems::BoundInconsistently,
        2 => BindingProblems::NotBoundAcrossAll,
    }
    InlineAsmKind {
        0 => InlineAsmKind::Asm, 1 => InlineAsmKind::GlobalAsm, 2 => InlineAsmKind::NakedAsm,
    }
    TraitBoundModifier {
        0 => TraitBoundModifier::None,
        1 => TraitBoundModifier::Maybe,
        2 => TraitBoundModifier::Const,
        3 => TraitBoundModifier::MaybeConst,
    }
    BuiltinInt {
        0 => BuiltinInt::Isize,
        1 => BuiltinInt::I8,
        2 => BuiltinInt::I16,
        3 => BuiltinInt::I32,
        4 => BuiltinInt::I64,
        5 => BuiltinInt::I128,
    }
    BuiltinUint {
        0 => BuiltinUint::Usize,
        1 => BuiltinUint::U8,
        2 => BuiltinUint::U16,
        3 => BuiltinUint::U32,
        4 => BuiltinUint::U64,
        5 => BuiltinUint::U128,
    }
    BuiltinFloat {
        0 => BuiltinFloat::F16, 1 => BuiltinFloat::F32, 2 => BuiltinFloat::F64, 3 => BuiltinFloat::F128,
    }
    ProcMacroKind {
        0 => ProcMacroKind::CustomDerive, 1 => ProcMacroKind::Bang, 2 => ProcMacroKind::Attr,
    }
    MissingBodyItemKind {
        0 => MissingBodyItemKind::AssocConst,
        1 => MissingBodyItemKind::AssocType,
        2 => MissingBodyItemKind::Const,
        3 => MissingBodyItemKind::Static,
        4 => MissingBodyItemKind::TypeAlias,
    }
}

/// `Code` for enums each variant of which holds one value: its number, then the value.
macro_rules! sum_codes {
    ($($ty:ident { $($tag:literal => $variant:ident),* $(,)? })*) => {$(
        impl Code for $ty {
            fn put(&self, w: &mut Words) {
                match self {
                    $($ty::$variant(it) => {
                        w.u32($tag);
                        it.put(w);
                    })*
                }
            }

            fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
                Ok(match r.u32()? {
                    $($tag => $ty::$variant(r.get()?),)*
                    _ => return Err(r.invalid(concat!("a ", stringify!($ty), " out of range"))),
                })
            }
        }
    )*};
}

crate::hir::persist::query_ids!(
    FunctionId,
    StructId,
    UnionId,
    EnumId,
    ConstId,
    StaticId,
    TraitId,
    TypeAliasId,
    ImplId,
    EnumVariantId,
    Macro2Id,
    MacroRulesId,
    ProcMacroId,
);

sum_codes! {
    AdtId { 0 => StructId, 1 => UnionId, 2 => EnumId }
    GenericDefId {
        0 => AdtId,
        1 => ConstId,
        2 => FunctionId,
        3 => ImplId,
        4 => StaticId,
        5 => TraitId,
        6 => TypeAliasId,
    }
    MacroId { 0 => Macro2Id, 1 => MacroRulesId, 2 => ProcMacroId }
    LangItemTarget {
        0 => EnumId,
        1 => FunctionId,
        2 => ImplId,
        3 => StaticId,
        4 => StructId,
        5 => UnionId,
        6 => TypeAliasId,
        7 => TraitId,
        8 => EnumVariantId,
        9 => ConstId,
        10 => MacroId,
    }
    GenericArg { 0 => Type, 1 => Lifetime, 2 => Const }
    UseArgRef { 0 => Name, 1 => Lifetime }
    InlineAsmRegOrRegClass { 0 => Reg, 1 => RegClass }
}

impl Code for TypeParamId {
    fn put(&self, w: &mut Words) {
        self.parent().put(w);
        self.local_id().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let parent = r.get()?;
        Ok(TypeParamId::from_unchecked(TypeOrConstParamId { parent, local_id: r.get()? }))
    }
}

impl Code for LifetimeParamId {
    fn put(&self, w: &mut Words) {
        let LifetimeParamId { parent, local_id } = self;
        parent.put(w);
        local_id.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(LifetimeParamId { parent: r.get()?, local_id: r.get()? })
    }
}

impl Code for PathId {
    fn put(&self, w: &mut Words) {
        self.type_ref().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.get().map(PathId::from_type_ref_unchecked)
    }
}

impl Code for ConstRef {
    fn put(&self, w: &mut Words) {
        self.expr.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(ConstRef { expr: r.get()? })
    }
}

impl Code for GenericArgsRow {
    fn put(&self, w: &mut Words) {
        let GenericArgsRow { args: [args_at, args_len], bindings: [bindings_at, bindings_len], flags } =
            *self;
        w.u32(args_at);
        w.u32(args_len);
        w.u32(bindings_at);
        w.u32(bindings_len);
        w.u32(flags);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(GenericArgsRow {
            args: [r.u32()?, r.u32()?],
            bindings: [r.u32()?, r.u32()?],
            flags: r.u32()?,
        })
    }
}

impl Code for BindingRow {
    fn put(&self, w: &mut Words) {
        let BindingRow { name, args, type_ref, bounds: [bounds_at, bounds_len] } = *self;
        w.u32(name);
        w.u32(args);
        w.u32(type_ref);
        w.u32(bounds_at);
        w.u32(bounds_len);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(BindingRow {
            name: r.u32()?,
            args: r.u32()?,
            type_ref: r.u32()?,
            bounds: [r.u32()?, r.u32()?],
        })
    }
}

impl Code for Names {
    fn put(&self, w: &mut Words) {
        w.u32(self.start);
        w.u32(self.len);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(Names { start: r.u32()?, len: r.u32()? })
    }
}

impl Code for UseArgs {
    fn put(&self, w: &mut Words) {
        w.u32(self.start);
        w.u32(self.len);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(UseArgs { start: r.u32()?, len: r.u32()? })
    }
}

impl Code for TypeBound {
    fn put(&self, w: &mut Words) {
        match self {
            TypeBound::Path(path, modifier) => {
                w.u32(0);
                path.put(w);
                modifier.put(w);
            }
            TypeBound::ForLifetime(names, path) => {
                w.u32(1);
                names.put(w);
                path.put(w);
            }
            TypeBound::Lifetime(lifetime) => {
                w.u32(2);
                lifetime.put(w);
            }
            TypeBound::Use(args) => {
                w.u32(3);
                args.put(w);
            }
            TypeBound::Error => w.u32(4),
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(5, "a type bound out of range")? {
            0 => {
                let path = r.get()?;
                TypeBound::Path(path, r.get()?)
            }
            1 => {
                let names = r.get()?;
                TypeBound::ForLifetime(names, r.get()?)
            }
            2 => TypeBound::Lifetime(r.get()?),
            3 => TypeBound::Use(r.get()?),
            _ => TypeBound::Error,
        })
    }
}

impl Code for RefType {
    fn put(&self, w: &mut Words) {
        let RefType { ty, lifetime, mutability } = self;
        ty.put(w);
        lifetime.put(w);
        mutability.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(RefType { ty: r.get()?, lifetime: r.get()?, mutability: r.get()? })
    }
}

impl Code for ArrayType {
    fn put(&self, w: &mut Words) {
        let ArrayType { ty, len } = self;
        ty.put(w);
        len.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(ArrayType { ty: r.get()?, len: r.get()? })
    }
}

impl Code for ExternAbi {
    fn put(&self, w: &mut Words) {
        w.str(self.as_str());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let name = r.str()?;
        name.parse().map_err(|_| r.invalid("an ABI of no known name"))
    }
}

impl Code for FnType {
    fn put(&self, w: &mut Words) {
        let FnType { binder, params, is_varargs, is_unsafe, abi } = self;
        binder.put(w);
        params.put(w);
        is_varargs.put(w);
        is_unsafe.put(w);
        abi.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(FnType {
            binder: r.get()?,
            params: r.get()?,
            is_varargs: r.get()?,
            is_unsafe: r.get()?,
            abi: r.get()?,
        })
    }
}

impl Code for FloatTypeWrapper {
    fn put(&self, w: &mut Words) {
        w.str(self.as_str());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.symbol().map(FloatTypeWrapper::new)
    }
}

impl Code for Literal {
    fn put(&self, w: &mut Words) {
        match self {
            Literal::String(text) => {
                w.u32(0);
                w.symbol(text);
            }
            Literal::ByteString(bytes) => {
                w.u32(1);
                put_bytes(w, bytes);
            }
            Literal::CString(bytes) => {
                w.u32(2);
                put_bytes(w, bytes);
            }
            Literal::Char(char) => {
                w.u32(3);
                char.put(w);
            }
            Literal::Bool(bool) => {
                w.u32(4);
                bool.put(w);
            }
            Literal::Int(value, ty) => {
                w.u32(5);
                value.put(w);
                ty.put(w);
            }
            Literal::Uint(value, ty) => {
                w.u32(6);
                value.put(w);
                ty.put(w);
            }
            Literal::Float(value, ty) => {
                w.u32(7);
                value.put(w);
                ty.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(8, "a literal kind out of range")? {
            0 => Literal::String(r.symbol()?),
            1 => Literal::ByteString(get_bytes(r)?),
            2 => Literal::CString(get_bytes(r)?),
            3 => Literal::Char(r.get()?),
            4 => Literal::Bool(r.get()?),
            5 => {
                let value = r.get()?;
                Literal::Int(value, r.get()?)
            }
            6 => {
                let value = r.get()?;
                Literal::Uint(value, r.get()?)
            }
            _ => {
                let value = r.get()?;
                Literal::Float(value, r.get()?)
            }
        })
    }
}

impl Code for OffsetOf {
    fn put(&self, w: &mut Words) {
        let OffsetOf { container, fields } = self;
        container.put(w);
        fields.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(OffsetOf { container: r.get()?, fields: r.get()? })
    }
}

impl Code for AsmOptions {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(self.bits()));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let bits: u16 = r.get()?;
        AsmOptions::from_bits(bits).ok_or_else(|| r.invalid("asm options of unknown bits"))
    }
}

impl Code for AsmPiece {
    fn put(&self, w: &mut Words) {
        match self {
            AsmPiece::Lit(text) => {
                w.u32(0);
                text.put(w);
            }
            AsmPiece::Operand { index, modifier } => {
                w.u32(1);
                index.put(w);
                modifier.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "an asm piece out of range")? {
            0 => AsmPiece::Lit(r.get()?),
            _ => AsmPiece::Operand { index: r.get()?, modifier: r.get()? },
        })
    }
}

impl Code for AsmOperand {
    fn put(&self, w: &mut Words) {
        match self {
            AsmOperand::In { reg, expr } => {
                w.u32(0);
                reg.put(w);
                expr.put(w);
            }
            AsmOperand::Out { reg, expr, late } => {
                w.u32(1);
                reg.put(w);
                expr.put(w);
                late.put(w);
            }
            AsmOperand::InOut { reg, expr, late } => {
                w.u32(2);
                reg.put(w);
                expr.put(w);
                late.put(w);
            }
            AsmOperand::SplitInOut { reg, in_expr, out_expr, late } => {
                w.u32(3);
                reg.put(w);
                in_expr.put(w);
                out_expr.put(w);
                late.put(w);
            }
            AsmOperand::Label(expr) => {
                w.u32(4);
                expr.put(w);
            }
            AsmOperand::Const(expr) => {
                w.u32(5);
                expr.put(w);
            }
            AsmOperand::Sym(path) => {
                w.u32(6);
                path.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(7, "an asm operand out of range")? {
            0 => AsmOperand::In { reg: r.get()?, expr: r.get()? },
            1 => AsmOperand::Out { reg: r.get()?, expr: r.get()?, late: r.get()? },
            2 => AsmOperand::InOut { reg: r.get()?, expr: r.get()?, late: r.get()? },
            3 => AsmOperand::SplitInOut {
                reg: r.get()?,
                in_expr: r.get()?,
                out_expr: r.get()?,
                late: r.get()?,
            },
            4 => AsmOperand::Label(r.get()?),
            5 => AsmOperand::Const(r.get()?),
            _ => AsmOperand::Sym(r.get()?),
        })
    }
}

impl Code for InlineAsm {
    fn put(&self, w: &mut Words) {
        let InlineAsm { operands, template, options, kind, comment_only, clobber_abi } = self;
        operands.put(w);
        template.put(w);
        options.put(w);
        kind.put(w);
        comment_only.put(w);
        clobber_abi.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(InlineAsm {
            operands: r.get()?,
            template: r.get()?,
            options: r.get()?,
            kind: r.get()?,
            comment_only: r.get()?,
            clobber_abi: r.get()?,
        })
    }
}

/// `Code` for the expanders a builtin macro names: their place among their type's `ALL`, which
/// lists them in declaration order.
macro_rules! listed_codes {
    ($($ty:ty),* $(,)?) => {$(
        impl Code for $ty {
            fn put(&self, w: &mut Words) {
                w.u32(*self as u32);
            }

            fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
                let index = r.u32()? as usize;
                (<$ty>::ALL.get(index).copied())
                    .ok_or_else(|| r.invalid(concat!("a ", stringify!($ty), " out of range")))
            }
        }
    )*};
}

listed_codes!(BuiltinFnLikeExpander, BuiltinAttrExpander, BuiltinDeriveExpander, EagerExpander);

impl Code for MacroCallStyles {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(self.bits()));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let bits: u8 = r.get()?;
        MacroCallStyles::from_bits(bits).ok_or_else(|| r.invalid("macro call styles of unknown bits"))
    }
}

impl Code for CustomProcMacroExpander {
    fn put(&self, w: &mut Words) {
        match self.listed() {
            Some(id) => {
                w.u32(0);
                w.u32(id);
            }
            None if self.is_missing() => w.u32(1),
            None if self.is_disabled() => w.u32(2),
            None => {
                debug_assert!(self.is_disabled_proc_attr());
                w.u32(3);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(4, "a proc macro expander out of range")? {
            0 => {
                let id = r.u32()?;
                if id >= u32::MAX - 2 {
                    return Err(r.invalid("a proc macro id of a reserved value"));
                }
                CustomProcMacroExpander::new(id)
            }
            1 => CustomProcMacroExpander::missing_expander(),
            2 => CustomProcMacroExpander::disabled(),
            _ => CustomProcMacroExpander::disabled_proc_attr(),
        })
    }
}

impl Code for MacroDefKind {
    fn put(&self, w: &mut Words) {
        match self {
            MacroDefKind::Declarative(id, styles) => {
                w.u32(0);
                id.put(w);
                styles.put(w);
            }
            MacroDefKind::BuiltIn(id, expander) => {
                w.u32(1);
                id.put(w);
                expander.put(w);
            }
            MacroDefKind::BuiltInAttr(id, expander) => {
                w.u32(2);
                id.put(w);
                expander.put(w);
            }
            MacroDefKind::BuiltInDerive(id, expander) => {
                w.u32(3);
                id.put(w);
                expander.put(w);
            }
            MacroDefKind::BuiltInEager(id, expander) => {
                w.u32(4);
                id.put(w);
                expander.put(w);
            }
            MacroDefKind::UnimplementedBuiltIn(id) => {
                w.u32(5);
                id.put(w);
            }
            MacroDefKind::ProcMacro(id, expander, kind) => {
                w.u32(6);
                id.put(w);
                expander.put(w);
                kind.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(7, "a macro kind out of range")? {
            0 => {
                let id = r.get()?;
                MacroDefKind::Declarative(id, r.get()?)
            }
            1 => {
                let id = r.get()?;
                MacroDefKind::BuiltIn(id, r.get()?)
            }
            2 => {
                let id = r.get()?;
                MacroDefKind::BuiltInAttr(id, r.get()?)
            }
            3 => {
                let id = r.get()?;
                MacroDefKind::BuiltInDerive(id, r.get()?)
            }
            4 => {
                let id = r.get()?;
                MacroDefKind::BuiltInEager(id, r.get()?)
            }
            5 => MacroDefKind::UnimplementedBuiltIn(r.get()?),
            _ => {
                let id = r.get()?;
                let expander = r.get()?;
                MacroDefKind::ProcMacro(id, expander, r.get()?)
            }
        })
    }
}

impl Code for MacroDefId {
    fn put(&self, w: &mut Words) {
        let MacroDefId { krate, edition, kind, local_inner, allow_internal_unsafe, edition_panic } =
            self;
        krate.put(w);
        edition.put(w);
        kind.put(w);
        local_inner.put(w);
        allow_internal_unsafe.put(w);
        edition_panic.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(MacroDefId {
            krate: r.get()?,
            edition: r.get()?,
            kind: r.get()?,
            local_inner: r.get()?,
            allow_internal_unsafe: r.get()?,
            edition_panic: r.get()?,
        })
    }
}

/// Writes `body` and `source_map` each into a buffer, reads them back in place and decoded,
/// and checks both are what was written.
#[cfg(test)]
pub(crate) fn assert_round_trip(body: &Body, source_map: &crate::expr_store::BodySourceMap) {
    let bytes = blaze_format::to_bytes(body);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = Body::read(&buffer).expect("a body just written");
    tests::assert_reads(read.store(), &body.store);
    assert_eq!(read.params_len(), body.params.len());
    assert_eq!(read.params().collect::<Vec<_>>(), body.params.to_vec());
    assert_eq!(read.self_param(), body.self_param);
    let decoded = read.decode().expect("a body just written");
    assert_eq!(decoded, *body);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "a body written again as other bytes");
    source_map::assert_body_round_trip(source_map);
}

/// [`assert_round_trip`] for a store without a body, as a signature's.
#[cfg(test)]
pub(crate) fn assert_store_round_trip(
    store: &ExpressionStore,
    source_map: &crate::expr_store::ExpressionStoreSourceMap,
) {
    let bytes = blaze_format::to_bytes(store);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = ExpressionStore::read(&buffer).expect("a store just written");
    tests::assert_reads(read, store);
    let decoded = read.decode().expect("a store just written");
    assert_eq!(decoded, *store);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "a store written again as other bytes");
    source_map::assert_round_trip(source_map);
}

#[cfg(test)]
mod tests {
    use blaze_format::Aligned;
    use blaze_hash::HashSet;
    use hir_expand::fixture;

    use super::*;
    use crate::{
        DefWithBodyId, ModuleDefId,
        hir::{Expr, Pat},
        nameres::crate_def_map,
        signatures::FunctionSignature,
        test_db::{TestDB, TestDBExt},
        type_ref::TypeRef,
    };

    /// Which kind `expr` is, from the view, independently of the columns.
    fn expr_kind_of(expr: Expr<'_>) -> ExprKind {
        match expr {
            Expr::Missing => ExprKind::Missing,
            Expr::Path(_) => ExprKind::Path,
            Expr::If { .. } => ExprKind::If,
            Expr::Let { .. } => ExprKind::Let,
            Expr::Block { .. } => ExprKind::Block,
            Expr::Const(_) => ExprKind::Const,
            Expr::Loop { .. } => ExprKind::Loop,
            Expr::Call { .. } => ExprKind::Call,
            Expr::MethodCall { .. } => ExprKind::MethodCall,
            Expr::Match { .. } => ExprKind::Match,
            Expr::Continue { .. } => ExprKind::Continue,
            Expr::Break { .. } => ExprKind::Break,
            Expr::Return { .. } => ExprKind::Return,
            Expr::Become { .. } => ExprKind::Become,
            Expr::Yield { .. } => ExprKind::Yield,
            Expr::Yeet { .. } => ExprKind::Yeet,
            Expr::RecordLit { .. } => ExprKind::RecordLit,
            Expr::Field { .. } => ExprKind::Field,
            Expr::Await { .. } => ExprKind::Await,
            Expr::Cast { .. } => ExprKind::Cast,
            Expr::Ref { .. } => ExprKind::Ref,
            Expr::UnaryOp { .. } => ExprKind::UnaryOp,
            Expr::BinaryOp { .. } => ExprKind::BinaryOp,
            Expr::Assignment { .. } => ExprKind::Assignment,
            Expr::Index { .. } => ExprKind::Index,
            Expr::Closure { .. } => ExprKind::Closure,
            Expr::Tuple { .. } => ExprKind::Tuple,
            Expr::Array(_) => ExprKind::Array,
            Expr::Literal(_) => ExprKind::Literal,
            Expr::Underscore => ExprKind::Underscore,
            Expr::OffsetOf(_) => ExprKind::OffsetOf,
            Expr::InlineAsm(_) => ExprKind::InlineAsm,
            Expr::IncludeBytes => ExprKind::IncludeBytes,
            Expr::UnsafeBinderCast { .. } => ExprKind::UnsafeBinderCast,
        }
    }

    fn pat_kind_of(pat: Pat<'_>) -> PatKind {
        match pat {
            Pat::Missing => PatKind::Missing,
            Pat::Rest => PatKind::Rest,
            Pat::Wild => PatKind::Wild,
            Pat::Tuple { .. } => PatKind::Tuple,
            Pat::Or(_) => PatKind::Or,
            Pat::Record { .. } => PatKind::Record,
            Pat::Range { .. } => PatKind::Range,
            Pat::Slice { .. } => PatKind::Slice,
            Pat::Path(_) => PatKind::Path,
            Pat::Lit(_) => PatKind::Lit,
            Pat::Bind { .. } => PatKind::Bind,
            Pat::TupleStruct { .. } => PatKind::TupleStruct,
            Pat::Ref { .. } => PatKind::Ref,
            Pat::Box { .. } => PatKind::Box,
            Pat::Deref { .. } => PatKind::Deref,
            Pat::NotNull => PatKind::NotNull,
            Pat::Expr(_) => PatKind::Expr,
        }
    }

    fn type_kind_of(ty: TypeRef<'_>) -> TypeRefKind {
        match ty {
            TypeRef::Never => TypeRefKind::Never,
            TypeRef::Placeholder => TypeRefKind::Placeholder,
            TypeRef::Tuple(_) => TypeRefKind::Tuple,
            TypeRef::Path(_) => TypeRefKind::Path,
            TypeRef::RawPtr(..) => TypeRefKind::RawPtr,
            TypeRef::Reference(_) => TypeRefKind::Reference,
            TypeRef::Array(_) => TypeRefKind::Array,
            TypeRef::Slice(_) => TypeRefKind::Slice,
            TypeRef::Fn(_) => TypeRefKind::Fn,
            TypeRef::ImplTrait(_) => TypeRefKind::ImplTrait,
            TypeRef::DynTrait(_) => TypeRefKind::DynTrait,
            TypeRef::TypeParam(_) => TypeRefKind::TypeParam,
            TypeRef::PatternType(..) => TypeRefKind::PatternType,
            TypeRef::Error => TypeRefKind::Error,
            TypeRef::UnsafeBinder { .. } => TypeRefKind::UnsafeBinder,
        }
    }

    /// Checks what `read` reads in place against `store`.
    pub(super) fn assert_reads(read: ExpressionStoreRef<'_>, store: &ExpressionStore) {
        assert_eq!(read.has_exprs(), store.expr_only.is_some());
        assert_eq!(read.exprs_len(), store.exprs().count());
        assert_eq!(read.pats_len(), store.pats().count());
        assert_eq!(read.bindings_len(), store.bindings().count());
        assert_eq!(read.labels_len(), store.expr_only.as_ref().map_or(0, |it| it.labels.len()));
        assert_eq!(read.types_len(), store.types.len());
        assert_eq!(read.paths_len(), store.types.paths.len());
        assert_eq!(read.lifetimes_len(), store.lifetimes.len());
        for (id, expr) in store.exprs() {
            assert_eq!(read.expr_kind(id), Some(expr_kind_of(expr)), "{expr:?}");
            assert_eq!(read.inherits_overflow_checks(id), store.inherits_overflow_checks(id));
            assert_eq!(read.block_edition(id), store.block_edition(id));
        }
        for (id, pat) in store.pats() {
            assert_eq!(read.pat_kind(id), Some(pat_kind_of(pat)), "{pat:?}");
        }
        for (id, ty) in store.type_refs() {
            assert_eq!(read.type_kind(id), Some(type_kind_of(ty)), "{ty:?}");
        }
        for (id, lifetime) in store.lifetimes.iter() {
            let kind = LifetimeRefKind::from_u32(put_lifetime(lifetime, &mut Words::new()));
            assert_eq!(read.lifetime_kind(id), kind);
        }
        for (id, _) in store.bindings() {
            assert_eq!(read.binding_owner(id), store.binding_owner(id));
        }
        assert!(read.expr_roots().eq(store.expr_roots()));
        let blocks = store.expr_only.as_ref().map_or(&[][..], |it| &*it.block_scopes);
        assert!(read.block_scopes().eq(blocks.iter().copied()));
    }

    /// The functions of the fixture's crate root, each body lowered and round-tripped.
    pub(super) fn bodies(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> (TestDB, Vec<DefWithBodyId>) {
        let db = fixture::db(ra_fixture).into_db();
        let krate = db.fetch_test_crate();
        let def_map = crate_def_map(&db, krate);
        let mut defs: Vec<DefWithBodyId> = vec![];
        for (_, module) in def_map.modules(&db) {
            for decl in module.scope(&db).declarations() {
                if let ModuleDefId::FunctionId(it) = decl {
                    defs.push(it.into());
                }
            }
        }
        for &def in &defs {
            let (body, source_map) = Body::with_source_map(&db, def);
            assert_round_trip(body, source_map);
            if let Some((body, source_map)) = Body::relative(&db, def) {
                assert_round_trip(body, source_map);
            }
            if let DefWithBodyId::FunctionId(function) = def {
                let (signature, source_map) = FunctionSignature::with_source_map(&db, function);
                assert_store_round_trip(&signature.store, source_map);
            }
        }
        (db, defs)
    }

    const EVERY_KIND: &str = r#"
//- minicore: fmt, future, async_fn, iterator, range, try, option, coroutine, deref, index
struct S { a: u32, b: u32 }
enum E { A(u32), B { x: u32 }, C }
macro_rules! m { ($e:expr) => { $e }; }
macro_rules! p { ($p:pat) => { $p }; }

fn every_kind(x: u32, (y, _): (u32, u32)) -> u32 {
    let s = S { a: 1, b: 2 };
    let S { a, .. } = s;
    let t = S { a: 0, ..s };
    let _ = (1u8, 2i64, 3.0f32, 4.5, 'c', b'b', "s", b"bs", c"cs", true, 1_000u128, -7, 0x10usize);
    let arr = [1, 2, 3];
    let rep = [0u8; 4];
    let r = &arr[0];
    let rp = &raw const x;
    let _ = *r + 1 - 2 * 3 / 4 % 5 << 1 >> 1 & 1 | 1 ^ 1;
    let _ = !true && false || x == y && x != y && x < y && x <= y && x > y && x >= y;
    let _ = -(x as i64);
    let mut n = 0;
    n += 1;
    n = 2;
    (n, _) = (3, 4);
    let c = |a: u32, b| -> u32 { a + b };
    let mc = move || x;
    let fut = async { 1 };
    let ac = async move |z: u32| z;
    let co = #[coroutine] || { yield 1; };
    let tr = || -> Option<u32> { let v = Some(1)?; Some(v) };
    let tail = || become every_kind(1, (2, 3));
    let yeet = || -> Option<u32> { do yeet };
    let _ = c(1, 2);
    let _ = x.max::<u32>(1);
    let _ = s.a;
    let _ = x as u64;
    let _ = 0..1;
    let _ = ..;
    let _ = 1..=2;
    let _ = const { 5 };
    let _ = builtin#offset_of(S, a);
    let _ = builtin#format_args("{} {x} {:>5}", 1, 2.5);
    unsafe { builtin#asm("mov {0}, {1}", "nop", out(reg) n, in(reg) x, options(nostack, nomem)); }
    let _ = m!(1 + 1);
    let p!(q) = 5;
    'outer: loop {
        while x > 0 {
            break 'outer;
        }
        for i in 0..3 {
            continue;
        }
        break;
    }
    let v = 'blk: { break 'blk 1 };
    let _ = if let Some(w) = Some(1) { w } else if x == 0 { 0 } else { 1 };
    let Some(z) = Some(1) else { return 0 };
    match E::A(1) {
        E::A(0 | 1) => {}
        E::A(k @ 2..=5) if k > 3 => {}
        E::B { x: ref xx } => {}
        E::C => {}
        E::A(..) => {}
    }
    match &[1, 2, 3][..] {
        [first, .., last] => {}
        [] => {}
        _ => {}
    }
    match (1, 2) {
        (1, ..) => {}
        (j, l) => {}
    }
    let &bx = &1;
    let box bb = 1;
    let _ = async { fut.await };
    let _ = { struct Inner; 1 };
    macro_rules! inner { () => {} }
    #[cfg(any())]
    let off = 1;
    let _ = _;
    1
}
"#;

    #[test]
    fn every_kind_round_trips() {
        let (db, defs) = bodies(EVERY_KIND);
        let body = Body::of(&db, defs[0]);
        let buffer = Aligned::copy_from(&blaze_format::to_bytes(&*body));
        let read = Body::read(&buffer).expect("a body just written");
        let store = read.store();
        let kinds: HashSet<ExprKind> =
            (0..store.exprs_len() as u32).filter_map(|id| store.expr_kind(idx(id))).collect();
        for kind in [
            ExprKind::Path,
            ExprKind::If,
            ExprKind::Let,
            ExprKind::Block,
            ExprKind::Const,
            ExprKind::Loop,
            ExprKind::Call,
            ExprKind::MethodCall,
            ExprKind::Match,
            ExprKind::Continue,
            ExprKind::Break,
            ExprKind::Return,
            ExprKind::RecordLit,
            ExprKind::Field,
            ExprKind::Await,
            ExprKind::Cast,
            ExprKind::Ref,
            ExprKind::UnaryOp,
            ExprKind::BinaryOp,
            ExprKind::Assignment,
            ExprKind::Index,
            ExprKind::Closure,
            ExprKind::Tuple,
            ExprKind::Array,
            ExprKind::Literal,
            ExprKind::OffsetOf,
            ExprKind::InlineAsm,
        ] {
            assert!(kinds.contains(&kind), "no {kind:?} in the body");
        }
        let pats: HashSet<PatKind> =
            (0..store.pats_len() as u32).filter_map(|id| store.pat_kind(idx(id))).collect();
        for kind in [
            PatKind::Wild,
            PatKind::Tuple,
            PatKind::Or,
            PatKind::Record,
            PatKind::Range,
            PatKind::Slice,
            PatKind::Path,
            PatKind::Lit,
            PatKind::Bind,
            PatKind::TupleStruct,
            PatKind::Ref,
        ] {
            assert!(pats.contains(&kind), "no {kind:?} in the body");
        }
        assert_eq!(read.params_len(), 2);
    }

    #[test]
    fn signatures_round_trip() {
        bodies(
            r#"
//- minicore: fn, iterator, option, sized, clone
trait Tr<'a> { type Out; }
fn sig<'a, T: Clone + ?Sized, const N: usize>(
    a: &'a mut T,
    b: *const u8,
    c: [u8; N],
    d: &[T],
    e: fn(u32) -> !,
    g: impl Fn(u32) -> u32,
    h: &dyn Iterator<Item = u32>,
    i: (u32, &'static str),
    j: <T as Tr<'a>>::Out,
    k: impl for<'b> Tr<'b, Out = u32> + use<'a, T>,
) -> Option<[u8; { N + 1 }]>
where
    T: 'a,
{
    None
}
"#,
        );
    }

    #[test]
    fn closures_and_self_round_trip() {
        bodies(
            r#"
//- minicore: fn, future, async_fn
struct W(u32);
impl W {
    fn get(&self, v: u32) -> u32 {
        let f = |a: u32| {
            let inner = a + self.0;
            let g = move || inner;
            g()
        };
        f(v)
    }
}
async fn asynchronous(a: u32, (b, c): (u32, u32)) -> u32 { a + b + c }
fn main() {}
"#,
        );
    }

    /// `bytes` with the word at `at` replaced by `word`.
    pub(super) fn with_word(bytes: &[u8], at: usize, word: u32) -> Aligned {
        let mut bytes = bytes.to_vec();
        bytes[at..at + size_of::<u32>()].copy_from_slice(&word.to_le_bytes());
        Aligned::copy_from(&bytes)
    }

    pub(super) fn offset_in<T>(slice: &[T], buffer: &[u8]) -> usize {
        slice.as_ptr() as usize - buffer.as_ptr() as usize
    }

    #[test]
    fn a_corrupted_buffer_is_refused() {
        let (db, defs) = bodies(
            r#"
fn f() -> u32 {
    let t = (1, 2);
    if true { t.0 } else { 2 }
}
"#,
        );
        let body = Body::of(&db, defs[0]);
        let bytes = blaze_format::to_bytes(&body.store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStore::read(&buffer).expect("a store just written");
        let exprs = read.exprs_len() as u32;

        let data_len = at_of(&read.root.expr_data, &buffer) as usize + offset_of!(Slice<u32>, len);
        let short = with_word(&bytes, data_len, exprs - 1);
        assert!(ExpressionStore::read(&short).is_err(), "a data column of fewer words than nodes");

        let list = offset_in(read.expr_lists, &buffer);
        let past = with_word(&bytes, list, exprs);
        let past = ExpressionStore::read(&past).expect("only a list array changed");
        assert!(past.decode().is_err(), "an expression id past its arena in a list");

        assert_eq!(read.statements_len(), 1);
        assert_eq!(read.statement_kind(0), Some(StatementKind::Let));
        let mut bad = bytes.clone();
        bad[offset_in(read.stmt_kind, &buffer)] = StatementKind::ALL.len() as u8;
        assert!(ExpressionStore::read(&Aligned::copy_from(&bad)).is_err(), "a statement kind out of range");

        let data = offset_in(read.stmt_data, &buffer);
        let past = with_word(&bytes, data, read.extra.len() as u32);
        let past = ExpressionStore::read(&past).expect("only a statement's data word changed");
        assert!(past.decode().is_err(), "a `let` whose words begin past `extra`");

        let head = read.stmt_data[0] as usize;
        let flags = read.extra[head] & ((1 << columns::LET_BITS) - 1);
        let head_at = offset_in(read.extra, &buffer) + head * size_of::<u32>();
        let past = with_word(&bytes, head_at, (read.pats_len() as u32) << columns::LET_BITS | flags);
        let past = ExpressionStore::read(&past).expect("only `extra` changed");
        assert!(past.decode().is_err(), "a pattern id past its arena in a statement");

        let root = offset_in(read.expr_roots, &buffer) + offset_of!(ExprRootRec, root);
        assert!(ExpressionStore::read(&with_word(&bytes, root, exprs)).is_err());

        assert!(Body::read(&buffer).is_err(), "a store's buffer read as a body's");
        assert!(ExpressionStore::read(&Aligned::copy_from(&bytes[..bytes.len() - 16])).is_err());
    }

    #[test]
    fn a_corrupted_kind_or_list_range_is_refused() {
        let (db, defs) = bodies("fn f() -> (u32, u32, u32) { (1, 2, 3) }");
        let body = Body::of(&db, defs[0]);
        let columns = &body.store.expr_only.as_ref().expect("a body has expressions").columns;
        let (tuple, elements) = (body.store.exprs())
            .find_map(|(id, expr)| match expr {
                Expr::Tuple { exprs } if exprs.len() == 3 => Some((id, exprs)),
                _ => None,
            })
            .expect("the tuple");
        let start = (elements.as_ptr() as usize - columns.expr_lists.as_ptr() as usize)
            / size_of::<ExprId>();
        let range = (columns.extra.windows(2))
            .position(|pair| pair == [start as u32, 3])
            .expect("the tuple's list as a `(start, len)` pair in `extra`");

        let bytes = blaze_format::to_bytes(&body.store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStore::read(&buffer).expect("a store just written");
        assert_eq!(read.expr_kind(tuple), Some(ExprKind::Tuple));

        let kind = offset_in(read.expr_kind, &buffer) + raw(tuple) as usize;
        let mut bad = bytes.clone();
        bad[kind] = ExprKind::ALL.len() as u8;
        assert!(ExpressionStore::read(&Aligned::copy_from(&bad)).is_err(), "a kind out of range");

        let range_at = offset_in(read.extra, &buffer) + range * size_of::<u32>();
        let lists = read.expr_lists.len() as u32;
        let past = with_word(&bytes, range_at, lists);
        let past = ExpressionStore::read(&past).expect("only `extra` changed");
        assert!(past.decode().is_err(), "a list that starts past its array");

        let long = with_word(&bytes, range_at + size_of::<u32>(), lists + 1);
        let long = ExpressionStore::read(&long).expect("only `extra` changed");
        assert!(long.decode().is_err(), "a list that ends past its array");
    }

    #[test]
    fn a_corrupted_type_kind_or_list_range_is_refused() {
        let (db, defs) = bodies("fn f(a: (u32, u8)) {}");
        let DefWithBodyId::FunctionId(function) = defs[0] else { panic!("`f` is a function") };
        let (signature, _) = FunctionSignature::with_source_map(&db, function);
        let store: &ExpressionStore = &signature.store;
        let (tuple, elements) = (store.type_refs())
            .find_map(|(id, ty)| match ty {
                TypeRef::Tuple(elements) if elements.len() == 2 => Some((id, elements)),
                _ => None,
            })
            .expect("the tuple");
        let start = (elements.as_ptr() as usize - store.types.lists.as_ptr() as usize)
            / size_of::<TypeRefId>();
        let range = (store.types.extra.windows(2))
            .position(|pair| pair == [start as u32, 2])
            .expect("the tuple's list as a `(start, len)` pair in `extra`");

        let bytes = blaze_format::to_bytes(store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStore::read(&buffer).expect("a store just written");
        assert_eq!(read.type_kind(tuple), Some(TypeRefKind::Tuple));

        let kind = offset_in(read.ty_kind, &buffer) + raw(tuple) as usize;
        let mut bad = bytes.clone();
        bad[kind] = TypeRefKind::ALL.len() as u8;
        assert!(ExpressionStore::read(&Aligned::copy_from(&bad)).is_err(), "a type kind out of range");

        let range_at = offset_in(read.ty_extra, &buffer) + range * size_of::<u32>();
        let lists = read.ty_lists.len() as u32;
        let past = with_word(&bytes, range_at, lists);
        let past = ExpressionStore::read(&past).expect("only `ty_extra` changed");
        assert!(past.decode().is_err(), "a type list that starts past `lists`");

        let long = with_word(&bytes, range_at + size_of::<u32>(), lists + 1);
        let long = ExpressionStore::read(&long).expect("only `ty_extra` changed");
        assert!(long.decode().is_err(), "a type list that ends past `lists`");

        let element = offset_in(read.ty_lists, &buffer) + start * size_of::<u32>();
        let past = with_word(&bytes, element, read.types_len() as u32);
        let past = ExpressionStore::read(&past).expect("only `ty_lists` changed");
        assert!(past.decode().is_err(), "a tuple element past the types");
    }

    #[test]
    fn a_corrupted_path_column_is_refused() {
        let (db, defs) = bodies("fn f(a: Option<u32>) -> Option<u32> { a }");
        let DefWithBodyId::FunctionId(function) = defs[0] else { panic!("`f` is a function") };
        let (signature, _) = FunctionSignature::with_source_map(&db, function);
        let store: &ExpressionStore = &signature.store;
        let bytes = blaze_format::to_bytes(store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStore::read(&buffer).expect("a store just written");
        let paths = read.paths_len() as u32;
        assert!(paths > 0);

        let mut bad = bytes.clone();
        bad[offset_in(read.path_kind, &buffer)] = PathTag::ALL.len() as u8;
        assert!(ExpressionStore::read(&Aligned::copy_from(&bad)).is_err(), "a path kind out of range");

        let data_len = at_of(&read.root.path_data, &buffer) as usize + offset_of!(Slice<u32>, len);
        let short = with_word(&bytes, data_len, paths - 1);
        assert!(ExpressionStore::read(&short).is_err(), "a path data column of fewer words than paths");

        let slot = (read.path_slots.iter().position(|&word| word != 0))
            .expect("`Option<u32>`'s segment has arguments");
        let slot_at = offset_in(read.path_slots, &buffer) + slot * size_of::<u32>();
        let rows = store.types.paths.generic_args.len() as u32;
        let past = with_word(&bytes, slot_at, rows + 1);
        let past = ExpressionStore::read(&past).expect("only a slot changed");
        assert!(past.decode().is_err(), "a slot naming generic arguments past their rows");

        let bare = (store.types.paths.kind.iter())
            .position(|&kind| kind == PathTag::Bare as u8)
            .expect("`u32` is a bare path");
        let data_at = offset_in(read.path_data, &buffer) + bare * size_of::<u32>();
        let past = with_word(&bytes, data_at, store.types.paths.mod_paths.len() as u32);
        let past = ExpressionStore::read(&past).expect("only a path's data word changed");
        assert!(past.decode().is_err(), "a bare path's mod path past its table");
    }

    /// `decode_in` of a store in an image is `decode`'s store, with the `Copy` columns the
    /// buffer holds as they are read where they are; one outside the image is refused.
    #[test]
    fn decode_in_reads_in_its_image() {
        use std::sync::Arc;

        use crate::deps::Held;

        let (db, defs) = bodies(EVERY_KIND);
        let body = Body::of(&db, defs[0]);
        let bytes = blaze_format::to_bytes(&body.store);
        let image: ImageBuf = Arc::new(Aligned::copy_from(&bytes));
        let read = ExpressionStore::read(image.bytes()).expect("a store just written");
        let decoded = read.decode_in(&image).expect("a store in its image");
        assert_eq!(decoded, read.decode().expect("a store just written"));
        assert_eq!(decoded, body.store);

        let only = decoded.expr_only.as_deref().expect("a body has expressions");
        let columns = &only.columns;
        for (name, in_image) in [
            ("expr_kind", columns.expr_kind.in_image()),
            ("expr_data", columns.expr_data.in_image()),
            ("pat_kind", columns.pat_kind.in_image()),
            ("pat_data", columns.pat_data.in_image()),
            ("extra", columns.extra.in_image()),
            ("stmt_kind", columns.stmt_kind.in_image()),
            ("stmt_data", columns.stmt_data.in_image()),
            ("binding_owners", only.binding_owners.in_image()),
            ("ty_kind", decoded.types.kind.in_image()),
            ("ty_data", decoded.types.data.in_image()),
        ] {
            assert!(in_image, "{name} is read in the image");
        }
        let image_bytes = image.bytes().as_ptr_range();
        assert!(image_bytes.contains(&columns.expr_kind.as_ptr()), "expr_kind points into it");
        assert!(columns.expr_lists.in_slab(), "a column decoded from ids is copied");

        let other: ImageBuf = Arc::new(Aligned::copy_from(&bytes));
        assert!(read.decode_in(&other).is_err(), "a buffer outside its image");
    }

    /// Names are in the buffer as their text, which every process reads the same, and not as
    /// this process's symbol ids: dependency signature images carry this encoding and are
    /// shared between daemons that number their symbols each their own way.
    #[test]
    fn names_are_written_as_text() {
        let (db, defs) = bodies(EVERY_KIND);
        let body = Body::of(&db, defs[0]);
        let bytes = blaze_format::to_bytes(&*body);
        let mut names = Vec::new();
        for (_, expr) in body.store.exprs() {
            match expr {
                Expr::MethodCall { method_name, .. } => names.push(method_name),
                Expr::RecordLit { fields, .. } => names.extend(fields.iter().map(|it| &it.name)),
                _ => {}
            }
        }
        // A text of a byte or two could be in the buffer by chance.
        let long: Vec<&str> =
            names.iter().map(|name| name.symbol().as_str()).filter(|it| it.len() >= 3).collect();
        assert!(!long.is_empty(), "the fixture calls a method of a long name");
        for text in long {
            let text = text.as_bytes();
            assert!(bytes.windows(text.len()).any(|it| it == text), "{text:?} written as text");
        }
    }

    /// An encode on warm per-thread scratch writes the same bytes as on cold and grows none of
    /// the scratch's buffers, and its [`Words`] are this thread's spares, so into an output
    /// that has the room it allocates nothing.
    #[test]
    fn a_warm_encode_reuses_its_scratch() {
        let (db, defs) = bodies(EVERY_KIND);
        let body = Body::of(&db, defs[0]);
        let reference = blaze_format::to_bytes(&*body);
        let room = || {
            Writer::with_buffer(<Body as Persist>::SCHEMA, Vec::with_capacity(2 * reference.len()))
        };
        ENCODE.with(|it| *it.borrow_mut() = EncodeScratch::default());
        let mut out = room();
        let root = body.write(&mut out);
        assert_eq!(out.finish(root), reference, "the same bytes on fresh scratch");
        let held = ENCODE.with(|it| it.borrow().capacity());
        assert_ne!(held, 0, "the encode kept its buffers");

        let mut out = room();
        let root = blaze_alloc_count::assert_at_most(0, "a warm encode", || body.write(&mut out));
        assert_eq!(out.finish(root), reference, "the same bytes on reused scratch");
        assert_eq!(ENCODE.with(|it| it.borrow().capacity()), held, "a buffer grew on a warm encode");
    }
}
