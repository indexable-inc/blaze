//! Parses `format_args` input.

use either::Either;
use hir_expand::name::Name;
use intern::Symbol;
use rustc_parse_format as parse;
use smallvec::{SmallVec, smallvec};
use span::SyntaxContext;
use syntax::{
    AstPtr, TextRange, ast,
    view::{self, IsString as _},
};

use crate::hir::ExprId;

#[derive(Debug, PartialEq, Eq)]
pub struct FormatArgs {
    pub template: Box<[FormatArgsPiece]>,
    pub arguments: FormatArguments,
    pub orphans: Vec<ExprId>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct FormatArguments {
    pub arguments: Box<[FormatArgument]>,
    pub num_unnamed_args: usize,
    pub num_explicit_args: usize,
    pub names: Box<[(Name, usize)]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatArgsPiece {
    Literal(Symbol),
    Placeholder(FormatPlaceholder),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatPlaceholder {
    /// Index into [`FormatArgs::arguments`].
    pub argument: FormatArgPosition,
    /// The span inside the format string for the full `{…}` placeholder.
    pub span: Option<TextRange>,
    /// `{}`, `{:?}`, or `{:x}`, etc.
    pub format_trait: FormatTrait,
    /// `{}` or `{:.5}` or `{:-^20}`, etc.
    pub format_options: FormatOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatArgPosition {
    /// Which argument this position refers to (Ok),
    /// or would've referred to if it existed (Err).
    pub index: Result<usize, Either<usize, Name>>,
    /// What kind of position this is. See [`FormatArgPositionKind`].
    pub kind: FormatArgPositionKind,
    /// The span of the name or number.
    pub span: Option<TextRange>,
}

#[derive(Copy, Debug, Clone, PartialEq, Eq)]
pub enum FormatArgPositionKind {
    /// `{}` or `{:.*}`
    Implicit,
    /// `{1}` or `{:1$}` or `{:.1$}`
    Number,
    /// `{a}` or `{:a$}` or `{:.a$}`
    Named,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum FormatTrait {
    /// `{}`
    Display,
    /// `{:?}`
    Debug,
    /// `{:e}`
    LowerExp,
    /// `{:E}`
    UpperExp,
    /// `{:o}`
    Octal,
    /// `{:p}`
    Pointer,
    /// `{:b}`
    Binary,
    /// `{:x}`
    LowerHex,
    /// `{:X}`
    UpperHex,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct FormatOptions {
    /// The width. E.g. `{:5}` or `{:width$}`.
    pub width: Option<FormatCount>,
    /// The precision. E.g. `{:.5}` or `{:.precision$}`.
    pub precision: Option<FormatCount>,
    /// The alignment. E.g. `{:>}` or `{:<}` or `{:^}`.
    pub alignment: Option<FormatAlignment>,
    /// The fill character. E.g. the `.` in `{:.>10}`.
    pub fill: Option<char>,
    /// The `+` or `-` flag.
    pub sign: Option<FormatSign>,
    /// The `#` flag.
    pub alternate: bool,
    /// The `0` flag. E.g. the `0` in `{:02x}`.
    pub zero_pad: bool,
    /// The `x` or `X` flag (for `Debug` only). E.g. the `x` in `{:x?}`.
    pub debug_hex: Option<FormatDebugHex>,
}
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FormatSign {
    /// The `+` flag.
    Plus,
    /// The `-` flag.
    Minus,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FormatDebugHex {
    /// The `x` flag in `{:x?}`.
    Lower,
    /// The `X` flag in `{:X?}`.
    Upper,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FormatAlignment {
    /// `{:<}`
    Left,
    /// `{:>}`
    Right,
    /// `{:^}`
    Center,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatCount {
    /// `{:5}` or `{:.5}`
    Literal(u16),
    /// `{:.*}`, `{:.5$}`, or `{:a$}`, etc.
    Argument(FormatArgPosition),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatArgument {
    pub kind: FormatArgumentKind,
    pub expr: ExprId,
    pub syntax: Option<AstPtr<ast::Expr>>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormatArgumentKind {
    /// `format_args(…, arg)`
    Normal,
    /// `format_args(…, arg = 1)`
    Named(Name),
    /// `format_args("… {arg} …")`
    Captured(Name),
}

// Only used in parse_args and report_invalid_references,
// to indicate how a referred argument was used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PositionUsedAs {
    Placeholder(Option<TextRange>),
    Precision,
    Width,
}
use PositionUsedAs::*;

pub(crate) fn parse(
    s: view::String<'_>,
    string_ptr: AstPtr<ast::Expr>,
    fmt_snippet: Option<String>,
    mut args: FormatArgumentsCollector,
    is_direct_literal: bool,
    mut synth: impl FnMut(Name, Option<TextRange>) -> ExprId,
    mut record_usage: impl FnMut(Name, Option<TextRange>),
    call_ctx: SyntaxContext,
) -> FormatArgs {
    let Ok(text) = s.value() else {
        return FormatArgs {
            template: Default::default(),
            arguments: args.finish(),
            // alloc: none, an empty `Vec`; and an error path
            orphans: Vec::new(),
        };
    };
    let str_style = match s.quote_offsets() {
        Some(offsets) => {
            let raw = usize::from(offsets.quotes.0.len()) - 1;
            // subtract 1 for the `r` prefix
            (raw != 0).then(|| raw - 1)
        }
        None => None,
    };
    let mut parser =
        parse::Parser::new(&text, str_style, fmt_snippet, false, parse::ParseMode::Format);

    let mut pieces = SmallVec::<[parse::Piece<'_>; 8]>::new();
    while let Some(piece) = parser.next() {
        if !parser.errors.is_empty() {
            break;
        } else {
            pieces.push(piece);
        }
    }
    let is_source_literal = parser.is_source_literal;
    if !parser.errors.is_empty() {
        // FIXME: Diagnose
        return FormatArgs {
            template: Default::default(),
            arguments: args.finish(),
            // alloc: none, an empty `Vec`; and an error path
            orphans: Vec::new(),
        };
    }

    let to_span = |inner_span: std::ops::Range<usize>| {
        is_source_literal.then(|| {
            TextRange::new(inner_span.start.try_into().unwrap(), inner_span.end.try_into().unwrap())
        })
    };

    // alloc: inline for up to 16 explicit arguments
    let mut used: SmallVec<[bool; 16]> = smallvec![false; args.explicit_args().len()];
    // alloc: pushed to only for a template with errors
    let mut invalid_refs = Vec::new();
    // alloc: pushed to only for a named argument used by position, which rustc lints
    let mut numeric_references_to_named_arg = Vec::new();

    enum ArgRef<'a> {
        Index(usize),
        Name(&'a str, Option<TextRange>),
    }
    let mut lookup_arg = |arg: ArgRef<'_>,
                          span: Option<TextRange>,
                          used_as: PositionUsedAs,
                          kind: FormatArgPositionKind|
     -> FormatArgPosition {
        let index = match arg {
            ArgRef::Index(index) => {
                if let Some(arg) = args.by_index(index) {
                    used[index] = true;
                    if arg.kind.ident().is_some() {
                        // This was a named argument, but it was used as a positional argument.
                        numeric_references_to_named_arg.push((index, span, used_as));
                    }
                    Ok(index)
                } else {
                    // Doesn't exist as an explicit argument.
                    invalid_refs.push((Either::Left(index), span, used_as, kind));
                    Err(Either::Left(index))
                }
            }
            ArgRef::Name(name, span) => {
                let name = Name::new(name, call_ctx);
                if let Some((index, _)) = args.by_name(&name) {
                    record_usage(name, span);
                    // Name found in `args`, so we resolve it to its index.
                    if index < args.explicit_args().len() {
                        // Mark it as used, if it was an explicit argument.
                        used[index] = true;
                    }
                    Ok(index)
                } else {
                    // Name not found in `args`, so we add it as an implicitly captured argument.
                    if !is_direct_literal {
                        // For the moment capturing variables from format strings expanded from macros is
                        // disabled (see RFC #2795)
                        // FIXME: Diagnose
                        // clone: error path
                        invalid_refs.push((Either::Right(name.clone()), span, used_as, kind));
                        Err(Either::Right(name))
                    } else {
                        // clone: a `Name` is an interned symbol, a count bump per capture
                        record_usage(name.clone(), span);
                        Ok(args.add(FormatArgument {
                            // clone: as `record_usage`'s
                            kind: FormatArgumentKind::Captured(name.clone()),
                            // FIXME: This is problematic, we might want to synthesize a dummy
                            // expression proper and/or desugar these.
                            expr: synth(name, span),
                            // FIXME: This will lead us to show failed trait bounds (e.g. `T: Display`)
                            // on the whole template string for implicit arguments instead of just their name.
                            // Fixing this is hard since we don't have an `AstPtr` for the arg, and it's
                            // only part of an expression.
                            syntax: Some(string_ptr),
                        }))
                    }
                }
            }
        };
        FormatArgPosition { index, kind, span }
    };

    // alloc: the output, at most one piece per parsed piece
    let mut template = Vec::with_capacity(pieces.len());
    let mut literal = PendingLiteral::default();
    let mut placeholder_index = 0;

    for piece in pieces {
        match piece {
            parse::Piece::Lit(s) => literal.push(s),
            parse::Piece::NextArgument(arg) => {
                let parse::Argument { position, position_span, format } = *arg;
                literal.flush(&mut template);

                let span =
                    parser.arg_places.get(placeholder_index).and_then(|s| to_span(s.start..s.end));
                placeholder_index += 1;

                let position_span = to_span(position_span);
                let argument = match position {
                    parse::ArgumentImplicitlyIs(i) => lookup_arg(
                        ArgRef::Index(i),
                        position_span,
                        Placeholder(span),
                        FormatArgPositionKind::Implicit,
                    ),
                    parse::ArgumentIs(i) => lookup_arg(
                        ArgRef::Index(i),
                        position_span,
                        Placeholder(span),
                        FormatArgPositionKind::Number,
                    ),
                    parse::ArgumentNamed(name) => lookup_arg(
                        ArgRef::Name(name, position_span),
                        position_span,
                        Placeholder(span),
                        FormatArgPositionKind::Named,
                    ),
                };

                let alignment = match format.align {
                    parse::AlignUnknown => None,
                    parse::AlignLeft => Some(FormatAlignment::Left),
                    parse::AlignRight => Some(FormatAlignment::Right),
                    parse::AlignCenter => Some(FormatAlignment::Center),
                };

                let format_trait = match format.ty {
                    "" => FormatTrait::Display,
                    "?" => FormatTrait::Debug,
                    "e" => FormatTrait::LowerExp,
                    "E" => FormatTrait::UpperExp,
                    "o" => FormatTrait::Octal,
                    "p" => FormatTrait::Pointer,
                    "b" => FormatTrait::Binary,
                    "x" => FormatTrait::LowerHex,
                    "X" => FormatTrait::UpperHex,
                    _ => {
                        // FIXME: Diagnose
                        FormatTrait::Display
                    }
                };

                let precision_span = format.precision_span.and_then(to_span);
                let precision = match format.precision {
                    parse::CountIs(n) => Some(FormatCount::Literal(n)),
                    parse::CountIsName(name, name_span) => Some(FormatCount::Argument(lookup_arg(
                        ArgRef::Name(name, to_span(name_span)),
                        precision_span,
                        Precision,
                        FormatArgPositionKind::Named,
                    ))),
                    parse::CountIsParam(i) => Some(FormatCount::Argument(lookup_arg(
                        ArgRef::Index(i),
                        precision_span,
                        Precision,
                        FormatArgPositionKind::Number,
                    ))),
                    parse::CountIsStar(i) => Some(FormatCount::Argument(lookup_arg(
                        ArgRef::Index(i),
                        precision_span,
                        Precision,
                        FormatArgPositionKind::Implicit,
                    ))),
                    parse::CountImplied => None,
                };

                let width_span = format.width_span.and_then(to_span);
                let width = match format.width {
                    parse::CountIs(n) => Some(FormatCount::Literal(n)),
                    parse::CountIsName(name, name_span) => Some(FormatCount::Argument(lookup_arg(
                        ArgRef::Name(name, to_span(name_span)),
                        width_span,
                        Width,
                        FormatArgPositionKind::Named,
                    ))),
                    parse::CountIsParam(i) => Some(FormatCount::Argument(lookup_arg(
                        ArgRef::Index(i),
                        width_span,
                        Width,
                        FormatArgPositionKind::Number,
                    ))),
                    parse::CountIsStar(_) => unreachable!(),
                    parse::CountImplied => None,
                };

                template.push(FormatArgsPiece::Placeholder(FormatPlaceholder {
                    argument,
                    span,
                    format_trait,
                    format_options: FormatOptions {
                        fill: format.fill,
                        alignment,
                        sign: format.sign.map(|s| match s {
                            parse::Sign::Plus => FormatSign::Plus,
                            parse::Sign::Minus => FormatSign::Minus,
                        }),
                        alternate: format.alternate,
                        zero_pad: format.zero_pad,
                        debug_hex: format.debug_hex.map(|s| match s {
                            parse::DebugHex::Lower => FormatDebugHex::Lower,
                            parse::DebugHex::Upper => FormatDebugHex::Upper,
                        }),
                        precision,
                        width,
                    },
                }));
            }
        }
    }

    literal.flush(&mut template);

    if !invalid_refs.is_empty() {
        // FIXME: Diagnose
    }

    // FIXME: Diagnose unused arguments (named ones are `FormatArgumentKind::Named`).
    let orphans = used
        .iter()
        .enumerate()
        .filter(|&(_, used)| !used)
        .map(|(i, _)| args.explicit_args()[i].expr)
        // alloc: the output; none when every argument is used
        .collect::<Vec<_>>();

    FormatArgs { template: template.into_boxed_slice(), arguments: args.finish(), orphans }
}

/// The literal pieces since the last placeholder, as one template piece: the piece itself when
/// there is one, joined only when the parser split the text (at `{{`, `}}` or an escape).
#[derive(Default)]
struct PendingLiteral<'a> {
    first: Option<&'a str>,
    joined: String,
}

impl<'a> PendingLiteral<'a> {
    fn push(&mut self, piece: &'a str) {
        if piece.is_empty() {
            return;
        }
        let Some(first) = self.first else {
            self.first = Some(piece);
            return;
        };
        if self.joined.is_empty() {
            self.joined.push_str(first);
        }
        self.joined.push_str(piece);
    }

    fn flush(&mut self, template: &mut Vec<FormatArgsPiece>) {
        let Some(first) = self.first.take() else { return };
        let text = if self.joined.is_empty() { first } else { &self.joined };
        template.push(FormatArgsPiece::Literal(Symbol::intern(text)));
        self.joined.clear();
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct FormatArgumentsCollector {
    arguments: Vec<FormatArgument>,
    num_unnamed_args: usize,
    num_explicit_args: usize,
    names: Vec<(Name, usize)>,
}

impl FormatArgumentsCollector {
    pub(crate) fn finish(self) -> FormatArguments {
        FormatArguments {
            arguments: self.arguments.into_boxed_slice(),
            num_unnamed_args: self.num_unnamed_args,
            num_explicit_args: self.num_explicit_args,
            names: self.names.into_boxed_slice(),
        }
    }

    pub fn add(&mut self, arg: FormatArgument) -> usize {
        let index = self.arguments.len();
        if let Some(name) = arg.kind.ident() {
            // clone: a `Name` is an interned symbol, a count bump per named argument
            self.names.push((name.clone(), index));
        } else if self.names.is_empty() {
            // Only count the unnamed args before the first named arg.
            // (Any later ones are errors.)
            self.num_unnamed_args += 1;
        }
        if !matches!(arg.kind, FormatArgumentKind::Captured(..)) {
            // This is an explicit argument.
            // Make sure that all arguments so far are explicit.
            assert_eq!(
                self.num_explicit_args,
                self.arguments.len(),
                "captured arguments must be added last"
            );
            self.num_explicit_args += 1;
        }
        self.arguments.push(arg);
        index
    }

    pub fn by_name(&self, name: &Name) -> Option<(usize, &FormatArgument)> {
        let &(_, i) = self.names.iter().find(|(n, _)| n == name)?;
        Some((i, &self.arguments[i]))
    }

    pub fn by_index(&self, i: usize) -> Option<&FormatArgument> {
        (i < self.num_explicit_args).then(|| &self.arguments[i])
    }

    pub fn unnamed_args(&self) -> &[FormatArgument] {
        &self.arguments[..self.num_unnamed_args]
    }

    pub fn named_args(&self) -> &[FormatArgument] {
        &self.arguments[self.num_unnamed_args..self.num_explicit_args]
    }

    pub fn explicit_args(&self) -> &[FormatArgument] {
        &self.arguments[..self.num_explicit_args]
    }

    pub fn all_args(&self) -> &[FormatArgument] {
        &self.arguments[..]
    }

    pub fn all_args_mut(&mut self) -> &mut Vec<FormatArgument> {
        &mut self.arguments
    }
}

impl FormatArgumentKind {
    pub fn ident(&self) -> Option<&Name> {
        match self {
            Self::Normal => None,
            Self::Named(id) => Some(id),
            Self::Captured(id) => Some(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pieces of each run of literals become one piece, the concatenation, as the `String`
    /// the parser's pieces were once joined in made; an empty run makes none.
    #[test]
    fn literal_pieces_join_as_a_string_does() {
        let runs: [&[&str]; 6] =
            [&[], &[""], &["a"], &["a", "", "{", "b"], &["", "}"], &["x", "y", "z"]];
        let mut literal = PendingLiteral::default();
        let mut template = Vec::new();
        let mut want = Vec::new();
        for run in runs {
            for &piece in run {
                literal.push(piece);
            }
            literal.flush(&mut template);
            let joined = run.concat();
            if !joined.is_empty() {
                want.push(FormatArgsPiece::Literal(Symbol::intern(&joined)));
            }
            assert!(literal.first.is_none() && literal.joined.is_empty(), "flushed after {run:?}");
        }
        assert_eq!(template, want);
    }
}
