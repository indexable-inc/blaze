//! The words of the schema: an item's kind, a namespace, a visibility, an index among a file's
//! items and a byte range inside an item.

use core::fmt;

use blaze_format::FormatError;
use blaze_id::{ItemKey, Portable};

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

byte_enum! {
    /// What an item is. An item-level macro call is not an item: it is a
    /// [`crate::MacroCall`] of its file.
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
        /// A macro definition: `macro_rules!`, `macro`, or a proc macro's function.
        Macro = 10 => "macro",
        Use = 11 => "use",
        ExternCrate = 12 => "extern crate",
        Variant = 13 => "variant",
        Field = 14 => "field",
    }
}

byte_enum! {
    /// Which of a scope's namespaces a name is in. A unit or tuple struct is in two, and is two
    /// entries of a scope.
    pub enum Namespace, refused as "a byte that is no namespace" {
        Types = 0 => "types",
        Values = 1 => "values",
        Macros = 2 => "macros",
    }
}

byte_enum! {
    /// [`Visibility`]'s byte in a record; a restricted one's module is a field beside it.
    pub(crate) enum VisibilityTag, refused as "a byte that is no visibility" {
        Pub = 0 => "pub",
        Crate = 1 => "pub(crate)",
        Restricted = 2 => "pub(in ..)",
        Private = 3 => "private",
    }
}

byte_enum! {
    /// How a binding of a scope came to be (docs/v2/plan/design/bottom.md, "A binding has
    /// states"). Explicit bindings are a layer below globs: a declared or explicitly imported
    /// name shadows a glob's.
    pub enum Origin, refused as "a byte that is no binding origin" {
        /// The module declares the item itself ([`crate::ModuleDecls`]).
        Declared = 0 => "declared",
        /// A `use` naming it.
        Import = 1 => "import",
        /// A glob `use`.
        Glob = 2 => "glob",
        /// An item a macro call in the module made.
        Expanded = 3 => "expanded",
    }
}

byte_enum! {
    /// [`Target`]'s byte in a record; its keys are fields beside it.
    pub(crate) enum TargetTag, refused as "a byte that is no binding state" {
        Item = 0 => "item",
        Undetermined = 1 => "undetermined",
        Ambiguous = 2 => "ambiguous",
    }
}

byte_enum! {
    /// Which bindings of a scope a glob of it reads, and so which [`crate::ModuleScope`]
    /// `Exports` digest it reads: a reader is moved only when the bindings of its class change.
    pub enum ExportClass, refused as "a byte that is no export class" {
        /// `pub` bindings: what a glob from another crate sees.
        Pub = 0 => "pub",
        /// `pub` and `pub(crate)` bindings: what a glob elsewhere in the crate sees.
        Crate = 1 => "crate",
        /// Every binding: what a glob from inside every restriction sees (a child module's
        /// `use super::*`), which filters by its own place after reading.
        Within = 2 => "within",
    }
}

impl ExportClass {
    /// Whether a binding of visibility `vis` is in this class.
    pub fn holds(self, vis: Visibility) -> bool {
        match (self, vis) {
            (ExportClass::Within, _) => true,
            (ExportClass::Pub | ExportClass::Crate, Visibility::Pub) => true,
            (ExportClass::Crate, Visibility::Crate) => true,
            (ExportClass::Pub, Visibility::Crate) => false,
            (ExportClass::Pub | ExportClass::Crate, Visibility::Restricted(_) | Visibility::Private) => {
                false
            }
        }
    }
}

byte_enum! {
    /// A crate's edition, from its root record.
    pub enum Edition, refused as "a byte that is no edition" {
        E2015 = 0 => "2015",
        E2018 = 1 => "2018",
        E2021 = 2 => "2021",
        E2024 = 3 => "2024",
    }
}

byte_enum! {
    /// What a macro definition is ([`crate::MacroDef`]).
    pub enum MacroKind, refused as "a byte that is no macro kind" {
        /// `macro_rules! name { .. }`.
        Rules = 0 => "macro_rules",
        /// `macro name(..) { .. }`.
        Macro2 = 1 => "macro",
        /// `#[proc_macro]`.
        ProcBang = 2 => "function-like proc macro",
        /// `#[proc_macro_attribute]`.
        ProcAttr = 3 => "attribute proc macro",
        /// `#[proc_macro_derive]`.
        ProcDerive = 4 => "derive proc macro",
        /// A macro the compiler implements (`#[rustc_builtin_macro]`).
        Builtin = 5 => "builtin macro",
    }
}

byte_enum! {
    /// [`Place`]'s byte in a record.
    pub(crate) enum PlaceTag, refused as "a byte that is no place" {
        File = 0 => "file",
        Expansion = 1 => "expansion",
    }
}

/// What a name of a scope is bound to (bottom.md, "A binding has states"): `undetermined` and
/// `ambiguous` are values, not errors, so the fixpoint over a cycle of globs is well defined.
/// `K` is [`ItemKey`] in a record and [`crate::Local`] inside a task.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Target<K> {
    /// The item.
    Item(K),
    /// An import whose answer is not final yet.
    Undetermined,
    /// Two candidates of one layer: rustc's ambiguity, reported by the reader that needs it.
    Ambiguous(K, K),
}

impl<K> Target<K> {
    /// The same state, its keys mapped by `f`.
    pub fn try_map<L, E>(self, mut f: impl FnMut(K) -> Result<L, E>) -> Result<Target<L>, E> {
        Ok(match self {
            Target::Item(key) => Target::Item(f(key)?),
            Target::Undetermined => Target::Undetermined,
            Target::Ambiguous(one, other) => Target::Ambiguous(f(one)?, f(other)?),
        })
    }
}

impl Target<ItemKey> {
    /// Its byte and keys as a record holds them: zeros where there is no key, so two builders
    /// of one scope write equal bytes.
    pub(crate) fn to_record(self) -> (TargetTag, blaze_id::ContentId, blaze_id::ContentId) {
        let zero = blaze_id::ContentId([0; 32]);
        match self {
            Target::Item(key) => (TargetTag::Item, key.0, zero),
            Target::Undetermined => (TargetTag::Undetermined, zero, zero),
            Target::Ambiguous(one, other) => (TargetTag::Ambiguous, one.0, other.0),
        }
    }

    /// What [`Target::to_record`] wrote.
    pub(crate) fn from_record(
        tag: TargetTag,
        one: blaze_id::ContentId,
        other: blaze_id::ContentId,
    ) -> Target<ItemKey> {
        match tag {
            TargetTag::Item => Target::Item(ItemKey(one)),
            TargetTag::Undetermined => Target::Undetermined,
            TargetTag::Ambiguous => Target::Ambiguous(ItemKey(one), ItemKey(other)),
        }
    }
}

// SAFETY: a state and `Portable` keys; `Target<Local<_>>` is not `Portable`, as `Local` is not.
unsafe impl<K: Portable> Portable for Target<K> {}

/// Where an item is written: a row of a file's [`crate::FileItems`], or a row of the items one
/// macro call made (an `Expansion` record, keyed by the call; not built yet).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Place {
    /// Row `index` of the `FileItems` of the file with this content.
    File { file: blaze_id::ContentId, index: ItemIndex },
    /// Row `index` of the items of the expansion with this key.
    Expansion { expansion: blaze_id::ContentId, index: ItemIndex },
}

impl Place {
    pub(crate) fn to_record(self) -> (PlaceTag, blaze_id::ContentId, u32) {
        match self {
            Place::File { file, index } => (PlaceTag::File, file, index.0),
            Place::Expansion { expansion, index } => (PlaceTag::Expansion, expansion, index.0),
        }
    }

    pub(crate) fn from_record(tag: PlaceTag, origin: blaze_id::ContentId, index: u32) -> Place {
        match tag {
            PlaceTag::File => Place::File { file: origin, index: ItemIndex(index) },
            PlaceTag::Expansion => Place::Expansion { expansion: origin, index: ItemIndex(index) },
        }
    }
}

// SAFETY: a content hash and an index among the rows that content fixes.
unsafe impl Portable for Place {}

/// Who may name an item through a scope.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Visibility {
    /// `pub`: anyone.
    Pub,
    /// `pub(crate)`: the item's crate.
    Crate,
    /// `pub(super)`, `pub(in path)`, `pub(self)` spelled out: the module with this key and the
    /// modules under it.
    Restricted(ItemKey),
    /// No visibility written: the module the item is declared in and the modules under it.
    Private,
}

impl Visibility {
    /// Its byte and module as a record holds them: the module is all zeros unless restricted,
    /// so two builders of one scope write equal bytes.
    pub(crate) fn to_record(self) -> (VisibilityTag, blaze_id::ContentId) {
        match self {
            Visibility::Pub => (VisibilityTag::Pub, blaze_id::ContentId([0; 32])),
            Visibility::Crate => (VisibilityTag::Crate, blaze_id::ContentId([0; 32])),
            Visibility::Restricted(module) => (VisibilityTag::Restricted, module.0),
            Visibility::Private => (VisibilityTag::Private, blaze_id::ContentId([0; 32])),
        }
    }

    /// What [`Visibility::to_record`] wrote.
    pub(crate) fn from_record(tag: VisibilityTag, module: blaze_id::ContentId) -> Visibility {
        match tag {
            VisibilityTag::Pub => Visibility::Pub,
            VisibilityTag::Crate => Visibility::Crate,
            VisibilityTag::Restricted => Visibility::Restricted(ItemKey(module)),
            VisibilityTag::Private => Visibility::Private,
        }
    }
}

// SAFETY: a tag and, when restricted, an item key: `Portable` both.
unsafe impl Portable for Visibility {}

/// Which item of a file: its place among the file's items, in the file's order (a
/// [`crate::FileItems`] record's row). It names an item only together with the file's key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ItemIndex(pub u32);

// SAFETY: a fixed-width integer whose meaning is the file's order, which the file's bytes (its
// key) fix; no run-time order.
unsafe impl Portable for ItemIndex {}

impl fmt::Display for ItemIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "item {}", self.0)
    }
}

blaze_format::record! {
    /// Bytes inside an item: `len` of them from `start`, counted from the item's first byte
    /// ([`crate::FileItem::at`]), so an edit above the item moves none of its ranges (rule 71).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct ByteRange: 1 {
        pub start: u32,
        pub len: u32,
    }
}

impl ByteRange {
    pub const EMPTY: ByteRange = ByteRange { start: 0, len: 0 };

    /// Where it ends, from the item's start; `None` past `u32::MAX`.
    pub fn end(self) -> Option<u32> {
        self.start.checked_add(self.len)
    }

    /// The bytes of the file it covers, for an item starting at `at` in the file; `None` past
    /// `u32::MAX`.
    pub fn in_file(self, at: u32) -> Option<core::ops::Range<usize>> {
        let start = at.checked_add(self.start)?;
        let end = start.checked_add(self.len)?;
        Some(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
    }
}

// SAFETY: two fixed-width integers counted from an item's start; nothing of the run.
unsafe impl Portable for ByteRange {}
