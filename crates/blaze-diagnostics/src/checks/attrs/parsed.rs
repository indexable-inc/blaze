//! What attribute parsing makes of a node's builtin attributes, as far as the checks after it
//! read it: rustc's `AttributeKind`, of the attributes `check_mod_attrs` and the parsers'
//! cross-attribute checks look at. Ranges are of the file the attribute is written in.
//!
//! Follows rustc's `compiler/rustc_hir/src/attrs/data_structures.rs` (`AttributeKind`,
//! `InlineAttr`, `OptimizeAttr`, `ReprAttr`, `DocAttribute`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::TextRange;

/// One parsed attribute of a node: what it is, and which of the node's attributes it is
/// (for one that combines several, the first).
#[derive(Clone, Debug)]
pub(crate) struct ParsedAttr {
    pub(crate) parsed: Parsed,
    /// Its index in [`crate::lints::syntax::attrs::Owner::attrs`].
    pub(crate) attr: usize,
}

/// rustc's `AttributeKind`, of what the checks read; the rest is `Other`.
#[derive(Clone, Debug)]
pub(crate) enum Parsed {
    /// Parsed, and nothing later reads more of it than what it is: the name of rustc's
    /// `AttributeKind` variant (`"Cold"`).
    Other(&'static str),
    Inline(Inline),
    Optimize(Optimize),
    /// The hints of one `#[repr(..)]`, each with its range.
    Repr(Vec<(Repr, TextRange)>),
    /// `#[lang = "name"]`.
    Lang(&'static str),
    /// `#[linkage = ".."]`, a linkage rustc knows.
    Linkage(String),
    /// `#[export_name = ".."]`, a name rustc takes.
    ExportName(String),
    NoMangle,
    TrackCaller,
    NonExhaustive,
    MayDangle,
    /// `#[unsafe(naked)]`.
    Naked,
    ProcMacro(ProcMacroKind),
    RustcAllowConstFnUnstable,
    RustcDumpObjectLifetimeDefaults,
    /// `#[rustc_legacy_const_generics(..)]`: each index and its range.
    RustcLegacyConstGenerics(Vec<(usize, TextRange)>),
    /// `#[rustc_must_implement_one_of(..)]`: each name and its range.
    RustcMustImplementOneOf(Vec<(String, TextRange)>),
    RustcPassIndirectlyInNonRusticAbis,
    /// What one `#[doc(..)]` says that the checks read.
    Doc(Doc),
    /// `#[macro_export]`, `#[macro_export(local_inner_macros)]`.
    MacroExport,
    /// `#[stable]` or `#[unstable]`.
    Stability(Stability),
    /// `#[rustc_const_stable]` or `#[rustc_const_unstable]`.
    ConstStability(Stability),
    /// `#[rustc_default_body_unstable]`.
    BodyStability(Stability),
    RustcConstStableIndirect,
    /// `#[deprecated]`, by its `since`.
    Deprecated(DeprecatedSince),
}

/// rustc's `InlineAttr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inline {
    /// `#[inline]`.
    Hint,
    Always,
    Never,
    /// `#[rustc_force_inline]`.
    Force,
}

/// rustc's `OptimizeAttr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Optimize {
    Default,
    DoNotOptimize,
    Speed,
    Size,
}

/// rustc's `ReprAttr`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Repr {
    Int(&'static str),
    Rust,
    C,
    Packed(u64),
    Simd,
    Transparent,
    Align(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcMacroKind {
    FunctionLike,
    Derive,
    Attribute,
}

impl ProcMacroKind {
    /// As rustc's `ProcMacroBadSig` names it.
    pub(crate) fn descr(self) -> &'static str {
        match self {
            ProcMacroKind::Attribute => "attribute proc macro",
            ProcMacroKind::Derive => "derive proc macro",
            ProcMacroKind::FunctionLike => "function-like proc macro",
        }
    }
}

/// rustc's `DocInline`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DocInline {
    Inline,
    NoInline,
}

/// What one `#[doc(..)]` (or `#[doc = ".."]`) says that `check_doc_attrs` reads: rustc's
/// `DocAttribute`, of one attribute (the checks take the node's together).
#[derive(Clone, Debug, Default)]
pub(crate) struct Doc {
    pub(crate) aliases: Vec<(String, InFile<TextRange>)>,
    pub(crate) keyword: Option<(String, InFile<TextRange>)>,
    pub(crate) attribute: Option<(String, InFile<TextRange>)>,
    pub(crate) fake_variadic: Option<InFile<TextRange>>,
    pub(crate) search_unbox: Option<InFile<TextRange>>,
    pub(crate) inline: Vec<(DocInline, InFile<TextRange>)>,
    pub(crate) masked: Option<InFile<TextRange>>,
}

/// rustc's `Stability`, `PartialConstStability` and `DefaultBodyStability`, of what the
/// stability checks read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stability {
    pub(crate) feature: String,
    pub(crate) level: StabilityLevel,
    /// The attribute that gives it, of the node's.
    pub(crate) attr: usize,
}

/// rustc's `StabilityLevel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StabilityLevel {
    Unstable,
    Stable(StableSince),
}

/// rustc's `StableSince`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StableSince {
    Version(RustcVersion),
    /// `CURRENT_RUSTC_VERSION`.
    Current,
    /// Not a version; an error was given.
    Err,
}

/// rustc's `DeprecatedSince`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeprecatedSince {
    RustcVersion(RustcVersion),
    /// `"TBD"`.
    Future,
    /// Outside the standard library, as written.
    NonStandard(String),
    Unspecified,
    /// Not a version; an error was given.
    Err,
}

/// rustc's `RustcVersion`: major, minor, patch.
pub(crate) type RustcVersion = (u16, u16, u16);
