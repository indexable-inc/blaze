//! The lints of attributes: what rustc's attribute parser (`rustc_attr_parsing`), its
//! attribute checks (`rustc_passes/src/check_attr.rs`), expansion's checks of the attributes of
//! macro calls and the early lint pass say of the attributes of a crate.
//!
//! - [`target`]: what an attribute is applied to (rustc's `Target`).
//! - [`table`]: rustc's builtin attribute parsers: each one's path, template, targets and what
//!   a second one does.
//! - `parse`: what attribute parsing gives: an attribute where it does nothing or is not
//!   allowed, a duplicate, an empty list, an ill-formed input, `unsafe` left out, a crate-level
//!   attribute below the crate root, and expansion's checks of the attributes of macro calls.
//! - `check_attr`: what `check_mod_attrs` lints: `conflicting_repr_hints`, `#[inline]` on an
//!   exported function, `#[link]` on what it does nothing for, empty lint attributes, ..
//! - `doc`: `invalid_doc_attributes`, the lints of `#[doc(..)]`, `unused_doc_comments`.
//! - `diagnostic`: the lints of the `#[diagnostic::..]` attributes.
//! - `features`: `incomplete_features`, `internal_features`, `unstable_features`.
//!
//! In this nightly rustc parses attributes as it lowers the AST to HIR and emits the lints it
//! finds then (`rustc_hir::lints::DelayedLint`) in `emit_delayed_lints`, right after
//! `check_mod_attrs`; the attributes of a macro call are parsed at expansion and their lints
//! buffered for the early lint pass.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/interface.rs` (`parse_attribute_list`),
//! `compiler/rustc_ast_lowering/src/lib.rs` (`lower_attrs`), `compiler/rustc_expand/src/
//! config.rs` (`expand_cfg_attr`) and `compiler/rustc_interface/src/passes.rs`
//! (`emit_delayed_lints`) (MIT or Apache-2.0).

mod check_attr;
mod diagnostic;
mod doc;
pub(crate) mod features;
pub(crate) mod parse;
pub(crate) mod table;
pub(crate) mod target;

pub(crate) use target::Target;

use blaze_hash::HashMap;
use either::Either;
use hir::{HirFileId, InFile};
use intern::Symbol;
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode},
};

use super::{Emitted, Pass, Stage, Walk};
use crate::{cx::Cx, lower::Phase, model::Diag};

/// An attribute of a node as rustc's attribute parser takes it: one of a `cfg_attr`'s
/// attributes is one of its own, a doc comment is `#[doc = ".."]`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Attr<'t> {
    /// The file it is written in (a module file's inner attributes are its `mod` item's).
    pub(crate) file: HirFileId,
    pub(crate) kind: AttrKind<'t>,
    /// `#![..]` or `//!`.
    pub(crate) inner: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AttrKind<'t> {
    /// `#[path args]`.
    Meta {
        /// The attribute, without the `unsafe(..)` around it.
        meta: ast::Meta<'t>,
        /// The `#[..]` it is written in: its own, or the `cfg_attr`'s that gives it.
        written: ast::Attr<'t>,
        /// The `unsafe(..)` around it.
        unsafety: Option<ast::UnsafeMeta<'t>>,
        /// Whether a `cfg_attr` gives it.
        from_cfg_attr: bool,
    },
    /// `///`, `//!`, `/** */`, `/*! */`.
    Doc(ast::DocComment<'t>),
}

/// The segments of an attribute's path, interned; inline up to two.
pub(crate) type Path = SmallVec<[Symbol; 2]>;

/// An attribute's arguments: rustc's `ArgParser`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Args<'t> {
    /// `#[path]`.
    Word,
    /// `#[path(..)]`, `#[path[..]]`, `#[path{..}]`.
    List(ast::TokenTree<'t>),
    /// `#[path = value]`; a doc comment's is its text.
    NameValue(Option<ast::Expr<'t>>),
}

impl<'t> Attr<'t> {
    pub(crate) fn meta(&self) -> Option<ast::Meta<'t>> {
        match self.kind {
            AttrKind::Meta { meta, .. } => Some(meta),
            AttrKind::Doc(_) => None,
        }
    }

    pub(crate) fn is_doc_comment(&self) -> bool {
        matches!(self.kind, AttrKind::Doc(_))
    }

    /// The segments of its path as written, read in place: `inline`; `diagnostic`,
    /// `on_unimplemented`; a doc comment's is `doc`.
    pub(crate) fn segments(&self) -> impl Iterator<Item = &'t str> + use<'t> {
        let (doc, path) = match self.kind {
            AttrKind::Meta { meta, .. } => (None, meta.path()),
            AttrKind::Doc(_) => (Some("doc"), None),
        };
        doc.into_iter().chain(path.into_iter().flat_map(|it| it.segments()).map(|it| match it.name_ref() {
            Some(name) => name.text(),
            None => it.syntax().text(),
        }))
    }

    /// The segments of its path: `["inline"]`, `["diagnostic", "on_unimplemented"]`; a doc
    /// comment's is `["doc"]`.
    pub(crate) fn path(&self) -> Path {
        // alloc: inline for up to two segments, as nearly every attribute's path is
        self.segments().map(Symbol::intern).collect()
    }

    /// Its path as rustc prints it: `inline`, `diagnostic::on_unimplemented`.
    pub(crate) fn name(&self) -> String {
        // alloc: a diagnostic's text
        let segments: SmallVec<[&str; 2]> = self.segments().collect();
        // alloc: as above
        segments.join("::")
    }

    /// Whether its path is the one segment `name`.
    pub(crate) fn is(&self, name: &str) -> bool {
        let mut segments = self.segments();
        segments.next() == Some(name) && segments.next().is_none()
    }

    /// Whether it is written `#[unsafe(..)]`.
    pub(crate) fn is_unsafe(&self) -> bool {
        matches!(&self.kind, AttrKind::Meta { unsafety: Some(_), .. })
    }

    /// rustc's `attr_span`: the whole `#[..]`, or for one a `cfg_attr` gives, the attribute
    /// in it (its outer and inner spans are the same).
    pub(crate) fn range(&self) -> TextRange {
        match &self.kind {
            AttrKind::Doc(doc) => doc.syntax().text_range(),
            AttrKind::Meta { from_cfg_attr: true, .. } => self.inner_range(),
            AttrKind::Meta { written, .. } => written.syntax().text_range(),
        }
    }

    /// rustc's `inner_span`: what is between `#[` and `]`, the `unsafe(..)` included.
    pub(crate) fn inner_range(&self) -> TextRange {
        match &self.kind {
            AttrKind::Doc(doc) => doc.syntax().text_range(),
            AttrKind::Meta { unsafety: Some(unsafety), .. } => unsafety.syntax().text_range(),
            AttrKind::Meta { meta, .. } => meta.syntax().text_range(),
        }
    }

    /// Where its path is written; a doc comment's is the comment.
    pub(crate) fn path_range(&self) -> TextRange {
        match self.meta().and_then(|it| it.path()) {
            Some(path) => path.syntax().text_range(),
            None => self.inner_range(),
        }
    }

    /// Its arguments.
    pub(crate) fn args(&self) -> Args<'t> {
        match self.kind {
            AttrKind::Doc(_) => Args::NameValue(None),
            AttrKind::Meta { meta, .. } => match meta {
                ast::Meta::TokenTreeMeta(it) => match it.token_tree() {
                    Some(tt) => Args::List(tt),
                    None => Args::Word,
                },
                ast::Meta::KeyValueMeta(it) => Args::NameValue(it.expr()),
                _ => Args::Word,
            },
        }
    }

    /// `range` of the file it is written in.
    pub(crate) fn in_file(&self, range: TextRange) -> InFile<TextRange> {
        InFile::new(self.file, range)
    }
}

/// A node with attributes, as rustc lowers it: what they are applied to and each of them, in
/// order (outer, then inner).
#[derive(Clone, Debug)]
pub(crate) struct Owner<'db> {
    pub(crate) node: InFile<ast::Node<'db>>,
    pub(crate) target: Target,
    pub(crate) attrs: Vec<Attr<'db>>,
    /// For a macro call, the node expansion buffers its attributes' lints at
    /// (`current_expansion.lint_node_id`).
    pub(crate) buffered_at: Option<InFile<SyntaxNodePtr>>,
}

impl Owner<'_> {
    /// The node a lint of its attributes is emitted at.
    pub(crate) fn at(&self) -> InFile<SyntaxNodePtr> {
        self.node.map(ast::ptr)
    }

    /// Whether it is in a macro call's expansion.
    pub(crate) fn in_expansion(&self) -> bool {
        self.node.file_id.is_macro()
    }
}

/// Every node of the crate rustc lowers attributes of, as the walk comes to them, each with
/// its attributes as `cfg` and `cfg_attr` leave them; the nodes with none left out.
pub(crate) fn owners<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Owner<'db>> {
    // alloc: empty, allocates nothing
    let Some(root) = cx.krate.root_module(cx.db).as_source_file_id(cx.db) else { return Vec::new() };
    let crate_root = HirFileId::from(root);
    let mut declared: HashMap<HirFileId, InFile<SyntaxNodePtr>> = HashMap::default();
    for module in crate::checks::modules(cx) {
        if let Some(file) = module.as_source_file_id(cx.db)
            && let Some(declaration) = module.declaration_source_view(cx.db)
        {
            declared.insert(HirFileId::from(file), declaration.map(|it| ast::ptr(it.syntax())));
        }
    }
    // alloc: setup, once per crate: the table of the owners every attribute check reads
    let mut owners: Vec<Owner<'db>> = Vec::new();
    // A module file's inner attributes are its `mod` item's: only those are looked up.
    let mut module_items: HashMap<InFile<SyntaxNodePtr>, usize> = HashMap::default();
    for tree in &walk.trees {
        for node in tree.of_kind(|kind| ast::AnyHasAttrs::can_cast(kind) || kind == SyntaxKind::MACRO_EXPR) {
            // The parser puts a macro call expression's attributes on the `MACRO_EXPR`, which has
            // no `HasAttrs`; rustc's expansion checks them as the call's (`check_attributes` of
            // `collect_bang`).
            if let Some(call) = ast::MacroExpr::cast(node).and_then(|it| it.macro_call()) {
                // alloc: empty until an attribute, then the owner's row of the table
                let mut attrs = Vec::new();
                let written = node.children().filter_map(ast::AnyAttr::cast).filter(|it| it.kind().is_outer());
                written_attributes(walk, tree.file, written, &mut attrs);
                if !attrs.is_empty() {
                    let node = InFile::new(tree.file, call.syntax());
                    owners.push(Owner { node, target: Target::MacroCall, attrs, buffered_at: None });
                }
                continue;
            }
            // `Tree::attrs` marks no doc comment's owner, so each node that can have one is read.
            let Some(has_attrs) = ast::AnyHasAttrs::cast(node) else { continue };
            let kind = node.kind();
            if kind == SyntaxKind::SOURCE_FILE
                && tree.in_file()
                && let Some(declaration) = declared.get(&tree.file)
                && let Some(&at) = module_items.get(declaration)
            {
                attributes(walk, tree.file, has_attrs, &mut owners[at].attrs);
                continue;
            }
            // alloc: empty until an attribute, then the owner's row of the table
            let mut attrs = Vec::new();
            attributes(walk, tree.file, has_attrs, &mut attrs);
            let is_module = kind == SyntaxKind::MODULE;
            if attrs.is_empty() && !is_module {
                continue;
            }
            let Some(target) = Target::of(cx, InFile::new(tree.file, node), crate_root) else { continue };
            if is_module {
                module_items.insert(tree.ptr(node), owners.len());
            }
            owners.push(Owner { node: InFile::new(tree.file, node), target, attrs, buffered_at: None });
        }
    }
    // `-Z crate-attr`'s, each a file of its own, after the crate root's own inner attributes.
    let applied = base_db::applied_crate_attr_files(cx.db, cx.krate.into());
    if !applied.is_empty() {
        let at = match owners.iter().position(|it| it.target == Target::Crate) {
            Some(at) => at,
            None => {
                let root = walk.tree(crate_root).expect("the walk has the crate root's tree").root;
                // alloc: empty, filled below
                owners.insert(0, Owner { node: InFile::new(crate_root, root), target: Target::Crate, attrs: Vec::new(), buffered_at: None });
                0
            }
        };
        for &file in applied {
            let source = file.parse(cx.db).view();
            let has_attrs = ast::AnyHasAttrs::cast(source.syntax()).expect("a source file has attributes");
            attributes(walk, HirFileId::from(file), has_attrs, &mut owners[at].attrs);
        }
    }
    owners.retain(|it| !it.attrs.is_empty());
    let mut modules = doc::comments::Modules::default();
    for owner in owners.iter_mut().filter(|it| it.target == Target::MacroCall) {
        owner.buffered_at = Some(doc::comments::lint_node(cx, owner.node, &mut modules));
    }
    owners
}

/// Of each item a builtin attribute macro replaces, the attribute that invokes it, as an owner of that
/// one attribute: expansion reads it (`expand_invoc`, a builtin macro's `validate_input`), while
/// the walk sees neither the item nor, in what it expands to, the attribute.
pub(crate) fn invoking_attrs<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Owner<'db>> {
    let db = cx.db;
    let Some(root) = cx.krate.root_module(db).as_source_file_id(db) else {
        // alloc: empty, allocates nothing
        return Vec::new();
    };
    let crate_root = HirFileId::from(root);
    // alloc: empty until an attribute macro, then a row of each
    let mut invoking = Vec::new();
    for tree in &walk.trees {
        for &node in &tree.replaced {
            let Some(item) = ast::Item::cast(node) else { continue };
            let Some(call) = cx.vsema.expand_attr_macro(item).and_then(|it| it.value.file_id.macro_file()) else {
                continue;
            };
            let loc = call.loc(db);
            // Only a builtin one's input is a meta item (`LegacyAttr`); a proc macro's is tokens.
            if !matches!(loc.def.kind, hir_expand::MacroDefKind::BuiltInAttr(..)) {
                continue;
            }
            let hir_expand::MacroCallKind::Attr { ast_id, censored_attr_ids, .. } = &loc.kind else { continue };
            let invoked = censored_attr_ids.invoc_attr().find_attr_range_view(db, loc.krate, *ast_id).1;
            let range = invoked.syntax().text_range();
            // alloc: the item's attributes, once per attribute macro call
            let mut attrs = Vec::new();
            let Some(has_attrs) = ast::AnyHasAttrs::cast(node) else { continue };
            attributes(walk, tree.file, has_attrs, &mut attrs);
            attrs.retain(|it| matches!(it.kind, AttrKind::Meta { meta, .. } if meta.syntax().text_range() == range));
            if attrs.is_empty() {
                continue;
            }
            let node = InFile::new(tree.file, node);
            let Some(target) = Target::of(cx, node, crate_root) else { continue };
            invoking.push(Owner { node, target, attrs, buffered_at: None });
        }
    }
    invoking
}

/// The attributes of `owner`, written in `file`, with `cfg_attr` expanded as the crate's cfg
/// says and the `cfg`s left out (rustc keeps only a trace of them), pushed onto `attrs`.
fn attributes<'db>(walk: &Walk<'db>, file: HirFileId, owner: ast::AnyHasAttrs<'db>, attrs: &mut Vec<Attr<'db>>) {
    written_attributes(walk, file, ast::attrs_with_doc_including_inner(&owner), attrs);
}

/// [`attributes`] of the attributes and doc comments `written`, as they are written.
fn written_attributes<'db>(
    walk: &Walk<'db>,
    file: HirFileId,
    written: impl Iterator<Item = ast::AnyAttr<'db>>,
    attrs: &mut Vec<Attr<'db>>,
) {
    let written = written.map(|it| match it {
        ast::AnyAttr::Attr(it) => Either::Left(it),
        ast::AnyAttr::DocComment(it) => Either::Right(it),
    });
    hir_expand::attrs::expand_cfg_attr_with_doc_comments_view::<_, ()>(written, || walk.cfg, |it| {
        match it {
            Either::Left((ast::Meta::CfgMeta(_), _)) => {}
            Either::Left((meta, written)) => {
                let unsafety = meta.syntax().parent().and_then(ast::UnsafeMeta::cast);
                let top = written.meta();
                let from_cfg_attr = matches!(top, Some(ast::Meta::CfgAttrMeta(_)))
                    || matches!(top, Some(ast::Meta::UnsafeMeta(it)) if matches!(it.meta(), Some(ast::Meta::CfgAttrMeta(_))));
                let inner = written.kind().is_inner();
                attrs.push(Attr { file, kind: AttrKind::Meta { meta, written, unsafety, from_cfg_attr }, inner });
            }
            Either::Right(doc) => {
                let inner = doc.kind().is_inner();
                attrs.push(Attr { file, kind: AttrKind::Doc(doc), inner });
            }
        }
        std::ops::ControlFlow::Continue(())
    });
}

/// A lint attribute parsing gives as AST lowering parses `owner`'s attributes: emitted with
/// the rest of the delayed lints, right after `check_mod_attrs`; for a macro call's attributes,
/// which expansion parses, buffered for the early lint pass.
pub(crate) fn parsed(diag: Diag, owner: &Owner<'_>) -> Emitted {
    match &owner.buffered_at {
        // clone: a pointer, copied for a lint emitted
        Some(at) => Emitted::new(diag, Phase::Resolve, Pass::Buffered, at.clone()).buffered(Stage::Collected),
        None => Emitted::new(diag, Phase::Items, Pass::Delayed, owner.at()),
    }
}

/// The node a lint of `owner`'s attributes is emitted at: for a macro call, the one
/// expansion buffers at.
pub(crate) fn lint_at(owner: &Owner<'_>) -> InFile<SyntaxNodePtr> {
    // clone: a pointer, copied for a lint emitted
    owner.buffered_at.clone().unwrap_or_else(|| owner.at())
}

/// A lint `check_mod_attrs` gives of `owner`'s attributes.
pub(crate) fn checked(diag: Diag, owner: &Owner<'_>) -> Emitted {
    Emitted::new(diag, Phase::Items, Pass::Early, owner.at())
}

/// The lints of the attributes of the crate.
pub(crate) fn lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Emitted> {
    let owners = owners(cx, walk);
    let mut emitted = parse::lints(cx, walk, &owners);
    emitted.extend(check_attr::lints(cx, walk, &owners));
    emitted.extend(doc::lints(cx, walk, &owners));
    emitted.extend(diagnostic::lints(cx, walk, &owners));
    emitted.extend(features::lints(cx, walk, &owners));
    emitted
}
