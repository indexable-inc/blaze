//! blaze's server of the pinned `proc_macro` bridge: the state one expansion's requests are
//! answered from, a sans-IO state machine. The driver ([`crate::host`]) hands it each request's
//! bytes and the module's output; it never touches the module, the clock or the disk.
//!
//! A token stream is flat, as blaze-expand's is: a group is its opening token, its tokens and
//! its closing token, so no walk here recurses. Each stream the client holds is a handle into
//! [`Server::streams`]; an owned handle the client passes back is taken out, as rustc's server
//! does.
//!
//! A span is a handle into [`Server::spans`]: where it points (a token of the call's input or
//! of its attribute's arguments, or none) and whose hygiene it has. Nothing in a span is a
//! position in a file: `line`, `column`, `local_file` and `byte_range` are refused
//! ([`ServerError::Position`], rule 71), `source_text` prints the tokens it covers, and `file`
//! is answered for a token written in the file holding the call ([`ProcCall::file`]) and is a
//! read of the run ([`Server::asked_file`]). The output's spans become
//! [`TokenOrigin`]s: the input's token for a span of the input, [`TokenOrigin::Made`] of the
//! site's transparency for `call_site` (transparent), `mixed_site` (semi-opaque) and `def_site`
//! (opaque). `resolved_at` keeps the location of one span and the hygiene of the other, and the
//! hygiene is what the origin follows.

use std::alloc::Global;

use blaze_expand::{
    Ctx, Delim, Edition, HostError, LitKind, ProcCall, ProcDiagnostic, ProcKind, ProcLevel, ProcLimit, Produced, Raw, Reads,
    Spacing, Text, TokenKind, TokenOrigin, TokenStream, Transparency, lex,
};
use blaze_own::{AVec, Alloc};

use crate::{
    protocol::Method,
    wire::{Malformed, Reader, Writer},
};

/// The most tokens the streams of one expansion hold together, the most bytes of symbol text,
/// spans and live handles: an expansion past one is stopped by [`ProcLimit::Output`].
#[derive(Clone, Copy, Debug)]
pub struct ServerLimits {
    pub tokens: usize,
    pub text: usize,
    pub spans: usize,
    pub handles: usize,
}

impl ServerLimits {
    /// Far past what serde's, clap's or `sol!`'s largest expansions in tempo make (each under a
    /// million tokens), and small enough that a runaway module stops before it fills memory.
    pub const STANDARD: ServerLimits =
        ServerLimits { tokens: 1 << 24, text: 1 << 28, spans: 1 << 24, handles: 1 << 22 };
}

/// Why the server stopped an expansion: the module sent what no client of the pinned bridge
/// sends, or went past a limit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServerError {
    /// A request the bridge never sends, in `method`.
    Malformed { method: Option<Method>, what: Malformed },
    /// A request tag past the bridge's methods.
    NoSuchMethod(u8),
    /// A handle the server never gave, or gave and took back.
    NoSuchStream { method: Method, handle: u32 },
    NoSuchSpan { method: Method, handle: u32 },
    /// A `Punct` whose character is not one rustc's lexer makes.
    BadPunct(u8),
    /// A limit of [`ServerLimits`].
    Limit(ProcLimit),
    /// Bytes after a request's last argument.
    Trailing { method: Method },
    /// A question about where a span sits in its file (`line`, `column`, `local_file`,
    /// `byte_range`), which is not served.
    Position { method: Method },
    /// `file` of a call whose file the host does not answer (a toolchain crate's, a host told
    /// no file): `why`, as the host gave it. No path is made up (rule 3).
    FileUnknown { why: HostError },
    /// `file` of a site span (`call_site`, `mixed_site`, `def_site`): the file holding the
    /// call's own text or the macro's definition is not told to the server, only the file
    /// holding the call's item.
    FileOfSite,
    /// `file` of a token an expansion made (no root context): its file is where the macro
    /// that wrote it is defined, which the server is not told.
    FileOfMadeToken,
    /// Attribute arguments given to a call of `kind`, which takes none.
    ArgumentsOnNonAttribute { kind: ProcKind },
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::Malformed { method: Some(method), what } => {
                write!(f, "sent `{}` {what}", method.name())
            }
            ServerError::Malformed { method: None, what } => write!(f, "gave as its output {what}"),
            ServerError::NoSuchMethod(tag) => write!(f, "sent the request tag {tag}, which no method of the pinned bridge has"),
            ServerError::NoSuchStream { method, handle } => {
                write!(f, "passed `{}` the token stream {handle}, which it does not hold", method.name())
            }
            ServerError::NoSuchSpan { method, handle } => {
                write!(f, "passed `{}` the span {handle}, which the server never gave", method.name())
            }
            ServerError::BadPunct(ch) => write!(f, "made a `Punct` of the byte {ch}, which is no punctuation"),
            ServerError::Limit(limit) => write!(f, "went past its {limit:?} limit"),
            ServerError::Trailing { method } => write!(f, "sent `{}` bytes after its last argument", method.name()),
            ServerError::Position { method } => write!(
                f,
                "asked `{}`, where a span sits in its file, which blaze does not answer: an expansion is keyed by its item, not its place (rule 71)",
                method.name()
            ),
            ServerError::FileUnknown { why } => {
                write!(f, "asked `span_file` of a call whose file the host does not answer ({why}): no path is made up (rule 3)")
            }
            ServerError::FileOfSite => write!(
                f,
                "asked `span_file` of a site span (`call_site`, `mixed_site`, `def_site`), which blaze does not answer: only the file of a token written in the call's item is told to the macro"
            ),
            ServerError::FileOfMadeToken => write!(
                f,
                "asked `span_file` of a token a macro wrote, which blaze does not answer: its file is where that macro is defined, which is not told to the macro"
            ),
            ServerError::ArgumentsOnNonAttribute { kind } => write!(f, "was given attribute arguments on a {kind:?} call, which takes none"),
        }
    }
}

/// A symbol: a range of [`Server::text`].
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

/// Where a span points.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// Tokens `lo..=hi` of the call's input.
    Input { lo: u32, hi: u32 },
    /// Of the attribute's arguments.
    Attr { lo: u32, hi: u32 },
    /// Nowhere in the call: a site.
    Site,
}

/// A span: where it points and the origin a token of it gets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SpanData {
    place: Place,
    hygiene: TokenOrigin,
}

/// The fixed handles of [`ExpnGlobals`](crate::protocol): `def_site`, `call_site`,
/// `mixed_site`.
const DEF_SITE: SpanId = 1;
const CALL_SITE: SpanId = 2;
const MIXED_SITE: SpanId = 3;

/// A diagnostic the macro emitted: its level, its message's range in `diag_text`, its first
/// span.
#[derive(Clone, Copy, Debug)]
struct Diag {
    level: ProcLevel,
    message: (u32, u32),
    span: Option<SpanId>,
}

/// What the module's run gave.
#[derive(Debug)]
pub enum Outcome {
    /// The stream of this handle; `None` for the empty stream (`proc_macro::TokenStream`
    /// holds an `Option` of the bridge's, and its encoding is that `Option`'s).
    Tokens(Option<u32>),
    /// A panic, with its message.
    Panicked(String),
}

/// What a `track_*` call told the server: the driver records each as a read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Tracked {
    Env(String),
    Path(String),
}

/// One expansion's server.
pub struct Server {
    edition: Edition,
    limits: ServerLimits,
    /// Streams by handle - 1; `None` once taken back.
    streams: Vec<Option<Vec<Tok, Global>>, Global>,
    live: usize,
    tokens: usize,
    /// Spans by handle - 1.
    spans: Vec<SpanData, Global>,
    text: Vec<u8, Global>,
    /// The call's input and attribute arguments, as the first streams' tokens: what
    /// `source_text` prints.
    input: Vec<Tok, Global>,
    attr: Vec<Tok, Global>,
    diags: Vec<Diag, Global>,
    diag_text: Vec<u8, Global>,
    pub(crate) tracked: Vec<Tracked, Global>,
    /// The call's file as the host shows it ([`ProcCall::file`]), a range of `text`, or why
    /// the host does not answer it.
    file: Result<Sym, HostError>,
    /// Whether each token of the input, and of the attribute's arguments, was written in the
    /// file holding the call: a root context, no expansion made it. Only such a token's
    /// `file` is the call's.
    written_input: AVec<'static, bool>,
    written_attr: AVec<'static, bool>,
    /// Whether `span_file` was answered: the driver then records the call's file as a read of
    /// the run (host.rs, sandbox.rs `CALL_FILE`), so a kept answer is taken only for a call
    /// whose file is shown the same.
    pub(crate) asked_file: bool,
    /// Requests answered: the expansion's span says how many.
    pub requests: u64,
    /// The allocator `ts_from_str` lexes in: the runner's scratch.
    scratch: Alloc<'static>,
}

fn origin_byte(origin: TokenOrigin) -> u8 {
    match origin {
        TokenOrigin::Input(_) => 0,
        TokenOrigin::AttrArgs(_) => 1,
        TokenOrigin::Def(_) | TokenOrigin::Params(_) => 2,
        TokenOrigin::Made(_) => 3,
    }
}

fn delim_tag(delim: Delim) -> u8 {
    match delim {
        Delim::Paren => 0,
        Delim::Brace => 1,
        Delim::Bracket => 2,
        Delim::Invisible => 3,
    }
}

fn delim_of(tag: u8) -> Result<Delim, Malformed> {
    Ok(match tag {
        0 => Delim::Paren,
        1 => Delim::Brace,
        2 => Delim::Bracket,
        3 => Delim::Invisible,
        other => return Err(Malformed::Tag("the Delimiter tag", other)),
    })
}

fn lit_write(w: &mut Writer<'_>, kind: LitKind) {
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

fn lit_read(r: &mut Reader<'_>) -> Result<LitKind, Malformed> {
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
        other => return Err(Malformed::Tag("the LitKind tag", other)),
    })
}

fn level_of(tag: u8) -> Result<ProcLevel, Malformed> {
    Ok(match tag {
        0 => ProcLevel::Error,
        1 => ProcLevel::Warning,
        2 => ProcLevel::Note,
        3 => ProcLevel::Help,
        other => return Err(Malformed::Tag("the Level tag", other)),
    })
}

/// The punctuation rustc's lexer makes one `Punct` of.
fn is_punct(ch: u8) -> bool {
    b"=<>!~+-*/%^&|@.,;:#$?'".contains(&ch)
}

/// Appends `text`'s source form of a literal: its quotes and prefix around the symbol, then the
/// suffix.
fn write_literal(out: &mut String, kind: LitKind, sym: &str, suffix: &str) {
    let hashes = |n: u8| "#".repeat(n as usize);
    match kind {
        LitKind::Byte => out.push_str(&format!("b'{sym}'")),
        LitKind::Char => out.push_str(&format!("'{sym}'")),
        LitKind::Integer | LitKind::Float | LitKind::Err => out.push_str(sym),
        LitKind::Str => out.push_str(&format!("\"{sym}\"")),
        LitKind::StrRaw(n) => out.push_str(&format!("r{0}\"{sym}\"{0}", hashes(n))),
        LitKind::ByteStr => out.push_str(&format!("b\"{sym}\"")),
        LitKind::ByteStrRaw(n) => out.push_str(&format!("br{0}\"{sym}\"{0}", hashes(n))),
        LitKind::CStr => out.push_str(&format!("c\"{sym}\"")),
        LitKind::CStrRaw(n) => out.push_str(&format!("cr{0}\"{sym}\"{0}", hashes(n))),
    }
    out.push_str(suffix);
}

impl Server {
    /// The server of one call of `edition`, and the bytes the client is run on
    /// (`ExpnGlobals` and the input's handles) written to `run_input`. The streams served
    /// follow the call's kind: an attribute macro always gets two, `(arguments, item)`, as
    /// rustc passes them, so `#[m]` gets empty arguments, never none (its client reads a
    /// handle for each); a derive or a fn-like macro gets its one input.
    pub fn new(edition: Edition, limits: ServerLimits, call: &ProcCall<'_>, scratch: Alloc<'static>, run_input: &mut Vec<u8>) -> Result<Server, ServerError> {
        let empty = TokenStream::new_in(scratch);
        let input = call.input;
        let attr = match call.kind {
            ProcKind::Attr => Some(call.attr_args.unwrap_or(&empty)),
            ProcKind::FnLike | ProcKind::Derive => match call.attr_args {
                None => None,
                Some(_) => return Err(ServerError::ArgumentsOnNonAttribute { kind: call.kind }),
            },
        };
        // The call's file is the text's first symbol: a path past the text limit stops the
        // expansion as any symbol past it does.
        let mut text = Vec::new_in(Global);
        let file = match call.file {
            Ok(file) if file.len() > limits.text => return Err(ServerError::Limit(ProcLimit::Output)),
            Ok(file) => {
                text.extend_from_slice(file.as_bytes());
                Ok(Sym { at: 0, len: file.len() as u32 })
            }
            Err(why) => Err(why),
        };
        let written = |stream: &TokenStream<'_>| {
            let mut out = AVec::with_capacity_in(stream.tokens().len(), scratch);
            for token in stream.tokens() {
                out.push(token.ctx.root_edition().is_some());
            }
            out
        };
        let (written_input, written_attr) = (written(input), attr.map_or_else(|| AVec::new_in(scratch), written));
        let mut server = Server {
            edition,
            limits,
            streams: Vec::new_in(Global),
            live: 0,
            tokens: 0,
            spans: Vec::new_in(Global),
            text,
            input: Vec::new_in(Global),
            attr: Vec::new_in(Global),
            diags: Vec::new_in(Global),
            diag_text: Vec::new_in(Global),
            tracked: Vec::new_in(Global),
            file,
            written_input,
            written_attr,
            asked_file: false,
            requests: 0,
            scratch,
        };
        let site = |t| SpanData { place: Place::Site, hygiene: TokenOrigin::Made(t) };
        server.spans.extend([site(Transparency::Opaque), site(Transparency::Transparent), site(Transparency::SemiOpaque)]);
        let item = server.import(input, |i| SpanData { place: Place::Input { lo: i, hi: i }, hygiene: TokenOrigin::Input(i) })?;
        server.input = item.clone();
        let attr_handle = match attr {
            Some(attr) => {
                let args = server.import(attr, |i| SpanData { place: Place::Attr { lo: i, hi: i }, hygiene: TokenOrigin::AttrArgs(i) })?;
                server.attr = args.clone();
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
        // `expand2`'s client reads `(attribute, item)`, as rustc passes them.
        if let Some(attr) = attr_handle {
            w.handle(attr);
        }
        w.handle(item);
        Ok(server)
    }

    fn span(&mut self, data: SpanData) -> Result<SpanId, ServerError> {
        if self.spans.len() >= self.limits.spans {
            return Err(ServerError::Limit(ProcLimit::Output));
        }
        self.spans.push(data);
        Ok(self.spans.len() as SpanId)
    }

    fn span_data(&self, method: Method, handle: SpanId) -> Result<SpanData, ServerError> {
        handle
            .checked_sub(1)
            .and_then(|at| self.spans.get(at as usize).copied())
            .ok_or(ServerError::NoSuchSpan { method, handle })
    }

    /// What `span_file` answers for `span`: the file holding the call, for a span whose first
    /// token was written in it (rustc's `Span::file` of a joined or resolved span is its first
    /// token's place's file). A site's file, a made token's, or a call whose file the host
    /// does not answer is refused by name.
    fn file_of(&self, span: SpanData) -> Result<Sym, ServerError> {
        let written = match span.place {
            Place::Input { lo, .. } => self.written_input.get(lo as usize) == Some(&true),
            Place::Attr { lo, .. } => self.written_attr.get(lo as usize) == Some(&true),
            Place::Site => return Err(ServerError::FileOfSite),
        };
        if !written {
            return Err(ServerError::FileOfMadeToken);
        }
        self.file.map_err(|why| ServerError::FileUnknown { why })
    }

    fn sym(&mut self, text: &str) -> Result<Sym, ServerError> {
        if self.text.len() + text.len() > self.limits.text {
            return Err(ServerError::Limit(ProcLimit::Output));
        }
        let at = self.text.len() as u32;
        self.text.extend_from_slice(text.as_bytes());
        Ok(Sym { at, len: text.len() as u32 })
    }

    fn sym_text(&self, sym: Sym) -> &str {
        // Each symbol is a whole `&str` the server pushed.
        std::str::from_utf8(&self.text[sym.at as usize..(sym.at + sym.len) as usize]).unwrap_or("")
    }

    /// `stream`'s tokens as server tokens, the span of the token at `i` made by `span_of(i)`
    /// (whole groups get a span from their opening to their closing token).
    fn import(&mut self, stream: &TokenStream<'_>, span_of: impl Fn(u32) -> SpanData) -> Result<Vec<Tok, Global>, ServerError> {
        let tokens = stream.tokens();
        let mut out = Vec::with_capacity_in(tokens.len(), Global);
        for (i, token) in tokens.iter().enumerate() {
            let i = i as u32;
            let text = stream.text_of(token);
            let tok = match token.kind {
                TokenKind::Open(delim) => {
                    let close = stream.close_of(i as usize).map_or(i, |it| it as u32);
                    let open = self.span(span_of(i))?;
                    let close_span = self.span(span_of(close))?;
                    let mut entire = span_of(i);
                    entire.place = match entire.place {
                        Place::Input { lo, .. } => Place::Input { lo, hi: close },
                        Place::Attr { lo, .. } => Place::Attr { lo, hi: close },
                        Place::Site => Place::Site,
                    };
                    let entire = self.span(entire)?;
                    Tok::Open { delim, open, close: close_span, entire }
                }
                TokenKind::Close(delim) => Tok::Close { delim },
                TokenKind::Ident { raw, .. } => {
                    Tok::Ident { sym: self.sym(text)?, raw: raw == Raw::Yes, span: self.span(span_of(i))? }
                }
                TokenKind::Punct { char, spacing } => {
                    let ch = u8::try_from(char).map_err(|_| ServerError::BadPunct(0))?;
                    Tok::Punct { ch, joint: spacing == Spacing::Joint, span: self.span(span_of(i))? }
                }
                TokenKind::Literal { kind, suffix_len, .. } => {
                    let split = text.len().saturating_sub(suffix_len as usize);
                    let (body, suffix) = text.split_at(split);
                    let sym = self.sym(body)?;
                    let suffix = if suffix.is_empty() { None } else { Some(self.sym(suffix)?) };
                    Tok::Literal { kind, sym, suffix, span: self.span(span_of(i))? }
                }
            };
            out.push(tok);
        }
        Ok(out)
    }

    /// A handle for `toks`.
    fn hold(&mut self, toks: Vec<Tok, Global>) -> Result<u32, ServerError> {
        self.tokens += toks.len();
        if self.tokens > self.limits.tokens || self.live >= self.limits.handles {
            return Err(ServerError::Limit(ProcLimit::Output));
        }
        self.live += 1;
        self.streams.push(Some(toks));
        Ok(self.streams.len() as u32)
    }

    fn take(&mut self, method: Method, handle: u32) -> Result<Vec<Tok, Global>, ServerError> {
        let slot = handle.checked_sub(1).and_then(|at| self.streams.get_mut(at as usize));
        let toks = slot.and_then(Option::take).ok_or(ServerError::NoSuchStream { method, handle })?;
        self.live -= 1;
        self.tokens -= toks.len();
        Ok(toks)
    }

    fn get(&self, method: Method, handle: u32) -> Result<&Vec<Tok, Global>, ServerError> {
        handle
            .checked_sub(1)
            .and_then(|at| self.streams.get(at as usize))
            .and_then(Option::as_ref)
            .ok_or(ServerError::NoSuchStream { method, handle })
    }

    /// Answers `request` into `reply` (cleared first).
    pub fn dispatch(&mut self, request: &[u8], reply: &mut Vec<u8>) -> Result<(), ServerError> {
        self.requests += 1;
        reply.clear();
        let mut r = Reader::new(request);
        let tag = r.u8().map_err(|what| ServerError::Malformed { method: None, what })?;
        let method = Method::of(tag).ok_or(ServerError::NoSuchMethod(tag))?;
        let bad = |what| ServerError::Malformed { method: Some(method), what };
        // `Result::<_, PanicMessage>::Ok`: no request of this server fails by panicking.
        reply.push(0);
        let mut w = Writer { out: reply };
        match method {
            Method::TrackEnvVar => {
                let var = r.str().map_err(bad)?.to_owned();
                if r.some().map_err(bad)? {
                    r.str().map_err(bad)?;
                }
                self.tracked.push(Tracked::Env(var));
            }
            Method::TrackPath => {
                let path = r.str().map_err(bad)?.to_owned();
                self.tracked.push(Tracked::Path(path));
            }
            Method::LiteralFromStr => {
                let text = r.str().map_err(bad)?;
                match self.literal_from_str(text)? {
                    Some((kind, sym, suffix)) => {
                        w.ok(true);
                        lit_write(&mut w, kind);
                        w.str(&sym);
                        w.some(suffix.is_some());
                        if let Some(suffix) = &suffix {
                            w.str(suffix);
                        }
                        w.handle(CALL_SITE);
                    }
                    None => {
                        w.ok(false);
                        w.str(&format!("{text:?} is not a literal"));
                    }
                }
            }
            Method::EmitDiagnostic => self.emit(&mut r, method)?,
            Method::TsDrop => {
                let handle = r.handle().map_err(bad)?;
                self.take(method, handle)?;
            }
            Method::TsClone => {
                let handle = r.handle().map_err(bad)?;
                let toks = self.get(method, handle)?.clone();
                let held = self.hold(toks)?;
                w.handle(held);
            }
            Method::TsIsEmpty => {
                let handle = r.handle().map_err(bad)?;
                let empty = self.get(method, handle)?.is_empty();
                w.bool(empty);
            }
            Method::TsExpandExpr => {
                // Eager expansion of an expression is not served: `Err(())`, which the client
                // reports to the macro as rustc's own refusal of a non-literal.
                r.handle().map_err(bad)?;
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
                let text = self.print(self.get(method, handle)?);
                w.str(&text);
            }
            Method::TsFromTokenTree => {
                let mut toks = Vec::new_in(Global);
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
                    toks.extend(self.take(method, handle)?);
                }
                let held = self.hold(toks)?;
                w.handle(held);
            }
            Method::TsIntoTrees => {
                let handle = r.handle().map_err(bad)?;
                let toks = self.take(method, handle)?;
                self.write_trees(&toks, w.out)?;
            }
            Method::SpanDebug => {
                let span = self.span_data(method, r.handle().map_err(bad)?)?;
                let text = match span.place {
                    Place::Input { lo, hi } => format!("#{} input {lo}..={hi}", origin_byte(span.hygiene)),
                    Place::Attr { lo, hi } => format!("#{} attribute {lo}..={hi}", origin_byte(span.hygiene)),
                    Place::Site => format!("#{} site", origin_byte(span.hygiene)),
                };
                w.str(&text);
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
                // The file holding the call (turso_macros' `get_caller_file` asks it of its
                // input's first token): a read of the run, not of the item's place in the file.
                let span = self.span_data(method, r.handle().map_err(bad)?)?;
                let file = self.file_of(span)?;
                self.asked_file = true;
                w.str(self.sym_text(file));
            }
            Method::SpanByteRange | Method::SpanLine | Method::SpanColumn | Method::SpanLocalFile => {
                // Where the item sits in its file is not an input of an expansion (rule 71):
                // answering it would need a keyed read of the position, which the host does
                // not serve. Refused, never answered with a made-up place. `local_file` is a
                // path on this machine (rule 25), never answered.
                self.span_data(method, r.handle().map_err(bad)?)?;
                return Err(ServerError::Position { method });
            }
            Method::SpanJoin => {
                let a = r.handle().map_err(bad)?;
                let b = r.handle().map_err(bad)?;
                let (da, db) = (self.span_data(method, a)?, self.span_data(method, b)?);
                let place = match (da.place, db.place) {
                    (Place::Input { lo, hi }, Place::Input { lo: lo2, hi: hi2 }) => {
                        Some(Place::Input { lo: lo.min(lo2), hi: hi.max(hi2) })
                    }
                    (Place::Attr { lo, hi }, Place::Attr { lo: lo2, hi: hi2 }) => {
                        Some(Place::Attr { lo: lo.min(lo2), hi: hi.max(hi2) })
                    }
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
                    Place::Input { lo, hi } => Some((&self.input, lo, hi)),
                    Place::Attr { lo, hi } => Some((&self.attr, lo, hi)),
                    Place::Site => None,
                };
                match range.and_then(|(toks, lo, hi)| toks.get(lo as usize..=hi as usize)) {
                    Some(toks) => {
                        let text = self.print(toks);
                        w.some(true);
                        w.str(&text);
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
        if !r.is_empty() {
            return Err(ServerError::Trailing { method });
        }
        Ok(())
    }

    /// `Option<TokenStream>`: the base's tokens, taken, or none.
    fn read_base(&mut self, r: &mut Reader<'_>, method: Method) -> Result<Vec<Tok, Global>, ServerError> {
        let bad = |what| ServerError::Malformed { method: Some(method), what };
        if r.some().map_err(bad)? {
            let handle = r.handle().map_err(bad)?;
            self.take(method, handle)
        } else {
            Ok(Vec::new_in(Global))
        }
    }

    /// One `TokenTree`, its group's stream taken, appended to `out` flat.
    fn read_tree(&mut self, r: &mut Reader<'_>, method: Method, out: &mut Vec<Tok, Global>) -> Result<(), ServerError> {
        let bad = |what| ServerError::Malformed { method: Some(method), what };
        match r.u8().map_err(bad)? {
            0 => {
                let delim = delim_of(r.u8().map_err(bad)?).map_err(bad)?;
                let inner = self.read_base(r, method)?;
                let open = r.handle().map_err(bad)?;
                let close = r.handle().map_err(bad)?;
                let entire = r.handle().map_err(bad)?;
                for span in [open, close, entire] {
                    self.span_data(method, span)?;
                }
                out.push(Tok::Open { delim, open, close, entire });
                out.extend(inner);
                out.push(Tok::Close { delim });
            }
            1 => {
                let ch = r.u8().map_err(bad)?;
                let joint = r.bool().map_err(bad)?;
                let span = r.handle().map_err(bad)?;
                self.span_data(method, span)?;
                if !is_punct(ch) {
                    return Err(ServerError::BadPunct(ch));
                }
                out.push(Tok::Punct { ch, joint, span });
            }
            2 => {
                let text = r.str().map_err(bad)?;
                let sym = self.sym(text)?;
                let raw = match r.u8().map_err(bad)? {
                    0 | 2 => false,
                    1 => true,
                    other => return Err(bad(Malformed::Tag("the IdentKind tag", other))),
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
            other => return Err(bad(Malformed::Tag("the TokenTree tag", other))),
        }
        Ok(())
    }

    /// `Vec<TokenTree>` of `toks`' top level, each group's inside a new handle.
    fn write_trees(&mut self, toks: &[Tok], out: &mut Vec<u8>) -> Result<(), ServerError> {
        let count_at = out.len();
        Writer { out }.usize(0);
        let mut count = 0u64;
        let mut i = 0;
        while i < toks.len() {
            count += 1;
            match toks[i] {
                Tok::Open { delim, open, close, entire } => {
                    let end = close_index(toks, i);
                    let inner = &toks[i + 1..end];
                    let mut w = Writer { out };
                    w.u8(0);
                    w.u8(delim_tag(delim));
                    if inner.is_empty() {
                        w.some(false);
                    } else {
                        let mut copy = Vec::with_capacity_in(inner.len(), Global);
                        copy.extend_from_slice(inner);
                        let held = self.hold(copy)?;
                        let mut w = Writer { out };
                        w.some(true);
                        w.handle(held);
                    }
                    let mut w = Writer { out };
                    w.handle(open);
                    w.handle(close);
                    w.handle(entire);
                    i = end + 1;
                    continue;
                }
                Tok::Close { .. } => {}
                Tok::Ident { sym, raw, span } => {
                    let mut w = Writer { out };
                    w.u8(2);
                    w.str(self.sym_text(sym));
                    w.u8(u8::from(raw));
                    w.handle(span);
                }
                Tok::Punct { ch, joint, span } => {
                    let mut w = Writer { out };
                    w.u8(1);
                    w.u8(ch);
                    w.bool(joint);
                    w.handle(span);
                }
                Tok::Literal { kind, sym, suffix, span } => {
                    let mut w = Writer { out };
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

    /// `text` lexed as the edition's tokens, each spanned `call_site`; `None` when it is not
    /// tokens.
    fn from_str(&mut self, text: &str) -> Result<Option<Vec<Tok, Global>>, ServerError> {
        let Ok(stream) = lex(text, self.edition, self.scratch) else { return Ok(None) };
        let mut depth = 0usize;
        for token in stream.tokens() {
            match token.kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) if depth == 0 => return Ok(None),
                TokenKind::Close(_) => depth -= 1,
                TokenKind::Literal { kind: LitKind::Err, .. } => return Ok(None),
                _ => {}
            }
        }
        if depth != 0 {
            return Ok(None);
        }
        let site = SpanData { place: Place::Site, hygiene: TokenOrigin::Made(Transparency::Transparent) };
        self.import(&stream, |_| site).map(Some)
    }

    /// `text` as one literal (a `-` before a number is part of it, as rustc reads it).
    fn literal_from_str(&mut self, text: &str) -> Result<Option<(LitKind, String, Option<String>)>, ServerError> {
        let Ok(stream) = lex(text, self.edition, self.scratch) else { return Ok(None) };
        let tokens = stream.tokens();
        let (minus, token) = match tokens {
            [token] => ("", token),
            [minus, token] if matches!(minus.kind, TokenKind::Punct { char: '-', .. }) => ("-", token),
            _ => return Ok(None),
        };
        let TokenKind::Literal { kind, suffix_len, .. } = token.kind else { return Ok(None) };
        if kind == LitKind::Err || (!minus.is_empty() && !matches!(kind, LitKind::Integer | LitKind::Float)) {
            return Ok(None);
        }
        let all = stream.text_of(token);
        let (body, suffix) = all.split_at(all.len() - suffix_len as usize);
        Ok(Some((kind, format!("{minus}{body}"), (!suffix.is_empty()).then(|| suffix.to_owned()))))
    }

    fn is_ident(&self, text: &str) -> bool {
        if text == "_" {
            return true;
        }
        let Ok(stream) = lex(text, self.edition, self.scratch) else { return false };
        matches!(stream.tokens(), [token] if matches!(token.kind, TokenKind::Ident { raw: Raw::No, .. }) && stream.text_of(token) == text)
    }

    /// `toks` printed as rustc's `TokenStream::to_string` spaces them: a space between two
    /// tokens, but none after a joint `Punct`, an opening delimiter, or before a closing one.
    fn print(&self, toks: &[Tok]) -> String {
        let mut out = String::new();
        let mut glue = true;
        for tok in toks {
            let closing = matches!(tok, Tok::Close { .. });
            if !glue && !closing {
                out.push(' ');
            }
            glue = false;
            match *tok {
                Tok::Open { delim, .. } => {
                    out.push_str(match delim {
                        Delim::Paren => "(",
                        Delim::Brace => "{",
                        Delim::Bracket => "[",
                        Delim::Invisible => "",
                    });
                    glue = true;
                }
                Tok::Close { delim } => out.push_str(match delim {
                    Delim::Paren => ")",
                    Delim::Brace => "}",
                    Delim::Bracket => "]",
                    Delim::Invisible => "",
                }),
                Tok::Ident { sym, raw, .. } => {
                    if raw {
                        out.push_str("r#");
                    }
                    out.push_str(self.sym_text(sym));
                }
                Tok::Punct { ch, joint, .. } => {
                    out.push(char::from(ch));
                    glue = joint;
                }
                Tok::Literal { kind, sym, suffix, .. } => {
                    let suffix = suffix.map_or("", |it| self.sym_text(it));
                    write_literal(&mut out, kind, self.sym_text(sym), suffix);
                }
            }
        }
        out
    }

    /// `emit_diagnostic`: the diagnostic and its children, each kept with its first span.
    fn emit(&mut self, r: &mut Reader<'_>, method: Method) -> Result<(), ServerError> {
        let bad = |what| ServerError::Malformed { method: Some(method), what };
        // Diagnostics left to read at each depth: the walk of the tree, without recursion.
        let mut left: Vec<usize, Global> = Vec::new_in(Global);
        left.push(1);
        while let Some(top) = left.last_mut() {
            if *top == 0 {
                left.pop();
                continue;
            }
            *top -= 1;
            let level = level_of(r.u8().map_err(bad)?).map_err(bad)?;
            let message = r.str().map_err(bad)?;
            let spans = r.count().map_err(bad)?;
            let mut first = None;
            for _ in 0..spans {
                let span = r.handle().map_err(bad)?;
                self.span_data(method, span)?;
                first.get_or_insert(span);
            }
            if self.diag_text.len() + message.len() > self.limits.text {
                return Err(ServerError::Limit(ProcLimit::Output));
            }
            let at = self.diag_text.len() as u32;
            self.diag_text.extend_from_slice(message.as_bytes());
            self.diags.push(Diag { level, message: (at, message.len() as u32), span: first });
            let children = r.count().map_err(bad)?;
            left.push(children);
        }
        Ok(())
    }

    /// What the run gave: `Result<TokenStream, PanicMessage>`.
    pub fn outcome(&self, output: &[u8]) -> Result<Outcome, ServerError> {
        let mut r = Reader::new(output);
        let bad = |what| ServerError::Malformed { method: None, what };
        let ok = r.u8().map_err(bad)?;
        let outcome = match ok {
            0 => Outcome::Tokens(if r.some().map_err(bad)? { Some(r.handle().map_err(bad)?) } else { None }),
            1 => {
                // `PanicMessage` is encoded as `Option<&str>`.
                let message = if r.some().map_err(bad)? { r.str().map_err(bad)?.to_owned() } else { "a panic with no message".to_owned() };
                Outcome::Panicked(message)
            }
            other => return Err(bad(Malformed::Tag("the Result tag", other))),
        };
        if !r.is_empty() {
            return Err(bad(Malformed::Short("the output, which has bytes after its end")));
        }
        Ok(outcome)
    }

    /// The stream `handle` (none for a panic) as blaze tokens with their origins, the
    /// diagnostics, and `reads`, in `alloc`.
    pub fn produce<'a>(&mut self, handle: Option<u32>, panicked: Option<&str>, reads: Reads<'a>, alloc: Alloc<'a>) -> Result<Produced<'a>, ProduceError> {
        let mut tokens = TokenStream::new_in(alloc);
        let mut origins: AVec<'a, TokenOrigin> = AVec::new_in(alloc);
        let toks = match handle {
            Some(handle) => self.take(Method::TsDrop, handle).map_err(ProduceError::Server)?,
            None => Vec::new_in(Global),
        };
        let ctx = Ctx::root(self.edition);
        let origin = |spans: &[SpanData], span: SpanId| spans[span as usize - 1].hygiene;
        let mut closes: Vec<SpanId, Global> = Vec::new_in(Global);
        for tok in toks.iter() {
            match *tok {
                Tok::Open { delim, open, close, .. } => {
                    tokens.open(delim, ctx).map_err(ProduceError::Stream)?;
                    origins.push(origin(&self.spans, open));
                    closes.push(close);
                }
                Tok::Close { delim } => {
                    tokens.close(delim, ctx).map_err(ProduceError::Stream)?;
                    let close = closes.pop().unwrap_or(CALL_SITE);
                    origins.push(origin(&self.spans, close));
                }
                Tok::Ident { sym, raw, span } => {
                    let kind = TokenKind::Ident { raw: if raw { Raw::Yes } else { Raw::No }, spacing: Spacing::Alone };
                    tokens.push(kind, self.sym_text(sym), ctx).map_err(ProduceError::Stream)?;
                    origins.push(origin(&self.spans, span));
                }
                Tok::Punct { ch, joint, span } => {
                    let spacing = if joint { Spacing::Joint } else { Spacing::Alone };
                    tokens.punct(char::from(ch), spacing, ctx).map_err(ProduceError::Stream)?;
                    origins.push(origin(&self.spans, span));
                }
                Tok::Literal { kind, sym, suffix, span } => {
                    let mut body = self.sym_text(sym);
                    if matches!(kind, LitKind::Integer | LitKind::Float)
                        && let Some(rest) = body.strip_prefix('-')
                    {
                        tokens.punct('-', Spacing::Alone, ctx).map_err(ProduceError::Stream)?;
                        origins.push(origin(&self.spans, span));
                        body = rest;
                    }
                    let suffix = suffix.map_or("", |it| self.sym_text(it));
                    let mut text = String::with_capacity(body.len() + suffix.len());
                    text.push_str(body);
                    text.push_str(suffix);
                    tokens.literal(kind, &text, suffix.len() as u32, ctx).map_err(ProduceError::Stream)?;
                    origins.push(origin(&self.spans, span));
                }
            }
        }
        let mut text: AVec<'a, u8> = AVec::new_in(alloc);
        let mut diagnostics: AVec<'a, ProcDiagnostic> = AVec::new_in(alloc);
        for diag in self.diags.iter() {
            let (at, len) = diag.message;
            let message = Text { at: text.len() as u32, len };
            text.extend_from_slice(&self.diag_text[at as usize..(at + len) as usize]);
            let at = diag.span.map(|span| origin(&self.spans, span));
            diagnostics.push(ProcDiagnostic::Emitted { level: diag.level, message, at });
        }
        if let Some(panicked) = panicked {
            let message = Text { at: text.len() as u32, len: panicked.len() as u32 };
            text.extend_from_slice(panicked.as_bytes());
            diagnostics.push(ProcDiagnostic::Panicked { message });
        }
        Ok(Produced { tokens, origins, reads, diagnostics, text })
    }
}

#[cfg(test)]
mod tests {
    use blaze_expand::{lex, Edition, LitKind, ProcCall, ProcKind, Reads, TokenKind};
    use blaze_own::Alloc;

    use super::{Server, ServerLimits, SpanId, Tok};

    fn produce_literal(kind: LitKind, text: &str) -> blaze_expand::TokenStream<'static> {
        let alloc = Alloc::global();
        let input = lex("input", Edition::E2021, alloc).expect("the test input lexes");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };
        let mut run_input = Vec::new();
        let mut server = Server::new(Edition::E2021, ServerLimits::STANDARD, &call, alloc, &mut run_input).expect("the server starts");
        let sym = server.sym(text).expect("the literal text fits");
        let span = server.spans.len() as SpanId;
        let handle = server.hold(vec![Tok::Literal { kind, sym, suffix: None, span }]).expect("the output stream is held");
        server.produce(Some(handle), None, Reads::new_in(alloc), alloc).expect("the output is produced").tokens
    }

    /// What `span_file` of the span `pick` chooses answers, for the input `input` of a call in
    /// `file`: the string, or why it was refused; and whether the server then records the
    /// call's file as a read.
    fn span_file(
        input: &blaze_expand::TokenStream<'_>,
        file: Result<&str, blaze_expand::HostError>,
        method: crate::protocol::Method,
        pick: impl Fn(&Server) -> SpanId,
    ) -> (Result<String, super::ServerError>, bool) {
        let call = ProcCall { kind: ProcKind::FnLike, input, attr_args: None, file };
        let mut run_input = Vec::new();
        let mut server = Server::new(Edition::E2021, ServerLimits::STANDARD, &call, Alloc::global(), &mut run_input).expect("the server starts");
        let mut request = vec![method as u8];
        crate::wire::Writer { out: &mut request }.handle(pick(&server));
        let mut reply = Vec::new();
        let answered = server.dispatch(&request, &mut reply).map(|()| {
            let mut r = crate::wire::Reader::new(&reply);
            assert_eq!(r.u8().expect("the reply's tag"), 0, "an `Ok` reply");
            r.str().expect("a string").to_owned()
        });
        (answered, server.asked_file)
    }

    fn span_of(server: &Server, at: usize) -> SpanId {
        match server.input[at] {
            Tok::Ident { span, .. } | Tok::Punct { span, .. } | Tok::Literal { span, .. } => span,
            Tok::Open { open, .. } => open,
            Tok::Close { .. } => panic!("a close has no span of its own"),
        }
    }

    /// turso_macros' `get_caller_file` (macros/src/lib.rs:1690) asks
    /// `first_token.span().start().file()` of its input: 25 of turso's errors (c1647
    /// check.out) were its refusal. A token written in the call's file answers the call's
    /// file as the host shows it, and the run records it as a read. The twin: a host that does
    /// not answer the file refuses it, naming why, and records nothing. The controls: a site
    /// span, a token an expansion made, and `line` stay refused.
    #[test]
    fn span_file_answers_the_calls_file_for_a_token_written_in_it() {
        use crate::protocol::Method;
        use blaze_expand::{Ctx, CtxNode, DefSite, HostAsk, HostError, HostWhy, Mark, MarkOf, Transparency};

        let mut input = lex("x > 0", Edition::E2021, Alloc::global()).expect("the input lexes");
        let site = DefSite { krate: crate::hook::crate_key("m"), module: None };
        let mark = Mark { of: MarkOf::This, transparency: Transparency::SemiOpaque, edition: Edition::E2021, site };
        let made = input.context(CtxNode { parent: Ctx::root(Edition::E2021), mark }).expect("a context");
        input.ident("y", made).expect("a made token");
        let file = Ok("crates/core/src/lib.rs");

        let first = span_file(&input, file, Method::SpanFile, |s| span_of(s, 0));
        assert_eq!(first, (Ok("crates/core/src/lib.rs".to_owned()), true), "the real case: the call's file, recorded");
        // Any token written in the item answers the same (`span_start` gives back the handle it
        // is passed, so `span().start().file()` asks this same span).
        let punct = span_file(&input, file, Method::SpanFile, |s| span_of(s, 1));
        assert_eq!(punct, (Ok("crates/core/src/lib.rs".to_owned()), true));

        let refused = HostError { ask: HostAsk::Place, why: HostWhy::Refused };
        let twin = span_file(&input, Err(refused), Method::SpanFile, |s| span_of(s, 0));
        assert_eq!(twin, (Err(super::ServerError::FileUnknown { why: refused }), false), "no file known: refused, no read");

        let site = span_file(&input, file, Method::SpanFile, |_| super::CALL_SITE);
        assert_eq!(site, (Err(super::ServerError::FileOfSite), false));
        let made = span_file(&input, file, Method::SpanFile, |s| span_of(s, 3));
        assert_eq!(made, (Err(super::ServerError::FileOfMadeToken), false));
        let line = span_file(&input, file, Method::SpanLine, |s| span_of(s, 0));
        assert_eq!(line, (Err(super::ServerError::Position { method: Method::SpanLine }), false), "`line` stays refused (rule 71)");
    }

    #[test]
    fn a_literal_payload_keeps_a_leading_minus_inside_strings_but_splits_numeric_minus() {
        let string = produce_literal(LitKind::Str, "-output");
        assert!(matches!(string.tokens()[0].kind, TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, .. }));
        assert_eq!(string.text_at(0), "-output");

        let integer = produce_literal(LitKind::Integer, "-1");
        assert_eq!(integer.tokens().len(), 2);
        assert!(matches!(integer.tokens()[0].kind, TokenKind::Punct { char: '-', .. }));
        assert!(matches!(integer.tokens()[1].kind, TokenKind::Literal { kind: LitKind::Integer, .. }));
        assert_eq!(integer.text_at(1), "1");
    }
}

/// Why the output could not be made blaze tokens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProduceError {
    Server(ServerError),
    Stream(blaze_expand::StreamError),
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
