//! The builtin fn-like macros `println!` and its kin need, written here on the portable form.
//!
//! The eager ones (`concat!`, `env!`, `option_env!`, `include_str!`, `include_bytes!`,
//! `compile_error!`, and the `format_args!` family's template) first list the macro calls in
//! their input as [`Expanded::NeedsExpanded`](crate::Expanded::NeedsExpanded); re-entered with
//! the answers, they read the input with each call replaced by its answer, whose tokens take
//! the origin of the call's path.

use core::alloc::Allocator;

use alloc::vec::Vec;

use blaze_id::ContentId;

use crate::{
    bridge::{self, Role},
    call::Inside,
    cx::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, Place, PlaceOf},
    def::BuiltinFn,
    error::{ExpandError, Stream, Unbuilt},
    expand::Out,
    expansion::{
        Diagnostic, Hole, HoleOf, IncludeFailure, NestedCall, NestedKind, Origin, ReadAnswer,
        ReadKind,
    },
    literal::{self, Buf, LitError},
    nested,
    tokens::{Delim, LitKind, Raw, Spacing, TokenKind, TokenStream, Transparency, index},
};

/// What a builtin did: its expansion is in `out`, or it needs its nested calls expanded.
pub(crate) enum Step<A: Allocator> {
    Done,
    Needs(Vec<NestedCall, A>),
}

/// The tokens builtins make: call-site names, def-site `$crate` (rustc's `mixed_site`).
const MADE: Transparency = Transparency::SemiOpaque;

pub(crate) fn expand<A: Allocator + Clone>(
    which: BuiltinFn,
    out: &mut Out<'_, A>,
    cx: &impl ExpandCx,
) -> Result<Step<A>, ExpandError> {
    match which {
        BuiltinFn::Concat
        | BuiltinFn::Env
        | BuiltinFn::OptionEnv
        | BuiltinFn::IncludeStr
        | BuiltinFn::IncludeBytes
        | BuiltinFn::CompileError
        | BuiltinFn::FormatArgs
        | BuiltinFn::FormatArgsNl
        | BuiltinFn::ConstFormatArgs => return eager(which, out, cx),
        BuiltinFn::Stringify => stringify(out)?,
        BuiltinFn::Cfg => cfg(out, cx)?,
        BuiltinFn::Line => place(out, cx, HoleOf::Line)?,
        BuiltinFn::Column => place(out, cx, HoleOf::Column)?,
        BuiltinFn::File => place(out, cx, HoleOf::File)?,
        BuiltinFn::ModulePath => place(out, cx, HoleOf::ModulePath)?,
        BuiltinFn::Panic => panic(out, "panic")?,
        BuiltinFn::Unreachable => panic(out, "unreachable")?,
        BuiltinFn::Include
        | BuiltinFn::Assert
        | BuiltinFn::ConcatBytes
        | BuiltinFn::Asm
        | BuiltinFn::GlobalAsm
        | BuiltinFn::NakedAsm
        | BuiltinFn::CfgSelect
        | BuiltinFn::TraceMacros
        | BuiltinFn::LogSyntax => {
            return Err(ExpandError::NotBuilt { what: Unbuilt::BuiltinFn(which) });
        }
    }
    Ok(Step::Done)
}

/// A builtin's input: a stream and, when nested calls were replaced by their answers, each
/// token's origin.
struct Args<'s, A: Allocator> {
    stream: &'s TokenStream<A>,
    from: Option<&'s [Origin]>,
}

impl<A: Allocator> Args<'_, A> {
    fn origin(&self, at: usize) -> Origin {
        match self.from {
            Some(from) => from[at],
            None => Origin::Input(index(at)),
        }
    }

    /// The comma-separated arguments, each without the invisible groups around it; a trailing
    /// comma ends the list.
    fn split<B: Allocator>(&self, alloc: B) -> Vec<core::ops::Range<usize>, B> {
        let tokens = self.stream.tokens();
        let mut pieces = Vec::new_in(alloc);
        let mut depth = 0u32;
        let mut start = 0;
        for (at, token) in tokens.iter().enumerate() {
            match token.kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) => depth -= 1,
                TokenKind::Punct { char: ',', .. } if depth == 0 => {
                    pieces.push(self.unwrap(start..at));
                    start = at + 1;
                }
                _ => {}
            }
        }
        if start < tokens.len() {
            pieces.push(self.unwrap(start..tokens.len()));
        }
        pieces
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
fn eager<A: Allocator + Clone>(
    which: BuiltinFn,
    out: &mut Out<'_, A>,
    cx: &impl ExpandCx,
) -> Result<Step<A>, ExpandError> {
    let call = out.call;
    let input = call.input();
    let alloc = out.alloc.clone();
    let mut calls = nested::find_calls(input, alloc.clone())?;
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
    let mut stream = TokenStream::new_in(alloc.clone());
    let mut from = Vec::new_in(alloc);
    let mut at = 0;
    for (n, (nested, answer)) in calls.iter().zip(answers.iter()).enumerate() {
        while at < nested.path.start as usize {
            stream.push_from(input, at)?;
            from.push(Origin::Input(index(at)));
            at += 1;
        }
        let in_answer =
            |error| ExpandError::Stream { stream: Some(Stream::Answer(index(n))), error };
        for token in 0..answer.len() {
            stream.push_from(*answer, token).map_err(in_answer)?;
            from.push(Origin::Input(nested.path.start));
        }
        at = nested.args.end as usize;
    }
    while at < input.len() {
        stream.push_from(input, at)?;
        from.push(Origin::Input(index(at)));
        at += 1;
    }
    run_eager(which, &Args { stream: &stream, from: Some(&from) }, out, cx)?;
    Ok(Step::Done)
}

fn run_eager<A: Allocator + Clone>(
    which: BuiltinFn,
    args: &Args<'_, A>,
    out: &mut Out<'_, A>,
    cx: &impl ExpandCx,
) -> Result<(), ExpandError> {
    match which {
        BuiltinFn::Concat => concat(args, out),
        BuiltinFn::Env => env(args, out, cx, Optional::No),
        BuiltinFn::OptionEnv => env(args, out, cx, Optional::Yes),
        BuiltinFn::IncludeStr => include(args, out, cx, IncludeAs::Str),
        BuiltinFn::IncludeBytes => include(args, out, cx, IncludeAs::Bytes),
        BuiltinFn::CompileError => compile_error(args, out),
        BuiltinFn::FormatArgs | BuiltinFn::ConstFormatArgs => format_args(args, out, Newline::No),
        BuiltinFn::FormatArgsNl => format_args(args, out, Newline::Yes),
        _ => unreachable!("`expand` sends only the eager builtins to `eager`"),
    }
}

/// A builtin's own error about its arguments.
fn refuse<A: Allocator + Clone>(
    out: &mut Out<'_, A>,
    message: &str,
    at: Option<Origin>,
) -> Result<(), ExpandError> {
    let message = out.note(message)?;
    out.diagnose(Diagnostic::Builtin { message, at });
    Ok(())
}

/// Where argument token `at` is, or the end of the input.
fn at_token<A: Allocator>(args: &Args<'_, A>, at: usize) -> Option<Origin> {
    (at < args.stream.len()).then(|| args.origin(at))
}

/// The value of a string literal argument, into `value`.
fn string_arg<A: Allocator + Clone>(
    args: &Args<'_, A>,
    range: core::ops::Range<usize>,
    value: &mut Buf<A>,
) -> Result<(), (LitError, Option<Origin>)> {
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
fn push_str_literal<A: Allocator + Clone>(
    out: &mut Out<'_, A>,
    value: &str,
) -> Result<(), ExpandError> {
    let mut text = Buf::new_in(out.alloc.clone());
    literal::escape_str(value, &mut text);
    let kind = TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone };
    out.made(kind, text.as_str(), MADE)
}

fn ident<A: Allocator + Clone>(out: &mut Out<'_, A>, name: &str) -> Result<(), ExpandError> {
    out.made(TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone }, name, MADE)
}

fn punct<A: Allocator + Clone>(
    out: &mut Out<'_, A>,
    char: char,
    spacing: Spacing,
) -> Result<(), ExpandError> {
    out.made(TokenKind::Punct { char, spacing }, "", MADE)
}

/// `::name` for each name, after `$crate`.
fn crate_path<A: Allocator + Clone>(
    out: &mut Out<'_, A>,
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

fn concat<A: Allocator + Clone>(
    args: &Args<'_, A>,
    out: &mut Out<'_, A>,
) -> Result<(), ExpandError> {
    let mut value = Buf::new_in(out.alloc.clone());
    for range in args.split(out.alloc.clone()) {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Optional {
    No,
    Yes,
}

fn env<A: Allocator + Clone>(
    args: &Args<'_, A>,
    out: &mut Out<'_, A>,
    cx: &impl ExpandCx,
    optional: Optional,
) -> Result<(), ExpandError> {
    let pieces = args.split(out.alloc.clone());
    let most = if optional == Optional::Yes { 1 } else { 2 };
    if pieces.is_empty() || pieces.len() > most {
        let message = if optional == Optional::Yes {
            "`option_env!` takes 1 argument"
        } else {
            "`env!` takes 1 or 2 arguments"
        };
        return refuse(out, message, None);
    }
    let mut name = Buf::new_in(out.alloc.clone());
    if let Err((error, at)) = string_arg(args, pieces[0].clone(), &mut name) {
        return refuse(out, error.message(), at);
    }
    let mut message = Buf::new_in(out.alloc.clone());
    let has_message = pieces.len() == 2;
    if has_message && let Err((error, at)) = string_arg(args, pieces[1].clone(), &mut message) {
        return refuse(out, error.message(), at);
    }
    let value = cx.env(name.as_str())?;
    let answer = match value {
        Some(value) => {
            ReadAnswer::Digest(ContentId::of("blaze v2 expand env value 1", value.as_bytes()))
        }
        None => ReadAnswer::Absent,
    };
    out.exp.reads.push(ReadKind::Env, name.as_str(), answer)?;
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

fn include<A: Allocator + Clone>(
    args: &Args<'_, A>,
    out: &mut Out<'_, A>,
    cx: &impl ExpandCx,
    how: IncludeAs,
) -> Result<(), ExpandError> {
    let pieces = args.split(out.alloc.clone());
    if pieces.len() != 1 {
        return refuse(out, "expected one string literal: the path", None);
    }
    let mut path = Buf::new_in(out.alloc.clone());
    if let Err((error, at)) = string_arg(args, pieces[0].clone(), &mut path) {
        return refuse(out, error.message(), at);
    }
    let kind = match how {
        IncludeAs::Str => ReadKind::IncludeStr,
        IncludeAs::Bytes => ReadKind::IncludeBytes,
    };
    let bytes = match cx.include(path.as_str(), how)? {
        Included::Bytes(bytes) => bytes,
        Included::Missing => {
            out.exp.reads.push(kind, path.as_str(), ReadAnswer::Absent)?;
            let path = out.note(path.as_str())?;
            out.diagnose(Diagnostic::Include { path, why: IncludeFailure::Missing });
            return Ok(());
        }
    };
    let digest = ContentId::of("blaze v2 expand included file 1", bytes);
    out.exp.reads.push(kind, path.as_str(), ReadAnswer::Digest(digest))?;
    match how {
        IncludeAs::Str => match core::str::from_utf8(bytes) {
            Ok(text) => push_str_literal(out, text),
            Err(_) => {
                let path = out.note(path.as_str())?;
                out.diagnose(Diagnostic::Include { path, why: IncludeFailure::NotUtf8 });
                Ok(())
            }
        },
        IncludeAs::Bytes => {
            let mut text = Buf::new_in(out.alloc.clone());
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

fn compile_error<A: Allocator + Clone>(
    args: &Args<'_, A>,
    out: &mut Out<'_, A>,
) -> Result<(), ExpandError> {
    let pieces = args.split(out.alloc.clone());
    let mut message = Buf::new_in(out.alloc.clone());
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
fn format_args<A: Allocator + Clone>(
    args: &Args<'_, A>,
    out: &mut Out<'_, A>,
    newline: Newline,
) -> Result<(), ExpandError> {
    let template = if newline == Newline::Yes {
        let first = args.split(out.alloc.clone()).first().cloned();
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
                let mut with_newline = Buf::new_in(out.alloc.clone());
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

/// `stringify!`: the input's text, spaced as rustc spaces it.
fn stringify<A: Allocator + Clone>(out: &mut Out<'_, A>) -> Result<(), ExpandError> {
    let input = out.call.input();
    let mut text = Buf::new_in(out.alloc.clone());
    stringified(input, out.call.site.edition, &mut text)?;
    push_str_literal(out, text.as_str())
}

/// The text of `stream`, by tt's printer, its spacing the stream's, appended to `into`.
fn stringified<A: Allocator, B: Allocator>(
    stream: &TokenStream<A>,
    edition: crate::tokens::Edition,
    into: &mut Buf<B>,
) -> Result<(), ExpandError> {
    let tree = bridge::to_tt(stream, Role::Input, Stream::Input, edition)?;
    let joined = |span, _: &str| bridge::source_joined(stream, span);
    into.push_str(&tt::stringify(tree.token_trees(), &joined, false));
    Ok(())
}

/// `cfg!(predicate)`: `true` or `false`, the outcome read.
fn cfg<A: Allocator + Clone>(out: &mut Out<'_, A>, cx: &impl ExpandCx) -> Result<(), ExpandError> {
    let input = out.call.input();
    if input.is_empty() {
        return refuse(out, "`cfg!` takes a cfg predicate", None);
    }
    let outcome = cx.cfg(input)?;
    let mut subject = Buf::new_in(out.alloc.clone());
    stringified(input, out.call.site.edition, &mut subject)?;
    out.exp.reads.push(ReadKind::Cfg, subject.as_str(), ReadAnswer::Bool(outcome))?;
    ident(out, if outcome { "true" } else { "false" })
}

/// `line!()`, `column!()`, `file!()`, `module_path!()`: a placeholder and a [`Hole`] where the
/// call is written; inside eager input, the value read.
fn place<A: Allocator + Clone>(
    out: &mut Out<'_, A>,
    cx: &impl ExpandCx,
    of: HoleOf,
) -> Result<(), ExpandError> {
    if !out.call.input().is_empty() {
        return refuse(out, "takes no arguments", Some(Origin::Input(0)));
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
    match (cx.place(ask)?, number) {
        (Place::Number(value), true) => {
            out.exp.reads.push(kind, "", ReadAnswer::Number(value))?;
            let mut text = Buf::new_in(out.alloc.clone());
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
            let digest = ContentId::of("blaze v2 expand place 1", value.as_bytes());
            out.exp.reads.push(kind, "", ReadAnswer::Digest(digest))?;
            push_str_literal(out, value)
        }
        // The context answered a number for a path or a path for a number.
        (Place::Number(_), false) | (Place::Text(_), true) => Err(wrong.into()),
    }
}

/// `panic!(..)` as `$crate::panic::panic_2021!(..)` or `panic_2015!`, by the call's panic
/// edition; `unreachable!` alike.
fn panic<A: Allocator + Clone>(out: &mut Out<'_, A>, name: &str) -> Result<(), ExpandError> {
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
        out.push(token.kind, input.text_of(token), Origin::Input(index(at)))?;
    }
    out.made(TokenKind::Close(Delim::Paren), "", MADE)
}
