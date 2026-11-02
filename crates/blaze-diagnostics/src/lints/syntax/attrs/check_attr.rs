//! What rustc's attribute checks (`check_mod_attrs`, `CheckAttrVisitor`) lint, for each node in
//! the order `check_attributes` gives them: each attribute in turn (those rustc's attribute
//! parser leaves unparsed, as lint attributes are, come before the parsed ones), then the
//! `#[repr]` hints together, then `#[no_mangle]` with `#[export_name]`.
//!
//! - `check_unused_attribute` (`unused_attributes`, "unused attribute"): `#[allow()]` and the
//!   other lint attributes with an empty list or with only a `reason`, `dead_code_pub_in_binary`
//!   at the root of a library, `linker_messages`/`linker_info` at the root of a crate that is
//!   not linked, `default_method_body_is_const`; and, below the crate root, `linker_messages`/
//!   `linker_info` as "crate-level attribute should be an inner attribute" / "... in the root
//!   module". The two that ask for the crate types are not given where the front end does not
//!   know them (a cargo library's or example's `crate-type`).
//! - `check_inline` (`unused_attributes`): `#[inline]` on a function, inherent method or
//!   provided trait method that `#[no_mangle]`, `#[export_name]`, `#[linkage]`,
//!   `#[rustc_std_internal_symbol]` or a weak lang item exports. A closure is never linted: it
//!   is no HIR owner there. An implementation of an externally implementable item (the
//!   unstable `extern_item_impls`) is exported too; the syntax does not say which functions
//!   are, and those are not linted.
//! - `check_link` (`unused_attributes`): `#[link]` on an `extern "Rust"` block, or one whose ABI
//!   rustc does not know (lowered as `Rust`).
//! - `check_macro_export` (`unused_attributes`): `#[macro_export]` on a `macro` (2.0).
//! - `check_repr` (`conflicting_repr_hints`, deny): two integer hints, `C` with `simd`, or `C`
//!   with an integer on an enum whose variants have no fields, at every hint. rustc gives
//!   this lint the code E0566 (`error[E0566]: conflicting representation hints`); the model
//!   has no code for a lint yet (asked of the parent).
//! - `check_mix_no_mangle_export` (`unused_attributes`).
//!
//! The rest of the file gives errors (`diagnostics-items`' `checks/attrs.rs`):
//! `check_non_exhaustive`, `check_track_caller`, `check_proc_macro`, `check_linkage`,
//! `check_optimize_and_inline`, `check_rustc_force_inline`, `check_naked`, `check_may_dangle`,
//! the errors of `check_repr`, ..; `check_doc_*` is the `doc` sibling's, `check_diagnostic_*` the
//! `diagnostic` sibling's, `check_deprecated` (`useless_deprecated`) `diagnostics-lints-names`'.
//! `#[must_use]`, `#[inline]` and the rest where they do nothing are the attribute parser's
//! target check in this nightly (`parse`).
//!
//! Follows rustc's `compiler/rustc_passes/src/check_attr.rs` and `diagnostics.rs` (the text),
//! `compiler/rustc_attr_parsing/src/attributes/{repr,inline,link_attrs,codegen_attrs,
//! macro_attrs}.rs` (which attributes are parsed, and to what), `compiler/rustc_attr_parsing/
//! src/interface.rs` (`parse_attribute_list`: the order of a node's attributes),
//! `compiler/rustc_codegen_ssa/src/codegen_attrs.rs` and `compiler/rustc_middle/src/middle/
//! codegen_fn_attrs.rs` (`contains_extern_indicator`), `compiler/rustc_abi/src/extern_abi.rs`
//! (the ABIs) and `compiler/rustc_interface/src/passes.rs` (`collect_crate_types`) as of the July
//! 2026 checkout; nothing of them is known to differ in `nightly-2026-10-27` (MIT or
//! Apache-2.0).

mod meta;

use std::borrow::Cow;

use base_db::TargetKind;
use hir::InFile;
use smallvec::SmallVec;
use span::Edition;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode, AstToken},
};

use self::meta::{Inner, ItemArgs};
use super::super::{Emitted, Walk, source_fact};
use super::{Args, Attr, Owner, Target, checked, target::MethodKind};
use crate::{
    cx::Cx,
    model::{Applicability, Diag, Span},
};

/// The lints `check_mod_attrs` gives of the attributes of `owners`, each node's in rustc's
/// order.
pub(crate) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>, owners: &[Owner<'_>]) -> Vec<Emitted> {
    let types = CrateTypes::of(cx, owners);
    // alloc: empty, allocates nothing until a lint
    let mut emitted = Vec::new();
    // alloc: one buffer for the crate, cleared for each owner
    let mut words: Vec<Option<&str>> = Vec::new();
    // A macro call is expanded before HIR: `check_mod_attrs` never sees its attributes.
    for owner in owners.iter().filter(|it| it.target != Target::MacroCall) {
        words.clear();
        words.extend(owner.attrs.iter().map(word));
        let node = Node { owner, words: &words };
        let mut emit = |mut diag: Diag| {
            source_fact(cx, &mut diag, owner.at());
            emitted.push(checked(diag, owner));
        };
        for (attr, word) in owner.attrs.iter().zip(&words) {
            if let Some(word) = word
                && let Some(diag) = unused(cx, owner, attr, word, types)
            {
                emit(diag);
            }
        }
        let parsed = match owner.target {
            Target::ForeignMod => link(cx, &node),
            Target::MacroDef => macro_export(cx, &node),
            _ => inline(cx, &node),
        };
        if let Some(diag) = parsed {
            emit(diag);
        }
        if let Some(diag) = repr(cx, walk, &node) {
            emit(diag);
        }
        if let Some(diag) = no_mangle_with_export_name(cx, &node) {
            emit(diag);
        }
    }
    emitted
}

/// A node being checked, with the one-word path of each of its attributes.
struct Node<'a> {
    owner: &'a Owner<'a>,
    words: &'a [Option<&'a str>],
}

impl<'a> Node<'a> {
    /// Its attributes whose path is the one word `name`, in order.
    fn named(&self, name: &'a str) -> impl Iterator<Item = &'a Attr<'a>> + 'a {
        self.owner.attrs.iter().zip(self.words).filter(move |(_, word)| **word == Some(name)).map(|(attr, _)| attr)
    }
}

/// An attribute's path when it is one word (rustc's `name()` of a one-segment path): the
/// text of that word's token.
fn word<'t>(attr: &Attr<'t>) -> Option<&'t str> {
    let path = attr.meta()?.path()?;
    if path.segment()?.coloncolon_token().is_some() {
        return None;
    }
    Some(path.as_single_name_ref()?.syntax().first_token()?.text())
}

/// `attr`'s span, as rustc's `attr.span()`.
fn span_of(cx: &Cx<'_, '_>, attr: &Attr<'_>) -> Span {
    cx.span(attr.in_file(attr.range()))
}

/// The crate types rustc builds the crate as, as far as `check_attr` asks: whether one is an
/// executable, and whether one is linked (anything but an rlib or a staticlib); `None` where
/// the front end does not know (the `crate-type` of a cargo library or example), and neither
/// lint that asks is given.
#[derive(Clone, Copy)]
struct CrateTypes {
    executable: Option<bool>,
    needs_link: Option<bool>,
}

impl CrateTypes {
    const BIN: CrateTypes = CrateTypes { executable: Some(true), needs_link: Some(true) };

    /// rustc's `collect_crate_types`: `--test` builds an executable; else the `--crate-type`
    /// cargo gives for the target, else (a crate rustc is run on alone) the `#![crate_type]`s
    /// of the crate, else `bin`.
    fn of(cx: &Cx<'_, '_>, owners: &[Owner<'_>]) -> CrateTypes {
        match cx.krate.target_kind(cx.db) {
            TargetKind::Bin | TargetKind::Test | TargetKind::Bench | TargetKind::BuildScript => CrateTypes::BIN,
            TargetKind::Lib { is_proc_macro: true } => CrateTypes { executable: Some(false), needs_link: Some(true) },
            // A cargo library is never an executable; its `crate-type` can be an rlib or a
            // cdylib. An example can be a library too.
            TargetKind::Lib { is_proc_macro: false } => CrateTypes { executable: Some(false), needs_link: None },
            TargetKind::Example => CrateTypes { executable: None, needs_link: None },
            TargetKind::Other => CrateTypes::from_attrs(owners),
        }
    }

    fn from_attrs(owners: &[Owner<'_>]) -> CrateTypes {
        let Some(root) = owners.iter().find(|it| it.target == Target::Crate) else { return CrateTypes::BIN };
        let (mut any, mut executable, mut needs_link) = (false, false, false);
        for attr in root.attrs.iter().filter(|it| it.inner && word(it) == Some("crate_type")) {
            let Some(value) = string_value(attr) else { continue };
            let (bin, linked) = match &*value {
                "bin" => (true, true),
                "lib" | "rlib" | "staticlib" => (false, false),
                "dylib" | "cdylib" | "proc-macro" | "sdylib" => (false, true),
                _ => continue,
            };
            any = true;
            executable |= bin;
            needs_link |= linked;
        }
        match any {
            true => CrateTypes { executable: Some(executable), needs_link: Some(needs_link) },
            false => CrateTypes::BIN,
        }
    }
}

/// rustc's `check_unused_attribute` of an attribute it leaves unparsed, whose path is the one
/// word `name`.
fn unused(cx: &Cx<'_, '_>, owner: &Owner<'_>, attr: &Attr<'_>, name: &str, types: CrateTypes) -> Option<Diag> {
    let note = match name {
        "allow" | "expect" | "warn" | "deny" | "forbid" => {
            let Args::List(tt) = attr.args() else { return None };
            let list = meta::list(tt)?;
            let lints_named = |names: &[&str]| {
                list.iter().any(|it| it.item().is_some_and(|it| names.iter().any(|name| it.is(name))))
            };
            if list.is_empty() {
                // alloc: the diagnostic's text, as is each note below
                format!("attribute `{name}` with an empty list has no effect")
            } else if let [Inner::Item(item)] = list.as_slice()
                && matches!(item.args, ItemArgs::NameValue(_))
                && item.is("reason")
            {
                // alloc: as above
                format!("attribute `{name}` without any lints has no effect")
            } else if lints_named(&["linker_messages", "linker_info"]) {
                if owner.target != Target::Crate {
                    return Some(crate_level(cx, attr));
                }
                if types.needs_link != Some(false) {
                    return None;
                }
                // alloc: as above
                "the `linker_messages` and `linker_info` lints can only be controlled at the root of a crate that needs to be linked".to_owned()
            } else if owner.target == Target::Crate
                && lints_named(&["dead_code_pub_in_binary"])
                && types.executable == Some(false)
            {
                // alloc: as above
                "the `dead_code_pub_in_binary` lint has no effect in library crates".to_owned()
            } else {
                return None;
            }
        }
        "default_method_body_is_const" => {
            // alloc: as above
            "`default_method_body_is_const` has been replaced with `const` on traits".to_owned()
        }
        _ => return None,
    };
    let span = span_of(cx, attr);
    // clone: the diagnostic's span
    let mut diag = Diag::lint("unused_attributes", "unused attribute", span.clone());
    diag.suggestion(span, "remove this attribute", "", Applicability::MachineApplicable);
    diag.note(note);
    Some(diag)
}

/// `linker_messages` or `linker_info` set below the crate root: rustc's `OuterCrateLevelAttr`
/// with its `!` to add, or `InnerCrateLevelAttr`.
fn crate_level(cx: &Cx<'_, '_>, attr: &Attr<'_>) -> Diag {
    let span = span_of(cx, attr);
    if attr.inner {
        return Diag::lint("unused_attributes", "crate-level attribute should be in the root module", span);
    }
    let bang = until_bracket(cx, &span).shrink_to_hi();
    let mut diag = Diag::lint("unused_attributes", "crate-level attribute should be an inner attribute", span);
    // alloc: the diagnostic's text
    diag.multipart_suggestion("add a `!`", vec![(bang, "!".to_owned())], Applicability::Unspecified);
    diag
}

/// rustc's `span_until_char(span, '[')`: up to the first `[`, its white space trimmed; the
/// whole span when that leaves nothing or more than one line.
fn until_bracket(cx: &Cx<'_, '_>, span: &Span) -> Span {
    // clone: the diagnostic's span (`crate_level`)
    let Some(text) = cx.snippet(span) else { return span.clone() };
    let before = text.split('[').next().unwrap_or_default().trim_end();
    if before.is_empty() || before.contains('\n') {
        // clone: as above
        return span.clone();
    }
    span.with_hi(span.lo + before.len() as u32)
}

/// rustc's `check_inline`: `#[inline]`, `#[inline(always)]` on a function, an inherent method
/// or a provided trait method that is exported (a method of a trait impl is not checked).
fn inline(cx: &Cx<'_, '_>, node: &Node<'_>) -> Option<Diag> {
    let functions = matches!(
        node.owner.target,
        Target::Fn | Target::Method(MethodKind::Inherent | MethodKind::Trait { body: true })
    );
    if !functions {
        return None;
    }
    let (attr, never) = node.named("inline").find_map(|it| inline_never(it).map(|never| (it, never)))?;
    if never || !exported(node) {
        return None;
    }
    let mut diag =
        Diag::lint("unused_attributes", "`#[inline]` is ignored on externally exported functions", span_of(cx, attr));
    diag.help("externally exported functions are functions with `#[no_mangle]`, `#[export_name]`, or `#[linkage]`");
    Some(diag)
}

/// rustc's `InlineParser` of one `#[inline]`: whether it is `#[inline(never)]`; `None` for one
/// it rejects (and so keeps the next).
fn inline_never(attr: &Attr<'_>) -> Option<bool> {
    match attr.args() {
        Args::Word => Some(false),
        Args::List(tt) => {
            let list = meta::list(tt)?;
            let [Inner::Item(item)] = list.as_slice() else { return None };
            if !item.is_word() {
                return None;
            }
            match item.word()? {
                "always" => Some(false),
                "never" => Some(true),
                _ => None,
            }
        }
        Args::NameValue(_) => None,
    }
}

/// rustc's `contains_extern_indicator` of a function's codegen attributes: `#[no_mangle]`,
/// `#[rustc_std_internal_symbol]` or a weak lang item (which `codegen_fn_attrs` makes one), a
/// well-formed `#[export_name]`, or a `#[linkage]` that is not `internal` (the first
/// well-formed one is the one kept).
fn exported(node: &Node<'_>) -> bool {
    node.named("no_mangle").next().is_some()
        || node.named("rustc_std_internal_symbol").next().is_some()
        || weak_lang_item(node)
        || node.named("export_name").any(|it| export_name(it).is_some())
        || node.named("linkage").find_map(linkage).is_some_and(|it| it != "internal")
}

/// Whether the lang item rustc's `find_attr!(attrs, Lang(..))` finds is a weak one
/// (`panic_impl`, `eh_personality`: those with a `link_name`). `#[panic_handler]` is
/// `panic_impl`; of it and `#[lang = ".."]`, the parsed attributes are in the order each is
/// first written. Of several `#[lang]`s the first with a string is taken: one naming no lang
/// item is an error (E0522) that would leave the next.
fn weak_lang_item(node: &Node<'_>) -> bool {
    let first_of = |name: &'static str| node.words.iter().position(|it| *it == Some(name));
    let lang = node.named("lang").find_map(string_value);
    let weak = |lang: &str| matches!(lang, "panic_impl" | "eh_personality");
    match (lang, first_of("panic_handler")) {
        (Some(lang), Some(panic_handler)) => first_of("lang").is_none_or(|it| it > panic_handler) || weak(&lang),
        (Some(lang), None) => weak(&lang),
        (None, panic_handler) => panic_handler.is_some(),
    }
}

/// The name of a well-formed `#[export_name = ".."]`: rustc's `ExportNameParser` takes a string,
/// neither empty nor with a NUL in it.
fn export_name<'t>(attr: &Attr<'t>) -> Option<Cow<'t, str>> {
    string_value(attr).filter(|it| !it.is_empty() && !it.contains('\0'))
}

/// The linkage of a well-formed `#[linkage = ".."]` (rustc's `LinkageParser`).
fn linkage<'t>(attr: &Attr<'t>) -> Option<Cow<'t, str>> {
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
    string_value(attr).filter(|it| LINKAGES.contains(&&**it))
}

/// The text of `#[path = ".."]`, unescaped, as rustc's `expect_string_literal` takes it.
fn string_value<'t>(attr: &Attr<'t>) -> Option<Cow<'t, str>> {
    let Args::NameValue(Some(ast::Expr::Literal(lit))) = attr.args() else { return None };
    let ast::LiteralKind::String(text) = lit.kind() else { return None };
    text.value().ok()
}

/// rustc's `check_link`: `#[link]` (any of them; the lint is at the first) on an `extern`
/// block whose ABI is `Rust`.
fn link(cx: &Cx<'_, '_>, node: &Node<'_>) -> Option<Diag> {
    let attr = node.named("link").next()?;
    let block = ast::ExternBlock::cast(node.owner.node.value)?;
    // Without a string the ABI is `C`; a string rustc does not know is lowered as `Rust`.
    let abi = block.abi()?.abi_string()?;
    if abi.value().is_ok_and(|it| &*it != "Rust" && ABIS.contains(&&*it)) {
        return None;
    }
    let mut diag =
        Diag::lint("unused_attributes", "attribute should be applied to an `extern` block with non-Rust ABI", span_of(cx, attr));
    diag.warn("this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!");
    Some(diag)
}

/// Every ABI rustc knows (`ExternAbi`'s names).
const ABIS: &[&str] = &[
    "C",
    "C-unwind",
    "Rust",
    "Swift",
    "aapcs",
    "aapcs-unwind",
    "avr-interrupt",
    "avr-non-blocking-interrupt",
    "cdecl",
    "cdecl-unwind",
    "cmse-nonsecure-call",
    "cmse-nonsecure-entry",
    "custom",
    "efiapi",
    "fastcall",
    "fastcall-unwind",
    "gpu-kernel",
    "msp430-interrupt",
    "ptx-kernel",
    "riscv-interrupt-m",
    "riscv-interrupt-s",
    "rust-call",
    "rust-cold",
    "rust-invalid",
    "rust-preserve-none",
    "stdcall",
    "stdcall-unwind",
    "system",
    "system-unwind",
    "sysv64",
    "sysv64-unwind",
    "tail",
    "thiscall",
    "thiscall-unwind",
    "unadjusted",
    "vectorcall",
    "vectorcall-unwind",
    "win64",
    "win64-unwind",
    "x86-interrupt",
];

/// rustc's `check_macro_export`: a well-formed `#[macro_export]` on a `macro` (2.0).
fn macro_export(cx: &Cx<'_, '_>, node: &Node<'_>) -> Option<Diag> {
    if node.owner.node.value.kind() != SyntaxKind::MACRO_DEF {
        return None;
    }
    let attr = node.named("macro_export").find(|it| macro_export_parses(it))?;
    let mut diag =
        Diag::lint("unused_attributes", "`#[macro_export]` has no effect on declarative macro definitions", span_of(cx, attr));
    diag.note("declarative macros follow the same exporting rules as regular items");
    Some(diag)
}

/// Whether rustc's `MacroExportParser` takes `attr`: `#[macro_export]` or
/// `#[macro_export(local_inner_macros)]`.
fn macro_export_parses(attr: &Attr<'_>) -> bool {
    match attr.args() {
        Args::Word => true,
        Args::List(tt) => match meta::list(tt).as_deref() {
            Some([Inner::Item(item)]) => item.is_word() && item.is("local_inner_macros"),
            _ => false,
        },
        Args::NameValue(_) => false,
    }
}

/// What a `#[repr]` hint counts as in `check_repr`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hint {
    Int,
    C,
    Simd,
    /// `Rust`, `transparent`, `align(..)`, `packed`: shown with the rest, conflicting with
    /// none of these.
    Other,
}

/// rustc's `check_repr` lint: the hints of every `#[repr]` of the node, together, conflict.
fn repr(cx: &Cx<'_, '_>, walk: &Walk<'_>, node: &Node<'_>) -> Option<Diag> {
    let mut hints: SmallVec<[(Hint, InFile<TextRange>); 4]> = SmallVec::new();
    for attr in node.named("repr") {
        let Args::List(tt) = attr.args() else { continue };
        let Some(list) = meta::list(tt) else { continue };
        for inner in &list {
            if let Some(item) = inner.item()
                && let Some(hint) = hint(item)
            {
                hints.push((hint, attr.in_file(inner.range())));
            }
        }
    }
    let count = |of: Hint| hints.iter().filter(|(it, _)| *it == of).count();
    let (ints, c, simd) = (count(Hint::Int), count(Hint::C) > 0, count(Hint::Simd) > 0);
    let conflict = ints > 1 || (simd && c) || (ints == 1 && c && c_like_enum(walk, node.owner));
    if !conflict {
        return None;
    }
    let mut spans = hints.iter().map(|(_, at)| cx.span(*at));
    let mut diag = Diag::lint("conflicting_repr_hints", "conflicting representation hints", spans.next()?);
    for span in spans {
        diag.primary(span);
    }
    Some(diag)
}

/// rustc's `parse_repr` of one hint: what it counts as, or `None` for one it rejects.
fn hint(item: &meta::Item<'_>) -> Option<Hint> {
    let word = item.word()?;
    match (word, item.args) {
        ("align", ItemArgs::List(tt)) | ("packed", ItemArgs::List(tt)) => alignment(tt).then_some(Hint::Other),
        ("packed", ItemArgs::Word) => Some(Hint::Other),
        (_, ItemArgs::Word) => match word {
            "C" => Some(Hint::C),
            "simd" => Some(Hint::Simd),
            "Rust" | "transparent" => Some(Hint::Other),
            "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "i128" | "u128" | "isize" | "usize" => {
                Some(Hint::Int)
            }
            _ => None,
        },
        _ => None,
    }
}

/// rustc's `parse_repr_align`: one unsuffixed integer, a power of two of at most 2^29 (below
/// `isize::MAX` on every target of 32 bits or more).
fn alignment(tt: ast::TokenTree<'_>) -> bool {
    let Some(list) = meta::list(tt) else { return false };
    let [Inner::Lit(lit)] = list.as_slice() else { return false };
    let Some(int) = ast::IntNumber::cast(*lit) else { return false };
    int.suffix().is_none() && int.value().is_ok_and(|it| it.is_power_of_two() && it <= 1 << 29)
}

/// rustc's `is_c_like_enum`: an enum none of whose variants (those `cfg` keeps) has fields,
/// not even `()` or `{}`.
fn c_like_enum(walk: &Walk<'_>, owner: &Owner<'_>) -> bool {
    let Some(item) = ast::Enum::cast(owner.node.value) else { return false };
    let Some(variants) = item.variant_list() else { return true };
    variants
        .variants()
        .filter(|it| walk.on(ast::AnyHasAttrs::from(*it)))
        .all(|it| it.field_list().is_none())
}

/// rustc's `check_mix_no_mangle_export`: `#[no_mangle]` with a well-formed `#[export_name]`,
/// each written as its edition writes it.
fn no_mangle_with_export_name(cx: &Cx<'_, '_>, node: &Node<'_>) -> Option<Diag> {
    let no_mangle = node.named("no_mangle").next()?;
    let export = node.named("export_name").find(|it| export_name(it).is_some())?;
    let written = |attr: &Attr<'_>, safe: &'static str, unsafe_: &'static str| match attr.file.edition(cx.db) >= Edition::Edition2024 {
        true => unsafe_,
        false => safe,
    };
    let no_mangle_attr = written(no_mangle, "#[no_mangle]", "#[unsafe(no_mangle)]");
    let export_name_attr = written(export, "#[export_name]", "#[unsafe(export_name)]");
    let no_mangle_span = span_of(cx, no_mangle);
    let mut diag = Diag::lint(
        "unused_attributes",
        // alloc: the diagnostic's text and spans, as is all below
        format!("`{no_mangle_attr}` attribute may not be used in combination with `{export_name_attr}`"),
        // clone: as above
        no_mangle_span.clone(),
    );
    // alloc: as above
    diag.span_label(no_mangle_span.clone(), format!("`{no_mangle_attr}` is ignored"));
    // alloc: as above
    diag.span_note(span_of(cx, export), format!("`{export_name_attr}` takes precedence"));
    diag.suggestion_verbose(
        no_mangle_span,
        // alloc: as above
        format!("remove the `{no_mangle_attr}` attribute"),
        "",
        Applicability::MachineApplicable,
    );
    Some(diag)
}
