//! Unit tests for the `#[blaze::capability]` sans-IO firewall (`rules/capability.rs`,
//! `blaze::capability_in_sans_io`):
//! - direct `#[blaze::capability]` field in a `#![blaze::sans_io]` struct
//! - transitive capability through nested structs (`Outer.mid.cap`)
//! - generic wrapper (`Wrap<T> { inner: T }` instantiated with `Wrap<Alloc>`)
//! - borrowed capability (`&Alloc`, `&mut Fs`, `*const Net`)
//! - capability inside an `enum` variant, tuple `(u32, Alloc)`, array `[Alloc; 4]`
//! - non-sans-IO driver struct holding a capability (allowed)
//! - `#[blaze::capability]` wrapper holding another capability (allowed)
//! - function pointer field `fn(&Alloc) -> u32` and pure data in `#![blaze::sans_io]` (allowed)
//! - recursive / cyclic ADTs (`Node { next: *const Node }`) terminate cleanly
//! - body check (`blaze_typeck::check`) constructing a violating sans-IO struct emits
//!   `TypedFinding::CapabilityInSansIo` with `lint() == "capability-in-sans-io"`

use std::vec;

use blaze_own::Alloc;

use crate::fixture::{Bb, Fx};
use crate::ids::{FnPtrHeader, ItemKey, LangItem, Mutbl, Name, Safety, Abi};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::rules::args_inline::TypedFinding;
use crate::world::{CtorKind, Def, TypeAttrs};
use crate::check_adt_capabilities;

use super::check_body;

fn alloc() -> Alloc<'static> {
    Alloc::global()
}

fn names_of<'a>(xs: &'a [Name]) -> std::vec::Vec<Name> {
    xs.to_vec()
}

#[test]
fn direct_and_transitive_capability_in_sans_io_is_rejected() {
    let mut fx = Fx::new();
    let u32_ = fx.u32();

    // #[blaze::capability] struct Alloc { ptr: u32 }
    let cap_alloc = fx.strukt("Alloc", 0, &[("ptr", u32_)]);
    fx.set_attrs(cap_alloc, TypeAttrs(TypeAttrs::CAPABILITY));
    let cap_alloc_ty = fx.adt(cap_alloc, &[]);

    // #[blaze::sans_io] struct DirectBad { count: u32, scratch: Alloc }
    let direct_bad = fx.strukt("DirectBad", 0, &[("count", u32_), ("scratch", cap_alloc_ty)]);
    fx.set_attrs(direct_bad, TypeAttrs(TypeAttrs::SANS_IO));

    // struct Mid { cap: Alloc } (defined outside sans_io — still cannot be smuggled into sans_io!)
    let mid = fx.strukt("Mid", 0, &[("cap", cap_alloc_ty)]);
    let mid_ty = fx.adt(mid, &[]);

    // #[blaze::sans_io] struct TransitiveBad { inner: Mid }
    let trans_bad = fx.strukt("TransitiveBad", 0, &[("inner", mid_ty)]);
    fx.set_attrs(trans_bad, TypeAttrs(TypeAttrs::SANS_IO));

    // Non-sans-IO driver struct holding Mid & Alloc: allowed!
    let driver_ok = fx.strukt("DriverOk", 0, &[("inner", mid_ty), ("alloc", cap_alloc_ty)]);

    // #[blaze::capability] wrapper inside sans_io crate (e.g. BrandedAlloc wrapping Alloc): allowed!
    let branded_alloc = fx.strukt("BrandedAlloc", 0, &[("alloc", cap_alloc_ty)]);
    fx.set_attrs(branded_alloc, TypeAttrs(TypeAttrs::SANS_IO | TypeAttrs::CAPABILITY));

    fx.finish();

    // 1. Direct violation.
    let f1 = check_adt_capabilities(&fx, direct_bad, alloc()).expect("direct check");
    assert_eq!(f1.len(), 1);
    assert_eq!(f1[0].adt, direct_bad);
    assert_eq!(f1[0].capability, cap_alloc);
    assert_eq!(f1[0].field, Name::of("scratch"));
    assert_eq!(f1[0].via_adt, None);
    assert!(!f1[0].borrowed);
    assert_eq!(names_of(&f1[0].path), vec![Name::of("scratch")]);

    // 2. Transitive violation with full field path `inner.cap`.
    let f2 = check_adt_capabilities(&fx, trans_bad, alloc()).expect("transitive check");
    assert_eq!(f2.len(), 1);
    assert_eq!(f2[0].adt, trans_bad);
    assert_eq!(f2[0].capability, cap_alloc);
    assert_eq!(f2[0].field, Name::of("inner"));
    assert_eq!(f2[0].via_adt, Some(mid));
    assert!(!f2[0].borrowed);
    assert_eq!(names_of(&f2[0].path), vec![Name::of("inner"), Name::of("cap")]);

    // 3. Driver struct & capability definition itself are clean.
    assert!(check_adt_capabilities(&fx, driver_ok, alloc()).expect("driver").is_empty());
    assert!(check_adt_capabilities(&fx, branded_alloc, alloc()).expect("cap def").is_empty());
}

#[test]
fn borrowed_generic_enum_tuple_and_array_capabilities_are_rejected() {
    let mut fx = Fx::new();
    let u32_ = fx.u32();
    let p0 = fx.param(0);

    let fs_cap = fx.strukt("Fs", 0, &[("fd", u32_)]);
    fx.set_attrs(fs_cap, TypeAttrs(TypeAttrs::CAPABILITY));
    let fs_ty = fx.adt(fs_cap, &[]);

    let ref_fs = fx.r(Mutbl::Not, fs_ty);
    let ptr_fs = fx.ty(PNode::Ptr(Mutbl::Mut, fs_ty));
    let tup_fs = fx.tuple(&[u32_, fs_ty]);
    let usize_ = fx.usize();
    let arr_len = fx.ty(PNode::ConstValue { ty: usize_, bits: 4 });
    let arr_fs = fx.ty(PNode::Array(fs_ty, arr_len));

    // Generic wrapper `struct Wrap<T> { held: T }`
    let wrap = fx.strukt("Wrap", 1, &[("held", p0)]);
    let wrap_fs = fx.adt(wrap, &[fs_ty]);
    let wrap_u32 = fx.adt(wrap, &[u32_]);

    // Borrowed ref & raw ptr in sans_io
    let borrowed_bad = fx.strukt("BorrowedBad", 0, &[("r", ref_fs), ("p", ptr_fs)]);
    fx.set_attrs(borrowed_bad, TypeAttrs(TypeAttrs::SANS_IO));

    // Generic wrapper instantiated with capability vs pure primitive
    let gen_bad = fx.strukt("GenBad", 0, &[("w", wrap_fs)]);
    fx.set_attrs(gen_bad, TypeAttrs(TypeAttrs::SANS_IO));
    let gen_ok = fx.strukt("GenOk", 0, &[("w", wrap_u32)]);
    fx.set_attrs(gen_ok, TypeAttrs(TypeAttrs::SANS_IO));

    // Enum variant holding tuple / array of capability
    let (enum_bad, _) = fx.enum_full(
        "EnumBad",
        0,
        &[
            ("Pure", CtorKind::Braces, &[("x", u32_)]),
            ("LeakyTuple", CtorKind::Braces, &[("pair", tup_fs)]),
            ("LeakyArr", CtorKind::Braces, &[("many", arr_fs)]),
        ],
    );
    fx.set_attrs(enum_bad, TypeAttrs(TypeAttrs::SANS_IO));

    // Function pointer `fn(&Fs) -> u32` does NOT store `Fs`: must be allowed!
    let fn_sig = fx.list(&[ref_fs, u32_]);
    let fn_ptr = fx.ty(PNode::FnPtr(fn_sig, FnPtrHeader { safety: Safety::Safe, abi: Abi::Rust, c_variadic: false }));
    let fn_ptr_ok = fx.strukt("FnPtrOk", 0, &[("cb", fn_ptr)]);
    fx.set_attrs(fn_ptr_ok, TypeAttrs(TypeAttrs::SANS_IO));

    fx.finish();

    // Borrowed ref + raw ptr both caught with `borrowed == true`.
    let fb = check_adt_capabilities(&fx, borrowed_bad, alloc()).expect("borrowed");
    assert_eq!(fb.len(), 2);
    assert!(fb.iter().all(|f| f.capability == fs_cap && f.borrowed));

    // Generic wrapper `Wrap<Fs>` reports full path `["w", "held"]`; `Wrap<u32>` is clean.
    let fg = check_adt_capabilities(&fx, gen_bad, alloc()).expect("gen_bad");
    assert_eq!(fg.len(), 1);
    assert_eq!(fg[0].capability, fs_cap);
    assert_eq!(fg[0].via_adt, Some(wrap));
    assert_eq!(names_of(&fg[0].path), vec![Name::of("w"), Name::of("held")]);
    assert!(check_adt_capabilities(&fx, gen_ok, alloc()).expect("gen_ok").is_empty());

    // Enum variants holding tuple and array of capability are both caught with variant names.
    let fe = check_adt_capabilities(&fx, enum_bad, alloc()).expect("enum_bad");
    assert_eq!(fe.len(), 2);
    assert_eq!(fe[0].variant, Some(Name::of("LeakyTuple")));
    assert_eq!(fe[0].field, Name::of("pair"));
    assert_eq!(fe[1].variant, Some(Name::of("LeakyArr")));
    assert_eq!(fe[1].field, Name::of("many"));

    // Function pointer field is allowed (zero false positives).
    assert!(check_adt_capabilities(&fx, fn_ptr_ok, alloc()).expect("fn_ptr_ok").is_empty());
}

#[test]
fn cyclic_adt_terminates_cleanly_and_catches_sibling_capability() {
    let mut fx = Fx::new();
    let u32_ = fx.u32();

    let cap = fx.strukt("Clock", 0, &[("id", u32_)]);
    fx.set_attrs(cap, TypeAttrs(TypeAttrs::CAPABILITY));
    let cap_ty = fx.adt(cap, &[]);

    // Self-referential pure node: `struct PureNode { next: *const PureNode, val: u32 }`
    let pure_node_key = ItemKey(0x9001);
    let pure_node_ty = fx.adt(pure_node_key, &[]);
    let pure_ptr = fx.ty(PNode::Ptr(Mutbl::Not, pure_node_ty));
    let pure_node = fx.strukt("PureNode", 0, &[("next", pure_ptr), ("val", u32_)]);
    fx.set_attrs(pure_node, TypeAttrs(TypeAttrs::SANS_IO));

    // Self-referential node WITH a sibling capability field.
    let leaky_node = fx.strukt("LeakyNode", 0, &[("next", pure_ptr), ("clock", cap_ty)]);
    fx.set_attrs(leaky_node, TypeAttrs(TypeAttrs::SANS_IO));

    fx.finish();

    assert!(check_adt_capabilities(&fx, pure_node, alloc()).expect("pure_node").is_empty());
    let fl = check_adt_capabilities(&fx, leaky_node, alloc()).expect("leaky_node");
    assert_eq!(fl.len(), 1);
    assert_eq!(fl[0].capability, cap);
    assert_eq!(fl[0].field, Name::of("clock"));
}

#[test]
fn body_check_emits_typed_finding_when_constructing_violating_sans_io_struct() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let u32_ = fx.u32();

    let cap = fx.strukt("Net", 0, &[("sock", u32_)]);
    fx.set_attrs(cap, TypeAttrs(TypeAttrs::CAPABILITY));
    let cap_ty = fx.adt(cap, &[]);

    let bad = fx.strukt("BadCore", 0, &[("net", cap_ty)]);
    fx.set_attrs(bad, TypeAttrs(TypeAttrs::SANS_IO));
    let bad_ty = fx.adt(bad, &[]);

    fx.use_path("BadCore", Def::Adt(bad));
    let owner = fx.func("make_bad", 0, &[cap_ty], bad_ty, vec![]);
    fx.finish();

    let mut b = Bb::new();
    let n = b.param("n");
    let e_n = b.local(n);
    let e_struct = b.struct_lit(&["BadCore"], &[("net", e_n)], None);
    let root = b.block(&[], Some(e_struct));

    let checked = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    assert!(checked.diags.is_empty(), "no type errors: {:?}", checked.diags);
    assert_eq!(checked.findings.len(), 1, "expected 1 capability finding, got {:?}", checked.findings);
    assert_eq!(checked.findings[0].lint(), "capability-in-sans-io");
    assert!(
        matches!(
            checked.findings[0],
            TypedFinding::CapabilityInSansIo { adt, capability, field, via_adt: None, borrowed: false, .. }
                if adt == bad && capability == cap && field == Name::of("net")
        ),
        "unexpected finding: {:?}",
        checked.findings[0]
    );
}
