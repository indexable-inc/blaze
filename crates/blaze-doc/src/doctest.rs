//! A library's doctests, built by blaze with no rustdoc (docs/v2/plan/design/doctests.md): every
//! code block of its documentation that is Rust is found as rustdoc finds it ([`examples`])
//! and turned into the program rustdoc makes of it ([`Program::of`]), one crate per example.
//! blaze never merges examples into one crate, in any edition: each is its own unit, built,
//! run in a process of its own and patched on an edit as any test of `blaze test` is.
//!
//! The rules are rustdoc's (`librustdoc/doctest.rs`, `doctest/make.rs` and
//! `html/markdown.rs`, rust-lang/rust master of 2026-11-02, the pinned nightly's line):
//! hidden `# ` lines are code, the example's inner attributes and `extern crate`s go before
//! the rest, an `extern crate` of the library is added when the example's text names it,
//! and the rest goes into a `fn main` unless the example has one and nothing but items.

use std::fmt::Write as _;

use base_db::Db;
use hir::{Adt, AssocItem, HasAttrs, Module, ModuleDef, view::HasSourceView as _};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use syntax::{
    Edition, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasName as _},
};

use crate::markdown::{LangString, tested_line};

/// One code block of the documentation that is Rust.
#[derive(Debug, Clone)]
pub struct Example {
    /// The path of the documented item in its crate: empty for the crate itself.
    pub item: String,
    /// The file the documentation is written in, as its source root has it; empty when it
    /// is in none.
    pub file: String,
    /// The line the block's fence is on, from 1, as rustdoc counts it.
    pub line: usize,
    pub(crate) lang: LangString,
    /// The code with its hidden lines as code.
    pub code: String,
    /// What `#[doc(test(attr(..)))]` asks of it: those of the crate's root, then of each item
    /// on the path down to the documented one, its own last (rustdoc's `global_crate_attrs`).
    pub(crate) attrs: Vec<String>,
}

/// The examples of `krate`'s documentation, in the order of a walk of its modules as rustdoc
/// walks them; one an item gets twice (a macro exported from where it is written) once. An
/// item with `#[target_feature]`s the crate's target lacks (its `cfg(target_feature)`s) is
/// skipped with every item inside it, as rustdoc skips it.
pub fn examples(db: &Db, krate: hir::Crate) -> Vec<Example> {
    // alloc: once a doctest crate, the target's features
    let features = krate.cfg(db).get_cfg_values("target_feature").cloned().collect();
    // alloc: once a doctest crate, the examples found and the attributes on the walk's path
    let mut found =
        Found { db, examples: Vec::new(), seen: Vec::new(), attrs: Vec::new(), features };
    found.module(krate.root_module(db), "");
    found.examples
}

struct Found<'db> {
    db: &'db Db,
    examples: Vec<Example>,
    /// Where each block found is, with its item.
    seen: Vec<(String, usize, String)>,
    /// The `doc(test(attr(..)))`s of the items on the path to the one being walked.
    attrs: Vec<String>,
    /// The `target_feature`s the crate's target has.
    features: Vec<intern::Symbol>,
}

fn join(path: &str, name: &str) -> String {
    // alloc: an item's path, once per item documented
    if path.is_empty() { name.to_owned() } else { format!("{path}::{name}") }
}

impl Found<'_> {
    fn module(&mut self, module: Module, path: &str) {
        let db = self.db;
        let Some(mark) = self.enter(module, path) else { return };
        for def in module.declarations(db) {
            let named = |name: hir::Name| join(path, name.as_str());
            match def {
                ModuleDef::Module(it) => {
                    if let Some(name) = it.name(db) {
                        self.module(it, &named(name));
                    }
                }
                ModuleDef::Function(it) => self.documented(it, &named(it.name(db))),
                ModuleDef::Adt(it) => self.adt(it, &named(it.name(db))),
                // One without a name is `_` to rustdoc.
                ModuleDef::Const(it) => match it.name(db) {
                    Some(name) => self.documented(it, &named(name)),
                    None => self.documented(it, &join(path, "_")),
                },
                ModuleDef::Static(it) => self.documented(it, &named(it.name(db))),
                ModuleDef::TypeAlias(it) => self.documented(it, &named(it.name(db))),
                ModuleDef::Trait(it) => {
                    let path = named(it.name(db));
                    if let Some(mark) = self.enter(it, &path) {
                        self.items(it.items(db), &path);
                        self.attrs.truncate(mark);
                    }
                }
                // A `macro_rules!` is told of where it is written, below; the examples of a
                // procedural one are those of its function.
                ModuleDef::Macro(it) if !is_macro_rules(db, it) && !it.is_proc_macro() => {
                    self.documented(it, &named(it.name(db)))
                }
                _ => {}
            }
        }
        // A `macro_rules!` is in every module after it: its documentation is where it is
        // written.
        for it in module.legacy_macros(db).into_iter().filter(|it| it.module(db) == module) {
            self.documented(it, &join(path, it.name(db).as_str()));
        }
        for it in module.extern_blocks(db) {
            self.documented(it, path);
        }
        for it in module.use_decls(db) {
            self.documented(it, path);
        }
        for it in module.extern_crate_decls(db) {
            let name = it.alias_or_name(db);
            self.documented(it, &join(path, name.as_ref().map_or("_", |it| it.as_str())));
        }
        for it in module.impl_defs(db) {
            let Some(source) = it.source_view(db) else { continue };
            let Some(ty) = source.value.self_ty() else { continue };
            let path = join(path, &shown_type(ty));
            if let Some(mark) = self.enter(it, &path) {
                self.items(it.items(db), &path);
                self.attrs.truncate(mark);
            }
        }
        self.attrs.truncate(mark);
    }

    fn items(&mut self, items: Vec<AssocItem>, path: &str) {
        for item in items {
            if let Some(name) = item.name(self.db) {
                self.documented(item, &join(path, name.as_str()));
            }
        }
    }

    fn adt(&mut self, adt: Adt, path: &str) {
        let db = self.db;
        let Some(mark) = self.enter(adt, path) else { return };
        let fields = match adt {
            Adt::Struct(it) => it.fields(db),
            Adt::Union(it) => it.fields(db),
            Adt::Enum(it) => {
                for variant in it.variants(db) {
                    let path = join(path, variant.name(db).as_str());
                    let Some(mark) = self.enter(variant, &path) else { continue };
                    for field in variant.fields(db) {
                        self.documented(field, &join(&path, field.name(db).as_str()));
                    }
                    self.attrs.truncate(mark);
                }
                Vec::new()
            }
        };
        for field in fields {
            self.documented(field, &join(path, field.name(db).as_str()));
        }
        self.attrs.truncate(mark);
    }

    /// Takes the examples out of the documentation of `item`, an item with nothing walked
    /// inside it.
    fn documented(&mut self, item: impl HasAttrs + Copy, path: &str) {
        if let Some(mark) = self.enter(item, path) {
            self.attrs.truncate(mark);
        }
    }

    /// Enters `item` as rustdoc's `visit_testable` does: adds its `doc(test(attr(..)))`s to
    /// those of the path and takes the examples out of its documentation. `None` when the
    /// item has a `#[target_feature]` the target lacks, which rustdoc skips with everything
    /// inside it; else how many attributes the path had before it, to truncate back to once
    /// the items inside it are walked.
    fn enter(&mut self, item: impl HasAttrs + Copy, path: &str) -> Option<usize> {
        use hir::AttrsOwner;
        use hir_def::{AttrDefId, attrs::AttrFlags};
        let db = self.db;
        let mark = self.attrs.len();
        let owner = match item.attr_id(db) {
            AttrsOwner::AttrDef(it) => Some(either::Either::Left(it)),
            AttrsOwner::Field(it) => Some(either::Either::Right(it)),
            _ => None,
        };
        if let Some(owner) = owner {
            if let either::Either::Left(AttrDefId::FunctionId(function)) = owner {
                let wanted = AttrFlags::target_feature_list(db, function);
                if !wanted.iter().all(|it| self.features.contains(it)) {
                    return None;
                }
            }
            self.attrs.extend(AttrFlags::doc_test_attrs(db, owner));
        }
        self.take_examples(item, path);
        Some(mark)
    }

    /// Takes the examples out of the documentation of `item`, when it has any.
    fn take_examples(&mut self, item: impl HasAttrs, path: &str) {
        let db = self.db;
        let Some(docs) = item.hir_docs(db) else { return };
        let text = docs.docs();
        let mut start = None;
        for block in blocks(text) {
            let lang = match &block.lang {
                Some(info) => LangString::parse(info),
                None => LangString { rust: true, ..LangString::default() },
            };
            if !lang.rust {
                continue;
            }
            // clone: where the documentation starts, once per example
            let start = start.get_or_insert_with(|| place(db, docs, 0)).clone();
            // The line of the fence where the text maps to one; else counted from where the
            // documentation starts, as rustdoc counts when it does not find the fence.
            let (file, line) = match place(db, docs, block.start) {
                Some(it) => it,
                None => match start {
                    Some((file, first)) => {
                        (file, first + text[..block.start].matches('\n').count())
                    }
                    // alloc: an example with no file, once
                    None => (String::new(), 1 + text[..block.start].matches('\n').count()),
                },
            };
            // clone: the key of an example, once per example
            let key = (file.clone(), line, path.to_owned());
            if self.seen.contains(&key) {
                continue;
            }
            self.seen.push(key);
            let code = block.code.lines().map(tested_line).collect::<Vec<_>>().join("\n");
            // clone: the path's test attributes, once per example
            let attrs = self.attrs.clone();
            self.examples.push(Example { item: path.to_owned(), file, line, lang, code, attrs });
        }
    }
}

/// The file and line, from 1, the character at `at` of the documentation `docs` is written
/// at; `None` for one a macro wrote.
fn place(db: &Db, docs: &hir::Docs, at: usize) -> Option<(String, usize)> {
    let range = TextRange::at(TextSize::new(at as u32), TextSize::new(1));
    let (range, _) = docs.find_ast_range(range)?;
    let written = range.original_node_file_range_rooted(db);
    let file = written.file_id.file_id(db);
    let path = crate::source::path_of(db, file)?;
    let text = db.file_text(file).text(db);
    let offset = usize::from(written.range.start()).min(text.len());
    Some((path, 1 + text[..offset].matches('\n').count()))
}

fn is_macro_rules(db: &Db, it: hir::Macro) -> bool {
    let source = it.source_view(db).and_then(|it| it.value.left());
    matches!(source, Some(ast::Macro::MacroRules(_)))
}

/// The type an `impl` is of as rustdoc names it: as the compiler writes it out, where a
/// reference has its lifetime even when none is written, and with no spaces.
fn shown_type(ty: ast::Type<'_>) -> String {
    // alloc: once an impl documented, its name
    let mut shown = String::new();
    for token in ty.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
        if token.kind().is_trivia() {
            continue;
        }
        shown.push_str(token.text());
        let reference = token.parent().and_then(ast::RefType::cast);
        if token.kind() == SyntaxKind::AMP && reference.is_some_and(|it| it.lifetime().is_none()) {
            shown.push_str("'_");
        }
    }
    shown
}

/// A code block of a Markdown text.
struct Block {
    /// Where in the text it starts.
    start: usize,
    /// What follows the fence that opens it: `None` for an indented block.
    lang: Option<String>,
    code: String,
}

fn blocks(markdown: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION;
    // alloc: once a documentation with a code block, its blocks
    let mut blocks = Vec::new();
    let mut open: Option<Block> = None;
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => Some(lang.to_string()),
                    CodeBlockKind::Indented => None,
                };
                open = Some(Block { start: range.start, lang, code: String::new() });
            }
            Event::Text(text) => {
                if let Some(block) = &mut open {
                    block.code.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) => blocks.extend(open.take()),
            _ => {}
        }
    }
    blocks
}

/// What `#![doc(test(..))]` of the crate's root asks of every example besides its attributes,
/// which [`Example::attrs`] has with those of the items on its path.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OfTheCrate {
    /// `no_crate_inject`: no `extern crate` of the library is added.
    pub(crate) no_crate_inject: bool,
}

impl OfTheCrate {
    pub(crate) fn read(root: &str) -> OfTheCrate {
        use syntax::NodeOrToken;
        fn before(node: ast::Node<'_>) -> Option<&str> {
            let mut prev = node.prev_sibling_or_token();
            while prev.is_some_and(|it| it.kind().is_trivia()) {
                prev = prev.and_then(|it| it.prev_sibling_or_token());
            }
            prev.map(|it| match it {
                NodeOrToken::Node(it) => it.text(),
                NodeOrToken::Token(it) => it.text(),
            })
        }
        let parse = syntax::SourceFile::parse(root, Edition::CURRENT);
        let mut of_the_crate = OfTheCrate::default();
        // `test(..)` directly inside `doc(..)` of an inner attribute of the file itself.
        let inner = parse
            .root()
            .children()
            .filter_map(ast::Attr::cast)
            .filter(|it| it.excl_token().is_some());
        for attr in inner {
            let trees =
                attr.syntax().descendants().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
            for tree in trees {
                let outer = tree.parent().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
                let in_doc = outer.is_some_and(|it| before(it) == Some("doc"));
                if before(tree) != Some("test") || !in_doc {
                    continue;
                }
                let mut words = tree.children_with_tokens().filter_map(|it| it.into_token());
                of_the_crate.no_crate_inject |= words.any(|it| it.text() == "no_crate_inject");
            }
        }
        of_the_crate
    }
}

/// An example taken apart as rustdoc takes it apart (`make.rs`'s `parse_source`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Parts {
    /// Its inner attributes that are not lints (`#![feature]`, `#![no_std]`), or
    /// `allow(internal_features)`.
    pub(crate) crate_attrs: String,
    /// Its inner attributes that are lints (`allow`, `warn`, `deny`, `forbid`, `expect`).
    pub(crate) module_attrs: String,
    /// Its `extern crate`s, and what is between its attributes and its first statement.
    pub(crate) crates: String,
    pub(crate) everything_else: String,
    /// Whether it has a `fn main` (or a macro call with `fn main(..)` among its tokens) and
    /// nothing but items.
    pub(crate) has_main: bool,
    /// Whether one of its `extern crate`s is of the library.
    pub(crate) has_extern_crate: bool,
    /// Whether it does not parse: it is compiled as it is written, for the compiler to refuse.
    pub(crate) invalid: bool,
    /// Where in the example `everything_else` starts, past its leading whitespace: the line
    /// its program is aligned to ([`source`]).
    pub(crate) rest_at: usize,
}

const WRAP: &str = "fn f(){";

/// The lints an inner attribute may name and stay the example's, not its crate's.
const LINTS: [&str; 5] = ["forbid", "allow", "warn", "deny", "expect"];

impl Parts {
    pub(crate) fn of(code: &str, crate_name: &str, edition: Edition) -> Parts {
        // Statements and inner attributes are read inside a function, as rustdoc reads them.
        let wrapped = format!("{WRAP}{code}\n}}");
        let parse = syntax::SourceFile::parse(&wrapped, edition);
        let whole = TextSize::of(&*wrapped);
        let body = parse
            .root()
            .children()
            .find_map(ast::Fn::cast)
            .filter(|it| it.syntax().text_range().end() == whole)
            .and_then(|it| it.body())
            .and_then(|it| it.stmt_list());
        let invalid =
            || Parts { everything_else: code.to_owned(), invalid: true, ..Parts::default() };
        let Some(body) = body.filter(|_| parse.errors().is_empty()) else { return invalid() };
        let mut parts = Parts::default();
        let mut taken = 0;
        let mut non_items = false;
        let at = |offset: TextSize| (usize::from(offset) - WRAP.len()).min(code.len());
        let mut rest_from = None;
        for child in body.syntax().children_with_tokens() {
            // A `;` alone is an empty statement, which is not an item: rustdoc keeps it with
            // the rest of the example, the last one too.
            let node = match child {
                syntax::NodeOrToken::Token(it) if it.kind() == SyntaxKind::SEMICOLON => {
                    non_items = true;
                    let (start, end) = (at(it.text_range().start()), at(it.text_range().end()));
                    if parts.everything_else.is_empty()
                        && (!parts.module_attrs.is_empty() || !parts.crate_attrs.is_empty())
                    {
                        parts.crates.push_str(&code[taken..start]);
                        taken = start;
                    }
                    rest_from.get_or_insert(taken);
                    parts.everything_else.push_str(&code[taken..end]);
                    taken = end;
                    continue;
                }
                syntax::NodeOrToken::Token(_) => continue,
                syntax::NodeOrToken::Node(it) => it,
            };
            let (start, end) = (at(node.text_range().start()), at(node.text_range().end()));
            if let Some(attr) = ast::Attr::cast(node).filter(|it| it.excl_token().is_some()) {
                let path = attr.path().map(|it| it.syntax().text());
                let lint = path.is_some_and(|it| LINTS.contains(&it));
                // `allow(internal_features)` is its crate's, as rustdoc has it.
                let internal = path == Some("allow") && {
                    let mut words =
                        attr.syntax().descendants_with_tokens().filter_map(|it| it.into_token());
                    words.any(|it| matches!(it.text(), "internal_features" | "incomplete_features"))
                };
                let into = if lint && !internal {
                    &mut parts.module_attrs
                } else {
                    &mut parts.crate_attrs
                };
                into.push_str(&code[taken..end]);
                taken = end;
                continue;
            }
            if ast::Attr::can_cast(node.kind()) {
                continue;
            }
            match statement(node) {
                Statement::Item => {
                    parts.has_main |= ast::Fn::cast(node)
                        .is_some_and(|it| it.name().is_some_and(|it| it.text() == "main"));
                }
                Statement::MacroCall(call) => {
                    parts.has_main |= !parts.has_main && writes_main(call)
                }
                Statement::Other => non_items = true,
            }
            if parts.everything_else.is_empty()
                && (!parts.module_attrs.is_empty() || !parts.crate_attrs.is_empty())
            {
                parts.crates.push_str(&code[taken..start]);
                taken = start;
            }
            let text = &code[taken..end];
            taken = end;
            match ast::ExternCrate::cast(node) {
                Some(it) => {
                    parts.has_extern_crate |= it
                        .name_ref()
                        .is_some_and(|name| name.text().trim_start_matches("r#") == crate_name);
                    parts.crates.push_str(text);
                }
                None => {
                    rest_from.get_or_insert(taken - text.len());
                    parts.everything_else.push_str(text)
                }
            }
        }
        // rustdoc wraps all of an example with a statement that is not an item, `main` or not.
        parts.has_main &= !non_items;
        let leading = parts.everything_else.len() - parts.everything_else.trim_start().len();
        parts.rest_at = rest_from.map_or(code.len(), |it| it + leading);
        parts
    }
}

/// A statement of an example as rustc's parser has it.
enum Statement<'a> {
    Item,
    /// A macro call statement (`StmtKind::MacCall`): one in braces, or one with its `;`.
    MacroCall(ast::MacroCall<'a>),
    /// A `let`, an expression, a macro call in parentheses or brackets with no `;` (rustc's
    /// `parse_stmt_mac` makes that an expression statement, `StmtKind::Expr`).
    Other,
}

fn statement(node: ast::Node<'_>) -> Statement<'_> {
    let (expr, semicolon) = match node.kind() {
        SyntaxKind::LET_STMT => return Statement::Other,
        SyntaxKind::EXPR_STMT => match ast::ExprStmt::cast(node) {
            Some(it) => (it.expr(), it.semicolon_token().is_some()),
            None => (None, false),
        },
        _ => match ast::Expr::cast(node) {
            Some(it) => (Some(it), false),
            None => (None, false),
        },
    };
    let call = match expr {
        Some(ast::Expr::MacroExpr(it)) => it.macro_call(),
        Some(_) => return Statement::Other,
        None if node.kind() == SyntaxKind::MACRO_CALL => ast::MacroCall::cast(node),
        None => return Statement::Item,
    };
    let Some(call) = call else { return Statement::Other };
    let braces = call.token_tree().and_then(|it| it.l_curly_token()).is_some();
    let semicolon = semicolon || call.semicolon_token().is_some();
    match braces || semicolon {
        true => Statement::MacroCall(call),
        false => Statement::Other,
    }
}

/// Whether the tokens a macro is given have `fn main` then a group in parentheses among
/// their outermost ones: rustdoc's guess at a `main` a macro writes (rust-lang/rust#56898).
fn writes_main(call: ast::MacroCall<'_>) -> bool {
    let Some(tree) = call.token_tree() else { return false };
    let mut children = tree
        .syntax()
        .children_with_tokens()
        .filter(|it| !it.kind().is_trivia())
        .collect::<Vec<_>>();
    // The group's own delimiters.
    if children.len() >= 2 {
        children.remove(0);
        children.pop();
    }
    children.windows(3).any(|it| {
        let word = |at: usize| it[at].as_token().map(|it| it.text());
        let parens = it[2].as_node().is_some_and(|it| {
            it.kind() == SyntaxKind::TOKEN_TREE
                && it.first_token().is_some_and(|it| it.kind() == SyntaxKind::L_PAREN)
        });
        word(0) == Some("fn") && word(1) == Some("main") && parens
    })
}

/// The program of one example, as rustdoc writes it to compile it alone
/// (`generate_unique_doctest`), with what its test is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// The name libtest shows, rustdoc's `src/lib.rs - item::path (line 12)`.
    pub name: String,
    /// rustdoc's id of it: the file's name with every character not a letter or digit `_`,
    /// its line and its number among those of the file and line.
    pub id: String,
    /// The file the example is in, from the package's directory: what its program's file is
    /// named, for its errors and panics to point into the documentation.
    pub file: String,
    /// The line of the example's fence in `file`.
    pub line: usize,
    /// Its program, each line of the example's rest on the line rustdoc shows it at
    /// ([`Pieces::aligned`]).
    pub source: String,
    /// rustdoc's `full_test_line_offset`, how many lines it says it writes before the example's.
    pub line_offset: usize,
    pub edition: Edition,
    pub ignore: bool,
    pub no_run: bool,
    pub should_panic: bool,
    pub compile_fail: bool,
    /// The error codes a `compile_fail` example must fail with.
    pub error_codes: Vec<String>,
    /// Whether its own `#[test]`s run, built with `--test`, and it has no `main` added.
    pub test_harness: bool,
}

/// What the crate's root asks of every example, and its name and edition.
pub struct OfLibrary {
    pub(crate) name: String,
    pub(crate) edition: Edition,
    pub(crate) of_the_crate: OfTheCrate,
}

impl OfLibrary {
    /// Refuses a crate with no name: its examples name it by it (`use app::f;`).
    pub fn read(db: &Db, krate: hir::Crate) -> Result<OfLibrary, base_db::UnnamedCrate> {
        let root = db.file_text(krate.root_file(db));
        Ok(OfLibrary {
            name: crate::crate_name(db, krate)?,
            edition: krate.edition(db),
            of_the_crate: OfTheCrate::read(root.text(db)),
        })
    }
}

/// The programs of `examples`, built for `triple`, the library being `of`, its package at
/// `package_dir`, which the names of the files are from.
pub fn programs(
    of: &OfLibrary,
    examples: &[Example],
    triple: &str,
    package_dir: &str,
) -> Vec<Program> {
    // alloc: once a library's doctests, the number of each file and line
    let mut seen: Vec<(String, usize, usize)> = Vec::new();
    examples
        .iter()
        .map(|example| {
            // alloc: an example's file name, once per example
            let file = shown_file(&example.file, package_dir).to_owned();
            let flat: String =
                file.chars().map(|it| if it.is_ascii_alphanumeric() { it } else { '_' }).collect();
            let number =
                match seen.iter_mut().find(|(it, line, _)| *it == flat && *line == example.line) {
                    Some((_, _, number)) => {
                        *number += 1;
                        *number
                    }
                    None => {
                        // clone: the key of a file and line, once each
                        seen.push((flat.clone(), example.line, 0));
                        0
                    }
                };
            let id = format!("{flat}_{}_{number}", example.line);
            Program::of(of, example, test_name(example, package_dir), id, file, triple)
        })
        .collect()
}

/// The file of an example as rustdoc names it: from the package's directory.
fn shown_file<'a>(file: &'a str, package_dir: &str) -> &'a str {
    let dir = package_dir.trim_end_matches('/');
    file.strip_prefix(dir).and_then(|it| it.strip_prefix('/')).unwrap_or(file)
}

/// The name libtest shows for `example`, rustdoc's (`ScrapedDocTest::new`): its file from the
/// package's directory, its item's path with no spaces and a space after when it has one, and
/// its line.
pub fn test_name(example: &Example, package_dir: &str) -> String {
    let mut item: String = example.item.chars().filter(|it| *it != ' ').collect();
    if !item.is_empty() {
        item.push(' ');
    }
    // alloc: a test's name, once per example
    format!("{} - {item}(line {})", shown_file(&example.file, package_dir), example.line)
}

fn edition_of(year: &str) -> Option<Edition> {
    Some(match year {
        "2015" => Edition::Edition2015,
        "2018" => Edition::Edition2018,
        "2021" => Edition::Edition2021,
        "2024" => Edition::Edition2024,
        _ => return None,
    })
}

impl Program {
    pub(crate) fn of(
        of: &OfLibrary,
        example: &Example,
        name: String,
        id: String,
        file: String,
        triple: &str,
    ) -> Program {
        let lang = &example.lang;
        // rustdoc's `LangString::parse` reads an edition it does not know as none.
        let edition = lang.edition.as_deref().and_then(edition_of).unwrap_or(of.edition);
        let parts = Parts::of(&example.code, &of.name, edition);
        let pieces = Pieces::of(of, &example.attrs, &parts, &example.code, &id, lang.test_harness);
        Program {
            name,
            id,
            file,
            line: example.line,
            source: pieces.aligned(example.line),
            line_offset: pieces.line_offset,
            edition,
            ignore: lang.ignore || lang.ignore_on.iter().any(|it| triple.contains(it.as_str())),
            no_run: lang.no_run,
            should_panic: lang.should_panic,
            compile_fail: lang.compile_fail,
            // clone: an example's error codes, once
            error_codes: lang.error_codes.clone(),
            test_harness: lang.test_harness,
        }
    }
}

/// rustdoc's `generate_unique_doctest`, taken apart: what it adds before the example, the
/// example's attributes and `extern crate`s, what it adds between those and the rest, and
/// the rest.
struct Pieces<'a> {
    /// The inner attributes it adds, one each: `allow(unused)`, or the `doc(test(attr(..)))`s
    /// of the example's path.
    attrs: Vec<String>,
    /// The example's inner attributes and `extern crate`s, each part ending in a newline.
    prefix: String,
    /// The `extern crate` of the library it adds.
    inject: Option<String>,
    /// The `fn main` it wraps the rest in: what opens it, ending in a newline, and what closes it.
    wrap: Option<(String, String)>,
    rest: &'a str,
    /// rustdoc's `full_test_line_offset`: how many lines it says come before the example's,
    /// which counts the two of its `extern crate` as one.
    line_offset: usize,
}

impl<'a> Pieces<'a> {
    fn of(
        of: &OfLibrary,
        crate_attrs: &[String],
        parts: &'a Parts,
        code: &'a str,
        id: &str,
        test_harness: bool,
    ) -> Pieces<'a> {
        if parts.invalid {
            // alloc: none, an empty list
            let attrs = Vec::new();
            return Pieces {
                attrs,
                prefix: String::new(),
                inject: None,
                wrap: None,
                rest: code,
                line_offset: 0,
            };
        }
        // alloc: an example's few attributes, once
        let attrs: Vec<String> = match crate_attrs.is_empty() {
            true => vec!["#![allow(unused)]".to_owned()],
            false => crate_attrs.iter().map(|it| format!("#![{it}]")).collect(),
        };
        let mut line_offset = attrs.len();
        let mut prefix = String::new();
        for text in [&parts.crate_attrs, &parts.module_attrs, &parts.crates] {
            if !text.is_empty() {
                prefix.push_str(text);
                if !text.ends_with('\n') {
                    prefix.push('\n');
                }
            }
        }
        let name = of.name.as_str();
        let inject = (!parts.has_extern_crate
            && !of.of_the_crate.no_crate_inject
            && name != "std"
            && code.contains(name))
        .then(|| format!("#[allow(unused_extern_crates)]\nextern crate r#{name};\n"));
        line_offset += usize::from(inject.is_some());
        let rest = parts.everything_else.trim();
        let no_std =
            attrs.iter().any(|it| it.contains("![no_std]")) || prefix.contains("![no_std]");
        let wrap = (!test_harness && !parts.has_main && !no_std).then(|| {
            let inner = format!("_doctest_main_{id}");
            match rest.ends_with("(())") {
                true => (
                    format!(
                        "fn main() {{ #[allow(non_snake_case)] fn {inner}() -> core::result::Result<(), impl core::fmt::Debug> {{\n"
                    ),
                    format!("\n}} {inner}().unwrap() }}"),
                ),
                false => (format!("fn main() {{ #[allow(non_snake_case)] fn {inner}() {{\n"), format!("\n}} {inner}() }}")),
            }
        });
        line_offset += usize::from(wrap.is_some());
        Pieces { attrs, prefix, inject, wrap, rest, line_offset }
    }

    /// The program as rustdoc writes it.
    fn rustdoc(&self) -> String {
        let mut out = String::new();
        for attr in &self.attrs {
            _ = writeln!(out, "{attr}");
        }
        out.push_str(&self.prefix);
        out.push_str(self.inject.as_deref().unwrap_or(""));
        let (open, close) =
            self.wrap.as_ref().map_or(("", ""), |(open, close)| (open.as_str(), close.as_str()));
        _ = write!(out, "{open}{}{close}", self.rest);
        out
    }

    /// The program blaze compiles, the example's rest on the line and column rustdoc shows it
    /// at, its file being the one the example is in: rustdoc tells rustc to show a line `line -
    /// line_offset` past where it is, blaze writes it there. What rustdoc adds is joined on as
    /// few lines as it can be, so the rest is reached from the first line of any example:
    /// rustdoc shows the rest at `newlines before it + 1 + line - line_offset`, which is past
    /// the line this puts it on with no padding.
    fn aligned(&self, line: usize) -> String {
        let open = self.wrap.as_ref().map_or("", |(open, _)| open.as_str());
        let close = self.wrap.as_ref().map_or("", |(_, close)| close.as_str());
        let inject = self.inject.as_deref().unwrap_or("");
        let rustdoc_before = self.attrs.len()
            + self.prefix.matches('\n').count()
            + inject.matches('\n').count()
            + open.matches('\n').count();
        // alloc: an example's program, once
        let mut before = self.attrs.join(" ");
        let between = [inject.trim_end().replace('\n', " "), open.trim_end().to_owned()];
        let between = between
            .iter()
            .filter(|it| !it.is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        match self.prefix.is_empty() {
            false => {
                if !before.is_empty() {
                    before.push(' ');
                }
                before.push_str(&self.prefix);
                if !between.is_empty() {
                    _ = writeln!(before, "{between}");
                }
            }
            true => {
                if !before.is_empty() && !between.is_empty() {
                    before.push(' ');
                }
                before.push_str(&between);
                if !before.is_empty() {
                    before.push('\n');
                }
            }
        }
        // rustdoc shows the rest's first line at `rustdoc_before + 1 + line - line_offset`.
        // `line` is at least 1, and of the lines rustdoc writes before the rest `line_offset`
        // takes back all but those of the example's own prefix and one of its `extern crate`'s,
        // while this writes at most one more than the prefix's: so this is not negative.
        let pad =
            (rustdoc_before + line).saturating_sub(self.line_offset + before.matches('\n').count());
        let mut out = "\n".repeat(pad);
        _ = write!(out, "{before}{}{close}", self.rest);
        out
    }
}

/// What building an example's program gave.
pub enum Built<'a> {
    Built,
    /// The compiler refused it, rendering `errors`, each with its code as `error[E0308]`.
    Refused {
        errors: &'a str,
    },
}

/// rustdoc's verdict on an example once it is built (`run_test`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// It is to run, judged by its runner ([`runner`]).
    Run,
    Passed,
    /// It failed, saying this, rustdoc's words.
    Failed(String),
}

impl Program {
    /// What `built` makes of this example's test, as rustdoc judges it: a `compile_fail`
    /// example passes when it is refused with each of its error codes, any other fails when
    /// refused, and a `no_run` one passes once built.
    pub fn after_build(&self, built: Built<'_>) -> Verdict {
        // alloc: a verdict's words, once per example
        match (built, self.compile_fail) {
            (Built::Built, true) => Verdict::Failed(
                "Test compiled successfully, but it's marked `compile_fail`.".to_owned(),
            ),
            (Built::Refused { errors }, true) => {
                let missing: Vec<&String> = self
                    .error_codes
                    .iter()
                    .filter(|code| !errors.contains(&format!("error[{code}]")))
                    .collect();
                match missing.is_empty() {
                    true => Verdict::Passed,
                    false => Verdict::Failed(format!(
                        "{errors}\nSome expected error codes were not found: {missing:?}"
                    )),
                }
            }
            (Built::Refused { errors }, false) => {
                Verdict::Failed(format!("{errors}\nCouldn't compile the test."))
            }
            (Built::Built, false) if self.no_run => Verdict::Passed,
            (Built::Built, false) => Verdict::Run,
        }
    }
}

/// The line of a runner's verdicts file for `program`: its id, then `run`, `pass` or `fail`
/// and what it says, a tab between each and its newlines and backslashes escaped.
pub fn verdict_line(program: &Program, verdict: &Verdict) -> String {
    let (kind, says) = match verdict {
        Verdict::Run => ("run", ""),
        Verdict::Passed => ("pass", ""),
        Verdict::Failed(says) => ("fail", says.as_str()),
    };
    // alloc: a verdict's line, once per example
    let says = says.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t");
    format!("{}\t{kind}\t{says}\n", program.id)
}

/// What the directory of a runner's examples is named: the runner's path, then this. Its
/// examples are `<dir>/<id>`, their verdicts `<dir>/verdicts` ([`VERDICTS`]).
pub const DIR_SUFFIX: &str = ".doctests";

/// The file of the verdicts in a runner's directory of examples ([`verdict_line`]).
pub const VERDICTS: &str = "verdicts";

/// The source of a library's doctest runner: a libtest harness with one test per program,
/// under rustdoc's names and sorted as rustdoc sorts them, each its own process. Each runs
/// the executable `<dir>/<id>`, `dir` being the runner's own path and `.doctests`, after reading its
/// verdict in `<dir>/verdicts` ([`verdict_line`]), and judges its exit as rustdoc does: a
/// `should_panic` example passes when it fails, any other when it succeeds.
pub fn runner(programs: &[Program]) -> String {
    // alloc: once per library's doctests, its programs in rustdoc's order
    let mut sorted: Vec<&Program> = programs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut tests = String::new();
    for it in sorted {
        _ = writeln!(
            tests,
            "    ({:?}, {:?}, {}, {}, {}, {}),",
            it.name, it.id, it.ignore, it.no_run, it.should_panic, it.compile_fail
        );
    }
    format!(
        "{RUNNER_HEAD}const TESTS: &[(&str, &str, bool, bool, bool, bool)] = &[\n{tests}];\n{RUNNER_BODY}"
    )
}

const RUNNER_HEAD: &str = "\
//! The doctests of a library, written by blaze: each example is its own program, run as its own
//! process, judged as rustdoc judges it.
#![feature(test)]
#![allow(unused)]
extern crate test;

use std::{path::{Path, PathBuf}, process::Command, sync::Arc};

";

const RUNNER_BODY: &str = r#"
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (c, c == '\\') {
            (_, true) => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            },
            (c, false) => out.push(c),
        }
    }
    out
}

fn fail(says: &str) -> ! {
    eprint!("{says}");
    std::panic::resume_unwind(Box::new(()))
}

fn run(dir: &Path, id: &str, should_panic: bool, nocapture: bool) -> Result<(), String> {
    let exe = dir.join(format!("{id}{}", std::env::consts::EXE_SUFFIX));
    let mut command = Command::new(&exe);
    let out = match nocapture {
        true => command.status().map(|status| std::process::Output { status, stdout: Vec::new(), stderr: Vec::new() }),
        false => command.output(),
    };
    let out = match out {
        Ok(it) => it,
        Err(why) => fail(&format!("Couldn't run the test: {why} ({})", exe.display())),
    };
    if should_panic && out.status.success() {
        fail("Test executable succeeded, but it's marked `should_panic`.");
    }
    if !should_panic && !out.status.success() {
        let mut says = format!("Test executable failed ({}).\n", out.status);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stdout.is_empty() || !stderr.is_empty() {
            says.push('\n');
            if !stdout.is_empty() {
                says.push_str(&format!("stdout:\n{stdout}\n"));
            }
            if !stderr.is_empty() {
                says.push_str(&format!("stderr:\n{stderr}\n"));
            }
        }
        fail(&says);
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let nocapture = args.iter().any(|it| it == "--nocapture" || it == "--no-capture")
        || std::env::var_os("RUST_TEST_NOCAPTURE").is_some_and(|it| it != "0");
    let exe = std::env::current_exe().unwrap_or_else(|why| panic!("the doctest runner can't find its own path: {why}"));
    let mut dir = exe.into_os_string();
    dir.push(".doctests");
    let dir = PathBuf::from(dir);
    let verdicts = std::fs::read_to_string(dir.join("verdicts"))
        .unwrap_or_else(|why| panic!("the doctest runner can't read {}: {why}", dir.join("verdicts").display()));
    let mut owned = Vec::new();
    for &(name, id, ignore, no_run, should_panic, compile_fail) in TESTS {
        let line = verdicts.lines().find(|it| it.split('\t').next() == Some(id));
        let mut fields = line.map(|it| it.splitn(3, '\t').skip(1));
        let kind = fields.as_mut().and_then(|it| it.next()).unwrap_or("");
        let says = unescape(fields.as_mut().and_then(|it| it.next()).unwrap_or(""));
        let kind = match (line, kind) {
            (None, _) if ignore => String::from("pass"),
            (None, _) => String::from("missing"),
            (_, kind) => String::from(kind),
        };
        let dir = dir.clone();
        let id = String::from(id);
        owned.push(test::TestDescAndFn {
            desc: test::TestDesc {
                name: test::DynTestName(String::from(name)),
                ignore,
                ignore_message: None,
                source_file: "",
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 0,
                should_panic: test::ShouldPanic::No,
                compile_fail,
                no_run,
                test_type: test::TestType::DocTest,
            },
            testfn: test::DynTestFn(Arc::new(move || match kind.as_str() {
                "run" => run(&dir, &id, should_panic, nocapture),
                "pass" => Ok(()),
                "fail" => fail(&says),
                _ => fail(&format!("blaze wrote no verdict for doctest `{id}` in {}", dir.join("verdicts").display())),
            })),
        });
    }
    let tests: Vec<&test::TestDescAndFn> = owned.iter().collect();
    test::test_main(&args, &tests)
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_fenced_and_indented() {
        let found =
            blocks("Text.\n\n```no_run\nlet a = 1;\n```\n\n    let b = 2;\n\n~~~text\nc\n~~~\n");
        let found: Vec<_> = found.iter().map(|it| (it.lang.as_deref(), it.code.as_str())).collect();
        assert_eq!(
            found,
            [(Some("no_run"), "let a = 1;\n"), (None, "let b = 2;\n"), (Some("text"), "c\n")]
        );
    }

    #[test]
    fn type_of_an_impl_as_rustdoc_names_it() {
        let shown = |ty: &str| {
            let file = format!("impl<'a, R> T for {ty} {{}}");
            let parse = syntax::SourceFile::parse(&file, Edition::CURRENT);
            let found = parse.root().descendants().find_map(ast::Impl::cast);
            shown_type(found.and_then(|it| it.self_ty()).unwrap())
        };
        assert_eq!(shown("&mut Deserializer<R>"), "&'_mutDeserializer<R>");
        assert_eq!(shown("&'a Map<K, V>"), "&'aMap<K,V>");
    }

    #[test]
    fn hidden_lines_are_code() {
        let code: Vec<_> =
            "# let a = 1;\n  #\n##[derive(Debug)]\n#[x]\nb".lines().map(tested_line).collect();
        assert_eq!(code.join("\n"), "let a = 1;\n\n#[derive(Debug)]\n#[x]\nb");
    }

    #[test]
    fn the_crates_test_attributes() {
        let read = OfTheCrate::read(
            "#![doc(test(attr(deny(warnings), allow(dead_code)), no_crate_inject))]\nfn f() {}",
        );
        assert!(read.no_crate_inject);
        assert_eq!(OfTheCrate::read("#![doc = \"test\"]"), OfTheCrate::default());
    }

    fn parts(code: &str) -> Parts {
        Parts::of(code, "app", Edition::Edition2021)
    }

    #[test]
    fn parts_of_an_example() {
        let it = parts(
            "#![allow(dead_code)]\n#![feature(x)]\n\n#[macro_use] extern crate app;\nfn main() {}\n",
        );
        assert_eq!(it.module_attrs, "#![allow(dead_code)]");
        assert_eq!(it.crate_attrs, "\n#![feature(x)]");
        // Until the first statement that is not an `extern crate`, the text between statements
        // goes with the crates, as rustdoc has it.
        assert_eq!(it.crates, "\n\n#[macro_use] extern crate app;\n");
        assert_eq!(it.everything_else, "fn main() {}");
        assert!(it.has_extern_crate && it.has_main);
        let it = parts("#![allow(internal_features)]");
        assert_eq!(
            (it.crate_attrs.as_str(), it.module_attrs.as_str()),
            ("#![allow(internal_features)]", "")
        );
        assert!(parts("fn main() {").invalid);
        assert!(parts("extern crate app as other;").has_extern_crate);
        assert!(!parts("extern crate other as app;").has_extern_crate);
    }

    #[test]
    fn where_a_main_is() {
        assert!(parts("fn main() {}").has_main);
        assert!(parts("use std::fmt;\nfn main() {}").has_main);
        // A statement that is not an item has all of it wrapped.
        assert!(!parts("let x = 1;\nfn main() {}").has_main);
        assert!(!parts("fn main() {}\nmain()").has_main);
        assert!(!parts("fn main() {};").has_main);
        // A macro whose outermost tokens have `fn main(..)`.
        assert!(parts("wrap! { fn main() {} }").has_main);
        assert!(parts("wrap!(fn main() {});").has_main);
        assert!(!parts("wrap! { mod m { fn main() {} } }").has_main);
        assert!(!parts("wrap! { fn main }").has_main);
    }

    fn example(item: &str, line: usize, info: &str, code: &str) -> Example {
        Example {
            item: item.to_owned(),
            file: "/w/app/src/lib.rs".to_owned(),
            line,
            lang: LangString::parse(info),
            code: code.to_owned(),
            attrs: Vec::new(),
        }
    }

    fn library(no_crate_inject: bool) -> OfLibrary {
        OfLibrary {
            name: "app".to_owned(),
            edition: Edition::Edition2021,
            of_the_crate: OfTheCrate { no_crate_inject },
        }
    }

    /// The program rustdoc writes for `example`, with the id `id`.
    fn rustdoc(of: &OfLibrary, example: &Example, id: &str) -> String {
        let edition = example.lang.edition.as_deref().and_then(edition_of).unwrap_or(of.edition);
        let parts = Parts::of(&example.code, &of.name, edition);
        Pieces::of(of, &example.attrs, &parts, &example.code, id, example.lang.test_harness)
            .rustdoc()
    }

    /// The line `needle` is first on in `text`, from 1.
    fn line_of(text: &str, needle: &str) -> usize {
        1 + text[..text.find(needle).unwrap()].matches('\n').count()
    }

    #[test]
    fn programs_as_rustdoc_writes_them() {
        let examples = [
            example("", 1, "", "assert_eq!(app::double(2), 4);"),
            example(
                "double",
                5,
                "no_run",
                "let n: u32 = \"7\".parse()?;\nOk::<(), std::num::ParseIntError>(())",
            ),
            example("double", 5, "should_panic,edition2018", "fn main() { panic!() }"),
            example(
                "Counter<T> :: bump",
                12,
                "ignore-x86_64,compile_fail,E0308",
                "#![no_std]\nlet x: u8 = \"\";",
            ),
            example("Counter", 20, "test_harness", "#[test]\nfn t() {}"),
        ];
        let lib = library(false);
        let programs = programs(&lib, &examples, "x86_64-unknown-linux-gnu", "/w/app/");
        let rustdoc: Vec<_> = examples
            .iter()
            .zip(&programs)
            .map(|(it, program)| rustdoc(&lib, it, &program.id))
            .collect();
        let names: Vec<_> = programs.iter().map(|it| (it.name.as_str(), it.id.as_str())).collect();
        assert_eq!(
            names,
            [
                ("src/lib.rs - (line 1)", "src_lib_rs_1_0"),
                ("src/lib.rs - double (line 5)", "src_lib_rs_5_0"),
                ("src/lib.rs - double (line 5)", "src_lib_rs_5_1"),
                ("src/lib.rs - Counter<T>::bump (line 12)", "src_lib_rs_12_0"),
                ("src/lib.rs - Counter (line 20)", "src_lib_rs_20_0"),
            ]
        );
        assert_eq!(
            rustdoc[0],
            "#![allow(unused)]\n#[allow(unused_extern_crates)]\nextern crate r#app;\n\
             fn main() { #[allow(non_snake_case)] fn _doctest_main_src_lib_rs_1_0() {\n\
             assert_eq!(app::double(2), 4);\n} _doctest_main_src_lib_rs_1_0() }"
        );
        assert_eq!(programs[0].line_offset, 3);
        assert_eq!(
            programs[0].source,
            "\n#![allow(unused)] #[allow(unused_extern_crates)] extern crate r#app; \
             fn main() { #[allow(non_snake_case)] fn _doctest_main_src_lib_rs_1_0() {\n\
             assert_eq!(app::double(2), 4);\n} _doctest_main_src_lib_rs_1_0() }"
        );
        assert_eq!(
            rustdoc[1],
            "#![allow(unused)]\n\
             fn main() { #[allow(non_snake_case)] fn _doctest_main_src_lib_rs_5_0() -> core::result::Result<(), impl core::fmt::Debug> {\n\
             let n: u32 = \"7\".parse()?;\nOk::<(), std::num::ParseIntError>(())\n} _doctest_main_src_lib_rs_5_0().unwrap() }"
        );
        assert!(programs[1].no_run && !programs[1].ignore);
        assert_eq!(rustdoc[2], "#![allow(unused)]\nfn main() { panic!() }");
        assert!(programs[2].should_panic && programs[2].edition == Edition::Edition2018);
        let failing = &programs[3];
        assert!(
            failing.ignore
                && failing.compile_fail
                && failing.no_run
                && failing.error_codes == ["E0308"]
        );
        assert_eq!(rustdoc[3], "#![allow(unused)]\n#![no_std]\n\nlet x: u8 = \"\";");
        // rustdoc's verdicts once built.
        let refused = |errors| Built::Refused { errors };
        assert_eq!(programs[0].after_build(Built::Built), Verdict::Run);
        assert_eq!(programs[1].after_build(Built::Built), Verdict::Passed);
        assert!(
            matches!(programs[0].after_build(refused("error[E0425]")), Verdict::Failed(it) if it.ends_with("Couldn't compile the test."))
        );
        assert_eq!(failing.after_build(refused("error[E0308]: mismatched types")), Verdict::Passed);
        assert!(
            matches!(failing.after_build(refused("error[E0425]")), Verdict::Failed(it) if it.ends_with("[\"E0308\"]"))
        );
        assert!(matches!(failing.after_build(Built::Built), Verdict::Failed(_)));
        assert_eq!(
            verdict_line(failing, &Verdict::Failed("a\tb\nc\\".to_owned())),
            "src_lib_rs_12_0\tfail\ta\\tb\\nc\\\\\n"
        );
        let runner = runner(&programs);
        assert!(
            runner.find("src/lib.rs - (line 1)").unwrap()
                < runner.find("src/lib.rs - Counter (line 20)").unwrap()
        );
        // Where the daemon puts the examples is where the runner looks.
        assert!(
            runner.contains(&format!("dir.push({DIR_SUFFIX:?})"))
                && runner.contains(&format!("{VERDICTS:?}"))
        );
        assert!(programs[4].test_harness);
        assert_eq!(rustdoc[4], "#![allow(unused)]\n#[test]\nfn t() {}");
        // Each example's rest is on the line rustdoc has rustc show it at, its own line moved
        // by `line - line_offset`.
        for ((example, program), rustdoc) in examples.iter().zip(&programs).zip(&rustdoc) {
            let rest = example.code.lines().find(|it| !it.starts_with("#!")).unwrap();
            let shown = line_of(rustdoc, rest) + program.line - program.line_offset;
            assert_eq!(line_of(&program.source, rest), shown, "{}", program.name);
            assert_eq!(program.file, "src/lib.rs");
        }
    }

    #[test]
    fn the_crates_attributes_and_no_inject() {
        let mut examples = [example("", 1, "", "app::f();")];
        examples[0].attrs = vec!["deny(warnings)".to_owned()];
        let lib = library(true);
        let programs = programs(&lib, &examples, "x86_64-unknown-linux-gnu", "/w/app");
        assert_eq!(
            rustdoc(&lib, &examples[0], &programs[0].id),
            "#![deny(warnings)]\n\
             fn main() { #[allow(non_snake_case)] fn _doctest_main_src_lib_rs_1_0() {\napp::f();\n} _doctest_main_src_lib_rs_1_0() }"
        );
    }

    #[test]
    fn examples_of_a_crate() {
        let text = r#"
//- /lib.rs crate:app
//! The crate.
//!
//! ```
//! # let n = 1;
//! assert_eq!(app::double(n), 2);
//! ```

/// Doubles.
///
/// ```should_panic
/// panic!();
/// ```
///
/// ```text
/// not an example
/// ```
pub fn double(n: u32) -> u32 { n * 2 }

pub struct Counter;

impl Counter {
    /// ```no_run
    /// loop {}
    /// ```
    pub fn bump(&mut self) {}
}

pub mod inner {
    /// ```ignore
    /// x
    /// ```
    pub fn f() {}
}
"#;
        let db = hir_expand::fixture::db(text).into_db();
        let krate = hir::Crate::all(&db)
            .into_iter()
            .find(|it| it.base().rust_name(&db).is_ok_and(|it| it.as_str() == "app"))
            .unwrap();
        let found = examples(&db, krate);
        let found: Vec<_> = found
            .iter()
            .map(|it| (it.item.as_str(), it.line, it.code.as_str(), it.lang.no_run, it.lang.ignore))
            .collect();
        assert_eq!(
            found,
            [
                // The line of each block's fence, as rustdoc names its test.
                ("", 3, "let n = 1;\nassert_eq!(app::double(n), 2);", false, false),
                ("double", 10, "panic!();", false, false),
                ("inner::f", 29, "x", false, true),
                ("Counter::bump", 22, "loop {}", true, false),
            ]
        );
        assert!(examples(&db, krate)[1].lang.should_panic);
    }

    #[test]
    fn test_attributes_down_the_path_and_target_features() {
        let text = r#"
//- /lib.rs crate:app cfg:target_feature=sse2
#![doc(test(attr(deny(warnings))))]

/// ```
/// top
/// ```
pub fn top() {}

#[doc(test(attr(allow(dead_code), feature(x))))]
pub mod inner {
    /// ```
    /// inside
    /// ```
    #[cfg_attr(all(), doc(test(attr(warn(unused)))))]
    pub fn f() {}

    /// ```
    /// after
    /// ```
    pub fn g() {}
}

/// ```
/// wide
/// ```
#[target_feature(enable = "avx512f")]
pub fn wide() {}

/// ```
/// narrow
/// ```
#[target_feature(enable = "sse2")]
pub fn narrow() {}
"#;
        let db = hir_expand::fixture::db(text).into_db();
        let krate = hir::Crate::all(&db)
            .into_iter()
            .find(|it| it.base().rust_name(&db).is_ok_and(|it| it.as_str() == "app"))
            .unwrap();
        let found = examples(&db, krate);
        let found: Vec<_> = found.iter().map(|it| (it.code.as_str(), it.attrs.join(" "))).collect();
        let found: Vec<_> = found.iter().map(|(code, attrs)| (*code, attrs.as_str())).collect();
        assert_eq!(
            found,
            [
                ("top", "deny(warnings)"),
                ("inside", "deny(warnings) allow(dead_code) feature(x) warn(unused)"),
                ("after", "deny(warnings) allow(dead_code) feature(x)"),
                // `wide` wants a feature the target lacks: skipped, as rustdoc skips it.
                ("narrow", "deny(warnings)"),
            ]
        );
    }
}
