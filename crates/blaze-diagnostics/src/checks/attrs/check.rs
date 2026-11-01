//! The errors of `check_mod_attrs` (`CheckAttrVisitor::check_attributes`) of a node's parsed
//! attributes, in its order: an exported `macro_rules` with `#[inline]` first, then each parsed
//! attribute in turn, then the `#[repr]` hints together, `#[rustc_force_inline]` on a
//! coroutine, and `#[optimize(none)]` with `#[inline]`. Its lints are
//! `lints::syntax::attrs::check_attr`'s.
//!
//! - `check_rustc_allow_const_fn_unstable`, `check_dump_object_lifetime_defaults` (whose
//!   `object_lifetime_default` is read from the bounds as `resolve_bound_vars` does),
//!   `check_naked`, `check_track_caller`, `check_non_exhaustive`, `check_may_dangle`,
//!   `check_rustc_legacy_const_generics`, `check_doc_attrs` (`parsers::doc::check_doc`),
//!   `check_rustc_must_implement_one_of`, `check_linkage`.
//! - `check_repr`: E0692 (`transparent` with another hint or with
//!   `#[rustc_pass_indirectly_in_non_rustic_abis]`), E0566 (`Rust` with `C`, `simd` or an
//!   integer); the `conflicting_repr_hints` lint is `check_attr`'s.
//! - `check_rustc_force_inline`, `check_optimize_and_inline`,
//!   `check_non_exported_macro_for_invalid_attrs` (E0518).
//!
//! Not here: `check_proc_macro`'s `ProcMacroBadSig` compares the function's signature with
//! `fn(TokenStream) -> TokenStream` in the trait solver (asked of the front end,
//! `v2/requests/diagnostics.md`); `check_eii_impl` is of the externally implementable items'
//! attribute macros, which no builtin parser here takes. In this checkout E0517 (a hint on
//! what it cannot be applied to) is `repr`'s parser's target check (`parsers::codegen`), and
//! E0084 and E0587 are `rustc_hir_analysis`' (`check/check.rs`), not `check_mod_attrs`'.
//!
//! Follows rustc's `compiler/rustc_passes/src/check_attr.rs` and the texts of
//! `compiler/rustc_passes/src/diagnostics.rs`, `compiler/rustc_hir_analysis/src/collect/
//! resolve_bound_vars.rs` (`object_lifetime_default`), `compiler/rustc_hir/src/
//! weak_lang_items.rs` and `compiler/rustc_abi/src/extern_abi.rs` (`is_rustic_abi`) (MIT or
//! Apache-2.0).

use blaze_hash::HashMap;
use hir::{InFile, LangItem};
use syntax::{
    SyntaxKind, TextRange, WalkEvent,
    view::{
        self as ast, AstNode, AstToken as _, HasAttrs as _, HasGenericArgs as _,
        HasGenericParams as _, HasName as _, HasTypeBounds as _, IsString as _,
    },
};

use super::{
    accept::Session,
    parsed::{Inline, Optimize, Parsed, ParsedAttr, Repr},
    parsers::{Finalize, doc},
};
use crate::{
    cx::{
        Cx, def_range_view as def_range, item_range_view as item_range,
        signature_range_view as signature_range,
    },
    lints::syntax::attrs::{
        Owner, Target,
        target::{GenericParamKind, MethodKind},
    },
    model::Diag,
};

/// The errors `check_attributes` gives of `owner`'s attributes, parsed to `parsed`.
pub(crate) fn check<'db>(
    cx: &Cx<'_, 'db>,
    session: &Session,
    owner: &Owner<'db>,
    parsed: &[ParsedAttr],
    errors: &mut Vec<Diag>,
) {
    let mut fx = Finalize::new(cx, session, owner, errors);
    non_exported_macro(&mut fx, parsed);
    for attr in parsed {
        one(&mut fx, parsed, attr);
    }
    repr(&mut fx, parsed);
    force_inline(&mut fx, parsed);
    optimize_and_inline(&mut fx, parsed);
}

/// `check_one_parsed_attribute`: the errors of one attribute.
fn one(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr], attr: &ParsedAttr) {
    match &attr.parsed {
        Parsed::RustcAllowConstFnUnstable => allow_const_fn_unstable(fx, attr.attr),
        Parsed::RustcDumpObjectLifetimeDefaults => object_lifetime_defaults(fx),
        Parsed::Naked => naked(fx),
        Parsed::TrackCaller => track_caller(fx, parsed, attr.attr),
        Parsed::NonExhaustive => non_exhaustive(fx, attr.attr),
        Parsed::MayDangle => may_dangle(fx, attr.attr),
        Parsed::RustcLegacyConstGenerics(indices) => legacy_const_generics(fx, attr.attr, indices),
        Parsed::Doc(it) => doc::check_doc(fx, it),
        Parsed::RustcMustImplementOneOf(names) => must_implement_one_of(fx, attr.attr, names),
        Parsed::Linkage(_) => linkage(fx, attr.attr),
        Parsed::Other(_)
        | Parsed::Inline(_)
        | Parsed::Optimize(_)
        | Parsed::Repr(_)
        | Parsed::Lang(_)
        | Parsed::ExportName(_)
        | Parsed::NoMangle
        | Parsed::ProcMacro(_)
        | Parsed::RustcPassIndirectlyInNonRusticAbis
        | Parsed::MacroExport
        | Parsed::Stability(_)
        | Parsed::ConstStability(_)
        | Parsed::BodyStability(_)
        | Parsed::RustcConstStableIndirect
        | Parsed::Deprecated(_) => {}
    }
}

/// The node's syntax as `N`, when it is one.
fn node<'db, N: AstNode<'db>>(fx: &Finalize<'_, '_, 'db>) -> Option<N> {
    N::cast(fx.owner.node.value)
}

/// A range of the file the node is in.
fn node_span(fx: &Finalize<'_, '_, '_>, range: TextRange) -> crate::model::Span {
    fx.cx.span(InFile::new(fx.owner.node.file_id, range))
}

/// Whether `node` has the keyword `kind` among its own tokens (`const trait`, `impl const`).
fn has_keyword(node: ast::Node<'_>, kind: SyntaxKind) -> bool {
    node.children_with_tokens().any(|it| it.as_token().is_some_and(|it| it.kind() == kind))
}

/// `check_rustc_allow_const_fn_unstable`: on a function that is not a `const fn`.
fn allow_const_fn_unstable(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    if !matches!(fx.target(), Target::Fn | Target::Method(_)) {
        return;
    }
    let Some(function) = node::<ast::Fn<'_>>(fx) else { return };
    if is_const_fn(function) {
        return;
    }
    let mut diag = Diag::error(None, "attribute should be applied to `const fn`", fx.attr_span(attr));
    diag.span_label(fx.target_span(), "not a `const fn`");
    fx.emit_err(diag);
}

/// rustc's `is_const_fn`: written `const`, or a method of a `const trait` or of a const impl.
fn is_const_fn(function: ast::Fn<'_>) -> bool {
    if function.const_token().is_some() {
        return true;
    }
    let Some(container) = function.syntax().parent().and_then(|it| it.parent()) else { return false };
    match container.kind() {
        SyntaxKind::TRAIT | SyntaxKind::IMPL => has_keyword(container, SyntaxKind::CONST_KW),
        _ => false,
    }
}

/// rustc's `ObjectLifetimeDefault` of one type parameter, as the dump prints it.
#[derive(Clone, PartialEq, Eq)]
enum ObjectLifetimeDefault {
    Empty,
    Static,
    Param(String),
    Ambiguous,
}

impl ObjectLifetimeDefault {
    fn text(&self) -> &str {
        match self {
            ObjectLifetimeDefault::Empty => "Empty",
            ObjectLifetimeDefault::Static => "'static",
            ObjectLifetimeDefault::Param(name) => name,
            ObjectLifetimeDefault::Ambiguous => "Ambiguous",
        }
    }
}

/// `check_dump_object_lifetime_defaults`: an error naming each of the node's own type
/// parameters' object lifetime default, at the parameter (a trait's `Self` first, at the trait;
/// a function's `impl Trait` arguments last).
fn object_lifetime_defaults(fx: &mut Finalize<'_, '_, '_>) {
    let item = fx.owner.node.value;
    let Some(generic) = ast::AnyHasGenericParams::cast(item) else { return };
    let predicates: Vec<ast::WherePred<'_>> =
        // alloc: only for `#[rustc_dump_object_lifetime_defaults]`, always an error
        generic.where_clause().map(|it| it.predicates().collect()).unwrap_or_default();
    // alloc: as above
    let mut found: Vec<(TextRange, ObjectLifetimeDefault)> = Vec::new();
    if let Some(trait_) = ast::Trait::cast(item) {
        let mut set = LifetimeSet::Empty;
        set.add(trait_.type_bound_list());
        for &predicate in predicates.iter().filter(|it| bounds_param(**it, "Self")) {
            set.add(predicate.type_bound_list());
        }
        found.push((def_range(item), set.default()));
    }
    for param in generic.generic_param_list().into_iter().flat_map(|it| it.generic_params()) {
        let ast::GenericParam::TypeParam(param) = param else { continue };
        let Some(name) = param.name() else { continue };
        let mut set = LifetimeSet::Empty;
        set.add(param.type_bound_list());
        for &predicate in predicates.iter().filter(|it| bounds_param(**it, name.text())) {
            set.add(predicate.type_bound_list());
        }
        found.push((name.syntax().text_range(), set.default()));
    }
    if let Some(function) = ast::Fn::cast(item) {
        for param in function.param_list().into_iter().flat_map(|it| it.params()) {
            let Some(ty) = param.ty() else { continue };
            for synthetic in ty.syntax().descendants().filter(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE) {
                found.push((synthetic.text_range(), ObjectLifetimeDefault::Empty));
            }
        }
    }
    for (range, default) in found {
        let diag = Diag::error(None, default.text(), node_span(fx, range));
        fx.emit_err(diag);
    }
}

/// Whether a `where` predicate bounds exactly the parameter `name`, without `for<..>`.
fn bounds_param(predicate: ast::WherePred<'_>, name: &str) -> bool {
    if predicate.for_binder().is_some() {
        return false;
    }
    let Some(ast::Type::PathType(path)) = predicate.ty() else { return false };
    let Some(path) = path.path() else { return false };
    path.qualifier().is_none()
        && path.segment().is_some_and(|it| it.generic_arg_list().is_none() && it.syntax().text() == name)
}

/// rustc's `Set1` of the lifetimes a type parameter outlives.
enum LifetimeSet<'t> {
    Empty,
    One(&'t str),
    Many,
}

impl<'t> LifetimeSet<'t> {
    fn add(&mut self, bounds: Option<ast::TypeBoundList<'t>>) {
        for bound in bounds.into_iter().flat_map(|it| it.bounds()) {
            let Some(lifetime) = bound.lifetime() else { continue };
            let name = lifetime.syntax().text();
            *self = match *self {
                LifetimeSet::Empty => LifetimeSet::One(name),
                LifetimeSet::One(it) if it == name => LifetimeSet::One(it),
                _ => LifetimeSet::Many,
            };
        }
    }

    fn default(self) -> ObjectLifetimeDefault {
        match self {
            LifetimeSet::Empty => ObjectLifetimeDefault::Empty,
            LifetimeSet::One("'static") => ObjectLifetimeDefault::Static,
            LifetimeSet::One("'_") => ObjectLifetimeDefault::Ambiguous,
            // alloc: only for `#[rustc_dump_object_lifetime_defaults]`, always an error
            LifetimeSet::One(it) => ObjectLifetimeDefault::Param(it.to_owned()),
            LifetimeSet::Many => ObjectLifetimeDefault::Ambiguous,
        }
    }
}

/// `check_naked`: `#[unsafe(naked)]` on a function of a Rust ABI, unstable.
fn naked(fx: &mut Finalize<'_, '_, '_>) {
    if !matches!(
        fx.target(),
        Target::Fn | Target::Method(MethodKind::Trait { body: true } | MethodKind::Inherent)
    ) {
        return;
    }
    let Some(function) = node::<ast::Fn<'_>>(fx) else { return };
    // The function's ABI as rustc names it: `Rust` without `extern`, `C` for `extern` alone.
    let string = function.abi().map(|abi| abi.string_token().and_then(ast::String::cast));
    let abi = match string {
        None => std::borrow::Cow::Borrowed("Rust"),
        Some(None) => std::borrow::Cow::Borrowed("C"),
        Some(Some(it)) => it.value().unwrap_or_else(|_| std::borrow::Cow::Borrowed(it.text_without_quotes())),
    };
    if !matches!(&*abi, "Rust" | "rust-call" | "rust-cold" | "rust-preserve-none" | "tail") {
        return;
    }
    let span = node_span(fx, signature_range(function));
    // alloc: only for a naked function of a Rust ABI
    fx.gate("naked_functions_rustic_abi", span, format!("`#[naked]` is currently unstable on `extern \"{abi}\"` functions"));
}

/// The weak lang items (`weak_lang_items!`): called through `extern` declarations.
const WEAK_LANG_ITEMS: &[&str] = &["panic_impl", "eh_personality"];

/// `check_track_caller`: `#[track_caller]` on a weak lang item.
fn track_caller(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr], attr: usize) {
    if fx.target() != Target::Fn {
        return;
    }
    let Some(name) = parsed.iter().find_map(|it| match &it.parsed {
        Parsed::Lang(name) => Some(*name),
        _ => None,
    }) else {
        return;
    };
    if !WEAK_LANG_ITEMS.contains(&name) {
        return;
    }
    let Some(function) = node::<ast::Fn<'_>>(fx) else { return };
    let what = match name {
        // alloc: the error's message
        "panic_impl" => "`#[panic_handler]`".to_owned(),
        // alloc: the error's message
        other => format!("`{other}` lang item"),
    };
    // alloc: the error's message
    let message = format!("{what} function is not allowed to have `#[track_caller]`");
    // clone: the error's message
    let mut diag = Diag::error(None, message.clone(), fx.attr_span(attr));
    diag.span_label(node_span(fx, signature_range(function)), message);
    fx.emit_err(diag);
}

/// `check_non_exhaustive`: on a struct with default field values.
fn non_exhaustive(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    if fx.target() != Target::Struct {
        return;
    }
    let Some(ast::FieldList::RecordFieldList(fields)) = node::<ast::Struct<'_>>(fx).and_then(|it| it.field_list()) else {
        return;
    };
    if !fields.fields().any(|it| it.default_val().is_some()) {
        return;
    }
    let mut diag = Diag::error(
        None,
        "`#[non_exhaustive]` can't be used to annotate items with default field values",
        fx.attr_span(attr),
    );
    diag.span_label(fx.target_span(), "this struct has default field values");
    fx.emit_err(diag);
}

/// `check_may_dangle`: on anything but a lifetime or type parameter of an `impl Drop`.
fn may_dangle(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    let Target::GenericParam { kind: GenericParamKind::Lifetime | GenericParamKind::Type, .. } = fx.target() else {
        return;
    };
    let in_drop_impl = || {
        let list = fx.owner.node.value.parent().filter(|it| it.kind() == SyntaxKind::GENERIC_PARAM_LIST)?;
        let impl_ = list.parent().and_then(ast::Impl::cast)?;
        let trait_ = fx.cx.vsema.to_def(impl_)?.trait_(fx.cx.db)?;
        Some(hir::Trait::lang(fx.cx.db, fx.cx.krate, LangItem::Drop)? == trait_)
    };
    if in_drop_impl() == Some(true) {
        return;
    }
    let diag = Diag::error(
        None,
        "`#[may_dangle]` must be applied to a lifetime or type generic parameter in `Drop` impl",
        fx.attr_span(attr),
    );
    fx.emit_err(diag);
}

/// `check_rustc_legacy_const_generics`, of a function item.
fn legacy_const_generics(fx: &mut Finalize<'_, '_, '_>, attr: usize, indices: &[(usize, TextRange)]) {
    if fx.target() != Target::Fn {
        return;
    }
    let Some(function) = node::<ast::Fn<'_>>(fx) else { return };
    // The generic parameters, the `impl Trait` arguments last: how many, and the first that is
    // not a const's.
    let (mut params, mut non_const) = (0, None);
    let mut param = |is_const: bool, range: TextRange| {
        params += 1;
        if !is_const && non_const.is_none() {
            non_const = Some(range);
        }
    };
    for it in function.generic_param_list().into_iter().flat_map(|it| it.generic_params()) {
        param(matches!(it, ast::GenericParam::ConstParam(_)), it.syntax().text_range());
    }
    let mut inputs = 0;
    for input in function.param_list().into_iter().flat_map(|it| it.params()) {
        inputs += 1;
        let Some(ty) = input.ty() else { continue };
        for it in ty.syntax().descendants().filter(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE) {
            param(false, it.text_range());
        }
    }
    if let Some(range) = non_const {
        let mut diag = Diag::error(None, "#[rustc_legacy_const_generics] functions must only have const generics", fx.attr_span(attr));
        diag.span_label(node_span(fx, range), "non-const generic parameter");
        fx.emit_err(diag);
        return;
    }
    if indices.len() != params {
        let generics = match function.generic_param_list() {
            Some(list) => list.syntax().text_range(),
            None => {
                let end = function.name().map_or(function.syntax().text_range().start(), |it| it.syntax().text_range().end());
                TextRange::empty(end)
            }
        };
        let mut diag = Diag::error(
            None,
            "#[rustc_legacy_const_generics] must have one index for each generic parameter",
            fx.attr_span(attr),
        );
        diag.span_label(node_span(fx, generics), "generic parameters");
        fx.emit_err(diag);
        return;
    }
    let self_param = function.param_list().and_then(|it| it.self_param()).is_some();
    let arg_count = inputs + usize::from(self_param) + params;
    for &(index, range) in indices {
        if index < arg_count {
            continue;
        }
        let span = fx.span(attr, range);
        let label = match arg_count {
            // alloc: the error's label
            1 => "there is only 1 argument".to_owned(),
            // alloc: the error's label
            n => format!("there are only {n} arguments"),
        };
        // clone: the error's span
        let mut diag = Diag::error(None, "index exceeds number of arguments", span.clone());
        diag.span_label(span, label);
        fx.emit_err(diag);
    }
}

/// An associated item of a trait, as `check_rustc_must_implement_one_of` looks at it.
struct AssocItem<'t> {
    name: &'t str,
    /// rustc's `def_span`.
    range: TextRange,
    /// A function, with whether it has a default body.
    function: Option<bool>,
}

fn assoc_items(trait_: ast::Trait<'_>) -> Vec<AssocItem<'_>> {
    // alloc: only for `#[rustc_must_implement_one_of]`
    let mut items = Vec::new();
    for item in trait_.assoc_item_list().into_iter().flat_map(|it| it.assoc_items()) {
        let (name, range, function) = match item {
            ast::AssocItem::Fn(it) => (it.name(), signature_range(it), Some(it.body().is_some())),
            ast::AssocItem::Const(it) => {
                let start = item_range(it.syntax()).start();
                let end = it.ty().map(|ty| ty.syntax().text_range().end()).or_else(|| it.name().map(|n| n.syntax().text_range().end()));
                (it.name(), TextRange::new(start, end.unwrap_or(start)), None)
            }
            ast::AssocItem::TypeAlias(it) => {
                let start = item_range(it.syntax()).start();
                let end = it
                    .generic_param_list()
                    .map(|g| g.syntax().text_range().end())
                    .or_else(|| it.name().map(|n| n.syntax().text_range().end()));
                (it.name(), TextRange::new(start, end.unwrap_or(start)), None)
            }
            ast::AssocItem::MacroCall(_) => continue,
        };
        let Some(name) = name else { continue };
        items.push(AssocItem { name: name.text(), range, function });
    }
    items
}

/// `check_rustc_must_implement_one_of`: each name must be a function of the trait with a
/// default body, named once.
fn must_implement_one_of(fx: &mut Finalize<'_, '_, '_>, attr: usize, names: &[(String, TextRange)]) {
    if fx.target() != Target::Trait {
        return;
    }
    let Some(trait_) = node::<ast::Trait<'_>>(fx) else { return };
    let items = assoc_items(trait_);
    let attr_span = fx.attr_span(attr);
    for (name, range) in names {
        match items.iter().find(|it| it.name == name.as_str()) {
            Some(AssocItem { function: Some(true), .. }) => {}
            Some(AssocItem { function: Some(false), range, .. }) => {
                let mut diag = Diag::error(None, "function doesn't have a default implementation", node_span(fx, *range));
                // clone: the error's span
                diag.span_note(attr_span.clone(), "required by this annotation");
                fx.emit_err(diag);
            }
            Some(AssocItem { function: None, range, .. }) => {
                let mut diag = Diag::error(None, "not a function", node_span(fx, *range));
                // clone: the error's span
                diag.span_note(attr_span.clone(), "required by this annotation");
                diag.note("all `#[rustc_must_implement_one_of]` arguments must be associated function names");
                fx.emit_err(diag);
            }
            None => {
                let diag = Diag::error(None, "function not found in this trait", fx.span(attr, *range));
                fx.emit_err(diag);
            }
        }
    }
    let mut seen: HashMap<&str, TextRange> = HashMap::default();
    for (name, range) in names {
        if let Some(dup) = seen.insert(name.as_str(), *range) {
            let mut diag = Diag::error(None, "functions names are duplicated", fx.span(attr, dup));
            diag.primary(fx.span(attr, *range));
            diag.note("all `#[rustc_must_implement_one_of]` arguments must be unique");
            fx.emit_err(diag);
        }
    }
}

/// `check_linkage`: on an extern `static mut`, or a `const fn`.
fn linkage(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    match fx.target() {
        Target::ForeignStatic if node::<ast::Static<'_>>(fx).is_some_and(|it| it.mut_token().is_some()) => {
            let mut diag = Diag::error(None, "extern mutable statics are incompatible with the `linkage` attribute", fx.attr_span(attr));
            diag.note(
                "the `linkage` attribute on extern statics generates a symbol that contains the address of another static. Making the extern static mutable would allow changing the address, rather than the static the address is pointing to",
            );
            fx.emit_err(diag);
        }
        Target::Fn if node::<ast::Fn<'_>>(fx).is_some_and(|it| it.const_token().is_some()) => {
            let mut diag = Diag::error(None, "`const fn` are incompatible with the `linkage` attribute", fx.attr_span(attr));
            diag.note("`const fn` may be called at compile time, which happens before linking");
            fx.emit_err(diag);
        }
        _ => {}
    }
}

/// The first parsed attribute that is `what` makes of it, with its index.
fn find<'p, T>(parsed: &'p [ParsedAttr], what: impl Fn(&'p Parsed) -> Option<T>) -> Option<(T, usize)> {
    parsed.iter().find_map(|it| what(&it.parsed).map(|found| (found, it.attr)))
}

/// `check_repr`'s errors.
fn repr(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    let Some((reprs, first)) = find(parsed, |it| match it {
        Parsed::Repr(reprs) => Some(reprs.as_slice()),
        _ => None,
    }) else {
        return;
    };
    let mut ints = 0;
    let (mut rust, mut c, mut simd, mut transparent) = (false, false, false, false);
    for (repr, _) in reprs {
        match repr {
            Repr::Rust => rust = true,
            Repr::C => c = true,
            Repr::Simd => simd = true,
            Repr::Transparent => transparent = true,
            Repr::Int(_) => ints += 1,
            Repr::Align(_) | Repr::Packed(_) => {}
        }
    }
    let hints = |fx: &Finalize<'_, '_, '_>| -> Vec<crate::model::Span> {
        // alloc: the error's spans
        reprs.iter().map(|(_, range)| fx.span(first, *range)).collect()
    };
    let target = fx.target().name();
    if transparent && reprs.len() > 1 {
        // alloc: the error's message
        let diag = multi(Some("E0692"), format!("transparent {target} cannot have other repr hints"), hints(fx));
        fx.emit_err(diag);
    }
    if transparent
        && let Some(((), pass)) = find(parsed, |it| matches!(it, Parsed::RustcPassIndirectlyInNonRusticAbis).then_some(()))
    {
        // alloc: the error's spans
        let spans = vec![fx.target_span(), fx.attr_span(pass)];
        // alloc: the error's message
        let diag = multi(Some("E0692"), format!("transparent {target} cannot have other repr hints"), spans);
        fx.emit_err(diag);
    }
    if rust && (ints > 0 || c || simd) {
        let diag = multi(Some("E0566"), "conflicting representation hints", hints(fx));
        fx.emit_err(diag);
    }
}

/// An error whose primary spans are `spans`.
fn multi(code: Option<&str>, message: impl Into<String>, spans: Vec<crate::model::Span>) -> Diag {
    let mut spans = spans.into_iter();
    let Some(first) = spans.next() else { unreachable!("a repr error has a span") };
    let mut diag = Diag::error(code, message, first);
    for span in spans {
        diag.primary(span);
    }
    diag
}

/// `check_rustc_force_inline`: the coroutines of a function with `#[rustc_force_inline]` (an
/// `async`, `gen` or `async gen` function's body, and each such block or closure in it), each
/// at the function.
fn force_inline(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    let Some(((), attr)) = find(parsed, |it| matches!(it, Parsed::Inline(Inline::Force)).then_some(())) else { return };
    let Some(function) = node::<ast::Fn<'_>>(fx) else { return };
    let mut coroutines = usize::from(function.async_token().is_some() || function.gen_token().is_some());
    if let Some(body) = function.body() {
        let mut events = body.syntax().preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if node != body.syntax() && ast::Item::can_cast(node.kind()) {
                events.skip_subtree();
                continue;
            }
            if ast::BlockExpr::can_cast(node.kind()) {
                let block = ast::BlockExpr::cast(node).expect("a block");
                if block.async_token().is_some() || block.gen_token().is_some() {
                    coroutines += 1;
                }
            } else if let Some(closure) = ast::ClosureExpr::cast(node)
                && (closure.async_token().is_some() || closure.gen_token().is_some())
            {
                if closure.attrs().any(|it| it.simple_name() == Some("rustc_force_inline")) {
                    continue;
                }
                coroutines += 1;
            }
        }
    }
    let span = node_span(fx, signature_range(function));
    for _ in 0..coroutines {
        let mut diag =
            Diag::error(None, "attribute cannot be applied to a `async`, `gen` or `async gen` function", fx.attr_span(attr));
        // clone: the error's span
        diag.span_label(span.clone(), "`async`, `gen` or `async gen` function");
        fx.emit_err(diag);
    }
}

/// `check_optimize_and_inline`: `#[optimize(none)]` with an `#[inline]` that is not `never`.
fn optimize_and_inline(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    let Some(((), optimize)) = find(parsed, |it| matches!(it, Parsed::Optimize(Optimize::DoNotOptimize)).then_some(()))
    else {
        return;
    };
    let Some((inline, at)) = find(parsed, |it| match it {
        Parsed::Inline(kind) => Some(*kind),
        _ => None,
    }) else {
        return;
    };
    if inline == Inline::Never {
        return;
    }
    let optimize_span = fx.attr_span(optimize);
    // clone: the error's span
    let mut diag = Diag::error(None, "`#[optimize(none)]` cannot be used with `#[inline]` attributes", optimize_span.clone());
    diag.span_label(optimize_span, "`#[optimize(none)]` here");
    diag.span_label(fx.attr_span(at), "`#[inline]` here");
    fx.emit_err(diag);
}

/// `check_non_exported_macro_for_invalid_attrs`: E0518, `#[inline]` on a `macro_rules` that is
/// not `#[macro_export]`.
fn non_exported_macro(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    if fx.target() != Target::MacroDef || node::<ast::MacroRules<'_>>(fx).is_none() {
        return;
    }
    if parsed.iter().any(|it| matches!(it.parsed, Parsed::MacroExport)) {
        return;
    }
    let Some((_, attr)) = find(parsed, |it| match it {
        Parsed::Inline(kind) if *kind != Inline::Force => Some(()),
        _ => None,
    }) else {
        return;
    };
    let span = fx.attr_span(attr);
    // clone: the error's span
    let mut diag = Diag::error(Some("E0518"), "attribute should be applied to function or closure", span.clone());
    diag.span_label(span, "not a function or closure");
    fx.emit_err(diag);
}

#[cfg(test)]
mod tests {
    use syntax::{
        Parse, SourceFile,
        view::{self as ast, AstNode, HasTypeBounds as _},
    };

    use super::{LifetimeSet, ObjectLifetimeDefault, bounds_param};

    fn parse(text: &str) -> Parse<SourceFile> {
        SourceFile::parse(text, span::Edition::Edition2021)
    }

    fn first<'t, N: AstNode<'t>>(parse: &'t Parse<SourceFile>) -> N {
        parse.root().descendants().find_map(N::cast).unwrap()
    }

    #[test]
    fn one_lifetime_twice_is_that_lifetime() {
        let parse = parse("struct S<'a, T: 'a + 'a>(&'a T);");
        let param: ast::TypeParam<'_> = first(&parse);
        let mut set = LifetimeSet::Empty;
        set.add(param.type_bound_list());
        assert!(set.default() == ObjectLifetimeDefault::Param("'a".to_owned()));
    }

    #[test]
    fn two_lifetimes_are_ambiguous_and_static_is_static() {
        let two = parse("struct S<'a, 'b, T: 'a + 'b>(&'a T);");
        let param: ast::TypeParam<'_> = first(&two);
        let mut set = LifetimeSet::Empty;
        set.add(param.type_bound_list());
        assert!(set.default() == ObjectLifetimeDefault::Ambiguous);
        let one = parse("struct S<T: 'static>(T);");
        let param: ast::TypeParam<'_> = first(&one);
        let mut set = LifetimeSet::Empty;
        set.add(param.type_bound_list());
        assert!(set.default() == ObjectLifetimeDefault::Static);
    }

    #[test]
    fn a_where_predicate_bounds_only_its_own_parameter() {
        let plain = parse("struct S<T>(T) where T: 'static;");
        let pred: ast::WherePred<'_> = first(&plain);
        assert!(bounds_param(pred, "T"));
        assert!(!bounds_param(pred, "U"));
        let binder = parse("struct S<T>(T) where for<'a> T: 'a;");
        let pred: ast::WherePred<'_> = first(&binder);
        assert!(!bounds_param(pred, "T"));
    }

    #[test]
    fn a_traits_items_are_named_in_order_without_its_macro_calls() {
        let text = "trait T { fn a(); fn b() {} const C: u8; type D<X>; m!(); }";
        let parse = parse(text);
        let items = super::assoc_items(first(&parse));
        let names: Vec<_> = items.iter().map(|it| it.name).collect();
        assert_eq!(names, ["a", "b", "C", "D"]);
        let spans: Vec<_> = items.iter().map(|it| &text[it.range]).collect();
        assert_eq!(spans, ["fn a();", "fn b()", "const C: u8", "type D<X>"]);
        let functions: Vec<_> = items.iter().map(|it| it.function).collect();
        assert_eq!(functions, [Some(false), Some(true), None, None]);
    }
}
