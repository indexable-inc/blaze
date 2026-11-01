//! `hir_expand` deals with macro expansion.
//!
//! Specifically, it implements a concept of `MacroFile` -- a file whose syntax
//! tree originates not from the text of some `FileId`, but from some macro
//! expansion.
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api, allocator_ext)]
// It's useful to refer to code that is private in doc comments.
#![allow(rustdoc::private_intra_doc_links)]
#![forbid(unsafe_code)]

pub use intern;

pub mod attrs;
pub mod builtin;
pub mod change;
pub mod declarative;
pub mod eager;
pub mod files;
#[cfg(any(test, feature = "fixture"))]
pub mod fixture;
pub mod hygiene;
pub mod inert_attr_macro;
pub mod mod_path;
pub mod name;
pub mod proc_macro;
pub mod span_map;

mod cfg_process;
mod fixup;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod view_tests;

use blaze_query::db::{AsId, FromId};
use thin_vec::ThinVec;
use triomphe::Arc;

use core::fmt;
use std::ops;

use base_db::{Crate, Db};
use blaze_ids::radix;
use blaze_mem::Frame;
use either::Either;
use mbe::MatchedArmIndex;
use span::{
    AstIdMap, Edition, ErasedFileAstId, NO_DOWNMAP_ERASED_FILE_AST_ID_MARKER, Span,
    SyntaxContext,
};
use syntax::{
    Parse, SyntaxError, SyntaxNode, T, TextRange, TextSize,
    ast::{self, AstNode},
    view::{self, AstNode as _},
};
use syntax_bridge::DocCommentDesugarMode;

use crate::{
    attrs::AttrId,
    builtin::{
        BuiltinAttrExpander, BuiltinDeriveExpander, BuiltinFnLikeExpander, EagerExpander,
        include_input_to_file_id,
    },
    cfg_process::attr_macro_input_to_token_tree,
    fixup::SyntaxFixupUndoInfo,
    hygiene::{span_with_call_site_ctxt, span_with_def_site_ctxt, span_with_mixed_site_ctxt},
    proc_macro::{CustomProcMacroExpander, ProcMacroKind},
    span_map::{ExpansionSpanMap, InOrder, MacroRulesArgs, SpanMap},
};

pub use crate::files::{AstId, ErasedAstId, FileRange, InFile, InMacroFile, InRealFile};

pub use base_db::EditionedFileId;
pub use mbe::{
    DeclarativeMacro, ExpandErrorKind as MbeErrorKind, MacroCallStyle, MacroCallStyles,
    MatchFailure, ValueResult,
};

pub use syntax_bridge::parse_to_token_tree_static_span;
pub use tt;

/// What [`MacroCallId::macro_arg`] keeps: the argument's tree, how to undo its fixups, the
/// call's span, and why the call is not expanded when it is not (its tree is then empty).
type MacroArgResult = (tt::TopSubtree, SyntaxFixupUndoInfo, Span, Option<ExpandError>);

/// A macro call's argument as [`MacroCallId::macro_arg_considering_derives`] reads it: from the
/// memo of `macro_arg`, or for an eager call from its interned location.
pub type MacroArg<'db> = (&'db tt::TopSubtree, &'db SyntaxFixupUndoInfo, Span);

/// A macro's expansion tree in one run of [`MacroCallId::parse_macro_expansion`]: a proc macro's
/// tree or an eager call's input, read in place; or a tree built in the run's frame.
pub(crate) enum Expansion<'db, 'f> {
    Borrowed(&'db tt::TopSubtree),
    Framed(tt::TopSubtree<&'f Frame>),
}

impl Expansion<'_, '_> {
    pub(crate) fn view(&self) -> tt::SubtreeView<'_> {
        match self {
            Expansion::Borrowed(it) => it.view(),
            Expansion::Framed(it) => it.view(),
        }
    }
}

static NO_FIXUPS: SyntaxFixupUndoInfo = SyntaxFixupUndoInfo::NONE;

thread_local! {
    /// The scratch of the radix sorts of ranges, kept by the worker.
    static RANGE_SORT: std::cell::RefCell<blaze_mem::scratch::Scratch<TextRange>> =
        const { std::cell::RefCell::new(blaze_mem::scratch::Scratch::new()) };
}

#[macro_export]
macro_rules! impl_intern_lookup {
    ($id:ident, $loc:ident) => {
        impl $crate::Intern for $loc {
            type ID = $id;
            fn intern(self, db: &::base_db::Db) -> Self::ID {
                $id::new(db, self)
            }
        }

        impl $crate::Lookup for $id {
            type Data = $loc;
            fn lookup<'db>(&self, db: &'db ::base_db::Db) -> &'db Self::Data {
                self.loc(db)
            }
        }
    };
}

// ideally these would be defined in base-db, but the orphan rule doesn't let us
pub trait Intern {
    type ID;
    fn intern(self, db: &Db) -> Self::ID;
}

pub trait Lookup {
    type Data;
    fn lookup<'db>(&self, db: &'db Db) -> &'db Self::Data;
}

impl_intern_lookup!(MacroCallId, MacroCallLoc);

pub type ExpandResult<T> = ValueResult<T, ExpandError>;

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct ExpandError {
    inner: Arc<(ExpandErrorKind, Span)>,
}

impl ExpandError {
    pub fn new(span: Span, kind: ExpandErrorKind) -> Self {
        ExpandError { inner: Arc::new((kind, span)) } // alloc: one per error, on the error path
    }
    pub fn other(span: Span, msg: impl Into<Box<str>>) -> Self {
        ExpandError { inner: Arc::new((ExpandErrorKind::Other(msg.into()), span)) } // alloc: one per error, on the error path
    }
    /// A semantic refusal of a built-in derive, rather than a failure to parse or expand it.
    pub fn builtin_derive(span: Span, msg: impl Into<Box<str>>) -> Self {
        Self::new(span, ExpandErrorKind::BuiltinDerive(msg.into()))
    }
    /// An error of a builtin macro or derive in rustc's words, at `span`.
    pub fn builtin(span: Span, error: BuiltinError) -> Self {
        Self::new(span, ExpandErrorKind::Builtin(Box::new(error))) // alloc: one per error, on the error path
    }
    pub fn kind(&self) -> &ExpandErrorKind {
        &self.inner.0
    }
    pub fn span(&self) -> Span {
        self.inner.1
    }
    /// This error, at `span` instead.
    pub fn at(self, span: Span) -> Self {
        // clone: of a shared error's kind only, on the error path
        let (kind, _) = Arc::try_unwrap(self.inner).unwrap_or_else(|shared| (*shared).clone());
        Self::new(span, kind)
    }

    pub fn render_to_string(&self, db: &Db) -> RenderedExpandError {
        self.inner.0.render_to_string(db)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum ExpandErrorKind {
    /// Attribute macro expansion is disabled.
    ProcMacroAttrExpansionDisabled,
    /// A macro of proc-macro crate `krate` with no expander, and why, as the proc-macro map
    /// said when the call was expanded.
    MissingProcMacroExpander { krate: Crate, why: Box<str> },
    /// The macro for this call is disabled.
    MacroDisabled,
    /// The macro definition has errors.
    MacroDefinition,
    Mbe(mbe::ExpandErrorKind),
    RecursionOverflow,
    Other(Box<str>),
    BuiltinDerive(Box<str>),
    DefaultNoVariant,
    ProcMacroPanic(Box<str>),
    /// An error a builtin macro or derive gives as `rustc_builtin_macros` gives it.
    Builtin(Box<BuiltinError>),
}

/// An error of a builtin macro or derive as `rustc_builtin_macros` words it: the message, its
/// error code, the primary spans it has besides the error's own, and the notes and helps under
/// it, in rustc's order.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct BuiltinError {
    pub message: Box<str>,
    /// The number of the code: 802 for E0802.
    pub code: Option<u16>,
    pub also_at: Box<[Span]>,
    pub children: Box<[(BuiltinChild, Box<str>)]>,
    /// A built-in derive's, rendered as one: [`RenderedExpandError::BUILTIN_DERIVE`].
    pub derive: bool,
}

/// What a line under a [`BuiltinError`] is.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum BuiltinChild {
    Note,
    Help,
}

impl BuiltinError {
    pub fn new(message: impl Into<Box<str>>) -> Self {
        BuiltinError {
            message: message.into(),
            code: None,
            also_at: Box::default(),
            children: Box::default(),
            derive: false,
        }
    }

    /// The error of a built-in derive.
    pub fn derive(message: impl Into<Box<str>>) -> Self {
        BuiltinError { derive: true, ..BuiltinError::new(message) }
    }

    pub fn code(self, code: u16) -> Self {
        BuiltinError { code: Some(code), ..self }
    }

    pub fn note(self, text: impl Into<Box<str>>) -> Self {
        self.child(BuiltinChild::Note, text.into())
    }

    pub fn help(self, text: impl Into<Box<str>>) -> Self {
        self.child(BuiltinChild::Help, text.into())
    }

    /// The primary spans after the error's own.
    pub fn also_at(self, spans: impl Into<Box<[Span]>>) -> Self {
        BuiltinError { also_at: spans.into(), ..self }
    }

    pub fn at(self, span: Span) -> ExpandError {
        ExpandError::builtin(span, self)
    }

    fn child(self, kind: BuiltinChild, text: Box<str>) -> Self {
        // alloc: an error's few lines, on the error path
        let mut children = self.children.into_vec();
        children.push((kind, text));
        BuiltinError { children: children.into_boxed_slice(), ..self }
    }
}

pub struct RenderedExpandError {
    pub message: String,
    pub error: bool,
    pub kind: &'static str,
}

impl fmt::Display for RenderedExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl RenderedExpandError {
    const GENERAL_KIND: &str = "macro-error";
    const DISABLED: &str = "proc-macro-disabled";
    const ATTR_EXP_DISABLED: &str = "attribute-expansion-disabled";
    /// An [`ExpandErrorKind::Builtin`] of a builtin macro: the message is rustc's own.
    pub const BUILTIN: &str = "builtin-macro-error";
    /// An [`ExpandErrorKind::BuiltinDerive`], or an [`ExpandErrorKind::Builtin`] of a built-in
    /// derive: the message is rustc's own, and the error fails a build.
    pub const BUILTIN_DERIVE: &str = "builtin-derive-error";
}

impl ExpandErrorKind {
    pub fn render_to_string(&self, db: &Db) -> RenderedExpandError {
        match self {
            ExpandErrorKind::ProcMacroAttrExpansionDisabled => RenderedExpandError {
                message: "procedural attribute macro expansion is disabled".to_owned(), // alloc: a diagnostic's text, at the edge
                error: false,
                kind: RenderedExpandError::ATTR_EXP_DISABLED,
            },
            ExpandErrorKind::MacroDisabled => RenderedExpandError {
                message: "proc-macro is explicitly disabled".to_owned(), // alloc: a diagnostic's text, at the edge
                error: false,
                kind: RenderedExpandError::DISABLED,
            },
            ExpandErrorKind::MissingProcMacroExpander { why, .. } => RenderedExpandError {
                message: why.to_string(), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: RenderedExpandError::GENERAL_KIND,
            },
            ExpandErrorKind::MacroDefinition => RenderedExpandError {
                message: "macro definition has parse errors".to_owned(), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: RenderedExpandError::GENERAL_KIND,
            },
            ExpandErrorKind::Mbe(e) => RenderedExpandError {
                message: e.to_string(), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: RenderedExpandError::GENERAL_KIND,
            },
            ExpandErrorKind::RecursionOverflow => RenderedExpandError {
                message: "overflow expanding the original macro".to_owned(), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: RenderedExpandError::GENERAL_KIND,
            },
            ExpandErrorKind::Other(e) => RenderedExpandError {
                message: (**e).to_owned(), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: RenderedExpandError::GENERAL_KIND,
            },
            ExpandErrorKind::BuiltinDerive(e) => RenderedExpandError {
                message: (**e).to_owned(), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: RenderedExpandError::BUILTIN_DERIVE,
            },
            ExpandErrorKind::DefaultNoVariant => RenderedExpandError {
                message: "`#[derive(Default)]` on enum with no `#[default]`".to_owned(), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: "E0665",
            },
            ExpandErrorKind::ProcMacroPanic(e) => RenderedExpandError {
                message: format!("proc-macro panicked: {e}"), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: RenderedExpandError::GENERAL_KIND,
            },
            ExpandErrorKind::Builtin(e) => RenderedExpandError {
                message: (*e.message).to_owned(), // alloc: a diagnostic's text, at the edge
                error: true,
                kind: if e.derive {
                    RenderedExpandError::BUILTIN_DERIVE
                } else {
                    RenderedExpandError::BUILTIN
                },
            },
        }
    }
}

impl From<mbe::ExpandError> for ExpandError {
    fn from(mbe: mbe::ExpandError) -> Self {
        ExpandError { inner: Arc::new((ExpandErrorKind::Mbe(mbe.inner.1.clone()), mbe.inner.0)) } // alloc: clone: one per error, on the error path
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MacroCallLoc {
    pub def: MacroDefId,
    pub krate: Crate,
    pub kind: MacroCallKind,
    pub ctxt: SyntaxContext,
    /// The macro recursion depth of this expansion.
    ///
    /// Because macro expansions can cross `DefMap` boundaries, we must track this inside the expansion
    /// to not recurse infinitely.
    pub macro_depth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacroDefId {
    pub krate: Crate,
    pub edition: Edition,
    pub kind: MacroDefKind,
    pub local_inner: bool,
    pub allow_internal_unsafe: bool,
    /// Whether the macro has `#[allow_internal_unstable(edition_panic)]`: a `panic!` in it is
    /// of the edition of whoever calls the macro.
    pub edition_panic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacroDefKind {
    Declarative(AstId<ast::Macro>, MacroCallStyles),
    BuiltIn(AstId<ast::Macro>, BuiltinFnLikeExpander),
    BuiltInAttr(AstId<ast::Macro>, BuiltinAttrExpander),
    BuiltInDerive(AstId<ast::Macro>, BuiltinDeriveExpander),
    BuiltInEager(AstId<ast::Macro>, EagerExpander),
    UnimplementedBuiltIn(AstId<ast::Macro>),
    ProcMacro(AstId<ast::Fn>, CustomProcMacroExpander, ProcMacroKind),
}

impl MacroDefKind {
    #[inline]
    pub fn is_declarative(&self) -> bool {
        matches!(self, MacroDefKind::Declarative(..))
    }

    pub fn erased_ast_id(&self) -> ErasedAstId {
        match *self {
            MacroDefKind::ProcMacro(id, ..) => id.erase(),
            MacroDefKind::BuiltIn(id, _)
            | MacroDefKind::BuiltInAttr(id, _)
            | MacroDefKind::BuiltInDerive(id, _)
            | MacroDefKind::BuiltInEager(id, _)
            | MacroDefKind::Declarative(id, ..)
            | MacroDefKind::UnimplementedBuiltIn(id) => id.erase(),
        }
    }

    /// The kind of macro this is: what a call of it expands as.
    pub fn macro_kind(&self) -> MacroKind {
        match self {
            MacroDefKind::Declarative(..) | MacroDefKind::UnimplementedBuiltIn(..) => {
                MacroKind::Declarative
            }
            MacroDefKind::BuiltIn(..) | MacroDefKind::BuiltInEager(..) => {
                MacroKind::DeclarativeBuiltIn
            }
            MacroDefKind::BuiltInDerive(..) => MacroKind::DeriveBuiltIn,
            MacroDefKind::BuiltInAttr(..) => MacroKind::AttrBuiltIn,
            MacroDefKind::ProcMacro(_, _, ProcMacroKind::CustomDerive) => MacroKind::Derive,
            MacroDefKind::ProcMacro(_, _, ProcMacroKind::Attr) => MacroKind::Attr,
            MacroDefKind::ProcMacro(_, _, ProcMacroKind::Bang) => MacroKind::ProcMacro,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct EagerCallInfo {
    /// The expanded argument of the eager macro.
    pub arg: tt::TopSubtree,
    /// Call id of the eager macro's input file (this is the macro file for its fully expanded input).
    pub arg_id: MacroCallId,
    pub error: Option<ExpandError>,
    /// The call site span of the eager macro
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MacroCallKind {
    FnLike {
        ast_id: AstId<ast::MacroCall>,
        expand_to: ExpandTo,
        /// Some if this is a macro call for an eager macro. Note that this is `None`
        /// for the eager input macro file.
        // FIXME: This is being interned, subtrees can vary quickly differing just slightly causing
        // leakage problems here
        // Shared, as `attr_args` is: the query engine clones an interned key, and a token tree is never cloned.
        eager: Option<Arc<EagerCallInfo>>,
    },
    Derive {
        ast_id: AstId<ast::Adt>,
        /// Syntactical index of the invoking `#[derive]` attribute.
        derive_attr_index: AttrId,
        /// Index of the derive macro in the derive attribute
        derive_index: u32,
        /// The "parent" macro call.
        /// We will resolve the same token tree for all derive macros in the same derive attribute.
        derive_macro_id: MacroCallId,
    },
    Attr {
        ast_id: AstId<ast::Item>,
        // FIXME: This shouldn't be here, we can derive this from `invoc_attr_index`.
        attr_args: Option<Arc<tt::TopSubtree>>,
        /// This contains the list of all *active* attributes (derives and attr macros) preceding this
        /// attribute, including this attribute. You can retrieve the [`AttrId`] of the current attribute
        /// by calling [`invoc_attr()`] on this.
        ///
        /// The macro should not see the attributes here.
        ///
        /// [`invoc_attr()`]: AttrMacroAttrIds::invoc_attr
        censored_attr_ids: AttrMacroAttrIds,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttrMacroAttrIds(AttrMacroAttrIdsRepr);

impl AttrMacroAttrIds {
    #[inline]
    pub fn from_one(id: AttrId) -> Self {
        Self(AttrMacroAttrIdsRepr::One(id))
    }

    #[inline]
    pub fn from_many(ids: &[AttrId]) -> Self {
        if let &[id] = ids {
            Self(AttrMacroAttrIdsRepr::One(id))
        } else {
            Self(AttrMacroAttrIdsRepr::ManyDerives(ids.iter().copied().collect())) // alloc: the list a call's interned key owns, once per attribute call with several derives
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AttrMacroAttrIdsRepr {
    One(AttrId),
    ManyDerives(ThinVec<AttrId>),
}

impl ops::Deref for AttrMacroAttrIds {
    type Target = [AttrId];

    #[inline]
    fn deref(&self) -> &Self::Target {
        match &self.0 {
            AttrMacroAttrIdsRepr::One(one) => std::slice::from_ref(one),
            AttrMacroAttrIdsRepr::ManyDerives(many) => many,
        }
    }
}

impl AttrMacroAttrIds {
    #[inline]
    pub fn invoc_attr(&self) -> AttrId {
        match &self.0 {
            AttrMacroAttrIdsRepr::One(it) => *it,
            AttrMacroAttrIdsRepr::ManyDerives(it) => {
                *it.last().expect("should always have at least one `AttrId`")
            }
        }
    }
}

impl MacroCallKind {
    pub(crate) fn call_style(&self) -> MacroCallStyle {
        match self {
            MacroCallKind::FnLike { .. } => MacroCallStyle::FnLike,
            MacroCallKind::Derive { .. } => MacroCallStyle::Derive,
            MacroCallKind::Attr { .. } => MacroCallStyle::Attr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacroKind {
    /// `macro_rules!` or Macros 2.0 macro.
    Declarative,
    /// A built-in function-like macro.
    DeclarativeBuiltIn,
    /// A custom derive.
    Derive,
    /// A builtin-in derive.
    DeriveBuiltIn,
    /// A procedural attribute macro.
    Attr,
    /// A built-in attribute macro.
    AttrBuiltIn,
    /// A function-like procedural macro.
    ProcMacro,
}

impl MacroCallId {
    /// The call: the macro call, the meta of the derive in its attribute, or the item an
    /// attribute macro is on.
    pub fn call_node_view(self, db: &Db) -> InFile<view::Node<'_>> {
        self.loc(db).to_node_view(db)
    }
    pub fn expansion_level(self, db: &Db) -> u32 {
        let mut level = 0;
        let mut macro_file = self;
        loop {
            let loc = macro_file.loc(db);

            level += 1;
            macro_file = match loc.kind.file_id() {
                HirFileId::FileId(_) => break level,
                HirFileId::MacroFile(it) => it,
            };
        }
    }
    pub fn parent(self, db: &Db) -> HirFileId {
        self.loc(db).kind.file_id()
    }

    /// Return expansion information if it is a macro-expansion file
    pub fn expansion_info(self, db: &Db) -> ExpansionInfo<'_> {
        ExpansionInfo::new(db, self)
    }

    pub fn kind(self, db: &Db) -> MacroKind {
        self.loc(db).def.kind.macro_kind()
    }

    pub fn is_include_macro(self, db: &Db) -> bool {
        self.loc(db).def.is_include()
    }

    pub fn is_include_like_macro(self, db: &Db) -> bool {
        self.loc(db).def.is_include_like()
    }

    pub fn is_env_or_option_env(self, db: &Db) -> bool {
        self.loc(db).def.is_env_or_option_env()
    }

    pub fn is_eager(self, db: &Db) -> bool {
        let loc = self.loc(db);
        matches!(loc.def.kind, MacroDefKind::BuiltInEager(..))
    }

    pub fn eager_arg(self, db: &Db) -> Option<MacroCallId> {
        let loc = self.loc(db);
        match &loc.kind {
            MacroCallKind::FnLike { eager, .. } => eager.as_ref().map(|it| it.arg_id),
            _ => None,
        }
    }

    pub fn is_derive_attr_pseudo_expansion(self, db: &Db) -> bool {
        let loc = self.loc(db);
        loc.def.is_attribute_derive()
    }
}

#[blaze_query::tracked]
impl MacroCallId {
    /// Implementation of [`HirFileId::parse_or_expand`] for the macro case.
    // FIXME: We should verify that the parsed node is one of the many macro node variants we expect
    // instead of having it be untyped
    #[blaze_query::tracked(returns(ref), lru = 512, mem_tag = Expand)]
    pub fn parse_macro_expansion(
        self,
        db: &Db,
    ) -> ExpandResult<(Parse<view::Untyped>, ExpansionSpanMap)> {
        let _p = tracing::info_span!("parse_macro_expansion").entered();
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
            let loc = self.loc(db);
            // A refused call's empty tree is no expression, pattern or type: parsed as one it
            // would add a parse error to the refusal. As statements it is an empty list, which
            // is no node of the kind its caller casts to, so the caller has none there.
            let expand_to = match self.arg_refusal(db, &loc.kind) {
                Some(_) if !matches!(loc.expand_to(), ExpandTo::Items) => ExpandTo::Statements,
                _ => loc.expand_to(),
            };
            let mbe::ValueResult { value: (tt, matched_arm, made), err } =
                self.macro_expand(db, frame, loc);

            let (parse, mut rev_token_map, semicolon) =
                token_tree_to_syntax_node(db, frame, tt.view(), expand_to);
            rev_token_map.matched_arm = matched_arm;
            rev_token_map.dropped_semicolon = semicolon;
            // alloc: only for an expansion whose `${concat(..)}`s made identifiers
            rev_token_map.made_idents = made.into_iter().collect();
            let err = err.or_else(|| ignored_semicolon(db, loc, semicolon));

            ExpandResult { value: (parse, rev_token_map), err }
        })
    }

    pub fn parse_macro_expansion_error(
        self,
        db: &Db,
    ) -> Option<ExpandResult<Arc<[SyntaxError]>>> {
        let e: ExpandResult<Arc<[SyntaxError]>> =
            self.parse_macro_expansion(db).as_ref().map(|it| Arc::from(it.0.errors()));
        if e.value.is_empty() && e.err.is_none() { None } else { Some(e) }
    }

    /// This resolves the [MacroCallId] to check if it is a derive macro if so get the [macro_arg] for the derive.
    /// Other wise return the [macro_arg] for the macro_call_id.
    ///
    /// This is not connected to the database so it does not cache the result. However, the inner [macro_arg] query is
    ///
    /// [macro_arg]: Self::macro_arg
    #[expect(deprecated, reason = "we are `macro_arg_considering_derives`")]
    pub fn macro_arg_considering_derives<'db>(
        self,
        db: &'db Db,
        kind: &MacroCallKind,
    ) -> MacroArg<'db> {
        let (tt, undo_info, span, _) = match kind {
            // Get the macro arg for the derive macro
            MacroCallKind::Derive { derive_macro_id, .. } => derive_macro_id.macro_arg(db),
            MacroCallKind::FnLike { eager: Some(_), .. } => {
                let MacroCallKind::FnLike { eager: Some(eager), .. } = &self.loc(db).kind else {
                    unreachable!("`kind` is this call's own");
                };
                return (&eager.arg, &NO_FIXUPS, eager.span);
            }
            _ => self.macro_arg(db),
        };
        (tt, undo_info, *span)
    }

    /// Why the call is not expanded at all: its argument's delimiters do not pair or it has
    /// none, so [`MacroCallId::macro_arg`] holds an empty tree where the argument would be.
    #[expect(deprecated, reason = "we are `macro_arg_considering_derives`'s companion")]
    fn arg_refusal(self, db: &Db, kind: &MacroCallKind) -> Option<ExpandError> {
        let refusal = match kind {
            MacroCallKind::Derive { derive_macro_id, .. } => &derive_macro_id.macro_arg(db).3,
            MacroCallKind::FnLike { eager: Some(_), .. } => return None,
            _ => &self.macro_arg(db).3,
        };
        refusal.clone() // clone: an `Option<ExpandError>`: an `Arc` count, only when refused
    }

    /// Lowers syntactic macro call to a token tree representation. That's a firewall
    /// query, only typing in the macro call itself changes the returned
    /// subtree.
    #[blaze_query::tracked(returns(ref), mem_tag = Expand)]
    #[deprecated = "calling this is incorrect, call `macro_arg_considering_derives` instead"]
    fn macro_arg(self, db: &Db) -> MacroArgResult {
        use syntax::view::AstNode as _;

        let loc = self.loc(db);

        if let MacroCallKind::FnLike { eager: Some(_), .. } = &loc.kind {
            unreachable!("an eager call's argument is its location's, read by `macro_arg_considering_derives`");
        }

        let map = loc.kind.file_id().span_map(db);

        let (is_derive, censor_item_tree_attr_ids, item_node, span) = match &loc.kind {
            MacroCallKind::FnLike { ast_id, .. } => {
                let node = ast_id.to_view(db);
                let path_range = node
                    .path()
                    .map_or_else(|| node.syntax().text_range(), |path| path.syntax().text_range());
                let span = map.span_for_range(path_range);

                // A refused call's tree is also its expansion: no tokens, not even delimiters.
                let refused = |why: &str| {
                    (
                        tt::TopSubtree::from_token_trees(
                            tt::Delimiter {
                                open: span,
                                close: span,
                                kind: tt::DelimiterKind::Invisible,
                            },
                            tt::TokenTreesView::empty(),
                        ),
                        SyntaxFixupUndoInfo::default(),
                        span,
                        Some(ExpandError::other(span, why)),
                    )
                };

                let Some(tt) = node.token_tree() else {
                    return refused(
                        "this macro call has no `(..)`, `[..]` or `{..}` argument, so it is not expanded",
                    );
                };
                let first = tt.left_delimiter_token().map(|it| it.kind()).unwrap_or(T!['(']);
                let last = tt.right_delimiter_token().map(|it| it.kind()).unwrap_or(T![.]);

                let mismatched_delimiters = !matches!(
                    (first, last),
                    (T!['('], T![')']) | (T!['['], T![']']) | (T!['{'], T!['}'])
                );
                if mismatched_delimiters {
                    // An unbalanced call is not expanded: matching its tokens can make
                    // pathological, deeply nested code. The expansion is refused, with why.
                    cov_mark::hit!(issue9358_bad_macro_stack_overflow);

                    // alloc: an error's message, on the error path
                    let why = match tt.right_delimiter_token() {
                        None => format!(
                            "this macro call's `{}` is never closed, so it is not expanded",
                            first.text()
                        ),
                        Some(_) => format!(
                            "this macro call opens with `{}` and closes with `{}`, so it is not expanded",
                            first.text(),
                            last.text()
                        ),
                    };
                    return refused(&why);
                }

                let mode = if loc.def.is_proc_macro() {
                    DocCommentDesugarMode::ProcMacro
                } else {
                    DocCommentDesugarMode::Mbe
                };
                // A procedural macro is handed a fragment as one group without delimiters, as a
                // `macro_rules` macro matches it as one token tree (rustc's transcriber wraps
                // it in `Delimiter::Invisible`, `transcribe_pnr` in
                // `compiler/rustc_expand/src/mbe/transcribe.rs`).
                let mut tt = match loc.def.kind {
                    MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..) => {
                        syntax_bridge::call_input(
                            tt.syntax(),
                            MacroRulesArgs(InOrder::new(map)),
                            span,
                            mode,
                        )
                    }
                    _ => syntax_bridge::call_input(tt.syntax(), InOrder::new(map), span, mode),
                };
                if loc.def.is_proc_macro() {
                    // proc macros expect their inputs without parentheses, MBEs expect it with them included
                    tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Invisible);
                }
                return (tt, SyntaxFixupUndoInfo::NONE, span, None);
            }
            // MacroCallKind::Derive should not be here. As we are getting the argument for the derive macro
            MacroCallKind::Derive { .. } => {
                unreachable!("`MacroCallId::macro_arg` called with `MacroCallKind::Derive`")
            }
            MacroCallKind::Attr { ast_id, censored_attr_ids: attr_ids, .. } => {
                let (_, attr) = attr_ids.invoc_attr().find_attr_range_with_source_view(
                    db,
                    loc.krate,
                    syntax::view::AnyHasAttrs::new(ast_id.to_view(db)),
                );
                let range = attr
                    .path()
                    .map(|path| path.syntax().text_range())
                    .unwrap_or_else(|| attr.syntax().text_range());
                let span = map.span_for_range(range);

                let is_derive = matches!(loc.def.kind, MacroDefKind::BuiltInAttr(_, expander) if expander.is_derive());
                (is_derive, &**attr_ids, ast_id.to_view(db), span)
            }
        };

        let fix_errors =
            loc.kind.file_id().has_syntax_error_in(db, item_node.syntax().text_range());
        let (mut tt, undo_info) = attr_macro_input_to_token_tree(
            db,
            item_node.syntax(),
            map,
            span,
            is_derive,
            censor_item_tree_attr_ids,
            loc.krate,
            fix_errors,
        );

        if loc.def.is_proc_macro() {
            // proc macros expect their inputs without parentheses, MBEs expect it with them included
            tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Invisible);
        }

        (tt, undo_info, span, None)
    }

    fn macro_expand<'db, 'f>(
        self,
        db: &'db Db,
        frame: &'f Frame,
        loc: &MacroCallLoc,
    ) -> ExpandResult<(Expansion<'db, 'f>, MatchedArmIndex, mbe::MadeIdents)> {
        let _p = tracing::info_span!("macro_expand").entered();

        let mut made = mbe::MadeIdents::new();
        let ExpandResult { value: (tt, matched_arm), err } = match loc.def.kind {
            MacroDefKind::ProcMacro(..) => {
                return self
                    .expand_proc_macro(db)
                    .as_ref()
                    .map(|it| (Expansion::Borrowed(it), None, mbe::MadeIdents::new()));
            }
            _ => {
                let (macro_arg, undo_info, span) =
                    self.macro_arg_considering_derives(db, &loc.kind);
                if let Some(refusal) = self.arg_refusal(db, &loc.kind) {
                    return ExpandResult::new(
                        (Expansion::Borrowed(macro_arg), None, mbe::MadeIdents::new()),
                        refusal,
                    );
                }

                let arg = macro_arg;
                let res = match loc.def.kind {
                    MacroDefKind::Declarative(id, _) => id
                        .decl_macro_expander(db, loc.def.krate)
                        .expand_making(db, frame, arg, self, span)
                        .map(|(tt, arm, idents)| {
                            made = idents;
                            (Expansion::Framed(tt), arm)
                        }),
                    MacroDefKind::BuiltIn(_, it) => it
                        .expand(db, frame, self, arg, span)
                        .map_err(Into::into)
                        .map(Expansion::Framed)
                        .zip_val(None),
                    MacroDefKind::BuiltInDerive(_, it) => {
                        let ExpandResult { value, err } = it.expand(db, frame, self, arg, span);
                        let at_call = |it: &ExpandError| it.span().range == span.range && it.span().anchor == span.anchor;
                        let err = match err {
                            Some(err) if at_call(&err) => match derive_path_span(db, loc) {
                                Some(path) => Some(err.at(path)),
                                None => Some(err),
                            },
                            err => err,
                        };
                        ExpandResult { value: Expansion::Framed(value), err }.zip_val(None)
                    }
                    MacroDefKind::UnimplementedBuiltIn(_) => {
                        expand_unimplemented_builtin_macro(frame, span)
                            .map(Expansion::Framed)
                            .zip_val(None)
                    }
                    MacroDefKind::BuiltInEager(_, it) => {
                        // This might look a bit odd, but we do not expand the inputs to eager macros here.
                        // Eager macros inputs are expanded, well, eagerly when we collect the macro calls.
                        // That kind of expansion uses the ast id map of an eager macros input though which goes through
                        // the HirFileId machinery. As eager macro inputs are assigned a macro file id that query
                        // will end up going through here again, whereas we want to just want to inspect the raw input.
                        // As such we just return the input subtree here.
                        let eager = match &loc.kind {
                            MacroCallKind::FnLike { eager: None, .. } => {
                                return ExpandResult::ok((
                                    Expansion::Borrowed(macro_arg),
                                    None,
                                    mbe::MadeIdents::new(),
                                ));
                            }
                            MacroCallKind::FnLike { eager: Some(eager), .. } => Some(&**eager),
                            _ => None,
                        };

                        let mut res = it.expand(db, frame, self, arg, span).map_err(Into::into);

                        if let Some(EagerCallInfo { error, .. }) = eager {
                            // FIXME: We should report both errors!
                            res.err = error.clone().or(res.err); // clone: an `Option<ExpandError>`: an `Arc` count, only when the eager input had an error
                        }
                        res.map(Expansion::Framed).zip_val(None)
                    }
                    MacroDefKind::BuiltInAttr(_, it) => {
                        let mut res = it.expand(db, frame, self, arg, span);
                        fixup::reverse_fixups(&mut res.value, undo_info);
                        res.map(Expansion::Framed).zip_val(None)
                    }
                    MacroDefKind::ProcMacro(_, _, _) => unreachable!(),
                };
                res
            }
        };

        ExpandResult { value: (tt, matched_arm, made), err }
    }

    /// Special case of [`Self::macro_expand`] for procedural macros. We can't LRU
    /// proc macros, since they are not deterministic in general, and
    /// non-determinism breaks the query engine in a very, very, very bad way.
    /// @edwin0cheng heroically debugged this once! See #4315 for details
    #[blaze_query::tracked(returns(ref), mem_tag = Expand)]
    fn proc_macro_expansion(
        self,
        db: &Db,
    ) -> (ExpandResult<tt::TopSubtree>, proc_macro::ExpansionReads) {
        let loc = self.loc(db);
        let (macro_arg, undo_info, _) = self.macro_arg_considering_derives(db, &loc.kind);
        if let Some(refusal) = self.arg_refusal(db, &loc.kind) {
            let empty = tt::TopSubtree::empty(macro_arg.top_subtree().delimiter.delim_span());
            return (ExpandResult::new(empty, refusal), proc_macro::ExpansionReads::Nothing);
        }

        let (ast, expander) = match loc.def.kind {
            MacroDefKind::ProcMacro(ast, expander, _) => (ast, expander),
            _ => unreachable!(),
        };

        let attr_arg = match &loc.kind {
            MacroCallKind::Attr { attr_args: Some(attr_args), .. } => Some(&**attr_args),
            _ => None,
        };

        // A report left by an expansion that panicked is not this one's.
        proc_macro::take_reads();
        let ExpandResult { value: mut tt, err } = {
            let span = proc_macro_span(db, ast);
            expander.expand(
                db,
                loc.def.krate,
                loc.krate,
                macro_arg,
                attr_arg,
                span_with_def_site_ctxt(db, span, self.into(), loc.def.edition),
                span_with_call_site_ctxt(db, span, self.into(), loc.def.edition),
                span_with_mixed_site_ctxt(db, span, self.into(), loc.def.edition),
            )
        };
        let reported = proc_macro::take_reads();
        let reads = match expander.requires_session(db, loc.def.krate) {
            true => proc_macro::ExpansionReads::Unknown(
                "its macro keeps a session: an expansion depends on the calls before it",
            ),
            false => reported.unwrap_or(proc_macro::ExpansionReads::Unknown(
                "its expander reported nothing of what it read",
            )),
        };

        fixup::reverse_fixups(&mut tt, undo_info);

        (ExpandResult { value: tt, err }, reads)
    }
}

impl MacroCallId {
    /// Special case of [`Self::macro_expand`] for procedural macros.
    fn expand_proc_macro(self, db: &Db) -> &ExpandResult<tt::TopSubtree> {
        &self.proc_macro_expansion(db).0
    }

    /// What the expansion of this proc-macro call read besides its call, as its expander
    /// reported it (`proc_macro::report_reads`). Panics for a call of another kind of macro.
    pub fn expansion_reads(self, db: &Db) -> &proc_macro::ExpansionReads {
        &self.proc_macro_expansion(db).1
    }
}

fn expand_unimplemented_builtin_macro(
    frame: &Frame,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&Frame>> {
    ExpandResult::new(
        tt::TopSubtree::empty_in(tt::DelimSpan::from_single(span), frame),
        ExpandError::other(span, "this built-in macro is not implemented"),
    )
}

/// Retrieves the span to be used for a proc-macro expansions spans.
/// This is a firewall query as it requires parsing the file, which we don't want proc-macros to
/// directly depend on as that would cause to frequent invalidations, mainly because of the
/// parse queries being LRU cached. If they weren't the invalidations would only happen if the
/// user wrote in the file that defines the proc-macro.
fn proc_macro_span(db: &Db, ast: AstId<ast::Fn>) -> Span {
    #[blaze_query::tracked(returns(copy), mem_tag = Expand)]
    fn proc_macro_span(db: &Db, ast: AstId<ast::Fn>, _: ()) -> Span {
        let span_map = ast.file_id.span_map(db);
        let node = ast.to_view(db);
        let range = view::HasName::name(&node)
            .map_or_else(|| node.syntax().text_range(), |name| name.syntax().text_range());
        span_map.span_for_range(range)
    }
    proc_macro_span(db, ast, ())
}

/// The syntax `tt` parses to as `expand_to`, and the span of the `;` an expression's expansion
/// ends in, which is left out of the parse.
pub(crate) fn token_tree_to_syntax_node(
    db: &Db,
    frame: &Frame,
    tt: tt::SubtreeView<'_>,
    expand_to: ExpandTo,
) -> (Parse<view::Untyped>, ExpansionSpanMap, Option<Span>) {
    let entry_point = match expand_to {
        ExpandTo::Statements => syntax_bridge::TopEntryPoint::MacroStmts,
        ExpandTo::Items => syntax_bridge::TopEntryPoint::MacroItems,
        ExpandTo::Pattern => syntax_bridge::TopEntryPoint::Pattern,
        ExpandTo::Type => syntax_bridge::TopEntryPoint::Type,
        ExpandTo::Expr => syntax_bridge::TopEntryPoint::Expr,
    };
    let unterminated = match expand_to {
        ExpandTo::Expr => without_last_semicolon(frame, tt),
        _ => None,
    };
    let (tt, semicolon) = match &unterminated {
        Some((tt, semicolon)) => (tt.view(), Some(*semicolon)),
        None => (tt, None),
    };
    let (parse, map) =
        syntax_bridge::token_tree_to_syntax_node(tt, entry_point, &mut |ctx| ctx.edition(db));
    (parse, map, semicolon)
}

/// rustc's error for the `;` an expression's expansion ends in, at `semicolon`, when the macro
/// is not a `macro_rules!` one: `ParserAnyMacro::make` drops the `;` only for those, and for
/// any other macro `ensure_complete_parse` reports it, keeping the expression before it.
fn ignored_semicolon(
    db: &Db,
    loc: &MacroCallLoc,
    semicolon: Option<Span>,
) -> Option<ExpandError> {
    let semicolon = semicolon?;
    let eager_input = matches!(
        (&loc.def.kind, &loc.kind),
        (MacroDefKind::BuiltInEager(..), MacroCallKind::FnLike { eager: None, .. })
    );
    if eager_input || matches!(loc.def.kind, MacroDefKind::Declarative(..)) {
        return None;
    }
    let mut error = BuiltinError::new("macro expansion ignores `;` and any tokens following");
    if let MacroCallKind::FnLike { ast_id, .. } = &loc.kind
        && let Some(path) = ast_id.to_view(db).path()
    {
        use syntax::view::AstNode as _;
        // alloc: the path as rustc prints it, on the error path
        let mut written = String::new();
        let tokens = path.syntax().descendants_with_tokens().filter_map(|it| it.into_token());
        for token in tokens.filter(|it| !it.kind().is_trivia()) {
            written.push_str(token.text());
        }
        // alloc: as above
        let note = format!("the usage of `{written}!` is likely invalid in expression context");
        error = error.note(note);
    }
    Some(error.at(semicolon))
}

/// The tokens before the semicolon an expansion ends in, and the semicolon's span: what rustc
/// drops where a `macro_rules!` macro is called as an expression.
fn without_last_semicolon<'f>(
    frame: &'f Frame,
    tt: tt::SubtreeView<'_>,
) -> Option<(tt::TopSubtree<&'f Frame>, Span)> {
    let mut rest = tt.token_trees().iter();
    let start = rest.savepoint();
    let mut before = rest.from_savepoint(start);
    let mut last = None;
    loop {
        let seen = rest.from_savepoint(start);
        let Some(token) = rest.next() else { break };
        before = seen;
        last = Some(token);
    }
    match last? {
        tt::TtElement::Leaf(tt::Leaf::Punct(punct)) if punct.char == ';' => Some((
            tt::TopSubtree::from_token_trees_in(tt.top_subtree().delimiter, before, frame),
            punct.span,
        )),
        _ => None,
    }
}

impl MacroDefId {
    pub fn make_call(
        self,
        db: &Db,
        krate: Crate,
        kind: MacroCallKind,
        ctxt: SyntaxContext,
        macro_depth: u32,
    ) -> MacroCallId {
        MacroCallId::new(db, MacroCallLoc { def: self, krate, kind, ctxt, macro_depth })
    }

    pub fn definition_range(&self, db: &Db) -> InFile<TextRange> {
        match self.kind {
            MacroDefKind::Declarative(id, _)
            | MacroDefKind::BuiltIn(id, _)
            | MacroDefKind::BuiltInAttr(id, _)
            | MacroDefKind::BuiltInDerive(id, _)
            | MacroDefKind::BuiltInEager(id, _)
            | MacroDefKind::UnimplementedBuiltIn(id) => {
                id.with_value(id.file_id.ast_id_map(db).get(id.value).text_range())
            }
            MacroDefKind::ProcMacro(id, _, _) => {
                id.with_value(id.file_id.ast_id_map(db).get(id.value).text_range())
            }
        }
    }

    pub fn ast_id(&self) -> Either<AstId<ast::Macro>, AstId<ast::Fn>> {
        match self.kind {
            MacroDefKind::ProcMacro(id, ..) => Either::Right(id),
            MacroDefKind::Declarative(id, _)
            | MacroDefKind::BuiltIn(id, _)
            | MacroDefKind::BuiltInAttr(id, _)
            | MacroDefKind::BuiltInDerive(id, _)
            | MacroDefKind::BuiltInEager(id, _)
            | MacroDefKind::UnimplementedBuiltIn(id) => Either::Left(id),
        }
    }

    pub fn is_proc_macro(&self) -> bool {
        matches!(self.kind, MacroDefKind::ProcMacro(..))
    }

    pub fn is_attribute(&self) -> bool {
        match self.kind {
            MacroDefKind::BuiltInAttr(..)
            | MacroDefKind::ProcMacro(_, _, ProcMacroKind::Attr)
            | MacroDefKind::UnimplementedBuiltIn(_) => true,
            MacroDefKind::Declarative(_, styles) => styles.contains(MacroCallStyles::ATTR),
            _ => false,
        }
    }

    pub fn is_derive(&self) -> bool {
        match self.kind {
            MacroDefKind::BuiltInDerive(..)
            | MacroDefKind::ProcMacro(_, _, ProcMacroKind::CustomDerive)
            | MacroDefKind::UnimplementedBuiltIn(_) => true,
            MacroDefKind::Declarative(_, styles) => styles.contains(MacroCallStyles::DERIVE),
            _ => false,
        }
    }

    pub fn is_fn_like(&self) -> bool {
        matches!(
            self.kind,
            MacroDefKind::BuiltIn(..)
                | MacroDefKind::ProcMacro(_, _, ProcMacroKind::Bang)
                | MacroDefKind::BuiltInEager(..)
                | MacroDefKind::Declarative(..)
                | MacroDefKind::UnimplementedBuiltIn(_)
        )
    }

    pub fn is_attribute_derive(&self) -> bool {
        matches!(self.kind, MacroDefKind::BuiltInAttr(_, expander) if expander.is_derive())
    }

    pub fn is_include(&self) -> bool {
        matches!(self.kind, MacroDefKind::BuiltInEager(_, expander) if expander.is_include())
    }

    pub fn is_include_like(&self) -> bool {
        matches!(self.kind, MacroDefKind::BuiltInEager(_, expander) if expander.is_include_like())
    }

    pub fn is_env_or_option_env(&self) -> bool {
        matches!(self.kind, MacroDefKind::BuiltInEager(_, expander) if expander.is_env_or_option_env())
    }
}

/// Where rustc puts a built-in derive's own errors: the path of its trait in its
/// `#[derive(..)]` (`From` of `#[derive(From)]`, `core::From` of `#[derive(core::From)]`).
/// `None` for a call that is no derive, or whose attribute names fewer paths than its index:
/// its errors stay at the attribute.
fn derive_path_span(db: &Db, loc: &MacroCallLoc) -> Option<Span> {
    let MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } = &loc.kind else { return None };
    let (_, meta) = derive_attr_index.find_attr_range_view(db, loc.krate, *ast_id);
    let (_, tt) = meta.as_simple_call()?;
    // The paths are counted as the collector numbers them: each run between commas that
    // names something.
    let (mut at, mut first, mut last) = (0, None::<TextRange>, None::<TextRange>);
    for token in tt.syntax().children_with_tokens().filter_map(|it| it.into_token()) {
        match token.kind() {
            T![,] | T![')'] => {
                if let (Some(first), Some(last)) = (first.take(), last.take()) {
                    if at == *derive_index {
                        let range = TextRange::new(first.start(), last.end());
                        return Some(ast_id.file_id.span_map(db).span_for_range(range));
                    }
                    at += 1;
                }
            }
            T!['('] | syntax::SyntaxKind::WHITESPACE | syntax::SyntaxKind::COMMENT => {}
            _ => {
                first.get_or_insert(token.text_range());
                last = Some(token.text_range());
            }
        }
    }
    None
}

impl MacroCallLoc {
    /// The call, the meta of the derive in its attribute, or the item an attribute macro is on.
    pub fn to_node_view<'db>(&self, db: &'db Db) -> InFile<view::Node<'db>> {
        match &self.kind {
            MacroCallKind::FnLike { ast_id, .. } => ast_id.with_value(ast_id.to_view(db).syntax()),
            MacroCallKind::Derive { ast_id, derive_attr_index, .. } => {
                let (_, meta) = derive_attr_index.find_attr_range_view(db, self.krate, *ast_id);
                ast_id.with_value(meta.syntax())
            }
            MacroCallKind::Attr { ast_id, censored_attr_ids: attr_ids, .. } => {
                if self.def.is_attribute_derive() {
                    let (_, meta) =
                        attr_ids.invoc_attr().find_attr_range_view(db, self.krate, *ast_id);
                    ast_id.with_value(meta.syntax())
                } else {
                    ast_id.with_value(ast_id.to_view(db).syntax())
                }
            }
        }
    }

    /// The item the call is in: the macro call item, or the item a derive or attribute is on.
    pub fn to_node_item_view<'db>(&self, db: &'db Db) -> InFile<view::Item<'db>> {
        match self.kind {
            MacroCallKind::FnLike { ast_id, .. } => {
                InFile::new(ast_id.file_id, ast_id.upcast::<ast::Item>().to_view(db))
            }
            MacroCallKind::Derive { ast_id, .. } => {
                InFile::new(ast_id.file_id, ast_id.upcast::<ast::Item>().to_view(db))
            }
            MacroCallKind::Attr { ast_id, .. } => InFile::new(ast_id.file_id, ast_id.to_view(db)),
        }
    }

    fn expand_to(&self) -> ExpandTo {
        match self.kind {
            MacroCallKind::FnLike { expand_to, .. } => expand_to,
            MacroCallKind::Derive { .. } => ExpandTo::Items,
            MacroCallKind::Attr { .. } if self.def.is_attribute_derive() => ExpandTo::Items,
            MacroCallKind::Attr { .. } => {
                // FIXME(stmt_expr_attributes)
                ExpandTo::Items
            }
        }
    }

    pub fn include_file_id(
        &self,
        db: &Db,
        macro_call_id: MacroCallId,
    ) -> Option<EditionedFileId> {
        if self.def.is_include()
            && let MacroCallKind::FnLike { eager: Some(eager), .. } = &self.kind
            && let Ok(it) = include_input_to_file_id(db, macro_call_id, &eager.arg)
        {
            return Some(it);
        }

        None
    }
}

impl MacroCallKind {
    pub fn descr(&self) -> &'static str {
        match self {
            MacroCallKind::FnLike { .. } => "macro call",
            MacroCallKind::Derive { .. } => "derive macro",
            MacroCallKind::Attr { .. } => "attribute macro",
        }
    }

    /// Returns the file containing the macro invocation.
    pub fn file_id(&self) -> HirFileId {
        match *self {
            MacroCallKind::FnLike { ast_id: InFile { file_id, .. }, .. }
            | MacroCallKind::Derive { ast_id: InFile { file_id, .. }, .. }
            | MacroCallKind::Attr { ast_id: InFile { file_id, .. }, .. } => file_id,
        }
    }

    pub fn erased_ast_id(&self) -> ErasedFileAstId {
        match *self {
            MacroCallKind::FnLike { ast_id: InFile { value, .. }, .. } => value.erase(),
            MacroCallKind::Derive { ast_id: InFile { value, .. }, .. } => value.erase(),
            MacroCallKind::Attr { ast_id: InFile { value, .. }, .. } => value.erase(),
        }
    }

    /// Returns the original file range that best describes the location of this macro call.
    ///
    /// This spans the entire macro call, including its input. That is for
    /// - fn_like! {}, it spans the path and token tree
    /// - #\[derive], it spans the `#[derive(...)]` attribute and the annotated item
    /// - #\[attr], it spans the `#[attr(...)]` attribute and the annotated item
    pub fn original_call_range_with_input(&self, db: &Db) -> FileRange {
        let get_range = |kind: &_| match kind {
            MacroCallKind::FnLike { ast_id, .. } => ast_id.erase(),
            MacroCallKind::Derive { ast_id, .. } => ast_id.erase(),
            MacroCallKind::Attr { ast_id, .. } => ast_id.erase(),
        };

        let mut ast_id = get_range(self);
        let mut file_id = self.file_id();
        let file_id = loop {
            match file_id {
                HirFileId::MacroFile(file) => {
                    let kind = &file.loc(db).kind;
                    ast_id = get_range(kind);
                    file_id = kind.file_id();
                }
                HirFileId::FileId(file_id) => break file_id,
            }
        };

        FileRange { range: ast_id.to_ptr(db).text_range(), file_id }
    }

    /// Returns the original file range that best describes the location of this macro call.
    ///
    /// Here we try to roughly match what rustc does to improve diagnostics: fn-like macros
    /// get the macro path (rustc shows the whole `ast::MacroCall`), attribute macros get the
    /// attribute's range, and derives get only the specific derive that is being referred to.
    pub fn original_call_range(&self, db: &Db, krate: Crate) -> FileRange {
        let get_range = |kind: &_| match kind {
            MacroCallKind::FnLike { ast_id, .. } => {
                let node = ast_id.to_view(db);
                node.path()
                    .unwrap()
                    .syntax()
                    .text_range()
                    .cover(node.excl_token().unwrap().text_range())
            }
            MacroCallKind::Derive { ast_id, derive_attr_index, .. } => {
                // FIXME: should be the range of the macro name, not the whole derive
                derive_attr_index.find_attr_range_view(db, krate, *ast_id).1.syntax().text_range()
            }
            // FIXME: handle `cfg_attr`
            MacroCallKind::Attr { ast_id, censored_attr_ids: attr_ids, .. } => {
                attr_ids
                    .invoc_attr()
                    .find_attr_range_view(db, krate, *ast_id)
                    .1
                    .syntax()
                    .text_range()
            }
        };

        let mut range = get_range(self);
        let mut file_id = self.file_id();
        let file_id = loop {
            match file_id {
                HirFileId::MacroFile(file) => {
                    let kind = &file.loc(db).kind;
                    range = get_range(kind);
                    file_id = kind.file_id();
                }
                HirFileId::FileId(file_id) => break file_id,
            }
        };

        FileRange { range, file_id }
    }

    /// The macro's input: the token tree of a call, or the item of a derive or attribute.
    fn arg_view<'db>(&self, db: &'db Db) -> InFile<Option<view::Node<'db>>> {
        match self {
            MacroCallKind::FnLike { ast_id, .. } => {
                ast_id.to_in_file_view(db).map(|it| Some(it.token_tree()?.syntax()))
            }
            MacroCallKind::Derive { ast_id, .. } => {
                ast_id.to_in_file_view(db).map(|it| Some(it.syntax()))
            }
            MacroCallKind::Attr { ast_id, .. } => {
                ast_id.to_in_file_view(db).map(|it| Some(it.syntax()))
            }
        }
    }
}

/// ExpansionInfo mainly describes how to map text range between src and expanded macro
// FIXME: can be expensive to create, we should check the use sites and maybe replace them with
// simpler function calls if the map is only used once
#[derive(Clone, Debug)]
pub struct ExpansionInfo<'db> {
    expanded_file: MacroCallId,
    /// The expansion's root and the argument TokenTree or item for attributes, as views of the
    /// trees the database keeps.
    expanded_root: view::Node<'db>,
    arg_file: HirFileId,
    arg_root: Option<view::Node<'db>>,
    exp_map: &'db ExpansionSpanMap,
    arg_map: SpanMap<'db>,
    loc: &'db MacroCallLoc,
}

/// Every field is a function of the macro file.
impl PartialEq for ExpansionInfo<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.expanded_file == other.expanded_file
    }
}

impl Eq for ExpansionInfo<'_> {}

impl<'db> ExpansionInfo<'db> {
    /// The expansion's root.
    pub fn expanded_view(&self) -> InMacroFile<view::Node<'db>> {
        InMacroFile::new(self.expanded_file, self.expanded_root)
    }

    /// The macro's input, in the file of the call.
    pub fn arg_view(&self) -> InFile<Option<view::Node<'db>>> {
        InFile::new(self.arg_file, self.arg_root)
    }

    /// Maps `span` down into the expansion, to tokens of its view: only the ranges whose span is
    /// `span` exactly.
    pub fn map_range_down_exact_view(
        &self,
        span: Span,
    ) -> Option<InMacroFile<impl Iterator<Item = (view::Token<'db>, SyntaxContext)> + '_>> {
        if span.anchor.ast_id == NO_DOWNMAP_ERASED_FILE_AST_ID_MARKER {
            return None;
        }

        let root = self.expanded_root;
        let tokens = self.exp_map.ranges_with_span_exact(span).flat_map(move |(range, ctx)| {
            root.covering_element(range).into_token().zip(Some(ctx))
        });

        Some(InMacroFile::new(self.expanded_file, tokens))
    }

    /// Maps the passed in file range down into a macro expansion if it is the input to a macro call.
    /// Unlike [`ExpansionInfo::map_range_down_exact_view`], this will consider spans that contain the given span.
    ///
    /// A binary search of the span map's index by anchor, then the run of entries in it.
    pub fn map_range_down_view(
        &self,
        span: Span,
    ) -> Option<InMacroFile<impl Iterator<Item = (view::Token<'db>, SyntaxContext)> + '_>> {
        if span.anchor.ast_id == NO_DOWNMAP_ERASED_FILE_AST_ID_MARKER {
            return None;
        }

        let root = self.expanded_root;
        let tokens = self.exp_map.ranges_with_span(span).flat_map(move |(range, ctx)| {
            root.covering_element(range).into_token().zip(Some(ctx))
        });

        Some(InMacroFile::new(self.expanded_file, tokens))
    }

    pub fn call_file(&self) -> HirFileId {
        self.arg_file
    }

    pub fn is_attr(&self) -> bool {
        matches!(
            self.loc.def.kind,
            MacroDefKind::BuiltInAttr(..) | MacroDefKind::ProcMacro(_, _, ProcMacroKind::Attr)
        )
    }

    /// Looks up the span at the given offset.
    pub fn span_for_offset(
        &self,
        db: &Db,
        offset: TextSize,
    ) -> (FileRange, SyntaxContext) {
        debug_assert!(self.expanded_root.text_range().contains(offset));
        span_for_offset(db, self.exp_map, offset)
    }

    /// Maps up the text range out of the expansion hierarchy back into the original file its from.
    pub fn map_node_range_up(
        &self,
        db: &Db,
        range: TextRange,
    ) -> Option<(FileRange, SyntaxContext)> {
        debug_assert!(self.expanded_root.text_range().contains_range(range));
        map_node_range_up(db, self.exp_map, range)
    }

    /// Maps up the text range out of the expansion into its macro call.
    ///
    /// Note that this may return multiple ranges as we lose the precise association between input to output
    /// and as such we may consider inputs that are unrelated.
    pub fn map_range_up_once(
        &self,
        db: &Db,
        token: TextRange,
    ) -> InFile<smallvec::SmallVec<[TextRange; 1]>> {
        debug_assert!(self.expanded_root.text_range().contains_range(token));
        let span = self.exp_map.span_at(token.start());
        match &self.arg_map {
            SpanMap::RealSpanMap(_) => {
                let range = resolve_span(db, span);
                InFile { file_id: range.file_id.into(), value: smallvec::SmallVec::from_buf([range.range]) }
            }
            SpanMap::ExpansionSpanMap(arg_map) => {
                let Some(arg_node) = self.arg_root else {
                    return InFile::new(self.arg_file, smallvec::SmallVec::new());
                };
                let arg_range = arg_node.text_range();
                InFile::new(
                    self.arg_file,
                    arg_map
                        .ranges_with_span_exact(span)
                        .map(|(range, _)| range)
                        .filter(|range| range.intersect(arg_range).is_some())
                        .collect(), // alloc: none for one range (inline); IDE up-mapping
                )
            }
        }
    }

    pub fn new(db: &'db Db, macro_file: MacroCallId) -> ExpansionInfo<'db> {
        let _p = tracing::info_span!("ExpansionInfo::new").entered();
        let loc = macro_file.loc(db);

        let arg = loc.kind.arg_view(db);
        let arg_map = arg.file_id.span_map(db);

        let (parse, exp_map) = &macro_file.parse_macro_expansion(db).value;

        ExpansionInfo {
            expanded_file: macro_file,
            expanded_root: parse.root(),
            arg_file: arg.file_id,
            arg_root: arg.value,
            loc,
            exp_map,
            arg_map,
        }
    }
}

/// Maps up the text range out of the expansion hierarchy back into the original file its from only
/// considering the root spans contained.
/// Unlike [`map_node_range_up`], this will not return `None` if any anchors or syntax contexts differ.
pub fn map_node_range_up_rooted(
    db: &Db,
    exp_map: &ExpansionSpanMap,
    range: TextRange,
) -> Option<FileRange> {
    map_node_range_up_rooted_span(exp_map, range).map(|span| resolve_span(db, span))
}

/// [`map_node_range_up_rooted`] as a span, relative to its anchor: what does not change while
/// edits around the anchor move it.
pub fn map_node_range_up_rooted_span(exp_map: &ExpansionSpanMap, range: TextRange) -> Option<Span> {
    // A node that is exactly a macro fragment is at its metavariable, which no token holds.
    if let Some(span) = exp_map.fragment_span(range)
        && span.ctx.is_root()
    {
        return Some(span);
    }
    let mut spans = exp_map.spans_for_range(range).filter(|span| span.ctx.is_root());
    let Span { range, anchor, ctx } = spans.next()?;
    let mut start = range.start();
    let mut end = range.end();

    for span in spans {
        if span.anchor != anchor {
            return None;
        }
        start = start.min(span.range.start());
        end = end.max(span.range.end());
    }
    Some(Span { range: TextRange::new(start, end), anchor, ctx })
}

/// Maps up the text range out of the expansion hierarchy back into the original file its from.
///
/// this will return `None` if any anchors or syntax contexts differ.
pub fn map_node_range_up(
    db: &Db,
    exp_map: &ExpansionSpanMap,
    range: TextRange,
) -> Option<(FileRange, SyntaxContext)> {
    // A node that is exactly a macro fragment is at its metavariable, which no token holds.
    if let Some(span) = exp_map.fragment_span(range) {
        return Some((resolve_span(db, span), span.ctx));
    }
    // A fragment inside the node is its metavariable too (rustc spans a substituted `$e` as the
    // invisible group's span): `let $p = $e` is the macro's, though `$p` and `$e` hold the call
    // site's tokens. What is between the fragments is the tokens'.
    let mut merged: Option<Span> = None;
    let mut merge = |span: Span| match &mut merged {
        None => {
            merged = Some(span);
            true
        }
        Some(it) if it.anchor == span.anchor && it.ctx == span.ctx => {
            it.range = it.range.cover(span.range);
            true
        }
        Some(_) => false,
    };
    let gap = |from: TextSize, to: TextSize| {
        (from < to).then(|| exp_map.spans_for_range(TextRange::new(from, to))).into_iter().flatten()
    };
    let mut from = range.start();
    for (fragment, span) in exp_map.outermost_fragments_in(range) {
        if !gap(from, fragment.start()).all(&mut merge) || !merge(span) {
            return None;
        }
        from = fragment.end();
    }
    if !gap(from, range.end()).all(&mut merge) {
        return None;
    }
    let span = merged?;
    Some((resolve_span(db, span), span.ctx))
}

/// Looks up the span at the given offset.
pub fn span_for_offset(
    db: &Db,
    exp_map: &ExpansionSpanMap,
    offset: TextSize,
) -> (FileRange, SyntaxContext) {
    let span = exp_map.span_at(offset);
    (resolve_span(db, span), span.ctx)
}

// FIXME: This is only public because of its use in `blaze_lower::answer` (which we should consider
// removing by moving the implementations of the subrequests to `hir_expand`, and calling within
// blaze-lower).
// Avoid adding any more outside uses.
pub fn resolve_span(db: &Db, Span { range, anchor, ctx: _ }: Span) -> FileRange {
    let file_id = EditionedFileId::from_span_file_id(db, anchor.file_id);
    let anchor_offset =
        HirFileId::from(file_id).ast_id_map(db).get_erased(anchor.ast_id).text_range().start();
    FileRange { file_id, range: range + anchor_offset }
}

/// In Rust, macros expand token trees to token trees. When we want to turn a
/// token tree into an AST node, we need to figure out what kind of AST node we
/// want: something like `foo` can be a type, an expression, or a pattern.
///
/// Naively, one would think that "what this expands to" is a property of a
/// particular macro: macro `m1` returns an item, while macro `m2` returns an
/// expression, etc. That's not the case -- macros are polymorphic in the
/// result, and can expand to any type of the AST node.
///
/// What defines the actual AST node is the syntactic context of the macro
/// invocation. As a contrived example, in `let T![*] = T![*];` the first `T`
/// expands to a pattern, while the second one expands to an expression.
///
/// `ExpandTo` captures this bit of information about a particular macro call
/// site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpandTo {
    Statements,
    Items,
    Pattern,
    Type,
    Expr,
}

impl ExpandTo {
    /// What the macro call `call` expands to where it is; the kind of its parent if no macro
    /// call can be there.
    pub fn of_call_site_view(call: view::MacroCall<'_>) -> Result<ExpandTo, syntax::SyntaxKind> {
        use syntax::SyntaxKind::*;

        let syn = call.syntax();

        let parent = match syn.parent() {
            Some(it) => it,
            None => return Ok(ExpandTo::Statements),
        };

        // FIXME: macros in statement position are treated as expression statements, they should
        // probably be their own statement kind. The *grand*parent indicates what's valid.
        if parent.kind() == MACRO_EXPR
            && let Some(grandparent) = parent.parent()
            && matches!(grandparent.kind(), EXPR_STMT | STMT_LIST | MACRO_STMTS)
        {
            // rustc's `parse_stmt_mac`: the last call of a block, not in braces and with no `;`
            // after it, is followed by the block's `}`, so it is an expression. The last call
            // of a statements expansion is followed by the end of its tokens: a statement.
            let braced = call.token_tree().and_then(|it| it.l_curly_token()).is_some();
            let is_tail = view::StmtList::cast(grandparent)
                .and_then(|list| list.tail_expr())
                .is_some_and(|tail| tail.syntax() == parent);
            return Ok(if is_tail && !braced { ExpandTo::Expr } else { ExpandTo::Statements });
        }

        ExpandTo::of_parent_kind(parent.kind())
    }

    /// What a macro call whose parent is a `kind` expands to, past the statement case.
    fn of_parent_kind(kind: syntax::SyntaxKind) -> Result<ExpandTo, syntax::SyntaxKind> {
        use syntax::SyntaxKind::*;

        Ok(match kind {
            MACRO_ITEMS | SOURCE_FILE | ITEM_LIST | ASSOC_ITEM_LIST | EXTERN_ITEM_LIST => {
                ExpandTo::Items
            }
            MACRO_STMTS | EXPR_STMT | STMT_LIST => ExpandTo::Statements,
            MACRO_PAT => ExpandTo::Pattern,
            MACRO_TYPE => ExpandTo::Type,

            ARG_LIST | ARRAY_EXPR | AWAIT_EXPR | BIN_EXPR | BREAK_EXPR | CALL_EXPR | CAST_EXPR
            | CLOSURE_EXPR | FIELD_EXPR | FOR_EXPR | IF_EXPR | INDEX_EXPR | LET_EXPR
            | MATCH_ARM | MATCH_EXPR | MATCH_GUARD | METHOD_CALL_EXPR | PAREN_EXPR | PATH_EXPR
            | PREFIX_EXPR | RANGE_EXPR | RECORD_EXPR_FIELD | REF_EXPR | RETURN_EXPR | TRY_EXPR
            | TUPLE_EXPR | WHILE_EXPR | MACRO_EXPR => ExpandTo::Expr,
            other => return Err(other),
        })
    }
}

/// Macro ids. That's probably the tricksiest bit in rust-analyzer, and the
/// reason why we use a query engine at all.
///
/// We encode macro definitions into ids of macro calls, this what allows us
/// to be incremental.
#[blaze_query::interned(unsafe(no_lifetime), debug)]
#[doc(alias = "MacroFileId")]
pub struct MacroCallId {
    #[returns(ref)]
    pub loc: MacroCallLoc,
}

impl From<span::MacroCallId> for MacroCallId {
    #[inline]
    fn from(value: span::MacroCallId) -> Self {
        MacroCallId::from_id(value.0)
    }
}

impl From<MacroCallId> for span::MacroCallId {
    #[inline]
    fn from(value: MacroCallId) -> span::MacroCallId {
        span::MacroCallId(value.as_id())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum HirFileId {
    FileId(EditionedFileId),
    MacroFile(MacroCallId),
}

impl From<EditionedFileId> for HirFileId {
    #[inline]
    fn from(file_id: EditionedFileId) -> Self {
        HirFileId::FileId(file_id)
    }
}

impl From<MacroCallId> for HirFileId {
    #[inline]
    fn from(file_id: MacroCallId) -> Self {
        HirFileId::MacroFile(file_id)
    }
}

impl PartialEq<EditionedFileId> for HirFileId {
    fn eq(&self, &other: &EditionedFileId) -> bool {
        *self == HirFileId::from(other)
    }
}
impl PartialEq<HirFileId> for EditionedFileId {
    fn eq(&self, &other: &HirFileId) -> bool {
        other == HirFileId::from(*self)
    }
}

impl HirFileId {
    #[inline]
    pub fn macro_file(self) -> Option<MacroCallId> {
        match self {
            HirFileId::FileId(_) => None,
            HirFileId::MacroFile(it) => Some(it),
        }
    }

    #[inline]
    pub fn is_macro(self) -> bool {
        matches!(self, HirFileId::MacroFile(_))
    }

    #[inline]
    pub fn file_id(self) -> Option<EditionedFileId> {
        match self {
            HirFileId::FileId(it) => Some(it),
            HirFileId::MacroFile(_) => None,
        }
    }

    pub fn syntax_context(self, db: &Db, edition: Edition) -> SyntaxContext {
        match self {
            HirFileId::FileId(_) => SyntaxContext::root(edition),
            HirFileId::MacroFile(m) => {
                let kind = &m.loc(db).kind;
                m.macro_arg_considering_derives(db, kind).2.ctx
            }
        }
    }

    pub fn edition(self, db: &Db) -> Edition {
        match self {
            HirFileId::FileId(file_id) => file_id.edition(db),
            HirFileId::MacroFile(m) => m.loc(db).def.edition,
        }
    }

    pub fn original_file(self, db: &Db) -> EditionedFileId {
        let mut file_id = self;
        loop {
            match file_id {
                HirFileId::FileId(id) => break id,
                HirFileId::MacroFile(macro_call_id) => {
                    file_id = macro_call_id.loc(db).kind.file_id()
                }
            }
        }
    }

    pub fn original_file_respecting_includes(mut self, db: &Db) -> EditionedFileId {
        loop {
            match self {
                HirFileId::FileId(id) => break id,
                HirFileId::MacroFile(file) => {
                    let loc = file.loc(db);
                    if loc.def.is_include()
                        && let MacroCallKind::FnLike { eager: Some(eager), .. } = &loc.kind
                        && let Ok(it) = include_input_to_file_id(db, file, &eager.arg)
                    {
                        break it;
                    }
                    self = loc.kind.file_id();
                }
            }
        }
    }

    /// The outermost call, in the real file.
    pub fn original_call_node_view(self, db: &Db) -> Option<InRealFile<view::Node<'_>>> {
        let mut call = self.macro_file()?.loc(db).to_node_view(db);
        loop {
            match call.file_id {
                HirFileId::FileId(file_id) => {
                    break Some(InRealFile { file_id, value: call.value });
                }
                HirFileId::MacroFile(macro_call_id) => {
                    call = macro_call_id.loc(db).to_node_view(db);
                }
            }
        }
    }

    /// The macro file's call, as [`MacroCallId::call_node_view`].
    pub fn call_node_view(self, db: &Db) -> Option<InFile<view::Node<'_>>> {
        Some(self.macro_file()?.loc(db).to_node_view(db))
    }

    /// Main public API -- parses a hir file, not caring whether it's a real
    /// file or a macro expansion.
    pub fn parse_or_expand(self, db: &Db) -> SyntaxNode {
        match self {
            HirFileId::FileId(file_id) => file_id.parse(db).syntax_node(),
            HirFileId::MacroFile(macro_file) => {
                macro_file.parse_macro_expansion(db).value.0.syntax_node()
            }
        }
    }

    /// The file's root as a view, borrowed from the parse the database keeps: what
    /// [`Self::parse_or_expand`] is, with no handle made.
    pub fn root(self, db: &Db) -> syntax::view::Node<'_> {
        match self {
            HirFileId::FileId(file_id) => file_id.parse(db).root(),
            HirFileId::MacroFile(macro_file) => macro_file.parse_macro_expansion(db).value.0.root(),
        }
    }

    /// How many macro expansions separate this file from the real file it originates from.
    ///
    /// `Expander` and `DefCollector` seed their expansion depth counters from this. Both otherwise
    /// start at zero for every body and every block, so a macro expanding to an item that invokes
    /// the macro again advances neither counter past one and never reaches the recursion limit.
    ///
    /// Written recursively rather than as a loop so that the query engine memoizes each file's depth. Callers
    /// ask for this once per body, and the bodies of one expansion chain sit at depths 1..n, so a
    /// loop would walk 1 + 2 + ... + n steps for that chain. Reusing the parent's memoized depth
    /// makes each file O(1) once computed, and the chain linear.
    #[inline]
    pub fn macro_expansion_depth(self, db: &Db) -> u32 {
        match self {
            HirFileId::MacroFile(macro_call) => macro_call.loc(db).macro_depth,
            HirFileId::FileId(_) => 0,
        }
    }
}

#[blaze_query::tracked]
impl HirFileId {
    #[blaze_query::tracked(lru = 1024, returns(ref), mem_tag = Expand)]
    pub fn ast_id_map(self, db: &Db) -> AstIdMap {
        AstIdMap::from_view(self.root(db))
    }

    /// The ranges of the file's syntax errors, sorted by where they start: what
    /// [`HirFileId::has_syntax_error_in`] searches, so the errors are found once per file.
    #[blaze_query::tracked(returns(ref), mem_tag = Expand)]
    fn syntax_error_ranges(self, db: &Db) -> Box<[TextRange]> {
        let mut ranges: Vec<TextRange> = match self {
            HirFileId::FileId(file_id) => {
                file_id.parse_errors(db).unwrap_or_default().iter().map(SyntaxError::range).collect() // alloc: the query's value, once per file
            }
            HirFileId::MacroFile(macro_file) => macro_file
                .parse_macro_expansion(db)
                .value
                .0
                .errors()
                .iter()
                .map(SyntaxError::range)
                .collect(), // alloc: the query's value, once per file
        };
        RANGE_SORT.with_borrow_mut(|scratch| {
            radix::sort_copy_by_key_in(
                &mut ranges,
                |range| radix::pack_32(range.start().into(), range.end().into()),
                scratch.take(),
            )
        });
        ranges.into_boxed_slice()
    }
}

impl HirFileId {
    /// Whether a syntax error of the file touches `range`. Only then is the input of an
    /// attribute macro or derive fixed up ([`fixup::fixup_syntax`]): code without errors goes
    /// to the macro exactly as written, as rustc passes it.
    pub(crate) fn has_syntax_error_in(self, db: &Db, range: TextRange) -> bool {
        touches_any(self.syntax_error_ranges(db), range)
    }
}

/// Whether one of `ranges`, sorted by start, overlaps or touches `range`.
fn touches_any(ranges: &[TextRange], range: TextRange) -> bool {
    let before_end = ranges.partition_point(|it| it.start() <= range.end());
    ranges[..before_end].iter().any(|it| it.end() >= range.start())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(start.into(), end.into())
    }

    #[test]
    fn error_ranges_touching() {
        let errors = [range(3, 3), range(10, 20), range(15, 16), range(40, 41)];
        let naive = |it: TextRange| {
            errors.iter().any(|e| e.start() <= it.end() && e.end() >= it.start())
        };
        for start in 0..45 {
            for end in start..45 {
                let it = range(start, end);
                assert_eq!(touches_any(&errors, it), naive(it), "{it:?}");
            }
        }
        assert!(!touches_any(&[], range(0, 10)));
        assert!(touches_any(&errors, range(0, 3)));
        assert!(!touches_any(&errors, range(4, 9)));
        assert!(touches_any(&errors, range(21, 40)));
    }
}
