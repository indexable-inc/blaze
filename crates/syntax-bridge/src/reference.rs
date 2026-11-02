//! The conversions the build no longer runs, kept as the references tests hold the new ones to
//! (compiled for this crate's tests and with the `reference` feature, which only tests turn on):
//! the parser's input made by a cursor over the token trees, each leaf decoded, identifiers'
//! kinds from their text and editions through a map; and a syntax handle's tokens as a token
//! tree, read through the cursor ([`syntax_node_to_token_tree`], whose view form is
//! [`crate::call_input`]).

use std::alloc::{Allocator, Global};

use blaze_hash::HashMap;
use parser::TopEntryPoint;
use smallvec::SmallVec;
use span::{Edition, Span, SpanMap, SyntaxContext};
use stdx::never;
use syntax::{
    Parse, PreorderWithTokens, SyntaxElement, SyntaxKind, SyntaxKind::*, SyntaxNode, SyntaxToken,
    T, TextRange, TextSize, WalkEvent, view::Untyped,
};

use crate::{
    Appended, DocCommentDesugarMode, FlatConverter, Inserted, Removed, SpanMapper, SrcToken,
    TokenConverter, convert_doc_comment, convert_tokens_kept, float_pieces, may_continue_float,
    punct_char, split_float,
};

/// [`crate::to_parser_input`] as it was.
pub fn to_parser_input<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    buffer: tt::TokenTreesView<'_>,
    span_to_edition: &mut F,
) -> parser::Input {
    convert(buffer, span_to_edition, false)
}

/// [`crate::to_matcher_input`] as it was.
pub fn to_matcher_input<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    buffer: tt::TokenTreesView<'_>,
    span_to_edition: &mut F,
) -> parser::Input {
    convert(buffer, span_to_edition, true)
}

/// [`crate::token_tree_to_syntax_node`] parsing the input [`to_parser_input`] makes.
pub fn token_tree_to_syntax_node<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    tt: &tt::TopSubtree,
    entry_point: TopEntryPoint,
    span_to_edition: &mut F,
) -> (Parse<Untyped>, SpanMap) {
    let buffer = tt.view().strip_invisible();
    let parser_input = to_parser_input(buffer, span_to_edition);
    crate::parse_tokens(buffer, entry_point, parser_input.tokens(), std::alloc::Global)
}

/// Whether two inputs are the same tokens, each the same to the parser.
pub fn same_input(a: &parser::Input, b: &parser::Input) -> bool {
    a.len() == b.len() && (0..a.len()).all(|i| a.token(i) == b.token(i))
}

fn convert<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    buffer: tt::TokenTreesView<'_>,
    span_to_edition: &mut F,
    fragments: bool,
) -> parser::Input {
    let mut res = parser::Input::with_capacity(buffer.len());
    // Where the fragment that was opened last starts, until its first token is pushed.
    let mut fragment_at = None;

    let mut current = buffer.cursor();
    let mut syntax_context_to_edition_cache = HashMap::default();
    let mut last: Option<(SyntaxContext, Edition)> = None;
    let mut ctx_edition = |ctx: SyntaxContext| {
        if let Some((last_ctx, edition)) = last
            && last_ctx == ctx
        {
            return edition;
        }
        let edition =
            *syntax_context_to_edition_cache.entry(ctx).or_insert_with(|| span_to_edition(ctx));
        last = Some((ctx, edition));
        edition
    };

    while !current.eof() {
        if let Some(at) = fragment_at
            && res.len() > at
        {
            res.mark_fragment_start(at);
            fragment_at = None;
        }
        let tt = current.token_tree();

        // Check if it is lifetime
        if let Some(tt::TokenTree::Leaf(tt::Leaf::Punct(punct))) = tt
            && punct.char == '\''
        {
            current.bump();
            match current.token_tree() {
                Some(tt::TokenTree::Leaf(tt::Leaf::Ident(ident))) => {
                    res.push(LIFETIME_IDENT, ctx_edition(ident.span.ctx));
                    current.bump();
                    continue;
                }
                _ => panic!("Next token must be ident"),
            }
        }

        match tt {
            Some(tt::TokenTree::Leaf(leaf)) => {
                match leaf {
                    tt::Leaf::Literal(lit) => {
                        let kind = match lit.kind {
                            tt::LitKind::Byte => SyntaxKind::BYTE,
                            tt::LitKind::Char => SyntaxKind::CHAR,
                            tt::LitKind::Integer => SyntaxKind::INT_NUMBER,
                            tt::LitKind::Float => SyntaxKind::FLOAT_NUMBER,
                            tt::LitKind::Str | tt::LitKind::StrRaw(_) => SyntaxKind::STRING,
                            tt::LitKind::ByteStr | tt::LitKind::ByteStrRaw(_) => {
                                SyntaxKind::BYTE_STRING
                            }
                            tt::LitKind::CStr | tt::LitKind::CStrRaw(_) => SyntaxKind::C_STRING,
                            tt::LitKind::Err(_) => SyntaxKind::ERROR,
                        };
                        res.push(kind, ctx_edition(lit.span.ctx));

                        if kind == FLOAT_NUMBER && !lit.text().ends_with('.') {
                            res.was_joint();
                        }
                    }
                    tt::Leaf::Ident(ident) => {
                        let edition = ctx_edition(ident.span.ctx);
                        match ident.sym.as_str() {
                            "_" => res.push(T![_], edition),
                            i if i.starts_with('\'') => res.push(LIFETIME_IDENT, edition),
                            _ if ident.is_raw.yes() => res.push(IDENT, edition),
                            text => match SyntaxKind::from_keyword(text, edition) {
                                Some(kind) => res.push(kind, edition),
                                None => {
                                    let contextual_keyword =
                                        SyntaxKind::from_contextual_keyword(text, edition)
                                            .unwrap_or(SyntaxKind::IDENT);
                                    res.push_ident(contextual_keyword, edition);
                                }
                            },
                        }
                    }
                    tt::Leaf::Punct(punct) => {
                        let kind = SyntaxKind::from_char(punct.char)
                            .unwrap_or_else(|| panic!("{punct:#?} is not a valid punct"));
                        res.push(kind, ctx_edition(punct.span.ctx));
                        if punct.spacing == tt::Spacing::Joint {
                            res.was_joint();
                        }
                    }
                }
                current.bump();
            }
            Some(tt::TokenTree::Subtree(subtree)) => {
                if let Some(kind) = match subtree.delimiter.kind {
                    tt::DelimiterKind::Parenthesis => Some(T!['(']),
                    tt::DelimiterKind::Brace => Some(T!['{']),
                    tt::DelimiterKind::Bracket => Some(T!['[']),
                    tt::DelimiterKind::Invisible => None,
                } {
                    res.push(kind, ctx_edition(subtree.delimiter.open.ctx));
                } else if fragments && fragment_at.is_none() {
                    fragment_at = Some(res.len());
                }
                current.bump();
            }
            None => {
                let subtree = current.end();
                if let Some(kind) = match subtree.delimiter.kind {
                    tt::DelimiterKind::Parenthesis => Some(T![')']),
                    tt::DelimiterKind::Brace => Some(T!['}']),
                    tt::DelimiterKind::Bracket => Some(T![']']),
                    tt::DelimiterKind::Invisible => None,
                } {
                    res.push(kind, ctx_edition(subtree.delimiter.close.ctx));
                }
            }
        };
    }
    if let Some(at) = fragment_at
        && res.len() > at
    {
        res.mark_fragment_start(at);
    }

    res
}

/// [`crate::call_input`] of a syntax handle, as it was: a node of a flat tree is read from its
/// arrays, through the parts of the lazy nodes in it; a node of a green tree, one being edited,
/// through the cursor.
pub fn syntax_node_to_token_tree<SpanMap>(
    node: &SyntaxNode,
    map: SpanMap,
    span: Span,
    mode: DocCommentDesugarMode,
) -> tt::TopSubtree
where
    SpanMap: SpanMapper,
{
    if let Some((tree, at)) = node.flat() {
        let mut c = FlatConverter::new(tree, at, map, span, mode);
        let len = c.tokens.len_hint();
        return convert_tokens_kept(&mut c, len);
    }
    let append = Appended::<SyntaxElement, Global>::default();
    let remove = Removed::<SyntaxElement, Global>::default();
    let mut c =
        Converter::new(node, map, append, remove, span, mode, |_, _| (true, Inserted::new()));
    convert_tokens_kept(&mut c, 0)
}

/// [`crate::call_input_modified`] of a syntax handle, as it was: the cursor's walk, with
/// elements removed and appended after by their handles.
pub fn syntax_node_to_token_tree_modified<SpanMap, OnEvent, A: Allocator + Clone>(
    node: &SyntaxNode,
    map: SpanMap,
    append: Appended<SyntaxElement, A>,
    remove: Removed<SyntaxElement, A>,
    call_site: Span,
    mode: DocCommentDesugarMode,
    on_enter: OnEvent,
) -> tt::TopSubtree
where
    SpanMap: SpanMapper,
    OnEvent: FnMut(&mut PreorderWithTokens, &WalkEvent<SyntaxElement>) -> (bool, Inserted),
{
    let mut c = Converter::new(node, map, append, remove, call_site, mode, on_enter);
    convert_tokens_kept(&mut c, 0)
}

struct Converter<SpanMap, OnEvent, A: Allocator + Clone> {
    current: Option<SyntaxToken>,
    /// The float in the node that the parser split and `current` is the first piece of.
    float: Option<TextRange>,
    /// Whether `current` ends a token of the source's lexing.
    ends_token: bool,
    current_leaves: SmallVec<[tt::Leaf; 4]>,
    preorder: PreorderWithTokens,
    range: TextRange,
    punct_offset: Option<(SyntaxToken, TextSize)>,
    /// Used to make the emitted text ranges in the spans relative to the span anchor.
    map: SpanMap,
    append: Appended<SyntaxElement, A>,
    remove: Removed<SyntaxElement, A>,
    call_site: Span,
    mode: DocCommentDesugarMode,
    on_event: OnEvent,
}

impl<SpanMap, OnEvent, A: Allocator + Clone> Converter<SpanMap, OnEvent, A>
where
    OnEvent: FnMut(&mut PreorderWithTokens, &WalkEvent<SyntaxElement>) -> (bool, Inserted),
{
    fn new(
        node: &SyntaxNode,
        map: SpanMap,
        append: Appended<SyntaxElement, A>,
        remove: Removed<SyntaxElement, A>,
        call_site: Span,
        mode: DocCommentDesugarMode,
        on_enter: OnEvent,
    ) -> Self {
        let mut converter = Converter {
            current: None,
            float: None,
            ends_token: false,
            preorder: node.preorder_with_tokens(),
            range: node.text_range(),
            punct_offset: None,
            map,
            append,
            remove,
            call_site,
            current_leaves: SmallVec::new(),
            mode,
            on_event: on_enter,
        };
        converter.current = converter.next_token();
        converter.settle(Some(converter.range.start()));
        converter
    }

    /// The text of `float`, which `first` is the first piece of.
    fn float_text(&self, first: &SyntaxToken, float: TextRange) -> String {
        // clone: a token handle, to walk from.
        std::iter::successors(Some(first.clone()), |it| it.next_token())
            .take_while(|it| it.text_range().start() < float.end())
            .map(|it| it.text().to_owned())
            .collect()
    }

    /// `crate::ViewConverter::settle`, the window made of the tokens' texts.
    fn settle(&mut self, boundary: Option<TextSize>) {
        self.float = None;
        self.ends_token = false;
        let Some(token) = self.current.clone() else { return };
        let range = token.text_range();
        let prev = || token.prev_token().map(|it| (it.kind(), it.text_range()));
        if boundary != Some(range.start()) && may_continue_float(token.kind(), range, prev()) {
            return;
        }
        self.ends_token = true;
        if token.kind() != INT_NUMBER {
            return;
        }
        let next = std::iter::successors(token.next_token(), |it| it.next_token())
            .map(|it| (it.kind(), it.text_range()));
        let Some(ends) = float_pieces(range, next) else { return };
        let mut window = String::new();
        for it in std::iter::successors(Some(token), |it| it.next_token()) {
            if it.text_range().start() < ends[1] {
                window.push_str(it.text());
            } else if let Some(char) = it.text().chars().next() {
                window.push(char);
                break;
            }
        }
        match split_float(range, ends, &window) {
            Some(float) if self.range.contains_range(float) => self.float = Some(float),
            Some(_) => self.ends_token = false,
            None => {}
        }
    }

    fn next_token(&mut self) -> Option<SyntaxToken> {
        while let Some(ev) = self.preorder.next() {
            let (keep_event, insert_leaves) = (self.on_event)(&mut self.preorder, &ev);
            self.current_leaves.extend(insert_leaves);
            if !keep_event {
                continue;
            }
            match ev {
                WalkEvent::Enter(token) => {
                    if self.remove.contains(&token) {
                        match token {
                            syntax::NodeOrToken::Token(_) => {
                                continue;
                            }
                            node => {
                                self.preorder.skip_subtree();
                                if let Some(v) = self.append.remove(&node) {
                                    self.current_leaves.extend(v);
                                    continue;
                                }
                            }
                        }
                    } else if let syntax::NodeOrToken::Token(token) = token {
                        return Some(token);
                    }
                }
                WalkEvent::Leave(ele) => {
                    if let Some(v) = self.append.remove(&ele) {
                        self.current_leaves.extend(v);
                        continue;
                    }
                }
            }
        }
        None
    }
}

#[derive(Debug)]
enum SynToken {
    Ordinary(SyntaxToken),
    Punct { token: SyntaxToken, offset: usize },
    Float(String),
    Leaf(tt::Leaf),
}

impl SynToken {
    fn token(&self) -> &SyntaxToken {
        match self {
            SynToken::Ordinary(it) | SynToken::Punct { token: it, offset: _ } => it,
            SynToken::Float(_) | SynToken::Leaf(_) => unreachable!(),
        }
    }
}

impl<SpanMap, OnEvent, A: Allocator + Clone> SrcToken<Converter<SpanMap, OnEvent, A>>
    for SynToken
{
    fn kind(&self, _ctx: &Converter<SpanMap, OnEvent, A>) -> SyntaxKind {
        match self {
            SynToken::Ordinary(token) => token.kind(),
            SynToken::Punct { token, offset: i } => {
                SyntaxKind::from_char(punct_char(token.text(), *i).unwrap()).unwrap()
            }
            SynToken::Float(_) => FLOAT_NUMBER,
            SynToken::Leaf(_) => {
                never!();
                SyntaxKind::ERROR
            }
        }
    }
    fn to_char(&self, _ctx: &Converter<SpanMap, OnEvent, A>) -> Option<char> {
        match self {
            SynToken::Ordinary(_) | SynToken::Float(_) => None,
            SynToken::Punct { token: it, offset: i } => punct_char(it.text(), *i),
            SynToken::Leaf(_) => None,
        }
    }
    fn to_text<'a>(&'a self, _ctx: &'a Converter<SpanMap, OnEvent, A>) -> &'a str {
        match self {
            SynToken::Ordinary(token) | SynToken::Punct { token, offset: _ } => token.text(),
            SynToken::Float(text) => text,
            SynToken::Leaf(_) => {
                never!();
                ""
            }
        }
    }
    fn as_leaf(&self) -> Option<&tt::Leaf> {
        match self {
            SynToken::Ordinary(_) | SynToken::Punct { .. } | SynToken::Float(_) => None,
            SynToken::Leaf(it) => Some(it),
        }
    }
}

impl<SpanMap, OnEvent, A: Allocator + Clone> TokenConverter for Converter<SpanMap, OnEvent, A>
where
    SpanMap: SpanMapper,
    OnEvent: FnMut(&mut PreorderWithTokens, &WalkEvent<SyntaxElement>) -> (bool, Inserted),
{
    type Token = SynToken;
    fn convert_doc_comment(
        &self,
        token: &Self::Token,
        is_inner: bool,
        span: Span,
        builder: &mut tt::TopSubtreeBuilder,
    ) {
        convert_doc_comment(token.token().text(), is_inner, span, self.mode, builder);
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        if let Some((punct, offset)) = &mut self.punct_offset
            && usize::from(*offset) + 1 < punct.text().len()
        {
            *offset += TextSize::of('.');
            let range = TextRange::at(punct.text_range().start() + *offset, TextSize::of('.'));
            let offset = u32::from(*offset) as usize;
            // clone: a token handle, for the token returned as well as the one kept.
            return Some((SynToken::Punct { token: punct.clone(), offset }, range));
        }

        if !self.current_leaves.is_empty() {
            let leaf = self.current_leaves.remove(0);
            return Some((SynToken::Leaf(leaf), TextRange::empty(TextSize::new(0))));
        }

        let curr = self.current.take()?;
        if !self.range.contains_range(curr.text_range()) {
            self.current = Some(curr);
            return None;
        }

        if let Some(float) = self.float {
            let text = self.float_text(&curr, float);
            self.current = self.next_token();
            while self.current.as_ref().is_some_and(|it| it.text_range().start() < float.end()) {
                self.current = self.next_token();
            }
            self.settle(Some(float.end()));
            self.punct_offset = None;
            return Some((SynToken::Float(text), float));
        }
        self.current = self.next_token();
        self.settle(self.ends_token.then_some(curr.text_range().end()));
        let token = if curr.kind().is_punct() {
            // clone: a token handle, for the token returned as well as the one kept.
            self.punct_offset = Some((curr.clone(), 0.into()));
            let range = curr.text_range();
            let range = TextRange::at(range.start(), TextSize::of('.'));
            (SynToken::Punct { token: curr, offset: 0_usize }, range)
        } else {
            self.punct_offset = None;
            let range = curr.text_range();
            (SynToken::Ordinary(curr), range)
        };

        Some(token)
    }

    fn peek(&self) -> Option<Self::Token> {
        if let Some((punct, offset)) = &self.punct_offset {
            let offset = *offset + TextSize::of('.');
            if usize::from(offset) < punct.text().len() {
                let offset = usize::from(offset);
                // clone: a token handle, for the token returned.
                return Some(SynToken::Punct { token: punct.clone(), offset });
            }
        }

        let curr = self.current.as_ref()?;
        if !self.range.contains_range(curr.text_range()) {
            return None;
        }
        if let Some(float) = self.float {
            return Some(SynToken::Float(self.float_text(curr, float)));
        }
        // clone: a token handle, for the token returned.
        let curr = curr.clone();

        let token = if curr.kind().is_punct() {
            SynToken::Punct { token: curr, offset: 0_usize }
        } else {
            SynToken::Ordinary(curr)
        };
        Some(token)
    }

    fn span_for(&self, range: TextRange) -> Span {
        self.map.span_for(range)
    }

    fn fragment_bounds(&self, range: TextRange) -> crate::FragmentBounds {
        self.map.fragment_bounds(range)
    }
    fn call_site(&self) -> Span {
        self.call_site
    }
}
