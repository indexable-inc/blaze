//! What each body of the base calls things by, so that an edit to the signatures of functions
//! alone looks at what names one of them rather than at all the code that can see the file.
//!
//! A body depends on the signature of a function it calls by name: a path to it, a method call
//! of its name (also one that resolved to another method before, which the new signature can
//! take the place of), or a path under another name that resolved to it. Its own signature is
//! written out, so what calls it depends on that alone. Not so for a body that the rest of the
//! program sees into: an `async` or `const` function, one that returns `impl Trait`, a constant
//! or a static. What names one of those depends on what it names, and so on.

use hir_def::{
    CallableDefId, DefWithBodyId, FunctionId,
    expr_store::Body,
    hir::{Expr, ExprOrPatId, type_ref::TypeRef},
    signatures::{ConstSignature, FunctionSignature, StaticSignature},
};
use hir_ty::{InferenceResult, db::HirDatabase, next_solver::TyKind};
use intern::Symbol;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::inherent::IntoKind as _;

use super::{
    Instance,
    home::mentioned_bodies,
    patch::{BaseInstance, FxIndexMap},
};

/// For each name, the roots and instances of the base whose bodies name it.
#[derive(Clone, Default)]
pub(super) struct Naming {
    /// Whether there is any: a base that is rustc's executable has none.
    pub(super) made: bool,
    roots: FxHashMap<Symbol, Vec<u32>>,
    instances: FxHashMap<Symbol, Vec<u32>>,
    /// The instances whose bodies can't be told, which any such edit can have changed.
    unknown: Vec<u32>,
    /// For each name, the bodies seen into that name it, by their own names, or `None` for
    /// one that has none.
    seen_into: FxHashMap<Symbol, Vec<Option<Symbol>>>,
}

/// What one body names.
struct Named {
    names: Box<[Symbol]>,
    /// Its own name, when it is seen into.
    seen_into: Option<Option<Symbol>>,
}

impl Naming {
    pub(super) fn new(
        db: &dyn HirDatabase,
        compiled: &[(FunctionId, String)],
        instances: &FxIndexMap<Instance, BaseInstance>,
    ) -> Naming {
        let mut naming = Naming { made: true, ..Naming::default() };
        let mut bodies: FxHashMap<DefWithBodyId, Named> = FxHashMap::default();
        let mut named = |naming: &mut Naming, body: DefWithBodyId| -> Box<[Symbol]> {
            let found = bodies.entry(body).or_insert_with(|| {
                let found = named(db, body);
                if let Some(own) = &found.seen_into {
                    for name in &found.names {
                        naming.seen_into.entry(name.clone()).or_default().push(own.clone());
                    }
                }
                found
            });
            found.names.clone()
        };
        for (i, &(root, _)) in compiled.iter().enumerate() {
            for name in named(&mut naming, root.into()) {
                naming.roots.entry(name).or_default().push(i as u32);
            }
        }
        for (i, instance) in instances.keys().enumerate() {
            let Some(of) = mentioned_bodies(db, instance) else {
                naming.unknown.push(i as u32);
                continue;
            };
            let mut names: Vec<Symbol> =
                of.into_iter().flat_map(|body| named(&mut naming, body)).collect();
            names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
            names.dedup();
            for name in names {
                naming.instances.entry(name).or_default().push(i as u32);
            }
        }
        naming
    }

    /// The names whose functions an edit of the signatures of `changed` can have changed
    /// what is compiled of their callers: those and what is seen into that names them. `None`
    /// when one of those has no name to be called by.
    pub(super) fn reach(&self, changed: &[Symbol]) -> Option<FxHashSet<Symbol>> {
        let mut reached: FxHashSet<Symbol> = changed.iter().cloned().collect();
        let mut queue = changed.to_vec();
        while let Some(name) = queue.pop() {
            for by in self.seen_into.get(&name).into_iter().flatten() {
                let by = by.as_ref()?;
                if reached.insert(by.clone()) {
                    queue.push(by.clone());
                }
            }
        }
        Some(reached)
    }

    /// The roots, by their index in the base, whose bodies name one of `names`.
    pub(super) fn roots<'a>(
        &'a self,
        names: &'a FxHashSet<Symbol>,
    ) -> impl Iterator<Item = u32> + 'a {
        names.iter().filter_map(|it| self.roots.get(it)).flatten().copied()
    }

    /// The instances, by their index in the base, whose bodies name one of `names` or can't
    /// be told.
    pub(super) fn instances<'a>(
        &'a self,
        names: &'a FxHashSet<Symbol>,
    ) -> impl Iterator<Item = u32> + 'a {
        let named = names.iter().filter_map(|it| self.instances.get(it)).flatten();
        named.chain(&self.unknown).copied()
    }
}

fn named(db: &dyn HirDatabase, body: DefWithBodyId) -> Named {
    let store = Body::of(db, body);
    let infer = InferenceResult::of(db, body);
    let mut names = Vec::new();
    for (id, expr) in store.exprs() {
        match expr {
            Expr::MethodCall { method_name, .. } => names.push(method_name.symbol().clone()),
            Expr::Path(path) => {
                if let Some(last) = path.segments().last() {
                    names.push(last.name.symbol().clone());
                }
                // A path can call it by another name `use` gave it.
                let ty = infer.type_of_expr_or_pat(ExprOrPatId::ExprId(id));
                if let Some(TyKind::FnDef(def, _)) = ty.map(|it| it.kind())
                    && let CallableDefId::FunctionId(func) = def.0
                {
                    names.push(FunctionSignature::of(db, func).name.symbol().clone());
                }
            }
            _ => {}
        }
    }
    let seen_into = match body {
        DefWithBodyId::FunctionId(func) => {
            let signature = FunctionSignature::of(db, func);
            names.push(signature.name.symbol().clone());
            let opaque =
                signature.store.types.iter().any(|(_, it)| matches!(it, TypeRef::ImplTrait(_)));
            (signature.is_async() || signature.is_const() || signature.is_gen() || opaque)
                .then(|| Some(signature.name.symbol().clone()))
        }
        DefWithBodyId::ConstId(id) => {
            let name = ConstSignature::of(db, id).name.as_ref().map(|it| it.symbol().clone());
            Some(name)
        }
        DefWithBodyId::StaticId(id) => {
            Some(Some(StaticSignature::of(db, id).name.symbol().clone()))
        }
        DefWithBodyId::VariantId(_) => Some(None),
    };
    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    names.dedup();
    Named { names: names.into(), seen_into }
}
