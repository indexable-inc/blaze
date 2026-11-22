//! Paths to constants the walk builds from their values (walk.rs `raw`'s `Expr::Path` arm and
//! walk/promote.rs `data_rows`), never from a body.
//!
//! The Reference, "Generic parameters", "Const generics": "const parameters may be used ... as a
//! standalone expression", so `N` in `fn f<const N: u32>() -> u32 { N + 1 }` is a value, and each
//! instance has its own (core's `<[T]>::as_chunks::<N>` reads `N != 0`). "Constant evaluation":
//! a constant of a fn pointer type (`const F: fn() -> u32 = seven;`) is a value whose word is
//! the fn's address, so the fn is code the function using `F` refers to (std's
//! `io::Error::from_raw_os_error` reaches one).

use super::*;
use crate::data::{DataReloc, DataTarget};

/// `fn main<const N: u32>() -> u32 { N + 1 }` at `N = 41` returns 42 and at `N = 6` returns 7:
/// the path `N` is the instance's argument. Before the `Def::TyParam` arm the build refused it
/// ("a path to other than a fn item, a unit struct or a unit variant"); a version that reads
/// anything but the instance's argument returns the same for both and fails one assertion.
///
/// Prints: `test tests::const_paths::a_const_parameter_is_the_instances_argument ... ok`
#[test]
fn a_const_parameter_is_the_instances_argument() {
    let mut w = TestWorld::new();
    w.generic_func(1, "main", 1, &[], U32);
    let u32_ = w.ty(U32);
    w.const_params.push((0, u32_));
    w.param_names.push((Name::of("N"), 0));
    let mut b = BodyBuf::new();
    let n = b.path("N");
    let n = b.e(Expr::Path(n));
    let one_ = b.int(1, None);
    let root = b.bin(BinOp::Add, n, one_);
    for (arg, want) in [(41u128, 42u128), (6, 7)] {
        let mut ground = PortableTys::new_in(Alloc::global());
        let ty = ground.intern(U32).expect("a type interns");
        let value = ground.intern(PNode::ConstValue { ty, bits: arg }).expect("a value interns");
        let built = build_instance(&w, &b, MAIN, root, &[value], &mut ground).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(one(&built, &[]), want, "`N + 1` at N = {arg}");
    }
}

/// `const F: fn() -> u32 = seven; fn main() -> fn() -> u32 { F }`: the evaluator's record of
/// `F` is one word relocated to `seven`'s instance key. The built `main` loads the word from a
/// data row whose relocation is a `Func` reference, and `seven` is its one callee instance
/// (`Built::refs`), as `main() -> fn() -> u32 { seven }` would file it. With no answer for the
/// key the build is refused by name. Before this, the build refused every such constant ("a
/// const holding a fn pointer"), and the first assertion failed.
///
/// Prints: `test tests::const_paths::a_constants_fn_pointer_is_a_reference_of_the_function ... ok`
#[test]
fn a_constants_fn_pointer_is_a_reference_of_the_function() {
    let mut w = TestWorld::new();
    let u32_ = w.ty(U32);
    let io = w.types.intern_list(&[u32_]).expect("a list interns");
    let fn_ptr = PNode::FnPtr(io, blaze_typeck::FnPtrHeader::RUST);
    w.func(1, "main", &[], fn_ptr);
    w.func(40, "seven", &[], U32);
    w.func(41, "F", &[], fn_ptr);
    let key = crate::InstanceKey(blaze_ids::ContentId::of("test fn", b"seven"));
    let mut bytes = Vec::new_in(Global);
    bytes.extend_from_slice(&[0u8; 8]);
    let mut relocs = Vec::new_in(Global);
    relocs.push(DataReloc { offset: 0, target: DataTarget::Fn(key) });
    let mut allocs = Vec::new_in(Global);
    allocs.push(DataAlloc { bytes, align: 3, mutable: false, relocs });
    w.consts.push((ItemKey(41), Some(DataRecord { allocs })));
    let seven = Instance { item: ItemKey(40), args: List::EMPTY };
    w.const_fns.push((key, seven));
    let mut b = BodyBuf::new();
    let f = b.path("F");
    let root = b.e(Expr::Path(f));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(built.refs.as_slice(), &[seven], "`seven` is the function's one reference");
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("data_addr"), "`F` is read from its data row:\n{text}");

    w.const_fns.clear();
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("holds a fn pointer to instance"), "{refused}");
}
