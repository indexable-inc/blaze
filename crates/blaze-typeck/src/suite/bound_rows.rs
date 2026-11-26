//! A callee's trait bound is a `Checked::selected` row at the call whatever its trait names
//! (src/selected.rs, "Bound rows"; rules/call.rs `record_lifetime_impls`), where its instance
//! may hold a region: the Reference, "Trait and lifetime bounds": "Bounds on an item must be
//! satisfied when using the item", through the impl that applies at the instance. A selected
//! impl's own where-clauses are further rows at the same call (writeback/selected_rows.rs
//! `impl_where_rows`). Before, a row was filed only for a bound whose trait has lifetime
//! parameters, on a bare type parameter, in a caller assuming only `Sized`.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! trait Marker {}
//! impl Marker for &'static u8 {}
//! impl Marker for u8 {}
//! fn need<T: Marker>(t: T) -> u8 { 0 }
//! fn caller() -> u8 { let n = 1u8; need(&n) }       // a row `&u8: Marker`, Impl
//! fn caller() -> u8 { need(5u8) }                   // no row: `u8` holds no region
//! fn gen_caller<U: Marker>(u: U) -> u8 { let n = 1u8; need(&n) }  // Impl: `U: Marker` cannot answer
//! fn assumes() -> u8 where &'a u8: Marker { let n = 1u8; need(&n) } // Generic: it may
//!
//! trait Clone { fn clone(&self) -> Self; }
//! struct Vec<T>;
//! impl<T: Clone> Clone for Vec<T> {}
//! impl<T> Clone for &T {}
//! fn cloner(v: Vec<&u8>) { v.clone(); }   // `Vec<&u8>: Clone`, then `&u8: Clone`, both Impl
//! ```

use std::string::String;
use std::vec::Vec;

use super::check_body;
use crate::body::IntSuffix;
use crate::columns::Checked;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::selected::Selected;

struct World {
    fx: Fx,
    marker: ItemKey,
    marker_ref: ItemKey,
    clone: ItemKey,
    clone_vec: ItemKey,
    clone_ref: ItemKey,
    caller: ItemKey,
    gen_caller: ItemKey,
    assumes: ItemKey,
    assumes_other: ItemKey,
    cloner: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let (u8_, unit) = (fx.u8(), fx.unit());
    let ref_u8 = fx.r(Mutbl::Not, u8_);
    let marker = fx.tr("Marker", 1, Vec::new(), &[]);
    let marker_ref = fx.imp("impl Marker for &u8", 0, ref_u8, Some((marker, &[])), &[], Vec::new());
    fx.imp("impl Marker for u8", 0, u8_, Some((marker, &[])), &[], Vec::new());
    let t_marker = fx.bound(marker, &[p0]);
    fx.func("need", 1, &[p0], u8_, std::vec![t_marker]);
    let caller = fx.func("caller", 0, &[], u8_, Vec::new());
    let gen_caller = fx.func("gen_caller", 1, &[p0], u8_, std::vec![t_marker]);
    let ref_u8_marker = fx.bound(marker, &[ref_u8]);
    let assumes = fx.func("assumes", 0, &[], u8_, std::vec![ref_u8_marker]);
    let u16_ = fx.u16();
    let ref_u16 = fx.r(Mutbl::Not, u16_);
    fx.imp("impl Marker for &u16", 0, ref_u16, Some((marker, &[])), &[], Vec::new());
    let ref_u16_marker = fx.bound(marker, &[ref_u16]);
    let assumes_other = fx.func("assumes_other", 0, &[], u8_, std::vec![ref_u16_marker]);
    let clone = fx.tr("Clone", 1, Vec::new(), &[]);
    let self_ref = fx.r(Mutbl::Not, p0);
    fx.trait_fn(clone, "clone", true, &[self_ref], p0);
    fx.use_trait(clone);
    let vec = fx.strukt("Vec", 1, &[]);
    let vec_t = fx.adt(vec, &[p0]);
    let t_clone = fx.bound(clone, &[p0]);
    let clone_vec = fx.imp("impl Clone for Vec", 1, vec_t, Some((clone, &[])), &[], std::vec![t_clone]);
    let clone_ref = fx.imp("impl Clone for &T", 1, self_ref, Some((clone, &[])), &[], Vec::new());
    let vec_ref_u8 = fx.adt(vec, &[ref_u8]);
    let cloner = fx.func("cloner", 0, &[vec_ref_u8], unit, Vec::new());
    fx.finish();
    World { fx, marker, marker_ref, clone, clone_vec, clone_ref, caller, gen_caller, assumes, assumes_other, cloner }
}

/// The rows at `call` as `(trait, Self as written, selected, global_env)`; each a bound row.
fn rows_at(w: &World, c: &Checked<'_>, call: ExprId) -> Vec<(ItemKey, String, Selected, bool)> {
    let tys = c.types.view();
    c.call_selections(call)
        .iter()
        .map(|r| {
            assert_eq!(r.name, None, "`need` and `clone` name no projection: only bound rows");
            let self_ty = *tys.list(r.args).expect("a row's arguments in the record").first().expect("a row's `Self`");
            (r.trait_, show(&w.fx, tys, self_ty), r.selected, r.global_env)
        })
        .collect()
}

/// `{ let n = 1u8; need(&n) }` in `owner`: its findings and the rows at the call.
fn need_ref_n(w: &World, owner: ItemKey) -> (usize, Vec<(ItemKey, String, Selected, bool)>) {
    let mut b = Bb::new();
    if owner == w.gen_caller {
        b.param("u");
    }
    let n = b.bind("n");
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let s0 = b.let_(n, None, one);
    let f = b.path(&["need"]);
    let x = b.local(n);
    let r = b.borrow(Mutbl::Not, x);
    let call = b.call(f, &[r]);
    let root = b.block(&[s0], Some(call));
    let c = check_body(&w.fx, &b, owner, root, Plant::None, InferPlant::None);
    (c.diags.len(), rows_at(w, &c, call))
}

/// (a) `need(&n)` under `T: Marker`, `impl Marker for &'static u8`: `Marker` names no
/// lifetime, yet the bound at `T = &'x u8` holds only through that impl (rustc E0597 here:
/// `n` must be `'static`). One row `&u8: Marker`, selected that impl; `T: Sized` is no row.
#[test]
fn a_lifetimeless_bound_at_a_reference_is_a_row_selecting_its_impl() {
    let w = world();
    let (diags, rows) = need_ref_n(&w, w.caller);
    assert_eq!(diags, 0);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (tr, ref self_ty, sel, global) = rows[0];
    assert_eq!((tr, self_ty.as_str(), global), (w.marker, "&u8", true));
    assert!(matches!(sel, Selected::Impl { imp, .. } if imp == w.marker_ref), "{sel:?}");
}

/// (c) The control (rule 23): the same bound at `T = u8` (`impl Marker for u8`): the
/// instance holds no region and no caller's parameter, so the bound relates nothing: no row.
#[test]
fn a_bound_at_an_instance_with_no_region_is_no_row() {
    let w = world();
    let mut b = Bb::new();
    let f = b.path(&["need"]);
    let five = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let call = b.call(f, &[five]);
    let root = b.block(&[], Some(call));
    let c = check_body(&w.fx, &b, w.caller, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0);
    let rows = rows_at(&w, &c, call);
    assert!(rows.is_empty(), "{rows:?}");
}

/// (b) `v.clone()` on `Vec<&u8>`: the call's own row `Vec<&u8>: Clone` selects
/// `impl<T: Clone> Clone for Vec<T>`, whose where-clause at `T = &u8` is a further row
/// `&u8: Clone` at the same call, after it, selecting `impl<T> Clone for &T` (whose `T: Sized`
/// is no row).
#[test]
fn a_selected_impls_where_clause_is_a_further_row_at_the_call() {
    let w = world();
    let mut b = Bb::new();
    let v = b.param("v");
    let recv = b.local(v);
    let call = b.method(recv, "clone", &[]);
    let s0 = b.semi(call);
    let root = b.block(&[s0], None);
    let c = check_body(&w.fx, &b, w.cloner, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0);
    let rows = rows_at(&w, &c, call);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[0].0, rows[0].1.as_str()), (w.clone, "Vec<&u8>"));
    assert!(matches!(rows[0].2, Selected::Impl { imp, .. } if imp == w.clone_vec), "{:?}", rows[0].2);
    assert_eq!((rows[1].0, rows[1].1.as_str()), (w.clone, "&u8"));
    assert!(matches!(rows[1].2, Selected::Impl { imp, .. } if imp == w.clone_ref), "{:?}", rows[1].2);
}

/// The caller's environment at a ground row. `gen_caller<U: Marker>` assumes more than
/// `Sized`, but `U: Marker` names a placeholder no ground goal unifies with: the row still
/// selects the impl (`global_env` false). Its twin `assumes() where &'a u8: Marker` holds a
/// clause naming no parameter, which rustc prefers over the impl where it applies, and the
/// erased row cannot tell its `'a` from the impl's `'static`: the row is `Generic`, never the
/// impl (the borrow check refuses that tie by name).
#[test]
fn a_callers_clause_naming_no_parameter_leaves_a_ground_row_undecided() {
    let w = world();
    let (diags, rows) = need_ref_n(&w, w.gen_caller);
    assert_eq!(diags, 0);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0, rows[0].1.as_str(), rows[0].3), (w.marker, "&u8", false));
    assert!(matches!(rows[0].2, Selected::Impl { imp, .. } if imp == w.marker_ref), "{:?}", rows[0].2);
    let (diags, rows) = need_ref_n(&w, w.assumes);
    assert_eq!(diags, 0);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0, rows[0].1.as_str(), rows[0].3), (w.marker, "&u8", false));
    assert_eq!(rows[0].2, Selected::Generic);
}

/// The ground rule's other side. rustc select/mod.rs `winnow_candidates`: a where-clause is a
/// candidate only where it unifies with the goal (`match_where_clause_trait_ref`), and a
/// global one is "ignored" in favor of an impl. `assumes_other() where &'static u16: Marker`
/// holds a parameter-free clause of `Marker` too, but `&u16` differs from the row's `&u8`
/// past regions: no candidate, so the row selects the impl, as in `caller` (rustc accepts
/// nothing here it would not without the clause; before, the row was `Generic` and the borrow
/// check refused the call by name: a false refusal, rule 4). The twin is the `assumes` half
/// of the test above (the clause equal up to regions: `Generic`); the control is `caller`
/// (no clause: `Impl`).
#[test]
fn a_callers_ground_clause_on_other_types_leaves_the_impl_selected() {
    let w = world();
    let (diags, rows) = need_ref_n(&w, w.assumes_other);
    assert_eq!(diags, 0);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0, rows[0].1.as_str(), rows[0].3), (w.marker, "&u8", false));
    assert!(matches!(rows[0].2, Selected::Impl { imp, .. } if imp == w.marker_ref), "the clause `&u16: Marker` is no candidate at `&u8`: {:?}", rows[0].2);
    let (diags, rows) = need_ref_n(&w, w.assumes);
    assert_eq!(diags, 0);
    assert_eq!(rows[0].2, Selected::Generic, "TWIN: `&'a u8: Marker` equals the row up to regions: {rows:?}");
    let (diags, rows) = need_ref_n(&w, w.caller);
    assert_eq!(diags, 0);
    assert!(matches!(rows[0].2, Selected::Impl { imp, .. } if imp == w.marker_ref), "CONTROL: no clause: {rows:?}");
}

/// Projection normalization at writeback must elaborate associated-type item bounds and their
/// supertraits (`Node: FullNodeTypes<Types = TempoNode>` -> `Node::Provider: FullProvider<Adapter<Node::Types>>`
/// -> `<Node::Provider as ChainSpecProvider>::ChainSpec = <Adapter<TempoNode> as NodeTypes>::ChainSpec`
/// -> `<TempoNode as NodeTypes>::ChainSpec` -> `TempoChainSpec`), filing the intermediate
/// `NodeTypes::ChainSpec` impl rows and the normalized `TempoHardforks` impl row at the call.
#[test]
fn alias_bound_supertrait_projection_normalizes_and_selects_intermediate_impls() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let unit = fx.unit();

    let node_types = fx.tr("NodeTypes", 1, Vec::new(), &["ChainSpec"]);
    let cs_provider = fx.tr("ChainSpecProvider", 1, Vec::new(), &["ChainSpec"]);
    let full_provider = fx.tr("FullProvider", 2, Vec::new(), &[]);
    let p1_cs = fx.alias(node_types, "ChainSpec", &[p1]);
    let fp_sup_tr = fx.bound(cs_provider, &[p0]);
    let fp_sup_proj = fx.proj(cs_provider, &[p0], "ChainSpec", p1_cs);
    fx.set_supers(full_provider, std::vec![fp_sup_tr, fp_sup_proj]);

    let adapter = fx.strukt("Adapter", 1, &[]);
    let adapter_p0 = fx.adt(adapter, &[p0]);
    let p0_nt = fx.bound(node_types, &[p0]);
    let p0_cs = fx.alias(node_types, "ChainSpec", &[p0]);
    let impl_nt_adapter = fx.imp(
        "impl<T: NodeTypes> NodeTypes for Adapter<T>",
        1,
        adapter_p0,
        Some((node_types, &[])),
        &[("ChainSpec", p0_cs)],
        std::vec![p0_nt],
    );

    let full_node_types = fx.tr("FullNodeTypes", 1, Vec::new(), &["Types", "Provider"]);
    let fnt_types = fx.alias(full_node_types, "Types", &[p0]);
    let fnt_provider = fx.alias(full_node_types, "Provider", &[p0]);
    let fnt_types_nt = fx.bound(node_types, &[fnt_types]);
    let fnt_types_sized = fx.bound(sized, &[fnt_types]);
    fx.set_assoc_bounds(full_node_types, "Types", std::vec![fnt_types_nt, fnt_types_sized]);
    let adapter_types = fx.adt(adapter, &[fnt_types]);
    let fnt_prov_fp = fx.bound(full_provider, &[fnt_provider, adapter_types]);
    let fnt_prov_sized = fx.bound(sized, &[fnt_provider]);
    fx.set_assoc_bounds(full_node_types, "Provider", std::vec![fnt_prov_fp, fnt_prov_sized]);

    let tempo_node = fx.strukt("TempoNode", 0, &[]);
    let tempo_node_t = fx.adt(tempo_node, &[]);
    let tempo_cs = fx.strukt("TempoChainSpec", 0, &[]);
    let tempo_cs_t = fx.adt(tempo_cs, &[]);
    let impl_nt_tempo = fx.imp(
        "impl NodeTypes for TempoNode",
        0,
        tempo_node_t,
        Some((node_types, &[])),
        &[("ChainSpec", tempo_cs_t)],
        Vec::new(),
    );

    let hf = fx.tr("TempoHardforks", 1, Vec::new(), &[]);
    let impl_hf_tempo = fx.imp("impl TempoHardforks for TempoChainSpec", 0, tempo_cs_t, Some((hf, &[])), &[], Vec::new());

    let p0_csp = fx.bound(cs_provider, &[p0]);
    let p0_csp_cs = fx.alias(cs_provider, "ChainSpec", &[p0]);
    let p0_csp_cs_hf = fx.bound(hf, &[p0_csp_cs]);
    fx.func("need_hf", 1, &[p0], unit, std::vec![p0_csp, p0_csp_cs_hf]);

    let node_fnt = fx.bound(full_node_types, &[p0]);
    let node_types_eq = fx.proj(full_node_types, &[p0], "Types", tempo_node_t);
    let components = fx.func("components", 1, &[fnt_provider], unit, std::vec![node_fnt, node_types_eq]);
    fx.finish();

    let mut b = Bb::new();
    let p_param = b.param("p");
    let f = b.path(&["need_hf"]);
    let arg = b.local(p_param);
    let call = b.call(f, &[arg]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, components, root, Plant::None, InferPlant::None);
    assert_eq!(c.diags.len(), 0, "{:?}", c.diags);

    let rows = c.call_selections(call);
    let has_impl = |tr: ItemKey, is_proj: bool, want_imp: ItemKey| -> bool {
        rows.iter().any(|r| {
            r.trait_ == tr && r.name.is_some() == is_proj && matches!(r.selected, Selected::Impl { imp, .. } if imp == want_imp)
        })
    };
    assert!(has_impl(node_types, true, impl_nt_adapter), "missing Adapter NodeTypes impl row: {rows:?}");
    assert!(has_impl(node_types, true, impl_nt_tempo), "missing TempoNode NodeTypes impl row: {rows:?}");
    assert!(has_impl(hf, false, impl_hf_tempo), "missing normalized TempoHardforks impl row: {rows:?}");
}
