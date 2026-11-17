//! The answers: [`Answer`], one variant per question, and [`Vocab`], the back end's own types
//! some answers are of.

use blaze_ids::ContentId;
use blaze_typeck::{Adt, FnSig, ItemKey, PTy, PTys};

use crate::digest::Digest;
use crate::fact::{FactKind, Term};

/// The back end's types that answers are of and that this crate does not define: blaze-build's
/// (`ReprAttrs`, `Abi`, `Intrinsic`, `ObjectTrait`, `DataRecord`, ..), blaze-consteval's
/// (`FnKind`, `Typed`), blaze-flow-lower's (`StateRecipe`). One implementation, by the crate
/// that owns the types (blaze-build's `BuildVocab`, the port plan's step 1), names each once:
/// each type keeps its one home (rule 30) and this crate stays below the crates that ask.
/// Each is a [`Digest`], so a read list covers it by content.
pub trait Vocab {
    /// An ADT's `repr` attributes.
    type Repr: Copy + Digest;
    /// An ADT's valid-range attributes.
    type ValidRange: Copy + Digest;
    /// The ABI a fn is declared with.
    type Abi: Copy + Digest;
    /// A compiler intrinsic the build makes in place.
    type Intrinsic: Copy + Digest;
    /// A `static` item's kind.
    type StaticItem: Copy + Digest;
    /// What a call of a fn item does, to the evaluator.
    type FnKind: Copy + Digest;
    /// A trait as a trait object sees it.
    type ObjectTrait<'w>: Copy + Digest;
    /// A checked body, as the evaluator reads it.
    type Body<'w>: Digest;
    /// One piece of a `format_args!` template.
    type FmtPiece<'w>: Copy + Digest;
    /// Where an expression is, item-relative.
    type CallerSite<'w>: Copy + Digest;
    /// A closure's facts.
    type Closure<'w>: Copy + Digest;
    /// An async block's facts.
    type Coroutine<'w>: Copy + Digest;
    /// An async block's state recipe (or the refusal of its flow).
    type Recipe<'w>: Digest;
    /// A coroutine instance's state: its shape and its tag's niche.
    type CoroutineState: Copy + Digest;
    /// An evaluated constant's record.
    type Data<'w>: Digest;
    /// The build's panic strategy and overflow checks.
    type Profile: Copy + Digest;
}

/// A fact the world does not know, on purpose: it writes this variant and why, so a forgotten
/// answer cannot pass as "none". The ask that met it is an error naming the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unknown {
    pub why: &'static str,
}

/// A literal's value, as written (escapes processed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Literal<'w> {
    /// An integer's magnitude, unsigned (a negative literal is a negation of it; a `NegLit`
    /// pattern's the builder negates).
    Int(u128),
    /// A float's text rounded once to the nearest `f64` and once to the nearest `f32`: the
    /// reader picks by the type the check gave it.
    Float { f64: u64, f32: u32 },
    /// A string literal's text.
    Str(&'w str),
    /// A byte string's bytes (a C string's without its nul).
    Bytes(&'w [u8]),
}

/// An instance in the world's own table: what [`crate::Fact::Resolve`] and
/// [`crate::Fact::KeyedInstance`] answer. The reader brings `args` into its ground table.
#[derive(Clone, Copy, Debug)]
pub struct Resolved<'w> {
    pub item: ItemKey,
    pub args: &'w [PTy],
    pub types: PTys<'w>,
}

/// The answer to one [`crate::Fact`], of the variant of the same name, or [`Answer::Unknown`]
/// or [`Answer::Missing`] for any. `Reads::ask` refuses an answer of another variant
/// (`FactError::WrongAnswer`), so a world's slip is a typed error at its first ask.
pub enum Answer<'w, V: Vocab> {
    Adt(Option<Adt<'w>>),
    Repr(V::Repr),
    ValidRange(V::ValidRange),
    Discriminant(Option<i128>),
    DiscriminantBody(Option<ItemKey>),
    Sig(Option<FnSig<'w>>),
    Abi(V::Abi),
    Nounwind(bool),
    CVariadic(Option<u32>),
    LegacyConstGenerics(&'w [u32]),
    Intrinsic(Option<V::Intrinsic>),
    Static(Option<V::StaticItem>),
    FnKind(V::FnKind),
    CodeId(ContentId),
    CtorOf(Option<(ItemKey, u32)>),
    OpaqueOf(Option<(ItemKey, u32)>),
    ObjectTrait(V::ObjectTrait<'w>),
    ItemPath(&'w str),
    NameText(&'w str),
    ConstBody(Option<V::Body<'w>>),
    Lang(Option<ItemKey>),
    LangMember(Option<ItemKey>),
    Literal(Literal<'w>),
    FormatTemplate(&'w [V::FmtPiece<'w>]),
    CallerSite(V::CallerSite<'w>),
    Closure(V::Closure<'w>),
    Coroutine(V::Coroutine<'w>),
    CoroutineRecipe(V::Recipe<'w>),
    /// The normalized type, in the world's table, holding no projection.
    Normalize(Term<'w>),
    /// The hidden type, in the world's table, holding no opaque type.
    OpaqueHidden(Term<'w>),
    DropImpl(Option<ItemKey>),
    Selects(bool),
    Resolve(Resolved<'w>),
    TrackCaller(bool),
    InstanceKey(ContentId),
    KeyedInstance(Resolved<'w>),
    CoroutineState(V::CoroutineState),
    Value(V::Data<'w>),
    Profile(V::Profile),
    /// The world does not know this fact, and says why (any question).
    Unknown(Unknown),
    /// The record that answers it is not made yet (a constant not evaluated, a body not
    /// checked, a coroutine not split): the ask is a hole the caller fills and asks again.
    Missing,
}

impl<V: Vocab> Answer<'_, V> {
    /// The question this answers; `None` for [`Answer::Unknown`] and [`Answer::Missing`],
    /// which answer any.
    pub const fn kind(&self) -> Option<FactKind> {
        Some(match self {
            Answer::Adt(_) => FactKind::Adt,
            Answer::Repr(_) => FactKind::Repr,
            Answer::ValidRange(_) => FactKind::ValidRange,
            Answer::Discriminant(_) => FactKind::Discriminant,
            Answer::DiscriminantBody(_) => FactKind::DiscriminantBody,
            Answer::Sig(_) => FactKind::Sig,
            Answer::Abi(_) => FactKind::Abi,
            Answer::Nounwind(_) => FactKind::Nounwind,
            Answer::CVariadic(_) => FactKind::CVariadic,
            Answer::LegacyConstGenerics(_) => FactKind::LegacyConstGenerics,
            Answer::Intrinsic(_) => FactKind::Intrinsic,
            Answer::Static(_) => FactKind::Static,
            Answer::FnKind(_) => FactKind::FnKind,
            Answer::CodeId(_) => FactKind::CodeId,
            Answer::CtorOf(_) => FactKind::CtorOf,
            Answer::OpaqueOf(_) => FactKind::OpaqueOf,
            Answer::ObjectTrait(_) => FactKind::ObjectTrait,
            Answer::ItemPath(_) => FactKind::ItemPath,
            Answer::NameText(_) => FactKind::NameText,
            Answer::ConstBody(_) => FactKind::ConstBody,
            Answer::Lang(_) => FactKind::Lang,
            Answer::LangMember(_) => FactKind::LangMember,
            Answer::Literal(_) => FactKind::Literal,
            Answer::FormatTemplate(_) => FactKind::FormatTemplate,
            Answer::CallerSite(_) => FactKind::CallerSite,
            Answer::Closure(_) => FactKind::Closure,
            Answer::Coroutine(_) => FactKind::Coroutine,
            Answer::CoroutineRecipe(_) => FactKind::CoroutineRecipe,
            Answer::Normalize(_) => FactKind::Normalize,
            Answer::OpaqueHidden(_) => FactKind::OpaqueHidden,
            Answer::DropImpl(_) => FactKind::DropImpl,
            Answer::Selects(_) => FactKind::Selects,
            Answer::Resolve(_) => FactKind::Resolve,
            Answer::TrackCaller(_) => FactKind::TrackCaller,
            Answer::InstanceKey(_) => FactKind::InstanceKey,
            Answer::KeyedInstance(_) => FactKind::KeyedInstance,
            Answer::CoroutineState(_) => FactKind::CoroutineState,
            Answer::Value(_) => FactKind::Value,
            Answer::Profile(_) => FactKind::Profile,
            Answer::Unknown(_) | Answer::Missing => return None,
        })
    }
}
