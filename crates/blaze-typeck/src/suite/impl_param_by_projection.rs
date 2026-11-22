//! An impl parameter bound only through a where-clause projection is decided by that clause
//! when the impl is selected to normalize an associated type. reth's shape (tempo
//! crates/zones/node/tests/it/utils.rs:2658, `.launch_with_debug_capabilities()`): reth
//! crates/node/builder/src/launch/debug.rs
//!
//! ```text
//! impl<L, Target, N, AddOns> LaunchNode<Target> for DebugNodeLauncher<L>
//! where L: LaunchNode<Target, Node = NodeHandle<N, AddOns>>, ..
//! { type Future = DebugNodeLauncherFuture<L, Target, N>; .. }
//! ```
//!
//! `N` and `AddOns` appear in no part of the header: only the projection clause names them.
//! rustc's selection of the impl instantiates them as inference variables and the nested
//! obligation `<L as LaunchNode<Target>>::Node == NodeHandle<?N, ?AddOns>` binds them (the
//! Reference, "Trait and lifetime bounds": the bounds must be satisfied when using the item),
//! so `Future` is a decided type. Left unbound, the call's type is "type not decided".
//!
//! Reduced:
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! struct H<N> { n: N }  struct Eng;  struct Dbg<L> { l: L }  struct F<L, T, N> { .. }
//! trait Launch<T> { type Node; type Fut; }
//! impl<T> Launch<T> for Eng { type Node = H<T>; type Fut = u8; }          // or H<u16>
//! impl<L, T, N> Launch<T> for Dbg<L> where L: Launch<T, Node = H<N>> { type Node = H<N>; type Fut = F<L, T, N>; }
//! fn go<T>(t: T) -> <Dbg<Eng> as Launch<T>>::Fut;
//! fn main(x: u8) { go(x); }
//! ```
//!
//! What the paths' omitted defaults do was checked against rustc for this shape too (no test
//! needed, both already agree): a TYPE path takes each omitted default with the parameters
//! before it put in (`type Future = DebugNodeLauncherFuture<L, Target, N>` is `<.., B =
//! DefaultDebugBlockProvider<N>>`: blaze-ladder front2/types.rs `with_defaults`); an
//! EXPRESSION path with no arguments written makes every parameter an inference variable
//! (rustc `CtorGenericArgsCtxt::inferred_kind`: `infer_args`; here rules/adt.rs `struct_path`,
//! `fresh_generic_args`), and one written short of parameters that have defaults takes them
//! (blaze-ladder front2/adt_paths.rs).

use std::vec::Vec;

use super::{Want, check_body, expect};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, PatId};
use crate::infer::InferPlant;
use crate::machine::Plant;

/// The world of the module doc; `eng_u16`: `Eng`'s `Node` is `H<u16>` instead of `H<T>`.
/// Returns it and `main`.
fn world(eng_u16: bool) -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1, p2) = (fx.param(0), fx.param(1), fx.param(2));
    let (u8_, u16_) = (fx.u8(), fx.u16());
    let unit = fx.unit();
    let h = fx.strukt("H", 1, &[("n", p0)]);
    let eng = fx.strukt("Eng", 0, &[]);
    let dbg = fx.strukt("Dbg", 1, &[("l", p0)]);
    let f = fx.strukt("F", 3, &[("l", p0), ("t", p1), ("n", p2)]);
    let launch = fx.tr("Launch", 2, Vec::new(), &["Node", "Fut"]);
    let eng_t = fx.adt(eng, &[]);
    let eng_node = if eng_u16 { fx.adt(h, &[u16_]) } else { fx.adt(h, &[p0]) };
    fx.imp("impl<T> Launch<T> for Eng", 1, eng_t, Some((launch, &[p0])), &[("Node", eng_node), ("Fut", u8_)], Vec::new());
    let dbg_l = fx.adt(dbg, &[p0]);
    let h_n = fx.adt(h, &[p2]);
    let f_ltn = fx.adt(f, &[p0, p1, p2]);
    let l_launches = fx.bound(launch, &[p0, p1]);
    let l_node = fx.proj(launch, &[p0, p1], "Node", h_n);
    fx.imp(
        "impl<L, T, N> Launch<T> for Dbg<L> where L: Launch<T, Node = H<N>>",
        3,
        dbg_l,
        Some((launch, &[p1])),
        &[("Node", h_n), ("Fut", f_ltn)],
        std::vec![l_launches, l_node],
    );
    let dbg_eng = fx.adt(dbg, &[eng_t]);
    let fut = fx.alias(launch, "Fut", &[dbg_eng, p0]);
    fx.func("go", 1, &[p0], fut, Vec::new());
    let main = fx.func("main", 0, &[u8_], unit, Vec::new());
    fx.finish();
    (fx, main)
}

/// `go(x);` in `main`; ids: callee 0, `x` 1, the call 2, the block 3.
fn go_call(eng_u16: bool, call_ty: &str) {
    let (fx, main) = world(eng_u16);
    let mut b = Bb::new();
    let x = b.param("x");
    let callee = b.path(&["go"]);
    let arg = b.local(x);
    let call = b.call(callee, &[arg]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "fn go<u8>"), (ExprId(1), "u8"), (ExprId(2), call_ty), (ExprId(3), "()")],
            pats: &[(PatId(0), "u8")],
            diags: &[],
        },
    );
}

/// `go(x)` is `F<Eng, u8, u8>`: `N` is decided by `<Eng as Launch<u8>>::Node == H<N>`.
#[test]
fn an_impl_parameter_bound_only_by_a_projection_clause_is_decided() {
    go_call(false, "F<Eng, u8, u8>");
}

/// The negative twin: with `Eng`'s `Node` `H<u16>` the same call is `F<Eng, u8, u16>`, so
/// `N` is the clause's answer, not `T` by coincidence (nor a default, nor left open).
#[test]
fn an_impl_parameter_bound_only_by_a_projection_clause_takes_the_clauses_answer() {
    go_call(true, "F<Eng, u8, u16>");
}
