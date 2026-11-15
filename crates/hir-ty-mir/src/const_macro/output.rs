//! A const macro's output, a `const_ast::Output` read out of the evaluator ([`FlatOutput`]),
//! made into the token tree hir-expand takes: built into a flat syntax tree (a splice copied
//! from the call's own input arrays), turned into tokens with the span the call's table holds
//! for each, and checked to be the parser's tree of those tokens, node for node.
//!
//! The parser is the grammar's one home: nothing here says which node may hold which. A tree
//! that is not its tokens' parse (a `STRUCT` around `impl`'s tokens, a type where a name goes)
//! is refused at the first element where the two differ, naming the macro and that element of
//! its output. Every number in the output is user code's: each index is checked against the
//! arrays it indexes, each span against the call's table, and the tree built against
//! [`OUTPUT_ELEMENTS`].

use intern::Symbol;
use span::Span;
use syntax::{
    Edition, FlatTree, LexedStr, SyntaxKind, SyntaxTreeBuilder, TextRange, TextSize,
    view::Untyped,
};
use syntax_bridge::{DocCommentDesugarMode, SpanMapper};

use hir_expand::tt;

use crate::mir::{FlatInput, FlatOutput, OUTPUT_ELEMENTS};

/// No output element: the root a list of items or statements is built under.
const NO_ELEMENT: u32 = u32::MAX;

/// The token tree of `output`, the output of the const macro `name`, whose inputs are
/// `inputs` and whose call's span table is `table`; or why it is refused, naming the macro
/// and the element.
pub(super) fn expansion(
    name: &Symbol,
    output: &FlatOutput,
    inputs: &[&FlatInput],
    table: &[Span],
    edition: Edition,
    call_site: Span,
) -> Result<tt::TopSubtree, String> {
    let refuse = |why: String| format!("the const macro `{name}` returned a tree that is refused: {why}");
    let entry = Entry::of(output.entry).ok_or_else(|| {
        refuse(format!("its entry is {}; an entry is 0 to 4", output.entry))
    })?;
    let built = build(output, inputs, table, entry, edition).map_err(refuse)?;
    // The tokens, each with its span, read by the start of its text in the built tree.
    let tokens = syntax_bridge::call_input(
        built.parse.root(),
        Spans { starts: &built.spans, call_site },
        call_site,
        DocCommentDesugarMode::ProcMacro,
    );
    // The check: the tokens parsed at the output's entry are the tree built, node for node.
    let (parsed, _) =
        syntax_bridge::token_tree_to_syntax_node(tokens.view(), entry.parser(), &mut |_| edition);
    if let Some(error) = parsed.errors().first() {
        return Err(refuse(format!(
            "built {} elements that are not {}: {error}",
            output.kinds.len(),
            entry.name()
        )));
    }
    let (built_tree, built_root) = built.parse.root().flat();
    let (parsed_tree, parsed_root) = parsed.root().flat();
    let (ours, theirs) = (shape(built_tree, built_root), shape(parsed_tree, parsed_root));
    for at in 0..ours.len().max(theirs.len()) {
        match (ours.get(at), theirs.get(at)) {
            (Some(ours), Some(theirs)) if ours.depth == theirs.depth && ours.what == theirs.what => {}
            (Some(ours), theirs) => {
                let element = built.origin[ours.at as usize];
                let here = match element {
                    NO_ELEMENT => "as the root".to_owned(), // alloc: error path
                    element => format!("at output element {element}"),
                };
                let theirs = match theirs {
                    Some(it) => it.describe(),
                    None => "nothing more".to_owned(), // alloc: error path
                };
                return Err(refuse(format!(
                    "built {} {here} where its tokens parse to {theirs}",
                    ours.describe()
                )));
            }
            (None, Some(theirs)) => {
                return Err(refuse(format!(
                    "built nothing more where its tokens parse to {}",
                    theirs.describe()
                )));
            }
            (None, None) => unreachable!("`at` is below the longer shape's length"),
        }
    }
    Ok(tokens)
}

/// What an output is parsed as: `const_ast::Entry`, by its number.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    Items,
    Stmts,
    Expr,
    Type,
    Pat,
}

impl Entry {
    fn of(number: u8) -> Option<Entry> {
        Some(match number {
            0 => Entry::Items,
            1 => Entry::Stmts,
            2 => Entry::Expr,
            3 => Entry::Type,
            4 => Entry::Pat,
            _ => return None,
        })
    }

    fn parser(self) -> parser::TopEntryPoint {
        match self {
            Entry::Items => parser::TopEntryPoint::MacroItems,
            Entry::Stmts => parser::TopEntryPoint::MacroStmts,
            Entry::Expr => parser::TopEntryPoint::Expr,
            Entry::Type => parser::TopEntryPoint::Type,
            Entry::Pat => parser::TopEntryPoint::Pattern,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Entry::Items => "items",
            Entry::Stmts => "statements",
            Entry::Expr => "an expression",
            Entry::Type => "a type",
            Entry::Pat => "a pattern",
        }
    }

    /// The node the output's elements are the children of: a list of items or statements has
    /// one; an expression, a type or a pattern is the output's one top-level node.
    fn root(self) -> Option<SyntaxKind> {
        match self {
            Entry::Items => Some(SyntaxKind::MACRO_ITEMS),
            Entry::Stmts => Some(SyntaxKind::MACRO_STMTS),
            Entry::Expr | Entry::Type | Entry::Pat => None,
        }
    }
}

/// The flat tree an output is, built.
struct Built {
    parse: syntax::Parse<Untyped>,
    /// The output element each element of the tree was written for: the element itself, the
    /// splice it was copied for, the token a separating space goes before, or [`NO_ELEMENT`]
    /// for the root of a list.
    origin: Vec<u32>,
    /// Where each token's text starts in the tree's text, with its span, in order: whitespace
    /// has none, and is never asked for one.
    spans: Vec<(TextSize, Span)>,
}

/// The output's elements built into a flat tree: every kind, size, span and splice checked
/// against what it indexes.
fn build(
    output: &FlatOutput,
    inputs: &[&FlatInput],
    table: &[Span],
    entry: Entry,
    edition: Edition,
) -> Result<Built, String> {
    let len = output.kinds.len() as u32;
    if [output.sizes.len(), output.spans.len(), output.text_ends.len()]
        .iter()
        .any(|&it| it != len as usize)
    {
        // alloc: error path
        return Err("its arrays are not as long as each other".to_owned());
    }
    let mut out = Writer {
        builder: SyntaxTreeBuilder::default(),
        // alloc: the tree's side tables, dropped once the expansion is made
        origin: Vec::with_capacity(len as usize * 2),
        spans: Vec::with_capacity(len as usize * 2),
        offset: 0,
        separate: false,
    };
    match entry.root() {
        Some(root) => {
            out.builder.start_node(root);
            out.origin.push(NO_ELEMENT);
        }
        None => {
            if len == 0 || output.sizes[0] != len {
                return Err(format!(
                    "{} is one node; it built {len} elements, {} of them at the top",
                    entry.name(),
                    top_level(&output.sizes)
                ));
            }
            if output.kinds[0] == const_ast::TOKEN {
                return Err(format!("{} is one node; it built a token", entry.name()));
            }
        }
    }
    // The ends of the nodes open, innermost last.
    let mut open: Vec<u32> = Vec::new(); // alloc: as deep as the output
    for i in 0..len {
        while open.last() == Some(&i) {
            out.builder.finish_node();
            open.pop();
        }
        let (kind, size, span) =
            (output.kinds[i as usize], output.sizes[i as usize], output.spans[i as usize]);
        let parent_end = open.last().copied().unwrap_or(len);
        let end = u64::from(i) + u64::from(size);
        if size == 0 || end > u64::from(parent_end) {
            return Err(format!(
                "element {i} ends at {end}, past its parent, which ends at {parent_end}"
            ));
        }
        if kind == const_ast::TOKEN {
            let text = text_of(output, i);
            if size != 1 {
                return Err(format!("token {i} `{text}` has size {size}; a token has 1"));
            }
            let Some(&at) = table.get(span as usize) else {
                return Err(format!(
                    "token {i} `{text}` has span {span}; the call's table has {}",
                    table.len()
                ));
            };
            let Some(token) = token_kind(text, edition) else {
                return Err(format!("token {i} `{text}` is not one token"));
            };
            out.separate(i);
            out.token(token, text, Some(at), i);
        } else if kind & const_ast::TOKEN != 0 {
            return Err(format!("element {i} has kind {kind:#06x}, which is no node's"));
        } else if kind & const_ast::SPLICE != 0 {
            let input = u32::from(kind & !const_ast::SPLICE);
            if size != 1 {
                return Err(format!("element {i} splices and has size {size}; a splice has 1"));
            }
            let Some(&from) = inputs.get(input as usize) else {
                return Err(format!(
                    "element {i} splices input {input}; the call has {}",
                    inputs.len()
                ));
            };
            if span as usize >= from.kinds.len() {
                return Err(format!(
                    "element {i} splices element {span} of input {input}, which has {}",
                    from.kinds.len()
                ));
            }
            if i == 0 && entry.root().is_none() && from.kinds[span as usize] & const_ast::TOKEN != 0
            {
                return Err(format!("{} is one node; it spliced a token", entry.name()));
            }
            out.separate(i);
            out.splice(from, span, table, i)?;
        } else {
            let node = SyntaxKind::from(kind_below_last(kind).ok_or_else(|| no_node(i, kind))?);
            if kind < SyntaxKind::ABI as u16 || node.fixed_text().is_some() {
                return Err(no_node(i, kind));
            }
            out.builder.start_node(node);
            out.origin.push(i);
            open.push(end as u32);
        }
        if out.origin.len() > OUTPUT_ELEMENTS {
            return Err(format!("built more than {OUTPUT_ELEMENTS} elements"));
        }
    }
    for _ in open {
        out.builder.finish_node();
    }
    if entry.root().is_some() {
        out.builder.finish_node();
    }
    Ok(Built { parse: out.builder.finish_view(), origin: out.origin, spans: out.spans })
}

/// `kind` if it is a `SyntaxKind`'s number.
fn kind_below_last(kind: u16) -> Option<u16> {
    (kind < SyntaxKind::__LAST as u16).then_some(kind)
}

/// The refusal of element `i`'s kind: a node kind is one from `ABI` on whose `text()` is
/// empty (eight contextual keywords are numbered after the nodes, and are tokens).
fn no_node(i: u32, kind: u16) -> String {
    format!("element {i} has kind {kind}, which is no node's")
}

/// How many elements are at the top of an output of `sizes`.
fn top_level(sizes: &[u32]) -> usize {
    let mut at = 0usize;
    let mut count = 0;
    while at < sizes.len() && sizes[at] != 0 {
        at += sizes[at] as usize;
        count += 1;
    }
    count
}

/// The text of output element `i`.
fn text_of(output: &FlatOutput, i: u32) -> &str {
    let end = output.text_ends[i as usize] as usize;
    let start = match i {
        0 => 0,
        _ => output.text_ends[i as usize - 1] as usize,
    };
    // `read_output` wrote the ends in order, each at a char boundary of `text`.
    output.text.get(start..end).unwrap_or("")
}

/// The kind of `text` as one token, if it is one: what the lexer makes of it alone, or the
/// punctuation whose text it is (`::`, `->`, `..=`, which the lexer gives as single
/// characters and the parser joins). Whitespace, comments and errors are no token a macro
/// writes.
fn token_kind(text: &str, edition: Edition) -> Option<SyntaxKind> {
    if let Some((kind, error)) = LexedStr::single_token(edition, text) {
        return match error.is_none() && !kind.is_trivia() && kind != SyntaxKind::ERROR {
            true => Some(kind),
            false => None,
        };
    }
    (0..SyntaxKind::__LAST as u16)
        .map(SyntaxKind::from)
        .find(|kind| kind.is_punct() && kind.text() == text)
}

/// Writes the built tree and its side tables.
struct Writer {
    builder: SyntaxTreeBuilder,
    origin: Vec<u32>,
    spans: Vec<(TextSize, Span)>,
    /// Where the next token's text starts.
    offset: u32,
    /// Whether a token or a splice was written: the next one gets a space before it.
    separate: bool,
}

impl Writer {
    /// A space before the token or splice of output element `i`, unless it is the first: two
    /// built punctuation tokens are never glued by being adjacent (the bridge reads a punct's
    /// spacing from the token after it).
    fn separate(&mut self, i: u32) {
        if std::mem::replace(&mut self.separate, true) {
            self.token(SyntaxKind::WHITESPACE, " ", None, i);
        }
    }

    /// A token of `kind` and `text`, with `span` unless it is whitespace, for output element
    /// `i`.
    fn token(&mut self, kind: SyntaxKind, text: &str, span: Option<Span>, i: u32) {
        self.builder.token(kind, text);
        self.origin.push(i);
        if let Some(span) = span {
            self.spans.push((TextSize::from(self.offset), span));
        }
        self.offset += text.len() as u32;
    }

    /// Element `at` of `input` with its subtree, copied for output element `i`, each token's
    /// span the table's.
    fn splice(
        &mut self,
        input: &FlatInput,
        at: u32,
        table: &[Span],
        i: u32,
    ) -> Result<(), String> {
        let end = input.ends[at as usize];
        if end <= at || end as usize > input.kinds.len() {
            return Err(format!("element {i} splices element {at}, which has no subtree"));
        }
        let mut open: Vec<u32> = Vec::new(); // alloc: as deep as the spliced subtree
        for j in at..end {
            while open.last() == Some(&j) {
                self.builder.finish_node();
                open.pop();
            }
            let kind = input.kinds[j as usize];
            let raw = kind & !const_ast::TOKEN;
            let Some(raw) = kind_below_last(raw) else {
                return Err(format!("element {i} splices element {j}, of no kind"));
            };
            let syntax = SyntaxKind::from(raw);
            if kind & const_ast::TOKEN != 0 {
                let start = match j {
                    0 => 0,
                    _ => input.text_ends[j as usize - 1] as usize,
                };
                let text = input.text.get(start..input.text_ends[j as usize] as usize);
                let span = table.get(input.spans[j as usize] as usize).copied();
                let (Some(text), Some(span)) = (text, span) else {
                    return Err(format!(
                        "element {i} splices element {j}, whose text or span is not the call's"
                    ));
                };
                let span = (!syntax.is_trivia()).then_some(span);
                self.token(syntax, text, span, i);
            } else {
                self.builder.start_node(syntax);
                self.origin.push(i);
                open.push(input.ends[j as usize]);
            }
        }
        for _ in open {
            self.builder.finish_node();
        }
        Ok(())
    }
}

/// The span of each token of a built tree: the entry at or before where its text starts, so
/// each character of a punctuation (`::` is two puncts) and both halves of a lifetime have the
/// token's span.
struct Spans<'a> {
    starts: &'a [(TextSize, Span)],
    /// What a range before every token gets; never asked, since every token that is not
    /// whitespace has an entry and the bridge asks no span of whitespace.
    call_site: Span,
}

impl SpanMapper for Spans<'_> {
    fn span_for(&self, range: TextRange) -> Span {
        let after = self.starts.partition_point(|&(start, _)| start <= range.start());
        match after {
            0 => self.call_site,
            after => self.starts[after - 1].1,
        }
    }
}

/// One element of a tree's shape: what the check compares.
#[derive(PartialEq, Eq)]
enum What<'t> {
    Node(SyntaxKind),
    Token(&'t str),
}

/// An element of a tree's shape at its depth, and the tree's element it is of.
struct Shaped<'t> {
    at: u32,
    depth: u32,
    what: What<'t>,
}

impl Shaped<'_> {
    fn describe(&self) -> String {
        match self.what {
            What::Node(kind) => format!("`{kind:?}` (depth {})", self.depth),
            What::Token(text) => format!("the token `{text}` (depth {})", self.depth),
        }
    }
}

/// The shape of the subtree of `root` in `tree`: each node's kind and each token's text, at its
/// depth, in preorder, with no whitespace. Inside a `TOKEN_TREE` only tokens count, each
/// punctuation one character at a time: a token tree is tokens to the parser, however they
/// were grouped.
fn shape(tree: &FlatTree, root: u32) -> Vec<Shaped<'_>> {
    // alloc: the check's two shapes, dropped once compared
    let mut shape = Vec::with_capacity((tree.end(root) - root) as usize);
    let mut open: Vec<u32> = Vec::new(); // alloc: as deep as the tree
    // The end of the outermost token tree the walk is in, and its depth.
    let mut token_tree: Option<(u32, u32)> = None;
    for at in root..tree.end(root) {
        while open.last() == Some(&at) {
            open.pop();
        }
        if token_tree.is_some_and(|(end, _)| at >= end) {
            token_tree = None;
        }
        let kind = SyntaxKind::from(tree.kind(at).0);
        let depth = open.len() as u32;
        if tree.is_token(at) {
            if kind.is_trivia() {
                continue;
            }
            let text = tree.token_text(at);
            match token_tree {
                Some((_, depth)) if kind.is_punct() => {
                    for (start, char) in text.char_indices() {
                        let text = &text[start..start + char.len_utf8()];
                        shape.push(Shaped { at, depth: depth + 1, what: What::Token(text) });
                    }
                }
                Some((_, depth)) => {
                    shape.push(Shaped { at, depth: depth + 1, what: What::Token(text) })
                }
                None => shape.push(Shaped { at, depth, what: What::Token(text) }),
            }
            continue;
        }
        open.push(tree.end(at));
        match token_tree {
            Some(_) => {}
            None => {
                if kind == SyntaxKind::TOKEN_TREE {
                    token_tree = Some((tree.end(at), depth));
                }
                shape.push(Shaped { at, depth, what: What::Node(kind) });
            }
        }
    }
    shape
}
