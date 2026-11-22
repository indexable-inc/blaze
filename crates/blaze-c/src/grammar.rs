//! The grammar of `blaze::c::import! { .. }`: its arguments, parsed into a typed, normalized
//! [`Request`].
//!
//! ```text
//! mod NAME;                       the module the declarations land in (once)
//! header "path";                  a header to include, under the package (once or more; in order)
//! include "dir";                  a folder to search, under the package (in order)
//! define NAME;                    `#define NAME 1`
//! define NAME = VALUE;            `#define NAME VALUE`: an integer (`-` allowed), an identifier,
//!                                 or a string, whose text is the replacement as written
//! allow "pattern";                render only declarations a pattern matches
//! block "pattern";                render none a pattern matches (after `allow`)
//! link "name";                    the library that defines the functions and data (once)
//! inline_bodies on|off;           render the functions the headers define (once; default on)
//! macro_fn NAME(a: T, ..) -> T;   a function of the function-like macro NAME, by signature
//! ```
//!
//! What the import's own C unit holds, and the two arguments that ask for it:
//! - `inline_bodies`: a function the headers DEFINE with internal or inline linkage (`static
//!   inline int PyArray_NDIM(const PyArrayObject *arr) { .. }`, numpy ndarraytypes.h:1594) has
//!   no symbol in any library: each translation unit compiles its own copy. With
//!   `inline_bodies on`, the import's unit compiles the headers and exports one thunk a function
//!   that calls it, and the rendering declares the function as a normal one bound to the
//!   thunk's symbol ([`crate::key::UnitKey::symbol`]). On by DEFAULT, for every import, a
//!   member's own too: in the headers the C4 rung names (cpython, numpy) the accessor API IS
//!   these functions (`Py_INCREF`, `Py_REFCNT`, `PyArray_NDIM`, `PyArray_DATA`), so an import
//!   without them is not the library's C API; `off` is for an import that must not compile C
//!   (each such function is then a finding, [`crate::Unexpressed::InlineFunction`]). A
//!   header-local variable (`static void **PyArray_API = NULL;`) lives in the same unit
//!   whatever the switch says, since a declaration that reads it needs it.
//! - `macro_fn`: a function-like macro whose expansion is a typed expression of its arguments
//!   (the front end's [`crate::Unexpressed::FunctionLikeMacro`]) has no type until a program
//!   gives one. `macro_fn PyArray_NDIM(arr: *const PyArrayObject) -> c_int;` asks the unit for a
//!   C function `int f(const PyArrayObject *arr) { return PyArray_NDIM(arr); }` (the C compiler
//!   expands the macro, so the record needs no macro text) and the rendering declares it. A
//!   type is `*const T`, `*mut T` (to [`MOST_POINTERS`] levels) or a name: a `core::ffi` C
//!   scalar (`c_int`, `c_void` behind a pointer, and the rest of [`C_SCALARS`]) or a type the
//!   module renders (a record, enum or typedef of the header). No `->` returns `void`. Requests
//!   are a set by macro name (sorted; one name twice refused) with parameters in order.
//!
//! The two arguments past the brief's four, and why:
//! - `allow` and `block`: a header like `windows.h` or a libc header declares thousands of
//!   names a crate uses five of. Selecting them changes the RENDERING only (the record holds the
//!   whole header, so imports that select differently share one record; see [`crate::key`]),
//!   and every declaration left out is still a finding ([`crate::Unexpressed::NotSelected`]),
//!   so nothing is dropped unseen. A pattern is a C identifier, or one ending in `*` for a
//!   prefix (`rocksdb_*`): C libraries name by prefix, and a regular expression would be a
//!   second language for one job.
//! - `link`: which library defines the symbols. blaze has no link step, so the import is where a
//!   program says that `rocksdb_open` comes from rocksdb's units (or a system library); the
//!   rendering writes it as `#[link(name = ..)]` on the `extern` blocks, which `run`'s symbol
//!   index reads.
//!
//! The expansion side maps its tokens one to one onto [`Tok`] (its literal values unescaped),
//! with each token's byte range in the call's text: an error names its argument and that range,
//! which is item-relative (rule 71).

use blaze_own::{AVec, Alloc, Vec};

use crate::model::is_c_identifier;
use crate::place::{PlaceError, RelPath};

/// A token of the call, as the expansion side gives it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tok<'t> {
    pub kind: TokKind<'t>,
    /// The token's bytes in the call's text, from the call's start.
    pub at: u32,
    pub len: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokKind<'t> {
    /// An identifier or keyword (`mod`, `header`, `ROCKSDB_LITE`).
    Ident(&'t str),
    /// A string literal's value (unescaped by the expansion side).
    Str(&'t str),
    /// An integer literal's text, as written (`1`, `0x10`).
    Int(&'t str),
    /// One punctuation character (`;`, `=`, `-`).
    Punct(char),
}

/// Which argument an error is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg {
    Mod,
    Header,
    Include,
    Define,
    Allow,
    Block,
    Link,
    InlineBodies,
    MacroFn,
}

/// What a token was expected to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expected {
    /// An argument's name.
    Argument,
    Ident,
    Str,
    Semi,
    /// A define's value: an integer, an identifier or a string.
    Value,
    /// `on` or `off`.
    Switch,
    /// One punctuation character of a signature (`(`, `)`, `,`, `:`, `>` after `-`).
    Punct(char),
    /// A type: `*const`, `*mut` or a name.
    Type,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GrammarFault {
    /// A word that is no argument's name.
    UnknownArgument,
    /// Something else, or the end of the call, where `Expected` was.
    Expected(Expected),
    /// `mod` or `link` given twice.
    Repeated,
    /// One name defined twice: its two values would be ordered, and defines are a set.
    DefinedTwice,
    /// No `mod`, or no `header`.
    Missing,
    /// A path that is not a relative path under the package.
    Path(PlaceError),
    /// A module or define name that is not an identifier.
    Name,
    /// A pattern that is not an identifier with an optional trailing `*`.
    Pattern,
    /// A library name that is empty or holds `/` or a NUL.
    Library,
    /// More arguments than [`MOST_ARGUMENTS`].
    TooMany,
    /// A `macro_fn` type nested past [`MOST_POINTERS`], `c_void` not behind a pointer, or a
    /// name that is not a C identifier; or a parameter name that is not one.
    Type,
}

/// Why a call's arguments are not a request: the argument (`None` before one is named), the
/// byte range of the token at fault, and the fault (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GrammarError {
    pub arg: Option<Arg>,
    pub at: u32,
    pub len: u32,
    pub fault: GrammarFault,
}

/// The most arguments a call takes (rule 18).
pub const MOST_ARGUMENTS: usize = 4096;

/// A define's value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DefineValue<'r> {
    /// A `-` before an integer.
    pub negative: bool,
    /// The replacement text.
    pub text: &'r str,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Define<'r> {
    pub name: &'r str,
    /// `None` is `define NAME;`, which C's `-D NAME` makes `1`.
    pub value: Option<DefineValue<'r>>,
}

/// A selection pattern: a name, or a prefix when `prefix`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pattern<'r> {
    pub text: &'r str,
    pub prefix: bool,
}

impl Pattern<'_> {
    pub fn matches(&self, name: &str) -> bool {
        match self.prefix {
            true => name.starts_with(self.text),
            false => name == self.text,
        }
    }
}

/// The most pointer levels a `macro_fn` type has (`*const *mut T` is two).
pub const MOST_POINTERS: u8 = 8;

/// The `core::ffi` scalars a `macro_fn` type may name, each the C type of its name.
pub const C_SCALARS: &[&str] = &[
    "c_char", "c_schar", "c_uchar", "c_short", "c_ushort", "c_int", "c_uint", "c_long", "c_ulong", "c_longlong",
    "c_ulonglong", "c_float", "c_double", "c_void",
];

/// A `macro_fn` type: `depth` pointers (bit `j` of `muts` set when the `j`-th from the outside
/// is `*mut`) to `name`, a [`C_SCALARS`] name or a type of the header.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MacroType<'r> {
    pub name: &'r str,
    pub depth: u8,
    pub muts: u8,
}

impl MacroType<'_> {
    /// Whether the name is a `core::ffi` scalar (else a type of the header).
    pub fn is_scalar(&self) -> bool {
        C_SCALARS.contains(&self.name)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MacroParam<'r> {
    pub name: &'r str,
    pub ty: MacroType<'r>,
}

/// `macro_fn NAME(..) -> T;`: its parameters are `params_len` of [`Request::macro_params`] from
/// `params_start`; `result` `None` is `void`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MacroFn<'r> {
    pub name: &'r str,
    pub params_start: u32,
    pub params_len: u32,
    pub result: Option<MacroType<'r>>,
}

/// A call's request, normalized: two calls that mean the same thing are equal requests (and so
/// one request key). Headers keep their order; include folders keep theirs with later repeats
/// removed (a repeat never changes a search); defines are sorted by name; the patterns are
/// sorted with repeats removed; `macro_fn`s are sorted by name with their parameters laid out
/// in that order.
#[derive(Debug)]
pub struct Request<'r> {
    pub module: &'r str,
    pub headers: AVec<'r, RelPath<'r>>,
    pub include: AVec<'r, RelPath<'r>>,
    pub defines: AVec<'r, Define<'r>>,
    pub allow: AVec<'r, Pattern<'r>>,
    pub block: AVec<'r, Pattern<'r>>,
    pub link: Option<&'r str>,
    /// `inline_bodies`: on unless the call says `off`.
    pub inline_bodies: bool,
    pub macro_fns: AVec<'r, MacroFn<'r>>,
    pub macro_params: AVec<'r, MacroParam<'r>>,
}

impl<'r> Request<'r> {
    /// The parameters of `f`, one of this request's `macro_fns`.
    pub fn params_of(&self, f: &MacroFn<'r>) -> &[MacroParam<'r>] {
        let start = crate::at(f.params_start);
        &self.macro_params[start..start + crate::at(f.params_len)]
    }
}

impl Request<'_> {
    /// Whether the rendering keeps a declaration named `name`: allowed (by any pattern, or
    /// every name when there is no `allow`) and not blocked.
    pub fn selects(&self, name: &str) -> bool {
        let allowed = self.allow.is_empty() || self.allow.iter().any(|p| p.matches(name));
        allowed && !self.block.iter().any(|p| p.matches(name))
    }
}

struct Cursor<'k, 't> {
    toks: &'k [Tok<'t>],
    next: usize,
    /// Where the call ends, for an error at the end.
    end: u32,
}

impl<'k, 't> Cursor<'k, 't> {
    fn peek(&self) -> Option<Tok<'t>> {
        self.toks.get(self.next).copied()
    }

    fn take(&mut self) -> Option<Tok<'t>> {
        let tok = self.peek()?;
        self.next += 1;
        Some(tok)
    }

    fn error(&self, arg: Option<Arg>, tok: Option<Tok<'t>>, fault: GrammarFault) -> GrammarError {
        match tok {
            Some(tok) => GrammarError { arg, at: tok.at, len: tok.len, fault },
            None => GrammarError { arg, at: self.end, len: 0, fault },
        }
    }

    fn ident(&mut self, arg: Arg) -> Result<(&'t str, Tok<'t>), GrammarError> {
        match self.take() {
            Some(tok @ Tok { kind: TokKind::Ident(text), .. }) => Ok((text, tok)),
            other => Err(self.error(Some(arg), other, GrammarFault::Expected(Expected::Ident))),
        }
    }

    fn string(&mut self, arg: Arg) -> Result<(&'t str, Tok<'t>), GrammarError> {
        match self.take() {
            Some(tok @ Tok { kind: TokKind::Str(text), .. }) => Ok((text, tok)),
            other => Err(self.error(Some(arg), other, GrammarFault::Expected(Expected::Str))),
        }
    }

    fn semi(&mut self, arg: Arg) -> Result<(), GrammarError> {
        match self.take() {
            Some(Tok { kind: TokKind::Punct(';'), .. }) => Ok(()),
            other => Err(self.error(Some(arg), other, GrammarFault::Expected(Expected::Semi))),
        }
    }

    fn path(&mut self, arg: Arg) -> Result<RelPath<'t>, GrammarError> {
        let (text, tok) = self.string(arg)?;
        RelPath::new(text).map_err(|e| self.error(Some(arg), Some(tok), GrammarFault::Path(e)))
    }

    fn pattern(&mut self, arg: Arg) -> Result<Pattern<'t>, GrammarError> {
        let (text, tok) = self.string(arg)?;
        let (name, prefix) = match text.strip_suffix('*') {
            Some(name) => (name, true),
            None => (text, false),
        };
        let ok = match prefix {
            // A prefix may be empty of nothing but must itself be identifier bytes.
            true => !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$'),
            false => is_c_identifier(name),
        };
        match ok {
            true => Ok(Pattern { text: name, prefix }),
            false => Err(self.error(Some(arg), Some(tok), GrammarFault::Pattern)),
        }
    }

    fn punct(&mut self, arg: Arg, want: char) -> Result<(), GrammarError> {
        match self.take() {
            Some(Tok { kind: TokKind::Punct(p), .. }) if p == want => Ok(()),
            other => Err(self.error(Some(arg), other, GrammarFault::Expected(Expected::Punct(want)))),
        }
    }

    fn at_punct(&self, want: char) -> bool {
        matches!(self.peek(), Some(Tok { kind: TokKind::Punct(p), .. }) if p == want)
    }

    /// A C identifier (a macro's or a parameter's name).
    fn c_name(&mut self, arg: Arg) -> Result<&'t str, GrammarError> {
        let (text, tok) = self.ident(arg)?;
        match is_c_identifier(text) {
            true => Ok(text),
            false => Err(self.error(Some(arg), Some(tok), GrammarFault::Type)),
        }
    }

    /// `*const T`, `*mut T` or a name.
    fn macro_type(&mut self, arg: Arg) -> Result<MacroType<'t>, GrammarError> {
        let (mut depth, mut muts) = (0u8, 0u8);
        loop {
            let star = self.peek();
            if !self.at_punct('*') {
                break;
            }
            self.take();
            if depth == MOST_POINTERS {
                return Err(self.error(Some(arg), star, GrammarFault::Type));
            }
            match self.take() {
                Some(Tok { kind: TokKind::Ident("const"), .. }) => {}
                Some(Tok { kind: TokKind::Ident("mut"), .. }) => muts |= 1 << depth,
                other => return Err(self.error(Some(arg), other, GrammarFault::Expected(Expected::Type))),
            }
            depth += 1;
        }
        let (name, tok) = match self.take() {
            Some(tok @ Tok { kind: TokKind::Ident(text), .. }) => (text, tok),
            other => return Err(self.error(Some(arg), other, GrammarFault::Expected(Expected::Type))),
        };
        if !is_c_identifier(name) || (name == "c_void" && depth == 0) {
            return Err(self.error(Some(arg), Some(tok), GrammarFault::Type));
        }
        Ok(MacroType { name, depth, muts })
    }
}

/// The request `toks` spell, in `alloc`. `end` is where the call's text ends (for an error at
/// the end of the arguments).
pub fn parse<'r>(toks: &[Tok<'r>], end: u32, alloc: Alloc<'r>) -> Result<Request<'r>, GrammarError> {
    let mut c = Cursor { toks, next: 0, end };
    let mut module: Option<&'r str> = None;
    let mut link: Option<&'r str> = None;
    let mut headers: AVec<'r, RelPath<'r>> = Vec::new_in(alloc);
    let mut include: AVec<'r, RelPath<'r>> = Vec::new_in(alloc);
    let mut defines: AVec<'r, (Define<'r>, Tok<'r>)> = Vec::new_in(alloc);
    let mut allow: AVec<'r, Pattern<'r>> = Vec::new_in(alloc);
    let mut block: AVec<'r, Pattern<'r>> = Vec::new_in(alloc);
    let mut inline_bodies: Option<bool> = None;
    // Each `macro_fn` with its parameters, as written; laid out sorted at the end.
    let mut macro_fns: AVec<'r, (MacroFn<'r>, Tok<'r>)> = Vec::new_in(alloc);
    let mut written_params: AVec<'r, MacroParam<'r>> = Vec::new_in(alloc);
    let mut count = 0usize;
    while let Some(word) = c.take() {
        count += 1;
        if count > MOST_ARGUMENTS {
            return Err(c.error(None, Some(word), GrammarFault::TooMany));
        }
        let TokKind::Ident(name) = word.kind else {
            return Err(c.error(None, Some(word), GrammarFault::Expected(Expected::Argument)));
        };
        let arg = match name {
            "mod" => Arg::Mod,
            "header" => Arg::Header,
            "include" => Arg::Include,
            "define" => Arg::Define,
            "allow" => Arg::Allow,
            "block" => Arg::Block,
            "link" => Arg::Link,
            "inline_bodies" => Arg::InlineBodies,
            "macro_fn" => Arg::MacroFn,
            _ => return Err(c.error(None, Some(word), GrammarFault::UnknownArgument)),
        };
        match arg {
            Arg::Mod => {
                let (text, tok) = c.ident(arg)?;
                if module.is_some() {
                    return Err(c.error(Some(arg), Some(word), GrammarFault::Repeated));
                }
                let bare = text.strip_prefix("r#").unwrap_or(text);
                if bare.is_empty() || bare.as_bytes()[0].is_ascii_digit() || !bare.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    return Err(c.error(Some(arg), Some(tok), GrammarFault::Name));
                }
                module = Some(text);
            }
            Arg::Header => headers.push(c.path(arg)?),
            Arg::Include => {
                let dir = c.path(arg)?;
                if !include.contains(&dir) {
                    include.push(dir);
                }
            }
            Arg::Define => {
                let (name, tok) = c.ident(arg)?;
                if !is_c_identifier(name) {
                    return Err(c.error(Some(arg), Some(tok), GrammarFault::Name));
                }
                let value = match c.peek() {
                    Some(Tok { kind: TokKind::Punct('='), .. }) => {
                        c.take();
                        let negative = matches!(c.peek(), Some(Tok { kind: TokKind::Punct('-'), .. }));
                        if negative {
                            c.take();
                        }
                        match (c.take(), negative) {
                            (Some(Tok { kind: TokKind::Int(text), .. }), _) => Some(DefineValue { negative, text }),
                            (Some(Tok { kind: TokKind::Ident(text) | TokKind::Str(text), .. }), false) => {
                                Some(DefineValue { negative, text })
                            }
                            (other, _) => return Err(c.error(Some(arg), other, GrammarFault::Expected(Expected::Value))),
                        }
                    }
                    _ => None,
                };
                if defines.iter().any(|(d, _)| d.name == name) {
                    return Err(c.error(Some(arg), Some(tok), GrammarFault::DefinedTwice));
                }
                defines.push((Define { name, value }, tok));
            }
            Arg::Allow => allow.push(c.pattern(arg)?),
            Arg::Block => block.push(c.pattern(arg)?),
            Arg::Link => {
                let (text, tok) = c.string(arg)?;
                if link.is_some() {
                    return Err(c.error(Some(arg), Some(word), GrammarFault::Repeated));
                }
                if text.is_empty() || text.bytes().any(|b| b == b'/' || b == b'\\' || b == 0 || b == b'"') {
                    return Err(c.error(Some(arg), Some(tok), GrammarFault::Library));
                }
                link = Some(text);
            }
            Arg::InlineBodies => {
                let on = match c.take() {
                    Some(Tok { kind: TokKind::Ident("on"), .. }) => true,
                    Some(Tok { kind: TokKind::Ident("off"), .. }) => false,
                    other => return Err(c.error(Some(arg), other, GrammarFault::Expected(Expected::Switch))),
                };
                if inline_bodies.is_some() {
                    return Err(c.error(Some(arg), Some(word), GrammarFault::Repeated));
                }
                inline_bodies = Some(on);
            }
            Arg::MacroFn => {
                let name_tok = c.peek();
                let name = c.c_name(arg)?;
                c.punct(arg, '(')?;
                let params_start = narrow_count(written_params.len(), &c, arg, name_tok)?;
                while !c.at_punct(')') {
                    // Parameters count against the arguments' bound too (rule 18).
                    count += 1;
                    if count > MOST_ARGUMENTS {
                        return Err(c.error(Some(arg), name_tok, GrammarFault::TooMany));
                    }
                    let param = c.c_name(arg)?;
                    c.punct(arg, ':')?;
                    let ty = c.macro_type(arg)?;
                    written_params.push(MacroParam { name: param, ty });
                    if !c.at_punct(')') {
                        c.punct(arg, ',')?;
                    }
                }
                c.punct(arg, ')')?;
                let result = match c.at_punct('-') {
                    true => {
                        c.take();
                        c.punct(arg, '>')?;
                        Some(c.macro_type(arg)?)
                    }
                    false => None,
                };
                let params_len = narrow_count(written_params.len(), &c, arg, name_tok)? - params_start;
                let tok = name_tok.expect("a name was taken");
                if macro_fns.iter().any(|(f, _)| f.name == name) {
                    return Err(c.error(Some(arg), Some(tok), GrammarFault::DefinedTwice));
                }
                macro_fns.push((MacroFn { name, params_start, params_len, result }, tok));
            }
        }
        c.semi(arg)?;
    }
    let Some(module) = module else {
        return Err(c.error(Some(Arg::Mod), None, GrammarFault::Missing));
    };
    if headers.is_empty() {
        return Err(c.error(Some(Arg::Header), None, GrammarFault::Missing));
    }
    defines.sort_unstable_by(|x, y| x.0.name.cmp(y.0.name));
    let mut sorted: AVec<'r, Define<'r>> = Vec::with_capacity_in(defines.len(), alloc);
    for (define, _) in defines.iter() {
        sorted.push(*define);
    }
    for list in [&mut allow, &mut block] {
        list.sort_unstable();
        list.dedup();
    }
    // `macro_fn`s by name (names are unique), each one's parameters laid out in that order.
    macro_fns.sort_unstable_by(|x, y| x.0.name.cmp(y.0.name));
    let mut fns: AVec<'r, MacroFn<'r>> = Vec::with_capacity_in(macro_fns.len(), alloc);
    let mut macro_params: AVec<'r, MacroParam<'r>> = Vec::with_capacity_in(written_params.len(), alloc);
    for (f, tok) in macro_fns.iter() {
        let start = narrow_count(macro_params.len(), &c, Arg::MacroFn, Some(*tok))?;
        let first = crate::at(f.params_start);
        macro_params.extend_from_slice(&written_params[first..first + crate::at(f.params_len)]);
        fns.push(MacroFn { params_start: start, ..*f });
    }
    Ok(Request {
        module,
        headers,
        include,
        defines: sorted,
        allow,
        block,
        link,
        inline_bodies: inline_bodies.unwrap_or(true),
        macro_fns: fns,
        macro_params,
    })
}

/// `n` as a row number of the request, or the fault at `tok`.
fn narrow_count<'t>(n: usize, c: &Cursor<'_, 't>, arg: Arg, tok: Option<Tok<'t>>) -> Result<u32, GrammarError> {
    crate::narrow(n).ok_or_else(|| c.error(Some(arg), tok, GrammarFault::TooMany))
}
