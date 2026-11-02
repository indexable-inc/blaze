//! An impl whose header leaves a lifetime out holds for every lifetime there, as rustc's
//! resolver makes each a fresh lifetime parameter of the impl: core's `impl_fn_for_zst!` writes
//! `impl Fn<(&u8,)> for IsAsciiWhitespace`, which `slice::Split`'s `P: FnMut(&T) -> bool` must
//! meet for `str::split_ascii_whitespace` and `SplitAsciiWhitespace`'s `next` and `size_hint`
//! to resolve their calls (the 00:03 self-build: cc's build-script dependencies refused their
//! MIR, `UnresolvedMethod`). Queue 01225, whose hir-def half is 01358. On a failure the message
//! has each body's diagnostics and each method call with no resolution.

use hir_def::{
    AssocItemId, DefWithBodyId, ModuleDefId, expr_store::Body, hir::Expr, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use rustc_type_ir::TypeVisitableExt;

use crate::{InferBodyId, InferenceResult};

const FIXTURE: &str = r#"
//- minicore: sized, fn, option, copy, slice
#![feature(unboxed_closures, fn_traits)]
pub trait Iterator {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
    fn filter<P>(self, predicate: P) -> Filter<Self, P>
    where
        Self: Sized,
        P: FnMut(&Self::Item) -> bool,
    {
        Filter { iter: self, predicate }
    }
    fn map<B, F>(self, f: F) -> Map<Self, F>
    where
        Self: Sized,
        F: FnMut(Self::Item) -> B,
    {
        Map { iter: self, f }
    }
}
pub struct Split<'a, T, P> {
    v: &'a [T],
    pred: P,
}
impl<'a, T, P> Iterator for Split<'a, T, P>
where
    P: FnMut(&T) -> bool,
{
    type Item = &'a [T];
    fn next(&mut self) -> Option<&'a [T]> {
        None
    }
}
pub struct Filter<I, P> {
    iter: I,
    predicate: P,
}
impl<I: Iterator, P> Iterator for Filter<I, P>
where
    P: FnMut(&I::Item) -> bool,
{
    type Item = I::Item;
    fn next(&mut self) -> Option<I::Item> {
        None
    }
}
pub struct Map<I, F> {
    iter: I,
    f: F,
}
impl<B, I: Iterator, F> Iterator for Map<I, F>
where
    F: FnMut(I::Item) -> B,
{
    type Item = B;
    fn next(&mut self) -> Option<B> {
        None
    }
}

macro_rules! impl_fn_for_zst {
    ($(
        $vis:vis struct $Name: ident impl$( <$( $lifetime : lifetime ),+> )? Fn =
            |$( $arg: ident: $ArgTy: ty ),*| -> $ReturnTy: ty
            $body: block;
    )+) => {
        $(
            $vis struct $Name;

            impl $( <$( $lifetime ),+> )? Fn<($( $ArgTy, )*)> for $Name {
                extern "rust-call" fn call(&self, ($( $arg, )*): ($( $ArgTy, )*)) -> $ReturnTy {
                    $body
                }
            }

            impl $( <$( $lifetime ),+> )? FnMut<($( $ArgTy, )*)> for $Name {
                extern "rust-call" fn call_mut(
                    &mut self,
                    ($( $arg, )*): ($( $ArgTy, )*)
                ) -> $ReturnTy {
                    Fn::call(&*self, ($( $arg, )*))
                }
            }

            impl $( <$( $lifetime ),+> )? FnOnce<($( $ArgTy, )*)> for $Name {
                type Output = $ReturnTy;

                extern "rust-call" fn call_once(self, ($( $arg, )*): ($( $ArgTy, )*)) -> $ReturnTy {
                    Fn::call(&self, ($( $arg, )*))
                }
            }
        )+
    }
}
impl_fn_for_zst! {
    pub struct IsAsciiWhitespace impl Fn = |byte: &u8| -> bool {
        *byte == 32
    };

    pub struct BytesIsNotEmpty impl<'a, 'b> Fn = |s: &'a &'b [u8]| -> bool {
        true
    };

    pub struct UnsafeBytesToStr impl<'a> Fn = |bytes: &'a [u8]| -> &'a [u8] {
        bytes
    };
}

pub struct SplitAsciiWhitespace<'a> {
    inner: Map<Filter<Split<'a, u8, IsAsciiWhitespace>, BytesIsNotEmpty>, UnsafeBytesToStr>,
}
fn split(v: &[u8]) -> Split<'_, u8, IsAsciiWhitespace> {
    Split { v, pred: IsAsciiWhitespace }
}
fn split_ascii_whitespace(v: &[u8]) -> SplitAsciiWhitespace<'_> {
    let inner = split(v).filter(BytesIsNotEmpty).map(UnsafeBytesToStr);
    SplitAsciiWhitespace { inner }
}
impl<'a> Iterator for SplitAsciiWhitespace<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        self.inner.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

pub trait Pred<A> {}
impl Pred<&u8> for IsAsciiWhitespace {}
fn need<P: for<'r> Pred<&'r u8>>(_: P) {}
fn plain() {
    need(IsAsciiWhitespace)
}
"#;

#[test]
fn an_elided_impl_header_lifetime_holds_for_every_lifetime() {
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
                    "`{name}`: `.{}` unresolved on {:?}\n",
                    method_name.as_str(),
                    infer.expr_ty(receiver),
                ));
            }
            checked.push(name);
        }
        for name in ["split_ascii_whitespace", "next", "size_hint", "plain"] {
            assert!(checked.iter().any(|it| it == name), "`{name}` was not checked: {checked:?}");
        }
        assert!(failed.is_empty(), "{failed}");
    });
}

/// The proc-macro bridge's `Decode` impls (`rpc.rs`, `client.rs`), each leaving its header's
/// lifetimes out, and `run_client`'s closure calling `<(ExpnGlobals<Span>, A)>::decode`: the
/// 00:49 self-run's serde derives stopped there, the callee typeless (queue 01481).
const BRIDGE: &str = r#"
//- minicore: sized, fn, copy, slice, index, range, deref, coerce_unsized
pub trait Decode<'a, 's, S>: Sized {
    fn decode(r: &mut &'a [u8], s: &'s mut S) -> Self;
}
impl<S> Decode<'_, '_, S> for () {
    fn decode(_: &mut &[u8], _: &mut S) -> Self {}
}
impl<'a, S, A: for<'s> Decode<'a, 's, S>, B: for<'s> Decode<'a, 's, S>> Decode<'a, '_, S> for (A, B) {
    fn decode(r: &mut &'a [u8], s: &mut S) -> Self {
        (Decode::decode(r, s), Decode::decode(r, s))
    }
}
macro_rules! rpc_encode_decode {
    (le $ty:ty) => {
        impl<S> Decode<'_, '_, S> for $ty {
            fn decode(r: &mut &[u8], _: &mut S) -> Self {
                let x = r[0] as $ty;
                *r = &r[1..];
                x
            }
        }
    };
    (struct $name:ident <$T:ident> { $($field:ident),* }) => {
        impl<'a, S, $T: for<'s> Decode<'a, 's, S>> Decode<'a, '_, S> for $name<$T> {
            fn decode(r: &mut &'a [u8], s: &mut S) -> Self {
                $name { $($field: Decode::decode(r, s)),* }
            }
        }
    };
}
macro_rules! compound_traits {
    ($($t:tt)*) => { rpc_encode_decode!($($t)*); };
}
rpc_encode_decode!(le u32);
pub struct Handle(u32);
impl<S> Decode<'_, '_, S> for Handle {
    fn decode(r: &mut &[u8], s: &mut S) -> Self {
        Handle(u32::decode(r, s))
    }
}
pub struct Span {
    handle: Handle,
}
impl<S> Decode<'_, '_, S> for Span {
    fn decode(r: &mut &[u8], s: &mut S) -> Self {
        Span { handle: Handle::decode(r, s) }
    }
}
pub struct ExpnGlobals<Span> {
    pub def_site: Span,
    pub call_site: Span,
    pub mixed_site: Span,
}
compound_traits!(struct ExpnGlobals<Span> { def_site, call_site, mixed_site });
pub struct Buffer {
    data: [u8; 4],
}
impl core::ops::Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}
impl Buffer {
    fn take(&mut self) -> Buffer {
        Buffer { data: self.data }
    }
}
pub struct BridgeConfig<'a> {
    pub input: Buffer,
    pub dispatch: &'a (),
}
fn catch_unwind<R, F: FnOnce() -> R>(f: F) -> R {
    f()
}
pub fn run_client<A: for<'a, 's> Decode<'a, 's, ()>>(
    config: BridgeConfig<'_>,
    f: impl FnOnce(A) -> u32,
) -> u32 {
    let BridgeConfig { input: mut buf, dispatch } = config;
    let res = catch_unwind(|| {
        let reader = &mut &buf[..];
        let (globals, input) = <(ExpnGlobals<Span>, A)>::decode(reader, &mut ());
        let cached = buf.take();
        let _kept = (globals, cached, dispatch);
        f(input)
    });
    buf = Buffer { data: [0; 4] };
    let _cleared = buf;
    res
}
"#;

#[test]
fn the_bridges_decode_impls_resolve_for_run_client() {
    let db = fixture::db(BRIDGE).into_db();
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
        // Inference leaves the regions of an expression's type as errors; a type error is what
        // is left once they are erased.
        let interner = crate::next_solver::DbInterner::new_no_crate(&db);
        let mut failed = String::new();
        let mut checked = Vec::new();
        for func in funcs {
            let name = FunctionSignature::of(&db, func).name.as_str().to_owned();
            let def = DefWithBodyId::FunctionId(func);
            let body = Body::of(&db, def);
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            if infer.has_errors || !infer.diagnostics().is_empty() {
                failed.push_str(&format!(
                    "`{name}`: has_errors {}, diagnostics {:?}\n",
                    infer.has_errors,
                    infer.diagnostics(),
                ));
            }
            for (id, expr) in body.exprs() {
                let ty = infer.expr_ty(id);
                if crate::providers::erase_regions(interner, ty).references_error() {
                    failed.push_str(&format!("`{name}`: expr {id:?} has an error type {ty:?}: {expr:?}\n"));
                }
            }
            checked.push(name);
        }
        assert!(checked.iter().any(|it| it == "run_client"), "`run_client` was not checked");
        assert!(failed.is_empty(), "{failed}");
    });
}
