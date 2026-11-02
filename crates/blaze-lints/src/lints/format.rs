//! The format macros' arguments as rustc's `FormatArgs` has them: each placeholder of the
//! template with the trait it formats with and the arguments it takes.
//!
//! Follows `rustc_parse_format` and `rustc_builtin_macros/src/format.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0), for what the lints read.

use std::borrow::Cow;

use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use crate::check::Cx;

/// The std macros clippy counts as format macros (`FORMAT_MACRO_DIAG_ITEMS`).
const MACROS: &[&str] = &[
    "assert_eq",
    "assert",
    "assert_ne",
    "panic",
    "debug_assert_eq",
    "debug_assert",
    "debug_assert_ne",
    "eprint",
    "eprintln",
    "format_args",
    "format",
    "print",
    "println",
    "todo",
    "unimplemented",
    "write",
    "writeln",
];

/// The trait a placeholder formats with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Trait {
    Display,
    Debug,
    /// `x`, `X`, `o`, `b`, `e`, `E` or `p`.
    Other,
}

/// What a placeholder or its width or precision takes: an argument by index into all the
/// arguments written, or one captured from the scope by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Arg<'a> {
    Index(usize),
    Captured(&'a str),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Placeholder<'a> {
    pub(crate) arg: Arg<'a>,
    pub(crate) format_trait: Trait,
    pub(crate) width: Option<Arg<'a>>,
    pub(crate) precision: Option<Arg<'a>>,
}

/// A format macro call written in the file: its name, its placeholders and the arguments it
/// was handed, as its expansion has them.
pub(crate) struct FormatCall<'t> {
    pub(crate) name: &'static str,
    /// Whether it is one of std's macros, not one marked `#[clippy::format_args]`.
    pub(crate) std: bool,
    format_args: ast::FormatArgsExpr<'t>,
    /// How many arguments it was handed, every one an expression.
    pub(crate) arg_count: usize,
    /// How many placeholders the template has, every one read.
    pub(crate) placeholder_count: usize,
    /// The template, its escapes read.
    pub(crate) template: Cow<'t, str>,
    /// Where the template's literal is written.
    pub(crate) template_range: TextRange,
}

impl<'t> FormatCall<'t> {
    /// The argument of index `index`, in the order written.
    pub(crate) fn arg(&self, index: usize) -> Option<ast::Expr<'t>> {
        self.format_args.args().nth(index)?.expr()
    }

    /// The placeholders of the template, in order.
    pub(crate) fn placeholders(&self) -> Placeholders<'_, impl Fn(&str) -> Option<usize> + '_> {
        let args = self.format_args;
        Placeholders::new(&self.template, move |name: &str| named_index(args, name))
    }
}

/// The index of the argument named `name`.
fn named_index(format_args: ast::FormatArgsExpr<'_>, name: &str) -> Option<usize> {
    format_args.args().position(|arg| arg.name().is_some_and(|it| it.text() == name))
}

/// `call` as a format macro call, if it is one of std's or one its definition marks
/// `#[clippy::format_args]` (as `log`'s are), as clippy's `is_format_macro` has it, and its
/// template is a string literal written in it.
pub(crate) fn of<'t>(cx: &Cx<'_, 't>, call: ast::MacroCall<'t>) -> Option<FormatCall<'t>> {
    let (name, std) = match cx.std_macro(call) {
        Some(name) => {
            let name = name.as_str();
            (MACROS.contains(&name).then_some(name)?, true)
        }
        None => (marked_format_args(cx, call)?, false),
    };
    let input = cx.range(call.token_tree()?.syntax())?;
    let format_args = format_args_in(cx, call, input)?;
    let ast::Expr::Literal(literal) = format_args.template()? else { return None };
    let template_range = cx.original_range(literal.syntax())?;
    let ast::LiteralKind::String(template) = literal.kind() else { return None };
    let template = template.value().ok()?;
    let mut arg_count = 0;
    for arg in format_args.args() {
        arg.expr()?;
        arg_count += 1;
    }
    let mut placeholders =
        Placeholders::new(&template, |name: &str| named_index(format_args, name));
    let placeholder_count = placeholders.by_ref().count();
    if placeholders.failed {
        return None;
    }
    Some(FormatCall {
        name,
        std,
        format_args,
        arg_count,
        placeholder_count,
        template,
        template_range,
    })
}

/// The name of the macro `call` calls, if its definition has `#[clippy::format_args]`.
fn marked_format_args<'t>(cx: &Cx<'_, 't>, call: ast::MacroCall<'t>) -> Option<&'static str> {
    let mac = cx.macro_def(call)?;
    let source = cx.macro_source(mac)?;
    let marked = source
        .attrs()
        .any(|attr| attr.path().is_some_and(|path| path.syntax().text() == "clippy::format_args"));
    marked.then(|| mac.name(cx.db()).symbol().as_str())
}

/// The `format_args!` call `call` expands to, at some depth, whose template is written in
/// `input`. The expansions are searched last found first, each to its end before the next.
fn format_args_in<'t>(
    cx: &Cx<'_, 't>,
    call: ast::MacroCall<'t>,
    input: TextRange,
) -> Option<ast::FormatArgsExpr<'t>> {
    const MOST: usize = 16;
    let mut pending: [Option<ast::Node<'t>>; MOST + 1] = [None; MOST + 1];
    pending[0] = Some(cx.expand(call)?);
    let mut len = 1;
    let mut expanded = 0;
    while len > 0 {
        len -= 1;
        let Some(root) = pending[len].take() else { continue };
        for node in root.descendants() {
            if let Some(format_args) = ast::FormatArgsExpr::cast(node) {
                let written = format_args.template().and_then(|it| cx.original_range(it.syntax()));
                if written.is_some_and(|it| input.contains_range(it)) {
                    return Some(format_args);
                }
            } else if let Some(inner) = ast::MacroCall::cast(node)
                && expanded < MOST
                && let Some(expansion) = cx.expand(inner)
            {
                expanded += 1;
                pending[len] = Some(expansion);
                len += 1;
            }
        }
    }
    None
}

/// The placeholders of a template, read as rustc reads them; `named` gives the index of a
/// named argument. Stops at the first rustc would not read, setting [`Placeholders::failed`].
pub(crate) struct Placeholders<'a, F> {
    parser: Parser<'a, F>,
    pub(crate) failed: bool,
}

impl<'a, F: Fn(&str) -> Option<usize>> Placeholders<'a, F> {
    pub(crate) fn new(template: &'a str, named: F) -> Self {
        Placeholders { parser: Parser { text: template, at: 0, next: 0, named }, failed: false }
    }
}

impl<'a, F: Fn(&str) -> Option<usize>> Iterator for Placeholders<'a, F> {
    type Item = Placeholder<'a>;

    fn next(&mut self) -> Option<Placeholder<'a>> {
        if self.failed {
            return None;
        }
        let parser = &mut self.parser;
        let found = loop {
            let Some(c) = parser.bump() else { return None };
            match c {
                '{' if parser.eat('{') => {}
                '{' => break parser.placeholder(),
                '}' if parser.eat('}') => {}
                '}' => break None,
                _ => {}
            }
        };
        self.failed = found.is_none();
        found
    }
}

/// The placeholders of `template`, its named arguments `names` given with their indices;
/// `None` if rustc would not read it.
#[cfg(test)]
fn parse<'a>(template: &'a str, names: &[(&str, usize)]) -> Option<Vec<Placeholder<'a>>> {
    let named = |name: &str| names.iter().find(|(it, _)| *it == name).map(|(_, index)| *index);
    let mut placeholders = Placeholders::new(template, named);
    let found: Vec<Placeholder<'a>> = placeholders.by_ref().collect();
    (!placeholders.failed).then_some(found)
}

/// A reader of a template, by byte offset.
struct Parser<'a, F> {
    text: &'a str,
    at: usize,
    /// The index the next implicit argument takes.
    next: usize,
    named: F,
}

impl<'a, F: Fn(&str) -> Option<usize>> Parser<'a, F> {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.text[self.at..].chars().nth(ahead)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        let eaten = self.peek() == Some(c);
        if eaten {
            self.at += c.len_utf8();
        }
        eaten
    }

    fn eat_while(&mut self, pred: impl Fn(char) -> bool) {
        while let Some(c) = self.peek()
            && pred(c)
        {
            self.at += c.len_utf8();
        }
    }

    fn integer(&mut self) -> Option<usize> {
        let start = self.at;
        self.eat_while(|it| it.is_ascii_digit());
        (self.at > start).then(|| self.text[start..self.at].parse().ok())?
    }

    fn word(&mut self) -> Option<&'a str> {
        let start = self.at;
        if !self.peek().is_some_and(|it| it == '_' || it.is_alphabetic()) {
            return None;
        }
        self.eat_while(|it| it == '_' || it.is_alphanumeric());
        Some(&self.text[start..self.at])
    }

    fn named(&self, name: &'a str) -> Arg<'a> {
        match (self.named)(name) {
            Some(index) => Arg::Index(index),
            None => Arg::Captured(name),
        }
    }

    /// A placeholder, after its `{`.
    fn placeholder(&mut self) -> Option<Placeholder<'a>> {
        self.eat_while(char::is_whitespace);
        let explicit = if let Some(index) = self.integer() {
            Some(Arg::Index(index))
        } else {
            self.word().map(|it| self.named(it))
        };
        self.eat_while(char::is_whitespace);
        let mut width = None;
        let mut precision = None;
        let mut star = false;
        let mut format_trait = Trait::Display;
        if self.eat(':') {
            if self.peek_at(1).is_some_and(|it| matches!(it, '<' | '^' | '>')) {
                self.bump();
                self.bump();
            } else if self.peek().is_some_and(|it| matches!(it, '<' | '^' | '>')) {
                self.bump();
            }
            if !self.eat('+') {
                self.eat('-');
            }
            self.eat('#');
            if self.peek() == Some('0') && self.peek_at(1) == Some('$') {
                width = self.count();
            } else {
                self.eat('0');
                width = self.count();
            }
            if self.eat('.') {
                if self.eat('*') {
                    star = true;
                } else {
                    precision = self.count();
                }
            }
            let start = self.at;
            self.eat_while(|it| it != '}');
            format_trait = match self.text[start..self.at].trim() {
                "" => Trait::Display,
                "?" | "x?" | "X?" => Trait::Debug,
                "x" | "X" | "o" | "b" | "e" | "E" | "p" => Trait::Other,
                _ => return None,
            };
        }
        if !self.eat('}') {
            return None;
        }
        // `.*` takes the next argument for the precision, before the value's.
        if star {
            precision = Some(Arg::Index(self.next));
            self.next += 1;
        }
        let arg = explicit.unwrap_or_else(|| {
            self.next += 1;
            Arg::Index(self.next - 1)
        });
        Some(Placeholder { arg, format_trait, width, precision })
    }

    /// A width or precision: a number, or an argument followed by `$`. A literal number takes
    /// no argument.
    fn count(&mut self) -> Option<Arg<'a>> {
        let start = self.at;
        if let Some(index) = self.integer() {
            if self.eat('$') {
                return Some(Arg::Index(index));
            }
            return None;
        }
        if let Some(name) = self.word() {
            if self.eat('$') {
                return Some(self.named(name));
            }
            self.at = start;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Arg, Placeholder, Trait, parse};

    fn at(index: usize, format_trait: Trait) -> Placeholder<'static> {
        Placeholder { arg: Arg::Index(index), format_trait, width: None, precision: None }
    }

    #[test]
    fn placeholders_as_rustc_reads_them() {
        assert_eq!(
            parse("{{}} {} {:?} {x}", &[]),
            Some(vec![
                at(0, Trait::Display),
                at(1, Trait::Debug),
                Placeholder { arg: Arg::Captured("x"), ..at(0, Trait::Display) },
            ])
        );
        assert_eq!(
            parse("{0:1$.2$} {:016x?} {:.*} {n:>w$}", &[("n", 5)]),
            Some(vec![
                Placeholder {
                    width: Some(Arg::Index(1)),
                    precision: Some(Arg::Index(2)),
                    ..at(0, Trait::Display)
                },
                at(0, Trait::Debug),
                Placeholder { precision: Some(Arg::Index(1)), ..at(2, Trait::Display) },
                Placeholder { width: Some(Arg::Captured("w")), ..at(5, Trait::Display) },
            ])
        );
        assert_eq!(
            parse("{:p} {:#x} {:08.3e}", &[]),
            Some(vec![at(0, Trait::Other), at(1, Trait::Other), at(2, Trait::Other),])
        );
        assert_eq!(parse("}", &[]), None);
    }

    #[test]
    fn a_multibyte_fill_is_one_character() {
        assert_eq!(
            parse("{:é>5} {}", &[]),
            Some(vec![at(0, Trait::Display), at(1, Trait::Display)])
        );
        assert_eq!(parse("{é}", &[]), Some(vec![Placeholder {
            arg: Arg::Captured("é"),
            ..at(0, Trait::Display)
        }]));
    }
}
