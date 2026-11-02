//! Builtin macro

mod args;

use std::{borrow::Cow, sync::OnceLock};

use base_db::{AnchoredPath, Db};
use blaze_mem::Frame;
use cfg::CfgExpr;
use either::Either;
use intern::{Symbol, sym};
use itertools::Itertools;
use mbe::{DelimiterKind, expect_fragment};
use span::{Edition, FileId, Span};
use stdx::edit_distance::edit_distance;
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self, AstNode as _},
};
use self::args::{Arg, Array, IntSuffix, Lit, lit_value};
use crate::{
    BuiltinError, EditionedFileId, ExpandError, ExpandResult, FileRange, HirFileId, InFile,
    MacroCallId,
    builtin::quote::{WithDelimiter, dollar_crate, emit, group, str_literal},
    hygiene::{span_with_call_site_ctxt, span_with_def_site_ctxt},
    name,
    tt::{self, DelimSpan, TtElement},
};

macro_rules! register_builtin {
    ( $EXPANDER:ident: $(($name:ident, $kind: ident) => $expand:ident),* $(,)? ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $EXPANDER {
            $($kind),*
        }

        impl $EXPANDER {
            /// Every variant, in declaration order.
            pub const ALL: &[Self] = &[$(Self::$kind),*];

            fn expander(&self) -> for<'f> fn (&Db, &'f Frame, MacroCallId, &tt::TopSubtree, Span) -> ExpandResult<tt::TopSubtree<&'f Frame>>  {
                match *self {
                    $( Self::$kind => $expand, )*
                }
            }

            /// The builtin `ident` names: its declared index and one load. A name given twice
            /// is its first.
            fn find_by_name(ident: &name::Name) -> Option<Self> {
                static BY_SYMBOL: intern::KnownNames =
                    intern::KnownNames::new(&[$( sym::Declared::$name, )*]);
                BY_SYMBOL.get(ident.symbol()).map(|at| Self::ALL[usize::from(at)])
            }

            /// Each builtin's name, in declaration order.
            #[cfg(test)]
            const NAMES: &[Symbol] = &[$( sym::$name, )*];

            /// [`Self::find_by_name`] as a chain of compares, the reference it is tested by.
            #[cfg(test)]
            fn find_by_name_by_compare(ident: &name::Name) -> Option<Self> {
                match ident {
                    $( id if *id == sym::$name => Some(Self::$kind), )*
                    _ => None,
                }
            }
        }
    }
}

impl BuiltinFnLikeExpander {
    pub fn expand<'f>(
        &self,
        db: &Db,
        frame: &'f Frame,
        id: MacroCallId,
        tt: &tt::TopSubtree,
        span: Span,
    ) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
        let span = span_with_def_site_ctxt(db, span, id.into(), Edition::CURRENT);
        self.expander()(db, frame, id, tt, span)
    }

    pub fn is_asm(&self) -> bool {
        matches!(self, Self::Asm | Self::GlobalAsm | Self::NakedAsm)
    }
}

impl EagerExpander {
    pub fn expand<'f>(
        &self,
        db: &Db,
        frame: &'f Frame,
        id: MacroCallId,
        tt: &tt::TopSubtree,
        span: Span,
    ) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
        let span = span_with_def_site_ctxt(db, span, id.into(), Edition::CURRENT);
        self.expander()(db, frame, id, tt, span)
    }

    pub fn is_include(&self) -> bool {
        matches!(self, EagerExpander::Include)
    }

    pub fn is_include_like(&self) -> bool {
        matches!(
            self,
            EagerExpander::Include | EagerExpander::IncludeStr | EagerExpander::IncludeBytes
        )
    }

    pub fn is_env_or_option_env(&self) -> bool {
        matches!(self, EagerExpander::Env | EagerExpander::OptionEnv)
    }
}

pub fn find_builtin_macro(
    ident: &name::Name,
) -> Option<Either<BuiltinFnLikeExpander, EagerExpander>> {
    (BuiltinFnLikeExpander::find_by_name(ident).map(Either::Left))
        .or_else(|| EagerExpander::find_by_name(ident).map(Either::Right))
}

register_builtin! {
    BuiltinFnLikeExpander:
    (column, Column) => column_expand,
    (file, File) => file_expand,
    (line, Line) => line_expand,
    (module_path, ModulePath) => module_path_expand,
    (assert, Assert) => assert_expand,
    (stringify, Stringify) => stringify_expand,
    (asm, Asm) => asm_expand,
    (global_asm, GlobalAsm) => global_asm_expand,
    (naked_asm, NakedAsm) => naked_asm_expand,
    (cfg_select, CfgSelect) => cfg_select_expand,
    (cfg, Cfg) => cfg_expand,
    (core_panic, CorePanic) => panic_expand,
    (std_panic, StdPanic) => panic_expand,
    (unreachable, Unreachable) => unreachable_expand,
    (log_syntax, LogSyntax) => log_syntax_expand,
    (trace_macros, TraceMacros) => trace_macros_expand,
    (format_args, FormatArgs) => format_args_expand,
    (const_format_args, ConstFormatArgs) => format_args_expand,
    (format_args_nl, FormatArgsNl) => format_args_nl_expand,
    (quote, Quote) => quote_expand,
    (pattern_type, PatternType) => pattern_type_expand,
    (gca, Gca) => gca_expand,
}

register_builtin! {
    EagerExpander:
    (compile_error, CompileError) => compile_error_expand,
    (concat, Concat) => concat_expand,
    (concat_bytes, ConcatBytes) => concat_bytes_expand,
    (include, Include) => include_expand,
    (include_bytes, IncludeBytes) => include_bytes_expand,
    (include_str, IncludeStr) => include_str_expand,
    (env, Env) => env_expand,
    (option_env, OptionEnv) => option_env_expand,
}

fn mk_pound(span: Span) -> tt::Leaf {
    crate::tt::Leaf::Punct(crate::tt::Punct { char: '#', spacing: crate::tt::Spacing::Alone, span })
}

/// The string literal of `text`, a constant: escaped and interned on the first call, then one
/// acquire load.
fn constant_str_literal(cell: &'static OnceLock<Symbol>, text: &str, span: Span) -> tt::Literal {
    tt::Literal {
        text_and_suffix: *cell.get_or_init(|| str_literal(text, span).text_and_suffix),
        span,
        kind: tt::LitKind::Str,
        suffix_len: 0,
    }
}

/// The call's arguments in parentheses at the spans of its own delimiters: the tree spliced with
/// its top subtree made a parenthesis, read where it is instead of cloned to be changed.
fn parenthesized(tt: &tt::TopSubtree) -> WithDelimiter<'_> {
    WithDelimiter {
        delimiter: tt::Delimiter {
            kind: tt::DelimiterKind::Parenthesis,
            ..tt.top_subtree().delimiter
        },
        token_trees: tt.token_trees(),
    }
}

fn module_path_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    static PLACEHOLDER: OnceLock<Symbol> = OnceLock::new();
    let placeholder = constant_str_literal(&PLACEHOLDER, MODULE_PATH_PLACEHOLDER, span);
    ExpandResult::ok(quote_in! {frame, span =>
         #placeholder
    })
}

/// What `module_path!()` expands to: the expander doesn't know modules, and whoever lowers a
/// literal with this in it puts the path of the module there.
pub const MODULE_PATH_PLACEHOLDER: &str = "\u{0}module_path\u{0}";

fn line_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    place_expand(db, frame, id, tt, span, "line!", |(line, _)| line)
}

fn column_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    place_expand(db, frame, id, tt, span, "column!", |(_, column)| column)
}

/// `line!()` or `column!()`: the `u32` that `pick` takes of [`written_place`], as rustc's
/// `expand_line` and `expand_column` give it. Arguments are an error, after which the value is
/// given all the same (rustc's `check_zero_tts`).
fn place_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
    name: &str,
    pick: fn((u32, u32)) -> u32,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let err = (!tt.token_trees().is_empty()).then(|| {
        // alloc: an error's message, on the error path
        BuiltinError::new(format!("{name} takes no arguments")).at(call_span(db, id, span))
    });
    let value = match written_place(db, id) {
        Some(place) => pick(place),
        None => {
            // alloc: an error's message, on the error path
            let message = format!("`{name}`: the call is not in its file's text");
            let err = ExpandError::other(span, message);
            let empty = tt::TopSubtree::empty_in(DelimSpan::from_single(span), frame);
            return ExpandResult::new(empty, err);
        }
    };
    let mut text = [0u8; 13];
    let mut rest = &mut text[..];
    std::io::Write::write_fmt(&mut rest, format_args!("{value}u32"))
        .expect("a `u32`'s 10 digits and its suffix fit in 13 bytes");
    let len = 13 - rest.len();
    let literal = tt::Literal {
        text_and_suffix: Symbol::intern(
            std::str::from_utf8(&text[..len]).expect("digits and `u32` are ASCII"),
        ),
        span,
        kind: tt::LitKind::Integer,
        suffix_len: 3,
    };
    let expanded =
        tt::TopSubtree::invisible_from_leaves_in(span, [tt::Leaf::Literal(literal)], frame);
    match err {
        Some(err) => ExpandResult::new(expanded, err),
        None => ExpandResult::ok(expanded),
    }
}

/// The line and column, both from 1, where the macro call `id` comes from in the end (rustc's
/// `expansion_cause`): the column in code points, as `column!` counts them.
pub fn written_place(db: &Db, id: MacroCallId) -> Option<(u32, u32)> {
    let call = written_call(db, id);
    let range = InFile::new(call.file_id, view::written_range(call.value));
    line_and_column(db, range.original_node_file_range_rooted(db))
}

/// The call `id` is, when it is written in a file or in what a macro was passed; else the call
/// of the macro whose expansion wrote it, and so on up. The tokens of an `include!`d file are
/// its own, so the walk stops in it, as rustc's stops at `include!`.
fn written_call(db: &Db, id: MacroCallId) -> InFile<view::Node<'_>> {
    let mut call = id.loc(db).to_node_view(db);
    while let Some(outer) = call.file_id.macro_file() {
        let start = TextRange::empty(view::written_range(call.value).start());
        if call.file_id.span_map(db).span_for_range(start).ctx.is_root() {
            break;
        }
        call = outer.loc(db).to_node_view(db);
    }
    call
}

/// rustc's span of the call `id`, the `sp` its expander is given: from the macro's path to the
/// closing delimiter, in the context of `span`.
fn call_span(db: &Db, id: MacroCallId, span: Span) -> Span {
    let call = id.loc(db).to_node_view(db);
    let mut range = view::written_range(call.value);
    let map = call.file_id.span_map(db);
    // The `;` of a call in item position is the call's in the syntax tree, not in rustc's span.
    if let Some(args) = view::MacroCall::cast(call.value).and_then(|it| it.token_tree()) {
        range = TextRange::new(range.start(), args.syntax().text_range().end());
    }
    let first = map.span_for_range(range);
    let Some(end) = range.end().checked_sub(TextSize::new(1)).filter(|_| !range.is_empty()) else {
        return Span { ctx: span.ctx, ..first };
    };
    let last = map.span_for_range(TextRange::at(end, TextSize::new(1)));
    Span { ctx: span.ctx, ..first.cover(last) }
}

/// The line and column, from 1, where `range` begins, as rustc's source map counts them: a
/// line ends at each `'\n'`, a column is a code point, a byte order mark that starts the
/// file is no column, and the `\r` of a `\r\n` is no column of its `\n`.
pub fn line_and_column(db: &Db, range: FileRange) -> Option<(u32, u32)> {
    let text = db.file_text(range.file_id.file_id(db));
    let place = base_db::line_index(db, text).place(range.range.start())?;
    Some((place.line(), place.char_column(text.text(db))?))
}

fn log_syntax_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    ExpandResult::ok(quote_in! {frame, span =>})
}

fn trace_macros_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    ExpandResult::ok(quote_in! {frame, span =>})
}

/// Whether the source has another token right after the token `text` at `span`. `None` if the
/// source does not have that token there, which is the case for a token made by a macro.
fn source_joined(db: &Db, span: Span, text: &str) -> Option<bool> {
    let crate::FileRange { file_id, range } = crate::resolve_span(db, span);
    let source = db.file_text(file_id.file_id(db)).text(db);
    if source.get(std::ops::Range::<usize>::from(range))? != text {
        return None;
    }
    let rest = &source[usize::from(range.end())..];
    let is_trivia = rest.starts_with(char::is_whitespace)
        || rest.starts_with("//")
        || rest.starts_with("/*")
        || rest.is_empty();
    Some(!is_trivia)
}

/// Whether the tokens of the source a call of `id` is given came out of a proc macro on their
/// way there: some expansion it is in is a proc macro's.
fn through_proc_macro(db: &Db, id: MacroCallId) -> bool {
    let mut file = id.parent(db);
    while let HirFileId::MacroFile(call) = file {
        if call.loc(db).def.is_proc_macro() {
            return true;
        }
        file = call.parent(db);
    }
    false
}

fn stringify_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let pretty = ::tt::stringify(
        tt.token_trees(),
        &|span, text| source_joined(db, span, text),
        through_proc_macro(db, id),
    );

    let expanded = quote_in! {frame, span =>
        #pretty
    };

    ExpandResult::ok(expanded)
}

/// Formats the binary operators and paths in the expression reported by a failed assertion.
/// `stringify!` instead preserves the token spacing supplied by the caller. Appended to `out`.
fn write_assert_condition(out: &mut String, tokens: &str, edition: Edition) {
    use syntax::{SyntaxKind, T};

    let parse = syntax::ast::Expr::parse(tokens, edition);
    let expr = parse.root();
    let start = out.len();
    let mut whitespace = false;
    let mut after_operator = false;
    let mut after_separator = false;
    for token in expr.descendants_with_tokens().filter_map(|it| it.into_token()) {
        if token.kind() == SyntaxKind::WHITESPACE {
            whitespace = true;
            continue;
        }
        let parent = token.parent().map(|it| it.kind());
        let operator = parent == Some(SyntaxKind::BIN_EXPR);
        let separator = token.kind() == T![::] && parent == Some(SyntaxKind::PATH);
        if out.len() > start
            && (operator || after_operator || (whitespace && !separator && !after_separator))
        {
            out.push(' ');
        }
        out.push_str(token.text());
        whitespace = false;
        after_operator = operator;
        after_separator = separator;
    }
}

fn assert_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let call_site_span = span_with_call_site_ctxt(db, span, id.into(), Edition::CURRENT);

    let mut iter = tt.iter();

    let cond = expect_fragment(
        db,
        &mut iter,
        parser::PrefixEntryPoint::Expr,
        tt.top_subtree().delimiter.delim_span(),
    );
    _ = iter.expect_char(',');
    let rest = iter.remaining();

    let dollar_crate = dollar_crate(span);
    // Without a message of its own, the assertion reports the condition.
    let message = rest.is_empty().then(|| {
        let tokens = ::tt::stringify(
            cond.value,
            &|span, text| source_joined(db, span, text),
            through_proc_macro(db, id),
        );
        const HEAD: &str = "assertion failed: ";
        let mut message = blaze_mem::scratch::lend_string();
        message.push_str(HEAD);
        write_assert_condition(&mut message, &tokens, span.ctx.edition(db));
        message
    });
    let panic_2021 = message.is_none() && use_panic_2021(db, span);
    // Written in the expansion's own builder, in the invisible group a spliced `quote!` adds.
    let mac = group(call_site_span, |b| {
        let panic_args = rest.iter();
        match &message {
            Some(message) => {
                let message: &str = message;
                quote_into!(b, call_site_span => #dollar_crate::panicking::panic(#message))
            }
            None if panic_2021 => {
                quote_into!(b, call_site_span => #dollar_crate::panic::panic_2021!(# #panic_args))
            }
            // Unqualified at the call site, as rustc's `panic_path`: std's `panic!` in a std
            // crate, whose 2015 arm takes any payload, and core's only under `no_std`.
            None => quote_into!(b, call_site_span => panic!(# #panic_args)),
        }
    });
    let value = cond.value;
    let expanded = quote_in! {frame, call_site_span =>{
        if !(#value) {
            #mac;
        }
    }};

    match cond.err {
        Some(err) => ExpandResult::new(expanded, err.into()),
        None => ExpandResult::ok(expanded),
    }
}

fn file_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    // FIXME: RA purposefully lacks knowledge of absolute file names
    // so just return "".
    static FILE: OnceLock<Symbol> = OnceLock::new();
    let file_name = constant_str_literal(&FILE, "file", span);

    let expanded = quote_in! {frame, span =>
        #file_name
    };

    ExpandResult::ok(expanded)
}

fn format_args_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let pound = mk_pound(span);
    let args = parenthesized(tt);
    ExpandResult::ok(quote_in! {frame, span =>
        builtin #pound format_args #args
    })
}

fn format_args_nl_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    match format_args_nl_of_literal(frame, tt, span) {
        Some(expanded) => ExpandResult::ok(expanded),
        None => format_args_nl_of_macro(db, frame, tt, span),
    }
}

/// `format_args_nl!` whose template is a string literal: `format_args!` of the literal with a
/// newline appended. `None` if the first token is not a string literal.
fn format_args_nl_of_literal<'f>(
    frame: &'f Frame,
    tt: &tt::TopSubtree,
    span: Span,
) -> Option<tt::TopSubtree<&'f Frame>> {
    let mut args = tt.iter();
    let Some(tt::TtElement::Leaf(tt::Leaf::Literal(
        lit @ tt::Literal { kind: tt::LitKind::Str | tt::LitKind::StrRaw(_), .. },
    ))) = args.next()
    else {
        return None;
    };
    // A raw string has no escapes: the newline goes in as it is.
    let newline = if lit.kind == tt::LitKind::Str { "\\n" } else { "\n" };
    let (text, suffix) = lit.text_and_suffix();
    let mut joined = blaze_mem::scratch::lend_string();
    joined.push_str(text);
    joined.push_str(newline);
    joined.push_str(suffix);
    let template = tt::Literal { text_and_suffix: Symbol::intern(&joined), ..lit };
    let rest = args.remaining();
    // The arguments with the template's text changed, written where they are read: the tree is
    // not cloned and rebuilt to change one token.
    let delimiter = parenthesized(tt).delimiter;
    let args = emit(|b: &mut tt::TopSubtreeBuilder| {
        b.open(delimiter.kind, delimiter.open);
        b.push(template.clone().into()); // clone: a copy (symbol and span)
        b.extend_with_tt(rest);
        b.close(delimiter.close);
    });
    let pound = mk_pound(span);
    Some(quote_in! {frame, span =>
        builtin #pound format_args #args
    })
}

/// `format_args_nl!` whose template is not written as a string: a macro that expands to one.
///
/// rustc expands the template and then appends the newline to the string it got
/// (`expand_format_args_impl` with `append_newline` in
/// `compiler/rustc_builtin_macros/src/format.rs`). Here the template becomes
/// `concat!(template, "\n")`, which expands to that same string.
fn format_args_nl_of_macro<'f>(
    db: &Db,
    frame: &'f Frame,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let pound = mk_pound(span);
    let mut iter = tt.iter();
    let template = expect_fragment(
        db,
        &mut iter,
        parser::PrefixEntryPoint::Expr,
        tt.top_subtree().delimiter.delim_span(),
    );
    let rest = iter.remaining();
    let arguments = rest.iter();
    let dollar_crate = dollar_crate(span);
    let value = template.value;
    let expanded = quote_in! {frame, span =>
        builtin #pound format_args (#dollar_crate::concat!(#value, "\n") # #arguments)
    };
    match template.err {
        Some(err) => ExpandResult::new(expanded, err.into()),
        None => ExpandResult::ok(expanded),
    }
}

fn asm_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let args = parenthesized(tt);
    let pound = mk_pound(span);
    let expanded = quote_in! {frame, span =>
        builtin #pound asm #args
    };
    ExpandResult::ok(expanded)
}

fn global_asm_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let args = parenthesized(tt);
    let pound = mk_pound(span);
    let expanded = quote_in! {frame, span =>
        builtin #pound global_asm #args
    };
    ExpandResult::ok(expanded)
}

fn naked_asm_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let args = parenthesized(tt);
    let pound = mk_pound(span);
    let expanded = quote_in! {frame, span =>
        builtin #pound naked_asm #args
    };
    ExpandResult::ok(expanded)
}

fn cfg_select_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let loc = id.loc(db);
    let cfg_options = loc.krate.cfg_options(db);

    let mut iter = tt.iter();
    let mut expand_to = None;
    while let Some(next) = iter.peek() {
        let active = if let tt::TtElement::Leaf(tt::Leaf::Ident(ident)) = next
            && ident.sym == sym::underscore
        {
            iter.next();
            true
        } else {
            cfg_options.check(&CfgExpr::parse_from_iter(&mut iter)) != Some(false)
        };
        match iter.expect_glued_punct() {
            Ok(it) if it.len() == 2 && it[0].char == '=' && it[1].char == '>' => {}
            _ => {
                let err_span = iter.peek().map(|it| it.first_span()).unwrap_or(span);
                return ExpandResult::new(
                    tt::TopSubtree::empty_in(tt::DelimSpan::from_single(span), frame),
                    ExpandError::other(err_span, "expected `=>` after cfg expression"),
                );
            }
        }
        let expand_to_if_active = match iter.peek() {
            Some(tt::TtElement::Subtree(sub, tt)) if sub.delimiter.kind == DelimiterKind::Brace => {
                iter.next();
                tt.remaining()
            }
            None | Some(TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: ',', .. }))) => {
                let err_span = iter.peek().map(|it| it.first_span()).unwrap_or(span);
                iter.next();
                return ExpandResult::new(
                    tt::TopSubtree::empty_in(tt::DelimSpan::from_single(span), frame),
                    ExpandError::other(err_span, "expected a token tree after `=>`"),
                );
            }
            Some(_) => {
                let expr = expect_fragment(
                    db,
                    &mut iter,
                    parser::PrefixEntryPoint::Expr,
                    tt.top_subtree().delimiter.delim_span(),
                );
                if let Some(err) = expr.err {
                    return ExpandResult::new(
                        tt::TopSubtree::empty_in(tt::DelimSpan::from_single(span), frame),
                        err.into(),
                    );
                }
                expr.value
            }
        };
        if let Some(TtElement::Leaf(tt::Leaf::Punct(p))) = iter.peek()
            && p.char == ','
        {
            iter.next();
        }

        if expand_to.is_none() && active {
            expand_to = Some(expand_to_if_active);
        }
    }
    match expand_to {
        Some(expand_to) => {
            let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter {
                kind: tt::DelimiterKind::Invisible,
                open: span,
                close: span,
            });
            builder.extend_with_tt(expand_to);
            ExpandResult::ok(builder.build_in(frame))
        }
        None => ExpandResult::new(
            tt::TopSubtree::empty_in(tt::DelimSpan::from_single(span), frame),
            BuiltinError::new("none of the predicates in this `cfg_select` evaluated to true")
                .at(call_span(db, id, span)),
        ),
    }
}

fn cfg_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let pound = mk_pound(span);
    // Support ide features, like completion and hover
    // This implementation should not be used for 'cfg_select' because 'cfg_select' expands any token.
    let expanded = quote_in! {frame, span =>
        builtin #pound cfg #tt
    };
    ExpandResult::ok(expanded)
}

fn panic_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let dollar_crate = dollar_crate(span);
    let call_site_span = span_with_call_site_ctxt(db, span, id.into(), Edition::CURRENT);

    let mac = if use_panic_2021(db, span) { sym::panic_2021 } else { sym::panic_2015 };

    // Pass the original arguments
    let subtree = WithDelimiter {
        delimiter: tt::Delimiter {
            open: call_site_span,
            close: call_site_span,
            kind: tt::DelimiterKind::Parenthesis,
        },
        token_trees: tt.token_trees(),
    };

    // Expand to a macro call `$crate::panic::panic_{edition}`
    let call = quote_in!(frame, call_site_span =>#dollar_crate::panic::#mac! #subtree);

    ExpandResult::ok(call)
}

fn unreachable_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let dollar_crate = dollar_crate(span);
    let call_site_span = span_with_call_site_ctxt(db, span, id.into(), Edition::CURRENT);

    let mac = if use_panic_2021(db, span) { sym::unreachable_2021 } else { sym::unreachable_2015 };

    // Pass the original arguments
    let subtree = WithDelimiter {
        delimiter: tt::Delimiter {
            open: call_site_span,
            close: call_site_span,
            kind: tt::DelimiterKind::Parenthesis,
        },
        token_trees: tt.token_trees(),
    };

    // Expand to a macro call `$crate::panic::panic_{edition}`
    let call = quote_in!(frame, call_site_span =>#dollar_crate::panic::#mac! #subtree);

    ExpandResult::ok(call)
}

/// Whether `panic!` written at `span` is the one of edition 2021: the edition of where it is
/// written, or of the macro that wrote it. A macro of the standard library that writes one
/// (`assert!`, `debug_assert!`) has `#[allow_internal_unstable(edition_panic)]`, which only
/// such a macro can have, and stands for where it is called itself.
fn use_panic_2021(db: &Db, span: Span) -> bool {
    // To determine the edition, we check the first span up the expansion
    // stack that does not have #[allow_internal_unstable(edition_panic)].
    // (To avoid using the edition of e.g. the assert!() or debug_assert!() definition.)
    let mut ctx = span.ctx;
    loop {
        let Some(expn) = ctx.outer_expn(db) else {
            break ctx.edition(db) >= Edition::Edition2021;
        };
        let expn = crate::MacroCallId::from(expn).loc(db);
        if expn.def.edition_panic {
            ctx = expn.ctxt;
            continue;
        }
        break expn.def.edition >= Edition::Edition2021;
    }
}

fn compile_error_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let err = match tt.iter().collect_array() {
        Some(
            [
                tt::TtElement::Leaf(tt::Leaf::Literal(
                    lit @ tt::Literal { kind: tt::LitKind::Str | tt::LitKind::StrRaw(_), .. },
                )),
            ],
        ) => ExpandError::other(span, Box::from(unescape_str(lit.text()))),
        _ => ExpandError::other(span, "`compile_error!` argument must be a string"),
    };

    ExpandResult { value: quote_in! {frame, span =>}, err: Some(err) }
}

/// rustc's `expand_concat`. The string is at the cover of the spans of the string, character
/// and boolean literals it is made of, else at the call's open delimiter.
fn concat_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    _arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    _call_site: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let open = tt.top_subtree().delimiter.open;
    let exprs = match args::exprs(db, tt) {
        Ok(it) => it,
        Err(err) => return ExpandResult::new(quote_in!(frame, open => ""), err),
    };
    concat_text_in(frame, &exprs, open)
}

/// [`concat_expand`] of `tt`'s arguments split at its top-level commas: the budget test's form,
/// which has no database to parse them as expressions.
#[cfg(test)]
pub(crate) fn concat_of_literals_in<'f>(
    frame: &'f Frame,
    tt: &tt::TopSubtree,
    open: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    concat_text_in(frame, &args::comma_separated(tt), open)
}

/// `concat!`'s string of its arguments, `open` its span when no argument gives one.
fn concat_text_in<'f>(
    frame: &'f Frame,
    exprs: &[args::Arg<'_>],
    open: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let mut span: Option<Span> = None;
    let mut record_span = |s: Span| match &mut span {
        Some(span) if span.anchor == s.anchor => span.range = span.range.cover(s.range),
        Some(_) => (),
        None => span = Some(s),
    };
    let mut text = blaze_mem::scratch::lend_string();
    let mut err = None;
    // alloc: the arguments that are no literal, on the error path
    let mut missing = Vec::new();
    for arg in exprs {
        if let Some(lit) = arg.literal() {
            match lit_value(&lit) {
                Ok(Lit::Str(it)) => {
                    text.push_str(&it);
                    record_span(lit.span);
                }
                Ok(Lit::Float(it)) => text.push_str(&it),
                Ok(Lit::Char(it)) => {
                    text.push(it);
                    record_span(lit.span);
                }
                Ok(Lit::Int(it, _)) => {
                    use std::fmt::Write as _;
                    write!(text, "{it}").expect("writing to a `String` doesn't fail");
                }
                Ok(Lit::CStr) => {
                    err.get_or_insert(
                        BuiltinError::new("cannot concatenate a C string literal").at(arg.span),
                    );
                }
                Ok(Lit::Byte(_) | Lit::ByteStr(_)) => {
                    err.get_or_insert(
                        BuiltinError::new("cannot concatenate a byte string literal").at(arg.span),
                    );
                }
                // The lexer has reported it.
                Ok(Lit::Err) => {}
                Err(e) => {
                    err.get_or_insert(e.at(arg.span));
                }
            }
        } else if let Some(value) = arg.bool() {
            text.push_str(if value { "true" } else { "false" });
            if let Some(it) = arg.tokens.first_span() {
                record_span(it);
            }
        } else if let Some(lit) = arg.negated_literal() {
            match lit_value(&lit) {
                Ok(Lit::Int(it, _)) => {
                    use std::fmt::Write as _;
                    write!(text, "-{it}").expect("writing to a `String` doesn't fail");
                }
                Ok(Lit::Float(it)) => {
                    text.push('-');
                    text.push_str(&it);
                }
                Err(e) => {
                    err.get_or_insert(e.at(lit.span));
                }
                Ok(_) => missing.push(arg.span),
            }
        } else {
            missing.push(arg.span);
        }
    }
    let err = err.or_else(|| {
        let (&first, rest) = missing.split_first()?;
        Some(
            BuiltinError::new("expected a literal")
                .note("only literals (like `\"foo\"`, `-42` and `3.14`) can be passed to `concat!()`")
                .also_at(rest)
                .at(first),
        )
    });
    let span = span.unwrap_or(open);
    let text: &str = &text;
    ExpandResult { value: quote_in!(frame, span =>#text), err }
}

/// rustc's `expand_concat_bytes`. The literal has the call's span, as rustc's `expr_byte_str(sp,
/// ..)` gives it.
fn concat_bytes_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    _arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let byte_str = |bytes: &[u8]| {
        let literal = blaze_mem::scratch::with_bytes(|out| {
            tt::Literal::new_no_suffix(escape_ascii(bytes, out), call_site, tt::LitKind::ByteStr)
        });
        tt::TopSubtree::invisible_from_leaves_in(call_site, [tt::Leaf::Literal(literal)], frame)
    };
    let exprs = match args::exprs(db, tt) {
        Ok(it) => it,
        Err(err) => return ExpandResult::new(byte_str(&[]), err),
    };
    let mut bytes = blaze_mem::scratch::lend_bytes();
    let mut err = None;
    // alloc: the arguments that are no byte literal, on the error path
    let mut missing = Vec::new();
    for arg in &exprs {
        if let Some(array) = args::array(db, arg) {
            match array {
                Ok(Array::List(elements)) => {
                    for element in &elements {
                        if let Some(byte) = array_element(db, element, &mut err, &mut missing) {
                            bytes.push(byte);
                        }
                    }
                }
                Ok(Array::Repeat(element, count)) => {
                    let count_value = count.literal().and_then(|it| match lit_value(&it) {
                        Ok(Lit::Int(value, _)) => Some(value),
                        _ => None,
                    });
                    match count_value {
                        Some(count) => {
                            if let Some(byte) =
                                array_element(db, &element, &mut err, &mut missing)
                            {
                                let count = usize::try_from(count).unwrap_or(usize::MAX);
                                let len = bytes.len().saturating_add(count);
                                bytes.resize(len, byte);
                            }
                        }
                        None => {
                            err.get_or_insert(
                                BuiltinError::new("repeat count is not a positive number")
                                    .at(count.span),
                            );
                        }
                    }
                }
                Err(e) => {
                    err.get_or_insert(e);
                }
            }
        } else if let Some(lit) = arg.literal() {
            match lit_value(&lit) {
                Ok(Lit::Byte(byte)) => bytes.push(byte),
                Ok(Lit::ByteStr(it)) => bytes.extend_from_slice(&it),
                Ok(Lit::Err) => {}
                Ok(value) => {
                    err.get_or_insert(invalid_byte_literal(&lit, &value, false).at(arg.span));
                }
                Err(e) => {
                    err.get_or_insert(e.at(arg.span));
                }
            }
        } else if arg.bool().is_some() {
            err.get_or_insert(BuiltinError::new("cannot concatenate boolean literals").at(arg.span));
        } else {
            missing.push(arg.span);
        }
    }
    let err = err.or_else(|| {
        let (&first, rest) = missing.split_first()?;
        Some(
            BuiltinError::new("expected a byte literal")
                .note(
                    "only byte literals (like `b\"foo\"`, `b's'` and `[3, 4, 5]`) can be passed \
                     to `concat_bytes!()`",
                )
                .also_at(rest)
                .at(first),
        )
    });
    ExpandResult { value: byte_str(&bytes), err }
}

/// rustc's `handle_array_element`: the byte an element of an array argument of `concat_bytes!`
/// is.
fn array_element(
    db: &Db,
    element: &Arg<'_>,
    err: &mut Option<ExpandError>,
    missing: &mut Vec<Span>,
) -> Option<u8> {
    if let Some(lit) = element.literal() {
        match lit_value(&lit) {
            Ok(Lit::Int(value, IntSuffix::None | IntSuffix::U8)) if value <= u128::from(u8::MAX) => {
                return Some(value as u8);
            }
            Ok(Lit::Byte(byte)) => return Some(byte),
            Ok(Lit::ByteStr(_)) => {
                err.get_or_insert(
                    BuiltinError::new("cannot concatenate doubly nested array")
                        .note("byte strings are treated as arrays of bytes")
                        .help("try flattening the array")
                        .at(element.span),
                );
            }
            Ok(Lit::Err) => {}
            Ok(value) => {
                err.get_or_insert(invalid_byte_literal(&lit, &value, true).at(element.span));
            }
            Err(e) => {
                err.get_or_insert(e.at(element.span));
            }
        }
    } else if element.bool().is_some() {
        err.get_or_insert(BuiltinError::new("cannot concatenate boolean literals").at(element.span));
    } else if args::array(db, element).is_some() {
        err.get_or_insert(BuiltinError::new("cannot concatenate doubly nested array").at(element.span));
    } else {
        missing.push(element.span);
    }
    None
}

/// rustc's `invalid_type_err` for `lit`, of the value `value` that is neither a byte nor a byte
/// string, in an array argument or not.
fn invalid_byte_literal(lit: &tt::Literal, value: &Lit<'_>, is_nested: bool) -> BuiltinError {
    const C_STR_NOTE: &str = "concatenating C strings is ambiguous about including the '\\0'";
    match value {
        Lit::CStr => {
            let error = BuiltinError::new("cannot concatenate C string literals");
            // The suggestion of a cooked `c"..."` brings a note of its own.
            let error = if lit.kind == tt::LitKind::CStr { error.note(C_STR_NOTE) } else { error };
            error.note(C_STR_NOTE)
        }
        Lit::Char(_) => BuiltinError::new("cannot concatenate character literals"),
        Lit::Str(_) => BuiltinError::new("cannot concatenate string literals"),
        Lit::Float(_) => BuiltinError::new("cannot concatenate float literals"),
        Lit::Int(..) if !is_nested => BuiltinError::new("cannot concatenate numeric literals"),
        Lit::Int(_, IntSuffix::None | IntSuffix::U8) => {
            BuiltinError::new("numeric literal is out of bounds")
        }
        Lit::Int(_, IntSuffix::Other) => BuiltinError::new("numeric literal is not a `u8`"),
        Lit::Byte(_) | Lit::ByteStr(_) | Lit::Err => {
            unreachable!("bytes, byte strings and refused literals are not invalid here")
        }
    }
}

fn relative_file(
    db: &Db,
    call_id: MacroCallId,
    path_str: &str,
    allow_recursion: bool,
    err_span: Span,
) -> Result<EditionedFileId, ExpandError> {
    let lookup = call_id.loc(db);
    let call_site = lookup.kind.file_id().original_file_respecting_includes(db).file_id(db);
    let path = AnchoredPath { anchor: call_site, path: path_str };
    let res: FileId = db.resolve_path(path).ok_or_else(|| {
        db.include_source(path, lookup.krate);
        ExpandError::other(err_span, format!("failed to load file `{path_str}`")) // alloc: an error's message, on the error path
    })?;
    // Prevent include itself
    if res == call_site && !allow_recursion {
        Err(ExpandError::other(err_span, format!("recursive inclusion of `{path_str}`"))) // alloc: an error's message, on the error path
    } else {
        Ok(EditionedFileId::new(db, res, lookup.krate.data(db).edition))
    }
}

/// The path `include!`, `include_str!` or `include_bytes!` (`name`) is given and its span, as
/// rustc's `get_single_str_spanned_from_tts` reads it; with an error, that of rustc.
fn include_path(
    db: &Db,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    name: &str,
) -> ExpandResult<Option<(Symbol, Span)>> {
    let call = call_span(db, id, tt.top_subtree().delimiter.open);
    args::single_string(db, tt, name, call)
}

fn include_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let ExpandResult { value: path, err } = include_path(db, arg_id, tt, "include!");
    let empty = || tt::TopSubtree::empty_in(DelimSpan { open: span, close: span }, frame);
    let Some((path, path_span)) = path else {
        return ExpandResult { value: empty(), err };
    };
    let editioned_file_id = match relative_file(db, arg_id, path.as_str(), false, path_span) {
        Ok(editioned_file_id) => editioned_file_id,
        Err(e) => return ExpandResult::new(empty(), err.unwrap_or(e)),
    };
    // FIXME: Parse errors
    let expanded = syntax_bridge::call_input_in(
        editioned_file_id.parse(db).root(),
        crate::span_map::InOrder::new(crate::HirFileId::from(editioned_file_id).span_map(db)),
        span,
        syntax_bridge::DocCommentDesugarMode::ProcMacro,
        frame,
    );
    ExpandResult { value: expanded, err }
}

pub fn include_input_to_file_id(
    db: &Db,
    arg_id: MacroCallId,
    arg: &tt::TopSubtree,
) -> Result<EditionedFileId, ExpandError> {
    let ExpandResult { value: path, err } = include_path(db, arg_id, arg, "include!");
    // An argument after the path is an error after which rustc includes the file all the same.
    match path {
        Some((path, span)) => relative_file(db, arg_id, path.as_str(), false, span),
        None => Err(err.expect("`single_string` gives a string or an error")),
    }
}

fn include_bytes_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    // The length of what is not read is not known, so its type is `&[u8; _]`.
    let unread = || {
        let pound = mk_pound(span);
        quote_in! {frame, span => builtin #pound include_bytes }
    };
    let ExpandResult { value: path, err } = include_path(db, arg_id, tt, "include_bytes!");
    let Some((path, path_span)) = path else { return ExpandResult { value: unread(), err } };
    match included_bytes(db, arg_id, tt, path.as_str(), path_span) {
        Ok(bytes) => {
            let literal = blaze_mem::scratch::with_bytes(|out| {
                tt::Literal::new_no_suffix(escape_ascii(bytes, out), span, tt::LitKind::ByteStr)
            });
            ExpandResult { value: quote_in!(frame, span =>#literal), err }
        }
        Err(error) => ExpandResult::new(unread(), err.unwrap_or(error)),
    }
}

/// `<[u8]>::escape_ascii` of `bytes` as one string, written in runs: the bytes it keeps (the
/// printable ASCII other than quotes and backslashes) are copied whole, not formatted one by
/// one, written into `out` (emptied first).
fn escape_ascii<'o>(bytes: &[u8], out: &'o mut Vec<u8>) -> &'o str {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let kept = |b: u8| matches!(b, 0x20..=0x7e) && !matches!(b, b'"' | b'\'' | b'\\');
    out.clear();
    out.reserve(bytes.len() + bytes.len() / 8);
    let mut run = 0;
    for (at, &b) in bytes.iter().enumerate() {
        if kept(b) {
            continue;
        }
        out.extend_from_slice(&bytes[run..at]);
        run = at + 1;
        match b {
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\\' | b'\'' | b'"' => out.extend_from_slice(&[b'\\', b]),
            _ => out.extend_from_slice(&[
                b'\\',
                b'x',
                HEX[usize::from(b >> 4)],
                HEX[usize::from(b & 0xf)],
            ]),
        }
    }
    out.extend_from_slice(&bytes[run..]);
    std::str::from_utf8(out).expect("escaped bytes are ASCII")
}

/// The contents of the file an `include_bytes!` or `include_str!` at `call_id` (its argument
/// `tt`) names, `path` at `path_span`; when the file can't be read, rustc's error at the call.
fn included_bytes<'db>(
    db: &'db Db,
    call_id: MacroCallId,
    tt: &tt::TopSubtree,
    path: &str,
    path_span: Span,
) -> Result<&'db [u8], ExpandError> {
    let anchor = call_id.loc(db).kind.file_id().original_file_respecting_includes(db).file_id(db);
    let path = AnchoredPath { anchor, path };
    // A file of the VFS that is not UTF-8 is read from disk like one outside it.
    if let Some(file) = db.resolve_path(path)
        && db.has_file_text(file)
    {
        return Ok(db.file_text(file).text(db).as_bytes());
    }
    let unloaded = || {
        // alloc: an error's message, on the error path
        ExpandError::other(path_span, format!("failed to load file `{}`", path.path))
    };
    let file = db.included_file(call_id.loc(db).krate, path).ok_or_else(unloaded)?;
    match file.read(db) {
        Ok(bytes) => Ok(&bytes[..]),
        Err(error) => {
            let shown = db.included_path(path).ok_or_else(unloaded)?;
            let call = call_span(db, call_id, tt.top_subtree().delimiter.open);
            // alloc: an error's message, on the error path
            Err(ExpandError::other(call, format!("couldn't read `{shown}`: {error}")))
        }
    }
}

fn include_str_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let ExpandResult { value: path, err } = include_path(db, arg_id, tt, "include_str!");
    let Some((path, path_span)) = path else {
        let empty =
            tt::TopSubtree::empty_in(DelimSpan { open: call_site, close: call_site }, frame);
        return ExpandResult { value: empty, err };
    };

    // A UTF-8 file is escaped where it is read, not copied first.
    let text = included_bytes(db, arg_id, tt, path.as_str(), path_span).and_then(|bytes| {
        std::str::from_utf8(bytes).map_err(|_| {
            let call = call_span(db, arg_id, tt.top_subtree().delimiter.open);
            // alloc: an error's message, on the error path
            ExpandError::other(call, format!("`{}` wasn't a utf-8 file", path.as_str()))
        })
    });
    match text {
        Ok(text) => {
            let literal = str_literal(text, call_site);
            ExpandResult { value: quote_in!(frame, call_site =>#literal), err }
        }
        Err(error) => ExpandResult::new(quote_in!(frame, call_site =>""), err.unwrap_or(error)),
    }
}

fn get_env_inner(db: &Db, arg_id: MacroCallId, key: &Symbol) -> Option<String> {
    let krate = arg_id.loc(db).krate;
    db.env_is_read(krate, key.as_str());
    // As rustc reads them: what cargo set for the crate, then the process the build is for.
    // The standard library's crates read their own alone, as a std built elsewhere does: no
    // workspace's environment is in their identity.
    let own = krate.env(db).get(key.as_str());
    match krate.data(db).origin.is_lang() {
        true => own,
        false => own.or_else(|| base_db::process_env_var(db, key.as_str())),
    }
}

fn env_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let empty = || tt::TopSubtree::empty_in(DelimSpan { open: span, close: span }, frame);
    let call = call_span(db, arg_id, span);
    let exprs = match args::exprs(db, tt) {
        Ok(it) => it,
        Err(e) => return ExpandResult::new(empty(), e),
    };
    let (var, message) = match exprs[..] {
        [var] => (var, None),
        [var, message] => (var, Some(message)),
        _ => {
            let err = BuiltinError::new("`env!()` takes 1 or 2 arguments").at(call);
            return ExpandResult::new(empty(), err);
        }
    };
    let key = match args::string(&var, "expected string literal") {
        Ok(it) => it,
        Err(e) => return ExpandResult::new(empty(), e),
    };
    let message = match message.map(|it| args::string(&it, "expected string literal")) {
        None => None,
        Some(Ok(it)) => Some(it),
        Some(Err(e)) => return ExpandResult::new(empty(), e),
    };

    let mut err = None;
    let s = get_env_inner(db, arg_id, &key).unwrap_or_else(|| {
        db.env_is_unset(arg_id.loc(db).krate, key.as_str());
        let lit = var.literal().expect("`args::string` took a literal");
        err = Some(env_not_defined(key.as_str(), &lit, message).at(call));

        // If the variable is unset, still return a dummy string to help type inference along.
        // We cannot use an empty string here, because for
        // `include!(concat!(env!("OUT_DIR"), "/foo.rs"))` will become
        // `include!("foo.rs"), which might go to infinite loop
        "UNRESOLVED_ENV_VAR".to_owned() // alloc: an error's message, on the error path
    });
    let span = var.span;
    let expanded = quote_in! {frame, span => #s };

    ExpandResult { value: expanded, err }
}

/// rustc's error for `env!` of the unset variable `var`, written as `lit`, with the message the
/// call gives if it gives one: `EnvNotDefinedWithUserMessage`, else the `EnvNotDefined` that
/// `expand_env` picks.
fn env_not_defined(var: &str, lit: &tt::Literal, message: Option<Symbol>) -> BuiltinError {
    if let Some(message) = message {
        return BuiltinError::new(message.as_str());
    }
    // rustc names the variable by the literal's symbol, its text before unescaping.
    let name = lit.text();
    // alloc: an error's message and helps, on the error path
    let error = BuiltinError::new(format!("environment variable `{name}` not defined at compile time"));
    if let Some(suggested) = similar_cargo_var(var)
        && suggested != var
    {
        // alloc: as above
        error.help(format!("there is a similar Cargo environment variable: `{suggested}`"))
    } else if is_cargo_env_var(var) {
        // alloc: as above
        let available = format!("`{name}` may not be available for the current Cargo target");
        // alloc: as above
        let at_run_time = format!(
            "Cargo sets build script variables at run time. Use `std::env::var({lit})` instead"
        );
        error.help(available).help(at_run_time)
    } else {
        // alloc: as above
        error.help(format!("use `std::env::var({lit})` to read the variable at run time"))
    }
}

/// rustc's `is_cargo_env_var`.
fn is_cargo_env_var(var: &str) -> bool {
    var.starts_with("CARGO_")
        || var.starts_with("DEP_")
        || matches!(var, "OUT_DIR" | "OPT_LEVEL" | "PROFILE" | "HOST" | "TARGET")
}

/// rustc's `KNOWN_CARGO_VARS`: what Cargo sets for a crate.
const KNOWN_CARGO_VARS: &[&str] = &[
    "CARGO_BIN_NAME",
    "CARGO_CRATE_NAME",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_NAME",
    "CARGO_PKG_README",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_TARGET_TMPDIR",
];

/// rustc's `find_similar_cargo_var`: the known Cargo variable closest to `var` within a third of
/// its length, the first of equally close ones.
fn similar_cargo_var(var: &str) -> Option<&'static str> {
    if !var.starts_with("CARGO_") {
        return None;
    }
    let max_dist = std::cmp::max(var.chars().count(), 3) / 3;
    let mut best = None;
    let mut best_distance = usize::MAX;
    for &known in KNOWN_CARGO_VARS {
        if let Some(mut distance) = edit_distance(var, known, max_dist) {
            // `PACKAGE` counts as `PKG`.
            if var.contains("PACKAGE") && known.contains("PKG") {
                distance = distance.saturating_sub("PACKAGE".len() - "PKG".len());
            }
            if distance < best_distance {
                best_distance = distance;
                best = Some(known);
            }
        }
    }
    best
}

fn option_env_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let call = call_span(db, arg_id, call_site);
    let ExpandResult { value, err } = args::single_string(db, tt, "option_env!", call);
    let Some((key, span)) = value else {
        let empty =
            tt::TopSubtree::empty_in(DelimSpan { open: call_site, close: call_site }, frame);
        return ExpandResult { value: empty, err };
    };
    let dollar_crate = dollar_crate(call_site);
    let expanded = match get_env_inner(db, arg_id, &key) {
        None => quote_in! {frame, call_site => #dollar_crate::option::Option::None::<&str> },
        Some(s) => {
            let value = group(span, |b| quote_into!(b, span => #s));
            quote_in! {frame, call_site => #dollar_crate::option::Option::Some(#value) }
        }
    };

    ExpandResult { value: expanded, err }
}

fn quote_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _arg_id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    ExpandResult::new(
        tt::TopSubtree::empty_in(tt::DelimSpan { open: span, close: span }, frame),
        ExpandError::other(span, "quote! is not implemented"),
    )
}

fn unescape_str(s: &str) -> Cow<'_, str> {
    if s.contains('\\') {
        let mut buf = String::with_capacity(s.len());
        unescape_str_into(s, &mut buf);
        Cow::Owned(buf)
    } else {
        Cow::Borrowed(s)
    }
}

/// [`unescape_str`] of `s` appended to `out`, with no buffer of its own.
fn unescape_str_into(s: &str, out: &mut String) {
    if s.contains('\\') {
        syntax::unescape::unescape_str(s, |_, c| {
            if let Ok(c) = c {
                out.push(c)
            }
        });
    } else {
        out.push_str(s);
    }
}

/// `gca!(expr)`, a generic const argument: the expression, which is what rustc evaluates it to.
fn gca_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    _call_site: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let mut tt = tt::TopSubtree::from_subtree_in(tt.view(), frame);
    tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Invisible);
    ExpandResult::ok(tt)
}

fn pattern_type_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let ty = WithDelimiter {
        delimiter: tt::Delimiter { kind: tt::DelimiterKind::Invisible, ..tt.top_subtree().delimiter },
        token_trees: tt.token_trees(),
    };
    let pound = mk_pound(call_site);
    ExpandResult::ok(quote_in! {frame, call_site => builtin #pound pattern_type ( #ty ) })
}

#[cfg(test)]
mod assert_condition_tests {
    use super::{Edition, write_assert_condition};

    #[test]
    fn formats_the_asserted_expression() {
        let mut out = String::new();
        write_assert_condition(
            &mut out,
            r#"value<= Self :: MAX && "A :: B".is_empty()"#,
            Edition::CURRENT,
        );
        assert_eq!(out, r#"value <= Self::MAX && "A :: B".is_empty()"#);
    }

    /// Written after a head, the condition is spaced as it is alone: the first token has no
    /// space before it.
    #[test]
    fn appends_after_the_head() {
        let mut out = String::from("assertion failed: ");
        write_assert_condition(&mut out, "!x", Edition::CURRENT);
        assert_eq!(out, "assertion failed: !x");
        let mut out = String::from("assertion failed: ");
        write_assert_condition(&mut out, "a ==b", Edition::CURRENT);
        assert_eq!(out, "assertion failed: a == b");
    }
}

/// The expansions that read the call's tree where it is against the clone-and-change they
/// replace: the same trees (nodes, spans, symbols), `Debug` and `Display`.
#[cfg(test)]
mod tests {
    use intern::Symbol;
    use span::{Edition, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext};
    use syntax::{TextRange, TextSize};

    use super::{WithDelimiter, escape_ascii, format_args_nl_of_literal, mk_pound, parenthesized};
    use crate::tt;

    fn at(offset: u32) -> Span {
        Span {
            range: TextRange::empty(TextSize::new(offset)),
            anchor: SpanAnchor {
                file_id: span::EditionedFileId::new(span::FileId::from_raw(3), Edition::CURRENT),
                ast_id: ROOT_ERASED_FILE_AST_ID,
            },
            ctx: SyntaxContext::root(Edition::CURRENT),
        }
    }

    const CALL: u32 = 100;

    fn assert_same<A: std::alloc::Allocator, B: std::alloc::Allocator>(
        old: &tt::TopSubtree<A>,
        new: &tt::TopSubtree<B>,
    ) {
        assert_eq!(format!("{old:#?}"), format!("{new:#?}"));
        assert_eq!(old.to_string(), new.to_string());
        assert!(old == new);
    }

    /// A call's arguments `{ <first>, x [y] }`, every token at a span of its own.
    fn call_args(first: Option<tt::Literal>) -> tt::TopSubtree {
        let mut b = tt::TopSubtreeBuilder::new(tt::Delimiter {
            kind: tt::DelimiterKind::Brace,
            open: at(1),
            close: at(9),
        });
        match first {
            Some(lit) => b.push(lit.into()),
            None => b.push(tt::Leaf::Ident(tt::Ident::new("ident", at(2)))),
        }
        b.push(tt::Leaf::Punct(tt::Punct { char: ',', spacing: tt::Spacing::Alone, span: at(3) }));
        b.push(tt::Leaf::Ident(tt::Ident::new("x", at(4))));
        b.open(tt::DelimiterKind::Bracket, at(5));
        b.push(tt::Leaf::Ident(tt::Ident::new("y", at(6))));
        b.close(at(7));
        b.build()
    }

    fn templates() -> Vec<Option<tt::Literal>> {
        vec![
            None,
            Some(tt::Literal::new_no_suffix("a {} b", at(2), tt::LitKind::Str)),
            Some(tt::Literal::new_no_suffix("esc\\t\\\"{x}", at(2), tt::LitKind::Str)),
            Some(tt::Literal::new_no_suffix("raw {}", at(2), tt::LitKind::StrRaw(2))),
            Some(tt::Literal::new("suffixed", at(2), tt::LitKind::Str, "sfx")),
            Some(tt::Literal::new_no_suffix("b", at(2), tt::LitKind::ByteStr)),
        ]
    }

    #[test]
    fn parenthesized_is_the_clone_made_a_parenthesis() {
        let span = at(CALL);
        for first in templates() {
            let tt = call_args(first);
            let pound = mk_pound(span);
            let old = {
                let mut tt = tt.copy();
                tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
                quote! {span => builtin #pound format_args #tt}
            };
            let args = parenthesized(&tt);
            assert_same(&old, &quote! {span => builtin #pound format_args #args});
        }
    }

    /// `unreachable!`'s arguments at the call site's spans, and `pattern_type!`'s made
    /// invisible.
    #[test]
    fn with_delimiter_is_the_clone_changed() {
        let span = at(CALL);
        let tt = call_args(None);
        let old = {
            let mut tt = tt.copy();
            tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
            tt.set_top_subtree_delimiter_span(tt::DelimSpan { open: span, close: span });
            quote! {span => m! #tt}
        };
        let new = WithDelimiter {
            delimiter: tt::Delimiter { open: span, close: span, kind: tt::DelimiterKind::Parenthesis },
            token_trees: tt.token_trees(),
        };
        assert_same(&old, &quote! {span => m! #new});

        let old = {
            let mut tt = tt.copy();
            tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Invisible);
            quote! {span => pattern_type ( #tt )}
        };
        let new = WithDelimiter {
            delimiter: tt::Delimiter { kind: tt::DelimiterKind::Invisible, ..tt.top_subtree().delimiter },
            token_trees: tt.token_trees(),
        };
        assert_same(&old, &quote! {span => pattern_type ( #new )});
    }

    /// The old `format_args_nl!` of a literal: the tree cloned and rebuilt with the token changed.
    fn format_args_nl_by_set_token(tt: &tt::TopSubtree, span: Span) -> Option<tt::TopSubtree> {
        let pound = mk_pound(span);
        let lit = tt.as_token_trees().iter_flat_tokens().nth(1);
        let Some(tt::TokenTree::Leaf(tt::Leaf::Literal(
            mut lit @ tt::Literal { kind: tt::LitKind::Str | tt::LitKind::StrRaw(_), .. },
        ))) = lit
        else {
            return None;
        };
        let mut tt = tt.copy();
        tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
        let newline = if lit.kind == tt::LitKind::Str { "\\n" } else { "\n" };
        let (text, suffix) = lit.text_and_suffix();
        lit.text_and_suffix = Symbol::intern(&format!("{text}{newline}{suffix}"));
        tt.set_token(1, lit.into());
        Some(quote! {span => builtin #pound format_args #tt})
    }

    #[test]
    fn format_args_nl_of_literal_is_set_token() {
        let frame = &blaze_mem::Frame::new(blaze_mem::Tag::Expand).expect("a test's frame");
        let span = at(CALL);
        for first in templates() {
            let tt = call_args(first);
            let old = format_args_nl_by_set_token(&tt, span);
            let new = format_args_nl_of_literal(frame, &tt, span);
            assert_eq!(old.is_some(), new.is_some());
            if let (Some(old), Some(new)) = (old, new) {
                assert_same(&old, &new);
            }
        }
        // A template alone, and nothing at all.
        let alone = tt::TopSubtree::invisible_from_leaves(
            at(1),
            [tt::Leaf::Literal(tt::Literal::new_no_suffix("t", at(2), tt::LitKind::Str))],
        );
        assert_same(
            &format_args_nl_by_set_token(&alone, span).unwrap(),
            &format_args_nl_of_literal(frame, &alone, span).unwrap(),
        );
        let empty = tt::TopSubtree::empty(tt::DelimSpan::from_single(at(1)));
        assert!(format_args_nl_of_literal(frame, &empty, span).is_none());
    }

    #[test]
    fn escape_ascii_is_std() {
        let every: Vec<u8> = (0..=255).collect();
        let mut out = Vec::new();
        assert_eq!(escape_ascii(&every, &mut out), every.escape_ascii().to_string());
        // Runs of kept bytes between escaped ones, from a fixed seed.
        let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        let mut bytes = Vec::with_capacity(4096);
        for _ in 0..4096 {
            let draw = rng.next_u64();
            let b = draw as u8;
            bytes.push(if draw & 0x300 == 0 { b } else { b'a' + b % 26 });
        }
        for len in [0, 1, 2, 7, 64, 4096] {
            let it = &bytes[..len];
            assert_eq!(escape_ascii(it, &mut out), it.escape_ascii().to_string(), "{len}");
        }
        assert_eq!(
            escape_ascii(b"\t\r\n\\'\"\x00\x7f\xff ok", &mut out),
            "\\t\\r\\n\\\\\\'\\\"\\x00\\x7f\\xff ok"
        );
    }
}

#[cfg(test)]
mod name_tests {
    use intern::{Symbol, sym};

    use super::{BuiltinFnLikeExpander, EagerExpander};
    use crate::name::Name;

    /// Every builtin's name, the same texts interned at run time, near misses of each and
    /// other names.
    fn candidates() -> Vec<Name> {
        let mut symbols: Vec<Symbol> =
            BuiltinFnLikeExpander::NAMES.iter().chain(EagerExpander::NAMES).cloned().collect();
        let texts: Vec<String> = symbols.iter().map(|it| it.as_str().to_owned()).collect();
        for text in &texts {
            symbols.push(Symbol::intern(text));
            symbols.push(Symbol::intern(&format!("{text}_")));
            symbols.push(Symbol::intern(&text.to_uppercase()));
            symbols.push(Symbol::intern(&text[..text.len() - 1]));
        }
        symbols.extend([sym::self_, sym::derive, sym::Copy, sym::sized, Symbol::intern("panic")]);
        symbols.into_iter().map(Name::new_symbol_root).collect()
    }

    #[test]
    fn builtin_macros_found_as_by_compare() {
        for name in candidates() {
            assert_eq!(
                BuiltinFnLikeExpander::find_by_name(&name),
                BuiltinFnLikeExpander::find_by_name_by_compare(&name),
                "{name:?}"
            );
            assert_eq!(
                EagerExpander::find_by_name(&name),
                EagerExpander::find_by_name_by_compare(&name),
                "{name:?}"
            );
        }
        for (at, symbol) in BuiltinFnLikeExpander::NAMES.iter().enumerate() {
            let name = Name::new_symbol_root(symbol.clone());
            assert_eq!(
                BuiltinFnLikeExpander::find_by_name(&name),
                Some(BuiltinFnLikeExpander::ALL[at])
            );
        }
        for (at, symbol) in EagerExpander::NAMES.iter().enumerate() {
            let name = Name::new_symbol_root(symbol.clone());
            assert_eq!(EagerExpander::find_by_name(&name), Some(EagerExpander::ALL[at]));
        }
    }
}
