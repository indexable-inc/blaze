//! `#[track_caller]` (walk/track_caller.rs): the hidden last parameter, made at an untracked
//! caller and forwarded by a tracked one.
//!
//! The world (keys in brackets):
//! ```text
//! struct Location { filename: &str, line: u32, col: u32 }   // [20], `Adts::location_adt`
//! fn main(x: u32) -> u32 { callee(x) }                       // [1], tracked or not by case
//! #[track_caller] fn callee(x: u32) -> u32;                  // [3]
//! ```
//! The Reference ("The `track_caller` attribute"): the attribute "allows code within the
//! function to get a hint of the Location of the top-most tracked call that led to the
//! function's invocation". So an untracked `main`'s call of `callee` passes a `Location` of the
//! call's place, and a tracked `main` passes its own, which it took as its last parameter.

use super::*;

const LOCATION: ItemKey = ItemKey(20);
const CALLEE: ItemKey = ItemKey(3);

/// `main` (tracked when `tracked_main`) calling `callee(x)`, built.
fn build_main(tracked_main: bool) -> Result<Built<Global>, String> {
    let mut w = TestWorld::new();
    let str_ref = w.ref_(Mutbl::Not, PNode::Str);
    w.strukt(20, "Location", &[("filename", str_ref), ("line", U32), ("col", U32)]);
    w.location = Some(LOCATION);
    w.func(1, "main", &[U32], U32);
    w.func(3, "callee", &[U32], U32);
    w.tracked.push(CALLEE);
    if tracked_main {
        w.tracked.push(MAIN);
    }
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    b.params.push(x);
    let x_e = b.local(x);
    let root = b.call("callee", &[x_e]);
    build(&w, &b, root)
}

/// An untracked `main`: its call of `callee` passes a `Location` row of [`TRACK_SITE`]
/// (item-relative: file, item, offset), whose line and column are two source positions of
/// that site, their bytes zero (never an absolute line). A tracked `main`: it takes the pointer
/// as its own last parameter and passes it on with no row of its own.
///
/// A builder that writes the row with a line fails the zero-bytes `assert`; one without the
/// hidden parameter fails a call-signature `assert` (the call is `(i32) -> i32`); one that
/// makes a row in a tracked fn instead of forwarding fails the last.
/// Prints `track caller: an untracked caller passes a row of two source positions, a tracked one forwards its own`.
#[test]
fn a_tracked_call_passes_its_location_last() {
    let main = build_main(false).unwrap_or_else(|e| panic!("{e}"));
    let view = main.func.view();
    let text = blaze_lir::print(view);
    assert!(has_call_sig(&main, &[Ty::I32, Ty::PTR], &[Ty::I32]), "{text}");
    let with = (0..view.datas.len() as u32).map(blaze_lir::func::DataId).find(|&d| !view.data_positions_of(d).is_empty());
    let row = with.unwrap_or_else(|| panic!("no `Location` row with source positions: {text}"));
    let positions = view.data_positions_of(row);
    assert_eq!(positions.len(), 2, "a line and a column: {text}");
    for p in positions {
        assert_eq!((p.item_key(), p.site), (TRACK_SITE.item.0, TRACK_SITE.offset), "{text}");
        let bytes = view.data_bytes_of(row);
        assert_eq!(&bytes[p.offset as usize..p.offset as usize + 4], &[0; 4], "no absolute line or column: {text}");
    }

    let main = build_main(true).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(main.func.view());
    assert_eq!(own_sig(&main), (vec_of(&[Ty::I32, Ty::PTR]), vec_of(&[Ty::I32])), "{text}");
    assert!(has_call_sig(&main, &[Ty::I32, Ty::PTR], &[Ty::I32]), "{text}");
    assert!(main.func.view().datas.is_empty(), "a tracked fn forwards, it makes no row: {text}");
    println!("track caller: an untracked caller passes a row of two source positions, a tracked one forwards its own");
}

/// `FnMut::call_mut`, the item a `Fn*` shim's body is named by (`Adts::fn_trait_method`).
const CALL_MUT: ItemKey = ItemKey(21);
/// Where `call_mut` is defined (`Adts::def_site`): core's `ops/function.rs`, item-relative.
const CALL_MUT_SITE: crate::CallerSite<'static> = crate::CallerSite { file: "core/src/ops/function.rs", item: CALL_MUT, offset: 52 };

/// The `FnMut` shim of fn item `callee` (tracked when `tracked`; `def_site` answered when
/// `site`): clap 4.5's `.map(unwrap_downcast_into::<T>)` (arg_matches.rs:1893), a tracked fn
/// passed where `F: FnMut(..)` is bound, reduced to `callee(x: u32) -> u32`.
fn build_shim(tracked: bool, site: bool) -> Result<Built<Global>, String> {
    let mut w = TestWorld::new();
    let str_ref = w.ref_(Mutbl::Not, PNode::Str);
    w.strukt(20, "Location", &[("filename", str_ref), ("line", U32), ("col", U32)]);
    w.location = Some(LOCATION);
    w.func(3, "callee", &[U32], U32);
    if tracked {
        w.tracked.push(CALLEE);
    }
    w.fn_trait_method = Some(CALL_MUT);
    w.def_site = site.then_some(CALL_MUT_SITE);
    let mut ground = PortableTys::new_in(Alloc::global());
    let u32_ = ground.intern(U32).map_err(|_| String::from("intern"))?;
    let inputs = ground.intern_list(&[u32_]).map_err(|_| String::from("intern"))?;
    let tuple = ground.intern(PNode::Tuple(inputs)).map_err(|_| String::from("intern"))?;
    let callee = ground.intern(PNode::FnDef(CALLEE, List::EMPTY)).map_err(|_| String::from("intern"))?;
    let sig = w.sig(CALLEE).ok_or_else(|| String::from("callee's signature"))?;
    let body = BodyBuf::new();
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let built = crate::build_fn_shim(callee, tuple, crate::FnTrait::FnMut, Some(&sig), &mut subst, &TestAdts { world: &w, body: &body }, Global)
        .map_err(|error| format!("{error}"))?;
    blaze_lir::verify(built.func.view()).map_err(|error| format!("does not verify: {error}"))?;
    Ok(built)
}

/// rustc_mir_transform shim.rs `build_call_shim`: a fn item's `Fn*` shim calls the fn at a
/// span that is `tcx.def_span` of the `Fn*` method, and the shim is not tracked (rustc_middle
/// instance.rs `requires_caller_location`: `false` for a `FnPtr` shim), so a tracked fn
/// called through it is passed a `Location` of that definition: a row whose two source
/// positions are `def_site`'s (item-relative, zero bytes), passed LAST.
///
/// The twin: an untracked fn item's shim passes nothing and makes no row. The control: with
/// no `def_site` the shim is refused by name (the tracked fn's item), never built with no
/// location (which would be a call of the wrong signature) or with another place. Before
/// this change every tracked fn item's shim was refused ("the shim ... is not built yet":
/// 35 of tempo's tests NOT RUN at t1800).
/// Prints `track caller: a fn item's Fn* shim passes its Fn* method's definition site`.
#[test]
fn a_fn_shim_passes_its_fn_methods_definition_site() {
    let shim = build_shim(true, true).unwrap_or_else(|e| panic!("{e}"));
    let view = shim.func.view();
    let text = blaze_lir::print(view);
    assert!(has_call_sig(&shim, &[Ty::I32, Ty::PTR], &[Ty::I32]), "the location passed last: {text}");
    let rows: Vec<_, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend((0..view.datas.len() as u32).map(blaze_lir::func::DataId).filter(|&d| !view.data_positions_of(d).is_empty()));
        v
    };
    assert_eq!(rows.len(), 1, "one `Location` row: {text}");
    let positions = view.data_positions_of(rows[0]);
    assert_eq!(positions.len(), 2, "a line and a column: {text}");
    for p in positions {
        assert_eq!((p.item_key(), p.site), (CALL_MUT_SITE.item.0, CALL_MUT_SITE.offset), "the `Fn*` method's site: {text}");
    }

    let plain = build_shim(false, true).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(plain.func.view());
    assert!(has_call_sig(&plain, &[Ty::I32], &[Ty::I32]), "{text}");
    assert!(!has_call_sig(&plain, &[Ty::I32, Ty::PTR], &[Ty::I32]), "an untracked fn is passed no location: {text}");
    assert!(plain.func.view().datas.is_empty(), "no row for an untracked fn: {text}");

    let Err(refused) = build_shim(true, false) else { panic!("a tracked fn's shim with no definition site must be refused") };
    assert!(refused.contains("`Adts::def_site`"), "{refused}");
    println!("track caller: a fn item's Fn* shim passes its Fn* method's definition site");
}

/// Where `callee` is defined (`Adts::def_site`): item-relative.
const CALLEE_DEF_SITE: crate::CallerSite<'static> = crate::CallerSite { file: "src/lib.rs", item: CALLEE, offset: 17 };

/// Builds `ReifyShim` for `CALLEE` (`fn(u32) -> u32`) using `blaze_own::testing::heap().alloc()`.
fn build_reify(tracked: bool, site: bool) -> Result<Built<blaze_own::Alloc<'static>>, crate::BuildError> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut w = TestWorld::new();
    let str_ref = w.ref_(Mutbl::Not, PNode::Str);
    w.strukt(20, "Location", &[("filename", str_ref), ("line", U32), ("col", U32)]);
    w.location = Some(LOCATION);
    w.func(3, "callee", &[U32], U32);
    if tracked {
        w.tracked.push(CALLEE);
    }
    w.def_site = site.then_some(CALLEE_DEF_SITE);
    let mut ground = PortableTys::new_in(alloc);
    let sig = w.sig(CALLEE).expect("callee's signature");
    let body = BodyBuf::new();
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: alloc };
    let built = crate::build_reify_shim(CALLEE, &sig, &mut subst, &TestAdts { world: &w, body: &body }, alloc)?;
    blaze_lir::verify(built.func.view()).unwrap_or_else(|error| panic!("does not verify: {error}"));
    Ok(built)
}

/// Taking a `#[track_caller]` fn as a fn pointer references `REIFY_SHIM_ITEM([FnDef(CALLEE, [])])`
/// instead of `CALLEE` directly; taking an untracked fn references `CALLEE` directly.
#[test]
fn reifying_a_tracked_fn_references_its_reify_shim() {
    let alloc = blaze_own::testing::heap().alloc();
    for tracked in [true, false] {
        let mut w = TestWorld::new();
        let str_ref = w.ref_(Mutbl::Not, PNode::Str);
        w.strukt(20, "Location", &[("filename", str_ref), ("line", U32), ("col", U32)]);
        w.location = Some(LOCATION);
        w.func(1, "main", &[U32], U32);
        w.func(3, "callee", &[U32], U32);
        if tracked {
            w.tracked.push(CALLEE);
        }
        let mut b = BodyBuf::new();
        let x = b.bind("x");
        b.params.push(x);
        let u32_t = b.types.intern(U32).expect("intern u32");
        let fl = b.types.intern_list(&[u32_t, u32_t]).expect("intern fn ptr list");
        let fn_ty = b.types.intern(PNode::FnPtr(fl, blaze_typeck::FnPtrHeader::RUST)).expect("intern fn ptr");
        let f = b.bind("f");
        let c_path = b.path("callee");
        let c_expr = b.e(Expr::Path(c_path));
        let f_e = b.local(f);
        let x_e = b.local(x);
        let args = b.exprs_list(&[x_e]);
        let call = b.e(Expr::Call { callee: f_e, args });
        let root = b.block(&[Stmt::Let { pat: f, ty: Some(fn_ty), init: Some(c_expr), else_: None }], Some(call));

        let view = b.view(MAIN, root);
        let mut ty = InferCtx::new_in(alloc);
        let checked = match blaze_typeck::check(&view, &w, &mut ty, &Needs::new_in(alloc), alloc) {
            Ok(Outcome::Done(c)) => c,
            Ok(Outcome::Need(n)) => panic!("check needed {:?}", n.questions),
            Err(error) => panic!("check failed: {error:?}"),
        };
        assert!(checked.diags.is_empty(), "diags: {:?}", checked.diags);
        let sig = w.sig(MAIN).expect("main sig");
        let mut ground = PortableTys::new_in(alloc);
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: alloc };
        let built = crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &b }, alloc)
            .unwrap_or_else(|e| panic!("{e}"));
        blaze_lir::verify(built.func.view()).expect("verifies");
        assert_eq!(built.refs.len(), 1, "one func ref");
        if tracked {
            assert_eq!(built.refs[0].item, crate::REIFY_SHIM_ITEM, "tracked fn reifies to REIFY_SHIM_ITEM");
            let inner = crate::reify_shim_of(built.refs[0], ground.view()).expect("reify_shim_of decodes");
            assert_eq!(inner, Instance { item: CALLEE, args: List::EMPTY });
        } else {
            assert_eq!(built.refs[0], Instance { item: CALLEE, args: List::EMPTY }, "untracked fn reifies directly");
        }
    }
}

/// Checks whether `view`'s own signature has exact `want_params` and `want_rets`.
fn has_own_sig(view: blaze_lir::func::View<'_>, want_params: &[Ty], want_rets: &[Ty]) -> bool {
    let s = view.sig(SigId(view.head.sig));
    s.params.len() == want_params.len()
        && s.params.iter().zip(want_params).all(|(p, &w)| Ty(p.ty) == w)
        && s.rets.len() == want_rets.len()
        && s.rets.iter().zip(want_rets).all(|(r, &w)| Ty(r.ty) == w)
}

/// Checks whether some `Call` / `Invoke` in `view` has exact `want_params` and `want_rets`.
fn has_direct_call_sig(view: blaze_lir::func::View<'_>, want_params: &[Ty], want_rets: &[Ty]) -> bool {
    use blaze_lir::Op;
    (0..view.len()).any(|inst| {
        let mut o = view.operands(inst);
        let sig = match view.op(inst) {
            Op::Call | Op::Invoke | Op::TailCall => {
                let _callee = o.reference();
                o.sig()
            }
            _ => return false,
        };
        let s = view.sig(sig);
        s.params.len() == want_params.len()
            && s.params.iter().zip(want_params).all(|(p, &w)| Ty(p.ty) == w)
            && s.rets.len() == want_rets.len()
            && s.rets.iter().zip(want_rets).all(|(r, &w)| Ty(r.ty) == w)
    })
}

/// `build_reify_shim` produces a plain fn-pointer signature (no hidden `Location` param) that
/// materializes a `Location` row for the fn's OWN definition site and calls the tracked fn with
/// that location passed last; refuses with `Adts::def_site` when no definition site is given.
#[test]
fn a_reify_shim_passes_its_fns_own_definition_site() {
    let alloc = blaze_own::testing::heap().alloc();
    let shim = build_reify(true, true).unwrap_or_else(|e| panic!("{e}"));
    let view = shim.func.view();
    let text = blaze_lir::print(view);
    // Own signature has NO hidden location param: `(i32) -> i32`.
    assert!(has_own_sig(view, &[Ty::I32], &[Ty::I32]), "reify shim takes no hidden location: {text}");
    // Direct call to `CALLEE` passes the hidden location LAST: `(i32, ptr) -> i32`.
    assert_eq!(shim.refs.as_slice(), &[Instance { item: CALLEE, args: List::EMPTY }], "{text}");
    assert!(has_direct_call_sig(view, &[Ty::I32, Ty::PTR], &[Ty::I32]), "reify shim calls callee with hidden location last: {text}");
    let mut rows = blaze_own::Vec::new_in(alloc);
    for d in (0..view.datas.len() as u32).map(blaze_lir::func::DataId) {
        if !view.data_positions_of(d).is_empty() {
            rows.push(d);
        }
    }
    assert_eq!(rows.len(), 1, "one `Location` row: {text}");
    let positions = view.data_positions_of(rows[0]);
    assert_eq!(positions.len(), 2, "a line and a column: {text}");
    for p in positions {
        assert_eq!((p.item_key(), p.site), (CALLEE_DEF_SITE.item.0, CALLEE_DEF_SITE.offset), "callee's own def site: {text}");
        let bytes = view.data_bytes_of(rows[0]);
        assert_eq!(&bytes[p.offset as usize..p.offset as usize + 4], &[0; 4], "no absolute line or column: {text}");
    }

    let Err(refused) = build_reify(true, false) else { panic!("a tracked fn's reify shim with no def_site must be refused") };
    assert!(format!("{refused}").contains("`Adts::def_site`"), "{refused}");

    // Also verify a generic `#[track_caller]` fn with aggregate parameter and sret aggregate return.
    let mut w = TestWorld::new();
    let str_ref = w.ref_(Mutbl::Not, PNode::Str);
    w.strukt(20, "Location", &[("filename", str_ref), ("line", U32), ("col", U32)]);
    w.location = Some(LOCATION);
    w.strukt(25, "Pair", &[("a", PNode::Uint(UintTy::U64)), ("b", PNode::Uint(UintTy::U64))]);
    let pair = PNode::Adt(ItemKey(25), List::EMPTY);
    w.generic_func(3, "gen_callee", 1, &[PNode::Param(0)], PNode::Param(0));
    w.tracked.push(CALLEE);
    w.def_site = Some(CALLEE_DEF_SITE);
    let mut ground = PortableTys::new_in(alloc);
    let pair_g = ground.intern(pair).expect("intern pair");
    let sig = w.sig(CALLEE).expect("gen_callee sig");
    let body = BodyBuf::new();
    let mut subst = Subst { args: &[pair_g], ground: &mut ground, scratch: alloc };
    let gen_shim = crate::build_reify_shim(CALLEE, &sig, &mut subst, &TestAdts { world: &w, body: &body }, alloc)
        .unwrap_or_else(|e| panic!("generic agg reify shim failed: {e}"));
    blaze_lir::verify(gen_shim.func.view()).expect("generic agg reify shim verifies");
    let gview = gen_shim.func.view();
    let gtext = blaze_lir::print(gview);
    // Own sig: `(sret: ptr, arg: ptr) -> ()`; call sig: `(sret: ptr, arg: ptr, loc: ptr) -> ()`.
    assert!(has_own_sig(gview, &[Ty::PTR, Ty::PTR], &[]), "sret + agg arg, no location: {gtext}");
    assert!(has_direct_call_sig(gview, &[Ty::PTR, Ty::PTR, Ty::PTR], &[]), "generic agg reify shim calls callee with (sret, arg, loc): {gtext}");
}
