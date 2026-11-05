/// Lang items used by the new trait solver. This can be mapped to whatever internal
/// representation of `LangItem`s used in the underlying compiler implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SolverProjectionLangItem {
    // tidy-alphabetical-start
    AsyncFnKindUpvars,
    AsyncFnOnceOutput,
    CallOnceFuture,
    CallRefFuture,
    CoroutineReturn,
    CoroutineYield,
    FieldBase,
    FieldType,
    FutureOutput,
    Metadata,
    /// blaze: `<V as core::reflect::Variant<E>>::View`, the variant view `VariantOf<E, v>`.
    ReflectVariantView,
    // tidy-alphabetical-end
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SolverAdtLangItem {
    // tidy-alphabetical-start
    DynMetadata,
    Option,
    OwnedBox,
    Poll,
    // tidy-alphabetical-end
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SolverTraitLangItem {
    // tidy-alphabetical-start
    /// blaze: the arbitrary-width integers (`u3`, `i17`), a marker the solver answers.
    ArbitraryInt,
    /// blaze: the signed arbitrary-width integers.
    ArbitrarySigned,
    AsyncFn,
    AsyncFnKindHelper,
    AsyncFnMut,
    AsyncFnOnce,
    AsyncIterator,
    BikeshedGuaranteedNoDrop,
    Clone,
    Copy,
    Coroutine,
    Destruct,
    DiscriminantKind,
    Drop,
    Field,
    Fn,
    FnMut,
    FnOnce,
    FnPtrTrait,
    Freeze,
    FusedIterator,
    Future,
    /// blaze: `Self: IntFrom<S>`, a lossless conversion between two integer types, one of them
    /// arbitrary-width.
    IntFrom,
    /// blaze: `Self: IntTryFrom<S>`, a fallible conversion between two integer types, one of
    /// them arbitrary-width.
    IntTryFrom,
    Iterator,
    MetaSized,
    PointeeSized,
    PointeeTrait,
    /// blaze: `core::reflect::Enum` (`#![feature(blaze_comptime)]`): every enum.
    ReflectEnum,
    /// blaze: `F: core::reflect::Field<C>`: `F` is the field representing type of a field of
    /// the struct, tuple or variant view `C`.
    ReflectField,
    /// blaze: `core::reflect::FnItem`: fn item and closure types.
    ReflectFnItem,
    /// blaze: `core::reflect::Struct`: a struct whose fields reflection may read, a tuple, or
    /// a variant view.
    ReflectStruct,
    /// blaze: `V: core::reflect::Variant<E>`: `V` is the variant representing type of a variant
    /// of the enum `E`.
    ReflectVariant,
    Sized,
    TransmuteTrait,
    TrivialClone,
    TryAsDyn,
    Tuple,
    Unpin,
    Unsize,
    // tidy-alphabetical-end
}
