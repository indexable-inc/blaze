use std::{borrow::Cow, fmt, hash::Hash, marker::PhantomData, num::{NonZero, NonZeroU32}};

use base_db::Db;
use hir_def::{AdtId, VariantId};
use macros::{GenericTypeVisitable, TypeFoldable, TypeVisitable};
use rustc_abi::{Size, TargetDataLayout};
use blaze_hashcons::Interned;
use rustc_type_ir::{GenericTypeVisitable, TypeFoldable, TypeVisitable, inherent::IntoKind};

use crate::{
    MemoryMap, ParamEnvAndCrate, PointerAt, consteval,
    db::HirDatabase,
    layout::{InternedLayout, Layout},
    mir::{IsSigned, pad16},
    next_solver::{
        Const, Consts, ErrorGuaranteed, TyKind, WorldExposer, ensure_sufficient_stack,
        interner::table,
    },
    utils::detect_variant_from_bytes,
};

use super::{DbInterner, Ty};

pub type ValTreeKind<'db> = rustc_type_ir::ValTreeKind<DbInterner<'db>>;

/// A type-level constant value.
///
/// Represents a typed, fully evaluated constant.
#[derive(
    Debug, Copy, Clone, Eq, PartialEq, Hash, TypeFoldable, TypeVisitable, GenericTypeVisitable,
)]
pub struct ValueConst<'db> {
    pub ty: Ty<'db>,
    pub value: ValTree<'db>,
}

impl<'db> ValueConst<'db> {
    #[inline]
    pub fn new(ty: Ty<'db>, value: ValTree<'db>) -> Self {
        ValueConst { ty, value }
    }

    /// Attempts to convert to a `ValTreeKind::Leaf` value.
    pub fn try_to_leaf(self) -> Option<ScalarInt> {
        match self.value.inner() {
            ValTreeKind::Leaf(s) => Some(*s),
            ValTreeKind::Branch(_) => None,
        }
    }

    /// Attempts to extract the raw bits from the constant.
    ///
    /// Fails if the value can't be represented as bits (e.g. because it is a reference
    /// or an aggregate).
    #[inline]
    pub fn try_to_bits(
        self,
        db: &'db Db,
        param_env: ParamEnvAndCrate<'db>,
    ) -> Option<u128> {
        let (TyKind::Bool | TyKind::Char | TyKind::Uint(_) | TyKind::Int(_) | TyKind::Float(_)) =
            self.ty.kind()
        else {
            return None;
        };
        let scalar = self.try_to_leaf()?;
        let size = db.layout_of_ty(self.ty.store(), param_env.store()).ok()?.size;
        Some(scalar.to_bits(size))
    }

    pub fn try_to_target_usize(self, data_layout: &TargetDataLayout) -> Option<u64> {
        if !self.ty.is_usize() {
            return None;
        }
        self.try_to_leaf().map(|s| s.to_target_usize(data_layout))
    }
}

/// rustc's `VALTREE_MAX_NODES`: the most nodes a value tree may have.
pub const VALTREE_MAX_NODES: usize = 100000;

/// Why a constant has no value tree, as rustc's `ValTreeCreationError`. The caller reports
/// it: rustc's "maximum number of nodes exceeded in constant" for [`Self::NodesOverflow`], and
/// for [`Self::NonSupportedType`] whatever the constant was for (E0741 for the type of a const
/// parameter, "cannot be used as pattern" for a pattern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValTreeCreationError<'db> {
    /// The tree would have more than [`VALTREE_MAX_NODES`] nodes.
    NodesOverflow,
    /// No value of this type, met inside the constant, has a value tree.
    NonSupportedType(Ty<'db>),
    /// The constant's type holds an error, already reported.
    ErrorGuaranteed(ErrorGuaranteed),
    /// The evaluated bytes are not a value of `ty`: `what`. The evaluation that made them
    /// and this reader disagree, which rustc's validation of the value rules out.
    Unreadable { ty: Ty<'db>, what: &'static str },
}

type Made<'db> = Result<ValTree<'db>, ValTreeCreationError<'db>>;

fn unreadable<'db>(ty: Ty<'db>, what: &'static str) -> ValTreeCreationError<'db> {
    ValTreeCreationError::Unreadable { ty, what }
}

/// The type-level constant of the value of `ty` that `memory` holds, its pointers resolved
/// through `memory_map`: rustc's `const_to_valtree_inner` from the evaluated value's place.
pub(super) fn allocation_to_const<'db>(
    interner: DbInterner<'db>,
    ty: Ty<'db>,
    memory: &[u8],
    memory_map: &MemoryMap<'db>,
    param_env: ParamEnvAndCrate<'db>,
) -> Result<Const<'db>, ValTreeCreationError<'db>> {
    let data_layout = interner
        .db
        .target_data_layout(param_env.krate)
        .map_err(|_| unreadable(ty, "the target's data layout did not load"))?;
    let mut reader = Reader { interner, param_env, data_layout, nodes: 0 };
    let valtree = reader.value(ty, memory, memory_map, PointerAt::Value(0), None)?;
    Ok(Const::new_value(interner, valtree, ty))
}

/// One conversion of evaluated bytes to a value tree.
struct Reader<'a, 'db> {
    interner: DbInterner<'db>,
    param_env: ParamEnvAndCrate<'db>,
    data_layout: &'a TargetDataLayout,
    /// rustc's `num_nodes`: the leaves, function items and empty branches made so far.
    nodes: usize,
}

impl<'db> Reader<'_, 'db> {
    fn layout(
        &self,
        ty: Ty<'db>,
    ) -> Result<Interned<'static, InternedLayout>, ValTreeCreationError<'db>> {
        self.interner
            .db
            .layout_of_ty(ty.store(), self.param_env.store())
            .map_err(|_| unreadable(ty, "its layout could not be computed"))
    }

    /// The bytes of a target `usize`, little-endian, as a host one.
    fn word(&self, ty: Ty<'db>, bytes: &[u8]) -> Result<usize, ValTreeCreationError<'db>> {
        let scalar = scalar(ty, bytes, self.data_layout.pointer_size())?;
        usize::try_from(scalar.to_bits_unchecked())
            .map_err(|_| unreadable(ty, "a target address wider than the host's"))
    }

    fn leaf(&mut self, scalar: ScalarInt) -> Made<'db> {
        self.nodes += 1;
        Ok(ValTree::new(ValTreeKind::Leaf(scalar)))
    }

    /// rustc's `const_to_valtree_inner`: the tree of the value of `ty` in `bytes`, which are at
    /// `at` in `map`, whose pointers `map` resolves. `len` is the length of an unsized value
    /// (`str`, a slice, or a struct or tuple ending in one) from the metadata of the pointer it
    /// was read through.
    fn value(
        &mut self,
        ty: Ty<'db>,
        bytes: &[u8],
        map: &MemoryMap<'db>,
        at: PointerAt,
        len: Option<usize>,
    ) -> Made<'db> {
        if self.nodes >= VALTREE_MAX_NODES {
            return Err(ValTreeCreationError::NodesOverflow);
        }
        let pointer = self.data_layout.pointer_size();
        match ty.kind() {
            TyKind::FnDef(..) => {
                self.nodes += 1;
                Ok(ValTree::zst())
            }
            TyKind::Bool => match scalar(ty, bytes, Size::from_bytes(1))?.to_bits_unchecked() {
                0 => self.leaf(ScalarInt::FALSE),
                1 => self.leaf(ScalarInt::TRUE),
                _ => Err(unreadable(ty, "a bool that is neither 0 nor 1")),
            },
            TyKind::Char => {
                let it = scalar(ty, bytes, Size::from_bytes(4))?;
                if char::from_u32(it.to_u32()).is_none() {
                    return Err(unreadable(ty, "a char that is no Unicode scalar value"));
                }
                self.leaf(it)
            }
            TyKind::Int(int) => {
                self.leaf(scalar(ty, bytes, int.bit_width().map_or(pointer, Size::from_bits))?)
            }
            TyKind::Uint(uint) => {
                self.leaf(scalar(ty, bytes, uint.bit_width().map_or(pointer, Size::from_bits))?)
            }
            TyKind::Float(float) => {
                let bits = match float {
                    rustc_ast_ir::FloatTy::F16 => 16,
                    rustc_ast_ir::FloatTy::F32 => 32,
                    rustc_ast_ir::FloatTy::F64 => 64,
                    rustc_ast_ir::FloatTy::F128 => 128,
                };
                self.leaf(scalar(ty, bytes, Size::from_bits(bits))?)
            }
            TyKind::Pat(base, _) => {
                ensure_sufficient_stack(|| self.value(base, bytes, map, at, len))
            }
            // A thin raw pointer is a leaf when it is an integer in disguise, a word without
            // provenance; a wide one is refused even then, as rustc does.
            TyKind::RawPtr(pointee, _) => {
                let db = self.interner.db;
                let tail = crate::layout::struct_tail_erasing_lifetimes(db, pointee, |it| it);
                let wide =
                    matches!(tail.kind(), TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..));
                if wide || map.has_provenance(at) {
                    return Err(ValTreeCreationError::NonSupportedType(ty));
                }
                Ok(ValTree::new(ValTreeKind::Leaf(scalar(ty, bytes, pointer)?)))
            }
            TyKind::FnPtr(..) => Err(ValTreeCreationError::NonSupportedType(ty)),
            TyKind::Ref(_, pointee, _) => self.deref(ty, pointee, bytes, map),
            TyKind::Str => {
                let len = len.ok_or_else(|| unreadable(ty, "a str read through no pointer"))?;
                self.bytes(ty, bytes, len)
            }
            TyKind::Slice(elem) => {
                let len = len.ok_or_else(|| unreadable(ty, "a slice read through no pointer"))?;
                self.elements(ty, elem, bytes, map, at, len)
            }
            TyKind::Array(elem, count) => {
                let len = consteval::try_const_usize(self.interner.db, count)
                    .and_then(|it| usize::try_from(it).ok())
                    .ok_or_else(|| unreadable(ty, "an array of no known length"))?;
                self.elements(ty, elem, bytes, map, at, len)
            }
            TyKind::Tuple(tys) => {
                let layout = self.layout(ty)?;
                self.branches(&layout, tys.iter(), None, bytes, map, at, len)
            }
            TyKind::Adt(def, _) if matches!(def.def_id(), AdtId::UnionId(_)) => {
                Err(ValTreeCreationError::NonSupportedType(ty))
            }
            TyKind::Adt(def, args) => {
                let db = self.interner.db;
                let layout = self.layout(ty)?;
                let (variant, index, layout): (VariantId, Option<u32>, Cow<'_, Layout>) =
                    match def.def_id() {
                        AdtId::UnionId(_) => return Err(ValTreeCreationError::NonSupportedType(ty)),
                        AdtId::StructId(id) => (id.into(), None, Cow::Borrowed(&**layout)),
                        AdtId::EnumId(id) => {
                            if matches!(layout.variants, hir_def::layout::Variants::Empty) {
                                return Err(unreadable(ty, "a value of an enum with no variants"));
                            }
                            if bytes.len() < layout.size.bytes_usize() {
                                return Err(unreadable(ty, "fewer bytes than the enum takes"));
                            }
                            let (variant, layout) =
                                detect_variant_from_bytes(&layout, db, self.data_layout, bytes, id)
                                    .ok_or_else(|| unreadable(ty, "a tag that names no variant"))?;
                            let index = id
                                .enum_variants(db)
                                .variants
                                .values()
                                .position(|it| it.0 == variant)
                                .ok_or_else(|| unreadable(ty, "a variant not of its enum"))?;
                            (variant.into(), Some(index as u32), layout)
                        }
                    };
                let interner = self.interner;
                let fields = db.field_types(variant);
                let tys = fields
                    .values()
                    .map(|field| field.ty().instantiate(interner, args).skip_norm_wip());
                self.branches(&layout, tys, index, bytes, map, at, len)
            }
            TyKind::Error(guar) => Err(ValTreeCreationError::ErrorGuaranteed(guar)),
            TyKind::Dynamic(..)
            | TyKind::Never
            | TyKind::Foreign(_)
            | TyKind::Closure(..)
            | TyKind::CoroutineClosure(..)
            | TyKind::Coroutine(..)
            | TyKind::CoroutineWitness(..)
            | TyKind::UnsafeBinder(_)
            | TyKind::Alias(..)
            | TyKind::Param(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Infer(_) => Err(ValTreeCreationError::NonSupportedType(ty)),
        }
    }

    /// rustc's `deref_pointer` and the tree of the place it gives: the value of `pointee` that
    /// `bytes`, a value of the pointer type `ty`, points to.
    fn deref(
        &mut self,
        ty: Ty<'db>,
        pointee: Ty<'db>,
        bytes: &[u8],
        map: &MemoryMap<'db>,
    ) -> Made<'db> {
        let word = self.data_layout.pointer_size().bytes_usize();
        let tail = crate::layout::struct_tail_erasing_lifetimes(self.interner.db, pointee, |it| it);
        let wide = match tail.kind() {
            TyKind::Str | TyKind::Slice(_) => true,
            // Where rustc's walk would stop at the tail, after the fields before it.
            TyKind::Dynamic(..) | TyKind::Foreign(_) => {
                return Err(ValTreeCreationError::NonSupportedType(tail));
            }
            _ => false,
        };
        if bytes.len() != word * (1 + usize::from(wide)) {
            return Err(unreadable(ty, "a pointer of the wrong size"));
        }
        let addr = self.word(ty, &bytes[..word])?;
        let (len, size) = match wide {
            true => {
                let len = self.word(ty, &bytes[word..])?;
                (Some(len), self.unsized_size(pointee, len)?)
            }
            false => (None, self.layout(pointee)?.size.bytes_usize()),
        };
        if let Some((id, offset)) = map.static_at(addr) {
            let value = self.interner.db.const_eval_static(id).map_err(|_| {
                unreadable(pointee, "a reference to a static that failed to evaluate")
            })?;
            let bytes = offset
                .checked_add(size)
                .and_then(|end| value.memory.get(offset..end))
                .ok_or_else(|| unreadable(pointee, "a reference past the end of its static"))?;
            let at = PointerAt::Value(offset);
            return ensure_sufficient_stack(|| {
                self.value(pointee, bytes, &value.memory_map, at, len)
            });
        }
        let bytes = match size {
            0 => &[][..],
            _ => map.get(addr, size).filter(|it| it.len() == size).ok_or_else(|| {
                unreadable(pointee, "a reference to memory the map does not hold")
            })?,
        };
        ensure_sufficient_stack(|| self.value(pointee, bytes, map, PointerAt::Memory(addr), len))
    }

    /// The size of an unsized value of `ty` whose pointer's metadata is `len`, as the
    /// evaluator's `size_of_unsized_struct` computes it.
    fn unsized_size(&self, ty: Ty<'db>, len: usize) -> Result<usize, ValTreeCreationError<'db>> {
        let too_big = || unreadable(ty, "an unsized value larger than the address space");
        match ty.kind() {
            TyKind::Str => Ok(len),
            TyKind::Slice(elem) => {
                self.layout(elem)?.size.bytes_usize().checked_mul(len).ok_or_else(too_big)
            }
            TyKind::Adt(..) | TyKind::Tuple(_) => {
                let layout = self.layout(ty)?;
                let (index, last) = self
                    .tail_field(ty)
                    .ok_or_else(|| unreadable(ty, "an unsized type with no fields"))?;
                let start = layout.fields.offset(index).bytes_usize();
                let tail = self.unsized_size(last, len)?;
                let align = usize::try_from(layout.align.bytes()).map_err(|_| too_big())?;
                start
                    .checked_add(tail)
                    .and_then(|end| end.checked_next_multiple_of(align))
                    .ok_or_else(too_big)
            }
            _ => Err(unreadable(ty, "an unsized type that is no str, slice, struct or tuple")),
        }
    }

    /// The index and type of the last field of the struct or tuple `ty`.
    fn tail_field(&self, ty: Ty<'db>) -> Option<(usize, Ty<'db>)> {
        match ty.kind() {
            TyKind::Adt(def, args) => {
                let AdtId::StructId(id) = def.def_id() else { return None };
                let fields = self.interner.db.field_types(id.into());
                let last = fields.values().next_back()?;
                let last = last.ty().instantiate(self.interner, args).skip_norm_wip();
                Some((fields.len() - 1, last))
            }
            TyKind::Tuple(tys) => Some((tys.len().checked_sub(1)?, *tys.as_slice().last()?)),
            _ => None,
        }
    }

    /// rustc's `branches`: for an enum its variant's index as a `u32` leaf, then the tree of
    /// each field. The last field gets `len`, the metadata of an unsized tail.
    fn branches(
        &mut self,
        layout: &Layout,
        tys: impl ExactSizeIterator<Item = Ty<'db>>,
        variant: Option<u32>,
        bytes: &[u8],
        map: &MemoryMap<'db>,
        at: PointerAt,
        len: Option<usize>,
    ) -> Made<'db> {
        let interner = self.interner;
        let count = tys.len();
        let index = variant.map(|it| {
            let leaf = ValTree::new(ValTreeKind::Leaf(ScalarInt::from(it)));
            Ok(Const::new_value(interner, leaf, interner.default_types().types.u32))
        });
        let fields = tys.enumerate().map(|(i, ty)| {
            let offset = layout.fields.offset(i).bytes_usize();
            let (size, len) = match len {
                Some(len) if i + 1 == count => (self.unsized_size(ty, len)?, Some(len)),
                _ => (self.layout(ty)?.size.bytes_usize(), None),
            };
            let bytes = offset
                .checked_add(size)
                .and_then(|end| bytes.get(offset..end))
                .ok_or_else(|| unreadable(ty, "a field outside the value it is of"))?;
            let at = shifted(at, offset);
            let tree = ensure_sufficient_stack(|| self.value(ty, bytes, map, at, len))?;
            Ok(Const::new_value(interner, tree, ty))
        });
        let consts = Consts::new_from_iter(index.into_iter().chain(fields))?;
        if consts.is_empty() {
            self.nodes += 1;
        }
        Ok(ValTree::new(ValTreeKind::Branch(consts)))
    }

    /// rustc's `slice_branches` for a `str` or `[u8]`: a branch of `len` `u8` leaves, the
    /// shared constants of each byte.
    fn bytes(&mut self, ty: Ty<'db>, bytes: &[u8], len: usize) -> Made<'db> {
        if bytes.len() != len {
            return Err(unreadable(ty, "a length its bytes do not have"));
        }
        // rustc counts each leaf as it makes it, and stops at the one that would reach the
        // budget; `nodes` is below it here.
        if len > VALTREE_MAX_NODES - self.nodes {
            return Err(ValTreeCreationError::NodesOverflow);
        }
        self.nodes += len;
        let u8_values = &self.interner.default_types().consts.u8_values;
        let elements =
            Consts::new_from_iter(bytes.iter().map(|&byte| u8_values[usize::from(byte)]));
        Ok(ValTree::new(ValTreeKind::Branch(elements)))
    }

    /// rustc's `slice_branches`: a branch of the tree of each of the `len` elements of type
    /// `elem` in `bytes`, a value of `ty`. No node of its own, even when empty.
    fn elements(
        &mut self,
        ty: Ty<'db>,
        elem: Ty<'db>,
        bytes: &[u8],
        map: &MemoryMap<'db>,
        at: PointerAt,
        len: usize,
    ) -> Made<'db> {
        if let TyKind::Uint(rustc_ast_ir::UintTy::U8) = elem.kind() {
            return self.bytes(ty, bytes, len);
        }
        let size = self.layout(elem)?.size.bytes_usize();
        if size.checked_mul(len) != Some(bytes.len()) {
            return Err(unreadable(ty, "a length its bytes do not have"));
        }
        let interner = self.interner;
        let elements = (0..len).map(|i| {
            let bytes = &bytes[i * size..(i + 1) * size];
            let at = shifted(at, i * size);
            let tree = ensure_sufficient_stack(|| self.value(elem, bytes, map, at, None))?;
            Ok(Const::new_value(interner, tree, elem))
        });
        Ok(ValTree::new(ValTreeKind::Branch(Consts::new_from_iter(elements)?)))
    }
}

/// The word `by` bytes past `at`.
fn shifted(at: PointerAt, by: usize) -> PointerAt {
    match at {
        PointerAt::Value(offset) => PointerAt::Value(offset + by),
        PointerAt::Memory(addr) => PointerAt::Memory(addr + by),
    }
}

/// A leaf of the little-endian bits in `bytes`, which must be `size` long.
fn scalar<'db>(
    ty: Ty<'db>,
    bytes: &[u8],
    size: Size,
) -> Result<ScalarInt, ValTreeCreationError<'db>> {
    if size.bytes() == 0 || bytes.len() as u64 != size.bytes() || bytes.len() > 16 {
        return Err(unreadable(ty, "a scalar of the wrong size"));
    }
    Ok(ScalarInt::raw(u128::from_le_bytes(pad16(bytes, IsSigned::No)), size))
}

impl<'db> rustc_type_ir::inherent::ValueConst<DbInterner<'db>> for ValueConst<'db> {
    #[inline]
    fn new(ty: Ty<'db>, valtree: ValTree<'db>) -> Self {
        ValueConst { ty, value: valtree }
    }

    fn ty(self) -> Ty<'db> {
        self.ty
    }

    fn valtree(self) -> ValTree<'db> {
        self.value
    }

    fn try_to_target_usize(self, interner: DbInterner<'db>) -> Option<u64> {
        ValueConst::try_to_target_usize(self, rustc_abi::HasDataLayout::data_layout(&interner))
    }
}

impl fmt::Display for ValueConst<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.ty.kind(), self.value.inner()) {
            (TyKind::Bool, ValTreeKind::Leaf(leaf)) => match leaf.try_to_bool() {
                Ok(value) => write!(f, "{value}"),
                Err(()) => write!(f, "{leaf:?}_bool"),
            },
            (TyKind::Char, ValTreeKind::Leaf(leaf)) => match char::try_from(*leaf) {
                Ok(value) => write!(f, "{value:?}"),
                Err(_) => write!(f, "{leaf:?}_char"),
            },
            (TyKind::Int(int), ValTreeKind::Leaf(leaf)) => {
                write!(f, "{}_{}", leaf.to_int_unchecked(), int.name_str())
            }
            (TyKind::Uint(uint), ValTreeKind::Leaf(leaf)) => {
                write!(f, "{}_{}", leaf.to_uint_unchecked(), uint.name_str())
            }
            (_, valtree) => write!(f, "{valtree:?}: {:?}", self.ty),
        }
    }
}

/// The value a [`ScalarInt`] leaf stands for (`Interner::Scalar`). A value tree's leaf is always
/// an integer (hir-ty keeps no pointer in a leaf), so the scalar is the integer itself.
pub type Scalar = ScalarInt;

impl<'db> rustc_type_ir::inherent::ScalarInt<DbInterner<'db>> for ScalarInt {
    #[inline]
    fn to_scalar(self) -> Scalar {
        self
    }

    #[inline]
    fn try_from_uint(i: impl Into<u128>, size: Size) -> Option<Self> {
        ScalarInt::try_from_uint(i, size)
    }
}

/// Every interned [`ValTree`], kept whole: [`ValTree::inner`] hands out a reference.
static VALTREES: table::Values<ValTreeKind<'static>> = table::Values::new("valtrees");
table::erase!(ValTreeKind);

/// A value tree: its row in [`VALTREES`], which is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ValTree<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<ValTree<'static>>>() == 4);

// SAFETY: the handle holds one value, its row's kind, which this visits whenever the visitor
// asks to go into the row.
unsafe impl<'db, V: WorldExposer> GenericTypeVisitable<V> for ValTree<'db> {
    fn generic_visit_with(&self, visitor: &mut V) {
        if visitor.on_interned(VALTREES.name(), self.index.get()).is_continue() {
            self.inner().generic_visit_with(visitor);
        }
    }
}

impl<'db> TypeVisitable<DbInterner<'db>> for ValTree<'db> {
    fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        self.inner().visit_with(visitor)
    }
}

impl<'db> TypeFoldable<DbInterner<'db>> for ValTree<'db> {
    fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        self.inner().try_fold_with(folder).map(ValTree::new)
    }

    fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(self, folder: &mut F) -> Self {
        ValTree::new(self.inner().fold_with(folder))
    }
}

impl<'db> IntoKind for ValTree<'db> {
    type Kind = ValTreeKind<'db>;

    fn kind(self) -> Self::Kind {
        *self.inner()
    }
}

impl<'db> ValTree<'db> {
    #[inline]
    pub fn new(kind: ValTreeKind<'db>) -> Self {
        Self::at(VALTREES.intern(kind))
    }

    #[inline]
    pub fn inner(&self) -> &ValTreeKind<'db> {
        VALTREES.get(self.index)
    }

    pub fn from_scalar_int(_interner: DbInterner<'db>, i: ScalarInt) -> Self {
        ValTree::new(ValTreeKind::Leaf(i))
    }

    /// The value of a zero-sized type, as rustc's `ValTree::zst`: a branch with no fields.
    #[inline]
    pub fn zst() -> Self {
        ValTree::new(ValTreeKind::Branch(Consts::empty()))
    }

    #[inline]
    pub(in super::super) fn at(index: NonZeroU32) -> Self {
        ValTree { index, _db: PhantomData }
    }

    #[inline]
    pub(in super::super) fn row(self) -> NonZeroU32 {
        self.index
    }
}

impl std::fmt::Debug for ValTree<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner().fmt(f)
    }
}

/// The raw bytes of a simple value.
///
/// This is a packed struct in order to allow this type to be optimally embedded in enums
/// (like Scalar).
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
#[repr(Rust, packed)]
pub struct ScalarInt {
    /// The first `size` bytes of `data` are the value.
    /// Do not try to read less or more bytes than that. The remaining bytes must be 0.
    data: u128,
    size: NonZero<u8>,
}

impl ScalarInt {
    pub const TRUE: ScalarInt = ScalarInt { data: 1_u128, size: NonZero::new(1).unwrap() };
    pub const FALSE: ScalarInt = ScalarInt { data: 0_u128, size: NonZero::new(1).unwrap() };

    fn raw(data: u128, size: Size) -> Self {
        Self { data, size: NonZero::new(size.bytes() as u8).unwrap() }
    }

    #[inline]
    pub fn size(self) -> Size {
        Size::from_bytes(self.size.get())
    }

    /// Make sure the `data` fits in `size`.
    /// This is guaranteed by all constructors here, but having had this check saved us from
    /// bugs many times in the past, so keeping it around is definitely worth it.
    #[inline(always)]
    fn check_data(self) {
        // Using a block `{self.data}` here to force a copy instead of using `self.data`
        // directly, because `debug_assert_eq` takes references to its arguments and formatting
        // arguments and would thus borrow `self.data`. Since `Self`
        // is a packed struct, that would create a possibly unaligned reference, which
        // is UB.
        debug_assert_eq!(
            self.size().truncate(self.data),
            { self.data },
            "Scalar value {:#x} exceeds size of {} bytes",
            { self.data },
            self.size
        );
    }

    #[inline]
    pub fn null(size: Size) -> Self {
        Self::raw(0, size)
    }

    #[inline]
    pub fn is_null(self) -> bool {
        self.data == 0
    }

    #[inline]
    pub fn try_from_uint(i: impl Into<u128>, size: Size) -> Option<Self> {
        let (r, overflow) = Self::truncate_from_uint(i, size);
        if overflow { None } else { Some(r) }
    }

    /// Returns the truncated result, and whether truncation changed the value.
    #[inline]
    pub fn truncate_from_uint(i: impl Into<u128>, size: Size) -> (Self, bool) {
        let data = i.into();
        let r = Self::raw(size.truncate(data), size);
        (r, r.data != data)
    }

    #[inline]
    pub fn try_from_int(i: impl Into<i128>, size: Size) -> Option<Self> {
        let (r, overflow) = Self::truncate_from_int(i, size);
        if overflow { None } else { Some(r) }
    }

    /// Returns the truncated result, and whether truncation changed the value.
    #[inline]
    pub fn truncate_from_int(i: impl Into<i128>, size: Size) -> (Self, bool) {
        let data = i.into();
        // `into` performed sign extension, we have to truncate
        let r = Self::raw(size.truncate(data as u128), size);
        (r, size.sign_extend(r.data) != data)
    }

    #[inline]
    pub fn try_from_target_usize(
        i: impl Into<u128>,
        data_layout: &TargetDataLayout,
    ) -> Option<Self> {
        Self::try_from_uint(i, data_layout.pointer_size())
    }

    /// Try to convert this ScalarInt to the raw underlying bits.
    /// Fails if the size is wrong. Generally a wrong size should lead to a panic,
    /// but Miri sometimes wants to be resilient to size mismatches,
    /// so the interpreter will generally use this `try` method.
    #[inline]
    pub fn try_to_bits(self, target_size: Size) -> Result<u128, Size> {
        assert_ne!(target_size.bytes(), 0, "you should never look at the bits of a ZST");
        if target_size.bytes() == u64::from(self.size.get()) {
            self.check_data();
            Ok(self.data)
        } else {
            Err(self.size())
        }
    }

    #[inline]
    pub fn to_bits(self, target_size: Size) -> u128 {
        self.try_to_bits(target_size).unwrap_or_else(|size| {
            panic!("expected int of size {}, but got size {}", target_size.bytes(), size.bytes())
        })
    }

    /// Extracts the bits from the scalar without checking the size.
    #[inline]
    pub fn to_bits_unchecked(self) -> u128 {
        self.check_data();
        self.data
    }

    /// Converts the `ScalarInt` to an unsigned integer of the given size.
    /// Panics if the size of the `ScalarInt` is not equal to `size`.
    #[inline]
    pub fn to_uint(self, size: Size) -> u128 {
        self.to_bits(size)
    }

    #[inline]
    pub fn to_uint_unchecked(self) -> u128 {
        self.data
    }

    /// Converts the `ScalarInt` to `u8`.
    /// Panics if the `size` of the `ScalarInt`in not equal to 1 byte.
    #[inline]
    pub fn to_u8(self) -> u8 {
        self.to_uint(Size::from_bits(8)).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to `u16`.
    /// Panics if the size of the `ScalarInt` in not equal to 2 bytes.
    #[inline]
    pub fn to_u16(self) -> u16 {
        self.to_uint(Size::from_bits(16)).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to `u32`.
    /// Panics if the `size` of the `ScalarInt` in not equal to 4 bytes.
    #[inline]
    pub fn to_u32(self) -> u32 {
        self.to_uint(Size::from_bits(32)).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to `u64`.
    /// Panics if the `size` of the `ScalarInt` in not equal to 8 bytes.
    #[inline]
    pub fn to_u64(self) -> u64 {
        self.to_uint(Size::from_bits(64)).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to `u128`.
    /// Panics if the `size` of the `ScalarInt` in not equal to 16 bytes.
    #[inline]
    pub fn to_u128(self) -> u128 {
        self.to_uint(Size::from_bits(128))
    }

    #[inline]
    pub fn to_target_usize(&self, data_layout: &TargetDataLayout) -> u64 {
        self.to_uint(data_layout.pointer_size()).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to `bool`.
    /// Panics if the `size` of the `ScalarInt` is not equal to 1 byte.
    /// Errors if it is not a valid `bool`.
    #[inline]
    pub fn try_to_bool(self) -> Result<bool, ()> {
        match self.to_u8() {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(()),
        }
    }

    /// Converts the `ScalarInt` to a signed integer of the given size.
    /// Panics if the size of the `ScalarInt` is not equal to `size`.
    #[inline]
    pub fn to_int(self, size: Size) -> i128 {
        let b = self.to_bits(size);
        size.sign_extend(b)
    }

    #[inline]
    pub fn to_int_unchecked(self) -> i128 {
        self.size().sign_extend(self.data)
    }

    /// Converts the `ScalarInt` to i8.
    /// Panics if the size of the `ScalarInt` is not equal to 1 byte.
    pub fn to_i8(self) -> i8 {
        self.to_int(Size::from_bits(8)).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to i16.
    /// Panics if the size of the `ScalarInt` is not equal to 2 bytes.
    pub fn to_i16(self) -> i16 {
        self.to_int(Size::from_bits(16)).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to i32.
    /// Panics if the size of the `ScalarInt` is not equal to 4 bytes.
    pub fn to_i32(self) -> i32 {
        self.to_int(Size::from_bits(32)).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to i64.
    /// Panics if the size of the `ScalarInt` is not equal to 8 bytes.
    pub fn to_i64(self) -> i64 {
        self.to_int(Size::from_bits(64)).try_into().unwrap()
    }

    /// Converts the `ScalarInt` to i128.
    /// Panics if the size of the `ScalarInt` is not equal to 16 bytes.
    pub fn to_i128(self) -> i128 {
        self.to_int(Size::from_bits(128))
    }

    #[inline]
    pub fn to_target_isize(&self, data_layout: &TargetDataLayout) -> i64 {
        self.to_int(data_layout.pointer_size()).try_into().unwrap()
    }
}

macro_rules! from_x_for_scalar_int {
    ($($ty:ty),*) => {
        $(
            impl From<$ty> for ScalarInt {
                #[inline]
                fn from(u: $ty) -> Self {
                    Self {
                        data: u128::from(u),
                        size: NonZero::new(size_of::<$ty>() as u8).unwrap(),
                    }
                }
            }
        )*
    }
}

macro_rules! from_scalar_int_for_x {
    ($($ty:ty),*) => {
        $(
            impl From<ScalarInt> for $ty {
                #[inline]
                fn from(int: ScalarInt) -> Self {
                    // The `unwrap` cannot fail because to_uint (if it succeeds)
                    // is guaranteed to return a value that fits into the size.
                    int.to_uint(Size::from_bytes(size_of::<$ty>()))
                       .try_into().unwrap()
                }
            }
        )*
    }
}

from_x_for_scalar_int!(u8, u16, u32, u64, u128, bool);
from_scalar_int_for_x!(u8, u16, u32, u64, u128);

impl TryFrom<ScalarInt> for bool {
    type Error = ();
    #[inline]
    fn try_from(int: ScalarInt) -> Result<Self, ()> {
        int.try_to_bool()
    }
}

impl From<char> for ScalarInt {
    #[inline]
    fn from(c: char) -> Self {
        (c as u32).into()
    }
}

macro_rules! from_x_for_scalar_int_signed {
    ($($ty:ty),*) => {
        $(
            impl From<$ty> for ScalarInt {
                #[inline]
                fn from(u: $ty) -> Self {
                    Self {
                        data: u128::from(u.cast_unsigned()), // go via the unsigned type of the same size
                        size: NonZero::new(size_of::<$ty>() as u8).unwrap(),
                    }
                }
            }
        )*
    }
}

macro_rules! from_scalar_int_for_x_signed {
    ($($ty:ty),*) => {
        $(
            impl From<ScalarInt> for $ty {
                #[inline]
                fn from(int: ScalarInt) -> Self {
                    // The `unwrap` cannot fail because to_int (if it succeeds)
                    // is guaranteed to return a value that fits into the size.
                    int.to_int(Size::from_bytes(size_of::<$ty>()))
                       .try_into().unwrap()
                }
            }
        )*
    }
}

from_x_for_scalar_int_signed!(i8, i16, i32, i64, i128);
from_scalar_int_for_x_signed!(i8, i16, i32, i64, i128);

impl From<std::cmp::Ordering> for ScalarInt {
    #[inline]
    fn from(c: std::cmp::Ordering) -> Self {
        // Here we rely on `cmp::Ordering` having the same values in host and target!
        ScalarInt::from(c as i8)
    }
}

/// Error returned when a conversion from ScalarInt to char fails.
#[derive(Debug)]
pub struct CharTryFromScalarInt;

impl TryFrom<ScalarInt> for char {
    type Error = CharTryFromScalarInt;

    #[inline]
    fn try_from(int: ScalarInt) -> Result<Self, Self::Error> {
        match char::from_u32(int.to_u32()) {
            Some(c) => Ok(c),
            None => Err(CharTryFromScalarInt),
        }
    }
}

impl fmt::Debug for ScalarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Dispatch to LowerHex below.
        write!(f, "0x{self:x}")
    }
}

impl fmt::LowerHex for ScalarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.check_data();
        if f.alternate() {
            // Like regular ints, alternate flag adds leading `0x`.
            write!(f, "0x")?;
        }
        // Format as hex number wide enough to fit any value of the given `size`.
        // So data=20, size=1 will be "0x14", but with size=4 it'll be "0x00000014".
        // Using a block `{self.data}` here to force a copy instead of using `self.data`
        // directly, because `write!` takes references to its formatting arguments and
        // would thus borrow `self.data`. Since `Self`
        // is a packed struct, that would create a possibly unaligned reference, which
        // is UB.
        write!(f, "{:01$x}", { self.data }, self.size.get() as usize * 2)
    }
}

impl fmt::UpperHex for ScalarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.check_data();
        // Format as hex number wide enough to fit any value of the given `size`.
        // So data=20, size=1 will be "0x14", but with size=4 it'll be "0x00000014".
        // Using a block `{self.data}` here to force a copy instead of using `self.data`
        // directly, because `write!` takes references to its formatting arguments and
        // would thus borrow `self.data`. Since `Self`
        // is a packed struct, that would create a possibly unaligned reference, which
        // is UB.
        write!(f, "{:01$X}", { self.data }, self.size.get() as usize * 2)
    }
}

impl fmt::Display for ScalarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.check_data();
        write!(f, "{}", { self.data })
    }
}

#[cfg(test)]
mod tests {
    use base_db::Crate;
    use hir_def::{CallableDefId, ModuleDefId, nameres::crate_def_map};
    use rustc_type_ir::inherent::Ty as _;
    use hir_expand::fixture;

    use super::*;
    use crate::{
        ComplexMemoryMap,
        next_solver::{GenericArgs, ParamEnv},
    };

    const FIXTURE: &str = r#"
//- minicore: sized, copy
fn f() {}
struct B(u8);
union U { a: u8, b: u16 }
"#;

    fn item(db: &Db, krate: Crate, name: &str) -> ModuleDefId {
        let def_map = crate_def_map(db, krate);
        let scope = def_map.module(db, def_map.root_module_id()).scope(db);
        scope
            .types()
            .map(|(it, item)| (it, item.def))
            .chain(scope.values().map(|(it, item)| (it, item.def)))
            .find(|(it, _)| it.as_str() == name)
            .map(|(_, def)| def)
            .unwrap_or_else(|| panic!("the fixture defines `{name}`"))
    }

    struct Fixture {
        db: Db,
        krate: Crate,
    }

    impl Fixture {
        fn new() -> Fixture {
            let db = fixture::db(FIXTURE).into_db();
            let krate = fixture::test_crate(&db);
            Fixture { db, krate }
        }

        fn interner(&self) -> DbInterner<'_> {
            DbInterner::new_with(&self.db, self.krate)
        }

        fn convert<'db>(
            &'db self,
            ty: Ty<'db>,
            memory: &[u8],
            map: &MemoryMap<'db>,
        ) -> Result<Const<'db>, ValTreeCreationError<'db>> {
            let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: self.krate };
            allocation_to_const(self.interner(), ty, memory, map, env)
        }

        /// A target `usize` of each of `words`, one after another.
        fn words(&self, words: &[usize]) -> Vec<u8> {
            let size = self.db.target_data_layout(self.krate).unwrap().pointer_size();
            let size = size.bytes_usize();
            words.iter().flat_map(|it| it.to_le_bytes()[..size].to_vec()).collect()
        }

        fn bytes_branch<'db>(&'db self, ty: Ty<'db>, bytes: &[u8]) -> Const<'db> {
            let interner = self.interner();
            let u8_values = &interner.default_types().consts.u8_values;
            let elements =
                Consts::new_from_iter(bytes.iter().map(|&it| u8_values[usize::from(it)]));
            Const::new_value(interner, ValTree::new(ValTreeKind::Branch(elements)), ty)
        }
    }

    fn piece<'db>(at: usize, bytes: &[u8]) -> MemoryMap<'db> {
        let mut map = ComplexMemoryMap::default();
        map.insert(at, Box::from(bytes));
        MemoryMap::Complex(Box::new(map))
    }

    #[test]
    fn a_fn_item_is_a_zero_sized_branch() {
        let fixture = Fixture::new();
        let interner = fixture.interner();
        let ModuleDefId::FunctionId(f) = item(&fixture.db, fixture.krate, "f") else {
            panic!("`f` is a function")
        };
        let ty =
            Ty::new_fn_def(interner, CallableDefId::FunctionId(f).into(), GenericArgs::empty());
        let konst = fixture.convert(ty, &[], &MemoryMap::Empty);
        assert_eq!(konst, Ok(Const::new_value(interner, ValTree::zst(), ty)));
    }

    #[test]
    fn a_raw_pointer_is_a_leaf_only_without_provenance() {
        let fixture = Fixture::new();
        let interner = fixture.interner();
        let ty = Ty::new_imm_ptr(interner, interner.default_types().types.u8);
        let unsupported = Err(ValTreeCreationError::NonSupportedType(ty));
        let pointer = fixture.db.target_data_layout(fixture.krate).unwrap().pointer_size();
        let leaf = |word: u128| {
            let leaf = ValTree::new(ValTreeKind::Leaf(ScalarInt::raw(word, pointer)));
            Ok(Const::new_value(interner, leaf, ty))
        };
        // An integer made a pointer, with or without memory at that address.
        assert_eq!(fixture.convert(ty, &fixture.words(&[0x10]), &MemoryMap::Empty), leaf(0x10));
        let map = piece(0x1000, &[7]);
        assert_eq!(fixture.convert(ty, &fixture.words(&[0x1000]), &map), leaf(0x1000));
        // The same word when the evaluation wrote a pointer there.
        let with_provenance = |at| {
            let mut map = ComplexMemoryMap::default();
            map.insert(0x1000, Box::from(&[7u8][..]));
            map.note_pointer(at);
            MemoryMap::Complex(Box::new(map))
        };
        let map = with_provenance(PointerAt::Value(0));
        assert_eq!(fixture.convert(ty, &fixture.words(&[0x1000]), &map), unsupported);
        // Inside a tuple, the word's own offset is what is asked.
        let pair = Ty::new_tup(interner, &[ty, ty]);
        let words = fixture.words(&[0x10, 0x1000]);
        let map = with_provenance(PointerAt::Value(words.len() / 2));
        assert_eq!(fixture.convert(pair, &words, &map), unsupported);
        let map = with_provenance(PointerAt::Value(words.len()));
        assert!(fixture.convert(pair, &words, &map).is_ok());
        // Behind a reference, the word's address is.
        let reference = Ty::new_imm_ref(interner, interner.default_types().regions.statik, ty);
        let mut map = ComplexMemoryMap::default();
        map.insert(0x2000, Box::from(&fixture.words(&[0x1000])[..]));
        map.note_pointer(PointerAt::Memory(0x2000));
        let map = MemoryMap::Complex(Box::new(map));
        assert_eq!(fixture.convert(reference, &fixture.words(&[0x2000]), &map), unsupported);
        // A wide one, even without provenance.
        let ty = Ty::new_imm_ptr(interner, interner.default_types().types.u8_slice);
        let map = piece(0x1000, &[7, 8]);
        assert_eq!(
            fixture.convert(ty, &fixture.words(&[0x1000, 2]), &map),
            Err(ValTreeCreationError::NonSupportedType(ty)),
        );
    }

    #[test]
    fn a_str_reference_is_a_branch_of_its_bytes() {
        let fixture = Fixture::new();
        let ty = fixture.interner().default_types().types.static_str_ref;
        let map = MemoryMap::simple(Box::from(&b"hi"[..]));
        let konst = fixture.convert(ty, &fixture.words(&[0, 2]), &map);
        assert_eq!(konst, Ok(fixture.bytes_branch(ty, b"hi")));
    }

    #[test]
    fn a_byte_slice_reference_is_a_branch_of_its_elements() {
        let fixture = Fixture::new();
        let ty = fixture.interner().default_types().types.static_u8_slice;
        let map = piece(0x1000, &[1, 2, 3, 4]);
        let konst = fixture.convert(ty, &fixture.words(&[0x1000, 3]), &map);
        assert_eq!(konst, Ok(fixture.bytes_branch(ty, &[1, 2, 3])));
        let empty = fixture.convert(ty, &fixture.words(&[0x1000, 0]), &map);
        assert_eq!(empty, Ok(fixture.bytes_branch(ty, &[])));
        // A length past the memory the map holds.
        let past = fixture.convert(ty, &fixture.words(&[0x1000, 5]), &map);
        assert!(matches!(past, Err(ValTreeCreationError::Unreadable { .. })), "{past:?}");
    }

    #[test]
    fn a_union_has_no_value_tree() {
        let fixture = Fixture::new();
        let interner = fixture.interner();
        let ModuleDefId::AdtId(u) = item(&fixture.db, fixture.krate, "U") else {
            panic!("`U` is a union")
        };
        let ty = Ty::new_adt(interner, u, GenericArgs::empty());
        assert_eq!(
            fixture.convert(ty, &[1], &MemoryMap::Empty),
            Err(ValTreeCreationError::NonSupportedType(ty)),
        );
        // Also behind a reference.
        let reference = Ty::new_imm_ref(interner, interner.default_types().regions.statik, ty);
        let map = piece(0x1000, &[1, 0]);
        assert_eq!(
            fixture.convert(reference, &fixture.words(&[0x1000]), &map),
            Err(ValTreeCreationError::NonSupportedType(ty)),
        );
    }

    #[test]
    fn a_tree_of_more_than_the_node_budget_overflows() {
        let fixture = Fixture::new();
        let ty = fixture.interner().default_types().types.static_u8_slice;
        let bytes = vec![0; VALTREE_MAX_NODES + 1];
        let map = piece(0x1000, &bytes);
        let at_budget = fixture.convert(ty, &fixture.words(&[0x1000, VALTREE_MAX_NODES]), &map);
        assert_eq!(at_budget, Ok(fixture.bytes_branch(ty, &bytes[..VALTREE_MAX_NODES])));
        let over = fixture.convert(ty, &fixture.words(&[0x1000, VALTREE_MAX_NODES + 1]), &map);
        assert_eq!(over, Err(ValTreeCreationError::NodesOverflow));
    }

    #[test]
    fn a_struct_is_a_branch_of_its_fields() {
        let fixture = Fixture::new();
        let interner = fixture.interner();
        let ModuleDefId::AdtId(b) = item(&fixture.db, fixture.krate, "B") else {
            panic!("`B` is a struct")
        };
        let ty = Ty::new_adt(interner, b, GenericArgs::empty());
        let field = interner.default_types().consts.u8_values[9];
        let expected = ValTree::new(ValTreeKind::Branch(Consts::new_from_slice(&[field])));
        let konst = fixture.convert(ty, &[9], &MemoryMap::Empty);
        assert_eq!(konst, Ok(Const::new_value(interner, expected, ty)));
    }
}
