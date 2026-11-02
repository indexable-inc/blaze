//! Attributes for anything that is not name resolution.
//!
//! The fundamental idea of this module stems from the observation that most "interesting"
//! attributes have a more memory-compact form than storing their full syntax, and
//! that most of the attributes are flags, and those that are not are rare. Therefore,
//! this module defines [`AttrFlags`], which is a bitflag enum that contains only a yes/no
//! answer to whether an attribute is present on an item. For most attributes, that's all
//! that is interesting us; for the rest of them, we define another query that extracts
//! their data. A key part is that every one of those queries will have a wrapper method
//! that queries (or is given) the `AttrFlags` and checks for the presence of the attribute;
//! if it is not present, we do not call the query, to keep the query database from needing to record
//! its value. This way, queries are only called on items that have the attribute, which is
//! usually only a few.
//!
//! Documentation (doc comments and `#[doc = "..."]` attributes) is handled by the [`docs`]
//! submodule.
//!
//! Reading flags costs one walk of an item's attributes, once per item (the query is cached).
//! Each attribute's name, and each name inside one the flags or a value query reads (`doc`,
//! `repr`, `link`, ...), is found by a perfect hash built at compile time ([`name_set!`]): one
//! hash of its bytes and one compare, never a `String` built. A value query
//! (`lang`, `repr`, `export_name`, ...) runs only for an item whose flag says it has the
//! attribute.

use std::{borrow::Cow, convert::Infallible, iter::Peekable, ops::ControlFlow};

use base_db::{Crate, Db, EditionedFileId};
use blaze_hash::HashSet;
use blaze_mem::Frame;
use blaze_query::db::{AsId, FromId};
use cfg::{CfgExpr, CfgOptions};
use either::Either;
use hir_expand::{
    AstId, HirFileId, InFile, Lookup,
    attrs::expand_cfg_attr_view,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;
use itertools::Itertools;
use la_arena::ArenaMap;
use rustc_abi::ReprOptions;
use smallvec::SmallVec;
use span::AstIdNode;
use syntax::{
    NodeOrToken, SmolStr, SourceFile, SyntaxKind, T,
    view::{self as ast, AstNode, AstToken, HasAttrs, TokenTreeChildren, Typed},
};

use crate::{
    AdtId, AstIdLoc, AttrDefId, BlockId, ExternBlockId, FieldId, FunctionId, GenericDefId,
    HasModule, ItemContainerId, LifetimeParamId, LocalFieldId, MacroId, ModuleId, TraitId,
    TypeOrConstParamId, VariantId,
    hir::generics::{GenericParams, LocalLifetimeParamId, LocalTypeOrConstParamId},
    nameres::ModuleOrigin,
    resolver::{HasResolver, Resolver},
    src::HasSource,
};

pub mod docs;
pub mod stability;

pub use self::docs::{Docs, IsInnerDoc};

/// Which tree an item's attributes are read off.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tree {
    /// Its file's outline ([`crate::signatures::outline_item`]) when the item is in it: the
    /// file's text less the bodies the outline leaves out, none of which holds an attribute of
    /// its item, so the attributes read the same, and a body edit leaves the outline the same.
    /// For what reads only the attributes' text: the flags and the value queries over
    /// [`collect_attrs`]. An item the outline doesn't have (in a body it leaves out, or written
    /// by a macro) is read off its file's parse.
    Outline,
    /// The file's parse: for what maps an attribute back to its place in the file (the docs'
    /// source map, a macro call in a value, a field found by its range).
    Live,
}

#[inline]
fn attrs_from_ast_id_loc<'db, N: AstIdNode + Typed + Into<syntax::ast::AnyHasAttrs>>(
    db: &'db Db,
    lookup: impl Lookup<Data = impl AstIdLoc<Ast = N> + HasModule>,
    tree: Tree,
) -> (InFile<ast::AnyHasAttrs<'db>>, Crate) {
    let loc = lookup.lookup(db);
    let krate = loc.krate(db);
    let id = loc.ast_id();
    (InFile::new(id.file_id, any_has_attrs(item_node(db, id, loc.module(db), tree).syntax())), krate)
}

/// The node at `id`, an item of `module`, off the tree `tree` says: for [`Tree::Outline`], an
/// item of a block of a body is read off that body's syntax.
fn item_node<'db, N: AstIdNode + Typed>(db: &'db Db, id: AstId<N>, module: ModuleId, tree: Tree) -> N::Of<'db> {
    if tree == Tree::Outline
        && let Some(node) =
            crate::signatures::outline_item(db, id).or_else(|| crate::signatures::body_item(db, id, module))
    {
        return node;
    }
    id.to_view(db)
}

/// The root of `file`, off the tree `tree` says.
fn file_root(db: &Db, file: EditionedFileId, tree: Tree) -> ast::SourceFile<'_> {
    match tree {
        Tree::Outline => crate::signatures::outline_root(db, file),
        Tree::Live => file.parse(db).view(),
    }
}

/// `node` as the attribute owner it is: the node of an item whose handle type is
/// `Into<AnyHasAttrs>`, so the cast holds by the caller's bound.
#[inline]
fn any_has_attrs(node: ast::Node<'_>) -> ast::AnyHasAttrs<'_> {
    ast::AnyHasAttrs::cast(node).unwrap_or_else(|| {
        panic!("a {:?} at {:?} holds no attributes", node.kind(), node.text_range())
    })
}

/// The [`AttrFlags`] of the items of one file's outline ([`crate::signatures::file_outline`]),
/// for one crate's cfg: each item outside the bodies the outline leaves out and outside every
/// const's, static's and fn's expression, but modules, whose attributes are also their
/// declaration's and the crate's ([`attrs_source`]). Built in one walk of the outline, so an
/// item's flags cost one binary search of `ids` once the column is built, and a body edit,
/// which leaves the outline the same, runs nothing.
#[derive(Debug, PartialEq, Eq)]
pub struct FileAttrFlags {
    /// Each item's erased ast id, ascending.
    ids: Box<[u32]>,
    /// Each item's flags, in the order of `ids`.
    flags: Box<[AttrFlags]>,
}

impl FileAttrFlags {
    /// The flags of the item `id`, if the column holds it.
    #[inline]
    fn get(&self, id: span::ErasedFileAstId) -> Option<AttrFlags> {
        let at = self.ids.binary_search(&id.into_raw()).ok()?;
        Some(self.flags[at])
    }
}

/// The [`FileAttrFlags`] of `file` for `krate`'s cfg.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
fn file_attr_flags(db: &Db, file: EditionedFileId, krate: Crate) -> FileAttrFlags {
    blaze_mem::frame::current(|frame| {
        let (root, ast_id_map) = crate::signatures::outline_with_ids(db, file);
        let mut cfg_options = None;
        let mut cfg = || *cfg_options.get_or_insert_with(|| krate.cfg_options(db));
        let mut items = Vec::new_in(frame);
        outline_attr_owners(root, &mut |item| {
            let ptr = ast::ptr(item);
            let id = if item.kind() == SyntaxKind::VARIANT {
                let ptr = syntax::AstPtr::<syntax::ast::Variant>::try_from_raw(ptr);
                ast_id_map.ast_id_for_ptr(ptr.expect("a VARIANT is a variant")).erase()
            } else {
                let ptr = syntax::AstPtr::<syntax::ast::Item>::try_from_raw(ptr);
                ast_id_map.ast_id_for_ptr(ptr.expect("an owner walked is an item")).erase()
            };
            let mut attr_flags = AttrFlags::empty();
            let attrs = ast::attrs_including_inner(&any_has_attrs(item));
            expand_cfg_attr_view(attrs, &mut cfg, |attr, _| match_attr_flags(&mut attr_flags, attr));
            items.push((id.into_raw(), attr_flags));
        });
        items.sort_unstable_by_key(|&(id, _)| id);
        FileAttrFlags {
            // alloc: the memo's ids, exact
            ids: items.iter().map(|&(id, _)| id).collect(),
            // alloc: the memo's flags, exact
            flags: items.iter().map(|&(_, flags)| flags).collect(),
        }
    })
}

/// Builds the [`FileAttrFlags`] of each of `files` for `krate`, one task a file on the
/// database's workers ([`Db::run_all`]), so a pass that then reads them in order
/// ([`CrateAttrFlags`]) finds them built. A built column is a memo hit.
pub(crate) fn build_columns(db: &Db, krate: Crate, files: &[EditionedFileId]) {
    db.run_all(blaze_mem::Tag::Nameres, files.len(), |db, at| {
        _ = file_attr_flags(db, files[at], krate);
    });
}

/// Each attribute owner [`FileAttrFlags`] holds under `node` (a file, or an item list in it),
/// in order: the items of the file and of each inline module, impl, trait and extern block,
/// and each enum's variants. No body is walked, so nothing here parses one.
fn outline_attr_owners<'t, F: FnMut(ast::Node<'t>)>(node: ast::Node<'t>, owner: &mut F) {
    for item in node.children() {
        let list = match item.kind() {
            SyntaxKind::MODULE => {
                if let Some(list) = item.child_where(|kind| kind == SyntaxKind::ITEM_LIST) {
                    outline_attr_owners(list, owner);
                }
                continue;
            }
            SyntaxKind::IMPL | SyntaxKind::TRAIT => Some(SyntaxKind::ASSOC_ITEM_LIST),
            SyntaxKind::ENUM => Some(SyntaxKind::VARIANT_LIST),
            SyntaxKind::EXTERN_BLOCK => Some(SyntaxKind::EXTERN_ITEM_LIST),
            SyntaxKind::FN
            | SyntaxKind::STRUCT
            | SyntaxKind::UNION
            | SyntaxKind::CONST
            | SyntaxKind::STATIC
            | SyntaxKind::TYPE_ALIAS
            | SyntaxKind::MACRO_RULES
            | SyntaxKind::MACRO_DEF
            | SyntaxKind::EXTERN_CRATE
            | SyntaxKind::USE
            | SyntaxKind::VARIANT => None,
            _ => continue,
        };
        owner(item);
        if let Some(list) = list
            && let Some(list) = item.child_where(|kind| kind == list)
        {
            outline_attr_owners(list, owner);
        }
    }
}

/// [`AttrFlags::query`] of `owner` off its file's [`FileAttrFlags`], `column` giving the column
/// of a file for a crate; `None` for an owner no column holds (a module, an item in a body or
/// written by a macro).
#[inline]
fn column_flags<'db>(
    db: &'db Db,
    owner: AttrDefId,
    column: &mut impl FnMut(EditionedFileId, Crate) -> &'db FileAttrFlags,
) -> Option<AttrFlags> {
    #[inline]
    fn of<'db, N: AstIdNode>(
        db: &'db Db,
        lookup: impl Lookup<Data = impl AstIdLoc<Ast = N> + HasModule>,
        column: &mut impl FnMut(EditionedFileId, Crate) -> &'db FileAttrFlags,
    ) -> Option<AttrFlags> {
        let loc = lookup.lookup(db);
        let id = loc.ast_id();
        let HirFileId::FileId(file) = id.file_id else { return None };
        column(file, loc.krate(db)).get(id.value.erase())
    }
    match owner {
        AttrDefId::ModuleId(_) => None,
        AttrDefId::AdtId(AdtId::StructId(it)) => of(db, it, column),
        AttrDefId::AdtId(AdtId::UnionId(it)) => of(db, it, column),
        AttrDefId::AdtId(AdtId::EnumId(it)) => of(db, it, column),
        AttrDefId::FunctionId(it) => of(db, it, column),
        AttrDefId::EnumVariantId(it) => of(db, it, column),
        AttrDefId::StaticId(it) => of(db, it, column),
        AttrDefId::ConstId(it) => of(db, it, column),
        AttrDefId::TraitId(it) => of(db, it, column),
        AttrDefId::TypeAliasId(it) => of(db, it, column),
        AttrDefId::MacroId(MacroId::MacroRulesId(it)) => of(db, it, column),
        AttrDefId::MacroId(MacroId::Macro2Id(it)) => of(db, it, column),
        AttrDefId::MacroId(MacroId::ProcMacroId(it)) => of(db, it, column),
        AttrDefId::ImplId(it) => of(db, it, column),
        AttrDefId::ExternBlockId(it) => of(db, it, column),
        AttrDefId::ExternCrateId(it) => of(db, it, column),
        AttrDefId::UseId(it) => of(db, it, column),
    }
}

/// The flags of `owner` walked off its syntax ([`attrs_source`]): what [`AttrFlags::query`]
/// answers for an owner no [`FileAttrFlags`] holds, and what a column's entry equals.
fn walked_flags(db: &Db, owner: AttrDefId) -> AttrFlags {
    let mut attr_flags = AttrFlags::empty();
    collect_attrs(db, owner, |attr| match_attr_flags(&mut attr_flags, attr));
    attr_flags
}

/// [`AttrFlags::query`] of many items of one crate, read off their files' columns: a pass over
/// a crate's items (`crate_lang_items`) reads each file's column once for a run of items of the
/// file, depends on the columns, and runs no item's query. An owner no column holds is asked of
/// [`AttrFlags::query`].
pub(crate) struct CrateAttrFlags<'db> {
    db: &'db Db,
    last: Option<(EditionedFileId, Crate, &'db FileAttrFlags)>,
}

impl<'db> CrateAttrFlags<'db> {
    pub(crate) fn new(db: &'db Db) -> Self {
        CrateAttrFlags { db, last: None }
    }

    pub(crate) fn of(&mut self, owner: AttrDefId) -> AttrFlags {
        let db = self.db;
        let last = &mut self.last;
        let mut column = |file: EditionedFileId, krate: Crate| match *last {
            Some((last_file, last_crate, column)) if last_file == file && last_crate == krate => {
                column
            }
            _ => {
                let column = file_attr_flags(db, file, krate);
                *last = Some((file, krate, column));
                column
            }
        };
        column_flags(db, owner, &mut column).unwrap_or_else(|| AttrFlags::query(db, owner))
    }

    /// [`AttrFlags::lang_item`] of `owner`, its flags read as [`CrateAttrFlags::of`] reads them.
    pub(crate) fn lang_item(&mut self, owner: AttrDefId) -> Option<Symbol> {
        self.of(owner).lang_item_with_attrs(self.db, owner)
    }
}

/// The flags of a `#[doc(...)]`: the atoms [`DocAtom::parse`] reads, stepped through the same
/// way, without building them.
#[inline]
fn extract_doc_tt_attr(attr_flags: &mut AttrFlags, tt: ast::TokenTree<'_>) {
    let mut iter = TokenTreeChildren::new(tt).peekable();
    while let Some(child) = iter.next() {
        let NodeOrToken::Token(name) = child else { continue };
        if !name.kind().is_any_identifier() {
            continue;
        }
        let name = DocWord::of(name.text());
        match iter.peek() {
            Some(NodeOrToken::Token(eq)) if eq.kind() == T![=] => {
                iter.next();
                let Some(NodeOrToken::Token(value)) = iter.next() else { continue };
                let flag = match name {
                    Some(DocWord::alias) => AttrFlags::HAS_DOC_ALIASES,
                    Some(DocWord::keyword) => AttrFlags::HAS_DOC_KEYWORD,
                    _ => continue,
                };
                if ast::String::cast(value).is_some_and(|it| it.value().is_ok()) {
                    attr_flags.insert(flag);
                }
            }
            Some(NodeOrToken::Node(_)) => {
                if name == Some(DocWord::alias) {
                    iter.next();
                    attr_flags.insert(AttrFlags::HAS_DOC_ALIASES);
                }
            }
            _ => match name {
                Some(DocWord::notable_trait) => attr_flags.insert(AttrFlags::IS_DOC_NOTABLE_TRAIT),
                Some(DocWord::hidden) => attr_flags.insert(AttrFlags::IS_DOC_HIDDEN),
                _ => {}
            },
        }
    }
}

fn extract_ra_completions(attr_flags: &mut AttrFlags, tt: ast::TokenTree<'_>) {
    let tt = TokenTreeChildren::new(tt);
    if let Ok(NodeOrToken::Token(option)) = Itertools::exactly_one(tt)
        && option.kind().is_any_identifier()
    {
        match Completions::of(option.text()) {
            Some(Completions::ignore_flyimport) => {
                attr_flags.insert(AttrFlags::COMPLETE_IGNORE_FLYIMPORT)
            }
            Some(Completions::ignore_methods) => {
                attr_flags.insert(AttrFlags::COMPLETE_IGNORE_METHODS)
            }
            Some(Completions::ignore_flyimport_methods) => {
                attr_flags.insert(AttrFlags::COMPLETE_IGNORE_FLYIMPORT_METHODS)
            }
            None => {}
        }
    }
}

fn extract_ra_macro_style(attr_flags: &mut AttrFlags, tt: ast::TokenTree<'_>) {
    let tt = TokenTreeChildren::new(tt);
    if let Ok(NodeOrToken::Token(option)) = Itertools::exactly_one(tt)
        && option.kind().is_any_identifier()
    {
        match MacroStyle::of(option.text()) {
            Some(MacroStyle::braces) => attr_flags.insert(AttrFlags::MACRO_STYLE_BRACES),
            Some(MacroStyle::brackets) => attr_flags.insert(AttrFlags::MACRO_STYLE_BRACKETS),
            Some(MacroStyle::parentheses) => attr_flags.insert(AttrFlags::MACRO_STYLE_PARENTHESES),
            None => {}
        }
    }
}

fn extract_rustc_skip_during_method_dispatch(attr_flags: &mut AttrFlags, tt: ast::TokenTree<'_>) {
    let iter = TokenTreeChildren::new(tt);
    for kind in iter {
        if let NodeOrToken::Token(kind) = kind
            && kind.kind().is_any_identifier()
        {
            match SkipDuringMethodDispatch::of(kind.text()) {
                Some(SkipDuringMethodDispatch::array) => {
                    attr_flags.insert(AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH)
                }
                Some(SkipDuringMethodDispatch::boxed_slice) => {
                    attr_flags.insert(AttrFlags::RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH)
                }
                None => {}
            }
        }
    }
}

#[inline]
fn match_attr_flags(attr_flags: &mut AttrFlags, attr: ast::Meta<'_>) -> ControlFlow<Infallible> {
    match attr {
        ast::Meta::CfgMeta(_) => attr_flags.insert(AttrFlags::HAS_CFG),
        ast::Meta::KeyValueMeta(attr) => {
            let Some(key) = attr.path().and_then(|path| path.as_single_name_ref()) else {
                return ControlFlow::Continue(());
            };
            let flag = match word(name_text(key)) {
                Some(Word::deprecated) => AttrFlags::IS_DEPRECATED,
                Some(Word::ignore) => AttrFlags::IS_IGNORE,
                Some(Word::lang) => AttrFlags::LANG_ITEM,
                Some(Word::must_use) => AttrFlags::IS_MUST_USE,
                Some(Word::path) => AttrFlags::HAS_PATH,
                Some(Word::unstable) => AttrFlags::IS_UNSTABLE,
                Some(Word::rustc_reservation_impl) => AttrFlags::RUSTC_RESERVATION_IMPL,
                Some(Word::export_name) => {
                    if string_value_is(attr, "main") {
                        attr_flags.insert(AttrFlags::IS_EXPORT_NAME_MAIN);
                    }
                    AttrFlags::HAS_EXPORT_NAME
                }
                Some(Word::link_name) => AttrFlags::HAS_LINK_NAME,
                Some(Word::link_section) => AttrFlags::HAS_LINK_SECTION,
                Some(Word::linkage) => AttrFlags::HAS_LINKAGE,
                Some(Word::rustc_diagnostic_item) => AttrFlags::HAS_DIAGNOSTIC_ITEM,
                Some(Word::must_not_suspend) => AttrFlags::MUST_NOT_SUSPEND,
                _ => return ControlFlow::Continue(()),
            };
            attr_flags.insert(flag);
        }
        ast::Meta::TokenTreeMeta(attr) => {
            let (Some((first_segment, second_segment)), Some(tt)) =
                (attr.path().and_then(up_to_two_segments), attr.token_tree())
            else {
                return ControlFlow::Continue(());
            };
            let first = word(name_text(first_segment));
            match second_segment {
                None => match first {
                    Some(Word::deprecated) => attr_flags.insert(AttrFlags::IS_DEPRECATED),
                    Some(Word::used) => attr_flags.insert(AttrFlags::USED),
                    Some(Word::inline) => {
                        if !tt.syntax().text().contains('n') {
                            attr_flags.insert(AttrFlags::IS_INLINE);
                            if tt_has_token(tt, "always") {
                                attr_flags.insert(AttrFlags::INLINE_ALWAYS)
                            }
                        } else if tt_has_token(tt, "never") {
                            attr_flags.insert(AttrFlags::INLINE_NEVER)
                        }
                    }
                    Some(Word::doc) => extract_doc_tt_attr(attr_flags, tt),
                    Some(Word::repr | Word::rustc_scalable_vector) => {
                        attr_flags.insert(AttrFlags::HAS_REPR)
                    }
                    Some(Word::target_feature) => attr_flags.insert(AttrFlags::HAS_TARGET_FEATURE),
                    Some(Word::link_ordinal) => attr_flags.insert(AttrFlags::HAS_LINK_ORDINAL),
                    Some(Word::proc_macro_derive | Word::rustc_builtin_macro) => {
                        attr_flags.insert(AttrFlags::IS_DERIVE_OR_BUILTIN_MACRO)
                    }
                    Some(Word::unstable) => attr_flags.insert(AttrFlags::IS_UNSTABLE),
                    Some(Word::stable) => attr_flags.insert(AttrFlags::IS_STABLE),
                    Some(
                        Word::rustc_layout_scalar_valid_range_start
                        | Word::rustc_layout_scalar_valid_range_end,
                    ) => attr_flags.insert(AttrFlags::RUSTC_LAYOUT_SCALAR_VALID_RANGE),
                    Some(Word::rustc_legacy_const_generics) => {
                        attr_flags.insert(AttrFlags::HAS_LEGACY_CONST_GENERICS)
                    }
                    Some(Word::rustc_skip_during_method_dispatch) => {
                        extract_rustc_skip_during_method_dispatch(attr_flags, tt)
                    }
                    Some(Word::rustc_deprecated_safe_2024) => {
                        attr_flags.insert(AttrFlags::RUSTC_DEPRECATED_SAFE_2024)
                    }
                    Some(Word::rustc_must_implement_one_of) => {
                        attr_flags.insert(AttrFlags::HAS_RUSTC_MUST_IMPLEMENT_ONE_OF)
                    }
                    Some(Word::sanitize)
                        if attr.path().is_some_and(|path| path.qualifier().is_none()) =>
                    {
                        attr_flags.insert(AttrFlags::HAS_SANITIZE)
                    }
                    Some(Word::instruction_set)
                        if attr.path().is_some_and(|path| path.qualifier().is_none()) =>
                    {
                        attr_flags.insert(AttrFlags::HAS_INSTRUCTION_SET)
                    }
                    Some(Word::optimize)
                        if attr.path().is_some_and(|path| path.qualifier().is_none())
                            && tt_has_token(tt, "none") =>
                    {
                        attr_flags.insert(AttrFlags::OPTIMIZE_NONE)
                    }
                    _ => {}
                },
                Some(second_segment) => {
                    if first == Some(Word::rust_analyzer) {
                        match word(name_text(second_segment)) {
                            Some(Word::completions) => extract_ra_completions(attr_flags, tt),
                            Some(Word::macro_style) => extract_ra_macro_style(attr_flags, tt),
                            Some(Word::define_opaque) => {
                                attr_flags.insert(AttrFlags::HAS_DEFINE_OPAQUE)
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        ast::Meta::PathMeta(attr) => {
            let Some(path) = attr.path() else { return ControlFlow::Continue(()) };
            let segment1 = path.segment().and_then(|it| it.name_ref());
            let qualifier = path.qualifier();
            if let Some(segment1) = &segment1
                && segment1.text() == "test"
            {
                let segment3 = qualifier.as_ref().and_then(|it| it.qualifier());
                let segment4 = segment3.as_ref().and_then(|it| it.qualifier());
                let segment3 = segment3.and_then(|it| it.segment()?.name_ref());
                let segment4 = segment4.and_then(|it| it.segment()?.name_ref());
                if segment3.is_none_or(|it| it.text() == "prelude")
                    && segment4.is_none_or(|it| matches!(it.text(), "core" | "std"))
                {
                    attr_flags.insert(AttrFlags::IS_TEST);
                }
            }

            let Some(this) = segment1 else { return ControlFlow::Continue(()) };
            let parent = qualifier.as_ref().and_then(|it| it.as_single_name_ref());
            let Some(parent) = parent else {
                // One segment, or three and more read by their last, as `as_up_to_two_segment`
                // reads them; the flags `has_word` answers by are set for one segment only.
                let single = qualifier.is_none();
                let flag = match word(name_text(this)) {
                    Some(Word::rustc_has_incoherent_inherent_impls) => {
                        AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS
                    }
                    Some(Word::rustc_allow_incoherent_impl) => {
                        AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL
                    }
                    Some(Word::rustc_scalable_vector) => AttrFlags::HAS_REPR,
                    Some(Word::fundamental) => AttrFlags::FUNDAMENTAL,
                    Some(Word::no_std) => AttrFlags::IS_NO_STD,
                    Some(Word::may_dangle) => AttrFlags::MAY_DANGLE,
                    Some(Word::rustc_paren_sugar) => AttrFlags::RUSTC_PAREN_SUGAR,
                    Some(Word::rustc_coinductive) => AttrFlags::RUSTC_COINDUCTIVE,
                    Some(Word::rustc_force_inline) => AttrFlags::RUSTC_FORCE_INLINE,
                    Some(Word::rustc_promotable) => AttrFlags::RUSTC_PROMOTABLE,
                    Some(Word::rustc_pass_indirectly_in_non_rustic_abis) => {
                        AttrFlags::RUSTC_PASS_INDIRECTLY_IN_NON_RUSTIC_ABIS
                    }
                    Some(Word::unstable) => AttrFlags::IS_UNSTABLE,
                    Some(Word::deprecated) => AttrFlags::IS_DEPRECATED,
                    Some(Word::macro_export) => AttrFlags::IS_MACRO_EXPORT,
                    Some(Word::must_use) => AttrFlags::IS_MUST_USE,
                    Some(Word::no_mangle) => AttrFlags::NO_MANGLE,
                    Some(Word::rustc_std_internal_symbol) => AttrFlags::LINKED_BY_NAME,
                    Some(Word::panic_handler) if single => {
                        AttrFlags::LINKED_BY_NAME | AttrFlags::PANIC_HANDLER
                    }
                    Some(Word::panic_handler) => AttrFlags::LINKED_BY_NAME,
                    Some(Word::used) => AttrFlags::USED,
                    Some(Word::track_caller) => AttrFlags::TRACK_CALLER,
                    Some(Word::inline) => AttrFlags::IS_INLINE,
                    Some(Word::rustc_inherit_overflow_checks) => {
                        AttrFlags::RUSTC_INHERIT_OVERFLOW_CHECKS
                    }
                    Some(Word::thread_local) => AttrFlags::THREAD_LOCAL,
                    Some(Word::pointee) => AttrFlags::IS_POINTEE,
                    Some(Word::non_exhaustive) => AttrFlags::NON_EXHAUSTIVE,
                    Some(Word::ignore) => AttrFlags::IS_IGNORE,
                    Some(Word::bench) => AttrFlags::IS_BENCH,
                    Some(Word::rustc_const_panic_str) => AttrFlags::RUSTC_CONST_PANIC_STR,
                    Some(Word::rustc_intrinsic) => AttrFlags::RUSTC_INTRINSIC,
                    Some(Word::rustc_safe_intrinsic) => AttrFlags::RUSTC_SAFE_INTRINSIC,
                    Some(Word::rustc_intrinsic_must_be_overridden) => {
                        AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN
                    }
                    Some(Word::rustc_allocator) => AttrFlags::RUSTC_ALLOCATOR,
                    Some(Word::rustc_deallocator) => AttrFlags::RUSTC_DEALLOCATOR,
                    Some(Word::rustc_reallocator) => AttrFlags::RUSTC_REALLOCATOR,
                    Some(Word::rustc_allocator_zeroed) => AttrFlags::RUSTC_ALLOCATOR_ZEROED,
                    Some(Word::rustc_reservation_impl) => AttrFlags::RUSTC_RESERVATION_IMPL,
                    Some(Word::rustc_deprecated_safe_2024) => AttrFlags::RUSTC_DEPRECATED_SAFE_2024,
                    Some(Word::rustc_skip_array_during_method_dispatch) => {
                        AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH
                    }
                    Some(Word::rustc_nounwind) if single => AttrFlags::RUSTC_NOUNWIND,
                    Some(Word::rustc_comptime) if single => AttrFlags::RUSTC_COMPTIME,
                    Some(Word::rustc_insignificant_dtor) if single => {
                        AttrFlags::RUSTC_INSIGNIFICANT_DTOR
                    }
                    Some(Word::rustc_as_ptr) if single => AttrFlags::RUSTC_AS_PTR,
                    Some(Word::rustc_no_implicit_autorefs) if single => {
                        AttrFlags::RUSTC_NO_IMPLICIT_AUTOREFS
                    }
                    Some(Word::rustc_never_returns_null_ptr) if single => {
                        AttrFlags::RUSTC_NEVER_RETURNS_NULL_PTR
                    }
                    Some(Word::rustc_canonical_symbol) if single => {
                        AttrFlags::RUSTC_CANONICAL_SYMBOL
                    }
                    Some(Word::marker) if single => AttrFlags::MARKER,
                    Some(Word::automatically_derived) if single => {
                        AttrFlags::AUTOMATICALLY_DERIVED
                    }
                    Some(Word::rustc_main) if single => AttrFlags::RUSTC_MAIN,
                    Some(Word::default) if single => AttrFlags::DEFAULT,
                    Some(Word::naked) if single => AttrFlags::NAKED,
                    Some(Word::rustc_deny_explicit_impl) if single => {
                        AttrFlags::RUSTC_DENY_EXPLICIT_IMPL
                    }
                    Some(Word::rustc_no_mir_inline) if single => AttrFlags::RUSTC_NO_MIR_INLINE,
                    Some(Word::cold) if single => AttrFlags::COLD,
                    Some(Word::must_not_suspend) if single => AttrFlags::MUST_NOT_SUSPEND,
                    _ => return ControlFlow::Continue(()),
                };
                attr_flags.insert(flag);
                return ControlFlow::Continue(());
            };
            let flag = match (word(name_text(parent)), word(name_text(this))) {
                (Some(Word::rust_analyzer), Some(Word::skip)) => AttrFlags::RUST_ANALYZER_SKIP,
                (Some(Word::rust_analyzer), Some(Word::prefer_underscore_import)) => {
                    AttrFlags::PREFER_UNDERSCORE_IMPORT
                }
                (Some(Word::diagnostic), Some(Word::do_not_recommend)) => {
                    AttrFlags::DIAGNOSTIC_DO_NOT_RECOMMEND
                }
                (Some(Word::blaze), Some(Word::may_suspend)) => AttrFlags::MAY_SUSPEND,
                _ => return ControlFlow::Continue(()),
            };
            attr_flags.insert(flag);
        }
        _ => {}
    };
    ControlFlow::Continue(())
}

/// The text of a path segment, `r#` stripped, as `AstPathExt` reads it.
#[inline]
fn name_text<'t>(name: ast::NameRef<'t>) -> &'t str {
    let text = name.text();
    text.strip_prefix("r#").unwrap_or(text)
}

/// The path's last segment and, when its qualifier is one segment, that qualifier first:
/// `AstPathExt::as_up_to_two_segment` without building a string.
#[inline]
fn up_to_two_segments<'t>(
    path: ast::Path<'t>,
) -> Option<(ast::NameRef<'t>, Option<ast::NameRef<'t>>)> {
    let parent = path.qualifier().and_then(|it| it.as_single_name_ref());
    let this = path.segment()?.name_ref()?;
    Some(match parent {
        Some(parent) => (parent, Some(this)),
        None => (this, None),
    })
}

/// Whether the path is the one segment `name`: `AstPathExt::is1` without building a string.
#[inline]
fn path_is(path: Option<ast::Path<'_>>, name: &str) -> bool {
    path.is_some_and(|it| is_one(it, name))
}

/// [`path_is`] of a path at hand.
#[inline]
fn is_one(path: ast::Path<'_>, name: &str) -> bool {
    path.as_single_name_ref().is_some_and(|it| name_text(it) == name)
}

/// Whether the token tree has a token `text` among its direct children.
fn tt_has_token(tt: ast::TokenTree<'_>, text: &str) -> bool {
    TokenTreeChildren::new(tt).any(|it| matches!(it, NodeOrToken::Token(it) if it.text() == text))
}

/// Whether a key-value attribute's value is the string literal `text`: `value_string` without
/// building a string.
fn string_value_is(attr: ast::KeyValueMeta<'_>, text: &str) -> bool {
    string_value(attr).is_some_and(|it| *it == *text)
}

/// The string literal a key-value attribute's value is, `AstKeyValueMetaExt::value_string`
/// lent from the tree: a buffer is made only when unescaping changes the text.
fn string_value(attr: ast::KeyValueMeta<'_>) -> Option<Cow<'_, str>> {
    if let Some(ast::Expr::Literal(value)) = attr.expr()
        && let ast::LiteralKind::String(value) = value.kind()
    {
        value.value().ok()
    } else {
        None
    }
}

/// The text of a [`name_set!`] member: its own name unless it gives one.
macro_rules! member_text {
    ($member:ident) => {
        stringify!($member)
    };
    ($member:ident = $text:literal) => {
        $text
    };
}

/// Declares `enum $Enum`, a `u8` tag for each name of a fixed set, with `$Enum::ALL` and
/// `$Enum::TEXTS` at the tags' indices and `$Enum::of`, the member a text is: a perfect hash
/// built at compile time ([`blaze_phf`]), one hash of the bytes, two loads and one compare.
macro_rules! name_set {
    ($(#[$attr:meta])* enum $Enum:ident { $($member:ident $(= $text:literal)?),* $(,)? }) => {
        $(#[$attr])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        #[allow(non_camel_case_types)]
        #[repr(u8)]
        enum $Enum {
            $($member,)*
        }

        impl $Enum {
            /// Every member, at its tag's index.
            const ALL: &[$Enum] = &[$($Enum::$member,)*];
            /// Each member's text, at its tag's index.
            const TEXTS: &[&str] = &[$(member_text!($member $(= $text)?),)*];

            /// The member `name` is, if any.
            #[inline]
            fn of(name: &str) -> Option<$Enum> {
                blaze_phf::phf_table!(static TABLE = $Enum::TEXTS;);
                TABLE.get(name).map(|at| $Enum::ALL[at])
            }
        }

        const _: () = assert!($Enum::ALL.len() < u8::MAX as usize);
    };
}

name_set! {
    /// A `#[doc(..)]` atom the flags read.
    enum DocWord { alias, hidden, keyword, notable_trait }
}

name_set! {
    /// An option of `#[rust_analyzer::completions(..)]`.
    enum Completions { ignore_flyimport, ignore_flyimport_methods, ignore_methods }
}

name_set! {
    /// An option of `#[rust_analyzer::macro_style(..)]`.
    enum MacroStyle { braces, brackets, parentheses }
}

name_set! {
    /// A receiver `#[rustc_skip_during_method_dispatch(..)]` names.
    enum SkipDuringMethodDispatch { array, boxed_slice }
}

name_set! {
    /// A `#[repr(..)]` hint, `r#` stripped.
    enum ReprWord {
        align, packed, simd, transparent, C,
        i8, i16, i32, i64, i128, isize,
        u8, u16, u32, u64, u128, usize,
    }
}

name_set! {
    /// A key of `#[link(..)]` that a native library reads.
    enum LinkKey { cfg, import_name_type, kind, modifiers, name }
}

name_set! {
    /// A `#[link(kind = "..")]`.
    enum LinkKind { dylib, framework, raw_dylib = "raw-dylib", static_ = "static" }
}

name_set! {
    /// A modifier of `#[link(modifiers = "..")]`, its sign split off.
    enum LinkModifier { as_needed = "as-needed", bundle, verbatim, whole_archive = "whole-archive" }
}

/// The [`Word`] a name is, if any.
#[inline]
fn word(name: &str) -> Option<Word> {
    Word::of(name)
}

name_set! {
    /// An attribute name the flags or the value queries read.
    enum Word {
    automatically_derived,
    bench,
    blaze,
    cold,
    completions,
    default,
    define_opaque,
    deprecated,
    diagnostic,
    do_not_recommend,
    doc,
    export_name,
    fundamental,
    ignore,
    inline,
    instruction_set,
    lang,
    link_name,
    link_ordinal,
    link_section,
    linkage,
    macro_export,
    macro_style,
    marker,
    may_dangle,
    may_suspend,
    must_not_suspend,
    must_use,
    naked,
    no_mangle,
    no_std,
    non_exhaustive,
    optimize,
    panic_handler,
    path,
    pointee,
    prefer_underscore_import,
    proc_macro_derive,
    repr,
    rust_analyzer,
    rustc_allocator,
    rustc_allocator_zeroed,
    rustc_allow_incoherent_impl,
    rustc_as_ptr,
    rustc_builtin_macro,
    rustc_canonical_symbol,
    rustc_coinductive,
    rustc_comptime,
    rustc_const_panic_str,
    rustc_deallocator,
    rustc_deny_explicit_impl,
    rustc_deprecated_safe_2024,
    rustc_diagnostic_item,
    rustc_force_inline,
    rustc_has_incoherent_inherent_impls,
    rustc_inherit_overflow_checks,
    rustc_insignificant_dtor,
    rustc_intrinsic,
    rustc_intrinsic_must_be_overridden,
    rustc_layout_scalar_valid_range_end,
    rustc_layout_scalar_valid_range_start,
    rustc_legacy_const_generics,
    rustc_main,
    rustc_must_implement_one_of,
    rustc_never_returns_null_ptr,
    rustc_no_implicit_autorefs,
    rustc_no_mir_inline,
    rustc_nounwind,
    rustc_paren_sugar,
    rustc_pass_indirectly_in_non_rustic_abis,
    rustc_promotable,
    rustc_reallocator,
    rustc_reservation_impl,
    rustc_safe_intrinsic,
    rustc_scalable_vector,
    rustc_skip_array_during_method_dispatch,
    rustc_skip_during_method_dispatch,
    rustc_std_internal_symbol,
    sanitize,
    skip,
    stable,
    target_feature,
    thread_local,
    track_caller,
    unstable,
    used,
    }
}

bitflags::bitflags! {
    /// One word per item, field or generic parameter: 16 bytes, read with one bit test.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct AttrFlags: u128 {
        const RUST_ANALYZER_SKIP = 1 << 0;

        const LANG_ITEM = 1 << 1;

        const HAS_DOC_ALIASES = 1 << 2;
        const HAS_DOC_KEYWORD = 1 << 3;
        const IS_DOC_NOTABLE_TRAIT = 1 << 4;
        const IS_DOC_HIDDEN = 1 << 5;

        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS = 1 << 6;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
        const FUNDAMENTAL = 1 << 8;
        const RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH = 1 << 9;
        const RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH = 1 << 10;
        const HAS_REPR = 1 << 11;
        const HAS_TARGET_FEATURE = 1 << 12;
        const RUSTC_DEPRECATED_SAFE_2024 = 1 << 13;
        const HAS_LEGACY_CONST_GENERICS = 1 << 14;
        const NO_MANGLE = 1 << 15;
        const NON_EXHAUSTIVE = 1 << 16;
        const RUSTC_RESERVATION_IMPL = 1 << 17;
        const RUSTC_CONST_PANIC_STR = 1 << 18;
        const MAY_DANGLE = 1 << 19;

        const RUSTC_INTRINSIC = 1 << 20;
        const RUSTC_SAFE_INTRINSIC = 1 << 21;
        const RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN = 1 << 22;
        const RUSTC_ALLOCATOR = 1 << 23;
        const RUSTC_DEALLOCATOR = 1 << 24;
        const RUSTC_REALLOCATOR = 1 << 25;
        const RUSTC_ALLOCATOR_ZEROED = 1 << 26;

        const IS_UNSTABLE = 1 << 27;
        const IS_IGNORE = 1 << 28;
        // FIXME: `IS_TEST` and `IS_BENCH` should be based on semantic information, not textual match.
        const IS_BENCH = 1 << 29;
        const IS_TEST = 1 << 30;
        const IS_EXPORT_NAME_MAIN = 1 << 31;
        const IS_MACRO_EXPORT = 1 << 32;
        const IS_NO_STD = 1 << 33;
        const IS_DERIVE_OR_BUILTIN_MACRO = 1 << 34;
        const IS_DEPRECATED = 1 << 35;
        const HAS_PATH = 1 << 36;
        const HAS_CFG = 1 << 37;

        const COMPLETE_IGNORE_FLYIMPORT = 1 << 38;
        const COMPLETE_IGNORE_FLYIMPORT_METHODS = 1 << 39;
        const COMPLETE_IGNORE_METHODS = 1 << 40;

        const RUSTC_LAYOUT_SCALAR_VALID_RANGE = 1 << 41;
        const RUSTC_PAREN_SUGAR = 1 << 42;
        const RUSTC_COINDUCTIVE = 1 << 43;
        const RUSTC_FORCE_INLINE = 1 << 44;
        const IS_POINTEE = 1 << 45;

        const MACRO_STYLE_BRACES = 1 << 46;
        const MACRO_STYLE_BRACKETS = 1 << 47;
        const MACRO_STYLE_PARENTHESES = 1 << 48;

        const PREFER_UNDERSCORE_IMPORT = 1 << 49;

        const IS_MUST_USE = 1 << 50;

        const DIAGNOSTIC_DO_NOT_RECOMMEND = 1 << 51;

        const HAS_RUSTC_MUST_IMPLEMENT_ONE_OF = 1 << 52;

        const TRACK_CALLER = 1 << 53;
        const THREAD_LOCAL = 1 << 54;
        const RUSTC_PROMOTABLE = 1 << 55;
        const HAS_DEFINE_OPAQUE = 1 << 56;
        const RUSTC_INHERIT_OVERFLOW_CHECKS = 1 << 57;
        const RUSTC_PASS_INDIRECTLY_IN_NON_RUSTIC_ABIS = 1 << 58;
        /// `#[inline]` or `#[inline(always)]`: the crates that use the function compile it.
        const IS_INLINE = 1 << 59;
        /// `#[used]`: kept in the object though nothing names it.
        const USED = 1 << 60;
        /// `#[inline(never)]`: not built into its callers, also where they are optimized.
        const INLINE_NEVER = 1 << 61;
        /// `#[inline(always)]`: built into its callers where they are optimized.
        const INLINE_ALWAYS = 1 << 62;
        /// `#[rustc_std_internal_symbol]` or `#[panic_handler]`: a function a declaration of
        /// another crate links to by its name.
        const LINKED_BY_NAME = 1 << 63;

        // The flags below gate a value query, so an item without the attribute never runs it.
        /// `#[export_name = ..]`: [`AttrFlags::export_name`].
        const HAS_EXPORT_NAME = 1 << 64;
        /// `#[link_name = ..]`: [`AttrFlags::link_name`].
        const HAS_LINK_NAME = 1 << 65;
        /// `#[link_section = ..]`: [`AttrFlags::link_section`].
        const HAS_LINK_SECTION = 1 << 66;
        /// `#[linkage = ..]`: [`AttrFlags::linkage`].
        const HAS_LINKAGE = 1 << 67;
        /// `#[rustc_diagnostic_item = ..]`: [`AttrFlags::diagnostic_item`].
        const HAS_DIAGNOSTIC_ITEM = 1 << 68;
        /// `#[rustc_nounwind]`, one segment: [`AttrFlags::is_rustc_nounwind`].
        const RUSTC_NOUNWIND = 1 << 69;
        /// `#[rustc_comptime]`, one segment: `has_word(.., "rustc_comptime")`.
        const RUSTC_COMPTIME = 1 << 70;
        /// `#[panic_handler]`, one segment: `has_word(.., "panic_handler")`.
        const PANIC_HANDLER = 1 << 71;
        /// `#[stable(..)]`: [`stability::stability`].
        const IS_STABLE = 1 << 72;
        // One segment each, as rustc's parsers of these words read them.
        /// `#[rustc_insignificant_dtor]` on an ADT: its drop matters only through its type
        /// arguments.
        const RUSTC_INSIGNIFICANT_DTOR = 1 << 73;
        /// `#[rustc_as_ptr]`.
        const RUSTC_AS_PTR = 1 << 74;
        /// `#[rustc_no_implicit_autorefs]`.
        const RUSTC_NO_IMPLICIT_AUTOREFS = 1 << 75;
        /// `#[rustc_never_returns_null_ptr]`.
        const RUSTC_NEVER_RETURNS_NULL_PTR = 1 << 76;
        /// `#[rustc_canonical_symbol]` on a foreign function of `core` or `std`.
        const RUSTC_CANONICAL_SYMBOL = 1 << 77;
        /// `#[sanitize(..)]`, one segment: [`disabled_sanitizers`].
        const HAS_SANITIZE = 1 << 78;
        /// `#[marker]`.
        const MARKER = 1 << 79;
        /// `#[automatically_derived]`.
        const AUTOMATICALLY_DERIVED = 1 << 80;
        /// `#[rustc_main]`.
        const RUSTC_MAIN = 1 << 81;
        /// `#[default]`, on an enum's variant.
        const DEFAULT = 1 << 82;
        /// `#[naked]` or `#[unsafe(naked)]` on a function.
        const NAKED = 1 << 83;
        /// `#[rustc_deny_explicit_impl]`, on a trait: no impl of it may be written.
        const RUSTC_DENY_EXPLICIT_IMPL = 1 << 84;
        /// `#[link_ordinal(..)]`, on an item of an `extern` block: imported by ordinal.
        const HAS_LINK_ORDINAL = 1 << 85;
        // One segment each; the inliner refuses on them as rustc's `is_inline_valid_on_fn`
        // and `check_codegen_attributes` do.
        /// `#[rustc_no_mir_inline]`.
        const RUSTC_NO_MIR_INLINE = 1 << 86;
        /// `#[cold]`.
        const COLD = 1 << 87;
        /// `#[optimize(none)]`: rustc's `OptimizeAttr::DoNotOptimize`.
        const OPTIMIZE_NONE = 1 << 88;
        /// `#[instruction_set(..)]`, any argument.
        const HAS_INSTRUCTION_SET = 1 << 89;
        /// `#[blaze::may_suspend]`, a word, on a function or a field: a blaze extension, read by
        /// `hir-ty-suspend` (a call of it may park its fiber).
        const MAY_SUSPEND = 1 << 90;
        /// rustc's `#[must_not_suspend]`, one segment, a word or `= "reason"`, on an ADT: a value
        /// of it must not be held across a suspend.
        const MUST_NOT_SUSPEND = 1 << 91;
    }
}

const _: () = assert!(size_of::<AttrFlags>() == 16);

/// The attributes rustc appends to `krate`'s root's own inner attributes, each in its file: the
/// crate attributes given on the command line that it applies
/// ([`base_db::applied_crate_attr_files`]), in the order given.
pub fn cli_crate_attrs(db: &Db, krate: Crate) -> impl Iterator<Item = InFile<ast::Attr<'_>>> {
    base_db::applied_crate_attr_files(db, krate).iter().flat_map(move |&file| {
        file.parse(db).view().attrs().map(move |attr| InFile::new(HirFileId::from(file), attr))
    })
}

/// Whether the crate root declares `#![no_std]`, looking through `cfg_attr` gating.
#[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
pub(crate) fn crate_supports_no_std(db: &Db, krate: Crate) -> bool {
    fn contains_no_std(meta: ast::Meta<'_>) -> bool {
        match meta {
            ast::Meta::PathMeta(meta) => path_is(meta.path(), "no_std"),
            ast::Meta::CfgAttrMeta(meta) => meta.metas().any(contains_no_std),
            ast::Meta::UnsafeMeta(meta) => meta.meta().is_some_and(contains_no_std),
            ast::Meta::CfgMeta(_) | ast::Meta::KeyValueMeta(_) | ast::Meta::TokenTreeMeta(_) => {
                false
            }
        }
    }

    let root_file = krate.root_file_id(db).parse(db).view();
    root_file
        .attrs()
        .chain(cli_crate_attrs(db, krate).map(|attr| attr.value))
        .filter_map(|attr| attr.meta())
        .any(contains_no_std)
}

/// The syntax `owner`'s attributes are read off, its outer `mod` declaration, the files of the
/// crate attributes applied after its own for a crate root ([`cli_crate_attrs`]), and its
/// crate: off the tree `tree` says.
fn attrs_source(
    db: &Db,
    owner: AttrDefId,
    tree: Tree,
) -> (InFile<ast::AnyHasAttrs<'_>>, Option<InFile<ast::Module<'_>>>, &[EditionedFileId], Crate) {
    let (owner, krate) = match owner {
        AttrDefId::ModuleId(id) => {
            let def_map = id.def_map(db);
            let krate = def_map.krate();
            let (definition, declaration, crate_attrs) = match def_map.module(db, id).origin {
                ModuleOrigin::CrateRoot { definition } => {
                    let definition_source = file_root(db, definition, tree);
                    let definition = InFile::new(definition.into(), definition_source.into());
                    (definition, None, base_db::applied_crate_attr_files(db, krate))
                }
                ModuleOrigin::File { declaration, declaration_tree_id, definition, .. } => {
                    let definition_source = file_root(db, definition, tree);
                    let definition = InFile::new(definition.into(), definition_source.into());
                    let declaration = InFile::new(declaration_tree_id.file_id(), declaration);
                    let declaration = declaration.with_value(item_node(db, declaration, id, tree));
                    (definition, Some(declaration), &[][..])
                }
                ModuleOrigin::Inline { definition_tree_id, definition } => {
                    let definition = InFile::new(definition_tree_id.file_id(), definition);
                    let definition =
                        definition.with_value(item_node(db, definition, id, tree).into());
                    (definition, None, &[][..])
                }
                ModuleOrigin::BlockExpr { block, .. } => {
                    let definition = block.to_view(db);
                    (block.with_value(definition.into()), None, &[][..])
                }
            };
            return (definition, declaration, crate_attrs, krate);
        }
        AttrDefId::AdtId(AdtId::StructId(it)) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::AdtId(AdtId::UnionId(it)) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::AdtId(AdtId::EnumId(it)) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::FunctionId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::EnumVariantId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::StaticId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::ConstId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::TraitId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::TypeAliasId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::MacroId(MacroId::MacroRulesId(it)) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::MacroId(MacroId::Macro2Id(it)) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::MacroId(MacroId::ProcMacroId(it)) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::ImplId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::ExternBlockId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::ExternCrateId(it) => attrs_from_ast_id_loc(db, it, tree),
        AttrDefId::UseId(it) => attrs_from_ast_id_loc(db, it, tree),
    };
    (owner, None, &[], krate)
}

fn resolver_for_attr_def_id(db: &Db, owner: AttrDefId) -> Resolver<'_> {
    match owner {
        AttrDefId::ModuleId(id) => id.resolver(db),
        AttrDefId::AdtId(AdtId::StructId(id)) => id.resolver(db),
        AttrDefId::AdtId(AdtId::UnionId(id)) => id.resolver(db),
        AttrDefId::AdtId(AdtId::EnumId(id)) => id.resolver(db),
        AttrDefId::FunctionId(id) => id.resolver(db),
        AttrDefId::EnumVariantId(id) => id.resolver(db),
        AttrDefId::StaticId(id) => id.resolver(db),
        AttrDefId::ConstId(id) => id.resolver(db),
        AttrDefId::TraitId(id) => id.resolver(db),
        AttrDefId::TypeAliasId(id) => id.resolver(db),
        AttrDefId::MacroId(MacroId::Macro2Id(id)) => id.resolver(db),
        AttrDefId::MacroId(MacroId::MacroRulesId(id)) => id.resolver(db),
        AttrDefId::MacroId(MacroId::ProcMacroId(id)) => id.resolver(db),
        AttrDefId::ImplId(id) => id.resolver(db),
        AttrDefId::ExternBlockId(id) => id.resolver(db),
        AttrDefId::ExternCrateId(id) => id.resolver(db),
        AttrDefId::UseId(id) => id.resolver(db),
    }
}

fn collect_attrs<'db, BreakValue>(
    db: &'db Db,
    owner: AttrDefId,
    mut callback: impl FnMut(ast::Meta<'db>) -> ControlFlow<BreakValue>,
) -> Option<BreakValue> {
    let (source, outer_mod_decl, crate_attrs, krate) = attrs_source(db, owner, Tree::Outline);
    collect_attrs_in(db, source, outer_mod_decl, crate_attrs, krate, |meta, _| callback(meta))
}

/// [`collect_attrs`] over a source [`attrs_source`] already found, each attribute with its file,
/// in rustc's order: the outer `mod` declaration's, the owner's own, then the crate
/// attributes' (`crate_attrs`, rustc appends them to the crate root's).
fn collect_attrs_in<'db, BreakValue>(
    db: &'db Db,
    source: InFile<ast::AnyHasAttrs<'db>>,
    outer_mod_decl: Option<InFile<ast::Module<'db>>>,
    crate_attrs: &'db [EditionedFileId],
    krate: Crate,
    mut callback: impl FnMut(ast::Meta<'db>, HirFileId) -> ControlFlow<BreakValue>,
) -> Option<BreakValue> {
    let mut cfg_options = None;
    let mut cfg = || *cfg_options.get_or_insert_with(|| krate.cfg_options(db));
    if let Some(decl) = outer_mod_decl {
        let file = decl.file_id;
        let found =
            expand_cfg_attr_view(decl.value.attrs(), &mut cfg, |meta, _| callback(meta, file));
        if found.is_some() {
            return found;
        }
    }
    let file = source.file_id;
    let attrs = ast::attrs_including_inner(&source.value);
    let found = expand_cfg_attr_view(attrs, &mut cfg, |meta, _| callback(meta, file));
    if found.is_some() {
        return found;
    }
    for &attr_file in crate_attrs {
        let file = HirFileId::from(attr_file);
        let attrs = attr_file.parse(db).view().attrs();
        let found = expand_cfg_attr_view(attrs, &mut cfg, |meta, _| callback(meta, file));
        if found.is_some() {
            return found;
        }
    }
    None
}

/// The value of the `#[name = value]` attribute of `owner`, with the macros in it expanded:
/// rustc expands the expression of a key-value attribute before it reads it
/// (`rustc_expand::expand::InvocationCollector::visit_attribute` walks into `AttrArgs::Eq`).
pub(crate) fn symbol_attr(db: &Db, owner: AttrDefId, name: &str) -> Option<Symbol> {
    let (source, outer_mod_decl, crate_attrs, krate) = attrs_source(db, owner, Tree::Live);
    let value =
        collect_attrs_in(db, source, outer_mod_decl, crate_attrs, krate, |attr, file| match attr {
            ast::Meta::KeyValueMeta(attr) if path_is(attr.path(), name) => match attr.expr() {
                Some(value) => ControlFlow::Break(InFile::new(file, value)),
                None => ControlFlow::Continue(()),
            },
            _ => ControlFlow::Continue(()),
        })?;
    attr_value(db, owner, krate, value)
}

/// The string a key-value attribute's value is, with the macros in it expanded by the
/// resolver of `owner`.
fn attr_value(
    db: &Db,
    owner: AttrDefId,
    krate: Crate,
    value: InFile<ast::Expr<'_>>,
) -> Option<Symbol> {
    let InFile { file_id, value } = value;
    // A string literal, the usual value, reads as `expand_attr_value` reads it, with no
    // resolver or span map built for it.
    let mut literal = value;
    while let ast::Expr::ParenExpr(paren) = literal {
        literal = paren.expr()?;
    }
    match literal {
        ast::Expr::Literal(literal) => match literal.kind() {
            ast::LiteralKind::String(string) => Some(Symbol::intern(&string.value().ok()?)),
            _ => None,
        },
        ast::Expr::MacroExpr(_) => {
            let resolver = resolver_for_attr_def_id(db, owner);
            let value = docs::expand_attr_value(db, krate, resolver, InFile::new(file_id, value))?;
            Some(Symbol::intern(&value))
        }
        _ => None,
    }
}

fn collect_field_attrs<'db, T>(
    db: &'db Db,
    variant: VariantId,
    tree: Tree,
    frame: &Frame,
    mut field_attrs: impl FnMut(&CfgOptions, InFile<ast::AnyHasAttrs<'db>>) -> T,
) -> ArenaMap<LocalFieldId, T> {
    let mut values = Vec::new_in(frame);
    for_each_field(db, variant, tree, |_, cfg_options, field| {
        values.push(field_attrs(cfg_options, field));
    });
    // alloc: the memo's map, exact: one slot a field the cfg leaves in
    let mut result = ArenaMap::with_capacity(values.len());
    // `for_each_field` numbers the fields `0..` with no gap, so the index is the position.
    for (idx, value) in values.drain(..).enumerate() {
        result.insert(la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(idx as u32)), value);
    }
    result
}

/// An `ArenaMap` of `entries` (in index order), with room for exactly the last index.
fn exact_arena_map<T, V: Copy>(
    entries: &[(la_arena::Idx<T>, V)],
) -> ArenaMap<la_arena::Idx<T>, V> {
    let len = entries.last().map_or(0, |(idx, _)| u32::from(idx.into_raw()) as usize + 1);
    // alloc: the memo's map, exact
    let mut map = ArenaMap::with_capacity(len);
    for &(idx, value) in entries {
        map.insert(idx, value);
    }
    map
}

/// Calls `each` with the index, the crate's cfg options and the syntax of every field of
/// `variant` its cfg leaves in, in order: the indices are `0..` with no gap. The fields are
/// read off the tree `tree` says.
fn for_each_field<'db>(
    db: &'db Db,
    variant: VariantId,
    tree: Tree,
    mut each: impl FnMut(u32, &CfgOptions, InFile<ast::AnyHasAttrs<'db>>),
) {
    let (variant_syntax, krate) = match variant {
        VariantId::EnumVariantId(it) => attrs_from_ast_id_loc(db, it, tree),
        VariantId::StructId(it) => attrs_from_ast_id_loc(db, it, tree),
        VariantId::UnionId(it) => attrs_from_ast_id_loc(db, it, tree),
    };
    let cfg_options = krate.cfg_options(db);
    let variant_syntax =
        variant_syntax.with_value(ast::VariantDef::cast(variant_syntax.value.syntax()).unwrap());
    let fields = match variant_syntax.value {
        ast::VariantDef::Struct(it) => it.field_list(),
        ast::VariantDef::Union(it) => it.record_field_list().map(ast::FieldList::RecordFieldList),
        ast::VariantDef::Variant(it) => it.field_list(),
    };
    let Some(fields) = fields else {
        return;
    };

    let mut idx = 0;
    match fields {
        ast::FieldList::RecordFieldList(fields) => {
            for field in fields.fields() {
                if AttrFlags::is_cfg_enabled_for(field, cfg_options).is_ok() {
                    each(idx, cfg_options, variant_syntax.with_value(field.into()));
                    idx += 1;
                }
            }
        }
        ast::FieldList::TupleFieldList(fields) => {
            for field in fields.fields() {
                if AttrFlags::is_cfg_enabled_for(field, cfg_options).is_ok() {
                    each(idx, cfg_options, variant_syntax.with_value(field.into()));
                    idx += 1;
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RustcLayoutScalarValidRange {
    pub start: Option<u128>,
    pub end: Option<u128>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct DeriveInfo {
    pub trait_name: Symbol,
    pub helpers: Box<[Symbol]>,
}

fn extract_doc_aliases(
    result: &mut Vec<Symbol, &Frame>,
    attr: ast::Meta<'_>,
) -> ControlFlow<Infallible> {
    if let ast::Meta::TokenTreeMeta(attr) = attr
        && path_is(attr.path(), "doc")
        && let Some(tt) = attr.token_tree()
    {
        for atom in DocAtom::parse(tt) {
            match atom {
                DocAtom::Alias(aliases) => {
                    result.extend(aliases.into_iter().map(|alias| Symbol::intern(&alias)))
                }
                DocAtom::KeyValue { key, value } if key == "alias" => {
                    result.push(Symbol::intern(&value))
                }
                _ => {}
            }
        }
    }
    ControlFlow::Continue(())
}

/// A native library a crate is linked against, from a `#[link(...)]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeLib {
    pub name: Symbol,
    pub kind: NativeLibKind,
    /// `+verbatim`: the name is the file's, with no `lib` or extension added.
    pub verbatim: bool,
    /// `+whole-archive`: every object of a static library is linked, reached or not.
    pub whole_archive: bool,
    /// Not `-bundle`: a static library's objects go into the archive of a `staticlib` that
    /// links it, as rustc's default is.
    pub bundle: bool,
    /// `+as-needed` or `-as-needed`, when written: `None` leaves the linker's default.
    pub as_needed: Option<bool>,
    /// `import_name_type = ".."`, as written.
    pub import_name_type: Option<Symbol>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeLibKind {
    /// No `kind`: the linker takes whichever it finds, as `dylib` does.
    Unspecified,
    Dylib,
    Static,
    Framework,
    RawDylib,
    /// A `kind` other than these (`"link-arg"`, or one rustc refuses), as written.
    Other(Symbol),
}

/// The library of a `link(...)`, and the `cfg(...)` it is linked under. `None` for one with no
/// name (`wasm_import_module` alone).
fn parse_link_tt(tt: ast::TokenTree<'_>) -> Option<(NativeLib, Option<CfgExpr>)> {
    let mut lib = NativeLib {
        name: Symbol::empty(),
        kind: NativeLibKind::Unspecified,
        verbatim: false,
        whole_archive: false,
        bundle: true,
        as_needed: None,
        import_name_type: None,
    };
    let mut cfg = None;
    let mut named = false;
    let mut children = TokenTreeChildren::new(tt).peekable();
    while let Some(child) = children.next() {
        let NodeOrToken::Token(key) = child else { continue };
        if key.kind() != SyntaxKind::IDENT {
            continue;
        }
        let key = LinkKey::of(key.text());
        match children.peek() {
            Some(NodeOrToken::Node(group)) if key == Some(LinkKey::cfg) => {
                cfg = link_cfg(*group);
                children.next();
            }
            Some(NodeOrToken::Token(eq)) if eq.kind() == T![=] => {
                children.next();
                let Some(NodeOrToken::Token(value)) = children.next() else { continue };
                let Some(value) = ast::String::cast(value) else { continue };
                let Ok(value) = value.value() else { continue };
                match key {
                    Some(LinkKey::name) => {
                        lib.name = Symbol::intern(&value);
                        named = true;
                    }
                    Some(LinkKey::kind) => {
                        lib.kind = match LinkKind::of(&value) {
                            Some(LinkKind::static_) => NativeLibKind::Static,
                            Some(LinkKind::dylib) => NativeLibKind::Dylib,
                            Some(LinkKind::framework) => NativeLibKind::Framework,
                            Some(LinkKind::raw_dylib) => NativeLibKind::RawDylib,
                            None => NativeLibKind::Other(Symbol::intern(&value)),
                        }
                    }
                    Some(LinkKey::import_name_type) => {
                        lib.import_name_type = Some(Symbol::intern(&value));
                    }
                    Some(LinkKey::modifiers) => {
                        for modifier in value.split(',') {
                            let (on, name) = modifier.split_at(modifier.len().min(1));
                            let on = on == "+";
                            match LinkModifier::of(name) {
                                Some(LinkModifier::verbatim) => lib.verbatim = on,
                                Some(LinkModifier::whole_archive) => lib.whole_archive = on,
                                Some(LinkModifier::bundle) => lib.bundle = on,
                                Some(LinkModifier::as_needed) => lib.as_needed = Some(on),
                                None => (),
                            }
                        }
                    }
                    Some(LinkKey::cfg) | None => (),
                }
            }
            _ => (),
        }
    }
    named.then_some((lib, cfg))
}

/// The string a `link(...)` gives `key`, as `key = "value"`, interned.
fn link_symbol(tt: ast::TokenTree<'_>, key: &str) -> Option<Symbol> {
    let mut children = TokenTreeChildren::new(tt).peekable();
    while let Some(child) = children.next() {
        let NodeOrToken::Token(name) = child else { continue };
        if name.kind() != SyntaxKind::IDENT || name.text() != key {
            continue;
        }
        let Some(NodeOrToken::Token(eq)) = children.next() else { continue };
        if eq.kind() != T![=] {
            continue;
        }
        let Some(NodeOrToken::Token(value)) = children.next() else { continue };
        return ast::String::cast(value).and_then(|it| Some(Symbol::intern(&it.value().ok()?)));
    }
    None
}

/// The `cfg(...)` of a `link(...)`, the group after `cfg`: read as the predicate of a
/// `#[cfg]` with that group. Only a `#[link(.., cfg(..))]`, once per extern block, inside its
/// memo.
fn link_cfg(group: ast::TokenTree<'_>) -> Option<CfgExpr> {
    let parse = blaze_mem::scratch::with_string(|text| {
        use std::fmt::Write;
        // A `String` takes every write.
        let _ = write!(text, "#[cfg{}] fn f() {{}}", group.syntax().text());
        SourceFile::parse(text, span::Edition::CURRENT)
    });
    let attr = parse.root().descendants().find_map(ast::Attr::cast)?;
    match attr.meta()? {
        ast::Meta::CfgMeta(it) => Some(CfgExpr::parse_from_view(it.cfg_predicate()?)),
        _ => None,
    }
}

fn extract_cfgs(
    result: &mut Vec<CfgExpr, &Frame>,
    attr: ast::Meta<'_>,
) -> ControlFlow<Infallible> {
    if let ast::Meta::CfgMeta(attr) = attr
        && let Some(cfg_predicate) = attr.cfg_predicate()
    {
        result.push(CfgExpr::parse_from_view(cfg_predicate));
    }
    ControlFlow::Continue(())
}


#[blaze_query::tracked]
impl AttrFlags {
    #[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
    pub fn query(db: &Db, owner: AttrDefId) -> AttrFlags {
        let mut column = |file: EditionedFileId, krate: Crate| file_attr_flags(db, file, krate);
        column_flags(db, owner, &mut column).unwrap_or_else(|| walked_flags(db, owner))
    }

    #[inline]
    pub fn query_field(db: &Db, field: FieldId) -> AttrFlags {
        return field_attr_flags(db, field.parent)
            .get(u32::from(field.local_id.into_raw()) as usize)
            .copied()
            .unwrap_or_else(AttrFlags::empty);

        /// The flags of each field the cfg leaves in, indexed by `LocalFieldId`: 16 bytes a
        /// field, read by index.
        #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
        fn field_attr_flags(db: &Db, variant: VariantId) -> Box<[AttrFlags]> {
            blaze_mem::frame::current(|frame| {
                let mut result = Vec::new_in(frame);
                for_each_field(db, variant, Tree::Outline, |_, cfg_options, field| {
                    let mut attr_flags = AttrFlags::empty();
                    expand_cfg_attr_view(
                        field.value.attrs(),
                        || cfg_options,
                        |attr, _| match_attr_flags(&mut attr_flags, attr),
                    );
                    result.push(attr_flags);
                });
                // alloc: the memo's flags, exact
                Box::from(&result[..])
            })
        }
    }

    #[inline]
    pub fn query_generic_params(
        db: &Db,
        def: GenericDefId,
    ) -> &(ArenaMap<LocalLifetimeParamId, AttrFlags>, ArenaMap<LocalTypeOrConstParamId, AttrFlags>)
    {
        let generic_params = GenericParams::of(db, def);
        let params_count_excluding_self =
            generic_params.len() - usize::from(generic_params.trait_self_param().is_some());
        if params_count_excluding_self == 0 {
            return const { &(ArenaMap::new(), ArenaMap::new()) };
        }
        return generic_params_attr_flags(db, def);

        #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
        fn generic_params_attr_flags(
            db: &Db,
            def: GenericDefId,
        ) -> (ArenaMap<LocalLifetimeParamId, AttrFlags>, ArenaMap<LocalTypeOrConstParamId, AttrFlags>)
        {
            blaze_mem::frame::current(|frame| {
                let mut cfg_options = None;
                let mut cfg_options =
                    || *cfg_options.get_or_insert_with(|| def.krate(db).cfg_options(db));

                let mut flags_of = |attrs: ast::AstChildren<'_, ast::Attr<'_>>| {
                    let mut attr_flags = AttrFlags::empty();
                    expand_cfg_attr_view(attrs, &mut cfg_options, |attr, _| {
                        match_attr_flags(&mut attr_flags, attr)
                    });
                    attr_flags
                };
                // One walk of the written params, each zipped with its id in order, as
                // `HasChildSourceView` finds one of them.
                let generic_params = GenericParams::of(db, def);
                let (_, list) = def.file_id_and_params_of_view(db);

                let mut lifetimes = Vec::new_in(frame);
                let lifetime_params = list.into_iter().flat_map(|it| it.lifetime_params());
                for ((lifetime_id, _), lifetime) in generic_params.iter_lt().zip(lifetime_params) {
                    let attr_flags = flags_of(lifetime.attrs());
                    if !attr_flags.is_empty() {
                        lifetimes.push((lifetime_id, attr_flags));
                    }
                }

                let mut type_and_consts = Vec::new_in(frame);
                let mut ids = generic_params.iter_type_or_consts().map(|(id, _)| id);
                // For traits the first type index is `Self`, the trait itself.
                if let GenericDefId::TraitId(id) = def
                    && let Some(self_id) = ids.next()
                {
                    let attr_flags = flags_of(id.lookup(db).source_view(db).value.attrs());
                    if !attr_flags.is_empty() {
                        type_and_consts.push((self_id, attr_flags));
                    }
                }
                let type_or_const_params = list.into_iter().flat_map(|it| it.type_or_const_params());
                for (type_or_const_id, type_or_const) in ids.zip(type_or_const_params) {
                    let attr_flags = flags_of(type_or_const.attrs());
                    if !attr_flags.is_empty() {
                        type_and_consts.push((type_or_const_id, attr_flags));
                    }
                }

                (exact_arena_map(&lifetimes), exact_arena_map(&type_and_consts))
            })
        }
    }

    #[inline]
    pub fn query_lifetime_param(db: &Db, owner: LifetimeParamId) -> AttrFlags {
        AttrFlags::query_generic_params(db, owner.parent)
            .0
            .get(owner.local_id)
            .copied()
            .unwrap_or_else(AttrFlags::empty)
    }
    #[inline]
    pub fn query_type_or_const_param(
        db: &Db,
        owner: TypeOrConstParamId,
    ) -> AttrFlags {
        AttrFlags::query_generic_params(db, owner.parent)
            .1
            .get(owner.local_id)
            .copied()
            .unwrap_or_else(AttrFlags::empty)
    }

    #[inline]
    pub(crate) fn is_cfg_enabled_for<'t>(
        owner: impl HasAttrs<'t>,
        cfg_options: &CfgOptions,
    ) -> Result<(), CfgExpr> {
        Self::is_cfg_enabled_in(ast::AnyHasAttrs::new(owner), cfg_options)
    }

    /// [`AttrFlags::is_cfg_enabled_for`] of an owner as the view every attribute owner is:
    /// one instance for every kind of owner.
    pub(crate) fn is_cfg_enabled_in(
        owner: ast::AnyHasAttrs<'_>,
        cfg_options: &CfgOptions,
    ) -> Result<(), CfgExpr> {
        let result = expand_cfg_attr_view(
            ast::attrs_including_inner(&owner),
            || cfg_options,
            |attr, _| {
                if let ast::Meta::CfgMeta(attr) = attr
                    && let Some(cfg_predicate) = attr.cfg_predicate()
                    && let cfg = CfgExpr::parse_from_view(cfg_predicate)
                    && cfg_options.check(&cfg) == Some(false)
                {
                    ControlFlow::Break(cfg)
                } else {
                    ControlFlow::Continue(())
                }
            },
        );
        match result {
            Some(cfg) => Err(cfg),
            None => Ok(()),
        }
    }

    #[inline]
    pub fn lang_item(db: &Db, owner: AttrDefId) -> Option<Symbol> {
        AttrFlags::query(db, owner).lang_item_with_attrs(db, owner)
    }

    /// The symbol a foreign item is declared to link against, if it is not its own name.
    #[inline]
    pub fn link_name(db: &Db, owner: AttrDefId) -> Option<Symbol> {
        if !AttrFlags::query(db, owner).contains(AttrFlags::HAS_LINK_NAME) {
            return None;
        }
        return link_name(db, owner);

        #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
        fn link_name(db: &Db, owner: AttrDefId) -> Option<Symbol> {
            symbol_attr(db, owner, "link_name")
        }
    }

    /// The diagnostic item `#[rustc_diagnostic_item = ".."]` names an item, rustc's
    /// `diagnostic_items` entry for it.
    #[inline]
    pub fn diagnostic_item(db: &Db, owner: AttrDefId) -> Option<Symbol> {
        if !AttrFlags::query(db, owner).contains(AttrFlags::HAS_DIAGNOSTIC_ITEM) {
            return None;
        }
        return diagnostic_item(db, owner);

        #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
        fn diagnostic_item(db: &Db, owner: AttrDefId) -> Option<Symbol> {
            symbol_attr(db, owner, "rustc_diagnostic_item")
        }
    }

    /// The native libraries the `#[link(...)]`s of an `extern` block link its crate against,
    /// those whose `cfg(...)` the crate does not have left out, in the order written.
    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    pub fn native_libs(db: &Db, owner: ExternBlockId) -> Box<[NativeLib]> {
        let krate = owner.lookup(db).container.krate(db);
        blaze_mem::frame::current(|frame| {
            let mut libs = Vec::new_in(frame);
            collect_attrs::<Infallible>(db, AttrDefId::ExternBlockId(owner), |attr| {
                if let ast::Meta::TokenTreeMeta(attr) = &attr
                    && path_is(attr.path(), "link")
                    && let Some(tt) = attr.token_tree()
                    && let Some((lib, cfg)) = parse_link_tt(tt)
                    && cfg.is_none_or(|it| krate.cfg_options(db).check(&it) != Some(false))
                {
                    libs.push(lib);
                }
                ControlFlow::Continue(())
            });
            // alloc: the memo's libraries, exact; nothing for an extern block without `#[link]`
            libs.drain(..).collect()
        })
    }

    /// The module a wasm module imports the functions of an `extern` block from,
    /// `#[link(wasm_import_module = "...")]`: they are its imports, not a library's.
    #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
    pub fn wasm_import_module(db: &Db, owner: ExternBlockId) -> Option<Symbol> {
        collect_attrs(db, AttrDefId::ExternBlockId(owner), |attr| {
            if let ast::Meta::TokenTreeMeta(attr) = &attr
                && path_is(attr.path(), "link")
                && let Some(tt) = attr.token_tree()
                && let Some(module) = link_symbol(tt, "wasm_import_module")
            {
                return ControlFlow::Break(module);
            }
            ControlFlow::Continue(())
        })
    }

    /// The section `#[link_section]` puts an item in.
    #[inline]
    pub fn link_section(db: &Db, owner: AttrDefId) -> Option<Symbol> {
        if !AttrFlags::query(db, owner).contains(AttrFlags::HAS_LINK_SECTION) {
            return None;
        }
        return link_section(db, owner);

        #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
        fn link_section(db: &Db, owner: AttrDefId) -> Option<Symbol> {
            symbol_attr(db, owner, "link_section")
        }
    }

    /// The linkage `#[linkage]` asks of an item, as `extern_weak`.
    #[inline]
    pub fn linkage(db: &Db, owner: AttrDefId) -> Option<Symbol> {
        if !AttrFlags::query(db, owner).contains(AttrFlags::HAS_LINKAGE) {
            return None;
        }
        return linkage(db, owner);

        #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
        fn linkage(db: &Db, owner: AttrDefId) -> Option<Symbol> {
            symbol_attr(db, owner, "linkage")
        }
    }

    /// The symbol `#[export_name]` gives an item.
    #[inline]
    pub fn export_name(db: &Db, owner: AttrDefId) -> Option<Symbol> {
        if !AttrFlags::query(db, owner).contains(AttrFlags::HAS_EXPORT_NAME) {
            return None;
        }
        return export_name(db, owner);

        #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
        fn export_name(db: &Db, owner: AttrDefId) -> Option<Symbol> {
            symbol_attr(db, owner, "export_name")
        }
    }

    /// Whether `owner` has the attribute `#[name]`, once its `cfg_attr`s are expanded. The words
    /// the flags record are one bit test; any other name walks the attributes.
    pub fn has_word(db: &Db, owner: AttrDefId, name: &str) -> bool {
        let flag = match word(name) {
            Some(Word::rustc_nounwind) => AttrFlags::RUSTC_NOUNWIND,
            Some(Word::rustc_comptime) => AttrFlags::RUSTC_COMPTIME,
            Some(Word::panic_handler) => AttrFlags::PANIC_HANDLER,
            Some(Word::rustc_canonical_symbol) => AttrFlags::RUSTC_CANONICAL_SYMBOL,
            Some(Word::marker) => AttrFlags::MARKER,
            Some(Word::automatically_derived) => AttrFlags::AUTOMATICALLY_DERIVED,
            Some(Word::rustc_main) => AttrFlags::RUSTC_MAIN,
            Some(Word::default) => AttrFlags::DEFAULT,
            Some(Word::naked) => AttrFlags::NAKED,
            Some(Word::rustc_deny_explicit_impl) => AttrFlags::RUSTC_DENY_EXPLICIT_IMPL,
            Some(Word::rustc_no_mir_inline) => AttrFlags::RUSTC_NO_MIR_INLINE,
            Some(Word::cold) => AttrFlags::COLD,
            _ => return Self::has_word_in_source(db, owner, name),
        };
        AttrFlags::query(db, owner).contains(flag)
    }

    /// [`AttrFlags::has_word`] read from the source.
    fn has_word_in_source(db: &Db, owner: AttrDefId, name: &str) -> bool {
        let found = collect_attrs(db, owner, |attr| match attr {
            ast::Meta::PathMeta(attr) if path_is(attr.path(), name) => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        });
        found.is_some()
    }

    #[inline]
    pub fn lang_item_with_attrs(self, db: &Db, owner: AttrDefId) -> Option<Symbol> {
        if !self.contains(AttrFlags::LANG_ITEM) {
            // Don't create the query in case this is not a lang item, this wastes memory.
            return None;
        }

        return lang_item(db, owner);

        #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
        fn lang_item(db: &Db, owner: AttrDefId) -> Option<Symbol> {
            collect_attrs(db, owner, |attr| {
                if let ast::Meta::KeyValueMeta(attr) = attr
                    && path_is(attr.path(), "lang")
                    && let Some(value) = string_value(attr)
                {
                    ControlFlow::Break(Symbol::intern(&value))
                } else {
                    ControlFlow::Continue(())
                }
            })
        }
    }

    #[inline]
    pub fn repr(db: &Db, owner: AdtId) -> Option<ReprOptions> {
        if !AttrFlags::query(db, owner.into()).contains(AttrFlags::HAS_REPR) {
            // Don't create the query in case this has no repr, this wastes memory.
            return None;
        }

        Self::repr_assume_has(db, owner)
    }

    /// Only call this when you've verified the type indeed has a `#[repr]` attribute!
    ///
    /// Prefer [`AttrFlags::repr()`] in non-perf-sensitive places as it also has a check that
    /// that the ADT has repr.
    #[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
    pub fn repr_assume_has(db: &Db, owner: AdtId) -> Option<ReprOptions> {
        let mut result = None;
        collect_attrs::<Infallible>(db, owner.into(), |attr| {
            let mut current = None;
            if let ast::Meta::TokenTreeMeta(attr) = &attr
                && let Some(path) = attr.path()
                && let Some(tt) = attr.token_tree()
            {
                if is_one(path, "repr")
                    && let Some(repr) = parse_repr_tt(tt)
                {
                    current = Some(repr);
                } else if is_one(path, "rustc_scalable_vector")
                    && let mut tt = TokenTreeChildren::new(tt)
                    && let Some(NodeOrToken::Token(scalable)) = tt.next()
                    && let Some(scalable) = ast::IntNumber::cast(scalable)
                    && let Ok(scalable) = scalable.value()
                    && let Ok(scalable) = scalable.try_into()
                {
                    current = Some(ReprOptions {
                        scalable: Some(rustc_abi::ScalableElt::ElementCount(scalable)),
                        ..ReprOptions::default()
                    });
                }
            } else if let ast::Meta::PathMeta(attr) = &attr
                && path_is(attr.path(), "rustc_scalable_vector")
            {
                current = Some(ReprOptions {
                    scalable: Some(rustc_abi::ScalableElt::Container),
                    ..ReprOptions::default()
                });
            }

            if let Some(current) = current {
                match &mut result {
                    Some(existing) => merge_repr(existing, current),
                    None => result = Some(current),
                }
            }
            ControlFlow::Continue(())
        });
        result
    }

    /// Call this only if there are legacy const generics, to save memory.
    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    pub(crate) fn legacy_const_generic_indices(
        db: &Db,
        owner: FunctionId,
    ) -> Option<Box<[u32]>> {
        blaze_mem::frame::current(|frame| {
            let result = collect_attrs(db, owner.into(), |attr| {
                if let ast::Meta::TokenTreeMeta(attr) = attr
                    && path_is(attr.path(), "rustc_legacy_const_generics")
                    && let Some(tt) = attr.token_tree()
                {
                    let result = parse_rustc_legacy_const_generics(tt, frame);
                    ControlFlow::Break(result)
                } else {
                    ControlFlow::Continue(())
                }
            });
            result.filter(|it| !it.is_empty())
        })
    }

    // There aren't typically many crates, so it's okay to always make this a query without a flag.
    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    pub fn doc_html_root_url(db: &Db, krate: Crate) -> Option<SmolStr> {
        let root_file_id = krate.root_file_id(db);
        let syntax = root_file_id.parse(db).view();
        let crate_attrs = cli_crate_attrs(db, krate).map(|attr| attr.value);

        let mut cfg_options = None;
        expand_cfg_attr_view(
            syntax.attrs().chain(crate_attrs),
            || cfg_options.get_or_insert(krate.cfg_options(db)),
            |attr, _| {
                if let ast::Meta::TokenTreeMeta(attr) = attr
                    && path_is(attr.path(), "doc")
                    && let Some(tt) = attr.token_tree()
                    && let Some(result) = DocAtom::parse(tt).into_iter().find_map(|atom| {
                        if let DocAtom::KeyValue { key, value } = atom
                            && key == "html_root_url"
                        {
                            Some(SmolStr::new(&*value))
                        } else {
                            None
                        }
                    })
                {
                    ControlFlow::Break(result)
                } else {
                    ControlFlow::Continue(())
                }
            },
        )
    }

    #[inline]
    pub fn target_features(db: &Db, owner: FunctionId) -> &HashSet<Symbol> {
        if !AttrFlags::query(db, owner.into()).contains(AttrFlags::HAS_TARGET_FEATURE) {
            return const { &HashSet::with_hasher(blaze_hash::FixedState) };
        }

        return target_features(db, owner);

        #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
        fn target_features(db: &Db, owner: FunctionId) -> HashSet<Symbol> {
            blaze_mem::frame::current(|frame| {
                let mut found = Vec::new_in(frame);
                collect_attrs::<Infallible>(db, owner.into(), |attr| {
                    if let ast::Meta::TokenTreeMeta(attr) = attr
                        && path_is(attr.path(), "target_feature")
                        && let Some(tt) = attr.token_tree()
                    {
                        let mut tt = TokenTreeChildren::new(tt);
                        while let Some(NodeOrToken::Token(enable_ident)) = tt.next()
                            && enable_ident.text() == "enable"
                            && let Some(NodeOrToken::Token(eq_token)) = tt.next()
                            && eq_token.kind() == T![=]
                            && let Some(NodeOrToken::Token(features)) = tt.next()
                            && let Some(features) = ast::String::cast(features)
                            && let Ok(features) = features.value()
                        {
                            found.extend(features.split(',').map(Symbol::intern));
                            if tt
                                .next()
                                .and_then(NodeOrToken::into_token)
                                .is_none_or(|it| it.kind() != T![,])
                            {
                                break;
                            }
                        }
                    }
                    ControlFlow::Continue(())
                });
                // alloc: the memo's set, once: room for every feature named (a feature named
                // twice is the one case with room to spare)
                let mut set =
                    HashSet::with_capacity_and_hasher(found.len(), Default::default());
                set.extend(found.drain(..));
                set
            })
        }
    }

    /// The features `#[target_feature(enable = ..)]` names, in the order the attributes name
    /// them, each as often as it is named: rustc's `CodegenFnAttrs::target_features` before
    /// it adds what each implies, which its MIR inliner compares in order.
    pub fn target_feature_list(db: &Db, owner: FunctionId) -> Vec<Symbol> {
        if !AttrFlags::query(db, owner.into()).contains(AttrFlags::HAS_TARGET_FEATURE) {
            // alloc: an empty `Vec`, no allocation
            return Vec::new();
        }
        // alloc: only a function with `#[target_feature]`; the caller owns the list
        let mut result = Vec::new();
        collect_attrs::<Infallible>(db, owner.into(), |attr| {
            if let ast::Meta::TokenTreeMeta(attr) = attr
                && path_is(attr.path(), "target_feature")
                && let Some(tt) = attr.token_tree()
            {
                let mut tt = TokenTreeChildren::new(tt);
                while let Some(NodeOrToken::Token(enable_ident)) = tt.next()
                    && enable_ident.text() == "enable"
                    && let Some(NodeOrToken::Token(eq_token)) = tt.next()
                    && eq_token.kind() == T![=]
                    && let Some(NodeOrToken::Token(features)) = tt.next()
                    && let Some(features) = ast::String::cast(features)
                    && let Ok(features) = features.value()
                {
                    result.extend(features.split(',').map(Symbol::intern));
                    if tt
                        .next()
                        .and_then(NodeOrToken::into_token)
                        .is_none_or(|it| it.kind() != T![,])
                    {
                        break;
                    }
                }
            }
            ControlFlow::Continue(())
        });
        result
    }

    /// Whether the function is `#[rustc_nounwind]`, which rustc's `fn_can_unwind` takes as
    /// never unwinding.
    #[inline]
    pub fn is_rustc_nounwind(db: &Db, owner: FunctionId) -> bool {
        AttrFlags::query(db, owner.into()).contains(AttrFlags::RUSTC_NOUNWIND)
    }

    #[inline]
    pub fn rustc_layout_scalar_valid_range(
        db: &Db,
        owner: AdtId,
    ) -> RustcLayoutScalarValidRange {
        if !AttrFlags::query(db, owner.into()).contains(AttrFlags::RUSTC_LAYOUT_SCALAR_VALID_RANGE)
        {
            return RustcLayoutScalarValidRange::default();
        }

        return rustc_layout_scalar_valid_range(db, owner);

        #[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
        fn rustc_layout_scalar_valid_range(
            db: &Db,
            owner: AdtId,
        ) -> RustcLayoutScalarValidRange {
            let mut result = RustcLayoutScalarValidRange::default();
            collect_attrs::<Infallible>(db, owner.into(), |attr| {
                if let ast::Meta::TokenTreeMeta(attr) = attr
                    && let Some(path) = attr.path()
                    && (is_one(path, "rustc_layout_scalar_valid_range_start")
                        || is_one(path, "rustc_layout_scalar_valid_range_end"))
                    && let Some(tt) = attr.token_tree()
                    && let tt = TokenTreeChildren::new(tt)
                    && let Ok(NodeOrToken::Token(value)) = Itertools::exactly_one(tt)
                    && let Some(value) = ast::IntNumber::cast(value)
                    && let Ok(value) = value.value()
                {
                    if is_one(path, "rustc_layout_scalar_valid_range_start") {
                        result.start = Some(value)
                    } else {
                        result.end = Some(value);
                    }
                }
                ControlFlow::Continue(())
            });
            result
        }
    }

    #[inline]
    pub fn doc_aliases(
        self,
        db: &Db,
        owner: Either<AttrDefId, FieldId>,
    ) -> &[Symbol] {
        if !self.contains(AttrFlags::HAS_DOC_ALIASES) {
            return &[];
        }
        return match owner {
            Either::Left(it) => doc_aliases(db, it),
            Either::Right(field) => fields_doc_aliases(db, field.parent)
                .get(field.local_id)
                .map(|it| &**it)
                .unwrap_or_default(),
        };

        #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
        fn doc_aliases(db: &Db, owner: AttrDefId) -> Box<[Symbol]> {
            blaze_mem::frame::current(|frame| {
                let mut result = Vec::new_in(frame);
                collect_attrs::<Infallible>(db, owner, |attr| {
                    extract_doc_aliases(&mut result, attr)
                });
                // alloc: the memo's value, exact, only for an item whose flag says `#[doc(alias)]`
                result.drain(..).collect()
            })
        }

        #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
        fn fields_doc_aliases(
            db: &Db,
            variant: VariantId,
        ) -> ArenaMap<LocalFieldId, Box<[Symbol]>> {
            blaze_mem::frame::current(|frame| {
                collect_field_attrs(db, variant, Tree::Outline, frame, |cfg_options, field| {
                    let mut result = Vec::new_in(frame);
                    expand_cfg_attr_view(
                        field.value.attrs(),
                        || cfg_options,
                        |attr, _| extract_doc_aliases(&mut result, attr),
                    );
                    // alloc: the field's aliases, exact; nothing for a field with none
                    result.drain(..).collect::<Box<[Symbol]>>()
                })
            })
        }
    }

    #[inline]
    pub fn cfgs(
        self,
        db: &Db,
        owner: Either<AttrDefId, FieldId>,
    ) -> Option<&CfgExpr> {
        if !self.contains(AttrFlags::HAS_CFG) {
            return None;
        }
        return match owner {
            Either::Left(it) => cfgs(db, it).as_ref(),
            Either::Right(field) => {
                fields_cfgs(db, field.parent).get(field.local_id).and_then(|it| it.as_ref())
            }
        };

        // We LRU this query because it is only used by IDE.
        #[blaze_query::tracked(returns(ref), lru = 250, mem_tag = Nameres)]
        fn cfgs(db: &Db, owner: AttrDefId) -> Option<CfgExpr> {
            blaze_mem::frame::current(|frame| {
                let mut result = Vec::new_in(frame);
                collect_attrs::<Infallible>(db, owner, |attr| extract_cfgs(&mut result, attr));
                match result.len() {
                    0 => None,
                    1 => result.into_iter().next(),
                    // alloc: the memo's value, exact, only for an item whose flag says `#[cfg]`
                    _ => Some(CfgExpr::All(result.drain(..).collect())),
                }
            })
        }

        // We LRU this query because it is only used by IDE.
        #[blaze_query::tracked(returns(ref), lru = 50, mem_tag = Nameres)]
        fn fields_cfgs(
            db: &Db,
            variant: VariantId,
        ) -> ArenaMap<LocalFieldId, Option<CfgExpr>> {
            blaze_mem::frame::current(|frame| {
                collect_field_attrs(db, variant, Tree::Outline, frame, |cfg_options, field| {
                    let mut result = Vec::new_in(frame);
                    expand_cfg_attr_view(
                        field.value.attrs(),
                        || cfg_options,
                        |attr, _| extract_cfgs(&mut result, attr),
                    );
                    match result.len() {
                        0 => None,
                        1 => result.into_iter().next(),
                        // alloc: the field's cfgs, exact
                        _ => Some(CfgExpr::All(result.drain(..).collect())),
                    }
                })
            })
        }
    }

    #[inline]
    pub fn doc_keyword(db: &Db, owner: ModuleId) -> Option<Symbol> {
        if !AttrFlags::query(db, AttrDefId::ModuleId(owner)).contains(AttrFlags::HAS_DOC_KEYWORD) {
            return None;
        }
        return doc_keyword(db, owner);

        #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
        fn doc_keyword(db: &Db, owner: ModuleId) -> Option<Symbol> {
            collect_attrs(db, AttrDefId::ModuleId(owner), |attr| {
                if let ast::Meta::TokenTreeMeta(attr) = attr
                    && path_is(attr.path(), "doc")
                    && let Some(tt) = attr.token_tree()
                {
                    for atom in DocAtom::parse(tt) {
                        if let DocAtom::KeyValue { key, value } = atom
                            && key == "keyword"
                        {
                            return ControlFlow::Break(Symbol::intern(&value));
                        }
                    }
                }
                ControlFlow::Continue(())
            })
        }
    }

    // We LRU this query because it is only used by IDE.
    #[blaze_query::tracked(returns(as_deref), lru = 250, mem_tag = Nameres)]
    pub fn docs(db: &Db, owner: AttrDefId) -> Option<Box<Docs>> {
        let (source, outer_mod_decl, _crate_attrs, krate) = attrs_source(db, owner, Tree::Live);
        let inner_attrs_node = docs::inner_docs_node(source.value);
        // Note: we don't have to pass down `_crate_attrs` here, since `extract_docs`
        // does not handle crate-level attributes related to docs.
        // See: https://doc.rust-lang.org/rustdoc/write-documentation/the-doc-attribute.html#at-the-crate-level
        self::docs::extract_docs(
            db,
            krate,
            &|| resolver_for_attr_def_id(db, owner),
            &|| krate.cfg_options(db),
            source,
            outer_mod_decl,
            inner_attrs_node,
        )
    }

    #[inline]
    pub fn field_docs(db: &Db, field: FieldId) -> Option<&Docs> {
        Self::fields_docs(db, field.parent).get(field.local_id).and_then(|it| it.as_deref())
    }

    // We LRU this query because it is only used by IDE.
    #[blaze_query::tracked(returns(ref), lru = 50, mem_tag = Nameres)]
    pub fn fields_docs(
        db: &Db,
        variant: VariantId,
    ) -> ArenaMap<LocalFieldId, Option<Box<Docs>>> {
        let krate = variant.module(db).krate(db);
        blaze_mem::frame::current(|frame| {
            collect_field_attrs(db, variant, Tree::Live, frame, |cfg_options, field| {
                self::docs::extract_docs(
                    db,
                    krate,
                    &|| variant.resolver(db),
                    &|| cfg_options,
                    field,
                    None,
                    None,
                )
            })
        })
    }

    #[inline]
    pub fn derive_info(db: &Db, owner: MacroId) -> Option<&DeriveInfo> {
        if !AttrFlags::query(db, owner.into()).contains(AttrFlags::IS_DERIVE_OR_BUILTIN_MACRO) {
            return None;
        }

        return derive_info(db, owner).as_ref();

        #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
        fn derive_info(db: &Db, owner: MacroId) -> Option<DeriveInfo> {
            blaze_mem::frame::current(|frame| {
                collect_attrs(db, owner.into(), |attr| {
                    if let ast::Meta::TokenTreeMeta(attr) = attr
                        && (path_is(attr.path(), "proc_macro_derive")
                            || path_is(attr.path(), "rustc_builtin_macro"))
                        && let Some(tt) = attr.token_tree()
                        && let mut tt = TokenTreeChildren::new(tt)
                        && let Some(NodeOrToken::Token(trait_name)) = tt.next()
                        && trait_name.kind().is_any_identifier()
                    {
                        let trait_name = Symbol::intern(trait_name.text());

                        let helpers = if let Some(NodeOrToken::Token(comma)) = tt.next()
                            && comma.kind() == T![,]
                            && let Some(NodeOrToken::Token(attributes)) = tt.next()
                            && attributes.text() == "attributes"
                            && let Some(NodeOrToken::Node(attributes)) = tt.next()
                        {
                            let mut helpers = Vec::new_in(frame);
                            helpers.extend(
                                attributes
                                    .syntax()
                                    .children_with_tokens()
                                    .filter_map(NodeOrToken::into_token)
                                    .filter(|it| it.kind().is_any_identifier())
                                    .map(|it| Symbol::intern(it.text())),
                            );
                            // alloc: the memo's helpers, exact
                            helpers.drain(..).collect::<Box<[_]>>()
                        } else {
                            Box::default()
                        };

                        ControlFlow::Break(DeriveInfo { trait_name, helpers })
                    } else {
                        ControlFlow::Continue(())
                    }
                })
            })
        }
    }

    pub fn unstable_feature(self, db: &Db, owner: AttrDefId) -> Option<Symbol> {
        if !self.contains(AttrFlags::IS_UNSTABLE) {
            return None;
        }

        return unstable_feature(db, owner);

        #[blaze_query::tracked(returns(clone), mem_tag = Nameres)]
        fn unstable_feature(db: &Db, owner: AttrDefId) -> Option<Symbol> {
            collect_attrs(db, owner, |attr| {
                if let ast::Meta::TokenTreeMeta(attr) = attr
                    && path_is(attr.path(), "unstable")
                    && let Some(tt) = attr.token_tree()
                {
                    let mut tt = TokenTreeChildren::new(tt);
                    // Technically the `feature = "..."` always comes first, but it's not a requirement.
                    while let Some(token) = tt.next() {
                        if let NodeOrToken::Token(token) = token
                            && token.text() == "feature"
                            && let Some(NodeOrToken::Token(eq)) = tt.next()
                            && eq.kind() == T![=]
                            && let Some(NodeOrToken::Token(feature)) = tt.next()
                            && let Some(feature) = ast::String::cast(feature)
                            && let Ok(feature) = feature.value()
                        {
                            return ControlFlow::Break(Symbol::intern(&feature));
                        }
                    }
                }
                ControlFlow::Continue(())
            })
        }
    }

    /// Returns `None` if there is no `#[must_use]`, `Some(None)` if there is a `#[must_use]` without a message,
    /// and `Some(Some(message))` if there is a `#[must_use]` with a message.
    pub fn must_use_message(db: &Db, owner: AttrDefId) -> Option<Option<&str>> {
        if !AttrFlags::query(db, owner).contains(AttrFlags::IS_MUST_USE) {
            return None;
        }
        return Some(must_use_message(db, owner));

        #[blaze_query::tracked(returns(as_deref), mem_tag = Nameres)]
        fn must_use_message(db: &Db, owner: AttrDefId) -> Option<Box<str>> {
            collect_attrs(db, owner, |attr| {
                if let ast::Meta::KeyValueMeta(attr) = attr
                    && path_is(attr.path(), "must_use")
                    && let Some(message) = string_value(attr)
                {
                    return ControlFlow::Break(Box::from(&*message));
                }
                ControlFlow::Continue(())
            })
        }
    }

    /// The type aliases a function names in `#[define_opaque(..)]`, which has expanded to
    /// `#[rust_analyzer::define_opaque(..)]` by now.
    pub fn define_opaque(db: &Db, owner: FunctionId) -> &[ModPath] {
        if !AttrFlags::query(db, owner.into()).contains(AttrFlags::HAS_DEFINE_OPAQUE) {
            return &[];
        }
        return define_opaque(db, owner);

        #[blaze_query::tracked(returns(deref), mem_tag = Nameres)]
        fn define_opaque(db: &Db, owner: FunctionId) -> Box<[ModPath]> {
            blaze_mem::frame::current(|frame| {
                let mut result = Vec::new_in(frame);
                let mut tokens = Vec::new_in(frame);
                let mut segments = Vec::new_in(frame);
                collect_attrs::<Infallible>(db, owner.into(), |attr| {
                    if let ast::Meta::TokenTreeMeta(attr) = attr
                        && attr.path().and_then(up_to_two_segments).is_some_and(|(_, tool)| {
                            tool.is_some_and(|it| name_text(it) == "define_opaque")
                        })
                        && let Some(tt) = attr.token_tree()
                    {
                        tokens.clear();
                        tokens.extend(
                            TokenTreeChildren::new(tt).filter_map(NodeOrToken::into_token),
                        );
                        for path in tokens.split(|it| it.kind() == T![,]) {
                            let mut kind = PathKind::Plain;
                            segments.clear();
                            for token in path {
                                match token.kind() {
                                    T![crate] => kind = PathKind::Crate,
                                    T![self] => kind = PathKind::SELF,
                                    T![super] => {
                                        kind = match kind {
                                            PathKind::Super(n) => PathKind::Super(n + 1),
                                            _ => PathKind::Super(1),
                                        }
                                    }
                                    SyntaxKind::IDENT => segments
                                        .push(Name::new_symbol_root(Symbol::intern(token.text()))),
                                    _ => {}
                                }
                            }
                            if !segments.is_empty() {
                                result.push(ModPath::from_segments(kind, segments.drain(..)));
                            }
                        }
                    }
                    ControlFlow::Continue(())
                });
                // alloc: the memo's value, exact, only for a function with `#[define_opaque]`
                result.drain(..).collect()
            })
        }
    }

    pub fn must_implement_one_of(db: &Db, owner: TraitId) -> Option<&[Name]> {
        if !AttrFlags::query(db, owner.into()).contains(AttrFlags::HAS_RUSTC_MUST_IMPLEMENT_ONE_OF)
        {
            return None;
        }
        return must_implement_one_of(db, owner);

        #[blaze_query::tracked(returns(as_deref), mem_tag = Nameres)]
        pub fn must_implement_one_of(
            db: &Db,
            owner: TraitId,
        ) -> Option<Box<[Name]>> {
            blaze_mem::frame::current(|frame| {
                let mut names = Vec::new_in(frame);
                collect_attrs(db, owner.into(), |attr| {
                    if let ast::Meta::TokenTreeMeta(attr) = attr
                        && path_is(attr.path(), "rustc_must_implement_one_of")
                        && let Some(tt) = attr.token_tree()
                    {
                        names.clear();
                        names.extend(
                            tt.token_trees_and_tokens()
                                .filter_map(|it| ast::Ident::cast(it.into_token()?))
                                .map(|name| Name::new_symbol_root(Symbol::intern(name.text()))),
                        );
                        if !names.is_empty() {
                            // alloc: the memo's names, exact
                            return ControlFlow::Break(names.drain(..).collect::<Box<[_]>>());
                        }
                    }
                    ControlFlow::Continue(())
                })
            })
        }
    }
}

/// The bits of [`disabled_sanitizers`]: the sanitizers blaze instruments, numbered as
/// `blaze_codegen::settings::Sanitizers` numbers them. rustc's other keys (`cfi`, `kcfi`,
/// `hwaddress`, `memtag`, `shadow_call_stack`, `safestack`, `realtime`) are read and left out.
pub mod sanitizer {
    /// `address`, and `kernel_address`, which rustc turns on and off with it.
    pub const ADDRESS: u8 = 1 << 0;
    pub const THREAD: u8 = 1 << 1;
    pub const MEMORY: u8 = 1 << 2;
}

/// The sanitizers `#[sanitize(address = "off", ..)]` turns off for `def` ([`sanitizer`] bits):
/// rustc's `sanitizer_settings_for`. It starts from its parent's set (the crate root's parent
/// has none) and each `#[sanitize]` on the way down clears what it turns on, then sets what it
/// turns off. The parents are rustc's: the impl or trait, the module, and for an item in a
/// body the items and closures that body is in.
pub fn disabled_sanitizers(db: &Db, def: FunctionId) -> u8 {
    let inherited = match def.lookup(db).container {
        ItemContainerId::ModuleId(it) => module_disabled_sanitizers(db, it),
        ItemContainerId::ImplId(it) => {
            let module = module_disabled_sanitizers(db, it.lookup(db).container);
            own_disabled_sanitizers(db, it.into(), module)
        }
        ItemContainerId::TraitId(it) => {
            let module = module_disabled_sanitizers(db, it.lookup(db).container);
            own_disabled_sanitizers(db, it.into(), module)
        }
        ItemContainerId::ExternBlockId(it) => {
            let module = module_disabled_sanitizers(db, it.lookup(db).container);
            own_disabled_sanitizers(db, AttrDefId::ExternBlockId(it), module)
        }
    };
    own_disabled_sanitizers(db, def.into(), inherited)
}

/// [`disabled_sanitizers`] of the items of `module`.
#[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
fn module_disabled_sanitizers(db: &Db, module: ModuleId) -> u8 {
    let inherited = match module.containing_module_inside_def_map(db) {
        Some(parent) => module_disabled_sanitizers(db, ModuleId::from_id(parent.as_id())),
        None => match module.block(db) {
            // A block is no item of rustc's: its items' parent is what the block is in.
            Some(block) => return block_disabled_sanitizers(db, BlockId::from_id(block.as_id())),
            None => 0,
        },
    };
    own_disabled_sanitizers(db, module.into(), inherited)
}

/// [`disabled_sanitizers`] of the items of `block`: the set of the module holding it, then the
/// `#[sanitize]` of each item and closure between the two, outermost first, through the macro
/// calls the block was expanded from.
#[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
fn block_disabled_sanitizers(db: &Db, block: BlockId) -> u8 {
    let outer = block.module(db);
    let krate = outer.krate(db);
    // The block the module holding `block` is, if it is a block's: the walk stops there, as it
    // does at a `mod` or a file, which bound every other module.
    let outer_block = match (outer.block(db), outer.containing_module_inside_def_map(db)) {
        (Some(it), None) => Some(it.ast_id(db)),
        _ => None,
    };
    let ast_id = block.ast_id(db);
    let mut file_id = ast_id.file_id;
    // A block of a body written in a file is walked in that body's syntax alone, as
    // `block_item_tree_query` lowers it: every ancestor of its anchor up to the module holding
    // it has no attributes (`real_span_map`), so the walk finds the same `#[sanitize]` there.
    let syntax = block
        .owner(db)
        .and_then(|owner| crate::expr_store::body::body_syntax(db, owner).as_ref())
        .filter(|syntax| syntax.file_id == ast_id.file_id);
    let tree = syntax.map(|it| it.tree(db));
    let (mut node, stop) = match (syntax, &tree) {
        (Some(syntax), Some(tree)) => {
            let ptr = syntax.ast_id_map.try_get_erased(ast_id.value.erase()).unwrap_or_else(|| {
                panic!("{block:?}: block {:?} is not in its body's syntax, which made it", ast_id.value)
            });
            let stop = outer_block.filter(|it| it.file_id == file_id).map(|it| {
                let ptr = syntax.ast_id_map.try_get_erased(it.value.erase()).unwrap_or_else(|| {
                    panic!("{block:?}: the block {:?} holding it is not in its body's syntax", it.value)
                });
                (file_id, ptr.text_range())
            });
            (ast::to_node(ptr, ast::Node::root(tree)).parent(), stop)
        }
        _ => (
            ast_id.to_view(db).syntax().parent(),
            outer_block.map(|it| (it.file_id, it.to_range(db))),
        ),
    };
    let mut cfg_options = None;
    let mut sets = SmallVec::<[SanitizeSets; 4]>::new();
    'walk: loop {
        while let Some(it) = node {
            let kind = it.kind();
            if kind == SyntaxKind::BLOCK_EXPR
                && stop.is_some_and(|(file, range)| file == file_id && range == it.text_range())
            {
                break 'walk;
            }
            node = it.parent();
            match kind {
                SyntaxKind::MODULE | SyntaxKind::SOURCE_FILE => break 'walk,
                SyntaxKind::FN
                | SyntaxKind::CLOSURE_EXPR
                | SyntaxKind::IMPL
                | SyntaxKind::TRAIT
                | SyntaxKind::CONST
                | SyntaxKind::STATIC => {
                    if let Some(owner) = ast::AnyHasAttrs::cast(it) {
                        sets.extend(expand_cfg_attr_view(
                            ast::attrs_including_inner(&owner),
                            || cfg_options.get_or_insert_with(|| krate.cfg_options(db)),
                            |meta, _| sanitize_sets(meta),
                        ));
                    }
                }
                _ => {}
            }
        }
        let Some(call) = file_id.call_node_view(db) else { break };
        file_id = call.file_id;
        node = call.value.parent();
    }
    let module = module_disabled_sanitizers(db, ModuleId::from_id(outer.as_id()));
    sets.iter().rev().fold(module, |disabled, sets| sets.apply(disabled))
}

/// What `owner`'s own `#[sanitize]` leaves of `inherited`.
fn own_disabled_sanitizers(db: &Db, owner: AttrDefId, inherited: u8) -> u8 {
    if !AttrFlags::query(db, owner).contains(AttrFlags::HAS_SANITIZE) {
        return inherited;
    }
    collect_attrs(db, owner, sanitize_sets).map_or(inherited, |sets| sets.apply(inherited))
}

/// One `#[sanitize(..)]`: the sanitizers it turns on and those it turns off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SanitizeSets {
    on: u8,
    off: u8,
}

impl SanitizeSets {
    /// rustc's order: what it turns on is cleared first, so a sanitizer named both ways is off.
    #[inline]
    fn apply(self, inherited: u8) -> u8 {
        (inherited & !self.on) | self.off
    }
}

/// The sets of `attr` if it is `#[sanitize(..)]`, which ends the walk: rustc reads an item's
/// first (a second is an error). Its arguments are `key = "on" | "off"`, comma separated.
fn sanitize_sets(attr: ast::Meta<'_>) -> ControlFlow<SanitizeSets> {
    let ast::Meta::TokenTreeMeta(attr) = attr else { return ControlFlow::Continue(()) };
    if !path_is(attr.path(), "sanitize") {
        return ControlFlow::Continue(());
    }
    let Some(tt) = attr.token_tree() else { return ControlFlow::Continue(()) };
    let mut sets = SanitizeSets::default();
    let mut tt = TokenTreeChildren::new(tt);
    while let Some(NodeOrToken::Token(key)) = tt.next()
        && let Some(NodeOrToken::Token(eq_token)) = tt.next()
        && eq_token.kind() == T![=]
        && let Some(NodeOrToken::Token(value)) = tt.next()
        && let Some(value) = ast::String::cast(value)
        && let Ok(value) = value.value()
    {
        let key = key.text();
        let set = match key.strip_prefix("r#").unwrap_or(key) {
            "address" | "kernel_address" => sanitizer::ADDRESS,
            "thread" => sanitizer::THREAD,
            "memory" => sanitizer::MEMORY,
            _ => 0,
        };
        match &*value {
            "on" => sets.on |= set,
            "off" => sets.off |= set,
            _ => {}
        }
        if tt.next().and_then(NodeOrToken::into_token).is_none_or(|it| it.kind() != T![,]) {
            break;
        }
    }
    ControlFlow::Break(sets)
}

fn merge_repr(this: &mut ReprOptions, other: ReprOptions) {
    let ReprOptions { int, align, pack, flags, scalable, field_shuffle_seed: _ } = this;
    flags.insert(other.flags);
    *align = (*align).max(other.align);
    *pack = match (*pack, other.pack) {
        (Some(pack), None) | (None, Some(pack)) => Some(pack),
        _ => (*pack).min(other.pack),
    };
    if other.int.is_some() {
        *int = other.int;
    }
    if other.scalable.is_some() {
        *scalable = other.scalable;
    }
}

fn parse_repr_tt(tt: ast::TokenTree<'_>) -> Option<ReprOptions> {
    use rustc_abi::{Align, Integer, IntegerType, ReprFlags, ReprOptions};

    let mut tts = TokenTreeChildren::new(tt).peekable();

    let int = |int: Integer, signed: bool| ReprOptions {
        int: Some(IntegerType::Fixed(int, signed)),
        ..Default::default()
    };
    let mut acc = ReprOptions::default();
    while let Some(tt) = tts.next() {
        let NodeOrToken::Token(ident) = tt else {
            continue;
        };
        if !ident.kind().is_any_identifier() {
            continue;
        }
        let repr = match ReprWord::of(ident.text().trim_start_matches("r#")) {
            Some(ReprWord::packed) => {
                let pack = if let Some(NodeOrToken::Node(tt)) =
                    tts.next_if(|it| matches!(it, NodeOrToken::Node(_)))
                {
                    let mut tt_iter = TokenTreeChildren::new(tt);
                    if let Some(NodeOrToken::Token(lit)) = tt_iter.next()
                        && let Some(lit) = ast::IntNumber::cast(lit)
                        && let Ok(lit) = lit.value()
                        && let Ok(lit) = lit.try_into()
                    {
                        lit
                    } else {
                        0
                    }
                } else {
                    0
                };
                let pack = Some(Align::from_bytes(pack).unwrap_or(Align::ONE));
                ReprOptions { pack, ..Default::default() }
            }
            Some(ReprWord::align) => {
                let mut align = None;
                if let Some(NodeOrToken::Node(tt)) =
                    tts.next_if(|it| matches!(it, NodeOrToken::Node(_)))
                {
                    let mut tt_iter = TokenTreeChildren::new(tt);
                    if let Some(NodeOrToken::Token(lit)) = tt_iter.next()
                        && let Some(lit) = ast::IntNumber::cast(lit)
                        && let Ok(lit) = lit.value()
                        && let Ok(lit) = lit.try_into()
                    {
                        align = Align::from_bytes(lit).ok();
                    }
                }
                ReprOptions { align, ..Default::default() }
            }
            Some(ReprWord::C) => ReprOptions { flags: ReprFlags::IS_C, ..Default::default() },
            Some(ReprWord::transparent) => {
                ReprOptions { flags: ReprFlags::IS_TRANSPARENT, ..Default::default() }
            }
            Some(ReprWord::simd) => ReprOptions { flags: ReprFlags::IS_SIMD, ..Default::default() },
            Some(ReprWord::isize) => {
                ReprOptions { int: Some(IntegerType::Pointer(true)), ..Default::default() }
            }
            Some(ReprWord::i8) => int(Integer::I8, true),
            Some(ReprWord::i16) => int(Integer::I16, true),
            Some(ReprWord::i32) => int(Integer::I32, true),
            Some(ReprWord::i64) => int(Integer::I64, true),
            Some(ReprWord::i128) => int(Integer::I128, true),
            Some(ReprWord::usize) => {
                ReprOptions { int: Some(IntegerType::Pointer(false)), ..Default::default() }
            }
            Some(ReprWord::u8) => int(Integer::I8, false),
            Some(ReprWord::u16) => int(Integer::I16, false),
            Some(ReprWord::u32) => int(Integer::I32, false),
            Some(ReprWord::u64) => int(Integer::I64, false),
            Some(ReprWord::u128) => int(Integer::I128, false),
            None => ReprOptions::default(),
        };
        merge_repr(&mut acc, repr);
    }

    Some(acc)
}

fn parse_rustc_legacy_const_generics(tt: ast::TokenTree<'_>, frame: &Frame) -> Box<[u32]> {
    let mut found = Vec::<u32, _>::new_in(frame);
    found.extend(TokenTreeChildren::new(tt).filter_map(|param| {
        u32::try_from(ast::IntNumber::cast(param.into_token()?)?.value().ok()?).ok()
    }));
    // alloc: the memo's value, exact, only for a function with `#[rustc_legacy_const_generics]`
    Box::from(&found[..])
}

#[derive(Debug)]
enum DocAtom<'t> {
    /// eg. `#[doc(hidden)]`
    Flag(&'t str),
    /// eg. `#[doc(alias = "it")]`
    ///
    /// Note that a key can have multiple values that are all considered "active" at the same time.
    /// For example, `#[doc(alias = "x")]` and `#[doc(alias = "y")]`.
    KeyValue { key: &'t str, value: Cow<'t, str> },
    /// eg. `#[doc(alias("x", "y"))]`
    Alias(SmallVec<[Cow<'t, str>; 2]>),
}

impl<'t> DocAtom<'t> {
    fn parse(tt: ast::TokenTree<'t>) -> SmallVec<[DocAtom<'t>; 1]> {
        let mut iter = TokenTreeChildren::new(tt).peekable();
        let mut result = SmallVec::new();
        while iter.peek().is_some() {
            if let Some(expr) = next_doc_expr(&mut iter) {
                result.push(expr);
            }
        }
        result
    }
}

fn next_doc_expr<'t>(it: &mut Peekable<TokenTreeChildren<'t>>) -> Option<DocAtom<'t>> {
    let name = match it.next() {
        Some(NodeOrToken::Token(token)) if token.kind().is_any_identifier() => token.text(),
        _ => return None,
    };

    let ret = match it.peek() {
        Some(NodeOrToken::Token(eq)) if eq.kind() == T![=] => {
            it.next();
            if let Some(NodeOrToken::Token(value)) = it.next()
                && let Some(value) = ast::String::cast(value)
                && let Ok(value) = value.value()
            {
                DocAtom::KeyValue { key: name, value }
            } else {
                return None;
            }
        }
        Some(NodeOrToken::Node(subtree)) => {
            if name != "alias" {
                return None;
            }
            let aliases = TokenTreeChildren::new(*subtree)
                .filter_map(|alias| ast::String::cast(alias.into_token()?)?.value().ok())
                // alloc: a `doc(alias(..))` of 3+ names, read only by the doc-alias memo and docs
                .collect();
            it.next();
            DocAtom::Alias(aliases)
        }
        _ => DocAtom::Flag(name),
    };
    Some(ret)
}

#[cfg(test)]
mod tests;
