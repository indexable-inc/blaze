//! `box_collection`: `Box` of a std collection (`Box<Vec<T>>`, `Box<String>`), which is on the
//! heap already. This file is also clippy's `Types` pass, which checks the types written in
//! signatures, fields, constants and statics, and runs `vec_box`, `rc_buffer` and `rc_mutex` as
//! well.
//!
//! Follows clippy's `clippy_lints/src/types/mod.rs` (`Types::check_ty` and the `check_*` hooks
//! that call it) and `types/box_collection.rs` (MIT or Apache-2.0), ported from what is known of
//! their source at rust-lang/rust's nightly of 2026-10: no reference checkout of clippy was at
//! hand. Where `check_ty` stops at a type another lint of the pass reports (`borrowed_box`,
//! `redundant_allocation`, `option_option`, `linked_list`, `owned_cow`,
//! `type_complexity`), so does this, those lints not being reported here. As in clippy, the
//! paths of a `let`'s type or a closure's signature are not looked at (`in_body`), and with
//! `avoid-breaking-exported-api` (the default) what another crate can name is left alone.

use hir::PathResolution;
use syntax::view::{self as ast, AstNode as _, HasGenericArgs as _, HasTypeBounds as _};

use super::{Implemented, rc_buffer, vec_box};
use crate::check::Cx;

const LINT: Implemented = Implemented::BoxCollection;

/// clippy's `CheckTyContext`.
#[derive(Clone, Copy, Default)]
struct Context {
    is_in_trait_impl: bool,
    is_exported: bool,
    is_nested_call: bool,
}

/// The std types the pass knows, by their lang item or diagnostic item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Std {
    Box,
    String,
    Global,
    Vec,
    VecDeque,
    LinkedList,
    HashMap,
    HashSet,
    BTreeMap,
    BTreeSet,
    BinaryHeap,
    Rc,
    Arc,
    OsString,
    PathBuf,
    Option,
    Mutex,
    Cow,
}

impl Std {
    /// clippy's `get_std_collection`: the collections `box_collection` names.
    fn is_collection(self) -> bool {
        matches!(
            self,
            Std::HashMap
                | Std::String
                | Std::Vec
                | Std::HashSet
                | Std::VecDeque
                | Std::LinkedList
                | Std::BTreeMap
                | Std::BTreeSet
                | Std::BinaryHeap
        )
    }

    /// The diagnostic item's name, as clippy prints it.
    pub(super) fn name(self) -> &'static str {
        match self {
            Std::Box => "Box",
            Std::String => "String",
            Std::Global => "Global",
            Std::Vec => "Vec",
            Std::VecDeque => "VecDeque",
            Std::LinkedList => "LinkedList",
            Std::HashMap => "HashMap",
            Std::HashSet => "HashSet",
            Std::BTreeMap => "BTreeMap",
            Std::BTreeSet => "BTreeSet",
            Std::BinaryHeap => "BinaryHeap",
            Std::Rc => "Rc",
            Std::Arc => "Arc",
            Std::OsString => "OsString",
            Std::PathBuf => "PathBuf",
            Std::Option => "Option",
            Std::Mutex => "Mutex",
            Std::Cow => "Cow",
        }
    }

    const ALL: [Std; 18] = [
        Std::Box,
        Std::String,
        Std::Global,
        Std::Vec,
        Std::VecDeque,
        Std::LinkedList,
        Std::HashMap,
        Std::HashSet,
        Std::BTreeMap,
        Std::BTreeSet,
        Std::BinaryHeap,
        Std::Rc,
        Std::Arc,
        Std::OsString,
        Std::PathBuf,
        Std::Option,
        Std::Mutex,
        Std::Cow,
    ];
}

/// Which of the std types `adt` is: one of a sysroot crate named as one, and that crate's lang
/// item or diagnostic item of that name.
pub(super) fn std_adt(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<Std> {
    let db = cx.db();
    if !adt.module(db).krate(db).origin(db).is_lang() {
        return None;
    }
    let name = adt.name(db);
    let std = Std::ALL.into_iter().find(|it| it.name() == name.as_str())?;
    let lang = match std {
        Std::Box => Some(hir::LangItem::OwnedBox),
        Std::String => Some(hir::LangItem::String),
        Std::Global => Some(hir::LangItem::GlobalAlloc),
        _ => None,
    };
    let found = match lang {
        Some(item) => {
            let items = hir_def::lang_item::lang_items(db, cx.krate().base());
            match item.from_lang_items(items)? {
                hir_def::lang_item::LangItemTarget::StructId(it) => {
                    Some(hir::Adt::from(hir_def::AdtId::StructId(it)))
                }
                _ => None,
            }
        }
        None => cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern(std.name())),
    };
    (found == Some(adt)).then_some(std)
}

/// The std type the path type `ty` names, if it is one.
pub(super) fn std_of<'t>(cx: &Cx<'_, 't>, ty: ast::Type<'t>) -> Option<Std> {
    let ast::Type::PathType(path) = ty else { return None };
    match cx.path(path.path()?)? {
        PathResolution::Def(hir::ModuleDef::Adt(adt)) => std_adt(cx, adt),
        _ => None,
    }
}

/// clippy's `last_path_segment(qpath).args`: the generic arguments of the path's last segment.
pub(super) fn last_args<'t>(path: ast::PathType<'t>) -> Option<ast::GenericArgList<'t>> {
    path.path()?.segment()?.generic_arg_list()
}

/// clippy's `qpath_generic_tys`: the types among the last segment's generic arguments.
pub(super) fn generic_tys<'t>(path: ast::PathType<'t>) -> impl Iterator<Item = ast::Type<'t>> {
    last_args(path).into_iter().flat_map(|it| it.generic_args()).filter_map(|it| match it {
        ast::GenericArg::TypeArg(it) => it.ty(),
        _ => None,
    })
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    let lints = Implemented::BoxCollection.bit()
        | Implemented::VecBox.bit()
        | Implemented::RcBuffer.bit()
        | super::rc_mutex::LINT.bit();
    if !cx.on_any(lints) {
        return;
    }
    let mut types: Vec<ast::Type<'t>> = Vec::new();
    let Some(context) = signature_types(cx, node, &mut types) else { return };
    let mut pass = Pass { cx };
    for ty in types {
        pass.check_ty(ty, context);
    }
}

/// The types clippy's `check_fn`, `check_item`, `check_impl_item`, `check_trait_item` and
/// `check_field_def` give `check_ty` for `node`, and the context they give.
fn signature_types<'t>(
    cx: &Cx<'_, 't>,
    node: ast::Node<'t>,
    types: &mut Vec<ast::Type<'t>>,
) -> Option<Context> {
    let in_trait = |node: ast::Node<'t>| {
        node.parent()
            .and_then(ast::AssocItemList::cast)
            .and_then(|it| it.syntax().parent())
            .is_some_and(|it| ast::Trait::can_cast(it.kind()))
    };
    let in_trait_impl = |node: ast::Node<'t>| {
        node.parent()
            .and_then(ast::AssocItemList::cast)
            .and_then(|it| it.syntax().parent())
            .and_then(ast::Impl::cast)
            .is_some_and(|it| it.trait_().is_some())
    };
    let in_impl = |node: ast::Node<'t>| {
        node.parent()
            .and_then(ast::AssocItemList::cast)
            .and_then(|it| it.syntax().parent())
            .is_some_and(|it| ast::Impl::can_cast(it.kind()))
    };
    let foreign = |node: ast::Node<'t>| {
        node.parent().is_some_and(|it| ast::ExternItemList::can_cast(it.kind()))
    };
    let exported = || {
        cx.conf().bool("avoid-breaking-exported-api", true) && cx.is_exported(node)
    };
    if let Some(function) = ast::Fn::cast(node) {
        if foreign(node) {
            return None;
        }
        // A method of a trait's impl is checked by nothing: its signature is the trait's.
        let is_in_trait_impl = in_trait_impl(node);
        let params = function.param_list()?;
        if let Some(it) = params.self_param().and_then(|it| it.ty()) {
            types.push(it);
        }
        types.extend(params.params().filter_map(|it| it.ty()));
        types.extend(function.ret_type().and_then(|it| it.ty()));
        if types.is_empty() {
            return None;
        }
        return Some(Context { is_in_trait_impl, is_exported: exported(), ..Context::default() });
    }
    if let Some(field) = ast::RecordField::cast(node) {
        types.extend(field.ty());
    } else if let Some(field) = ast::TupleField::cast(node) {
        types.extend(field.ty());
    } else if let Some(konst) = ast::Const::cast(node) {
        types.extend(konst.ty());
        if in_impl(node) {
            // `check_impl_item`: the context has no `is_exported`.
            return (!types.is_empty())
                .then(|| Context { is_in_trait_impl: in_trait_impl(node), ..Context::default() });
        }
    } else if let Some(statik) = ast::Static::cast(node) {
        if foreign(node) {
            return None;
        }
        types.extend(statik.ty());
    } else if let Some(alias) = ast::TypeAlias::cast(node) {
        // Only a trait's associated type with a default is checked.
        if !in_trait(node) {
            return None;
        }
        types.extend(alias.ty());
    } else {
        return None;
    }
    if types.is_empty() {
        return None;
    }
    Some(Context { is_exported: exported(), ..Context::default() })
}

struct Pass<'c, 'q, 't> {
    cx: &'c mut Cx<'q, 't>,
}

impl<'q, 't> Pass<'_, 'q, 't> {
    /// clippy's `Types::check_ty`.
    fn check_ty(&mut self, ty: ast::Type<'t>, mut context: Context) {
        let cx = &*self.cx;
        if !cx.is_written(ty.syntax()) || cx.range(ty.syntax()).is_none() {
            return;
        }
        if context.is_in_trait_impl {
            return;
        }
        if !context.is_nested_call {
            let threshold = u64::try_from(cx.conf().int("type-complexity-threshold", 250)).expect("a count, checked as the configuration loads");
            if complexity(ty, 1) > threshold {
                return;
            }
        }
        match ty {
            ast::Type::ParenType(it) => {
                // HIR has no parentheses: `(T)` is `T`.
                if let Some(inner) = it.ty() {
                    self.check_ty(inner, context);
                }
            }
            ast::Type::PathType(path) => {
                let allowed = !(context.is_exported
                    && cx.conf().bool("avoid-breaking-exported-api", true));
                if allowed && let Some(outer) = std_of(cx, ty) && self.triggered(path, outer) {
                    return;
                }
                context.is_nested_call = true;
                let Some(path) = path.path() else { return };
                for ty in path_types(path) {
                    self.check_ty(ty, context);
                }
            }
            ast::Type::RefType(it) => {
                context.is_nested_call = true;
                if !borrowed_box(cx, it)
                    && let Some(inner) = it.ty()
                {
                    self.check_ty(inner, context);
                }
            }
            ast::Type::SliceType(it) => {
                context.is_nested_call = true;
                if let Some(inner) = it.ty() {
                    self.check_ty(inner, context);
                }
            }
            ast::Type::ArrayType(it) => {
                context.is_nested_call = true;
                if let Some(inner) = it.ty() {
                    self.check_ty(inner, context);
                }
            }
            ast::Type::PtrType(it) => {
                context.is_nested_call = true;
                if let Some(inner) = it.ty() {
                    self.check_ty(inner, context);
                }
            }
            ast::Type::TupleType(it) => {
                context.is_nested_call = true;
                for inner in it.fields() {
                    self.check_ty(inner, context);
                }
            }
            _ => {}
        }
    }

    /// The lints of the pass on a path type naming the std type `outer`, in clippy's order;
    /// whether one of them (reported here or not) was triggered, which stops the walk there.
    fn triggered(&mut self, path: ast::PathType<'t>, outer: Std) -> bool {
        let mut triggered = false;
        triggered |= box_collection(self.cx, path, outer);
        triggered |= redundant_allocation(self.cx, path, outer);
        triggered |= rc_buffer::check(self.cx, path, outer);
        triggered |= vec_box::check(self.cx, path, outer);
        let first = generic_tys(path).next();
        let first_std = first.and_then(|it| std_of(self.cx, it));
        triggered |= super::rc_mutex::check(self.cx, path, outer);
        // `option_option`, `linked_list` and `owned_cow`, not reported here.
        triggered |= outer == Std::Option && first_std == Some(Std::Option);
        triggered |= outer == Std::LinkedList;
        triggered |= outer == Std::Cow
            && matches!(first_std, Some(Std::String | Std::Vec | Std::OsString | Std::PathBuf));
        triggered
    }
}

/// clippy's `box_collection::check`.
fn box_collection<'t>(cx: &mut Cx<'_, 't>, path: ast::PathType<'t>, outer: Std) -> bool {
    if outer != Std::Box {
        return false;
    }
    let Some(item) = generic_tys(path).next().and_then(|it| std_of(cx, it)) else { return false };
    if !item.is_collection() {
        return false;
    }
    if !cx.on(LINT) {
        return true;
    }
    let Some(range) = cx.range(path.syntax()) else { return true };
    let generic = if item == Std::String { "" } else { "<..>" };
    let content = format!("{}{generic}", item.name());
    let span = cx.span(range);
    cx.lint(
        LINT,
        path.syntax(),
        span,
        format!("you seem to be trying to use `Box<{content}>`. Consider using just `{content}`"),
    )
    .help(format!("`{content}` is already on the heap, `Box<{content}>` makes an extra allocation"));
    true
}

/// Whether clippy's `redundant_allocation::check` reports `path` (`Box<&T>`, `Rc<Box<T>>` of a
/// sized `T`), which stops the walk there.
fn redundant_allocation<'t>(cx: &Cx<'_, 't>, path: ast::PathType<'t>, outer: Std) -> bool {
    if !matches!(outer, Std::Box | Std::Rc | Std::Arc) {
        return false;
    }
    let Some(first) = generic_tys(path).next() else { return false };
    if matches!(first, ast::Type::RefType(_)) {
        return true;
    }
    if !matches!(std_of(cx, first), Some(Std::Box | Std::Rc | Std::Arc)) {
        return false;
    }
    let ast::Type::PathType(inner) = first else { return false };
    let Some(inner) = generic_tys(inner).next() else { return false };
    cx.resolve_type(inner).is_some_and(|it| it.is_sized(cx.db()))
}

/// Whether clippy's `borrowed_box::check` reports `&Box<T>`, which stops the walk there: a
/// shared reference to `Box<T>`, written as one segment, of a `T` that is not `dyn Any`.
fn borrowed_box<'t>(cx: &Cx<'_, 't>, reference: ast::RefType<'t>) -> bool {
    if reference.mut_token().is_some() {
        return false;
    }
    let Some(ast::Type::PathType(boxed)) = reference.ty() else { return false };
    let Some(path) = boxed.path() else { return false };
    if path.qualifier().is_some() || std_of(cx, ast::Type::PathType(boxed)) != Some(Std::Box) {
        return false;
    }
    let Some(segment) = path.segment() else { return false };
    if segment.parenthesized_arg_list().is_some() {
        return false;
    }
    let Some(inner) = generic_tys(boxed).next() else { return false };
    !is_any_trait(cx, inner)
}

/// clippy's `is_any_trait`: `dyn Any`, its first bound `core::any::Any`.
fn is_any_trait<'t>(cx: &Cx<'_, 't>, ty: ast::Type<'t>) -> bool {
    let ast::Type::DynTraitType(dyn_) = ty else { return false };
    let Some(bound) = dyn_.type_bound_list().and_then(|it| it.bounds().next()) else { return false };
    let Some(ast::Type::PathType(path)) = bound.ty() else { return false };
    let Some(path) = path.path() else { return false };
    let db = cx.db();
    match cx.path(path) {
        Some(PathResolution::Def(hir::ModuleDef::Trait(it))) => {
            it.name(db).as_str() == "Any" && it.module(db).krate(db).origin(db).is_lang()
        }
        _ => false,
    }
}

/// The types `walk_qpath` gives `check_ty` for a path: the anchor's type of `<T as Trait>::X`
/// and the type arguments of every segment, first segment first.
fn path_types<'t>(path: ast::Path<'t>) -> Vec<ast::Type<'t>> {
    let mut segments = Vec::new();
    let mut at = Some(path);
    while let Some(it) = at {
        segments.extend(it.segment());
        at = it.qualifier();
    }
    let mut types = Vec::new();
    for segment in segments.into_iter().rev() {
        if let Some(anchor) = segment.type_anchor() {
            types.extend(anchor.ty());
        }
        for arg in segment.generic_arg_list().into_iter().flat_map(|it| it.generic_args()) {
            if let ast::GenericArg::TypeArg(it) = arg {
                types.extend(it.ty());
            }
        }
    }
    types
}

/// clippy's `TypeComplexityVisitor` score of `ty`, `nest` deep.
fn complexity(ty: ast::Type<'_>, nest: u64) -> u64 {
    let (add, sub_nest) = match ty {
        ast::Type::InferType(_) | ast::Type::PtrType(_) | ast::Type::RefType(_) => (1, 0),
        ast::Type::PathType(_)
        | ast::Type::SliceType(_)
        | ast::Type::TupleType(_)
        | ast::Type::ArrayType(_) => (10 * nest, 1),
        ast::Type::FnPtrType(_) => (50 * nest, 1),
        ast::Type::DynTraitType(it) => {
            let higher_ranked = it
                .type_bound_list()
                .into_iter()
                .flat_map(|it| it.bounds())
                .any(|it| it.for_binder().is_some());
            if higher_ranked { (50 * nest, 1) } else { (20 * nest, 0) }
        }
        _ => (0, 0),
    };
    let nest = nest + sub_nest;
    let mut score = add;
    let inner = |ty: Option<ast::Type<'_>>| ty.map_or(0, |it| complexity(it, nest));
    match ty {
        ast::Type::ParenType(it) => score += inner(it.ty()),
        ast::Type::PtrType(it) => score += inner(it.ty()),
        ast::Type::RefType(it) => score += inner(it.ty()),
        ast::Type::SliceType(it) => score += inner(it.ty()),
        ast::Type::ArrayType(it) => score += inner(it.ty()),
        ast::Type::TupleType(it) => score += it.fields().map(|it| complexity(it, nest)).sum::<u64>(),
        ast::Type::PathType(it) => score += it.path().map_or(0, |it| path_complexity(it, nest)),
        ast::Type::FnPtrType(it) => {
            if let Some(params) = it.param_list() {
                score += params.params().map(|it| inner(it.ty())).sum::<u64>();
            }
            score += inner(it.ret_type().and_then(|it| it.ty()));
        }
        ast::Type::DynTraitType(it) => {
            score += bounds_complexity(it.type_bound_list(), nest);
        }
        ast::Type::ImplTraitType(it) => {
            score += bounds_complexity(it.type_bound_list(), nest);
        }
        _ => {}
    }
    score
}

fn bounds_complexity(bounds: Option<ast::TypeBoundList<'_>>, nest: u64) -> u64 {
    bounds
        .into_iter()
        .flat_map(|it| it.bounds())
        .filter_map(|it| match it.ty() {
            Some(ast::Type::PathType(path)) => path.path(),
            _ => None,
        })
        .map(|it| path_complexity(it, nest))
        .sum()
}

/// What `walk_path` adds for a path's generic arguments: each type argument, each associated
/// type's type and bounds, and `Fn(A, B) -> C` as HIR has it, a tuple `(A, B)` and `Output = C`.
fn path_complexity(path: ast::Path<'_>, nest: u64) -> u64 {
    let mut score = 0;
    let mut at = Some(path);
    while let Some(path) = at {
        if let Some(segment) = path.segment() {
            if let Some(anchor) = segment.type_anchor() {
                score += anchor.ty().map_or(0, |it| complexity(it, nest));
            }
            for arg in segment.generic_arg_list().into_iter().flat_map(|it| it.generic_args()) {
                match arg {
                    ast::GenericArg::TypeArg(it) => {
                        score += it.ty().map_or(0, |it| complexity(it, nest));
                    }
                    ast::GenericArg::AssocTypeArg(it) => {
                        score += it.ty().map_or(0, |it| complexity(it, nest));
                        score += bounds_complexity(it.type_bound_list(), nest);
                    }
                    _ => {}
                }
            }
            if let Some(params) = segment.parenthesized_arg_list() {
                score += 10 * nest;
                score += params
                    .type_args()
                    .filter_map(|it| it.ty())
                    .map(|it| complexity(it, nest + 1))
                    .sum::<u64>();
                score += segment.ret_type().and_then(|it| it.ty()).map_or(0, |it| complexity(it, nest));
            }
        }
        at = path.qualifier();
    }
    score
}
