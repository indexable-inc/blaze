//! The words of the schema, each defined once for every crate of the item layer (rule 30):
//! what an item is, a namespace, an edition, a visibility as written and as resolved, its
//! class, a binding's target, origin and state, rustc's ambiguity kinds, a macro's call shape,
//! where an item is written, an index among a file's items and a byte range.
//!
//! blaze-file-items, blaze-scope, blaze-impls and blaze-expand import these; none defines its
//! own.

use core::fmt;

use blaze_format::FormatError;
use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};

use crate::column::at_of;

/// A fieldless `#[repr(u8)]` enum that is a record field: one byte, checked when a buffer is
/// opened, so a byte no variant has is refused by name there and never read as a value.
macro_rules! byte_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident, refused as $what:literal {
            $( $(#[$variant_meta:meta])* $variant:ident = $byte:literal => $text:literal ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        #[repr(u8)]
        $vis enum $name {
            $( $(#[$variant_meta])* $variant = $byte, )*
        }

        impl $name {
            /// Every value, in byte order: an array, so `for x in T::ALL` yields values.
            $vis const ALL: [$name; [$( $name::$variant, )*].len()] = [$( $name::$variant, )*];

            /// The value whose byte is `byte`, if any.
            $vis const fn from_byte(byte: u8) -> Option<$name> {
                match byte {
                    $( $byte => Some($name::$variant), )*
                    _ => None,
                }
            }

            /// Its byte, as a record holds it.
            #[expect(blaze::as_cast, reason = "a fieldless `#[repr(u8)]` enum's discriminant is its byte; the one cast of the type")]
            $vis const fn byte(self) -> u8 {
                self as u8
            }

            /// Its name, as an error or a report prints it.
            $vis const fn name(self) -> &'static str {
                match self {
                    $( $name::$variant => $text, )*
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.name())
            }
        }

        // SAFETY: a value is its one byte, the same on every host and in every snapshot; it
        // names nothing interned.
        unsafe impl Portable for $name {}

        // SAFETY: `#[repr(u8)]` and fieldless: one byte, no padding, no reference. `check`
        // accepts exactly the bytes of the declared variants, so every byte it accepts is a
        // valid value; `PLAIN` and `SHALLOW_PLAIN` stay false, so no byte is read unchecked.
        unsafe impl blaze_format::Record for $name {
            const SCHEMA: blaze_format::SchemaHash = blaze_format::schema_of(&[
                blaze_format::Part::Str(::core::stringify!($name)),
                blaze_format::Part::U64(1),
                $(
                    blaze_format::Part::Str(::core::stringify!($variant)),
                    blaze_format::Part::U64($byte),
                )*
            ]);

            fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
                match bytes {
                    [byte] if $name::from_byte(*byte).is_some() => Ok(()),
                    _ => Err(FormatError::Invalid { at: at_of(bytes, buffer), what: $what }),
                }
            }
        }
    };
}

pub(crate) use byte_enum;

byte_enum! {
    /// What an entry of a file's items is: an item, a variant, a field, an item-position macro
    /// call, or a stretch the parser could not read. The kind's own data (a `mod`'s
    /// `#[path]`, a field's type, ...) is beside it in the entry (`crate::ItemDetail`).
    pub enum ItemKind, refused as "a byte that is no item kind" {
        Function = 0 => "function",
        Struct = 1 => "struct",
        Enum = 2 => "enum",
        Union = 3 => "union",
        Trait = 4 => "trait",
        Impl = 5 => "impl",
        Const = 6 => "const",
        Static = 7 => "static",
        TypeAlias = 8 => "type alias",
        Module = 9 => "module",
        /// `macro_rules! name { .. }`.
        MacroRules = 10 => "macro_rules",
        Use = 11 => "use",
        ExternCrate = 12 => "extern crate",
        Variant = 13 => "variant",
        Field = 14 => "field",
        /// `macro name(..) { .. }`, macros 2.0.
        Macro2 = 15 => "macro",
        /// `trait A = B + C;`.
        TraitAlias = 16 => "trait alias",
        /// `extern "abi" { .. }`: its foreign items are its children.
        ExternBlock = 17 => "extern block",
        /// A macro call in item position.
        MacroCall = 18 => "macro call",
        /// An item-level stretch the parser could not read as an item.
        Error = 19 => "error",
        /// An item-level node of a kind the outline does not take apart
        /// (`builtin # global_asm(..)`): listed, never dropped.
        Unhandled = 20 => "unhandled item",
    }
}

byte_enum! {
    /// Which of a module's three name tables a name is in. A unit or tuple struct is in two,
    /// and is two entries of a scope. The order is the order a scope is sorted in.
    pub enum Namespace, refused as "a byte that is no namespace" {
        Types = 0 => "types",
        Values = 1 => "values",
        Macros = 2 => "macros",
    }
}

impl Namespace {
    /// Its place in [`Namespace::ALL`].
    pub const fn index(self) -> usize {
        match self {
            Namespace::Types => 0,
            Namespace::Values => 1,
            Namespace::Macros => 2,
        }
    }
}

byte_enum! {
    /// A crate's edition: what its root record says, what a file is parsed as, and what a
    /// hygiene root of an expansion is of.
    pub enum Edition, refused as "a byte that is no edition" {
        E2015 = 0 => "2015",
        E2018 = 1 => "2018",
        E2021 = 2 => "2021",
        E2024 = 3 => "2024",
    }
}

byte_enum! {
    /// Which macro sub-namespace a macro is in, and so which calls it takes: a path written
    /// `m!()` finds only function-like macros, `#[m]` only attribute macros, `#[derive(m)]`
    /// only derives (rustc's `sub_namespace_match`).
    pub enum CallShape, refused as "a byte that is no macro call shape" {
        /// `m!(..)`, `m![..]`, `m! {..}`.
        FnLike = 0 => "function-like",
        /// `#[m]` on an item.
        Attr = 1 => "attribute",
        /// `#[derive(m)]`.
        Derive = 2 => "derive",
    }
}

byte_enum! {
    /// What a macro call is to the item it is written on (attr-bang-kind-2). rustc takes an
    /// item's derive first, then resolves each later one-segment attribute with the derive's
    /// helpers innermost (`InvocationCollector::take_first_attr`): a helper is inert, an
    /// attribute macro takes the item. The lowering cannot tell which, so it sends both kinds of
    /// call below and the cells decide.
    pub enum CallRole, refused as "a byte that is no macro call role" {
        /// Any other call: answered by its path alone.
        Call = 0 => "call",
        /// A one-segment attribute written after a derive on the item itself: inert as a
        /// derive's helper; an attribute macro takes the item unless one written before it did.
        AfterDerive = 1 => "after derive",
        /// The item itself, kept after its `AfterDerive` calls: lowered when none took it.
        Keep = 2 => "keep",
        /// An `AfterDerive` call with a `#[derive]` written after it on the same item: when it
        /// names an attribute macro the call is refused (`Unresolved::DeriveAfterAttrMacro`),
        /// since that derive would run on the item here and again on the macro's output.
        /// Lowering takes the item's attributes in written order (`AttrOrder::Written`), so only
        /// the planted old order (`AttrOrder::DerivesAtOnce`) reaches this refusal, kept as that control's oracle.
        AfterDeriveBeforeDerive = 3 => "after derive, before a derive",
        /// A proc-macro crate's harness (blaze-scope task/lower.rs `Lower::proc_macro_harness`,
        /// D175): no macro and no item; its input is the `_DECLS` static the lowering wrote,
        /// lowered as its expansion, declaring no name (rustc's def-site `_DECLS`).
        ProcMacroHarness = 4 => "proc-macro harness",
    }
}

/// The call shapes a macro takes: never empty. A `macro_rules!` takes each shape one of its
/// arms is written for (`attr(..)` arms: attribute, `derive(..)` arms: derive, plain arms:
/// function-like, as the pinned nightly reads them); a builtin or procedural macro takes one.
/// A lookup by shape asks [`MacroShapes::accepts`] (rustc's `sub_namespace_match` over a set).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct MacroShapes(u8);

impl MacroShapes {
    pub const FN_LIKE: MacroShapes = MacroShapes::of(CallShape::FnLike);
    pub const ATTR: MacroShapes = MacroShapes::of(CallShape::Attr);
    pub const DERIVE: MacroShapes = MacroShapes::of(CallShape::Derive);

    /// The set of `shape` alone.
    pub const fn of(shape: CallShape) -> MacroShapes {
        MacroShapes(1 << shape.byte())
    }

    /// This set and `shape`.
    pub const fn with(self, shape: CallShape) -> MacroShapes {
        MacroShapes(self.0 | MacroShapes::of(shape).0)
    }

    /// Whether a call of `shape` may name this macro.
    pub const fn accepts(self, shape: CallShape) -> bool {
        self.0 & MacroShapes::of(shape).0 != 0
    }

    /// Its byte, as a record holds it: a bit per shape, `1 << CallShape::byte()`.
    pub const fn byte(self) -> u8 {
        self.0
    }

    /// The set whose byte is `byte`: one to seven (a non-empty set of the three shapes).
    pub const fn from_byte(byte: u8) -> Option<MacroShapes> {
        match byte {
            1..=7 => Some(MacroShapes(byte)),
            _ => None,
        }
    }

    /// Its shapes, in [`CallShape`] order.
    pub fn shapes(self) -> impl Iterator<Item = CallShape> {
        CallShape::ALL.into_iter().filter(move |&s| self.accepts(s))
    }
}

impl fmt::Display for MacroShapes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (n, shape) in self.shapes().enumerate() {
            if n > 0 {
                f.write_str(", ")?;
            }
            f.write_str(shape.name())?;
        }
        Ok(())
    }
}

// SAFETY: one byte, the same on every host and in every snapshot; it names nothing interned.
unsafe impl Portable for MacroShapes {}

// SAFETY: `#[repr(transparent)]` over a `u8`: one byte, no padding, no reference. `check`
// accepts exactly the bytes `from_byte` takes, so every byte it accepts is a valid value;
// `PLAIN` and `SHALLOW_PLAIN` stay false, so no byte is read unchecked.
unsafe impl blaze_format::Record for MacroShapes {
    const SCHEMA: blaze_format::SchemaHash = blaze_format::schema_of(&[
        blaze_format::Part::Str("MacroShapes"),
        blaze_format::Part::U64(1),
        blaze_format::Part::Str("bit per CallShape"),
    ]);

    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        match bytes {
            [byte] if MacroShapes::from_byte(*byte).is_some() => Ok(()),
            _ => Err(FormatError::Invalid { at: at_of(bytes, buffer), what: "a byte that is no set of macro call shapes" }),
        }
    }
}

/// What a binding's target is, as far as resolving a path through it needs: a module or an
/// enum can be descended into, a macro has the call shapes it takes, anything else ends a path.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum DefKind {
    Module,
    Enum,
    Trait,
    Macro(MacroShapes),
    Other,
}

byte_enum! {
    /// [`DefKind`]'s byte in a record; a macro's [`MacroShapes`] is a field beside it.
    pub(crate) enum DefKindTag, refused as "a byte that is no definition kind" {
        Module = 0 => "module",
        Enum = 1 => "enum",
        Trait = 2 => "trait",
        Macro = 3 => "macro",
        Other = 4 => "other",
    }
}

impl DefKind {
    /// Its bytes as a record holds them: the shapes are `FN_LIKE` unless a macro, so two
    /// builders write equal bytes (a zero byte for "no macro" would need every row's reader to
    /// check the pair; the set type keeps the field one checked byte).
    pub(crate) fn to_record(self) -> (DefKindTag, MacroShapes) {
        match self {
            DefKind::Module => (DefKindTag::Module, MacroShapes::FN_LIKE),
            DefKind::Enum => (DefKindTag::Enum, MacroShapes::FN_LIKE),
            DefKind::Trait => (DefKindTag::Trait, MacroShapes::FN_LIKE),
            DefKind::Macro(shapes) => (DefKindTag::Macro, shapes),
            DefKind::Other => (DefKindTag::Other, MacroShapes::FN_LIKE),
        }
    }

    /// What [`DefKind::to_record`] wrote.
    pub(crate) fn from_record(tag: DefKindTag, shapes: MacroShapes) -> DefKind {
        match tag {
            DefKindTag::Module => DefKind::Module,
            DefKindTag::Enum => DefKind::Enum,
            DefKindTag::Trait => DefKind::Trait,
            DefKindTag::Macro => DefKind::Macro(shapes),
            DefKindTag::Other => DefKind::Other,
        }
    }
}

// SAFETY: a fieldless choice and a call shape, both `Portable`.
unsafe impl Portable for DefKind {}

/// A binding's target: the item's key and its kind.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Def {
    pub key: ItemKey,
    pub kind: DefKind,
}

impl Def {
    /// A module's def.
    pub fn module(key: ItemKey) -> Def {
        Def { key, kind: DefKind::Module }
    }

    /// Whether a path can go on through it (a module or an enum).
    pub fn has_scope(&self) -> bool {
        matches!(self.kind, DefKind::Module | DefKind::Enum)
    }
}

// SAFETY: an item key and a `Portable` kind.
unsafe impl Portable for Def {}

/// A resolved visibility: who may name a binding.
///
/// `pub(self)`, `pub(super)`, `pub(in path)` and no visibility at all are `Restricted` to the
/// module they name (a private item's own module); a restriction to the crate root is written
/// `Crate`, so one visibility has one form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Visibility {
    /// `pub`: anyone.
    Public,
    /// `pub(crate)`: the modules of this crate.
    Crate(CrateItemKey),
    /// The module with this key and the modules under it.
    Restricted(ItemKey),
}

byte_enum! {
    /// [`Visibility`]'s byte in a record; the crate or module it names is a field beside it.
    pub(crate) enum VisibilityTag, refused as "a byte that is no visibility" {
        Public = 0 => "pub",
        Crate = 1 => "pub(crate)",
        Restricted = 2 => "pub(in ..)",
    }
}

impl Visibility {
    /// The class of this visibility, which a scope's `Exports` digests group its bindings by.
    pub fn class(self) -> VisibilityClass {
        match self {
            Visibility::Public => VisibilityClass::Public,
            Visibility::Crate(_) => VisibilityClass::Crate,
            Visibility::Restricted(module) => VisibilityClass::Restricted(module),
        }
    }

    /// Its byte and key as a record holds them: zeros for `pub`, so two builders write equal
    /// bytes.
    pub(crate) fn to_record(self) -> (VisibilityTag, ContentId) {
        match self {
            Visibility::Public => (VisibilityTag::Public, ContentId([0; 32])),
            Visibility::Crate(krate) => (VisibilityTag::Crate, krate.0),
            Visibility::Restricted(module) => (VisibilityTag::Restricted, module.0),
        }
    }

    /// What [`Visibility::to_record`] wrote.
    pub(crate) fn from_record(tag: VisibilityTag, key: ContentId) -> Visibility {
        match tag {
            VisibilityTag::Public => Visibility::Public,
            VisibilityTag::Crate => Visibility::Crate(CrateItemKey(key)),
            VisibilityTag::Restricted => Visibility::Restricted(ItemKey(key)),
        }
    }
}

// SAFETY: a tag and a crate or item key: `Portable` both.
unsafe impl Portable for Visibility {}

/// A class of visibility, the unit a glob reads a module's names in: a reader asks for the
/// classes it can see, so a name added in a class it cannot see moves none of its reads.
///
/// - `Public`: every reader.
/// - `Crate`: readers in the module's crate.
/// - `Restricted(r)`: readers under `r`, an ancestor of the module or the module itself (a
///   private item is `Restricted` to its own module). A reader outside `r` never asks for it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum VisibilityClass {
    Public,
    Crate,
    Restricted(ItemKey),
}

impl VisibilityClass {
    /// Its byte and module as a record holds them (zeros unless restricted).
    pub(crate) fn to_record(self) -> (VisibilityTag, ContentId) {
        match self {
            VisibilityClass::Public => (VisibilityTag::Public, ContentId([0; 32])),
            VisibilityClass::Crate => (VisibilityTag::Crate, ContentId([0; 32])),
            VisibilityClass::Restricted(module) => (VisibilityTag::Restricted, module.0),
        }
    }

    /// What [`VisibilityClass::to_record`] wrote.
    pub(crate) fn from_record(tag: VisibilityTag, key: ContentId) -> VisibilityClass {
        match tag {
            VisibilityTag::Public => VisibilityClass::Public,
            VisibilityTag::Crate => VisibilityClass::Crate,
            VisibilityTag::Restricted => VisibilityClass::Restricted(ItemKey(key)),
        }
    }
}

// SAFETY: a tag and, when restricted, an item key.
unsafe impl Portable for VisibilityClass {}

/// A visibility as written, before it is resolved: what a file's items hold (`P` a [`Range`]
/// of the path's text, item-relative) and what a module's declarations hold (`P` the path as
/// names). Resolving it against the module's chain gives a [`Visibility`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum WrittenVis<P> {
    /// None written.
    Private,
    /// `pub`.
    Pub,
    /// `pub(crate)` (and `crate` in 2015's `crate fn`).
    Crate,
    /// `pub(super)`.
    Super,
    /// `pub(self)`.
    SelfOnly,
    /// `pub(in path)`: the path.
    In(P),
    /// `pub(path)` with a path that is not `crate`, `super` or `self` and no `in`, which the
    /// parser takes and Rust refuses: the path, for the stage that refuses it to name.
    WithoutIn(P),
}

impl<P> WrittenVis<P> {
    /// The same visibility, its path mapped by `f`.
    pub fn map<Q>(self, f: impl FnOnce(P) -> Q) -> WrittenVis<Q> {
        match self {
            WrittenVis::Private => WrittenVis::Private,
            WrittenVis::Pub => WrittenVis::Pub,
            WrittenVis::Crate => WrittenVis::Crate,
            WrittenVis::Super => WrittenVis::Super,
            WrittenVis::SelfOnly => WrittenVis::SelfOnly,
            WrittenVis::In(path) => WrittenVis::In(f(path)),
            WrittenVis::WithoutIn(path) => WrittenVis::WithoutIn(f(path)),
        }
    }

    /// Its path, by reference.
    pub fn as_ref(&self) -> WrittenVis<&P> {
        match self {
            WrittenVis::Private => WrittenVis::Private,
            WrittenVis::Pub => WrittenVis::Pub,
            WrittenVis::Crate => WrittenVis::Crate,
            WrittenVis::Super => WrittenVis::Super,
            WrittenVis::SelfOnly => WrittenVis::SelfOnly,
            WrittenVis::In(path) => WrittenVis::In(path),
            WrittenVis::WithoutIn(path) => WrittenVis::WithoutIn(path),
        }
    }
}

byte_enum! {
    /// [`WrittenVis`]'s byte in a record; the path is a field beside it.
    pub(crate) enum WrittenVisTag, refused as "a byte that is no written visibility" {
        Private = 0 => "private",
        Pub = 1 => "pub",
        Crate = 2 => "pub(crate)",
        Super = 3 => "pub(super)",
        SelfOnly = 4 => "pub(self)",
        In = 5 => "pub(in ..)",
        WithoutIn = 6 => "pub(..) without in",
    }
}

impl<P> WrittenVis<P> {
    /// Its byte, and its path if any.
    pub(crate) fn split(self) -> (WrittenVisTag, Option<P>) {
        match self {
            WrittenVis::Private => (WrittenVisTag::Private, None),
            WrittenVis::Pub => (WrittenVisTag::Pub, None),
            WrittenVis::Crate => (WrittenVisTag::Crate, None),
            WrittenVis::Super => (WrittenVisTag::Super, None),
            WrittenVis::SelfOnly => (WrittenVisTag::SelfOnly, None),
            WrittenVis::In(path) => (WrittenVisTag::In, Some(path)),
            WrittenVis::WithoutIn(path) => (WrittenVisTag::WithoutIn, Some(path)),
        }
    }

    /// What [`WrittenVis::split`] gave; `path` is asked for only by the two forms with one.
    pub(crate) fn join(tag: WrittenVisTag, path: impl FnOnce() -> P) -> WrittenVis<P> {
        match tag {
            WrittenVisTag::Private => WrittenVis::Private,
            WrittenVisTag::Pub => WrittenVis::Pub,
            WrittenVisTag::Crate => WrittenVis::Crate,
            WrittenVisTag::Super => WrittenVis::Super,
            WrittenVisTag::SelfOnly => WrittenVis::SelfOnly,
            WrittenVisTag::In => WrittenVis::In(path()),
            WrittenVisTag::WithoutIn => WrittenVis::WithoutIn(path()),
        }
    }
}

byte_enum! {
    /// Where a binding of a scope came from (rustc's binding kinds; docs/v2/plan/design/
    /// bottom.md, "A binding has states"). Whether a macro expansion wrote it is the binding's
    /// expansion depth, not its origin.
    pub enum Origin, refused as "a byte that is no binding origin" {
        /// An item, `mod`, `struct`, `macro`, ... the module declares (itself, or through a
        /// macro expansion).
        Declared = 0 => "declared",
        /// A named `use`.
        Import = 1 => "import",
        /// A `use ..::*`.
        Glob = 2 => "glob",
        /// An `extern crate` item.
        ExternCrate = 3 => "extern crate",
        /// A `#[macro_export]` `macro_rules!` of the crate, which lives in the crate root's
        /// scope.
        MacroExport = 4 => "macro export",
        /// A prelude (the extern prelude, the `#[macro_use]` prelude, the standard prelude):
        /// never a binding of a scope, only what a path's first segment was found through.
        Prelude = 5 => "prelude",
    }
}

byte_enum! {
    /// rustc's ambiguity kinds that a scope can hold or an early resolution can be refused
    /// with (rustc dev guide, "Name resolution"; `rustc_resolve::AmbiguityKind`).
    pub enum AmbiguityKind, refused as "a byte that is no ambiguity kind" {
        /// Two glob imports bring one name with different targets: an error only where the
        /// name is used.
        GlobVsGlob = 0 => "glob vs glob",
        /// A macro-expanded name shadows a glob-imported one that an early resolution of this
        /// module used.
        GlobVsExpanded = 1 => "glob vs expanded",
        /// A glob-imported name and a name of an outer scope (a prelude) differ, and an early
        /// resolution went through one of them.
        GlobVsOuter = 2 => "glob vs outer",
        /// A macro-expanded name and a name of an outer scope differ, and an early resolution
        /// went through one of them.
        MoreExpandedVsOuter = 3 => "more expanded vs outer",
        /// A macro-expanded `macro_rules!` and a path-based macro of the same name.
        MacroRulesVsModularized = 4 => "macro_rules vs modularized",
        /// An attribute macro of a nearer scope with the name of a built-in attribute.
        BuiltinAttr = 5 => "builtin attribute",
    }
}

/// What a settled binding is: final, or final and ambiguous. A binding not final yet is not
/// a value of this type: the solver carries "undetermined" in its own working state, and a
/// scope that would leave a name undetermined settles it to not found first, so no record
/// and no reader of one can hold it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum BindingState {
    /// Final.
    Determined,
    /// Final, and ambiguous: naming it is an error of this kind.
    Ambiguous(AmbiguityKind),
}

byte_enum! {
    /// [`BindingState`]'s byte in a record; an ambiguity's kind is a field beside it.
    pub(crate) enum StateTag, refused as "a byte that is no binding state" {
        Determined = 0 => "determined",
        Ambiguous = 2 => "ambiguous",
    }
}

impl BindingState {
    /// Its bytes as a record holds them: the kind is `GlobVsGlob` unless ambiguous, so two
    /// builders write equal bytes.
    pub(crate) fn to_record(self) -> (StateTag, AmbiguityKind) {
        match self {
            BindingState::Determined => (StateTag::Determined, AmbiguityKind::GlobVsGlob),
            BindingState::Ambiguous(kind) => (StateTag::Ambiguous, kind),
        }
    }

    /// What [`BindingState::to_record`] wrote.
    pub(crate) fn from_record(tag: StateTag, kind: AmbiguityKind) -> BindingState {
        match tag {
            StateTag::Determined => BindingState::Determined,
            StateTag::Ambiguous => BindingState::Ambiguous(kind),
        }
    }
}

// SAFETY: a fieldless choice and a fieldless kind.
unsafe impl Portable for BindingState {}

/// A name's binding in a scope: what it names, who may name it, where it came from, how deep
/// in macro expansions it was written, and whether it is final.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Binding {
    pub def: Def,
    pub vis: Visibility,
    pub origin: Origin,
    /// How deep in macro expansions it was written: 0 in the module's own text, `n` in the
    /// expansion of a call made at depth `n - 1`. A binding at depth above 0 is
    /// "macro-expanded" in rustc's ambiguity rules.
    pub expansion: u16,
    pub state: BindingState,
}

// SAFETY: `Portable` fields only.
unsafe impl Portable for Binding {}

/// One entry of a scope: a binding by namespace and name. `N` is the name: `&str` read from a
/// record, an interned symbol or a test's `&'static str` in a working form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ScopeEntry<N> {
    pub ns: Namespace,
    pub name: N,
    pub binding: Binding,
}

/// Where an item is written: a row of a file's [`crate::FileItems`], or a row of the items one
/// macro call made (an `Expansion` record, keyed by the call; not built yet).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Place {
    /// Row `index` of the `FileItems` of the file with this content.
    File { file: ContentId, index: ItemIndex },
    /// Row `index` of the items of the expansion with this key.
    Expansion { expansion: ContentId, index: ItemIndex },
}

byte_enum! {
    /// [`Place`]'s byte in a record.
    pub(crate) enum PlaceTag, refused as "a byte that is no place" {
        File = 0 => "file",
        Expansion = 1 => "expansion",
    }
}

impl Place {
    pub(crate) fn to_record(self) -> (PlaceTag, ContentId, u32) {
        match self {
            Place::File { file, index } => (PlaceTag::File, file, index.0),
            Place::Expansion { expansion, index } => (PlaceTag::Expansion, expansion, index.0),
        }
    }

    pub(crate) fn from_record(tag: PlaceTag, origin: ContentId, index: u32) -> Place {
        match tag {
            PlaceTag::File => Place::File { file: origin, index: ItemIndex(index) },
            PlaceTag::Expansion => Place::Expansion { expansion: origin, index: ItemIndex(index) },
        }
    }
}

// SAFETY: a content hash and an index among the rows that content fixes.
unsafe impl Portable for Place {}

/// Which item of a file: its place among the file's items, in the file's order (a
/// [`crate::FileItems`] record's row). It names an item only together with the file's key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ItemIndex(pub u32);

impl ItemIndex {
    /// As an index of a slice.
    pub fn at(self) -> usize {
        usize_of(self.0)
    }
}

// SAFETY: a fixed-width integer whose meaning is the file's order, which the file's bytes (its
// key) fix; no run-time order.
unsafe impl Portable for ItemIndex {}

impl fmt::Display for ItemIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "item {}", self.0)
    }
}

/// `u32` to `usize`: every target blaze builds has a `usize` of at least 32 bits.
pub(crate) fn usize_of(at: u32) -> usize {
    usize::try_from(at).expect("a u32 fits a usize on every target blaze builds")
}

blaze_format::record! {
    /// A byte range, `start..end`: of a file, of an item (counted from the item's first byte,
    /// rule 71), of a call's tokens, ... as the field holding it says.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct Range: 1 {
        pub start: u32,
        pub end: u32,
    }
}

impl Range {
    /// `start..end`.
    pub const fn new(start: u32, end: u32) -> Range {
        Range { start, end }
    }

    /// The number of bytes.
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    /// Whether it holds no byte.
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Whether `at` is in it.
    pub const fn contains(self, at: u32) -> bool {
        self.start <= at && at < self.end
    }

    /// The range moved `by` bytes on.
    pub const fn shifted(self, by: u32) -> Range {
        Range { start: self.start + by, end: self.end + by }
    }

    /// The range moved `by` bytes on; `None` past `u32::MAX`.
    pub const fn checked_shifted(self, by: u32) -> Option<Range> {
        match (self.start.checked_add(by), self.end.checked_add(by)) {
            (Some(start), Some(end)) => Some(Range { start, end }),
            _ => None,
        }
    }

    /// As a range of `usize`, for slicing.
    pub fn to_usize(self) -> core::ops::Range<usize> {
        usize_of(self.start)..usize_of(self.end)
    }

    /// The bytes of `text` it covers, as `text[start..end]`; panics as that does.
    pub fn of(self, text: &str) -> &str {
        &text[self.to_usize()]
    }

    /// The bytes of `text` it covers; `None` where that is out of `text` or cuts a character.
    pub fn get(self, text: &str) -> Option<&str> {
        text.get(self.to_usize())
    }
}

// SAFETY: two fixed-width integers; nothing of the run.
unsafe impl Portable for Range {}
