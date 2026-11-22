//! An expansion's tokens as a blaze-tokens record (docs/v2/plan/design/drive.md, D122 "An
//! expansion is its token record"): built from the same stream the item renderer walks today
//! (blaze-scope resolve.rs `render_with`), with no text rendered and nothing lexed again.
//!
//! # A stream token and a record token
//!
//! The record's tokens are the parser's words of the stream, made by the one conversion a
//! stream has ([`crate::parser_words`], `rules::words_of`), each word kept verbatim (its
//! `JOINT`, `FRAGMENT` and extension bits included). So:
//!
//! - a lifetime's `'` and its name are ONE record token: its lexeme `'name`, its context and
//!   edition the name's (as `words_of` reads the name's edition and `body_words` its hygiene),
//!   its origin the `'`'s;
//! - an invisible group's two delimiters make NO record token: the group is a
//!   [`TokensBuilder::group`] over the record tokens between them;
//! - every other stream token is one record token; its lexeme, for a kind with no fixed text
//!   (an identifier, a lifetime, a literal, an error), is the token's own spelling as
//!   [`crate::write_token`] writes that one token (an identifier with its `r#`, a literal with
//!   its prefix, quotes and suffix; `$crate` as `$crate`, never a crate's name), never a slice
//!   of a rendered text;
//! - `joined` is the stream's spacing of the record token's last stream token being `Joint`:
//!   a punctuation character glued to the next (`>` `>` read as `>>`), the one spacing the
//!   parser reads (its `JOINT` bit). `JointHidden`, an identifier or literal touching the next
//!   token in the text it was lexed from, is a fact of that text's spaces: it is not kept, so
//!   two expansions that differ only in spaces are one record and one digest. A delimiter's
//!   spacing is `Alone`, so a delimiter is never joined.
//!
//! # How origins are counted
//!
//! In RECORD tokens of the source the origin names. `Origin::Input(n)` is record token `n` of
//! the call input's own token record (what [`record_indexes`] of the input gives a stream
//! index), `AttrArgs`, `Def` and `Params` likewise of the attribute's arguments, the
//! definition's body and its parameters. The stream origin [`TokenOrigin`] the expander made
//! counts STREAM tokens; it is mapped through the source's [`record_indexes`], which the
//! writer is given ([`OriginMaps`], [`SourceIndexes::of`]): the stream does not hold its
//! sources. A lifetime's origin is its `'`'s, whose index maps to the lifetime's one record
//! token (its name's maps to the same). `Made` needs no source.

use core::fmt;

use blaze_own::{AString, AVec, Alloc, Vec};
use blaze_tokens::{BuildError, NewToken, Origin, TokenRange, TokensBuilder, TokensBytes};

use crate::{
    body::{parser_words, write_token},
    call::Call,
    def::MacroDefinition,
    expansion::{Expansion, TokenOrigin},
    tokens::{Ctx, Delim, Mark, MarkOf, Spacing, Token, TokenKind, TokenStream, index},
};

/// For each source an origin can name, its [`record_indexes`]: entry `n` the record token that
/// stream token `n` is, or is part of; one entry past the last at the stream's length.
#[derive(Clone, Copy, Debug)]
pub struct OriginMaps<'m> {
    /// The call's input ([`Call::input`]).
    pub input: Option<&'m [u32]>,
    /// The attribute's arguments ([`Call::attr_args`]).
    pub attr_args: Option<&'m [u32]>,
    /// The definition's body ([`MacroDefinition::body`]).
    pub body: Option<&'m [u32]>,
    /// The definition's parameters ([`MacroDefinition::params`]).
    pub params: Option<&'m [u32]>,
}

impl OriginMaps<'static> {
    /// No source: for tokens all made by the expansion, or none (a failure's record). A token
    /// that names a source is then refused ([`TokenRecordError::NoSource`]).
    pub const NONE: OriginMaps<'static> =
        OriginMaps { input: None, attr_args: None, body: None, params: None };
}

/// The [`record_indexes`] of one call's and one definition's sources, in the allocator named.
pub struct SourceIndexes<'a> {
    input: AVec<'a, u32>,
    attr_args: Option<AVec<'a, u32>>,
    body: AVec<'a, u32>,
    params: Option<AVec<'a, u32>>,
}

impl<'a> SourceIndexes<'a> {
    /// The sources of `call` of `def`: what [`crate::expand`] read the expansion's tokens from.
    pub fn of(def: &MacroDefinition<'_>, call: &Call<'_>, alloc: Alloc<'a>) -> SourceIndexes<'a> {
        SourceIndexes {
            input: record_indexes(call.input(), alloc),
            attr_args: call.attr_args().map(|args| record_indexes(args, alloc)),
            body: record_indexes(def.body(), alloc),
            params: def.params().map(|params| record_indexes(params, alloc)),
        }
    }

    pub fn maps(&self) -> OriginMaps<'_> {
        OriginMaps {
            input: Some(&self.input[..]),
            attr_args: self.attr_args.as_ref().map(|map| &map[..]),
            body: Some(&self.body[..]),
            params: self.params.as_ref().map(|map| &map[..]),
        }
    }
}

/// For each token of `stream`, the record token it is or is part of (a lifetime's `'` and name
/// the same one; an invisible delimiter the record token after it), then the record's token
/// count: the `word_at` of the one conversion ([`crate::parser_words`]).
pub fn record_indexes<'a>(stream: &TokenStream<'_>, alloc: Alloc<'a>) -> AVec<'a, u32> {
    parser_words(stream, alloc).word_at
}

/// Why an expansion's tokens are not a token record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenRecordError {
    /// blaze-tokens' builder refused a token, context or group.
    Build(BuildError),
    /// Record token `token` came from `origin`, a source no map was given for.
    NoSource { token: u32, origin: TokenOrigin },
    /// Record token `token` came from `origin`, past the `tokens` tokens of its source's map.
    PastSource { token: u32, origin: TokenOrigin, tokens: u32 },
    /// Stream token `token`: an invisible group's delimiters do not pair up (`what`).
    Group { token: u32, what: &'static str },
    /// Stream token `token` is a `'` with no token after it for a name.
    LoneQuote { token: u32 },
    /// The record tokens made (`tokens`) are not the parser words of the stream (`words`).
    Words { tokens: u32, words: u32 },
}

impl From<BuildError> for TokenRecordError {
    fn from(error: BuildError) -> TokenRecordError {
        TokenRecordError::Build(error)
    }
}

impl fmt::Display for TokenRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenRecordError::Build(error) => write!(f, "an expansion's token record: {error}"),
            TokenRecordError::NoSource { token, origin } => write!(
                f,
                "an expansion's token record, token {token}: it came from {origin:?}, a source the writer was not given"
            ),
            TokenRecordError::PastSource { token, origin, tokens } => write!(
                f,
                "an expansion's token record, token {token}: it came from {origin:?}, past the {tokens} tokens of that source"
            ),
            TokenRecordError::Group { token, what } => {
                write!(f, "an expansion's token record, stream token {token}: {what}")
            }
            TokenRecordError::LoneQuote { token } => write!(
                f,
                "an expansion's token record, stream token {token}: a `'` with no token after it to name"
            ),
            TokenRecordError::Words { tokens, words } => write!(
                f,
                "an expansion's token record made {tokens} tokens of a stream of {words} parser words"
            ),
        }
    }
}

impl core::error::Error for TokenRecordError {}

impl Expansion<'_> {
    /// The expansion's tokens as a blaze-tokens record in `alloc` (see the module's doc), its
    /// origins counted in record tokens of the sources `maps` gives. Equal expansions with
    /// equal maps give equal bytes.
    pub fn token_record<'b>(
        &self,
        maps: &OriginMaps<'_>,
        alloc: Alloc<'b>,
    ) -> Result<TokensBytes<'b>, TokenRecordError> {
        token_record(&self.tokens, &self.origins, maps, alloc)
    }
}

/// `stream`, whose token `i` came from `origins[i]`, as a blaze-tokens record in `alloc`.
pub fn token_record<'b>(
    stream: &TokenStream<'_>,
    origins: &[TokenOrigin],
    maps: &OriginMaps<'_>,
    alloc: Alloc<'b>,
) -> Result<TokensBytes<'b>, TokenRecordError> {
    let parser = parser_words(stream, alloc);
    let words = &parser.words[..parser.words.len() - parser::Tokens::PAD];
    let tokens = stream.tokens();
    assert_eq!(tokens.len(), origins.len(), "an expansion holds one origin per token");
    let mut out = TokensBuilder::new_in(alloc);
    // The stream's context table in its order: each parent is a root or an earlier node, so
    // node `k` of the stream is node `k` of the record.
    let mut ctxs: AVec<'b, blaze_tokens::Ctx> = Vec::with_capacity_in(stream.contexts().len(), alloc);
    for node in stream.contexts() {
        let parent = record_ctx(node.parent, &ctxs);
        ctxs.push(out.context(blaze_tokens::CtxNode { parent, mark: record_mark(node.mark) })?);
    }
    let made = Made { words, origins, maps, ctxs: &ctxs };
    let mut spelled = AString::new_in(alloc);
    // The record token each open invisible group starts at, the innermost last.
    let mut open: AVec<'b, u32> = Vec::new_in(alloc);
    let mut groups: AVec<'b, TokenRange> = Vec::new_in(alloc);
    // A `'` waiting for its name: `words_of` makes the next token, whatever it is, its name.
    let mut quote: Option<usize> = None;
    for (at, token) in tokens.iter().enumerate() {
        if let Some(first) = quote.take() {
            spelled.clear();
            spelled.push('\'');
            write_token(stream, token, &mut spelled);
            made.push(&mut out, first, token, &spelled)?;
            continue;
        }
        match token.kind {
            TokenKind::Punct { char: '\'', .. } => quote = Some(at),
            TokenKind::Open(Delim::Invisible) => open.push(index(out.len())),
            TokenKind::Close(Delim::Invisible) => {
                let start = open.pop().ok_or(TokenRecordError::Group {
                    token: index(at),
                    what: "an invisible group closed that no open delimiter began",
                })?;
                groups.push(TokenRange::new(start, index(out.len())));
            }
            _ => {
                spelled.clear();
                write_token(stream, token, &mut spelled);
                made.push(&mut out, at, token, &spelled)?;
            }
        }
    }
    if let Some(at) = quote {
        return Err(TokenRecordError::LoneQuote { token: index(at) });
    }
    if !open.is_empty() {
        return Err(TokenRecordError::Group {
            token: index(tokens.len()),
            what: "an invisible group the stream leaves open",
        });
    }
    if out.len() != words.len() {
        return Err(TokenRecordError::Words { tokens: index(out.len()), words: index(words.len()) });
    }
    // Closed innermost first; the record takes them by start, an outer group first.
    groups.sort_unstable_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    for group in groups.iter() {
        out.group(*group)?;
    }
    Ok(out.finish()?)
}

/// What every record token is made with: the stream's parser words, its origins, the sources'
/// maps and the record's contexts.
struct Made<'w> {
    words: &'w [u32],
    origins: &'w [TokenOrigin],
    maps: &'w OriginMaps<'w>,
    ctxs: &'w [blaze_tokens::Ctx],
}

impl Made<'_> {
    /// The next record token: stream tokens `first..=last`'s, `last` being `token` (a
    /// lifetime's name, else `first` itself), spelled `spelled`.
    fn push(
        &self,
        out: &mut TokensBuilder<'_>,
        first: usize,
        token: &Token,
        spelled: &str,
    ) -> Result<(), TokenRecordError> {
        let k = out.len();
        let word = *self.words.get(k).ok_or(TokenRecordError::Words {
            tokens: index(k + 1),
            words: index(self.words.len()),
        })?;
        let lexeme = match parser::word_kind(word).fixed_text() {
            None => Some(spelled),
            Some(_) => None,
        };
        let origin = record_origin(self.origins[first], self.maps, index(k))?;
        out.push(NewToken {
            word,
            lexeme,
            joined: token.kind.spacing() == Spacing::Joint,
            ctx: record_ctx(token.ctx, self.ctxs),
            origin,
        })?;
        Ok(())
    }
}

/// `ctx`, a context of the stream, as the record's: a root by its edition, a node by the
/// record's context made for the stream's node of that index.
fn record_ctx(ctx: Ctx, made: &[blaze_tokens::Ctx]) -> blaze_tokens::Ctx {
    match ctx.node() {
        None => blaze_tokens::Ctx::root(
            ctx.root_edition().expect("a context that is no node of its table is an edition's root"),
        ),
        Some(node) => made[node as usize],
    }
}

fn record_mark(mark: Mark) -> blaze_tokens::Mark {
    blaze_tokens::Mark {
        of: match mark.of {
            MarkOf::This => blaze_tokens::MarkOf::This,
            MarkOf::Outer(key) => blaze_tokens::MarkOf::Outer(key),
        },
        transparency: mark.transparency,
        edition: mark.edition,
        site: mark.site,
    }
}

/// `origin`, a stream origin of record token `token`, counted in record tokens of its source.
fn record_origin(
    origin: TokenOrigin,
    maps: &OriginMaps<'_>,
    token: u32,
) -> Result<Origin, TokenRecordError> {
    let (at, map, record): (u32, Option<&[u32]>, fn(u32) -> Origin) = match origin {
        TokenOrigin::Input(at) => (at, maps.input, Origin::Input),
        TokenOrigin::AttrArgs(at) => (at, maps.attr_args, Origin::AttrArgs),
        TokenOrigin::Def(at) => (at, maps.body, Origin::Def),
        TokenOrigin::Params(at) => (at, maps.params, Origin::Params),
        TokenOrigin::Made(transparency) => return Ok(Origin::Made(transparency)),
    };
    let map = map.ok_or(TokenRecordError::NoSource { token, origin })?;
    // One entry per source token, then one for the record's end: an origin names a token.
    let tokens = map.len().saturating_sub(1);
    match (at as usize) < tokens {
        true => Ok(record(map[at as usize])),
        false => Err(TokenRecordError::PastSource { token, origin, tokens: index(tokens) }),
    }
}
