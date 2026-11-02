//! `mismatched_lifetime_syntaxes`: a lifetime of a function's return type that its parameters
//! write in another syntax: hidden in a path (`ContainsLifetime`), elided (`&T`, `'_`, `+ '_`)
//! or named (`'a`, `'static`). Each lifetime the return type has is grouped with the parameters'
//! lifetimes that resolve to it (an elided one to what elision gives it); a group whose members
//! are not all of one syntax is linted, with the fixes rustc suggests.
//!
//! Functions with bodies (rustc's `check_fn`), trait functions (`check_trait_item`, so a provided
//! one twice, as rustc does) and foreign functions are checked. Closures with a written return
//! type, which rustc's `check_fn` also checks with all their elided lifetimes resolved to one
//! `LifetimeKind::Infer`, are not yet.
//!
//! Follows `compiler/rustc_lint/src/lifetime_syntax.rs`, `compiler/rustc_hir/src/hir.rs`
//! (`Lifetime::suggestion`, `LifetimeSource`), `compiler/rustc_ast_lowering/src/path.rs` (the
//! spans of a path's hidden lifetimes), `compiler/rustc_resolve/src/late.rs`
//! (`resolve_fn_params`, `find_lifetime_for_self`) and `compiler/rustc_lint/src/lints.rs`
//! (`MismatchedLifetimeSyntaxes`) (MIT or Apache-2.0).

use blaze_items::Fact;
use hir::{InFile, ModuleDef, PathResolution};
use std::collections::hash_map::Entry;

use blaze_hash::HashMap;
use smallvec::SmallVec;
use syntax::{
    SmolStr, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode, HasGenericArgs, HasName},
};

use super::walk::{Container, Item};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// What a lifetime resolves to: a name (its index in [`Walk::names`]), or one elided lifetime
/// (or one a nested binder owns).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Res {
    Named(u32),
    Fresh(u32),
    /// An elided lifetime of the return type outside a nested binder: what elision gives it,
    /// known once the parameters are walked.
    Target,
}

/// rustc's `LifetimeSyntax`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Syntax {
    Implicit,
    ExplicitAnonymous,
    ExplicitBound,
}

/// rustc's `AngleBrackets` of a path that hides a lifetime.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Angle {
    Missing,
    Empty,
    Full,
}

/// rustc's `LifetimeSource`, without `Other`, which is never linted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Reference,
    Path(Angle),
    OutlivesBound,
    PreciseCapturing,
}

/// rustc's `LifetimeSyntaxCategory`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Category {
    Hidden,
    Elided,
    Named,
}

/// One lifetime of the signature: rustc's `Info`.
#[derive(Clone, Debug)]
struct Info {
    res: Res,
    name: SmolStr,
    syntax: Syntax,
    source: Source,
    /// The lifetime's own span: where an implicit one would be written.
    ident: TextRange,
    /// The innermost type it is in, and a key for that type.
    ty: TextRange,
    /// For a reference, where its referent starts.
    referent: Option<TextSize>,
}

impl Info {
    fn category(&self) -> Category {
        match (self.syntax, self.source) {
            (Syntax::Implicit, Source::Path(_)) => Category::Hidden,
            (Syntax::ExplicitBound, _) => Category::Named,
            _ => Category::Elided,
        }
    }

    fn reporting(&self) -> TextRange {
        match self.syntax {
            Syntax::Implicit => self.ty,
            _ => self.ident,
        }
    }

    /// The span that removes an explicit lifetime of a reference, up to its referent.
    fn removing(&self) -> TextRange {
        match (self.source, self.referent) {
            (Source::Reference, Some(referent)) => TextRange::new(self.ident.start(), referent.max(self.ident.start())),
            _ => self.ident,
        }
    }

    /// rustc's `Lifetime::suggestion`, of what is not a hidden lifetime of a path.
    fn suggestion(&self, name: &str) -> (TextRange, String) {
        match self.syntax {
            Syntax::Implicit => (self.ident, format!("{name} ")), // alloc: the firing lint's suggestion
            _ => (self.ident, name.to_owned()),                    // alloc: the firing lint's suggestion
        }
    }
}

/// The lints of `item` at rustc's `check_fn`, `check_trait_item` and `check_foreign_item`.
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, anonymous_in_impl_trait: bool) -> Vec<Found> {
    let ast::Item::Fn(f) = item.node.value else { return Vec::new() }; // alloc: empty, allocates nothing
    let times = match (&item.container, f.body().is_some()) {
        (Container::Trait(_), true) => 2,
        (Container::Trait(_) | Container::Extern(_), false) | (_, true) => 1,
        (_, false) => 0,
    };
    if times == 0 {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    check(cx, item, f, anonymous_in_impl_trait, &mut found);
    if times == 2 {
        found.extend_from_within(..);
    }
    found
}

/// rustc's `check_fn_like`. The return type is walked first: one with no lifetime (most) can
/// mismatch nothing, and its parameters are not looked at.
fn check<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, f: ast::Fn<'db>, anonymous_in_impl_trait: bool, found: &mut Vec<Found>) {
    let Some(params) = f.param_list() else { return };
    if params.self_param().is_none() && params.params().next().is_none() {
        return;
    }
    let Some(output) = f.ret_type().and_then(|it| it.ty()) else { return };
    let mut walk = Walk {
        cx,
        fresh: 0,
        anonymous_in_impl_trait,
        bound: SmallVec::new(),
        names: SmallVec::new(),
        output: true,
        refs: false,
    };
    let mut outputs = Walked::default();
    walk.ty(output.syntax(), Scope::default(), &mut outputs);
    if outputs.infos.is_empty() {
        return;
    }
    walk.output = false;
    let impl_self = match &item.container {
        Container::Impl(imp) => imp.value.self_ty(),
        _ => None,
    };
    // The parameters' lifetimes, with the candidates elision counts of the one being walked;
    // what elision gives so far; and the lifetimes of references to `Self`.
    let mut inputs = Walked::default();
    let mut elision: Option<Option<Res>> = None;
    let mut self_refs: SmallVec<[Res; 2]> = SmallVec::new();
    if let Some(param) = params.self_param() {
        match param.ty() {
            Some(ty) => {
                walk.refs = true;
                walk.ty(ty.syntax(), Scope::default(), &mut inputs);
                walk.refs = false;
                for &(inner, res) in &inputs.refs {
                    if names_self(cx, inner, impl_self) && !self_refs.contains(&res) {
                        self_refs.push(res);
                    }
                }
            }
            None => {
                // `&self` is `self: &Self`, whose type is all of it.
                if let Some(amp) = param.amp_token() {
                    let scope = Scope { ty: Some(param.syntax().text_range()), ..Scope::default() };
                    let (res, name, syntax, ident) = match param.lifetime() {
                        Some(lifetime) => (
                            walk.named(lifetime, &scope),
                            SmolStr::new(lifetime.text()),
                            syntax_of(lifetime),
                            lifetime.syntax().text_range(),
                        ),
                        None => (walk.fresh(), SmolStr::default(), Syntax::Implicit, TextRange::empty(amp.text_range().end())),
                    };
                    if let Some(res) = res {
                        let referent = param.name().map(|it| it.syntax().text_range().start());
                        self_refs.push(res);
                        let info = Info { res, name, syntax, source: Source::Reference, ident, ty: ident, referent };
                        walk.push(&mut inputs, &scope, info);
                    }
                }
            }
        }
        elide(&mut elision, &inputs.candidates);
        inputs.candidates.clear();
    }
    for param in params.params() {
        let Some(ty) = param.ty() else { continue };
        walk.ty(ty.syntax(), Scope::default(), &mut inputs);
        elide(&mut elision, &inputs.candidates);
        inputs.candidates.clear();
    }
    let target = match self_refs.as_slice() {
        [one] => Some(*one),
        [_, _, ..] => None,
        [] => elision.flatten(),
    };
    // An elided lifetime of the return type is what elision gives it; none when it fails
    // (E0106).
    outputs.infos.retain(|info| {
        if info.res != Res::Target {
            return true;
        }
        match target {
            Some(target) => {
                info.res = target;
                true
            }
            None => false,
        }
    });
    // Each group, one per lifetime of the return type in the order it first appears there, with
    // the parameters' lifetimes that resolve to it.
    let (ins, outs) = (inputs.infos.as_slice(), outputs.infos.as_slice());
    for (at, info) in outs.iter().enumerate() {
        let res = info.res;
        if outs[..at].iter().any(|it| it.res == res) {
            continue;
        }
        let Some(first) = resolving(ins, res).next() else { continue };
        let category = first.category();
        if resolving(ins, res).chain(resolving(outs, res)).all(|it| it.category() == category) {
            continue;
        }
        // alloc: the group of the lint that fires
        let (group_ins, group_outs): (Vec<&Info>, Vec<&Info>) = (resolving(ins, res).collect(), resolving(outs, res).collect());
        found.push(mismatch(cx, item, &group_ins, &group_outs));
    }
}

/// The lifetimes among `infos` that resolve to `res`, in order.
fn resolving(infos: &[Info], res: Res) -> impl Iterator<Item = &Info> {
    infos.iter().filter(move |it| it.res == res)
}

/// rustc's `resolve_fn_params` without `self`, one parameter at a time: `elision` is `None`
/// before a parameter with lifetimes, then the one lifetime of the one parameter that has
/// lifetimes, if there is one.
fn elide(elision: &mut Option<Option<Res>>, param: &[Res]) {
    let mut distinct: SmallVec<[Res; 4]> = SmallVec::new();
    for &res in param {
        if !distinct.contains(&res) {
            distinct.push(res);
        }
    }
    if distinct.is_empty() {
        return;
    }
    *elision = match (*elision, distinct.as_slice()) {
        (None, [one]) => Some(Some(*one)),
        _ => Some(None),
    };
}

/// Whether `ty`, a reference's referent in `self`'s type, names `Self` or the impl's self type
/// (rustc's `find_lifetime_for_self`). A path is resolved only when it is not `Self` and the
/// impl's self type names a definition.
fn names_self<'db>(cx: &Cx<'_, 'db>, ty: ast::Node<'db>, impl_self: Option<ast::Type<'db>>) -> bool {
    let impl_def = impl_self.and_then(|it| match it {
        ast::Type::PathType(p) => p.path().and_then(|p| cx.vsema.resolve_path(p)),
        _ => None,
    });
    let paths = ty.descendants().filter(|it| it.kind() == SyntaxKind::PATH_TYPE).filter_map(ast::PathType::cast);
    paths.filter_map(|it| it.path()).any(|path| {
        if path.qualifier().is_none() && path.segment().is_some_and(|it| it.self_type_token().is_some()) {
            return true;
        }
        let Some(PathResolution::Def(def)) = &impl_def else { return false };
        matches!(cx.vsema.resolve_path(path), Some(PathResolution::Def(it)) if it == *def)
    })
}

/// Whether elision is its own where a type is, and that type: a copy per node (the lifetimes
/// `for<..>` binders own are on [`Walk::bound`]).
#[derive(Clone, Copy, Default)]
struct Scope {
    /// In a `fn` type or `Fn(..)` sugar, which elide on their own.
    nested: bool,
    /// In an `impl Trait` of a parameter, whose elided lifetimes elision does not count.
    impl_trait: bool,
    /// The innermost type.
    ty: Option<TextRange>,
}

/// What a walk of one parameter's or the return type's type found.
#[derive(Default)]
struct Walked<'db> {
    infos: SmallVec<[Info; 4]>,
    candidates: SmallVec<[Res; 4]>,
    /// Each reference's referent and lifetime, when [`Walk::refs`] asks for them.
    refs: SmallVec<[(ast::Node<'db>, Res); 2]>,
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    fresh: u32,
    anonymous_in_impl_trait: bool,
    /// The lifetimes the `for<..>` binders around the node being walked own, innermost last.
    bound: SmallVec<[SmolStr; 4]>,
    /// The names lifetimes resolve to, each once: [`Res::Named`] indexes it.
    names: SmallVec<[SmolStr; 4]>,
    /// Walking the return type: its elided lifetimes outside a nested binder are
    /// [`Res::Target`].
    output: bool,
    /// Recording each reference's referent: only `self`'s type needs them.
    refs: bool,
}

impl<'db> Walk<'_, '_, 'db> {
    fn fresh(&mut self) -> Option<Res> {
        self.fresh += 1;
        Some(Res::Fresh(self.fresh))
    }

    /// What an elided lifetime resolves to at `scope`: its own, or in the return type outside
    /// a nested binder what elision gives.
    fn elided(&mut self, scope: &Scope) -> Option<Res> {
        if self.output && !scope.nested {
            return Some(Res::Target);
        }
        self.fresh()
    }

    fn named(&mut self, lifetime: ast::Lifetime<'_>, scope: &Scope) -> Option<Res> {
        let name = lifetime.text();
        if name == "'_" {
            return self.elided(scope);
        }
        if self.bound.iter().any(|it| it == name) {
            return self.fresh();
        }
        let index = match self.names.iter().position(|it| it == name) {
            Some(index) => index,
            None => {
                self.names.push(SmolStr::new(name));
                self.names.len() - 1
            }
        };
        Some(Res::Named(index as u32))
    }

    /// Whether elision counts `res` written at `scope` as a lifetime of its parameter.
    fn counts(&self, scope: &Scope, res: &Res) -> bool {
        let elided = matches!(res, Res::Fresh(_));
        !scope.nested && (!scope.impl_trait || !elided || self.anonymous_in_impl_trait)
    }

    /// Records `info`, whose `ty` is the innermost type of `scope`.
    fn push(&self, out: &mut Walked<'_>, scope: &Scope, mut info: Info) {
        if self.counts(scope, &info.res) {
            out.candidates.push(info.res);
        }
        info.ty = scope.ty.unwrap_or(info.ident);
        out.infos.push(info);
    }

    /// The lifetimes of `node`, in the order rustc's `walk_unambig_ty` visits them.
    fn ty(&mut self, node: ast::Node<'db>, mut scope: Scope, out: &mut Walked<'db>) {
        let kind = node.kind();
        if ast::Type::can_cast(kind) {
            scope.ty = Some(node.text_range());
        }
        match kind {
            SyntaxKind::CONST_ARG | SyntaxKind::FOR_BINDER => return,
            SyntaxKind::FN_PTR_TYPE => scope.nested = true,
            // `Fn(..) -> ..`, whose return type is the segment's, not the argument list's.
            SyntaxKind::PATH_SEGMENT if node.children().any(|it| it.kind() == SyntaxKind::PARENTHESIZED_ARG_LIST) => {
                scope.nested = true
            }
            SyntaxKind::IMPL_TRAIT_TYPE if !self.output => scope.impl_trait = true,
            _ => {}
        }
        let mark = self.bound.len();
        // The nodes of a type that have a `for<..>`.
        if matches!(kind, SyntaxKind::FOR_TYPE | SyntaxKind::TYPE_BOUND | SyntaxKind::WHERE_PRED)
            && let Some(binder) = node.children().find(|it| it.kind() == SyntaxKind::FOR_BINDER)
        {
            let params = binder.descendants().filter(|it| it.kind() == SyntaxKind::LIFETIME_PARAM);
            for param in params.filter_map(ast::LifetimeParam::cast) {
                if let Some(lifetime) = param.lifetime() {
                    self.bound.push(SmolStr::new(lifetime.text()));
                }
            }
        }
        self.node(node, kind, &scope, out);
        self.bound.truncate(mark);
    }

    /// [`Walk::ty`] of `node`, of the kind `kind`, once its scope is known.
    fn node(&mut self, node: ast::Node<'db>, kind: SyntaxKind, scope: &Scope, out: &mut Walked<'db>) {
        if kind == SyntaxKind::REF_TYPE
            && let Some(r) = ast::RefType::cast(node)
        {
            let inner = r.ty();
            let referent = inner.as_ref().map(|it| it.syntax().text_range().start());
            let (res, syntax, ident, name) = match r.lifetime() {
                Some(lifetime) => {
                    (self.named(lifetime, scope), syntax_of(lifetime), lifetime.syntax().text_range(), SmolStr::new(lifetime.text()))
                }
                None => {
                    let at = r.amp_token().map_or(node.text_range().start(), |it| it.text_range().end());
                    (self.elided(scope), Syntax::Implicit, TextRange::empty(at), SmolStr::default())
                }
            };
            if let Some(res) = res {
                if self.refs
                    && let Some(inner) = inner
                {
                    out.refs.push((inner.syntax(), res));
                }
                let info = Info { res, name, syntax, source: Source::Reference, ident, ty: ident, referent };
                self.push(out, scope, info);
            }
            if let Some(inner) = inner {
                self.ty(inner.syntax(), *scope, out);
            }
            return;
        }
        if kind == SyntaxKind::PATH_TYPE
            && let Some(path) = ast::PathType::cast(node).and_then(|it| it.path())
        {
            let hidden = self.hidden(path);
            if hidden > 0 {
                let segment = path.segment();
                let args = segment.as_ref().and_then(|it| it.generic_arg_list());
                let (angle, ident) = match &args {
                    None => {
                        let name = segment.as_ref().and_then(|it| it.name_ref()).map(|it| it.syntax().text_range());
                        (Angle::Missing, name.unwrap_or(node.text_range()))
                    }
                    Some(args) => {
                        let after = args.l_angle_token().map_or(args.syntax().text_range().start(), |it| it.text_range().end());
                        let angle = if args.generic_args().next().is_none() { Angle::Empty } else { Angle::Full };
                        (angle, TextRange::empty(after))
                    }
                };
                for _ in 0..hidden {
                    if let Some(res) = self.elided(scope) {
                        let source = Source::Path(angle);
                        let name = SmolStr::default();
                        let info = Info { res, name, syntax: Syntax::Implicit, source, ident, ty: ident, referent: None };
                        self.push(out, scope, info);
                    }
                }
            }
        }
        if kind == SyntaxKind::LIFETIME {
            let Some(lifetime) = ast::Lifetime::cast(node) else { return };
            let Some(source) = source_of(node) else { return };
            if let Some(res) = self.named(lifetime, scope) {
                let (name, syntax, ident) = (SmolStr::new(lifetime.text()), syntax_of(lifetime), node.text_range());
                self.push(out, scope, Info { res, name, syntax, source, ident, ty: ident, referent: None });
            }
            return;
        }
        for child in node.children() {
            if child.kind() == SyntaxKind::LIFETIME && source_of(child).is_none() {
                // An object lifetime bound or a declaration: elision still counts the former.
                if kind == SyntaxKind::TYPE_BOUND
                    && let Some(lifetime) = ast::Lifetime::cast(child)
                    && let Some(res) = self.named(lifetime, scope)
                    && self.counts(scope, &res)
                {
                    out.candidates.push(res);
                }
                continue;
            }
            self.ty(child, *scope, out);
        }
    }

    /// How many lifetimes a path leaves out: all of its type's, when it gives none.
    fn hidden(&self, path: ast::Path<'db>) -> usize {
        let given = path.segment().and_then(|it| it.generic_arg_list()).is_some_and(|it| it.lifetime_args().next().is_some());
        if given {
            return 0;
        }
        let def = match self.cx.vsema.resolve_path(path) {
            Some(PathResolution::Def(ModuleDef::Adt(it))) => hir::GenericDef::Adt(it),
            Some(PathResolution::Def(ModuleDef::TypeAlias(it))) => hir::GenericDef::TypeAlias(it),
            Some(PathResolution::Def(ModuleDef::Trait(it))) => hir::GenericDef::Trait(it),
            _ => return 0,
        };
        def.lifetime_params(self.cx.db).len()
    }
}

/// The source of a lifetime written in a type, by where it is: `None` for a declaration and for
/// a `dyn Trait`'s bound (rustc's `LifetimeSource::Other`), and in a reference (which
/// [`Walk::ty`] reads at the reference).
fn source_of(lifetime: ast::Node<'_>) -> Option<Source> {
    let parent = lifetime.parent()?;
    match parent.kind() {
        SyntaxKind::LIFETIME_ARG => Some(Source::Path(Angle::Full)),
        SyntaxKind::USE_BOUND_GENERIC_ARGS => Some(Source::PreciseCapturing),
        SyntaxKind::TYPE_BOUND => {
            let list = parent.parent()?;
            match list.parent()?.kind() {
                SyntaxKind::IMPL_TRAIT_TYPE => Some(Source::OutlivesBound),
                _ => None,
            }
        }
        _ => None,
    }
}

fn syntax_of(lifetime: ast::Lifetime<'_>) -> Syntax {
    if lifetime.text() == "'_" { Syntax::ExplicitAnonymous } else { Syntax::ExplicitBound }
}

/// rustc's `emit_mismatch_diagnostic` of one group.
fn mismatch<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, inputs: &[&Info], outputs: &[&Info]) -> Found {
    let file = item.node.file_id;
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let mut bound: Option<&Info> = None;
    let mut to_bound: Vec<&Info> = Vec::new(); // alloc: the firing lint's suggestions
    let mut to_mixed_implicit: Vec<&Info> = Vec::new(); // alloc: as above
    let mut to_mixed_anonymous: Vec<&Info> = Vec::new(); // alloc: as above
    let mut to_implicit: Vec<&Info> = Vec::new(); // alloc: as above
    let mut to_anonymous: Vec<&Info> = Vec::new(); // alloc: as above
    let mut allow_implicit = true;
    let (mut saw_reference, mut saw_path) = (false, false);
    for &info in inputs.iter().chain(outputs) {
        if info.syntax == Syntax::ExplicitBound {
            bound = Some(info);
        }
        match (info.syntax, info.source) {
            (Syntax::Implicit, Source::Reference) => {
                to_anonymous.push(info);
                to_bound.push(info);
            }
            (Syntax::ExplicitAnonymous, Source::Reference) => {
                to_implicit.push(info);
                to_bound.push(info);
            }
            (Syntax::Implicit, Source::Path(_)) => {
                to_mixed_anonymous.push(info);
                to_anonymous.push(info);
                to_bound.push(info);
            }
            (Syntax::ExplicitAnonymous, _) => to_bound.push(info),
            (Syntax::ExplicitBound, Source::Reference) => {
                to_implicit.push(info);
                to_mixed_implicit.push(info);
                to_anonymous.push(info);
            }
            (Syntax::ExplicitBound, _) => {
                to_mixed_anonymous.push(info);
                to_anonymous.push(info);
            }
            (Syntax::Implicit, _) => {}
        }
        if !matches!(info.source, Source::Reference) {
            allow_implicit = false;
        }
        match info.source {
            Source::Reference => saw_reference = true,
            Source::Path(_) => saw_path = true,
            _ => {}
        }
    }
    let categorized = |infos: &[&Info]| {
        let mut by = [Vec::new(), Vec::new(), Vec::new()]; // alloc: the firing lint's labels
        for info in infos {
            let index = match info.category() {
                Category::Hidden => 0,
                Category::Elided => 1,
                Category::Named => 2,
            };
            by[index].push(info.reporting());
        }
        by
    };
    let [in_hidden, in_elided, in_named] = categorized(inputs);
    let [out_hidden, out_elided, out_named] = categorized(outputs);
    let counts = [
        in_hidden.len() + out_hidden.len(),
        in_elided.len() + out_elided.len(),
        in_named.len() + out_named.len(),
    ];
    let message = match counts {
        [_, _, 0] => "hiding a lifetime that's elided elsewhere is confusing",
        [_, 0, _] => "hiding a lifetime that's named elsewhere is confusing",
        [0, _, _] => "eliding a lifetime that's named elsewhere is confusing",
        _ => "hiding or eliding a lifetime that's named elsewhere is confusing",
    };
    // alloc: the firing lint's primary spans
    let primary: Vec<Span> = in_hidden.iter().chain(&in_elided).chain(&out_hidden).chain(&out_elided).map(|&it| span(it)).collect();
    let first = primary.first().cloned().unwrap_or_else(|| span(item.node.value.syntax().text_range()));
    let mut diag = Diag::lint("mismatched_lifetime_syntaxes", message, first);
    diag.span.primary = primary;
    for &it in &in_hidden {
        diag.span_label(span(it), "the lifetime is hidden here");
    }
    for &it in &in_elided {
        diag.span_label(span(it), "the lifetime is elided here");
    }
    for &it in &in_named {
        diag.span_label(span(it), "the lifetime is named here");
    }
    let mut hidden_counts: Vec<(TextRange, usize)> = Vec::new(); // alloc: the firing lint's labels
    for &it in &out_hidden {
        match hidden_counts.iter_mut().find(|(range, _)| *range == it) {
            Some((_, count)) => *count += 1,
            None => hidden_counts.push((it, 1)),
        }
    }
    for (range, count) in hidden_counts {
        let label = match count {
            1 => "the same lifetime is hidden here",
            _ => "the same lifetimes are hidden here",
        };
        diag.span_label(span(range), label);
    }
    for &it in &out_elided {
        diag.span_label(span(it), "the same lifetime is elided here");
    }
    for &it in &out_named {
        diag.span_label(span(it), "the same lifetime is named here");
    }
    diag.help("the same lifetime is referred to in inconsistent ways, making the signature confusing");

    let mut suggestions: Vec<(String, Vec<(TextRange, String)>)> = Vec::new(); // alloc: the firing lint's suggestions
    if let Some(bound) = bound {
        // alloc: the firing lint's suggestion
        suggestions.push((format!("consistently use `{}`", bound.name), for_lifetime(&bound.name, &to_bound)));
    }
    let mixed = saw_reference
        && saw_path
        && (!to_mixed_implicit.is_empty() || !to_mixed_anonymous.is_empty())
        && bound.is_none();
    if mixed {
        let message = match to_mixed_implicit.is_empty() {
            true => "use `'_` for type paths",
            false => "remove the lifetime name from references and use `'_` for type paths",
        };
        // alloc: the firing lint's suggestion
        let mut parts: Vec<(TextRange, String)> = to_mixed_implicit.iter().map(|it| (it.removing(), String::new())).collect();
        parts.extend(for_lifetime("'_", &to_mixed_anonymous));
        suggestions.push((message.to_owned(), parts)); // alloc: the firing lint's suggestion
    }
    if !to_implicit.is_empty() && allow_implicit && bound.is_none() {
        let parts = to_implicit.iter().map(|it| (it.removing(), String::new())).collect(); // alloc: the firing lint's suggestion
        suggestions.push(("remove the lifetime name from references".to_owned(), parts)); // alloc: the firing lint's suggestion
    }
    if !to_anonymous.is_empty() && !mixed && bound.is_none() {
        suggestions.push(("consistently use `'_`".to_owned(), for_lifetime("'_", &to_anonymous))); // alloc: the firing lint's suggestion
    }
    for (index, (message, parts)) in suggestions.into_iter().enumerate() {
        let parts = parts.into_iter().map(|(range, code)| (span(range), code)).collect(); // alloc: the firing lint's suggestion
        let (applicability, style) = match index {
            0 => (Applicability::MachineApplicable, SuggestionStyle::ShowAlways),
            _ => (Applicability::MaybeIncorrect, SuggestionStyle::CompletelyHidden),
        };
        diag.suggestion_with_style(parts, message, applicability, style);
    }
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(Fact::Signature(cx.key(id)), format!("the signature of `{name}`")); // alloc: the firing lint's fact
    }
    Found::lint(diag, Phase::Late, item.ptr())
}

/// rustc's `build_mismatch_suggestions_for_lifetime`: each lifetime written as `name`, the hidden
/// ones of one path together.
fn for_lifetime(name: &str, infos: &[&Info]) -> Vec<(TextRange, String)> {
    let mut parts = Vec::new(); // alloc: the firing lint's suggestion
    let mut paths: Vec<((TextRange, Angle), (TextRange, usize))> = Vec::new(); // alloc: the firing lint's suggestion
    let mut index: HashMap<(TextRange, Angle), usize> = HashMap::default();
    for info in infos {
        if let (Syntax::Implicit, Source::Path(angle)) = (info.syntax, info.source) {
            let at = match angle {
                Angle::Missing => TextRange::empty(info.ident.end()),
                Angle::Empty | Angle::Full => info.ident,
            };
            let key = (info.ty, angle);
            match index.entry(key) {
                Entry::Occupied(it) => paths[*it.get()].1.1 += 1,
                Entry::Vacant(it) => {
                    it.insert(paths.len());
                    paths.push((key, (at, 1)));
                }
            }
            continue;
        }
        parts.push(info.suggestion(name));
    }
    for ((_, angle), (at, count)) in paths {
        let repeated = vec![name; count].join(", "); // alloc: the firing lint's suggestion
        let code = match angle {
            Angle::Missing => format!("<{repeated}>"), // alloc: the firing lint's suggestion
            Angle::Empty => repeated,
            Angle::Full => format!("{repeated}, "), // alloc: the firing lint's suggestion
        };
        parts.push((at, code));
    }
    parts
}
