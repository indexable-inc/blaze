//! E0106 "missing lifetime specifier": a path to a type with lifetime parameters written
//! without them where none can be elided, in an item's signature or in a function's return
//! type when elision fails, with the lifetimes rustc suggests: one in scope, a new named one
//! on the nearest generics, or `'static`.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_elided_lifetimes_in_path`,
//! `resolve_fn_signature`, `resolve_fn_params`, the lifetime ribs of items, bounds and
//! function pointers, `LifetimeBinderKind::descr`) and `compiler/rustc_resolve/src/late/
//! diagnostics.rs` (`report_missing_lifetime_specifiers`,
//! `add_missing_lifetime_specifiers_label`, `suggest_introducing_lifetime`) (MIT or
//! Apache-2.0).

use hir::{GenericDef, HirFileId, InFile, MissingLifetimeKind, ModuleDef, PathResolution};
use syntax::{
    SmolStr, TextRange, TextSize, WalkEvent,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasGenericParams as _, HasName as _},
};

use crate::{
    cx::Cx,
    lower::{
        Found, Phase,
        incorrect_generics_len::{Segment, generics_fact, s, site_fact},
    },
    model::{Applicability, Diag, MultiSpan, Sublevel},
};

/// rustc's `MissingLifetimeKind`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `'_`, at the lifetime.
    Underscore,
    /// `&T`, at the `&`.
    Ampersand,
    /// `Ty<T>`, at the `<`.
    Comma,
    /// `Ty`, at the name.
    Brackets,
}

/// rustc's `MissingLifetime`: where lifetimes are missing, how many and how they are left out.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Missing {
    pub(crate) range: TextRange,
    pub(crate) kind: Kind,
    pub(crate) count: usize,
    /// For `Comma`: whether the brackets are empty, `Ty<>`.
    pub(crate) empty_brackets: bool,
}

impl Missing {
    /// `&T`'s lifetime, at the `&`.
    pub(crate) fn ampersand(amp: TextRange) -> Missing {
        Missing { range: amp, kind: Kind::Ampersand, count: 1, empty_brackets: false }
    }

    /// `'_`.
    pub(crate) fn underscore(lifetime: TextRange) -> Missing {
        Missing { range: lifetime, kind: Kind::Underscore, count: 1, empty_brackets: false }
    }

    /// The `count` lifetimes `seg` leaves out: at its `<`, or at its name when it has no
    /// arguments.
    pub(crate) fn of_segment(seg: &Segment<'_>, count: usize) -> Missing {
        match seg.list.and_then(|it| Some((it, it.l_angle_token()?))) {
            Some((list, angle)) => Missing {
                range: angle.text_range(),
                kind: Kind::Comma,
                count,
                empty_brackets: list.generic_args().next().is_none(),
            },
            None => Missing {
                range: seg.name.syntax().text_range(),
                kind: Kind::Brackets,
                count,
                empty_brackets: false,
            },
        }
    }
}

/// rustc's `ElisionFnParameter`: a parameter whose type has lifetimes.
pub(crate) struct FnParam {
    index: usize,
    ident: Option<SmolStr>,
    lifetime_count: usize,
    range: TextRange,
}

/// rustc's `LifetimeBinderKind`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binder {
    FnPtrType,
    PolyTrait,
    WhereBound,
    Item,
    ConstItem,
    Function,
    ImplBlock,
    ImplAssocType,
}

impl Binder {
    fn descr(self) -> &'static str {
        match self {
            Binder::FnPtrType => "type",
            Binder::PolyTrait | Binder::WhereBound => "bound",
            Binder::Item | Binder::ConstItem => "item",
            Binder::ImplAssocType => "associated type",
            Binder::ImplBlock => "impl block",
            Binder::Function => "function",
        }
    }

    pub(crate) fn higher_ranked(self) -> bool {
        matches!(self, Binder::FnPtrType | Binder::PolyTrait | Binder::WhereBound)
    }
}

/// Where rustc's generics span of a binder is: empty where it has none, else its `<..>`.
#[derive(Clone, Copy)]
pub(crate) enum At {
    Empty(TextSize),
    /// Just after the `<`, where rustc's `span_through_char(span, '<')` ends.
    AfterAngle(TextSize),
}

/// A lifetime rib of rustc's resolver with generics: what it binds and where.
pub(crate) struct Rib {
    pub(crate) kind: Binder,
    pub(crate) at: At,
    pub(crate) lifetimes: Vec<(SmolStr, TextRange)>,
}

fn rib(kind: Binder, list: Option<ast::GenericParamList<'_>>, empty_at: TextSize) -> Rib {
    let Some(list) = list else {
        // alloc: empty, allocates nothing
        return Rib { kind, at: At::Empty(empty_at), lifetimes: Vec::new() };
    };
    let at = match list.l_angle_token() {
        Some(it) => At::AfterAngle(it.text_range().end()),
        None => At::AfterAngle(list.syntax().text_range().start()),
    };
    let lifetimes = list
        .lifetime_params()
        .filter_map(|param| {
            let lifetime = param.lifetime()?;
            Some((SmolStr::new(lifetime.text()), lifetime.syntax().text_range()))
        })
        // alloc: the error's parts, for an error hir found
        .collect();
    Rib { kind, at, lifetimes }
}

fn after_name<'t>(item: impl ast::HasName<'t>) -> TextSize {
    match item.name() {
        Some(it) => it.syntax().text_range().end(),
        None => item.syntax().text_range().start(),
    }
}

/// The lifetime ribs with generics around `node`, innermost first, to the item rustc stops
/// at (its `LifetimeRibKind::Item`): an associated item's goes on to its trait or impl.
pub(crate) fn ribs(node: ast::Node<'_>) -> Vec<Rib> {
    // alloc: the error's parts, for an error hir found
    let mut ribs = Vec::new();
    for ancestor in node.ancestors() {
        if let Some(bound) = ast::TypeBound::cast(ancestor) {
            let Some(ty) = bound.ty() else { continue };
            let ty_start = ty.syntax().text_range().start();
            let list = bound.for_binder().and_then(|it| it.generic_param_list());
            let mut it = rib(Binder::PolyTrait, list, ty_start);
            // The span of a bound with modifiers and no binder is the modifiers, which has no `<`.
            if let At::Empty(at) = it.at
                && bound.syntax().text_range().start() < at
            {
                it.at = At::AfterAngle(at);
            }
            ribs.push(it);
            continue;
        }
        if let Some(pred) = ast::WherePred::cast(ancestor) {
            if pred.ty().is_none() {
                continue;
            }
            let list = pred.for_binder().and_then(|it| it.generic_param_list());
            ribs.push(rib(Binder::WhereBound, list, pred.syntax().text_range().start()));
            continue;
        }
        if let Some(ptr) = ast::FnPtrType::cast(ancestor) {
            let list = ptr
                .syntax()
                .parent()
                .and_then(ast::ForType::cast)
                .and_then(|it| it.for_binder())
                .and_then(|it| it.generic_param_list());
            ribs.push(rib(Binder::FnPtrType, list, ptr.syntax().text_range().start()));
            continue;
        }
        let Some(item) = ast::Item::cast(ancestor) else { continue };
        let in_impl = ancestor
            .parent()
            .and_then(ast::AssocItemList::cast)
            .and_then(|it| it.syntax().parent())
            .is_some_and(|it| ast::Impl::can_cast(it.kind()));
        let it = match item {
            ast::Item::Fn(it) => rib(Binder::Function, it.generic_param_list(), after_name(it)),
            ast::Item::Struct(it) => rib(Binder::Item, it.generic_param_list(), after_name(it)),
            ast::Item::Enum(it) => rib(Binder::Item, it.generic_param_list(), after_name(it)),
            ast::Item::Union(it) => rib(Binder::Item, it.generic_param_list(), after_name(it)),
            ast::Item::Trait(it) => rib(Binder::Item, it.generic_param_list(), after_name(it)),
            ast::Item::TypeAlias(it) => {
                let kind = if in_impl { Binder::ImplAssocType } else { Binder::Item };
                rib(kind, it.generic_param_list(), after_name(it))
            }
            ast::Item::Const(it) => rib(Binder::ConstItem, it.generic_param_list(), after_name(it)),
            ast::Item::Impl(it) => {
                let at = match it.impl_token() {
                    Some(token) => token.text_range().end(),
                    None => it.syntax().text_range().start(),
                };
                rib(Binder::ImplBlock, it.generic_param_list(), at)
            }
            _ => break,
        };
        ribs.push(it);
        let assoc = ancestor.parent().is_some_and(|it| ast::AssocItemList::can_cast(it.kind()));
        if !assoc {
            break;
        }
    }
    ribs
}

/// The signature whose return type `node` is in: an item's, a function pointer's, or a
/// `Fn(A) -> B` bound's.
#[derive(Clone, Copy)]
pub(crate) enum Sig<'t> {
    Fn(ast::Fn<'t>),
    Ptr(ast::FnPtrType<'t>),
    Sugar(ast::ParenthesizedArgList<'t>),
}

impl<'t> Sig<'t> {
    pub(crate) fn syntax(self) -> ast::Node<'t> {
        match self {
            Sig::Fn(it) => it.syntax(),
            Sig::Ptr(it) => it.syntax(),
            Sig::Sugar(it) => it.syntax(),
        }
    }
}

pub(crate) fn return_of(node: ast::Node<'_>) -> Option<Sig<'_>> {
    for ancestor in node.ancestors().skip(1) {
        if let Some(ret) = ast::RetType::cast(ancestor) {
            let parent = ret.syntax().parent()?;
            if let Some(it) = ast::Fn::cast(parent) {
                return Some(Sig::Fn(it));
            }
            if let Some(it) = ast::FnPtrType::cast(parent) {
                return Some(Sig::Ptr(it));
            }
            return ast::PathSegment::cast(parent)?.parenthesized_arg_list().map(Sig::Sugar);
        }
        let kind = ancestor.kind();
        if ast::ParamList::can_cast(kind)
            || ast::ParenthesizedArgList::can_cast(kind)
            || ast::FnPtrType::can_cast(kind)
            || ast::Item::can_cast(kind)
            || ast::ConstArg::can_cast(kind)
            || ast::BlockExpr::can_cast(kind)
        {
            return None;
        }
    }
    None
}

/// What the lifetimes of a type resolve to, and where they are missing: rustc's elision
/// candidates of a parameter, or the elision failures of a return type.
#[derive(Default)]
struct Lifetimes {
    /// Distinct names; a lifetime's name fits `SmolStr`'s inline buffer, so no heap.
    named: Vec<SmolStr>,
    fresh: usize,
    missing: Vec<Missing>,
    /// When set, the walk stops once a missing site with this range is recorded.
    until: Option<TextRange>,
    reached: bool,
}

impl Lifetimes {
    fn count(&self) -> usize {
        self.named.len() + self.fresh
    }

    fn lifetime(&mut self, lifetime: ast::Lifetime<'_>) {
        let text = lifetime.text();
        if text == "'_" {
            self.fresh += 1;
            self.missing.push(Missing {
                range: lifetime.syntax().text_range(),
                kind: Kind::Underscore,
                count: 1,
                empty_brackets: false,
            });
        } else if !self.named.iter().any(|it| it == text) {
            self.named.push(SmolStr::new(text));
        }
    }
}

/// The number of lifetime parameters of the type or trait `path` names.
fn path_lifetimes<'db>(cx: &Cx<'_, 'db>, path: ast::Path<'db>) -> usize {
    let def = match cx.vsema.resolve_path(path) {
        Some(PathResolution::Def(ModuleDef::Adt(it))) => GenericDef::from(it),
        Some(PathResolution::Def(ModuleDef::TypeAlias(it))) => GenericDef::from(it),
        Some(PathResolution::Def(ModuleDef::Trait(it))) => GenericDef::from(it),
        _ => return 0,
    };
    def.lifetime_params(cx.db).len()
}

/// The lifetimes a path leaves out: those of the segment that names a type or trait, when it
/// gives none.
pub(crate) fn path_missing<'db>(cx: &Cx<'_, 'db>, path: ast::Path<'db>) -> Option<Missing> {
    let mut at = Some(path);
    while let Some(path) = at {
        at = path.qualifier();
        let Some(segment) = path.segment() else { continue };
        let count = path_lifetimes(cx, path);
        if count == 0 {
            continue;
        }
        let list = segment.generic_arg_list();
        if list.is_some_and(|it| it.generic_args().any(|arg| matches!(arg, ast::GenericArg::LifetimeArg(_)))) {
            return None;
        }
        return Some(match list.and_then(|it| Some((it, it.l_angle_token()?))) {
            Some((list, angle)) => Missing {
                range: angle.text_range(),
                kind: Kind::Comma,
                count,
                empty_brackets: list.generic_args().next().is_none(),
            },
            None => Missing {
                range: segment.name_ref()?.syntax().text_range(),
                kind: Kind::Brackets,
                count,
                empty_brackets: false,
            },
        });
    }
    None
}

/// Walks the type at `node` as rustc's resolver records its lifetimes, leaving out the
/// signatures in it, which elide on their own.
fn walk<'db>(cx: &Cx<'_, 'db>, node: ast::Node<'db>, into: &mut Lifetimes) {
    let mut preorder = node.preorder();
    while let Some(event) = preorder.next() {
        if into.until.is_some_and(|until| into.missing.last().is_some_and(|it| it.range == until)) {
            into.reached = true;
            return;
        }
        let WalkEvent::Enter(node) = event else { continue };
        let kind = node.kind();
        if ast::FnPtrType::can_cast(kind)
            || ast::ParenthesizedArgList::can_cast(kind)
            || ast::ConstArg::can_cast(kind)
            || ast::BlockExpr::can_cast(kind)
            || (ast::RetType::can_cast(kind)
                && node.parent().is_some_and(|it| ast::PathSegment::can_cast(it.kind())))
        {
            preorder.skip_subtree();
            continue;
        }
        if let Some(it) = ast::RefType::cast(node) {
            match it.lifetime() {
                Some(lifetime) => into.lifetime(lifetime),
                None => {
                    if let Some(amp) = it.amp_token() {
                        into.fresh += 1;
                        into.missing.push(Missing {
                            range: amp.text_range(),
                            kind: Kind::Ampersand,
                            count: 1,
                            empty_brackets: false,
                        });
                    }
                }
            }
            continue;
        }
        if let Some(it) = ast::LifetimeArg::cast(node) {
            if let Some(lifetime) = it.lifetime() {
                into.lifetime(lifetime);
            }
            continue;
        }
        if let Some(it) = ast::TypeBound::cast(node) {
            if let Some(lifetime) = it.lifetime() {
                into.lifetime(lifetime);
            }
            continue;
        }
        if let Some(it) = ast::PathType::cast(node)
            && let Some(path) = it.path()
            && let Some(missing) = path_missing(cx, path)
        {
            into.fresh += missing.count;
            into.missing.push(missing);
        }
    }
}

/// The parameters of `sig` with lifetimes in their types, and the lifetimes they leave out:
/// rustc's `resolve_fn_params` when elision fails.
pub(crate) fn fn_params<'db>(cx: &Cx<'_, 'db>, sig: Sig<'db>) -> (Vec<Missing>, Vec<FnParam>) {
    // alloc: the error's parts, for an error hir found
    let mut inputs: Vec<(Option<SmolStr>, ast::Node<'db>, TextRange)> = Vec::new();
    let mut self_param = None;
    let params = match sig {
        Sig::Fn(it) => it.param_list(),
        Sig::Ptr(it) => it.param_list(),
        Sig::Sugar(it) => {
            for arg in it.type_args() {
                if let Some(ty) = arg.ty() {
                    inputs.push((None, ty.syntax(), ty.syntax().text_range()));
                }
            }
            None
        }
    };
    if let Some(list) = params {
        if let Some(it) = list.self_param() {
            self_param = Some(it);
        }
        for param in list.params() {
            let Some(ty) = param.ty() else { continue };
            let ident = match param.pat() {
                Some(ast::Pat::IdentPat(pat)) => pat.name().map(|it| SmolStr::new(it.text())),
                _ => None,
            };
            inputs.push((ident, ty.syntax(), ty.syntax().text_range()));
        }
    }
    // alloc: the error's parts, for an error hir found
    let mut missing = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut out = Vec::new();
    let mut index = 0;
    if let Some(it) = self_param {
        let mut lifetimes = Lifetimes::default();
        let range = match it.ty() {
            Some(ty) => {
                walk(cx, ty.syntax(), &mut lifetimes);
                ty.syntax().text_range()
            }
            None => {
                match (it.amp_token(), it.lifetime()) {
                    (Some(_), Some(lifetime)) => lifetimes.lifetime(lifetime),
                    (Some(amp), None) => {
                        lifetimes.fresh += 1;
                        lifetimes.missing.push(Missing {
                            range: amp.text_range(),
                            kind: Kind::Ampersand,
                            count: 1,
                            empty_brackets: false,
                        });
                    }
                    _ => {}
                }
                it.syntax().text_range()
            }
        };
        if lifetimes.count() != 0 {
            out.push(FnParam {
                index,
                ident: Some(SmolStr::new_static("self")),
                lifetime_count: lifetimes.count(),
                range,
            });
            missing.extend(lifetimes.missing);
        }
        index += 1;
    }
    for (ident, ty, range) in inputs {
        let mut lifetimes = Lifetimes::default();
        walk(cx, ty, &mut lifetimes);
        if lifetimes.count() != 0 {
            out.push(FnParam { index, ident, lifetime_count: lifetimes.count(), range });
            missing.extend(lifetimes.missing);
        }
        index += 1;
    }
    (missing, out)
}

fn ret_type(sig: Sig<'_>) -> Option<ast::RetType<'_>> {
    match sig {
        Sig::Fn(it) => it.ret_type(),
        Sig::Ptr(it) => it.ret_type(),
        Sig::Sugar(it) => ast::PathSegment::cast(it.syntax().parent()?)?.ret_type(),
    }
}

/// rustc's `build_sugg`: `existing` written where `lt` leaves lifetimes out.
fn build_sugg(lt: &Missing, existing: &str) -> (TextRange, String) {
    let end = TextRange::empty(lt.range.end());
    // alloc: the error's text, for an error hir found
    let repeated = || vec![existing; lt.count].join(", ");
    match lt.kind {
        // alloc: the error's text, for an error hir found
        Kind::Underscore => (lt.range, existing.to_owned()),
        // alloc: the error's text, for an error hir found
        Kind::Ampersand => (end, format!("{existing} ")),
        Kind::Comma => match lt.empty_brackets {
            true => (end, repeated()),
            // alloc: the error's text, for an error hir found
            false => (end, format!("{}, ", repeated())),
        },
        // alloc: the error's text, for an error hir found
        Kind::Brackets => (end, format!("<{}>", repeated())),
    }
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MissingLifetime) -> Vec<Found> {
    let expected = d.expected as usize;
    let Some(seg) = Segment::of(cx, d.generics_or_segment) else {
        let span = cx.span_of_ptr(d.generics_or_segment);
        let mut diag =
            // alloc: the error's text, for an error hir found
            Diag::error(Some("E0106"), format!("missing lifetime specifier{}", s(expected)), span);
        generics_fact(cx, &mut diag, d.def);
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(diag, Phase::Resolve)];
    };
    let this = Missing::of_segment(&seg, expected);

    // A return type's elision failures are one error, given with its first path; hir gives
    // those of a type's paths with the rest of the return type's as `MissingLifetimes`, so
    // these are the paths it lowers on their own.
    let sig = match d.kind {
        MissingLifetimeKind::ElisionFailure => return_of(seg.node),
        MissingLifetimeKind::Missing | MissingLifetimeKind::NotAllowed => None,
    };
    let (refs, params) = match sig {
        Some(sig) => {
            // Each site of the group has its own hir diagnostic: the walk up to this one's
            // site says whether a path came before it, without resolving the rest.
            if let Some(ret) = ret_type(sig) {
                let mut upto = Lifetimes { until: Some(this.range), ..Lifetimes::default() };
                walk(cx, ret.syntax(), &mut upto);
                let reached = upto.reached
                    || upto.missing.last().is_some_and(|it| it.range == this.range);
                let earlier_path = upto.missing[..upto.missing.len().saturating_sub(1)]
                    .iter()
                    .any(|it| matches!(it.kind, Kind::Comma | Kind::Brackets));
                if reached && earlier_path {
                    // alloc: empty, allocates nothing
                    return Vec::new();
                }
            }
            let mut refs = Lifetimes::default();
            if let Some(ret) = ret_type(sig) {
                walk(cx, ret.syntax(), &mut refs);
            }
            let refs = match refs.missing.iter().any(|it| it.range == this.range) {
                true => refs.missing,
                // alloc: the error's parts, for an error hir found
                false => vec![this],
            };
            let first_path =
                refs.iter().find(|it| matches!(it.kind, Kind::Comma | Kind::Brackets));
            if first_path.is_some_and(|it| it.range != this.range) {
                // alloc: empty, allocates nothing
                return Vec::new();
            }
            (refs, Some(fn_params(cx, sig)))
        }
        // alloc: the error's parts, for an error hir found
        None => (vec![this], None),
    };

    let rib_at = sig.map_or(seg.node, Sig::syntax);
    let mut diag = report(cx, seg.file, rib_at, &refs, params);
    generics_fact(cx, &mut diag, d.def);
    site_fact(cx, &mut diag, InFile::new(seg.file, seg.node));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}

/// rustc's `report_missing_lifetime_specifiers` of `refs` in `file`, under the lifetime ribs
/// around `rib_at`, with the parameters of the signature when they are a return type's
/// elision failures: E0106 with its labels, help and suggestions.
pub(crate) fn report<'db>(
    cx: &Cx<'_, 'db>,
    file: HirFileId,
    rib_at: ast::Node<'db>,
    refs: &[Missing],
    params: Option<(Vec<Missing>, Vec<FnParam>)>,
) -> Diag {
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let total: usize = refs.iter().map(|it| it.count).sum();
    let mut diag =
        // alloc: the error's text, for an error hir found
        Diag::error(Some("E0106"), format!("missing lifetime specifier{}", s(total)), span(refs[0].range));
    for lt in &refs[1..] {
        diag.primary(span(lt.range));
    }
    for lt in refs {
        let expected = match lt.count {
            // alloc: the error's text, for an error hir found
            1 => "named".to_owned(),
            // alloc: the error's text, for an error hir found
            n => n.to_string(),
        };
        // alloc: the error's text, for an error hir found
        diag.span_label(span(lt.range), format!("expected {expected} lifetime parameter{}", s(lt.count)));
    }

    let ribs = ribs(rib_at);
    // alloc: the error's parts, for an error hir found
    let mut in_scope: Vec<(SmolStr, Option<TextRange>)> = Vec::new();
    for rib in &ribs {
        for (name, range) in &rib.lifetimes {
            if name != "'_" {
                // clone: the error's parts, for an error hir found
                in_scope.push((name.clone(), Some(*range)));
            }
        }
    }

    let mut maybe_static = false;
    if let Some((param_lifetimes, params)) = &params {
        let elided_len = param_lifetimes.len();
        let num_params = params.len();
        // alloc: the error's text, for an error hir found
        let mut m = String::new();
        for (i, param) in params.iter().enumerate() {
            diag.span_label(span(param.range), "");
            if i != 0 {
                if i + 1 < num_params {
                    m.push_str(", ");
                } else if num_params == 2 {
                    m.push_str(" or ");
                } else {
                    m.push_str(", or ");
                }
            }
            let help_name = match &param.ident {
                // alloc: the error's text, for an error hir found
                Some(ident) => format!("`{ident}`"),
                // alloc: the error's text, for an error hir found
                None => format!("argument {}", param.index + 1),
            };
            match param.lifetime_count {
                1 => m.push_str(&help_name),
                // alloc: the error's text, for an error hir found
                n => m.push_str(&format!("one of {help_name}'s {n} lifetimes")),
            }
        }
        if num_params == 0 || elided_len == 0 {
            diag.help(match num_params {
                0 => "this function's return type contains a borrowed value, but there is no \
                      value for it to be borrowed from",
                _ => "this function's return type contains a borrowed value with an elided \
                      lifetime, but the lifetime cannot be derived from the arguments",
            });
            if in_scope.is_empty() {
                maybe_static = true;
                in_scope.push((SmolStr::new_static("'static"), None));
            }
        } else if num_params == 1 {
            // alloc: the error's text, for an error hir found
            diag.help(format!(
                "this function's return type contains a borrowed value, but the signature does \
                 not say which {m} it is borrowed from"
            ));
        } else {
            // alloc: the error's text, for an error hir found
            diag.help(format!(
                "this function's return type contains a borrowed value, but the signature does \
                 not say whether it is borrowed from {m}"
            ));
        }
    }

    let existing = match &in_scope[..] {
        [] => "'a",
        [(name, _)] => name.as_str(),
        _ => "'lifetime",
    };
    let mut spans_suggs: Vec<(TextRange, String)> =
        // alloc: the error's parts, for an error hir found
        refs.iter().map(|lt| build_sugg(lt, existing)).collect();
    match in_scope.len() {
        0 => {
            if let Some((param_lifetimes, _)) = &params {
                spans_suggs.extend(param_lifetimes.iter().map(|lt| build_sugg(lt, existing)));
            }
            for rib in &ribs {
                if matches!(rib.kind, Binder::ImplAssocType | Binder::ConstItem) {
                    continue;
                }
                let higher_ranked = rib.kind.higher_ranked();
                let (at, sugg) = match rib.at {
                    At::Empty(at) => match higher_ranked {
                        // alloc: the error's text, for an error hir found
                        true => (at, "for<'a> ".to_owned()),
                        // alloc: the error's text, for an error hir found
                        false => (at, "<'a>".to_owned()),
                    },
                    // alloc: the error's text, for an error hir found
                    At::AfterAngle(at) => (at, "'a, ".to_owned()),
                };
                let message = match higher_ranked {
                    // alloc: the error's text, for an error hir found
                    true => format!(
                        "consider making the {} lifetime-generic with a new `'a` lifetime",
                        rib.kind.descr()
                    ),
                    // alloc: the error's text, for an error hir found
                    false => "consider introducing a named lifetime parameter".to_owned(),
                };
                let parts = std::iter::once((TextRange::empty(at), sugg))
                    .chain(spans_suggs.iter().cloned())
                    .map(|(range, code)| (span(range), code))
                    // alloc: the error's parts, for an error hir found
                    .collect();
                diag.multipart_suggestion(message, parts, Applicability::MaybeIncorrect);
                if !higher_ranked {
                    break;
                }
                diag.sub(
                    Sublevel::Note,
                    "for more information on higher-ranked polymorphism, visit \
                     https://doc.rust-lang.org/nomicon/hrtb.html",
                    MultiSpan::default(),
                    true,
                );
            }
        }
        1 => {
            let post = match maybe_static {
                true => {
                    let owned = match &refs[..] {
                        [lt] if lt.kind != Kind::Ampersand => ", or if you will only have owned values",
                        _ => "",
                    };
                    // alloc: the error's text, for an error hir found
                    format!(
                        ", but this is uncommon unless you're returning a borrowed value from a \
                         `const` or a `static`{owned}"
                    )
                }
                // alloc: empty, allocates nothing
                false => String::new(),
            };
            // alloc: the error's parts, for an error hir found
            let parts = spans_suggs.into_iter().map(|(range, code)| (span(range), code)).collect();
            diag.multipart_suggestion(
                // alloc: the error's text, for an error hir found
                format!("consider using the `{existing}` lifetime{post}"),
                parts,
                Applicability::MaybeIncorrect,
            );
        }
        _ => {
            // alloc: the error's parts, for an error hir found
            let spans = in_scope.iter().filter_map(|(_, range)| range.map(span)).collect();
            diag.span_note(
                // alloc: empty, allocates nothing
                MultiSpan { primary: spans, labels: Vec::new() },
                "these named lifetimes are available to use",
            );
            if !spans_suggs.is_empty() {
                let parts =
                    // alloc: the error's parts, for an error hir found
                    spans_suggs.into_iter().map(|(range, code)| (span(range), code)).collect();
                diag.multipart_suggestion(
                    "consider using one of the available lifetimes here",
                    parts,
                    Applicability::HasPlaceholders,
                );
            }
        }
    }
    diag
}
