//! What blaze hands the macro's process: the program (which compiled crate, which of its macros)
//! and one call's bytes. The call's bytes go to the process when it is started for the call (the
//! launcher's job, piece 4) and are the `input` of blaze-comptime's [`ProcMacroOpts`], so the
//! run's key covers them (rule 21).
//!
//! # No marks cross
//!
//! The call carries its kind, its edition and its tokens' kinds and texts, never a hygiene mark.
//! The process sees three fixed site spans (`def_site`, `call_site`, `mixed_site`: the handles 1,
//! 2 and 3 of `ExpnGlobals`) and a span per input token; what comes back names each token's
//! origin as one of those ([`TokenOrigin`]), and blaze-expand gives the expansion's tokens their
//! real marks (`MarkOf::This`, the definition, the edition) as it writes the expansion. So no mark
//! of another crate or another macro is in the process to be forged, and none it sends back is
//! taken (outcome.rs checks every origin against this call).
//!
//! [`ProcMacroOpts`]: blaze_comptime::ProcMacroOpts

use core::fmt;

use blaze_expand::{Delim, Edition, LitKind, ProcKind, Raw, Spacing, TokenKind, TokenOrigin, TokenStream, Transparency};
use blaze_id::ContentId;
use blaze_own::AVec;

use crate::bridge::{Field, Malformed, Reader, Writer, index};

/// The version of the call's and the outcome's bytes: a process built for another is refused by
/// its first byte.
pub const FORMAT: u8 = 1;

/// A macro of a program: its index in the crate's `#[rustc_proc_macro_decls]` static, which is
/// the order the harness lists the crate's macros in (rustc_builtin_macros proc_macro_harness.rs
/// `mk_decls`; the interim builder's `macros` lines).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MacroId(pub u32);

/// One macro of a program, as the crate's records declare it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroDecl<'p> {
    pub kind: ProcKind,
    /// The name it is called by (`Serialize`, `tokio::main`'s `main`).
    pub name: &'p str,
    /// A derive's helper attributes (`serde`).
    pub helpers: &'p [&'p str],
}

/// A compiled proc-macro crate: what the launcher maps and what errors name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroProgram<'p> {
    /// The crate's name, as an error prints it.
    pub krate: &'p str,
    /// The code record of the crate's compiled code, std and proc_macro with it (piece 3).
    pub code: ContentId,
    /// The symbol of its `#[rustc_proc_macro_decls]` static: a `&[proc_macro::bridge::client::Client]`.
    pub decls: &'p str,
    /// Its macros, in the static's order.
    pub macros: &'p [MacroDecl<'p>],
}

impl<'p> MacroProgram<'p> {
    pub fn get(&self, which: MacroId) -> Option<&'p MacroDecl<'p>> {
        self.macros.get(usize::try_from(which.0).ok()?)
    }

    /// The macro called `name` of `kind`.
    pub fn find(&self, kind: ProcKind, name: &str) -> Option<MacroId> {
        let at = self.macros.iter().position(|m| m.kind == kind && m.name == name)?;
        Some(MacroId(index(at)))
    }
}

/// One call of a macro of a program.
#[derive(Clone, Copy)]
pub struct Call<'c, 's> {
    pub which: MacroId,
    pub kind: ProcKind,
    /// The call's edition: the one `ts_from_str` lexes in and the host checks the outcome by.
    pub edition: Edition,
    /// The item (a derive, an attribute) or the arguments (a function-like macro).
    pub input: &'c TokenStream<'s>,
    /// An attribute's arguments; `#[m]` has empty ones, never none (its client reads a handle
    /// for them, as rustc passes them).
    pub attr: Option<&'c TokenStream<'s>>,
}

/// A call that cannot be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallError {
    /// Attribute arguments given to a call of `kind`, which takes none.
    ArgumentsOnNonAttribute { kind: ProcKind },
    /// An input whose delimiters do not pair, at this token.
    Unbalanced { at: u32 },
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::ArgumentsOnNonAttribute { kind } => write!(f, "attribute arguments given to a {kind} macro's call, which takes none"),
            CallError::Unbalanced { at } => write!(f, "a call's input whose delimiters do not pair at its token {at}"),
        }
    }
}

impl Call<'_, '_> {
    /// The call's bytes, onto `out` (cleared first). Equal calls give equal bytes.
    pub fn encode(&self, out: &mut AVec<'_, u8>) -> Result<(), CallError> {
        match self.kind {
            ProcKind::Attr => {}
            ProcKind::FnLike | ProcKind::Derive => {
                if self.attr.is_some() {
                    return Err(CallError::ArgumentsOnNonAttribute { kind: self.kind });
                }
            }
        }
        check_balanced(self.input)?;
        if let Some(attr) = self.attr {
            check_balanced(attr)?;
        }
        out.clear();
        let mut w = Writer { out };
        w.u8(FORMAT);
        w.u32(self.which.0);
        w.u8(self.kind.byte());
        w.u8(self.edition.byte());
        put_stream(&mut w, self.input);
        match (self.kind, self.attr) {
            (ProcKind::Attr, Some(attr)) => {
                w.some(true);
                put_stream(&mut w, attr);
            }
            // `#[m]`: empty arguments.
            (ProcKind::Attr, None) => {
                w.some(true);
                w.usize(0);
            }
            _ => w.some(false),
        }
        Ok(())
    }
}

fn check_balanced(stream: &TokenStream<'_>) -> Result<(), CallError> {
    let mut depth = 0u32;
    for (at, token) in stream.tokens().iter().enumerate() {
        match token.kind {
            TokenKind::Open(_) => depth = depth.saturating_add(1),
            TokenKind::Close(_) if depth == 0 => return Err(CallError::Unbalanced { at: index(at) }),
            TokenKind::Close(_) => depth -= 1,
            _ => {}
        }
    }
    if depth == 0 { Ok(()) } else { Err(CallError::Unbalanced { at: index(stream.tokens().len()) }) }
}

fn put_stream(w: &mut Writer<'_, '_>, stream: &TokenStream<'_>) {
    w.usize(u64::from(index(stream.tokens().len())));
    for token in stream.tokens() {
        put_token(w, token.kind, stream.text_of(token));
    }
}

/// A call's bytes, read: what the macro's process runs.
#[derive(Clone, Copy, Debug)]
pub struct CallView<'b> {
    pub which: MacroId,
    pub kind: ProcKind,
    pub edition: Edition,
    pub input: Tokens<'b>,
    pub attr: Option<Tokens<'b>>,
}

impl<'b> CallView<'b> {
    /// `bytes` read whole: every token checked to be one the format writes.
    pub fn read(bytes: &'b [u8]) -> Result<CallView<'b>, Malformed> {
        let mut r = Reader::new(bytes);
        let version = r.tag(Field::Version)?;
        if version != FORMAT {
            return Err(Malformed::Tag(Field::Version, version));
        }
        let which = MacroId(r.u32()?);
        let kind = r.tag(Field::ProcKind)?;
        let kind = ProcKind::from_byte(kind).ok_or(Malformed::Tag(Field::ProcKind, kind))?;
        let edition = r.tag(Field::Edition)?;
        let edition = Edition::from_byte(edition).ok_or(Malformed::Tag(Field::Edition, edition))?;
        let input = Tokens::read(&mut r)?;
        let attr = if r.some()? { Some(Tokens::read(&mut r)?) } else { None };
        r.end()?;
        Ok(CallView { which, kind, edition, input, attr })
    }
}

/// A list of tokens in the format's bytes: kinds and texts, no marks.
#[derive(Clone, Copy, Debug)]
pub struct Tokens<'b> {
    count: usize,
    bytes: &'b [u8],
}

impl<'b> Tokens<'b> {
    fn read(r: &mut Reader<'b>) -> Result<Tokens<'b>, Malformed> {
        let count = r.count()?;
        let start = r.rest();
        for _ in 0..count {
            read_token(r)?;
        }
        let used = start.len() - r.rest().len();
        Ok(Tokens { count, bytes: &start[..used] })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The tokens, each its kind and text. Read once already, so each item is `Ok`.
    pub fn iter(self) -> impl Iterator<Item = Result<(TokenKind, &'b str), Malformed>> + 'b {
        let mut r = Reader::new(self.bytes);
        (0..self.count).map(move |_| read_token(&mut r))
    }
}

/// One token: its kind, then its text for an identifier or a literal.
pub(crate) fn put_token(w: &mut Writer<'_, '_>, kind: TokenKind, text: &str) {
    put_kind(w, kind);
    if has_text(kind) {
        w.str(text);
    }
}

pub(crate) fn read_token<'b>(r: &mut Reader<'b>) -> Result<(TokenKind, &'b str), Malformed> {
    let kind = read_kind(r)?;
    let text = if has_text(kind) { r.str()? } else { "" };
    Ok((kind, text))
}

pub(crate) fn has_text(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Ident { .. } | TokenKind::Literal { .. })
}

pub(crate) fn put_kind(w: &mut Writer<'_, '_>, kind: TokenKind) {
    match kind {
        TokenKind::Open(delim) => {
            w.u8(0);
            w.u8(delim_tag(delim));
        }
        TokenKind::Close(delim) => {
            w.u8(1);
            w.u8(delim_tag(delim));
        }
        TokenKind::Ident { raw, spacing } => {
            w.u8(2);
            w.bool(raw == Raw::Yes);
            w.u8(spacing_tag(spacing));
        }
        TokenKind::Punct { char, spacing } => {
            w.u8(3);
            w.u32(u32::from(char));
            w.u8(spacing_tag(spacing));
        }
        TokenKind::Literal { kind, suffix_len, spacing } => {
            w.u8(4);
            lit_write(w, kind);
            w.u32(suffix_len);
            w.u8(spacing_tag(spacing));
        }
    }
}

pub(crate) fn read_kind(r: &mut Reader<'_>) -> Result<TokenKind, Malformed> {
    Ok(match r.tag(Field::TokenKind)? {
        0 => TokenKind::Open(delim_of(r.tag(Field::Delimiter)?)?),
        1 => TokenKind::Close(delim_of(r.tag(Field::Delimiter)?)?),
        2 => {
            let raw = if r.bool()? { Raw::Yes } else { Raw::No };
            TokenKind::Ident { raw, spacing: spacing_of(r.tag(Field::Spacing)?)? }
        }
        3 => {
            let code = r.u32()?;
            let char = char::from_u32(code).ok_or(Malformed::Tag(Field::Char, u8::try_from(code & 0xff).unwrap_or(0)))?;
            TokenKind::Punct { char, spacing: spacing_of(r.tag(Field::Spacing)?)? }
        }
        4 => {
            let kind = lit_read(r)?;
            let suffix_len = r.u32()?;
            TokenKind::Literal { kind, suffix_len, spacing: spacing_of(r.tag(Field::Spacing)?)? }
        }
        other => return Err(Malformed::Tag(Field::TokenKind, other)),
    })
}

/// A token's origin: the input's or the attribute arguments' token at an index, a site
/// (`Made`), or a definition's token (which no proc macro has, written only to be refused by
/// name: outcome.rs).
pub(crate) fn put_origin(w: &mut Writer<'_, '_>, origin: TokenOrigin) {
    let (tag, value) = match origin {
        TokenOrigin::Input(at) => (0, at),
        TokenOrigin::AttrArgs(at) => (1, at),
        TokenOrigin::Def(at) => (2, at),
        TokenOrigin::Params(at) => (3, at),
        TokenOrigin::Made(t) => (4, u32::from(t.byte())),
    };
    w.u8(tag);
    w.u32(value);
}

pub(crate) fn read_origin(r: &mut Reader<'_>) -> Result<TokenOrigin, Malformed> {
    let tag = r.tag(Field::Origin)?;
    let value = r.u32()?;
    Ok(match tag {
        0 => TokenOrigin::Input(value),
        1 => TokenOrigin::AttrArgs(value),
        2 => TokenOrigin::Def(value),
        3 => TokenOrigin::Params(value),
        4 => {
            let byte = u8::try_from(value).map_err(|_| Malformed::Tag(Field::Transparency, u8::MAX))?;
            TokenOrigin::Made(Transparency::from_byte(byte).ok_or(Malformed::Tag(Field::Transparency, byte))?)
        }
        other => return Err(Malformed::Tag(Field::Origin, other)),
    })
}

/// The bridge's `Delimiter` tags: `Parenthesis`, `Brace`, `Bracket`, `None`.
pub(crate) fn delim_tag(delim: Delim) -> u8 {
    match delim {
        Delim::Paren => 0,
        Delim::Brace => 1,
        Delim::Bracket => 2,
        Delim::Invisible => 3,
    }
}

pub(crate) fn delim_of(tag: u8) -> Result<Delim, Malformed> {
    Ok(match tag {
        0 => Delim::Paren,
        1 => Delim::Brace,
        2 => Delim::Bracket,
        3 => Delim::Invisible,
        other => return Err(Malformed::Tag(Field::Delimiter, other)),
    })
}

fn spacing_tag(spacing: Spacing) -> u8 {
    match spacing {
        Spacing::Alone => 0,
        Spacing::Joint => 1,
        Spacing::JointHidden => 2,
    }
}

fn spacing_of(tag: u8) -> Result<Spacing, Malformed> {
    Ok(match tag {
        0 => Spacing::Alone,
        1 => Spacing::Joint,
        2 => Spacing::JointHidden,
        other => return Err(Malformed::Tag(Field::Spacing, other)),
    })
}

/// The bridge's `LitKind` tags, a raw kind's `#` count after its tag.
pub(crate) fn lit_write(w: &mut Writer<'_, '_>, kind: LitKind) {
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

pub(crate) fn lit_read(r: &mut Reader<'_>) -> Result<LitKind, Malformed> {
    Ok(match r.tag(Field::LitKind)? {
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
        other => return Err(Malformed::Tag(Field::LitKind, other)),
    })
}
