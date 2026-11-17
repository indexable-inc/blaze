//! The questions: [`Fact`], one variant per question, and the handles a question at ground
//! arguments carries ([`Ground`], [`Instance`], [`Term`]).

use blaze_ids::ContentId;
use blaze_typeck::{ExprId, ItemKey, Name, PTy, PTys, PatId};

/// A type of a table, read in place: a question's ground argument, or an answer's type in the
/// world's own table. Its digest is its content (the term it reaches), never its index.
#[derive(Clone, Copy, Debug)]
pub struct Term<'t> {
    pub ty: PTy,
    pub types: PTys<'t>,
}

/// Ground arguments: types of `types` with no parameter, no hole and no inference variable.
/// The questions at an instance carry them, so one world serves the lazy front end and the
/// whole program alike, and the question's key is their content.
#[derive(Clone, Copy, Debug)]
pub struct Ground<'t> {
    pub args: &'t [PTy],
    pub types: PTys<'t>,
}

impl Ground<'static> {
    /// No arguments: a free item's.
    pub const NONE: Ground<'static> = Ground { args: &[], types: PTys::EMPTY };
}

/// A fn item at ground arguments: what is built, called, or tracked.
#[derive(Clone, Copy, Debug)]
pub struct Instance<'t> {
    pub item: ItemKey,
    pub args: Ground<'t>,
}

/// Where a literal is in its owner's body: an expression or a pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LitAt {
    Expr(ExprId),
    Pat(PatId),
}

/// What constant a value is of: a const item (a trait's or an impl's included, at the path's
/// arguments), a `const { .. }` block of a body, or a promoted borrow `&expr` of a body
/// (`Checked::promoted`). One question: each is a body the evaluator ran at ground arguments,
/// and the builder reads each from the same record form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConstOf {
    Item(ItemKey),
    Block { owner: ItemKey, expr: ExprId },
    Promoted { owner: ItemKey, expr: ExprId },
}

/// A question the evaluator or the builder asks of the front end. Exhaustive: a new question
/// is a new variant here and in [`FactKind`] and [`crate::Answer`], and every world's `match`
/// then fails to compile until it answers it (no default, no silent "none").
///
/// The crate doc's inventory says which of today's trait methods each variant replaces.
#[derive(Clone, Copy, Debug)]
pub enum Fact<'q> {
    // ---- Items: facts of an item's text, read before any body is built.
    /// An ADT's definition.
    Adt(ItemKey),
    /// An ADT's `repr` attributes.
    Repr(ItemKey),
    /// An ADT's `#[rustc_layout_scalar_valid_range_*]`.
    ValidRange(ItemKey),
    /// The evaluated explicit discriminant `= n` of a variant (`None`: not written).
    Discriminant { adt: ItemKey, variant: u32 },
    /// The anonymous constant that is a variant's explicit discriminant (`None`: not written).
    DiscriminantBody { adt: ItemKey, variant: u32 },
    /// A fn item's signature.
    Sig(ItemKey),
    /// The ABI a fn item is declared with.
    Abi(ItemKey),
    /// Whether a fn item may not unwind (a non-unwinding ABI, or `#[rustc_nounwind]`).
    Nounwind(ItemKey),
    /// A C-variadic foreign fn's declared parameter count (`None`: not variadic).
    CVariadic(ItemKey),
    /// `#[rustc_legacy_const_generics(i, ..)]`'s positions (empty: none).
    LegacyConstGenerics(ItemKey),
    /// Which compiler intrinsic a fn item is (`None`: not one).
    Intrinsic(ItemKey),
    /// Which kind of `static` an item is (`None`: not a static).
    Static(ItemKey),
    /// What a call of a fn item does, as the evaluator reads it.
    FnKind(ItemKey),
    /// An item's code identity (its text's id): part of every record key.
    CodeId(ItemKey),
    /// The ADT and variant a ctor key builds (`None`: not a ctor).
    CtorOf(ItemKey),
    /// The fn whose head made an opaque type, and its index among that fn's.
    OpaqueOf(ItemKey),
    /// A trait as a trait object sees it.
    ObjectTrait(ItemKey),
    /// An item's full path, as `type_name` and messages print it.
    ItemPath(ItemKey),
    /// The text of a name (an associated type's, in `dyn Iterator<Item = u8>`).
    NameText(Name),
    /// The checked body of a const, a static, a `const fn` or an anonymous constant.
    ConstBody(ItemKey),

    // ---- The library: items the build names by their `#[lang = ".."]` mark.
    /// The item marked `#[lang = "<name>"]` (`None`: the program's crates mark none).
    Lang(Name),
    /// A member of a marked item: a trait's method (`Fn::call`), an ADT's inherent fn
    /// (`Argument::new_display`), an enum's variant (`Poll::Ready`), by name.
    LangMember { lang: Name, member: Name },

    // ---- A body's own facts, by its owner and an id of the body.
    /// A literal's value.
    Literal { owner: ItemKey, at: LitAt },
    /// A `format_args!` expression's template.
    FormatTemplate { owner: ItemKey, expr: ExprId },
    /// Where an expression is, item-relative (rule 71).
    CallerSite { owner: ItemKey, expr: ExprId },
    /// A closure's kind, signature and environment, in its owner's generic parameters.
    Closure { owner: ItemKey, ordinal: u32 },
    /// An async block's upvars and output, in its owner's generic parameters.
    Coroutine { owner: ItemKey, ordinal: u32 },
    /// An async block's state recipe, made with its owner's check.
    CoroutineRecipe { owner: ItemKey, ordinal: u32 },

    // ---- At ground arguments.
    /// `<args[0] as trait_<args[1..]>>::name`, normalized.
    Normalize { trait_: ItemKey, name: Name, args: Ground<'q> },
    /// The hidden type of `impl Trait` type `opaque` at its arguments.
    OpaqueHidden { opaque: ItemKey, args: Ground<'q> },
    /// A ground type's `Drop` impl's `drop` fn (`None`: no impl).
    DropImpl(Term<'q>),
    /// Whether trait `trait_` has an impl for a ground type.
    Selects { trait_: ItemKey, ty: Term<'q> },
    /// The instance that runs for an associated fn or const at ground arguments (impl
    /// selection); an inherent item resolves to itself.
    Resolve(Instance<'q>),
    /// Whether the fn an instance resolves to is `#[track_caller]`.
    TrackCaller(Instance<'q>),
    /// The key the program gives an instance.
    InstanceKey(Instance<'q>),
    /// The instance a key names (the inverse of [`Fact::InstanceKey`]).
    KeyedInstance(ContentId),
    /// The state of coroutine `ordinal` of `owner` at the owner's ground arguments: its shape
    /// and its tag's niche, from one record.
    CoroutineState { owner: ItemKey, ordinal: u32, args: Ground<'q> },
    /// The evaluated value of a constant at ground arguments.
    Value { of: ConstOf, args: Ground<'q> },

    // ---- The build.
    /// What a panic does and whether integer overflow is checked: part of every code record's
    /// key.
    Profile,
}

/// Which question a [`Fact`] is: its variant, without its arguments. A read list's rows and an
/// answer's check are by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FactKind {
    Adt,
    Repr,
    ValidRange,
    Discriminant,
    DiscriminantBody,
    Sig,
    Abi,
    Nounwind,
    CVariadic,
    LegacyConstGenerics,
    Intrinsic,
    Static,
    FnKind,
    CodeId,
    CtorOf,
    OpaqueOf,
    ObjectTrait,
    ItemPath,
    NameText,
    ConstBody,
    Lang,
    LangMember,
    Literal,
    FormatTemplate,
    CallerSite,
    Closure,
    Coroutine,
    CoroutineRecipe,
    Normalize,
    OpaqueHidden,
    DropImpl,
    Selects,
    Resolve,
    TrackCaller,
    InstanceKey,
    KeyedInstance,
    CoroutineState,
    Value,
    Profile,
}

impl Fact<'_> {
    /// Which question this is.
    pub const fn kind(&self) -> FactKind {
        match self {
            Fact::Adt(_) => FactKind::Adt,
            Fact::Repr(_) => FactKind::Repr,
            Fact::ValidRange(_) => FactKind::ValidRange,
            Fact::Discriminant { .. } => FactKind::Discriminant,
            Fact::DiscriminantBody { .. } => FactKind::DiscriminantBody,
            Fact::Sig(_) => FactKind::Sig,
            Fact::Abi(_) => FactKind::Abi,
            Fact::Nounwind(_) => FactKind::Nounwind,
            Fact::CVariadic(_) => FactKind::CVariadic,
            Fact::LegacyConstGenerics(_) => FactKind::LegacyConstGenerics,
            Fact::Intrinsic(_) => FactKind::Intrinsic,
            Fact::Static(_) => FactKind::Static,
            Fact::FnKind(_) => FactKind::FnKind,
            Fact::CodeId(_) => FactKind::CodeId,
            Fact::CtorOf(_) => FactKind::CtorOf,
            Fact::OpaqueOf(_) => FactKind::OpaqueOf,
            Fact::ObjectTrait(_) => FactKind::ObjectTrait,
            Fact::ItemPath(_) => FactKind::ItemPath,
            Fact::NameText(_) => FactKind::NameText,
            Fact::ConstBody(_) => FactKind::ConstBody,
            Fact::Lang(_) => FactKind::Lang,
            Fact::LangMember { .. } => FactKind::LangMember,
            Fact::Literal { .. } => FactKind::Literal,
            Fact::FormatTemplate { .. } => FactKind::FormatTemplate,
            Fact::CallerSite { .. } => FactKind::CallerSite,
            Fact::Closure { .. } => FactKind::Closure,
            Fact::Coroutine { .. } => FactKind::Coroutine,
            Fact::CoroutineRecipe { .. } => FactKind::CoroutineRecipe,
            Fact::Normalize { .. } => FactKind::Normalize,
            Fact::OpaqueHidden { .. } => FactKind::OpaqueHidden,
            Fact::DropImpl(_) => FactKind::DropImpl,
            Fact::Selects { .. } => FactKind::Selects,
            Fact::Resolve(_) => FactKind::Resolve,
            Fact::TrackCaller(_) => FactKind::TrackCaller,
            Fact::InstanceKey(_) => FactKind::InstanceKey,
            Fact::KeyedInstance(_) => FactKind::KeyedInstance,
            Fact::CoroutineState { .. } => FactKind::CoroutineState,
            Fact::Value { .. } => FactKind::Value,
            Fact::Profile => FactKind::Profile,
        }
    }
}
