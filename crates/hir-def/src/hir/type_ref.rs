//! HIR for references to types. Paths in these are not yet resolved. They can
//! be directly created from an ast::TypeRef, without further queries.
//!
//! A store keeps its type refs as columns ([`TypeColumns`], `docs/v2/plan/design/data.md`): a
//! `u8` kind and a `u32` data word per type ref, the words of the variants that need two in
//! `extra`, lists as `(start, len)` ranges into `lists` and `bounds`, and the cold values
//! (paths, references, arrays, fn pointers, type params) in side tables by index. [`TypeRef`]
//! is a view decoded from them on the fly.
//!
//! | type ref | data | `extra` |
//! |---|---|---|
//! | `Never`, `Placeholder`, `Error` | 0 | |
//! | `Tuple(types)` | e | `[start, len]` into `lists` |
//! | `Path(path)` | path id in `paths` | |
//! | `RawPtr(inner, mutability)` | `inner << 1 \| mutability` | |
//! | `Reference(ref_type)` | `refs` t | |
//! | `Array(array)` | `arrays` t | |
//! | `Slice(inner)` | inner | |
//! | `Fn(fn_type)` | `fns` t | |
//! | `ImplTrait(bounds)`, `DynTrait(bounds)` | e | `[start, len]` into `bounds` |
//! | `TypeParam(param)` | `type_params` t | |
//! | `PatternType(ty, pat)` | e | `[ty, pat]` |
//! | `UnsafeBinder { lifetimes, ty }` | e | `[ty, start, len]`, the lifetimes' run of names |
//!
//! `e` is the index in `extra` of the type ref's words, `t` an index into the side table named.
//! `paths` is the store's [`PathColumns`] (`expr_store/path.rs`), which also hold the names and
//! `use<..>` arguments the bounds' runs ([`Names`], [`UseArgs`]) point into.

use hir_expand::name::Name;
use la_arena::Idx;
use rustc_abi::ExternAbi;

use crate::{
    LifetimeParamId, TypeParamId,
    expr_store::{
        ExpressionStore,
        path::{Names, Path, PathColumns, PathIdx, UseArgs},
    },
    hir::{
        ExprId, PatId,
        col::{SlabLayout, SlabWriter},
        columns::{Checked, ColumnVec, Frozen, Growing, below, entry, idx, len, list, raw, take_exact},
    },
};

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Mutability {
    Shared,
    Mut,
}

impl Mutability {
    pub fn from_mutable(mutable: bool) -> Mutability {
        if mutable { Mutability::Mut } else { Mutability::Shared }
    }

    pub fn as_keyword_for_ref(self) -> &'static str {
        match self {
            Mutability::Shared => "",
            Mutability::Mut => "mut ",
        }
    }

    pub fn as_keyword_for_ptr(self) -> &'static str {
        match self {
            Mutability::Shared => "const ",
            Mutability::Mut => "mut ",
        }
    }

    /// Returns `true` if the mutability is [`Mut`].
    ///
    /// [`Mut`]: Mutability::Mut
    #[must_use]
    pub fn is_mut(&self) -> bool {
        matches!(self, Self::Mut)
    }

    /// Returns `true` if the mutability is [`Shared`].
    ///
    /// [`Shared`]: Mutability::Shared
    #[must_use]
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Shared)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Rawness {
    RawPtr,
    Ref,
}

impl Rawness {
    pub fn from_raw(is_raw: bool) -> Rawness {
        if is_raw { Rawness::RawPtr } else { Rawness::Ref }
    }

    pub fn is_raw(&self) -> bool {
        matches!(self, Self::RawPtr)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
/// A `TypeRefId` that is guaranteed to always be `TypeRef::Path`. We use this for things like
/// impl's trait, that are always paths but need to be traced back to source code.
pub struct PathId(TypeRefId);

impl PathId {
    #[inline]
    pub fn from_type_ref_unchecked(type_ref: TypeRefId) -> Self {
        Self(type_ref)
    }

    #[inline]
    pub fn type_ref(self) -> TypeRefId {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TraitRef {
    pub path: PathId,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)] // clone: `TypeColumns::push`, once per lowered fn type
pub struct FnType {
    pub binder: Option<Box<[Name]>>,
    pub params: Box<[(Option<Name>, TypeRefId)]>,
    pub is_varargs: bool,
    pub is_unsafe: bool,
    pub abi: ExternAbi,
}

impl FnType {
    #[inline]
    pub fn split_params_and_ret(&self) -> (&[(Option<Name>, TypeRefId)], TypeRefId) {
        let (ret, params) = self.params.split_last().expect("should have at least return type");
        (params, ret.1)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ArrayType {
    pub ty: TypeRefId,
    pub len: ConstRef,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RefType {
    pub ty: TypeRefId,
    pub lifetime: Option<LifetimeRefId>,
    pub mutability: Mutability,
}

/// A type ref of a store, decoded from the store's columns ([`TypeColumns`]): ids and small
/// enums by value, lists and cold values borrowed from the store. Compare ty::Ty.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TypeRef<'a> {
    Never,
    Placeholder,
    Tuple(&'a [TypeRefId]),
    Path(Path<'a>),
    RawPtr(TypeRefId, Mutability),
    Reference(&'a RefType),
    Array(&'a ArrayType),
    Slice(TypeRefId),
    /// A fn pointer. Last element of the vector is the return type.
    Fn(&'a FnType),
    ImplTrait(&'a [TypeBound]),
    DynTrait(&'a [TypeBound]),
    TypeParam(TypeParamId),
    PatternType(TypeRefId, PatId),
    Error,
    /// `unsafe<'a, ..> ty`: the binder's lifetimes are a run of names in the store, read with
    /// [`ExpressionStore::names`].
    UnsafeBinder {
        lifetimes: Names,
        ty: TypeRefId,
    },
}

pub type TypeRefId = Idx<TypeRef<'static>>;

pub type LifetimeRefId = Idx<LifetimeRef>;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum LifetimeRef {
    Named(Name),
    Static,
    Placeholder,
    Param(LifetimeParamId),
    Error,
}

/// A bound, a plain record: its `for<..>` binder and `use<..>` arguments are runs in its store,
/// read with [`ExpressionStore::names`] and [`ExpressionStore::use_args`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TypeBound {
    Path(PathId, TraitBoundModifier),
    ForLifetime(Names, PathId),
    Lifetime(LifetimeRefId),
    Use(UseArgs),
    Error,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum UseArgRef {
    Name(Name),
    Lifetime(LifetimeRefId),
}

/// A modifier on a bound: `?Sized` is `Maybe`; `const Tr` is `Const` and `[const] Tr` (or
/// `~const Tr`) is `MaybeConst`, which are trait bounds like `None` that also carry the effect.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TraitBoundModifier {
    None,
    Maybe,
    Const,
    MaybeConst,
}

impl TypeRef<'static> {
    pub(crate) const UNIT: TypeRef<'static> = TypeRef::Tuple(&[]);
}

impl TypeBound {
    pub fn as_path<'a>(&self, map: &'a ExpressionStore) -> Option<(Path<'a>, TraitBoundModifier)> {
        match *self {
            TypeBound::Path(p, m) => Some((map.path(p), m)),
            TypeBound::ForLifetime(_, p) => Some((map.path(p), TraitBoundModifier::None)),
            TypeBound::Lifetime(_) | TypeBound::Error | TypeBound::Use(_) => None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ConstRef {
    pub expr: ExprId,
}

/// Which variant of [`TypeRef`] a type ref is, in variant order.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TypeRefKind {
    Never,
    Placeholder,
    Tuple,
    Path,
    RawPtr,
    Reference,
    Array,
    Slice,
    Fn,
    ImplTrait,
    DynTrait,
    TypeParam,
    PatternType,
    Error,
    UnsafeBinder,
}

impl TypeRefKind {
    /// Every kind, at the index of its value.
    pub(crate) const ALL: [TypeRefKind; 15] = {
        use TypeRefKind::*;
        [
            Never,
            Placeholder,
            Tuple,
            Path,
            RawPtr,
            Reference,
            Array,
            Slice,
            Fn,
            ImplTrait,
            DynTrait,
            TypeParam,
            PatternType,
            Error,
            UnsafeBinder,
        ]
    };

    pub(crate) fn of(type_ref: &TypeRef<'_>) -> TypeRefKind {
        match type_ref {
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
}

/// Every column of a store's type refs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct TypeColumns<V: ColumnVec = Frozen> {
    /// [`TypeRefKind`], by type ref.
    pub(crate) kind: V::Of<u8>,
    pub(crate) data: V::Of<u32>,
    pub(crate) extra: V::Of<u32>,
    /// The elements of every `Tuple`.
    pub(crate) lists: V::Of<TypeRefId>,
    /// The bounds of every `ImplTrait` and `DynTrait`.
    pub(crate) bounds: V::Of<TypeBound>,
    /// The store's paths, of type refs, expressions and patterns alike.
    pub(crate) paths: PathColumns,
    pub(crate) refs: V::Of<RefType>,
    pub(crate) arrays: V::Of<ArrayType>,
    pub(crate) fns: V::Of<FnType>,
    pub(crate) type_params: V::Of<TypeParamId>,
}

/// The bytes of the columns of one type ref (`kind` and `data`): 5, one cache line holding 12
/// type refs' kinds and data words; `extra`, the lists and the side tables are counted apart
/// (two words of `extra` for `Tuple`, `ImplTrait`, `DynTrait` and `PatternType`, three for
/// `UnsafeBinder`).
pub(crate) const TYPE_REF_BYTES: usize = size_of::<u8>() + size_of::<u32>();

const _: () = {
    assert!(TYPE_REF_BYTES <= 12);
    assert!(size_of::<TypeRefId>() == size_of::<u32>());
    let mut i = 0;
    while i < TypeRefKind::ALL.len() {
        assert!(TypeRefKind::ALL[i] as usize == i);
        i += 1;
    }
};

// The side tables' elements, on 64-bit. `RefType` would be 12 and `ArrayType` stays 8 once
// `la_arena::Idx` has a niche (`Option<LifetimeRefId>` is 8 bytes today). A path is a word
// here; its own columns are sized in `expr_store/path.rs`.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<RefType>() == 16);
    assert!(size_of::<ArrayType>() == 8);
    assert!(size_of::<TypeBound>() == 16);
};

const TYPE_ID: &str = "a type id past its arena";
const LIFETIME_ID: &str = "a lifetime id past its arena";
const NOT_A_PATH: &str = "a path id that is not a path";
const EXTRA: &str = "a type ref's words past the extra column";
const LIST: &str = "a tuple's types past their array";
const BOUNDS: &str = "a bound list past its array";

impl<V: ColumnVec> TypeColumns<V> {
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.kind.len()
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.kind.is_empty()
    }

    /// The type ref `id`. Columns are checked when they are read ([`TypeColumns::check`]) and
    /// made by [`TypeColumns::push`] alone, so one that doesn't decode is a bug in the store.
    #[inline]
    pub(crate) fn get(&self, id: TypeRefId) -> TypeRef<'_> {
        match self.decode(id) {
            Ok(type_ref) => type_ref,
            Err(what) => bad_types(what, raw(id)),
        }
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (TypeRefId, TypeRef<'_>)> + '_ {
        (0..len(self.len())).map(move |at| (idx(at), self.get(idx(at))))
    }

    /// The path a [`PathId`] names: one load of its kind and one of its data word, then the
    /// path's own.
    #[inline]
    pub(crate) fn path(&self, id: PathId) -> Path<'_> {
        match self.path_of(id) {
            Ok(path) => self.paths.get(path),
            Err(what) => bad_types(what, raw(id.type_ref())),
        }
    }

    #[inline]
    fn path_of(&self, id: PathId) -> Checked<PathIdx> {
        let at = raw(id.type_ref()) as usize;
        match (self.kind.get(at), self.data.get(at)) {
            (Some(&kind), Some(&d)) if kind == TypeRefKind::Path as u8 => self.path_idx(d),
            (Some(_), Some(_)) => Err(NOT_A_PATH),
            _ => Err(TYPE_ID),
        }
    }

    #[inline]
    fn path_idx(&self, d: u32) -> Checked<PathIdx> {
        below(d, self.paths.len(), PATH).map(|()| idx(d))
    }

    #[inline]
    fn pair(&self, at: u32) -> Checked<[u32; 2]> {
        let at = at as usize;
        match self.extra.get(at..at.wrapping_add(2)) {
            Some(&[a, b]) => Ok([a, b]),
            _ => Err(EXTRA),
        }
    }

    #[inline]
    fn triple(&self, at: u32) -> Checked<[u32; 3]> {
        let at = at as usize;
        match self.extra.get(at..at.wrapping_add(3)) {
            Some(&[a, b, c]) => Ok([a, b, c]),
            _ => Err(EXTRA),
        }
    }

    #[inline]
    fn decode(&self, id: TypeRefId) -> Checked<TypeRef<'_>> {
        let at = raw(id) as usize;
        let (Some(&kind), Some(&d)) = (self.kind.get(at), self.data.get(at)) else {
            return Err(TYPE_ID);
        };
        let kind = *TypeRefKind::ALL.get(kind as usize).ok_or("a type ref kind out of range")?;
        Ok(match kind {
            TypeRefKind::Never => TypeRef::Never,
            TypeRefKind::Placeholder => TypeRef::Placeholder,
            TypeRefKind::Error => TypeRef::Error,
            TypeRefKind::Tuple => {
                let [start, count] = self.pair(d)?;
                TypeRef::Tuple(list(&self.lists, start, count, LIST)?)
            }
            TypeRefKind::Path => TypeRef::Path(self.paths.decode(self.path_idx(d)?)?),
            TypeRefKind::RawPtr => TypeRef::RawPtr(
                idx(d >> 1),
                if d & 1 == 1 { Mutability::Mut } else { Mutability::Shared },
            ),
            TypeRefKind::Reference => TypeRef::Reference(entry(&self.refs, d, REF)?),
            TypeRefKind::Array => TypeRef::Array(entry(&self.arrays, d, ARRAY)?),
            TypeRefKind::Slice => TypeRef::Slice(idx(d)),
            TypeRefKind::Fn => TypeRef::Fn(entry(&self.fns, d, FN)?),
            TypeRefKind::ImplTrait => {
                let [start, count] = self.pair(d)?;
                TypeRef::ImplTrait(list(&self.bounds, start, count, BOUNDS)?)
            }
            TypeRefKind::DynTrait => {
                let [start, count] = self.pair(d)?;
                TypeRef::DynTrait(list(&self.bounds, start, count, BOUNDS)?)
            }
            TypeRefKind::TypeParam => {
                TypeRef::TypeParam(*entry(&self.type_params, d, TYPE_PARAM)?)
            }
            TypeRefKind::PatternType => {
                let [ty, pat] = self.pair(d)?;
                TypeRef::PatternType(idx(ty), idx(pat))
            }
            TypeRefKind::UnsafeBinder => {
                let [ty, start, len] = self.triple(d)?;
                TypeRef::UnsafeBinder { lifetimes: Names { start, len }, ty: idx(ty) }
            }
        })
    }
}

const PATH: &str = "a path index past its table";
const REF: &str = "a reference index past its table";
const ARRAY: &str = "an array index past its table";
const FN: &str = "a fn pointer index past its table";
const TYPE_PARAM: &str = "a type param index past its table";

#[cold]
#[inline(never)]
#[track_caller]
fn bad_types(what: &str, id: u32) -> ! {
    panic!("type ref column bug at type ref {id}: {what}")
}

/// Appends `items`, as the `(start, len)` of the range they take.
fn append<T: Clone>(array: &mut Vec<T>, items: &[T]) -> [u32; 2] {
    let start = len(array.len());
    array.extend_from_slice(items);
    [start, len(items.len())]
}

/// Appends `value`, as its index.
fn push_one<T>(table: &mut Vec<T>, value: T) -> u32 {
    let at = len(table.len());
    table.push(value);
    at
}

impl TypeColumns<Growing> {
    pub(crate) fn push(&mut self, type_ref: TypeRef<'_>) -> TypeRefId {
        let kind = TypeRefKind::of(&type_ref);
        let data = match type_ref {
            TypeRef::Never | TypeRef::Placeholder | TypeRef::Error => 0,
            TypeRef::Tuple(types) => {
                let words = append(&mut self.lists, types);
                self.words(words)
            }
            TypeRef::Path(path) => raw(self.paths.push_view(path)),
            TypeRef::RawPtr(inner, mutability) => {
                assert!(raw(inner) >> (u32::BITS - 1) == 0, "a type id too large to share its word");
                raw(inner) << 1 | mutability as u32
            }
            TypeRef::Reference(ref_type) => push_one(&mut self.refs, *ref_type),
            TypeRef::Array(array) => push_one(&mut self.arrays, *array),
            TypeRef::Slice(inner) => raw(inner),
            // clone: a view's fn type copied into this table; lowering pushes each one once
            TypeRef::Fn(fn_type) => push_one(&mut self.fns, fn_type.clone()),
            TypeRef::ImplTrait(bounds) | TypeRef::DynTrait(bounds) => {
                let words = append(&mut self.bounds, bounds);
                self.words(words)
            }
            TypeRef::TypeParam(param) => push_one(&mut self.type_params, param),
            TypeRef::PatternType(ty, pat) => self.words([raw(ty), raw(pat)]),
            TypeRef::UnsafeBinder { lifetimes, ty } => {
                self.words([raw(ty), lifetimes.start, lifetimes.len])
            }
        };
        self.node(kind, data)
    }

    /// A `Path` type ref of the path `path` of these columns, where [`TypeColumns::push`]
    /// copies a view's rows.
    pub(crate) fn push_path(&mut self, path: PathIdx) -> TypeRefId {
        debug_assert!((raw(path) as usize) < self.paths.len(), "a path of other columns");
        self.node(TypeRefKind::Path, raw(path))
    }

    /// A `Fn` type ref, moving `fn_type` into its table.
    pub(crate) fn push_fn(&mut self, fn_type: FnType) -> TypeRefId {
        let data = push_one(&mut self.fns, fn_type);
        self.node(TypeRefKind::Fn, data)
    }

    fn node(&mut self, kind: TypeRefKind, data: u32) -> TypeRefId {
        let id = idx(len(self.kind.len()));
        self.kind.push(kind as u8);
        self.data.push(data);
        id
    }

    fn words<const N: usize>(&mut self, words: [u32; N]) -> u32 {
        let at = len(self.extra.len());
        self.extra.extend_from_slice(&words);
        at
    }

    pub(crate) fn clear(&mut self) {
        let TypeColumns { kind, data, extra, lists, bounds, paths, refs, arrays, fns, type_params } =
            self;
        kind.clear();
        data.clear();
        extra.clear();
        lists.clear();
        bounds.clear();
        paths.clear();
        refs.clear();
        arrays.clear();
        fns.clear();
        type_params.clear();
    }

    /// The elements across every column, for the scratch pool's cap.
    pub(crate) fn capacity(&self) -> usize {
        self.kind.capacity()
            + self.extra.capacity()
            + self.lists.capacity()
            + self.bounds.capacity()
            + self.paths.capacity()
    }

    /// The store's paths, for lowering to push paths, generic arguments and bound runs into.
    #[inline]
    pub(crate) fn paths_mut(&mut self) -> &mut PathColumns {
        &mut self.paths
    }

    /// The columns of a finished store, each copied out at its exact length: the `Copy` ones
    /// into one slab together ([`SlabWriter`]), widest first so no padding is between them, the
    /// fn types into a box of their own ([`take_exact`]) and the paths as
    /// [`PathColumns::freeze_into`] makes them. `self` is left empty with its capacity, for the
    /// next store built on this thread.
    pub(crate) fn freeze_into(&mut self) -> TypeColumns<Frozen> {
        let mut layout = SlabLayout::default();
        layout.add::<TypeBound>(self.bounds.len());
        layout.add::<RefType>(self.refs.len());
        layout.add::<ArrayType>(self.arrays.len());
        layout.add::<TypeParamId>(self.type_params.len());
        layout.add::<u32>(self.data.len());
        layout.add::<u32>(self.extra.len());
        layout.add::<TypeRefId>(self.lists.len());
        layout.add::<u8>(self.kind.len());
        let mut slab = SlabWriter::new(layout);
        let bounds = slab.take(&mut self.bounds);
        let refs = slab.take(&mut self.refs);
        let arrays = slab.take(&mut self.arrays);
        let type_params = slab.take(&mut self.type_params);
        let data = slab.take(&mut self.data);
        let extra = slab.take(&mut self.extra);
        let lists = slab.take(&mut self.lists);
        let kind = slab.take(&mut self.kind);
        TypeColumns {
            kind,
            data,
            extra,
            lists,
            bounds,
            paths: self.paths.freeze_into(),
            refs,
            arrays,
            fns: take_exact(&mut self.fns).into(),
            type_params,
        }
    }
}

impl TypeColumns<Frozen> {
    /// Every kind in range, every id, list range and side-table index in bounds, every
    /// [`PathId`] of a bound naming a `Path`, and the paths' columns ([`PathColumns::check`]:
    /// their rows, runs and the ids inside them). The encoding calls it after reading a buffer
    /// and maps `Err(what)` to `FormatError::Invalid`.
    pub(crate) fn check(&self, exprs: usize, pats: usize, lifetimes: usize) -> Checked {
        if self.kind.len() != self.data.len() {
            return Err("type ref kind and data columns of different lengths");
        }
        if u32::try_from(self.len()).is_err() {
            return Err("more type refs than ids");
        }
        let count = self.len();
        let ty = |id: TypeRefId| below(raw(id), count, TYPE_ID);
        let path = |id: PathId| self.path_of(id).map(drop);
        self.paths.check(count, lifetimes, exprs, path)?;
        for at in 0..len(count) {
            match self.decode(idx(at))? {
                TypeRef::Never
                | TypeRef::Placeholder
                | TypeRef::Error
                | TypeRef::Path(_)
                | TypeRef::TypeParam(_) => {}
                TypeRef::Tuple(types) => types.iter().try_for_each(|&it| ty(it))?,
                TypeRef::RawPtr(inner, _) | TypeRef::Slice(inner) => ty(inner)?,
                TypeRef::Reference(RefType { ty: inner, lifetime: region, mutability: _ }) => {
                    ty(*inner)?;
                    region.map_or(Ok(()), |id| below(raw(id), lifetimes, LIFETIME_ID))?;
                }
                TypeRef::Array(ArrayType { ty: inner, len: ConstRef { expr } }) => {
                    ty(*inner)?;
                    below(raw(*expr), exprs, "an expression id past its arena")?;
                }
                TypeRef::Fn(FnType { params, binder: _, is_varargs: _, is_unsafe: _, abi: _ }) => {
                    if params.is_empty() {
                        return Err("a fn pointer without its return type");
                    }
                    params.iter().try_for_each(|&(_, param)| ty(param))?;
                }
                TypeRef::ImplTrait(bounds) | TypeRef::DynTrait(bounds) => {
                    for bound in bounds {
                        self.paths.check_bound(bound, lifetimes, &path)?;
                    }
                }
                TypeRef::PatternType(inner, pat) => {
                    ty(inner)?;
                    below(raw(pat), pats, "a pattern id past its arena")?;
                }
                TypeRef::UnsafeBinder { lifetimes, ty: inner } => {
                    ty(inner)?;
                    self.paths.check_names(lifetimes)?;
                }
            }
        }
        Ok(())
    }

    /// A bound held outside the columns (a where predicate's, a type alias's), checked as
    /// [`TypeColumns::check`] checks the columns' own.
    pub(crate) fn check_outside_bound(&self, bound: &TypeBound, lifetimes: usize) -> Checked {
        self.paths.check_bound(bound, lifetimes, &|id: PathId| self.path_of(id).map(drop))
    }
}

#[cfg(test)]
mod tests {
    use hir_expand::mod_path::{ModPath, PathKind};
    use intern::Symbol;
    use la_arena::RawIdx;
    use blaze_query::db::FromId;

    use super::*;
    use crate::{FunctionId, GenericDefId, TypeOrConstParamId};

    const T0: TypeRefId = Idx::from_raw(RawIdx::from_u32(0));
    const T1: TypeRefId = Idx::from_raw(RawIdx::from_u32(1));

    fn t(at: u32) -> TypeRefId {
        idx(at)
    }

    fn name(text: &str) -> Name {
        Name::new_symbol_root(Symbol::intern(text))
    }

    fn mod_path(text: &str) -> ModPath {
        ModPath::from_segments(PathKind::Plain, [name(text)])
    }

    fn type_param(local: u32) -> TypeParamId {
        TypeParamId::from_unchecked(TypeOrConstParamId {
            parent: GenericDefId::FunctionId(FunctionId::from_id(blaze_query::db::Id::from_u32(
                5 + (1 << 20),
            ))),
            local_id: Idx::from_raw(RawIdx::from_u32(local)),
        })
    }

    struct Values {
        mod_paths: [ModPath; 2],
        refs: [RefType; 2],
        array: ArrayType,
        fn_type: FnType,
        bounds: Vec<TypeBound>,
    }

    fn values() -> Values {
        Values {
            mod_paths: [mod_path("a"), mod_path("b")],
            refs: [
                RefType { ty: t(0), lifetime: Some(idx(0)), mutability: Mutability::Mut },
                RefType { ty: t(1), lifetime: None, mutability: Mutability::Shared },
            ],
            array: ArrayType { ty: t(0), len: ConstRef { expr: idx(0) } },
            fn_type: FnType {
                binder: Some(Box::new([name("'a")])),
                params: Box::new([(Some(name("x")), t(0)), (None, t(1))]),
                is_varargs: true,
                is_unsafe: false,
                abi: ExternAbi::Rust,
            },
            bounds: vec![
                TypeBound::Path(PathId::from_type_ref_unchecked(t(0)), TraitBoundModifier::Maybe),
                TypeBound::ForLifetime(Names { start: 0, len: 1 }, PathId(t(0))),
                TypeBound::Lifetime(idx(0)),
                TypeBound::Use(UseArgs { start: 0, len: 2 }),
                TypeBound::Error,
            ],
        }
    }

    /// Every variant, with every list empty and not and both mutabilities; type ref 0 is a
    /// path, so every [`PathId`] in the bounds names one.
    fn every_type_ref(v: &Values) -> Vec<TypeRef<'_>> {
        vec![
            TypeRef::Path(Path::BarePath(&v.mod_paths[0])),
            TypeRef::Never,
            TypeRef::Placeholder,
            TypeRef::Tuple(&[T0, T1, T0]),
            TypeRef::UNIT,
            TypeRef::Path(Path::BarePath(&v.mod_paths[1])),
            TypeRef::RawPtr(t(1), Mutability::Mut),
            TypeRef::RawPtr(t(0), Mutability::Shared),
            TypeRef::Reference(&v.refs[0]),
            TypeRef::Reference(&v.refs[1]),
            TypeRef::Array(&v.array),
            TypeRef::Slice(t(2)),
            TypeRef::Fn(&v.fn_type),
            TypeRef::ImplTrait(&v.bounds),
            TypeRef::ImplTrait(&[]),
            TypeRef::DynTrait(&v.bounds[..1]),
            TypeRef::TypeParam(type_param(0)),
            TypeRef::TypeParam(type_param(3)),
            TypeRef::PatternType(t(0), idx(0)),
            TypeRef::Error,
            TypeRef::UnsafeBinder { lifetimes: Names { start: 0, len: 1 }, ty: t(8) },
            TypeRef::UnsafeBinder { lifetimes: Names::EMPTY, ty: t(0) },
        ]
    }

    /// `type_refs` pushed into `columns`, after the runs the bounds of [`values`] point into.
    fn fill(columns: &mut TypeColumns<Growing>, type_refs: &[TypeRef<'_>]) {
        assert_eq!(columns.paths.push_names([name("'b")]), Names { start: 0, len: 1 });
        let use_args = [UseArgRef::Name(name("T")), UseArgRef::Name(name("U"))];
        assert_eq!(columns.paths.push_use_args(use_args), UseArgs { start: 0, len: 2 });
        for (at, &type_ref) in type_refs.iter().enumerate() {
            assert_eq!(columns.push(type_ref), t(at as u32));
        }
    }

    fn columns_of(type_refs: &[TypeRef<'_>]) -> TypeColumns<Growing> {
        let mut columns = TypeColumns::<Growing>::default();
        fill(&mut columns, type_refs);
        columns
    }

    #[test]
    fn every_variant_reads_back_as_pushed() {
        let v = values();
        let type_refs = every_type_ref(&v);
        let kinds: std::collections::HashSet<_> = type_refs.iter().map(TypeRefKind::of).collect();
        assert_eq!(kinds.len(), TypeRefKind::ALL.len(), "every kind is tested");

        let mut columns = columns_of(&type_refs);
        for (at, &type_ref) in type_refs.iter().enumerate() {
            assert_eq!(columns.get(t(at as u32)), type_ref);
        }
        assert_eq!(columns.iter().map(|(_, it)| it).collect::<Vec<_>>(), type_refs);
        assert_eq!(columns.path(PathId(t(5))), Path::BarePath(&v.mod_paths[1]));

        let frozen = columns.freeze_into();
        assert!(columns.is_empty() && columns.paths.is_empty(), "freezing empties the scratch");
        assert_eq!(frozen.check(1, 1, 1), Ok(()));
        for (at, &type_ref) in type_refs.iter().enumerate() {
            assert_eq!(frozen.get(t(at as u32)), type_ref);
        }

        fill(&mut columns, &type_refs);
        assert_eq!(columns.freeze_into(), frozen, "reused scratch makes the same columns");
    }

    #[test]
    fn by_value_pushes_match_borrowed_ones() {
        let v = values();
        let mut borrowed = TypeColumns::<Growing>::default();
        borrowed.push(TypeRef::Path(Path::BarePath(&v.mod_paths[0])));
        borrowed.push(TypeRef::Fn(&v.fn_type));
        let mut owned = TypeColumns::<Growing>::default();
        let path = owned.paths.push_bare(v.mod_paths[0]);
        owned.push_path(path);
        owned.push_fn(v.fn_type.clone());
        assert_eq!(owned, borrowed);
    }

    #[test]
    fn check_refuses_a_kind_out_of_range() {
        let mut columns = columns_of(&[TypeRef::Never]).freeze_into();
        columns.kind[0] = TypeRefKind::ALL.len() as u8;
        assert_eq!(columns.check(0, 0, 0), Err("a type ref kind out of range"));
    }

    #[test]
    fn check_refuses_an_id_past_its_arena() {
        let columns = columns_of(&[TypeRef::Slice(t(1))]).freeze_into();
        assert_eq!(columns.check(0, 0, 0), Err(TYPE_ID));
        let columns = columns_of(&[TypeRef::Tuple(&[t(0), t(1)])]).freeze_into();
        assert_eq!(columns.check(0, 0, 0), Err(TYPE_ID));
        let columns = columns_of(&[TypeRef::PatternType(t(0), idx(0))]).freeze_into();
        assert_eq!(columns.check(0, 0, 0), Err("a pattern id past its arena"));
        assert_eq!(columns.check(0, 1, 0), Ok(()));

        let v = values();
        let columns = columns_of(&[TypeRef::Array(&v.array)]).freeze_into();
        assert_eq!(columns.check(0, 0, 0), Err("an expression id past its arena"));
        let columns = columns_of(&[TypeRef::Reference(&v.refs[0])]).freeze_into();
        assert_eq!(columns.check(0, 0, 0), Err("a lifetime id past its arena"));
        assert_eq!(columns.check(0, 0, 1), Ok(()));

        let mut columns = columns_of(&[TypeRef::Never, TypeRef::DynTrait(&v.bounds[..1])]);
        assert_eq!(columns.freeze_into().check(0, 0, 0), Err(NOT_A_PATH));
    }

    #[test]
    fn check_refuses_a_list_past_its_array() {
        let mut columns = columns_of(&[TypeRef::Tuple(&[t(0)])]).freeze_into();
        columns.extra[1] = 2;
        assert_eq!(columns.check(0, 0, 0), Err(LIST));
        columns.extra[0] = u32::MAX;
        columns.extra[1] = 1;
        assert_eq!(columns.check(0, 0, 0), Err(LIST));

        let v = values();
        let mut columns =
            columns_of(&[TypeRef::Path(Path::BarePath(&v.mod_paths[0])), TypeRef::ImplTrait(&v.bounds)])
                .freeze_into();
        columns.extra[1] = v.bounds.len() as u32 + 1;
        assert_eq!(columns.check(0, 0, 1), Err(BOUNDS));

        let binder = [TypeBound::ForLifetime(Names { start: 0, len: 2 }, PathId(t(0)))];
        let columns =
            columns_of(&[TypeRef::Path(Path::BarePath(&v.mod_paths[0])), TypeRef::ImplTrait(&binder)])
                .freeze_into();
        assert_eq!(columns.check(0, 0, 0), Err("a name run past its table"));

        let binder = TypeRef::UnsafeBinder { lifetimes: Names { start: 0, len: 2 }, ty: t(0) };
        let columns = columns_of(&[TypeRef::Never, binder]).freeze_into();
        assert_eq!(columns.check(0, 0, 0), Err("a name run past its table"));
        let mut columns = columns_of(&[TypeRef::Never, binder]).freeze_into();
        columns.data[1] = 1;
        assert_eq!(columns.check(0, 0, 0), Err(EXTRA));

        let mut columns = columns_of(&[TypeRef::Path(Path::BarePath(&v.mod_paths[0]))]).freeze_into();
        columns.data[0] = 1;
        assert_eq!(columns.check(0, 0, 0), Err(PATH));

        let mut columns = columns_of(&[TypeRef::PatternType(t(0), idx(0))]).freeze_into();
        columns.data[0] = 1;
        assert_eq!(columns.check(0, 1, 0), Err(EXTRA));
    }
}
