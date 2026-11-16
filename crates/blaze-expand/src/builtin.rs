//! The builtin fn-like macros `println!` and its kin need, written here on the portable form.
//!
//! The eager ones (`concat!`, `concat_bytes!`, `env!`, `option_env!`, `include!`,
//! `include_str!`, `include_bytes!`, `compile_error!`, and the `format_args!` family's template) first list the macro calls in
//! their input as [`Expanded::NeedsExpanded`](crate::Expanded::NeedsExpanded); re-entered with
//! the answers, they read the input with each call replaced by its answer, whose tokens take
//! the origin of the call's path.

use blaze_own::{AVec, Alloc, Vec};

use blaze_id::ContentId;

use crate::{
    call::Inside,
    cx::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, Place, PlaceOf},
    def::BuiltinFn,
    error::{ExpandError, Stream, Unbuilt},
    expand::Out,
    expansion::{
        Diagnostic, Hole, HoleOf, IncludeFailure, NestedCall, NestedKind, TokenOrigin, ReadAnswer,
        ReadKind,
    },
    literal::{self, Buf, LitError},
    nested,
    tokens::{Delim, LitKind, Raw, Spacing, TokenKind, TokenStream, Transparency, index},
};

/// What a builtin did: its expansion is in `out`, or it needs its nested calls expanded.
pub(crate) enum Step<'a> {
    Done,
    Needs(AVec<'a, NestedCall>),
}

/// The tokens builtins make: call-site names, def-site `$crate` (rustc's `mixed_site`).
const MADE: Transparency = Transparency::SemiOpaque;

pub(crate) fn expand<'a>(
    which: BuiltinFn,
    out: &mut Out<'_, 'a>,
    cx: &impl ExpandCx,
) -> Result<Step<'a>, ExpandError> {
    match which {
        BuiltinFn::Concat
        | BuiltinFn::ConcatBytes
        | BuiltinFn::Env
        | BuiltinFn::OptionEnv
        | BuiltinFn::Include
        | BuiltinFn::IncludeStr
        | BuiltinFn::IncludeBytes
        | BuiltinFn::CompileError
        | BuiltinFn::FormatArgs
        | BuiltinFn::FormatArgsNl
        | BuiltinFn::ConstFormatArgs => return eager(which, out, cx),
        BuiltinFn::Stringify => stringify(out)?,
        BuiltinFn::Cfg => cfg(out, cx)?,
        BuiltinFn::CfgSelect => cfg_select(out, cx)?,
        BuiltinFn::Assert => assert(out)?,
        BuiltinFn::Line => place(out, cx, HoleOf::Line)?,
        BuiltinFn::Column => place(out, cx, HoleOf::Column)?,
        BuiltinFn::File => place(out, cx, HoleOf::File)?,
        BuiltinFn::ModulePath => place(out, cx, HoleOf::ModulePath)?,
        BuiltinFn::Panic => panic(out, "panic")?,
        BuiltinFn::Unreachable => panic(out, "unreachable")?,
        BuiltinFn::PatternType => pattern_type(out)?,
        BuiltinFn::Asm => asm(out, "asm")?,
        BuiltinFn::GlobalAsm => asm(out, "global_asm")?,
        BuiltinFn::NakedAsm => asm(out, "naked_asm")?,
        BuiltinFn::TraceMacros | BuiltinFn::LogSyntax => {
            return Err(ExpandError::NotBuilt { what: Unbuilt::BuiltinFn(which) });
        }
    }
    Ok(Step::Done)
}

/// A builtin's input: a stream and, when nested calls were replaced by their answers, each
/// token's origin.
struct Args<'s> {
    stream: &'s TokenStream<'s>,
    from: Option<&'s [TokenOrigin]>,
}

impl Args<'_> {
    fn origin(&self, at: usize) -> TokenOrigin {
        match self.from {
            Some(from) => from[at],
            None => TokenOrigin::Input(index(at)),
        }
    }

    /// The comma-separated arguments, each without the invisible groups around it; a trailing
    /// comma ends the list.
    fn split<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, core::ops::Range<usize>> {
        self.split_in(0..self.stream.len(), ',', alloc)
    }

    /// The pieces of the tokens `range` between the `sep`s outside any group, each without
    /// the invisible groups around it; a trailing `sep` ends the list.
    fn split_in<'b>(
        &self,
        range: core::ops::Range<usize>,
        sep: char,
        alloc: Alloc<'b>,
    ) -> AVec<'b, core::ops::Range<usize>> {
        let tokens = self.stream.tokens();
        let mut pieces = Vec::new_in(alloc);
        let mut depth = 0u32;
        let mut start = range.start;
        for at in range.clone() {
            match tokens[at].kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) => depth -= 1,
                TokenKind::Punct { char, .. } if char == sep && depth == 0 => {
                    pieces.push(self.unwrap(start..at));
                    start = at + 1;
                }
                _ => {}
            }
        }
        if start < range.end {
            pieces.push(self.unwrap(start..range.end));
        }
        pieces
    }

    /// The inside of the `[..]` group that is all of `range`.
    fn bracketed(&self, range: &core::ops::Range<usize>) -> Option<core::ops::Range<usize>> {
        let tokens = self.stream.tokens();
        let whole = range.len() >= 2
            && tokens[range.start].kind == TokenKind::Open(Delim::Bracket)
            && self.stream.close_of(range.start) == Some(range.end - 1);
        whole.then(|| range.start + 1..range.end - 1)
    }

    /// `range` without the invisible groups that wrap all of it.
    fn unwrap(&self, mut range: core::ops::Range<usize>) -> core::ops::Range<usize> {
        let tokens = self.stream.tokens();
        while range.len() >= 2
            && tokens[range.start].kind == TokenKind::Open(Delim::Invisible)
            && self.stream.close_of(range.start) == Some(range.end - 1)
        {
            range = range.start + 1..range.end - 1;
        }
        range
    }

    /// The end of the first argument.
    fn first_end(&self) -> usize {
        let mut depth = 0u32;
        for (at, token) in self.stream.tokens().iter().enumerate() {
            match token.kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) => depth -= 1,
                TokenKind::Punct { char: ',', .. } if depth == 0 => return at,
                _ => {}
            }
        }
        self.stream.len()
    }

    /// Where the expression that starts the input ends (rustc's `parse_expr`, as
    /// `expand_assert` reads its condition): a comma in `f::<A, B>()` is in no delimiter but
    /// ends no argument. `None` when the input does not start with an expression, or it is
    /// followed by something other than a `,`.
    fn expr_end(&self, alloc: Alloc<'_>) -> Option<usize> {
        let words = crate::body::parser_words(self.stream, alloc);
        let output = parser::PrefixEntryPoint::Expr.parse(parser::Tokens::new(&words.words, true));
        if output.iter().any(|step| matches!(step, parser::Step::Error { .. })) {
            return None;
        }
        let taken = crate::rules::consumed(&output);
        let len = self.stream.len();
        let tokens = self.stream.tokens();
        let mut end = (0..len).find(|&at| words.word_at[at] as usize >= taken).unwrap_or(len);
        // An invisible group's delimiters start no word: the close of a `$e:expr` that held
        // the expression belongs to it.
        while tokens.get(end).is_some_and(|t| t.kind == TokenKind::Close(Delim::Invisible)) {
            end += 1;
        }
        match tokens.get(end).map(|t| t.kind) {
            None | Some(TokenKind::Punct { char: ',', .. }) => Some(end),
            Some(_) => None,
        }
    }

    /// What one argument is, for the builtins that take literals.
    fn atom(&self, range: core::ops::Range<usize>) -> Atom<'_> {
        let tokens = &self.stream.tokens()[range.clone()];
        let literal = |at: usize, negative: bool| match self.stream.tokens()[at].kind {
            TokenKind::Literal { kind, suffix_len, .. } => {
                Atom::Lit { kind, text: self.stream.text_at(at), suffix_len, negative, at }
            }
            _ => Atom::Other { at },
        };
        match tokens {
            [token] => match token.kind {
                TokenKind::Literal { .. } => literal(range.start, false),
                TokenKind::Ident { raw: Raw::No, .. } => match self.stream.text_of(token) {
                    "true" => Atom::Bool(true),
                    "false" => Atom::Bool(false),
                    _ => Atom::Other { at: range.start },
                },
                _ => Atom::Other { at: range.start },
            },
            [minus, number]
                if matches!(minus.kind, TokenKind::Punct { char: '-', .. })
                    && matches!(
                        number.kind,
                        TokenKind::Literal { kind: LitKind::Integer | LitKind::Float, .. }
                    ) =>
            {
                literal(range.start + 1, true)
            }
            _ => Atom::Other { at: range.start },
        }
    }
}

enum Atom<'s> {
    Lit {
        kind: LitKind,
        text: &'s str,
        suffix_len: u32,
        negative: bool,
        at: usize,
    },
    Bool(bool),
    /// Anything else, starting at `at` (the end of the input for an empty argument).
    Other {
        at: usize,
    },
}

/// An eager builtin: its nested calls first, then the builtin on the input with the answers.
fn eager<'a>(
    which: BuiltinFn,
    out: &mut Out<'_, 'a>,
    cx: &impl ExpandCx,
) -> Result<Step<'a>, ExpandError> {
    let call = out.call;
    let input = call.input();
    let alloc = out.alloc;
    let mut calls = nested::find_calls(input, alloc)?;
    calls.retain(|it| it.kind == NestedKind::FnLike);
    if matches!(which, BuiltinFn::FormatArgs | BuiltinFn::FormatArgsNl | BuiltinFn::ConstFormatArgs)
    {
        // Only the template is expanded first (`format_args!(concat!(..), x)`).
        let first = Args { stream: input, from: None }.first_end();
        calls.retain(|it| it.args.end as usize <= first);
    }
    let answers = call.eager.answers;
    let given = index(answers.len());
    if calls.is_empty() {
        if given != 0 {
            return Err(ExpandError::EagerAnswers { wanted: 0, given });
        }
        run_eager(which, &Args { stream: input, from: None }, out, cx)?;
        return Ok(Step::Done);
    }
    if answers.is_empty() {
        return Ok(Step::Needs(calls));
    }
    let wanted = index(calls.len());
    if wanted != given {
        return Err(ExpandError::EagerAnswers { wanted, given });
    }
    let mut stream = TokenStream::new_in(alloc);
    let mut from = Vec::new_in(alloc);
    let mut at = 0;
    for (n, (nested, answer)) in calls.iter().zip(answers.iter()).enumerate() {
        while at < nested.path.start as usize {
            stream.push_from(input, at)?;
            from.push(TokenOrigin::Input(index(at)));
            at += 1;
        }
        let in_answer =
            |error| ExpandError::Stream { stream: Some(Stream::Answer(index(n))), error };
        for token in 0..answer.len() {
            stream.push_from(*answer, token).map_err(in_answer)?;
            from.push(TokenOrigin::Input(nested.path.start));
        }
        at = nested.args.end as usize;
    }
    while at < input.len() {
        stream.push_from(input, at)?;
        from.push(TokenOrigin::Input(index(at)));
        at += 1;
    }
    run_eager(which, &Args { stream: &stream, from: Some(&from) }, out, cx)?;
    Ok(Step::Done)
}

fn run_eager(
    which: BuiltinFn,
    args: &Args<'_>,
    out: &mut Out<'_, '_>,
    cx: &impl ExpandCx,
) -> Result<(), ExpandError> {
    match which {
        BuiltinFn::Concat => concat(args, out),
        BuiltinFn::ConcatBytes => concat_bytes(args, out),
        BuiltinFn::Env => env(args, out, cx, Optional::No),
        BuiltinFn::OptionEnv => env(args, out, cx, Optional::Yes),
        BuiltinFn::Include => include(args, out, cx, Inclusion::Tokens),
        BuiltinFn::IncludeStr => include(args, out, cx, Inclusion::Str),
        BuiltinFn::IncludeBytes => include(args, out, cx, Inclusion::Bytes),
        BuiltinFn::CompileError => compile_error(args, out),
        BuiltinFn::FormatArgs | BuiltinFn::ConstFormatArgs => format_args(args, out, Newline::No),
        BuiltinFn::FormatArgsNl => format_args(args, out, Newline::Yes),
        _ => unreachable!("`expand` sends only the eager builtins to `eager`"),
    }
}

/// A builtin's own error about its arguments.
fn refuse(
    out: &mut Out<'_, '_>,
    message: &str,
    at: Option<TokenOrigin>,
) -> Result<(), ExpandError> {
    let message = out.note(message)?;
    out.diagnose(Diagnostic::Builtin { message, at });
    Ok(())
}

/// Where argument token `at` is, or the end of the input.
fn at_token(args: &Args<'_>, at: usize) -> Option<TokenOrigin> {
    (at < args.stream.len()).then(|| args.origin(at))
}

/// The value of a string literal argument, into `value`.
fn string_arg(
    args: &Args<'_>,
    range: core::ops::Range<usize>,
    value: &mut Buf<'_>,
) -> Result<(), (LitError, Option<TokenOrigin>)> {
    match args.atom(range) {
        Atom::Lit { kind, text, suffix_len, negative: false, at } => {
            literal::string_value(kind, text, suffix_len, value)
                .map_err(|it| (it, Some(args.origin(at))))
        }
        Atom::Lit { at, .. } | Atom::Other { at } => {
            Err((LitError::NotAString, at_token(args, at)))
        }
        Atom::Bool(_) => Err((LitError::NotAString, None)),
    }
}

/// Pushes a made string literal whose value is `value`.
fn push_str_literal(
    out: &mut Out<'_, '_>,
    value: &str,
) -> Result<(), ExpandError> {
    let mut text = Buf::new_in(out.alloc);
    literal::escape_str(value, &mut text);
    let kind = TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone };
    out.made(kind, text.as_str(), MADE)
}

fn ident(out: &mut Out<'_, '_>, name: &str) -> Result<(), ExpandError> {
    out.made(TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone }, name, MADE)
}

fn punct(
    out: &mut Out<'_, '_>,
    char: char,
    spacing: Spacing,
) -> Result<(), ExpandError> {
    out.made(TokenKind::Punct { char, spacing }, "", MADE)
}

/// `::name` for each name, after `$crate`.
fn crate_path(
    out: &mut Out<'_, '_>,
    names: &[&str],
) -> Result<(), ExpandError> {
    ident(out, "$crate")?;
    for name in names {
        punct(out, ':', Spacing::Joint)?;
        punct(out, ':', Spacing::Alone)?;
        ident(out, name)?;
    }
    Ok(())
}

fn concat(
    args: &Args<'_>,
    out: &mut Out<'_, '_>,
) -> Result<(), ExpandError> {
    let mut value = Buf::new_in(out.alloc);
    for range in args.split(out.alloc) {
        match args.atom(range) {
            Atom::Lit { kind, text, suffix_len, negative, at } => {
                if let Err(error) =
                    literal::concat_piece(kind, text, suffix_len, negative, &mut value)
                {
                    return refuse(out, error.message(), Some(args.origin(at)));
                }
            }
            Atom::Bool(value_of) => value.push_str(if value_of { "true" } else { "false" }),
            Atom::Other { at } => {
                return refuse(out, "expected a literal", at_token(args, at));
            }
        }
    }
    push_str_literal(out, value.as_str())
}

/// `concat_bytes!`: byte and byte string literals, and arrays (`[1, b'a']`, `[b'a'; 3]`) of
/// byte and `u8` literals, as one byte string literal (rustc's `expand_concat_bytes`).
fn concat_bytes(
    args: &Args<'_>,
    out: &mut Out<'_, '_>,
) -> Result<(), ExpandError> {
    let mut value = Vec::new_in(out.alloc);
    for range in args.split(out.alloc) {
        if let Some(inside) = args.bracketed(&range) {
            if let Err((message, at)) = bytes_array(args, inside, &mut value, out.alloc) {
                return refuse(out, message, at);
            }
            continue;
        }
        match args.atom(range) {
            Atom::Lit { kind, text, suffix_len, negative: false, at } => {
                if let Err(error) = literal::bytes_piece(kind, text, suffix_len, &mut value) {
                    return refuse(out, error.message(), Some(args.origin(at)));
                }
            }
            Atom::Lit { at, .. } | Atom::Other { at } => {
                return refuse(out, "expected a byte literal", at_token(args, at));
            }
            Atom::Bool(_) => return refuse(out, "cannot concatenate a boolean literal", None),
        }
    }
    // The literal's text is a table entry: at most `u32::MAX` bytes, and escapes widen it.
    u32::try_from(value.len().saturating_mul(4))
        .map_err(|_| crate::error::StreamError::TooLarge { table: crate::error::Table::Text })?;
    let mut text = Buf::new_in(out.alloc);
    literal::escape_bytes(&value, &mut text);
    let kind =
        TokenKind::Literal { kind: LitKind::ByteStr, suffix_len: 0, spacing: Spacing::Alone };
    out.made(kind, text.as_str(), MADE)
}

/// The bytes of a `concat_bytes!` array whose inside is `inside`, appended to `value`; or the
/// refusal and where.
fn bytes_array(
    args: &Args<'_>,
    inside: core::ops::Range<usize>,
    value: &mut AVec<'_, u8>,
    alloc: Alloc<'_>,
) -> Result<(), (&'static str, Option<TokenOrigin>)> {
    let element = |range: core::ops::Range<usize>| -> Result<u8, (&'static str, Option<TokenOrigin>)> {
        if args.bracketed(&range).is_some() {
            return Err(("cannot concatenate a doubly nested array", at_token(args, range.start)));
        }
        match args.atom(range) {
            Atom::Lit { kind, text, suffix_len, negative: false, at } => {
                literal::byte_element(kind, text, suffix_len)
                    .map_err(|error| (error.message(), Some(args.origin(at))))
            }
            Atom::Lit { at, .. } | Atom::Other { at } => {
                Err(("expected a byte or a `u8` literal", at_token(args, at)))
            }
            Atom::Bool(_) => Err(("cannot concatenate a boolean literal", None)),
        }
    };
    let repeat = args.split_in(inside.clone(), ';', alloc);
    if let [byte, count] = &repeat[..] {
        let byte = element(byte.clone())?;
        let count = match args.atom(count.clone()) {
            Atom::Lit { kind: LitKind::Integer, text, suffix_len, negative: false, .. } => {
                literal::integer_of(text, suffix_len)
            }
            _ => None,
        };
        let too_large = ("the repeated array is too large", at_token(args, inside.start));
        let count = count.ok_or(("the repeat count is not an integer literal", None))?;
        let count = usize::try_from(count).map_err(|_| too_large)?;
        // A byte string is kept under `u32::MAX` bytes, as every text of a stream.
        let len = value.len().checked_add(count).filter(|&len| u32::try_from(len).is_ok());
        len.ok_or(too_large)?;
        value.extend(core::iter::repeat_n(byte, count));
        return Ok(());
    }
    for range in args.split_in(inside, ',', alloc) {
        value.push(element(range)?);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Optional {
    No,
    Yes,
}

fn env(
    args: &Args<'_>,
    out: &mut Out<'_, '_>,
    cx: &impl ExpandCx,
    optional: Optional,
) -> Result<(), ExpandError> {
    let pieces = args.split(out.alloc);
    let most = if optional == Optional::Yes { 1 } else { 2 };
    if pieces.is_empty() || pieces.len() > most {
        let message = if optional == Optional::Yes {
            "`option_env!` takes 1 argument"
        } else {
            "`env!` takes 1 or 2 arguments"
        };
        return refuse(out, message, None);
    }
    let mut name = Buf::new_in(out.alloc);
    if let Err((error, at)) = string_arg(args, pieces[0].clone(), &mut name) {
        return refuse(out, error.message(), at);
    }
    let mut message = Buf::new_in(out.alloc);
    let has_message = pieces.len() == 2;
    if has_message && let Err((error, at)) = string_arg(args, pieces[1].clone(), &mut message) {
        return refuse(out, error.message(), at);
    }
    let value = cx.env(name.as_str())?;
    out.exp.reads.push(ReadKind::Env, name.as_str(), ReadAnswer::of_env(value))?;
    match (value, optional) {
        (Some(value), Optional::No) => push_str_literal(out, value),
        (None, Optional::No) => {
            let name = out.note(name.as_str())?;
            let message = if has_message { Some(out.note(message.as_str())?) } else { None };
            out.diagnose(Diagnostic::EnvNotDefined { name, message });
            Ok(())
        }
        // `$crate::option::Option::Some::<&'static str>("value")`, or `None::<..>`.
        (value, Optional::Yes) => {
            crate_path(out, &["option", "Option", if value.is_some() { "Some" } else { "None" }])?;
            punct(out, ':', Spacing::Joint)?;
            punct(out, ':', Spacing::Alone)?;
            punct(out, '<', Spacing::Alone)?;
            // `&` is not joined to the lifetime after it: `&'` is no operator.
            punct(out, '&', Spacing::Alone)?;
            punct(out, '\'', Spacing::Joint)?;
            ident(out, "static")?;
            ident(out, "str")?;
            punct(out, '>', Spacing::Alone)?;
            if let Some(value) = value {
                out.made(TokenKind::Open(Delim::Paren), "", MADE)?;
                push_str_literal(out, value)?;
                out.made(TokenKind::Close(Delim::Paren), "", MADE)?;
            }
            Ok(())
        }
    }
}

/// What an include makes of its file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Inclusion {
    /// `include!`: the file's tokens.
    Tokens,
    /// `include_str!`: a string literal.
    Str,
    /// `include_bytes!`: a byte string literal.
    Bytes,
}

fn include(
    args: &Args<'_>,
    out: &mut Out<'_, '_>,
    cx: &impl ExpandCx,
    inclusion: Inclusion,
) -> Result<(), ExpandError> {
    let pieces = args.split(out.alloc);
    if pieces.len() != 1 {
        return refuse(out, "expected one string literal: the path", None);
    }
    let mut path = Buf::new_in(out.alloc);
    if let Err((error, at)) = string_arg(args, pieces[0].clone(), &mut path) {
        return refuse(out, error.message(), at);
    }
    // `include!` reads the file as text, the same fact `include_str!` reads.
    let (kind, how) = match inclusion {
        Inclusion::Tokens | Inclusion::Str => (ReadKind::IncludeStr, IncludeAs::Str),
        Inclusion::Bytes => (ReadKind::IncludeBytes, IncludeAs::Bytes),
    };
    // The read list's subject: the root-relative path a host with a read root answers (D60),
    // else the path as written. A host's root-relative path that is not one is refused, never
    // recorded: it would put the machine's folder or a spelling into the record.
    let (subject, found, rooted) = match cx.include(path.as_str(), how)? {
        Included::Bytes(bytes) => (path.as_str(), Some(bytes), false),
        Included::Missing => (path.as_str(), None, false),
        Included::At { path: at, bytes } => (at, Some(bytes), true),
        Included::MissingAt { path: at } => (at, None, true),
        Included::Outside => {
            let path = out.note(path.as_str())?;
            out.diagnose(Diagnostic::Include { path, why: IncludeFailure::Outside });
            return Ok(());
        }
    };
    if rooted && !crate::cx::is_root_relative(subject) {
        return Err(HostError { ask: HostAsk::Include, why: HostWhy::Unreadable }.into());
    }
    let Some(bytes) = found else {
        out.exp.reads.push(kind, subject, ReadAnswer::Absent)?;
        let path = out.note(path.as_str())?;
        out.diagnose(Diagnostic::Include { path, why: IncludeFailure::Missing });
        return Ok(());
    };
    let digest = ContentId::of("blaze v2 expand included file 1", bytes);
    out.exp.reads.push(kind, subject, ReadAnswer::Digest(digest))?;
    let not_utf8 = |out: &mut Out<'_, '_>| -> Result<(), ExpandError> {
        let path = out.note(path.as_str())?;
        out.diagnose(Diagnostic::Include { path, why: IncludeFailure::NotUtf8 });
        Ok(())
    };
    match inclusion {
        Inclusion::Str => match core::str::from_utf8(bytes) {
            Ok(text) => push_str_literal(out, text),
            Err(_) => not_utf8(out),
        },
        Inclusion::Tokens => match core::str::from_utf8(bytes) {
            Ok(text) => included_tokens(out, path.as_str(), text),
            Err(_) => not_utf8(out),
        },
        Inclusion::Bytes => {
            let mut text = Buf::new_in(out.alloc);
            literal::escape_bytes(bytes, &mut text);
            let kind = TokenKind::Literal {
                kind: LitKind::ByteStr,
                suffix_len: 0,
                spacing: Spacing::Alone,
            };
            out.made(kind, text.as_str(), MADE)
        }
    }
}

/// `include!`'s tokens: the file's text without a byte order mark or a `#!` line, lexed in the
/// call's edition, each token made at the call site ([`Transparency::Transparent`]): rustc
/// parses the file with its own spans, which no expansion marks, so its names resolve where
/// the call is.
fn included_tokens(
    out: &mut Out<'_, '_>,
    path: &str,
    text: &str,
) -> Result<(), ExpandError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let text = match text.strip_prefix("#!") {
        // `#![attr]` is an inner attribute, not a shebang.
        Some(rest) if !rest.trim_start().starts_with('[') => {
            rest.find('\n').map_or("", |at| &rest[at..])
        }
        _ => text,
    };
    let tokens = match crate::tokens::lex(text, out.call.site.edition, out.alloc) {
        Ok(tokens) => tokens,
        Err(crate::error::LexError::Refused { .. }) => {
            let path = out.note(path)?;
            out.diagnose(Diagnostic::Include { path, why: IncludeFailure::DoesNotLex });
            return Ok(());
        }
        Err(crate::error::LexError::Stream(error)) => return Err(error.into()),
    };
    for token in tokens.tokens() {
        out.made(token.kind, tokens.text_of(token), Transparency::Transparent)?;
    }
    Ok(())
}

/// `assert!(cond)` as `if !(cond) { $crate::panicking::panic("assertion failed: cond") }`, and
/// `assert!(cond, args..)` as `if !(cond) { $crate::panic::panic_2021!(args..) }` (or
/// `panic_2015!` by the call's panic edition), as rustc's `expand_assert`.
fn assert(out: &mut Out<'_, '_>) -> Result<(), ExpandError> {
    let input = out.call.input();
    let args = Args { stream: input, from: None };
    let Some(end) = args.expr_end(out.alloc) else {
        return refuse(out, "`assert!` takes a boolean expression, then optionally `,` and a message", None);
    };
    let cond = args.unwrap(0..end);
    if cond.is_empty() {
        return refuse(out, "`assert!` takes a boolean expression", None);
    }
    // `assert!(cond,)` has no message.
    let message = (end + 1 < input.len()).then(|| end + 1..input.len());
    ident(out, "if")?;
    punct(out, '!', Spacing::Alone)?;
    out.made(TokenKind::Open(Delim::Paren), "", MADE)?;
    for at in cond.clone() {
        let token = input.tokens()[at];
        out.push(token.kind, input.text_of(&token), TokenOrigin::Input(index(at)))?;
    }
    out.made(TokenKind::Close(Delim::Paren), "", MADE)?;
    out.made(TokenKind::Open(Delim::Brace), "", MADE)?;
    match message {
        Some(message) => {
            let which = if out.call.site.panic_edition >= crate::tokens::Edition::E2021 {
                "panic_2021"
            } else {
                "panic_2015"
            };
            crate_path(out, &["panic", which])?;
            punct(out, '!', Spacing::Alone)?;
            out.made(TokenKind::Open(Delim::Paren), "", MADE)?;
            for at in message {
                let token = input.tokens()[at];
                out.push(token.kind, input.text_of(&token), TokenOrigin::Input(index(at)))?;
            }
            out.made(TokenKind::Close(Delim::Paren), "", MADE)?;
        }
        None => {
            let cond = input.copy_in(cond, out.alloc)?;
            let mut text = Buf::new_in(out.alloc);
            text.push_str("assertion failed: ");
            stringified(&cond, out.alloc, &mut text)?;
            crate_path(out, &["panicking", "panic"])?;
            out.made(TokenKind::Open(Delim::Paren), "", MADE)?;
            push_str_literal(out, text.as_str())?;
            out.made(TokenKind::Close(Delim::Paren), "", MADE)?;
        }
    }
    out.made(TokenKind::Close(Delim::Brace), "", MADE)
}

/// `cfg_select! { pred => { tokens } .. _ => expr }`: the tokens inside the braces of the
/// first arm whose predicate holds (`_` always does), or its expression up to its `,`. Every
/// arm is read; the predicates are
/// asked of the context in order up to the first that holds, each outcome recorded.
fn cfg_select(
    out: &mut Out<'_, '_>,
    cx: &impl ExpandCx,
) -> Result<(), ExpandError> {
    let input = out.call.input();
    let tokens = input.tokens();
    let mut chosen = None;
    let mut at = 0;
    while at < tokens.len() {
        let start = at;
        let mut depth = 0u32;
        let arrow = loop {
            let Some(token) = tokens.get(at) else { break None };
            match token.kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) => depth -= 1,
                TokenKind::Punct { char: '=', spacing: Spacing::Joint }
                    if depth == 0
                        && matches!(
                            tokens.get(at + 1).map(|it| it.kind),
                            Some(TokenKind::Punct { char: '>', .. })
                        ) =>
                {
                    break Some(at);
                }
                _ => {}
            }
            at += 1;
        };
        let input_at = |at: usize| (at < tokens.len()).then(|| TokenOrigin::Input(index(at)));
        let Some(arrow) = arrow else {
            let message = "expected `=>` after a `cfg_select!` predicate";
            return refuse(out, message, input_at(start));
        };
        if arrow == start {
            return refuse(out, "expected a cfg predicate or `_` before `=>`", input_at(arrow));
        }
        let open = arrow + 2;
        // An arm is `{ tokens }`, or an expression up to the next `,` outside any group
        // (rustc's arms since `cfg_select!` took expressions).
        let body = match tokens.get(open).map(|it| it.kind) {
            Some(TokenKind::Open(Delim::Brace)) => match input.close_of(open) {
                Some(close) => {
                    at = close + 1;
                    open + 1..close
                }
                None => return refuse(out, "a `{` in `cfg_select!` that does not close", input_at(open)),
            },
            None => return refuse(out, "expected an arm after `=>` in `cfg_select!`", input_at(arrow)),
            Some(_) => {
                let mut depth = 0u32;
                let mut end = open;
                while let Some(token) = tokens.get(end) {
                    match token.kind {
                        TokenKind::Open(_) => depth += 1,
                        TokenKind::Close(_) => depth = depth.saturating_sub(1),
                        TokenKind::Punct { char: ',', .. } if depth == 0 => break,
                        _ => {}
                    }
                    end += 1;
                }
                at = end;
                open..end
            }
        };
        if matches!(tokens.get(at).map(|it| it.kind), Some(TokenKind::Punct { char: ',', .. })) {
            at += 1;
        }
        if chosen.is_some() {
            continue;
        }
        let wildcard = arrow == start + 1
            && match tokens[start].kind {
                TokenKind::Ident { raw: Raw::No, .. } => input.text_at(start) == "_",
                TokenKind::Punct { char: '_', .. } => true,
                _ => false,
            };
        let holds = if wildcard {
            true
        } else {
            let predicate = input.copy_in(start..arrow, out.alloc)?;
            let outcome = cx.cfg(&predicate)?;
            let mut subject = Buf::new_in(out.alloc);
            stringified(&predicate, out.alloc, &mut subject)?;
            out.exp.reads.push(ReadKind::Cfg, subject.as_str(), ReadAnswer::Bool(outcome))?;
            outcome
        };
        if holds {
            chosen = Some(body);
        }
    }
    let Some(body) = chosen else {
        return refuse(out, "none of the predicates in this `cfg_select!` holds", None);
    };
    for at in body {
        let token = tokens[at];
        out.push(token.kind, input.text_of(&token), TokenOrigin::Input(index(at)))?;
    }
    Ok(())
}

fn compile_error(
    args: &Args<'_>,
    out: &mut Out<'_, '_>,
) -> Result<(), ExpandError> {
    let pieces = args.split(out.alloc);
    let mut message = Buf::new_in(out.alloc);
    if pieces.len() != 1 {
        return refuse(out, "`compile_error!` takes 1 string literal", None);
    }
    if let Err((error, at)) = string_arg(args, pieces[0].clone(), &mut message) {
        return refuse(out, error.message(), at);
    }
    let message = out.note(message.as_str())?;
    out.diagnose(Diagnostic::CompileError { message });
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Newline {
    No,
    Yes,
}

/// `builtin # format_args ( args )`, the form the parser reads `format_args!` in; for
/// `format_args_nl!`, the template with `\n` added.
fn format_args(
    args: &Args<'_>,
    out: &mut Out<'_, '_>,
    newline: Newline,
) -> Result<(), ExpandError> {
    let template = if newline == Newline::Yes {
        let first = args.split(out.alloc).first().cloned();
        let Some(first) = first else {
            return refuse(out, "`format_args_nl!` takes a format string", None);
        };
        match args.atom(first.clone()) {
            Atom::Lit {
                kind: kind @ (LitKind::Str | LitKind::StrRaw(_)),
                text,
                suffix_len: 0,
                negative: false,
                at,
            } => {
                let mut with_newline = Buf::new_in(out.alloc);
                match kind {
                    LitKind::Str => with_newline.push_str(text),
                    _ => literal::escape_str(text, &mut with_newline),
                }
                with_newline.push_str("\\n");
                Some((first, at, with_newline))
            }
            Atom::Lit { at, .. } | Atom::Other { at } => {
                return refuse(out, "a format string must be a string literal", at_token(args, at));
            }
            Atom::Bool(_) => return refuse(out, "a format string must be a string literal", None),
        }
    } else {
        None
    };
    ident(out, "builtin")?;
    punct(out, '#', Spacing::Alone)?;
    ident(out, "format_args")?;
    out.made(TokenKind::Open(Delim::Paren), "", MADE)?;
    let mut at = 0;
    while at < args.stream.len() {
        if let Some((range, literal_at, text)) = &template
            && at == range.start
        {
            let kind =
                TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone };
            out.push(kind, text.as_str(), args.origin(*literal_at))?;
            at = range.end;
            continue;
        }
        let token = args.stream.tokens()[at];
        out.push(token.kind, args.stream.text_of(&token), args.origin(at))?;
        at += 1;
    }
    out.made(TokenKind::Close(Delim::Paren), "", MADE)
}

/// `asm!(..)`, `global_asm!(..)` and `naked_asm!(..)` as `builtin # asm ( input )` (and
/// `global_asm`, `naked_asm`): the one typed form of inline assembly is the parser's
/// `ASM_EXPR` (the template pieces, then each operand as `ASM_OPERAND_NAMED` over an
/// `ASM_REG_OPERAND` with its `ASM_DIR_SPEC` (`in`, `out`, `inout`, `lateout`, `inlateout`) and
/// `ASM_REG_SPEC`, or an `ASM_CONST`, `ASM_SYM` or `ASM_LABEL`; `ASM_OPTIONS`;
/// `ASM_CLOBBER_ABI`), so nothing here reads the operands a second time. The input's tokens
/// are kept, each from its place, so an operand's expression lowers and type-checks where it
/// is written. The pinned core (library/core/src/arch.rs): "`pub macro asm("assembly
/// template", $(operands,)* $(options($(option),*))?) { /* compiler built-in */ }`"; rustc
/// refuses a call with no template ("requires at least a template string argument").
fn asm(out: &mut Out<'_, '_>, keyword: &str) -> Result<(), ExpandError> {
    let input = out.call.input();
    if input.is_empty() {
        return refuse(out, "requires at least a template string argument", None);
    }
    ident(out, "builtin")?;
    punct(out, '#', Spacing::Alone)?;
    ident(out, keyword)?;
    out.made(TokenKind::Open(Delim::Paren), "", MADE)?;
    for at in 0..input.len() {
        let token = input.tokens()[at];
        out.push(token.kind, input.text_of(&token), TokenOrigin::Input(index(at)))?;
    }
    out.made(TokenKind::Close(Delim::Paren), "", MADE)
}

/// `stringify!`: the input's text, spaced as rustc spaces it.
/// `builtin # pattern_type ( input )`, the form the parser reads a pattern type in
/// (`T is PAT`; library/core/src/pat.rs: "Creates a pattern type. `type Positive =
/// std::pat::pattern_type!(i32 is 1..);`"): the input's tokens kept, each from its place, so
/// the base and the pattern lower as written and the parser refuses what is no pattern type.
fn pattern_type(out: &mut Out<'_, '_>) -> Result<(), ExpandError> {
    let input = out.call.input();
    if input.is_empty() {
        return refuse(out, "`pattern_type!` takes a type, `is` and a pattern", None);
    }
    ident(out, "builtin")?;
    punct(out, '#', Spacing::Alone)?;
    ident(out, "pattern_type")?;
    out.made(TokenKind::Open(Delim::Paren), "", MADE)?;
    for at in 0..input.len() {
        let token = input.tokens()[at];
        out.push(token.kind, input.text_of(&token), TokenOrigin::Input(index(at)))?;
    }
    out.made(TokenKind::Close(Delim::Paren), "", MADE)
}

fn stringify(out: &mut Out<'_, '_>) -> Result<(), ExpandError> {
    let input = out.call.input();
    let mut text = Buf::new_in(out.alloc);
    stringified(input, out.alloc, &mut text)?;
    push_str_literal(out, text.as_str())
}

/// The text of `stream` appended to `into`, spaced as rustc's `print_tts` spaces it: a space
/// goes after a token the stream has `Alone`, unless it is one of the places rustc never puts
/// one (`space_between`); after a joint one, only between two identifiers. A brace group is
/// written `{ .. }` when its opening brace is `Alone` and it is not empty.
///
/// One pass over the tokens, the trees of each level paired on an explicit stack: the space
/// between two trees is decided when the second one starts.
fn stringified(
    stream: &TokenStream<'_>,
    alloc: Alloc<'_>,
    into: &mut Buf<'_>,
) -> Result<(), ExpandError> {
    if let Some(token) = stream.unbalanced() {
        return Err(ExpandError::Unbalanced { stream: Stream::Input, token });
    }
    let tokens = stream.tokens();
    // Per open group: whether its brace was written spaced. The tree before a group is not
    // needed once the group starts, so it is not kept.
    let mut levels: AVec<'_, bool> = Vec::new_in(alloc);
    let mut prev: Option<Printed> = None;
    for (at, token) in tokens.iter().enumerate() {
        let next = match token.kind {
            TokenKind::Close(delim) => {
                let spaced = levels.pop().expect("the stream was checked balanced");
                into.push_str(match (delim, spaced) {
                    (Delim::Brace, true) => " }",
                    (Delim::Brace, false) => "}",
                    (Delim::Bracket, _) => "]",
                    (Delim::Paren, _) => ")",
                    (Delim::Invisible, _) => "",
                });
                prev = Some(Printed::Group { delim, joint: token.kind.spacing() != Spacing::Alone });
                continue;
            }
            TokenKind::Open(delim) => Printed::Group { delim, joint: false },
            TokenKind::Punct { char, spacing } => Printed::Punct { char, joint: spacing != Spacing::Alone },
            TokenKind::Ident { raw, spacing } => Printed::Ident {
                keyword: raw == Raw::No && KEYWORDS.contains(&stream.text_of(token)),
                joint: spacing != Spacing::Alone,
            },
            TokenKind::Literal { spacing, .. } => Printed::Literal { joint: spacing != Spacing::Alone },
        };
        if let Some(prev) = prev
            && prev.space_before(next)
        {
            into.push_char(' ');
        }
        match token.kind {
            TokenKind::Open(delim) => {
                let empty = matches!(tokens.get(at + 1), Some(t) if matches!(t.kind, TokenKind::Close(_)));
                let spaced = delim == Delim::Brace && !empty && token.kind.spacing() == Spacing::Alone;
                into.push_str(match (delim, spaced) {
                    (Delim::Brace, true) => "{ ",
                    (Delim::Brace, false) => "{",
                    (Delim::Bracket, _) => "[",
                    (Delim::Paren, _) => "(",
                    (Delim::Invisible, _) => "",
                });
                levels.push(spaced);
                prev = None;
                continue;
            }
            TokenKind::Punct { char, .. } => into.push_char(char),
            TokenKind::Ident { raw, .. } => {
                if raw == Raw::Yes {
                    into.push_str("r#");
                }
                into.push_str(stream.text_of(token));
            }
            TokenKind::Literal { kind, suffix_len, .. } => {
                crate::rules::write_literal(into, kind, stream.text_of(token), suffix_len)
                    .expect("writing to a `Buf` does not fail");
            }
            TokenKind::Close(_) => unreachable!("a close is written above"),
        }
        prev = Some(next);
    }
    Ok(())
}

/// What `stringified` needs of the tree before: its kind, and whether rustc's token for its
/// end is joint with what follows (a group's: its closing delimiter's).
#[derive(Clone, Copy)]
enum Printed {
    Punct { char: char, joint: bool },
    Ident { keyword: bool, joint: bool },
    Literal { joint: bool },
    Group { delim: Delim, joint: bool },
}

/// The words `space_between` writes no space between and a parenthesis.
const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "for", "if",
    "impl", "in", "let", "loop", "match", "mod", "move", "mut", "ref", "return", "self", "static",
    "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "async",
    "await", "dyn", "abstract", "become", "box", "do", "final", "macro", "override", "priv",
    "typeof", "unsized", "virtual", "yield", "try", "gen",
];

impl Printed {
    fn punct(self) -> Option<char> {
        match self {
            Printed::Punct { char, .. } => Some(char),
            _ => None,
        }
    }

    /// Whether a space goes between `self` and `next`, the tree after it at its level.
    fn space_before(self, next: Printed) -> bool {
        let joined = match self {
            // A lifetime is one token to rustc.
            Printed::Punct { char: '\'', .. } if matches!(next, Printed::Ident { .. }) => true,
            Printed::Group { delim: Delim::Invisible, .. } => false,
            Printed::Punct { joint, .. }
            | Printed::Ident { joint, .. }
            | Printed::Literal { joint }
            | Printed::Group { joint, .. } => joint,
        };
        if joined {
            // rustc's `idents_would_merge`.
            return matches!((self, next), (Printed::Ident { .. }, Printed::Ident { .. }));
        }
        // rustc's `space_between`.
        match (self, next) {
            _ if self.punct() == Some('.') && next.punct().is_none() => false,
            (_, Printed::Ident { .. }) if self.punct() == Some('$') => false,
            _ if self.punct().is_none() && matches!(next.punct(), Some(',' | ';' | '.')) => false,
            (Printed::Ident { keyword, .. }, Printed::Group { delim: Delim::Paren, .. }) => keyword,
            (_, Printed::Group { delim, .. }) if self.punct() == Some('#') => delim != Delim::Bracket,
            _ => true,
        }
    }
}

/// `cfg!(predicate)`: `true` or `false`, the outcome read.
fn cfg(out: &mut Out<'_, '_>, cx: &impl ExpandCx) -> Result<(), ExpandError> {
    let input = out.call.input();
    if input.is_empty() {
        return refuse(out, "`cfg!` takes a cfg predicate", None);
    }
    let outcome = cx.cfg(input)?;
    let mut subject = Buf::new_in(out.alloc);
    stringified(input, out.alloc, &mut subject)?;
    out.exp.reads.push(ReadKind::Cfg, subject.as_str(), ReadAnswer::Bool(outcome))?;
    ident(out, if outcome { "true" } else { "false" })
}

/// `line!()`, `column!()`, `file!()`, `module_path!()`: a placeholder and a [`Hole`] where the
/// call is written; inside eager input, the value read.
fn place(
    out: &mut Out<'_, '_>,
    cx: &impl ExpandCx,
    of: HoleOf,
) -> Result<(), ExpandError> {
    if !out.call.input().is_empty() {
        return refuse(out, "takes no arguments", Some(TokenOrigin::Input(0)));
    }
    let number = matches!(of, HoleOf::Line | HoleOf::Column);
    if out.call.eager.inside == Inside::Plain {
        let token = index(out.exp.tokens.len());
        if number {
            let kind = TokenKind::Literal {
                kind: LitKind::Integer,
                suffix_len: 3,
                spacing: Spacing::Alone,
            };
            out.made(kind, "0u32", MADE)?;
        } else {
            push_str_literal(out, "")?;
        }
        out.exp.holes.push(Hole { token, of });
        return Ok(());
    }
    let (ask, kind) = match of {
        HoleOf::Line => (PlaceOf::Line, ReadKind::Line),
        HoleOf::Column => (PlaceOf::Column, ReadKind::Column),
        HoleOf::File => (PlaceOf::File, ReadKind::File),
        HoleOf::ModulePath => (PlaceOf::ModulePath, ReadKind::ModulePath),
    };
    let wrong = HostError { ask: HostAsk::Place, why: HostWhy::Unreadable };
    let place = cx.place(ask)?;
    match (place, number) {
        (Place::Number(value), true) => {
            out.exp.reads.push(kind, "", ReadAnswer::of_place(place))?;
            let mut text = Buf::new_in(out.alloc);
            core::fmt::Write::write_fmt(&mut text, format_args!("{value}u32"))
                .expect("a `Buf` takes any text");
            let kind = TokenKind::Literal {
                kind: LitKind::Integer,
                suffix_len: 3,
                spacing: Spacing::Alone,
            };
            out.made(kind, text.as_str(), MADE)
        }
        (Place::Text(value), false) => {
            out.exp.reads.push(kind, "", ReadAnswer::of_place(place))?;
            push_str_literal(out, value)
        }
        // The context answered a number for a path or a path for a number.
        (Place::Number(_), false) | (Place::Text(_), true) => Err(wrong.into()),
    }
}

/// `panic!(..)` as `$crate::panic::panic_2021!(..)` or `panic_2015!`, by the call's panic
/// edition; `unreachable!` alike.
fn panic(out: &mut Out<'_, '_>, name: &str) -> Result<(), ExpandError> {
    let edition = out.call.site.panic_edition;
    let which = match (name, edition >= crate::tokens::Edition::E2021) {
        ("panic", true) => "panic_2021",
        ("panic", false) => "panic_2015",
        (_, true) => "unreachable_2021",
        (_, false) => "unreachable_2015",
    };
    crate_path(out, &["panic", which])?;
    punct(out, '!', Spacing::Alone)?;
    out.made(TokenKind::Open(Delim::Paren), "", MADE)?;
    let input = out.call.input();
    for (at, token) in input.tokens().iter().enumerate() {
        out.push(token.kind, input.text_of(token), TokenOrigin::Input(index(at)))?;
    }
    out.made(TokenKind::Close(Delim::Paren), "", MADE)
}
