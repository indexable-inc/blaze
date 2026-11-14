//! Code patterns: Rust text with `$name` (one node) and `$$$name` (a run of sibling elements,
//! possibly empty) metavariables, parsed once as the form asked for, and matched tree against
//! tree with whitespace and comments ignored.
//!
//! A metavariable is the outermost pattern node whose whole text is its placeholder, so `$e`
//! in `$e.unwrap()` stands for the receiver whatever its kind. `_` (as an expression, a type or
//! a pattern) matches any node. A path of two or more segments that starts at a crate's name
//! (`core::mem::take`, `::std::sync::Mutex`) is a def path: it matches a path of the code that
//! the front end resolves to the same item, however that path is spelled, and the generic
//! arguments of its last segment, when it has any, match the code's. Every other token matches
//! by kind and text.

use std::fmt;

use blaze_hash::HashMap;
use std::alloc::Global;
use syntax::{
    Edition, Parse, SourceFile, SyntaxKind, TextRange, TextSize, WalkEvent,
    view::{self as ast, NodeId},
};

use crate::error::{Error, PatternParseSnafu, PatternShapeSnafu};

const ONE: &str = "__astlog_one_";
const MANY: &str = "__astlog_many_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Expr,
    Type,
    Pat,
    Stmt,
    Item,
    Path,
}

impl Form {
    pub fn name(self) -> &'static str {
        match self {
            Form::Expr => "expr",
            Form::Type => "type",
            Form::Pat => "pat",
            Form::Stmt => "stmt",
            Form::Item => "item",
            Form::Path => "path",
        }
    }

    /// The text around a pattern that makes a file of it.
    fn wrap(self) -> (&'static str, &'static str) {
        match self {
            Form::Expr | Form::Path => ("const __ASTLOG: () = ", ";\n"),
            Form::Type => ("type __Astlog = ", ";\n"),
            Form::Pat => ("fn __astlog() { match () { ", " => {} } }\n"),
            Form::Stmt => ("fn __astlog() { ", " }\n"),
            Form::Item => ("mod __astlog { ", " }\n"),
        }
    }
}

/// One metavariable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Var {
    pub name: String,
    /// `$$$name`: a run of sibling elements, bound as their text.
    pub many: bool,
}

/// What a pattern node stands for, beyond its kind and children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Var(usize),
    Wild,
    /// The `n`th def path of the pattern.
    DefPath(usize),
}

/// A code pattern, compiled.
pub struct Pattern {
    pub form: Form,
    pub text: String,
    pub line: usize,
    parse: Parse<SourceFile>,
    root: NodeId,
    pub root_kind: SyntaxKind,
    /// The metavariables, each once, in the order they first appear: the atom's columns after
    /// the matched node.
    pub vars: Vec<Var>,
    /// The def paths, as written with no generic arguments and no leading `::`.
    pub paths: Vec<String>,
    marks: HashMap<NodeId, Mark, Global>,
    /// The longest identifier or literal every match spells as written: a file without its
    /// bytes holds no match.
    pub literal: Option<String>,
}

impl fmt::Debug for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pattern")
            .field("form", &self.form)
            .field("text", &self.text)
            .field("vars", &self.vars)
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

/// A metavariable's value in one match.
#[derive(Clone, Copy, Debug)]
pub enum Bound<'t> {
    Node(ast::Node<'t>),
    /// A run, by the range of the target's text it spans (empty for an empty run).
    Run(ast::Node<'t>, TextRange),
}

impl Bound<'_> {
    fn same(self, other: Bound<'_>) -> bool {
        tokens(self) == tokens(other)
    }
}

/// The non-trivia token texts of a bound value.
fn tokens<'t>(bound: Bound<'t>) -> Vec<&'t str> {
    let (node, range) = match bound {
        Bound::Node(node) => (node, node.text_range()),
        Bound::Run(parent, range) => (parent, range),
    };
    node.descendants_with_tokens()
        .filter_map(syntax::NodeOrToken::into_token)
        .filter(|it| !it.kind().is_trivia() && range.contains_range(it.text_range()))
        .map(|it| it.text())
        .collect() // alloc: one compare of a metavariable used twice
}

/// Rewrites `$name` and `$$$name` to placeholder identifiers, recording the variables.
fn placeholders(text: &str, form: Form, line: usize) -> Result<(String, Vec<Var>), Error> {
    let mut out = String::with_capacity(text.len() + 16);
    let mut vars: Vec<Var> = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'$' {
            let next = text[at..].find('$').map_or(text.len(), |it| at + it);
            out.push_str(&text[at..next]);
            at = next;
            continue;
        }
        let many = text[at..].starts_with("$$$");
        let start = at + if many { 3 } else { 1 };
        let end = text[start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(text.len(), |it| start + it);
        let name = &text[start..end];
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(PatternShapeSnafu {
                line,
                form: form.name(),
                text,
                message: format!("`$` at byte {at} starts no metavariable (`$name` or `$$$name`)"),
            }
            .build()
            .into());
        }
        if name == "_" && !many {
            out.push('_');
        } else {
            match vars.iter().find(|it| it.name == name) {
                Some(seen) if seen.many != many => {
                    return Err(PatternShapeSnafu {
                        line,
                        form: form.name(),
                        text,
                        message: format!("`{name}` is both `$` and `$$$`"),
                    }
                    .build()
                    .into());
                }
                Some(_) => {}
                None => vars.push(Var { name: name.to_owned(), many }),
            }
            out.push_str(if many { MANY } else { ONE });
            out.push_str(name);
        }
        at = end;
    }
    Ok((out, vars))
}

/// The placeholder `text` is, as a variable name and whether it is a run.
fn placeholder(text: &str) -> Option<(&str, bool)> {
    let (name, many) = match text.strip_prefix(MANY) {
        Some(name) => (name, true),
        None => (text.strip_prefix(ONE)?, false),
    };
    // `__astlog_one_x + __astlog_one_x` starts with one but is more than one.
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_').then_some((name, many))
}

/// The segments of a path written from a crate's name, or `None` for a path that is not one.
fn def_path(path: ast::Node<'_>) -> Option<String> {
    let text = path.text();
    if placeholder_in(text) || text.starts_with('<') {
        return None;
    }
    let mut segments = Vec::new(); // alloc: a pattern path's few segments, once a load
    for segment in path.descendants().filter(|it| it.kind() == SyntaxKind::PATH_SEGMENT) {
        let name = segment.children().find(|it| it.kind() == SyntaxKind::NAME_REF)?;
        segments.push(name.text());
    }
    if segments.len() < 2 || matches!(segments[0], "crate" | "self" | "super" | "Self") {
        return None;
    }
    Some(segments.join("::"))
}

fn placeholder_in(text: &str) -> bool {
    text.contains(ONE) || text.contains(MANY)
}

impl Pattern {
    /// Parses `text` as `form`.
    ///
    /// # Errors
    ///
    /// A pattern that is not that form of Rust, holds a stray `$`, or is nothing but one
    /// metavariable.
    pub fn parse(form: Form, text: &str, line: usize) -> Result<Pattern, Error> {
        let (body, vars) = placeholders(text.trim(), form, line)?;
        let (before, after) = form.wrap();
        let source = format!("{before}{body}{after}");
        let parse = SourceFile::parse(&source, Edition::CURRENT);
        let shape = |message: String| -> Error {
            PatternShapeSnafu { line, form: form.name(), text, message }.build().into()
        };
        if let Some(error) = parse.errors_with_bodies().first() {
            return PatternParseSnafu { line, form: form.name(), text, error: error.to_string() }
                .fail()
                .map_err(Error::from);
        }
        let range = TextRange::at(TextSize::of(before), TextSize::of(body.as_str()));
        let file = parse.root();
        let mut root = file
            .descendants()
            .find(|it| it.text_range() == range)
            .ok_or_else(|| shape(format!("is no single {}", form.name())))?;
        if form == Form::Path {
            root = root
                .descendants()
                .find(|it| it.kind() == SyntaxKind::PATH && it.text_range() == range)
                .ok_or_else(|| shape("is no path".to_owned()))?;
        }
        if placeholder(root.text()).is_some() {
            return Err(shape(
                "is one metavariable, which matches every node: say what to match".to_owned(),
            ));
        }
        let mut marks = HashMap::new_in(Global);
        let mut paths: Vec<String> = Vec::new();
        let mut literal: Option<&str> = None;
        let mut walk = root.preorder_with_tokens();
        while let Some(event) = walk.next() {
            let WalkEvent::Enter(element) = event else { continue };
            match element {
                syntax::NodeOrToken::Node(node) => {
                    if let Some((name, _)) = placeholder(node.text()) {
                        let index = vars.iter().position(|it| it.name == name).ok_or_else(|| {
                            shape(format!("`{name}` is a metavariable the rewrite lost"))
                        })?;
                        marks.insert(node.id(), Mark::Var(index));
                        walk.skip_subtree();
                    } else if matches!(
                        node.kind(),
                        SyntaxKind::UNDERSCORE_EXPR | SyntaxKind::INFER_TYPE | SyntaxKind::WILDCARD_PAT
                    ) {
                        marks.insert(node.id(), Mark::Wild);
                        walk.skip_subtree();
                    } else if node.kind() == SyntaxKind::PATH
                        && node.parent().is_none_or(|it| it.kind() != SyntaxKind::PATH)
                        && let Some(path) = def_path(node)
                    {
                        marks.insert(node.id(), Mark::DefPath(paths.len()));
                        paths.push(path);
                        // The last segment's generic arguments still match by structure.
                        walk.skip_subtree();
                        if let Some(args) = last_generic_args(node) {
                            note_literals(args, &mut literal);
                        }
                    }
                }
                syntax::NodeOrToken::Token(token) => {
                    if matches!(
                        token.kind(),
                        SyntaxKind::IDENT | SyntaxKind::INT_NUMBER | SyntaxKind::STRING
                    ) && placeholder(token.text()).is_none()
                    {
                        keep_longest(&mut literal, token.text());
                    }
                }
            }
        }
        Ok(Pattern {
            form,
            text: text.to_owned(),
            line,
            root_kind: root.kind(),
            root: root.id(),
            literal: literal.map(str::to_owned),
            parse,
            vars,
            paths,
            marks,
        })
    }

    fn root(&self) -> ast::Node<'_> {
        node_at(self.parse.root(), self.root)
    }

    /// The metavariables' values when `target` matches, in [`Pattern::vars`] order. `resolves`
    /// says whether a path of the target resolves to the `n`th def path.
    ///
    /// # Errors
    ///
    /// What `resolves` fails with.
    pub fn matches<'t>(
        &self,
        target: ast::Node<'t>,
        resolves: &mut dyn FnMut(usize, ast::Node<'t>) -> Result<bool, Error>,
    ) -> Result<Option<Vec<Bound<'t>>>, Error> {
        if target.kind() != self.root_kind {
            return Ok(None);
        }
        let mut cx = Cx { pattern: self, resolves };
        let mut bound = vec![None; self.vars.len()]; // alloc: one match's metavariables
        if !cx.node(self.root(), target, &mut bound)? {
            return Ok(None);
        }
        Ok(Some(
            bound
                .into_iter()
                .map(|it| it.expect("every metavariable of a match is bound"))
                .collect(),
        ))
    }
}

fn keep_longest<'a>(literal: &mut Option<&'a str>, text: &'a str) {
    if literal.is_none_or(|it| text.len() > it.len()) {
        *literal = Some(text);
    }
}

fn note_literals<'a>(node: ast::Node<'a>, literal: &mut Option<&'a str>) {
    for token in node.descendants_with_tokens().filter_map(syntax::NodeOrToken::into_token) {
        if token.kind() == SyntaxKind::IDENT && placeholder(token.text()).is_none() {
            keep_longest(literal, token.text());
        }
    }
}

/// The generic argument list of a path's last segment, if it has one.
fn last_generic_args(path: ast::Node<'_>) -> Option<ast::Node<'_>> {
    let segment = path.children().find(|it| it.kind() == SyntaxKind::PATH_SEGMENT)?;
    segment.children().find(|it| it.kind() == SyntaxKind::GENERIC_ARG_LIST)
}

pub(crate) fn node_at(root: ast::Node<'_>, id: NodeId) -> ast::Node<'_> {
    ast::Node::at(root.shared_file(), id).into_node().expect("a node id names a node")
}

fn significant(node: ast::Node<'_>) -> Vec<ast::Element<'_>> {
    node.children_with_tokens().filter(|it| !it.kind().is_trivia()).collect() // alloc: one level of a match
}

struct Cx<'p, 'r, 't> {
    pattern: &'p Pattern,
    resolves: &'r mut dyn FnMut(usize, ast::Node<'t>) -> Result<bool, Error>,
}

type Bindings<'t> = Vec<Option<Bound<'t>>>;

impl<'t> Cx<'_, '_, 't> {
    fn bind(&self, var: usize, value: Bound<'t>, bound: &mut Bindings<'t>) -> bool {
        match bound[var] {
            Some(seen) => seen.same(value),
            None => {
                bound[var] = Some(value);
                true
            }
        }
    }

    fn node(
        &mut self,
        pattern: ast::Node<'_>,
        target: ast::Node<'t>,
        bound: &mut Bindings<'t>,
    ) -> Result<bool, Error> {
        match self.pattern.marks.get(&pattern.id()).copied() {
            Some(Mark::Var(var)) => return Ok(self.bind(var, Bound::Node(target), bound)),
            Some(Mark::Wild) => return Ok(true),
            Some(Mark::DefPath(path)) => {
                if target.kind() != SyntaxKind::PATH || !(self.resolves)(path, target)? {
                    return Ok(false);
                }
                return match (last_generic_args(pattern), last_generic_args(target)) {
                    (None, _) => Ok(true),
                    (Some(_), None) => Ok(false),
                    (Some(want), Some(have)) => self.node(want, have, bound),
                };
            }
            None => {}
        }
        if pattern.kind() != target.kind() {
            return Ok(false);
        }
        let want = significant(pattern);
        let have = significant(target);
        self.seq(target, &want, &have, bound)
    }

    fn seq(
        &mut self,
        parent: ast::Node<'t>,
        want: &[ast::Element<'_>],
        have: &[ast::Element<'t>],
        bound: &mut Bindings<'t>,
    ) -> Result<bool, Error> {
        let Some((first, rest)) = want.split_first() else { return Ok(have.is_empty()) };
        if let syntax::NodeOrToken::Node(node) = first
            && let Some(Mark::Var(var)) = self.pattern.marks.get(&node.id()).copied()
            && self.pattern.vars[var].many
        {
            for take in 0..=have.len() {
                let range = match (have.first(), take) {
                    (Some(start), 1..) => {
                        TextRange::new(start.text_range().start(), have[take - 1].text_range().end())
                    }
                    _ => TextRange::empty(
                        have.first().map_or(parent.text_range().end(), |it| it.text_range().start()),
                    ),
                };
                let mut tried = bound.clone(); // clone: one branch of a run's backtracking
                if self.bind(var, Bound::Run(parent, range), &mut tried)
                    && self.seq(parent, rest, &have[take..], &mut tried)?
                {
                    *bound = tried;
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        let Some((next, left)) = have.split_first() else { return Ok(false) };
        let matched = match (first, next) {
            (syntax::NodeOrToken::Node(want), syntax::NodeOrToken::Node(have)) => {
                self.node(*want, *have, bound)?
            }
            (syntax::NodeOrToken::Token(want), syntax::NodeOrToken::Token(have)) => {
                want.kind() == have.kind() && want.text() == have.text()
            }
            _ => false,
        };
        if !matched {
            return Ok(false);
        }
        self.seq(parent, rest, left, bound)
    }
}

/// A type pattern for `type-matches`: `_`, `$name`, a def path with generic arguments
/// (`core::result::Result<_, $e>`), a builtin or other single name (by text), `&T`, `&mut T`,
/// a tuple, a slice or an array.
pub struct TypePattern {
    pub text: String,
    pub line: usize,
    parse: Parse<SourceFile>,
    root: NodeId,
    pub vars: Vec<String>,
    pub paths: Vec<String>,
}

impl fmt::Debug for TypePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypePattern").field("text", &self.text).finish_non_exhaustive()
    }
}

/// One step of a type pattern, as the matcher reads it.
pub enum TypeShape<'p> {
    Wild,
    Var(usize),
    /// The `n`th def path, and the pattern's generic arguments of its last segment.
    Def(usize, Vec<ast::Node<'p>>),
    Name(&'p str),
    Ref { mutable: bool, inner: ast::Node<'p> },
    Tuple(Vec<ast::Node<'p>>),
    Slice(ast::Node<'p>),
    Array(ast::Node<'p>),
}

impl TypePattern {
    /// # Errors
    ///
    /// A pattern that is no type, or a type form the matcher does not read.
    pub fn parse(text: &str, line: usize) -> Result<TypePattern, Error> {
        let pattern = Pattern::parse(Form::Type, text, line)?;
        let type_pattern = TypePattern {
            text: text.to_owned(),
            line,
            root: pattern.root,
            vars: pattern.vars.iter().map(|it| it.name.clone()).collect(),
            paths: pattern.paths.clone(),
            parse: pattern.parse.clone(), // clone: an `Arc` count, the type pattern's own
        };
        if let Some(var) = pattern.vars.iter().find(|it| it.many) {
            return PatternShapeSnafu {
                line,
                form: "type",
                text,
                message: format!("`$$${}` is a run, and a type argument is one type", var.name),
            }
            .fail()
            .map_err(Error::from);
        }
        let mut stack = vec![type_pattern.root()];
        while let Some(node) = stack.pop() {
            match type_pattern.shape(node) {
                Some(TypeShape::Def(_, args) | TypeShape::Tuple(args)) => stack.extend(args),
                Some(TypeShape::Ref { inner, .. } | TypeShape::Slice(inner) | TypeShape::Array(inner)) => {
                    stack.push(inner);
                }
                Some(TypeShape::Wild | TypeShape::Var(_) | TypeShape::Name(_)) => {}
                None => {
                    return PatternShapeSnafu {
                        line,
                        form: "type",
                        text,
                        message: format!(
                            "`{}` is a {:?}, which `type-matches` does not read",
                            node.text(),
                            node.kind()
                        ),
                    }
                    .fail()
                    .map_err(Error::from);
                }
            }
        }
        Ok(type_pattern)
    }

    pub fn root(&self) -> ast::Node<'_> {
        node_at(self.parse.root(), self.root)
    }

    /// What a node of this pattern asks of a type.
    pub fn shape<'p>(&'p self, node: ast::Node<'p>) -> Option<TypeShape<'p>> {
        let text = node.text();
        if let Some((name, _)) = placeholder(text) {
            return self.vars.iter().position(|it| it == name).map(TypeShape::Var);
        }
        match node.kind() {
            SyntaxKind::INFER_TYPE => Some(TypeShape::Wild),
            SyntaxKind::PATH_TYPE => {
                let path = node.children().find(|it| it.kind() == SyntaxKind::PATH)?;
                let args: Vec<ast::Node<'p>> = last_generic_args(path)
                    .map(|list| {
                        list.children()
                            .filter(|it| it.kind() == SyntaxKind::TYPE_ARG)
                            .filter_map(|it| it.children().next())
                            .collect()
                    })
                    .unwrap_or_default();
                match def_path(path) {
                    Some(written) => {
                        let index = self.paths.iter().position(|it| *it == written)?;
                        Some(TypeShape::Def(index, args))
                    }
                    None if args.is_empty() && !text.contains("::") => Some(TypeShape::Name(text)),
                    None => None,
                }
            }
            SyntaxKind::REF_TYPE => Some(TypeShape::Ref {
                mutable: node.child_token(SyntaxKind::MUT_KW).is_some(),
                inner: node.children().last()?,
            }),
            SyntaxKind::TUPLE_TYPE => Some(TypeShape::Tuple(node.children().collect())),
            SyntaxKind::SLICE_TYPE => Some(TypeShape::Slice(node.children().next()?)),
            SyntaxKind::ARRAY_TYPE => Some(TypeShape::Array(node.children().next()?)),
            _ => None,
        }
    }
}
