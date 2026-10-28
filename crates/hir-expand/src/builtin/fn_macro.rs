//! Builtin macro

use std::borrow::Cow;

use base_db::{AnchoredPath, SourceDatabase};
use cfg::CfgExpr;
use either::Either;
use intern::{Symbol, sym};
use itertools::Itertools;
use mbe::{DelimiterKind, expect_fragment};
use span::{Edition, FileId, Span};
use stdx::format_to;
use syntax::{
    format_smolstr,
    unescape::{unescape_byte, unescape_char},
};
use syntax_bridge::syntax_node_to_token_tree;

use crate::{
    EditionedFileId, ExpandError, ExpandResult, HirFileId, MacroCallId,
    builtin::quote::{WithDelimiter, dollar_crate},
    hygiene::{span_with_call_site_ctxt, span_with_def_site_ctxt},
    name,
    tt::{self, DelimSpan, TtElement, TtIter},
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

            fn expander(&self) -> fn (&dyn SourceDatabase, MacroCallId, &tt::TopSubtree, Span) -> ExpandResult<tt::TopSubtree>  {
                match *self {
                    $( Self::$kind => $expand, )*
                }
            }

            fn find_by_name(ident: &name::Name) -> Option<Self> {
                match ident {
                    $( id if *id == sym::$name => Some(Self::$kind), )*
                    _ => None,
                }
            }
        }
    }
}

impl BuiltinFnLikeExpander {
    pub fn expand(
        &self,
        db: &dyn SourceDatabase,
        id: MacroCallId,
        tt: &tt::TopSubtree,
        span: Span,
    ) -> ExpandResult<tt::TopSubtree> {
        let span = span_with_def_site_ctxt(db, span, id.into(), Edition::CURRENT);
        self.expander()(db, id, tt, span)
    }

    pub fn is_asm(&self) -> bool {
        matches!(self, Self::Asm | Self::GlobalAsm | Self::NakedAsm)
    }
}

impl EagerExpander {
    pub fn expand(
        &self,
        db: &dyn SourceDatabase,
        id: MacroCallId,
        tt: &tt::TopSubtree,
        span: Span,
    ) -> ExpandResult<tt::TopSubtree> {
        let span = span_with_def_site_ctxt(db, span, id.into(), Edition::CURRENT);
        self.expander()(db, id, tt, span)
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
    (column, Column) => line_expand,
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

fn module_path_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let placeholder = MODULE_PATH_PLACEHOLDER;
    ExpandResult::ok(quote! {span =>
         #placeholder
    })
}

/// What `module_path!()` expands to: the expander doesn't know modules, and whoever lowers a
/// literal with this in it puts the path of the module there.
pub const MODULE_PATH_PLACEHOLDER: &str = "\u{0}module_path\u{0}";

fn line_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    // dummy implementation for type-checking purposes
    // Note that `line!` and `column!` will never be implemented properly, as they are by definition
    // not incremental
    ExpandResult::ok(tt::TopSubtree::invisible_from_leaves(
        span,
        [tt::Leaf::Literal(tt::Literal::new("0", span, tt::LitKind::Integer, "u32"))],
    ))
}

fn log_syntax_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    ExpandResult::ok(quote! {span =>})
}

fn trace_macros_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    ExpandResult::ok(quote! {span =>})
}

/// Whether the source has another token right after the token `text` at `span`. `None` if the
/// source does not have that token there, which is the case for a token made by a macro.
fn source_joined(db: &dyn SourceDatabase, span: Span, text: &str) -> Option<bool> {
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
fn through_proc_macro(db: &dyn SourceDatabase, id: MacroCallId) -> bool {
    let mut file = id.parent(db);
    while let HirFileId::MacroFile(call) = file {
        if call.loc(db).def.is_proc_macro() {
            return true;
        }
        file = call.parent(db);
    }
    false
}

fn stringify_expand(
    db: &dyn SourceDatabase,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let pretty = ::tt::stringify(
        tt.token_trees(),
        &|span, text| source_joined(db, span, text),
        through_proc_macro(db, id),
    );

    let expanded = quote! {span =>
        #pretty
    };

    ExpandResult::ok(expanded)
}

/// Formats the binary operators and paths in the expression reported by a failed assertion.
/// `stringify!` instead preserves the token spacing supplied by the caller.
fn assert_condition(tokens: &str, edition: Edition) -> String {
    use syntax::{SyntaxKind, T};

    let expr = syntax::ast::Expr::parse(tokens, edition).syntax_node();
    let mut out = String::new();
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
        if !out.is_empty()
            && (operator || after_operator || (whitespace && !separator && !after_separator))
        {
            out.push(' ');
        }
        out.push_str(token.text());
        whitespace = false;
        after_operator = operator;
        after_separator = separator;
    }
    out
}

fn assert_expand(
    db: &dyn SourceDatabase,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
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
    let panic_args = rest.iter();
    let mac = if rest.is_empty() {
        // Without a message of its own, the assertion reports the condition.
        let tokens = ::tt::stringify(
            cond.value,
            &|span, text| source_joined(db, span, text),
            through_proc_macro(db, id),
        );
        let message =
            format!("assertion failed: {}", assert_condition(&tokens, span.ctx.edition(db)));
        quote! {call_site_span => #dollar_crate::panicking::panic(#message) }
    } else if use_panic_2021(db, span) {
        quote! {call_site_span => #dollar_crate::panic::panic_2021!(# #panic_args) }
    } else {
        quote! {call_site_span => #dollar_crate::panic!(# #panic_args) }
    };
    let value = cond.value;
    let expanded = quote! {call_site_span =>{
        if !(#value) {
            #mac;
        }
    }};

    match cond.err {
        Some(err) => ExpandResult::new(expanded, err.into()),
        None => ExpandResult::ok(expanded),
    }
}

fn file_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    // FIXME: RA purposefully lacks knowledge of absolute file names
    // so just return "".
    let file_name = "file";

    let expanded = quote! {span =>
        #file_name
    };

    ExpandResult::ok(expanded)
}

fn format_args_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let pound = mk_pound(span);
    let mut tt = tt.clone();
    tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
    ExpandResult::ok(quote! {span =>
        builtin #pound format_args #tt
    })
}

fn format_args_nl_expand(
    db: &dyn SourceDatabase,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let pound = mk_pound(span);
    let lit = tt.as_token_trees().iter_flat_tokens().nth(1);
    if let Some(tt::TokenTree::Leaf(tt::Leaf::Literal(
        mut lit @ tt::Literal { kind: tt::LitKind::Str | tt::LitKind::StrRaw(_), .. },
    ))) = lit
    {
        let mut tt = tt.clone();
        tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
        // A raw string has no escapes: the newline goes in as it is.
        let newline = if lit.kind == tt::LitKind::Str { "\\n" } else { "\n" };
        let (text, suffix) = lit.text_and_suffix();
        lit.text_and_suffix = Symbol::intern(&format_smolstr!("{text}{newline}{suffix}"));
        tt.set_token(1, lit.into());
        return ExpandResult::ok(quote! {span =>
            builtin #pound format_args #tt
        });
    }
    format_args_nl_of_macro(db, tt, span)
}

/// `format_args_nl!` whose template is not written as a string: a macro that expands to one.
///
/// rustc expands the template and then appends the newline to the string it got
/// (`expand_format_args_impl` with `append_newline` in
/// `compiler/rustc_builtin_macros/src/format.rs`). Here the template becomes
/// `concat!(template, "\n")`, which expands to that same string.
fn format_args_nl_of_macro(
    db: &dyn SourceDatabase,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
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
    let expanded = quote! {span =>
        builtin #pound format_args (#dollar_crate::concat!(#value, "\n") # #arguments)
    };
    match template.err {
        Some(err) => ExpandResult::new(expanded, err.into()),
        None => ExpandResult::ok(expanded),
    }
}

fn asm_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let mut tt = tt.clone();
    tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
    let pound = mk_pound(span);
    let expanded = quote! {span =>
        builtin #pound asm #tt
    };
    ExpandResult::ok(expanded)
}

fn global_asm_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let mut tt = tt.clone();
    tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
    let pound = mk_pound(span);
    let expanded = quote! {span =>
        builtin #pound global_asm #tt
    };
    ExpandResult::ok(expanded)
}

fn naked_asm_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let mut tt = tt.clone();
    tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
    let pound = mk_pound(span);
    let expanded = quote! {span =>
        builtin #pound naked_asm #tt
    };
    ExpandResult::ok(expanded)
}

fn cfg_select_expand(
    db: &dyn SourceDatabase,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
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
                    tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
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
                    tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
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
                        tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
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
            ExpandResult::ok(builder.build())
        }
        None => ExpandResult::new(
            tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
            ExpandError::other(
                span,
                "none of the predicates in this `cfg_select` evaluated to true",
            ),
        ),
    }
}

fn cfg_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let pound = mk_pound(span);
    // Support ide features, like completion and hover
    // This implementation should not be used for 'cfg_select' because 'cfg_select' expands any token.
    let expanded = quote! {span =>
        builtin #pound cfg #tt
    };
    ExpandResult::ok(expanded)
}

fn panic_expand(
    db: &dyn SourceDatabase,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
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
    let call = quote!(call_site_span =>#dollar_crate::panic::#mac! #subtree);

    ExpandResult::ok(call)
}

fn unreachable_expand(
    db: &dyn SourceDatabase,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let dollar_crate = dollar_crate(span);
    let call_site_span = span_with_call_site_ctxt(db, span, id.into(), Edition::CURRENT);

    let mac = if use_panic_2021(db, span) { sym::unreachable_2021 } else { sym::unreachable_2015 };

    // Pass the original arguments
    let mut subtree = tt.clone();
    subtree.set_top_subtree_delimiter_kind(tt::DelimiterKind::Parenthesis);
    subtree.set_top_subtree_delimiter_span(tt::DelimSpan {
        open: call_site_span,
        close: call_site_span,
    });

    // Expand to a macro call `$crate::panic::panic_{edition}`
    let call = quote!(call_site_span =>#dollar_crate::panic::#mac! #subtree);

    ExpandResult::ok(call)
}

/// Whether `panic!` written at `span` is the one of edition 2021: the edition of where it is
/// written, or of the macro that wrote it. A macro of the standard library that writes one
/// (`assert!`, `debug_assert!`) has `#[allow_internal_unstable(edition_panic)]`, which only
/// such a macro can have, and stands for where it is called itself.
fn use_panic_2021(db: &dyn SourceDatabase, span: Span) -> bool {
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

fn compile_error_expand(
    _db: &dyn SourceDatabase,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
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

    ExpandResult { value: quote! {span =>}, err: Some(err) }
}

fn concat_expand(
    _db: &dyn SourceDatabase,
    _arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree> {
    let mut err = None;
    let mut text = String::new();
    let mut span: Option<Span> = None;
    let mut record_span = |s: Span| match &mut span {
        Some(span) if span.anchor == s.anchor => span.range = span.range.cover(s.range),
        Some(_) => (),
        None => span = Some(s),
    };

    let mut i = 0;
    let mut iter = tt.iter();
    while let Some(mut t) = iter.next() {
        // FIXME: hack on top of a hack: `$e:expr` captures get surrounded in parentheses
        // to ensure the right parsing order, so skip the parentheses here. Ideally we'd
        // implement rustc's model. cc https://github.com/rust-lang/rust-analyzer/pull/10623
        if let TtElement::Subtree(subtree, subtree_iter) = &t
            && let Some([tt::TtElement::Leaf(tt)]) = subtree_iter.clone().collect_array()
            && subtree.delimiter.kind == tt::DelimiterKind::Parenthesis
        {
            t = TtElement::Leaf(tt);
        }
        match t {
            TtElement::Leaf(tt::Leaf::Literal(it)) if i % 2 == 0 => {
                // concat works with string and char literals, so remove any quotes.
                // It also works with integer, float and boolean literals, so just use the rest
                // as-is.
                match it.kind {
                    tt::LitKind::Char => {
                        if let Ok(c) = unescape_char(it.text()) {
                            text.push(c);
                        }
                        record_span(it.span);
                    }
                    tt::LitKind::Integer | tt::LitKind::Float => {
                        format_to!(text, "{}", it.text())
                    }
                    tt::LitKind::Str => {
                        text.push_str(&unescape_str(it.text()));
                        record_span(it.span);
                    }
                    tt::LitKind::StrRaw(_) => {
                        format_to!(text, "{}", it.text());
                        record_span(it.span);
                    }
                    tt::LitKind::Byte
                    | tt::LitKind::ByteStr
                    | tt::LitKind::ByteStrRaw(_)
                    | tt::LitKind::CStr
                    | tt::LitKind::CStrRaw(_)
                    | tt::LitKind::Err(_) => {
                        err = Some(ExpandError::other(it.span, "unexpected literal"))
                    }
                }
            }
            // handle boolean literals
            TtElement::Leaf(tt::Leaf::Ident(id))
                if i % 2 == 0 && (id.sym == sym::true_ || id.sym == sym::false_) =>
            {
                text.push_str(id.sym.as_str());
                record_span(id.span);
            }
            TtElement::Leaf(tt::Leaf::Punct(punct)) if i % 2 == 1 && punct.char == ',' => (),
            // handle negative numbers
            TtElement::Leaf(tt::Leaf::Punct(punct)) if i % 2 == 0 && punct.char == '-' => {
                let t = match iter.next() {
                    Some(t) => t,
                    None => {
                        err.get_or_insert(ExpandError::other(
                            call_site,
                            "unexpected end of input after '-'",
                        ));
                        break;
                    }
                };

                match t {
                    TtElement::Leaf(tt::Leaf::Literal(it))
                        if matches!(it.kind, tt::LitKind::Integer | tt::LitKind::Float) =>
                    {
                        format_to!(text, "-{}", it.text());
                        record_span(punct.span.cover(it.span));
                    }
                    _ => {
                        err.get_or_insert(ExpandError::other(
                            call_site,
                            "expected integer or floating pointer number after '-'",
                        ));
                        break;
                    }
                }
            }
            _ => {
                err.get_or_insert(ExpandError::other(call_site, "unexpected token"));
            }
        }
        i += 1;
    }
    let span = span.unwrap_or_else(|| tt.top_subtree().delimiter.open);
    ExpandResult { value: quote!(span =>#text), err }
}

fn concat_bytes_expand(
    _db: &dyn SourceDatabase,
    _arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree> {
    let mut bytes = String::new();
    let mut err = None;
    let mut span: Option<Span> = None;
    let mut record_span = |s: Span| match &mut span {
        Some(span) if span.anchor == s.anchor => span.range = span.range.cover(s.range),
        Some(_) => (),
        None => span = Some(s),
    };
    for (i, t) in tt.iter().enumerate() {
        match t {
            TtElement::Leaf(tt::Leaf::Literal(lit @ tt::Literal { span, kind, .. })) => {
                let text = lit.text();
                record_span(span);
                match kind {
                    tt::LitKind::Byte => {
                        if let Ok(b) = unescape_byte(text) {
                            bytes.extend(
                                b.escape_ascii().filter_map(|it| char::from_u32(it as u32)),
                            );
                        }
                    }
                    tt::LitKind::ByteStr => {
                        bytes.push_str(text);
                    }
                    tt::LitKind::ByteStrRaw(_) => {
                        bytes.extend(text.escape_debug());
                    }
                    _ => {
                        err.get_or_insert(ExpandError::other(span, "unexpected token"));
                        break;
                    }
                }
            }
            TtElement::Leaf(tt::Leaf::Punct(punct)) if i % 2 == 1 && punct.char == ',' => (),
            TtElement::Subtree(tree, tree_iter)
                if tree.delimiter.kind == tt::DelimiterKind::Bracket =>
            {
                if let Err(e) =
                    concat_bytes_expand_subtree(tree_iter, &mut bytes, &mut record_span, call_site)
                {
                    err.get_or_insert(e);
                    break;
                }
            }
            _ => {
                err.get_or_insert(ExpandError::other(call_site, "unexpected token"));
                break;
            }
        }
    }
    let span = span.unwrap_or(tt.top_subtree().delimiter.open);
    ExpandResult {
        value: tt::TopSubtree::invisible_from_leaves(
            span,
            [tt::Leaf::Literal(tt::Literal::new_no_suffix(&bytes, span, tt::LitKind::ByteStr))],
        ),
        err,
    }
}

fn concat_bytes_expand_subtree(
    tree_iter: TtIter<'_>,
    bytes: &mut String,
    mut record_span: impl FnMut(Span),
    err_span: Span,
) -> Result<(), ExpandError> {
    for (ti, tt) in tree_iter.enumerate() {
        match tt {
            TtElement::Leaf(tt::Leaf::Literal(
                lit @ tt::Literal { span, kind: tt::LitKind::Byte, .. },
            )) => {
                if let Ok(b) = unescape_byte(lit.text()) {
                    bytes.extend(b.escape_ascii().filter_map(|it| char::from_u32(it as u32)));
                }
                record_span(span);
            }
            TtElement::Leaf(tt::Leaf::Literal(
                lit @ tt::Literal { span, kind: tt::LitKind::Integer, .. },
            )) => {
                record_span(span);
                if let Ok(b) = lit.text().parse::<u8>() {
                    bytes.extend(b.escape_ascii().filter_map(|it| char::from_u32(it as u32)));
                }
            }
            TtElement::Leaf(tt::Leaf::Punct(punct)) if ti % 2 == 1 && punct.char == ',' => (),
            _ => {
                return Err(ExpandError::other(err_span, "unexpected token"));
            }
        }
    }
    Ok(())
}

fn relative_file(
    db: &dyn SourceDatabase,
    call_id: MacroCallId,
    path_str: &str,
    allow_recursion: bool,
    err_span: Span,
) -> Result<EditionedFileId, ExpandError> {
    let lookup = call_id.loc(db);
    let call_site = lookup.kind.file_id().original_file_respecting_includes(db).file_id(db);
    let path = AnchoredPath { anchor: call_site, path: path_str };
    let res: FileId = db.resolve_path(path).ok_or_else(|| {
        db.include_source(path);
        ExpandError::other(err_span, format!("failed to load file `{path_str}`"))
    })?;
    // Prevent include itself
    if res == call_site && !allow_recursion {
        Err(ExpandError::other(err_span, format!("recursive inclusion of `{path_str}`")))
    } else {
        Ok(EditionedFileId::new(db, res, lookup.krate.data(db).edition))
    }
}

fn parse_string(tt: &tt::TopSubtree, allow_rest_args: bool) -> Result<(Symbol, Span), ExpandError> {
    let expect_literal = |span| ExpandError::other(span, "expected string literal");
    let mut tt = {
        let mut tt_iter = tt.iter();
        let extracted =
            tt_iter.next().ok_or_else(|| expect_literal(tt.top_subtree().delimiter.close))?;

        match tt_iter.next() {
            None => {}
            Some(TtElement::Leaf(tt::Leaf::Punct(it))) if it.char == ',' => {
                // Tail comma
                // FIXME: Ignored like env!("NAME", "compile_error message")
                if let Some(tt) = tt_iter.next()
                    && !allow_rest_args
                {
                    return Err(ExpandError::other(tt.first_span(), "unexpected input"));
                }
            }
            Some(tt) => {
                return Err(ExpandError::other(tt.first_span(), "unexpected input"));
            }
        }

        extracted
    };

    (|| {
        // FIXME: We wrap expression fragments in parentheses which can break this expectation
        // here
        // Remove this once we handle none delims correctly
        while let TtElement::Subtree(sub, tt_iter) = &mut tt
            && let DelimiterKind::Parenthesis | DelimiterKind::Invisible = sub.delimiter.kind
        {
            tt =
                // FIXME: rewrite in terms of `#![feature(exact_length_collection)]`. See: #149266
                Itertools::exactly_one(tt_iter).map_err(|_| sub.delimiter.open.cover(sub.delimiter.close))?;
        }

        match tt {
            TtElement::Leaf(tt::Leaf::Literal(lit @ tt::Literal {
                span,
                kind: tt::LitKind::Str,
                ..
            })) => Ok((Symbol::intern(&unescape_str(lit.text())), span)),
            TtElement::Leaf(tt::Leaf::Literal(lit @ tt::Literal {
                span,
                kind: tt::LitKind::StrRaw(_),
                ..
            })) => Ok((Symbol::intern(lit.text()), span)),
            TtElement::Leaf(l) => Err(*l.span()),
            TtElement::Subtree(tt, _) => Err(tt.delimiter.open.cover(tt.delimiter.close)),
        }
    })()
    .map_err(expect_literal)
}

fn include_expand(
    db: &dyn SourceDatabase,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let editioned_file_id = match include_input_to_file_id(db, arg_id, tt) {
        Ok(editioned_file_id) => editioned_file_id,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty(DelimSpan { open: span, close: span }),
                e,
            );
        }
    };
    // FIXME: Parse errors
    ExpandResult::ok(syntax_node_to_token_tree(
        &editioned_file_id.parse(db).syntax_node(),
        crate::HirFileId::from(editioned_file_id).span_map(db),
        span,
        syntax_bridge::DocCommentDesugarMode::ProcMacro,
    ))
}

pub fn include_input_to_file_id(
    db: &dyn SourceDatabase,
    arg_id: MacroCallId,
    arg: &tt::TopSubtree,
) -> Result<EditionedFileId, ExpandError> {
    let (s, span) = parse_string(arg, false)?;
    relative_file(db, arg_id, s.as_str(), false, span)
}

fn include_bytes_expand(
    db: &dyn SourceDatabase,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let read = parse_string(tt, false).and_then(|(path, input_span)| {
        included_bytes(db, arg_id, path.as_str())
            .ok_or_else(|| ExpandError::other(input_span, format!("failed to load file `{path}`")))
    });
    match read {
        Ok(bytes) => {
            let text = bytes.escape_ascii().to_string();
            let literal = tt::Literal::new_no_suffix(&text, span, tt::LitKind::ByteStr);
            ExpandResult::ok(quote!(span =>#literal))
        }
        // The length is not known, so the type is `&[u8; _]`.
        Err(e) => {
            let pound = mk_pound(span);
            ExpandResult::new(quote! {span => builtin #pound include_bytes }, e)
        }
    }
}

/// The contents of the file an `include_bytes!` or `include_str!` at `call_id` names.
fn included_bytes<'db>(
    db: &'db dyn SourceDatabase,
    call_id: MacroCallId,
    path: &str,
) -> Option<&'db [u8]> {
    let anchor = call_id.loc(db).kind.file_id().original_file_respecting_includes(db).file_id(db);
    let path = AnchoredPath { anchor, path };
    // A file of the VFS that is not UTF-8 is read from disk like one outside it.
    if let Some(file) = db.resolve_path(path)
        && db.has_file_text(file)
    {
        return Some(db.file_text(file).text(db).as_bytes());
    }
    db.included_file(call_id.loc(db).krate, path)?.bytes(db).as_deref()
}

fn include_str_expand(
    db: &dyn SourceDatabase,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree> {
    let (path, input_span) = match parse_string(tt, false) {
        Ok(it) => it,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty(DelimSpan { open: call_site, close: call_site }),
                e,
            );
        }
    };

    let text = included_bytes(db, arg_id, path.as_str())
        .map(|it| String::from_utf8_lossy(it).into_owned());
    match text {
        Some(text) => ExpandResult::ok(quote!(call_site =>#text)),
        None => ExpandResult::new(
            quote!(call_site =>""),
            ExpandError::other(input_span, format!("failed to load file `{path}`")),
        ),
    }
}

fn get_env_inner(db: &dyn SourceDatabase, arg_id: MacroCallId, key: &Symbol) -> Option<String> {
    let krate = arg_id.loc(db).krate;
    db.env_is_read(krate, key.as_str());
    // As rustc reads them: what cargo set for the crate, then the process the build is for.
    krate.env(db).get(key.as_str()).or_else(|| base_db::process_env_var(db, key.as_str()))
}

fn env_expand(
    db: &dyn SourceDatabase,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    let (key, span) = match parse_string(tt, true) {
        Ok(it) => it,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty(DelimSpan { open: span, close: span }),
                e,
            );
        }
    };

    let mut err = None;
    let s = get_env_inner(db, arg_id, &key).unwrap_or_else(|| {
        db.env_is_unset(arg_id.loc(db).krate, key.as_str());
        err = Some(ExpandError::other(
            span,
            if key.as_str() == "OUT_DIR" {
                r#"`OUT_DIR` not set, build scripts may have failed to run"#.to_owned()
            } else {
                format!("environment variable `{}` not defined at compile time", key.as_str())
            },
        ));

        // If the variable is unset, still return a dummy string to help type inference along.
        // We cannot use an empty string here, because for
        // `include!(concat!(env!("OUT_DIR"), "/foo.rs"))` will become
        // `include!("foo.rs"), which might go to infinite loop
        "UNRESOLVED_ENV_VAR".to_owned()
    });
    let expanded = quote! {span => #s };

    ExpandResult { value: expanded, err }
}

fn option_env_expand(
    db: &dyn SourceDatabase,
    arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree> {
    let (key, span) = match parse_string(tt, false) {
        Ok(it) => it,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty(DelimSpan { open: call_site, close: call_site }),
                e,
            );
        }
    };
    let dollar_crate = dollar_crate(call_site);
    let expanded = match get_env_inner(db, arg_id, &key) {
        None => quote! {call_site => #dollar_crate::option::Option::None::<&str> },
        Some(s) => {
            let s = quote! (span => #s);
            quote! {call_site => #dollar_crate::option::Option::Some(#s) }
        }
    };

    ExpandResult::ok(expanded)
}

fn quote_expand(
    _db: &dyn SourceDatabase,
    _arg_id: MacroCallId,
    _tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree> {
    ExpandResult::new(
        tt::TopSubtree::empty(tt::DelimSpan { open: span, close: span }),
        ExpandError::other(span, "quote! is not implemented"),
    )
}

fn unescape_str(s: &str) -> Cow<'_, str> {
    if s.contains('\\') {
        let mut buf = String::with_capacity(s.len());
        syntax::unescape::unescape_str(s, |_, c| {
            if let Ok(c) = c {
                buf.push(c)
            }
        });
        Cow::Owned(buf)
    } else {
        Cow::Borrowed(s)
    }
}

/// `gca!(expr)`, a generic const argument: the expression, which is what rustc evaluates it to.
fn gca_expand(
    _db: &dyn SourceDatabase,
    _arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    _call_site: Span,
) -> ExpandResult<tt::TopSubtree> {
    let mut tt = tt.clone();
    tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Invisible);
    ExpandResult::ok(tt)
}

fn pattern_type_expand(
    _db: &dyn SourceDatabase,
    _arg_id: MacroCallId,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree> {
    let mut tt = tt.clone();
    tt.set_top_subtree_delimiter_kind(tt::DelimiterKind::Invisible);
    let pound = mk_pound(call_site);
    ExpandResult::ok(quote! {call_site => builtin #pound pattern_type ( #tt ) })
}

#[cfg(test)]
mod assert_condition_tests {
    use super::{Edition, assert_condition};

    #[test]
    fn formats_the_asserted_expression() {
        assert_eq!(
            assert_condition(r#"value<= Self :: MAX && "A :: B".is_empty()"#, Edition::CURRENT),
            r#"value <= Self::MAX && "A :: B".is_empty()"#,
        );
    }
}
