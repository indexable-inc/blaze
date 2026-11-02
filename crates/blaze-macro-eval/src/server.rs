//! blaze's server for a macro's `proc_macro` client: each request the client's
//! `Closure::call` sends, read from its buffer, answered as rustc's server
//! (`rustc_expand::proc_macro_server`) answers it, the reply written back in the bridge's
//! encoding ([`protocol`](crate::protocol) checks it is the pinned one).
//!
//! Token streams are owned handles, dense from 1, freed as the client takes them; spans are
//! interned handles, the same span always the same handle, as the bridge's `InternedStore`
//! gives them. A question only the database can answer (a span's text, file, line or
//! parent) is asked through `blaze_lower::answer`, and the expansion is then not kept: its key
//! does not say what it was told.

use std::{fmt::Write as _, ops::Bound};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_lower::{SubRequest, SubResponse};
use intern::Symbol;
use proc_macro_api::bridge::spans::{self, spanned};
use span::{Edition, FIXUP_ERASED_FILE_AST_ID_MARKER, Span, TextRange, TextSize};

use crate::{
    guest::Stop,
    lex,
    print::{self, Op},
    protocol::Method,
    stream::{DelimSpan, Delimiter, IdentKind, LitKind, Spacing, Stream, Tree, push_given},
    wire::{Reader, Width, Writer},
};

/// A diagnostic the macro emitted (`proc_macro::Diagnostic::emit`), or an error rustc's
/// session would have reported for it (a lexer error of `ts_from_str`), rendered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Emitted {
    pub(crate) error: bool,
    pub(crate) text: String,
}

/// One expansion's server.
pub(crate) struct Server {
    /// `streams[h - 1]` is the stream of handle `h`, `None` once the client took it.
    streams: Vec<Option<Stream>>,
    spans: Vec<Span>,
    span_handles: HashMap<Span, u32>,
    /// `Span::save_span`'s, by the index it gave.
    saved: Vec<Span>,
    /// The def, call and mixed sites.
    sites: [Span; 3],
    /// The calling crate's edition: what text the macro parses is lexed in.
    edition: Edition,
    /// Whether the calling crate enables `forced_keywords`, which gates each `k#` it lexes.
    forced_keywords: bool,
    /// The client's `usize`, which its requests and the replies are written in.
    width: Width,
    /// Whether the expansion learned of a span what its key does not say.
    pub(crate) asked: bool,
    /// Paths given to `proc_macro::tracked_path::path`: inputs as much as files it read.
    pub(crate) tracked: Vec<String>,
    pub(crate) emitted: Vec<Emitted>,
    /// What the lexer and printer write into, kept for the next request.
    text: String,
    lexed: Vec<String>,
    warned: Vec<String>,
}

fn freed() -> Stop {
    Stop("used a token stream it had given back (use-after-free in a `proc_macro` handle)".to_owned()) // alloc: error path
}

/// The end of the run of trees from `at` that rustc's stream holds as one token, and the
/// operator it is (`None` for one tree, or a lifetime).
fn glued(trees: &[Tree], at: usize) -> (usize, Option<Op>) {
    let Some(Tree::Punct { ch, .. }) = trees.get(at) else { return (at + 1, None) };
    let Some(mut op) = Op::of(*ch) else { return (at + 1, None) };
    let mut end = at;
    while let Tree::Punct { spacing: Spacing::Joint | Spacing::JointHidden, .. } = &trees[end] {
        match trees.get(end + 1) {
            Some(Tree::Punct { ch, .. }) => match Op::of(*ch).and_then(|next| op.glue(next)) {
                Some(glued) => {
                    op = glued;
                    end += 1;
                }
                None => break,
            },
            Some(Tree::Ident { .. }) if op == Op::SingleQuote => return (end + 2, None),
            _ => break,
        }
    }
    (end + 1, if end > at { Some(op) } else { None })
}

/// `n` as a byte of the bridge's enums.
fn delimiter_of(n: u8) -> Result<Delimiter, Stop> {
    Ok(match n {
        0 => Delimiter::Parenthesis,
        1 => Delimiter::Brace,
        2 => Delimiter::Bracket,
        3 => Delimiter::Invisible,
        // alloc: error path
        other => return Err(Stop(format!("sent the server the delimiter {other}, which the pinned bridge never sends"))),
    })
}

fn ident_kind_of(n: u8) -> Result<IdentKind, Stop> {
    Ok(match n {
        0 => IdentKind::Normal,
        1 => IdentKind::Raw,
        2 => IdentKind::ForcedKeyword,
        // alloc: error path
        other => return Err(Stop(format!("sent the server the ident kind {other}, which the pinned bridge never sends"))),
    })
}

fn ident_kind_tag(kind: IdentKind) -> u8 {
    match kind {
        IdentKind::Normal => 0,
        IdentKind::Raw => 1,
        IdentKind::ForcedKeyword => 2,
    }
}

fn lit_kind_of(r: &mut Reader<'_>) -> Result<LitKind, Stop> {
    Ok(match r.u8()? {
        0 => LitKind::Byte,
        1 => LitKind::Char,
        2 => LitKind::Integer,
        3 => LitKind::Float,
        4 => LitKind::Str,
        5 => LitKind::StrRaw(r.u8()?),
        6 => LitKind::ByteStr,
        7 => LitKind::ByteStrRaw(r.u8()?),
        8 => LitKind::CStr,
        9 => LitKind::CStrRaw(r.u8()?),
        10 => LitKind::Err,
        // alloc: error path
        other => return Err(Stop(format!("sent the server the literal kind {other}, which the pinned bridge never sends"))),
    })
}

fn put_lit_kind(w: &mut Writer<'_>, kind: LitKind) {
    match kind {
        LitKind::Byte => w.u8(0),
        LitKind::Char => w.u8(1),
        LitKind::Integer => w.u8(2),
        LitKind::Float => w.u8(3),
        LitKind::Str => w.u8(4),
        LitKind::StrRaw(n) => {
            w.u8(5);
            w.u8(n);
        }
        LitKind::ByteStr => w.u8(6),
        LitKind::ByteStrRaw(n) => {
            w.u8(7);
            w.u8(n);
        }
        LitKind::CStr => w.u8(8),
        LitKind::CStrRaw(n) => {
            w.u8(9);
            w.u8(n);
        }
        LitKind::Err => w.u8(10),
    }
}

/// A `Bound<usize>` as `spans::subspan` takes one: `0` unbounded, `4n + 1` included, `4n + 3`
/// excluded.
fn bound_word(bound: Bound<u64>) -> u64 {
    match bound {
        Bound::Unbounded => 0,
        Bound::Included(n) => n.saturating_mul(4).saturating_add(1),
        Bound::Excluded(n) => n.saturating_mul(4).saturating_add(3),
    }
}

impl Server {
    /// The server of one expansion at `sites` (def, call, mixed), for a crate of `edition`
    /// that enables `forced_keywords` or not, speaking to a client whose `usize` is `width`.
    pub(crate) fn new(sites: [Span; 3], edition: Edition, forced_keywords: bool, width: Width) -> Server {
        let mut server = Server {
            // alloc: once per expansion, its tables, which grow with what it builds
            streams: Vec::new(),
            spans: Vec::new(), // alloc: once per expansion, as above
            span_handles: HashMap::default(),
            saved: Vec::new(), // alloc: once per expansion, as above
            sites,
            edition,
            forced_keywords,
            width,
            asked: false,
            tracked: Vec::new(), // alloc: once per expansion, as above
            emitted: Vec::new(), // alloc: once per expansion, as above
            text: String::new(), // alloc: once per expansion, as above
            lexed: Vec::new(), // alloc: once per expansion, as above
            warned: Vec::new(), // alloc: once per expansion, as above
        };
        for site in sites {
            server.span_handle(site);
        }
        server
    }

    /// A new handle for `stream`.
    pub(crate) fn stream_handle(&mut self, stream: Stream) -> u32 {
        self.streams.push(Some(stream));
        self.streams.len() as u32
    }

    /// The stream of `handle`, taken: the client gave it up.
    pub(crate) fn take(&mut self, handle: u32) -> Result<Stream, Stop> {
        self.streams.get_mut((handle as usize).wrapping_sub(1)).and_then(Option::take).ok_or_else(freed)
    }

    fn stream(&self, handle: u32) -> Result<&Stream, Stop> {
        self.streams.get((handle as usize).wrapping_sub(1)).and_then(Option::as_ref).ok_or_else(freed)
    }

    pub(crate) fn span_handle(&mut self, span: Span) -> u32 {
        let next = self.spans.len() as u32 + 1;
        let handle = *self.span_handles.entry(span).or_insert(next);
        if handle == next {
            self.spans.push(span);
        }
        handle
    }

    fn span(&self, handle: u32) -> Result<Span, Stop> {
        self.spans.get((handle as usize).wrapping_sub(1)).copied().ok_or_else(|| {
            // alloc: error path
            Stop(format!("sent the server the span {handle}, which it never gave"))
        })
    }

    fn read_span(&self, r: &mut Reader<'_>) -> Result<Span, Stop> {
        let handle = r.handle()?;
        self.span(handle)
    }

    /// The bytes the client's `run` is given: the sites, then each input stream's handle.
    pub(crate) fn input(&mut self, inputs: &[Stream], out: &mut Vec<u8>) {
        out.clear();
        let mut w = Writer { out, width: self.width };
        for site in self.sites {
            let handle = self.span_handle(site);
            w.handle(handle);
        }
        for stream in inputs {
            // clone: a stream shares its trees, so this copies a pointer
            let handle = self.stream_handle(stream.clone());
            w.handle(handle);
        }
    }

    /// What `run` gave back: the output stream, or the macro's panic message.
    pub(crate) fn output(&mut self, bytes: &[u8]) -> Result<Result<Stream, String>, Stop> {
        let mut r = Reader::new(bytes, self.width);
        let ok = r.some()?;
        let some = r.some()?;
        let answer = match (ok, some) {
            (true, true) => {
                let handle = r.handle()?;
                Ok(self.take(handle)?)
            }
            (true, false) => Ok(Stream::default()),
            // alloc: the macro's panic message, given to the error that says it
            (false, true) => Err(r.str()?.to_owned()),
            (false, false) => Err("the proc macro panicked with a payload that is not a string".to_owned()), // alloc: error path
        };
        Ok(answer)
    }

    fn ask(&mut self, db: &Db, req: SubRequest) -> SubResponse {
        self.asked = true;
        blaze_lower::answer(db, req)
    }

    fn read_tree(&mut self, r: &mut Reader<'_>, out: &mut Vec<Tree>) -> Result<(), Stop> {
        let tree = match r.u8()? {
            0 => {
                let delimiter = delimiter_of(r.u8()?)?;
                let stream = match r.some()? {
                    true => {
                        let handle = r.handle()?;
                        self.take(handle)?
                    }
                    false => Stream::default(),
                };
                let open = self.read_span(r)?;
                let close = self.read_span(r)?;
                let entire = self.read_span(r)?;
                let span = DelimSpan { open, close, entire };
                Tree::Group { delimiter, stream, span, open: Spacing::Alone, spacing: Spacing::Alone }
            }
            1 => {
                let ch = r.u8()?;
                let joint = r.bool()?;
                let span = self.read_span(r)?;
                let spacing = if joint { Spacing::Joint } else { Spacing::Alone };
                Tree::Punct { ch, spacing, span }
            }
            2 => {
                let sym = Symbol::intern(r.str()?);
                let kind = ident_kind_of(r.u8()?)?;
                let span = self.read_span(r)?;
                Tree::Ident { sym, kind, span, spacing: Spacing::Alone }
            }
            3 => {
                let kind = lit_kind_of(r)?;
                let symbol = Symbol::intern(r.str()?);
                let suffix = match r.some()? {
                    true => Some(Symbol::intern(r.str()?)),
                    false => None,
                };
                let span = self.read_span(r)?;
                Tree::Literal { kind, symbol, suffix, span, spacing: Spacing::Alone }
            }
            // alloc: error path
            other => return Err(Stop(format!("sent the server the token tree tag {other}, which the pinned bridge never sends"))),
        };
        push_given(tree, out);
        Ok(())
    }

    fn put_literal(&mut self, w: &mut Writer<'_>, kind: LitKind, symbol: &Symbol, suffix: Option<&Symbol>, span: Span) {
        put_lit_kind(w, kind);
        w.str(symbol.as_str());
        w.some(suffix.is_some());
        if let Some(suffix) = suffix {
            w.str(suffix.as_str());
        }
        let handle = self.span_handle(span);
        w.handle(handle);
    }

    /// `tree` as the client reads a token tree (`FromInternal`): a doc comment as the
    /// attribute it stands for, a group's stream as a new handle.
    fn put_tree(&mut self, w: &mut Writer<'_>, tree: &Tree, count: &mut u64) {
        match tree {
            Tree::DocComment { kind, inner, data, span, .. } => {
                // alloc: a doc comment taken apart, the attribute it stands for
                let mut desugared = Vec::with_capacity(3);
                lex::desugar_doc(*kind, *inner, data, *span, &mut desugared);
                for tree in &desugared {
                    self.put_tree(w, tree, count);
                }
            }
            Tree::Group { delimiter, stream, span, .. } => {
                *count += 1;
                w.u8(0);
                w.u8(*delimiter as u8);
                w.some(true);
                // clone: a stream shares its trees, so this copies a pointer
                let handle = self.stream_handle(stream.clone());
                w.handle(handle);
                for span in [span.open, span.close, span.entire] {
                    let handle = self.span_handle(span);
                    w.handle(handle);
                }
            }
            Tree::Punct { span, spacing, .. } | Tree::Ident { span, spacing, .. } => {
                self.put_token(w, tree, *span, *spacing == Spacing::Joint, count)
            }
            Tree::Literal { kind, symbol, suffix, span, .. } => {
                *count += 1;
                w.u8(3);
                self.put_literal(w, *kind, symbol, suffix.as_ref(), *span);
            }
        }
    }

    /// A punctuation mark or identifier as the client reads it, at `span`; `joint` is the
    /// mark's.
    fn put_token(&mut self, w: &mut Writer<'_>, tree: &Tree, span: Span, joint: bool, count: &mut u64) {
        match tree {
            Tree::Punct { ch, .. } => {
                w.u8(1);
                w.u8(*ch);
                w.bool(joint);
            }
            Tree::Ident { sym, kind, .. } => {
                w.u8(2);
                w.str(sym.as_str());
                w.u8(ident_kind_tag(*kind));
            }
            _ => return,
        }
        *count += 1;
        let handle = self.span_handle(span);
        w.handle(handle);
    }

    /// `trees` as the client reads them (`FromInternal`) of the stream rustc would hold:
    /// `TokenStream::push_tree` glues each token to the one before it when that one is joint
    /// (`Token::glue`), so a run of punctuation marks is one operator spanning theirs
    /// (`Span::to`), split again into characters, each its byte of a span exactly the
    /// operator's length and else the whole span; a `'` glued to an identifier is a lifetime,
    /// both at its span. Streams the lexer made hold glued tokens split this way already.
    fn put_trees(&mut self, db: &Db, w: &mut Writer<'_>, trees: &[Tree], count: &mut u64) -> Result<(), Stop> {
        let mut at = 0;
        while at < trees.len() {
            let (end, op) = glued(trees, at);
            if end == at + 1 {
                self.put_tree(w, &trees[at], count);
                at = end;
                continue;
            }
            let mut span = trees[at].ends().0;
            for tree in &trees[at + 1..end] {
                span = self.to(db, span, tree.ends().0)?;
            }
            let split = op.is_some_and(|op| usize::from(span.range.len()) == op.text().len());
            let last = end - at - 1;
            for (i, tree) in trees[at..end].iter().enumerate() {
                // Each character but the last is joint; the last has the glued token's spacing.
                let joint = i < last || tree.spacing() == Spacing::Joint;
                let span = match split {
                    true => Span {
                        range: TextRange::at(span.range.start() + TextSize::from(i as u32), TextSize::from(1)),
                        ..span
                    },
                    false => span,
                };
                self.put_token(w, tree, span, joint, count);
            }
            at = end;
        }
        Ok(())
    }

    /// rustc's `Span::to`: the span from `first` to `second`. Of spans of two contexts it is
    /// the one not of the root (`prepare_to_combine`); two contexts neither at the root, or
    /// two files, would need what rustc's source map has, so the call stops.
    fn to(&mut self, db: &Db, first: Span, second: Span) -> Result<Span, Stop> {
        if first.ctx == second.ctx {
            return spans::join(first, second, |req| Some(self.ask(db, req))).ok_or_else(|| {
                // alloc: error path
                Stop(format!("glued punctuation of two files ({first:?} and {second:?}), whose joined span blaze does not have"))
            });
        }
        match (first.ctx.is_root(), second.ctx.is_root()) {
            (true, _) => Ok(second),
            (false, true) => Ok(first),
            // alloc: error path
            (false, false) => Err(Stop(format!(
                "glued punctuation of two macro expansions ({first:?} and {second:?}), whose joined span blaze does not have"
            ))),
        }
    }

    /// A diagnostic and its children, each on a line of `text` indented by its depth: whether
    /// it is an error.
    fn read_diagnostic(&mut self, r: &mut Reader<'_>, depth: usize, text: &mut String) -> Result<bool, Stop> {
        let level = r.u8()?;
        let message = r.str()?;
        let spans = r.count()?;
        for _ in 0..spans {
            self.read_span(r)?;
        }
        let (error, name) = match level {
            0 => (true, "error"),
            1 => (false, "warning"),
            2 => (false, "note"),
            3 => (false, "help"),
            // alloc: error path
            other => return Err(Stop(format!("sent the server the diagnostic level {other}, which the pinned bridge never sends"))),
        };
        if depth > 0 {
            text.push('\n');
        }
        _ = write!(text, "{:indent$}{name}: {message}", "", indent = depth * 2);
        let children = r.count()?;
        for _ in 0..children {
            self.read_diagnostic(r, depth + 1, text)?;
        }
        Ok(error)
    }

    /// Answers the request `request`, writing the reply, `Result<_, PanicMessage>`, to `reply`.
    pub(crate) fn dispatch(&mut self, db: &Db, request: &[u8], reply: &mut Vec<u8>) -> Result<(), Stop> {
        reply.clear();
        let mut r = Reader::new(request, self.width);
        let tag = r.u8()?;
        let Some(method) = Method::of(tag) else {
            // alloc: error path
            return Err(Stop(format!("sent the server the request {tag}, which the pinned bridge has not")));
        };
        let mut w = Writer { out: reply, width: self.width };
        match method {
            Method::TrackEnvVar => {
                // The variable was read through `getenv`, which traced it.
                r.str()?;
                if r.some()? {
                    r.str()?;
                }
                w.ok(true);
            }
            Method::TrackPath => {
                // alloc: a tracked path, an input of the expansion
                self.tracked.push(r.str()?.to_owned());
                w.ok(true);
            }
            Method::LiteralFromStr => {
                let text = r.str()?;
                let call_site = self.sites[1];
                self.lexed.clear();
                let lexed = lex::literal(text, call_site, self.edition, self.forced_keywords, &mut self.lexed, &mut self.warned);
                self.lexed_errors();
                w.ok(true);
                match lexed {
                    Ok(Tree::Literal { kind, symbol, suffix, span, .. }) => {
                        w.ok(true);
                        self.put_literal(&mut w, kind, &symbol, suffix.as_ref(), span);
                    }
                    Ok(_) => {
                        return Err(Stop("got a literal from the lexer that is not one".to_owned())); // alloc: error path
                    }
                    Err(why) => {
                        w.ok(false);
                        w.str(&why);
                    }
                }
            }
            Method::EmitDiagnostic => {
                // alloc: a diagnostic the macro emitted, said once
                let mut text = String::new();
                let error = self.read_diagnostic(&mut r, 0, &mut text)?;
                self.emitted.push(Emitted { error, text });
                w.ok(true);
            }
            Method::TsDrop => {
                let handle = r.handle()?;
                self.take(handle)?;
                w.ok(true);
            }
            Method::TsClone => {
                let handle = r.handle()?;
                // clone: a stream shares its trees, so this copies a pointer
                let stream = self.stream(handle)?.clone();
                let handle = self.stream_handle(stream);
                w.ok(true);
                w.handle(handle);
            }
            Method::TsIsEmpty => {
                let handle = r.handle()?;
                let empty = self.stream(handle)?.is_empty();
                w.ok(true);
                w.bool(empty);
            }
            Method::TsExpandExpr => {
                return Err(Stop(
                    "asked the compiler to expand an expression (`TokenStream::expand_expr`), \
                     which blaze's server does not do"
                        .to_owned(), // alloc: error path
                ));
            }
            Method::TsFromStr => {
                let text = r.str()?;
                let call_site = self.sites[1];
                self.lexed.clear();
                let lexed = lex::stream(text, call_site, self.edition, self.forced_keywords, &mut self.lexed, &mut self.warned);
                self.lexed_errors();
                w.ok(true);
                match lexed {
                    Ok(stream) => {
                        let handle = self.stream_handle(stream);
                        w.ok(true);
                        w.handle(handle);
                    }
                    Err(why) => {
                        w.ok(false);
                        w.str(&why);
                    }
                }
            }
            Method::TsToString => {
                let handle = r.handle()?;
                let mut text = std::mem::take(&mut self.text);
                text.clear();
                // A span's edition is its context's, which the key numbers but does not say.
                let mut asked = false;
                print::to_string(self.stream(handle)?.trees(), &mut text, |span| {
                    asked = true;
                    span.ctx.edition(db)
                });
                self.asked |= asked;
                w.ok(true);
                w.str(&text);
                self.text = text;
            }
            Method::TsFromTokenTree => {
                // alloc: the new stream's trees
                let mut trees = Vec::with_capacity(2);
                self.read_tree(&mut r, &mut trees)?;
                let handle = self.stream_handle(Stream::of(trees));
                w.ok(true);
                w.handle(handle);
            }
            Method::TsConcatTrees => {
                let mut stream = match r.some()? {
                    true => {
                        let handle = r.handle()?;
                        self.take(handle)?
                    }
                    false => Stream::default(),
                };
                let count = r.count()?;
                if count > 0 {
                    let mut trees = std::mem::take(stream.trees_mut());
                    trees.reserve(count);
                    for _ in 0..count {
                        self.read_tree(&mut r, &mut trees)?;
                    }
                    stream = Stream::of(trees);
                }
                let handle = self.stream_handle(stream);
                w.ok(true);
                w.handle(handle);
            }
            Method::TsConcatStreams => {
                let mut stream = match r.some()? {
                    true => {
                        let handle = r.handle()?;
                        self.take(handle)?
                    }
                    false => Stream::default(),
                };
                let count = r.count()?;
                for _ in 0..count {
                    let handle = r.handle()?;
                    let next = self.take(handle)?;
                    if stream.is_empty() {
                        stream = next;
                    } else if !next.is_empty() {
                        stream.trees_mut().extend(next.into_trees());
                    }
                }
                let handle = self.stream_handle(stream);
                w.ok(true);
                w.handle(handle);
            }
            Method::TsIntoTrees => {
                let handle = r.handle()?;
                let stream = self.take(handle)?;
                w.ok(true);
                // The count is written once the trees are: a doc comment is three.
                let at = w.out.len();
                w.usize(0);
                let mut count = 0;
                self.put_trees(db, &mut w, stream.trees(), &mut count)?;
                w.set_usize(at, count);
            }
            Method::SpanDebug => {
                let span = self.read_span(&mut r)?;
                // What it says is of the database's numbering, which the key does not have.
                self.asked = true;
                let mut text = std::mem::take(&mut self.text);
                text.clear();
                _ = write!(text, "{span:?}");
                w.ok(true);
                w.str(&text);
                self.text = text;
            }
            Method::SpanParent => {
                let span = self.read_span(&mut r)?;
                let req = SubRequest::SpanParent {
                    file_id: span.anchor.file_id.as_u32(),
                    ast_id: span.anchor.ast_id.into_raw(),
                    start: span.range.start().into(),
                    end: span.range.end().into(),
                    ctx: span.ctx.into_u32(),
                };
                let parent = match self.ask(db, req) {
                    SubResponse::SpanParentResult { parent_span: Some(it) } => {
                        Some(spanned(it.file_id, it.ast_id, it.start, it.end, it.ctx))
                    }
                    _ => None,
                };
                w.ok(true);
                w.some(parent.is_some());
                if let Some(parent) = parent {
                    let handle = self.span_handle(parent);
                    w.handle(handle);
                }
            }
            Method::SpanSource => {
                let span = self.read_span(&mut r)?;
                let req = SubRequest::SpanSource {
                    file_id: span.anchor.file_id.as_u32(),
                    ast_id: span.anchor.ast_id.into_raw(),
                    start: span.range.start().into(),
                    end: span.range.end().into(),
                    ctx: span.ctx.into_u32(),
                };
                let source = match self.ask(db, req) {
                    SubResponse::SpanSourceResult { file_id, ast_id, start, end, ctx } => {
                        spanned(file_id, ast_id, start, end, ctx)
                    }
                    _ => span,
                };
                let handle = self.span_handle(source);
                w.ok(true);
                w.handle(handle);
            }
            Method::SpanByteRange => {
                let span = self.read_span(&mut r)?;
                let req = SubRequest::ByteRange {
                    file_id: span.anchor.file_id.as_u32(),
                    ast_id: span.anchor.ast_id.into_raw(),
                    start: span.range.start().into(),
                    end: span.range.end().into(),
                };
                let range = match self.ask(db, req) {
                    SubResponse::ByteRangeResult { range } => range,
                    _ => 0..0,
                };
                w.ok(true);
                w.usize(range.start as u64);
                w.usize(range.end as u64);
            }
            Method::SpanStart | Method::SpanEnd => {
                let span = self.read_span(&mut r)?;
                let at = match (span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER, method) {
                    (true, _) => span,
                    (false, Method::SpanStart) => Span { range: TextRange::empty(span.range.start()), ..span },
                    (false, _) => Span { range: TextRange::empty(span.range.end()), ..span },
                };
                let handle = self.span_handle(at);
                w.ok(true);
                w.handle(handle);
            }
            Method::SpanLine | Method::SpanColumn => {
                let span = self.read_span(&mut r)?;
                let req = SubRequest::LineColumn {
                    file_id: span.anchor.file_id.as_u32(),
                    ast_id: span.anchor.ast_id.into_raw(),
                    offset: span.range.start().into(),
                };
                let (line, column) = match self.ask(db, req) {
                    SubResponse::LineColumnResult { line, column } => (line, column),
                    _ => (1, 1),
                };
                let answer = match method {
                    Method::SpanLine => line,
                    _ => column,
                };
                w.ok(true);
                w.usize(u64::from(answer));
            }
            Method::SpanFile | Method::SpanLocalFile => {
                let span = self.read_span(&mut r)?;
                let file_id = span.anchor.file_id.file_id().index();
                let local = method == Method::SpanLocalFile;
                let req = match local {
                    true => SubRequest::LocalFilePath { file_id },
                    false => SubRequest::FilePath { file_id },
                };
                let name = match self.ask(db, req) {
                    SubResponse::FilePathResult { name } => Some(name),
                    SubResponse::LocalFilePathResult { name } => name,
                    _ => None,
                };
                w.ok(true);
                match (local, name) {
                    (false, name) => w.str(name.as_deref().unwrap_or("")),
                    (true, Some(name)) => {
                        w.some(true);
                        w.str(&name);
                    }
                    (true, None) => w.some(false),
                }
            }
            Method::SpanJoin => {
                let first = self.read_span(&mut r)?;
                let second = self.read_span(&mut r)?;
                let joined = spans::join(first, second, |req| Some(self.ask(db, req)));
                w.ok(true);
                w.some(joined.is_some());
                if let Some(joined) = joined {
                    let handle = self.span_handle(joined);
                    w.handle(handle);
                }
            }
            Method::SpanSubspan => {
                let span = self.read_span(&mut r)?;
                let start = r.bound()?;
                let end = r.bound()?;
                let sub = spans::subspan(span, bound_word(start), bound_word(end));
                w.ok(true);
                w.some(sub.is_some());
                if let Some(sub) = sub {
                    let handle = self.span_handle(sub);
                    w.handle(handle);
                }
            }
            Method::SpanResolvedAt => {
                let span = self.read_span(&mut r)?;
                let at = self.read_span(&mut r)?;
                let handle = self.span_handle(Span { ctx: at.ctx, ..span });
                w.ok(true);
                w.handle(handle);
            }
            Method::SpanSourceText => {
                let span = self.read_span(&mut r)?;
                let req = SubRequest::SourceText {
                    file_id: span.anchor.file_id.as_u32(),
                    ast_id: span.anchor.ast_id.into_raw(),
                    start: span.range.start().into(),
                    end: span.range.end().into(),
                };
                let text = match self.ask(db, req) {
                    SubResponse::SourceTextResult { text } => text,
                    _ => None,
                };
                w.ok(true);
                w.some(text.is_some());
                if let Some(text) = text {
                    w.str(&text);
                }
            }
            Method::SpanSaveSpan => {
                let span = self.read_span(&mut r)?;
                self.saved.push(span);
                w.ok(true);
                w.usize(self.saved.len() as u64 - 1);
            }
            Method::SpanRecoverProcMacroSpan => {
                let id = r.usize()?;
                let Some(&span) = usize::try_from(id).ok().and_then(|id| self.saved.get(id)) else {
                    // alloc: error path
                    return Err(Stop(format!("recovered the span {id}, which it never saved")));
                };
                let handle = self.span_handle(span);
                w.ok(true);
                w.handle(handle);
            }
            Method::SymbolNormalizeAndValidateIdent => {
                let text = r.str()?;
                w.ok(true);
                match lex::ident(text) {
                    Ok(sym) => {
                        w.ok(true);
                        w.str(sym.as_str());
                    }
                    Err(()) => w.ok(false),
                }
            }
        }
        if !r.is_empty() {
            // alloc: error path
            return Err(Stop(format!("sent `{}` more than the pinned bridge sends", method.name())));
        }
        Ok(())
    }

    /// The lexer's errors, errors of the expansion, and its lints, warnings of it.
    fn lexed_errors(&mut self) {
        for text in self.lexed.drain(..) {
            self.emitted.push(Emitted { error: true, text });
        }
        for text in self.warned.drain(..) {
            self.emitted.push(Emitted { error: false, text });
        }
    }
}

#[cfg(test)]
mod tests;
