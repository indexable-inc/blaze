//! A simple query to collect tall locals (upvars) a closure use.

use base_db::Db;
use blaze_hash::{FixedState, HashMap};
use blaze_ids::radix;
use blaze_mem::frame::Frame;
use hir_expand::mod_path::{ModPath, PathKind};
use intern::{Symbol, sym};

use crate::{
    DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, VariantId,
    expr_store::{ExpressionStore, StoreVisitor, StoreVisitorExt, path::Path},
    hir::{BindingId, Expr, ExprId, PatId},
    resolver::{HasResolver, Resolver, ValueNs},
    type_ref::TypeRefId,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
// Kept sorted.
pub struct Upvars(Box<[BindingId]>);

impl Upvars {
    #[inline]
    pub fn contains(&self, local: BindingId) -> bool {
        self.0.binary_search(&local).is_ok()
    }

    #[inline]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = BindingId> {
        self.0.iter().copied()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline]
    pub fn as_ref(&self) -> UpvarsRef<'_> {
        UpvarsRef(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
// Kept sorted.
pub struct UpvarsRef<'db>(&'db [BindingId]);

impl UpvarsRef<'_> {
    #[inline]
    pub fn contains(self, local: BindingId) -> bool {
        self.0.binary_search(&local).is_ok()
    }

    #[inline]
    pub fn iter(self) -> impl ExactSizeIterator<Item = BindingId> {
        self.0.iter().copied()
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    #[inline]
    pub const fn empty() -> Self {
        UpvarsRef(&[])
    }
}

/// Returns a map from `Expr::Closure` to its upvars.
pub fn upvars_mentioned(
    db: &Db,
    owner: ExpressionStoreOwnerId,
) -> Option<&HashMap<ExprId, Upvars>> {
    return match owner {
        ExpressionStoreOwnerId::Signature(owner) => signature_upvars_mentioned(db, owner),
        ExpressionStoreOwnerId::Body(owner) => body_upvars_mentioned(db, owner),
        ExpressionStoreOwnerId::VariantFields(owner) => variant_fields_upvars_mentioned(db, owner),
    };

    #[blaze_query::tracked(returns(as_deref), mem_tag = Nameres)]
    pub fn signature_upvars_mentioned(
        db: &Db,
        owner: GenericDefId,
    ) -> Option<Box<HashMap<ExprId, Upvars>>> {
        upvars_mentioned_impl(db, owner.into())
    }

    #[blaze_query::tracked(returns(as_deref), mem_tag = Nameres)]
    pub fn body_upvars_mentioned(
        db: &Db,
        owner: DefWithBodyId,
    ) -> Option<Box<HashMap<ExprId, Upvars>>> {
        upvars_mentioned_impl(db, owner.into())
    }

    #[blaze_query::tracked(returns(as_deref), mem_tag = Nameres)]
    pub fn variant_fields_upvars_mentioned(
        db: &Db,
        owner: VariantId,
    ) -> Option<Box<HashMap<ExprId, Upvars>>> {
        upvars_mentioned_impl(db, owner.into())
    }
}

fn upvars_mentioned_impl(
    db: &Db,
    owner: ExpressionStoreOwnerId,
) -> Option<Box<HashMap<ExprId, Upvars>>> {
    let store = ExpressionStore::of(db, owner);
    store.expr_roots().next()?;
    // Most bodies have no closure: they need no resolver and no walk.
    if !store.has_closures() {
        return None;
    }
    blaze_mem::frame::current(|frame| {
        let mut binding_names = hashbrown::HashSet::with_hasher_in(FixedState, frame);
        binding_names.extend(store.bindings().map(|(_, binding)| *binding.name.symbol()));
        let mut visitor = UpvarsMentionedVisitor {
            db,
            resolver: owner.resolver(db),
            owner,
            store,
            binding_names,
            closures: Vec::new_in(frame),
            current_closure: None,
            mentioned: Vec::new_in(frame),
        };
        visitor.on_exprs(store.expr_roots());
        if visitor.closures.is_empty() {
            return None;
        }
        // alloc: the memo's map, exact
        let mut result =
            HashMap::with_capacity_and_hasher(visitor.closures.len(), FixedState);
        result.extend(visitor.closures.drain(..));
        // alloc: the memo's box, once per body with a capture
        Some(Box::new(result))
    })
}

struct UpvarsMentionedVisitor<'db, 'f> {
    db: &'db Db,
    resolver: Resolver<'db>,
    owner: ExpressionStoreOwnerId,
    store: &'db ExpressionStore,
    /// The names of the store's bindings: a path that is none of them names no local.
    binding_names: hashbrown::HashSet<Symbol, FixedState, &'f Frame>,
    /// Each closure with upvars and its upvars, in the order walked: a closure's `on_expr` is
    /// its one visit.
    closures: Vec<(ExprId, Upvars), &'f Frame>,
    /// The closure being walked, and where its upvars start in `mentioned`.
    current_closure: Option<(ExprId, usize)>,
    /// The upvars of every closure being walked, outermost first: one buffer for the body
    /// where each closure had a set.
    mentioned: Vec<BindingId, &'f Frame>,
}

impl UpvarsMentionedVisitor<'_, '_> {
    fn resolve_maybe_upvar(&mut self, expr: ExprId, path: Path<'_>) {
        let Some((current_closure, _)) = self.current_closure else { return };

        if let Path::BarePath(mod_path) = path
        && matches!(mod_path.kind, PathKind::Plain | PathKind::SELF)
        // `self` is length zero.
        && mod_path.segments().len() <= 1
        && self.may_name_binding(mod_path)
        {
            // Could be a variable.
            let guard = self.resolver.update_to_inner_scope(self.db, self.owner, expr);
            let resolution = self.resolver.resolve_path_in_value_ns_fully(
                self.db,
                path,
                self.store.expr_or_pat_path_hygiene(expr.into()),
            );
            if let Some(ValueNs::LocalBinding(local)) = resolution
                && self.store.binding_owner(local) != Some(current_closure)
            {
                self.mentioned.push(local);
            }
            self.resolver.reset_to_guard(guard);
        }
    }

    /// Whether `path`, of one segment at most, may resolve to a local: the resolver finds one
    /// only by its name, so a name no binding of the store has needs no resolving.
    fn may_name_binding(&self, path: &ModPath) -> bool {
        let name = match path.segments() {
            _ if path.is_self() => sym::self_,
            [name] => *name.symbol(),
            _ => return true,
        };
        self.binding_names.contains(&name)
    }

    /// Sorts and dedups the upvars `mentioned` has from `start` on.
    fn sort_dedup_from(&mut self, start: usize) {
        let upvars = &mut self.mentioned[start..];
        radix::sort_copy_by_key(upvars, |it| it.into_raw().into_u32());
        let mut kept = 0;
        for i in 0..upvars.len() {
            if kept == 0 || upvars[kept - 1] != upvars[i] {
                upvars[kept] = upvars[i];
                kept += 1;
            }
        }
        self.mentioned.truncate(start + kept);
    }
}

impl StoreVisitor for UpvarsMentionedVisitor<'_, '_> {
    fn on_expr(&mut self, expr: ExprId) {
        let store = self.store;
        match store.expr(expr) {
            Expr::Path(path) => {
                self.resolve_maybe_upvar(expr, path);
                self.on_path(path);
            }
            Expr::Closure {
                body: body_expr,
                args,
                arg_types,
                ret_type,
                capture_by: _,
                closure_kind: _,
            } => {
                self.on_pats(args);
                arg_types.iter().for_each(|arg_ty| self.on_type_opt(*arg_ty));
                self.on_type_opt(ret_type);

                let start = self.mentioned.len();
                let outer = self.current_closure.replace((expr, start));
                self.on_expr(body_expr);
                self.current_closure = outer;

                self.sort_dedup_from(start);
                if self.mentioned.len() > start {
                    // alloc: the closure's upvars in the memo, exact
                    self.closures.push((expr, Upvars(self.mentioned[start..].into())));
                }
                match outer {
                    // All nested closure's upvars are also upvars of the parent closure, but
                    // those the parent binds.
                    Some((outer_closure, _)) => {
                        let mut kept = start;
                        for i in start..self.mentioned.len() {
                            let local = self.mentioned[i];
                            if store.binding_owner(local) != Some(outer_closure) {
                                self.mentioned[kept] = local;
                                kept += 1;
                            }
                        }
                        self.mentioned.truncate(kept);
                    }
                    None => self.mentioned.truncate(start),
                }
            }
            _ => store.visit_expr_children(expr, self),
        }
    }

    fn on_anon_const_expr(&mut self, expr: ExprId) {
        // Anon consts don't contribute upvars.
        let old_current_closure = self.current_closure.take();
        self.on_expr(expr);
        self.current_closure = old_current_closure;
    }

    fn on_pat(&mut self, pat: PatId) {
        self.store.visit_pat_children(pat, self);
    }

    fn on_type(&mut self, ty: TypeRefId) {
        // Anon consts don't contribute upvars.
        let old_current_closure = self.current_closure.take();
        self.store.visit_type_ref_children(ty, &mut *self);
        self.current_closure = old_current_closure;
    }
}

#[cfg(test)]
mod tests {
    use expect_test::{Expect, expect};
    use hir_expand::fixture;
    use itertools::Itertools;
    use span::Edition;

    use crate::{
        AssocItemId, DefWithBodyId, ModuleDefId, expr_store::Body, nameres::crate_def_map,
        upvars::upvars_mentioned,
    };

    #[track_caller]
    fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, expectation: Expect) {
        let db = fixture::db(ra_fixture).into_db();
        let def_map = crate_def_map(&db, fixture::test_crate(&db));
        let func = def_map
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations())
            .filter_map(|decl| match decl {
                ModuleDefId::FunctionId(func) => Some(func),
                _ => None,
            })
            .chain(def_map.modules(&db).flat_map(|(_, module)| {
                module
                    .scope(&db)
                    .impls()
                    .flat_map(|impl_| &*impl_.impl_items(&db).items)
                    .filter_map(|&(_, item)| match item {
                        AssocItemId::FunctionId(it) => Some(it),
                        _ => None,
                    })
            }))
            .exactly_one()
            .unwrap_or_else(|_| panic!("expected one function"));
        let (body, source_map) = Body::with_source_map(&db, func.into());
        let Some(upvars) = upvars_mentioned(&db, DefWithBodyId::from(func).into()) else {
            expectation.assert_eq("");
            return;
        };
        let mut closures = Vec::new();
        for (&closure, upvars) in upvars {
            let closure_range = source_map.expr_syntax(closure).unwrap().value.text_range();
            let upvars = upvars
                .iter()
                .map(|local| body[local].name.display(&db, Edition::CURRENT))
                .join(", ");
            closures.push((closure_range, upvars));
        }
        closures.sort_unstable_by_key(|(range, _)| (range.start(), range.end()));
        let closures =
            closures.into_iter().map(|(range, upvars)| format!("{range:?}: {upvars}")).join("\n");
        expectation.assert_eq(&closures);
    }

    /// As [`check`], without the closures' ranges: each closure with upvars, in source order.
    #[track_caller]
    fn check_names(#[rust_analyzer::rust_fixture] ra_fixture: &str, expectation: Expect) {
        let db = fixture::db(ra_fixture).into_db();
        let def_map = crate_def_map(&db, fixture::test_crate(&db));
        let func = def_map
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations())
            .filter_map(|decl| match decl {
                ModuleDefId::FunctionId(func) => Some(func),
                _ => None,
            })
            .filter(|&func| upvars_mentioned(&db, DefWithBodyId::from(func).into()).is_some())
            .exactly_one()
            .unwrap_or_else(|_| panic!("expected one function with closures that capture"));
        let (body, source_map) = Body::with_source_map(&db, func.into());
        let upvars = upvars_mentioned(&db, DefWithBodyId::from(func).into()).unwrap();
        let mut closures: Vec<_> = upvars
            .iter()
            .map(|(&closure, upvars)| {
                let start = source_map.expr_syntax(closure).unwrap().value.text_range().start();
                let names = upvars
                    .iter()
                    .map(|local| body[local].name.display(&db, Edition::CURRENT))
                    .join(", ");
                (start, names)
            })
            .collect();
        closures.sort_unstable_by_key(|&(start, _)| start);
        expectation.assert_eq(&closures.into_iter().map(|(_, names)| names).join("\n"));
    }

    #[test]
    fn nested_three_deep_with_items() {
        check_names(
            r#"
fn bar() {}
struct Unit;
fn foo() {
    let a = 1;
    let bar2 = 2;
    || {
        let b = a;
        || {
            || { a; b; bar(); bar2; Unit };
        };
    };
}
        "#,
            expect![[r#"
                a, bar2
                a, bar2, b
                a, bar2, b"#]],
        );
    }

    #[test]
    fn shadowed_by_a_closure_local() {
        check_names(
            r#"
fn foo() {
    let x = 1;
    || { let x = 2; x };
    || x;
}
        "#,
            expect!["x"],
        );
    }

    #[test]
    fn simple() {
        check(
            r#"
struct foo;
fn foo(param: i32) {
    let local = "boo";
    || { param; foo };
    || local;
    || { param; local; param; local; };
    || 0xDEAFBEAF;
}
        "#,
            expect![[r#"
                60..77: param
                83..91: local
                97..131: param, local"#]],
        );
    }

    #[test]
    fn nested() {
        check(
            r#"
fn foo() {
    let (a, b);
    || {
        || a;
        || b;
    };
}
        "#,
            expect![[r#"
                31..69: a, b
                44..48: a
                58..62: b"#]],
        );
    }

    #[test]
    fn closure_var() {
        check(
            r#"
fn foo() {
    let upvar = 1;
    |closure_param: i32| {
        let closure_local = closure_param;
        closure_local + upvar
    };
}
        "#,
            expect!["34..135: upvar"],
        );
    }

    #[test]
    fn closure_var_nested() {
        check(
            r#"
fn foo() {
    let a = 1;
    |b: i32| {
        || {
            let c = 123;
            a + b + c
        }
    };
}
        "#,
            expect![[r#"
                30..116: a
                49..110: a, b"#]],
        );
    }

    #[test]
    fn self_upvar() {
        check(
            r#"
struct Foo(i32);
impl Foo {
    fn foo(&self) {
        || self.0;
    }
}
        "#,
            expect!["56..65: self"],
        );
    }

    #[test]
    fn const_block() {
        check(
            r#"
fn main() {
    || {
        const {
            let v = ();
            v
        }
    };
}
        "#,
            expect![""],
        );
    }

    #[test]
    fn async_fn_local() {
        check(
            r#"
async fn foo() {
    let v = ();
    v
}
        "#,
            expect![[r#""#]],
        );
    }
}
