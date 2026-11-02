//! `NonPanicFmt`: `non_fmt_panics`, a `panic!`, `assert!`, `debug_assert!` or `unreachable!`
//! of edition 2015 given one argument that is not a format string: a string literal with
//! braces, which Rust 2021 reads as one, or no string literal at all.
//!
//! The lint is decided by rustc's spans: where a token is written, in a file of any crate, and
//! the expansion its syntax context was marked by last ([`At`]), read from hir-expand's span
//! maps, and the macro of each expansion with its diagnostic item.
//!
//! Follows rustc's `compiler/rustc_lint/src/non_fmt_panic.rs`, `compiler/rustc_lint/src/lints.rs`
//! (`NonFmtPanicUnused`, `NonFmtPanicBraces`), `compiler/rustc_span/src/lib.rs`
//! (`Span::source_callsite`, `contains`, `source_equal`, `prepare_to_combine`),
//! `compiler/rustc_span/src/hygiene.rs` (`SyntaxContext::in_external_macro`) and
//! `compiler/rustc_expand/src/expand.rs` (`ExpnKind::Macro` of a bang macro named by its path)
//! (MIT or Apache-2.0).

use std::ops::ControlFlow;

use base_db::Db;
use hir::{InFile, MacroCallId, db::HirDatabase};
use hir_def::{
    AdtId, AttrDefId, CallableDefId, HasModule as _, ModuleDefId, TraitId,
    attrs::AttrFlags,
    hir::{Expr, ExprId, Literal, type_ref::Rawness},
    nameres::crate_def_map,
};
use hir_expand::{
    MacroCallKind, MacroDefId, MacroDefKind,
    attrs::{AstKeyValueMetaExt as _, AstPathExt as _, expand_cfg_attr_view},
    builtin::EagerExpander,
};
use hir_ty::next_solver::{CallableIdWrapper, DbInterner, Ty, TyKind, infer::select::EvaluationResult};
use rustc_parse_format::{ParseMode, Parser, Piece};
use rustc_type_ir::inherent::IntoKind as _;
use span::FileId;
use syntax::{
    SmolStr, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use crate::{
    lints::bodies::{
        BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set,
        types::util::{self, Solver},
    },
    lower::{Found, Phase},
    model::{Applicability, Code, Diag, Level, MultiSpan, Span},
};

const LINT: &str = "non_fmt_panics";
const MORE: &str =
    "for more information, see <https://doc.rust-lang.org/edition-guide/rust-2021/panic-macro-consistency.html>";

#[derive(Default)]
pub(crate) struct NonPanicFmt {
    /// `Display` and `Debug`, found by their diagnostic items the first time a message is not a
    /// string literal.
    traits: Option<FmtTraits>,
}

/// rustc's `get_diagnostic_item(sym::Display)` and `(sym::Debug)`.
#[derive(Clone, Copy)]
struct FmtTraits {
    display: Option<TraitId>,
    debug: Option<TraitId>,
}

impl LatePass for NonPanicFmt {
    const LINTS: LintSet = lint_set([LINT]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Call]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Expr::Call { callee, args: &[arg] } = b.store().expr(expr) else { return };
        let TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(def)), _) = b.infer.expr_ty(callee).kind() else {
            return;
        };
        let db = b.db();
        let lang = DbInterner::new_with(db, b.owner.krate(db)).lang_items();
        let item = AttrFlags::diagnostic_item(db, AttrDefId::FunctionId(def));
        let item = item.as_ref().map(|it| it.as_str());
        let is_panic = lang.BeginPanic == Some(def) || lang.Panic == Some(def) || item == Some("panic_str_2015");
        if !is_panic && item != Some("unreachable_display") {
            return;
        }
        let Some(f) = b.expr_node(callee).and_then(|it| at_ptr(b, it)) else { return };
        let Some(expn) = f.expn else { return };
        let from = macro_item(db, expn.loc(db).def);
        if is_panic {
            if matches!(from.as_deref(), Some("core_panic_2015_macro" | "std_panic_2015_macro")) {
                self.check_panic(b, expr, &f, arg, out);
            }
        } else if from.as_deref() == Some("unreachable_2015_macro") {
            let Expr::Ref { expr: arg, rawness: Rawness::Ref, .. } = b.store().expr(arg) else { return };
            self.check_panic(b, expr, &f, arg, out);
        }
    }
}

impl NonPanicFmt {
    /// rustc's `check_panic`: the call `call` of `f` made by a 2015 panic macro, with the
    /// message `arg`.
    fn check_panic(&mut self, b: &BodyCx<'_, '_, '_>, call: ExprId, f: &At, arg: ExprId, out: &mut Vec<Found>) {
        if let Some(Literal::String(fmt)) = util::literal(b, arg) {
            out.extend(check_panic_str(b, call, f, arg, fmt.as_str()));
            return;
        }
        let Some((span, panic, symbol)) = panic_call(b, f) else { return };
        if in_external_macro(b, &span) {
            return;
        }
        // The argument as written in the call, before the macros it came out of
        // (`panic!(some_macro!())`), up to the call itself when that is in a macro.
        let Some(mut arg_span) = b.expr_node(arg).and_then(|it| at_ptr(b, it)) else { return };
        let mut arg_macro = None;
        while !span.contains(&arg_span) {
            let Some(expn) = arg_span.expn else { break };
            arg_macro = Some(expn.loc(b.db()).def);
            let Some(site) = call_site(b, expn) else { return };
            arg_span = site;
        }
        let traits = *self.traits.get_or_insert_with(|| FmtTraits::of(b));
        let message = NotLiteral { arg, arg_span, span, arg_macro, panic: panic.as_deref(), symbol: &symbol };
        let diag = message.diag(b, traits);
        let Some(at) = b.expr_node(call) else { return };
        out.push(b.lint(diag, Phase::Late, at));
    }
}

/// rustc's `check_panic_str`: the message is the string literal `arg`, `fmt` unescaped.
fn check_panic_str(b: &BodyCx<'_, '_, '_>, call: ExprId, f: &At, arg: ExprId, fmt: &str) -> Option<Found> {
    if !fmt.contains(['{', '}']) {
        return None;
    }
    let (span, _, _) = panic_call(b, f)?;
    let arg_span = at_ptr(b, b.expr_node(arg)?)?;
    if in_external_macro(b, &span) && in_external_macro(b, &arg_span) {
        return None;
    }
    let fmt_span = source_callsite(b, arg_span)?;
    let snippet = snippet(b, &fmt_span);
    // The number of `#`s between the `r` and the `"`.
    let style = snippet.and_then(|it| it.strip_prefix('r')).and_then(|it| it.find('"'));
    let mut parser = Parser::new(fmt, style, snippet.map(str::to_owned), false, ParseMode::Format);
    let n_arguments = (&mut parser).filter(|it| matches!(it, Piece::NextArgument(_))).count();
    let inside = is_arg_inside_call(&arg_span, &span);

    let diag = if n_arguments > 0 && parser.errors.is_empty() {
        let spans = match &parser.arg_places[..] {
            // alloc: the lint's diagnostic
            [] => vec![show(b, &fmt_span)],
            // alloc: the lint's diagnostic
            places => places.iter().map(|it| inner(b, &fmt_span, it.start, it.end)).collect(),
        };
        let message = match n_arguments {
            1 => "panic message contains an unused formatting placeholder",
            _ => "panic message contains unused formatting placeholders",
        };
        let mut diag = lint(message, spans);
        diag.note("this message is not used as a format string when given without arguments, but will be in Rust 2021");
        if inside {
            let arg_span = show(b, &arg_span);
            let missing = match n_arguments {
                1 => "add the missing argument",
                _ => "add the missing arguments",
            };
            diag.suggestion(arg_span.shrink_to_hi(), missing, ", ...", Applicability::HasPlaceholders);
            diag.suggestion(
                arg_span.shrink_to_lo(),
                "or add a \"{}\" format string to use the message literally",
                "\"{}\", ",
                Applicability::MachineApplicable,
            );
        }
        diag
    } else {
        let brace_spans: Option<Vec<Span>> = snippet.filter(|it| it.starts_with('"') || it.starts_with("r#")).map(|it| {
            it.char_indices()
                .filter(|&(_, c)| c == '{' || c == '}')
                .map(|(i, _)| inner(b, &fmt_span, i, i + 1))
                // alloc: the lint's diagnostic
                .collect()
        });
        // Without the snippet, any number but one.
        let count = brace_spans.as_ref().map_or(2, Vec::len);
        let message = match count {
            1 => "panic message contains a brace",
            _ => "panic message contains braces",
        };
        // alloc: the lint's diagnostic
        let mut diag = lint(message, brace_spans.unwrap_or_else(|| vec![show(b, &span)]));
        diag.note("this message is not used as a format string, but will be in Rust 2021");
        if inside {
            diag.suggestion(
                show(b, &arg_span).shrink_to_lo(),
                "add a \"{}\" format string to use the message literally",
                "\"{}\", ",
                Applicability::MachineApplicable,
            );
        }
        diag
    };
    Some(b.lint(diag, Phase::Late, b.expr_node(call)?))
}

/// rustc's `PanicMessageNotLiteral`.
struct NotLiteral<'s> {
    arg: ExprId,
    /// Where the argument is written in the panic macro's call.
    arg_span: At,
    /// The call of the outermost panic macro.
    span: At,
    /// The macro the argument came out of, if it did.
    arg_macro: Option<MacroDefId>,
    /// The diagnostic item of the outermost panic macro.
    panic: Option<&'s str>,
    /// The panic macro's name as it is called.
    symbol: &'s str,
}

impl NotLiteral<'_> {
    fn diag(&self, b: &BodyCx<'_, '_, '_>, traits: FmtTraits) -> Diag {
        let db = b.db();
        let NotLiteral { arg, ref arg_span, ref span, arg_macro, panic, symbol } = *self;
        let shown = show(b, arg_span);
        // clone: a span, for the lint's diagnostic
        let mut diag = Diag::lint(LINT, "panic message is not a string literal", shown.clone());
        // alloc: the lint's diagnostic
        diag.note(format!("this usage of `{symbol}!()` is deprecated; it will be a hard error in Rust 2021"));
        diag.note(MORE);
        if !is_arg_inside_call(arg_span, span) {
            // Where the argument comes from is not known.
            return diag;
        }
        if arg_macro.and_then(|it| macro_item(db, it)).as_deref() == Some("format_macro") {
            // alloc: the lint's diagnostic
            diag.note(format!(
                // alloc: the lint's diagnostic
                "the `{symbol}!()` macro supports formatting, so there's no need for the `format!()` macro here"
            ));
            if let Some((open, close, _)) = find_delimiters(b, arg_span) {
                let len = usize::from(arg_span.range.len());
                diag.multipart_suggestion(
                    // alloc: the lint's diagnostic
                    "remove the `format!(..)` macro call",
                    // alloc: the lint's diagnostic
                    vec![(inner(b, arg_span, 0, open + 1), String::new()), (inner(b, arg_span, close, len), String::new())],
                    Applicability::MachineApplicable,
                );
            }
            return diag;
        }

        let ty = b.infer.expr_ty(arg);
        let lang = DbInterner::new_with(db, b.owner.krate(db)).lang_items();
        // A `&str` or a `String` is a message to show with `"{}", `.
        let is_str = matches!(ty.kind(), TyKind::Ref(_, inner, _) if inner.is_str())
            || ty.as_adt().is_some_and(|(adt, _)| lang.String.is_some_and(|it| AdtId::StructId(it) == adt));
        let solver = Solver::of(b);
        // alloc: the lint's diagnostic
        let mut checked = Vec::new();
        let mut implements = |name: &'static str, it: Option<TraitId>| {
            let Some(it) = it else { return false };
            checked.push((name, it));
            !matches!(solver.infcx.type_implements_trait(it, [ty], solver.env), EvaluationResult::EvaluatedToErr)
        };
        let suggest_display = is_str || implements("Display", traits.display);
        let suggest_debug = !suggest_display && implements("Debug", traits.debug);
        let suggest_panic_any = !is_str && panic == Some("std_panic_macro");
        // With `panic_any` to suggest, a format string is not the only way.
        let applicability =
            if suggest_panic_any { Applicability::MaybeIncorrect } else { Applicability::MachineApplicable };

        if suggest_display {
            diag.suggestion_verbose(
                shown.shrink_to_lo(),
                "add a \"{}\" format string to `Display` the message",
                "\"{}\", ",
                applicability,
            );
        } else if suggest_debug {
            diag.suggestion_verbose(
                shown.shrink_to_lo(),
                // alloc: the lint's diagnostic
                format!("add a \"{{:?}}\" format string to use the `Debug` implementation of `{}`", b.ty(ty)),
                "\"{:?}\", ",
                applicability,
            );
        }
        if suggest_panic_any && let Some((open, close, delimiter)) = find_delimiters(b, span) {
            let message = if suggest_display || suggest_debug {
                "or use std::panic::panic_any instead"
            } else {
                "use std::panic::panic_any instead"
            };
            let parts = match delimiter {
                // alloc: the lint's diagnostic
                '(' => vec![(inner(b, span, 0, open), "std::panic::panic_any".to_owned())],
                // alloc: the lint's diagnostic
                _ => vec![
                    // alloc: the lint's diagnostic
                    (inner(b, span, 0, open + 1), "std::panic::panic_any(".to_owned()),
                    // alloc: the lint's diagnostic
                    (inner(b, span, close, close + 1), ")".to_owned()),
                ],
            };
            diag.multipart_suggestion(message, parts, Applicability::MachineApplicable);
        }
        trait_facts(b, &mut diag, arg, ty, &checked);
        diag
    }
}

/// Records on `diag` whether the message's type implements each of the traits it was checked
/// for.
fn trait_facts<'db>(b: &BodyCx<'_, '_, 'db>, diag: &mut Diag, arg: ExprId, ty: Ty<'db>, checked: &[(&str, TraitId)]) {
    if checked.is_empty() {
        return;
    }
    let Some(written) = util::syntax(b, arg).and_then(|it| ast::Expr::cast(it.value)) else { return };
    let Some(of) = b.cx.vsema.type_of_expr(written) else { return };
    let ty = of.original.derived_from_next_solver(ty);
    let printed = b.cx.ty(&ty);
    for &(name, it) in checked {
        // alloc: the lint's diagnostic
        let what = format!("whether `{printed}` implements `{name}`");
        diag.fact_opt(b.cx.implements_fact(hir::Trait::from(it), &ty), what);
    }
}

impl FmtTraits {
    /// The traits of the crates the body's crate depends on (itself included) that are the
    /// diagnostic items `Display` and `Debug`.
    fn of(b: &BodyCx<'_, '_, '_>) -> FmtTraits {
        let db = b.db();
        let mut traits = FmtTraits { display: None, debug: None };
        'crates: for krate in b.owner.krate(db).transitive_deps(db).iter().copied() {
            for (_, module) in crate_def_map(db, krate).modules(db) {
                for def in module.scope(db).declarations() {
                    let ModuleDefId::TraitId(it) = def else { continue };
                    match AttrFlags::diagnostic_item(db, AttrDefId::TraitId(it)).as_ref().map(|it| it.as_str()) {
                        Some("Display") => traits.display = Some(it),
                        Some("Debug") => traits.debug = Some(it),
                        _ => continue,
                    }
                    if traits.display.is_some() && traits.debug.is_some() {
                        break 'crates;
                    }
                }
            }
        }
        traits
    }
}

/// A lint of `message` at each of `spans`, which may be none.
fn lint(message: &str, spans: Vec<Span>) -> Diag {
    // alloc: the lint's diagnostic
    let mut diag = Diag::new(Level::Warning, Some(Code::Lint(LINT.to_owned())), message);
    // alloc: the lint's diagnostic
    diag.span = MultiSpan { primary: spans, labels: Vec::new() };
    diag
}

/// rustc's `Span` of a node: where its tokens are written, in a file of any crate, and the
/// expansion their syntax context was marked by last, with where blaze has it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct At {
    file: FileId,
    range: TextRange,
    /// rustc's `ctxt().outer_expn()`: none for code written in a file rather than by a macro.
    expn: Option<MacroCallId>,
    /// Where it is in the expansions, to show it by.
    shown: InFile<TextRange>,
}

impl At {
    /// rustc's `Span::contains`.
    fn contains(&self, other: &At) -> bool {
        self.file == other.file && self.range.contains_range(other.range)
    }
}

fn at_ptr(b: &BodyCx<'_, '_, '_>, ptr: InFile<SyntaxNodePtr>) -> Option<At> {
    at(b, b.cx.syntax_view(ptr))
}

/// rustc's span of `node`: its first token's to its last, as rustc's parser spans a node,
/// combined as `Span::to` combines spans of two contexts (the one a macro made, when the other
/// is written in a file; else the first). The output of a built-in macro (but `include!`'s) is
/// spanned at the macro's call, in its expansion, as rustc's built-in macros span what they
/// make.
fn at(b: &BodyCx<'_, '_, '_>, node: InFile<ast::Node<'_>>) -> Option<At> {
    let db = b.db();
    let range = node.value.text_range();
    let Some(call) = node.file_id.macro_file() else {
        let file = node.file_id.file_id()?.file_id(db);
        return Some(At { file, range, expn: None, shown: node.with_value(range) });
    };
    if is_builtin_output(db, call, node.value) {
        let site = call_site(b, call)?;
        return Some(At { expn: Some(call), ..site });
    }
    let first = token(db, node.with_value(first_token(node.value)?))?;
    let last = token(db, node.with_value(last_token(node.value)?))?;
    Some(if first.expn == last.expn && first.file == last.file {
        At { range: first.range.cover(last.range), shown: node.with_value(range), ..first }
    } else if first.expn.is_none() {
        last
    } else {
        first
    })
}

/// rustc's span of a token of an expansion.
fn token(db: &Db, token: InFile<ast::Token<'_>>) -> Option<At> {
    let range = token.value.text_range();
    let shown = token.with_value(range);
    let (written, ctxt) = shown.original_node_file_range_opt(db)?;
    let expn = ctxt.outer_expn(db).map(MacroCallId::from);
    Some(At { file: written.file_id.file_id(db), range: written.range, expn, shown })
}

fn first_token(node: ast::Node<'_>) -> Option<ast::Token<'_>> {
    let end = node.text_range().end();
    let mut token = node.first_token()?;
    while token.kind().is_trivia() {
        token = token.next_token().filter(|it| it.text_range().start() < end)?;
    }
    Some(token)
}

fn last_token(node: ast::Node<'_>) -> Option<ast::Token<'_>> {
    let start = node.text_range().start();
    let mut token = node.last_token()?;
    while token.kind().is_trivia() {
        token = token.prev_token().filter(|it| it.text_range().end() > start)?;
    }
    Some(token)
}

/// Whether `node` is the whole expansion of `call`, a call of a built-in macro other than
/// `include!` (whose expansion is the included file's code, spanned where it is written).
fn is_builtin_output(db: &Db, call: MacroCallId, node: ast::Node<'_>) -> bool {
    let builtin = match call.loc(db).def.kind {
        MacroDefKind::BuiltIn(..) => true,
        MacroDefKind::BuiltInEager(_, expander) => !matches!(expander, EagerExpander::Include),
        _ => false,
    };
    builtin && node.ancestors().last().is_some_and(|root| root.text_range() == node.text_range())
}

/// rustc's `outer_expn_data().call_site` of an expansion: the span of the macro call.
fn call_site(b: &BodyCx<'_, '_, '_>, call: MacroCallId) -> Option<At> {
    at(b, call.loc(b.db()).to_node_view(b.db()))
}

/// rustc's `Span::source_callsite`: the call of the outermost macro `at` came out of, written
/// in a file, or `at` when it is.
fn source_callsite(b: &BodyCx<'_, '_, '_>, mut at: At) -> Option<At> {
    while let Some(call) = at.expn {
        at = call_site(b, call)?;
    }
    Some(at)
}

/// rustc's `Span::in_external_macro`: made by a bang macro of another crate (a built-in one
/// included), or by an attribute or derive macro.
fn in_external_macro(b: &BodyCx<'_, '_, '_>, at: &At) -> bool {
    let Some(call) = at.expn else { return false };
    let loc = call.loc(b.db());
    match loc.kind {
        MacroCallKind::FnLike { .. } => hir::Crate::from(loc.def.krate) != b.cx.krate,
        _ => true,
    }
}

/// rustc's `panic_call`: from the expansion `f` (the callee) is in, up the calls of the panic
/// macros that made it (`panic!`, `assert!`, `debug_assert!`, `unreachable!`, which a 2015
/// macro was likely expanded from): the span of the outermost call, its macro's diagnostic item
/// and its name as called.
fn panic_call(b: &BodyCx<'_, '_, '_>, f: &At) -> Option<(At, Option<SmolStr>, String)> {
    let db = b.db();
    let mut expn = f.expn?;
    let mut panic = None;
    loop {
        let Some(parent) = call_site(b, expn)?.expn else { break };
        let Some(name) = macro_item(db, parent.loc(db).def) else { break };
        let is_panic = matches!(
            name.as_str(),
            "core_panic_macro" | "std_panic_macro" | "assert_macro" | "debug_assert_macro" | "unreachable_macro"
        );
        if !is_panic {
            break;
        }
        expn = parent;
        panic = Some(name);
    }
    Some((call_site(b, expn)?, panic, macro_symbol(b, expn)))
}

/// rustc's `ExpnKind::Macro` name of an expansion: the path a bang macro is called by
/// (`std::panic`), the name of an attribute or derive macro.
fn macro_symbol(b: &BodyCx<'_, '_, '_>, call: MacroCallId) -> String {
    let db = b.db();
    let loc = call.loc(db);
    let name: Option<String> = match loc.kind {
        MacroCallKind::FnLike { .. } => ast::MacroCall::cast(loc.to_node_view(db).value)
            .and_then(|it| it.path())
            // alloc: the macro's name as called, on the lint's path (a 2015 panic not given a plain literal)
            .map(|it| it.syntax().text().split_whitespace().collect()),
        _ => b.cx.expansion_of(call).map(|it| it.name),
    };
    // alloc: the lint's diagnostic
    name.unwrap_or_else(|| "panic".to_owned())
}

/// The name `#[rustc_diagnostic_item = "name"]` gives the macro `def`, its `cfg_attr`s
/// expanded: what rustc's `get_diagnostic_name` gives of a macro's `DefId`.
fn macro_item(db: &Db, def: MacroDefId) -> Option<SmolStr> {
    let attrs = match def.kind {
        MacroDefKind::Declarative(id, _)
        | MacroDefKind::BuiltIn(id, _)
        | MacroDefKind::BuiltInAttr(id, _)
        | MacroDefKind::BuiltInDerive(id, _)
        | MacroDefKind::BuiltInEager(id, _)
        | MacroDefKind::UnimplementedBuiltIn(id) => id.to_view(db).attrs(),
        MacroDefKind::ProcMacro(id, ..) => id.to_view(db).attrs(),
    };
    let cfg = def.krate.cfg_options(db);
    expand_cfg_attr_view(attrs, || cfg, |meta, _| match meta {
        ast::Meta::KeyValueMeta(attr) if attr.path().is1("rustc_diagnostic_item") => match attr.value_string() {
            Some(name) => ControlFlow::Break(name),
            None => ControlFlow::Continue(()),
        },
        _ => ControlFlow::Continue(()),
    })
}

/// rustc's `is_arg_inside_call`: the argument is written inside the call, and is not spanned
/// as the whole call (as a proc macro may span every token it makes).
fn is_arg_inside_call(arg: &At, call: &At) -> bool {
    call.contains(arg) && call.range != arg.range
}

/// rustc's `find_delimiters`: in the text of a macro call, where its first opening delimiter
/// and its last closing one are, and which the opening one is.
fn find_delimiters(b: &BodyCx<'_, '_, '_>, call: &At) -> Option<(usize, usize, char)> {
    let snippet = snippet(b, call)?;
    let (open, open_ch) = snippet.char_indices().find(|&(_, c)| "([{".contains(c))?;
    let close = snippet.rfind(|c| ")]}".contains(c))?;
    Some((open, close, open_ch))
}

/// The text `at` is written as.
fn snippet<'db>(b: &BodyCx<'_, '_, 'db>, at: &At) -> Option<&'db str> {
    b.cx.file_text(at.file).get(usize::from(at.range.start())..usize::from(at.range.end()))
}

/// The span `at` is shown by: where it is written when it is in a file, else as
/// [`crate::cx::Cx::span`] shows the expansion it is in.
fn show(b: &BodyCx<'_, '_, '_>, at: &At) -> Span {
    match at.expn {
        None => b.cx.span_in_file(at.file, at.range),
        Some(_) => b.cx.span(at.shown),
    }
}

/// rustc's `Span::from_inner`: bytes `start..end` of what `at` is written as, shown where `at`
/// is; all of `at` when it is shown elsewhere than where it is written (at the call of another
/// crate's macro).
fn inner(b: &BodyCx<'_, '_, '_>, at: &At, start: usize, end: usize) -> Span {
    let shown = show(b, at);
    if shown.hi - shown.lo != u32::from(at.range.len()) {
        return shown;
    }
    Span { lo: shown.lo + start as u32, hi: shown.lo + end as u32, ..shown }
}
