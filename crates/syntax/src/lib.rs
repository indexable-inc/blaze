//! Syntax Tree library used throughout the rust-analyzer.
//!
//! Properties:
//!   - easy and fast incremental re-parsing
//!   - graceful handling of errors
//!   - full-fidelity representation (*any* text can be precisely represented as
//!     a syntax tree)
//!
//! For more information, see the [RFC]. Current implementation is inspired by
//! the [Swift] one.
//!
//! The most interesting modules here are `syntax_node` (which defines concrete
//! syntax tree) and [`ast`] (which defines abstract syntax tree on top of the
//! CST). The actual parser live in a separate [`parser`] crate, though the
//! lexer lives in this crate.
//!
//! See `api_walkthrough` test in this file for a quick API tour!
//!
//! [RFC]: <https://github.com/rust-lang/rfcs/pull/2256>
//! [Swift]: <https://github.com/apple/swift/blob/13d593df6f359d0cb2fc81cfaac273297c539455/lib/Syntax/README.md>

#![deny(unsafe_code)]
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;
extern crate rustc_lexer;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

mod parsing;
mod ptr;
#[allow(unsafe_code)]
mod raw;
mod syntax_error;
mod syntax_node;
#[cfg(test)]
mod tests;
mod validation;

pub mod algo;
pub mod ast;
#[doc(hidden)]
pub mod fuzz;
pub mod hacks;
pub mod image;
pub mod syntax_editor;
pub mod utils;
pub mod view;

use std::{cell::RefCell, marker::PhantomData};

use blaze_mem::scratch::Scratch;

use stdx::format_to;
use triomphe::Arc;

pub use crate::{
    ast::{AstNode, AstToken},
    ptr::{AstPtr, SyntaxNodePtr},
    syntax_error::SyntaxError,
    syntax_node::{
        PreorderWithTokens, RustLanguage, SyntaxElement, SyntaxElementChildren, SyntaxNode,
        SyntaxNodeChildren, SyntaxToken, SyntaxTreeBuilder, Tree,
    },
};
pub use parser::{Edition, LexedStr, SyntaxKind, T};
pub use rustc_literal_escaper as unescape;
pub use smol_str::{SmolStr, SmolStrBuilder, ToSmolStr, format_smolstr};
pub use syntax_tree::{
    Direction, Expand, FlatTree, FlatTreeBuilder, IMAGE_LAYOUT, NodeOrToken, SyntaxText, Text,
    TextRange, TextSize, TokenAtOffset, WalkEvent, api::Preorder,
};

/// `Parse` is the result of the parsing: a syntax tree and a collection of
/// errors.
///
/// Note that we always produce a syntax tree, even for completely invalid
/// files.
#[derive(Debug, PartialEq, Eq)]
pub struct Parse<T> {
    tree: Option<Tree>,
    errors: Option<Arc<[SyntaxError]>>,
    /// The edition of an outline ([`SourceFile::parse_outline`]), in which its lazy bodies'
    /// errors are found when asked.
    outline: Option<Edition>,
    _ty: PhantomData<fn() -> T>,
}

/// What the derived `PartialEq` compares, without bounding the marker `T`: the tree's content
/// (never its address), the errors and the outline's edition.
impl<T> std::hash::Hash for Parse<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.tree.hash(state);
        self.errors.hash(state);
        self.outline.hash(state);
    }
}

impl<T> Clone for Parse<T> {
    fn clone(&self) -> Parse<T> {
        Parse {
            tree: self.tree.clone(),
            errors: self.errors.clone(),
            outline: self.outline,
            _ty: PhantomData,
        }
    }
}

impl<T> Parse<T> {
    fn new(tree: Tree, errors: Vec<SyntaxError>) -> Parse<T> {
        Parse::with_outline(tree, errors, None)
    }

    fn with_outline(tree: Tree, errors: Vec<SyntaxError>, outline: Option<Edition>) -> Parse<T> {
        Parse {
            tree: Some(tree),
            errors: if errors.is_empty() { None } else { Some(errors.into()) },
            outline,
            _ty: PhantomData,
        }
    }

    fn take(&mut self) -> Parse<SyntaxNode> {
        Parse {
            tree: self.tree.take(),
            errors: self.errors.take(),
            outline: self.outline,
            _ty: PhantomData,
        }
    }

    pub fn syntax_node(&self) -> SyntaxNode {
        self.tree.as_ref().unwrap().root()
    }

    pub fn downgrade(&self) -> WeakParse<T> {
        WeakParse {
            tree: std::sync::Arc::downgrade(self.tree.as_ref().unwrap().flat()),
            errors: self.errors.clone(),
            outline: self.outline,
            _ty: PhantomData,
        }
    }

    /// The errors of what is parsed. For a full parse, every error of the text. For an outline,
    /// those of the outline and of the lazy bodies parsed so far, whose parser errors are found
    /// by parsing each again (none is ever parsed here): which bodies that covers depends on
    /// what walked into them, so [`Parse::errors_with_bodies`] is the one to ask for all.
    pub fn errors(&self) -> Vec<SyntaxError> {
        self.errors_of(false)
    }

    /// Whether the parse stored errors, those its lexer and parser found: without the walk of
    /// every node [`Parse::errors`] makes to validate the tree, nor the errors of an outline's
    /// lazy bodies.
    pub fn has_parser_errors(&self) -> bool {
        self.errors.is_some()
    }

    /// [`Parse::errors`] at its exact length, `None` for none: what a memo keeps.
    pub fn errors_exact(&self) -> Option<Box<[SyntaxError]>> {
        self.exact_of(false)
    }

    /// [`Parse::errors_with_bodies`] at its exact length, `None` for none.
    pub fn errors_with_bodies_exact(&self) -> Option<Box<[SyntaxError]>> {
        self.exact_of(true)
    }

    fn errors_of(&self, all_bodies: bool) -> Vec<SyntaxError> {
        let mut errors = Vec::new(); // alloc: the caller's own list; memos use `errors_exact`
        self.errors_of_into(all_bodies, &mut errors);
        errors
    }

    fn exact_of(&self, all_bodies: bool) -> Option<Box<[SyntaxError]>> {
        // Taken, not borrowed: a body's parse inside validation then starts from an empty list.
        let mut kept = ERRORS.replace(Scratch::new());
        let errors = kept.take();
        self.errors_of_into(all_bodies, errors);
        // alloc: the result, at its exact length; the errors move, none is cloned
        let exact = (!errors.is_empty()).then(|| errors.drain(..).collect());
        ERRORS.set(kept);
        exact
    }

    fn errors_of_into(&self, all_bodies: bool, errors: &mut Vec<SyntaxError>) {
        if let Some(e) = self.errors.as_deref() {
            errors.extend_from_slice(e);
        }
        let root = self.root();
        if self.outline.is_none() {
            validation::validate(root, errors);
            return;
        }
        let tree = self.tree.as_ref().unwrap().flat();
        let mut kept = LAZIES.replace(Scratch::new());
        let lazies = kept.take();
        lazies.extend(tree.lazies().map(|at| (tree.range(at), at)));
        validation::validate_outline(
            root,
            &lazies[..],
            |at, errors| {
                if !all_bodies && !tree.is_parsed(at) {
                    return false;
                }
                errors.extend(parsing::body_errors(tree, at));
                true
            },
            errors,
        );
        LAZIES.set(kept);
    }
}

thread_local! {
    /// The lists a parse's errors are gathered in, kept at the largest they grew to.
    static ERRORS: RefCell<Scratch<SyntaxError>> = const { RefCell::new(Scratch::new()) };
    static LAZIES: RefCell<Scratch<(TextRange, u32)>> = const { RefCell::new(Scratch::new()) };
}

/// A [`Parse`] that does not keep its tree alive.
pub struct WeakParse<T> {
    tree: std::sync::Weak<FlatTree>,
    errors: Option<Arc<[SyntaxError]>>,
    outline: Option<Edition>,
    _ty: PhantomData<fn() -> T>,
}

impl<T> WeakParse<T> {
    /// The parse, while something else keeps its tree.
    pub fn upgrade(&self) -> Option<Parse<T>> {
        let tree = self.tree.upgrade()?;
        Some(Parse {
            tree: Some(Tree::new(tree)),
            errors: self.errors.clone(),
            outline: self.outline,
            _ty: PhantomData,
        })
    }
}

impl<T: AstNode> Parse<T> {
    /// Converts this parse result into a parse result for an untyped syntax tree.
    pub fn to_syntax(mut self) -> Parse<SyntaxNode> {
        self.take()
    }

    /// Gets the parsed syntax tree as a typed ast node.
    ///
    /// # Panics
    ///
    /// Panics if the root node cannot be casted into the typed ast node
    /// (e.g. if it's an `ERROR` node).
    pub fn tree(&self) -> T {
        T::cast(self.syntax_node()).unwrap()
    }

    /// Converts from `Parse<T>` to [`Result<T, Vec<SyntaxError>>`].
    pub fn ok(self) -> Result<T, Vec<SyntaxError>> {
        match self.errors() {
            errors if !errors.is_empty() => Err(errors),
            _ => Ok(self.tree()),
        }
    }
}

impl Parse<SyntaxNode> {
    pub fn cast<N: AstNode>(mut self) -> Option<Parse<N>> {
        if N::cast(self.syntax_node()).is_some() {
            Some(Parse {
                tree: self.tree.take(),
                errors: self.errors.take(),
                outline: self.outline,
                _ty: PhantomData,
            })
        } else {
            None
        }
    }
}

impl<T> Parse<T> {
    pub fn debug_dump(&self) -> String {
        let mut buf = format!("{:#?}", self.syntax_node());
        for err in self.errors() {
            format_to!(buf, "error {:?}: {}\n", err.range(), err);
        }
        buf
    }
}

impl Parse<SourceFile> {
    /// Every error of the file, bodies included: for an outline, its errors and those of every
    /// lazy body, each body parsed if it was not yet. They are the errors of a full parse of
    /// the text, in another order: each body's come where its walk reaches it.
    pub fn errors_with_bodies(&self) -> Vec<SyntaxError> {
        self.errors_of(true)
    }

    /// The parse of the text with `delete` replaced by `insert`: the smallest token, lazy
    /// body, block or item holding the edit parsed again alone and spliced into the tree, or
    /// the whole text when none can be (`parsing::reparsing` says when). An outline stays an
    /// outline.
    pub fn reparse(&self, delete: TextRange, insert: &str, edition: Edition) -> Parse<SourceFile> {
        self.incremental_reparse(delete, insert, edition)
            .unwrap_or_else(|| self.full_reparse(delete, insert, edition))
    }

    fn incremental_reparse(
        &self,
        delete: TextRange,
        insert: &str,
        edition: Edition,
    ) -> Option<Parse<SourceFile>> {
        // FIXME: validation errors are not handled here
        let (tree, errors, _reparsed_range) = parsing::incremental_reparse(
            self.tree.as_ref().unwrap().flat(),
            delete,
            insert,
            self.errors.as_deref().unwrap_or_default().iter().cloned(),
            edition,
            self.outline,
        )?;
        Some(Parse::with_outline(Tree::new(std::sync::Arc::new(tree)), errors, self.outline))
    }

    fn full_reparse(&self, delete: TextRange, insert: &str, edition: Edition) -> Parse<SourceFile> {
        let source = self.tree.as_ref().unwrap().flat().source();
        let text = raw::joined(&[
            &source[..usize::from(delete.start())],
            insert,
            &source[usize::from(delete.end())..],
        ]);
        match self.outline {
            Some(_) => SourceFile::parse_outline(text, edition),
            None => SourceFile::parse_shared(text, edition),
        }
    }

    /// The parse of `text`, made from this one: the bytes between the longest common prefix
    /// and suffix of this parse's text and `text` are the edit, parsed again as
    /// [`Parse::reparse`] parses one, and the tree shares `text`. The same parse as the one
    /// [`SourceFile::parse_shared`] (or, for an outline, [`SourceFile::parse_outline`]) makes
    /// of `text`; several edits since this parse are one edit covering them all. This parse is
    /// one made in `edition`, as for [`Parse::reparse`].
    pub fn reparse_to(&self, text: impl Into<Text>, edition: Edition) -> Parse<SourceFile> {
        let text = text.into();
        let tree = self.tree.as_ref().unwrap().flat();
        let same = tree.source().ptr_eq(&text) || **tree.source() == *text;
        if same && self.outline.is_none_or(|it| it == edition) {
            return self.clone();
        }
        let (delete, _insert) = parsing::edit_between(tree.source(), &text);
        self.reparse_onto(delete, text, edition)
    }

    /// The parse of `text`, which is this parse's text with `delete` replaced by the bytes of
    /// `text` at `delete.start()`: the edit parsed again as [`Parse::reparse`] parses one, and
    /// the tree shares `text`. The same parse a whole parse of `text` makes.
    pub fn reparse_onto(
        &self,
        delete: TextRange,
        text: impl Into<Text>,
        edition: Edition,
    ) -> Parse<SourceFile> {
        let text = text.into();
        let tree = self.tree.as_ref().unwrap().flat();
        let errors = self.errors.as_deref().unwrap_or_default().iter().cloned();
        match parsing::incremental_reparse_onto(tree, delete, &text, errors, edition, self.outline)
        {
            Some((tree, errors, _reparsed)) => {
                Parse::with_outline(Tree::new(std::sync::Arc::new(tree)), errors, self.outline)
            }
            None => match self.outline {
                Some(_) => SourceFile::parse_outline(text, edition),
                None => SourceFile::parse_shared(text, edition),
            },
        }
    }

    /// The parse of the text with each of `edits` made (`delete` replaced by `insert`; in order
    /// of start, none overlapping another, all in this parse's text): the new text made once,
    /// each edit's token, lazy body, block or item chosen and checked as [`Parse::reparse`]
    /// chooses and checks it, and the tree copied once with all of them in it. Edits whose
    /// checks read each other's text are parsed again as one. When one has no such element,
    /// the whole new text is parsed once, as [`Parse::reparse`] does for one edit. The same
    /// parse as [`Parse::reparse`] makes of them one by one.
    pub fn reparse_all(&self, edits: &[(TextRange, &str)], edition: Edition) -> Parse<SourceFile> {
        if edits.is_empty() && self.outline.is_none_or(|it| it == edition) {
            return self.clone();
        }
        let tree = self.tree.as_ref().unwrap().flat();
        let text = raw::spliced(tree.source(), edits);
        let errors = self.errors.as_deref().unwrap_or_default().iter().cloned();
        let reparsed = match edits.is_empty() {
            true => None,
            false => parsing::reparse_all(tree, edits, &text, errors, edition, self.outline),
        };
        match reparsed {
            Some((tree, errors)) => {
                Parse::with_outline(Tree::new(std::sync::Arc::new(tree)), errors, self.outline)
            }
            None => match self.outline {
                Some(_) => SourceFile::parse_outline(text, edition),
                None => SourceFile::parse_shared(text, edition),
            },
        }
    }

    /// Whether this is an outline ([`SourceFile::parse_outline`]).
    pub fn is_outline(&self) -> bool {
        self.outline.is_some()
    }
}

impl ast::Expr {
    /// Parses an `ast::Expr` from `text`.
    ///
    /// Note that if the parsed root node is not a valid expression, [`Parse::tree`] will panic.
    /// For example:
    /// ```rust,should_panic
    /// # use syntax::{ast, Edition};
    /// ast::Expr::parse("let fail = true;", Edition::CURRENT).tree();
    /// ```
    pub fn parse(text: &str, edition: Edition) -> Parse<ast::Expr> {
        let _p = tracing::info_span!("Expr::parse").entered();
        let (tree, errors) =
            parsing::parse_text_at(&text.into(), parser::TopEntryPoint::Expr, edition);
        let root = tree.root();

        assert!(
            ast::Expr::can_cast(root.kind()) || root.kind() == SyntaxKind::ERROR,
            "{:?} isn't an expression",
            root.kind()
        );
        Parse::new(tree, errors)
    }
}

impl ast::MacroStmts {
    /// Parses `text` as the statements a macro call in statement position expands to, as
    /// rustc retries an expansion that is no expression (`failed_to_match_macro`).
    pub fn parse(text: &str, edition: Edition) -> Parse<ast::MacroStmts> {
        let _p = tracing::info_span!("MacroStmts::parse").entered();
        let (tree, errors) =
            parsing::parse_text_at(&text.into(), parser::TopEntryPoint::MacroStmts, edition);
        assert_eq!(tree.root().kind(), SyntaxKind::MACRO_STMTS);
        Parse::new(tree, errors)
    }
}

#[cfg(not(no_async_drops))]
impl<T> Drop for Parse<T> {
    fn drop(&mut self) {
        // A count that is not the tree's last frees nothing: it is dropped here, not sent.
        let Some(tree) = self.tree.take().and_then(Tree::into_unique) else {
            return;
        };
        static PARSE_DROP_THREAD: std::sync::OnceLock<crossbeam_channel::Sender<FlatTree>> =
            std::sync::OnceLock::new();
        PARSE_DROP_THREAD
            .get_or_init(|| {
                let (sender, receiver) = crossbeam_channel::unbounded::<FlatTree>();
                std::thread::Builder::new()
                    .name("ParseNodeDropper".to_owned())
                    .spawn(move || {
                        loop {
                            // block on a receive
                            _ = receiver.recv();
                            // then drain the entire channel
                            while receiver.try_recv().is_ok() {}
                            // and sleep for a bit
                            #[allow(clippy::disallowed_methods, reason = "a thread of its own that batches drops: no job waits on it")]
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        // why do this over just a `receiver.iter().for_each(drop)`? To reduce contention on the channel lock.
                        // otherwise this thread will constantly wake up and sleep again.
                    })
                    .unwrap();
                sender
            })
            .send(tree)
            .unwrap();
    }
}

/// `SourceFile` represents a parse tree for a single Rust file.
pub use crate::ast::SourceFile;

impl SourceFile {
    /// A full parse of `text`, over a shared copy of it.
    pub fn parse(text: &str, edition: Edition) -> Parse<SourceFile> {
        SourceFile::parse_shared(Text::from(text), edition)
    }

    /// A full parse of `text`, whose tree shares it: the parse costs the tree's arrays only.
    pub fn parse_shared(text: impl Into<Text>, edition: Edition) -> Parse<SourceFile> {
        let _p = tracing::info_span!("SourceFile::parse").entered();
        let (tree, errors) = parsing::parse_text(&text.into(), edition, false);
        let root = tree.root();

        assert_eq!(root.kind(), SyntaxKind::SOURCE_FILE);
        Parse::new(tree, errors)
    }

    /// An outline of `text`, for the files of dependency crates (#1294): every `fn` item's body
    /// block (at any depth outside another body) a lazy `BLOCK_EXPR` node, parsed the first
    /// time a cursor walks into it, to exactly the tree a full parse makes there. A body whose
    /// tokens could parse otherwise alone (`parser::lazy_block_len`) is parsed in full here.
    /// Its errors: [`Parse::errors`], [`Parse::errors_with_bodies`].
    pub fn parse_outline(text: impl Into<Text>, edition: Edition) -> Parse<SourceFile> {
        let _p = tracing::info_span!("SourceFile::parse_outline").entered();
        let (tree, errors) = parsing::parse_text(&text.into(), edition, true);
        let root = tree.root();

        assert_eq!(root.kind(), SyntaxKind::SOURCE_FILE);
        Parse::with_outline(tree, errors, Some(edition))
    }
}

/// Matches a `SyntaxNode` against an `ast` type.
///
/// # Example:
///
/// ```ignore
/// match_ast! {
///     match node {
///         ast::CallExpr(it) => { ... },
///         ast::MethodCallExpr(it) => { ... },
///         ast::MacroCall(it) => { ... },
///         _ => None,
///     }
/// }
/// ```
#[macro_export]
macro_rules! match_ast {
    (match $node:ident { $($tt:tt)* }) => { $crate::match_ast!(match ($node) { $($tt)* }) };

    (match ($node:expr) {
        $( $( $path:ident )::+ ($it:pat) $(if $guard:expr)? => $res:expr, )*
        _ => $catch_all:expr $(,)?
    }) => {{
        #[allow(clippy::question_mark, reason = "if `$catch_all` is `return None` Clippy can mark this")]
        {
            $( if let Some($it) = $($path::)+cast($node.clone()) $(&& $guard)? { $res } else )*
            { $catch_all }
        }
    }};
}

/// This test does not assert anything and instead just shows off the crate's
/// API.
#[test]
fn api_walkthrough() {
    use ast::{HasModuleItem, HasName};

    let source_code = "
        fn foo() {
            1 + 1
        }
    ";
    // `SourceFile` is the main entry point.
    //
    // The `parse` method returns a `Parse` -- a pair of syntax tree and a list
    // of errors. That is, syntax tree is constructed even in presence of errors.
    let parse = SourceFile::parse(source_code, parser::Edition::CURRENT);
    assert!(parse.errors().is_empty());

    // The `tree` method returns an owned syntax node of type `SourceFile`.
    // Owned nodes are cheap: inside, they are `Rc` handles to the underlying data.
    let file: SourceFile = parse.tree();

    // `SourceFile` is the root of the syntax tree. We can iterate file's items.
    // Let's fetch the `foo` function.
    let mut func = None;
    for item in file.items() {
        match item {
            ast::Item::Fn(f) => func = Some(f),
            _ => unreachable!(),
        }
    }
    let func: ast::Fn = func.unwrap();

    // Each AST node has a bunch of getters for children. All getters return
    // `Option`s though, to account for incomplete code. Some getters are common
    // for several kinds of node. In this case, a trait like `ast::NameOwner`
    // usually exists. By convention, all ast types should be used with `ast::`
    // qualifier.
    let name: Option<ast::Name> = func.name();
    let name = name.unwrap();
    assert_eq!(name.text(), "foo");

    // Let's get the `1 + 1` expression!
    let body: ast::BlockExpr = func.body().unwrap();
    let stmt_list: ast::StmtList = body.stmt_list().unwrap();
    let expr: ast::Expr = stmt_list.tail_expr().unwrap();

    // Enums are used to group related ast nodes together, and can be used for
    // matching. However, because there are no public fields, it's possible to
    // match only the top level enum: that is the price we pay for increased API
    // flexibility
    let bin_expr: &ast::BinExpr = match &expr {
        ast::Expr::BinExpr(e) => e,
        _ => unreachable!(),
    };

    // Besides the "typed" AST API, there's an untyped CST one as well.
    // To switch from AST to CST, call `.syntax()` method:
    let expr_syntax: &SyntaxNode = expr.syntax();

    // Note how `expr` and `bin_expr` are in fact the same node underneath:
    assert!(expr_syntax == bin_expr.syntax());

    // To go from CST to AST, `AstNode::cast` function is used:
    let _expr: ast::Expr = match ast::Expr::cast(expr_syntax.clone()) {
        Some(e) => e,
        None => unreachable!(),
    };

    // The two properties each syntax node has is a `SyntaxKind`:
    assert_eq!(expr_syntax.kind(), SyntaxKind::BIN_EXPR);

    // And text range:
    assert_eq!(expr_syntax.text_range(), TextRange::new(32.into(), 37.into()));

    // You can get node's text as a `SyntaxText` object, which will traverse the
    // tree collecting token's text:
    let text: SyntaxText = expr_syntax.text();
    assert_eq!(text.to_string(), "1 + 1");

    // There's a bunch of traversal methods on `SyntaxNode`:
    assert_eq!(expr_syntax.parent().as_ref(), Some(stmt_list.syntax()));
    assert_eq!(stmt_list.syntax().first_child_or_token().map(|it| it.kind()), Some(T!['{']));
    assert_eq!(
        expr_syntax.next_sibling_or_token().map(|it| it.kind()),
        Some(SyntaxKind::WHITESPACE)
    );

    // As well as some iterator helpers:
    let f = expr_syntax.ancestors().find_map(ast::Fn::cast);
    assert_eq!(f, Some(func));
    assert!(expr_syntax.siblings_with_tokens(Direction::Next).any(|it| it.kind() == T!['}']));
    assert_eq!(
        expr_syntax.descendants_with_tokens().count(),
        8, // 5 tokens `1`, ` `, `+`, ` `, `1`
           // 2 child literal expressions: `1`, `1`
           // 1 the node itself: `1 + 1`
    );

    // There's also a `preorder` method with a more fine-grained iteration control:
    let mut buf = String::new();
    let mut indent = 0;
    for event in expr_syntax.preorder_with_tokens() {
        match event {
            WalkEvent::Enter(node) => {
                let text = match &node {
                    NodeOrToken::Node(it) => it.text().to_string(),
                    NodeOrToken::Token(it) => it.text().to_owned(),
                };
                format_to!(buf, "{:indent$}{:?} {:?}\n", " ", text, node.kind(), indent = indent);
                indent += 2;
            }
            WalkEvent::Leave(_) => indent -= 2,
        }
    }
    assert_eq!(indent, 0);
    assert_eq!(
        buf.trim(),
        r#"
"1 + 1" BIN_EXPR
  "1" LITERAL
    "1" INT_NUMBER
  " " WHITESPACE
  "+" PLUS
  " " WHITESPACE
  "1" LITERAL
    "1" INT_NUMBER
"#
        .trim()
    );

    // To recursively process the tree, there are three approaches:
    // 1. explicitly call getter methods on AST nodes.
    // 2. use descendants and `AstNode::cast`.
    // 3. use descendants and `match_ast!`.
    //
    // Here's how the first one looks like:
    let exprs_cast: Vec<String> = file
        .syntax()
        .descendants()
        .filter_map(ast::Expr::cast)
        .map(|expr| expr.syntax().text().to_string())
        .collect();

    // An alternative is to use a macro.
    let mut exprs_visit = Vec::new();
    for node in file.syntax().descendants() {
        match_ast! {
            match node {
                ast::Expr(it) => {
                    let res = it.syntax().text().to_string();
                    exprs_visit.push(res);
                },
                _ => (),
            }
        }
    }
    assert_eq!(exprs_cast, exprs_visit);
}
