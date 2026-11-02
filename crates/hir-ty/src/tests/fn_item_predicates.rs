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
    crate::next_solver::attach_db(&db, || {
        let mut funcs = Vec::new();
        for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
            let scope = module.scope(&db);
            funcs.extend(scope.declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            }));
            for imp in scope.impls() {
                funcs.extend(imp.impl_items(&db).items.iter().filter_map(|&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                }));
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
