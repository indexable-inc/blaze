//! Interned values as `u32` words: the codec, [`Code`], of what a value writes to a [`Sink`]
//! and reads back from a [`Source`]. It is prefix-free: a value's first word, its tag, says how
//! many words follow.
//!
//! - Persisted: a query result holds types as their place in a table of its own
//!   ([`TyTableWriter`], [`TyTableReader`]): each value written once, after the values it holds,
//!   as its words, each child as its place in the table, definitions as ids, an ADT as its
//!   id alone (its flags are read again from the database the table is read into).
//! - In memory a row keeps its kind whole (`interner::table::Rows`); the codec gives the row's
//!   tag when it is added ([`tag`]), and in debug builds checks that what it writes reads back
//!   as the same value, so every interned value is a round trip of the persisted codec.

use std::{convert::Infallible, marker::PhantomData, num::NonZeroU32};

use blaze_format::{
    ALIGN, FormatError, Part, Record, SchemaHash, Slice, Writer, record_at, schema_of,
};
use blaze_hash::HashMap;
use hir_def::{
    AdtId, ConstParamId, GenericDefId, LifetimeParamId, TypeOrConstParamId, TypeParamId,
};
use la_arena::{Idx, RawIdx};
use rustc_abi::{ExternAbi, Size};
use rustc_ast_ir::{FloatTy, IntTy, Mutability, UintTy};
use rustc_type_ir::{
    Alias, AliasConstKind, BoundConstness, BoundVar, BoundVarIndexKind, ClausePolarity, ConstVid,
    DebruijnIndex, FloatVid, InferConst, InferTy, IntVid, IsRigid, RegionVid, TyVid, TypeVisitable,
    UniverseIndex, inherent::IntoKind,
};
use smallvec::SmallVec;

use super::{
    AdtDef, AliasTermKind, AliasTyKind, AnonConstIdWrapper, Binder, BoundConst,
    BoundExistentialPredicate, BoundExistentialPredicates, BoundRegion, BoundRegionKind, BoundTy,
    BoundTyKind, BoundVarKinds, BoundVariableKind, CallableIdWrapper, Clause, ClauseKind, Clauses,
    ClosureIdWrapper, CoercePredicate, Const, ConstKind, Consts, CoroutineClosureIdWrapper,
    CoroutineIdWrapper, Ctor, DbInterner, EarlyParamRegion, ErrorGuaranteed, ExistentialPredicate,
    ExistentialProjection, ExistentialTraitRef, ExprConst, FnHeader, FnSigKind, FreeConstAliasId,
    FreeTermAliasId, FreeTyAliasId, GenericArg, GenericArgKind, GenericArgs, HostEffectClause,
    ImplOrTraitAssocConstId, ImplOrTraitAssocTermId, ImplOrTraitAssocTyId, InherentAssocConstId,
    InherentAssocTermId, InherentAssocTyId, InternedConst, InternedRegion, LateParamRegion,
    NormalizesTo, OpaqueTyIdWrapper, OutlivesClause, ParamConst, ParamTy, PatList, Pattern,
    PatternKind, PlaceholderConst, PlaceholderRegion, PlaceholderType, Predicate, PredicateKind,
    ProjectionClause, Region, RegionKind, ScalarInt, SolverDefId, SubtypePredicate, Term, TermKind,
    TraitAssocConstId, TraitAssocTermId, TraitAssocTyId, TraitClause, TraitIdWrapper, TraitRef, Ty,
    TyKind, TypeAliasIdWrapper, Tys, ValTree, ValTreeKind, ValueConst, abi::Safety,
    ensure_sufficient_stack, interner::Symbol,
};

/// Where a value's words go.
pub(crate) trait Sink<'db> {
    fn word(&mut self, word: u32);
    /// A child: its index in its table in memory, its place in the table persisted.
    fn node<N: Node<'db>>(&mut self, node: N);
    /// An ADT: in memory with its flags, persisted as its id alone.
    fn adt(&mut self, adt: AdtDef);
    /// An id as [`put_id`] writes it, unless the sink names ids otherwise.
    fn id<T: crate::check_cache::Relocate>(&mut self, id: T)
    where
        Self: Sized,
    {
        put_id(self, id.as_id());
    }
    /// A definition: its variant and id, unless the sink names definitions otherwise.
    fn def(&mut self, def: SolverDefId<'db>)
    where
        Self: Sized,
    {
        put_def(self, def);
    }
}

/// Where a value's words come from.
pub(crate) trait Source<'db> {
    type Error;
    fn word(&mut self) -> Result<u32, Self::Error>;
    fn node<N: Node<'db>>(&mut self) -> Result<N, Self::Error>;
    fn adt(&mut self) -> Result<AdtDef, Self::Error>;
    /// An id, as [`Sink::id`] wrote it.
    fn id<T: crate::check_cache::Relocate>(&mut self) -> Result<T, Self::Error>
    where
        Self: Sized,
    {
        get_id(self).map(T::from_id)
    }
    /// How many of something follow, each of at least one word.
    fn count(&mut self) -> Result<usize, Self::Error>;
    fn invalid(&self, what: &'static str) -> Self::Error;
    /// A definition, as [`Sink::def`] wrote it.
    fn def(&mut self) -> Result<SolverDefId<'db>, Self::Error>
    where
        Self: Sized,
    {
        get_def(self)
    }
}

/// A value as words.
pub(crate) trait Code<'db>: Sized {
    fn put(&self, sink: &mut impl Sink<'db>);
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error>;
}

/// The tables of interned values, as a persisted table names them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u32)]
pub(crate) enum Kind {
    Ty,
    Region,
    Const,
    Pattern,
    Predicate,
    ValTree,
    GenericArgs,
    Tys,
    Consts,
    BoundVarKinds,
    PatList,
    BoundExistentialPredicates,
    Clauses,
}

const KINDS: [Kind; 13] = [
    Kind::Ty,
    Kind::Region,
    Kind::Const,
    Kind::Pattern,
    Kind::Predicate,
    Kind::ValTree,
    Kind::GenericArgs,
    Kind::Tys,
    Kind::Consts,
    Kind::BoundVarKinds,
    Kind::PatList,
    Kind::BoundExistentialPredicates,
    Kind::Clauses,
];

/// An interned value: a row of a table of its own.
pub(crate) trait Node<'db>: Copy {
    const KIND: Kind;
    fn row(self) -> NonZeroU32;
    /// The value of a row of its table: one interning gave, or `published` accepted.
    fn at(row: NonZeroU32) -> Self;
    /// Its words in a persisted table.
    fn put_node(self, sink: &mut impl Sink<'db>);
    /// The value of the words [`Node::put_node`] wrote, interned.
    fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error>;
}

/// A value's words, its children as their rows: on the stack for any value but a long one.
type Words = SmallVec<[u32; 16]>;

impl<'db> Sink<'db> for Words {
    #[inline]
    fn word(&mut self, word: u32) {
        self.push(word);
    }

    #[inline]
    fn node<N: Node<'db>>(&mut self, node: N) {
        self.push(node.row().get());
    }

    fn adt(&mut self, adt: AdtDef) {
        SolverDefId::from(adt.def_id()).put(self);
        self.push(adt.flag_bits().into());
    }
}

/// The tag of `value`, a row being added: its codec's first word. In debug builds, also checks
/// that the codec reads back what it writes for `value`.
pub(crate) fn tag<'db, T: Code<'db> + PartialEq>(value: &T) -> u8 {
    let mut words = Words::new();
    value.put(&mut words);
    if cfg!(debug_assertions) {
        let read = match T::get(&mut Row { words: &words }) {
            Ok(read) => read,
            Err(never) => match never {},
        };
        assert!(read == *value, "the codec reads back a value other than it wrote");
    }
    let tag = words[0];
    u8::try_from(tag).unwrap_or_else(|_| panic!("a tag of {tag}"))
}

/// The words of a value, read back: what its `put` gave, so none is invalid.
struct Row<'r> {
    words: &'r [u32],
}

impl<'db> Source<'db> for Row<'_> {
    type Error = Infallible;

    #[inline]
    fn word(&mut self) -> Result<u32, Infallible> {
        let (&word, rest) = self.words.split_first().expect("a row as long as its value");
        self.words = rest;
        Ok(word)
    }

    #[inline]
    fn node<N: Node<'db>>(&mut self) -> Result<N, Infallible> {
        let row = self.word()?;
        Ok(N::at(NonZeroU32::new(row).expect("a row's child is a row")))
    }

    fn adt(&mut self) -> Result<AdtDef, Infallible> {
        let def = SolverDefId::get(self)?;
        let bits = self.word()?;
        let adt = match def {
            SolverDefId::AdtId(id) => u8::try_from(bits).ok().and_then(|bits| AdtDef::from_parts(id, bits)),
            _ => None,
        };
        Ok(adt.expect("a row's ADT is one `AdtDef::new` made"))
    }

    #[inline]
    fn count(&mut self) -> Result<usize, Infallible> {
        Ok(self.word()? as usize)
    }

    fn invalid(&self, what: &'static str) -> Infallible {
        panic!("a row that is not {what}")
    }
}

/// A word below `end`.
fn below<'db, S: Source<'db>>(
    source: &mut S,
    end: u32,
    what: &'static str,
) -> Result<u32, S::Error> {
    let word = source.word()?;
    if word < end { Ok(word) } else { Err(source.invalid(what)) }
}

impl<'db> Code<'db> for u32 {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.word(*self);
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        source.word()
    }
}

impl<'db> Code<'db> for bool {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.word(u32::from(*self));
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(below(source, 2, "a bool")? == 1)
    }
}

/// Types of one value, which take no words.
macro_rules! unit_code {
    ($($ty:ident),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, _: &mut impl Sink<'db>) {}
            #[inline]
            fn get<S: Source<'db>>(_: &mut S) -> Result<Self, S::Error> {
                Ok($ty)
            }
        }
    )*};
}

unit_code!(ErrorGuaranteed, ExprConst);

/// A name: its length in bytes, then its UTF-8 bytes four to a word, little-endian, the last
/// word zero-padded. The text, not a declared symbol's index, which another build may number
/// otherwise.
impl<'db> Code<'db> for Symbol {
    fn put(&self, sink: &mut impl Sink<'db>) {
        let bytes = self.0.as_str().as_bytes();
        sink.word(u32::try_from(bytes.len()).expect("a name shorter than 4 GiB"));
        let mut chunks = bytes.chunks_exact(4);
        for chunk in &mut chunks {
            sink.word(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 4];
            last[..rest.len()].copy_from_slice(rest);
            sink.word(u32::from_le_bytes(last));
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let len = source.word()? as usize;
        // Grows only with words actually read, so a length past the value's words costs nothing.
        let mut bytes = SmallVec::<[u8; 32]>::new();
        for _ in 0..len.div_ceil(4) {
            bytes.extend_from_slice(&source.word()?.to_le_bytes());
        }
        if bytes[len..].iter().any(|&pad| pad != 0) {
            return Err(source.invalid("a name padded with zeros"));
        }
        match std::str::from_utf8(&bytes[..len]) {
            Ok(text) => Ok(Symbol(intern::Symbol::intern(text))),
            Err(_) => Err(source.invalid("a name in UTF-8")),
        }
    }
}

/// `rustc_index` newtypes: one word, below their maximum.
macro_rules! index_code {
    ($($ty:ty),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                sink.word(self.as_u32());
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                let word = below(source, <$ty>::MAX_AS_U32 + 1, stringify!($ty))?;
                Ok(<$ty>::from_u32(word))
            }
        }
    )*};
}

index_code!(DebruijnIndex, BoundVar, UniverseIndex, TyVid, IntVid, FloatVid, RegionVid, ConstVid);

/// Enums without fields: their variant's number.
macro_rules! enum_code {
    ($ty:ty { $($index:literal => $variant:path),* $(,)? }) => {
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                sink.word(match *self { $($variant => $index,)* });
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                match source.word()? {
                    $($index => Ok($variant),)*
                    _ => Err(source.invalid(stringify!($ty))),
                }
            }
        }
    };
}

enum_code!(Mutability { 0 => Mutability::Not, 1 => Mutability::Mut });
enum_code!(IntTy {
    0 => IntTy::Isize,
    1 => IntTy::I8,
    2 => IntTy::I16,
    3 => IntTy::I32,
    4 => IntTy::I64,
    5 => IntTy::I128,
});
enum_code!(UintTy {
    0 => UintTy::Usize,
    1 => UintTy::U8,
    2 => UintTy::U16,
    3 => UintTy::U32,
    4 => UintTy::U64,
    5 => UintTy::U128,
});
enum_code!(FloatTy { 0 => FloatTy::F16, 1 => FloatTy::F32, 2 => FloatTy::F64, 3 => FloatTy::F128 });
enum_code!(ClausePolarity { 0 => ClausePolarity::Positive, 1 => ClausePolarity::Negative });
enum_code!(BoundConstness { 0 => BoundConstness::Const, 1 => BoundConstness::Maybe });
enum_code!(IsRigid { 0 => IsRigid::Yes, 1 => IsRigid::No });

/// Structs whose fields are all public: their fields in order.
macro_rules! struct_code {
    ($($ty:ty { $($field:ident),* $(,)? }),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                $( self.$field.put(sink); )*
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                Ok(Self { $( $field: Code::get(source)?, )* })
            }
        }
    )*};
}

struct_code! {
    ParamTy { id, index },
    ParamConst { id, index },
    EarlyParamRegion { id, index },
    LifetimeParamId { parent, local_id },
    LateParamRegion<'db> { scope, kind },
    BoundRegion<'db> { var, kind },
    BoundTy<'db> { var, kind },
    ValueConst<'db> { ty, value },
    TraitClause<'db> { trait_ref, polarity },
    ProjectionClause<'db> { projection_term, term },
    HostEffectClause<'db> { trait_ref, constness },
    SubtypePredicate<'db> { a_is_expected, a, b },
    CoercePredicate<'db> { a, b },
    NormalizesTo<'db> { alias, term },
    rustc_type_ir::FnSigTys<DbInterner<'db>> { inputs_and_output },
}

/// An id: its bits (never 0) and a generation word, always 0, which the format keeps.
fn put_id<'db>(sink: &mut impl Sink<'db>, id: blaze_query::db::Id) {
    sink.word(id.as_u32());
    sink.word(0);
}

fn get_id<'db, S: Source<'db>>(source: &mut S) -> Result<blaze_query::db::Id, S::Error> {
    let index = source.word()?;
    let generation = source.word()?;
    if index == 0 {
        return Err(source.invalid("an id of index 0"));
    }
    if generation != 0 {
        return Err(source.invalid("an id of a generation"));
    }
    Ok(blaze_query::db::Id::from_u32(index))
}

impl<'db> Code<'db> for SolverDefId<'db> {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.def(*self);
    }

    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        source.def()
    }
}

/// A definition as its variant and id: [`Sink::def`]'s default.
pub(crate) fn put_def<'db>(sink: &mut impl Sink<'db>, def: SolverDefId<'db>) {
    match def {
        SolverDefId::AdtId(AdtId::StructId(id)) => {
            sink.word(0);
            sink.id(id)
        }
        SolverDefId::AdtId(AdtId::UnionId(id)) => {
            sink.word(1);
            sink.id(id)
        }
        SolverDefId::AdtId(AdtId::EnumId(id)) => {
            sink.word(2);
            sink.id(id)
        }
        SolverDefId::ConstId(id) => {
            sink.word(3);
            sink.id(id)
        }
        SolverDefId::FunctionId(id) => {
            sink.word(4);
            sink.id(id)
        }
        SolverDefId::ImplId(id) => {
            sink.word(5);
            sink.id(id)
        }
        SolverDefId::BuiltinDeriveImplId(id) => {
            sink.word(6);
            sink.id(id)
        }
        SolverDefId::StaticId(id) => {
            sink.word(7);
            sink.id(id)
        }
        SolverDefId::AnonConstId(id) => {
            sink.word(8);
            sink.id(id)
        }
        SolverDefId::TraitId(id) => {
            sink.word(9);
            sink.id(id)
        }
        SolverDefId::TypeAliasId(id) => {
            sink.word(10);
            sink.id(id)
        }
        SolverDefId::InternedClosureId(id) => {
            sink.word(11);
            sink.id(id)
        }
        SolverDefId::InternedCoroutineId(id) => {
            sink.word(12);
            sink.id(id)
        }
        SolverDefId::InternedCoroutineClosureId(id) => {
            sink.word(13);
            sink.id(id)
        }
        SolverDefId::InternedOpaqueTyId(id) => {
            sink.word(14);
            sink.id(id)
        }
        SolverDefId::EnumVariantId(id) => {
            sink.word(15);
            sink.id(id)
        }
        SolverDefId::Ctor(Ctor::Struct(id)) => {
            sink.word(16);
            sink.id(id)
        }
        SolverDefId::Ctor(Ctor::Enum(id)) => {
            sink.word(17);
            sink.id(id)
        }
    }
}

/// [`put_def`]'s definition back: [`Source::def`]'s default.
pub(crate) fn get_def<'db, S: Source<'db>>(source: &mut S) -> Result<SolverDefId<'db>, S::Error> {
    Ok(match source.word()? {
        0 => SolverDefId::AdtId(AdtId::StructId(source.id()?)),
        1 => SolverDefId::AdtId(AdtId::UnionId(source.id()?)),
        2 => SolverDefId::AdtId(AdtId::EnumId(source.id()?)),
        3 => SolverDefId::ConstId(source.id()?),
        4 => SolverDefId::FunctionId(source.id()?),
        5 => SolverDefId::ImplId(source.id()?),
        6 => SolverDefId::BuiltinDeriveImplId(source.id()?),
        7 => SolverDefId::StaticId(source.id()?),
        8 => SolverDefId::AnonConstId(source.id()?),
        9 => SolverDefId::TraitId(source.id()?),
        10 => SolverDefId::TypeAliasId(source.id()?),
        11 => SolverDefId::InternedClosureId(source.id()?),
        12 => SolverDefId::InternedCoroutineId(source.id()?),
        13 => SolverDefId::InternedCoroutineClosureId(source.id()?),
        14 => SolverDefId::InternedOpaqueTyId(source.id()?),
        15 => SolverDefId::EnumVariantId(source.id()?),
        16 => SolverDefId::Ctor(Ctor::Struct(source.id()?)),
        17 => SolverDefId::Ctor(Ctor::Enum(source.id()?)),
        _ => return Err(source.invalid("a definition")),
    })
}

/// Ids that convert both ways with [`SolverDefId`]: as the one they convert to.
macro_rules! def_id_code {
    ($($ty:ty),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                SolverDefId::from(*self).put(sink);
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                let def = SolverDefId::get(source)?;
                <$ty>::try_from(def).map_err(|_| source.invalid(stringify!($ty)))
            }
        }
    )*};
}

def_id_code!(
    GenericDefId,
    TraitIdWrapper,
    TypeAliasIdWrapper,
    CallableIdWrapper,
    ClosureIdWrapper<'db>,
    CoroutineIdWrapper<'db>,
    CoroutineClosureIdWrapper<'db>,
    OpaqueTyIdWrapper<'db>,
    AnonConstIdWrapper<'db>,
    TraitAssocTyId,
    TraitAssocConstId,
    TraitAssocTermId,
    ImplOrTraitAssocTyId,
    ImplOrTraitAssocConstId,
    ImplOrTraitAssocTermId,
    FreeTyAliasId,
    FreeConstAliasId,
    FreeTermAliasId,
    InherentAssocTyId,
    InherentAssocConstId,
    InherentAssocTermId,
);

impl<'db> Code<'db> for AdtDef {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.adt(*self);
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        source.adt()
    }
}

impl<'db, T> Code<'db> for Idx<T> {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.word(self.into_raw().into_u32());
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(Idx::from_raw(RawIdx::from_u32(source.word()?)))
    }
}

impl<'db> Code<'db> for TypeParamId {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.parent().put(sink);
        self.local_id().put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let parent = GenericDefId::get(source)?;
        let local_id = Idx::get(source)?;
        Ok(TypeParamId::from_unchecked(TypeOrConstParamId { parent, local_id }))
    }
}

impl<'db> Code<'db> for ConstParamId {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.parent().put(sink);
        self.local_id().put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let parent = GenericDefId::get(source)?;
        let local_id = Idx::get(source)?;
        Ok(ConstParamId::from_unchecked(TypeOrConstParamId { parent, local_id }))
    }
}

// The structs below are rebuilt with `from_checked_parts`: their parts were checked when the
// value was first made (and `DbInterner`'s `debug_assert_*_args_compatible` are empty).

impl<'db> Code<'db> for TraitRef<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.def_id.put(sink);
        self.args.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let def_id = Code::get(source)?;
        let args = Code::get(source)?;
        Ok(TraitRef::from_checked_parts(def_id, args))
    }
}

impl<'db> Code<'db> for ExistentialTraitRef<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.def_id.put(sink);
        self.args.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let def_id = Code::get(source)?;
        let args = Code::get(source)?;
        Ok(ExistentialTraitRef::from_checked_parts(def_id, args))
    }
}

impl<'db> Code<'db> for ExistentialProjection<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.def_id.put(sink);
        self.args.put(sink);
        self.term.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let def_id = Code::get(source)?;
        let args = Code::get(source)?;
        let term = Code::get(source)?;
        Ok(ExistentialProjection::from_checked_parts(def_id, args, term))
    }
}

impl<'db, K: Code<'db> + Copy> Code<'db> for Alias<DbInterner<'db>, K> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.args.put(sink);
        self.kind.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let args = Code::get(source)?;
        let kind = K::get(source)?;
        Ok(Alias::from_checked_parts(kind, args))
    }
}

impl<'db> Code<'db> for AliasTyKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            AliasTyKind::Projection { def_id } => {
                sink.word(0);
                def_id.put(sink);
            }
            AliasTyKind::Inherent { def_id } => {
                sink.word(1);
                def_id.put(sink);
            }
            AliasTyKind::Opaque { def_id } => {
                sink.word(2);
                def_id.put(sink);
            }
            AliasTyKind::Free { def_id } => {
                sink.word(3);
                def_id.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => AliasTyKind::Projection { def_id: Code::get(source)? },
            1 => AliasTyKind::Inherent { def_id: Code::get(source)? },
            2 => AliasTyKind::Opaque { def_id: Code::get(source)? },
            3 => AliasTyKind::Free { def_id: Code::get(source)? },
            _ => return Err(source.invalid("an alias type kind")),
        })
    }
}

impl<'db> Code<'db> for AliasTermKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            AliasTermKind::ProjectionTy { def_id } => {
                sink.word(0);
                def_id.put(sink);
            }
            AliasTermKind::InherentTy { def_id } => {
                sink.word(1);
                def_id.put(sink);
            }
            AliasTermKind::OpaqueTy { def_id } => {
                sink.word(2);
                def_id.put(sink);
            }
            AliasTermKind::FreeTy { def_id } => {
                sink.word(3);
                def_id.put(sink);
            }
            AliasTermKind::AnonConst { def_id } => {
                sink.word(4);
                def_id.put(sink);
            }
            AliasTermKind::ProjectionConst { def_id } => {
                sink.word(5);
                def_id.put(sink);
            }
            AliasTermKind::FreeConst { def_id } => {
                sink.word(6);
                def_id.put(sink);
            }
            AliasTermKind::InherentConstSelf { def_id } => {
                sink.word(7);
                def_id.put(sink);
            }
            AliasTermKind::InherentConstImpl { def_id } => {
                sink.word(8);
                def_id.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => AliasTermKind::ProjectionTy { def_id: Code::get(source)? },
            1 => AliasTermKind::InherentTy { def_id: Code::get(source)? },
            2 => AliasTermKind::OpaqueTy { def_id: Code::get(source)? },
            3 => AliasTermKind::FreeTy { def_id: Code::get(source)? },
            4 => AliasTermKind::AnonConst { def_id: Code::get(source)? },
            5 => AliasTermKind::ProjectionConst { def_id: Code::get(source)? },
            6 => AliasTermKind::FreeConst { def_id: Code::get(source)? },
            7 => AliasTermKind::InherentConstSelf { def_id: Code::get(source)? },
            8 => AliasTermKind::InherentConstImpl { def_id: Code::get(source)? },
            _ => return Err(source.invalid("an alias term kind")),
        })
    }
}

impl<'db> Code<'db> for AliasConstKind<DbInterner<'db>> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            AliasConstKind::Projection { def_id } => {
                sink.word(0);
                def_id.put(sink);
            }
            AliasConstKind::InherentSelf { def_id } => {
                sink.word(1);
                def_id.put(sink);
            }
            AliasConstKind::InherentImpl { def_id } => {
                sink.word(2);
                def_id.put(sink);
            }
            AliasConstKind::Free { def_id } => {
                sink.word(3);
                def_id.put(sink);
            }
            AliasConstKind::Anon { def_id } => {
                sink.word(4);
                def_id.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => AliasConstKind::Projection { def_id: Code::get(source)? },
            1 => AliasConstKind::InherentSelf { def_id: Code::get(source)? },
            2 => AliasConstKind::InherentImpl { def_id: Code::get(source)? },
            3 => AliasConstKind::Free { def_id: Code::get(source)? },
            4 => AliasConstKind::Anon { def_id: Code::get(source)? },
            _ => return Err(source.invalid("an alias constant kind")),
        })
    }
}

impl<'db> Code<'db> for BoundVarIndexKind {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            BoundVarIndexKind::Bound(debruijn) => {
                sink.word(0);
                debruijn.put(sink);
            }
            BoundVarIndexKind::Canonical => sink.word(1),
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => BoundVarIndexKind::Bound(Code::get(source)?),
            1 => BoundVarIndexKind::Canonical,
            _ => return Err(source.invalid("a bound variable index")),
        })
    }
}

impl<'db> Code<'db> for BoundTyKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            BoundTyKind::Anon => sink.word(0),
            BoundTyKind::Param(def) => {
                sink.word(1);
                def.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => BoundTyKind::Anon,
            1 => BoundTyKind::Param(Code::get(source)?),
            _ => return Err(source.invalid("a bound type kind")),
        })
    }
}

impl<'db> Code<'db> for BoundRegionKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            BoundRegionKind::Anon => sink.word(0),
            BoundRegionKind::NamedForPrinting(symbol) => {
                sink.word(1);
                symbol.put(sink);
            }
            BoundRegionKind::Named(def) => {
                sink.word(2);
                def.put(sink);
            }
            BoundRegionKind::ClosureEnv => sink.word(3),
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => BoundRegionKind::Anon,
            1 => BoundRegionKind::NamedForPrinting(Code::get(source)?),
            2 => BoundRegionKind::Named(Code::get(source)?),
            3 => BoundRegionKind::ClosureEnv,
            _ => return Err(source.invalid("a bound region kind")),
        })
    }
}

impl<'db> Code<'db> for BoundVariableKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            BoundVariableKind::Ty(kind) => {
                sink.word(0);
                kind.put(sink);
            }
            BoundVariableKind::Region(kind) => {
                sink.word(1);
                kind.put(sink);
            }
            BoundVariableKind::Const => sink.word(2),
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => BoundVariableKind::Ty(Code::get(source)?),
            1 => BoundVariableKind::Region(Code::get(source)?),
            2 => BoundVariableKind::Const,
            _ => return Err(source.invalid("a bound variable kind")),
        })
    }
}

impl<'db> Code<'db> for BoundConst<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.var.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(BoundConst::new(Code::get(source)?))
    }
}

/// Placeholders: their universe and what they stand for.
macro_rules! placeholder_code {
    ($($ty:ident($bound:ty)),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty<'db> {
            fn put(&self, sink: &mut impl Sink<'db>) {
                self.universe.put(sink);
                self.bound.put(sink);
            }
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                let universe = UniverseIndex::get(source)?;
                let bound = <$bound>::get(source)?;
                Ok($ty::new(universe, bound))
            }
        }
    )*};
}

placeholder_code!(
    PlaceholderType(BoundTy<'db>),
    PlaceholderRegion(BoundRegion<'db>),
    PlaceholderConst(BoundConst<'db>),
);

impl<'db> Code<'db> for InferTy {
    fn put(&self, sink: &mut impl Sink<'db>) {
        let (variant, word) = match *self {
            InferTy::TyVar(var) => (0, var.as_u32()),
            InferTy::IntVar(var) => (1, var.as_u32()),
            InferTy::FloatVar(var) => (2, var.as_u32()),
            InferTy::FreshTy(n) => (3, n),
            InferTy::FreshIntTy(n) => (4, n),
            InferTy::FreshFloatTy(n) => (5, n),
        };
        sink.word(variant);
        sink.word(word);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => InferTy::TyVar(Code::get(source)?),
            1 => InferTy::IntVar(Code::get(source)?),
            2 => InferTy::FloatVar(Code::get(source)?),
            3 => InferTy::FreshTy(source.word()?),
            4 => InferTy::FreshIntTy(source.word()?),
            5 => InferTy::FreshFloatTy(source.word()?),
            _ => return Err(source.invalid("an inference type")),
        })
    }
}

impl<'db> Code<'db> for InferConst {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            InferConst::Var(var) => {
                sink.word(0);
                var.put(sink);
            }
            InferConst::Fresh(n) => {
                sink.word(1);
                sink.word(n);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => InferConst::Var(Code::get(source)?),
            1 => InferConst::Fresh(source.word()?),
            _ => return Err(source.invalid("an inference constant")),
        })
    }
}

impl<'db, T: Code<'db> + TypeVisitable<DbInterner<'db>>> Code<'db> for Binder<'db, T> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.as_ref().skip_binder().put(sink);
        sink.node(self.bound_vars());
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let value = T::get(source)?;
        let bound_vars = source.node::<BoundVarKinds<'db>>()?;
        Ok(Binder::bind_with_vars(value, bound_vars))
    }
}

impl<'db> Code<'db> for rustc_type_ir::UnsafeBinderInner<DbInterner<'db>> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        (**self).put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(Binder::<'db, Ty<'db>>::get(source)?.into())
    }
}

/// A function pointer's signature and header. The header's splatted argument (`u8::MAX` for
/// none) is checked against the signature's inputs, so the header is read after them.
fn put_fn_ptr<'db>(
    sig_tys: &Binder<'db, rustc_type_ir::FnSigTys<DbInterner<'db>>>,
    header: FnHeader<'db>,
    sink: &mut impl Sink<'db>,
) {
    sig_tys.put(sink);
    let kind = header.fn_sig_kind;
    sink.word(kind.abi().as_packed().into());
    kind.is_safe().put(sink);
    kind.c_variadic().put(sink);
    sink.word(kind.splatted().unwrap_or(FnSigKind::NO_SPLATTED_ARG_INDEX).into());
}

fn get_fn_ptr<'db, S: Source<'db>>(source: &mut S) -> Result<TyKind<'db>, S::Error> {
    let sig_tys = Binder::<'db, rustc_type_ir::FnSigTys<DbInterner<'db>>>::get(source)?;
    let abi = below(source, ExternAbi::ALL_VARIANTS.len() as u32, "an ABI")?;
    let safety = if bool::get(source)? { Safety::Safe } else { Safety::Unsafe };
    let c_variadic = bool::get(source)?;
    let splatted = below(source, u32::from(u8::MAX) + 1, "a splatted argument")? as u8;
    let splatted = (splatted != FnSigKind::NO_SPLATTED_ARG_INDEX).then_some(splatted);
    let inputs = sig_tys.as_ref().skip_binder().inputs_and_output.len().saturating_sub(1);
    let abi = ExternAbi::from_packed(abi as u8);
    let fn_sig_kind = FnSigKind::new(abi, safety, c_variadic, splatted, inputs)
        .map_err(|_| source.invalid("a splatted argument among the inputs"))?;
    Ok(TyKind::FnPtr(sig_tys, FnHeader { fn_sig_kind }))
}

impl<'db> Code<'db> for ScalarInt {
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.word(self.size().bytes() as u32);
        let data = self.to_bits_unchecked();
        for i in 0..4 {
            sink.word((data >> (32 * i)) as u32);
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let size = source.word()?;
        if !(1..=16).contains(&size) {
            return Err(source.invalid("a scalar of 1 to 16 bytes"));
        }
        let mut data = 0u128;
        for i in 0..4 {
            data |= u128::from(source.word()?) << (32 * i);
        }
        ScalarInt::try_from_uint(data, Size::from_bytes(size))
            .ok_or_else(|| source.invalid("a scalar wider than its size"))
    }
}

impl<'db> Code<'db> for GenericArg<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match self.kind() {
            GenericArgKind::Type(ty) => {
                sink.word(0);
                sink.node(ty);
            }
            GenericArgKind::Lifetime(region) => {
                sink.word(1);
                sink.node(region);
            }
            GenericArgKind::Const(konst) => {
                sink.word(2);
                sink.node(konst);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => source.node::<Ty<'db>>()?.into(),
            1 => source.node::<Region<'db>>()?.into(),
            2 => source.node::<Const<'db>>()?.into(),
            _ => return Err(source.invalid("a generic argument")),
        })
    }
}

impl<'db> Code<'db> for Term<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match self.kind() {
            TermKind::Ty(ty) => {
                sink.word(0);
                sink.node(ty);
            }
            TermKind::Const(konst) => {
                sink.word(1);
                sink.node(konst);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => source.node::<Ty<'db>>()?.into(),
            1 => source.node::<Const<'db>>()?.into(),
            _ => return Err(source.invalid("a term")),
        })
    }
}

impl<'db> Code<'db> for Clause<'db> {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.node(self.0);
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(Clause(source.node()?))
    }
}

impl<'db, A: Code<'db>> Code<'db> for OutlivesClause<'db, A> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.0.put(sink);
        self.1.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let a = A::get(source)?;
        let region = Code::get(source)?;
        Ok(rustc_type_ir::OutlivesClause(a, region))
    }
}

impl<'db> Code<'db> for ExistentialPredicate<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            ExistentialPredicate::Trait(trait_ref) => {
                sink.word(0);
                trait_ref.put(sink);
            }
            ExistentialPredicate::Projection(projection) => {
                sink.word(1);
                projection.put(sink);
            }
            ExistentialPredicate::AutoTrait(def_id) => {
                sink.word(2);
                def_id.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => ExistentialPredicate::Trait(Code::get(source)?),
            1 => ExistentialPredicate::Projection(Code::get(source)?),
            2 => ExistentialPredicate::AutoTrait(Code::get(source)?),
            _ => return Err(source.invalid("an existential predicate")),
        })
    }
}

impl<'db> Code<'db> for TyKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            TyKind::Bool => sink.word(0),
            TyKind::Char => sink.word(1),
            TyKind::Int(ty) => {
                sink.word(2);
                ty.put(sink);
            }
            TyKind::Uint(ty) => {
                sink.word(3);
                ty.put(sink);
            }
            TyKind::Float(ty) => {
                sink.word(4);
                ty.put(sink);
            }
            TyKind::Adt(adt, args) => {
                sink.word(5);
                adt.put(sink);
                args.put(sink);
            }
            TyKind::Foreign(def_id) => {
                sink.word(6);
                def_id.put(sink);
            }
            TyKind::Str => sink.word(7),
            TyKind::Array(ty, len) => {
                sink.word(8);
                ty.put(sink);
                len.put(sink);
            }
            TyKind::Pat(ty, pat) => {
                sink.word(9);
                ty.put(sink);
                pat.put(sink);
            }
            TyKind::Slice(ty) => {
                sink.word(10);
                ty.put(sink);
            }
            TyKind::RawPtr(ty, mutability) => {
                sink.word(11);
                ty.put(sink);
                mutability.put(sink);
            }
            TyKind::Ref(region, ty, mutability) => {
                sink.word(12);
                region.put(sink);
                ty.put(sink);
                mutability.put(sink);
            }
            TyKind::FnDef(def_id, args) => {
                sink.word(13);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::FnPtr(sig_tys, header) => {
                sink.word(14);
                put_fn_ptr(&sig_tys, header, sink);
            }
            TyKind::UnsafeBinder(inner) => {
                sink.word(15);
                inner.put(sink);
            }
            TyKind::Dynamic(predicates, region) => {
                sink.word(16);
                predicates.put(sink);
                region.put(sink);
            }
            TyKind::Closure(def_id, args) => {
                sink.word(17);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::CoroutineClosure(def_id, args) => {
                sink.word(18);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::Coroutine(def_id, args) => {
                sink.word(19);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::CoroutineWitness(def_id, args) => {
                sink.word(20);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::Never => sink.word(21),
            TyKind::Tuple(tys) => {
                sink.word(22);
                tys.put(sink);
            }
            TyKind::Alias(is_rigid, alias) => {
                sink.word(23);
                is_rigid.put(sink);
                alias.put(sink);
            }
            TyKind::Param(param) => {
                sink.word(24);
                param.put(sink);
            }
            TyKind::Bound(index, bound) => {
                sink.word(25);
                index.put(sink);
                bound.put(sink);
            }
            TyKind::Placeholder(placeholder) => {
                sink.word(26);
                placeholder.put(sink);
            }
            TyKind::Infer(infer) => {
                sink.word(27);
                infer.put(sink);
            }
            TyKind::Error(error) => {
                sink.word(28);
                error.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => TyKind::Bool,
            1 => TyKind::Char,
            2 => TyKind::Int(Code::get(source)?),
            3 => TyKind::Uint(Code::get(source)?),
            4 => TyKind::Float(Code::get(source)?),
            5 => TyKind::Adt(Code::get(source)?, Code::get(source)?),
            6 => TyKind::Foreign(Code::get(source)?),
            7 => TyKind::Str,
            8 => TyKind::Array(Code::get(source)?, Code::get(source)?),
            9 => TyKind::Pat(Code::get(source)?, Code::get(source)?),
            10 => TyKind::Slice(Code::get(source)?),
            11 => TyKind::RawPtr(Code::get(source)?, Code::get(source)?),
            12 => TyKind::Ref(Code::get(source)?, Code::get(source)?, Code::get(source)?),
            13 => TyKind::FnDef(Code::get(source)?, Code::get(source)?),
            14 => get_fn_ptr(source)?,
            15 => TyKind::UnsafeBinder(Code::get(source)?),
            16 => TyKind::Dynamic(Code::get(source)?, Code::get(source)?),
            17 => TyKind::Closure(Code::get(source)?, Code::get(source)?),
            18 => TyKind::CoroutineClosure(Code::get(source)?, Code::get(source)?),
            19 => TyKind::Coroutine(Code::get(source)?, Code::get(source)?),
            20 => TyKind::CoroutineWitness(Code::get(source)?, Code::get(source)?),
            21 => TyKind::Never,
            22 => TyKind::Tuple(Code::get(source)?),
            23 => TyKind::Alias(Code::get(source)?, Code::get(source)?),
            24 => TyKind::Param(Code::get(source)?),
            25 => TyKind::Bound(Code::get(source)?, Code::get(source)?),
            26 => TyKind::Placeholder(Code::get(source)?),
            27 => TyKind::Infer(Code::get(source)?),
            28 => TyKind::Error(Code::get(source)?),
            _ => return Err(source.invalid("a type")),
        })
    }
}

impl<'db> Code<'db> for RegionKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            RegionKind::ReEarlyParam(param) => {
                sink.word(0);
                param.put(sink);
            }
            RegionKind::ReBound(index, bound) => {
                sink.word(1);
                index.put(sink);
                bound.put(sink);
            }
            RegionKind::ReLateParam(param) => {
                sink.word(2);
                param.put(sink);
            }
            RegionKind::ReStatic => sink.word(3),
            RegionKind::ReVar(var) => {
                sink.word(4);
                var.put(sink);
            }
            RegionKind::RePlaceholder(placeholder) => {
                sink.word(5);
                placeholder.put(sink);
            }
            RegionKind::ReErased => sink.word(6),
            RegionKind::ReError(error) => {
                sink.word(7);
                error.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => RegionKind::ReEarlyParam(Code::get(source)?),
            1 => RegionKind::ReBound(Code::get(source)?, Code::get(source)?),
            2 => RegionKind::ReLateParam(Code::get(source)?),
            3 => RegionKind::ReStatic,
            4 => RegionKind::ReVar(Code::get(source)?),
            5 => RegionKind::RePlaceholder(Code::get(source)?),
            6 => RegionKind::ReErased,
            7 => RegionKind::ReError(Code::get(source)?),
            _ => return Err(source.invalid("a region")),
        })
    }
}

impl<'db> Code<'db> for ConstKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            ConstKind::Param(param) => {
                sink.word(0);
                param.put(sink);
            }
            ConstKind::Infer(infer) => {
                sink.word(1);
                infer.put(sink);
            }
            ConstKind::Bound(index, bound) => {
                sink.word(2);
                index.put(sink);
                bound.put(sink);
            }
            ConstKind::Placeholder(placeholder) => {
                sink.word(3);
                placeholder.put(sink);
            }
            ConstKind::Alias(is_rigid, alias) => {
                sink.word(4);
                is_rigid.put(sink);
                alias.put(sink);
            }
            ConstKind::Value(value) => {
                sink.word(5);
                value.put(sink);
            }
            ConstKind::Error(error) => {
                sink.word(6);
                error.put(sink);
            }
            ConstKind::Expr(expr) => {
                sink.word(7);
                expr.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => ConstKind::Param(Code::get(source)?),
            1 => ConstKind::Infer(Code::get(source)?),
            2 => ConstKind::Bound(Code::get(source)?, Code::get(source)?),
            3 => ConstKind::Placeholder(Code::get(source)?),
            4 => ConstKind::Alias(Code::get(source)?, Code::get(source)?),
            5 => ConstKind::Value(Code::get(source)?),
            6 => ConstKind::Error(Code::get(source)?),
            7 => ConstKind::Expr(Code::get(source)?),
            _ => return Err(source.invalid("a constant")),
        })
    }
}

impl<'db> Code<'db> for PatternKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            PatternKind::Range { start, end } => {
                sink.word(0);
                start.put(sink);
                end.put(sink);
            }
            PatternKind::Or(pats) => {
                sink.word(1);
                pats.put(sink);
            }
            PatternKind::NotNull => sink.word(2),
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => PatternKind::Range { start: Code::get(source)?, end: Code::get(source)? },
            1 => PatternKind::Or(Code::get(source)?),
            2 => PatternKind::NotNull,
            _ => return Err(source.invalid("a pattern")),
        })
    }
}

impl<'db> Code<'db> for ClauseKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            ClauseKind::Trait(predicate) => {
                sink.word(0);
                predicate.put(sink);
            }
            ClauseKind::RegionOutlives(predicate) => {
                sink.word(1);
                predicate.put(sink);
            }
            ClauseKind::TypeOutlives(predicate) => {
                sink.word(2);
                predicate.put(sink);
            }
            ClauseKind::Projection(predicate) => {
                sink.word(3);
                predicate.put(sink);
            }
            ClauseKind::ConstArgHasType(konst, ty) => {
                sink.word(4);
                konst.put(sink);
                ty.put(sink);
            }
            ClauseKind::WellFormed(term) => {
                sink.word(5);
                term.put(sink);
            }
            ClauseKind::ConstEvaluatable(konst) => {
                sink.word(6);
                konst.put(sink);
            }
            ClauseKind::HostEffect(predicate) => {
                sink.word(7);
                predicate.put(sink);
            }
            ClauseKind::UnstableFeature(symbol) => {
                sink.word(8);
                symbol.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => ClauseKind::Trait(Code::get(source)?),
            1 => ClauseKind::RegionOutlives(Code::get(source)?),
            2 => ClauseKind::TypeOutlives(Code::get(source)?),
            3 => ClauseKind::Projection(Code::get(source)?),
            4 => ClauseKind::ConstArgHasType(Code::get(source)?, Code::get(source)?),
            5 => ClauseKind::WellFormed(Code::get(source)?),
            6 => ClauseKind::ConstEvaluatable(Code::get(source)?),
            7 => ClauseKind::HostEffect(Code::get(source)?),
            8 => ClauseKind::UnstableFeature(Code::get(source)?),
            _ => return Err(source.invalid("a clause")),
        })
    }
}

impl<'db> Code<'db> for PredicateKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            PredicateKind::Clause(clause) => {
                sink.word(0);
                clause.put(sink);
            }
            PredicateKind::DynCompatible(def_id) => {
                sink.word(1);
                def_id.put(sink);
            }
            PredicateKind::Subtype(predicate) => {
                sink.word(2);
                predicate.put(sink);
            }
            PredicateKind::Coerce(predicate) => {
                sink.word(3);
                predicate.put(sink);
            }
            PredicateKind::ConstEquate(a, b) => {
                sink.word(4);
                a.put(sink);
                b.put(sink);
            }
            PredicateKind::Ambiguous => sink.word(5),
            PredicateKind::NormalizesTo(predicate) => {
                sink.word(6);
                predicate.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => PredicateKind::Clause(Code::get(source)?),
            1 => PredicateKind::DynCompatible(Code::get(source)?),
            2 => PredicateKind::Subtype(Code::get(source)?),
            3 => PredicateKind::Coerce(Code::get(source)?),
            4 => PredicateKind::ConstEquate(Code::get(source)?, Code::get(source)?),
            5 => PredicateKind::Ambiguous,
            6 => PredicateKind::NormalizesTo(Code::get(source)?),
            _ => return Err(source.invalid("a predicate")),
        })
    }
}

/// Interned values as children: their row, or their place.
macro_rules! child_code {
    ($($ty:ident),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty<'db> {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                sink.node(*self);
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                source.node()
            }
        }
    )*};
}

child_code!(
    Ty,
    Region,
    Const,
    Pattern,
    Predicate,
    ValTree,
    GenericArgs,
    Tys,
    Consts,
    BoundVarKinds,
    PatList,
    BoundExistentialPredicates,
    Clauses,
);

/// The interned kinds with a structure: a persisted node is the words of their row.
macro_rules! row_node {
    ($($ty:ident: $kind:ty),* $(,)?) => {$(
        impl<'db> Node<'db> for $ty<'db> {
            const KIND: Kind = Kind::$ty;
            #[inline]
            fn row(self) -> NonZeroU32 {
                $ty::row(self)
            }
            #[inline]
            fn at(row: NonZeroU32) -> Self {
                $ty::at(row)
            }
            fn put_node(self, sink: &mut impl Sink<'db>) {
                self.kind().put(sink);
            }
            fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                Ok($ty::intern(<$kind>::get(source)?))
            }
        }
    )*};
}

row_node!(
    Ty: TyKind<'db>,
    Pattern: PatternKind<'db>,
    Predicate: Binder<'db, PredicateKind<'db>>,
);

/// Regions and constants are `rustc_type_ir`'s structs over a handle: a node is the handle's
/// row, and its words are the row's kind.
macro_rules! handle_node {
    ($($ty:ident($handle:ident): $kind:ty),* $(,)?) => {$(
        impl<'db> Node<'db> for $ty<'db> {
            const KIND: Kind = Kind::$ty;
            #[inline]
            fn row(self) -> NonZeroU32 {
                self.0.row()
            }
            #[inline]
            fn at(row: NonZeroU32) -> Self {
                rustc_type_ir::$ty($handle::at(row))
            }
            fn put_node(self, sink: &mut impl Sink<'db>) {
                self.kind().put(sink);
            }
            fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                Ok(rustc_type_ir::$ty($handle::intern(<$kind>::get(source)?)))
            }
        }
    )*};
}

handle_node!(Region(InternedRegion): RegionKind<'db>, Const(InternedConst): ConstKind<'db>);

impl<'db> Node<'db> for ValTree<'db> {
    const KIND: Kind = Kind::ValTree;
    #[inline]
    fn row(self) -> NonZeroU32 {
        ValTree::row(self)
    }
    #[inline]
    fn at(row: NonZeroU32) -> Self {
        ValTree::at(row)
    }
    fn put_node(self, sink: &mut impl Sink<'db>) {
        match *self.inner() {
            ValTreeKind::Leaf(scalar) => {
                sink.word(0);
                scalar.put(sink);
            }
            ValTreeKind::Branch(consts) => {
                sink.word(1);
                consts.put(sink);
            }
        }
    }
    fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let kind = match source.word()? {
            0 => ValTreeKind::Leaf(Code::get(source)?),
            1 => ValTreeKind::Branch(Code::get(source)?),
            _ => return Err(source.invalid("a value tree")),
        };
        Ok(ValTree::new(kind))
    }
}

/// The interned lists: a persisted node is their length and their items.
macro_rules! list_node {
    ($($ty:ident: $item:ty),* $(,)?) => {$(
        impl<'db> Node<'db> for $ty<'db> {
            const KIND: Kind = Kind::$ty;
            #[inline]
            fn row(self) -> NonZeroU32 {
                $ty::row(self)
            }
            #[inline]
            fn at(row: NonZeroU32) -> Self {
                $ty::at(row)
            }
            fn put_node(self, sink: &mut impl Sink<'db>) {
                sink.word(self.len() as u32);
                self.iter().for_each(|item| item.put(sink));
            }
            fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                let len = source.count()?;
                let items = (0..len)
                    .map(|_| <$item>::get(source))
                    .collect::<Result<SmallVec<[$item; 8]>, _>>()?;
                Ok($ty::new_from_slice(&items))
            }
        }
    )*};
}

list_node!(
    GenericArgs: GenericArg<'db>,
    Tys: Ty<'db>,
    Consts: Const<'db>,
    BoundVarKinds: BoundVariableKind<'db>,
    PatList: Pattern<'db>,
    Clauses: Clause<'db>,
);

impl<'db> Node<'db> for BoundExistentialPredicates<'db> {
    const KIND: Kind = Kind::BoundExistentialPredicates;
    #[inline]
    fn row(self) -> NonZeroU32 {
        BoundExistentialPredicates::row(self)
    }
    #[inline]
    fn at(row: NonZeroU32) -> Self {
        BoundExistentialPredicates::at(row)
    }
    fn put_node(self, sink: &mut impl Sink<'db>) {
        sink.word(self.len() as u32);
        self.iter().for_each(|item| item.put(sink));
    }
    fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let len = source.count()?;
        let mut items = (0..len)
            .map(|_| BoundExistentialPredicate::get(source))
            .collect::<Result<SmallVec<[BoundExistentialPredicate<'db>; 8]>, _>>()?;
        if crate::check_cache::in_scope() {
            // In lowering's order, by this database's ids: the principal, projections, auto traits.
            items.sort_by(|a, b| {
                use ExistentialPredicate as P;
                use std::cmp::Ordering::{Greater, Less};
                match (a.skip_binder(), b.skip_binder()) {
                    (P::Trait(_), P::Trait(_)) => std::cmp::Ordering::Equal,
                    (P::Trait(_), _) => Less,
                    (_, P::Trait(_)) => Greater,
                    (P::Projection(a), P::Projection(b)) => a.def_id.0.cmp(&b.def_id.0),
                    (P::Projection(_), _) => Less,
                    (_, P::Projection(_)) => Greater,
                    (P::AutoTrait(a), P::AutoTrait(b)) => a.0.cmp(&b.0),
                }
            });
        }
        Ok(BoundExistentialPredicates::new_from_slice(&items))
    }
}

blaze_format::record! {
    /// A value of a persisted table: which kind, and where its words start.
    #[derive(Clone, Copy, Debug)]
    struct NodeRec: 1 {
        kind: u32,
        at: u32,
    }
}

blaze_format::record! {
    /// A persisted table of types and the other interned values they hold, each after the
    /// values it holds. A value's words end where the next value's start.
    #[derive(Clone, Copy, Debug)]
    struct TableRec: 2 {
        nodes: Slice<NodeRec>,
        words: Slice<u32>,
    }
}

/// The version of the words a table holds: of the codec, which follows the variant order of the
/// kinds of the pinned `rustc_type_ir`. Changes with any change to a `Code` impl.
pub const FORMAT_VERSION: u64 = 2;

/// The schema of a persisted type table, for the schema of a buffer that holds one: a table
/// written by another codec reads as a schema mismatch, never as other values.
pub const TABLE_SCHEMA: SchemaHash = schema_of(&[
    Part::Str("hir_ty::next_solver::persist::TyTable"),
    Part::U64(FORMAT_VERSION),
    Part::Schema(<TableRec as Record>::SCHEMA),
    Part::Schema(<NodeRec as Record>::SCHEMA),
]);

/// Writes the types (and regions, constants, lists and clauses) of a query result into a table
/// of its own, each once: each is named by its place in the table, from the `u32` its method
/// returns.
///
/// Cost: a value already written is one hash of a `u64` key and one probe. A new one is
/// written to `words` once, its children first; the words of the values being written wait
/// in `pending`, one stack reused for the whole table, so writing allocates only as the table
/// grows.
pub struct TyTableWriter<'db> {
    /// The place of each value written, by kind and row.
    places: HashMap<(Kind, NonZeroU32), u32>,
    nodes: Vec<NodeRec>,
    words: Vec<u32>,
    /// The words of the values being written, innermost last.
    pending: Vec<u32>,
    _db: PhantomData<DbInterner<'db>>,
}

impl<'db> TyTableWriter<'db> {
    /// A writer of a table read back with an interner of the same database (any revision).
    pub fn new(_interner: DbInterner<'db>) -> Self {
        Self::empty()
    }

    fn empty() -> Self {
        // alloc: none until the first value; one writer's buffers per table written.
        TyTableWriter {
            places: HashMap::default(),
            // alloc: as above.
            nodes: Vec::new(),
            // alloc: as above.
            words: Vec::new(),
            // alloc: as above.
            pending: Vec::new(),
            _db: PhantomData,
        }
    }

    pub fn ty(&mut self, ty: Ty<'db>) -> u32 {
        self.place(ty)
    }

    pub fn region(&mut self, region: Region<'db>) -> u32 {
        self.place(region)
    }

    pub fn konst(&mut self, konst: Const<'db>) -> u32 {
        self.place(konst)
    }

    pub fn args(&mut self, args: GenericArgs<'db>) -> u32 {
        self.place(args)
    }

    pub fn clauses(&mut self, clauses: Clauses<'db>) -> u32 {
        self.place(clauses)
    }

    pub fn predicate(&mut self, predicate: Predicate<'db>) -> u32 {
        self.place(predicate)
    }

    /// Writes the table, returning where it is in `out` for [`TyTableReader::open`].
    pub fn write(self, out: &mut Writer) -> u64 {
        let nodes = out.push_slice(&self.nodes);
        let words = out.push_slice(&self.words);
        let at = out.next::<TableRec>();
        let field = |offset: usize| at + offset as u64;
        out.push(&TableRec {
            nodes: Slice::between(field(std::mem::offset_of!(TableRec, nodes)), nodes, self.nodes.len()),
            words: Slice::between(field(std::mem::offset_of!(TableRec, words)), words, self.words.len()),
        })
    }

    /// The place of `node`, written with what it holds if it is not in the table yet. A miss
    /// hashes its key twice, as its children are written between the lookup and the insert.
    #[inline]
    fn place<N: Node<'db>>(&mut self, node: N) -> u32 {
        let key = (N::KIND, node.row());
        match self.places.get(&key) {
            Some(&place) => place,
            None => self.add(key, node),
        }
    }

    #[cold]
    fn add<N: Node<'db>>(&mut self, key: (Kind, NonZeroU32), node: N) -> u32 {
        let start = self.pending.len();
        ensure_sufficient_stack(|| node.put_node(&mut Children { writer: self }));
        let place = u32::try_from(self.nodes.len()).expect("a type table of fewer than 2^32 values");
        let at = u32::try_from(self.words.len()).expect("a type table of fewer than 2^32 words");
        self.nodes.push(NodeRec { kind: N::KIND as u32, at });
        self.words.extend_from_slice(&self.pending[start..]);
        self.pending.truncate(start);
        self.places.insert(key, place);
        place
    }
}

/// A finished buffer holding a table [`TyTableWriter::write`] wrote, as little-endian words, to
/// be placed with a record's columns at `u128` alignment.
pub(crate) fn table_words(bytes: &[u8]) -> Vec<u128> {
    assert!(bytes.len().is_multiple_of(ALIGN), "a finished buffer is padded to whole words");
    bytes
        .chunks_exact(size_of::<u128>())
        .map(|chunk| u128::from_le_bytes(chunk.try_into().expect("a chunk of 16 bytes")))
        .collect() // alloc: the record's type table, exact size, once per table persisted
}

/// The words of a node being written, onto the writer's `pending` stack above its parent's:
/// its children are written first.
struct Children<'w, 'db> {
    writer: &'w mut TyTableWriter<'db>,
}

impl<'db> Sink<'db> for Children<'_, 'db> {
    #[inline]
    fn word(&mut self, word: u32) {
        self.writer.pending.push(word);
    }

    #[inline]
    fn node<N: Node<'db>>(&mut self, node: N) {
        let place = self.writer.place(node);
        self.writer.pending.push(place);
    }

    fn adt(&mut self, adt: AdtDef) {
        SolverDefId::from(adt.def_id()).put(self);
    }

    fn id<T: crate::check_cache::Relocate>(&mut self, id: T) {
        let bits = crate::check_cache::id_bits(id);
        self.word(bits as u32);
        self.word((bits >> 32) as u32);
    }
}

/// A table [`TyTableWriter`] wrote, read back: every value checked and interned when it is
/// opened, so a value's place gives it with one lookup.
pub struct TyTableReader<'a, 'db> {
    /// Each value of the table by its place: its kind and its row.
    values: Vec<(Kind, NonZeroU32)>,
    _buffer: PhantomData<(&'a [u8], DbInterner<'db>)>,
}

impl<'a, 'db> TyTableReader<'a, 'db> {
    /// The table [`TyTableWriter::write`] wrote at `at` in `buffer`. An ADT's flags are read
    /// from `interner`'s database.
    pub fn open(
        interner: DbInterner<'db>,
        buffer: &'a [u8],
        at: u64,
    ) -> Result<Self, FormatError> {
        Self::open_with(buffer, at, |id| crate::check_cache::outside(|| AdtDef::new(id, interner)))
    }

    /// As [`Self::open`], an ADT made by `adt`.
    pub(crate) fn open_with(
        buffer: &'a [u8],
        at: u64,
        adt: impl Fn(AdtId) -> AdtDef,
    ) -> Result<Self, FormatError> {
        let table = record_at::<TableRec>(buffer, at)?;
        let nodes = table.nodes.follow(buffer)?;
        let words = table.words.follow(buffer)?;
        let mut values = Vec::with_capacity(nodes.len());
        for (place, node) in nodes.iter().enumerate() {
            let at = (node as *const NodeRec as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
            let start = node.at as usize;
            let end = nodes.get(place + 1).map_or(words.len(), |next| next.at as usize);
            if start > end || end > words.len() {
                return Err(FormatError::Invalid { at, what: "a type table value's words out of order" });
            }
            let kind = KINDS
                .get(node.kind as usize)
                .copied()
                .ok_or(FormatError::Invalid { at, what: "a type table value of no kind" })?;
            let mut source =
                Reading { values: &values, words: &words[start..end], adt: &adt, at };
            // A value's children are places of values read before it, so reading one is
            // shallow: no stack check.
            let row = match kind {
                Kind::Ty => Ty::get_node(&mut source).map(Node::row),
                Kind::Region => Region::get_node(&mut source).map(Node::row),
                Kind::Const => Const::get_node(&mut source).map(Node::row),
                Kind::Pattern => Pattern::get_node(&mut source).map(Node::row),
                Kind::Predicate => Predicate::get_node(&mut source).map(Node::row),
                Kind::ValTree => ValTree::get_node(&mut source).map(Node::row),
                Kind::GenericArgs => GenericArgs::get_node(&mut source).map(Node::row),
                Kind::Tys => Tys::get_node(&mut source).map(Node::row),
                Kind::Consts => Consts::get_node(&mut source).map(Node::row),
                Kind::BoundVarKinds => BoundVarKinds::get_node(&mut source).map(Node::row),
                Kind::PatList => PatList::get_node(&mut source).map(Node::row),
                Kind::BoundExistentialPredicates => {
                    BoundExistentialPredicates::get_node(&mut source).map(Node::row)
                }
                Kind::Clauses => Clauses::get_node(&mut source).map(Node::row),
            }?;
            if !source.words.is_empty() {
                return Err(FormatError::Invalid { at, what: "a type table value with words left over" });
            }
            values.push((kind, row));
        }
        Ok(TyTableReader { values, _buffer: PhantomData })
    }

    pub fn ty(&self, at: u32) -> Result<Ty<'db>, FormatError> {
        self.value(at)
    }

    pub fn region(&self, at: u32) -> Result<Region<'db>, FormatError> {
        self.value(at)
    }

    pub fn konst(&self, at: u32) -> Result<Const<'db>, FormatError> {
        self.value(at)
    }

    pub fn args(&self, at: u32) -> Result<GenericArgs<'db>, FormatError> {
        self.value(at)
    }

    pub fn clauses(&self, at: u32) -> Result<Clauses<'db>, FormatError> {
        self.value(at)
    }

    pub fn predicate(&self, at: u32) -> Result<Predicate<'db>, FormatError> {
        self.value(at)
    }

    fn value<N: Node<'db>>(&self, at: u32) -> Result<N, FormatError> {
        match self.values.get(at as usize) {
            Some(&(kind, row)) if kind == N::KIND => Ok(N::at(row)),
            Some(_) => {
                Err(FormatError::Invalid { at: u64::from(at), what: "a type table value of another kind" })
            }
            None => Err(FormatError::Invalid { at: u64::from(at), what: "a place past the type table" }),
        }
    }
}

/// The words of one value of a persisted table, its children among the values before it.
struct Reading<'r, A> {
    values: &'r [(Kind, NonZeroU32)],
    words: &'r [u32],
    adt: &'r A,
    /// Where the value's record is, for errors.
    at: u64,
}

impl<'db, A: Fn(AdtId) -> AdtDef> Source<'db> for Reading<'_, A> {
    type Error = FormatError;

    fn word(&mut self) -> Result<u32, FormatError> {
        let (&word, rest) = self.words.split_first().ok_or(FormatError::Invalid {
            at: self.at,
            what: "a type table value shorter than its kind",
        })?;
        self.words = rest;
        Ok(word)
    }

    fn node<N: Node<'db>>(&mut self) -> Result<N, FormatError> {
        let place = self.word()?;
        match self.values.get(place as usize) {
            Some(&(kind, row)) if kind == N::KIND => Ok(N::at(row)),
            Some(_) => Err(self.invalid("a child of another kind")),
            None => Err(self.invalid("a child that is not before its parent")),
        }
    }

    fn adt(&mut self) -> Result<AdtDef, FormatError> {
        match SolverDefId::get(self)? {
            SolverDefId::AdtId(id) => Ok((self.adt)(id)),
            _ => Err(self.invalid("an ADT that is another definition")),
        }
    }

    fn id<T: crate::check_cache::Relocate>(&mut self) -> Result<T, FormatError> {
        let index = self.word()?;
        let generation = self.word()?;
        crate::check_cache::id_of(u64::from(index) | (u64::from(generation) << 32))
            .ok_or_else(|| self.invalid("an id of index 0 or not in the table"))
    }

    fn count(&mut self) -> Result<usize, FormatError> {
        let count = self.word()? as usize;
        match count <= self.words.len() {
            true => Ok(count),
            false => Err(self.invalid("a list longer than its words")),
        }
    }

    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at, what }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Barrier};

    use blaze_format::{Aligned, HEADER_LEN, Writer};
    use blaze_query::db::FromId;
    use hir_def::{CallableDefId, FunctionId, TypeAliasId};
    use rustc_type_ir::{FlagComputation, Flags, INNERMOST, TypeFlags};

    use super::*;
    use crate::next_solver::TermId;

    #[test]
    fn record_at_views_only_checked_bytes_in_bounds_and_aligned() {
        let mut bytes = vec![0u8; HEADER_LEN + 24];
        bytes[HEADER_LEN..HEADER_LEN + 8].copy_from_slice(&0x0123_4567_89AB_CDEFu64.to_ne_bytes());
        bytes[HEADER_LEN + 8] = 1;
        bytes[HEADER_LEN + 9] = 2;
        let buffer = Aligned::copy_from(&bytes);
        let at = HEADER_LEN as u64;
        // The view equals the bytes read out safely.
        let copied = u64::from_ne_bytes(bytes[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap());
        assert_eq!(*record_at::<u64>(&buffer, at).unwrap(), copied);
        assert!(*record_at::<bool>(&buffer, at + 8).unwrap());
        assert!(record_at::<bool>(&buffer, at + 9).is_err(), "2 is no `bool`");
        assert!(record_at::<u64>(&buffer, at + 1).is_err(), "misaligned");
        assert!(record_at::<u64>(&buffer, at - 8).is_err(), "in the header");
        assert!(record_at::<u64>(&buffer, bytes.len() as u64).is_err(), "past the end");
        assert!(record_at::<u64>(&buffer, u64::MAX).is_err(), "past any buffer");
    }

    fn id<T: FromId>(index: u32) -> T {
        T::from_id(blaze_query::db::Id::from_u32(index))
    }

    fn local<T>(index: u32) -> Idx<T> {
        Idx::from_raw(RawIdx::from_u32(index))
    }

    fn adt(id: AdtId) -> AdtDef {
        AdtDef::from_parts(id, 0).expect("no flags are valid flags")
    }

    /// Values of every kind and every variant of each.
    struct Samples<'db> {
        tys: Vec<TyKind<'db>>,
        regions: Vec<RegionKind<'db>>,
        consts: Vec<ConstKind<'db>>,
        patterns: Vec<PatternKind<'db>>,
        predicates: Vec<Binder<'db, PredicateKind<'db>>>,
        args: GenericArgs<'db>,
    }

    fn region<'db>(kind: RegionKind<'db>) -> Region<'db> {
        rustc_type_ir::Region(InternedRegion::intern(kind))
    }

    fn konst<'db>(kind: ConstKind<'db>) -> Const<'db> {
        rustc_type_ir::Const(InternedConst::intern(kind))
    }

    fn name(text: &str) -> Symbol {
        Symbol(intern::Symbol::intern(text))
    }

    fn samples<'db>() -> Samples<'db> {
        let u8 = Ty::intern(TyKind::Uint(UintTy::U8));
        let statik = region(RegionKind::ReStatic);
        let function: FunctionId = id(3);
        let generic = GenericDefId::FunctionId(function);
        let param = TypeParamId::from_unchecked(TypeOrConstParamId { parent: generic, local_id: local(0) });
        let const_param =
            ConstParamId::from_unchecked(TypeOrConstParamId { parent: generic, local_id: local(1) });
        let lifetime = LifetimeParamId { parent: generic, local_id: local(0) };
        let trait_ = TraitIdWrapper(id(4));
        let alias: TypeAliasId = id(5);
        let args = GenericArgs::new_from_slice(&[u8.into(), statik.into()]);
        let vars = BoundVarKinds::new_from_slice(&[
            BoundVariableKind::Region(BoundRegionKind::Anon),
            BoundVariableKind::Region(BoundRegionKind::NamedForPrinting(name("'a"))),
            BoundVariableKind::Region(BoundRegionKind::Named(SolverDefId::FunctionId(function))),
            BoundVariableKind::Region(BoundRegionKind::ClosureEnv),
            BoundVariableKind::Ty(BoundTyKind::Anon),
            BoundVariableKind::Ty(BoundTyKind::Param(SolverDefId::TypeAliasId(alias))),
            BoundVariableKind::Const,
        ]);
        let seven = konst(ConstKind::Value(ValueConst::new(
            u8,
            ValTree::new(ValTreeKind::Leaf(ScalarInt::from(7u8))),
        )));
        // `pair` is `fn(u8) -> bool`: one input.
        let header = |abi, safety, c_variadic, splatted| FnHeader {
            fn_sig_kind: FnSigKind::new(abi, safety, c_variadic, splatted, 1)
                .expect("a splatted argument among the inputs"),
        };
        let pair = Tys::new_from_slice(&[u8, Ty::intern(TyKind::Bool)]);
        let trait_ref = TraitRef::from_checked_parts(trait_, args);
        let alias_term = |kind| Alias::from_checked_parts(kind, args);
        let alias_ty = |kind| Alias::from_checked_parts(kind, args);
        let alias_const = |kind| Alias::from_checked_parts(kind, args);
        let anon = AnonConstIdWrapper(id(11));
        let bound_ty = |var, kind| BoundTy { var: BoundVar::from_u32(var), kind };
        let bound_region = |var, kind| BoundRegion { var: BoundVar::from_u32(var), kind };
        let range = PatternKind::Range { start: seven, end: seven };

        let tys = vec![
            TyKind::Bool,
            TyKind::Char,
            TyKind::Int(IntTy::I32),
            TyKind::Uint(UintTy::U128),
            TyKind::Float(FloatTy::F64),
            TyKind::Adt(adt(AdtId::StructId(id(1))), args),
            TyKind::Adt(adt(AdtId::EnumId(id(2))), GenericArgs::empty()),
            TyKind::Foreign(TypeAliasIdWrapper(alias)),
            TyKind::Str,
            TyKind::Array(u8, seven),
            TyKind::Pat(u8, Pattern::intern(range)),
            TyKind::Slice(u8),
            TyKind::RawPtr(u8, Mutability::Mut),
            TyKind::Ref(statik, u8, Mutability::Not),
            TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(function)), Binder::dummy(args)),
            TyKind::FnDef(
                CallableIdWrapper(CallableDefId::FunctionId(function)),
                Binder::bind_with_vars(args, vars),
            ),
            TyKind::FnPtr(
                Binder::bind_with_vars(rustc_type_ir::FnSigTys { inputs_and_output: pair }, vars),
                header(ExternAbi::C { unwind: false }, Safety::Safe, true, None),
            ),
            TyKind::FnPtr(
                Binder::dummy(rustc_type_ir::FnSigTys { inputs_and_output: pair }),
                header(ExternAbi::Rust, Safety::Unsafe, false, None),
            ),
            TyKind::FnPtr(
                Binder::dummy(rustc_type_ir::FnSigTys { inputs_and_output: pair }),
                header(ExternAbi::Rust, Safety::Safe, false, Some(0)),
            ),
            TyKind::UnsafeBinder(Binder::bind_with_vars(u8, vars).into()),
            TyKind::Dynamic(
                BoundExistentialPredicates::new_from_slice(&[
                    Binder::bind_with_vars(
                        ExistentialPredicate::Trait(ExistentialTraitRef::from_checked_parts(
                            trait_, args,
                        )),
                        vars,
                    ),
                    Binder::dummy(ExistentialPredicate::Projection(
                        ExistentialProjection::from_checked_parts(
                            TraitAssocTermId(TermId::TypeAliasId(alias)),
                            args,
                            u8.into(),
                        ),
                    )),
                    Binder::dummy(ExistentialPredicate::AutoTrait(trait_)),
                ]),
                statik,
            ),
            TyKind::Closure(ClosureIdWrapper(id(7)), args),
            TyKind::CoroutineClosure(CoroutineClosureIdWrapper(id(8)), args),
            TyKind::Coroutine(CoroutineIdWrapper(id(9)), args),
            TyKind::CoroutineWitness(CoroutineIdWrapper(id(9)), args),
            TyKind::Never,
            TyKind::Tuple(pair),
            TyKind::Tuple(Tys::empty()),
            TyKind::Alias(
                IsRigid::No,
                alias_ty(AliasTyKind::Projection { def_id: TraitAssocTyId(alias) }),
            ),
            TyKind::Alias(
                IsRigid::Yes,
                alias_ty(AliasTyKind::Projection { def_id: TraitAssocTyId(alias) }),
            ),
            TyKind::Alias(
                IsRigid::No,
                alias_ty(AliasTyKind::Inherent { def_id: InherentAssocTyId(alias) }),
            ),
            TyKind::Alias(
                IsRigid::Yes,
                alias_ty(AliasTyKind::Opaque { def_id: OpaqueTyIdWrapper(id(10)) }),
            ),
            TyKind::Alias(IsRigid::No, alias_ty(AliasTyKind::Free { def_id: FreeTyAliasId(alias) })),
            TyKind::Param(ParamTy { id: param, index: 0 }),
            TyKind::Bound(BoundVarIndexKind::Bound(INNERMOST), bound_ty(4, BoundTyKind::Anon)),
            TyKind::Bound(
                BoundVarIndexKind::Canonical,
                bound_ty(5, BoundTyKind::Param(SolverDefId::TypeAliasId(alias))),
            ),
            TyKind::Placeholder(PlaceholderType::new(
                UniverseIndex::from_u32(2),
                bound_ty(0, BoundTyKind::Anon),
            )),
            TyKind::Infer(InferTy::TyVar(TyVid::from_u32(1))),
            TyKind::Infer(InferTy::IntVar(IntVid::from_u32(2))),
            TyKind::Infer(InferTy::FloatVar(FloatVid::from_u32(3))),
            TyKind::Infer(InferTy::FreshTy(4)),
            TyKind::Infer(InferTy::FreshIntTy(5)),
            TyKind::Infer(InferTy::FreshFloatTy(6)),
            TyKind::Error(ErrorGuaranteed),
        ];

        let regions = vec![
            RegionKind::ReEarlyParam(EarlyParamRegion { id: lifetime, index: 0 }),
            RegionKind::ReBound(
                BoundVarIndexKind::Bound(DebruijnIndex::from_u32(1)),
                bound_region(2, BoundRegionKind::Named(SolverDefId::FunctionId(function))),
            ),
            RegionKind::ReBound(BoundVarIndexKind::Canonical, bound_region(0, BoundRegionKind::Anon)),
            RegionKind::ReLateParam(LateParamRegion {
                scope: SolverDefId::FunctionId(function),
                kind: bound_region(1, BoundRegionKind::ClosureEnv),
            }),
            RegionKind::ReStatic,
            RegionKind::ReVar(RegionVid::from_u32(9)),
            RegionKind::RePlaceholder(PlaceholderRegion::new(
                UniverseIndex::from_u32(1),
                bound_region(0, BoundRegionKind::NamedForPrinting(Symbol(intern::sym::tick_static))),
            )),
            RegionKind::ReErased,
            RegionKind::ReError(ErrorGuaranteed),
        ];

        let consts = vec![
            ConstKind::Param(ParamConst { id: const_param, index: 1 }),
            ConstKind::Infer(InferConst::Var(ConstVid::from_u32(3))),
            ConstKind::Infer(InferConst::Fresh(2)),
            ConstKind::Bound(BoundVarIndexKind::Bound(INNERMOST), BoundConst::new(BoundVar::from_u32(6))),
            ConstKind::Bound(BoundVarIndexKind::Canonical, BoundConst::new(BoundVar::from_u32(0))),
            ConstKind::Placeholder(PlaceholderConst::new(
                UniverseIndex::ROOT,
                BoundConst::new(BoundVar::from_u32(0)),
            )),
            ConstKind::Alias(
                IsRigid::No,
                alias_const(AliasConstKind::Projection { def_id: TraitAssocConstId(id(6)) }),
            ),
            ConstKind::Alias(
                IsRigid::Yes,
                alias_const(AliasConstKind::InherentSelf { def_id: InherentAssocConstId(id(6)) }),
            ),
            ConstKind::Alias(
                IsRigid::No,
                alias_const(AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id(6)) }),
            ),
            ConstKind::Alias(
                IsRigid::No,
                alias_const(AliasConstKind::Free { def_id: FreeConstAliasId(id(6)) }),
            ),
            ConstKind::Alias(IsRigid::Yes, alias_const(AliasConstKind::Anon { def_id: anon })),
            ConstKind::Value(ValueConst::new(u8, ValTree::new(ValTreeKind::Leaf(ScalarInt::from(7u8))))),
            ConstKind::Value(ValueConst::new(
                Ty::intern(TyKind::Tuple(pair)),
                ValTree::new(ValTreeKind::Branch(Consts::new_from_slice(&[seven, seven]))),
            )),
            ConstKind::Error(ErrorGuaranteed),
            ConstKind::Expr(ExprConst),
        ];

        let patterns = vec![
            range,
            PatternKind::Or(PatList::new_from_slice(&[Pattern::intern(range), Pattern::intern(PatternKind::NotNull)])),
            PatternKind::NotNull,
        ];

        let clause = |kind| Binder::dummy(PredicateKind::Clause(kind));
        let normalizes = |kind, term| {
            Binder::dummy(PredicateKind::NormalizesTo(NormalizesTo { alias: alias_term(kind), term }))
        };
        let predicates = vec![
            clause(ClauseKind::Trait(TraitClause { trait_ref, polarity: ClausePolarity::Positive })),
            clause(ClauseKind::Trait(TraitClause { trait_ref, polarity: ClausePolarity::Negative })),
            clause(ClauseKind::RegionOutlives(rustc_type_ir::OutlivesClause(statik, statik))),
            clause(ClauseKind::TypeOutlives(rustc_type_ir::OutlivesClause(u8, statik))),
            clause(ClauseKind::Projection(ProjectionClause {
                projection_term: alias_term(AliasTermKind::ProjectionTy { def_id: TraitAssocTyId(alias) }),
                term: u8.into(),
            })),
            clause(ClauseKind::ConstArgHasType(seven, u8)),
            clause(ClauseKind::WellFormed(u8.into())),
            clause(ClauseKind::ConstEvaluatable(seven)),
            clause(ClauseKind::HostEffect(HostEffectClause { trait_ref, constness: BoundConstness::Const })),
            clause(ClauseKind::HostEffect(HostEffectClause { trait_ref, constness: BoundConstness::Maybe })),
            clause(ClauseKind::UnstableFeature(name("generic_const_items"))),
            clause(ClauseKind::UnstableFeature(name("const_fn"))),
            clause(ClauseKind::UnstableFeature(name(""))),
            Binder::dummy(PredicateKind::DynCompatible(trait_)),
            Binder::dummy(PredicateKind::Subtype(SubtypePredicate { a_is_expected: true, a: u8, b: u8 })),
            Binder::dummy(PredicateKind::Coerce(CoercePredicate { a: u8, b: u8 })),
            Binder::dummy(PredicateKind::ConstEquate(seven, seven)),
            Binder::dummy(PredicateKind::Ambiguous),
            normalizes(AliasTermKind::InherentTy { def_id: InherentAssocTyId(alias) }, u8.into()),
            normalizes(AliasTermKind::OpaqueTy { def_id: OpaqueTyIdWrapper(id(10)) }, u8.into()),
            normalizes(AliasTermKind::FreeTy { def_id: FreeTyAliasId(alias) }, u8.into()),
            normalizes(AliasTermKind::ProjectionTy { def_id: TraitAssocTyId(alias) }, u8.into()),
            normalizes(AliasTermKind::AnonConst { def_id: anon }, seven.into()),
            normalizes(AliasTermKind::ProjectionConst { def_id: TraitAssocConstId(id(6)) }, seven.into()),
            normalizes(AliasTermKind::FreeConst { def_id: FreeConstAliasId(id(6)) }, seven.into()),
            normalizes(
                AliasTermKind::InherentConstSelf { def_id: InherentAssocConstId(id(6)) },
                seven.into(),
            ),
            normalizes(
                AliasTermKind::InherentConstImpl { def_id: InherentAssocConstId(id(6)) },
                seven.into(),
            ),
            Binder::bind_with_vars(
                PredicateKind::Clause(ClauseKind::Trait(TraitClause {
                    trait_ref,
                    polarity: ClausePolarity::Positive,
                })),
                vars,
            ),
        ];

        Samples { tys, regions, consts, patterns, predicates, args }
    }

    #[test]
    fn interned_values_are_four_bytes() {
        assert_eq!(size_of::<Option<Ty<'static>>>(), 4);
        assert_eq!(size_of::<Option<Region<'static>>>(), 4);
        assert_eq!(size_of::<Option<Const<'static>>>(), 4);
        assert_eq!(size_of::<Option<Pattern<'static>>>(), 4);
        assert_eq!(size_of::<Option<Predicate<'static>>>(), 4);
        assert_eq!(size_of::<Option<Clause<'static>>>(), 4);
        assert_eq!(size_of::<Option<ValTree<'static>>>(), 4);
        assert_eq!(size_of::<Option<GenericArg<'static>>>(), 4);
        assert_eq!(size_of::<Option<Term<'static>>>(), 4);
        assert_eq!(size_of::<Option<GenericArgs<'static>>>(), 4);
        assert_eq!(size_of::<Option<Clauses<'static>>>(), 4);
    }

    #[test]
    fn a_value_interned_twice_has_one_index() {
        let samples = samples();
        for &kind in &samples.tys {
            let ty = Ty::intern(kind);
            assert_eq!(Ty::intern(kind).index(), ty.index());
        }
        for &kind in &samples.regions {
            assert_eq!(region(kind), region(kind));
        }
        for &kind in &samples.consts {
            assert_eq!(konst(kind), konst(kind));
        }
        for &kind in &samples.predicates {
            assert_eq!(Predicate::intern(kind), Predicate::intern(kind));
        }
        // Every sample is another value, so another row: the codec tells apart each variant and
        // each field (`IsRigid`, `FnDef`'s binder, the alias kinds), and drops none.
        let distinct: HashSet<_> = samples.tys.iter().map(|&kind| Ty::intern(kind)).collect();
        assert_eq!(distinct.len(), samples.tys.len());
        let distinct: HashSet<_> = samples.regions.iter().map(|&kind| region(kind)).collect();
        assert_eq!(distinct.len(), samples.regions.len());
        let distinct: HashSet<_> = samples.consts.iter().map(|&kind| konst(kind)).collect();
        assert_eq!(distinct.len(), samples.consts.len());
        let distinct: HashSet<_> =
            samples.predicates.iter().map(|&kind| Predicate::intern(kind)).collect();
        assert_eq!(distinct.len(), samples.predicates.len());
        assert_eq!(
            GenericArgs::new_from_slice(samples.args.as_slice()).index(),
            samples.args.index()
        );
    }

    #[test]
    fn every_kind_reads_back_from_its_row() {
        let samples = samples();
        for &kind in &samples.tys {
            assert!(Ty::intern(kind).kind() == kind);
        }
        for &kind in &samples.regions {
            assert!(region(kind).kind() == kind);
        }
        for &kind in &samples.consts {
            assert!(konst(kind).kind() == kind);
        }
        for &kind in &samples.patterns {
            assert!(Pattern::intern(kind).kind() == kind);
        }
        for &kind in &samples.predicates {
            assert!(Predicate::intern(kind).kind() == kind);
        }
    }

    #[test]
    fn flags_are_those_computed_again() {
        let samples = samples();
        for &kind in &samples.tys {
            let ty = Ty::intern(kind);
            let computed = FlagComputation::for_kind(&kind);
            assert_eq!(ty.flags(), computed.flags);
            assert_eq!(ty.outer_exclusive_binder(), computed.outer_exclusive_binder);
        }
        for &kind in &samples.consts {
            let value = konst(kind);
            let computed = FlagComputation::for_const_kind(&kind);
            assert_eq!(value.flags(), computed.flags);
            assert_eq!(value.outer_exclusive_binder(), computed.outer_exclusive_binder);
        }
        for &kind in &samples.predicates {
            let predicate = Predicate::intern(kind);
            let computed = FlagComputation::for_predicate(kind);
            assert_eq!(predicate.flags(), computed.flags);
            assert_eq!(predicate.outer_exclusive_binder(), computed.outer_exclusive_binder);
        }
        let clauses: Vec<_> =
            samples.predicates.iter().map(|&kind| Clause(Predicate::intern(kind))).collect();
        let computed = FlagComputation::<DbInterner<'_>>::for_clauses(&clauses);
        let clauses = Clauses::new_from_slice(&clauses);
        assert_eq!(clauses.flags(), computed.flags);
        assert_eq!(clauses.outer_exclusive_binder(), computed.outer_exclusive_binder);
        for &kind in &samples.patterns {
            let pattern = Pattern::intern(kind);
            let (flags, binder) = match kind {
                PatternKind::Range { start, end } => (
                    FlagComputation::for_const_kind(&start.kind()).flags
                        | FlagComputation::for_const_kind(&end.kind()).flags,
                    start.outer_exclusive_binder().max(end.outer_exclusive_binder()),
                ),
                PatternKind::Or(pats) => (
                    pats.iter().fold(TypeFlags::empty(), |flags, pat| flags | pat.flags()),
                    pats.iter().map(|pat| pat.outer_exclusive_binder()).max().unwrap_or(INNERMOST),
                ),
                PatternKind::NotNull => (TypeFlags::empty(), INNERMOST),
            };
            assert_eq!(pattern.flags(), flags);
            assert_eq!(pattern.outer_exclusive_binder(), binder);
        }
        assert_eq!(region(RegionKind::ReStatic).flags(), TypeFlags::HAS_FREE_REGIONS);
        assert_eq!(region(RegionKind::ReErased).flags(), TypeFlags::HAS_RE_ERASED);
        assert_eq!(
            region(RegionKind::ReVar(RegionVid::from_u32(0))).flags(),
            TypeFlags::HAS_FREE_REGIONS | TypeFlags::HAS_FREE_LOCAL_REGIONS | TypeFlags::HAS_RE_INFER
        );
        let bound = region(RegionKind::ReBound(
            BoundVarIndexKind::Bound(DebruijnIndex::from_u32(1)),
            BoundRegion { var: BoundVar::from_u32(0), kind: BoundRegionKind::Anon },
        ));
        assert_eq!(bound.flags(), TypeFlags::HAS_RE_BOUND);
        assert_eq!(bound.outer_exclusive_binder(), DebruijnIndex::from_u32(2));
    }

    #[test]
    fn eight_threads_interning_at_once_get_one_index_per_type() {
        const THREADS: u32 = 8;
        const TYPES: u32 = 4000;
        let barrier = Barrier::new(THREADS as usize);
        let kind = |n: u32| {
            let leaf = Ty::intern(TyKind::Infer(InferTy::FreshTy(n % 64)));
            match n % 3 {
                0 => TyKind::Slice(Ty::intern(TyKind::Infer(InferTy::FreshTy(n)))),
                1 => TyKind::Tuple(Tys::new_from_slice(&[leaf, Ty::intern(TyKind::Infer(InferTy::FreshIntTy(n)))])),
                _ => TyKind::Array(leaf, konst(ConstKind::Infer(InferConst::Fresh(n)))),
            }
        };
        let seen: Vec<Vec<u32>> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..THREADS)
                .map(|thread| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        let mut indices = vec![0; TYPES as usize];
                        for i in 0..TYPES {
                            let n = (i * 7919 + thread * 104_729) % TYPES;
                            indices[n as usize] = Ty::intern(kind(n)).index();
                        }
                        indices
                    })
                })
                .collect();
            threads.into_iter().map(|thread| thread.join().unwrap()).collect()
        });
        assert!(seen.iter().all(|indices| *indices == seen[0]));
        assert_eq!(seen[0].iter().collect::<HashSet<_>>().len(), TYPES as usize);
        for (n, &index) in seen[0].iter().enumerate() {
            assert!(Ty::at(NonZeroU32::new(index).unwrap()).kind() == kind(n as u32));
        }
    }

    #[test]
    fn a_table_reads_back_as_written() {
        let samples = samples();
        let tys: Vec<_> = samples.tys.iter().map(|&kind| Ty::intern(kind)).collect();
        let regions: Vec<_> = samples.regions.iter().map(|&kind| region(kind)).collect();
        let consts: Vec<_> = samples.consts.iter().map(|&kind| konst(kind)).collect();
        let clauses = Clauses::new_from_slice(
            &samples.predicates.iter().map(|&kind| Clause(Predicate::intern(kind))).collect::<Vec<_>>(),
        );
        let patterns: Vec<_> = samples
            .patterns
            .iter()
            .map(|&kind| Ty::intern(TyKind::Pat(tys[2], Pattern::intern(kind))))
            .collect();

        let mut out = Writer::new(TABLE_SCHEMA);
        let mut writer = TyTableWriter::empty();
        let ty_places: Vec<_> = tys.iter().chain(&patterns).map(|&ty| writer.ty(ty)).collect();
        let region_places: Vec<_> = regions.iter().map(|&region| writer.region(region)).collect();
        let const_places: Vec<_> = consts.iter().map(|&konst| writer.konst(konst)).collect();
        let args_place = writer.args(samples.args);
        let clauses_place = writer.clauses(clauses);
        assert_eq!(writer.ty(tys[0]), ty_places[0], "a value is written once");
        let table = writer.write(&mut out);
        let buffer = Aligned::copy_from(&out.finish(table));

        let reader = TyTableReader::open_with(&buffer, table, adt).unwrap();
        for (&ty, &place) in tys.iter().chain(&patterns).zip(&ty_places) {
            assert_eq!(reader.ty(place).unwrap(), ty);
        }
        for (&region, &place) in regions.iter().zip(&region_places) {
            assert_eq!(reader.region(place).unwrap(), region);
        }
        for (&konst, &place) in consts.iter().zip(&const_places) {
            assert_eq!(reader.konst(place).unwrap(), konst);
        }
        assert_eq!(reader.args(args_place).unwrap(), samples.args);
        assert_eq!(reader.clauses(clauses_place).unwrap(), clauses);
        assert!(reader.region(ty_places[0]).is_err(), "a place of another kind");
        assert!(reader.ty(u32::MAX).is_err(), "a place past the table");
    }

    /// Whether the table of `values`, each its kind and words, in order, opens.
    fn opens(values: &[(Kind, &[u32])]) -> bool {
        let mut out = Writer::new(TABLE_SCHEMA);
        let mut nodes = Vec::new();
        let mut words = Vec::new();
        for &(kind, value) in values {
            nodes.push(NodeRec { kind: kind as u32, at: words.len() as u32 });
            words.extend_from_slice(value);
        }
        let nodes_at = out.push_slice(&nodes);
        let words_at = out.push_slice(&words);
        let at = out.next::<TableRec>();
        let field = |offset: usize| at + offset as u64;
        let table = out.push(&TableRec {
            nodes: Slice::between(field(std::mem::offset_of!(TableRec, nodes)), nodes_at, nodes.len()),
            words: Slice::between(field(std::mem::offset_of!(TableRec, words)), words_at, words.len()),
        });
        let buffer = Aligned::copy_from(&out.finish(table));
        TyTableReader::open_with(&buffer, table, adt).is_ok()
    }

    #[test]
    fn a_table_that_names_a_later_value_is_refused() {
        // A tuple of the list at place 5, which is not before it.
        assert!(!opens(&[(Kind::Ty, &[22, 5])]));
        assert!(opens(&[(Kind::Tys, &[0]), (Kind::Ty, &[22, 0])]));
    }

    #[test]
    fn a_table_of_words_no_kind_has_is_refused() {
        // Places 0 to 3: no arguments, `bool`, no bound variables and the signature `() -> bool`.
        let before: [(Kind, &[u32]); 4] = [
            (Kind::GenericArgs, &[0]),
            (Kind::Ty, &[0]),
            (Kind::BoundVarKinds, &[0]),
            (Kind::Tys, &[1, 1]),
        ];
        let with = |kind: Kind, words: &[u32]| {
            let mut values = before.to_vec();
            values.push((kind, words));
            opens(&values)
        };
        // `TyKind::Alias(IsRigid, <bool as Trait>::Assoc)`, with an `IsRigid` past `No`.
        assert!(with(Kind::Ty, &[23, 1, 0, 0, 10, 5, 0]));
        assert!(!with(Kind::Ty, &[23, 2, 0, 0, 10, 5, 0]));
        // `ConstKind::Alias(No, <_ as Trait>::CONST)`, with an `AliasConstKind` past `Anon`.
        assert!(with(Kind::Const, &[4, 1, 0, 0, 3, 6, 0]));
        assert!(!with(Kind::Const, &[4, 1, 0, 5, 3, 6, 0]));
        // `NormalizesTo(<_ as Trait>::Assoc, bool)`, with an `AliasTermKind` past
        // `InherentConstImpl`.
        assert!(with(Kind::Predicate, &[6, 0, 0, 10, 5, 0, 0, 1, 2]));
        assert!(!with(Kind::Predicate, &[6, 0, 9, 10, 5, 0, 0, 1, 2]));
        // `PredicateKind`'s last variant is `NormalizesTo`: `AliasRelate` is gone.
        assert!(with(Kind::Predicate, &[5, 2]));
        assert!(!with(Kind::Predicate, &[7, 2]));
        // `UnstableFeature(name)`: the name's length, its bytes four to a word, zero-padded.
        let feature = |words: &[u32]| {
            let mut predicate = vec![0, 8];
            predicate.extend_from_slice(words);
            predicate.push(2);
            with(Kind::Predicate, predicate.as_slice())
        };
        assert!(feature(&[0]));
        assert!(feature(&[1, u32::from(b'a')]));
        assert!(feature(&[2, u32::from_le_bytes([0xC3, 0xA9, 0, 0])]), "`é`");
        assert!(!feature(&[1, u32::from_le_bytes([b'a', b'a', 0, 0])]), "padding that is not zero");
        assert!(!feature(&[1, 0xC3]), "half of `é`, not UTF-8");
        assert!(!feature(&[5, u32::from_le_bytes(*b"abcd")]), "a name longer than its words");
        // `fn() -> bool`: its ABI, safety, C-variadic flag and splatted argument, of no input.
        assert!(with(Kind::Ty, &[14, 3, 2, 0, 1, 0, u32::from(u8::MAX)]));
        assert!(!with(Kind::Ty, &[14, 3, 2, 0, 1, 0, 0]), "a splatted argument past the inputs");
        assert!(!with(Kind::Ty, &[14, 3, 2, 0, 1, 0, 256]), "a splatted argument past `u8`");
    }
}
