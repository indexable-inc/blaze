//! The errors of the parsers of codegen attributes: `#[inline]`, `#[optimize]`, `#[repr]`,
//! `#[link]` and the other linking attributes, `#[target_feature]`, `#[unsafe(naked)]`,
//! `#[sanitize]`, `#[instruction_set]`, `#[rustc_align]`, `#[rustc_autodiff]`, `#[custom_mir]`
//! and those of no arguments.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/` `codegen_attrs.rs`,
//! `inline.rs`, `link_attrs.rs`, `instruction_set.rs`, `repr.rs`, `cfi_encoding.rs`,
//! `rustc_allocator.rs`, `unroll.rs`, `splat.rs`, `autodiff.rs`, `prototype.rs`,
//! `loop_match.rs` and `body.rs`, with the texts of
//! `compiler/rustc_attr_parsing/src/session_diagnostics.rs` (MIT or Apache-2.0).

use syntax::TextRange;

use super::{Accepted, Finalize, GroupId, no_args, single_integer};
use crate::{
    checks::{
        attrs::{
            accept::{Accept, BinaryFormat},
            args::{ArgParser, MetaItem, MetaItemOrLit, MetaList, NameValue},
            cfg, is, with_path,
            parsed::{Inline, Optimize, Parsed, ParsedAttr, Repr},
        },
        features::attributes,
    },
    lints::syntax::attrs::{
        Target,
        table::{Policy, Targets},
    },
    model::Diag,
};

pub(crate) const PATHS: &[&[&str]] = &[
    &["cfi_encoding"],
    &["cold"],
    &["compiler_builtins"],
    &["const_continue"],
    &["coroutine"],
    &["coverage"],
    &["custom_mir"],
    &["export_name"],
    &["export_stable"],
    &["ffi_const"],
    &["ffi_pure"],
    &["force_target_feature"],
    &["inline"],
    &["instruction_set"],
    &["instrument_fn"],
    &["link"],
    &["link_name"],
    &["link_ordinal"],
    &["link_section"],
    &["linkage"],
    &["loop_match"],
    &["naked"],
    &["needs_allocator"],
    &["no_mangle"],
    &["optimize"],
    &["patchable_function_entry"],
    &["repr"],
    &["rustc_align"],
    &["rustc_align_static"],
    &["rustc_allocator"],
    &["rustc_allocator_zeroed"],
    &["rustc_allocator_zeroed_variant"],
    &["rustc_autodiff"],
    &["rustc_deallocator"],
    &["rustc_eii_foreign_item"],
    &["rustc_force_inline"],
    &["rustc_objc_class"],
    &["rustc_objc_selector"],
    &["rustc_pass_indirectly_in_non_rustic_abis"],
    &["rustc_reallocator"],
    &["rustc_splat"],
    &["rustc_std_internal_symbol"],
    &["sanitize"],
    &["target_feature"],
    &["thread_local"],
    &["track_caller"],
    &["unroll"],
    &["used"],
];

/// The state of the parsers of several attributes, of one node.
#[derive(Default)]
pub(crate) struct State {
    /// The first `#[unsafe(naked)]`.
    naked: Option<usize>,
    used: bool,
    target_feature: bool,
    force_target_feature: bool,
    repr: Vec<(Repr, TextRange)>,
    link: bool,
    align: Option<u64>,
    align_static: Option<u64>,
}

pub(crate) fn accept(state: &mut State, cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Accepted {
    let other = |name| Parsed::Other(name);
    match cx.builtin.path {
        ["optimize"] => Accepted::Single(optimize(cx, args)),
        ["cold"] => no_args(cx, args, other("Cold")),
        ["coverage"] => Accepted::Single(coverage(cx, args)),
        ["export_name"] => Accepted::Single(export_name(cx, args)),
        ["rustc_objc_class"] => Accepted::Single(objc(cx, args, "class")),
        ["rustc_objc_selector"] => Accepted::Single(objc(cx, args, "selector")),
        ["naked"] => {
            if cx.expect_no_args(args).is_some() && state.naked.is_none() {
                state.naked = Some(cx.index);
            }
            Accepted::Group("naked")
        }
        ["track_caller"] => no_args(cx, args, Parsed::TrackCaller),
        ["no_mangle"] => no_args(cx, args, Parsed::NoMangle),
        ["used"] => {
            state.used |= used(cx, args);
            Accepted::Group("used")
        }
        ["target_feature"] => {
            target_features(cx, args);
            state.target_feature = true;
            Accepted::Group("target_feature")
        }
        ["force_target_feature"] => {
            target_features(cx, args);
            state.force_target_feature = true;
            Accepted::Group("force_target_feature")
        }
        ["instrument_fn"] => Accepted::Single(instrument_fn(cx, args)),
        ["sanitize"] => Accepted::Single(sanitize(cx, args)),
        ["thread_local"] => no_args(cx, args, other("ThreadLocal")),
        ["rustc_pass_indirectly_in_non_rustic_abis"] => no_args(cx, args, Parsed::RustcPassIndirectlyInNonRusticAbis),
        ["rustc_eii_foreign_item"] => no_args(cx, args, other("RustcEiiForeignItem")),
        ["patchable_function_entry"] => Accepted::Single(patchable_function_entry(cx, args)),
        ["inline"] => Accepted::Single(inline(cx, args)),
        ["rustc_force_inline"] => Accepted::Single(force_inline(cx, args)),
        ["instruction_set"] => Accepted::Single(instruction_set(cx, args)),
        ["cfi_encoding"] => Accepted::Single(cfi_encoding(cx, args)),
        ["rustc_allocator"] => no_args(cx, args, other("RustcAllocator")),
        ["rustc_allocator_zeroed"] => no_args(cx, args, other("RustcAllocatorZeroed")),
        ["rustc_allocator_zeroed_variant"] => {
            let nv = cx.expect_name_value(args, cx.attr_range(), None);
            let name = nv.and_then(|nv| cx.expect_string_literal(nv));
            Accepted::Single(name.map(|_| other("RustcAllocatorZeroedVariant")))
        }
        ["rustc_deallocator"] => no_args(cx, args, other("RustcDeallocator")),
        ["rustc_reallocator"] => no_args(cx, args, other("RustcReallocator")),
        ["unroll"] => Accepted::Single(unroll(cx, args)),
        ["rustc_splat"] => no_args(cx, args, other("Splat")),
        ["loop_match"] => no_args(cx, args, other("LoopMatch")),
        ["const_continue"] => no_args(cx, args, other("ConstContinue")),
        ["coroutine"] => no_args(cx, args, other("Coroutine")),
        ["repr"] => {
            repr(cx, args, &mut state.repr);
            Accepted::Group("repr")
        }
        ["rustc_align"] => {
            if let Some(align) = rustc_align(cx, args) {
                state.align = state.align.max(Some(align));
            }
            Accepted::Group("rustc_align")
        }
        ["rustc_align_static"] => {
            if let Some(align) = rustc_align(cx, args) {
                state.align_static = state.align_static.max(Some(align));
            }
            Accepted::Group("rustc_align_static")
        }
        ["link_name"] => Accepted::Single(link_name(cx, args)),
        ["link"] => {
            state.link |= link(cx, args);
            Accepted::Group("link")
        }
        ["link_section"] => Accepted::Single(link_section(cx, args)),
        ["export_stable"] => no_args(cx, args, other("ExportStable")),
        ["ffi_const"] => no_args(cx, args, other("FfiConst")),
        ["ffi_pure"] => no_args(cx, args, other("FfiPure")),
        ["rustc_std_internal_symbol"] => no_args(cx, args, other("RustcStdInternalSymbol")),
        ["link_ordinal"] => Accepted::Single(link_ordinal(cx, args)),
        ["linkage"] => Accepted::Single(linkage(cx, args)),
        ["needs_allocator"] => no_args(cx, args, other("NeedsAllocator")),
        ["compiler_builtins"] => no_args(cx, args, other("CompilerBuiltins")),
        ["rustc_autodiff"] => Accepted::Single(autodiff(cx, args)),
        ["custom_mir"] => Accepted::Single(custom_mir(cx, args)),
        path => unreachable!("{path:?} is not a codegen attribute"),
    }
}

pub(crate) fn finalize(state: &mut State, group: &'static str, fx: &mut Finalize<'_, '_, '_>) -> Option<Parsed> {
    match group {
        "naked" => {
            let naked = state.naked?;
            naked_compatible(fx, naked);
            Some(Parsed::Naked)
        }
        "used" => state.used.then_some(Parsed::Other("Used")),
        "target_feature" => state.target_feature.then_some(Parsed::Other("TargetFeature")),
        "force_target_feature" => state.force_target_feature.then_some(Parsed::Other("TargetFeature")),
        "repr" => Some(Parsed::Repr(std::mem::take(&mut state.repr))),
        "link" => state.link.then_some(Parsed::Other("Link")),
        "rustc_align" => state.align.map(|_| Parsed::Other("RustcAlign")),
        "rustc_align_static" => state.align_static.map(|_| Parsed::Other("RustcAlign")),
        group => unreachable!("{group} is not a codegen parser group"),
    }
}

pub(crate) fn check(group: GroupId, first: usize, fx: &mut Finalize<'_, '_, '_>, _parsed: &[ParsedAttr]) {
    match group {
        GroupId::Named("target_feature") => {
            if fx.session.target.is_like_wasm() {
                return;
            }
            let lang =
                fx.owner.attrs.iter().find_map(|attr| ["panic_handler", "lang"].into_iter().find(|it| is(attr, it)));
            if let Some(kind) = lang {
                let what = match kind {
                    "panic_handler" => "`#[panic_handler]`",
                    _ => "lang item",
                };
                // alloc: the error's message
                let mut diag = Diag::error(None, format!("`#[target_feature]` cannot be applied to a {what} function"), fx.attr_span(first));
                // alloc: the error's label
                diag.span_label(fx.target_span(), format!("{what} function is not allowed to have `#[target_feature]`"));
                fx.emit_err(diag);
            }
        }
        GroupId::Single(["ffi_pure"]) => {
            if fx.owner.attrs.iter().any(|it| is(it, "ffi_const")) {
                let diag = Diag::error(Some("E0757"), "`#[ffi_const]` function cannot be `#[ffi_pure]`", fx.attr_span(first));
                fx.emit_err(diag);
            }
        }
        _ => {}
    }
}

/// The name of a meta item of no arguments of one segment: rustc's
/// `meta_item_no_args().and_then(|i| i.path().word_sym())`.
fn word_of(item: &MetaItemOrLit) -> Option<&str> {
    item.meta_item_no_args().and_then(|it| it.path.word_sym())
}

fn optimize(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let single = cx.expect_single_element_list(args, cx.attr_range())?;
    let optimize = match word_of(single) {
        Some("size") => Optimize::Size,
        Some("speed") => Optimize::Speed,
        Some("none") => Optimize::DoNotOptimize,
        _ => {
            cx.expected_specific_argument(single.range(), &["size", "speed", "none"]);
            Optimize::Default
        }
    };
    Some(Parsed::Optimize(optimize))
}

fn coverage(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let arg = cx.expect_single_element_list(args, cx.attr_range())?;
    let Some(item) = arg.meta_item_no_args() else {
        cx.expected_specific_argument(arg.range(), &["on", "off"]);
        return None;
    };
    match item.path.word_sym() {
        Some("off" | "on") => Some(Parsed::Other("Coverage")),
        _ => {
            cx.expected_specific_argument(item.range(), &["on", "off"]);
            None
        }
    }
}

fn export_name(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), None)?;
    let name = cx.expect_string_literal(nv)?;
    if name.contains('\0') {
        let diag = cx.err(Some("E0648"), "`export_name` may not contain null characters", cx.attr_range());
        cx.emit_err(diag);
        return None;
    }
    if name.is_empty() {
        let diag = cx.err(None, "`export_name` may not be empty", cx.attr_range());
        cx.emit_err(diag);
        return None;
    }
    Some(Parsed::ExportName(name))
}

/// `#[rustc_objc_class]` and `#[rustc_objc_selector]`, of `objc::class!` and `objc::selector!`.
fn objc(cx: &mut Accept<'_, '_, '_>, args: &ArgParser, what: &str) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), None)?;
    let Some(value) = nv.value_as_str() else {
        // alloc: the error's message
        let diag = cx.err(None, format!("`objc::{what}!` expected a string literal"), nv.value_range());
        cx.emit_err(diag);
        return None;
    };
    if value.contains('\0') {
        // alloc: the error's message
        let diag = cx.err(None, format!("`objc::{what}!` may not contain null characters"), nv.value_range());
        cx.emit_err(diag);
        return None;
    }
    Some(Parsed::Other(if what == "class" { "RustcObjcClass" } else { "RustcObjcSelector" }))
}

/// `NakedParser::finalize`: each attribute of the node not allowed with `#[unsafe(naked)]`.
fn naked_compatible(fx: &mut Finalize<'_, '_, '_>, naked: usize) {
    const ALLOW_LIST: &[&str] = &[
        "test",
        "ignore",
        "should_panic",
        "bench",
        "allow",
        "warn",
        "deny",
        "forbid",
        "deprecated",
        "must_use",
        "cold",
        "export_name",
        "link_section",
        "linkage",
        "no_mangle",
        "instruction_set",
        "repr",
        "rustc_std_internal_symbol",
        "rustc_align",
        "rustc_align_static",
        "naked",
        "doc",
    ];
    for index in 0..fx.owner.attrs.len() {
        let attr = fx.attr(index);
        if attr.is_doc_comment() {
            continue;
        }
        let session = fx.session;
        let allowed = with_path(attr, |path| {
            path.first().is_some_and(|it| session.is_tool(it))
                || matches!(path, [one] if ALLOW_LIST.contains(one))
                // blaze's own inert tool attribute (`blaze::parallelizable` reads it), opt-in.
                || matches!(path, ["blaze", "reassociate"])
        });
        if allowed {
            continue;
        }
        if is(attr, "target_feature") {
            let diag = attributes::naked_target_feature(fx.cx, &fx.session.features, attr.in_file(attr.path_range()));
            if let Some(diag) = diag {
                fx.emit_err(diag);
            }
            continue;
        }
        let path_span = fx.span(index, attr.path_range());
        // clone: the error's span
        let mut diag = Diag::error(Some("E0736"), "attribute incompatible with `#[unsafe(naked)]`", path_span.clone());
        // alloc: the error's label
        diag.span_label(path_span, format!("the `{}` attribute is incompatible with `#[unsafe(naked)]`", attr.name()));
        diag.span_label(fx.attr_span(naked), "function marked with `#[unsafe(naked)]` here");
        fx.emit_err(diag);
    }
}

/// `UsedParser`'s errors: whether it takes the attribute.
fn used(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> bool {
    match args {
        ArgParser::NoArgs => true,
        ArgParser::List(list) => {
            let Some(item) = cx.expect_single(list) else { return false };
            match word_of(item) {
                Some(by @ ("compiler" | "linker")) => {
                    cx.emit_gate(attributes::used_with_arg(cx.cx, &cx.session.features, by, cx.in_file(cx.attr_range())));
                    true
                }
                _ => {
                    cx.expected_specific_argument(item.range(), &["compiler", "linker"]);
                    false
                }
            }
        }
        ArgParser::NameValue(_) => false,
    }
}

/// `parse_tf_attribute`: `enable = ".."`, each.
fn target_features(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    let Some(list) = cx.expect_list(args, cx.attr_range()) else { return };
    if list.is_empty() {
        return;
    }
    for item in list.mixed() {
        let Some((ident, value)) = cx.expect_name_value_item(item, Some("enable")) else { return };
        if ident.name != "enable" {
            cx.expected_specific_argument(ident.range, &["enable"]);
            return;
        }
        if cx.expect_string_literal(value).is_none() {
            return;
        }
    }
}

fn instrument_fn(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    match args {
        ArgParser::NameValue(nv) => match nv.value_as_str() {
            Some("on" | "off") => Some(Parsed::Other("InstrumentFn")),
            _ => {
                cx.expected_specific_argument_strings(nv.value_range(), &["on", "off"]);
                None
            }
        },
        ArgParser::List(list) => {
            cx.expected_single_argument(list.range, list.len());
            None
        }
        ArgParser::NoArgs => {
            cx.expected_specific_argument_strings(cx.attr_range(), &["on", "off"]);
            None
        }
    }
}

fn sanitize(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    // The sanitizers each name sets, by rustc's `SanitizerSet` bit; `address` is allowed on
    // statics.
    const MEMORY: u32 = 1 << 2;
    const THREAD: u32 = 1 << 3;
    const HWADDRESS: u32 = 1 << 4;
    const CFI: u32 = 1 << 5;
    const MEMTAG: u32 = 1 << 6;
    const SHADOWCALLSTACK: u32 = 1 << 7;
    const KCFI: u32 = 1 << 8;
    const KERNELHWADDRESS: u32 = 1 << 10;
    const ADDRESS: u32 = 1 | 1 << 9;
    const NAMES: &[(u32, &str)] = &[
        (MEMORY, "memory"),
        (THREAD, "thread"),
        (HWADDRESS, "hwaddress"),
        (CFI, "cfi"),
        (MEMTAG, "memtag"),
        (SHADOWCALLSTACK, "shadow-call-stack"),
        (KCFI, "kcfi"),
        (KERNELHWADDRESS, "kernel-hwaddress"),
    ];
    let list = cx.expect_list(args, cx.attr_range())?;
    let mut set = 0u32;
    for item in list.mixed() {
        let Some((ident, value)) = cx.expect_name_value_item(item, None) else { continue };
        let mut apply = |cx: &mut Accept<'_, '_, '_>, bits: u32| match value.value_as_str() {
            Some("on" | "off") => set |= bits,
            _ => cx.expected_specific_argument_strings(value.value_range(), &["on", "off"]),
        };
        match ident.name.as_str() {
            "address" | "kernel_address" => apply(cx, ADDRESS),
            "cfi" => apply(cx, CFI),
            "kcfi" => apply(cx, KCFI),
            "memory" => apply(cx, MEMORY),
            "memtag" => apply(cx, MEMTAG),
            "shadow_call_stack" => apply(cx, SHADOWCALLSTACK),
            "thread" => apply(cx, THREAD),
            "hwaddress" | "kernel_hwaddress" => apply(cx, HWADDRESS | KERNELHWADDRESS),
            "realtime" => {
                if !matches!(value.value_as_str(), Some("nonblocking" | "blocking" | "caller")) {
                    cx.expected_specific_argument_strings(value.value_range(), &["nonblocking", "blocking", "caller"]);
                }
            }
            _ => cx.expected_specific_argument_strings(
                ident.range,
                &[
                    "address",
                    "kernel_address",
                    "cfi",
                    "kcfi",
                    "memory",
                    "memtag",
                    "shadow_call_stack",
                    "thread",
                    "hwaddress",
                    "kernel_hwaddress",
                    "realtime",
                ],
            ),
        }
    }
    if cx.target() == Target::Static
        && let Some((_, field)) = NAMES.iter().find(|(bit, _)| set & !ADDRESS & bit != 0)
    {
        // alloc: the error's message
        let mut diag = cx.err(None, format!("`#[sanitize({field} = ...)]` attribute cannot be used on statics"), cx.attr_range());
        diag.help("`#[sanitize]` can be used on statics if only the address is sanitized");
        cx.emit_err(diag);
    }
    Some(Parsed::Other("Sanitize"))
}

fn patchable_function_entry(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let list = cx.expect_list(args, cx.attr_range())?;
    if list.is_empty() {
        cx.expected_at_least_one_argument(list.range);
        return None;
    }
    let (mut prefix, mut entry, mut section) = (false, false, false);
    for item in list.mixed() {
        let (ident, value) = cx.expect_name_value_item(item, None)?;
        let seen = match ident.name.as_str() {
            key @ ("prefix_nops" | "entry_nops") => {
                let seen = if key == "prefix_nops" { &mut prefix } else { &mut entry };
                if *seen {
                    cx.duplicate_key(ident.range, key);
                    return None;
                }
                seen
            }
            "section" => {
                if section {
                    cx.duplicate_key(ident.range, "section");
                    return None;
                }
                let Some(name) = value.value_as_str() else {
                    cx.expected_string_literal(value.value_range(), Some(value.value_as_lit()));
                    return None;
                };
                if name.contains('\0') {
                    let diag = cx.err(Some("E0648"), "`section` may not contain null characters", value.value_range());
                    cx.emit_err(diag);
                }
                if name.is_empty() {
                    let diag = cx.err(None, "`section` may not be empty", value.value_range());
                    cx.emit_err(diag);
                }
                section = true;
                continue;
            }
            _ => {
                cx.expected_specific_argument(ident.range, &["prefix_nops", "entry_nops"]);
                return None;
            }
        };
        let Some(count) = value.value_as_lit().int() else {
            cx.expected_integer_literal(value.value_range());
            return None;
        };
        if u8::try_from(count).is_err() {
            cx.expected_integer_literal_in_range(value.value_range(), 0, 255);
            return None;
        }
        *seen = true;
    }
    Some(Parsed::Other("PatchableFunctionEntry"))
}

fn inline(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    match args {
        ArgParser::NoArgs => Some(Parsed::Inline(Inline::Hint)),
        ArgParser::List(list) => {
            let item = cx.expect_single(list)?;
            match word_of(item) {
                Some("always") => Some(Parsed::Inline(Inline::Always)),
                Some("never") => Some(Parsed::Inline(Inline::Never)),
                _ => {
                    cx.expected_specific_argument(item.range(), &["always", "never"]);
                    None
                }
            }
        }
        ArgParser::NameValue(_) => None,
    }
}

fn force_inline(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    match args {
        ArgParser::NoArgs => {}
        ArgParser::List(list) => {
            let item = cx.expect_single(list)?;
            cx.expect_string_literal_item(item)?;
        }
        ArgParser::NameValue(nv) => {
            let _ = cx.expect_string_literal(nv);
        }
    }
    Some(Parsed::Inline(Inline::Force))
}

fn instruction_set(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    const POSSIBLE: &[&str] = &["arm::a32", "arm::t32"];
    let item = cx.expect_single_element_list(args, cx.attr_range())?;
    let Some(meta) = item.meta_item_no_args() else {
        cx.expected_specific_argument(item.range(), POSSIBLE);
        return None;
    };
    let mut segments = meta.path.segments.iter();
    let Some(architecture) = segments.next() else {
        cx.expected_specific_argument(meta.range(), POSSIBLE);
        return None;
    };
    let Some(set) = segments.next() else {
        cx.expected_specific_argument(architecture.range, POSSIBLE);
        return None;
    };
    if architecture.name != "arm" {
        cx.expected_specific_argument(architecture.range, POSSIBLE);
        return None;
    }
    // rustc's `UnsupportedInstructionSet` (an `arm` set on a target without thumb
    // interworking) names the target tuple, which the crate's cfg does not give: asked of
    // the front end (`v2/requests/diagnostics.md`).
    match set.name.as_str() {
        "a32" | "t32" => Some(Parsed::Other("InstructionSet")),
        _ => {
            cx.expected_specific_argument(set.range, &["a32", "t32"]);
            None
        }
    }
}

fn cfi_encoding(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), Some("cfi_encoding"))?;
    let value = cx.expect_string_literal(nv)?;
    if value.trim().is_empty() {
        cx.expected_non_empty_string_literal(nv.value_range());
        return None;
    }
    Some(Parsed::Other("CfiEncoding"))
}

fn unroll(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    match args {
        ArgParser::NoArgs => Some(Parsed::Other("Unroll")),
        ArgParser::List(list) => {
            let item = cx.expect_single(list)?;
            if let Some(lit) = item.as_lit()
                && lit.suffix.is_none()
                && let Some(count) = lit.int()
            {
                if u32::try_from(count).is_err() {
                    cx.expected_integer_literal_in_range(item.range(), 0, i64::from(u32::MAX));
                    return None;
                }
                return Some(Parsed::Other("Unroll"));
            }
            match word_of(item) {
                Some("full" | "never") => Some(Parsed::Other("Unroll")),
                _ => {
                    cx.expected_specific_argument(item.range(), &["full", "never"]);
                    None
                }
            }
        }
        ArgParser::NameValue(_) => {
            cx.expected_list_or_no_args(cx.inner_range());
            None
        }
    }
}

/// `ReprParser::extend`: each hint of one `#[repr(..)]`, with its target errors.
fn repr(cx: &mut Accept<'_, '_, '_>, args: &ArgParser, reprs: &mut Vec<(Repr, TextRange)>) {
    let Some(list) = cx.expect_list(args, cx.attr_range()) else { return };
    if list.is_empty() {
        cx.check_target("()", Targets::AllowList(DATA));
        return;
    }
    for param in list.mixed() {
        let Some(item) = param.meta_item() else {
            cx.expected_identifier(param.range());
            continue;
        };
        if let Some(repr) = repr_hint(cx, item) {
            reprs.push((repr, param.range()));
        }
    }
}

const DATA: &[Policy] = &[
    Policy::Allow(Target::Struct),
    Policy::Allow(Target::Enum),
    Policy::Allow(Target::Union),
    Policy::Warn(Target::MacroCall),
];

/// `parse_repr`.
fn repr_hint(cx: &mut Accept<'_, '_, '_>, param: &MetaItem) -> Option<Repr> {
    const INTS: &[&str] = &["i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "i128", "u128", "isize", "usize"];
    /// Each of `INTS` as `check_target` writes its arguments.
    const INT_ARGS: [&str; 12] =
        ["(i8)", "(u8)", "(i16)", "(u16)", "(i32)", "(u32)", "(i64)", "(u64)", "(i128)", "(u128)", "(isize)", "(usize)"];
    let hint = param.path.word_sym();
    match hint {
        Some("align") => {
            cx.check_target("(align(...))", Targets::AllowList(DATA));
            let list = cx.expect_list(&param.args, param.range())?;
            repr_align(cx, list).map(Repr::Align)
        }
        Some("packed") => {
            cx.check_target(
                "(packed)",
                Targets::AllowList(&[
                    Policy::Allow(Target::Struct),
                    Policy::Allow(Target::Union),
                    Policy::Warn(Target::MacroCall),
                ]),
            );
            match &param.args {
                ArgParser::NoArgs => Some(Repr::Packed(1)),
                ArgParser::List(list) => repr_align(cx, list).map(Repr::Packed),
                ArgParser::NameValue(_) => {
                    cx.expected_list_or_no_args(param.range());
                    None
                }
            }
        }
        Some(name @ ("Rust" | "C" | "transparent")) => {
            let args = match name {
                "Rust" => "(Rust)",
                "C" => "(C)",
                _ => "(transparent)",
            };
            cx.check_target(args, Targets::AllowList(DATA));
            cx.expect_no_args(&param.args)?;
            Some(match name {
                "Rust" => Repr::Rust,
                "C" => Repr::C,
                _ => Repr::Transparent,
            })
        }
        Some("simd") => {
            cx.emit_gate(attributes::repr_simd(cx.cx, &cx.session.features, cx.in_file(param.range())));
            cx.check_target("(simd)", Targets::AllowList(&[Policy::Allow(Target::Struct)]));
            cx.expect_no_args(&param.args)?;
            Some(Repr::Simd)
        }
        Some(int) if INTS.contains(&int) => {
            let at = INTS.iter().position(|it| *it == int).expect("`INTS` has the hint");
            cx.check_target(
                INT_ARGS[at],
                Targets::AllowList(&[Policy::Allow(Target::Enum), Policy::Warn(Target::MacroCall)]),
            );
            cx.expect_no_args(&param.args)?;
            Some(Repr::Int(INTS[at]))
        }
        _ => {
            // alloc: the error's expected names
            let mut possibilities = vec!["align", "packed", "Rust", "C", "simd", "transparent"];
            possibilities.extend_from_slice(INTS);
            cx.expected_specific_argument(param.range(), &possibilities);
            None
        }
    }
}

/// `parse_repr_align`: `align(N)` and `packed(N)`.
fn repr_align(cx: &mut Accept<'_, '_, '_>, list: &MetaList) -> Option<u64> {
    let Some(align) = list.as_single() else {
        cx.expected_single_argument(list.range, list.len());
        return None;
    };
    let Some(lit) = align.as_lit() else {
        cx.expected_integer_literal(align.range());
        return None;
    };
    alignment(cx, lit)
}

/// `parse_alignment`, and `InvalidAlignmentValue` where it is not one.
fn alignment(cx: &mut Accept<'_, '_, '_>, lit: &crate::checks::attrs::args::Lit) -> Option<u64> {
    let error = match lit.int() {
        Some(value) if lit.suffix.is_none() => {
            if !value.is_power_of_two() {
                // alloc: the error's reason
                "not a power of two".to_owned()
            } else if value > 1 << 29 {
                // alloc: the error's reason
                "larger than 2^29".to_owned()
            } else {
                let max = cx.session.target.pointer_width.map(|it| (1u128 << (it - 1)) - 1);
                match max {
                    Some(max) if value > max => {
                        // alloc: the error's reason
                        format!("alignment larger than `isize::MAX` bytes ({max} for the current target)")
                    }
                    _ => return Some(value as u64),
                }
            }
        }
        // alloc: the error's reason
        _ => "not an unsuffixed integer".to_owned(),
    };
    // alloc: the error's message
    let diag = cx.err(Some("E0589"), format!("invalid alignment value: {error}"), lit.range);
    cx.emit_err(diag);
    None
}

/// `RustcAlignParser::parse`.
fn rustc_align(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<u64> {
    let list = cx.expect_list(args, cx.attr_range())?;
    let align = cx.expect_single(list)?;
    let Some(lit) = align.as_lit() else {
        cx.expected_integer_literal(align.range());
        return None;
    };
    alignment(cx, lit)
}

/// `NullOnLinkName` and `EmptyLinkName` of a link name.
fn link_name_errors(cx: &mut Accept<'_, '_, '_>, name: &str, range: TextRange) -> bool {
    let mut ok = true;
    if name.contains('\0') {
        let diag = cx.err(Some("E0648"), "link name may not contain null characters", range);
        cx.emit_err(diag);
        ok = false;
    }
    if name.is_empty() {
        let mut diag = cx.err(Some("E0454"), "link name must not be empty", range);
        diag.span_label(cx.span(range), "empty link name");
        cx.emit_err(diag);
        ok = false;
    }
    ok
}

fn link_name(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), None)?;
    let name = cx.expect_string_literal(nv)?;
    if name.contains('\0') {
        let diag = cx.err(Some("E0648"), "link name may not contain null characters", nv.value_range());
        cx.emit_err(diag);
        return None;
    }
    if name.is_empty() {
        let mut diag = cx.err(Some("E0454"), "link name must not be empty", nv.value_range());
        diag.span_label(cx.span(nv.value_range()), "empty link name");
        cx.emit_err(diag);
        return None;
    }
    Some(Parsed::Other("LinkName"))
}

/// rustc's `NativeLibKind`, of what `#[link]`'s checks read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkKind {
    Static,
    Dylib,
    Framework,
    RawDylib,
    LinkArg,
}

/// `LinkParser::extend`: whether it makes an entry.
fn link(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> bool {
    let items = match args {
        ArgParser::List(list) => list,
        ArgParser::NameValue(nv) if nv.value_as_str() == Some("dl") => return false,
        _ => {
            cx.expected_list(cx.attr_range(), args);
            return false;
        }
    };
    let mut name: Option<TextRange> = None;
    let mut kind: Option<LinkKind> = None;
    let mut kind_seen = false;
    let mut modifiers: Option<(String, TextRange)> = None;
    let mut cfg = false;
    let mut wasm_import_module: Option<TextRange> = None;
    let mut import_name_type: Option<TextRange> = None;
    for item in items.mixed() {
        let Some(item) = item.meta_item() else {
            cx.expected_not_literal(item.range());
            continue;
        };
        let cont = match item.path.word_sym() {
            Some("name") => {
                if name.is_some() {
                    cx.duplicate_key(item.range(), "name");
                    true
                } else {
                    match link_string(cx, item, "name") {
                        Some((value, range)) => {
                            link_name_errors(cx, &value, range);
                            name = Some(range);
                            true
                        }
                        None => false,
                    }
                }
            }
            Some("kind") => {
                if kind_seen {
                    cx.duplicate_key(item.range(), "kind");
                } else if let Some((value, range)) = link_string(cx, item, "kind") {
                    kind = link_kind(cx, &value, range);
                    kind_seen = kind.is_some();
                }
                true
            }
            Some("modifiers") => {
                if modifiers.is_some() {
                    cx.duplicate_key(item.range(), "modifiers");
                } else if let Some(found) = link_string(cx, item, "modifiers") {
                    modifiers = Some(found);
                }
                true
            }
            Some("cfg") => {
                if cfg {
                    cx.duplicate_key(item.range(), "cfg");
                } else if let Some(predicate) = cx.expect_single_element_list(&item.args, item.range()) {
                    cx.emit_gate(attributes::link_cfg(cx.cx, &cx.session.features, cx.in_file(item.range())));
                    cfg = cfg::entry(cx, predicate);
                }
                true
            }
            Some("wasm_import_module") => {
                if wasm_import_module.is_some() {
                    cx.duplicate_key(item.range(), "wasm_import_module");
                } else if link_string(cx, item, "wasm_import_module").is_some() {
                    wasm_import_module = Some(item.range());
                }
                true
            }
            Some("import_name_type") => {
                if import_name_type.is_some() {
                    cx.duplicate_key(item.range(), "import_name_type");
                } else if let Some((value, _)) = link_string(cx, item, "import_name_type") {
                    if cx.session.target.arch != "x86" {
                        let diag = cx.err(None, "import name type is only supported on x86", item.range());
                        cx.emit_err(diag);
                    } else if matches!(value.as_str(), "decorated" | "noprefix" | "undecorated") {
                        import_name_type = Some(item.range());
                    } else {
                        cx.expected_specific_argument_strings(item.range(), &["decorated", "noprefix", "undecorated"]);
                    }
                }
                true
            }
            _ => {
                cx.expected_specific_argument_strings(
                    item.range(),
                    &["name", "kind", "modifiers", "cfg", "wasm_import_module", "import_name_type"],
                );
                true
            }
        };
        if !cont {
            return false;
        }
    }
    if let Some((modifiers, range)) = &modifiers {
        link_modifiers(cx, modifiers, *range, kind);
    }
    if let Some(range) = wasm_import_module
        && (name.is_some() || kind.is_some() || modifiers.is_some() || cfg)
    {
        let diag = cx.err(None, "`wasm_import_module` is incompatible with other arguments in `#[link]` attributes", range);
        cx.emit_err(diag);
    }
    let is_wasm_import = wasm_import_module.is_some();
    if name.is_none() && !is_wasm_import {
        let mut diag = cx.err(Some("E0459"), "`#[link]` attribute requires a `name = \"string\"` argument", cx.attr_range());
        diag.span_label(cx.attr_span(), "missing `name` argument");
        cx.emit_err(diag);
        return false;
    }
    if let Some(range) = import_name_type
        && (is_wasm_import || kind != Some(LinkKind::RawDylib))
    {
        let diag = cx.err(None, "import name type can only be used with link kind `raw-dylib`", range);
        cx.emit_err(diag);
    }
    true
}

/// `name = "string"` of a `#[link]` argument.
fn link_string(cx: &mut Accept<'_, '_, '_>, item: &MetaItem, key: &str) -> Option<(String, TextRange)> {
    let nv: &NameValue = cx.expect_name_value(&item.args, item.range(), Some(key))?;
    let value = cx.expect_string_literal(nv)?;
    Some((value, nv.value_range()))
}

/// `parse_link_kind`'s errors of `kind = ".."`.
fn link_kind(cx: &mut Accept<'_, '_, '_>, value: &str, range: TextRange) -> Option<LinkKind> {
    let target = &cx.session.target;
    let (is_darwin, is_windows, is_elf) =
        (target.is_like_darwin(), target.is_like_windows(), target.binary_format() == BinaryFormat::Elf);
    Some(match value {
        "static" => LinkKind::Static,
        "dylib" => LinkKind::Dylib,
        "framework" => {
            if !is_darwin {
                let diag = cx.err(Some("E0455"), "link kind `framework` is only supported on Apple targets", range);
                cx.emit_err(diag);
            }
            LinkKind::Framework
        }
        "raw-dylib" => {
            if is_windows || is_elf && cx.feature("raw_dylib_elf") {
            } else if is_elf && cx.session.features.nightly {
                cx.emit_gate(attributes::link_kind_raw_dylib_elf(cx.cx, &cx.session.features, cx.in_file(range)));
            } else {
                let diag = cx.err(Some("E0455"), "link kind `raw-dylib` is only supported on Windows targets", range);
                cx.emit_err(diag);
            }
            LinkKind::RawDylib
        }
        "link-arg" => {
            cx.emit_gate(attributes::link_kind_link_arg(cx.cx, &cx.session.features, cx.in_file(range)));
            LinkKind::LinkArg
        }
        _ => {
            cx.expected_specific_argument_strings(range, &["static", "dylib", "framework", "raw-dylib", "link-arg"]);
            return None;
        }
    })
}

/// The errors of `modifiers = ".."`.
fn link_modifiers(cx: &mut Accept<'_, '_, '_>, modifiers: &str, range: TextRange, kind: Option<LinkKind>) {
    // Each of the five modifiers `assign` is given at most once.
    let (mut seen, mut count) = ([""; 5], 0);
    for modifier in modifiers.split(',') {
        let Some(name) = modifier.strip_prefix(['+', '-']) else {
            let diag = cx.err(
                None,
                "invalid linking modifier syntax, expected '+' or '-' prefix before one of: bundle, verbatim, whole-archive, as-needed, export-symbols",
                range,
            );
            cx.emit_err(diag);
            continue;
        };
        let mut assign = |cx: &mut Accept<'_, '_, '_>, name: &'static str| {
            if seen[..count].contains(&name) {
                // alloc: the error's message
                let diag = cx.err(None, format!("multiple `{name}` modifiers in a single `modifiers` argument"), range);
                cx.emit_err(diag);
            } else {
                seen[count] = name;
                count += 1;
            }
        };
        let is_static = kind == Some(LinkKind::Static);
        match name {
            "bundle" if is_static => assign(cx, "bundle"),
            "bundle" => {
                let diag = cx.err(None, "linking modifier `bundle` is only compatible with `static` linking kind", range);
                cx.emit_err(diag);
            }
            "export-symbols" if is_static => assign(cx, "export-symbols"),
            "export-symbols" => {
                let diag = cx.err(None, "linking modifier `export-symbols` is only compatible with `static` linking kind", range);
                cx.emit_err(diag);
            }
            "verbatim" => assign(cx, "verbatim"),
            "whole-archive" if is_static => assign(cx, "whole-archive"),
            "whole-archive" => {
                let diag = cx.err(None, "linking modifier `whole-archive` is only compatible with `static` linking kind", range);
                cx.emit_err(diag);
            }
            "as-needed" if matches!(kind, Some(LinkKind::Dylib | LinkKind::Framework | LinkKind::RawDylib)) => {
                cx.emit_gate(attributes::link_modifier(cx.cx, &cx.session.features, "as-needed", cx.in_file(range)));
                assign(cx, "as-needed");
            }
            "as-needed" => {
                let diag = cx.err(
                    None,
                    "linking modifier `as-needed` is only compatible with `dylib`, `framework` and `raw-dylib` linking kinds",
                    range,
                );
                cx.emit_err(diag);
            }
            _ => cx.expected_specific_argument_strings(
                range,
                &["bundle", "export-symbols", "verbatim", "whole-archive", "as-needed"],
            ),
        }
    }
}

fn link_section(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), None)?;
    let name = cx.expect_string_literal(nv)?;
    if name.contains('\0') {
        let diag = cx.err(Some("E0648"), "`link_section` may not contain null characters", cx.attr_range());
        cx.emit_err(diag);
        return None;
    }
    if cx.session.target.binary_format() == BinaryFormat::MachO {
        let mut parts = name.split(',').map(str::trim);
        let _segment = parts.next();
        let reason = match parts.next() {
            None | Some("") => Some(None),
            // alloc: the error's section name
            Some(section) if section.len() > 16 => Some(Some(section.to_owned())),
            Some(_) => None,
        };
        if let Some(reason) = reason {
            let mut diag = cx.err(None, "invalid Mach-O section specifier", nv.value_range());
            diag.span_label(cx.span(nv.value_range()), "not a valid Mach-O section specifier");
            match reason {
                None => {
                    diag.note("a Mach-O section specifier requires a segment and a section, separated by a comma");
                    diag.help("an example of a valid Mach-O section specifier is `__TEXT,__cstring`");
                }
                Some(section) => {
                    // alloc: the error's note
                    diag.note(format!("section name `{section}` is longer than 16 bytes"));
                }
            }
            cx.emit_err(diag);
            return None;
        }
    }
    Some(Parsed::Other("LinkSection"))
}

fn link_ordinal(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let ordinal = single_integer(cx, args)?;
    if u16::try_from(ordinal).is_err() {
        // alloc: the error's message
        let mut diag = cx.err(None, format!("ordinal value in `link_ordinal` is too large: `{ordinal}`"), cx.attr_range());
        diag.note("the value may not exceed `u16::MAX`");
        cx.emit_err(diag);
        return None;
    }
    Some(Parsed::Other("LinkOrdinal"))
}

fn linkage(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    const LINKAGES: &[&str] = &[
        "available_externally",
        "common",
        "extern_weak",
        "external",
        "internal",
        "linkonce",
        "linkonce_odr",
        "weak",
        "weak_odr",
    ];
    let nv = cx.expect_name_value(args, cx.attr_range(), Some("linkage"))?;
    let value = cx.expect_string_literal(nv)?;
    if !LINKAGES.contains(&value.as_str()) {
        cx.expected_specific_argument(nv.value_range(), LINKAGES);
        return None;
    }
    Some(Parsed::Linkage(value))
}

fn autodiff(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    const MODES: &[&str] = &["Source", "Forward", "Reverse"];
    const ACTIVITIES: &[&str] = &[
        "None",
        "Active",
        "ActiveOnly",
        "Const",
        "Dual",
        "Dualv",
        "DualOnly",
        "DualvOnly",
        "Duplicated",
        "DuplicatedOnly",
    ];
    let list = match args {
        ArgParser::NoArgs => return Some(Parsed::Other("RustcAutodiff")),
        ArgParser::List(list) => list,
        ArgParser::NameValue(_) => {
            cx.expected_list_or_no_args(cx.attr_range());
            return None;
        }
    };
    let mut items = list.mixed().peekable();
    let Some(mode) = items.next() else {
        cx.expected_at_least_one_argument(list.range);
        return None;
    };
    let Some(mode) = mode.meta_item() else {
        cx.expected_identifier(mode.range());
        return None;
    };
    cx.expect_no_args(&mode.args)?;
    let Some(name) = mode.path.word() else {
        cx.expected_identifier(mode.range());
        return None;
    };
    if !matches!(name.name.as_str(), "Error" | "Source" | "Forward" | "Reverse") {
        cx.expected_specific_argument(name.range, MODES);
        return None;
    }
    if let Some(MetaItemOrLit::Lit(width)) = items.peek()
        && width.int().is_some_and(|it| u32::try_from(it).is_ok())
    {
        items.next();
    }
    let mut activities = 0usize;
    for activity in items {
        let MetaItemOrLit::Item(activity) = activity else {
            cx.expected_specific_argument(activity.range(), ACTIVITIES);
            return None;
        };
        cx.expect_no_args(&activity.args)?;
        let Some(name) = activity.path.word() else {
            cx.expected_specific_argument(activity.range(), ACTIVITIES);
            return None;
        };
        if !ACTIVITIES.contains(&name.name.as_str()) {
            cx.expected_specific_argument(name.range, ACTIVITIES);
            return None;
        }
        activities += 1;
    }
    if activities == 0 {
        cx.expected_specific_argument(TextRange::empty(list.range.end()), ACTIVITIES);
        return None;
    }
    Some(Parsed::Other("RustcAutodiff"))
}

fn custom_mir(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let list = cx.expect_list(args, cx.attr_range())?;
    let mut dialect: Option<(String, TextRange)> = None;
    let mut phase: Option<(String, TextRange)> = None;
    let mut failed = false;
    for item in list.mixed() {
        let Some((path, value)) = cx.expect_name_value_item(item, None) else {
            failed = true;
            break;
        };
        let slot = match path.name.as_str() {
            "dialect" => &mut dialect,
            "phase" => &mut phase,
            _ => {
                cx.expected_specific_argument(item.range(), &["dialect", "phase"]);
                failed = true;
                continue;
            }
        };
        if slot.is_some() {
            cx.duplicate_key(item.range(), &path.name);
            failed = true;
            continue;
        }
        match cx.expect_string_literal(value) {
            Some(text) => *slot = Some((text, value.value_range())),
            None => failed = true,
        }
    }
    let dialect = dialect.and_then(|(name, range)| {
        if matches!(name.as_str(), "analysis" | "built" | "runtime") {
            Some((name, range))
        } else {
            cx.expected_specific_argument(range, &["analysis", "built", "runtime"]);
            failed = true;
            None
        }
    });
    let phase = phase.and_then(|(name, range)| {
        let shown = match name.as_str() {
            "initial" => "initial",
            "post_cleanup" => "post-cleanup",
            "optimized" => "optimized",
            _ => {
                cx.expected_specific_argument(range, &["initial", "post_cleanup", "optimized"]);
                failed = true;
                return None;
            }
        };
        Some((shown, range))
    });
    match (&dialect, phase) {
        (None, Some((_, phase_range))) => {
            failed = true;
            let mut diag = cx.err(None, "`dialect` key required", cx.attr_range());
            diag.span_label(cx.span(phase_range), "`phase` argument requires a `dialect` argument");
            cx.emit_err(diag);
        }
        (Some((dialect, dialect_range)), Some((phase, phase_range)))
            if dialect == "built" || dialect == "analysis" && phase == "optimized" =>
        {
            failed = true;
            // alloc: the error's message
            let mut diag = cx.err(None, format!("the {dialect} dialect is not compatible with the {phase} phase"), cx.attr_range());
            diag.span_label(cx.span(*dialect_range), "this dialect...");
            diag.span_label(cx.span(phase_range), "... is not compatible with this phase");
            cx.emit_err(diag);
        }
        _ => {}
    }
    (!failed).then_some(Parsed::Other("CustomMir"))
}

#[cfg(test)]
mod tests {
    use super::PATHS;

    #[test]
    fn paths_are_sorted_and_distinct() {
        assert!(PATHS.windows(2).all(|it| it[0] < it[1]));
    }
}
