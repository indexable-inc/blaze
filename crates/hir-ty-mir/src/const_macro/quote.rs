//! `const_ast::quote!`: what a const macro's author writes its output with, the library on
//! `const_ast::Builder` (docs/future/const-macros.md, "The tree API in const").
//!
//! `quote!(b, items => impl Named for #name { const LEN: usize = 5; })` is a function-like
//! macro of the proc-macro crate `const_ast_quote`, which blaze makes from `const_ast`'s own
//! root file; its expander is [`QuoteExpander`], host code run when the macro crate is
//! analyzed. It parses the quoted tokens with blaze's parser at the entry the call names
//! (`items`, `stmts`, `expr`, `ty` or `pat`), so a quote that is not Rust is an error where
//! the macro is written, and lowers the parse to calls on the builder `b` (a
//! `&mut const_ast::Builder`): `b.open(::const_ast::Kind::IMPL)` for a node, `b.token("impl")`
//! for a token, `b.close(..)`. The parser stays the grammar's one home: nothing here knows
//! which node holds which.
//!
//! In the tokens, `#x` splices the local `x` (a view of the input, or anything with a
//! `const fn splice(self, &mut Builder, &[Kind])`): `x.splice(b, &[..])`, given the kinds of
//! the nodes the quote's parse put around the placeholder, which a token's splice (a name)
//! writes around it and a node's splice ignores. `#{ CODE }` runs the statements `CODE` there;
//! `#for x in (E) { BODY }` writes `BODY` once for each `x` the cursor `E` gives, and `BODY`
//! must be whole nodes of one parent; `##` is `#`.

use std::any::Any;
use std::sync::Arc;

use blaze_hash::HashMap;
use hir_expand::{
    parse_to_token_tree_static_span,
    proc_macro::{
        Expanded, ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError,
        ProcMacroKind,
    },
    tt::{self, TtElement},
};
use intern::Symbol;
use span::Span;
use std::alloc::Global;
use syntax::{Edition, NodeOrToken, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

/// The expander of `const_ast::quote!`: [`lower`] of its call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteExpander;

/// `quote`, the one macro of the crate `const_ast_quote`, which blaze gives that crate.
pub fn proc_macro() -> ProcMacro {
    ProcMacro {
        name: Symbol::intern("quote"),
        kind: ProcMacroKind::Bang,
        expander: Arc::new(QuoteExpander), // alloc: once per macro graph and platform, its crate's one macro
        disabled: false,
    }
}

impl ProcMacroExpander for QuoteExpander {
    fn expand(
        &self,
        _db: &base_db::Db,
        ProcMacroCall { subtree, call_site, mixed_site, .. }: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        // It reads its call and nothing else, and keeps no state.
        lower(subtree, call_site, mixed_site)
            .map(Expanded::reads_nothing)
            .map_err(ProcMacroExpansionError::Panic)
    }

    // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any).downcast_ref::<Self>() == Some(self) // dyn: the downcast `eq_dyn` needs
    }
}

/// The call `B, ENTRY => TOKENS` lowered to one block of calls on the builder `B`, the
/// pieces this writes lexed at `mixed_site` (so its locals, `__nI` and `__cK`, are its own) and
/// `B`, the spliced locals, the cursors and the holes' code the caller's own tokens, with their
/// spans. `call_site` anchors the numbers it gives the quoted tokens to find them after the
/// parse; none of those spans is in the result.
pub(crate) fn lower(
    input: &tt::TopSubtree,
    call_site: Span,
    mixed_site: Span,
) -> Result<tt::TopSubtree, String> {
    let mut iter = input.view().token_trees().iter();
    let builder = match iter.next() {
        Some(TtElement::Leaf(tt::Leaf::Ident(ident))) => ident,
        _ => return Err(header("its first token is the builder, one identifier")),
    };
    if !matches!(iter.next(), Some(TtElement::Leaf(tt::Leaf::Punct(p))) if p.char == ',') {
        return Err(header("the builder is followed by `,`"));
    }
    let entry = match iter.next() {
        Some(TtElement::Leaf(tt::Leaf::Ident(ident))) => ident,
        _ => return Err(header("the entry follows the builder: items, stmts, expr, ty or pat")),
    };
    let (entry_point, entry_name) = match entry.sym.as_str() {
        "items" => (parser::TopEntryPoint::MacroItems, "items"),
        "stmts" => (parser::TopEntryPoint::MacroStmts, "statements"),
        "expr" => (parser::TopEntryPoint::Expr, "an expression"),
        "ty" => (parser::TopEntryPoint::Type, "a type"),
        "pat" => (parser::TopEntryPoint::Pattern, "a pattern"),
        other => {
            return Err(format!(
                "const_ast::quote!: the entry `{other}` is none of items, stmts, expr, ty, pat"
            ));
        }
    };
    let arrow = (iter.next(), iter.next());
    if !matches!(
        arrow,
        (Some(TtElement::Leaf(tt::Leaf::Punct(eq))), Some(TtElement::Leaf(tt::Leaf::Punct(gt))))
            if eq.char == '=' && gt.char == '>'
    ) {
        return Err(header("the entry is followed by `=>`"));
    }

    // 1. The quoted tokens, numbered, holes and repetitions taken out.
    let mut quoted = Quoted {
        tt: tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(call_site)),
        call_site,
        count: 0,
        marks: Vec::new(),   // alloc: the quote's holes and repetitions, while it lowers
        splices: Vec::new(), // alloc: the quote's splices, while it lowers
        splice_at: HashMap::new_in(Global),
    };
    quoted.rewrite(iter)?;
    let Quoted { tt: numbered, count, marks, splices, splice_at, .. } = quoted;
    let numbered = numbered.build();

    // 2. The parse, as the macro's output will be parsed.
    let (parse, span_map) = syntax_bridge::token_tree_to_syntax_node(
        numbered.view(),
        entry_point,
        &mut |_| Edition::CURRENT,
    );
    if let Some(error) = parse.errors().first() {
        return Err(format!("const_ast::quote!: the tokens are not {entry_name}: {error}"));
    }
    let root = parse.syntax_node();
    let numbers = |range: tt::TextRange| -> (u32, u32) {
        if range.is_empty() {
            return (u32::MAX, u32::MAX);
        }
        let first = span_map.span_at(range.start()).range.start();
        let last = span_map.span_at(range.end() - tt::TextSize::new(1)).range.end();
        (u32::from(first), u32::from(last) - 1)
    };

    // 3. The lowering.
    let mut lowering = Lowering {
        out: Out::new(mixed_site),
        builder,
        marks,
        splices,
        splice_at,
        numbers,
        nodes: 0,
        cursors: 0,
    };
    lowering.out.tt.open(tt::DelimiterKind::Brace, mixed_site);
    match entry.sym.as_str() {
        "items" | "stmts" => {
            let children = significant(&root);
            lowering.children(&children)?;
        }
        _ => lowering.children(&[NodeOrToken::Node(root)])?,
    }
    while let Some(at) = lowering.next_mark_at(count) {
        lowering.mark(at, &[])?;
    }
    if let Some(left) = lowering.marks.iter().find(|it| !it.done) {
        let what = match left.event {
            Event::Hole { .. } => "the hole `#{ .. }`",
            Event::Repeat { .. } => "the `#for`",
        };
        return Err(format!(
            "const_ast::quote!: {what} before quoted token {} is inside one token of the parse",
            left.at
        ));
    }
    lowering.out.tt.close(mixed_site);
    Ok(lowering.out.tt.build())
}

/// An error in the call's head, `B, ENTRY =>`.
fn header(rule: &str) -> String {
    format!("const_ast::quote!(b, items => ..): {rule}")
}

/// What the quoted tokens hold besides tokens, at the number of the token after it.
struct Mark<'a> {
    at: u32,
    event: Event<'a>,
    done: bool,
}

enum Event<'a> {
    /// `#{ CODE }`.
    Hole { code: tt::TokenTreesView<'a> },
    /// `#for var in (cursor) { .. }`, whose body's tokens are `at ..= last`.
    Repeat { var: tt::Ident, cursor: tt::TokenTreesView<'a>, last: u32 },
}

/// The quoted tokens being numbered: each leaf and delimiter gets the span
/// `call_site` at `n .. n + 1`, its number.
struct Quoted<'a> {
    tt: tt::TopSubtreeBuilder,
    call_site: Span,
    count: u32,
    /// In the order they are written, so that two at one number keep it.
    marks: Vec<Mark<'a>>,
    /// Each `#x`'s `x`, by `K` of its placeholder `__qK`.
    splices: Vec<tt::Ident>,
    /// The placeholders' numbers, to `K`.
    splice_at: HashMap<u32, usize, Global>,
}

impl<'a> Quoted<'a> {
    fn next_span(&mut self) -> Span {
        let at = tt::TextSize::new(self.count);
        self.count += 1;
        Span { range: tt::TextRange::new(at, at + tt::TextSize::new(1)), ..self.call_site }
    }

    fn rewrite(&mut self, mut iter: tt::TtIter<'a>) -> Result<(), String> {
        while let Some(element) = iter.next() {
            match element {
                TtElement::Leaf(tt::Leaf::Punct(punct)) if punct.char == '#' => {
                    self.special(&mut iter)?;
                }
                TtElement::Leaf(leaf) => {
                    let span = self.next_span();
                    self.tt.push(respan(leaf, span));
                }
                TtElement::Subtree(subtree, inner) => {
                    if subtree.delimiter.kind == tt::DelimiterKind::Invisible {
                        // No token of the parse: numbered, it would hold a hole no token starts.
                        self.tt.open(tt::DelimiterKind::Invisible, self.call_site);
                        self.rewrite(inner)?;
                        self.tt.close(self.call_site);
                        continue;
                    }
                    let open = self.next_span();
                    self.tt.open(subtree.delimiter.kind, open);
                    self.rewrite(inner)?;
                    let close = self.next_span();
                    self.tt.close(close);
                }
            }
        }
        Ok(())
    }

    /// What follows a `#`.
    fn special(&mut self, iter: &mut tt::TtIter<'a>) -> Result<(), String> {
        match iter.next() {
            Some(TtElement::Leaf(tt::Leaf::Punct(punct))) if punct.char == '#' => {
                let span = self.next_span();
                self.tt.push(tt::Leaf::Punct(tt::Punct { span, ..punct }));
            }
            Some(TtElement::Leaf(tt::Leaf::Ident(ident)))
                if ident.sym.as_str() == "for" && ident.is_raw.no() =>
            {
                self.repeat(iter)?;
            }
            Some(TtElement::Leaf(tt::Leaf::Ident(var))) => {
                let k = self.splices.len();
                let span = self.next_span();
                self.splice_at.insert(self.count - 1, k);
                self.tt.push(tt::Leaf::Ident(tt::Ident {
                    sym: Symbol::intern(&format!("__q{k}")), // alloc: the placeholder's name, interned
                    span,
                    is_raw: tt::IdentIsRaw::No,
                }));
                self.splices.push(var);
            }
            Some(TtElement::Subtree(subtree, inner))
                if subtree.delimiter.kind == tt::DelimiterKind::Brace =>
            {
                let at = self.count;
                self.marks.push(Mark { at, event: Event::Hole { code: inner.remaining() }, done: false });
            }
            Some(other) => {
                return Err(format!(
                    "const_ast::quote!: `#` is followed by `{}`; it takes `#name`, `#{{ code }}`, \
                     `#for x in (cursor) {{ .. }}` or `##`",
                    first_text(&other)
                ));
            }
            None => {
                return Err("const_ast::quote!: `#` ends the tokens; it takes `#name`, \
                            `#{ code }`, `#for x in (cursor) { .. }` or `##`"
                    .to_owned());
            }
        }
        Ok(())
    }

    /// `#for var in (cursor) { body }`, after `for`.
    fn repeat(&mut self, iter: &mut tt::TtIter<'a>) -> Result<(), String> {
        const FORM: &str = "const_ast::quote!: `#for` is `#for x in (cursor) { .. }`";
        let Some(TtElement::Leaf(tt::Leaf::Ident(var))) = iter.next() else {
            return Err(format!("{FORM}: `for` is followed by the name of each element"));
        };
        if !matches!(iter.next(), Some(TtElement::Leaf(tt::Leaf::Ident(it))) if it.sym.as_str() == "in")
        {
            return Err(format!("{FORM}: `#for {}` is followed by `in`", var.sym.as_str()));
        }
        let cursor = match iter.next() {
            Some(TtElement::Subtree(subtree, inner))
                if subtree.delimiter.kind == tt::DelimiterKind::Parenthesis =>
            {
                inner.remaining()
            }
            _ => return Err(format!("{FORM}: the cursor of `#for {}` is in parentheses", var.sym.as_str())),
        };
        let body = match iter.next() {
            Some(TtElement::Subtree(subtree, inner))
                if subtree.delimiter.kind == tt::DelimiterKind::Brace =>
            {
                inner
            }
            _ => return Err(format!("{FORM}: the body of `#for {}` is in braces", var.sym.as_str())),
        };
        let at = self.count;
        let mark = self.marks.len();
        let name = var.sym.as_str().to_owned();
        self.marks.push(Mark { at, event: Event::Repeat { var, cursor, last: at }, done: false });
        self.rewrite(body)?;
        if self.count == at {
            return Err(format!("const_ast::quote!: the body of `#for {name}` writes no token"));
        }
        if let Event::Repeat { last, .. } = &mut self.marks[mark].event {
            *last = self.count - 1;
        }
        Ok(())
    }
}

/// `leaf` with the span `span`.
fn respan(leaf: tt::Leaf, span: Span) -> tt::Leaf {
    match leaf {
        tt::Leaf::Literal(it) => tt::Leaf::Literal(tt::Literal { span, ..it }),
        tt::Leaf::Punct(it) => tt::Leaf::Punct(tt::Punct { span, ..it }),
        tt::Leaf::Ident(it) => tt::Leaf::Ident(tt::Ident { span, ..it }),
    }
}

/// The first token of `element`, as text, for an error.
fn first_text(element: &TtElement<'_>) -> String {
    match element {
        TtElement::Leaf(leaf) => leaf.to_string(),
        TtElement::Subtree(subtree, _) => match subtree.delimiter.kind {
            tt::DelimiterKind::Parenthesis => "(".to_owned(),
            tt::DelimiterKind::Bracket => "[".to_owned(),
            tt::DelimiterKind::Brace => "{".to_owned(),
            tt::DelimiterKind::Invisible => "an invisible group".to_owned(),
        },
    }
}

/// The children of `node` that are not whitespace.
fn significant(node: &SyntaxNode) -> Vec<SyntaxElement> {
    // alloc: one node's children, while it is lowered
    node.children_with_tokens().filter(|it| it.kind() != SyntaxKind::WHITESPACE).collect()
}

/// The tokens the lowering writes.
struct Out {
    tt: tt::TopSubtreeBuilder,
    mixed_site: Span,
}

impl Out {
    fn new(mixed_site: Span) -> Out {
        Out { tt: tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(mixed_site)), mixed_site }
    }

    /// `text`, lexed at `mixed_site`.
    fn text(&mut self, text: &str) -> Result<(), String> {
        let lexed = parse_to_token_tree_static_span(Edition::CURRENT, self.mixed_site, text)
            .ok_or_else(|| format!("const_ast::quote!: the lowering wrote `{text}`, which is not tokens"))?;
        let view = lexed.view();
        match view.top_subtree().delimiter.kind {
            tt::DelimiterKind::Invisible => self.tt.extend_with_tt(view.token_trees()),
            _ => self.tt.extend_with_tt(view.as_token_trees()),
        }
        Ok(())
    }

    fn leaf(&mut self, ident: &tt::Ident) {
        self.tt.push(tt::Leaf::Ident(ident.clone())); // clone: a symbol and a span
    }

    /// The caller's tokens, as they are.
    fn trees(&mut self, trees: tt::TokenTreesView<'_>) {
        self.tt.extend_with_tt(trees);
    }
}

/// The parse being lowered.
struct Lowering<'a, F> {
    out: Out,
    builder: tt::Ident,
    marks: Vec<Mark<'a>>,
    splices: Vec<tt::Ident>,
    splice_at: HashMap<u32, usize, Global>,
    /// The first and last quoted token's numbers of a text range of the parse.
    numbers: F,
    nodes: u32,
    cursors: u32,
}

impl<F: Fn(tt::TextRange) -> (u32, u32)> Lowering<'_, F> {
    fn range(&self, element: &SyntaxElement) -> (u32, u32) {
        (self.numbers)(element.text_range())
    }

    /// The first mark not yet written whose number is `at`, in the order they were written.
    fn next_mark_at(&self, at: u32) -> Option<usize> {
        self.marks.iter().position(|it| !it.done && it.at == at)
    }

    /// `children`, consecutive children of one node: the marks at each one's first token
    /// before it, outermost first, and each as itself.
    fn children(&mut self, children: &[SyntaxElement]) -> Result<(), String> {
        let mut i = 0;
        'children: while i < children.len() {
            let (first, last) = self.range(&children[i]);
            while let Some(at) = self.next_mark_at(first) {
                if let Event::Repeat { last: body_last, .. } = self.marks[at].event
                    && last > body_last
                {
                    // The child holds the body and more: the body is inside it.
                    if let NodeOrToken::Token(token) = &children[i] {
                        return Err(format!(
                            "const_ast::quote!: a `#for` body must be whole nodes of one parent; \
                             it ends inside the token `{}`",
                            token.text()
                        ));
                    }
                    break;
                }
                let used = self.mark(at, &children[i..])?;
                if used > 0 {
                    i += used;
                    continue 'children;
                }
            }
            self.element(&children[i])?;
            i += 1;
        }
        Ok(())
    }

    /// Writes the mark `at` before `run[0]`: a hole's code, or a repetition of the run of
    /// `run` that is its body. How many of `run` it wrote.
    fn mark(&mut self, at: usize, run: &[SyntaxElement]) -> Result<usize, String> {
        self.marks[at].done = true;
        match self.marks[at].event {
            Event::Hole { code } => {
                self.out.trees(code);
                Ok(0)
            }
            Event::Repeat { ref var, cursor, last } => {
                // clone: a symbol and a span, written into the loop's head
                let var = var.clone();
                let mut end = None;
                for (j, element) in run.iter().enumerate() {
                    let (_, element_last) = self.range(element);
                    if element_last == last {
                        end = Some(j + 1);
                        break;
                    }
                    if element_last > last {
                        let inside = match element {
                            NodeOrToken::Node(node) => format!("{:?}", node.kind()),
                            NodeOrToken::Token(token) => format!("the token `{}`", token.text()),
                        };
                        return Err(format!(
                            "const_ast::quote!: a `#for` body must be whole nodes of one parent; \
                             it ends inside {inside}"
                        ));
                    }
                }
                let Some(end) = end else {
                    let parent = run.first().and_then(|it| it.parent());
                    let outside = match parent {
                        Some(parent) => format!("{:?}", parent.kind()),
                        None => "the quote".to_owned(),
                    };
                    return Err(format!(
                        "const_ast::quote!: a `#for` body must be whole nodes of one parent; \
                         it starts inside {outside} and ends after it"
                    ));
                };
                let k = self.cursors;
                self.cursors += 1;
                let mixed = self.out.mixed_site;
                self.out.tt.open(tt::DelimiterKind::Brace, mixed);
                self.out.text(&format!("let mut __c{k} ="))?; // alloc: one piece's text
                self.out.tt.open(tt::DelimiterKind::Parenthesis, mixed);
                self.out.trees(cursor);
                self.out.tt.close(mixed);
                self.out.text(";while let Some")?;
                self.out.tt.open(tt::DelimiterKind::Parenthesis, mixed);
                self.out.leaf(&var);
                self.out.tt.close(mixed);
                self.out.text(&format!("= __c{k}.next()"))?; // alloc: one piece's text
                self.out.tt.open(tt::DelimiterKind::Brace, mixed);
                self.children(&run[..end])?;
                self.out.tt.close(mixed);
                self.out.tt.close(mixed);
                Ok(end)
            }
        }
    }

    /// One element of the parse, as itself.
    fn element(&mut self, element: &SyntaxElement) -> Result<(), String> {
        match element {
            NodeOrToken::Token(token) => self.token(token),
            NodeOrToken::Node(node) => {
                if let Some((k, wrap)) = self.splice_of(node)? {
                    return self.splice(k, &wrap);
                }
                let kind = node_kind(node.kind())?;
                let n = self.nodes;
                self.nodes += 1;
                self.out.text(&format!("let __n{n} ="))?; // alloc: one piece's text, lexed at once
                self.out.leaf(&self.builder);
                self.out.text(&format!(".open(::const_ast::Kind::{kind});"))?; // alloc: as above
                self.children(&significant(node))?;
                self.out.leaf(&self.builder);
                self.out.text(&format!(".close(__n{n});"))?; // alloc: as above
                Ok(())
            }
        }
    }

    fn token(&mut self, token: &SyntaxToken) -> Result<(), String> {
        let (first, _) = (self.numbers)(token.text_range());
        if let Some(&k) = self.splice_at.get(&first) {
            return self.splice(k, &[]);
        }
        self.out.leaf(&self.builder);
        self.out.text(&format!(".token({:?});", token.text())) // alloc: one piece's text, lexed at once
    }

    /// When `node`'s one token is a placeholder `__qK`: `K`, and the kinds from `node` down
    /// to the token's parent.
    fn splice_of(&self, node: &SyntaxNode) -> Result<Option<(usize, Vec<String>)>, String> {
        let mut tokens = node
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|it| it.kind() != SyntaxKind::WHITESPACE);
        let (Some(token), None) = (tokens.next(), tokens.next()) else { return Ok(None) };
        let (first, _) = (self.numbers)(token.text_range());
        let Some(&k) = self.splice_at.get(&first) else { return Ok(None) };
        let mut wrap = vec![node_kind(node.kind())?]; // alloc: a splice's few kinds
        let mut at = node.clone(); // clone: a handle on the parse
        loop {
            let inner = significant(&at)
                .into_iter()
                .find(|it| it.text_range().contains_range(token.text_range()) && !it.text_range().is_empty());
            match inner {
                Some(NodeOrToken::Node(child)) => {
                    wrap.push(node_kind(child.kind())?);
                    at = child;
                }
                Some(NodeOrToken::Token(_)) | None => break,
            }
        }
        Ok(Some((k, wrap)))
    }

    /// `x.splice(b, &[::const_ast::Kind::A, ..]);` for the placeholder `__qK`.
    fn splice(&mut self, k: usize, wrap: &[String]) -> Result<(), String> {
        self.out.leaf(&self.splices[k]);
        self.out.text(".splice")?;
        let mixed = self.out.mixed_site;
        self.out.tt.open(tt::DelimiterKind::Parenthesis, mixed);
        self.out.leaf(&self.builder);
        // alloc: one piece's text, lexed at once
        let kinds: Vec<String> = wrap.iter().map(|it| format!("::const_ast::Kind::{it}")).collect();
        self.out.text(&format!(", &[{}]", kinds.join(", ")))?; // alloc: as above
        self.out.tt.close(mixed);
        self.out.text(";")
    }
}

/// The name of `const_ast::Kind`'s constant for the node kind `kind`: `SyntaxKind`'s own.
fn node_kind(kind: SyntaxKind) -> Result<String, String> {
    if (kind as u16) < (SyntaxKind::ABI as u16) || kind == SyntaxKind::__LAST {
        return Err(format!("const_ast::quote!: the parse has a node of kind {kind:?}, no node's"));
    }
    Ok(format!("{kind:?}")) // alloc: the constant's name, written once
}

#[cfg(test)]
mod tests {
    //! `lower` on its own: the calls each quote lowers to, compared token for token with the
    //! text they are written as here.

    use hir_expand::{parse_to_token_tree_static_span, tt};
    use span::{Edition, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext};
    use syntax::{TextRange, TextSize};

    use super::lower;

    const USER: Span = Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: SpanAnchor {
            file_id: span::EditionedFileId::new(span::FileId::from_raw(0xe4e4e), Edition::CURRENT),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    };
    const CALL: Span = Span { range: TextRange::empty(TextSize::new(100)), ..USER };
    const MIXED: Span = Span { range: TextRange::empty(TextSize::new(200)), ..USER };

    /// The tokens of `text`, as a quote's call is given them, every span `USER`.
    fn call(text: &str) -> tt::TopSubtree {
        parse_to_token_tree_static_span(Edition::CURRENT, USER, text).expect("tokens")
    }

    /// Every token of `tree`, delimiters included, one space apart: what is compared, whatever
    /// the spacing each was lexed with.
    fn printed(tree: &tt::TopSubtree) -> String {
        fn walk(iter: tt::TtIter<'_>, out: &mut Vec<String>) {
            for element in iter {
                match element {
                    tt::TtElement::Leaf(leaf) => out.push(leaf.to_string()),
                    tt::TtElement::Subtree(subtree, inner) => {
                        let (open, close) = match subtree.delimiter.kind {
                            tt::DelimiterKind::Parenthesis => ("(", ")"),
                            tt::DelimiterKind::Brace => ("{", "}"),
                            tt::DelimiterKind::Bracket => ("[", "]"),
                            tt::DelimiterKind::Invisible => ("", ""),
                        };
                        if !open.is_empty() {
                            out.push(open.to_owned());
                        }
                        walk(inner, out);
                        if !close.is_empty() {
                            out.push(close.to_owned());
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        let view = tree.view();
        match view.top_subtree().delimiter.kind {
            tt::DelimiterKind::Invisible => walk(view.token_trees().iter(), &mut out),
            _ => walk(view.as_token_trees().iter(), &mut out),
        }
        out.join(" ")
    }

    /// `lower` of the call `quote`, as printed.
    fn lowered(quote: &str) -> Result<String, String> {
        lower(&call(quote), CALL, MIXED).map(|it| printed(&it))
    }

    /// `expected`'s tokens, as printed.
    fn expected(text: &str) -> String {
        printed(&call(text))
    }

    /// Every leaf of `tree` with its span.
    fn leaves(tree: &tt::TopSubtree) -> Vec<(String, Span)> {
        tree.view()
            .as_token_trees()
            .iter_flat_tokens()
            .filter_map(|it| match it {
                tt::TokenTree::Leaf(leaf) => Some((leaf.to_string(), *leaf.span())),
                tt::TokenTree::Subtree(_) => None,
            })
            .collect()
    }

    /// The invariant: a token is `b.token(TEXT)` and a node `open` of its kind, its children,
    /// then `close`, in the parser's own shape of the tokens.
    #[test]
    fn a_token_and_a_node_lower_to_token_and_open_close() {
        assert_eq!(
            lowered("b, ty => u8"),
            Ok(expected(
                "{ let __n0 = b.open(::const_ast::Kind::PATH_TYPE); \
                 let __n1 = b.open(::const_ast::Kind::PATH); \
                 let __n2 = b.open(::const_ast::Kind::PATH_SEGMENT); \
                 let __n3 = b.open(::const_ast::Kind::NAME_REF); \
                 b.token(\"u8\"); \
                 b.close(__n3); b.close(__n2); b.close(__n1); b.close(__n0); }"
            ))
        );
    }

    /// The invariant: `#t` is one call, `t.splice`, given the kinds the parse put around its
    /// place, outer to inner; `t` and `b` keep the caller's spans and what the lowering wrote
    /// has `mixed_site`.
    #[test]
    fn a_splice_names_the_kinds_it_stands_in() {
        assert_eq!(
            lowered("b, ty => #t"),
            Ok(expected(
                "{ t.splice(b, &[::const_ast::Kind::PATH_TYPE, ::const_ast::Kind::PATH, \
                 ::const_ast::Kind::PATH_SEGMENT, ::const_ast::Kind::NAME_REF]); }"
            ))
        );
        let tree = lower(&call("b, ty => #t"), CALL, MIXED).expect("lowered");
        let leaves = leaves(&tree);
        let span_of = |text: &str| {
            leaves.iter().find(|(it, _)| it == text).map(|(_, span)| *span).expect("a leaf")
        };
        assert_eq!((span_of("t"), span_of("b")), (USER, USER), "the caller's own tokens");
        assert_eq!((span_of("splice"), span_of("PATH_TYPE")), (MIXED, MIXED), "the lowering's");
        assert!(
            leaves.iter().all(|(_, span)| *span == USER || *span == MIXED),
            "no quoted token's number reaches the result: {leaves:?}"
        );
    }

    /// The invariant: a `#for` is a loop over its cursor around the run of whole nodes its
    /// body parsed to, written once; a hole's code is written where it stands.
    #[test]
    fn a_repetition_is_a_loop_over_whole_nodes() {
        let path_type = |n: u32, name: &str| {
            format!(
                "let __n{n} = b.open(::const_ast::Kind::PATH_TYPE); \
                 let __n{} = b.open(::const_ast::Kind::PATH); \
                 let __n{} = b.open(::const_ast::Kind::PATH_SEGMENT); \
                 let __n{} = b.open(::const_ast::Kind::NAME_REF); \
                 b.token(\"{name}\"); \
                 b.close(__n{}); b.close(__n{}); b.close(__n{}); b.close(__n{n});",
                n + 1,
                n + 2,
                n + 3,
                n + 3,
                n + 2,
                n + 1,
            )
        };
        let x = path_type(1, "X");
        let u8 = path_type(7, "u8");
        assert_eq!(
            lowered(
                "b, items => impl X { #for f in (fields) { #{ let _seen = 1; } const #f: u8 = 0; } }"
            ),
            Ok(expected(&format!(
                "{{ let __n0 = b.open(::const_ast::Kind::IMPL); b.token(\"impl\"); {x} \
                 let __n5 = b.open(::const_ast::Kind::ASSOC_ITEM_LIST); b.token(\"{{\"); \
                 {{ let mut __c0 = (fields); while let Some(f) = __c0.next() {{ \
                 let _seen = 1; \
                 let __n6 = b.open(::const_ast::Kind::CONST); b.token(\"const\"); \
                 f.splice(b, &[::const_ast::Kind::NAME]); b.token(\":\"); {u8} \
                 b.token(\"=\"); \
                 let __n11 = b.open(::const_ast::Kind::LITERAL); b.token(\"0\"); b.close(__n11); \
                 b.token(\";\"); b.close(__n6); }} }} \
                 b.token(\"}}\"); b.close(__n5); b.close(__n0); }}"
            )))
        );
    }

    /// The control: tokens that are not the entry's are refused by the parser, naming the
    /// quote, when the macro crate is analyzed; a quote that parses is not.
    #[test]
    fn tokens_that_do_not_parse_are_refused_where_the_macro_is_written() {
        let refused = lowered("b, items => impl for {}").expect_err("not an item");
        assert!(refused.starts_with("const_ast::quote!: the tokens are not items: "), "{refused}");
        assert!(lowered("b, items => impl X for Y {}").is_ok());
    }

    /// The control: a body that ends inside a node (`a + b` of `a + b * c`, whose parse is
    /// `a + (b * c)`) is refused naming the node; the same body around whole nodes is not.
    #[test]
    fn a_repetition_inside_a_node_is_refused_naming_it() {
        assert_eq!(
            lowered("b, expr => #for x in (xs) { a + b } * c"),
            Err("const_ast::quote!: a `#for` body must be whole nodes of one parent; it ends \
                 inside BIN_EXPR"
                .to_owned())
        );
        assert!(lowered("b, expr => a + #for x in (xs) { b * c }").is_ok());
    }
}
