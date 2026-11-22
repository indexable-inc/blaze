//! The read-back harness on hand-made records written at the layout's offsets: the leaves the
//! emitted code loads are the values written, in declaration order.

use std::alloc::Global;

use blaze_own::Alloc;
use blaze_typeck::{AdtKind, PNode, UintTy};

use super::read_back;
use crate::data::{DataAlloc, DataRecord, DataReloc, DataTarget};
use crate::layout::tests::{F, G, World, lay};
use crate::layout::{IntRepr, LayoutCx, Layouts, ReprAttrs};

fn record(allocs: Vec<(Vec<u8>, u8, Vec<DataReloc>)>) -> DataRecord<Global> {
    let mut v = Vec::new_in(Global);
    for (bytes, align, relocs) in allocs {
        v.push(DataAlloc { bytes: bytes.to_vec_in(Global), align, mutable: false, relocs: relocs.to_vec_in(Global) });
    }
    DataRecord { allocs: v }
}

fn put(bytes: &mut [u8], at: u64, value: u64, size: usize) {
    bytes[at as usize..at as usize + size].copy_from_slice(&value.to_le_bytes()[..size]);
}

/// `struct Mixed { a: u8, b: u64, c: u16, d: bool }`: repr(Rust) reorders the fields; the leaves
/// come back in declaration order.
#[test]
fn a_reordered_struct_reads_back_by_declaration() {
    let mut w = World::default();
    let mut g = G::new();
    let mixed = w.def(
        AdtKind::Struct,
        0,
        &[&[F::Leaf(PNode::Uint(UintTy::U8)), F::Leaf(PNode::Uint(UintTy::U64)), F::Leaf(PNode::Uint(UintTy::U16)), F::Leaf(PNode::Bool)]],
        ReprAttrs::RUST,
    );
    let t = g.adt(mixed, &[]);
    let mut ls = Layouts::new_in(Global);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    let mut bytes = vec![0u8; ls.get(id).size as usize];
    for (i, (v, size)) in [(1u64, 1), (0x0102_0304_0506_0708, 8), (0xbeef, 2), (1, 1)].into_iter().enumerate() {
        put(&mut bytes, ls.field_offset(id, i as u32).unwrap(), v, size);
    }
    assert_ne!(ls.field_offset(id, 1), Some(8), "the u64 is moved ahead of the u8 by repr(Rust)");
    let rec = record(vec![(bytes, ls.get(id).align, vec![])]);
    let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: Alloc::global() };
    let got = read_back(t, &rec, &mut ls, &mut cx).unwrap();
    assert_eq!(got, [1, 0x0102_0304_0506_0708, 0xbeef, 1]);
}

/// `#[repr(u8)] enum E { A = 3, B(u32) = 9, C }` and `[E::A, E::B(7), E::C]`: each element's
/// tag, then its variant's fields.
#[test]
fn an_array_of_a_tagged_enum_reads_tags_and_payloads() {
    let mut w = World::default();
    let mut g = G::new();
    let repr = ReprAttrs { int: Some(IntRepr::Unsigned(UintTy::U8)), ..ReprAttrs::RUST };
    let e = w.def(AdtKind::Enum, 0, &[&[], &[F::Leaf(PNode::Uint(UintTy::U32))], &[]], repr);
    w.def_mut(e).discrs = vec![Some(3), Some(9), None];
    let et = g.adt(e, &[]);
    let at = g.array(et, 3);
    let mut ls = Layouts::new_in(Global);
    let eid = lay(&w, &mut g, &mut ls, et).unwrap();
    let aid = lay(&w, &mut g, &mut ls, at).unwrap();
    let stride = ls.get(eid).size;
    let mut bytes = vec![0u8; ls.get(aid).size as usize];
    for (i, v) in [0u32, 1, 2].into_iter().enumerate() {
        let tw = ls.tag_write(eid, v).unwrap();
        put(&mut bytes, i as u64 * stride + tw.offset, tw.value as u64, tw.scalar.prim.size() as usize);
    }
    let payload = ls.field_offset(ls.variant(eid, 1).unwrap(), 0).unwrap();
    put(&mut bytes, stride + payload, 7, 4);
    let rec = record(vec![(bytes, ls.get(aid).align, vec![])]);
    let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: Alloc::global() };
    let got = read_back(at, &rec, &mut ls, &mut cx).unwrap();
    assert_eq!(got, [3, 9, 7, 10], "tags 3, 9 (then B's 7), 10");
}

/// `&(u16, &[u8])` = `&(5, &[1, 2, 3])`: two relocations and a wide pointer.
#[test]
fn references_and_a_slice_are_followed() {
    let w = World::default();
    let mut g = G::new();
    let u8t = g.n(PNode::Uint(UintTy::U8));
    let u16t = g.n(PNode::Uint(UintTy::U16));
    let slice = g.n(PNode::Slice(u8t));
    let sref = g.r(slice);
    let tup = g.tuple(&[u16t, sref]);
    let t = g.r(tup);
    let mut ls = Layouts::new_in(Global);
    let tid = lay(&w, &mut g, &mut ls, tup).unwrap();
    let mut tb = vec![0u8; ls.get(tid).size as usize];
    put(&mut tb, ls.field_offset(tid, 0).unwrap(), 5, 2);
    let wide = ls.field_offset(tid, 1).unwrap();
    put(&mut tb, wide + 8, 3, 8);
    let rec = record(vec![
        (vec![0; 8], 3, vec![DataReloc { offset: 0, target: DataTarget::Local(1) }]),
        (tb, ls.get(tid).align, vec![DataReloc { offset: wide as u32, target: DataTarget::Local(2) }]),
        (vec![1, 2, 3], 0, vec![]),
    ]);
    let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: Alloc::global() };
    let got = read_back(t, &rec, &mut ls, &mut cx).unwrap();
    assert_eq!(got, [5, 3, 1, 2, 3]);
}
