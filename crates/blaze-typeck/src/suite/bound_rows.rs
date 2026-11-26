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

/// An impl's own `where`-clause projection equality (`impl<Client> Pool for TempoPool<Client>
/// where Client: ChainSpecProvider<ChainSpec = <TempoNode as NodeTypes>::ChainSpec>`) must
/// normalize both its non-Impl LHS projection (through `Node::Provider`'s assoc-item supertraits)
/// and its RHS `term` alias, filing intermediate `NodeTypes::ChainSpec` impl rows at the call.
#[test]
fn impl_where_projection_clause_normalizes_lhs_and_rhs_aliases() {
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

    let pool_tr = fx.tr("Pool", 1, Vec::new(), &[]);
    let tempo_pool = fx.strukt("TempoPool", 1, &[]);
    let tempo_pool_p0 = fx.adt(tempo_pool, &[p0]);
    let tempo_node_cs = fx.alias(node_types, "ChainSpec", &[tempo_node_t]);
    let client_csp = fx.bound(cs_provider, &[p0]);
    let client_csp_eq = fx.proj(cs_provider, &[p0], "ChainSpec", tempo_node_cs);
    let impl_pool = fx.imp(
        "impl<C: ChainSpecProvider<ChainSpec = TempoNode::ChainSpec>> Pool for TempoPool<C>",
        1,
        tempo_pool_p0,
        Some((pool_tr, &[])),
        &[],
        std::vec![client_csp, client_csp_eq],
    );

    let p0_pool = fx.bound(pool_tr, &[p0]);
    fx.func("use_pool", 1, &[p0], unit, std::vec![p0_pool]);

    let node_fnt = fx.bound(full_node_types, &[p0]);
    let node_types_eq = fx.proj(full_node_types, &[p0], "Types", tempo_node_t);
    let tempo_pool_prov = fx.adt(tempo_pool, &[fnt_provider]);
    let components = fx.func("components", 1, &[tempo_pool_prov], unit, std::vec![node_fnt, node_types_eq]);
    fx.finish();

    let mut b = Bb::new();
    let p_param = b.param("p");
    let f = b.path(&["use_pool"]);
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
    assert!(has_impl(pool_tr, false, impl_pool), "missing Pool impl row: {rows:?}");
    assert!(has_impl(node_types, true, impl_nt_adapter), "missing Adapter NodeTypes impl row: {rows:?}");
    assert!(has_impl(node_types, true, impl_nt_tempo), "missing TempoNode NodeTypes impl row: {rows:?}");
}

/// A caller where-clause on a rigid parameter alias (`C::Spec: SubMarker`, where `SubMarker: Marker`
/// and no projection equality equates `C::Spec`) cannot match a ground row `&u8: Marker`, so the
/// ground row still selects `impl Marker for &'static u8`. Twin: when the caller also equates
/// `C::Spec = &u8`, the alias is no longer rigid and `&u8: Marker` stays `Selected::Generic`.
#[test]
fn rigid_param_alias_clause_does_not_shadow_ground_impl() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let unit = fx.unit();
    let u8_ = fx.u8();
    let ru8 = fx.r(Mutbl::Not, u8_);

    let marker = fx.tr("Marker", 1, Vec::new(), &[]);
    let super_marker = fx.bound(marker, &[p0]);
    let sub_marker = fx.tr("SubMarker", 1, std::vec![super_marker], &[]);
    let provider = fx.tr("Provider", 1, Vec::new(), &["Spec"]);
    let marker_ref = fx.imp("impl Marker for &u8", 0, ru8, Some((marker, &[])), &[], Vec::new());

    let p0_marker = fx.bound(marker, &[p0]);
    fx.func("need", 1, &[p0], unit, std::vec![p0_marker]);

    let p0_prov = fx.bound(provider, &[p0]);
    let p0_spec = fx.alias(provider, "Spec", &[p0]);
    let spec_sub = fx.bound(sub_marker, &[p0_spec]);
    let rigid_caller = fx.func("rigid_caller", 1, &[ru8], unit, std::vec![p0_prov, spec_sub]);

    let p0_prov2 = fx.bound(provider, &[p0]);
    let spec_eq = fx.proj(provider, &[p0], "Spec", ru8);
    let spec_sub2 = fx.bound(sub_marker, &[p0_spec]);
    let equated_caller = fx.func("equated_caller", 1, &[ru8], unit, std::vec![p0_prov2, spec_eq, spec_sub2]);
    fx.finish();

    let run = |owner: ItemKey| {
        let mut b = Bb::new();
        let x = b.param("x");
        let f = b.path(&["need"]);
        let lx = b.local(x);
        let call = b.call(f, &[lx]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        c.call_selections(call).to_vec()
    };

    let rigid_rows = run(rigid_caller);
    assert_eq!(rigid_rows.len(), 1, "{rigid_rows:?}");
    assert!(
        matches!(rigid_rows[0].selected, Selected::Impl { imp, .. } if imp == marker_ref),
        "rigid `C::Spec: SubMarker` must not shadow ground `&u8: Marker`: {rigid_rows:?}"
    );

    let equated_rows = run(equated_caller);
    assert_eq!(equated_rows.len(), 1, "{equated_rows:?}");
    assert_eq!(
        equated_rows[0].selected,
        Selected::Generic,
        "TWIN: equated `C::Spec = &u8` with `C::Spec: SubMarker` may shadow `&u8: Marker`: {equated_rows:?}"
    );
}

/// 2a: `E: From<E>` under caller bound `E: From<ShapeError<A>>` must NOT be shadowed by
/// `E: From<ShapeError<A>>` (`ShapeError<A>` cannot equal rigid `Param(E)`), so it selects the
/// reflexive blanket `impl<T> From<T> for T`. Twins:
/// - `E: From<ShapeError<A>>` DOES match the caller clause and selects `Selected::ParamEnv`;
/// - under two bounds `E: From<ShapeError<A>> + From<ShapeError<B>>`, `E: From<ShapeError<B>>`
///   uniquely selects the second clause (and `E: From<E>` still selects `impl<T> From<T> for T`).
#[test]
fn from_self_not_shadowed_by_other_from_bound() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1, p2) = (fx.param(0), fx.param(1), fx.param(2));

    let from_tr = fx.tr("From", 2, Vec::new(), &[]);
    let impl_from_self = fx.imp("impl<T> From<T> for T", 1, p0, Some((from_tr, &[p0])), &[], Vec::new());

    let shape_err = fx.strukt("ShapeError", 1, &[]);
    let shape_a = fx.adt(shape_err, &[p0]);
    let shape_b = fx.adt(shape_err, &[p1]);

    // fn convert<U, V: From<U>>(u: U) -> V
    let v_from_u = fx.bound(from_tr, &[p1, p0]);
    fx.func("convert", 2, &[p0], p1, std::vec![v_from_u]);

    // Caller 1: fn caller_self<A, E: From<ShapeError<A>>>(e: E) -> E { convert(e) }
    let e_from_sa = fx.bound(from_tr, &[p1, shape_a]);
    let caller_self = fx.func("caller_self", 2, &[p1], p1, std::vec![e_from_sa]);

    // Caller 2 (twin): fn caller_match<A, E: From<ShapeError<A>>>(se: ShapeError<A>) -> E { convert(se) }
    let caller_match = fx.func("caller_match", 2, &[shape_a], p1, std::vec![e_from_sa]);

    // Caller 3 (multi-clause): fn caller_two<A, B, E: From<ShapeError<A>> + From<ShapeError<B>>>(se: ShapeError<B>) -> E
    let e2_from_sa = fx.bound(from_tr, &[p2, shape_a]);
    let e2_from_sb = fx.bound(from_tr, &[p2, shape_b]);
    let caller_two_b = fx.func("caller_two_b", 3, &[shape_b], p2, std::vec![e2_from_sa, e2_from_sb]);
    let caller_two_self = fx.func("caller_two_self", 3, &[p2], p2, std::vec![e2_from_sa, e2_from_sb]);
    fx.finish();

    let run = |owner: ItemKey| {
        let mut b = Bb::new();
        let x = b.param("x");
        let f = b.path(&["convert"]);
        let lx = b.local(x);
        let call = b.call(f, &[lx]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        c.call_selections(call).to_vec()
    };

    let self_rows = run(caller_self);
    assert_eq!(self_rows.len(), 1, "{self_rows:?}");
    assert!(
        matches!(self_rows[0].selected, Selected::Impl { imp, .. } if imp == impl_from_self),
        "`E: From<E>` under `E: From<ShapeError<A>>` must select `impl<T> From<T> for T`: {self_rows:?}"
    );

    let match_rows = run(caller_match);
    assert_eq!(match_rows.len(), 1, "{match_rows:?}");
    assert_eq!(
        match_rows[0].selected,
        Selected::ParamEnv { clause: 0 },
        "TWIN: `E: From<ShapeError<A>>` must select caller clause 0: {match_rows:?}"
    );

    let two_b_rows = run(caller_two_b);
    assert_eq!(two_b_rows.len(), 1, "{two_b_rows:?}");
    assert_eq!(
        two_b_rows[0].selected,
        Selected::ParamEnv { clause: 1 },
        "TWIN: `E: From<ShapeError<B>>` under two `From` bounds must uniquely select clause 1: {two_b_rows:?}"
    );

    let two_self_rows = run(caller_two_self);
    assert_eq!(two_self_rows.len(), 1, "{two_self_rows:?}");
    assert!(
        matches!(two_self_rows[0].selected, Selected::Impl { imp, .. } if imp == impl_from_self),
        "`E: From<E>` under two `From` bounds must select `impl<T> From<T> for T`: {two_self_rows:?}"
    );
}

/// 2b: `FlatMap<Range, Closure>: Clone` under caller bounds `I: IntoIterator<Item = u8>` and
/// `I::IntoIter: Clone` (and also `Wrap<I::IntoIter>: Clone` vs `Wrap<&u8>: Clone`) must NOT be
/// shadowed by the rigid parameter alias `I::IntoIter`; twin with `I::IntoIter = FlatMap<..>`
/// stays `Selected::Generic`.
#[test]
fn rigid_param_alias_does_not_block_concrete_or_nested_clone_impl() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let clone = fx.tr("Clone", 1, Vec::new(), &[]);
    fx.lang(LangItem::Clone, clone);

    let (p0, p1) = (fx.param(0), fx.param(1));
    let u8_ = fx.u8();
    let unit = fx.unit();
    let ru8 = fx.r(Mutbl::Not, u8_);
    let ref_p0 = fx.r(Mutbl::Not, p0);

    fx.imp("impl Copy for u8", 0, u8_, Some((copy, &[])), &[], Vec::new());
    fx.imp("impl Clone for u8", 0, u8_, Some((clone, &[])), &[], Vec::new());
    fx.imp("impl<T> Clone for &T", 1, ref_p0, Some((clone, &[])), &[], Vec::new());
    fx.imp("impl<T> Copy for &T", 1, ref_p0, Some((copy, &[])), &[], Vec::new());

    let into_iter = fx.tr("IntoIterator", 1, Vec::new(), &["Item", "IntoIter"]);
    let range = fx.strukt("Range", 0, &[]);
    let range_ty = fx.adt(range, &[]);
    fx.imp("impl Clone for Range", 0, range_ty, Some((clone, &[])), &[], Vec::new());

    let flat_map = fx.strukt("FlatMap", 2, &[]);
    let flat_map_p = fx.adt(flat_map, &[p0, p1]);
    let p0_clone = fx.bound(clone, &[p0]);
    let p1_clone = fx.bound(clone, &[p1]);
    let impl_clone_fm = fx.imp(
        "impl<I: Clone, F: Clone> Clone for FlatMap<I, F>",
        2,
        flat_map_p,
        Some((clone, &[])),
        &[],
        std::vec![p0_clone, p1_clone],
    );
    fx.imp(
        "impl<I, F> IntoIterator for FlatMap<I, F>",
        2,
        flat_map_p,
        Some((into_iter, &[])),
        &[("Item", u8_), ("IntoIter", flat_map_p)],
        Vec::new(),
    );
    fx.func("mk_fm", 2, &[p0, p1], flat_map_p, Vec::new());

    let wrap = fx.strukt("Wrap", 1, &[]);
    let wrap_p0 = fx.adt(wrap, &[p0]);
    let impl_clone_wrap = fx.imp(
        "impl<T: Clone> Clone for Wrap<T>",
        1,
        wrap_p0,
        Some((clone, &[])),
        &[],
        std::vec![p0_clone],
    );

    // fn rebuild<X: IntoIterator>(x: X) where X::IntoIter: Clone
    let x_ii = fx.bound(into_iter, &[p0]);
    let x_into = fx.alias(into_iter, "IntoIter", &[p0]);
    let x_into_clone = fx.bound(clone, &[x_into]);
    fx.func("rebuild", 1, &[p0], unit, std::vec![x_ii, x_into_clone]);
    fx.func("need_clone", 1, &[p0], unit, std::vec![p0_clone]);

    // Caller 1: fn rebuild_graph<I, S>(r: Range, s: &S) where I: IntoIterator<Item = u8>, I::IntoIter: Clone
    let i_ii = fx.bound(into_iter, &[p0]);
    let i_item_u8 = fx.proj(into_iter, &[p0], "Item", u8_);
    let i_into = fx.alias(into_iter, "IntoIter", &[p0]);
    let i_into_clone = fx.bound(clone, &[i_into]);
    let ref_s = fx.r(Mutbl::Not, p1);
    let rebuild_graph = fx.func("rebuild_graph", 2, &[range_ty, ref_s], unit, std::vec![i_ii, i_item_u8, i_into_clone]);

    // Caller 2: nested rigid alias `where Wrap<I::IntoIter>: Clone` vs `Wrap<&u8>: Clone`
    let wrap_i_into = fx.adt(wrap, &[i_into]);
    let wrap_i_into_clone = fx.bound(clone, &[wrap_i_into]);
    let wrap_ru8 = fx.adt(wrap, &[ru8]);
    let nested_caller = fx.func(
        "nested_caller",
        1,
        &[wrap_ru8],
        unit,
        std::vec![i_ii, wrap_i_into_clone],
    );
    // Caller 3 (twin): `where I: IntoIterator<IntoIter = &u8>, Wrap<I::IntoIter>: Clone`
    // Equating `I::IntoIter` makes the alias non-rigid, so `Wrap<&u8>: Clone` stays conservative.
    let i_into_eq_ru8 = fx.proj(into_iter, &[p0], "IntoIter", ru8);
    let nested_equated = fx.func(
        "nested_equated",
        1,
        &[wrap_ru8],
        unit,
        std::vec![i_ii, i_into_eq_ru8, wrap_i_into_clone],
    );
    fx.finish();

    // Positive 1: `rebuild(mk_fm(r, move || s))` where `Closure` captures `&S` (Param(1)).
    {
        let mut b = Bb::new();
        let r = b.param("r");
        let s = b.param("s");
        let ls = b.local(s);
        let cbody = b.block(&[], Some(ls));
        let cl = b.move_closure(&[], cbody);
        let mk_fm = b.path(&["mk_fm"]);
        let lr = b.local(r);
        let fm = b.call(mk_fm, &[lr, cl]);
        let rebuild = b.path(&["rebuild"]);
        let call = b.call(rebuild, &[fm]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, rebuild_graph, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        let rows = c.call_selections(call);
        assert!(
            rows.iter().any(|r| r.trait_ == clone && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_clone_fm)),
            "`FlatMap<Range, Closure>: Clone` must select `impl Clone for FlatMap`: {rows:?}"
        );
    }

    // Positive 2 + Negative twin: nested rigid alias `Wrap<I::IntoIter>: Clone` vs `Wrap<&u8>: Clone`.
    let run_wrap = |owner: ItemKey| {
        let mut b = Bb::new();
        let w = b.param("w");
        let f = b.path(&["need_clone"]);
        let lw = b.local(w);
        let call = b.call(f, &[lw]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        c.call_selections(call).to_vec()
    };
    let nested_rows = run_wrap(nested_caller);
    assert!(
        nested_rows.iter().any(|r| r.trait_ == clone && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_clone_wrap)),
        "nested rigid alias `Wrap<I::IntoIter>: Clone` must not block `Wrap<&u8>: Clone`: {nested_rows:?}"
    );
    let eq_rows = run_wrap(nested_equated);
    assert_eq!(
        eq_rows[0].selected,
        Selected::Generic,
        "TWIN: equated `I::IntoIter = &u8` allows `Wrap<I::IntoIter>: Clone` to shadow `Wrap<&u8>: Clone`: {eq_rows:?}"
    );
}

/// 2c: `<Opaque as Iterator>::Item: IntoIterator` (where `fn mk() -> impl ExactSizeIterator<Item = Res<&u8>>`)
/// normalizes `<Opaque as Iterator>::Item` through the opaque's bounds to `Res<&u8>` and records
/// `impl<T> IntoIterator for Res<T>`. Twin: without the `Item = ...` bound on the opaque, no fake
/// impl is selected.
#[test]
fn opaque_alias_projection_normalizes_for_bound_row() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let unit = fx.unit();
    let u8_ = fx.u8();
    let ru8 = fx.r(Mutbl::Not, u8_);

    let iter_tr = fx.tr("Iterator", 1, Vec::new(), &["Item"]);
    let iter_sup = fx.bound(iter_tr, &[p0]);
    let exact_tr = fx.tr("ExactSizeIterator", 1, std::vec![iter_sup], &[]);
    let into_iter = fx.tr("IntoIterator", 1, Vec::new(), &["Item", "IntoIter"]);

    let res = fx.strukt("Res", 1, &[]);
    let res_p0 = fx.adt(res, &[p0]);
    let res_ru8 = fx.adt(res, &[ru8]);
    let impl_into_res = fx.imp(
        "impl<T> IntoIterator for Res<T>",
        1,
        res_p0,
        Some((into_iter, &[])),
        &[("Item", p0), ("IntoIter", res_p0)],
        Vec::new(),
    );

    // Opaque 1: `impl ExactSizeIterator<Item = Res<&u8>>`
    let op_eq = fx.opaque_key("mk_eq::{opaque#0}");
    let op_eq_ty = fx.ty(crate::portable::PNode::Opaque(op_eq, crate::ids::List::EMPTY));
    let op_eq_exact = fx.bound(exact_tr, &[op_eq_ty]);
    let op_eq_item = fx.proj(iter_tr, &[op_eq_ty], "Item", res_ru8);
    let op_eq_sized = fx.bound(sized, &[op_eq_ty]);
    fx.set_opaque_bounds(op_eq, std::vec![op_eq_exact, op_eq_item, op_eq_sized]);
    fx.func("mk_eq", 0, &[], op_eq_ty, Vec::new());

    // Opaque 2 (negative twin): `impl ExactSizeIterator` (no `Item = ...`, item itself only bounded by `IntoIterator`)
    let op_none = fx.opaque_key("mk_none::{opaque#0}");
    let op_none_ty = fx.ty(crate::portable::PNode::Opaque(op_none, crate::ids::List::EMPTY));
    let op_none_item = fx.alias(iter_tr, "Item", &[op_none_ty]);
    let op_none_exact = fx.bound(exact_tr, &[op_none_ty]);
    let op_none_ii = fx.bound(into_iter, &[op_none_item]);
    let op_none_sized = fx.bound(sized, &[op_none_ty]);
    fx.set_opaque_bounds(op_none, std::vec![op_none_exact, op_none_ii, op_none_sized]);
    fx.func("mk_none", 0, &[], op_none_ty, Vec::new());

    // fn flatten<I: Iterator>(i: I) where I::Item: IntoIterator
    let i_iter = fx.bound(iter_tr, &[p0]);
    let i_item = fx.alias(iter_tr, "Item", &[p0]);
    let i_item_ii = fx.bound(into_iter, &[i_item]);
    fx.func("flatten", 1, &[p0], unit, std::vec![i_iter, i_item_ii]);

    let user_eq = fx.func("user_eq", 0, &[], unit, Vec::new());
    let user_none = fx.func("user_none", 0, &[], unit, Vec::new());
    fx.finish();

    let run = |owner: ItemKey, mk_name: &str| {
        let mut b = Bb::new();
        let mk = b.path(&[mk_name]);
        let got = b.call(mk, &[]);
        let fl = b.path(&["flatten"]);
        let call = b.call(fl, &[got]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        c.call_selections(call).to_vec()
    };

    let eq_rows = run(user_eq, "mk_eq");
    assert!(
        eq_rows.iter().any(|r| r.trait_ == into_iter && r.name.is_none() && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_into_res)),
        "`<Opaque as Iterator>::Item: IntoIterator` must normalize to `Res<&u8>` and select `impl IntoIterator for Res`: {eq_rows:?}"
    );

    let none_rows = run(user_none, "mk_none");
    assert!(
        !none_rows.iter().any(|r| r.trait_ == into_iter && matches!(r.selected, Selected::Impl { .. })),
        "TWIN: opaque without `Item = ...` must not fabricate an `Impl` row: {none_rows:?}"
    );
}

/// 2d: Passing a trait method as a `FnDef` value (`apply(k, MyIter::next)`) must record the trait
/// selection (`Keys<&u8>: MyIter -> Selected::Impl`) at the `Expr::Path`. Negative control: calling
/// `MyIter::next(k)` directly records the selection at `Expr::Call` and does not duplicate it on
/// the `Expr::Path` callee.
#[test]
fn trait_method_path_value_records_selection_without_duplicating_direct_call() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let u8_ = fx.u8();
    let ru8 = fx.r(Mutbl::Not, u8_);

    let rust_call = crate::ids::FnPtrHeader { abi: crate::ids::Abi::RustCall, safety: crate::ids::Safety::Safe, c_variadic: false };
    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let out = fx.alias(fn_once, "Output", &[p0, p1]);
    let call_once = fx.trait_fn(fn_once, "call_once", true, &[p0, p1], out);
    fx.set_header(call_once, rust_call);

    let my_iter = fx.tr("MyIter", 1, Vec::new(), &["Item"]);
    let mut_self = fx.r(Mutbl::Mut, p0);
    let self_item = fx.alias(my_iter, "Item", &[p0]);
    fx.trait_fn(my_iter, "next", true, &[mut_self], self_item);
    fx.use_trait(my_iter);

    let keys = fx.strukt("Keys", 1, &[]);
    let keys_p0 = fx.adt(keys, &[p0]);
    let impl_keys_iter = fx.imp(
        "impl<T> MyIter for Keys<T>",
        1,
        keys_p0,
        Some((my_iter, &[])),
        &[("Item", p0)],
        Vec::new(),
    );

    // fn apply<T, F: FnOnce(&mut Keys<T>) -> T>(k: &mut Keys<T>, f: F) -> T
    let mut_keys_p0 = fx.r(Mutbl::Mut, keys_p0);
    let arg_tup = fx.tuple(&[mut_keys_p0]);
    let f_fn_once = fx.bound(fn_once, &[p1, arg_tup]);
    let f_out = fx.proj(fn_once, &[p1, arg_tup], "Output", p0);
    fx.func("apply", 2, &[mut_keys_p0, p1], p0, std::vec![f_fn_once, f_out]);

    let keys_ru8 = fx.adt(keys, &[ru8]);
    let mut_keys_ru8 = fx.r(Mutbl::Mut, keys_ru8);
    let caller_val = fx.func("caller_val", 0, &[mut_keys_ru8], ru8, Vec::new());
    let caller_direct = fx.func("caller_direct", 0, &[mut_keys_ru8], ru8, Vec::new());
    fx.finish();

    // Positive: `apply(k, MyIter::next)` -> selection recorded at `MyIter::next` (`Expr::Path`).
    {
        let mut b = Bb::new();
        let k = b.param("k");
        let lk = b.local(k);
        let next_path = b.path(&["MyIter", "next"]);
        let apply = b.path(&["apply"]);
        let call = b.call(apply, &[lk, next_path]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, caller_val, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        let path_rows = c.call_selections(next_path);
        assert!(
            path_rows.iter().any(|r| r.trait_ == my_iter && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_keys_iter)),
            "passing `MyIter::next` as a value must record `Keys<&u8>: MyIter` at the path expr: {path_rows:?}"
        );
    }

    // Negative control: direct `MyIter::next(k)` records on `call`, NOT duplicated on `next_path`.
    {
        let mut b = Bb::new();
        let k = b.param("k");
        let lk = b.local(k);
        let next_path = b.path(&["MyIter", "next"]);
        let call = b.call(next_path, &[lk]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, caller_direct, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        assert!(
            c.call_selections(next_path).is_empty(),
            "direct call callee path must not duplicate rows already on `Expr::Call`: {:?}",
            c.call_selections(next_path)
        );
        let call_rows = c.call_selections(call);
        assert!(
            call_rows.iter().any(|r| r.trait_ == my_iter && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_keys_iter)),
            "direct call must still record `Keys<&u8>: MyIter` on `Expr::Call`: {call_rows:?}"
        );
    }
}

/// 3: `AdjustKind::Unsize` coercions (`&T -> &dyn Trait<..>`, `MyBox<T> -> MyBox<dyn Trait>`,
/// `&P -> &dyn Trait`) record the exact selected impl (or `ParamEnv` clause) and object projections
/// at the coerced expression; negative control `&[u8; 4] -> &[u8]` records no trait row.
#[test]
fn unsize_to_dyn_records_trait_and_projection_selection() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce_unsized = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce_unsized);

    let (p0, p1) = (fx.param(0), fx.param(1));
    let u8_ = fx.u8();
    let ru8 = fx.r(Mutbl::Not, u8_);

    let dyn_tr = fx.tr("DynTr", 1, Vec::new(), &["Out"]);
    let my_s = fx.strukt("MyS", 0, &[]);
    let my_s_ty = fx.adt(my_s, &[]);
    let impl_dyn_s = fx.imp(
        "impl DynTr for MyS",
        0,
        my_s_ty,
        Some((dyn_tr, &[])),
        &[("Out", ru8)],
        Vec::new(),
    );

    let my_box = fx.strukt("MyBox", 1, &[("ptr", p0)]);
    let box_p0 = fx.adt(my_box, &[p0]);
    let box_p1 = fx.adt(my_box, &[p1]);
    let p0_unsize_p1 = fx.bound(unsize, &[p0, p1]);
    let box_coerce = fx.imp(
        "impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<MyBox<U>> for MyBox<T>",
        2,
        box_p0,
        Some((coerce_unsized, &[box_p1])),
        &[],
        std::vec![p0_unsize_p1],
    );
    fx.unsized_param(box_coerce, 0);
    fx.unsized_param(box_coerce, 1);

    let ex_tr = fx.ty(crate::portable::PNode::ExTrait(dyn_tr, crate::ids::List::EMPTY));
    let ex_out = fx.ty(crate::portable::PNode::ExProj {
        trait_: dyn_tr,
        name: crate::ids::Name::of("Out"),
        args: crate::ids::List::EMPTY,
        term: ru8,
    });
    let dyn_obj = fx.t.dynamic(&[ex_tr, ex_out]).expect("dynamic");
    let ref_dyn = fx.r(Mutbl::Not, dyn_obj);
    let ref_s = fx.r(Mutbl::Not, my_s_ty);
    let box_s = fx.adt(my_box, &[my_s_ty]);
    let box_dyn = fx.adt(my_box, &[dyn_obj]);
    let ref_p0 = fx.r(Mutbl::Not, p0);

    let four = fx.t.usize_value(4).expect("usize");
    let arr4 = fx.ty(crate::portable::PNode::Array(u8_, four));
    let slice_u8 = fx.ty(crate::portable::PNode::Slice(u8_));
    let ref_arr4 = fx.r(Mutbl::Not, arr4);
    let ref_slice = fx.r(Mutbl::Not, slice_u8);

    let coerce_ref = fx.func("coerce_ref", 0, &[ref_s], ref_dyn, Vec::new());
    let coerce_box = fx.func("coerce_box", 0, &[box_s], box_dyn, Vec::new());
    let p0_dyn = fx.bound(dyn_tr, &[p0]);
    let p0_dyn_out = fx.proj(dyn_tr, &[p0], "Out", ru8);
    let coerce_param = fx.func(
        "coerce_param",
        1,
        &[ref_p0],
        ref_dyn,
        std::vec![p0_dyn, p0_dyn_out],
    );
    let coerce_slice = fx.func("coerce_slice", 0, &[ref_arr4], ref_slice, Vec::new());
    fx.finish();

    let run = |owner: ItemKey| {
        let mut b = Bb::new();
        let x = b.param("x");
        let lx = b.local(x);
        let root = b.block(&[], Some(lx));
        let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        c.call_selections(lx).to_vec()
    };

    // Positive 1: `&MyS -> &dyn DynTr<Out = &u8>` records both `DynTr` bound and `Out` projection.
    let ref_rows = run(coerce_ref);
    assert!(
        ref_rows.iter().any(|r| r.trait_ == dyn_tr && r.name.is_none() && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_dyn_s)),
        "`&MyS -> &dyn DynTr` must record `MyS: DynTr` impl selection: {ref_rows:?}"
    );
    assert!(
        ref_rows.iter().any(|r| r.trait_ == dyn_tr && r.name == Some(crate::ids::Name::of("Out")) && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_dyn_s)),
        "`&MyS -> &dyn DynTr<Out = &u8>` must record `Out` projection selection: {ref_rows:?}"
    );

    // Positive 2: `MyBox<MyS> -> MyBox<dyn DynTr<Out = &u8>>` records `MyS: DynTr` impl selection.
    let box_rows = run(coerce_box);
    assert!(
        box_rows.iter().any(|r| r.trait_ == dyn_tr && r.name.is_none() && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_dyn_s)),
        "`MyBox<MyS> -> MyBox<dyn DynTr>` must record `MyS: DynTr` impl selection: {box_rows:?}"
    );

    // Positive 3: `&T -> &dyn DynTr<Out = &u8>` records `Selected::ParamEnv`.
    let param_rows = run(coerce_param);
    assert!(
        param_rows.iter().any(|r| r.trait_ == dyn_tr && r.name.is_none() && matches!(r.selected, Selected::ParamEnv { clause: 0 })),
        "`&T -> &dyn DynTr` must record `ParamEnv` selection: {param_rows:?}"
    );

    // Negative control: `&[u8; 4] -> &[u8]` unsizing is array-to-slice, NOT dyn: no trait row.
    let slice_rows = run(coerce_slice);
    assert!(
        slice_rows.is_empty(),
        "array-to-slice unsizing must record no trait selection row: {slice_rows:?}"
    );
}

/// 4: `impl_where_rows` scans the selected impl's own header (`self_ty` and `trait_args`) for
/// `PNode::Alias` nodes — e.g. `impl<T: AsContextMut> From<&mut T> for StoreContextMut<<T as
/// AsContext>::Data>` (wasmtime's `Caller<'_, Host>` -> `StoreContextMut<'_, Host>` in
/// `blaze-proc-macro/{script,host,wasm64_runtime}.rs`) — selecting the nested `<Caller<Host> as
/// AsContext>::Data` impl row at the call. Negative control: when `T` is a caller type parameter
/// `P: AsContextMut` with no concrete `AsContext` impl, no bogus `Impl` row is fabricated.
#[test]
fn impl_header_alias_selects_nested_assoc_impl() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let unit = fx.unit();

    // trait AsContext { type Data; }
    let as_ctx = fx.tr("AsContext", 1, Vec::new(), &["Data"]);
    let p0_as_ctx = fx.bound(as_ctx, &[p0]);
    let p0_data = fx.alias(as_ctx, "Data", &[p0]);
    let p0_data_sized = fx.bound(sized, &[p0_data]);
    fx.set_assoc_bounds(as_ctx, "Data", std::vec![p0_data_sized]);

    // trait AsContextMut: AsContext {}
    let as_ctx_mut = fx.tr("AsContextMut", 1, std::vec![p0_as_ctx], &[]);
    let p0_as_ctx_mut = fx.bound(as_ctx_mut, &[p0]);

    // trait MyFrom<T> { fn from(v: T) -> Self; }
    let my_from = fx.tr("MyFrom", 2, Vec::new(), &[]);
    fx.trait_fn(my_from, "from", false, &[p1], p0);

    // struct Host; struct Caller<T>(T); struct StoreContextMut<T>(T);
    let host = fx.strukt("Host", 0, &[]);
    let host_ty = fx.adt(host, &[]);
    let caller = fx.strukt("Caller", 1, &[("data", p0)]);
    let caller_p0 = fx.adt(caller, &[p0]);
    let caller_host = fx.adt(caller, &[host_ty]);
    let store_mut = fx.strukt("StoreContextMut", 1, &[("data", p0)]);
    let store_mut_data = fx.adt(store_mut, &[p0_data]);
    let store_mut_host = fx.adt(store_mut, &[host_ty]);
    let mut_p0 = fx.r(Mutbl::Mut, p0);
    let mut_caller_host = fx.r(Mutbl::Mut, caller_host);

    // impl<T> AsContext for Caller<T> { type Data = T; }
    let impl_as_ctx_caller = fx.imp(
        "impl<T> AsContext for Caller<T>",
        1,
        caller_p0,
        Some((as_ctx, &[])),
        &[("Data", p0)],
        Vec::new(),
    );
    // impl<T> AsContextMut for Caller<T> {}
    fx.imp(
        "impl<T> AsContextMut for Caller<T>",
        1,
        caller_p0,
        Some((as_ctx_mut, &[])),
        &[],
        Vec::new(),
    );
    // impl<T: AsContextMut> MyFrom<&mut T> for StoreContextMut<<T as AsContext>::Data> {}
    let impl_from_store = fx.imp(
        "impl<T: AsContextMut> MyFrom<&mut T> for StoreContextMut<<T as AsContext>::Data>",
        1,
        store_mut_data,
        Some((my_from, &[mut_p0])),
        &[],
        std::vec![p0_as_ctx_mut],
    );

    // fn data_and_store<S, T>(s: S) where StoreContextMut<T>: MyFrom<S>
    let store_mut_p1 = fx.adt(store_mut, &[p1]);
    let store_from_s = fx.bound(my_from, &[store_mut_p1, p0]);
    fx.func("data_and_store", 2, &[p0], unit, std::vec![store_from_s]);

    let owner_concrete = fx.func("owner_concrete", 0, &[mut_caller_host], unit, Vec::new());
    let owner_generic = fx.func("owner_generic", 1, &[mut_p0], unit, std::vec![p0_as_ctx_mut]);
    let _ = store_mut_host;
    fx.finish();

    // Positive: `data_and_store::<&mut Caller<Host>, Host>(c)` selects `impl_from_store` AND scans
    // its `self_ty` `StoreContextMut<<T as AsContext>::Data>` to file `<Caller<Host> as AsContext>::Data`.
    {
        use crate::ids::List;
        use crate::portable::PNode;
        let mut b = Bb::new();
        let b_host = b.ty(PNode::Adt(host, List::EMPTY));
        let b_host_l = b.t.intern_list(&[b_host]).expect("list");
        let b_caller_host = b.ty(PNode::Adt(caller, b_host_l));
        let b_mut_caller = b.ty(PNode::Ref(Mutbl::Mut, b_caller_host));
        let c_param = b.param("c");
        let lc = b.local(c_param);
        let f = b.path_args("data_and_store", &[b_mut_caller, b_host]);
        let call = b.call(f, &[lc]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, owner_concrete, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        let rows = c.call_selections(call);
        assert!(
            rows.iter().any(|r| r.trait_ == my_from && r.name.is_none() && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_from_store)),
            "expected `MyFrom` impl row on call: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.trait_ == as_ctx && r.name == Some(crate::ids::Name::of("Data")) && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_as_ctx_caller)),
            "impl header alias `<T as AsContext>::Data` must file `<Caller<Host> as AsContext>::Data -> impl_as_ctx_caller`: {rows:?}"
        );
    }

    // Negative control: generic `P: AsContextMut` with `data_and_store::<&mut P, <P as AsContext>::Data>(c)`
    // selects `impl_from_store`, and `<P as AsContext>::Data` must NOT fabricate a concrete `Impl` row.
    {
        use crate::portable::PNode;
        let mut b = Bb::new();
        let b_p0 = b.ty(PNode::Param(0));
        let b_mut_p0 = b.ty(PNode::Ref(Mutbl::Mut, b_p0));
        let b_p0_l = b.t.intern_list(&[b_p0]).expect("list");
        let b_p0_data = b.ty(PNode::Alias { trait_: as_ctx, name: crate::ids::Name::of("Data"), args: b_p0_l });
        let c_param = b.param("c");
        let lc = b.local(c_param);
        let f = b.path_args("data_and_store", &[b_mut_p0, b_p0_data]);
        let call = b.call(f, &[lc]);
        let root = b.block(&[], Some(call));
        let c = check_body(&fx, &b, owner_generic, root, Plant::None, InferPlant::None);
        assert_eq!(c.diags.len(), 0, "{:?}", c.diags);
        let rows = c.call_selections(call);
        assert!(
            rows.iter().any(|r| r.trait_ == my_from && r.name.is_none() && matches!(r.selected, Selected::Impl { imp, .. } if imp == impl_from_store)),
            "expected `MyFrom` impl row on generic call: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.trait_ == as_ctx && r.name == Some(crate::ids::Name::of("Data")) && matches!(r.selected, Selected::Impl { .. })),
            "generic `<P as AsContext>::Data` must not fabricate an `Impl` row: {rows:?}"
        );
    }
}
