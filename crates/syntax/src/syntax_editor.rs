//! Syntax Tree editor
//!
//! Inspired by Roslyn's [`SyntaxEditor`].
//!
//! [`SyntaxEditor`]: https://github.com/dotnet/roslyn/blob/43b0b05cc4f492fd5de00f6f6717409091df8daa/src/Workspaces/Core/Portable/Editing/SyntaxEditor.cs

use std::{
    cell::RefCell,
    fmt, iter,
    num::NonZeroU32,
    ops::RangeInclusive,
    sync::atomic::{AtomicU32, Ordering},
};

use blaze_hash::HashMap;
use syntax_tree::TextRange;

use crate::{
    AstNode, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken, T,
    ast::{self, edit::IndentLevel, syntax_factory::SyntaxFactory},
};

mod edit_algo;
mod edits;
mod mapping;

pub use edits::{GetOrCreateWhereClause, Removable};
pub use mapping::{SyntaxMapping, SyntaxMappingBuilder};

#[derive(Debug)]
pub struct SyntaxEditor {
    root: SyntaxNode,
    changes: RefCell<Vec<Change>>,
    annotations: RefCell<Vec<(SyntaxElement, SyntaxAnnotation)>>,
    make: SyntaxFactory,
}

impl SyntaxEditor {
    /// Creates a syntax editor from `root`.
    ///
    /// The returned `root` is guaranteed to be a detached, immutable node.
    /// If the provided node is not a root (i.e., has a parent), it is cloned
    /// into a fresh subtree to satisfy syntax editor invariants.
    pub fn new(root: SyntaxNode) -> (Self, SyntaxNode) {
        let mut root = root;

        if root.parent().is_some() {
            root = root.clone_subtree()
        };

        let editor = Self {
            root: root.clone(),
            changes: RefCell::new(Vec::new()),
            annotations: RefCell::new(Vec::new()),
            make: SyntaxFactory::with_mappings(),
        };

        (editor, root)
    }

    /// Typed-node variant of [`SyntaxEditor::new`].
    pub fn with_ast_node<T>(root: &T) -> (Self, T)
    where
        T: AstNode,
    {
        let (editor, root) = Self::new(root.syntax().clone());

        (editor, T::cast(root).unwrap())
    }

    pub fn make(&self) -> &SyntaxFactory {
        &self.make
    }

    pub fn add_annotation(&self, element: impl Element, annotation: SyntaxAnnotation) {
        self.annotations.borrow_mut().push((element.syntax_element(), annotation))
    }

    pub fn add_annotation_all(&self, elements: Vec<impl Element>, annotation: SyntaxAnnotation) {
        self.annotations
            .borrow_mut()
            .extend(elements.into_iter().map(|e| e.syntax_element()).zip(iter::repeat(annotation)));
    }

    pub fn merge(&self, other: SyntaxEditor) {
        debug_assert!(
            self.root == other.root || other.root.ancestors().any(|node| node == self.root),
            "{:?} is not in the same tree as {:?}",
            other.root,
            self.root
        );

        self.changes.borrow_mut().append(&mut other.changes.into_inner());
        if let Some(mut m) = self.make.mappings() {
            m.merge(other.make.take());
        }
        self.annotations.borrow_mut().append(&mut other.annotations.into_inner());
    }

    pub fn insert(&self, position: Position, element: impl Element) {
        debug_assert!(is_ancestor_or_self(&position.parent(), &self.root));
        self.changes.borrow_mut().push(Change::Insert(position, element.syntax_element()))
    }

    pub fn insert_all(&self, position: Position, elements: Vec<SyntaxElement>) {
        debug_assert!(is_ancestor_or_self(&position.parent(), &self.root));
        self.changes.borrow_mut().push(Change::InsertAll(position, elements))
    }

    pub fn insert_with_whitespace(&self, position: Position, element: impl Element) {
        self.insert_all_with_whitespace(position, vec![element.syntax_element()])
    }

    pub fn insert_all_with_whitespace(&self, position: Position, mut elements: Vec<SyntaxElement>) {
        if let Some(first) = elements.first()
            && let Some(ws) = ws_before(&position, first, &self.make)
        {
            elements.insert(0, ws.into());
        }
        if let Some(last) = elements.last()
            && let Some(ws) = ws_after(&position, last, &self.make)
        {
            elements.push(ws.into());
        }
        self.insert_all(position, elements)
    }

    pub fn delete(&self, element: impl Element) {
        let element = element.syntax_element();
        debug_assert!(is_ancestor_or_self_of_element(&element, &self.root));
        debug_assert!(
            !matches!(&element, SyntaxElement::Node(node) if node == &self.root),
            "should not delete root node"
        );
        let mut changes = self.changes.borrow_mut();
        for change in changes.iter_mut() {
            if let Change::Replace(existing, replacement) = change
                && *existing == element
            {
                if replacement.is_none() {
                    return;
                }
                *replacement = None;
                return;
            }
        }
        changes.push(Change::Replace(element, None));
    }

    pub fn delete_all(&self, range: RangeInclusive<SyntaxElement>) {
        if range.start() == range.end() {
            self.delete(range.start());
            return;
        }

        debug_assert!(is_ancestor_or_self_of_element(range.start(), &self.root));
        self.changes.borrow_mut().push(Change::ReplaceAll(range, Vec::new()))
    }

    pub fn replace(&self, old: impl Element, new: impl Element) {
        let old = old.syntax_element();
        debug_assert!(is_ancestor_or_self_of_element(&old, &self.root));
        let new = new.syntax_element();
        let mut changes = self.changes.borrow_mut();
        for change in changes.iter_mut() {
            if let Change::Replace(existing, replacement) = change
                && *existing == old
            {
                match replacement {
                    None => return,
                    Some(existing_new) if *existing_new == new => return,
                    Some(existing_new) => {
                        *existing_new = new;
                        return;
                    }
                }
            }
        }
        changes.push(Change::Replace(old, Some(new)));
    }

    pub fn replace_with_many(&self, old: impl Element, new: Vec<SyntaxElement>) {
        let old = old.syntax_element();
        debug_assert!(is_ancestor_or_self_of_element(&old, &self.root));
        debug_assert!(
            !(matches!(&old, SyntaxElement::Node(node) if node == &self.root) && new.len() > 1),
            "cannot replace root node with many elements"
        );
        self.changes.borrow_mut().push(Change::ReplaceWithMany(old.syntax_element(), new));
    }

    pub fn replace_all(&self, range: RangeInclusive<SyntaxElement>, new: Vec<SyntaxElement>) {
        if range.start() == range.end() {
            self.replace_with_many(range.start(), new);
            return;
        }

        debug_assert!(is_ancestor_or_self_of_element(range.start(), &self.root));
        self.changes.borrow_mut().push(Change::ReplaceAll(range, new))
    }

    pub fn finish(self) -> SyntaxEdit {
        edit_algo::apply_edits(self)
    }

    pub fn deleted(&self, element: impl Element) -> bool {
        let element = element.syntax_element();
        self.changes
            .borrow()
            .iter()
            .any(|change| matches!(change, Change::Replace(existing, None) if *existing == element))
    }
}

/// Represents a completed [`SyntaxEditor`] operation.
pub struct SyntaxEdit {
    old_root: SyntaxNode,
    new_root: SyntaxNode,
    changed_elements: Vec<SyntaxElement>,
    annotations: HashMap<SyntaxAnnotation, Vec<SyntaxElement>>,
}

impl SyntaxEdit {
    /// Root of the initial unmodified syntax tree.
    pub fn old_root(&self) -> &SyntaxNode {
        &self.old_root
    }

    /// Root of the modified syntax tree.
    pub fn new_root(&self) -> &SyntaxNode {
        &self.new_root
    }

    /// Which syntax elements in the modified syntax tree were inserted or
    /// modified as part of the edit.
    ///
    /// Note that for syntax nodes, only the upper-most parent of a set of
    /// changes is included, not any child elements that may have been modified.
    pub fn changed_elements(&self) -> &[SyntaxElement] {
        self.changed_elements.as_slice()
    }

    /// Finds which syntax elements have been annotated with the given
    /// annotation.
    ///
    /// Note that an annotation might not appear in the modified syntax tree if
    /// the syntax elements that were annotated did not make it into the final
    /// syntax tree.
    pub fn find_annotation(&self, annotation: SyntaxAnnotation) -> &[SyntaxElement] {
        self.annotations.get(&annotation).as_ref().map_or(&[], |it| it.as_slice())
    }

    pub fn find_element(&self, old_node: &SyntaxNode) -> Option<SyntaxNode> {
        let old_root_start = self.old_root.text_range().start();
        let old_start = old_node.text_range().start() - old_root_start;
        let new_root_start = self.new_root.text_range().start();
        let kind = old_node.kind();

        self.new_root
            .descendants()
            .find(|it| it.kind() == kind && it.text_range().start() - new_root_start == old_start)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct SyntaxAnnotation(NonZeroU32);

impl Default for SyntaxAnnotation {
    fn default() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(1);

        // Only consistency within a thread matters, as SyntaxElements are !Send
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);

        Self(NonZeroU32::new(id).expect("syntax annotation id overflow"))
    }
}

/// Position describing where to insert elements
#[derive(Debug)]
pub struct Position {
    repr: PositionRepr,
}

impl Position {
    pub(crate) fn parent(&self) -> SyntaxNode {
        self.place().0
    }

    pub(crate) fn place(&self) -> (SyntaxNode, usize) {
        match &self.repr {
            PositionRepr::FirstChild(parent) => (parent.clone(), 0),
            PositionRepr::After(child) => (child.parent().unwrap(), child.index() + 1),
        }
    }
}

#[derive(Debug)]
enum PositionRepr {
    FirstChild(SyntaxNode),
    After(SyntaxElement),
}

impl Position {
    pub fn after(elem: impl Element) -> Position {
        let repr = PositionRepr::After(elem.syntax_element());
        Position { repr }
    }

    pub fn before(elem: impl Element) -> Position {
        let elem = elem.syntax_element();
        let repr = match elem.prev_sibling_or_token() {
            Some(it) => PositionRepr::After(it),
            None => PositionRepr::FirstChild(elem.parent().unwrap()),
        };
        Position { repr }
    }

    pub fn first_child_of(node: &(impl Into<SyntaxNode> + Clone)) -> Position {
        let repr = PositionRepr::FirstChild(node.clone().into());
        Position { repr }
    }

    pub fn last_child_of(node: &(impl Into<SyntaxNode> + Clone)) -> Position {
        let node = node.clone().into();
        let repr = match node.last_child_or_token() {
            Some(it) => PositionRepr::After(it),
            None => PositionRepr::FirstChild(node),
        };
        Position { repr }
    }
}

#[derive(Debug)]
enum Change {
    /// Inserts a single element at the specified position.
    Insert(Position, SyntaxElement),
    /// Inserts many elements in-order at the specified position.
    InsertAll(Position, Vec<SyntaxElement>),
    /// Represents both a replace single element and a delete element operation.
    Replace(SyntaxElement, Option<SyntaxElement>),
    /// Replaces a single element with many elements.
    ReplaceWithMany(SyntaxElement, Vec<SyntaxElement>),
    /// Replaces a range of elements with another list of elements.
    /// Range will always have start != end.
    ReplaceAll(RangeInclusive<SyntaxElement>, Vec<SyntaxElement>),
}

impl Change {
    fn target_range(&self) -> TextRange {
        match self {
            Change::Insert(target, _) | Change::InsertAll(target, _) => match &target.repr {
                PositionRepr::FirstChild(parent) => TextRange::at(
                    parent.first_child_or_token().unwrap().text_range().start(),
                    0.into(),
                ),
                PositionRepr::After(child) => TextRange::at(child.text_range().end(), 0.into()),
            },
            Change::Replace(target, _) | Change::ReplaceWithMany(target, _) => target.text_range(),
            Change::ReplaceAll(range, _) => {
                range.start().text_range().cover(range.end().text_range())
            }
        }
    }

    fn target_parent(&self) -> SyntaxNode {
        match self {
            Change::Insert(target, _) | Change::InsertAll(target, _) => target.parent(),
            Change::Replace(target, _) | Change::ReplaceWithMany(target, _) => match target {
                SyntaxElement::Node(target) => target.parent().unwrap_or_else(|| target.clone()),
                SyntaxElement::Token(target) => target.parent().unwrap(),
            },
            Change::ReplaceAll(target, _) => target.start().parent().unwrap(),
        }
    }

    fn change_kind(&self) -> ChangeKind {
        match self {
            Change::Insert(_, _) | Change::InsertAll(_, _) => ChangeKind::Insert,
            Change::Replace(_, _) | Change::ReplaceWithMany(_, _) => ChangeKind::Replace,
            Change::ReplaceAll(_, _) => ChangeKind::ReplaceRange,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ChangeKind {
    Insert,
    ReplaceRange,
    Replace,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Insert(position, node_or_token) => {
                let parent = position.parent();
                let mut parent_str = parent.to_string();
                let target_range = self.target_range().start() - parent.text_range().start();

                parent_str.insert_str(
                    target_range.into(),
                    &format!("\x1b[42m{node_or_token}\x1b[0m\x1b[K"),
                );
                f.write_str(&parent_str)
            }
            Change::InsertAll(position, vec) => {
                let parent = position.parent();
                let mut parent_str = parent.to_string();
                let target_range = self.target_range().start() - parent.text_range().start();
                let insertion: String = vec.iter().map(|it| it.to_string()).collect();

                parent_str
                    .insert_str(target_range.into(), &format!("\x1b[42m{insertion}\x1b[0m\x1b[K"));
                f.write_str(&parent_str)
            }
            Change::Replace(old, new) => {
                if let Some(new) = new {
                    write!(f, "\x1b[41m{old}\x1b[42m{new}\x1b[0m\x1b[K")
                } else {
                    write!(f, "\x1b[41m{old}\x1b[0m\x1b[K")
                }
            }
            Change::ReplaceWithMany(old, vec) => {
                let new: String = vec.iter().map(|it| it.to_string()).collect();
                write!(f, "\x1b[41m{old}\x1b[42m{new}\x1b[0m\x1b[K")
            }
            Change::ReplaceAll(range, vec) => {
                let parent = range.start().parent().unwrap();
                let parent_str = parent.to_string();
                let pre_range =
                    TextRange::new(parent.text_range().start(), range.start().text_range().start());
                let old_range = TextRange::new(
                    range.start().text_range().start(),
                    range.end().text_range().end(),
                );
                let post_range =
                    TextRange::new(range.end().text_range().end(), parent.text_range().end());

                let pre_str = &parent_str[pre_range - parent.text_range().start()];
                let old_str = &parent_str[old_range - parent.text_range().start()];
                let post_str = &parent_str[post_range - parent.text_range().start()];
                let new: String = vec.iter().map(|it| it.to_string()).collect();

                write!(f, "{pre_str}\x1b[41m{old_str}\x1b[42m{new}\x1b[0m\x1b[K{post_str}")
            }
        }
    }
}

/// Utility trait to allow calling syntax editor functions with references or owned
/// nodes. Do not use outside of this module.
pub trait Element {
    fn syntax_element(self) -> SyntaxElement;
}

impl<E: Element + Clone> Element for &'_ E {
    fn syntax_element(self) -> SyntaxElement {
        self.clone().syntax_element()
    }
}

impl Element for SyntaxElement {
    fn syntax_element(self) -> SyntaxElement {
        self
    }
}

impl Element for SyntaxNode {
    fn syntax_element(self) -> SyntaxElement {
        self.into()
    }
}

impl Element for SyntaxToken {
    fn syntax_element(self) -> SyntaxElement {
        self.into()
    }
}

fn ws_before(
    position: &Position,
    new: &SyntaxElement,
    factory: &SyntaxFactory,
) -> Option<SyntaxToken> {
    let prev = match &position.repr {
        PositionRepr::FirstChild(_) => return None,
        PositionRepr::After(it) => it,
    };

    if prev.kind() == T!['{']
        && new.kind() == SyntaxKind::USE
        && let Some(item_list) = prev.parent().and_then(ast::ItemList::cast)
    {
        let mut indent = IndentLevel::from_element(&item_list.syntax().clone().into());
        indent.0 += 1;
        return Some(factory.whitespace(&format!("\n{indent}")));
    }

    if prev.kind() == T!['{']
        && ast::Stmt::can_cast(new.kind())
        && let Some(stmt_list) = prev.parent().and_then(ast::StmtList::cast)
    {
        let mut indent = IndentLevel::from_element(&stmt_list.syntax().clone().into());
        indent.0 += 1;
        return Some(factory.whitespace(&format!("\n{indent}")));
    }

    ws_between(prev, new, factory)
}

fn ws_after(
    position: &Position,
    new: &SyntaxElement,
    factory: &SyntaxFactory,
) -> Option<SyntaxToken> {
    let next = match &position.repr {
        PositionRepr::FirstChild(parent) => parent.first_child_or_token()?,
        PositionRepr::After(sibling) => sibling.next_sibling_or_token()?,
    };
    ws_between(new, &next, factory)
}

fn ws_between(
    left: &SyntaxElement,
    right: &SyntaxElement,
    factory: &SyntaxFactory,
) -> Option<SyntaxToken> {
    if left.kind() == SyntaxKind::WHITESPACE || right.kind() == SyntaxKind::WHITESPACE {
        return None;
    }
    if right.kind() == T![;] || right.kind() == T![,] {
        return None;
    }
    if left.kind() == T![<] || right.kind() == T![>] {
        return None;
    }
    if left.kind() == T![&] && right.kind() == SyntaxKind::LIFETIME {
        return None;
    }
    if right.kind() == SyntaxKind::GENERIC_ARG_LIST {
        return None;
    }
    if right.kind() == SyntaxKind::USE {
        let mut indent = IndentLevel::from_element(left);
        if left.kind() == SyntaxKind::USE {
            indent.0 = IndentLevel::from_element(right).0.max(indent.0);
        }
        return Some(factory.whitespace(&format!("\n{indent}")));
    }
    if left.kind() == SyntaxKind::ATTR {
        let mut indent = IndentLevel::from_element(right);
        if right.kind() == SyntaxKind::ATTR {
            indent.0 = IndentLevel::from_element(left).0.max(indent.0);
        }
        return Some(factory.whitespace(&format!("\n{indent}")));
    }
    Some(factory.whitespace(" "))
}

fn is_ancestor_or_self(node: &SyntaxNode, ancestor: &SyntaxNode) -> bool {
    node == ancestor || node.ancestors().any(|it| &it == ancestor)
}

fn is_ancestor_or_self_of_element(node: &SyntaxElement, ancestor: &SyntaxNode) -> bool {
    matches!(node, SyntaxElement::Node(node) if node == ancestor)
        || node.ancestors().any(|it| &it == ancestor)
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use expect_test::expect;

    use crate::{
        AstNode, Text,
        ast::{self, HasGenericParams as _, make},
    };

    use super::*;

    #[test]
    fn basic_usage() {
        let root = make::match_arm(
            make::wildcard_pat().into(),
            None,
            make::expr_tuple([
                make::expr_bin_op(
                    make::expr_literal("2").into(),
                    ast::BinaryOp::ArithOp(ast::ArithOp::Add),
                    make::expr_literal("2").into(),
                ),
                make::expr_literal("true").into(),
            ])
            .into(),
        );

        let (editor, root) = SyntaxEditor::with_ast_node(&root);
        let make = editor.make();

        let to_wrap = root.syntax().descendants().find_map(ast::TupleExpr::cast).unwrap();
        let to_replace = root.syntax().descendants().find_map(ast::BinExpr::cast).unwrap();

        let name = make::name("var_name");
        let name_ref = make::name_ref("var_name");

        let placeholder_snippet = SyntaxAnnotation::default();
        editor.add_annotation(name.syntax(), placeholder_snippet);
        editor.add_annotation(name_ref.syntax(), placeholder_snippet);

        let new_block = make.block_expr(
            [editor
                .make()
                .let_stmt(
                    make.ident_pat(false, false, name.clone()).into(),
                    None,
                    Some(to_replace.clone().into()),
                )
                .into()],
            Some(to_wrap.clone().into()),
        );

        editor.replace(to_replace.syntax(), name_ref.syntax());
        editor.replace(to_wrap.syntax(), new_block.syntax());

        let edit = editor.finish();

        let expect = expect![[r#"
            _ => {
                let var_name = 2 + 2;
                (var_name, true)
            },"#]];
        expect.assert_eq(&edit.new_root.to_string());

        assert_eq!(edit.find_annotation(placeholder_snippet).len(), 2);
        assert!(
            edit.annotations
                .values()
                .flatten()
                .all(|element| element.ancestors().any(|it| &it == edit.new_root()))
        )
    }

    #[test]
    fn test_insert_independent() {
        let root = make::block_expr(
            [make::let_stmt(
                make::ext::simple_ident_pat(make::name("second")).into(),
                None,
                Some(make::expr_literal("2").into()),
            )
            .into()],
            None,
        );

        let (editor, root) = SyntaxEditor::with_ast_node(&root);
        let make = editor.make();
        let second_let = root.syntax().descendants().find_map(ast::LetStmt::cast).unwrap();

        editor.insert(
            Position::first_child_of(root.stmt_list().unwrap().syntax()),
            make.let_stmt(
                make::ext::simple_ident_pat(make::name("first")).into(),
                None,
                Some(make::expr_literal("1").into()),
            )
            .syntax(),
        );

        editor.insert(
            Position::after(second_let.syntax()),
            make.let_stmt(
                make::ext::simple_ident_pat(make::name("third")).into(),
                None,
                Some(make::expr_literal("3").into()),
            )
            .syntax(),
        );

        let edit = editor.finish();

        let expect = expect![[r#"
            let first = 1;{
                let second = 2;let third = 3;
            }"#]];
        expect.assert_eq(&edit.new_root.to_string());
    }

    #[test]
    fn test_insert_dependent() {
        let root = make::block_expr(
            [],
            Some(
                make::block_expr(
                    [make::let_stmt(
                        make::ext::simple_ident_pat(make::name("second")).into(),
                        None,
                        Some(make::expr_literal("2").into()),
                    )
                    .into()],
                    None,
                )
                .into(),
            ),
        );

        let (editor, root) = SyntaxEditor::with_ast_node(&root);
        let make = editor.make();

        let inner_block =
            root.syntax().descendants().flat_map(ast::BlockExpr::cast).nth(1).unwrap();
        let second_let = root.syntax().descendants().find_map(ast::LetStmt::cast).unwrap();

        let new_block_expr = make.block_expr([], Some(ast::Expr::BlockExpr(inner_block.clone())));

        let first_let = make.let_stmt(
            make::ext::simple_ident_pat(make::name("first")).into(),
            None,
            Some(make::expr_literal("1").into()),
        );

        let third_let = make.let_stmt(
            make::ext::simple_ident_pat(make::name("third")).into(),
            None,
            Some(make::expr_literal("3").into()),
        );

        editor.insert(
            Position::first_child_of(inner_block.stmt_list().unwrap().syntax()),
            first_let.syntax(),
        );
        editor.insert(Position::after(second_let.syntax()), third_let.syntax());
        editor.replace(inner_block.syntax(), new_block_expr.syntax());

        let edit = editor.finish();

        let expect = expect![[r#"
            {
                {
                let first = 1;{
                let second = 2;let third = 3;
            }
            }
            }"#]];
        expect.assert_eq(&edit.new_root.to_string());
    }

    #[test]
    fn test_dependent_change_prefers_nearest_changed_ancestor() {
        let root = make::block_expr(
            [],
            Some(
                make::block_expr(
                    [make::let_stmt(
                        make::ext::simple_ident_pat(make::name("second")).into(),
                        None,
                        Some(make::expr_literal("2").into()),
                    )
                    .into()],
                    None,
                )
                .into(),
            ),
        );

        let (editor, root) = SyntaxEditor::with_ast_node(&root);
        let make = editor.make();

        let inner_block =
            root.syntax().descendants().flat_map(ast::BlockExpr::cast).nth(1).unwrap();

        let outer_replacement = make.block_expr([], Some(ast::Expr::BlockExpr(root.clone())));
        let inner_replacement =
            make.block_expr([], Some(ast::Expr::BlockExpr(inner_block.clone())));

        let first_let = make.let_stmt(
            make::ext::simple_ident_pat(make::name("first")).into(),
            None,
            Some(make::expr_literal("1").into()),
        );

        editor.insert(
            Position::first_child_of(inner_block.stmt_list().unwrap().syntax()),
            first_let.syntax(),
        );
        editor.replace(inner_block.syntax(), inner_replacement.syntax());
        editor.replace(root.syntax(), outer_replacement.syntax());

        let edit = editor.finish();

        let expect = expect![[r#"
            {
                {
                {
                let first = 1;{
                let second = 2;
            }
            }
            }
            }"#]];
        expect.assert_eq(&edit.new_root.to_string());
    }

    #[test]
    fn test_replace_root_with_dependent() {
        let root = make::block_expr(
            [make::let_stmt(
                make::ext::simple_ident_pat(make::name("second")).into(),
                None,
                Some(make::expr_literal("2").into()),
            )
            .into()],
            None,
        );

        let (editor, root) = SyntaxEditor::with_ast_node(&root);
        let make = editor.make();

        let inner_block = root;

        let new_block_expr = make.block_expr([], Some(ast::Expr::BlockExpr(inner_block.clone())));

        let first_let = make.let_stmt(
            make::ext::simple_ident_pat(make::name("first")).into(),
            None,
            Some(make::expr_literal("1").into()),
        );

        editor.insert(
            Position::first_child_of(inner_block.stmt_list().unwrap().syntax()),
            first_let.syntax(),
        );
        editor.replace(inner_block.syntax(), new_block_expr.syntax());

        let edit = editor.finish();

        let expect = expect![[r#"
            {
                let first = 1;{
                let second = 2;
            }
            }"#]];
        expect.assert_eq(&edit.new_root.to_string());
    }

    #[test]
    fn test_replace_token_in_parent() {
        let parent_fn = make::fn_(
            None,
            None,
            make::name("it"),
            None,
            None,
            make::param_list(None, []),
            make::block_expr([], Some(make::ext::expr_unit())),
            Some(make::ret_type(make::ty_unit())),
            false,
            false,
            false,
            false,
        );

        let (editor, parent_fn) = SyntaxEditor::with_ast_node(&parent_fn);

        if let Some(ret_ty) = parent_fn.ret_type() {
            editor.delete(ret_ty.syntax().clone());

            if let Some(SyntaxElement::Token(token)) = ret_ty.syntax().next_sibling_or_token()
                && token.kind().is_trivia()
            {
                editor.delete(token);
            }
        }

        if let Some(tail) = parent_fn.body().unwrap().tail_expr() {
            editor.delete(tail.syntax().clone());
        }

        let edit = editor.finish();

        let expect = expect![["fn it() {\n    \n}"]];
        expect.assert_eq(&edit.new_root.to_string());
    }

    #[test]
    fn test_more_times_replace_node_to_same_token() {
        let arg_list =
            make::arg_list([make::expr_literal("1").into(), make::expr_literal("2").into()]);

        let (editor, arg_list) = SyntaxEditor::with_ast_node(&arg_list);

        let target_expr = make::token(parser::SyntaxKind::UNDERSCORE);

        for arg in arg_list.args() {
            editor.replace(arg.syntax(), &target_expr);
        }

        let edit = editor.finish();

        let expect = expect![["(_, _)"]];
        expect.assert_eq(&edit.new_root.to_string());
    }

    #[test]
    fn test_more_times_replace_node_to_same_node() {
        let arg_list =
            make::arg_list([make::expr_literal("1").into(), make::expr_literal("2").into()]);

        let (editor, arg_list) = SyntaxEditor::with_ast_node(&arg_list);

        let target_expr = make::expr_literal("3");

        for arg in arg_list.args() {
            editor.replace(arg.syntax(), target_expr.syntax());
        }

        let edit = editor.finish();

        let expect = expect![["(3, 3)"]];
        expect.assert_eq(&edit.new_root.to_string());
    }

    #[test]
    fn test_more_times_insert_node_to_same_node() {
        let arg_list =
            make::arg_list([make::expr_literal("1").into(), make::expr_literal("2").into()]);

        let (editor, arg_list) = SyntaxEditor::with_ast_node(&arg_list);

        let target_expr = make::ext::expr_unit();

        for arg in arg_list.args() {
            editor.insert(Position::before(arg.syntax()), target_expr.syntax());
        }

        let edit = editor.finish();

        let expect = expect![["(()1, ()2)"]];
        expect.assert_eq(&edit.new_root.to_string());
    }

    /// What `derive(CoercePointee)` does: delete an attribute inside a struct taken from a
    /// file, and add a where clause after its generics.
    #[test]
    fn test_derive_style_edit() {
        let file = ast::SourceFile::parse(
            "struct S<#[pointee] T: ?Sized, U> { a: Box<T>, b: U }\n",
            parser::Edition::CURRENT,
        );
        let strukt = file.tree().syntax().descendants().find_map(ast::Struct::cast).unwrap();
        let (editor, strukt) = SyntaxEditor::with_ast_node(&strukt);
        assert!(strukt.syntax().parent().is_none());
        let attr = strukt.syntax().descendants().find_map(ast::Attr::cast).unwrap();
        editor.delete(attr.syntax());
        let preds = ["T", "U"].map(|name| {
            let bound = make::type_bound_text("Clone");
            make::where_pred(either::Either::Right(make::ty(name)), [bound])
        });
        strukt.get_or_create_where_clause(&editor, preds.into_iter());
        let edit = editor.finish();

        let expect =
            expect![["struct S< T: ?Sized, U> where T: Clone, U: Clone { a: Box<T>, b: U }"]];
        expect.assert_eq(&edit.new_root.to_string());
        let new = ast::Struct::cast(edit.new_root().clone()).unwrap();
        assert!(new.syntax().descendants().all(|it| it.kind() != SyntaxKind::ATTR));
        assert_eq!(new.where_clause().unwrap().predicates().count(), 2);
        assert_eq!(new.where_clause().unwrap().syntax().parent().as_ref(), Some(new.syntax()));
        // The tree edited is as it was.
        assert_eq!(
            strukt.syntax().to_string(),
            "struct S<#[pointee] T: ?Sized, U> { a: Box<T>, b: U }"
        );
    }

    /// An element as the random edits see it: its kind and, for a token, its text.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Shape {
        Node(SyntaxKind, Vec<Shape>),
        Token(SyntaxKind, String),
    }

    fn shape(element: &SyntaxElement) -> Shape {
        match element {
            SyntaxElement::Node(node) => Shape::Node(
                node.kind(),
                node.children_with_tokens().map(|it| shape(&it)).collect(),
            ),
            SyntaxElement::Token(token) => Shape::Token(token.kind(), token.text().to_owned()),
        }
    }

    /// The child index of `element` and of each of its ancestors below the root, root first.
    fn path(element: &SyntaxElement) -> Vec<usize> {
        let mut path = vec![element.index()];
        path.extend(
            iter::successors(element.parent(), SyntaxNode::parent)
                .filter(|it| it.parent().is_some())
                .map(|it| it.index()),
        );
        path.reverse();
        path
    }

    /// The children of the node at `path` in `shape`.
    fn children_at<'a>(shape: &'a mut Shape, path: &[usize]) -> &'a mut Vec<Shape> {
        let mut children = match shape {
            Shape::Node(_, children) => children,
            Shape::Token(..) => unreachable!("a path goes through nodes"),
        };
        for &index in path {
            children = match &mut children[index] {
                Shape::Node(_, it) => it,
                Shape::Token(..) => unreachable!("a path goes through nodes"),
            };
        }
        children
    }

    /// A small deterministic generator for [`random_edits_of_real_files`].
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(path);
            }
        }
    }

    enum Edit {
        Delete,
        Replace(SyntaxElement),
        InsertAfter(SyntaxElement),
    }

    /// Random deletes, replacements and inserts in real files, full and outline parses, with
    /// elements of other files: the edited tree is the edited structure, its text is the edits
    /// applied to the text, and the tree edited is unchanged.
    #[test]
    fn random_edits_of_real_files() {
        let mut files = Vec::new();
        let project = test_utils::project_root().into_std_path_buf();
        collect_rs(&project.join("crates/syntax/src"), &mut files);
        files.sort();
        assert!(files.len() > 20, "the sources of syntax");
        let texts: Vec<String> =
            files.iter().map(|it| std::fs::read_to_string(it).unwrap()).collect();
        let edition = parser::Edition::CURRENT;
        let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for (number, text) in texts.iter().enumerate() {
            let donor = ast::SourceFile::parse(&texts[(number + 1) % texts.len()], edition);
            let donors: Vec<SyntaxElement> =
                donor.tree().syntax().descendants_with_tokens().collect();
            for outline in [false, true] {
                let root = match outline {
                    false => ast::SourceFile::parse(text, edition),
                    true => ast::SourceFile::parse_outline(Text::from(text.as_str()), edition),
                }
                .syntax_node();
                let before = shape(&root.clone().into());
                let elements: Vec<SyntaxElement> = root.descendants_with_tokens().skip(1).collect();
                // Targets apart from each other: none holds another or touches its ends.
                let mut targets = Vec::new();
                let mut last_end = None;
                for element in &elements {
                    let range = element.text_range();
                    if range.is_empty() || last_end.is_some_and(|end| range.start() <= end) {
                        continue;
                    }
                    if rng.index(elements.len()) < 8 {
                        let edit = match rng.below(3) {
                            0 => Edit::Delete,
                            1 => Edit::Replace(donors[rng.index(donors.len())].clone()),
                            _ => Edit::InsertAfter(donors[rng.index(donors.len())].clone()),
                        };
                        targets.push((element.clone(), edit));
                        last_end = Some(range.end());
                    }
                }

                let (editor, edited) = SyntaxEditor::new(root.clone());
                assert_eq!(edited, root);
                let mut expected = before.clone();
                let mut expected_text = text.clone();
                for (target, edit) in targets.iter().rev() {
                    let path = path(target);
                    let (index, parent) = path.split_last().unwrap();
                    let children = children_at(&mut expected, parent);
                    let range = Range::<usize>::from(target.text_range());
                    match edit {
                        Edit::Delete => {
                            editor.delete(target);
                            children.remove(*index);
                            expected_text.replace_range(range, "");
                        }
                        Edit::Replace(with) => {
                            editor.replace(target, with);
                            children[*index] = shape(with);
                            expected_text.replace_range(range, &with.to_string());
                        }
                        Edit::InsertAfter(with) => {
                            editor.insert(Position::after(target), with);
                            children.insert(index + 1, shape(with));
                            expected_text.insert_str(range.end, &with.to_string());
                        }
                    }
                }
                let edit = editor.finish();
                let new = edit.new_root();
                let at = format!("{}, outline {outline}", files[number].display());
                assert!(new.parent().is_none(), "{at}");
                assert_eq!(new.to_string(), expected_text, "{at}");
                assert!(shape(&new.clone().into()) == expected, "{at}: not the edited structure");
                let mut offset = 0;
                for token in new.descendants_with_tokens().filter_map(SyntaxElement::into_token) {
                    assert_eq!(usize::from(token.text_range().start()), offset, "{at}");
                    offset += token.text().len();
                }
                assert!(shape(&root.clone().into()) == before, "{at}: the tree edited changed");
                assert_eq!(edit.old_root(), &root, "{at}");
            }
        }
    }
}
