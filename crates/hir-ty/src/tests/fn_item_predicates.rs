//! A method path given as a predicate resolves the adaptor's methods:
//! `.filter(Location::uses_memory).count()` over `.copied()` of an
//! `impl Iterator<Item = &Location>`, inside a `Format::uses_memory` of the same name, as
//! cranelift-assembler-x64-meta writes it (its MIR failed to lower,
//! `UnresolvedMethod("count")`, which stopped the build script). Its `Iterator` is core's
//! shape: `Copied` bounds `I: Iterator<Item = &'a T>`, `Filter` bounds
//! `P: FnMut(&I::Item) -> bool`, which `Location::uses_memory` meets only when the solver's
//! signature of it binds the lifetime `&self` leaves out (`lower::solver_fn_sig`).
//! On a failure the message has each method call with no resolution, and the types around it.

use hir_def::{
    AssocItemId, DefWithBodyId, ModuleDefId, expr_store::Body, hir::Expr, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult};

const FIXTURE: &str = r#"
//- minicore: sized, fn, option, clone, copy
//- /main.rs edition:2024
pub trait Iterator {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
    fn copied<'a, T>(self) -> Copied<Self>
    where
        Self: Sized + Iterator<Item = &'a T>,
        T: 'a + Copy,
    {
        Copied(self)
    }
    fn filter<P>(self, predicate: P) -> Filter<Self, P>
    where
        Self: Sized,
        P: FnMut(&Self::Item) -> bool,
    {
        Filter(self, predicate)
    }
    fn count(self) -> usize
    where
        Self: Sized,
    {
        0
    }
    fn find<P>(&mut self, _predicate: P) -> Option<Self::Item>
    where
        Self: Sized,
        P: FnMut(&Self::Item) -> bool,
    {
        None
    }
}
pub struct Copied<I>(I);
impl<'a, I, T: 'a> Iterator for Copied<I>
where
    I: Iterator<Item = &'a T>,
    T: Copy,
{
    type Item = T;
    fn next(&mut self) -> Option<T> {
        None
    }
}
pub struct Filter<I, P>(I, P);
impl<I: Iterator, P> Iterator for Filter<I, P>
where
    P: FnMut(&I::Item) -> bool,
{
    type Item = I::Item;
    fn next(&mut self) -> Option<I::Item> {
        None
    }
}
pub struct Iter<'a>(&'a Location);
impl<'a> Iterator for Iter<'a> {
    type Item = &'a Location;
    fn next(&mut self) -> Option<&'a Location> {
        None
    }
}

pub enum Location { Reg, Mem }
impl Clone for Location {
    fn clone(&self) -> Self {
        *self
    }
}
impl Copy for Location {}
impl Location {
    pub fn uses_memory(&self) -> bool {
        match self {
            Location::Reg => false,
            Location::Mem => true,
        }
    }
}

pub struct Format {
    location: Location,
}
macro_rules! check {
    ($e:expr) => {
        if $e {}
    };
}
impl Format {
    pub fn locations(&self) -> impl Iterator<Item = &Location> + '_ {
        Iter(&self.location)
    }
    pub fn uses_memory(&self) -> Option<Location> {
        check!(self.locations().copied().filter(Location::uses_memory).count() <= 1);
        self.locations().copied().find(Location::uses_memory)
    }
}
fn by_path(f: &Format) -> usize {
    f.locations().copied().filter(Location::uses_memory).count()
}
fn by_closure(f: &Format) -> usize {
    f.locations().copied().filter(|it| it.uses_memory()).count()
}
"#;

#[test]
fn a_method_path_predicate_resolves_the_adaptors_methods() {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let mut funcs = Vec::new();
        for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
            let scope = module.scope(&db);
            funcs.extend(scope.declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            }));
            for imp in scope.impls() {
                funcs.extend(imp.impl_items(&db).items.iter().filter_map(
                    |&(_, item)| match item {
                        AssocItemId::FunctionId(it) => Some(it),
                        _ => None,
                    },
                ));
            }
        }
        let mut failed = String::new();
        let mut checked = Vec::new();
        for func in funcs {
            let name = FunctionSignature::of(&db, func).name.as_str().to_owned();
            let def = DefWithBodyId::FunctionId(func);
            let body = Body::of(&db, def);
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            if !infer.diagnostics().is_empty() {
                failed.push_str(&format!("`{name}`: diagnostics {:?}\n", infer.diagnostics()));
            }
            for (id, expr) in body.exprs() {
                let Expr::MethodCall { receiver, method_name, .. } = expr else { continue };
                if infer.method_resolution(id).is_some() {
                    continue;
                }
                failed.push_str(&format!(
                    "`{name}`: `.{}` unresolved on {:?}, giving {:?}\n",
                    method_name.as_str(),
                    infer.expr_ty(receiver),
                    infer.expr_ty(id),
                ));
            }
            checked.push(name);
        }
        for name in ["uses_memory", "by_path", "by_closure"] {
            assert!(checked.iter().any(|it| it == name), "`{name}` was not checked: {checked:?}");
        }
        assert!(failed.is_empty(), "{failed}");
    });
}

/// blaze-image's `Builder::finish` (build.rs:93): `&crate::serial` handed where a
/// `&(dyn Fn(usize, &(dyn Fn(usize) + Sync)) + Sync)` is wanted. A parameter `&dyn Fn(usize)`
/// leaves out one lifetime, the reference's: the lifetimes of `Fn(..)` are the sugar's own and the
/// object's is its default, neither counted by rustc's elision. So the solver's signature of
/// `serial` binds it, `for<'a> fn(usize, &'a (dyn Fn(usize) + Sync + 'a))`, and the function
/// item meets `for<'a> Fn(usize, &'a (dyn Fn(usize) + Sync + 'a))`, as an unsizing to `dyn` and
/// as a bound. With the parameter's lifetimes left unknown the signature kept an error region,
/// and both were E0277, though `&usize` and an explicit `'x` passed.
const DYN_FN_PARAM: &str = r#"
//- minicore: sized, fn, coerce_unsized, sync
//- /main.rs edition:2024
pub type Jobs<'a> = &'a (dyn Fn(usize, &(dyn Fn(usize) + Sync)) + Sync);
pub fn serial(_n: usize, _job: &(dyn Fn(usize) + Sync)) {}
pub fn with_static(_n: usize, _job: &(dyn Fn(usize) + 'static)) {}
pub fn returns_it(job: &dyn Fn(usize)) -> &dyn Fn(usize) { job }
fn takes<F: Fn(usize, &(dyn Fn(usize) + Sync))>(_: F) {}
fn finish_on(_jobs: Jobs<'_>) {}
fn as_jobs() { finish_on(&serial) }
fn as_dyn() { let _j: &dyn Fn(usize, &(dyn Fn(usize) + Sync)) = &serial; }
fn as_static() { let _j: &dyn Fn(usize, &(dyn Fn(usize) + 'static)) = &with_static; }
fn as_bound() { takes(serial) }
fn through(job: &dyn Fn(usize)) { let _o: &dyn Fn(usize) = returns_it(job); }
"#;

#[test]
fn a_fn_item_with_a_reference_to_dyn_fn_parameter_meets_fn_of_every_lifetime() {
    let db = fixture::db(DYN_FN_PARAM).into_db();
    blaze_query::db::attach(&db, || {
        let mut failed = String::new();
        let mut checked = Vec::new();
        for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
            for def in module.scope(&db).declarations() {
                let ModuleDefId::FunctionId(func) = def else { continue };
                let name = FunctionSignature::of(&db, func).name.as_str().to_owned();
                let infer = InferenceResult::of(
                    &db,
                    InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)),
                );
                if !infer.diagnostics().is_empty()
                    || infer.has_type_mismatches()
                    || infer.is_erroneous()
                {
                    failed.push_str(&format!("`{name}`: diagnostics {:?}\n", infer.diagnostics()));
                }
                checked.push(name);
            }
        }
        for name in ["as_jobs", "as_dyn", "as_static", "as_bound", "through"] {
            assert!(checked.iter().any(|it| it == name), "`{name}` was not checked: {checked:?}");
        }
        assert!(failed.is_empty(), "{failed}");
    });
}

/// rustc_next_trait_solver's `probe_and_evaluate_goal_for_constituent_tys(..,
/// structural_traits::instantiate_constituent_tys_for_auto_trait)` (trait_goals.rs:257): a
/// function item whose parameters are `&EvalCtxt<'_, D>` and `I::Ty` handed to an
/// `impl Fn(&EvalCtxt<'_, D>, I::Ty)`. A parameter naming an associated type (`I::Ty`,
/// `<D as Delegate>::Interner`, or one in a default type argument written out) leaves out no
/// lifetime of its own, by rustc's `resolve_elided_lifetimes_in_path`: only the segment a path
/// resolves to elides its item's lifetimes. With such a parameter counted as unknown, the
/// solver's signature kept an error region for every `&` of the function, so the item never met
/// `for<'a> Fn(&'a ..)` and each call below was a false E0277, though `fn(&I, u8)` passed.
const ASSOC_TY_PARAM: &str = r#"
//- minicore: sized, fn
//- /main.rs edition:2024
pub trait Interner { type Ty; }
pub trait Delegate { type Interner: Interner; }
pub struct Ctxt<'a, D, I = <D as Delegate>::Interner>
where
    D: Delegate<Interner = I>,
    I: Interner,
{
    pub d: &'a D,
    pub i: *const I,
}
pub fn by_projection<I: Interner>(_x: &I, _t: I::Ty) {}
pub fn by_qualified<D: Delegate>(_x: &D, _t: <D as Delegate>::Interner) {}
pub fn by_default<D, I>(_c: &Ctxt<'_, D>, _t: I::Ty)
where
    D: Delegate<Interner = I>,
    I: Interner,
{
}
pub fn returns_first<I: Interner>(x: &I, _t: I::Ty) -> &I { x }
fn takes_projection<I: Interner, F: Fn(&I, I::Ty)>(_f: F) {}
fn takes_qualified<D: Delegate, F: Fn(&D, D::Interner)>(_f: F) {}
fn takes_returning<I: Interner, F: for<'a> Fn(&'a I, I::Ty) -> &'a I>(_f: F) {}
impl<D, I> Ctxt<'_, D>
where
    D: Delegate<Interner = I>,
    I: Interner,
{
    fn probe(&self, _f: impl Fn(&Ctxt<'_, D>, I::Ty)) {}
    fn as_default(&self) { self.probe(by_default) }
}
fn as_projection<I: Interner>() { takes_projection::<I, _>(by_projection::<I>) }
fn as_qualified<D: Delegate>() { takes_qualified::<D, _>(by_qualified::<D>) }
fn as_returning<I: Interner>() { takes_returning::<I, _>(returns_first::<I>) }
fn as_pointer<I: Interner>() { let _p: for<'a> fn(&'a I, I::Ty) -> &'a I = returns_first::<I>; }
"#;

#[test]
fn a_fn_item_with_an_associated_type_parameter_meets_fn_of_every_lifetime() {
    let db = fixture::db(ASSOC_TY_PARAM).into_db();
    blaze_query::db::attach(&db, || {
        let mut funcs = Vec::new();
        for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
            let scope = module.scope(&db);
            funcs.extend(scope.declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            }));
            for imp in scope.impls() {
                funcs.extend(imp.impl_items(&db).items.iter().filter_map(
                    |&(_, item)| match item {
                        AssocItemId::FunctionId(it) => Some(it),
                        _ => None,
                    },
                ));
            }
        }
        let mut failed = String::new();
        let mut checked = Vec::new();
        for func in funcs {
            let name = FunctionSignature::of(&db, func).name.as_str().to_owned();
            let def = DefWithBodyId::FunctionId(func);
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            if !infer.diagnostics().is_empty()
                || infer.has_type_mismatches()
                || infer.is_erroneous()
            {
                failed.push_str(&format!("`{name}`: diagnostics {:?}\n", infer.diagnostics()));
            }
            checked.push(name);
        }
        for name in ["as_default", "as_projection", "as_qualified", "as_returning", "as_pointer"] {
            assert!(checked.iter().any(|it| it == name), "`{name}` was not checked: {checked:?}");
        }
        assert!(failed.is_empty(), "{failed}");
    });
}
