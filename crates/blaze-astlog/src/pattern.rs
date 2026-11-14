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
    view::{self as ast, AstNode as _, NodeId},
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
    /// The editions that read the pattern's text as other Rust than [`Edition::CURRENT`], the
    /// one it is parsed in (`async {}` is a struct literal in 2015): code of such an edition
    /// is refused, never matched against the wrong tree.
    pub reads_otherwise: Vec<Edition>,
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
/// Only the segments' names make the def path: the last segment's generic arguments are
/// matched apart, by structure, and may hold metavariables (`std::vec::Vec<$a>`). A
/// metavariable as a segment's name, or generic arguments before the last segment
/// (`Vec::<$t>::new`), make the path one matched by structure.
fn def_path(path: ast::Node<'_>) -> Option<String> {
    if path.text().starts_with('<') {
        return None;
    }
    let mut segments = Vec::new(); // alloc: a pattern path's few segments, once a load
    let mut at = Some(path);
    while let Some(node) = at {
        let segment = node.children().find(|it| it.kind() == SyntaxKind::PATH_SEGMENT)?;
        let last = segments.is_empty();
        if !last && segment.children().any(|it| it.kind() == SyntaxKind::GENERIC_ARG_LIST) {
            return None;
        }
        let name = segment.children().find(|it| it.kind() == SyntaxKind::NAME_REF)?;
        if placeholder_in(name.text()) {
            return None;
        }
        segments.push(name.text());
        at = node.children().find(|it| it.kind() == SyntaxKind::PATH);
    }
    segments.reverse();
    if segments.len() < 2 || matches!(segments[0], "crate" | "self" | "super" | "Self") {
        return None;
    }
    Some(segments.join("::"))
}

/// The most non-trivia tokens a pattern or a template may have. The front end's parser
/// recurses with the text's nesting: on a 2 MiB thread (a test thread, a tokio worker) a
/// debug build overflows on `V<V<..u8..>>` at about 1900 levels, 5700 tokens, while 127000
/// nested `-` or `{` parse. 2048 tokens of the worst shape stay under 700 levels.
const MAX_TOKENS: usize = 2048;

/// The deepest a pattern may make the matcher recurse ([`recursion_depth`]). On a 2 MiB
/// thread a debug build overflows between depth 690 and 710 for each of 28 nesting shapes
/// measured (parens, calls, blocks, `if`/`else`, generic arguments, refs, closures and
/// others), so 128 leaves the caller over 1.5 MiB.
const MAX_DEPTH: usize = 128;

/// The non-trivia tokens `text` lexes as.
fn token_count(text: &str) -> usize {
    let lexed = syntax::LexedStr::new(Edition::CURRENT, text);
    (0..lexed.len()).filter(|&at| !lexed.kind(at).is_trivia()).count()
}

/// The refusal of a text with more than [`MAX_TOKENS`] tokens.
pub(crate) fn too_many_tokens(text: &str) -> Option<String> {
    let tokens = token_count(text);
    (tokens > MAX_TOKENS).then(|| {
        format!("has {tokens} tokens, and the parser takes at most {MAX_TOKENS}: split it into smaller rules")
    })
}

/// How deep [`Cx::node`] recurses to match the pattern under `root`: one frame pair a level
/// down the tree, and one more for each run metavariable before a node among its siblings,
/// since each run backtracks in a frame of its own.
fn recursion_depth(root: ast::Node<'_>, marks: &HashMap<NodeId, Mark, Global>, vars: &[Var]) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(root, 1)]; // alloc: one pattern's open nodes, once a load
    while let Some((node, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        let mut runs = 0;
        for child in node.children() {
            stack.push((child, depth + 1 + runs));
            if let Some(Mark::Var(var)) = marks.get(&child.id())
                && vars[*var].many
            {
                runs += 1;
            }
        }
    }
    deepest
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
        let shape = |message: String| -> Error {
            PatternShapeSnafu { line, form: form.name(), text, message }.build().into()
        };
        if let Some(message) = too_many_tokens(text) {
            return Err(shape(message));
        }
        let (body, vars) = placeholders(text.trim(), form, line)?;
        let (before, after) = form.wrap();
        let source = format!("{before}{body}{after}");
        let parse = SourceFile::parse(&source, Edition::CURRENT);
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
        // The pattern, then each def path's last generic arguments, which match by structure
        // and so hold metavariables, wildcards and def paths of their own.
        let mut starts = vec![root]; // alloc: a pattern's def paths with arguments, once a load
        while let Some(start) = starts.pop() {
            let mut walk = start.preorder_with_tokens();
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
                            walk.skip_subtree();
                            starts.extend(last_generic_args(node));
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
        }
        let depth = recursion_depth(root, &marks, &vars);
        if depth > MAX_DEPTH {
            return Err(shape(format!(
                "nests {depth} levels deep, and the matcher takes at most {MAX_DEPTH}: write an inner part \
                 as `$x` and match `x` with a pattern of its own"
            )));
        }
        let tree = |parse: &Parse<SourceFile>| -> Vec<SyntaxKind> {
            // alloc: one pattern's kinds, once an edition a load
            let walk = parse.root().preorder_with_tokens();
            walk.filter_map(|event| match event {
                WalkEvent::Enter(element) => Some(element.kind()),
                WalkEvent::Leave(_) => None,
            })
            .collect()
        };
        let current = tree(&parse);
        let reads_otherwise = Edition::iter()
            .filter(|edition| {
                let other = SourceFile::parse(&source, *edition);
                !other.errors_with_bodies().is_empty() || tree(&other) != current
            })
            .collect();
        Ok(Pattern {
            form,
            text: text.to_owned(),
            line,
            root_kind: root.kind(),
            root: root.id(),
            literal: literal.map(str::to_owned),
            reads_otherwise,
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

    /// `want` against `have`, sibling by sibling. Siblings are a loop and only a run
    /// metavariable recurses, so the stack grows with the pattern's
    /// [`recursion_depth`], never with a sibling list's length.
    fn seq(
        &mut self,
        parent: ast::Node<'t>,
        mut want: &[ast::Element<'_>],
        mut have: &[ast::Element<'t>],
        bound: &mut Bindings<'t>,
    ) -> Result<bool, Error> {
        loop {
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
            (want, have) = (rest, left);
        }
    }
}

/// A type pattern for `type-matches`: `_`, `$name`, a def path with generic arguments
/// (`core::result::Result<_, $e>`), a single name with or without them (`Vec<$a>`, `u32`,
/// `Bag`), `&T`, `&mut T`, `(T)`, `!`, a tuple, a slice or an array whose length is a number
/// or `_` (any length). A single name the std
/// prelude names is that item, resolved; a builtin type's name is that type; any other single
/// name matches a type of that name, by text.
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
    /// A builtin type by its name (`u32`, `str`).
    Builtin(&'p str),
    /// Any other single name and its generic arguments: what the std prelude names by it,
    /// or else a type of that name.
    Named { name: &'p str, args: Vec<ast::Node<'p>> },
    Never,
    Ref { mutable: bool, inner: ast::Node<'p> },
    Tuple(Vec<ast::Node<'p>>),
    Slice(ast::Node<'p>),
    Array(ast::Node<'p>, Length),
}

/// What an array pattern asks of the array's length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Length {
    /// `_`: any length, one the front end evaluates or not (`[T; N]` in a generic fn).
    Any,
    /// A number: that length, evaluated.
    Is(u128),
}

/// The length an array pattern's `; N` asks for: `_` or an integer literal.
fn length(arg: ast::Node<'_>) -> Option<Length> {
    let expr = match arg.kind() {
        SyntaxKind::CONST_ARG => arg.children().next()?,
        _ => arg,
    };
    if expr.kind() == SyntaxKind::UNDERSCORE_EXPR {
        return Some(Length::Any);
    }
    match ast::Literal::cast(expr)?.kind() {
        ast::LiteralKind::IntNumber(number) => match number.value() {
            Ok(value) => Some(Length::Is(value)),
            Err(_) => None,
        },
        _ => None,
    }
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
                Ok(TypeShape::Def(_, args) | TypeShape::Named { args, .. } | TypeShape::Tuple(args)) => {
                    stack.extend(args);
                }
                Ok(TypeShape::Ref { inner, .. } | TypeShape::Slice(inner) | TypeShape::Array(inner, _)) => {
                    stack.push(inner);
                }
                Ok(TypeShape::Wild | TypeShape::Var(_) | TypeShape::Builtin(_) | TypeShape::Never) => {}
                Err(message) => {
                    return PatternShapeSnafu { line, form: "type", text, message }
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

    /// What a node of this pattern asks of a type. [`TypePattern::parse`] refuses every
    /// pattern with a node this refuses.
    ///
    /// # Errors
    ///
    /// What to write instead of `node`, a form the matcher does not read.
    pub fn shape<'p>(&'p self, node: ast::Node<'p>) -> Result<TypeShape<'p>, String> {
        let text = node.text();
        if let Some((name, _)) = placeholder(text) {
            return self.vars.iter().position(|it| it == name).map(TypeShape::Var).ok_or_else(|| {
                // alloc: an error, once a load
                format!("`${name}` is a metavariable the pattern's parse lost: write it as `$name`")
            });
        }
        let readable = "write `_`, `$name`, a def path such as `std::vec::Vec<$a>`, a single name \
                        such as `Vec<$a>` or `u32`, `&T`, `&mut T`, `!`, a tuple, a slice or an array";
        // alloc: each refusal below is an error, once a load
        match node.kind() {
            SyntaxKind::INFER_TYPE => Ok(TypeShape::Wild),
            SyntaxKind::NEVER_TYPE => Ok(TypeShape::Never),
            SyntaxKind::PAREN_TYPE => match node.children().next() {
                Some(inner) => self.shape(inner),
                None => Err(format!("`{text}` holds no type: {readable}")),
            },
            SyntaxKind::PATH_TYPE => self.path_shape(node),
            SyntaxKind::REF_TYPE => match node.children().last() {
                Some(inner) => Ok(TypeShape::Ref {
                    mutable: node.child_token(SyntaxKind::MUT_KW).is_some(),
                    inner,
                }),
                None => Err(format!("`{text}` refers to no type: write `&T` or `&mut T`")),
            },
            SyntaxKind::TUPLE_TYPE => Ok(TypeShape::Tuple(node.children().collect())),
            SyntaxKind::SLICE_TYPE => match node.children().next() {
                Some(inner) => Ok(TypeShape::Slice(inner)),
                None => Err(format!("`{text}` has no element type: write `[T]`")),
            },
            SyntaxKind::ARRAY_TYPE => {
                let mut children = node.children();
                let (Some(inner), Some(len)) = (children.next(), children.next()) else {
                    return Err(format!("`{text}` has no element type or no length: write `[T; 4]` or `[T; _]`"));
                };
                match length(len) {
                    Some(length) => Ok(TypeShape::Array(inner, length)),
                    None => Err(format!(
                        "`{}` in `{text}` is a length `type-matches` does not evaluate: write the \
                         length as a number or `_`",
                        len.text()
                    )),
                }
            }
            SyntaxKind::IMPL_TRAIT_TYPE | SyntaxKind::DYN_TRAIT_TYPE => Err(format!(
                "`{text}` names a trait, not one type: match the type with `$t` and add \
                 `(implements t \"krate::Trait\")`"
            )),
            SyntaxKind::PTR_TYPE | SyntaxKind::FN_PTR_TYPE => Err(format!(
                "`{text}` is a raw pointer or fn pointer type, which `type-matches` does not take \
                 apart: match it whole with `$t` or `_`"
            )),
            _ => Err(format!("`{text}` is no type form `type-matches` reads: {readable}")),
        }
    }

    /// A path type's shape: a def path, a builtin's name, or another single name.
    fn path_shape<'p>(&'p self, node: ast::Node<'p>) -> Result<TypeShape<'p>, String> {
        let text = node.text();
        // alloc: each refusal below is an error, once a load
        let Some(path) = node.children().find(|it| it.kind() == SyntaxKind::PATH) else {
            return Err(format!("`{text}` is a path type with no path: write a def path such as `std::vec::Vec<$a>`"));
        };
        let args = type_args(path).map_err(|arg| {
            format!(
                "`{arg}` in `{text}` is no type argument `type-matches` reads: write a type, `_` or \
                 `$name` for each type argument, and leave lifetimes and associated types out"
            )
        })?;
        if let Some(written) = def_path(path) {
            return match self.paths.iter().position(|it| *it == written) {
                Some(index) => Ok(TypeShape::Def(index, args)),
                None => Err(format!("`{written}` is a def path the pattern's parse did not record")),
            };
        }
        let qualified = path.children().any(|it| it.kind() == SyntaxKind::PATH);
        let name = path
            .children()
            .find(|it| it.kind() == SyntaxKind::PATH_SEGMENT)
            .and_then(|it| it.children().find(|it| it.kind() == SyntaxKind::NAME_REF))
            .map(ast::Node::text);
        match (qualified, name) {
            (false, Some(name)) if placeholder_in(name) => Err(format!(
                "`{text}`: a metavariable stands for a whole type and takes no generic arguments; \
                 write `$t` alone, or a def path with `$name` among its arguments (`std::vec::Vec<$a>`)"
            )),
            (false, Some(name)) if is_builtin(name) => match args.is_empty() {
                true => Ok(TypeShape::Builtin(name)),
                false => Err(format!(
                    "`{text}`: `{name}` is a builtin type and takes no generic arguments; write `{name}`"
                )),
            },
            (false, Some("Self")) => Err(format!(
                "`{text}`: `Self` names no one type outside an impl; write the type's def path"
            )),
            (false, Some(name)) => Ok(TypeShape::Named { name, args }),
            _ => {
                let first = path.descendants().find(|it| it.kind() == SyntaxKind::PATH_SEGMENT);
                Err(match first.map(ast::Node::text) {
                    Some(first @ ("crate" | "self" | "super" | "Self")) => format!(
                        "`{text}` starts at `{first}`: write the def path from the crate's name \
                         (`mycrate::module::Type`), as a crate depending on it would"
                    ),
                    _ => format!(
                        "`{text}` is no def path: write the type's path from its crate's name with \
                         generic arguments only on the last segment (`std::vec::Vec<$a>`), or a \
                         single name (`Vec<$a>`)"
                    ),
                })
            }
        }
    }
}

/// Whether `name` is a builtin type's (`u32`, `str`, `bool`).
fn is_builtin(name: &str) -> bool {
    hir_def::builtin_type::BuiltinType::all_builtin_types().iter().any(|(it, _)| it.as_str() == name)
}

/// The type arguments of a path's last segment, or the text of the first generic argument
/// that is neither a type nor a lifetime (a const, an associated type's binding).
fn type_args(path: ast::Node<'_>) -> Result<Vec<ast::Node<'_>>, &str> {
    let Some(list) = last_generic_args(path) else { return Ok(Vec::new()) };
    let mut args = Vec::new(); // alloc: a pattern path's few arguments, once a match step
    for arg in list.children() {
        match arg.kind() {
            SyntaxKind::TYPE_ARG => args.extend(arg.children().next()),
            SyntaxKind::LIFETIME_ARG => {}
            _ => return Err(arg.text()),
        }
    }
    Ok(args)
}
