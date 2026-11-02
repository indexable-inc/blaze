//! The errors of the parsers of crate-level attributes (`#![crate_name]`, `#![crate_type]`,
//! the limits, `#![windows_subsystem]`, `#![feature]`, `#![register_tool]`, ..), of macro,
//! test, proc-macro and debugger attributes, `#[path]`, `#[must_use]`, `#[no_link]` and
//! `#[non_exhaustive]`; and of the attributes no parser takes (`validate`): a lint attribute or a
//! builtin attribute macro whose input is not of its form, a meta item that does not parse, a
//! key-value attribute whose value is not a literal, a value rustc reads before expansion makes
//! a literal of it.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/` `crate_level.rs`,
//! `macro_attrs.rs`, `test_attrs.rs`, `proc_macro_attrs.rs`, `debugger.rs`, `path.rs`,
//! `must_use.rs`, `no_link.rs` and `non_exhaustive.rs`, with the texts of
//! `compiler/rustc_attr_parsing/src/session_diagnostics.rs` and
//! `compiler/rustc_attr_parsing/src/diagnostics.rs` (`ToolReserved`);
//! `compiler/rustc_attr_parsing/src/validate_attr.rs` (`check_attr`, `parse_meta`,
//! `check_builtin_meta_item`, `emit_malformed_attribute`), `compiler/rustc_attr_parsing/src/parser.rs`
//! (`expr_to_lit`), `compiler/rustc_expand/src/expand.rs` (a `LegacyAttr`'s `parse_meta`),
//! `compiler/rustc_expand/src/module.rs` (`mod_file_path_from_attr`),
//! `compiler/rustc_interface/src/passes.rs` (the crate attributes read before expansion),
//! `compiler/rustc_builtin_macros/src/util.rs` (`check_builtin_macro_attribute`),
//! `compiler/rustc_builtin_macros/src/cfg_accessible.rs` (`validate_input`) and the texts of
//! `compiler/rustc_builtin_macros/src/diagnostics.rs` (`CfgAccessibleInvalid`),
//! `compiler/rustc_parse/src/parser/attr.rs` (`parse_meta_seq_top`, `parse_meta_item_inner`,
//! `parse_meta_item`, `parse_meta_item_kind`, `parse_unsuffixed_meta_item_lit`),
//! `compiler/rustc_parse/src/parser/mod.rs` (`parse_seq_to_before_tokens`, `token_descr`),
//! `compiler/rustc_parse/src/parser/diagnostics.rs` (`expected_one_of_not_found`),
//! `compiler/rustc_parse/src/diagnostics.rs` (`InvalidMetaItem`, `UseEqInstead`,
//! `MetaBadDelim`, `SuffixedLiteralInAttribute`) and `create_lit_error` in
//! `compiler/rustc_session/src/diagnostics.rs` (MIT or Apache-2.0).

use hir::HirFileId;
use smallvec::SmallVec;
use span::Edition;
use syntax::{
    NodeOrToken, SmolStr, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, AstToken as _, HasAttrs as _},
};

use super::{Accepted, Finalize, GroupId, limit, no_args};
use stdx::list::Join;
use crate::{
    checks::attrs::{
        accept::Accept,
        args::{self, ArgParser, MetaItemOrLit, NameValue},
        is, with_path,
        parsed::{Parsed, ParsedAttr, ProcMacroKind},
    },
    cx::Cx,
    lints::syntax::attrs::{
        Args, Attr, AttrKind, Target,
        table::{self, Template},
    },
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) const PATHS: &[&[&str]] = &[
    &["allow_internal_unsafe"],
    &["collapse_debuginfo"],
    &["crate_name"],
    &["crate_type"],
    &["debugger_visualizer"],
    &["default_lib_allocator"],
    &["feature"],
    &["ignore"],
    &["macro_escape"],
    &["macro_export"],
    &["macro_use"],
    &["move_size_limit"],
    &["must_use"],
    &["needs_panic_runtime"],
    &["no_builtins"],
    &["no_core"],
    &["no_link"],
    &["no_main"],
    &["no_std"],
    &["non_exhaustive"],
    &["panic_runtime"],
    &["path"],
    &["pattern_complexity_limit"],
    &["proc_macro"],
    &["proc_macro_attribute"],
    &["proc_macro_derive"],
    &["profiler_runtime"],
    &["recursion_limit"],
    &["reexport_test_harness_main"],
    &["register_attribute_tool"],
    &["register_lint_tool"],
    &["register_tool"],
    &["rustc_abi"],
    &["rustc_builtin_macro"],
    &["rustc_coherence_is_core"],
    &["rustc_delayed_bug_from_inside_query"],
    &["rustc_evaluate_where_clauses"],
    &["rustc_no_implicit_bounds"],
    &["rustc_preserve_ub_checks"],
    &["rustc_proc_macro_decls"],
    &["rustc_test_entrypoint_marker"],
    &["rustc_test_marker"],
    &["should_panic"],
    &["test_runner"],
    &["type_length_limit"],
    &["windows_subsystem"],
];

/// The state of the parsers of several attributes, of one node.
#[derive(Default)]
pub(crate) struct State {
    crate_type: bool,
    feature: bool,
    /// A tool was registered: rustc's `RegisterToolParser` gives nothing of lists that
    /// register none.
    register_tool: bool,
    debugger_visualizer: bool,
    macro_use: MacroUse,
}

/// rustc's `MacroUseParser`.
#[derive(Default)]
struct MacroUse {
    seen: bool,
    /// A `#[macro_use]` without arguments was seen: what lists after it name is linted.
    use_all: bool,
}

pub(crate) fn accept(state: &mut State, cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Accepted {
    let other = Parsed::Other;
    match cx.builtin.path {
        ["crate_name"] => {
            let range = cx.attr_range();
            Accepted::Single(name_value_str(cx, args, range, None).map(|_| other("CrateName")))
        }
        ["crate_type"] => {
            let range = cx.attr_range();
            let _ = early_fatal(cx, |cx| name_value_str(cx, args, range, None));
            state.crate_type = true;
            Accepted::Group("crate_type")
        }
        ["recursion_limit"] => {
            Accepted::Single(early_fatal(cx, |cx| limit_of(cx, args)).map(|_| other("RecursionLimit")))
        }
        ["move_size_limit"] => Accepted::Single(limit_of(cx, args).map(|_| other("MoveSizeLimit"))),
        ["type_length_limit"] => Accepted::Single(limit_of(cx, args).map(|_| other("TypeLengthLimit"))),
        ["pattern_complexity_limit"] => {
            Accepted::Single(limit_of(cx, args).map(|_| other("PatternComplexityLimit")))
        }
        ["no_core"] => no_args(cx, args, other("NoCore")),
        ["no_std"] => no_args(cx, args, other("NoStd")),
        ["no_main"] => no_args(cx, args, other("NoMain")),
        ["rustc_coherence_is_core"] => no_args(cx, args, other("RustcCoherenceIsCore")),
        ["panic_runtime"] => no_args(cx, args, other("PanicRuntime")),
        ["needs_panic_runtime"] => no_args(cx, args, other("NeedsPanicRuntime")),
        ["profiler_runtime"] => no_args(cx, args, other("ProfilerRuntime")),
        ["no_builtins"] => no_args(cx, args, other("NoBuiltins")),
        ["rustc_preserve_ub_checks"] => no_args(cx, args, other("RustcPreserveUbChecks")),
        ["rustc_no_implicit_bounds"] => no_args(cx, args, other("RustcNoImplicitBounds")),
        ["default_lib_allocator"] => no_args(cx, args, other("DefaultLibAllocator")),
        ["windows_subsystem"] => Accepted::Single(windows_subsystem(cx, args)),
        ["feature"] => {
            feature(cx, args);
            state.feature = true;
            Accepted::Group("feature")
        }
        ["register_tool" | "register_attribute_tool" | "register_lint_tool"] => {
            state.register_tool |= register_tool(cx, args);
            Accepted::Group("register_tool")
        }
        ["macro_escape"] => no_args(cx, args, other("MacroEscape")),
        ["macro_use"] => {
            macro_use(&mut state.macro_use, cx, args);
            Accepted::Group("macro_use")
        }
        ["allow_internal_unsafe"] => no_args(cx, args, other("AllowInternalUnsafe")),
        ["macro_export"] => Accepted::Single(macro_export(cx, args)),
        ["collapse_debuginfo"] => Accepted::Single(collapse_debuginfo(cx, args)),
        ["rustc_proc_macro_decls"] => no_args(cx, args, other("RustcProcMacroDecls")),
        ["ignore"] => Accepted::Single(ignore(args)),
        ["should_panic"] => Accepted::Single(should_panic(cx, args)),
        ["reexport_test_harness_main"] => {
            let range = args.range().unwrap_or(cx.inner_range());
            let name = name_value_str(cx, args, range, Some("reexport_test_harness_main"));
            Accepted::Single(name.map(|_| other("ReexportTestHarnessMain")))
        }
        ["rustc_abi"] => Accepted::Single(rustc_abi(cx, args)),
        ["rustc_delayed_bug_from_inside_query"] => no_args(cx, args, other("RustcDelayedBugFromInsideQuery")),
        ["rustc_evaluate_where_clauses"] => no_args(cx, args, other("RustcEvaluateWhereClauses")),
        ["rustc_test_entrypoint_marker"] => no_args(cx, args, other("RustcTestEntrypointMarker")),
        ["test_runner"] => Accepted::Single(test_runner(cx, args)),
        ["rustc_test_marker"] => Accepted::Single(rustc_test_marker(cx, args)),
        ["proc_macro"] => no_args(cx, args, Parsed::ProcMacro(ProcMacroKind::FunctionLike)),
        ["proc_macro_attribute"] => no_args(cx, args, Parsed::ProcMacro(ProcMacroKind::Attribute)),
        ["proc_macro_derive"] => {
            Accepted::Single(derive_like(cx, args, true).map(|()| Parsed::ProcMacro(ProcMacroKind::Derive)))
        }
        ["rustc_builtin_macro"] => Accepted::Single(derive_like(cx, args, false).map(|()| other("RustcBuiltinMacro"))),
        ["debugger_visualizer"] => {
            debugger_visualizer(cx, args);
            state.debugger_visualizer = true;
            Accepted::Group("debugger_visualizer")
        }
        ["path"] => Accepted::Single(path(cx, args)),
        ["must_use"] => Accepted::Single(must_use(cx, args)),
        ["no_link"] => no_args(cx, args, other("NoLink")),
        ["non_exhaustive"] => no_args(cx, args, Parsed::NonExhaustive),
        path => unreachable!("{path:?} is not a misc attribute"),
    }
}

pub(crate) fn finalize(state: &mut State, group: &'static str, _fx: &mut Finalize<'_, '_, '_>) -> Option<Parsed> {
    let (seen, parsed) = match group {
        "crate_type" => (state.crate_type, "CrateType"),
        "feature" => (state.feature, "Feature"),
        "register_tool" => (state.register_tool, "RegisterTool"),
        "macro_use" => (state.macro_use.seen, "MacroUse"),
        "debugger_visualizer" => (state.debugger_visualizer, "DebuggerVisualizer"),
        group => unreachable!("{group:?} is not a misc group"),
    };
    seen.then_some(Parsed::Other(parsed))
}

/// rustc's `AllowInternalUnsafeParser`'s check: on a function, only a proc macro may have it.
pub(crate) fn check(group: GroupId, first: usize, fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    if let GroupId::Single(["allow_internal_unsafe"]) = group
        && matches!(fx.target(), Target::Fn)
        && !parsed.iter().any(|it| matches!(it.parsed, Parsed::ProcMacro(_)))
    {
        let mut diag = Diag::error(None, "attribute should be applied to a macro", fx.attr_span(first));
        diag.span_label(fx.target_span(), "not a macro");
        fx.emit_err(diag);
    }
}

/// `expect_name_value`, then `expect_string_literal`.
fn name_value_str(cx: &mut Accept<'_, '_, '_>, args: &ArgParser, range: TextRange, name: Option<&str>) -> Option<String> {
    let nv = cx.expect_name_value(args, range, name)?;
    cx.expect_string_literal(nv)
}

/// `parse` of an attribute of the crate root rustc reads before expansion
/// (`collect_crate_types`, `get_recursion_limit`: `ShouldEmit::EarlyFatal`), its errors fatal
/// there; elsewhere the attribute is parsed as any other.
fn early_fatal<'a, 'c, 'db, T>(
    cx: &mut Accept<'a, 'c, 'db>,
    parse: impl FnOnce(&mut Accept<'a, 'c, 'db>) -> T,
) -> T {
    if cx.owner.target == Target::Crate && cx.attr.inner { cx.fatal(parse) } else { parse(cx) }
}

/// rustc's `RecursionLimitParser` and the other limits': `#![recursion_limit = "N"]`.
fn limit_of(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<u64> {
    let range = cx.attr_range();
    let nv = cx.expect_name_value(args, range, None)?;
    limit(cx, nv)
}

/// rustc's `WindowsSubsystemParser`.
fn windows_subsystem(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let range = cx.inner_range();
    let nv = cx.expect_name_value(args, range, Some("windows_subsystem"))?;
    match nv.value_as_str() {
        Some("console" | "windows") => Some(Parsed::Other("WindowsSubsystem")),
        _ => {
            cx.expected_specific_argument_strings(nv.value_range(), &["console", "windows"]);
            None
        }
    }
}

/// rustc's `FeatureParser`: each element a feature's name; an empty list is linted.
fn feature(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    let range = cx.attr_range();
    let Some(list) = cx.expect_list(args, range) else { return };
    for elem in list.mixed() {
        let Some(item) = elem.meta_item() else {
            cx.expected_identifier(elem.range());
            continue;
        };
        if cx.expect_no_args(&item.args).is_none() {
            continue;
        }
        if item.path.word().is_none() {
            cx.expected_identifier(item.path.range);
        }
    }
}

/// rustc's `RegisterToolParser`, of `register_tool`, `register_attribute_tool` and
/// `register_lint_tool`: whether a tool is registered.
fn register_tool(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> bool {
    let range = cx.attr_range();
    let Some(list) = cx.expect_list(args, range) else { return false };
    let mut registered = false;
    for elem in list.mixed() {
        let Some(item) = elem.meta_item() else {
            cx.expected_identifier(elem.range());
            continue;
        };
        if cx.expect_no_args(&item.args).is_none() {
            continue;
        }
        let Some(ident) = item.path.word() else {
            cx.expected_identifier(item.path.range);
            continue;
        };
        if ident.name == "rustc" {
            let diag = cx.err(None, "tool `rustc` is reserved and cannot be registered", ident.range);
            cx.emit_err(diag);
            continue;
        }
        registered = true;
    }
    registered
}

/// rustc's `MacroUseParser`: `#[macro_use]` or `#[macro_use(name1, name2, ..)]`; an empty
/// list, and a list after `#[macro_use]`, are linted.
fn macro_use(state: &mut MacroUse, cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    state.seen = true;
    match args {
        ArgParser::NoArgs => state.use_all = true,
        ArgParser::List(list) => {
            if list.is_empty() || state.use_all {
                return;
            }
            for item in list.mixed() {
                let Some(meta) = item.meta_item() else {
                    cx.expected_identifier(item.range());
                    continue;
                };
                if cx.expect_no_args(&meta.args).is_none() {
                    continue;
                }
                if meta.path.word().is_none() {
                    cx.expected_identifier(meta.range());
                }
            }
        }
        ArgParser::NameValue(nv) => cx.expected_list_or_no_args(nv.args_range()),
    }
}

/// rustc's `MacroExportParser`: a list other than `(local_inner_macros)` is linted.
fn macro_export(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    match args {
        ArgParser::NoArgs => Some(Parsed::MacroExport),
        ArgParser::List(list) => list
            .as_single()
            .and_then(MetaItemOrLit::meta_item_no_args)
            .is_some_and(|it| it.path.word_is("local_inner_macros"))
            .then_some(Parsed::MacroExport),
        ArgParser::NameValue(nv) => {
            cx.expected_list_or_no_args(nv.args_range());
            None
        }
    }
}

/// rustc's `CollapseDebugInfoParser`: `#[collapse_debuginfo(yes | no | external)]`.
fn collapse_debuginfo(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    const POSSIBLE: &[&str] = &["yes", "no", "external"];
    let range = cx.attr_range();
    let single = cx.expect_single_element_list(args, range)?;
    let Some(meta) = single.meta_item() else {
        cx.expected_not_literal(single.range());
        return None;
    };
    let _ = cx.expect_no_args(&meta.args);
    match meta.path.word_sym() {
        Some("yes" | "no" | "external") => Some(Parsed::Other("CollapseDebugInfo")),
        _ => {
            cx.expected_specific_argument(meta.range(), POSSIBLE);
            None
        }
    }
}

/// rustc's `IgnoreParser`: what is not `#[ignore]` or `#[ignore = "reason"]` is linted.
fn ignore(args: &ArgParser) -> Option<Parsed> {
    let valid = match args {
        ArgParser::NoArgs => true,
        ArgParser::NameValue(nv) => nv.value_as_str().is_some(),
        ArgParser::List(_) => false,
    };
    valid.then_some(Parsed::Other("Ignore"))
}

/// rustc's `ShouldPanicParser`: `#[should_panic]`, `#[should_panic = "reason"]` or
/// `#[should_panic(expected = "reason")]`.
fn should_panic(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    match args {
        ArgParser::NoArgs => {}
        ArgParser::NameValue(nv) => {
            let _ = cx.expect_string_literal(nv);
        }
        ArgParser::List(list) => {
            let single = cx.expect_single(list)?;
            let (ident, arg) = cx.expect_name_value_item(single, Some("expected"))?;
            if ident.name != "expected" {
                cx.expected_specific_argument_strings(list.range, &["expected"]);
                return None;
            }
            let _ = cx.expect_string_literal(arg);
        }
    }
    Some(Parsed::Other("ShouldPanic"))
}

/// rustc's `RustcAbiParser`: `#[rustc_abi(assert_eq | debug)]`.
fn rustc_abi(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    const POSSIBLE: &[&str] = &["assert_eq", "debug"];
    let Some(list) = args.as_list() else {
        let range = cx.attr_range();
        cx.expected_specific_argument_and_list(range, POSSIBLE);
        return None;
    };
    let arg = cx.expect_single(list)?;
    let Some(meta) = arg.meta_item_no_args() else {
        cx.expected_specific_argument(list.range, POSSIBLE);
        return None;
    };
    match meta.path.word_sym() {
        Some("assert_eq" | "debug") => Some(Parsed::Other("RustcAbi")),
        _ => {
            cx.expected_specific_argument(meta.range(), POSSIBLE);
            None
        }
    }
}

/// rustc's `TestRunnerParser`: `#![test_runner(path)]`.
fn test_runner(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let range = cx.attr_range();
    let single = cx.expect_single_element_list(args, range)?;
    if single.meta_item_no_args().is_none() {
        cx.expected_not_literal(single.range());
        return None;
    }
    Some(Parsed::Other("TestRunner"))
}

/// rustc's `RustcTestMarkerParser`: `#[rustc_test_marker = "name"]`, the name not blank.
fn rustc_test_marker(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let range = cx.attr_range();
    let nv = cx.expect_name_value(args, range, Some("rustc_test_marker"))?;
    let name = cx.expect_string_literal(nv)?;
    if name.trim().is_empty() {
        cx.expected_non_empty_string_literal(nv.value_range());
        return None;
    }
    Some(Parsed::Other("RustcTestMarker"))
}

/// rustc's `parse_derive_like`, of `#[proc_macro_derive(Trait, attributes(a, b))]` (the trait's
/// name mandatory) and `#[rustc_builtin_macro(Trait, attributes(..))]` (where it may be left
/// out); a helper attribute named as a builtin attribute is linted.
fn derive_like(cx: &mut Accept<'_, '_, '_>, args: &ArgParser, trait_name_mandatory: bool) -> Option<()> {
    let Some(list) = args.as_list() else {
        if args.as_no_args().is_ok() && !trait_name_mandatory {
            return Some(());
        }
        let range = cx.attr_range();
        cx.expected_list(range, args);
        return None;
    };
    let mut items = list.mixed();
    let Some(trait_attr) = items.next() else {
        cx.expected_at_least_one_argument(list.range);
        return None;
    };
    let Some(trait_attr) = trait_attr.meta_item() else {
        cx.expected_not_literal(trait_attr.range());
        return None;
    };
    let Some(trait_ident) = trait_attr.path.word() else {
        cx.expected_identifier(trait_attr.path.range);
        return None;
    };
    if !can_be_raw(&trait_ident.name) {
        cx.expected_identifier(trait_ident.range);
        return None;
    }
    cx.expect_no_args(&trait_attr.args)?;
    if let Some(attrs) = items.next() {
        let Some(attr_list) = attrs.meta_item() else {
            cx.expected_not_literal(attrs.range());
            return None;
        };
        if !attr_list.path.word_is("attributes") {
            cx.expected_specific_argument(attrs.range(), &["attributes"]);
            return None;
        }
        let attr_list = cx.expect_list(&attr_list.args, attrs.range())?;
        for attr in attr_list.mixed() {
            let Some(attr) = attr.meta_item() else {
                cx.expected_identifier(attr.range());
                return None;
            };
            cx.expect_no_args(&attr.args)?;
            let Some(ident) = attr.path.word() else {
                cx.expected_identifier(attr.path.range);
                return None;
            };
            if !can_be_raw(&ident.name) {
                cx.expected_identifier(ident.range);
                return None;
            }
        }
    }
    if let Some(next) = items.next() {
        cx.expected_no_args(next.range());
    }
    Some(())
}

/// rustc's `Symbol::can_be_raw`: an identifier `r#` can be written before.
fn can_be_raw(name: &str) -> bool {
    !matches!(name, "" | "_" | "self" | "super" | "crate" | "Self" | "{{root}}" | "$crate")
}

/// rustc's `DebuggerVisualizerParser`: `#[debugger_visualizer(natvis_file = "..")]` or
/// `gdb_script_file`. Reading the file is `check_mod_attrs`'.
fn debugger_visualizer(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    let range = cx.attr_range();
    let Some(single) = cx.expect_single_element_list(args, range) else { return };
    let Some((ident, value)) = cx.expect_name_value_item(single, None) else { return };
    if !matches!(ident.name.as_str(), "natvis_file" | "gdb_script_file") {
        cx.expected_specific_argument(ident.range, &["natvis_file", "gdb_script_file"]);
        return;
    }
    let _ = cx.expect_string_literal(value);
}

/// rustc's `PathParser`: `#[path = "file"]`. On a module, expansion reads the first one before
/// any parser: what is not a string there is its fatal error (`mod_file_path_from_attr`).
fn path(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    if matches!(cx.target(), Target::Mod)
        && args.as_name_value().and_then(NameValue::value_as_str).is_none()
        && cx.owner.attrs.iter().position(|it| is(it, "path")) == Some(cx.index)
    {
        let mut diag = malformed(cx.cx, cx.attr, "path", &PATH, cx.attr.inner);
        diag.stop();
        cx.emit_err(diag);
        return None;
    }
    let range = cx.attr_range();
    name_value_str(cx, args, range, None).map(|_| Parsed::Other("Path"))
}

/// rustc's `MustUseParser`: `#[must_use]` or `#[must_use = "reason"]`.
fn must_use(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    match args {
        ArgParser::NoArgs => {}
        ArgParser::NameValue(nv) => {
            let _ = cx.expect_string_literal(nv);
        }
        ArgParser::List(list) => {
            cx.expected_nv_or_no_args(list.range);
            return None;
        }
    }
    Some(Parsed::Other("MustUse"))
}

/// The lint attributes, the builtin attributes `check_attr` still reads as meta items.
const LINT_ATTRS: &[&str] = &["allow", "deny", "expect", "forbid", "warn"];

const LINT: Template = Template {
    word: false,
    list: Some(&["lint1", "lint1, lint2, ...", r#"lint1, lint2, lint3, reason = "...""#]),
    one_of: &[],
    name_value_str: None,
    docs: Some("https://doc.rust-lang.org/reference/attributes/diagnostics.html#lint-check-attributes"),
};

/// `check_builtin_macro_attribute`'s: the builtin attribute macros it checks are words.
const WORD: Template = Template { word: true, list: None, one_of: &[], name_value_str: None, docs: None };

const CFG_ACCESSIBLE: Template = Template { word: false, list: Some(&["path"]), one_of: &[], name_value_str: None, docs: None };

/// `mod_file_path_from_attr`'s.
const PATH: Template = Template {
    word: false,
    list: None,
    one_of: &[],
    name_value_str: Some(&["file"]),
    docs: Some("https://doc.rust-lang.org/reference/items/modules.html#the-path-attribute"),
};

/// A builtin attribute macro (`LegacyAttr`): expansion reads it as a meta item
/// (`parse_meta`), then the macro checks it against its template; `Some(None)` for one whose
/// wrong forms are linted (`test`, `bench`) or that reads its input itself (`define_opaque`).
/// `derive` is `lower::malformed_derive`'s.
fn attribute_macro(name: &str) -> Option<Option<Template>> {
    Some(match name {
        "alloc_error_handler" | "cfg_eval" | "global_allocator" | "test_case" => Some(WORD),
        "cfg_accessible" => Some(CFG_ACCESSIBLE),
        "bench" | "define_opaque" | "test" => None,
        _ => return None,
    })
}

/// expansion's and AST validation's `check_attr` of `attr`, and the errors of reading its
/// value where rustc reads it before expansion makes a literal of a macro call there.
pub(crate) fn validate<'db>(cx: &Cx<'_, 'db>, attr: &Attr<'db>, errors: &mut Vec<Diag>) {
    let AttrKind::Meta { written, .. } = attr.kind else { return };
    with_path(attr, |path| validate_path(cx, attr, written, path, errors));
}

/// [`validate`] of `attr`, written in `written`, whose path is `path`.
fn validate_path<'db>(
    cx: &Cx<'_, 'db>,
    attr: &Attr<'db>,
    written: ast::Attr<'db>,
    path: &[&str],
    errors: &mut Vec<Diag>,
) {
    let name = match path {
        [one] => Some(*one),
        _ => None,
    };
    let args = attr.args();
    if let Args::NameValue(Some(ast::Expr::MacroExpr(call))) = args
        && let Some(diag) = unexpanded_value(cx, attr, written, name, call)
    {
        errors.push(diag);
        return;
    }
    if table::builtin(path).is_some() {
        return;
    }
    let edition = attr.file.edition(cx.db);
    if let Some(name) = name
        && let Some(template) = attribute_macro(name)
    {
        let Some(meta) = parse_meta(cx, attr, args, edition, errors) else { return };
        if let Some(template) = template
            && !compatible(&template, &meta)
        {
            errors.push(malformed(cx, attr, name, &template, false));
        }
        if name == "cfg_accessible" {
            cfg_accessible(cx, attr, &meta, errors);
        }
        return;
    }
    if let Some(name) = name
        && table::is_builtin_attr_name(name)
    {
        // `cfg` and `cfg_attr` are gone by now; the rest of the unparsed ones are lint attributes.
        if LINT_ATTRS.contains(&name)
            && let Some(meta) = parse_meta(cx, attr, args, edition, errors)
            && !compatible(&LINT, &meta)
        {
            errors.push(malformed(cx, attr, name, &LINT, attr.inner));
        }
        return;
    }
    if matches!(args, Args::NameValue(_)) {
        let _ = parse_meta(cx, attr, args, edition, errors);
    }
}

/// A macro call as the value of an attribute rustc reads before the call is expanded: one of a
/// macro call (it expands first), one of the crate attributes read before expansion, the first
/// `#[path]` of a module.
fn unexpanded_value(
    cx: &Cx<'_, '_>,
    attr: &Attr<'_>,
    written: ast::Attr<'_>,
    name: Option<&str>,
    call: ast::MacroExpr<'_>,
) -> Option<Diag> {
    let parent = written.syntax().parent()?;
    let literal = || {
        let span = cx.span(attr.in_file(call.syntax().text_range()));
        Diag::error(None, "attribute value must be a literal", span)
    };
    if parent.kind() == SyntaxKind::MACRO_CALL {
        return Some(literal());
    }
    match name? {
        "crate_name" | "crate_type" | "recursion_limit"
            if attr.inner && parent.kind() == SyntaxKind::SOURCE_FILE && in_root_file(cx, attr) =>
        {
            let mut diag = literal();
            if name != Some("crate_name") {
                diag.stop();
            }
            Some(diag)
        }
        "path" if parent.kind() == SyntaxKind::MODULE && first_path(parent, written) => {
            let mut diag = malformed(cx, attr, "path", &PATH, attr.inner);
            diag.stop();
            Some(diag)
        }
        _ => None,
    }
}

fn in_root_file(cx: &Cx<'_, '_>, attr: &Attr<'_>) -> bool {
    cx.krate.root_module(cx.db).as_source_file_id(cx.db).is_some_and(|root| attr.file == HirFileId::from(root))
}

/// Whether `written` is the first `#[path]` of the item `item`.
fn first_path(item: ast::Node<'_>, written: ast::Attr<'_>) -> bool {
    let Some(item) = ast::AnyHasAttrs::cast(item) else { return false };
    item.attrs().find(|it| it.simple_name() == Some("path")).is_some_and(|it| it.syntax() == written.syntax())
}

/// `cfg_accessible`'s `validate_input`.
fn cfg_accessible(cx: &Cx<'_, '_>, attr: &Attr<'_>, meta: &Meta, errors: &mut Vec<Diag>) {
    let Meta::List(items) = meta else { return };
    let (message, range) = match items.as_slice() {
        [] => ("`cfg_accessible` path is not specified", attr.range()),
        [_, .., last] => ("multiple `cfg_accessible` paths are specified", last.range),
        [item] if item.lit => ("`cfg_accessible` path cannot be a literal", item.range),
        [item] if !item.is_word => ("`cfg_accessible` path cannot accept arguments", item.range),
        [_] => return,
    };
    errors.push(Diag::error(None, message, cx.span(attr.in_file(range))));
}

/// What `parse_meta` makes of an attribute: rustc's `MetaItemKind`, of what the templates read.
#[derive(Debug, PartialEq, Eq)]
enum Meta {
    Word,
    List(Vec<Item>),
    /// `= lit`: whether the literal is a string.
    NameValue(bool),
}

/// An element of a list: rustc's `MetaItemInner`.
#[derive(Debug, PartialEq, Eq)]
struct Item {
    range: TextRange,
    lit: bool,
    /// A path with no arguments (`MetaItemKind::Word`).
    is_word: bool,
    /// For a word of one segment that is a list's only element, its name.
    word: Option<String>,
}

/// rustc's `is_attr_template_compatible`.
fn compatible(template: &Template, meta: &Meta) -> bool {
    match meta {
        Meta::Word => template.word,
        Meta::List(items) => {
            template.list.is_some()
                || matches!(items.as_slice(), [Item { word: Some(word), .. }] if template.one_of.contains(&word.as_str()))
        }
        Meta::NameValue(true) => template.name_value_str.is_some(),
        Meta::NameValue(false) => false,
    }
}

/// rustc's `emit_malformed_attribute` of an error: "malformed `name` attribute input", with
/// the forms of `template` (none when there are more than 3).
fn malformed(cx: &Cx<'_, '_>, attr: &Attr<'_>, name: &str, template: &Template, inner: bool) -> Diag {
    let span = cx.span(attr.in_file(attr.range()));
    let bang = if inner { "!" } else { "" };
    let mut suggestions = Vec::with_capacity(4);
    if template.word {
        // alloc: the error's suggestion
        suggestions.push(format!("#{bang}[{name}]"));
    }
    for descr in template.list.into_iter().flatten() {
        // alloc: the error's suggestion
        suggestions.push(format!("#{bang}[{name}({descr})]"));
    }
    for word in template.one_of {
        // alloc: the error's suggestion
        suggestions.push(format!("#{bang}[{name}({word})]"));
    }
    for descr in template.name_value_str.into_iter().flatten() {
        // alloc: the error's suggestion
        suggestions.push(format!("#{bang}[{name} = \"{descr}\"]"));
    }
    if suggestions.len() > 3 {
        suggestions.clear();
    }
    suggestions.sort();
    // alloc: the error's message; clone: its span, used again for the suggestions
    let mut diag = Diag::error(None, format!("malformed `{name}` attribute input"), span.clone());
    if !suggestions.is_empty() {
        let message = if suggestions.len() == 1 { "must be of the form" } else { "the following are the possible correct uses" };
        diag.suggestions(span, message, suggestions, Applicability::HasPlaceholders, SuggestionStyle::ShowAlways);
    }
    if let Some(link) = template.docs {
        // alloc: the error's note
        diag.note(format!("for more information, visit <{link}>"));
    }
    diag
}

/// `validate_attr::parse_meta`: the attribute as a meta item, with the errors of reading it;
/// `None` where it is not one.
fn parse_meta<'db>(
    cx: &Cx<'_, 'db>,
    attr: &Attr<'db>,
    args: Args<'db>,
    edition: Edition,
    errors: &mut Vec<Diag>,
) -> Option<Meta> {
    match args {
        Args::Word => Some(Meta::Word),
        Args::List(tt) => {
            if tt.l_paren_token().is_none() {
                bad_delim(cx, attr, tt, errors);
            }
            let tokens = flatten(tt);
            let mut parser = MetaParser { cx, attr, edition, tokens: &tokens, at: 0, expected: SmallVec::new(), errors };
            parser.seq_top().map(Meta::List)
        }
        Args::NameValue(None) | Args::NameValue(Some(ast::Expr::MacroExpr(_))) => None,
        Args::NameValue(Some(ast::Expr::Literal(literal))) => {
            let token = literal.token();
            let span = cx.span(attr.in_file(token.text_range()));
            match check_lit(cx, attr, token, errors) {
                Lit::Suffixed => {
                    errors.push(suffixed(span));
                    None
                }
                lit => Some(Meta::NameValue(lit == Lit::Str)),
            }
        }
        Args::NameValue(Some(expr)) => {
            let span = cx.span(attr.in_file(expr.syntax().text_range()));
            errors.push(Diag::error(None, "attribute value must be a literal", span));
            None
        }
    }
}

/// `MetaBadDelim`: `#[path[..]]` or `#[path{..}]`.
fn bad_delim(cx: &Cx<'_, '_>, attr: &Attr<'_>, tt: ast::TokenTree<'_>, errors: &mut Vec<Diag>) {
    let (Some(open), Some(close)) = (tt.syntax().first_child_or_token(), tt.syntax().last_child_or_token()) else {
        return;
    };
    let span = |range| cx.span(attr.in_file(range));
    let mut diag = Diag::error(None, "wrong meta list delimiters", span(tt.syntax().text_range()));
    diag.multipart_suggestion(
        "the delimiters should be `(` and `)`",
        // alloc: the error's suggestion
        vec![(span(open.text_range()), "(".to_owned()), (span(close.text_range()), ")".to_owned())],
        Applicability::MachineApplicable,
    );
    errors.push(diag);
}

/// `SuffixedLiteralInAttribute`.
fn suffixed(span: Span) -> Diag {
    let mut diag = Diag::error(None, "suffixed literals are not allowed in attributes", span);
    diag.help("instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)");
    diag
}

/// A token as rustc's lexer makes it: punctuation written together is one token (`::`, `==`,
/// `->`), where the token tree has one per character.
#[derive(Debug)]
struct Tok<'t> {
    /// The first of the tokens it is made of.
    token: ast::Token<'t>,
    kind: SyntaxKind,
    /// Inline: a token's text, or punctuation glued of a few.
    text: SmolStr,
    range: TextRange,
    /// Whether a line ends between the token before it and its end.
    newline_before: bool,
}

/// The punctuation of more than one character rustc's lexer makes one token of; each one's
/// prefixes are ones too.
const GLUED: &[&str] = &[
    "!=", "%=", "&&", "&=", "*=", "+=", "-=", "->", "..", "...", "..=", "/=", "::", "<-", "<<", "<<=", "<=", "==", "=>",
    ">=", ">>", ">>=", "^=", "|=", "||",
];

/// The tokens between a token tree's delimiters, nested trees opened, trivia left out.
fn flatten(tt: ast::TokenTree<'_>) -> SmallVec<[Tok<'_>; 16]> {
    let mut elements = tt.syntax().children_with_tokens().peekable();
    if elements.peek().is_some_and(|it| it.kind().is_l_delimiter()) {
        elements.next();
    }
    let mut tokens: SmallVec<[Tok<'_>; 16]> = SmallVec::new();
    let mut newline = false;
    while let Some(element) = elements.next() {
        if elements.peek().is_none() && element.kind().is_r_delimiter() {
            break;
        }
        push_tokens(element, &mut newline, &mut tokens);
    }
    tokens
}

fn push_tokens<'t>(element: ast::Element<'t>, newline: &mut bool, tokens: &mut SmallVec<[Tok<'t>; 16]>) {
    match element {
        NodeOrToken::Node(node) => {
            for child in node.children_with_tokens() {
                push_tokens(child, newline, tokens);
            }
        }
        NodeOrToken::Token(token) => {
            let kind = token.kind();
            let text = token.text();
            if kind.is_trivia() {
                *newline |= text.contains('\n');
                return;
            }
            let range = token.text_range();
            let newline_before = std::mem::take(newline) || text.contains('\n');
            if is_glue(kind)
                && let Some(last) = tokens.last_mut()
                && !newline_before
                && is_glue(last.kind)
                && last.range.end() == range.start()
                && is_glued(&last.text, text)
            {
                // alloc: none: glued punctuation is at most three bytes, kept inline
                last.text = [last.text.as_str(), text].into_iter().collect();
                last.range = TextRange::new(last.range.start(), range.end());
                last.kind = match last.text.as_str() {
                    "::" => SyntaxKind::COLON2,
                    "==" => SyntaxKind::EQ2,
                    _ => last.kind,
                };
                return;
            }
            tokens.push(Tok { kind, text: SmolStr::new(text), range, newline_before, token });
        }
    }
}

fn is_glue(kind: SyntaxKind) -> bool {
    kind.is_punct() && !kind.is_l_delimiter() && !kind.is_r_delimiter()
}

fn is_glued(first: &str, then: &str) -> bool {
    let len = first.len() + then.len();
    GLUED.iter().any(|it| it.len() == len && it.starts_with(first) && it.ends_with(then))
}

/// rustc's parser of the tokens of a meta list (`parse_in` of `parse_meta_seq_top`), with the
/// tokens it expected since it last took one (`expected_token_types`).
struct MetaParser<'p, 'c, 'db> {
    cx: &'p Cx<'c, 'db>,
    attr: &'p Attr<'db>,
    edition: Edition,
    tokens: &'p [Tok<'db>],
    at: usize,
    /// Rarely more than four between two tokens taken.
    expected: SmallVec<[&'static str; 8]>,
    errors: &'p mut Vec<Diag>,
}

impl<'p, 'db> MetaParser<'p, '_, 'db> {
    fn span(&self, range: TextRange) -> Span {
        self.cx.span(self.attr.in_file(range))
    }

    fn peek(&self) -> Option<&'p Tok<'db>> {
        self.tokens.get(self.at)
    }

    fn prev(&self) -> Option<&'p Tok<'db>> {
        self.at.checked_sub(1).and_then(|it| self.tokens.get(it))
    }

    fn at_kind(&self, kind: SyntaxKind) -> bool {
        self.peek().is_some_and(|it| it.kind == kind)
    }

    fn bump(&mut self) {
        self.at += 1;
        self.expected.clear();
    }

    /// rustc's `check`: whether the token is `kind`, `descr` expected.
    fn check(&mut self, kind: SyntaxKind, descr: &'static str) -> bool {
        self.expected.push(descr);
        self.at_kind(kind)
    }

    fn eat(&mut self, kind: SyntaxKind, descr: &'static str) -> bool {
        let at = self.check(kind, descr);
        if at {
            self.bump();
        }
        at
    }

    /// The range of the token, or at the end, of the last one (rustc's span of `Eof`).
    fn token_range(&self) -> TextRange {
        match self.peek().or_else(|| self.tokens.last()) {
            Some(it) => it.range,
            None => self.attr.range(),
        }
    }

    fn prev_end(&self) -> TextSize {
        self.prev().map_or_else(|| self.token_range().start(), |it| it.range.end())
    }

    /// `parse_meta_seq_top`: the elements, else the error is said.
    fn seq_top(&mut self) -> Option<Vec<Item>> {
        let mut items = Vec::with_capacity(1);
        while self.peek().is_some() {
            match self.inner() {
                Ok(item) => items.push(item),
                Err(diag) => {
                    self.errors.push(diag);
                    return None;
                }
            }
            if !self.eat(SyntaxKind::COMMA, "`,`") {
                break;
            }
        }
        if self.peek().is_some() {
            let diag = self.expected_one_of();
            self.errors.push(diag);
            return None;
        }
        // A word of one segment is the one token its range covers, the list's first.
        if let [item] = items.as_mut_slice()
            && item.is_word
            && let Some(tok) = self.tokens.first().filter(|it| it.range == item.range)
        {
            // alloc: the name of a list's only word, which the templates compare
            item.word = Some(tok.text.to_string());
        }
        Some(items)
    }

    /// `parse_meta_item_inner`: a literal, else a meta item, else `InvalidMetaItem`.
    fn inner(&mut self) -> Result<Item, Diag> {
        if let Some(tok) = self.peek()
            && is_lit(tok.kind)
        {
            self.take_lit();
            return Ok(Item { range: tok.range, lit: true, is_word: false, word: None });
        }
        if let Some(item) = self.item() {
            return Ok(item);
        }
        Err(self.invalid_meta_item())
    }

    /// `parse_unsuffixed_meta_item_lit` at a literal: an error of it, or of its suffix, is said
    /// and the literal is taken.
    fn take_lit(&mut self) {
        let Some(tok) = self.peek() else { return };
        self.bump();
        if check_lit(self.cx, self.attr, tok.token, self.errors) == Lit::Suffixed {
            let diag = suffixed(self.span(tok.range));
            self.errors.push(diag);
        }
    }

    /// `parse_meta_item`: a path (`PathStyle::Mod`), then `= lit`, `(..)` or nothing; `None`
    /// where it is not one (rustc cancels that error for `InvalidMetaItem`).
    fn item(&mut self) -> Option<Item> {
        let start = self.peek()?.range.start();
        self.eat(SyntaxKind::COLON2, "`::`");
        loop {
            if !self.peek().is_some_and(|it| is_segment(it, self.edition)) {
                return None;
            }
            self.bump();
            if !self.eat(SyntaxKind::COLON2, "`::`") {
                break;
            }
        }
        let mut is_word = false;
        if self.eat(SyntaxKind::EQ, "`=`") {
            if !self.peek().is_some_and(|it| is_lit(it.kind)) {
                return None;
            }
            self.take_lit();
        } else if self.check(SyntaxKind::L_PAREN, "`(`") {
            self.paren_seq()?;
        } else {
            is_word = true;
        }
        Some(Item { range: TextRange::new(start, self.prev_end()), lit: false, is_word, word: None })
    }

    /// `parse_paren_comma_seq` of `parse_meta_item_inner`, at its `(`: rustc's
    /// `parse_seq_to_before_tokens`, which goes on past a missing `,` when an element follows.
    fn paren_seq(&mut self) -> Option<()> {
        self.bump();
        let mut first = true;
        loop {
            if self.eat(SyntaxKind::R_PAREN, "`)`") {
                return Some(());
            }
            if self.peek().is_none_or(|it| it.kind.is_r_delimiter()) {
                return Some(());
            }
            if first {
                first = false;
            } else if !self.eat(SyntaxKind::COMMA, "`,`") {
                let at = TextRange::empty(self.prev_end());
                let mut diag = self.expected_one_of();
                if self.peek().is_some_and(|it| matches!(it.text.as_str(), "." | "<" | ";")) {
                    self.bump();
                }
                if self.inner().is_err() {
                    self.errors.push(diag);
                    return None;
                }
                diag.suggestion_with_style(
                    // alloc: the error's suggestion
                    vec![(self.span(at), ",".to_owned())],
                    "missing `,`",
                    Applicability::MaybeIncorrect,
                    SuggestionStyle::HideCodeInline,
                );
                self.errors.push(diag);
                continue;
            }
            if self.eat(SyntaxKind::R_PAREN, "`)`") {
                return Some(());
            }
            if let Err(diag) = self.inner() {
                self.errors.push(diag);
                return None;
            }
        }
    }

    /// `InvalidMetaItem`: "expected unsuffixed literal, found ..", with quotes suggested around
    /// identifiers after a `=`.
    fn invalid_meta_item(&mut self) -> Diag {
        let range = self.token_range();
        // alloc: the error's message
        let descr = self.peek().map_or_else(|| "`<eof>`".to_owned(), |it| descr(it, self.edition));
        // alloc: the error's message
        let mut diag = Diag::error(None, format!("expected unsuffixed literal, found {descr}"), self.span(range));
        if self.prev().is_some_and(|it| it.kind == SyntaxKind::EQ) && self.peek().is_some_and(is_ident) {
            let before = TextRange::empty(range.start());
            while self.peek().is_some_and(is_ident) {
                self.bump();
            }
            let after = TextRange::empty(self.prev_end());
            diag.multipart_suggestion(
                "surround the identifier with quotation marks to make it into a string literal",
                // alloc: the error's suggestion
                vec![(self.span(before), "\"".to_owned()), (self.span(after), "\"".to_owned())],
                Applicability::MachineApplicable,
            );
        }
        diag
    }

    /// `expected_one_of_not_found` at a token: "expected one of `(`, `,`, `::`, or `=`, found
    /// `-`".
    fn expected_one_of(&self) -> Diag {
        let Some(found) = self.peek() else { return self.invalid_eof() };
        // clone: on the error path, the expected tokens sorted for the message
        let mut expected = self.expected.clone();
        expected.sort_unstable();
        expected.dedup();
        let prev = self.prev();
        let found_span = self.span(found.range);
        if found.kind == SyntaxKind::EQ2 && prev.is_some_and(is_ident) && expected.contains(&"`=`") {
            // clone: the error's span, used again for its suggestion
            let mut diag = Diag::error(None, "unexpected `==`", found_span.clone());
            diag.suggestion_verbose(found_span, "try using `=` instead", "=", Applicability::MachineApplicable);
            return diag;
        }
        let list = stdx::list::list(expected.iter(), Join::Or, false);
        let actual = descr(found, self.edition);
        let (message, label) = match expected.len() {
            // alloc: the error's message and label
            0 => (format!("unexpected token: {actual}"), "unexpected token after this".to_owned()),
            // alloc: the error's message and label
            1 => (format!("expected {list}, found {actual}"), format!("expected {list}")),
            n => {
                // alloc: the error's label; clone: the list, also in the message
                let short = if n > 6 { format!("{n} possible tokens") } else { list.clone() };
                // alloc: the error's message and label
                (format!("expected one of {list}, found {actual}"), format!("expected one of {short}"))
            }
        };
        // clone: the error's span, used again for its labels
        let mut diag = Diag::error(None, message, found_span.clone());
        if let Some(prev) = prev.filter(|it| is_ident(it)) {
            if matches!(prev.text.as_str(), "def" | "fun" | "func" | "function") {
                diag.suggestion_with_style(
                    // alloc: the error's suggestion
                    vec![(self.span(prev.range), "fn".to_owned())],
                    // alloc: the error's suggestion
                    format!("write `fn` instead of `{}` to declare a function", prev.text),
                    Applicability::MachineApplicable,
                    SuggestionStyle::HideCodeInline,
                );
            }
            if is_ident(found) && !prev.text.starts_with("r#") && !found.text.starts_with("r#") {
                // alloc: on the error path, the keyword the two identifiers might spell
                let concat = format!("{}{}", prev.text, found.text);
                if SyntaxKind::from_keyword(&concat, self.edition).is_some() || is_reserved_keyword(&concat, self.edition) {
                    diag.suggestion_verbose(
                        self.span(TextRange::new(prev.range.start(), found.range.end())),
                        // alloc: the error's suggestion
                        format!("consider removing the space to spell keyword `{concat}`"),
                        concat,
                        Applicability::MachineApplicable,
                    );
                }
            }
        }
        match prev {
            Some(prev) if found.newline_before => {
                let at = if expected.is_empty() { prev.range } else { TextRange::empty(prev.range.end()) };
                diag.span_label(self.span(at), label);
                diag.span_label(found_span, "unexpected token");
            }
            _ => {
                diag.span_label(found_span, label);
            }
        }
        diag
    }

    /// `InvalidMetaItem` at the end: what `expected_one_of` gives where no token is left.
    fn invalid_eof(&self) -> Diag {
        let range = self.token_range();
        Diag::error(None, "expected unsuffixed literal, found `<eof>`", self.span(range))
    }
}

/// rustc's `token_descr`: "`-`", "keyword `fn`", "reserved identifier `_`".
fn descr(tok: &Tok<'_>, edition: Edition) -> String {
    let text = &tok.text;
    if tok.kind == SyntaxKind::UNDERSCORE {
        // alloc: an error's message
        return format!("reserved identifier `{text}`");
    }
    if is_reserved_keyword(text, edition) {
        // alloc: an error's message
        return format!("reserved keyword `{text}`");
    }
    if SyntaxKind::from_keyword(text, edition).is_some_and(|it| it.is_strict_keyword(edition)) {
        // alloc: an error's message
        return format!("keyword `{text}`");
    }
    // alloc: an error's message
    format!("`{text}`")
}

/// rustc's `token::Ident`: an identifier or a keyword, `_`, `true` and `false` among them.
fn is_ident(tok: &Tok<'_>) -> bool {
    tok.kind.is_any_identifier() || tok.kind == SyntaxKind::UNDERSCORE
}

/// A segment of a path rustc's `parse_path_segment_ident` takes: an identifier that is not a
/// keyword of the edition, or `self`, `super`, `crate`, `Self`.
fn is_segment(tok: &Tok<'_>, edition: Edition) -> bool {
    matches!(
        tok.kind,
        SyntaxKind::IDENT | SyntaxKind::SELF_KW | SyntaxKind::SUPER_KW | SyntaxKind::CRATE_KW | SyntaxKind::SELF_TYPE_KW
    ) || (tok.kind.is_any_identifier() && SyntaxKind::from_keyword(&tok.text, edition).is_none())
}

/// rustc's `eat_token_lit`: a literal token, `true` or `false`.
fn is_lit(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
}

/// The keywords rustc reserves and does not use (`TokenDescription::ReservedKeyword`).
fn is_reserved_keyword(text: &str, edition: Edition) -> bool {
    matches!(
        text,
        "abstract" | "become" | "box" | "do" | "final" | "macro" | "override" | "priv" | "typeof" | "unsized" | "virtual" | "yield"
    ) || (edition >= Edition::Edition2018 && text == "try")
        || (edition >= Edition::Edition2024 && text == "gen")
}

/// What `MetaItemLit::from_token_lit` makes of a literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lit {
    Str,
    Other,
    /// A number with a type suffix: valid, but not in an attribute.
    Suffixed,
    /// rustc cannot make a value of it (`LitKind::Err`); the error is said.
    Err,
}

/// `MetaItemLit::from_token_lit` of a literal token, with `report_lit_error`'s errors
/// (`create_lit_error`).
fn check_lit(cx: &Cx<'_, '_>, attr: &Attr<'_>, token: ast::Token<'_>, errors: &mut Vec<Diag>) -> Lit {
    let span = cx.span(attr.in_file(token.text_range()));
    let kind = token.kind();
    let text = token.text();
    match kind {
        SyntaxKind::INT_NUMBER => {
            let Some(int) = ast::IntNumber::cast(token) else { return Lit::Other };
            let suffix = int.suffix();
            let symbol = &text[..text.len() - suffix.map_or(0, str::len)];
            match suffix {
                None | Some("i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64" | "u128" | "usize") => {
                    if int.value().is_err() {
                        errors.push(int_too_large(span, int.radix() as u32));
                        return Lit::Err;
                    }
                    if suffix.is_some() { Lit::Suffixed } else { Lit::Other }
                }
                Some("f16" | "f32" | "f64" | "f128") if int.radix() as u32 == 10 => Lit::Suffixed,
                Some(other) => {
                    errors.push(invalid_int_suffix(span, symbol, other));
                    Lit::Err
                }
            }
        }
        SyntaxKind::FLOAT_NUMBER => {
            match ast::FloatNumber::cast(token).and_then(ast::FloatNumber::suffix) {
                None => Lit::Other,
                Some("f16" | "f32" | "f64" | "f128") => Lit::Suffixed,
                Some(other) => {
                    errors.push(invalid_float_suffix(span, other));
                    Lit::Err
                }
            }
        }
        SyntaxKind::STRING | SyntaxKind::BYTE_STRING | SyntaxKind::C_STRING | SyntaxKind::CHAR | SyntaxKind::BYTE => {
            let Some(suffix) = args::suffix(text) else {
                return if kind == SyntaxKind::STRING { Lit::Str } else { Lit::Other };
            };
            let what = match kind {
                SyntaxKind::STRING => "string",
                SyntaxKind::BYTE_STRING => "byte string",
                SyntaxKind::C_STRING => "C string",
                SyntaxKind::CHAR => "char",
                _ => "byte",
            };
            // alloc: the error's message; clone: its span, used again for its label
            let mut diag = Diag::error(None, format!("suffixes on {what} literals are invalid"), span.clone());
            // alloc: the error's label
            diag.span_label(span, format!("invalid suffix `{suffix}`"));
            errors.push(diag);
            Lit::Err
        }
        _ => Lit::Other,
    }
}

/// `IntLiteralTooLarge`.
fn int_too_large(span: Span, radix: u32) -> Diag {
    let max = u128::MAX;
    let limit = match radix {
        // alloc: the error's note
        2 => format!("{max:#b}"),
        // alloc: the error's note
        8 => format!("{max:#o}"),
        // alloc: the error's note
        16 => format!("{max:#x}"),
        // alloc: the error's note
        _ => format!("{max}"),
    };
    let mut diag = Diag::error(None, "integer literal is too large", span);
    // alloc: the error's note
    diag.note(format!("value exceeds limit of `{limit}`"));
    diag
}

/// `InvalidIntLiteralWidth`, `InvalidNumLiteralBasePrefix` or `InvalidNumLiteralSuffix`.
fn invalid_int_suffix(span: Span, symbol: &str, suffix: &str) -> Diag {
    let looks_like_width =
        suffix.len() > 1 && suffix.starts_with(['i', 'u']) && suffix[1..].chars().all(|it| it.is_ascii_digit());
    if looks_like_width {
        // alloc: the error's message
        let mut diag = Diag::error(None, format!("invalid width `{}` for integer literal", &suffix[1..]), span);
        diag.help("valid widths are 8, 16, 32, 64 and 128");
        return diag;
    }
    if let Some(fixed) = fix_base_capitalisation(symbol, suffix) {
        // clone: the error's span, used again for its suggestion
        let mut diag = Diag::error(None, "invalid base prefix for number literal", span.clone());
        diag.note("base prefixes (`0xff`, `0b1010`, `0o755`) are lowercase");
        diag.suggestion(span, "try making the prefix lowercase", fixed, Applicability::MaybeIncorrect);
        return diag;
    }
    // alloc: the error's message; clone: its span, used again for its label
    let mut diag = Diag::error(None, format!("invalid suffix `{suffix}` for number literal"), span.clone());
    // alloc: the error's label
    diag.span_label(span, format!("invalid suffix `{suffix}`"));
    diag.help("the suffix must be one of the numeric types (`u32`, `isize`, `f32`, etc.)");
    diag
}

/// `InvalidFloatLiteralWidth` or `InvalidFloatLiteralSuffix`.
fn invalid_float_suffix(span: Span, suffix: &str) -> Diag {
    if suffix.len() > 1 && suffix.starts_with('f') && suffix[1..].chars().all(|it| it.is_ascii_digit()) {
        // alloc: the error's message
        let mut diag = Diag::error(None, format!("invalid width `{}` for float literal", &suffix[1..]), span);
        diag.help("valid widths are 32 and 64");
        return diag;
    }
    // alloc: the error's message; clone: its span, used again for its label
    let mut diag = Diag::error(None, format!("invalid suffix `{suffix}` for float literal"), span.clone());
    // alloc: the error's label
    diag.span_label(span, format!("invalid suffix `{suffix}`"));
    diag.help("valid suffixes are `f32` and `f64`");
    diag
}

/// rustc's `fix_base_capitalisation`: `0X1F` written for `0x1F`.
fn fix_base_capitalisation(symbol: &str, suffix: &str) -> Option<String> {
    let mut chars = suffix.chars();
    let base_char = chars.next()?;
    let base = match base_char {
        'B' => 2,
        'O' => 8,
        'X' => 16,
        _ => return None,
    };
    let valid = symbol == "0"
        && chars.filter(|it| *it != '_').take_while(|it| *it != 'i' && *it != 'u').all(|it| it.to_digit(base).is_some());
    // alloc: the error's suggestion
    valid.then(|| format!("0{}{}", base_char.to_ascii_lowercase(), &suffix[1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paths_are_sorted() {
        assert!(PATHS.windows(2).all(|it| it[0] < it[1]));
    }

    #[test]
    fn expected_tokens_are_listed_as_rustc_lists_them() {
        assert_eq!(stdx::list::list(["`,`"], stdx::list::Join::Or, false), "`,`");
        assert_eq!(stdx::list::list(["`)`", "`,`"], stdx::list::Join::Or, false), "`)` or `,`");
        assert_eq!(stdx::list::list(["`(`", "`,`", "`::`", "`=`"], stdx::list::Join::Or, false), "`(`, `,`, `::`, or `=`");
    }

    #[test]
    fn punctuation_is_glued_as_rustc_lexes_it() {
        assert!(is_glued(":", ":"));
        assert!(is_glued("=", "="));
        assert!(is_glued("..", "="));
        assert!(is_glued("<<", "="));
        assert!(!is_glued("==", "="));
        assert!(!is_glued("-", "-"));
        assert!(!is_glued(",", ","));
    }

    #[test]
    fn only_identifiers_r_hash_can_go_before_can_be_raw() {
        assert!(can_be_raw("foo"));
        assert!(can_be_raw("fn"));
        assert!(!can_be_raw("self"));
        assert!(!can_be_raw("_"));
    }

    #[test]
    fn a_template_takes_its_forms() {
        let word = |name: &str| Item {
            range: TextRange::default(),
            lit: false,
            is_word: true,
            word: Some(name.to_owned()),
        };
        assert!(!compatible(&LINT, &Meta::Word));
        assert!(compatible(&LINT, &Meta::List(Vec::new())));
        assert!(!compatible(&LINT, &Meta::NameValue(true)));
        assert!(compatible(&WORD, &Meta::Word));
        assert!(!compatible(&WORD, &Meta::List(vec![word("a")])));
        let one_of = Template { word: false, list: None, one_of: &["a"], name_value_str: None, docs: None };
        assert!(compatible(&one_of, &Meta::List(vec![word("a")])));
        assert!(!compatible(&one_of, &Meta::List(vec![word("b")])));
        assert!(compatible(&PATH, &Meta::NameValue(true)));
        assert!(!compatible(&PATH, &Meta::NameValue(false)));
    }

    #[test]
    fn a_capitalised_base_prefix_is_fixed() {
        assert_eq!(fix_base_capitalisation("0", "X1F").as_deref(), Some("0x1F"));
        assert_eq!(fix_base_capitalisation("0", "B12"), None);
    }
}
