//! An `async fn` passed where a blanket impl over `FnOnce(T1, .., Tn) -> Fut` is the only way
//! to meet the bound: the reduction of tempo's zone_rpc server.rs:63 (`.route("/",
//! post(handle_rpc))`, axum 0.8.9 handler/mod.rs `impl_handler!` over `all_the_tuples!`),
//! reported by blaze as "an unmet bound" on `FnDef(handle_rpc): Handler<(ViaRequest,
//! State<Arc<RpcState>>, HeaderMap, Bytes), Arc<RpcState>>`.
//!
//! ```text
//! #[lang = "fn_once"] trait FnOnce<Args> { type Output; }
//! #[lang = "future"] trait Future { type Output; }
//! trait IntoResponse {}                      impl IntoResponse for u8 {}
//! trait FromParts<S> {}                      trait FromReq<S, M> {}
//! enum ViaParts {}  enum ViaRequest {}
//! impl<S, T: FromParts<S>> FromReq<S, ViaParts> for T {}      // axum-core extract/mod.rs:91
//! struct Hdr; struct Body; struct Plain; struct St;
//! impl<S> FromParts<S> for Hdr {}                             // `HeaderMap`
//! impl<S> FromReq<S, ViaRequest> for Body {}                  // `Bytes` (`FromRequest<S>`, M defaulted)
//! trait Handler<T, S> {}
//! impl<F, Fut, S, Res, M, T1> Handler<(M, T1), S> for F
//!     where F: FnOnce(T1) -> Fut, Fut: Future<Output = Res>, Res: IntoResponse, T1: FromReq<S, M> {}
//! impl<F, Fut, S, Res, M, T1, T2> Handler<(M, T1, T2), S> for F
//!     where F: FnOnce(T1, T2) -> Fut, Fut: Future<Output = Res>, Res: IntoResponse,
//!           T1: FromParts<S>, T2: FromReq<S, M> {}
//! fn post<H: Handler<T, S>, T, S>(h: H, s: S) -> u8;
//! async fn handle(a: Hdr, b: Body) -> u8;        // its opaque: `Future<Output = u8>`
//! async fn by_parts(a: Hdr, b: Hdr) -> u8;       // the last one through `ViaParts`
//! async fn bad(a: Hdr, b: Plain) -> u8;          // no extractor for `Plain`
//! fn route(s: St) -> u8 { post(handle, s) }
//! fn route_parts(s: St) -> u8 { post(by_parts, s) }
//! fn route_bad(s: St) -> u8 { post(bad, s) }
//! ```
//!
//! The rule (the Reference, "Trait and lifetime bounds"; the next solver's impl candidates):
//! `T` is unknown at the call, so each impl whose header unifies with `Handler<?T, St>` for the
//! fn item is a candidate; the one-argument impl fails on `FnOnce<(T1,)>` (the fn item takes
//! two), the two-argument one holds: `Fut` is the projection's output (the opaque), `Res` its
//! `Future::Output` from the opaque's bounds (`u8`), and `M` is decided by the last argument's
//! one applicable `FromReq` impl (`Body`'s own, `ViaRequest`; for `Hdr` the `ViaParts`
//! blanket). So `route` and `route_parts` check clean, and `route_bad` is E0277 (`Plain` has
//! neither `FromReq` impl: the blanket's `Plain: FromParts<St>` fails).
//!
//! What this test settles for server.rs:63: candidates (a) the `FnOnce` projection with `Fut`
//! a variable, (b) `M` inferred through the last argument, (d) arity dispatch across two impls
//! of one trait. `handle_rpc` differs from the route beside it that blaze accepts
//! (`get(handle_ws_upgrade)`, the same `State`, `HeaderMap` and arity, its last argument
//! `WebSocketUpgrade` through `ViaParts`) only in its last extractor `Bytes`, a FOREIGN type
//! (crate `bytes`) with its own `FromRequest<S>` impl in axum-core, and in its body (the
//! future's `Send`, candidate (c), decided from its witness). If this test passes, (a), (b) and
//! (d) are cleared and the cause is in (c) or in the records of `Bytes`' impl.

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{List, LangItem};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// The world of the module doc.
fn world() -> Fx {
    let mut fx = Fx::new();
    // `Sized`: the fn-trait candidate requires `Output: Sized` (blaze-solve builtin.rs
    // `step_fn_trait`), and every generic parameter is implicitly `Sized`.
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1, p2, p3, p4, p5, p6) = (fx.param(0), fx.param(1), fx.param(2), fx.param(3), fx.param(4), fx.param(5), fx.param(6));
    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let future = fx.tr("Future", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let into_response = fx.tr("IntoResponse", 1, Vec::new(), &[]);
    let u8_ = fx.u8();
    fx.imp("impl IntoResponse for u8", 0, u8_, Some((into_response, &[])), &[], Vec::new());

    let from_parts = fx.tr("FromParts", 2, Vec::new(), &[]);
    let from_req = fx.tr("FromReq", 3, Vec::new(), &[]);
    let via_parts_k = fx.strukt("ViaParts", 0, &[]);
    let via_parts = fx.adt(via_parts_k, &[]);
    let via_request_k = fx.strukt("ViaRequest", 0, &[]);
    let via_request = fx.adt(via_request_k, &[]);
    // impl<S, T: FromParts<S>> FromReq<S, ViaParts> for T
    let t_parts = fx.bound(from_parts, &[p1, p0]);
    fx.imp("impl<S, T: FromParts<S>> FromReq<S, ViaParts> for T", 2, p1, Some((from_req, &[p0, via_parts])), &[], std::vec![t_parts]);

    let (hdr_k, body_k, plain_k, st_k) = (fx.strukt("Hdr", 0, &[]), fx.strukt("Body", 0, &[]), fx.strukt("Plain", 0, &[]), fx.strukt("St", 0, &[]));
    let (hdr, body, plain, st) = (fx.adt(hdr_k, &[]), fx.adt(body_k, &[]), fx.adt(plain_k, &[]), fx.adt(st_k, &[]));
    fx.imp("impl<S> FromParts<S> for Hdr", 1, hdr, Some((from_parts, &[p0])), &[], Vec::new());
    fx.imp("impl<S> FromReq<S, ViaRequest> for Body", 1, body, Some((from_req, &[p0, via_request])), &[], Vec::new());

    // trait Handler<T, S>; the impls' generics: F = 0, Fut = 1, S = 2, Res = 3, M = 4, T1 = 5,
    // T2 = 6.
    let handler = fx.tr("Handler", 3, Vec::new(), &[]);
    for arity in [1u32, 2] {
        let args = if arity == 1 { fx.tuple(&[p5]) } else { fx.tuple(&[p5, p6]) };
        let marked = if arity == 1 { fx.tuple(&[p4, p5]) } else { fx.tuple(&[p4, p5, p6]) };
        let last = if arity == 1 { p5 } else { p6 };
        let mut clauses = std::vec![
            fx.bound(fn_once, &[p0, args]),
            fx.proj(fn_once, &[p0, args], "Output", p1),
            fx.bound(future, &[p1]),
            fx.proj(future, &[p1], "Output", p3),
            fx.bound(into_response, &[p3]),
            fx.bound(from_req, &[last, p2, p4]),
        ];
        if arity == 2 {
            clauses.push(fx.bound(from_parts, &[p5, p2]));
        }
        let label = if arity == 1 { "impl Handler<(M, T1), S> for F" } else { "impl Handler<(M, T1, T2), S> for F" };
        fx.imp(label, 5 + arity, p0, Some((handler, &[marked, p2])), &[], clauses);
    }
    // fn post<H: Handler<T, S>, T, S>(h: H, s: S) -> u8
    let h_handler = fx.bound(handler, &[p0, p1, p2]);
    fx.func("post", 3, &[p0, p2], u8_, std::vec![h_handler]);

    // The three async fns' heads: each returns its own opaque, `impl Future<Output = u8>`.
    for (name, second) in [("handle", body), ("by_parts", hdr), ("bad", plain)] {
        let o = fx.opaque_key(&std::format!("{name}::{{opaque#0}}"));
        let opaque = fx.ty(PNode::Opaque(o, List::EMPTY));
        let is_future = fx.bound(future, &[opaque]);
        let out_u8 = fx.proj(future, &[opaque], "Output", u8_);
        // `Sized` is among an opaque's bounds unless `?Sized` is written (front2/heads.rs
        // `fn_head`, as suite/opaque.rs files it).
        let is_sized = fx.bound(sized, &[opaque]);
        fx.set_opaque_bounds(o, std::vec![is_future, out_u8, is_sized]);
        fx.func(name, 0, &[hdr, second], opaque, Vec::new());
    }
    for owner in ["route", "route_parts", "route_bad"] {
        fx.func(owner, 0, &[st], u8_, Vec::new());
    }
    fx.finish();
    fx
}

/// `fn <owner>(s: St) -> u8 { post(<handler>, s) }`: the diagnostics' codes.
fn route(fx: &Fx, owner: &str, handler: &str) -> Vec<DiagCode> {
    let mut b = Bb::new();
    let s = b.param("s");
    let post = b.path(&["post"]);
    let h = b.path(&[handler]);
    let s_e = b.local(s);
    let call = b.call(post, &[h, s_e]);
    let root = b.block(&[], Some(call));
    let c = check_body(fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code).collect()
}

/// `post(handle, s)`: the two-argument impl, `M = ViaRequest` from `Body`'s own impl.
#[test]
fn an_async_fn_meets_the_blanket_handler_impl_through_its_last_extractor() {
    let fx = world();
    assert_eq!(route(&fx, "route", "handle"), Vec::<DiagCode>::new());
}

/// `post(by_parts, s)`: `M = ViaParts` from the blanket over `FromParts` (the shape of the
/// `get(handle_ws_upgrade)` route blaze accepts).
#[test]
fn an_async_fn_meets_the_blanket_handler_impl_through_the_parts_blanket() {
    let fx = world();
    assert_eq!(route(&fx, "route_parts", "by_parts"), Vec::<DiagCode>::new());
}

/// Negative twin: `post(bad, s)`: `Plain` has no extractor impl, so no impl of `Handler`
/// holds: E0277.
#[test]
fn a_handler_whose_last_argument_has_no_extractor_is_unmet() {
    let fx = world();
    assert_eq!(route(&fx, "route_bad", "bad"), std::vec![DiagCode::Unsatisfied]);
}
