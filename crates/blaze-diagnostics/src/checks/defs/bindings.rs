//! E0415 and E0416: a name bound twice in the parameters of a function or closure, or twice in
//! one pattern.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_params`,
//! `resolve_pattern_inner`, `fresh_binding`) and its
//! `IdentifierBoundMoreThanOnceInParameterList` and `IdentifierBoundMoreThanOnceInSamePattern`
//! (MIT or Apache-2.0).

use blaze_hash::{HashMap, HashSet};
use blaze_items::{Fact, ItemId};
use hir::{EditionedFileId, InFile, Name};
use hir_def::{
    DefWithBodyId,
    expr_store::{Body, ExprOrPatSource, ExpressionStore, HygieneId},
    hir::{Pat, PatId},
    nameres::demand::every_body,
};
use syntax::{
    SyntaxNodePtr, TextSize, ast as tag,
    view::{self as ast, AstNode as _, HasName as _},
};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(super) fn check(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: empty, allocates only when a name is bound twice
    let mut found = Vec::new();
    let mut scratch = Scratch::default();
    for owner in every_body(cx.db, cx.krate.base()) {
        check_body(cx, owner, &mut scratch, &mut found);
    }
    found
}

/// What [`check_body`] keeps of one body, cleared and reused for the next.
#[derive(Default)]
struct Scratch {
    parent: HashMap<PatId, PatId>,
    seen: HashSet<InFile<SyntaxNodePtr>>,
    roots: Vec<Root>,
    /// The product context of each parameter list, its parameters' names in it.
    lists: HashMap<InFile<SyntaxNodePtr>, Ctx>,
    ctxs: Vec<Ctx>,
}

/// The patterns of one body, each walked as rustc walks it: the parameters of a function or
/// closure share one product context, every other pattern starts its own.
fn check_body(cx: &Cx<'_, '_>, owner: DefWithBodyId, scratch: &mut Scratch, found: &mut Vec<Found>) {
    let (body, map) = Body::with_source_map(cx.db, owner);
    let source = |pat: PatId| map.pat_syntax(pat).ok();
    let Scratch { parent, seen, roots, lists, ctxs } = scratch;
    parent.clear();
    seen.clear();
    roots.clear();
    lists.clear();
    for (pat, _) in body.pats() {
        body.walk_pats_shallow(pat, |child| {
            parent.insert(child, pat);
        });
    }
    for (pat, _) in body.pats() {
        // A pattern the lowering wrote (`Some(<pat>)` of a `for` loop) is no context of rustc's.
        if parent.get(&pat).is_some_and(|&up| source(up).is_some()) {
            continue;
        }
        let Some(src) = source(pat) else { continue };
        if !seen.insert(src.map(|it| it.syntax_node_ptr())) {
            continue;
        }
        let node = src.value.cast::<tag::Pat>().map(|ptr| cx.node_view(src.with_value(ptr)));
        let list = node.and_then(|node| {
            let list = node.value.syntax().parent().filter(|it| ast::Param::can_cast(it.kind()))?;
            let list = list.parent().filter(|it| ast::ParamList::can_cast(it.kind()))?;
            Some(node.with_value(ast::ptr(list)))
        });
        let at = node.map(|node| {
            let at = node.with_value(node.value.syntax().text_range()).original_node_file_range_rooted(cx.db);
            (at.file_id, at.range.start())
        });
        roots.push(Root { pat, list, at });
    }
    roots.sort_by_key(|root| root.at);

    let item = ItemId::of_body(cx.db, owner);
    let owner = match hir::DefWithBody::from(owner) {
        hir::DefWithBody::Function(it) => hir::ModuleDef::Function(it),
        hir::DefWithBody::Static(it) => hir::ModuleDef::Static(it),
        hir::DefWithBody::Const(it) => hir::ModuleDef::Const(it),
        hir::DefWithBody::EnumVariant(it) => hir::ModuleDef::EnumVariant(it),
    };
    let mut walk = Walk { cx, body: &body.store, source: &source, item, owner, ctxs: std::mem::take(ctxs), found };
    for root in roots.iter() {
        walk.ctxs.clear();
        match root.list {
            Some(list) => {
                let ctx = lists.entry(list).or_insert_with(Ctx::product);
                walk.ctxs.push(std::mem::replace(ctx, Ctx::product()));
                walk.pat(root.pat, true);
                *ctx = walk.ctxs.pop().expect("a pattern's walk leaves the context it starts in");
            }
            None => {
                walk.ctxs.push(Ctx::product());
                walk.pat(root.pat, false);
            }
        }
    }
    *ctxs = walk.ctxs;
}

/// A pattern rustc resolves on its own or as one parameter of a list.
struct Root {
    pat: PatId,
    /// The parameter list the pattern is a parameter of.
    list: Option<InFile<SyntaxNodePtr>>,
    /// Where it is written in a file, for a pattern of a macro's expansion where its tokens are.
    at: Option<(EditionedFileId, TextSize)>,
}

/// rustc's `PatBoundCtx` with the names bound in it.
struct Ctx {
    product: bool,
    names: HashSet<(Name, HygieneId)>,
}

impl Ctx {
    fn product() -> Ctx {
        Ctx { product: true, names: HashSet::default() }
    }

    fn or() -> Ctx {
        Ctx { product: false, names: HashSet::default() }
    }
}

struct Walk<'a, 'cx, 'db, S> {
    cx: &'a Cx<'cx, 'db>,
    body: &'a ExpressionStore,
    source: &'a S,
    item: ItemId,
    owner: hir::ModuleDef,
    ctxs: Vec<Ctx>,
    found: &'a mut Vec<Found>,
}

impl<S: Fn(PatId) -> Option<ExprOrPatSource>> Walk<'_, '_, '_, S> {
    fn pat(&mut self, pat: PatId, param: bool) {
        let body = self.body;
        match body.pat(pat) {
            Pat::Or(alternatives) => {
                self.ctxs.push(Ctx::or());
                for &alternative in alternatives {
                    self.ctxs.push(Ctx::product());
                    self.pat(alternative, param);
                    self.pop();
                }
                self.pop();
            }
            Pat::Bind { id, subpat } => {
                if (self.source)(pat).is_some() {
                    let binding = &body[id];
                    // clone: an interned name, kept in the context's set
                    self.bind(pat, (binding.name.clone(), binding.hygiene), param);
                }
                if let Some(subpat) = subpat {
                    self.pat(subpat, param);
                }
            }
            _ => body.walk_pats_shallow(pat, |child| self.pat(child, param)),
        }
    }

    /// Ends the innermost context, moving its names to the one around it.
    fn pop(&mut self) {
        let ctx = self.ctxs.pop().expect("a pattern has a binding context");
        self.ctxs.last_mut().expect("a pattern has a binding context").names.extend(ctx.names);
    }

    /// rustc's `fresh_binding`: a name a product context around it already binds is an error.
    fn bind(&mut self, pat: PatId, key: (Name, HygieneId), param: bool) {
        if self.ctxs.iter().any(|ctx| ctx.product && ctx.names.contains(&key)) {
            self.report(pat, &key.0, param);
        }
        self.ctxs.last_mut().expect("a pattern has a binding context").names.insert(key);
    }

    fn report(&mut self, pat: PatId, name: &Name, param: bool) {
        let Some(src) = (self.source)(pat) else { return };
        let Some(ptr) = src.value.cast::<tag::Pat>() else { return };
        let node = self.cx.node_view(src.with_value(ptr));
        let ast::Pat::IdentPat(ident) = node.value else { return };
        let Some(ident) = ident.name() else { return };
        let span = self.cx.span(node.with_value(ident.syntax().text_range()));
        let name = name.as_str();
        let (code, message, label) = if param {
            (
                "E0415",
                // alloc: the error's message
                format!("identifier `{name}` is bound more than once in this parameter list"),
                "used as parameter more than once",
            )
        } else {
            (
                "E0416",
                // alloc: the error's message
                format!("identifier `{name}` is bound more than once in the same pattern"),
                "used in a pattern more than once",
            )
        };
        // clone: the error's span
        let mut diag = Diag::error(Some(code), message, span.clone());
        diag.span_label(span, label);
        // alloc: the error's fact
        let what = format!("the patterns of `{}`", self.cx.def_path(self.owner));
        diag.fact(Fact::Code(self.cx.key(self.item)), what);
        self.found.push(Found::new(diag, Phase::Resolve));
    }
}
