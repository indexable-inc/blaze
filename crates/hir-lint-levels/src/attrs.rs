//! The lint attributes of a node as rustc reads them: with `cfg_attr` expanded, outer then
//! inner, each list read as `meta_item_list` reads it; which nodes have them; the node whose
//! attributes come next going out from one, through the macro call an expansion is of and the
//! `mod` a file is of.
//!
//! Follows rustc's `compiler/rustc_ast/src/attr/mod.rs` (`meta_item_list`,
//! `MetaItemKind::list_from_tokens`, `is_automatically_derived_attr`, `is_doc_hidden`),
//! `compiler/rustc_expand/src/expand.rs` (`InvocationCollector`: the attributes of a macro
//! call are not the expansion's; a derive's output is beside the item, an attribute macro's in
//! its place), `compiler/rustc_builtin_macros/src/cmdline_attrs.rs` (`-Z crate-attr`'s are the
//! crate's, after its own), `compiler/rustc_hir/src/intravisit.rs` and
//! `compiler/rustc_ast/src/visit.rs` (which nodes have lint attributes) (MIT or Apache-2.0).

use std::{borrow::Cow, ops::ControlFlow};

use base_db::{Crate, Db};
use blaze_lint_defs::{Level, Lint, LintId, LintSet, Store};
use cfg::CfgOptions;
use hir_def::nameres::{ModuleOrigin, crate_def_map};
use hir_expand::{AstId, HirFileId, InFile, MacroCallKind};
use intern::Symbol;
use smallvec::SmallVec;
use span::Edition;
use syntax::{
    NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode, AstToken, HasAttrs as _},
};

/// An attribute of a node as rustc's lint levels read it.
pub enum Read<'t> {
    /// `#[automatically_derived]` (`single_use_lifetimes`) or `#[doc(hidden)]`
    /// (`missing_docs`): a lint allowed with no attribute naming it.
    Allows(LintId),
    /// `#[allow(..)]`, `#[expect(..)]`, `#[warn(..)]`, `#[deny(..)]`, `#[forbid(..)]`, and
    /// whether it is written as itself rather than made by a `cfg_attr`, which rustc's
    /// pre-expansion lint pass sees unexpanded.
    Level(Level, Items<'t>, bool),
}

/// The elements of a list, inline for the usual few.
pub type Items<'t> = SmallVec<[Item<'t>; 4]>;

/// The lint attributes of one node: one or two nearly always.
pub type Reads<'t> = SmallVec<[Read<'t>; 2]>;

/// A path's segments, inline for a lint's (one) and a tool's lint's (two).
pub type Path<'t> = SmallVec<[Segment<'t>; 2]>;

/// An element of a lint attribute's list: rustc's `MetaItemInner`.
pub struct Item<'t> {
    pub range: TextRange,
    pub kind: ItemKind<'t>,
}

pub enum ItemKind<'t> {
    /// A path: a lint, a group or a tool's lint.
    Word(Path<'t>),
    /// `path = literal`: the literal's value when it is a string, and where the literal is.
    NameValue { path: Path<'t>, string: Option<Symbol>, value: TextRange },
    /// `path(..)`.
    List(Path<'t>),
    /// A literal alone.
    Lit,
}

impl ItemKind<'_> {
    /// Whether it is `reason = ..`.
    pub fn is_reason(&self) -> bool {
        matches!(self, ItemKind::NameValue { path, .. } if is_word(path, "reason"))
    }
}

/// A segment of a path, as rustc's `Ident` has it: its token, whose text is read in place, or
/// none for rustc's `{{root}}` of a leading `::`.
#[derive(Clone, Copy)]
pub struct Segment<'t> {
    token: Option<ast::Token<'t>>,
    pub range: TextRange,
}

impl<'t> Segment<'t> {
    pub fn name(&self) -> &'t str {
        self.token.map_or("{{root}}", |it| it.text())
    }
}

/// Whether `path` is the one segment `name`.
fn is_word(path: &[Segment<'_>], name: &str) -> bool {
    matches!(path, [only] if only.name() == name)
}

/// `path` as rustc's `pprust::path_to_string` writes it: the segment's own text for one.
pub fn path_string<'t>(path: &[Segment<'t>]) -> Cow<'t, str> {
    match path {
        [only] => Cow::Borrowed(only.name()),
        _ => {
            // alloc: a tool's lint's name (two segments), read once per name in an attribute
            let mut out = String::with_capacity(path.iter().map(|it| it.name().len() + 2).sum());
            for (at, segment) in path.iter().enumerate() {
                if at > 0 {
                    out.push_str("::");
                }
                out.push_str(segment.name());
            }
            Cow::Owned(out)
        }
    }
}

/// The node whose attributes set lint levels for what is in it, if `node` is one. Not a macro
/// call: its attributes are checked and dropped, not given to its expansion. Not the list an
/// item, a `match` or a function's body has its inner attributes in: they are the item's.
/// `generics`: whether a generic parameter is one, as it is to the early lint pass and not to
/// the HIR.
pub fn owner(node: ast::Node<'_>, generics: bool) -> Option<ast::AnyHasAttrs<'_>> {
    use SyntaxKind::*;
    let kind = node.kind();
    if !may_be_owner(kind, generics) {
        return None;
    }
    let owner = ast::AnyHasAttrs::cast(node)?;
    let inner_of_outer = matches!(kind, STMT_LIST | ITEM_LIST | ASSOC_ITEM_LIST | MATCH_ARM_LIST | EXTERN_ITEM_LIST)
        && node.ancestors().skip(1).take(2).any(|it| {
            ast::AnyHasAttrs::cast(it).is_some_and(|it| it.inner_attributes_node().is_some_and(|inner| inner == node))
        });
    (!inner_of_outer).then_some(owner)
}

/// Whether a node of `kind` may be an [`owner`], by its kind alone.
pub fn may_be_owner(kind: SyntaxKind, generics: bool) -> bool {
    use SyntaxKind::*;
    match kind {
        MACRO_CALL | MACRO_EXPR | MACRO_PAT | MACRO_TYPE | MACRO_STMTS | MACRO_ITEMS => false,
        TYPE_PARAM | CONST_PARAM | LIFETIME_PARAM => generics,
        _ => ast::AnyHasAttrs::can_cast(kind),
    }
}

/// Whether `node`, of a kind [`may_be_owner`] lets through, is an [`owner`] with an attribute.
pub fn owns(node: ast::Node<'_>, generics: bool) -> bool {
    owner(node, generics).is_some_and(|it| attrs_of(it).next().is_some())
}

/// The attributes of `owner`, outer then inner: a block's list has its own, inner ones only.
fn attrs_of(owner: ast::AnyHasAttrs<'_>) -> impl Iterator<Item = ast::Attr<'_>> {
    let (own, with_inner) = match owner.syntax().kind() == SyntaxKind::STMT_LIST {
        true => (Some(owner.attrs()), None),
        false => (None, Some(ast::attrs_including_inner(&owner))),
    };
    own.into_iter().flatten().chain(with_inner.into_iter().flatten())
}

/// The lint attributes of `owner`, `cfg_attr` expanded against `cfg`, outer then inner, in
/// order.
pub fn read<'t>(owner: ast::AnyHasAttrs<'t>, cfg: &CfgOptions, edition: Edition) -> Reads<'t> {
    read_attrs(attrs_of(owner), cfg, edition)
}

/// The lint attributes of the crate attribute file `root` (one `#![..]` of `-Z crate-attr`),
/// as [`read`] reads a node's. None of them is written in the crate's root file, so none is
/// seen by rustc's pre-expansion lint pass as written there.
pub fn read_crate_attr<'t>(root: ast::Node<'t>, cfg: &CfgOptions, edition: Edition) -> Reads<'t> {
    let Some(file) = ast::SourceFile::cast(root) else { return Reads::new() };
    let mut reads = read_attrs(file.attrs(), cfg, edition);
    for read in &mut reads {
        if let Read::Level(_, _, direct) = read {
            *direct = false;
        }
    }
    reads
}

fn read_attrs<'t>(attrs: impl Iterator<Item = ast::Attr<'t>>, cfg: &CfgOptions, edition: Edition) -> Reads<'t> {
    let mut found = Reads::new();
    hir_expand::attrs::expand_cfg_attr_view(attrs, || cfg, |meta, top| {
        let direct = top.meta().is_some_and(|it| it.syntax() == meta.syntax());
        if let Some(read) = read_meta(meta, edition, direct) {
            found.push(read);
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

/// Adds to `set` each lint `attr`, with `cfg_attr` expanded against `cfg`, gives a level other
/// than `allow`: what blaze-diagnostics' `may_fire` scans the crate for. A name that sets
/// nothing (an unknown one, a removed one, a tool's not in the tables) adds nothing; nothing is
/// checked of where the attribute is, which can only add a lint the levels leave allowed.
pub fn not_allowed(attr: ast::Attr<'_>, store: &Store, cfg: &CfgOptions, edition: Edition, set: &mut LintSet) {
    hir_expand::attrs::expand_cfg_attr_view::<()>(std::iter::once(attr), || cfg, |meta, _| {
        if let Some(Read::Level(level, items, _)) = read_meta(meta, edition, false)
            && level != Level::Allow
        {
            for item in &items {
                let ItemKind::Word(path) = &item.kind else { continue };
                let lints: &[&Lint] = match &path[..] {
                    [only] => store.lints_named(None, only.name()),
                    [tool, name] => store.lints_named(Some(tool.name()), name.name()),
                    _ => &[],
                };
                for lint in lints {
                    set.insert(lint.id());
                }
            }
        }
        ControlFlow::Continue(())
    });
}

/// The attributes the lint levels read: the five that set a level, at the indices of their
/// levels in [`LEVELS`], then `doc` and `automatically_derived`.
const ATTR_NAMES: &[&str] = &["allow", "expect", "warn", "deny", "forbid", "doc", "automatically_derived"];
const LEVELS: [Level; 5] = [Level::Allow, Level::Expect, Level::Warn, Level::Deny, Level::Forbid];
const DOC: usize = 5;
const AUTOMATICALLY_DERIVED: usize = 6;

blaze_phf::phf_table!(static ATTRS = ATTR_NAMES;);

fn read_meta<'t>(meta: ast::Meta<'t>, edition: Edition, direct: bool) -> Option<Read<'t>> {
    let name = |path: Option<ast::Path<'t>>| path.and_then(|it| it.as_single_name_ref());
    match meta {
        ast::Meta::PathMeta(it) => {
            let name = name(it.path())?;
            (ATTRS.get(name.text()) == Some(AUTOMATICALLY_DERIVED)).then_some(Read::Allows(LintId::SINGLE_USE_LIFETIMES))
        }
        ast::Meta::TokenTreeMeta(it) => {
            let name = name(it.path())?;
            let level = match ATTRS.get(name.text())? {
                DOC => None,
                at if at < LEVELS.len() => Some(LEVELS[at]),
                _ => return None,
            };
            let items = list(it.token_tree()?, edition)?;
            match level {
                Some(level) => Some(Read::Level(level, items, direct)),
                None => {
                    let hidden = items.iter().any(|it| matches!(&it.kind, ItemKind::Word(path) if is_word(path, "hidden")));
                    hidden.then_some(Read::Allows(LintId::MISSING_DOCS))
                }
            }
        }
        _ => None,
    }
}

/// A parenthesized list read as rustc's `MetaItemKind::list_from_tokens` reads it: `None`
/// when it is not one, and rustc's lint levels pass the attribute by.
pub fn list(tt: ast::TokenTree<'_>, edition: Edition) -> Option<Items<'_>> {
    let children: SmallVec<[ast::Element<'_>; 16]> =
        // alloc: none up to 16 tokens, a `SmallVec`
        tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    let (first, last) = (children.first()?, children.last()?);
    if first.kind() != SyntaxKind::L_PAREN || last.kind() != SyntaxKind::R_PAREN || children.len() < 2 {
        return None;
    }
    let inner = &children[1..children.len() - 1];
    let mut items = Items::new();
    let mut rest = inner;
    while !rest.is_empty() {
        let end = rest.iter().position(|it| it.kind() == SyntaxKind::COMMA).unwrap_or(rest.len());
        items.push(item(&rest[..end], edition)?);
        rest = match rest.get(end) {
            Some(_) => &rest[end + 1..],
            None => &[],
        };
    }
    Some(items)
}

fn item<'t>(elements: &[ast::Element<'t>], edition: Edition) -> Option<Item<'t>> {
    let range = TextRange::new(elements.first()?.text_range().start(), elements.last()?.text_range().end());
    if let [NodeOrToken::Token(token)] = elements
        && is_literal(token.kind())
    {
        return Some(Item { range, kind: ItemKind::Lit });
    }
    let (path, rest) = path(elements, edition)?;
    let kind = match rest {
        [] => ItemKind::Word(path),
        [NodeOrToken::Token(eq), NodeOrToken::Token(value)]
            if eq.kind() == SyntaxKind::EQ && is_literal(value.kind()) =>
        {
            let string = ast::String::cast(*value).and_then(|it| it.value().ok().map(|it| Symbol::intern(&it)));
            ItemKind::NameValue { path, string, value: value.text_range() }
        }
        [NodeOrToken::Node(tt)] if tt.kind() == SyntaxKind::TOKEN_TREE => {
            let open = tt.first_token().map(|it| it.kind());
            if open != Some(SyntaxKind::L_PAREN) {
                return None;
            }
            ItemKind::List(path)
        }
        _ => return None,
    };
    Some(Item { range, kind })
}

/// The path `elements` start with and what follows it. A path that starts with `::` has
/// rustc's `{{root}}` for its first segment, at the `::`.
fn path<'e, 't>(elements: &'e [ast::Element<'t>], edition: Edition) -> Option<(Path<'t>, &'e [ast::Element<'t>])> {
    let mut segments = Path::new();
    let mut rest = elements;
    if let Some((range, after)) = colons(rest) {
        segments.push(Segment { token: None, range });
        rest = after;
    }
    loop {
        let (NodeOrToken::Token(token), after) = rest.split_first()? else { return None };
        if !is_segment(token.kind(), edition) {
            return None;
        }
        segments.push(Segment { token: Some(*token), range: token.text_range() });
        rest = after;
        match colons(rest) {
            Some((_, after)) => rest = after,
            None => return Some((segments, rest)),
        }
    }
}

/// `::` at the start of `elements`, as one token or two `:` together.
fn colons<'e, 't>(elements: &'e [ast::Element<'t>]) -> Option<(TextRange, &'e [ast::Element<'t>])> {
    match elements {
        [NodeOrToken::Token(it), rest @ ..] if it.kind() == SyntaxKind::COLON2 => Some((it.text_range(), rest)),
        [NodeOrToken::Token(a), NodeOrToken::Token(b), rest @ ..]
            if a.kind() == SyntaxKind::COLON
                && b.kind() == SyntaxKind::COLON
                && a.text_range().end() == b.text_range().start() =>
        {
            Some((a.text_range().cover(b.text_range()), rest))
        }
        _ => None,
    }
}

fn is_segment(kind: SyntaxKind, edition: Edition) -> bool {
    use SyntaxKind::*;
    kind == IDENT
        || matches!(kind, SELF_KW | SUPER_KW | CRATE_KW | SELF_TYPE_KW)
        || (kind.is_any_identifier() && !kind.is_keyword(edition))
}

fn is_literal(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
}

/// The tools of the crate's `#![register_tool(..)]`s among `attrs`: the root file's inner
/// attributes, then those of `-Z crate-attr`.
pub fn registered_tools<'t>(attrs: impl Iterator<Item = ast::Attr<'t>>, cfg: &CfgOptions, edition: Edition) -> Vec<String> {
    // alloc: setup, once per crate's levels, the crate root's tools
    let mut tools = Vec::new();
    hir_expand::attrs::expand_cfg_attr_view::<()>(attrs, || cfg, |meta, _| {
        if let ast::Meta::TokenTreeMeta(meta) = meta
            && meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "register_tool")
            && let Some(items) = meta.token_tree().and_then(|it| list(it, edition))
        {
            for item in items {
                if let ItemKind::Word(path) = item.kind
                    && let [tool] = &path[..]
                {
                    // alloc: as above
                    tools.push(tool.name().to_owned());
                }
            }
        }
        ControlFlow::Continue(())
    });
    tools
}

/// The node whose attributes come next going out from `node`, in crate `krate`: its parent;
/// for the root of a macro's expansion, what is around the call (around the item, for a derive
/// or an attribute macro); for the root of a module's file, its `mod` item; none for the
/// crate's root file, or a file that is no module of `krate`.
pub fn parent<'db>(db: &'db Db, krate: Crate, node: InFile<ast::Node<'db>>) -> Option<InFile<ast::Node<'db>>> {
    if let Some(parent) = node.value.parent() {
        return Some(node.with_value(parent));
    }
    let Some(call) = node.file_id.macro_file() else {
        return declaration(db, krate, node.file_id);
    };
    let loc = call.loc(db);
    let at = loc.to_node_view(db);
    let around = match loc.kind {
        MacroCallKind::FnLike { .. } => {
            at.value.ancestors().skip(1).find(|it| !matches!(it.kind(), SyntaxKind::MACRO_EXPR | SyntaxKind::MACRO_PAT | SyntaxKind::MACRO_TYPE))
        }
        MacroCallKind::Derive { .. } => {
            at.value.ancestors().find(|it| ast::Adt::can_cast(it.kind())).and_then(|it| it.parent())
        }
        MacroCallKind::Attr { .. } => at.value.parent(),
    };
    Some(at.with_value(around?))
}

/// The `mod` item the module whose file is `file` is declared by, in `krate`'s def map.
fn declaration(db: &Db, krate: Crate, file: HirFileId) -> Option<InFile<ast::Node<'_>>> {
    let file = file.file_id()?.file_id(db);
    let def_map = crate_def_map(db, krate);
    let module = def_map.modules_for_file(db, file).next()?;
    match def_map.module(db, module).origin {
        ModuleOrigin::CrateRoot { .. } => None,
        ModuleOrigin::File { declaration, declaration_tree_id, .. } => {
            let file_id = declaration_tree_id.file_id();
            Some(InFile::new(file_id, AstId::new(file_id, declaration).to_view(db).syntax()))
        }
        ModuleOrigin::Inline { .. } | ModuleOrigin::BlockExpr { .. } => {
            unreachable!("a module found by its file is a file's or the crate root's")
        }
    }
}

/// `node` of its file, as a key.
pub fn key(node: InFile<ast::Node<'_>>) -> InFile<SyntaxNodePtr> {
    node.map(ast::ptr)
}

#[cfg(test)]
mod tests {
    use syntax::SourceFile;

    use super::*;

    /// What a [`Read`] says, to compare two.
    fn plain(read: Option<Read>) -> Option<(Option<Level>, Option<LintId>, usize, bool)> {
        read.map(|it| match it {
            Read::Allows(id) => (None, Some(id), 0, false),
            Read::Level(level, items, direct) => (Some(level), None, items.len(), direct),
        })
    }

    /// `read_meta` as it was, by string `match`.
    fn by_match<'t>(meta: ast::Meta<'t>, edition: Edition, direct: bool) -> Option<Read<'t>> {
        let name = |path: Option<ast::Path<'t>>| path.and_then(|it| it.as_single_name_ref());
        match meta {
            ast::Meta::PathMeta(it) if name(it.path()).is_some_and(|it| it.text() == "automatically_derived") => {
                Some(Read::Allows(LintId::SINGLE_USE_LIFETIMES))
            }
            ast::Meta::TokenTreeMeta(it) => {
                let name = name(it.path())?;
                let name = name.text();
                let level = match name {
                    "doc" => None,
                    "allow" => Some(Level::Allow),
                    "expect" => Some(Level::Expect),
                    "warn" => Some(Level::Warn),
                    "deny" => Some(Level::Deny),
                    "forbid" => Some(Level::Forbid),
                    _ => return None,
                };
                let items = list(it.token_tree()?, edition)?;
                match level {
                    Some(level) => Some(Read::Level(level, items, direct)),
                    None => {
                        let hidden = items.iter().any(|it| matches!(&it.kind, ItemKind::Word(path) if is_word(path, "hidden")));
                        hidden.then_some(Read::Allows(LintId::MISSING_DOCS))
                    }
                }
            }
            _ => None,
        }
    }

    #[test]
    fn attribute_names_are_read_as_matched() {
        let names = [
            "allow", "expect", "warn", "deny", "forbid", "doc", "automatically_derived", "Allow", "allows", "allo",
            "force_warn", "cfg", "derive", "automatically_derive", "docs", "deny_", "x", "inline", "rustfmt",
        ];
        let mut text = String::new();
        for name in names {
            for args in ["", "(dead_code)", "(hidden)", "(a, b, reason = \"r\")", " = \"v\"", "()"] {
                text.push_str(&format!("#[{name}{args}]\n#[rustc::{name}{args}]\n"));
            }
        }
        text.push_str("fn f() {}\n");
        let edition = Edition::Edition2021;
        let parse = SourceFile::parse(&text, edition);
        let mut seen = 0;
        for meta in parse.root().descendants().filter_map(ast::Meta::cast) {
            for direct in [false, true] {
                let written = meta.syntax();
                assert_eq!(plain(read_meta(meta, edition, direct)), plain(by_match(meta, edition, direct)), "{written}");
            }
            seen += 1;
        }
        assert_eq!(seen, names.len() * 6 * 2);
    }

    #[test]
    fn a_crate_attrs_levels_are_never_read_as_written_in_the_root() {
        let edition = Edition::Edition2021;
        let parse = SourceFile::parse("#![deny(dead_code)]\n", edition);
        let reads = read_crate_attr(parse.root(), &CfgOptions::default(), edition);
        assert_eq!(reads.len(), 1);
        assert!(matches!(&reads[0], Read::Level(Level::Deny, items, false) if items.len() == 1));
    }
}
