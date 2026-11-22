//! THE BRIDGE'S SERVER, in the macro's own process: the state one call's requests are answered
//! from, a sans-IO state machine the session (session.rs) hands each request's bytes. Ported from
//! blaze-proc-macro server.rs with no `std`, the allocator handed in and every error typed; that
//! file is deleted when the wasm host takes this one (piece 2), so the two never answer apart.
//!
//! Living on the macro's side of the process boundary, the server is not trusted: nothing it says
//! is taken by blaze unchecked. It answers the 31 methods from the call's tokens alone; the three
//! that need the world are asks the session sends to blaze ([`Ask`]): `track_env_var`,
//! `track_path`, and `span_file`, whose answer blaze decides from the span's place (it knows which
//! input token was written in the call's file; the server does not). Its output is the outcome's
//! bytes (outcome.rs), every token, origin and diagnostic of which the host checks again.
//!
//! A token stream is flat, as blaze-expand's is: a group is its opening token, its tokens and its
//! closing token, so no walk here recurses. Each stream the client holds is a handle into the
//! server's streams; an owned handle the client passes back is taken out, as rustc's server does.
//!
//! A span is a handle: where it points (a token of the call's input or of its attribute's
//! arguments, or a site) and whose hygiene it has, a [`TokenOrigin`]. Nothing in a span is a
//! position in a file: `line`, `column`, `local_file` and `byte_range` are refused
//! ([`ServerError::Position`], rule 71), and `source_text` prints the tokens it covers.
//!
//! # Which operations make a span
//!
//! Every span is one of the call's: the three sites (handles 1 to 3) and one per input or
//! attribute-argument token, made when the call starts. After that a span is made only by
//! `resolved_at` (the place of one, the hygiene of the other), `join` (the hygiene of the first,
//! the place of both, over one list) and `literal_from_str`/`ts_from_str` (`call_site`). Each
//! takes its hygiene from a span already held, so every origin is one of the call's: an input
//! token's, an attribute argument's, or a site's transparency. The host checks that set again
//! (outcome.rs), with each index under the call's lengths.

use core::fmt;

use blaze_expand::{Delim, Edition, LitKind, ProcKind, ProcLevel, Raw, Spacing, TokenKind, TokenOrigin, TokenStream, Transparency, lex};
use blaze_own::{AVec, Alloc, Vec};

use crate::bridge::{Field, Malformed, Reader, Writer, index, len64, wide};
use crate::call::{CallView, Tokens, delim_of, delim_tag, lit_read, lit_write};
use crate::method::Method;
use crate::outcome::{OutcomeWriter, write_stopped};

/// The most tokens the streams of one call hold together, the most bytes of symbol text (and of
/// diagnostic text), spans, live handles and diagnostics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ServerLimits {
    pub tokens: usize,
    pub text: usize,
    pub spans: usize,
    pub handles: usize,
    /// The host's own cap (outcome.rs `OutcomeLimits::most_diagnostics`), so the server stops
    /// where the host would refuse.
    pub diagnostics: usize,
}

impl ServerLimits {
    /// Far past what serde's, clap's or `sol!`'s largest expansions in tempo make (each under a
    /// million tokens), and small enough that a runaway macro stops before it fills memory.
    pub const SHIPPED: ServerLimits = ServerLimits { tokens: 1 << 24, text: 1 << 28, spans: 1 << 24, handles: 1 << 22, diagnostics: 4096 };
}

/// Which of [`ServerLimits`] was passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServerLimit {
    Tokens,
    Text,
    Spans,
    Handles,
    Diagnostics,
}

impl ServerLimit {
    fn byte(self) -> u8 {
        match self {
            ServerLimit::Tokens => 0,
            ServerLimit::Text => 1,
            ServerLimit::Spans => 2,
            ServerLimit::Handles => 3,
            ServerLimit::Diagnostics => 4,
        }
    }

    fn from_byte(byte: u8) -> Option<ServerLimit> {
        Some(match byte {
            0 => ServerLimit::Tokens,
            1 => ServerLimit::Text,
            2 => ServerLimit::Spans,
            3 => ServerLimit::Handles,
            4 => ServerLimit::Diagnostics,
            _ => return None,
        })
    }
}

/// Why the server stopped a call. It crosses to blaze inside a stopped outcome (outcome.rs) as
/// the macro's process's own account; blaze reports it, and takes nothing else from that run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServerError {
    /// A request no client of the pinned bridge sends, in `method` (`None`: its tag byte).
    Malformed { method: Option<Method>, what: Malformed },
    /// A request tag past the bridge's methods.
    NoSuchMethod(u8),
    /// A handle the server never gave, or gave and took back.
    NoSuchStream { method: Method, handle: u32 },
    NoSuchSpan { method: Method, handle: u32 },
    /// A `Punct` of a byte rustc's lexer makes no punctuation of.
    BadPunct { method: Method, ch: u8 },
    Limit(ServerLimit),
    /// A question about where a span sits in its file (`line`, `column`, `local_file`,
    /// `byte_range`), which is not answered: an expansion is keyed by its item, not its place
    /// (rule 71); `local_file` is a path on this machine (rule 25).
    Position { method: Method },
    /// `span_file`, which blaze refused (it holds why: a site's file, a made token's, a call
    /// whose file it does not show).
    FileRefused,
    /// The call's bytes are not the format's (a process started on another's bytes).
    Call(Malformed),
    /// The client's output is not `Result<TokenStream, PanicMessage>`.
    Output(Malformed),
    /// The client's output names a stream the server does not hold.
    NoSuchOutput { handle: u32 },
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::Malformed { method: Some(method), what } => write!(f, "sent `{}` {what}", method.name()),
            ServerError::Malformed { method: None, what } => write!(f, "sent a request with {what}"),
            ServerError::NoSuchMethod(tag) => write!(f, "sent the request tag {tag}, which no method of the pinned bridge has"),
            ServerError::NoSuchStream { method, handle } => {
                write!(f, "passed `{}` the token stream {handle}, which it does not hold", method.name())
            }
            ServerError::NoSuchSpan { method, handle } => {
                write!(f, "passed `{}` the span {handle}, which the server never gave", method.name())
            }
            ServerError::BadPunct { method, ch } => write!(f, "passed `{}` a `Punct` of the byte {ch}, which is no punctuation", method.name()),
            ServerError::Limit(limit) => write!(f, "went past its {limit:?} limit"),
            ServerError::Position { method } => write!(
                f,
                "asked `{}`, where a span sits in its file, which blaze does not answer: an expansion is keyed by its item, not its place (rule 71)",
                method.name()
            ),
            ServerError::FileRefused => f.write_str("asked `span_file`, which blaze refused"),
            ServerError::Call(what) => write!(f, "was started on a call's bytes with {what}"),
            ServerError::Output(what) => write!(f, "gave as its output {what}"),
            ServerError::NoSuchOutput { handle } => write!(f, "gave as its output the token stream {handle}, which it does not hold"),
        }
    }
}

const NO_METHOD: u8 = u8::MAX;

fn method_byte(method: Option<Method>) -> u8 {
    method.map_or(NO_METHOD, Method::tag)
}

fn method_of(byte: u8) -> Result<Method, Malformed> {
    Method::of(byte).ok_or(Malformed::Tag(Field::Method, byte))
}

impl ServerError {
    pub(crate) fn write(self, w: &mut Writer<'_, '_>) {
        fn malformed(w: &mut Writer<'_, '_>, m: Malformed) {
            for byte in m.bytes() {
                w.u8(byte);
            }
        }
        match self {
            ServerError::Malformed { method, what } => {
                w.u8(0);
                w.u8(method_byte(method));
                malformed(w, what);
            }
            ServerError::NoSuchMethod(tag) => {
                w.u8(1);
                w.u8(tag);
            }
            ServerError::NoSuchStream { method, handle } => {
                w.u8(2);
                w.u8(method.tag());
                w.u32(handle);
            }
            ServerError::NoSuchSpan { method, handle } => {
                w.u8(3);
                w.u8(method.tag());
                w.u32(handle);
            }
            ServerError::BadPunct { method, ch } => {
                w.u8(4);
                w.u8(method.tag());
                w.u8(ch);
            }
            ServerError::Limit(limit) => {
                w.u8(5);
                w.u8(limit.byte());
            }
            ServerError::Position { method } => {
                w.u8(6);
                w.u8(method.tag());
            }
            ServerError::FileRefused => w.u8(7),
            ServerError::Call(what) => {
                w.u8(8);
                malformed(w, what);
            }
            ServerError::Output(what) => {
                w.u8(9);
                malformed(w, what);
            }
            ServerError::NoSuchOutput { handle } => {
                w.u8(10);
                w.u32(handle);
            }
        }
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<ServerError, Malformed> {
        fn malformed(r: &mut Reader<'_>) -> Result<Malformed, Malformed> {
            let bytes = [r.u8()?, r.u8()?, r.u8()?];
            Malformed::from_bytes(bytes).ok_or(Malformed::Tag(Field::Error, bytes[0]))
        }
        Ok(match r.tag(Field::Error)? {
            0 => {
                let method = match r.u8()? {
                    NO_METHOD => None,
                    byte => Some(method_of(byte)?),
                };
                ServerError::Malformed { method, what: malformed(r)? }
            }
            1 => ServerError::NoSuchMethod(r.u8()?),
            2 => ServerError::NoSuchStream { method: method_of(r.u8()?)?, handle: r.u32()? },
            3 => ServerError::NoSuchSpan { method: method_of(r.u8()?)?, handle: r.u32()? },
            4 => ServerError::BadPunct { method: method_of(r.u8()?)?, ch: r.u8()? },
            5 => {
                let byte = r.tag(Field::Limit)?;
                ServerError::Limit(ServerLimit::from_byte(byte).ok_or(Malformed::Tag(Field::Limit, byte))?)
            }
            6 => ServerError::Position { method: method_of(r.u8()?)? },
            7 => ServerError::FileRefused,
            8 => ServerError::Call(malformed(r)?),
            9 => ServerError::Output(malformed(r)?),
            10 => ServerError::NoSuchOutput { handle: r.u32()? },
            other => return Err(Malformed::Tag(Field::Error, other)),
        })
    }
}

/// Where a span points.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// Tokens `lo..=hi` of the call's input.
    Input { lo: u32, hi: u32 },
    /// Of the attribute's arguments.
    Attr { lo: u32, hi: u32 },
    /// Nowhere in the call: a site.
    Site,
}

/// `span_file`'s question as it crosses to blaze: the span's first token's place. A joined or
/// resolved span's file is its first token's (rustc's `Span::file`).
pub(crate) fn put_place(w: &mut Writer<'_, '_>, place: Place) {
    let (tag, lo) = match place {
        Place::Input { lo, .. } => (0, lo),
        Place::Attr { lo, .. } => (1, lo),
        Place::Site => (2, 0),
    };
    w.u8(tag);
    w.u32(lo);
}

pub(crate) fn read_place(r: &mut Reader<'_>) -> Result<Place, Malformed> {
    let tag = r.tag(Field::Place)?;
    let lo = r.u32()?;
    Ok(match tag {
        0 => Place::Input { lo, hi: lo },
        1 => Place::Attr { lo, hi: lo },
        2 => Place::Site,
        other => return Err(Malformed::Tag(Field::Place, other)),
    })
}

/// A request the server cannot answer from the call alone: the session asks blaze, then writes
/// the reply ([`Server::file_reply`], [`Server::done_reply`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ask<'r> {
    /// `proc_macro::tracked_env::var`: blaze records the variable with its granted value.
    TrackEnv { name: &'r str, value: Option<&'r str> },
    /// `proc_macro::tracked_path::path`: blaze looks it up under the grants, records it, or
    /// refuses it by name.
    TrackPath { path: &'r str },
    /// `Span::file` of a span at `place`.
    SpanFile { place: Place },
}

/// What a request did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dispatched<'r> {
    /// The reply is written.
    Replied,
    Ask(Ask<'r>),
}

/// A symbol: a range of the server's text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sym {
    at: u32,
    len: u32,
}

/// A span handle, nonzero.
type SpanId = u32;

/// One token of a server stream.
#[derive(Clone, Copy, Debug)]
enum Tok {
    /// A group's opening, with its three spans.
    Open { delim: Delim, open: SpanId, close: SpanId, entire: SpanId },
    Close { delim: Delim },
    Ident { sym: Sym, raw: bool, span: SpanId },
    Punct { ch: u8, joint: bool, span: SpanId },
    Literal { kind: LitKind, sym: Sym, suffix: Option<Sym>, span: SpanId },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SpanData {
    place: Place,
    hygiene: TokenOrigin,
}

/// The fixed handles of `ExpnGlobals`: `def_site`, `call_site`, `mixed_site`, in its order.
const DEF_SITE: SpanId = 1;
const CALL_SITE: SpanId = 2;
const MIXED_SITE: SpanId = 3;

/// A diagnostic the macro emitted: its level, its message's range in `diag_text`, its first span.
#[derive(Clone, Copy, Debug)]
struct Diag {
    level: ProcLevel,
    message: (u32, u32),
    span: Option<SpanId>,
}

/// Which list of the call a token is of.
#[derive(Clone, Copy)]
enum Side {
    Input,
    Attr,
}

impl Side {
    fn place(self, lo: u32, hi: u32) -> Place {
        match self {
            Side::Input => Place::Input { lo, hi },
            Side::Attr => Place::Attr { lo, hi },
        }
    }

    fn origin(self, at: u32) -> TokenOrigin {
        match self {
            Side::Input => TokenOrigin::Input(at),
            Side::Attr => TokenOrigin::AttrArgs(at),
        }
    }

    fn span(self, lo: u32, hi: u32) -> SpanData {
        SpanData { place: self.place(lo, hi), hygiene: self.origin(lo) }
    }
}

/// The client's run output, read.
enum Returned<'o> {
    /// The stream of this handle; `None` for the empty stream.
    Tokens(Option<u32>),
    /// A panic, with its message if it had one (`PanicMessage` is encoded as `Option<&str>`).
    Panicked(Option<&'o str>),
}

fn read_output(output: &[u8]) -> Result<Returned<'_>, Malformed> {
    let mut r = Reader::new(output);
    let returned = if r.ok()? {
        Returned::Tokens(if r.some()? { Some(r.handle()?) } else { None })
    } else {
        Returned::Panicked(if r.some()? { Some(r.str()?) } else { None })
    };
    r.end()?;
    Ok(returned)
}

/// One call's server.
pub struct Server<'a> {
    edition: Edition,
    limits: ServerLimits,
    /// Streams by handle - 1; `None` once taken back.
    streams: AVec<'a, Option<AVec<'a, Tok>>>,
    live: usize,
    tokens: usize,
    /// Spans by handle - 1.
    spans: AVec<'a, SpanData>,
    text: AVec<'a, u8>,
    /// The call's input and attribute arguments as server tokens: what `source_text` prints.
    input: AVec<'a, Tok>,
    attr: AVec<'a, Tok>,
    diags: AVec<'a, Diag>,
    diag_text: AVec<'a, u8>,
    /// Requests answered.
    requests: u64,
    alloc: Alloc<'a>,
}

impl<'a> Server<'a> {
    /// The server of `call`, and the bytes the client is run on (`ExpnGlobals`, then the input's
    /// handles) written to `run_input`. An attribute macro always gets two streams,
    /// `(arguments, item)`, as rustc passes them; a derive or a function-like macro its one.
    pub fn new(call: &CallView<'_>, limits: ServerLimits, alloc: Alloc<'a>, run_input: &mut AVec<'_, u8>) -> Result<Server<'a>, ServerError> {
        let attr = match (call.kind, call.attr) {
            (ProcKind::Attr, Some(attr)) => Some(attr),
            (ProcKind::Attr, None) => return Err(ServerError::Call(Malformed::Tag(Field::OptionTag, 1))),
            (ProcKind::FnLike | ProcKind::Derive, None) => None,
            (ProcKind::FnLike | ProcKind::Derive, Some(_)) => return Err(ServerError::Call(Malformed::Tag(Field::OptionTag, 0))),
        };
        let mut server = Server {
            edition: call.edition,
            limits,
            streams: Vec::new_in(alloc),
            live: 0,
            tokens: 0,
            spans: Vec::new_in(alloc),
            text: Vec::new_in(alloc),
            input: Vec::new_in(alloc),
            attr: Vec::new_in(alloc),
            diags: Vec::new_in(alloc),
            diag_text: Vec::new_in(alloc),
            requests: 0,
            alloc,
        };
        for t in [Transparency::Opaque, Transparency::Transparent, Transparency::SemiOpaque] {
            server.span(SpanData { place: Place::Site, hygiene: TokenOrigin::Made(t) })?;
        }
        let item = server.import(call.input, Side::Input)?;
        server.input = Vec::from_slice_in(&item[..], alloc);
        let attr_handle = match attr {
            Some(attr) => {
                let args = server.import(attr, Side::Attr)?;
                server.attr = Vec::from_slice_in(&args[..], alloc);
                Some(server.hold(args)?)
            }
            None => None,
        };
        let item = server.hold(item)?;
        run_input.clear();
        let mut w = Writer { out: run_input };
        w.handle(DEF_SITE);
        w.handle(CALL_SITE);
        w.handle(MIXED_SITE);
        if let Some(attr) = attr_handle {
            w.handle(attr);
        }
        w.handle(item);
        Ok(server)
    }

    /// Requests answered so far.
    pub fn requests(&self) -> u64 {
        self.requests
    }

    fn span(&mut self, data: SpanData) -> Result<SpanId, ServerError> {
        if self.spans.len() >= self.limits.spans {
            return Err(ServerError::Limit(ServerLimit::Spans));
        }
        self.spans.push(data);
        Ok(index(self.spans.len()))
    }

    fn span_data(&self, method: Method, handle: SpanId) -> Result<SpanData, ServerError> {
        handle
            .checked_sub(1)
            .and_then(|at| self.spans.get(wide(at)).copied())
            .ok_or(ServerError::NoSuchSpan { method, handle })
    }

    /// The origin of a span the server holds. Every span a token holds was checked when it was
    /// stored; were one not held, the origin given is `Def`, which the host refuses by name
    /// (outcome.rs), so the impossible case is a refusal, never a quiet origin.
    fn origin(&self, span: SpanId) -> TokenOrigin {
        match span.checked_sub(1).and_then(|at| self.spans.get(wide(at))) {
            Some(data) => data.hygiene,
            None => TokenOrigin::Def(span),
        }
    }

    fn sym(&mut self, text: &str) -> Result<Sym, ServerError> {
        if self.text.len() + text.len() > self.limits.text {
            return Err(ServerError::Limit(ServerLimit::Text));
        }
        let at = index(self.text.len());
        self.text.extend_from_slice(text.as_bytes());
        Ok(Sym { at, len: index(text.len()) })
    }

    fn sym_text(&self, sym: Sym) -> &str {
        // Each symbol is a whole `&str` the server pushed.
        let start = wide(sym.at);
        self.text.get(start..start + wide(sym.len)).and_then(|b| core::str::from_utf8(b).ok()).unwrap_or("")
    }

    /// The call's `tokens` as server tokens, each spanned as a token of `side` (a group's
    /// `entire` from its opening to its closing token).
    fn import(&mut self, tokens: Tokens<'_>, side: Side) -> Result<AVec<'a, Tok>, ServerError> {
        let count = tokens.len();
        // The closing token of each opening one.
        let mut closes: AVec<'a, u32> = Vec::with_capacity_in(count, self.alloc);
        closes.resize(count, 0);
        let mut open: AVec<'a, (usize, Delim)> = Vec::new_in(self.alloc);
        for (at, token) in tokens.iter().enumerate() {
            let (kind, _) = token.map_err(ServerError::Call)?;
            match kind {
                TokenKind::Open(delim) => open.push((at, delim)),
                TokenKind::Close(delim) => match open.pop() {
                    Some((opened, d)) if d == delim => closes[opened] = index(at),
                    _ => return Err(ServerError::Call(Malformed::Tag(Field::Delimiter, delim_tag(delim)))),
                },
                _ => {}
            }
        }
        if !open.is_empty() {
            return Err(ServerError::Call(Malformed::Short(Field::Delimiter)));
        }
        let mut out = Vec::with_capacity_in(count, self.alloc);
        for (at, token) in tokens.iter().enumerate() {
            let (kind, text) = token.map_err(ServerError::Call)?;
            let i = index(at);
            let tok = match kind {
                TokenKind::Open(delim) => {
                    let close = closes[at];
                    let open = self.span(side.span(i, i))?;
                    let close_span = self.span(side.span(close, close))?;
                    let entire = self.span(SpanData { place: side.place(i, close), hygiene: side.origin(i) })?;
                    Tok::Open { delim, open, close: close_span, entire }
                }
                TokenKind::Close(delim) => Tok::Close { delim },
                TokenKind::Ident { raw, .. } => Tok::Ident { sym: self.sym(text)?, raw: raw == Raw::Yes, span: self.span(side.span(i, i))? },
                TokenKind::Punct { char, spacing } => {
                    let ch = u8::try_from(char).map_err(|_| ServerError::Call(Malformed::Tag(Field::Char, 0)))?;
                    Tok::Punct { ch, joint: spacing == Spacing::Joint, span: self.span(side.span(i, i))? }
                }
                TokenKind::Literal { kind, suffix_len, .. } => {
                    let split = text.len().checked_sub(wide(suffix_len)).ok_or(ServerError::Call(Malformed::Short(Field::Str)))?;
                    let (body, suffix) = text.split_at_checked(split).ok_or(ServerError::Call(Malformed::NotUtf8))?;
                    let sym = self.sym(body)?;
                    let suffix = if suffix.is_empty() { None } else { Some(self.sym(suffix)?) };
                    Tok::Literal { kind, sym, suffix, span: self.span(side.span(i, i))? }
                }
            };
            out.push(tok);
        }
        Ok(out)
    }

    /// A handle for `toks`.
    fn hold(&mut self, toks: AVec<'a, Tok>) -> Result<u32, ServerError> {
        if self.tokens + toks.len() > self.limits.tokens {
            return Err(ServerError::Limit(ServerLimit::Tokens));
        }
        if self.live >= self.limits.handles {
            return Err(ServerError::Limit(ServerLimit::Handles));
        }
        self.tokens += toks.len();
        self.live += 1;
        self.streams.push(Some(toks));
        Ok(index(self.streams.len()))
    }

    fn take(&mut self, method: Method, handle: u32) -> Result<AVec<'a, Tok>, ServerError> {
        let slot = handle.checked_sub(1).and_then(|at| self.streams.get_mut(wide(at)));
        let toks = slot.and_then(Option::take).ok_or(ServerError::NoSuchStream { method, handle })?;
        self.live -= 1;
        self.tokens -= toks.len();
        Ok(toks)
    }

    fn get(&self, method: Method, handle: u32) -> Result<&AVec<'a, Tok>, ServerError> {
        handle
            .checked_sub(1)
            .and_then(|at| self.streams.get(wide(at)))
            .and_then(Option::as_ref)
            .ok_or(ServerError::NoSuchStream { method, handle })
    }

    /// Answers `request` into `reply` (cleared first), or gives the [`Ask`] blaze answers.
    pub fn dispatch<'r>(&mut self, request: &'r [u8], reply: &mut AVec<'_, u8>) -> Result<Dispatched<'r>, ServerError> {
        self.requests += 1;
        reply.clear();
        let mut r = Reader::new(request);
        let tag = r.u8().map_err(|what| ServerError::Malformed { method: None, what })?;
        let method = Method::of(tag).ok_or(ServerError::NoSuchMethod(tag))?;
        let bad = |what| ServerError::Malformed { method: Some(method), what };
        // `Result::<_, PanicMessage>::Ok`: no request of this server fails by panicking.
        reply.push(0);
        let mut w = Writer { out: reply };
        let mut ask = None;
        match method {
            Method::TrackEnvVar => {
                let name = r.str().map_err(bad)?;
                let value = if r.some().map_err(bad)? { Some(r.str().map_err(bad)?) } else { None };
                ask = Some(Ask::TrackEnv { name, value });
            }
            Method::TrackPath => ask = Some(Ask::TrackPath { path: r.str().map_err(bad)? }),
            Method::LiteralFromStr => {
                let text = r.str().map_err(bad)?;
                if !self.literal_from_str(text, &mut w)? {
                    w.ok(false);
                    w.str_fmt(format_args!("{text:?} is not a literal"));
                }
            }
            Method::EmitDiagnostic => self.emit(&mut r, method)?,
            Method::TsDrop => {
                let handle = r.handle().map_err(bad)?;
                self.take(method, handle)?;
            }
            Method::TsClone => {
                let handle = r.handle().map_err(bad)?;
                let copy = Vec::from_slice_in(&self.get(method, handle)?[..], self.alloc);
                let held = self.hold(copy)?;
                w.handle(held);
            }
            Method::TsIsEmpty => {
                let handle = r.handle().map_err(bad)?;
                let empty = self.get(method, handle)?.is_empty();
                w.bool(empty);
            }
            Method::TsExpandExpr => {
                // Eager expansion of an expression is not served: `Err(())`, which the client
                // reports to the macro as rustc's own refusal of what it cannot expand.
                let handle = r.handle().map_err(bad)?;
                self.get(method, handle)?;
                w.ok(false);
            }
            Method::TsFromStr => {
                let text = r.str().map_err(bad)?;
                match self.from_str(text)? {
                    Some(toks) => {
                        let held = self.hold(toks)?;
                        w.ok(true);
                        w.handle(held);
                    }
                    None => {
                        w.ok(false);
                        w.str("cannot parse string into token stream");
                    }
                }
            }
            Method::TsToString => {
                let handle = r.handle().map_err(bad)?;
                let toks = self.get(method, handle)?;
                self.print_str(toks, &mut w);
            }
            Method::TsFromTokenTree => {
                let mut toks = Vec::new_in(self.alloc);
                self.read_tree(&mut r, method, &mut toks)?;
                let held = self.hold(toks)?;
                w.handle(held);
            }
            Method::TsConcatTrees => {
                let mut toks = self.read_base(&mut r, method)?;
                let count = r.count().map_err(bad)?;
                for _ in 0..count {
                    self.read_tree(&mut r, method, &mut toks)?;
                }
                let held = self.hold(toks)?;
                w.handle(held);
            }
            Method::TsConcatStreams => {
                let mut toks = self.read_base(&mut r, method)?;
                let count = r.count().map_err(bad)?;
                for _ in 0..count {
                    let handle = r.handle().map_err(bad)?;
                    let taken = self.take(method, handle)?;
                    toks.extend_from_slice(&taken[..]);
                }
                let held = self.hold(toks)?;
                w.handle(held);
            }
            Method::TsIntoTrees => {
                let handle = r.handle().map_err(bad)?;
                let toks = self.take(method, handle)?;
                self.write_trees(&toks[..], w.out)?;
            }
            Method::SpanDebug => {
                let span = self.span_data(method, r.handle().map_err(bad)?)?;
                let origin = origin_byte(span.hygiene);
                match span.place {
                    Place::Input { lo, hi } => w.str_fmt(format_args!("#{origin} input {lo}..={hi}")),
                    Place::Attr { lo, hi } => w.str_fmt(format_args!("#{origin} attribute {lo}..={hi}")),
                    Place::Site => w.str_fmt(format_args!("#{origin} site")),
                }
            }
            Method::SpanParent => {
                self.span_data(method, r.handle().map_err(bad)?)?;
                w.some(false);
            }
            Method::SpanSource | Method::SpanStart | Method::SpanEnd => {
                let handle = r.handle().map_err(bad)?;
                self.span_data(method, handle)?;
                w.handle(handle);
            }
            Method::SpanFile => {
                let span = self.span_data(method, r.handle().map_err(bad)?)?;
                ask = Some(Ask::SpanFile { place: span.place });
            }
            Method::SpanByteRange | Method::SpanLine | Method::SpanColumn | Method::SpanLocalFile => {
                self.span_data(method, r.handle().map_err(bad)?)?;
                return Err(ServerError::Position { method });
            }
            Method::SpanJoin => {
                let a = r.handle().map_err(bad)?;
                let b = r.handle().map_err(bad)?;
                let (da, db) = (self.span_data(method, a)?, self.span_data(method, b)?);
                let place = match (da.place, db.place) {
                    (Place::Input { lo, hi }, Place::Input { lo: lo2, hi: hi2 }) => Some(Place::Input { lo: lo.min(lo2), hi: hi.max(hi2) }),
                    (Place::Attr { lo, hi }, Place::Attr { lo: lo2, hi: hi2 }) => Some(Place::Attr { lo: lo.min(lo2), hi: hi.max(hi2) }),
                    (Place::Site, Place::Site) => Some(Place::Site),
                    _ => None,
                };
                match place {
                    Some(place) => {
                        let joined = self.span(SpanData { place, hygiene: da.hygiene })?;
                        w.some(true);
                        w.handle(joined);
                    }
                    None => w.some(false),
                }
            }
            Method::SpanSubspan => {
                self.span_data(method, r.handle().map_err(bad)?)?;
                r.bound().map_err(bad)?;
                r.bound().map_err(bad)?;
                w.some(false);
            }
            Method::SpanResolvedAt => {
                let a = self.span_data(method, r.handle().map_err(bad)?)?;
                let at = self.span_data(method, r.handle().map_err(bad)?)?;
                let span = self.span(SpanData { place: a.place, hygiene: at.hygiene })?;
                w.handle(span);
            }
            Method::SpanSourceText => {
                let span = self.span_data(method, r.handle().map_err(bad)?)?;
                let range = match span.place {
                    Place::Input { lo, hi } => self.input.get(wide(lo)..=wide(hi)),
                    Place::Attr { lo, hi } => self.attr.get(wide(lo)..=wide(hi)),
                    Place::Site => None,
                };
                match range {
                    Some(toks) => {
                        w.some(true);
                        self.print_str(toks, &mut w);
                    }
                    None => w.some(false),
                }
            }
            Method::SpanSaveSpan => {
                let handle = r.handle().map_err(bad)?;
                self.span_data(method, handle)?;
                w.usize(u64::from(handle));
            }
            Method::SpanRecoverProcMacroSpan => {
                let id = r.usize().map_err(bad)?;
                let handle = u32::try_from(id).map_err(|_| ServerError::NoSuchSpan { method, handle: 0 })?;
                self.span_data(method, handle)?;
                w.handle(handle);
            }
            Method::SymbolNormalizeAndValidateIdent => {
                let text = r.str().map_err(bad)?;
                if self.is_ident(text) {
                    w.ok(true);
                    w.str(text);
                } else {
                    w.ok(false);
                }
            }
        }
        r.end().map_err(bad)?;
        Ok(ask.map_or(Dispatched::Replied, Dispatched::Ask))
    }

    /// The reply to an answered `track_*` ask: `Ok(())`.
    pub fn done_reply(&self, reply: &mut AVec<'_, u8>) {
        reply.clear();
        reply.push(0);
    }

    /// The reply to an answered `span_file` ask: `Ok(file)`.
    pub fn file_reply(&self, file: &str, reply: &mut AVec<'_, u8>) {
        reply.clear();
        reply.push(0);
        Writer { out: reply }.str(file);
    }

    /// `Option<TokenStream>`: the base's tokens, taken, or none.
    fn read_base(&mut self, r: &mut Reader<'_>, method: Method) -> Result<AVec<'a, Tok>, ServerError> {
        let bad = |what| ServerError::Malformed { method: Some(method), what };
        if r.some().map_err(bad)? {
            let handle = r.handle().map_err(bad)?;
            self.take(method, handle)
        } else {
            Ok(Vec::new_in(self.alloc))
        }
    }

    /// One `TokenTree`, its group's stream taken, appended to `out` flat.
    fn read_tree(&mut self, r: &mut Reader<'_>, method: Method, out: &mut AVec<'a, Tok>) -> Result<(), ServerError> {
        let bad = |what| ServerError::Malformed { method: Some(method), what };
        match r.tag(Field::TokenTree).map_err(bad)? {
            0 => {
                let delim = delim_of(r.tag(Field::Delimiter).map_err(bad)?).map_err(bad)?;
                let inner = self.read_base(r, method)?;
                let open = r.handle().map_err(bad)?;
                let close = r.handle().map_err(bad)?;
                let entire = r.handle().map_err(bad)?;
                for span in [open, close, entire] {
                    self.span_data(method, span)?;
                }
                out.push(Tok::Open { delim, open, close, entire });
                out.extend_from_slice(&inner[..]);
                out.push(Tok::Close { delim });
            }
            1 => {
                let ch = r.u8().map_err(bad)?;
                let joint = r.bool().map_err(bad)?;
                let span = r.handle().map_err(bad)?;
                self.span_data(method, span)?;
                if !is_punct(ch) {
                    return Err(ServerError::BadPunct { method, ch });
                }
                out.push(Tok::Punct { ch, joint, span });
            }
            2 => {
                let text = r.str().map_err(bad)?;
                let sym = self.sym(text)?;
                let raw = match r.tag(Field::IdentKind).map_err(bad)? {
                    // `Normal` and `ForcedKeyword` (`k#`) print without `r#`.
                    0 | 2 => false,
                    1 => true,
                    other => return Err(bad(Malformed::Tag(Field::IdentKind, other))),
                };
                let span = r.handle().map_err(bad)?;
                self.span_data(method, span)?;
                out.push(Tok::Ident { sym, raw, span });
            }
            3 => {
                let kind = lit_read(r).map_err(bad)?;
                let text = r.str().map_err(bad)?;
                let sym = self.sym(text)?;
                let suffix = if r.some().map_err(bad)? {
                    let text = r.str().map_err(bad)?;
                    Some(self.sym(text)?)
                } else {
                    None
                };
                let span = r.handle().map_err(bad)?;
                self.span_data(method, span)?;
                out.push(Tok::Literal { kind, sym, suffix, span });
            }
            other => return Err(bad(Malformed::Tag(Field::TokenTree, other))),
        }
        Ok(())
    }

    /// `Vec<TokenTree>` of `toks`' top level, each group's inside under a new handle.
    fn write_trees(&mut self, toks: &[Tok], out: &mut AVec<'_, u8>) -> Result<(), ServerError> {
        let count_at = out.len();
        Writer { out: &mut *out }.usize(0);
        let mut count = 0u64;
        let mut i = 0;
        while i < toks.len() {
            count += 1;
            match toks[i] {
                Tok::Open { delim, open, close, entire } => {
                    let end = close_index(toks, i);
                    let inner = toks.get(i + 1..end).unwrap_or(&[]);
                    let held = if inner.is_empty() { None } else { Some(self.hold(Vec::from_slice_in(inner, self.alloc))?) };
                    let mut w = Writer { out: &mut *out };
                    w.u8(0);
                    w.u8(delim_tag(delim));
                    w.some(held.is_some());
                    if let Some(held) = held {
                        w.handle(held);
                    }
                    w.handle(open);
                    w.handle(close);
                    w.handle(entire);
                    i = end + 1;
                    continue;
                }
                // A closing token is its group's end, skipped with it above.
                Tok::Close { .. } => count -= 1,
                Tok::Ident { sym, raw, span } => {
                    let mut w = Writer { out: &mut *out };
                    w.u8(2);
                    w.str(self.sym_text(sym));
                    w.u8(u8::from(raw));
                    w.handle(span);
                }
                Tok::Punct { ch, joint, span } => {
                    let mut w = Writer { out: &mut *out };
                    w.u8(1);
                    w.u8(ch);
                    w.bool(joint);
                    w.handle(span);
                }
                Tok::Literal { kind, sym, suffix, span } => {
                    let mut w = Writer { out: &mut *out };
                    w.u8(3);
                    lit_write(&mut w, kind);
                    w.str(self.sym_text(sym));
                    w.some(suffix.is_some());
                    if let Some(suffix) = suffix {
                        w.str(self.sym_text(suffix));
                    }
                    w.handle(span);
                }
            }
            i += 1;
        }
        Writer { out }.set_usize(count_at, count);
        Ok(())
    }

    /// `text` lexed in the call's edition, each token spanned `call_site`; `None` when it is not
    /// tokens.
    fn from_str(&mut self, text: &str) -> Result<Option<AVec<'a, Tok>>, ServerError> {
        let Ok(stream) = lex(text, self.edition, self.alloc) else { return Ok(None) };
        let mut depth = 0usize;
        for token in stream.tokens() {
            match token.kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) if depth == 0 => return Ok(None),
                TokenKind::Close(_) => depth -= 1,
                TokenKind::Literal { kind: LitKind::Err, .. } => return Ok(None),
                TokenKind::Punct { char, .. } if !u8::try_from(char).is_ok_and(is_punct) => return Ok(None),
                _ => {}
            }
        }
        if depth != 0 {
            return Ok(None);
        }
        self.import_lexed(&stream).map(Some)
    }

    /// A lexed stream's tokens, every span `call_site`.
    fn import_lexed(&mut self, stream: &TokenStream<'_>) -> Result<AVec<'a, Tok>, ServerError> {
        let mut out = Vec::with_capacity_in(stream.tokens().len(), self.alloc);
        for token in stream.tokens() {
            let text = stream.text_of(token);
            let tok = match token.kind {
                TokenKind::Open(delim) => Tok::Open { delim, open: CALL_SITE, close: CALL_SITE, entire: CALL_SITE },
                TokenKind::Close(delim) => Tok::Close { delim },
                TokenKind::Ident { raw, .. } => Tok::Ident { sym: self.sym(text)?, raw: raw == Raw::Yes, span: CALL_SITE },
                TokenKind::Punct { char, spacing } => {
                    // Checked to be one byte of the punctuation set by `from_str`.
                    let ch = u8::try_from(char).unwrap_or(b'?');
                    Tok::Punct { ch, joint: spacing == Spacing::Joint, span: CALL_SITE }
                }
                TokenKind::Literal { kind, suffix_len, .. } => {
                    let split = text.len().saturating_sub(wide(suffix_len));
                    let (body, suffix) = text.split_at_checked(split).unwrap_or((text, ""));
                    let sym = self.sym(body)?;
                    let suffix = if suffix.is_empty() { None } else { Some(self.sym(suffix)?) };
                    Tok::Literal { kind, sym, suffix, span: CALL_SITE }
                }
            };
            out.push(tok);
        }
        Ok(out)
    }

    /// `text` as one literal, written to `w` as `Ok(Literal)` (a `-` before a number is part of
    /// it, as rustc reads it); `false` when it is not one.
    fn literal_from_str(&mut self, text: &str, w: &mut Writer<'_, '_>) -> Result<bool, ServerError> {
        let Ok(stream) = lex(text, self.edition, self.alloc) else { return Ok(false) };
        let tokens = stream.tokens();
        let (minus, token) = match tokens {
            [token] => ("", token),
            [minus, token] if matches!(minus.kind, TokenKind::Punct { char: '-', .. }) => ("-", token),
            _ => return Ok(false),
        };
        let TokenKind::Literal { kind, suffix_len, .. } = token.kind else { return Ok(false) };
        if kind == LitKind::Err || (!minus.is_empty() && !matches!(kind, LitKind::Integer | LitKind::Float)) {
            return Ok(false);
        }
        let all = stream.text_of(token);
        let Some((body, suffix)) = all.len().checked_sub(wide(suffix_len)).and_then(|at| all.split_at_checked(at)) else { return Ok(false) };
        w.ok(true);
        lit_write(w, kind);
        w.str_parts(&[minus, body]);
        w.some(!suffix.is_empty());
        if !suffix.is_empty() {
            w.str(suffix);
        }
        w.handle(CALL_SITE);
        Ok(true)
    }

    fn is_ident(&self, text: &str) -> bool {
        if text == "_" {
            return true;
        }
        let Ok(stream) = lex(text, self.edition, self.alloc) else { return false };
        matches!(stream.tokens(), [token] if matches!(token.kind, TokenKind::Ident { raw: Raw::No, .. }) && stream.text_of(token) == text)
    }

    /// `toks` printed as rustc's `TokenStream::to_string` spaces them, as a bridge string.
    fn print_str(&self, toks: &[Tok], w: &mut Writer<'_, '_>) {
        let at = w.out.len();
        w.usize(0);
        let start = w.out.len();
        self.print(toks, w.out);
        let len = w.out.len() - start;
        w.set_usize(at, len64(len));
    }

    /// A space between two tokens, but none after a joint `Punct` or an opening delimiter, nor
    /// before a closing one.
    fn print(&self, toks: &[Tok], out: &mut AVec<'_, u8>) {
        let mut glue = true;
        for tok in toks {
            let closing = matches!(tok, Tok::Close { .. });
            if !glue && !closing {
                out.push(b' ');
            }
            glue = false;
            match *tok {
                Tok::Open { delim, .. } => {
                    out.extend_from_slice(open_text(delim).as_bytes());
                    glue = true;
                }
                Tok::Close { delim } => out.extend_from_slice(close_text(delim).as_bytes()),
                Tok::Ident { sym, raw, .. } => {
                    if raw {
                        out.extend_from_slice(b"r#");
                    }
                    out.extend_from_slice(self.sym_text(sym).as_bytes());
                }
                Tok::Punct { ch, joint, .. } => {
                    out.push(ch);
                    glue = joint;
                }
                Tok::Literal { kind, sym, suffix, .. } => {
                    let suffix = suffix.map_or("", |it| self.sym_text(it));
                    write_literal(out, kind, self.sym_text(sym), suffix);
                }
            }
        }
    }

    /// `emit_diagnostic`: the diagnostic and its children, each kept with its first span.
    fn emit(&mut self, r: &mut Reader<'_>, method: Method) -> Result<(), ServerError> {
        let bad = |what| ServerError::Malformed { method: Some(method), what };
        // Diagnostics left to read at each depth: the walk of the tree, without recursion.
        let mut left: AVec<'a, usize> = Vec::new_in(self.alloc);
        left.push(1);
        while let Some(top) = left.last_mut() {
            if *top == 0 {
                left.pop();
                continue;
            }
            *top -= 1;
            let level = level_of(r.tag(Field::Level).map_err(bad)?).map_err(bad)?;
            let message = r.str().map_err(bad)?;
            let spans = r.count().map_err(bad)?;
            let mut first = None;
            for _ in 0..spans {
                let span = r.handle().map_err(bad)?;
                self.span_data(method, span)?;
                first.get_or_insert(span);
            }
            if self.diag_text.len() + message.len() > self.limits.text {
                return Err(ServerError::Limit(ServerLimit::Text));
            }
            if self.diags.len() >= self.limits.diagnostics {
                return Err(ServerError::Limit(ServerLimit::Diagnostics));
            }
            let at = index(self.diag_text.len());
            self.diag_text.extend_from_slice(message.as_bytes());
            self.diags.push(Diag { level, message: (at, index(message.len())), span: first });
            let children = r.count().map_err(bad)?;
            left.push(children);
        }
        Ok(())
    }

    /// The outcome of the client's run output `output` (`Result<TokenStream, PanicMessage>`),
    /// written to `out` (cleared first): the tokens with their origins and the diagnostics, or
    /// the stopped outcome naming what was wrong with `output`.
    pub fn outcome(&mut self, output: &[u8], out: &mut AVec<'_, u8>) {
        let toks = match read_output(output) {
            Err(what) => return write_stopped(out, ServerError::Output(what)),
            Ok(Returned::Panicked(message)) => return self.write_outcome(out, &[], Some(message.unwrap_or("a panic with no message"))),
            Ok(Returned::Tokens(None)) => Vec::new_in(self.alloc),
            Ok(Returned::Tokens(Some(handle))) => match self.take(Method::TsDrop, handle) {
                Ok(toks) => toks,
                Err(_) => return write_stopped(out, ServerError::NoSuchOutput { handle }),
            },
        };
        self.write_outcome(out, &toks[..], None);
    }

    /// The stopped outcome of `error`, written to `out`.
    pub fn stopped(&self, error: ServerError, out: &mut AVec<'_, u8>) {
        write_stopped(out, error);
    }

    fn write_outcome(&self, out: &mut AVec<'_, u8>, toks: &[Tok], panicked: Option<&str>) {
        let mut w = OutcomeWriter::start(out, panicked.is_some());
        // The closing span of each group open at this point.
        let mut closes: AVec<'a, SpanId> = Vec::new_in(self.alloc);
        for tok in toks {
            match *tok {
                Tok::Open { delim, open, close, .. } => {
                    w.token(TokenKind::Open(delim), self.origin(open), &[]);
                    closes.push(close);
                }
                Tok::Close { delim } => {
                    let close = closes.pop().unwrap_or(CALL_SITE);
                    w.token(TokenKind::Close(delim), self.origin(close), &[]);
                }
                Tok::Ident { sym, raw, span } => {
                    let raw = if raw { Raw::Yes } else { Raw::No };
                    w.token(TokenKind::Ident { raw, spacing: Spacing::Alone }, self.origin(span), &[self.sym_text(sym)]);
                }
                Tok::Punct { ch, joint, span } => {
                    let spacing = if joint { Spacing::Joint } else { Spacing::Alone };
                    w.token(TokenKind::Punct { char: char::from(ch), spacing }, self.origin(span), &[]);
                }
                Tok::Literal { kind, sym, suffix, span } => {
                    let mut body = self.sym_text(sym);
                    // A negative number is a `-` and the number, as blaze's tokens hold it.
                    if matches!(kind, LitKind::Integer | LitKind::Float)
                        && let Some(rest) = body.strip_prefix('-')
                    {
                        w.token(TokenKind::Punct { char: '-', spacing: Spacing::Alone }, self.origin(span), &[]);
                        body = rest;
                    }
                    let suffix = suffix.map_or("", |it| self.sym_text(it));
                    let kind = TokenKind::Literal { kind, suffix_len: index(suffix.len()), spacing: Spacing::Alone };
                    w.token(kind, self.origin(span), &[body, suffix]);
                }
            }
        }
        w.diagnostics();
        for diag in self.diags.iter() {
            let (at, len) = diag.message;
            let start = wide(at);
            let message = self.diag_text.get(start..start + wide(len)).and_then(|b| core::str::from_utf8(b).ok()).unwrap_or("");
            w.diagnostic(diag.level, diag.span.map(|span| self.origin(span)), message);
        }
        w.end(panicked);
    }
}

/// A span's origin as `span_debug` prints it.
fn origin_byte(origin: TokenOrigin) -> u8 {
    match origin {
        TokenOrigin::Input(_) => 0,
        TokenOrigin::AttrArgs(_) => 1,
        TokenOrigin::Def(_) | TokenOrigin::Params(_) => 2,
        TokenOrigin::Made(_) => 3,
    }
}

/// The bridge's `Level` tags: `Error`, `Warning`, `Note`, `Help`.
pub(crate) fn level_of(tag: u8) -> Result<ProcLevel, Malformed> {
    Ok(match tag {
        0 => ProcLevel::Error,
        1 => ProcLevel::Warning,
        2 => ProcLevel::Note,
        3 => ProcLevel::Help,
        other => return Err(Malformed::Tag(Field::Level, other)),
    })
}

/// The punctuation rustc's lexer makes one `Punct` of.
pub(crate) fn is_punct(ch: u8) -> bool {
    b"=<>!~+-*/%^&|@.,;:#$?'".contains(&ch)
}

fn open_text(delim: Delim) -> &'static str {
    match delim {
        Delim::Paren => "(",
        Delim::Brace => "{",
        Delim::Bracket => "[",
        Delim::Invisible => "",
    }
}

fn close_text(delim: Delim) -> &'static str {
    match delim {
        Delim::Paren => ")",
        Delim::Brace => "}",
        Delim::Bracket => "]",
        Delim::Invisible => "",
    }
}

/// A literal's source form: its prefix and quotes around `inside`, then `suffix`. The one
/// printer of a literal here: `to_string` uses it and the host's check (outcome.rs) lexes what it
/// prints.
pub(crate) fn write_literal(out: &mut AVec<'_, u8>, kind: LitKind, inside: &str, suffix: &str) {
    match kind {
        LitKind::Byte => quoted(out, b"b", b'\'', 0, inside),
        LitKind::Char => quoted(out, b"", b'\'', 0, inside),
        LitKind::Integer | LitKind::Float | LitKind::Err => out.extend_from_slice(inside.as_bytes()),
        LitKind::Str => quoted(out, b"", b'"', 0, inside),
        LitKind::StrRaw(n) => quoted(out, b"r", b'"', n, inside),
        LitKind::ByteStr => quoted(out, b"b", b'"', 0, inside),
        LitKind::ByteStrRaw(n) => quoted(out, b"br", b'"', n, inside),
        LitKind::CStr => quoted(out, b"c", b'"', 0, inside),
        LitKind::CStrRaw(n) => quoted(out, b"cr", b'"', n, inside),
    }
    out.extend_from_slice(suffix.as_bytes());
}

/// `prefix`, `hashes` `#`s, `inside` between two `quote`s, the `#`s again.
fn quoted(out: &mut AVec<'_, u8>, prefix: &[u8], quote: u8, hashes: u8, inside: &str) {
    out.extend_from_slice(prefix);
    for _ in 0..hashes {
        out.push(b'#');
    }
    out.push(quote);
    out.extend_from_slice(inside.as_bytes());
    out.push(quote);
    for _ in 0..hashes {
        out.push(b'#');
    }
}

/// The index of the `Close` matching the `Open` at `open`.
fn close_index(toks: &[Tok], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, tok) in toks.iter().enumerate().skip(open) {
        match tok {
            Tok::Open { .. } => depth += 1,
            Tok::Close { .. } => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    toks.len().saturating_sub(1)
}
