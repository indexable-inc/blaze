//! The format strings of `message`, `label` and `note`: rustc's format-string parser in its
//! diagnostic mode, and what the diagnostic attributes make of each argument.
//!
//! Follows rustc's `compiler/rustc_parse_format/src/lib.rs` (`Parser` with
//! `ParseMode::Diagnostic`: `next`, `new`, `argument`, `position`, `diagnostic`, `word`,
//! `integer`, `missing_closing_brace` and its suggestions) and
//! `compiler/rustc_attr_parsing/src/attributes/diagnostic/mod.rs` (`parse_format_string`,
//! `parse_arg`, `warn_on_format_spec`, `slice_span`) (MIT or Apache-2.0).

use std::ops::Range;

use syntax::{TextRange, TextSize};

use super::Mode;

/// Where an argument is: rustc's `Position`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Position<'a> {
    /// `{}`.
    Implicit,
    /// `{1}`.
    Index(usize),
    /// `{name}`.
    Named(&'a str),
}

/// What is between `{` and `}`: rustc's `Argument` with the only spec the diagnostic mode
/// parses, what follows a `:`.
#[derive(Clone, Debug)]
struct Argument<'a> {
    position: Position<'a>,
    /// What the position is written in, the white space after it included.
    position_span: Range<usize>,
    ty: &'a str,
    /// The `:` and what follows it, when there is one.
    ty_span: Option<Range<usize>>,
}

impl Argument<'_> {
    fn is_identifier(&self) -> bool {
        matches!(self.position, Position::Named(_)) && self.ty.is_empty() && self.ty_span.is_none()
    }
}

enum Piece<'a> {
    /// Text outside the braces, `{{` and `}}` as one brace each.
    Lit(&'a str),
    Arg(Argument<'a>),
}

/// rustc's `ParseError`, as far as the diagnostic attributes show it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParseError {
    description: String,
    label: String,
    span: Range<usize>,
}

/// rustc's `rustc_parse_format::Parser`, in `ParseMode::Diagnostic`.
struct Parser<'a> {
    input: &'a str,
    /// Each char of the input: where it is written in the literal, where it is in the input.
    input_vec: Vec<(Range<usize>, usize, char)>,
    index: usize,
    errors: Vec<ParseError>,
    is_source_literal: bool,
    end_of_snippet: usize,
}

impl<'a> Parser<'a> {
    /// The parser of `input`, the value of the string literal written `snippet`.
    fn new(input: &'a str, snippet: Option<&str>) -> Parser<'a> {
        let quote_offset = 1;
        let (is_source_literal, end_of_snippet, pre_input_vec) = match snippet {
            Some(snippet) if snippet.starts_with('"') && snippet.len() >= 2 => {
                let without_quotes = &snippet[1..snippet.len() - 1];
                // alloc: once per format string of a diagnostic attribute, as is all of `new`
                let (mut ok, mut vec) = (true, Vec::new());
                let mut chars = input.chars();
                syntax::unescape::unescape_str(without_quotes, |range, res| match res {
                    Ok(ch) if ok && chars.next().is_some_and(|c| ch == c) => vec.push((range, ch)),
                    _ => {
                        ok = false;
                        vec.clear();
                    }
                });
                let end = vec.last().map_or(0, |(range, _): &(Range<usize>, char)| range.end);
                if ok && chars.as_str().is_empty() {
                    (true, 1 + end, vec)
                } else {
                    // alloc: empty, allocates nothing
                    (false, snippet.len(), Vec::new())
                }
            }
            // alloc: as above
            Some(snippet) => (false, snippet.len(), Vec::new()),
            // alloc: as above
            None => (false, input.len(), Vec::new()),
        };
        let input_vec = if pre_input_vec.is_empty() {
            input
                .char_indices()
                .map(|(idx, c)| {
                    let i = idx + quote_offset;
                    (i..i + c.len_utf8(), idx, c)
                })
                // alloc: the format string's chars, once per format string
                .collect()
        } else {
            input
                .char_indices()
                .zip(pre_input_vec)
                .map(|((i, c), (r, _))| (r.start + quote_offset..r.end + quote_offset, i, c))
                // alloc: as above
                .collect()
        };
        // alloc: empty until an error
        Parser { input, input_vec, index: 0, errors: Vec::new(), is_source_literal, end_of_snippet }
    }

    fn next_piece(&mut self) -> Option<Piece<'a>> {
        let Some((Range { start, end }, idx, ch)) = self.peek() else { return None };
        match ch {
            '{' => {
                self.index += 1;
                if let Some((_, i, '{')) = self.peek() {
                    self.index += 1;
                    Some(Piece::Lit(self.string(i)))
                } else {
                    let arg = self.argument();
                    self.ws();
                    if self.consume_pos('}').is_none() {
                        self.missing_closing_brace(&arg);
                    }
                    Some(Piece::Arg(arg))
                }
            }
            '}' => {
                self.index += 1;
                if let Some((_, i, '}')) = self.peek() {
                    self.index += 1;
                    Some(Piece::Lit(self.string(i)))
                } else {
                    self.errors.push(ParseError {
                        // alloc: an error's text, as is each `ParseError`'s below
                        description: "unmatched `}` found".to_owned(),
                        // alloc: as above
                        label: "unmatched `}`".to_owned(),
                        span: start..end,
                    });
                    None
                }
            }
            _ => Some(Piece::Lit(self.string(idx))),
        }
    }

    fn peek(&self) -> Option<(Range<usize>, usize, char)> {
        self.input_vec.get(self.index).cloned()
    }

    fn peek_ahead(&self) -> Option<(Range<usize>, usize, char)> {
        self.input_vec.get(self.index + 1).cloned()
    }

    fn consume(&mut self, c: char) -> bool {
        self.consume_pos(c).is_some()
    }

    fn consume_pos(&mut self, ch: char) -> Option<(Range<usize>, usize)> {
        if let Some((r, i, c)) = self.peek()
            && ch == c
        {
            self.index += 1;
            return Some((r, i));
        }
        None
    }

    fn missing_closing_brace(&mut self, arg: &Argument<'_>) {
        let (range, description) = match self.peek() {
            // alloc: as above
            Some((r, _, c)) => {
                (r.start..r.start, format!("expected `}}`, found `{}`", c.escape_debug()))
            }
            // alloc: as above
            None => (
                self.end_of_snippet..self.end_of_snippet,
                "expected `}` but string was terminated".to_owned(),
            ),
        };
        // alloc: as above
        self.errors.push(ParseError {
            description,
            label: "expected `}`".to_owned(),
            span: range.start..range.start,
        });
        if let (Some((_, _, c)), Some((_, _, nc))) = (self.peek(), self.peek_ahead()) {
            match (c, nc) {
                ('?', '}') => self.missing_colon_before_debug_formatter(),
                ('?', _) => self.suggest_format_debug(),
                ('<' | '^' | '>', _) => self.suggest_format_align(c),
                (',', _) => self.suggest_unsupported_python_numeric_grouping(),
                ('=', '}') => self.suggest_rust_debug_printing_macro(),
                ('+', _) => self.suggest_format_missing_colon_for_sign(),
                _ => self.suggest_positional_arg_instead_of_captured_arg(arg),
            }
        }
    }

    fn ws(&mut self) {
        let rest = &self.input_vec[self.index..];
        self.index += rest.iter().position(|&(_, _, c)| !c.is_whitespace()).unwrap_or(rest.len());
    }

    /// What is outside the braces, from `start` on.
    fn string(&mut self, start: usize) -> &'a str {
        while let Some((_, i, c)) = self.peek() {
            if matches!(c, '{' | '}') {
                return &self.input[start..i];
            }
            self.index += 1;
        }
        &self.input[start..]
    }

    fn argument(&mut self) -> Argument<'a> {
        let start_idx = self.index;
        let position = self.position();
        self.ws();
        let end_idx = self.index;
        let (ty, ty_span) = self.diagnostic();
        let position = position.unwrap_or(Position::Implicit);
        let position_span = self.index2range(start_idx).start..self.index2range(end_idx).start;
        Argument { position, position_span, ty, ty_span }
    }

    fn position(&mut self) -> Option<Position<'a>> {
        if let Some(i) = self.integer() {
            return Some(Position::Index(i.into()));
        }
        match self.peek() {
            Some((range, _, c)) if is_id_start(c) => {
                let start = range.start;
                let word = self.word();
                if word == "r"
                    && let Some((_, _, '#')) = self.peek()
                    && self.peek_ahead().is_some_and(|(_, _, c)| is_id_start(c))
                {
                    self.index += 1;
                    let word = self.word();
                    let full_span = start..self.index2range(self.index).start;
                    self.errors.insert(
                        0,
                        ParseError {
                            // alloc: as above
                            description: "raw identifiers are not supported".to_owned(),
                            // alloc: as above
                            label: "raw identifier used here".to_owned(),
                            span: full_span,
                        },
                    );
                    return Some(Position::Named(word));
                }
                Some(Position::Named(word))
            }
            _ => None,
        }
    }

    fn index2pos(&self, index: usize) -> usize {
        match self.input_vec.get(index) {
            Some((_, pos, _)) => *pos,
            None => self.input.len(),
        }
    }

    fn index2range(&self, index: usize) -> Range<usize> {
        match self.input_vec.get(index) {
            // clone: a range, two integers copied
            Some((r, _, _)) => r.clone(),
            None => self.end_of_snippet..self.end_of_snippet,
        }
    }

    /// The spec the diagnostic mode takes: what follows a `:`, up to a brace.
    fn diagnostic(&mut self) -> (&'a str, Option<Range<usize>>) {
        let Some((Range { start, .. }, _)) = self.consume_pos(':') else { return ("", None) };
        let ty = self.string(self.index2pos(self.index));
        let end = self.index2range(self.index).start;
        (ty, Some(start..end))
    }

    fn word(&mut self) -> &'a str {
        let index = self.index;
        match self.peek() {
            Some((_, _, c)) if is_id_start(c) => self.index += 1,
            _ => return "",
        }
        let (err_end, end) = loop {
            match self.peek() {
                Some((r, i, c)) => {
                    if is_id_continue(c) {
                        self.index += 1;
                    } else {
                        break (r.start, i);
                    }
                }
                None => break (self.end_of_snippet, self.input.len()),
            }
        };
        let word = &self.input[self.index2pos(index)..end];
        if word == "_" {
            self.errors.push(ParseError {
                // alloc: as above
                description: "invalid argument name `_`".to_owned(),
                // alloc: as above
                label: "invalid argument name".to_owned(),
                span: self.index2range(index).start..err_end,
            });
        }
        word
    }

    fn integer(&mut self) -> Option<u16> {
        let mut cur: u16 = 0;
        let mut found = false;
        let mut overflow = false;
        let start_index = self.index;
        while let Some((_, _, c)) = self.peek() {
            let Some(i) = c.to_digit(10) else { break };
            self.index += 1;
            let (tmp, mul_overflow) = cur.overflowing_mul(10);
            let (tmp, add_overflow) = tmp.overflowing_add(i as u16);
            overflow |= mul_overflow || add_overflow;
            cur = tmp;
            found = true;
        }
        if overflow {
            let overflowed = &self.input[self.index2pos(start_index)..self.index2pos(self.index)];
            self.errors.push(ParseError {
                // alloc: as above
                description: format!(
                    "integer `{overflowed}` does not fit into the type `u16` whose range is `0..={}`",
                    u16::MAX
                ),
                // alloc: as above
                label: "integer out of range for `u16`".to_owned(),
                span: self.index2range(start_index).start..self.index2range(self.index).end,
            });
        }
        found.then_some(cur)
    }

    fn suggest_format_debug(&mut self) {
        let question = self.consume_pos('?');
        let colon = self.consume_pos(':');
        if let (Some((range, _)), Some(_)) = (question, colon) {
            self.word();
            self.errors.insert(
                0,
                ParseError {
                    // alloc: as above
                    description: "expected format parameter to occur after `:`".to_owned(),
                    // alloc: as above
                    label: "expected `?` to occur after `:`".to_owned(),
                    span: range,
                },
            );
        }
    }

    fn missing_colon_before_debug_formatter(&mut self) {
        if let Some((range, _)) = self.consume_pos('?') {
            self.errors.insert(
                0,
                ParseError {
                    // alloc: as above
                    description: "expected `}`, found `?`".to_owned(),
                    // alloc: as above
                    label: "expected `:` before `?` to format with `Debug`".to_owned(),
                    span: range,
                },
            );
        }
    }

    fn suggest_rust_debug_printing_macro(&mut self) {
        if let Some((range, _)) = self.consume_pos('=') {
            self.errors.insert(
                0,
                ParseError {
                    description:
                        "python's f-string debug `=` is not supported in rust, use `dbg(x)` instead"
                            // alloc: as above
                            .to_owned(),
                    // alloc: as above
                    label: "expected `}`".to_owned(),
                    span: range,
                },
            );
        }
    }

    fn suggest_format_align(&mut self, alignment: char) {
        if let Some((range, _)) = self.consume_pos(alignment) {
            self.errors.insert(
                0,
                ParseError {
                    description:
                        "expected alignment specifier after `:` in format string; example: `{:>?}`"
                            // alloc: as above
                            .to_owned(),
                    // alloc: as above
                    label: format!("expected `{alignment}` to occur after `:`"),
                    span: range,
                },
            );
        }
    }

    fn suggest_unsupported_python_numeric_grouping(&mut self) {
        if let Some((range, _)) = self.consume_pos(',') {
            self.errors.insert(
                0,
                ParseError {
                    // alloc: as above
                    description:
                        "python's numeric grouping `,` is not supported in rust format strings"
                            .to_owned(),
                    // alloc: as above
                    label: "expected `}`".to_owned(),
                    span: range,
                },
            );
        }
    }

    fn suggest_format_missing_colon_for_sign(&mut self) {
        if let Some((range, _)) = self.consume_pos('+') {
            self.errors.insert(
                0,
                ParseError {
                    // alloc: as above
                    description: "the `+` sign flag must appear after `:` in a format string"
                        .to_owned(),
                    // alloc: as above
                    label: "expected `:` before `+` sign flag".to_owned(),
                    span: range,
                },
            );
        }
    }

    fn suggest_positional_arg_instead_of_captured_arg(&mut self, arg: &Argument<'_>) {
        if !arg.is_identifier() {
            return;
        }
        if self.consume_pos('.').is_none() {
            return;
        }
        let field = self.argument();
        if !self.consume('}') {
            return;
        }
        let description = match field.position {
            Position::Named(_) => "field access isn't supported",
            Position::Index(_) => "tuple index access isn't supported",
            Position::Implicit => return,
        };
        self.errors.insert(
            0,
            ParseError {
                // alloc: as above
                description: description.to_owned(),
                // alloc: as above
                label: "not supported".to_owned(),
                span: arg.position_span.start..field.position_span.end,
            },
        );
    }
}

/// rustc_lexer's `is_id_start`.
fn is_id_start(c: char) -> bool {
    c == '_' || unicode_ident::is_xid_start(c)
}

/// rustc_lexer's `is_id_continue`.
fn is_id_continue(c: char) -> bool {
    unicode_ident::is_xid_continue(c)
}

/// What a format string's arguments are not allowed to be: rustc's `FormatWarning`, each
/// lint `malformed_diagnostic_format_literals`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Warning {
    /// `{}`.
    Positional(TextRange),
    /// `{1}`.
    Indexed(TextRange),
    /// `{x:?}`.
    Specifier(TextRange),
    /// A name `#[diagnostic::on_unknown]` or `on_unmatched_args` does not take.
    Disallowed(TextRange),
}

/// A format string that parsed: the generic parameters it names, each where its name is
/// written (rustc's `FormatArg::GenericParam`), and what is not allowed in it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Parsed {
    pub(super) params: Vec<(String, TextRange)>,
    pub(super) warnings: Vec<Warning>,
}

/// A format string that did not parse: rustc's first `ParseError`, its label where it is in
/// the literal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Failed {
    pub(super) description: String,
    pub(super) label: String,
    pub(super) at: TextRange,
}

/// rustc's `parse_format_string` of `input`, the value of the string literal `snippet`
/// written at `literal`, as an option of an attribute of `mode`.
pub(super) fn parse(
    input: &str,
    snippet: &str,
    literal: TextRange,
    mode: Mode,
) -> Result<Parsed, Failed> {
    let mut parser = Parser::new(input, Some(snippet));
    // alloc: once per format string of a diagnostic attribute, its arguments
    let mut args = Vec::new();
    while let Some(piece) = parser.next_piece() {
        if let Piece::Arg(arg) = piece {
            args.push(arg);
        }
    }
    let is_source_literal = parser.is_source_literal;
    if let Some(error) = parser.errors.into_iter().next() {
        return Err(Failed {
            description: error.description,
            label: error.label,
            at: slice(literal, error.span, true),
        });
    }
    let mut parsed = Parsed::default();
    for arg in &args {
        parse_arg(arg, mode, &mut parsed, literal, is_source_literal);
    }
    Ok(parsed)
}

/// rustc's `parse_filter_format`: the names an `on`-clause's value `input` formats, pushed to
/// `params`; its errors and specifiers are not looked at.
pub(super) fn filter_params(input: &str, params: &mut Vec<String>) {
    let mut parser = Parser::new(input, None);
    while let Some(piece) = parser.next_piece() {
        if let Piece::Arg(arg) = piece
            && let Some(name) = filter_arg(&arg)
        {
            // alloc: a parameter an `on(..)` filter names (`directive::filter`)
            params.push(name.to_owned());
        }
    }
}

/// rustc's `parse_filter_format` and `FilterFormatString::format`: an `on`-clause's value
/// `input` (`T = "&[{A}]"`) with each generic parameter it names filled in from `generic_args`
/// (`Self` among them), any other name printed back as `{name}`; the kinds it is compared with
/// (`{integral}`, `{union}`) are left as they are.
pub(crate) fn format_filter(input: &str, generic_args: &[(String, String)]) -> String {
    let mut parser = Parser::new(input, None);
    // alloc: one value of an `on(..)` filter, for an error being reported
    let mut out = String::with_capacity(input.len());
    while let Some(piece) = parser.next_piece() {
        match piece {
            Piece::Lit(lit) => out.push_str(lit),
            Piece::Arg(arg) => match (filter_arg(&arg), arg.position) {
                (Some(name), _) => match generic_args.iter().find(|(param, _)| param == name) {
                    Some((_, value)) => out.push_str(value),
                    None => push_braced(&mut out, name),
                },
                (None, Position::Named(name)) => push_braced(&mut out, name),
                (None, Position::Index(index)) => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "{{{index}}}");
                }
                (None, Position::Implicit) => out.push_str("{}"),
            },
        }
    }
    out
}

/// The generic parameter an argument of an `on`-clause's value names: not one of the kinds a
/// self type is compared as, nor `{}` or `{1}`.
fn filter_arg<'a>(arg: &Argument<'a>) -> Option<&'a str> {
    match arg.position {
        Position::Named("integer" | "integral" | "float" | "union" | "enum" | "struct") => None,
        Position::Named(name) => Some(name),
        Position::Index(_) | Position::Implicit => None,
    }
}

/// What an argument of a format string stands for under an attribute: rustc's `FormatArg`.
/// [`FormatArg::of`] is the one table of which names each attribute fills in; the lints of the
/// attribute and the error that fills it in (E0277's `on_unimplemented`) both read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormatArg<'a> {
    /// `{T}`: a generic parameter of the item, or, when it has none of that name, printed back
    /// as `{T}` (its format spec, if any, dropped).
    GenericParam(&'a str),
    /// `{Self}`: the self type.
    SelfUpper,
    /// `{this}` and `{this:name}`: the item the attribute is on.
    This,
    /// `{this:resolved}`: the trait with its arguments, `Fn(u8)` sugar for a parenthesized one
    /// (`#[rustc_on_unimplemented]` only).
    ThisResolved,
    /// `{this:path}`: the trait's path (`#[rustc_on_unimplemented]` only).
    ThisPath,
    /// `{item_context}`: what the code is in, "a function", "an async block"
    /// (`#[rustc_on_unimplemented]` only).
    ItemContext,
    /// `{found}` of `#[diagnostic::on_type_error]`.
    Found,
    /// `{expected}` of `#[diagnostic::on_type_error]`.
    Expected,
    /// `{unresolved}` of `#[diagnostic::on_unknown]`.
    Unresolved,
    /// A name the attribute does not take, printed back as `{name}`.
    Disallowed(&'a str),
    /// `{1}`, printed back as written.
    Indexed(usize),
    /// `{}`, printed back as written.
    Positional,
}

impl<'a> FormatArg<'a> {
    /// rustc's `parse_arg`: what `arg` stands for under `mode`, and whether its format spec is
    /// one the argument takes (`{this:resolved}`, `{this:path}`), not to be warned of.
    fn of(arg: &Argument<'a>, mode: Mode) -> (FormatArg<'a>, bool) {
        let name = match arg.position {
            Position::Named(name) => name,
            Position::Index(index) => return (FormatArg::Indexed(index), false),
            Position::Implicit => return (FormatArg::Positional, false),
        };
        let it = match (mode, name) {
            (Mode::RustcOnUnimplemented, "item_context") => FormatArg::ItemContext,
            (Mode::RustcOnUnimplemented, "this") => match arg.ty {
                "resolved" => return (FormatArg::ThisResolved, true),
                "path" => return (FormatArg::ThisPath, true),
                _ => FormatArg::This,
            },
            (Mode::DiagnosticOnTypeError, "found") => FormatArg::Found,
            (Mode::DiagnosticOnTypeError, "expected") => FormatArg::Expected,
            (Mode::DiagnosticOnUnknown, "unresolved") => FormatArg::Unresolved,
            // `diagnostic::on_unimplemented` does not take `{this}`: it is a generic name
            // there, as rustc's FIXME(mejrs) says.
            (
                Mode::DiagnosticOnUnknown
                | Mode::DiagnosticOnMove
                | Mode::DiagnosticOnUnmatchedArgs
                | Mode::DiagnosticOnTypeError,
                "this",
            ) => FormatArg::This,
            (
                Mode::RustcOnUnimplemented
                | Mode::DiagnosticOnUnimplemented
                | Mode::DiagnosticOnMove
                | Mode::DiagnosticOnConst,
                "Self",
            ) => FormatArg::SelfUpper,
            (
                Mode::RustcOnUnimplemented
                | Mode::DiagnosticOnUnimplemented
                | Mode::DiagnosticOnMove
                | Mode::DiagnosticOnConst
                | Mode::DiagnosticOnTypeError,
                param,
            ) => FormatArg::GenericParam(param),
            (Mode::DiagnosticOnUnknown | Mode::DiagnosticOnUnmatchedArgs, name) => {
                FormatArg::Disallowed(name)
            }
        };
        (it, false)
    }
}

/// What a format string's arguments are filled in with: rustc's `FormatArgs`. A field an
/// attribute does not take is never read for it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FormatArgs<'a> {
    /// The item the attribute is on, for `{this}`.
    pub(crate) this: &'a str,
    pub(crate) this_resolved: &'a str,
    pub(crate) this_path: &'a str,
    pub(crate) found: &'a str,
    pub(crate) expected: &'a str,
    pub(crate) unresolved: &'a str,
    pub(crate) item_context: &'a str,
    /// The item's generic parameters (`Self` among them) and what each is.
    pub(crate) generic_args: &'a [(String, String)],
}

impl FormatArgs<'_> {
    /// What the generic parameter `name` is.
    fn generic(&self, name: &str) -> Option<&str> {
        self.generic_args.iter().find(|(param, _)| param == name).map(|(_, value)| value.as_str())
    }

    /// rustc's `parse_format_string` and `FormatString::format`: `input`, a format string of
    /// `mode`'s attribute, with each argument filled in. A string that does not parse (the
    /// lint reports it) is used as it is written, as rustc does.
    pub(crate) fn format(&self, input: &str, mode: Mode) -> String {
        let mut parser = Parser::new(input, None);
        // alloc: one format string of an error's custom message, label or note
        let mut out = String::with_capacity(input.len());
        while let Some(piece) = parser.next_piece() {
            let arg = match piece {
                Piece::Lit(lit) => {
                    out.push_str(lit);
                    continue;
                }
                Piece::Arg(arg) => FormatArg::of(&arg, mode).0,
            };
            match arg {
                FormatArg::GenericParam(name) => match self.generic(name) {
                    Some(value) => out.push_str(value),
                    None => push_braced(&mut out, name),
                },
                FormatArg::SelfUpper => out.push_str(self.generic("Self").unwrap_or("Self")),
                FormatArg::This => out.push_str(self.this),
                FormatArg::ThisResolved => out.push_str(self.this_resolved),
                FormatArg::ThisPath => out.push_str(self.this_path),
                FormatArg::ItemContext => out.push_str(self.item_context),
                FormatArg::Found => out.push_str(self.found),
                FormatArg::Expected => out.push_str(self.expected),
                FormatArg::Unresolved => out.push_str(self.unresolved),
                FormatArg::Disallowed(name) => push_braced(&mut out, name),
                FormatArg::Indexed(index) => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "{{{index}}}");
                }
                FormatArg::Positional => out.push_str("{}"),
            }
        }
        if parser.errors.is_empty() {
            out
        } else {
            // alloc: as above, the string as written
            input.to_owned()
        }
    }
}

fn push_braced(out: &mut String, name: &str) {
    out.push('{');
    out.push_str(name);
    out.push('}');
}

/// rustc's `parse_arg`: what `arg` is under `mode` ([`FormatArg::of`]), the warnings it gives.
fn parse_arg(
    arg: &Argument<'_>,
    mode: Mode,
    parsed: &mut Parsed,
    literal: TextRange,
    is_source_literal: bool,
) {
    // clone: a range, two integers copied
    let span = slice(literal, arg.position_span.clone(), is_source_literal);
    let (it, takes_spec) = FormatArg::of(arg, mode);
    match it {
        // alloc: a generic parameter a diagnostic attribute's format string names
        FormatArg::GenericParam(param) => parsed.params.push((param.to_owned(), span)),
        FormatArg::Disallowed(_) => parsed.warnings.push(Warning::Disallowed(span)),
        FormatArg::Indexed(_) => parsed.warnings.push(Warning::Indexed(span)),
        FormatArg::Positional => parsed.warnings.push(Warning::Positional(span)),
        FormatArg::SelfUpper
        | FormatArg::This
        | FormatArg::ThisResolved
        | FormatArg::ThisPath
        | FormatArg::ItemContext
        | FormatArg::Found
        | FormatArg::Expected
        | FormatArg::Unresolved => {}
    }
    if !takes_spec && let Some(ty_span) = &arg.ty_span {
        // clone: a range, two integers copied
        parsed.warnings.push(Warning::Specifier(slice(
            literal,
            ty_span.clone(),
            is_source_literal,
        )));
    }
}

/// rustc's `slice_span`: `range` of the literal when the parser could map its input to the
/// source, else the whole literal.
fn slice(literal: TextRange, range: Range<usize>, is_source_literal: bool) -> TextRange {
    if !is_source_literal {
        return literal;
    }
    let at = |offset: usize| literal.start() + TextSize::from(offset as u32);
    TextRange::new(at(range.start), at(range.end.max(range.start)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(input: &str, snippet: &str, mode: Mode) -> Result<Parsed, Failed> {
        parse(input, snippet, TextRange::at(TextSize::from(100), TextSize::of(snippet)), mode)
    }

    fn at(lo: u32, hi: u32) -> TextRange {
        TextRange::new(TextSize::from(100 + lo), TextSize::from(100 + hi))
    }

    #[test]
    fn named_arguments_are_generic_parameters_where_their_names_are() {
        let it =
            parsed("a {T} {Self} {U }", "\"a {T} {Self} {U }\"", Mode::DiagnosticOnUnimplemented)
                .unwrap();
        assert_eq!(it.params, [("T".to_owned(), at(4, 5)), ("U".to_owned(), at(15, 17))]);
        assert!(it.warnings.is_empty());
    }

    #[test]
    fn positional_and_indexed_arguments_and_specifiers_are_warned_of() {
        let it =
            parsed("{} {1:} {Self:123}", "\"{} {1:} {Self:123}\"", Mode::DiagnosticOnUnimplemented)
                .unwrap();
        assert_eq!(
            it.warnings,
            [
                Warning::Positional(at(2, 2)),
                Warning::Indexed(at(5, 6)),
                Warning::Specifier(at(6, 7)),
                Warning::Specifier(at(14, 18)),
            ]
        );
    }

    #[test]
    fn an_unmatched_closing_brace_is_the_error() {
        let it = parsed("{{Test } thing", "\"{{Test } thing\"", Mode::DiagnosticOnUnimplemented)
            .unwrap_err();
        assert_eq!(it.description, "unmatched `}` found");
        assert_eq!(it.label, "unmatched `}`");
        assert_eq!(it.at, at(8, 9));
    }

    #[test]
    fn an_unclosed_brace_points_at_the_closing_quote() {
        let it = parsed("{T", "\"{T\"", Mode::DiagnosticOnMove).unwrap_err();
        assert_eq!(it.description, "expected `}` but string was terminated");
        assert_eq!(it.at, at(3, 3));
    }

    #[test]
    fn escapes_map_back_to_where_they_are_written() {
        let it = parsed("\u{0} {T}", "\"\\x00 {T}\"", Mode::DiagnosticOnMove).unwrap();
        assert_eq!(it.params, [("T".to_owned(), at(7, 8))]);
    }

    #[test]
    fn a_raw_string_is_not_mapped() {
        let it = parsed("{}", "r\"{}\"", Mode::DiagnosticOnMove).unwrap();
        assert_eq!(it.warnings, [Warning::Positional(at(0, 5))]);
    }

    #[test]
    fn modes_differ_in_what_names_they_take() {
        let it = parsed("{this} {T}", "\"{this} {T}\"", Mode::DiagnosticOnUnmatchedArgs).unwrap();
        assert_eq!(it.warnings, [Warning::Disallowed(at(9, 10))]);
        let it =
            parsed("{this:resolved}", "\"{this:resolved}\"", Mode::RustcOnUnimplemented).unwrap();
        assert!(it.warnings.is_empty() && it.params.is_empty());
        let it = parsed("{this:resolved}", "\"{this:resolved}\"", Mode::DiagnosticOnUnimplemented)
            .unwrap();
        assert_eq!(it.params, [("this".to_owned(), at(2, 6))]);
        assert_eq!(it.warnings, [Warning::Specifier(at(6, 15))]);
        let it = parsed("{expected}", "\"{expected}\"", Mode::DiagnosticOnMove).unwrap();
        assert_eq!(it.params, [("expected".to_owned(), at(2, 10))]);
    }

    #[test]
    fn rustc_on_unimplemented_fills_every_placeholder_it_takes_and_prints_back_the_rest() {
        // alloc: a test's parameters
        let generic_args =
            [("Self".to_owned(), "fn(u16)".to_owned()), ("Args".to_owned(), "(u8,)".to_owned())];
        let args = FormatArgs {
            this: "Fn",
            this_resolved: "Fn(u8)",
            this_path: "Fn<Args>",
            item_context: "a closure",
            generic_args: &generic_args,
            ..FormatArgs::default()
        };
        let input = "expected `{this:resolved}` signature, found `{Self}`; {this} {this:name} {this:path} \
                     in {item_context}, {Args} {X} {X:spec} {{}} {} {1}";
        assert_eq!(
            args.format(input, Mode::RustcOnUnimplemented),
            "expected `Fn(u8)` signature, found `fn(u16)`; Fn Fn Fn<Args> in a closure, (u8,) {X} {X} {} {} {1}"
        );
        // `diagnostic::on_unimplemented` takes none of `this`'s forms nor `item_context`: they
        // are generic names, printed back without their spec.
        assert_eq!(
            args.format(
                "{this} {this:resolved} {item_context} {Self}",
                Mode::DiagnosticOnUnimplemented
            ),
            "{this} {this} {item_context} fn(u16)"
        );
        // A string that does not parse is used as written.
        assert_eq!(args.format("{Self", Mode::RustcOnUnimplemented), "{Self");
    }
}
