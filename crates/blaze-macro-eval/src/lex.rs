//! What rustc's server answers to `ts_from_str` (`source_str_to_stream`: the source file's
//! normalization, the shebang stripped, then `lex_token_trees`), `literal_from_str` (the same
//! lexer under a parser's first tokens) and `symbol_normalize_and_validate_ident`, byte for
//! byte and error for error.
//!
//! rustc's lexer glues `::` or `'a` into one token; [`Stream`] holds one tree per character as
//! the bridge does, so a glued token becomes its characters, each but the last `Joint`, the
//! last with the token's spacing, which is what rustc's `FromInternal` gives a client and
//! what `print` glues back as rustc's printer sees it.
//!
//! Errors rustc emits to its session go to `emitted` and the call still answers `Ok`, as
//! rustc's does; the `Err` is what the client sees. The lints rustc's lexer buffers go to
//! `warned`: a warning naming the macro, never an error, so a dependency's build doesn't stop
//! on text its macro made.

mod cook;
mod unicode;

use std::borrow::Cow;

use intern::Symbol;
use span::{Edition, Span, TextRange, TextSize};

use crate::print::Op;
use crate::stream::{DelimSpan, Delimiter, DocKind, LitKind, Spacing, Stream, Tree};
use cook::{Fatal, Kind, Lexer, Token};

/// The lints rustc's lexer buffers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Lint {
    TextDirectionCodepointInComment,
    TextDirectionCodepointInLiteral,
}

/// What the lexer reported on the way.
#[derive(Default)]
pub(super) struct Emitted {
    pub(super) errors: Vec<String>,
    pub(super) lints: Vec<(Lint, String)>,
    /// Forced keywords lexed (`k#ident`), each gated by `forced_keywords`.
    pub(super) forced_keywords: u32,
}

impl Emitted {
    fn taking(emitted: &mut Vec<String>) -> Emitted {
        Emitted { errors: std::mem::take(emitted), ..Emitted::default() }
    }

    /// Gives the errors back, with the feature gate of each forced keyword unless the calling
    /// crate enables `forced_keywords` (`enabled`), and the buffered lints to `warned`.
    fn finish(self, enabled: bool, emitted: &mut Vec<String>, warned: &mut Vec<String>) {
        *emitted = self.errors;
        warned.extend(self.lints.into_iter().map(|(_, message)| message));
        let gated = if enabled { 0 } else { self.forced_keywords };
        for _ in 0..gated {
            emitted.push("forced keywords are experimental".to_owned()); // alloc: error path
        }
    }
}

/// `ts_from_str`: `text` as a stream, every token at `span`, lexed in `edition`, for a crate
/// that enables `forced_keywords` or not. Errors go to `emitted`, lints to `warned`.
pub(crate) fn stream(
    text: &str,
    span: Span,
    edition: Edition,
    forced_keywords: bool,
    emitted: &mut Vec<String>,
    warned: &mut Vec<String>,
) -> Result<Stream, String> {
    let source = normalize(text);
    let mut src: &str = &source;
    if let Some(shebang) = rustc_lexer::strip_shebang(src) {
        src = &src[shebang..];
    }
    let mut out = Emitted::taking(emitted);
    let (read, _) = Reader::new(src, span, edition, &mut out).read();
    out.finish(forced_keywords, emitted, warned);
    match read {
        Ok(trees) => Ok(Stream::of(trees)),
        Err(Stop::Fatal) => Err("failed to parse to tokenstream".to_owned()), // alloc: error path
        Err(stop) => Err(stop.message()),
    }
}

/// `literal_from_str`: `text` as one literal at `span`, a number with a leading `-` allowed
/// (the `-` then in its symbol).
pub(crate) fn literal(
    text: &str,
    span: Span,
    edition: Edition,
    forced_keywords: bool,
    emitted: &mut Vec<String>,
    warned: &mut Vec<String>,
) -> Result<Tree, String> {
    let source = normalize(text);
    let mut out = Emitted::taking(emitted);
    let (read, firsts) = Reader::new(&source, span, edition, &mut out).read();
    out.finish(forced_keywords, emitted, warned);
    match read {
        Ok(_) => {}
        Err(Stop::Fatal) => return Err("failed to parse to literal".to_owned()), // alloc: error path
        Err(stop) => return Err(stop.message()),
    }
    let not_a_literal = || Err("not a literal".to_owned()); // alloc: error path
    let Some(first) = firsts[0] else { return not_a_literal() };
    let minus = first.kind == Kind::Op(Op::Minus);
    let lit = if minus { firsts[1] } else { Some(first) };
    let Some(Token { kind: Kind::Literal { kind, mut symbol, suffix }, lo, hi }) = lit else {
        return not_a_literal();
    };
    if (hi - first.lo) as usize != text.len() {
        return Err("comment or whitespace around literal".to_owned()); // alloc: error path
    }
    if minus {
        if first.hi != lo {
            return Err("comment or whitespace after minus".to_owned()); // alloc: error path
        }
        if !matches!(kind, LitKind::Integer | LitKind::Float) {
            return Err("non-numeric literal may not be negated".to_owned()); // alloc: error path
        }
        symbol = Symbol::intern(&text[..1 + symbol.as_str().len()]);
    }
    Ok(Tree::Literal { kind, symbol, suffix, span, spacing: Spacing::Alone })
}

/// `symbol_normalize_and_validate_ident`: NFC, then the lexer's identifier rule.
pub(crate) fn ident(text: &str) -> Result<Symbol, ()> {
    let sym = cook::nfc_normalize(text);
    match rustc_lexer::is_ident(sym.as_str()) {
        true => Ok(sym),
        false => Err(()),
    }
}

/// A doc comment as rustc's `FromInternal` gives it to a client: `#`, `!` for an inner one,
/// and `[doc = "data"]` with each character of `data` `escape_debug`ged, every token `Alone`
/// at `span`. The comment's form is not kept.
pub(crate) fn desugar_doc(_kind: DocKind, inner: bool, data: &Symbol, span: Span, out: &mut Vec<Tree>) {
    // alloc: a doc comment taken apart, once per doc comment a client reads
    let mut escaped = String::with_capacity(data.as_str().len());
    for ch in data.as_str().chars() {
        escaped.extend(ch.escape_debug());
    }
    out.push(Tree::Punct { ch: b'#', spacing: Spacing::Alone, span });
    if inner {
        out.push(Tree::Punct { ch: b'!', spacing: Spacing::Alone, span });
    }
    // alloc: as above
    let stream = Stream::of(vec![
        Tree::Ident { sym: Symbol::intern("doc"), kind: crate::stream::IdentKind::Normal, span, spacing: Spacing::Alone },
        Tree::Punct { ch: b'=', spacing: Spacing::Alone, span },
        Tree::Literal {
            kind: LitKind::Str,
            symbol: Symbol::intern(&escaped),
            suffix: None,
            span,
            spacing: Spacing::Alone,
        },
    ]);
    out.push(Tree::Group {
        delimiter: Delimiter::Bracket,
        stream,
        span: DelimSpan { open: span, close: span, entire: span },
        open: Spacing::Alone,
        spacing: Spacing::Alone,
    });
}

/// `SourceFile`'s normalization of its text: a leading byte order mark removed, `\r\n` made
/// `\n`.
fn normalize(text: &str) -> Cow<'_, str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match text.contains("\r\n") {
        false => Cow::Borrowed(text),
        // alloc: text with Windows line ends, rare
        true => Cow::Owned(text.replace("\r\n", "\n")),
    }
}

/// Why reading stopped: a fatal lexer error, or the diagnostic rustc returns.
#[derive(Debug)]
enum Stop {
    Fatal,
    /// `eof_err`.
    Unclosed,
    /// `close_delim_err`, at the top level.
    UnexpectedClose(Delimiter),
    /// `MismatchedClosingDelimiter`, the first of `make_errors_for_mismatched_closing_delims`.
    Mismatched(Delimiter),
}

impl From<Fatal> for Stop {
    fn from(Fatal: Fatal) -> Stop {
        Stop::Fatal
    }
}

impl Stop {
    fn message(&self) -> String {
        match self {
            Stop::Fatal => "failed to parse".to_owned(), // alloc: error path
            Stop::Unclosed => "this file contains an unclosed delimiter".to_owned(), // alloc: error path
            Stop::UnexpectedClose(delimiter) => {
                format!("unexpected closing delimiter: `{}`", close_text(*delimiter)) // alloc: error path
            }
            Stop::Mismatched(delimiter) => {
                format!("mismatched closing delimiter: `{}`", close_text(*delimiter)) // alloc: error path
            }
        }
    }
}

fn close_text(delimiter: Delimiter) -> &'static str {
    match delimiter {
        Delimiter::Parenthesis => ")",
        Delimiter::Brace => "}",
        Delimiter::Bracket => "]",
        Delimiter::Invisible => "",
    }
}

/// `calculate_spacing`.
fn spacing_before(next: &Token) -> Spacing {
    if next.is_punct() {
        Spacing::Joint
    } else if next.kind == Kind::Eof {
        Spacing::Alone
    } else {
        Spacing::JointHidden
    }
}

/// rustc's `TokenTreesReader` (`lexer/tokentrees.rs`) over [`Lexer`].
struct Reader<'s, 'e> {
    lexer: Lexer<'s, 'e>,
    /// The span every tree gets.
    span: Span,
    /// The current token.
    token: Token,
    /// `diag_info.open_delimiters`.
    open: Vec<Delimiter>,
    /// The first closing delimiter that did not match, `unmatched_delims`' first with a
    /// found delimiter: with every token at one span, rustc records only the first.
    mismatched: Option<Delimiter>,
    /// The first two tokens a parser over the stream sees, glued, with their bytes.
    firsts: [Option<Token>; 2],
    /// Tokens taken past so far, the dummy before the first included.
    taken: u32,
}

impl<'s, 'e> Reader<'s, 'e> {
    fn new(src: &'s str, span: Span, edition: Edition, emitted: &'e mut Emitted) -> Self {
        Reader {
            lexer: Lexer::new(src, edition, emitted),
            span,
            token: Token { kind: Kind::Op(Op::Question), lo: 0, hi: 0 },
            open: Vec::new(), // alloc: nothing until a group opens, then one buffer per call
            mismatched: None,
            firsts: [None, None],
            taken: 0,
        }
    }

    /// `lex_token_trees(is_delimited: false)` and `make_errors_for_mismatched_closing_delims`:
    /// the top level's trees, or the first error rustc's caller would see.
    fn read(mut self) -> (Result<Vec<Tree>, Stop>, [Option<Token>; 2]) {
        // alloc: the stream lexed, once per call
        let mut trees = Vec::new();
        let read = self.trees(false, &mut trees);
        let result = match (read, self.mismatched) {
            (Err(Stop::Fatal), _) => Err(Stop::Fatal),
            (_, Some(delimiter)) => Err(Stop::Mismatched(delimiter)),
            (Ok(_), None) => Ok(trees),
            (Err(stop), None) => Err(stop),
        };
        (result, self.firsts)
    }

    /// `lex_token_trees`: the trees up to the closing delimiter or the end, and the spacing
    /// of the opening delimiter.
    fn trees(&mut self, is_delimited: bool, out: &mut Vec<Tree>) -> Result<Spacing, Stop> {
        let open_spacing = self.bump_minimal()?;
        loop {
            match self.token.kind {
                Kind::Open(delimiter) => {
                    let tree = self.group(delimiter)?;
                    out.push(tree);
                }
                Kind::Close(delimiter) => {
                    return match is_delimited {
                        true => Ok(open_spacing),
                        false => Err(Stop::UnexpectedClose(delimiter)),
                    };
                }
                Kind::Eof => {
                    return match is_delimited {
                        true => Err(Stop::Unclosed),
                        false => Ok(open_spacing),
                    };
                }
                _ => {
                    let (token, spacing) = self.bump()?;
                    self.push_token(token, spacing, out);
                }
            }
        }
    }

    /// `lex_token_tree_open_delim`.
    fn group(&mut self, open: Delimiter) -> Result<Tree, Stop> {
        self.open.push(open);
        // alloc: a group's trees, once per group lexed
        let mut inner = Vec::new();
        let open_spacing = self.trees(true, &mut inner)?;
        let close_spacing = match self.token.kind {
            Kind::Close(close) if close == open => {
                self.open.pop();
                self.bump_minimal()?
            }
            Kind::Close(close) => {
                if self.mismatched.is_none() {
                    self.mismatched = Some(close);
                }
                self.open.pop();
                // A closing delimiter that closes an earlier group is left to it.
                match self.open.contains(&close) {
                    true => Spacing::Alone,
                    false => self.bump_minimal()?,
                }
            }
            // The end: reported when the outer level sees it again.
            _ => Spacing::Alone,
        };
        let span = self.span;
        Ok(Tree::Group {
            delimiter: open,
            stream: Stream::of(inner),
            span: DelimSpan { open: span, close: span, entire: span },
            open: open_spacing,
            spacing: close_spacing,
        })
    }

    /// `bump`: the current token glued with what follows it at once, and its spacing.
    fn bump(&mut self) -> Result<(Token, Spacing), Fatal> {
        let (spacing, next) = loop {
            let (next, preceded_by_whitespace) = self.lexer.next_token()?;
            if preceded_by_whitespace {
                break (Spacing::Alone, next);
            } else if let Some(glued) = self.token.glue(&next) {
                self.token = glued;
            } else {
                break (spacing_before(&next), next);
            }
        };
        let token = std::mem::replace(&mut self.token, next);
        self.took(token);
        Ok((token, spacing))
    }

    /// `bump_minimal`: past a delimiter, giving its spacing.
    fn bump_minimal(&mut self) -> Result<Spacing, Fatal> {
        let (next, preceded_by_whitespace) = self.lexer.next_token()?;
        let spacing = match preceded_by_whitespace {
            true => Spacing::Alone,
            false => spacing_before(&next),
        };
        let token = std::mem::replace(&mut self.token, next);
        self.took(token);
        Ok(spacing)
    }

    fn took(&mut self, token: Token) {
        let index = self.taken as usize;
        self.taken += 1;
        // The first token taken past is the reader's dummy.
        if let Some(first) = index.checked_sub(1).and_then(|i| self.firsts.get_mut(i)) {
            *first = Some(token);
        }
    }

    /// A token as the bridge's trees: an operator one punctuation mark per character, a
    /// lifetime `'` and its name.
    fn push_token(&self, token: Token, spacing: Spacing, out: &mut Vec<Tree>) {
        let span = self.span;
        match token.kind {
            Kind::Op(op) => {
                let text = op.text().as_bytes();
                let last = text.len() - 1;
                // `FromInternal` gives each character its byte of a span exactly as long as
                // the operator, else the whole span.
                let split = usize::from(span.range.len()) == text.len();
                for (i, &ch) in text.iter().enumerate() {
                    let spacing = if i == last { spacing } else { Spacing::Joint };
                    let span = match split {
                        true => Span {
                            range: TextRange::at(span.range.start() + TextSize::from(i as u32), TextSize::from(1)),
                            ..span
                        },
                        false => span,
                    };
                    out.push(Tree::Punct { ch, spacing, span });
                }
            }
            Kind::Ident(sym, kind) => out.push(Tree::Ident { sym, kind, span, spacing }),
            Kind::Lifetime(sym, kind) => {
                let name = sym.as_str().strip_prefix('\'').map_or(sym, Symbol::intern);
                out.push(Tree::Punct { ch: b'\'', spacing: Spacing::Joint, span });
                out.push(Tree::Ident { sym: name, kind, span, spacing });
            }
            Kind::Literal { kind, symbol, suffix } => {
                out.push(Tree::Literal { kind, symbol, suffix, span, spacing })
            }
            Kind::Doc { kind, inner, data } => {
                out.push(Tree::DocComment { kind, inner, data, span, spacing })
            }
            // `trees` takes delimiters and the end itself.
            Kind::Open(_) | Kind::Close(_) | Kind::Eof => {}
        }
    }
}

#[cfg(test)]
mod tests;
