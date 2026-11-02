//! The checks of rustc's AST validation of functions, closures and function pointer types:
//! `visit_fn`'s (parameters, safety, the signature an ABI allows, `#[track_caller]`,
//! C-variadics, `const` with `async`, patterns of a function without a body or in an `extern`
//! block), and `check_fn_decl`, `check_extern_fn_signature` and `check_decl_no_pat`, which
//! function pointer types share.
//!
//! `patterns_in_fns_without_body` (a `mut x` of an associated function without a body) is
//! the early lints' (`lints::syntax::early::validation`), not given here.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`visit_fn`,
//! `check_fn_decl`, `check_decl_num_args`, `check_decl_cvariadic_pos`,
//! `check_decl_splatting`, `check_decl_attrs`, `check_decl_self_param`,
//! `check_extern_fn_signature`, `reject_safe_fn`, `reject_coroutine`, `reject_return`,
//! `reject_params_or_return`, `check_c_variadic_type`, `check_c_variadic_abi`,
//! `check_decl_no_pat`, `SplatSemantic`) and `diagnostics.rs` (`PatternInForeign`,
//! `PatternInBodiless`), `compiler/rustc_ast/src/ast.rs`
//! (`FnHeader::span`, `FnSig::header_span`, `safety_span`, `extern_span`, `Param::is_self`),
//! `compiler/rustc_abi/src/extern_abi.rs` (the ABIs' names, `supports_c_variadic`),
//! `compiler/rustc_target/src/spec/abi_map.rs` (`canonize_abi`) and
//! `compiler/rustc_target/src/spec/mod.rs` (`supports_c_variadic_definitions`) (MIT or
//! Apache-2.0).

use blaze_ids::radix;
use hir::InFile;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, AstToken as _, HasName as _},
};

use super::{Ctxt, Out, V, attr_range, feature_err, is_named, items, text_of, unraw};
use crate::{
    cx::signature_range_view as signature_range,
    lints::syntax::attrs::table::is_builtin_attr_name,
    model::{Applicability, Diag, Span},
};

/// rustc's `FnDecl::NO_SPLATTED_ARG_INDEX`: a `#[rustc_splat]` on a parameter at this index
/// or after it is refused.
const NO_SPLATTED_ARG_INDEX: usize = u8::MAX as usize;

const ONLY_C: &str = "only `extern \"C\"` and `extern \"C-unwind\"` functions may have a C variable argument list";

/// The ABIs rustc knows (`ExternAbi::from_str`).
const ABIS: &[&str] = &[
    "C", "C-unwind", "Rust", "Swift", "aapcs", "aapcs-unwind", "avr-interrupt", "avr-non-blocking-interrupt", "cdecl",
    "cdecl-unwind", "cmse-nonsecure-call", "cmse-nonsecure-entry", "custom", "efiapi", "fastcall", "fastcall-unwind",
    "gpu-kernel", "msp430-interrupt", "ptx-kernel", "riscv-interrupt-m", "riscv-interrupt-s", "rust-call", "rust-cold",
    "rust-invalid", "rust-preserve-none", "stdcall", "stdcall-unwind", "system", "system-unwind", "sysv64",
    "sysv64-unwind", "tail", "thiscall", "thiscall-unwind", "unadjusted", "vectorcall", "vectorcall-unwind", "win64",
    "win64-unwind", "x86-interrupt",
];

/// An ABI's string literal (`T![string]`, which always casts to `ast::String`).
pub(super) fn abi_string(abi: ast::Abi<'_>) -> Option<ast::String<'_>> {
    abi.string_token().and_then(ast::String::cast)
}

/// `f` of the name of the ABI `string` (an ABI's string literal) gives, unescaped.
pub(super) fn with_abi_text<R>(string: ast::String<'_>, f: impl FnOnce(&str) -> R) -> R {
    match string.value() {
        Ok(text) => f(&*text),
        Err(_) => f(string.text().trim_matches('"')),
    }
}

/// `ExternAbi::from_str`: the ABI named `text`, when rustc knows it.
pub(super) fn known_abi(text: &str) -> Option<&'static str> {
    ABIS.iter().copied().find(|it| *it == text)
}

/// `ExternAbi::from_str` of the ABI string literal `string`.
pub(super) fn known_abi_of(string: ast::String<'_>) -> Option<&'static str> {
    with_abi_text(string, known_abi)
}

/// The ABIs of [`ABIS`] no feature gates (`extern_abi_stability`): those `enabled_names`
/// suggests from when no ABI feature is on.
fn stable_abi(name: &str) -> bool {
    !matches!(
        name,
        "custom"
            | "gpu-kernel"
            | "ptx-kernel"
            | "msp430-interrupt"
            | "x86-interrupt"
            | "avr-interrupt"
            | "avr-non-blocking-interrupt"
            | "riscv-interrupt-m"
            | "riscv-interrupt-s"
            | "cmse-nonsecure-call"
            | "cmse-nonsecure-entry"
            | "rust-call"
            | "rust-cold"
            | "rust-invalid"
            | "rust-preserve-none"
            | "tail"
            | "unadjusted"
            | "vectorcall"
            | "vectorcall-unwind"
            | "Swift"
    )
}

/// E0703 of an `extern "x"` (a function, a function pointer type, an `extern` block) whose
/// string is no ABI rustc knows: rustc's `error_on_invalid_abi`, as it lowers the ABI, which
/// then lowers to the default one. The build stops on it, never on that default.
pub(super) fn invalid_abi(v: &V<'_, '_, '_>, node: ast::Node<'_>, out: &mut Out) {
    let Some(string) = ast::Abi::cast(node).and_then(abi_string) else { return };
    if known_abi_of(string).is_some() {
        return;
    }
    let at = v.span(string.syntax().text_range());
    // alloc: the error's message
    let found = with_abi_text(string, str::to_owned);
    // clone: a handle, on the error path
    let mut diag = Diag::error(Some("E0703"), format!("invalid ABI: found `{found}`"), at.clone());
    // clone: a handle, on the error path
    diag.span_label(at.clone(), "invalid ABI");
    diag.note("invoke `rustc --print=calling-conventions` for a full list of supported calling conventions");
    // alloc: the candidates, on the error path
    let stable: Vec<&str> = ABIS.iter().copied().filter(|it| stable_abi(it)).collect();
    if let Some(suggestion) = stdx::edit_distance::find_best_match_for_name(&stable, &found, None) {
        diag.suggestion_verbose(
            at,
            // alloc: the error's message
            format!("there's a similarly named valid ABI `{suggestion}`"),
            // alloc: the error's message
            format!("\"{suggestion}\""),
            Applicability::MaybeIncorrect,
        );
    }
    v.push(out, node, diag, super::Phase::Lower, super::Pass::Early);
}

/// The ABI a function's or a function pointer's `extern` gives (`FALLBACK`, `"C"`, for one
/// without a string), when rustc knows it.
pub(super) fn abi_of(abi: ast::Abi<'_>) -> Option<&'static str> {
    match abi_string(abi) {
        None => Some("C"),
        Some(string) => known_abi_of(string),
    }
}

/// `CoroutineKind`: a function's `async`, `gen` or `async gen`, where it is written.
pub(super) fn coroutine_kind(item: ast::Fn<'_>) -> Option<(TextRange, &'static str)> {
    match (item.async_token(), item.gen_token()) {
        (Some(async_), Some(gen_)) => Some((async_.text_range().cover(gen_.text_range()), "async gen")),
        (Some(async_), None) => Some((async_.text_range(), "async")),
        (None, Some(gen_)) => Some((gen_.text_range(), "gen")),
        (None, None) => None,
    }
}

/// `SplatSemantic`: where `#[rustc_splat]` may be.
#[derive(Clone, Copy)]
pub(super) enum Splat {
    Yes,
    /// On a closure, the closure's range.
    NoClosures(TextRange),
    /// With `extern "rust-call"`, where it is written.
    NoAbiCall(TextRange),
}

impl Splat {
    /// `SplatSemantic::from_extern`.
    pub(super) fn of(abi: Option<ast::Abi<'_>>) -> Splat {
        let Some(abi) = abi else { return Splat::Yes };
        match abi_string(abi) {
            Some(string) if with_abi_text(string, |it| it == "rust-call") => {
                Splat::NoAbiCall(abi.syntax().text_range())
            }
            _ => Splat::Yes,
        }
    }
}

/// The parameters of a declaration, without a heap allocation for up to eight.
pub(super) type Inputs<'db> = SmallVec<[Input<'db>; 8]>;

/// A parameter's node: blaze's `self` parameter, or any other.
#[derive(Clone, Copy)]
enum InputNode<'db> {
    SelfParam(ast::SelfParam<'db>),
    Param(ast::Param<'db>),
}

/// A parameter of a declaration, as rustc's `FnDecl::inputs` has it.
pub(super) struct Input<'db> {
    node: InputNode<'db>,
    /// rustc's `Param::span`: without its attributes; of `self`, up to the `self`.
    pub(super) range: TextRange,
    variadic: bool,
    is_self: bool,
}

impl<'db> Input<'db> {
    fn syntax(&self) -> ast::Node<'db> {
        match self.node {
            InputNode::SelfParam(it) => it.syntax(),
            InputNode::Param(it) => it.syntax(),
        }
    }
}

/// The parameters of `list` `cfg` keeps, `self` first.
pub(super) fn inputs<'db>(v: &V<'_, '_, 'db>, list: Option<ast::ParamList<'db>>) -> Inputs<'db> {
    let mut inputs = Inputs::new();
    let Some(list) = list else { return inputs };
    if let Some(it) = list.self_param()
        && v.kept(it.syntax())
    {
        let start = ast::written_range(it.syntax()).start();
        let end = it.name().map_or(it.syntax().text_range().end(), |name| name.syntax().text_range().end());
        inputs.push(Input {
            node: InputNode::SelfParam(it),
            range: TextRange::new(start, end.max(start)),
            variadic: false,
            is_self: true,
        });
    }
    // rustc's parser reads a `self` parameter of a function pointer as one (`parse_self_param`),
    // up to the `self`; a closure's is a pattern, the whole parameter.
    let fn_ptr = list.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::FN_PTR_TYPE);
    for param in list.params().filter(|it| v.kept(it.syntax())) {
        let is_self = param_is_self(param);
        let range = match param.pat() {
            Some(pat) if is_self && fn_ptr => pat.syntax().text_range(),
            _ => ast::written_range(param.syntax()),
        };
        let variadic = param.dotdotdot_token().is_some();
        inputs.push(Input { node: InputNode::Param(param), range, variadic, is_self });
    }
    inputs
}

/// `Param::is_self` of a parameter that is not blaze's `self` parameter: a closure's `self`
/// pattern, or a function pointer's `self`, `&self` or `&mut self` the parser read as a type.
fn param_is_self(param: ast::Param<'_>) -> bool {
    if let Some(pat) = param.pat() {
        return matches!(pat, ast::Pat::IdentPat(it) if it.name().is_some_and(|it| it.text() == "self"));
    }
    let mut ty = param.ty();
    if let Some(ast::Type::RefType(it)) = ty {
        ty = it.ty();
    }
    match ty {
        Some(ast::Type::PathType(it)) => it
            .path()
            .is_some_and(|it| it.qualifier().is_none() && it.segment().and_then(|it| it.name_ref()).is_some_and(|it| it.text() == "self")),
        _ => false,
    }
}

/// `check_fn_decl`: the number of parameters, where `...` is, `#[rustc_splat]`, the
/// attributes of parameters, and `self` where it is not allowed.
pub(super) fn check_decl<'db>(
    v: &V<'_, '_, 'db>,
    node: ast::Node<'db>,
    inputs: &[Input<'db>],
    self_allowed: bool,
    splat: Splat,
    out: &mut Out,
) {
    if inputs.len() > usize::from(u16::MAX) {
        // alloc: the error's message
        let message = format!("function can not have more than {} arguments", u16::MAX);
        v.emit(out, node, Diag::error(None, message, v.span(inputs[0].range)));
    }
    let mut c_variadic = None;
    if let [before @ .., last] = inputs {
        for input in before.iter().filter(|it| it.variadic) {
            c_variadic = Some(input.range);
            let message = "`...` must be the last argument of a C-variadic function";
            v.emit(out, node, Diag::error(None, message, v.span(input.range)));
        }
        if last.variadic {
            c_variadic = Some(last.range);
        }
    }
    splatting(v, node, inputs, c_variadic, splat, out);
    param_attrs(v, node, inputs, out);
    if !self_allowed
        && let Some(first) = inputs.first()
        && first.is_self
    {
        let span = v.span(first.range);
        // clone: a span, on the error path
        let mut diag = Diag::error(None, "`self` parameter is only allowed in associated functions", span.clone());
        diag.note("associated functions are those in `impl` or `trait` definitions");
        diag.span_label(span, "not semantically valid as function parameter");
        v.emit(out, node, diag);
    }
}

/// `check_decl_splatting`.
fn splatting<'db>(
    v: &V<'_, '_, 'db>,
    node: ast::Node<'db>,
    inputs: &[Input<'db>],
    c_variadic: Option<TextRange>,
    splat: Splat,
    out: &mut Out,
) {
    let mut splatted: SmallVec<[(usize, TextRange); 2]> = SmallVec::new();
    for (index, input) in inputs.iter().enumerate() {
        v.each_attr(input.syntax(), |meta, attr| {
            if is_named(meta, "rustc_splat") {
                splatted.push((index, attr_range(meta, attr)));
            }
        });
    }
    if splatted.is_empty() {
        return;
    }
    let at = splatted.partition_point(|(index, _)| *index < NO_SPLATTED_ARG_INDEX);
    let (valid, invalid) = splatted.split_at(at);
    if let Some(&(first, _)) = invalid.first() {
        // alloc: the error's message
        let message = format!(
            "`#[rustc_splat]` is only supported on argument index {} or less, this `#[rustc_splat]` is on index {first}",
            NO_SPLATTED_ARG_INDEX - 1
        );
        // alloc: the error's spans
        let spans: Vec<Span> = invalid.iter().map(|it| v.span(it.1)).collect();
        // clone: a span, on the error path
        let mut diag = Diag::error(None, message, spans[0].clone());
        for span in &spans[1..] {
            // clone: as above
            diag.primary(span.clone());
        }
        diag.help("remove `#[rustc_splat]`, or use it on an argument closer to the start of the argument list");
        for span in spans {
            diag.span_label(span, "`#[rustc_splat]` is not supported here");
        }
        v.emit(out, node, diag);
    }
    let Some(&(first_index, _)) = valid.first() else { return };
    let many = |message: &str, help: &str, extra: Option<TextRange>| {
        // alloc: the error's spans
        let spans: Vec<Span> = valid.iter().map(|it| it.1).chain(extra).map(|it| v.span(it)).collect();
        // clone: a span, on the error path
        let mut diag = Diag::error(None, message, spans[0].clone());
        for span in &spans[1..] {
            // clone: as above
            diag.primary(span.clone());
        }
        diag.help(help);
        diag
    };
    if valid.iter().any(|(index, _)| *index != first_index) {
        let diag = many(
            "multiple `#[rustc_splat]`s are not allowed in the same function argument list",
            "remove `#[rustc_splat]` from all but one argument",
            None,
        );
        v.emit(out, node, diag);
    }
    if let Some(variadic) = c_variadic {
        let diag = many(
            "`...` and `#[rustc_splat]` are not allowed in the same function argument list",
            "remove `#[rustc_splat]` or remove `...`",
            Some(variadic),
        );
        v.emit(out, node, diag);
    }
    match splat {
        Splat::Yes => {}
        Splat::NoClosures(closure) => {
            let diag = many(
                "`#[rustc_splat]` is not allowed on closure arguments",
                "remove `#[rustc_splat]` or turn the closure into a function",
                Some(closure),
            );
            v.emit(out, node, diag);
        }
        Splat::NoAbiCall(abi) => {
            let diag = many(
                "`#[rustc_splat]` is not allowed in the arguments of functions with the `rust-call` ABI",
                "remove `#[rustc_splat]` or change the ABI",
                Some(abi),
            );
            v.emit(out, node, diag);
        }
    }
}

/// The lint attributes and `#[rustc_splat]`: the builtin attributes a parameter may have.
const PARAM_ATTRS: &[&str] = &["allow", "deny", "expect", "forbid", "rustc_splat", "warn"];

/// `check_decl_attrs`: doc comments and builtin attributes other than the lint ones on
/// parameters, in the order they are written.
fn param_attrs<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, inputs: &[Input<'db>], out: &mut Out) {
    for input in inputs {
        let mut found: SmallVec<[(TextRange, bool); 4]> = SmallVec::new();
        for doc in input.syntax().children().filter(|it| it.kind() == SyntaxKind::DOC_COMMENT) {
            found.push((doc.text_range(), true));
        }
        v.each_attr(input.syntax(), |meta, attr| {
            if matches!(meta, ast::Meta::CfgMeta(_) | ast::Meta::CfgAttrMeta(_)) {
                return;
            }
            let Some(name) = meta.path().and_then(|it| it.as_single_name_ref()) else { return };
            let name = name.text();
            let name = unraw(name);
            if is_builtin_attr_name(name) && !PARAM_ATTRS.contains(&name) {
                found.push((attr_range(meta, attr), false));
            }
        });
        if found.is_empty() {
            continue;
        }
        radix::sort_copy_by_key(&mut found, |(range, _)| u32::from(range.start()));
        for (range, doc) in found {
            let span = v.span(range);
            let diag = if doc {
                let message = "documentation comments cannot be applied to function parameters";
                // clone: a span, on the error path
                let mut diag = Diag::error(None, message, span.clone());
                diag.span_label(span, "doc comments are not allowed here");
                diag
            } else {
                Diag::error(
                    None,
                    "allow, cfg, cfg_attr, deny, expect, forbid, and warn are the only allowed built-in attributes in function parameters",
                    span,
                )
            };
            v.emit(out, node, diag);
        }
    }
}

/// `check_decl_no_pat`: each parameter bound by anything but a name or `_` (a pattern macro
/// call as it expands), with whether it is a `mut` name (`mut x`). A trait's method in Rust
/// 2015 is not checked where rustc's parser already refused the pattern and put `_` in its
/// place (`recover_arg_parse`).
pub(super) fn patterns<'db>(
    v: &V<'_, '_, 'db>,
    inputs: &[Input<'db>],
    parser_recovers: bool,
    mut report: impl FnMut(Span, bool),
) {
    for input in inputs {
        let param = match input.node {
            InputNode::SelfParam(it) => {
                if it.amp_token().is_none()
                    && let (Some(mut_), Some(name)) = (it.mut_token(), it.name())
                {
                    report(v.span(TextRange::new(mut_.text_range().start(), name.syntax().text_range().end())), true);
                }
                continue;
            }
            InputNode::Param(it) => it,
        };
        let Some(pat) = param.pat() else { continue };
        if parser_recovers && !named_param(pat) {
            continue;
        }
        let pat = expanded_pat(v, pat);
        let span = || v.cx.span(pat.map(|it| it.syntax().text_range()));
        match pat.value {
            ast::Pat::WildcardPat(_) => {}
            ast::Pat::IdentPat(it) if it.ref_token().is_none() && it.pat().is_none() => {
                if it.mut_token().is_some() {
                    report(span(), true);
                }
            }
            _ => report(span(), false),
        }
    }
}

/// How deep pattern macro calls in a parameter's place are expanded.
const MACRO_DEPTH: usize = 128;

/// `pat` once the pattern macro calls in its place are expanded, as rustc's validator sees
/// it, in the file it is written in.
fn expanded_pat<'db>(v: &V<'_, '_, 'db>, pat: ast::Pat<'db>) -> InFile<ast::Pat<'db>> {
    let mut pat = InFile::new(v.tree.file, pat);
    for _ in 0..MACRO_DEPTH {
        let ast::Pat::MacroPat(call) = pat.value else { break };
        let Some(root) = call.macro_call().and_then(|it| v.cx.vsema.expand_macro_call(it)) else { break };
        let Some(next) = ast::Pat::cast(root.value) else { break };
        pat = InFile::new(root.file_id, next);
    }
    pat
}

/// rustc's `Parser::is_named_param` of a parameter of a trait's method in Rust 2015: a name
/// (after `&`, `&&` or `mut`) followed by `:`.
fn named_param(pat: ast::Pat<'_>) -> bool {
    let plain = |pat: ast::Pat<'_>| matches!(pat, ast::Pat::IdentPat(it) if it.ref_token().is_none() && it.mut_token().is_none() && it.pat().is_none());
    match pat {
        ast::Pat::IdentPat(it) => it.ref_token().is_none() && it.pat().is_none(),
        ast::Pat::RefPat(it) if it.mut_token().is_none() => match it.pat() {
            Some(ast::Pat::RefPat(inner)) if inner.mut_token().is_none() => inner.pat().is_some_and(plain),
            Some(inner) => plain(inner),
            None => false,
        },
        _ => false,
    }
}

/// rustc's `visit_fn` of a function, after `visit_item`'s, `visit_foreign_item`'s or
/// `visit_assoc_item`'s checks of it.
pub(super) fn visit_fn<'db>(
    v: &V<'_, '_, 'db>,
    node: ast::Node<'db>,
    item: ast::Fn<'db>,
    ctxt: &Ctxt<'db>,
    out: &mut Out,
) {
    let inputs = inputs(v, item.param_list());
    let abi = item.abi();
    check_decl(v, node, &inputs, ctxt.is_assoc(), Splat::of(abi), out);
    match ctxt {
        Ctxt::Foreign(block) => {
            items::safety_in_extern(v, node, item.safe_token().or(item.unsafe_token()), *block, out);
        }
        _ => items::safe_outside_extern(v, node, item.safe_token(), out),
    }
    if let Some(abi) = abi
        && let Some(name) = abi_of(abi)
    {
        let sig = Sig::of_fn(item, inputs.as_slice(), matches!(ctxt, Ctxt::Foreign(_)));
        signature(v, node, name, &sig, out);
        if let Some(attr) = v.attr(node, "track_caller")
            && name != "Rust"
        {
            requires_rust_abi(v, node, attr, v.span(abi.syntax().text_range()), out);
        }
    }
    c_variadic(v, node, item, &inputs, ctxt, out);
    if let (Some(const_), Some((coroutine, kind))) = (item.const_token(), coroutine_kind(item)) {
        let const_span = v.span(const_.text_range());
        let coroutine_span = v.span(coroutine);
        // alloc: the error's message
        let mut diag = Diag::error(None, format!("functions cannot be both `const` and `{kind}`"), coroutine_span.clone());
        // clone: a span, on the error path
        diag.primary(const_span.clone());
        diag.span_label(const_span, "`const` because of this");
        // alloc: the error's label
        diag.span_label(coroutine_span, format!("`{kind}` because of this"));
        diag.span_label(v.span(super::item_range(node)), "");
        v.emit(out, node, diag);
    }
    if item.body().is_none() {
        let parser_recovers = matches!(ctxt, Ctxt::Trait(_))
            && !v.edition_at(super::item_range(node).start()).at_least_2018();
        let foreign = matches!(ctxt, Ctxt::Foreign(_));
        patterns(v, &inputs, parser_recovers, |span, mut_ident| {
            if mut_ident && ctxt.is_assoc() {
                return;
            }
            let diag = if foreign {
                let message = "patterns aren't allowed in foreign function declarations";
                // clone: a span, on the error path
                let mut diag = Diag::error(Some("E0130"), message, span.clone());
                diag.span_label(span, "pattern not allowed in foreign function");
                diag
            } else {
                let message = "patterns aren't allowed in functions without bodies";
                // clone: a span, on the error path
                let mut diag = Diag::error(Some("E0642"), message, span.clone());
                diag.span_label(span, "pattern not allowed in function without body");
                diag
            };
            v.emit(out, node, diag);
        });
    }
}

/// rustc's `visit_fn` of a closure: `check_fn_decl`, `self` not allowed.
pub(super) fn closure<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(closure) = ast::ClosureExpr::cast(node) else { return };
    let inputs = inputs(v, closure.param_list());
    check_decl(v, node, &inputs, false, Splat::NoClosures(node.text_range()), out);
}

/// E0737 `RequiresRustAbi`: `#[track_caller]` at `attr` with the ABI written at `abi`.
pub(super) fn requires_rust_abi(v: &V<'_, '_, '_>, node: ast::Node<'_>, attr: TextRange, abi: Span, out: &mut Out) {
    let span = v.span(attr);
    // clone: a span, on the error path
    let mut diag = Diag::error(Some("E0737"), "`#[track_caller]` can only be used with the Rust ABI", span.clone());
    diag.span_label(span, "using `#[track_caller]` here");
    diag.span_label(abi, "not using the Rust ABI because of this");
    v.emit(out, node, diag);
}

/// `check_extern_fn_signature` of a function in an `extern` block, with the block's ABI.
pub(super) fn extern_signature<'db>(
    v: &V<'_, '_, 'db>,
    node: ast::Node<'db>,
    item: ast::Fn<'db>,
    abi: &str,
    out: &mut Out,
) {
    let inputs = inputs(v, item.param_list());
    let sig = Sig::of_fn(item, &inputs, true);
    signature(v, node, abi, &sig, out);
}

/// What `check_extern_fn_signature` reads of a signature (`BorrowedFnSig`).
pub(super) struct Sig<'i, 'db> {
    /// `sig.span`: a function's signature, or a function pointer's type after its binder.
    span: TextRange,
    safe: Option<TextRange>,
    unsafe_: Option<TextRange>,
    coroutine: Option<(TextRange, &'static str)>,
    /// `FnHeader::span`: from its first qualifier to its last.
    header: Option<TextRange>,
    inputs: &'i [Input<'db>],
    /// Where an absent return type would be: after the parameters.
    params_end: TextSize,
    ret: Option<ast::Type<'db>>,
    name: Option<ast::Name<'db>>,
    foreign: bool,
    fn_ptr: bool,
}

impl<'i, 'db> Sig<'i, 'db> {
    fn of_fn(item: ast::Fn<'db>, inputs: &'i [Input<'db>], foreign: bool) -> Sig<'i, 'db> {
        let coroutine = coroutine_kind(item);
        let qualifiers = [
            item.abi().map(|it| it.syntax().text_range()),
            item.safe_token().or(item.unsafe_token()).map(|it| it.text_range()),
            coroutine.map(|it| it.0),
            item.const_token().map(|it| it.text_range()),
        ];
        Sig {
            span: signature_range(item),
            safe: item.safe_token().map(|it| it.text_range()),
            unsafe_: item.unsafe_token().map(|it| it.text_range()),
            coroutine,
            header: qualifiers.into_iter().flatten().reduce(TextRange::cover),
            inputs,
            params_end: params_end(item.param_list(), item.syntax()),
            ret: item.ret_type().and_then(|it| it.ty()),
            name: item.name(),
            foreign,
            fn_ptr: false,
        }
    }

    pub(super) fn of_fn_ptr(ty: ast::FnPtrType<'db>, inputs: &'i [Input<'db>]) -> Sig<'i, 'db> {
        let safe = safe_token(ty.syntax()).map(|it| it.text_range());
        let qualifiers =
            [ty.abi().map(|it| it.syntax().text_range()), safe.or(ty.unsafe_token().map(|it| it.text_range()))];
        Sig {
            span: decl_range(ty.syntax()),
            safe,
            unsafe_: ty.unsafe_token().map(|it| it.text_range()),
            coroutine: None,
            header: qualifiers.into_iter().flatten().reduce(TextRange::cover),
            inputs,
            params_end: params_end(ty.param_list(), ty.syntax()),
            ret: ty.ret_type().and_then(|it| it.ty()),
            name: None,
            foreign: false,
            fn_ptr: true,
        }
    }

    /// `decl.output.span()`: the return type, or where it would be.
    fn output(&self) -> TextRange {
        self.ret.map_or(TextRange::empty(self.params_end), |it| it.syntax().text_range())
    }
}

/// rustc's `decl_span` of a function pointer type: from its qualifiers to its end, without
/// its `for<..>`.
pub(super) fn decl_range(node: ast::Node<'_>) -> TextRange {
    let start = node
        .children_with_tokens()
        .find(|it| match it {
            NodeOrToken::Node(node) => node.kind() != SyntaxKind::FOR_BINDER,
            NodeOrToken::Token(token) => !token.kind().is_trivia(),
        })
        .map_or(node.text_range().start(), |it| it.text_range().start());
    TextRange::new(start, node.text_range().end())
}

/// A function pointer type's `safe`, which blaze's tree keeps as a token of the type.
pub(super) fn safe_token(node: ast::Node<'_>) -> Option<ast::Token<'_>> {
    node.children_with_tokens().filter_map(NodeOrToken::into_token).find(|it| it.kind() == SyntaxKind::SAFE_KW)
}

fn params_end(list: Option<ast::ParamList<'_>>, owner: ast::Node<'_>) -> TextSize {
    list.map_or(owner.text_range().end(), |it| it.syntax().text_range().end())
}

/// What `canonize_abi` makes of an ABI on the crate's target, as far as
/// `check_extern_fn_signature` asks.
enum Canon {
    Unchecked,
    GpuKernel,
    Custom,
    Interrupt { x86: bool },
}

fn canon(abi: &str, arch: Option<&str>) -> Canon {
    let arch = arch.unwrap_or("");
    let riscv = matches!(arch, "riscv32" | "riscv64");
    match abi {
        "custom" if !matches!(arch, "wasm32" | "wasm64" | "spirv") => Canon::Custom,
        "gpu-kernel" if matches!(arch, "amdgpu" | "nvptx64") => Canon::GpuKernel,
        "ptx-kernel" if arch == "nvptx64" => Canon::GpuKernel,
        "x86-interrupt" if matches!(arch, "x86" | "x86_64") => Canon::Interrupt { x86: true },
        "avr-interrupt" | "avr-non-blocking-interrupt" if arch == "avr" => Canon::Interrupt { x86: false },
        "msp430-interrupt" if arch == "msp430" => Canon::Interrupt { x86: false },
        "riscv-interrupt-m" | "riscv-interrupt-s" if riscv => Canon::Interrupt { x86: false },
        _ => Canon::Unchecked,
    }
}

/// `check_extern_fn_signature`: what the ABI `abi` allows of `sig`.
pub(super) fn signature(v: &V<'_, '_, '_>, node: ast::Node<'_>, abi: &str, sig: &Sig<'_, '_>, out: &mut Out) {
    match canon(abi, v.arch()) {
        Canon::Unchecked => {}
        Canon::GpuKernel => {
            reject_coroutine(v, node, abi, sig, out);
            reject_return(v, node, abi, sig, out);
        }
        Canon::Custom => {
            reject_safe_fn(v, node, sig, out);
            reject_coroutine(v, node, abi, sig, out);
            reject_params_or_return(v, node, abi, sig, out);
        }
        Canon::Interrupt { x86: true } => {
            reject_coroutine(v, node, abi, sig, out);
            let count = sig.inputs.len();
            if !matches!(count, 1 | 2) {
                let ranges: Vec<TextRange> = match sig.inputs {
                    // alloc: the error's spans
                    [] => vec![sig.span],
                    // alloc: as above
                    inputs => inputs.iter().map(|it| it.range).collect(),
                };
                let mut diag = Diag::error(None, "invalid signature for `extern \"x86-interrupt\"` function", v.span(ranges[0]));
                for range in &ranges[1..] {
                    diag.primary(v.span(*range));
                }
                // alloc: the error's note
                diag.note(format!(
                    "functions with the \"x86-interrupt\" ABI must be have either 1 or 2 parameters (but found {count})"
                ));
                v.emit(out, node, diag);
            }
            reject_return(v, node, abi, sig, out);
        }
        Canon::Interrupt { x86: false } => {
            reject_coroutine(v, node, abi, sig, out);
            reject_params_or_return(v, node, abi, sig, out);
        }
    }
}

/// rustc's `span_until_non_whitespace` of `range` of `node`: up to the first character that
/// is not white space after some white space.
fn until_non_whitespace(node: ast::Node<'_>, range: TextRange) -> TextRange {
    let text = text_of(node, range);
    let mut seen_space = false;
    let mut len = 0;
    for c in text.chars() {
        if c.is_whitespace() {
            seen_space = true;
        } else if seen_space {
            break;
        }
        len += c.len_utf8();
    }
    TextRange::at(range.start(), TextSize::from(len as u32))
}

/// `reject_safe_fn` of the `"custom"` ABI.
fn reject_safe_fn(v: &V<'_, '_, '_>, node: ast::Node<'_>, sig: &Sig<'_, '_>, out: &mut Out) {
    if sig.unsafe_.is_some() {
        return;
    }
    let span = v.span(sig.span);
    match sig.safe {
        Some(_) if sig.fn_ptr => {}
        Some(safe) => {
            let mut diag = Diag::error(None, "foreign functions with the \"custom\" ABI cannot be safe", span);
            diag.suggestion_verbose(
                v.span(until_non_whitespace(node, safe.cover(sig.span))),
                "remove the `safe` keyword from this definition",
                "",
                Applicability::MaybeIncorrect,
            );
            v.emit(out, node, diag);
        }
        None if sig.foreign => {}
        None => {
            // clone: a span, on the error path
            let mut diag = Diag::error(None, "functions with the \"custom\" ABI must be unsafe", span.clone());
            diag.suggestion_verbose(
                span.shrink_to_lo(),
                "add the `unsafe` keyword to this definition",
                "unsafe ",
                Applicability::MaybeIncorrect,
            );
            v.emit(out, node, diag);
        }
    }
}

/// `reject_coroutine`: `async` or `gen` with an ABI that cannot have it.
fn reject_coroutine(v: &V<'_, '_, '_>, node: ast::Node<'_>, abi: &str, sig: &Sig<'_, '_>, out: &mut Out) {
    let Some((range, kind)) = sig.coroutine else { return };
    // alloc: the error's message
    let message = format!("functions with the \"{abi}\" ABI cannot be `{kind}`");
    let mut diag = Diag::error(None, message, v.span(sig.span));
    diag.suggestion_verbose(
        v.span(until_non_whitespace(node, range.cover(sig.span))),
        // alloc: the error's suggestion
        format!("remove the `{kind}` keyword from this definition"),
        "",
        Applicability::MaybeIncorrect,
    );
    v.emit(out, node, diag);
}

/// Whether `ty` is `!` or `()`.
fn never_or_unit(ty: ast::Type<'_>) -> (bool, bool) {
    match ty {
        ast::Type::NeverType(_) => (true, false),
        ast::Type::TupleType(it) => (false, it.fields().next().is_none()),
        _ => (false, false),
    }
}

/// `reject_return`: a return type other than `!` or `()`.
fn reject_return(v: &V<'_, '_, '_>, node: ast::Node<'_>, abi: &str, sig: &Sig<'_, '_>, out: &mut Out) {
    let Some(ret) = sig.ret else { return };
    let (never, unit) = never_or_unit(ret);
    if never || unit {
        return;
    }
    let span = v.span(ret.syntax().text_range());
    // alloc: the error's message, and a span clone on the error path
    let mut diag = Diag::error(None, format!("invalid signature for `extern \"{abi}\"` function"), span.clone());
    // alloc: the error's note
    diag.note(format!("functions with the \"{abi}\" ABI cannot have a return type"));
    diag.span_help(span, "remove the return type");
    v.emit(out, node, diag);
}

/// `reject_params_or_return`: any parameter, or a return type other than `()` (or `!` but for
/// the `"custom"` ABI).
fn reject_params_or_return(v: &V<'_, '_, '_>, node: ast::Node<'_>, abi: &str, sig: &Sig<'_, '_>, out: &mut Out) {
    let mut ranges: SmallVec<[TextRange; 8]> = SmallVec::new();
    ranges.extend(sig.inputs.iter().map(|it| it.range));
    if let Some(ret) = sig.ret {
        let (never, unit) = never_or_unit(ret);
        if !(unit || never && abi != "custom") {
            ranges.push(ret.syntax().text_range());
        }
    }
    let Some(&first) = ranges.first() else { return };
    let header = sig.header.unwrap_or(TextRange::empty(sig.span.start()));
    let suggestion = TextRange::empty(header.end()).cover(sig.output());
    let padding = if header.is_empty() { "" } else { " " };
    let symbol = sig.name.map_or("", |it| unraw(it.text()));
    let space = if sig.name.is_some() { " " } else { "" };
    // alloc: the error's message
    let mut diag = Diag::error(None, format!("invalid signature for `extern \"{abi}\"` function"), v.span(first));
    for range in &ranges[1..] {
        diag.primary(v.span(*range));
    }
    // alloc: the error's note
    diag.note(format!("functions with the \"{abi}\" ABI cannot have any parameters or return type"));
    diag.suggestion_verbose(
        v.span(suggestion),
        "remove the parameters and return type",
        // alloc: the error's suggestion
        format!("{padding}fn{space}{symbol}()"),
        Applicability::MaybeIncorrect,
    );
    v.emit(out, node, diag);
}

/// `supports_c_variadic_definitions` of the crate's target.
enum Variadics {
    Stable,
    /// Behind `c_variadic_experimental_arch`.
    Unstable,
    /// Never, with rustc's name of the target (`llvm_target`).
    NotSupported(&'static str),
}

fn variadics(v: &V<'_, '_, '_>) -> Variadics {
    match v.arch().unwrap_or("") {
        "bpf" => {
            let big = v.walk.cfg.get_cfg_values("target_endian").any(|it| it.as_str() == "big");
            Variadics::NotSupported(if big { "bpfeb" } else { "bpfel" })
        }
        "spirv" => Variadics::NotSupported("spirv"),
        "riscv32" if v.walk.cfg.get_cfg_values("target_abi").any(|it| it.as_str() == "ilp32e") => Variadics::Unstable,
        "sparc" | "avr" | "m68k" | "msp430" => Variadics::Unstable,
        "aarch64" | "amdgpu" | "arm" | "arm64ec" | "csky" | "hexagon" | "loongarch32" | "loongarch64" | "mips"
        | "mips32r6" | "mips64" | "mips64r6" | "nvptx64" | "powerpc" | "powerpc64" | "riscv32" | "riscv64" | "s390x"
        | "sparc64" | "wasm32" | "wasm64" | "x86" | "x86_64" | "xtensa" => Variadics::Stable,
        _ => Variadics::Unstable,
    }
}

/// `check_c_variadic_type`: what a function whose last parameter is `...` must be.
fn c_variadic<'db>(
    v: &V<'_, '_, 'db>,
    node: ast::Node<'db>,
    item: ast::Fn<'db>,
    inputs: &[Input<'db>],
    ctxt: &Ctxt<'db>,
    out: &mut Out,
) {
    let Some(last) = inputs.last().filter(|it| it.variadic) else { return };
    let variadic = v.span(last.range);
    let sig_span = v.span(signature_range(item));
    if item.const_token().is_some() && !v.feature("const_c_variadic") {
        // clone: a span, on the error path
        let diag = feature_err("const_c_variadic", "c-variadic const function definitions are unstable", sig_span.clone());
        v.emit(out, node, diag);
    }
    if let Some((coroutine, kind)) = coroutine_kind(item) {
        let coroutine = v.span(coroutine);
        // alloc: the error's message
        let message = format!("functions cannot be both `{kind}` and C-variadic");
        // clone: a span, on the error path
        let mut diag = Diag::error(None, message, coroutine.clone());
        // clone: as above
        diag.primary(variadic.clone());
        // alloc: the error's label
        diag.span_label(coroutine, format!("`{kind}` because of this"));
        // clone: a span, on the error path
        diag.span_label(variadic.clone(), "C-variadic because of this");
        v.emit(out, node, diag);
    }
    if matches!(ctxt, Ctxt::Foreign(_)) {
        return;
    }
    match variadics(v) {
        Variadics::NotSupported(target) => {
            // alloc: the error's message
            let message = format!("the `{target}` target does not support c-variadic functions");
            v.emit(out, node, Diag::error(None, message, variadic));
            return;
        }
        Variadics::Unstable if !v.feature("c_variadic_experimental_arch") => {
            let message = "C-variadic function definitions on this target are unstable";
            v.emit(out, node, feature_err("c_variadic_experimental_arch", message, variadic));
            return;
        }
        Variadics::Unstable | Variadics::Stable => {}
    }
    let Some(abi) = item.abi() else {
        let mut diag = Diag::error(None, "`...` is not supported for non-extern functions", variadic);
        diag.help(ONLY_C);
        v.emit(out, node, diag);
        return;
    };
    let abi_range = abi.syntax().text_range();
    if let Some(string) = abi_string(abi) {
        let Some(name) = known_abi_of(string) else { return };
        c_variadic_abi(v, node, name, &variadic, v.span(abi_range), sig_span, out);
    }
    if item.unsafe_token().is_none() {
        // `FnSig::safety_span`: the `safe`, else before the ABI.
        let at = item.safe_token().map_or(TextRange::empty(abi_range.start()), |it| it.text_range());
        let mut diag = Diag::error(None, "functions with a C variable argument list must be unsafe", variadic);
        diag.suggestion_verbose(
            v.span(at),
            "add the `unsafe` keyword to this definition",
            "unsafe ",
            Applicability::MaybeIncorrect,
        );
        v.emit(out, node, diag);
    }
}

/// `check_c_variadic_abi`: the ABIs a C-variadic definition may have, of a naked function and
/// of any other.
fn c_variadic_abi(
    v: &V<'_, '_, '_>,
    node: ast::Node<'_>,
    abi: &str,
    variadic: &Span,
    extern_span: Span,
    sig_span: Span,
    out: &mut Out,
) {
    let is_c = matches!(abi, "C" | "C-unwind");
    if v.has_attr(node, "naked") {
        // `ExternAbi::supports_c_variadic`.
        let supported = matches!(
            abi,
            "cdecl" | "cdecl-unwind" | "aapcs" | "aapcs-unwind" | "win64" | "win64-unwind" | "sysv64" | "sysv64-unwind"
                | "efiapi" | "system" | "system-unwind"
        );
        if is_c {
            // A naked C-variadic `extern "C"` function is allowed.
        } else if supported {
            if !v.feature("c_variadic_naked_functions") {
                // alloc: the error's message
                let message = format!("Naked c-variadic `extern \"{abi}\"` functions are unstable");
                v.emit(out, node, feature_err("c_variadic_naked_functions", message, sig_span));
            }
        } else {
            // alloc: the error's message
            let message = format!("`...` is not supported for `extern \"{abi}\"` naked functions");
            // clone: a span, on the error path
            let mut diag = Diag::error(None, message, variadic.clone());
            diag.help("C-variadic function must have a compatible calling convention");
            // alloc: the error's label
            diag.span_label(extern_span, format!("`extern \"{abi}\"` because of this"));
            v.emit(out, node, diag);
        }
    } else if !is_c {
        // alloc: the error's message
        let message = format!("`...` is not supported for `extern \"{abi}\"` functions");
        // clone: a span, on the error path
        let mut diag = Diag::error(None, message, variadic.clone());
        diag.help(ONLY_C);
        // alloc: the error's label
        diag.span_label(extern_span, format!("`extern \"{abi}\"` because of this"));
        v.emit(out, node, diag);
    }
}
