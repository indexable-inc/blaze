//! What resolve's late pass lints of lifetime parameters, given with its buffered lints
//! (`Phase::Resolve`), in the order the early lint pass reaches each parameter:
//!
//! - `unused_lifetimes`: a lifetime parameter of a function, an impl, a `for<..> fn` type or a
//!   `for<..>` of a `where` predicate that nothing names (not in what a derive expands to);
//! - `single_use_lifetimes`: one with no bounds that is named once, where `'_` (or nothing, in
//!   a `&'a T`) would mean the same: in a parameter's type of its own function or `fn` type, in
//!   the header of its own impl, or in a return type whose elided lifetime would be it.
//!
//! Structs, enums, unions, traits and type aliases are not linted (an unused parameter of
//! theirs is an error), nor `for<..>` of trait bounds and closures.
//!
//! Follows `compiler/rustc_resolve/src/late.rs` (`with_generic_param_rib`, `resolve_lifetime`:
//! the rib that decides a first use; `resolve_fn_params`, `find_lifetime_for_self`: elision)
//! and `compiler/rustc_resolve/src/late/diagnostics.rs` (`maybe_report_lifetime_uses`),
//! `compiler/rustc_resolve/src/diagnostics/mod.rs` (`UnusedLifetime`, `SingleUseLifetime`)
//! (MIT or Apache-2.0).

use blaze_items::Fact;
use hir::{InFile, ModuleDef, PathResolution};
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SmolStr, SyntaxKind, TextRange, T,
    view::{self as ast, AstNode, HasGenericArgs, HasGenericParams, HasTypeBounds},
};

use super::walk::{Item, Order, Orders};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// What introduces lifetimes resolve lints the uses of.
#[derive(Clone, Copy)]
enum Binder<'t> {
    Fn(ast::Fn<'t>),
    Impl(ast::Impl<'t>),
    FnPtr(ast::FnPtrType<'t>),
    Where,
}

/// What a lifetime in a function's parameters stands for, for elision: a name, inline up to
/// 23 bytes.
#[derive(Clone, PartialEq, Eq)]
enum Res {
    Named(SmolStr),
    /// An elided lifetime, each its own.
    Fresh(u32),
}

/// What the return type's elided lifetimes are: the one lifetime of the parameters (or of
/// `self`), or none.
type Elision = Option<Res>;

pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, items: &[Item<'db>]) -> Vec<Found> {
    let anonymous_in_impl_trait = super::feature(cx, "anonymous_lifetime_in_impl_trait");
    let mut found: Vec<(Order, Found)> = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    let mut lints = Lints { cx, anonymous_in_impl_trait, orders: Orders::default(), found: &mut found };
    for item in items {
        match item.node.value {
            ast::Item::Fn(f) => {
                if let Some(params) = f.generic_param_list() {
                    let span = params.syntax().text_range();
                    lints.binder(item, item.node.file_id, Binder::Fn(f), f.syntax(), params, span);
                }
            }
            ast::Item::Impl(imp) => {
                if let Some(params) = imp.generic_param_list() {
                    let span = params.syntax().text_range();
                    lints.binder(item, item.node.file_id, Binder::Impl(imp), imp.syntax(), params, span);
                }
            }
            _ => {}
        }
        for &own in item.nodes() {
            match own.value.kind() {
                SyntaxKind::FOR_TYPE => {
                    if let Some(ty) = ast::ForType::cast(own.value)
                        && let Some(ast::Type::FnPtrType(ptr)) = ty.ty()
                        && let Some(params) = ty.for_binder().and_then(|it| it.generic_param_list())
                    {
                        let span = TextRange::new(ty.syntax().text_range().start(), ptr.syntax().text_range().start());
                        lints.binder(item, own.file_id, Binder::FnPtr(ptr), ty.syntax(), params, span);
                    }
                }
                SyntaxKind::WHERE_PRED => {
                    if let Some(pred) = ast::WherePred::cast(own.value)
                        && let Some(params) = pred.for_binder().and_then(|it| it.generic_param_list())
                        && let Some(bounded) = pred.ty()
                    {
                        let span = TextRange::new(pred.syntax().text_range().start(), bounded.syntax().text_range().start());
                        lints.binder(item, own.file_id, Binder::Where, pred.syntax(), params, span);
                    }
                }
                _ => {}
            }
        }
    }
    found.sort_by(|(a, _), (b, _)| a.cmp(b));
    found.into_iter().map(|(_, it)| it).collect() // alloc: only what fired; nothing when none did
}

struct Lints<'c, 'a, 'db, 'f> {
    cx: &'c Cx<'a, 'db>,
    anonymous_in_impl_trait: bool,
    orders: Orders,
    found: &'f mut Vec<(Order, Found)>,
}

impl<'db> Lints<'_, '_, 'db, '_> {
    /// The lints of the lifetime parameters of `params`, which `binder` introduces for `scope`.
    /// A binder with none of its own (most have only types) costs one pass over `params`.
    fn binder(
        &mut self,
        item: &Item<'db>,
        file: hir::HirFileId,
        binder: Binder<'db>,
        scope: ast::Node<'db>,
        params: ast::GenericParamList<'db>,
        generics_span: TextRange,
    ) {
        let cx = self.cx;
        let linted = |param: ast::GenericParam<'_>| match param {
            ast::GenericParam::LifetimeParam(it) => it.lifetime().is_some_and(|it| !matches!(it.text(), "'_" | "'static")),
            _ => false,
        };
        if !params.generic_params().any(linted) {
            return;
        }
        // alloc: inline up to 4 parameters, and only for a binder with a lifetime parameter
        let all: SmallVec<[ast::GenericParam<'db>; 4]> = params.generic_params().collect();
        let derived = file.macro_file().is_some_and(|it| it.loc(cx.db).def.is_derive());
        let uses = uses(scope, binder);
        let mut elision: Option<Elision> = None;
        for (index, param) in all.iter().enumerate() {
            let ast::GenericParam::LifetimeParam(param) = param else { continue };
            let Some(lifetime) = param.lifetime() else { continue };
            let name = lifetime.text();
            if name == "'_" || name == "'static" {
                continue;
            }
            let mut named = uses.iter().copied().filter(|it| it.text() == name);
            let (first, second) = (named.next(), named.next());
            let span = |range: TextRange| cx.span(InFile::new(file, range));
            let at = InFile::new(file, ast::ptr(param.syntax()));
            let diag = match (first, second) {
                (None, _) => {
                    if derived {
                        continue;
                    }
                    let param_span = span(lifetime.syntax().text_range());
                    // alloc: the firing lint's message
                    let mut diag = Diag::lint("unused_lifetimes", format!("lifetime parameter `{name}` never used"), param_span);
                    if let Some(deletion) = deletion_span(&all, index, generics_span) {
                        diag.suggestion(span(deletion), "elide the unused lifetime", "", Applicability::MachineApplicable);
                    }
                    diag
                }
                (Some(token), None) => {
                    let bounded = param.type_bound_list().is_some_and(|it| it.bounds().next().is_some());
                    if bounded {
                        continue;
                    }
                    let elision = elision.get_or_insert_with(|| self.elision(binder));
                    if !self.single(binder, token, scope, name, elision) {
                        continue;
                    }
                    let param_span = span(lifetime.syntax().text_range());
                    let deletion = deletion_span(&all, index, generics_span);
                    let (use_range, replace) = match is_ref(token) {
                        true => (extended_over_whitespace(token), ""),
                        false => (token.text_range(), "'_"),
                    };
                    let message = format!("lifetime parameter `{name}` only used once"); // alloc: the firing lint's message
                    // clone: the firing lint's span, labelled too
                    let mut diag = Diag::lint("single_use_lifetimes", message, param_span.clone());
                    diag.span_label(param_span.clone(), "this lifetime..."); // clone: as above
                    diag.span_label(span(token.text_range()), "...is used only here");
                    if let Some(deletion) = deletion {
                        let mut parts = Vec::new(); // alloc: the firing lint's suggestion
                        if !deletion.is_empty() {
                            parts.push((span(deletion), String::new())); // alloc: the firing lint's suggestion
                        }
                        parts.push((span(use_range), replace.to_owned())); // alloc: the firing lint's suggestion
                        diag.multipart_suggestion("elide the single-use lifetime", parts, Applicability::MachineApplicable);
                    }
                    diag
                }
                (Some(_), Some(_)) => continue,
            };
            let mut diag = diag;
            if let Some((id, item_name)) = super::item_id(cx, item) {
                diag.fact(Fact::Signature(cx.key(id)), format!("the generics of `{item_name}`")); // alloc: the firing lint's fact
            }
            crate::lints::code_fact(cx, &mut diag, item.ptr());
            let key = self.orders.key(cx, item.module, cx.span(InFile::new(file, param.syntax().text_range())).lo);
            self.found.push((key, Found::lint(diag, Phase::Resolve, at)));
        }
    }

    /// Whether the one use `token` of the lifetime `name` of `binder` could be elided: the rib
    /// rustc's `resolve_lifetime` finds first, going out from the use, creates an anonymous
    /// parameter of the same binder, or elides to the same lifetime.
    fn single(&self, binder: Binder<'db>, token: ast::Token<'db>, scope: ast::Node<'db>, name: &str, elision: &Elision) -> bool {
        let Some(lifetime) = token.parent() else { return false };
        if !ast::Lifetime::can_cast(lifetime.kind()) {
            return false;
        }
        let own_ptr = match binder {
            Binder::FnPtr(ptr) => Some(ptr.syntax()),
            _ => None,
        };
        for node in lifetime.ancestors().take_while(|it| *it != scope) {
            if own_ptr == Some(node) {
                break;
            }
            let nested = match node.kind() {
                SyntaxKind::FN_PTR_TYPE | SyntaxKind::PARENTHESIZED_ARG_LIST => true,
                SyntaxKind::IMPL_TRAIT_TYPE => !self.anonymous_in_impl_trait,
                SyntaxKind::GENERIC_ARG_LIST => node.parent().is_some_and(|it| ast::AssocTypeArg::can_cast(it.kind())),
                SyntaxKind::CONST_ARG | SyntaxKind::WHERE_CLAUSE | SyntaxKind::GENERIC_PARAM_LIST => true,
                kind => ast::Expr::can_cast(kind) || ast::Pat::can_cast(kind),
            };
            if nested {
                return false;
            }
        }
        let within = |node: Option<ast::Node<'_>>| node.is_some_and(|it| it.text_range().contains_range(lifetime.text_range()));
        let returns = |ret: Option<ast::RetType<'_>>| {
            within(ret.as_ref().map(AstNode::syntax)) && matches!(elision, Some(Res::Named(it)) if it == name)
        };
        match binder {
            Binder::Fn(f) => within(f.param_list().as_ref().map(AstNode::syntax)) || returns(f.ret_type()),
            Binder::FnPtr(ptr) => within(ptr.param_list().as_ref().map(AstNode::syntax)) || returns(ptr.ret_type()),
            Binder::Impl(imp) => {
                within(imp.self_ty().as_ref().map(AstNode::syntax)) || within(imp.trait_().as_ref().map(AstNode::syntax))
            }
            Binder::Where => false,
        }
    }

    /// rustc's `resolve_fn_params`: what the return type of `binder`'s signature elides to.
    fn elision(&self, binder: Binder<'db>) -> Elision {
        let (list, impl_self) = match binder {
            Binder::Fn(f) => {
                let impl_self = f.syntax().ancestors().find_map(ast::Impl::cast).and_then(|it| it.self_ty());
                (f.param_list(), impl_self)
            }
            Binder::FnPtr(ptr) => (ptr.param_list(), None),
            Binder::Impl(_) | Binder::Where => return None,
        };
        let list = list?;
        let mut fresh = 0;
        // `self`'s lifetime, when it has one, is the return type's whatever the rest are.
        let mut distinct: SmallVec<[Res; 4]> = SmallVec::new();
        if let Some(param) = list.self_param() {
            match param.ty() {
                Some(ty) => self.self_lifetimes(ty, impl_self, &mut fresh, &mut distinct),
                None => match (param.amp_token(), param.lifetime()) {
                    (Some(_), Some(lifetime)) => distinct.push(named(lifetime, &mut fresh)),
                    (Some(_), None) => distinct.push(fresh_res(&mut fresh)),
                    _ => {}
                },
            }
            match distinct.len() {
                0 => {}
                1 => return distinct.pop(),
                _ => return None,
            }
        }
        // `None`: no parameter had lifetimes yet; `Some(None)`: elision failed.
        let mut elision: Option<Elision> = None;
        for param in list.params() {
            let Some(ty) = param.ty() else { continue };
            distinct.clear();
            self.candidates(ty, &mut fresh, &mut distinct);
            dedup(&mut distinct);
            if distinct.is_empty() {
                continue;
            }
            elision = match (elision, distinct.len()) {
                (None, 1) => Some(distinct.pop()),
                _ => Some(None),
            };
        }
        elision.flatten()
    }

    /// The lifetimes a parameter's type gives elision, each elided one its own: those named,
    /// `'_`, `&` without one and paths that leave theirs out; not in `fn` types, `Fn(..)`
    /// sugar or `impl Trait`, which elide on their own.
    fn candidates(&self, ty: ast::Type<'db>, fresh: &mut u32, found: &mut SmallVec<[Res; 4]>) {
        let mut walk = ty.syntax().preorder();
        while let Some(event) = walk.next() {
            let syntax::WalkEvent::Enter(node) = event else { continue };
            match node.kind() {
                SyntaxKind::FN_PTR_TYPE
                | SyntaxKind::PARENTHESIZED_ARG_LIST
                | SyntaxKind::IMPL_TRAIT_TYPE
                | SyntaxKind::CONST_ARG => walk.skip_subtree(),
                SyntaxKind::LIFETIME => {
                    if let Some(lifetime) = ast::Lifetime::cast(node) {
                        found.push(named(lifetime, fresh));
                    }
                }
                SyntaxKind::REF_TYPE => {
                    if ast::RefType::cast(node).is_some_and(|it| it.lifetime().is_none()) {
                        found.push(fresh_res(fresh));
                    }
                }
                SyntaxKind::PATH_TYPE => {
                    if let Some(path) = ast::PathType::cast(node).and_then(|it| it.path()) {
                        for _ in 0..self.elided_in_path(path) {
                            found.push(fresh_res(fresh));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The lifetimes a path leaves out: all of its type's, when it gives none.
    fn elided_in_path(&self, path: ast::Path<'db>) -> usize {
        let given = path
            .segment()
            .and_then(|it| it.generic_arg_list())
            .is_some_and(|it| it.lifetime_args().next().is_some());
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

    /// rustc's `find_lifetime_for_self`: the lifetimes of the references in `self`'s type whose
    /// referent names `Self` (or the impl's self type).
    fn self_lifetimes(&self, ty: ast::Type<'db>, impl_self: Option<ast::Type<'db>>, fresh: &mut u32, found: &mut SmallVec<[Res; 4]>) {
        let impl_adt = impl_self.and_then(|it| match it {
            ast::Type::PathType(p) => p.path().and_then(|p| self.cx.vsema.resolve_path(p)),
            _ => None,
        });
        let names_self = |node: ast::Node<'db>| {
            node.descendants().filter_map(ast::PathType::cast).filter_map(|it| it.path()).any(|path| {
                if path.qualifier().is_none() && path.segment().is_some_and(|it| it.self_type_token().is_some()) {
                    return true;
                }
                match (&impl_adt, self.cx.vsema.resolve_path(path)) {
                    (Some(PathResolution::Def(a)), Some(PathResolution::Def(b))) => *a == b,
                    _ => false,
                }
            })
        };
        for r in ty.syntax().descendants().filter_map(ast::RefType::cast) {
            let Some(inner) = r.ty() else { continue };
            if !names_self(inner.syntax()) {
                continue;
            }
            found.push(match r.lifetime() {
                Some(lifetime) => named(lifetime, fresh),
                None => fresh_res(fresh),
            });
        }
        dedup(found);
    }
}

fn named(lifetime: ast::Lifetime<'_>, fresh: &mut u32) -> Res {
    match lifetime.text() {
        "'_" => fresh_res(fresh),
        name => Res::Named(SmolStr::new(name)),
    }
}

fn fresh_res(fresh: &mut u32) -> Res {
    *fresh += 1;
    Res::Fresh(*fresh)
}

/// Keeps the first of each lifetime, in order.
fn dedup(res: &mut SmallVec<[Res; 4]>) {
    let mut kept = 0;
    for index in 0..res.len() {
        if !res[..kept].contains(&res[index]) {
            res.swap(kept, index);
            kept += 1;
        }
    }
    res.truncate(kept);
}

/// The lifetimes named in `scope` that may be uses of a parameter of `binder`: every lifetime
/// but the parameters' own names, labels and those in items nested in it (which cannot name
/// them), with those in macro calls' token trees (not labels there either).
fn uses<'t>(scope: ast::Node<'t>, binder: Binder<'_>) -> SmallVec<[ast::Token<'t>; 8]> {
    let assoc = matches!(binder, Binder::Impl(_));
    let mut found = SmallVec::new();
    let mut walk = scope.preorder_with_tokens();
    while let Some(event) = walk.next() {
        let syntax::WalkEvent::Enter(element) = event else { continue };
        let token = match element {
            NodeOrToken::Node(node) => {
                let nested = node != scope
                    && ast::Item::can_cast(node.kind())
                    && node.kind() != SyntaxKind::MACRO_CALL
                    && !(assoc && node.parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST));
                if nested {
                    walk.skip_subtree();
                }
                continue;
            }
            NodeOrToken::Token(token) => token,
        };
        if token.kind() != SyntaxKind::LIFETIME_IDENT {
            continue;
        }
        let Some(parent) = token.parent() else { continue };
        match parent.kind() {
            SyntaxKind::TOKEN_TREE => {
                if !is_label_in_tokens(token) {
                    found.push(token);
                }
            }
            SyntaxKind::LIFETIME => {
                let declared = parent.parent().is_some_and(|it| it.kind() == SyntaxKind::LIFETIME_PARAM)
                    && parent.prev_sibling().is_none();
                let label = parent.parent().is_some_and(|it| {
                    matches!(it.kind(), SyntaxKind::LABEL | SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR)
                });
                if !declared && !label {
                    found.push(token);
                }
            }
            _ => {}
        }
    }
    found
}

/// A lifetime token in a token tree that is a label: `'a: loop`, `break 'a`, `continue 'a`.
fn is_label_in_tokens(token: ast::Token<'_>) -> bool {
    let next = std::iter::successors(token.next_token(), |it| it.next_token()).find(|it| !it.kind().is_trivia());
    let prev = std::iter::successors(token.prev_token(), |it| it.prev_token()).find(|it| !it.kind().is_trivia());
    let labelled = next.is_some_and(|it| it.kind() == T![:])
        && std::iter::successors(next.and_then(|it| it.next_token()), |it| it.next_token())
            .find(|it| !it.kind().is_trivia())
            .is_some_and(|it| matches!(it.kind(), T![loop] | T![while] | T![for] | T!['{']));
    labelled || prev.is_some_and(|it| matches!(it.kind(), T![break] | T![continue]))
}

/// Whether a lifetime is that of a reference (`&'a T`, `&'a self`): rustc's
/// `LifetimeCtxt::Ref`, which elides to nothing.
fn is_ref(token: ast::Token<'_>) -> bool {
    let Some(parent) = token.parent().and_then(|it| it.parent()) else { return false };
    matches!(parent.kind(), SyntaxKind::REF_TYPE | SyntaxKind::SELF_PARAM)
}

/// A token's range with the white space after it: `span_extend_while_whitespace`.
fn extended_over_whitespace(token: ast::Token<'_>) -> TextRange {
    let mut end = token.text_range().end();
    let mut next = token.next_token();
    while let Some(it) = next {
        if it.kind() != SyntaxKind::WHITESPACE {
            break;
        }
        let text = it.text();
        let kept = text.len() - text.trim_start_matches([' ', '\t', '\n', '\r']).len();
        end += syntax::TextSize::from(kept as u32);
        if kept < text.len() {
            break;
        }
        next = it.next_token();
    }
    TextRange::new(token.text_range().start(), end)
}

/// rustc's `deletion_span` of the parameter at `index`: the whole `<..>` when it is the only
/// one, else it with the comma after it (the first) or before it.
fn deletion_span(params: &[ast::GenericParam<'_>], index: usize, generics_span: TextRange) -> Option<TextRange> {
    let range = |it: &ast::GenericParam<'_>| it.syntax().text_range();
    let inside = |it: TextRange| generics_span.contains_range(it).then_some(it);
    if params.len() == 1 {
        return Some(generics_span);
    }
    if index == 0 {
        let (param, next) = (inside(range(&params[0]))?, inside(range(params.get(1)?))?);
        return Some(TextRange::new(param.start(), next.start()));
    }
    let (param, prev) = (inside(range(&params[index]))?, inside(range(&params[index - 1]))?);
    Some(TextRange::new(prev.end(), param.end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(parse: &syntax::Parse<syntax::SourceFile>) -> (Vec<ast::GenericParam<'_>>, TextRange) {
        let list = parse.root().descendants().find_map(ast::GenericParamList::cast).expect("generics");
        (list.generic_params().collect(), list.syntax().text_range())
    }

    #[test]
    fn deletion_spans_take_a_comma() {
        let text = "fn f<'a, 'b, T>() {}";
        let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
        let (all, span) = params(&parse);
        assert_eq!(&text[deletion_span(&all, 0, span).unwrap()], "'a, ");
        assert_eq!(&text[deletion_span(&all, 1, span).unwrap()], ", 'b");
        let text = "fn f<'a>() {}";
        let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
        let (all, span) = params(&parse);
        assert_eq!(&text[deletion_span(&all, 0, span).unwrap()], "<'a>");
    }

    #[test]
    fn uses_skip_labels_declarations_and_nested_items() {
        let text = "fn f<'a, 'b: 'a>(x: &'a u8) { 'a: loop { break 'a; } fn g<'a>(_: &'a u8) {} m!('b: loop {}); m!(&'b u8) }";
        let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
        let f = parse.root().descendants().find_map(ast::Fn::cast).unwrap();
        let found: Vec<&str> = uses(f.syntax(), Binder::Fn(f)).iter().map(|it| it.text()).collect();
        assert_eq!(found, ["'a", "'a", "'b"]);
    }
}
