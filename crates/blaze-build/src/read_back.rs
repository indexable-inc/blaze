//! Reading a value's record back through emitted code (the `testing` feature): the harness in
//! which code the builder emits loads a constant's bytes at [`crate::layout`]'s offsets, so the
//! constant evaluator's writes and the builder's reads cannot drift apart unseen.
//!
//! [`read_back`] places the record's allocations in memory (its `Local` relocations filled),
//! builds one LIR function `fn(base: ptr, out: ptr)` that loads every scalar leaf of the value
//! and stores each, zero-extended, to `out[k]`, emits it with blaze-emit, runs it and returns the
//! leaves. The order is depth first:
//!
//! - a struct's or tuple's fields by declaration index, an array's elements in order;
//! - an enum: its tag's raw bits (at the tag's offset and width), then the fields of the variant
//!   [`Layouts::variant_of_tag`] decodes from the record's tag; an enum without a tag
//!   (`Variants::Single`) has no tag leaf;
//! - `&T` of a sized `T`: no leaf for the address (it differs per run), then `T`'s leaves
//!   through the pointer the code loaded;
//! - `&[T]` and `&str`: the length, then each element's leaves through the data pointer the
//!   code loaded (`str`: one `u8` leaf a byte);
//! - `bool`, `char` and integers to 64 bits: one leaf each.
//!
//! Which variant and how many elements are read from the placed bytes on the host side, by
//! the same layout; the loads that give the leaves are the emitted code's.

use std::alloc::{Allocator, Global};

use blaze_emit::{Code, Isa, Os, emit};
use blaze_inproc::exec::Executable;
use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv};
use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};
use blaze_typeck::{PNode, PTy, UintTy};

use crate::data::{DataRecord, DataTarget};
use crate::layout::{Fields, LayoutCx, LayoutError, Layouts, Prim, Variants};

/// Why a value was not read back.
#[derive(Debug)]
pub enum ReadBackError {
    Layout(LayoutError),
    /// A part of the value the harness does not read, named.
    Refused { ty: PTy, why: &'static str },
    /// The record breaks its form (a relocation past its allocation, a bad local).
    Record { why: &'static str },
    Build(blaze_lir::BuildError),
    Emit(blaze_emit::EmitError),
}

impl core::fmt::Display for ReadBackError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReadBackError::Layout(e) => write!(f, "{e}"),
            ReadBackError::Refused { ty, why } => write!(f, "type {}: the read-back harness does not read {why}", ty.0),
            ReadBackError::Record { why } => write!(f, "the record: {why}"),
            ReadBackError::Build(e) => write!(f, "the read-back function: {e}"),
            ReadBackError::Emit(e) => write!(f, "emitting the read-back function: {e}"),
        }
    }
}

impl core::error::Error for ReadBackError {}

impl From<LayoutError> for ReadBackError {
    fn from(e: LayoutError) -> ReadBackError {
        ReadBackError::Layout(e)
    }
}

/// One value to read: its type, the LIR pointer it is reached through and the byte offset from
/// it, and the same place's host address.
#[derive(Clone, Copy)]
struct Part {
    ty: PTy,
    ptr: Value,
    off: i64,
    host: u64,
}

fn host_read(at: u64, bytes: u64) -> u64 {
    let mut word = [0u8; 8];
    // SAFETY: every host address read is inside an allocation `read_back` placed and keeps
    // alive (checked against the allocation's length by the layout of the value read).
    unsafe { core::ptr::copy_nonoverlapping(at as *const u8, word.as_mut_ptr(), bytes as usize) };
    u64::from_le_bytes(word)
}

fn int_ty(bytes: u64) -> Option<(Ty, u8)> {
    match bytes {
        1 => Some((Ty::I8, 0)),
        2 => Some((Ty::I16, 1)),
        4 => Some((Ty::I32, 2)),
        8 => Some((Ty::I64, 3)),
        _ => None,
    }
}

/// The value of type `ty` that `record` holds, read back through emitted code (module doc).
pub fn read_back<A: Allocator + Clone>(
    ty: PTy,
    record: &DataRecord<A>,
    layouts: &mut Layouts<A>,
    cx: &mut LayoutCx<'_, '_>,
) -> Result<Vec<u64, Global>, ReadBackError> {
    // Place every allocation at its alignment in one buffer of 16-byte words, then fill its
    // relocations: each word is the target allocation's address plus the addend it holds.
    let mut at = Vec::with_capacity_in(record.allocs.len(), Global);
    let mut end = 0u64;
    for a in record.allocs.iter() {
        let align = 1u64 << a.align.min(4);
        if a.align > 4 {
            return Err(ReadBackError::Record { why: "an allocation aligned past 16 bytes" });
        }
        let start = end.next_multiple_of(align);
        at.push(start);
        end = start + a.bytes.len() as u64;
    }
    let mut words: Vec<u128, Global> = Vec::new_in(Global);
    words.resize(end.div_ceil(16) as usize + 1, 0);
    let base = words.as_mut_ptr() as u64;
    // SAFETY: `words` has `end` bytes or more, 16-aligned, alive until the function returns;
    // nothing else refers to it while it is filled.
    let buf = unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), end as usize) };
    for (a, &start) in record.allocs.iter().zip(at.iter()) {
        buf[start as usize..start as usize + a.bytes.len()].copy_from_slice(&a.bytes);
    }
    for (a, &start) in record.allocs.iter().zip(at.iter()) {
        for r in a.relocs.iter() {
            let word = r.offset as usize;
            if word + 8 > a.bytes.len() {
                return Err(ReadBackError::Record { why: "a relocation past its allocation's end" });
            }
            let DataTarget::Local(k) = r.target else {
                return Err(ReadBackError::Record { why: "a relocation to a static or a fn (only a record's own allocations are placed here)" });
            };
            let to = *at.get(k as usize).ok_or(ReadBackError::Record { why: "a relocation to an allocation the record lacks" })?;
            let w = start as usize + word;
            let mut addend = [0u8; 8];
            addend.copy_from_slice(&buf[w..w + 8]);
            let value = (base + to).wrapping_add_signed(i64::from_le_bytes(addend));
            buf[w..w + 8].copy_from_slice(&value.to_le_bytes());
        }
    }

    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let root = b.param(Ty::PTR);
    let out = b.param(Ty::PTR);
    let mut leaves = 0u32;
    let mut stack: Vec<Part, Global> = Vec::new_in(Global);
    stack.push(Part { ty, ptr: root, off: 0, host: base + at[0] });
    let mut parts: Vec<PTy, A> = Vec::new_in(layouts.alloc());
    let leaf =|b: &mut Builder<Global, Full>, v: Value, leaves: &mut u32| {
        let wide = match b.ty(v) == Ty::I64 {
            true => v,
            false => b.convert(Op::Zext, Flags::NONE, Ty::I64, v),
        };
        b.store(Flags::NONE, wide, out, (*leaves * 8) as i32, MemAttrs::aligned(3));
        *leaves += 1;
    };
    while let Some(p) = stack.pop() {
        let refused = |why| ReadBackError::Refused { ty: p.ty, why };
        let id = layouts.of(p.ty, cx)?;
        let l = *layouts.get(id);
        if l.is_zst() {
            continue;
        }
        let off = i32::try_from(p.off).map_err(|_| refused("a part past 2 GiB from its pointer"))?;
        let node = cx.ground.view().node(p.ty).map_err(|_| refused("a type past the ground table"))?;
        match node {
            PNode::Ref(_, inner) => {
                let addr = b.load(Ty::PTR, Flags::NONE, p.ptr, off, MemAttrs::aligned(3));
                let host = host_read(p.host, 8);
                let inner_node = cx.ground.view().node(inner).map_err(|_| refused("a type past the ground table"))?;
                let elem = match inner_node {
                    PNode::Str => Some(cx.ground.intern(PNode::Uint(UintTy::U8)).map_err(|_| refused("a full ground table"))?),
                    PNode::Slice(e) => Some(e),
                    _ => None,
                };
                match elem {
                    None => stack.push(Part { ty: inner, ptr: addr, off: 0, host }),
                    Some(e) => {
                        let len = b.load(Ty::I64, Flags::NONE, p.ptr, off + 8, MemAttrs::aligned(3));
                        leaf(&mut b, len, &mut leaves);
                        let n = host_read(p.host + 8, 8);
                        let eid = layouts.of(e, cx)?;
                        let stride = layouts.get(eid).size;
                        for i in (0..n).rev() {
                            stack.push(Part { ty: e, ptr: addr, off: (i * stride) as i64, host: host + i * stride });
                        }
                    }
                }
            }
            PNode::Ptr(..) => return Err(refused("a raw pointer (it may dangle)")),
            PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) => {
                let Some(s) = l.scalar else { return Err(refused("a primitive with no scalar layout")) };
                let Prim::Int { bytes, .. } = s.prim else { return Err(refused("a float")) };
                let (t, a) = int_ty(u64::from(bytes)).ok_or(refused("an integer of 128 bits"))?;
                let v = b.load(t, Flags::NONE, p.ptr, off, MemAttrs::aligned(a));
                leaf(&mut b, v, &mut leaves);
            }
            PNode::Float(_) => return Err(refused("a float")),
            PNode::Array(..) | PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Adt(..) => {
                parts.clear();
                layouts.field_types(p.ty, cx, &mut parts)?;
                // The fields of the variant the record holds, and that variant's layout.
                let (first, vid) = match l.variants {
                    Variants::Single { index: 0 } => (0, Some(id)),
                    Variants::Single { .. } => {
                        return Err(refused("an enum laid out as its one present variant when that is not its first"));
                    }
                    Variants::Multiple { .. } => {
                        let (tag_off, tag) = layouts.read_tag(id).ok_or(refused("an enum with no tag to read"))?;
                        let (t, a) = int_ty(tag.prim.size()).ok_or(refused("a tag of no integer width"))?;
                        let tag_at = i32::try_from(p.off + tag_off as i64).map_err(|_| refused("a tag past 2 GiB"))?;
                        let v = b.load(t, Flags::NONE, p.ptr, tag_at, MemAttrs::aligned(a));
                        leaf(&mut b, v, &mut leaves);
                        let bits = host_read(p.host + tag_off, tag.prim.size());
                        let index = layouts.variant_of_tag(id, u128::from(bits)).ok_or(refused("a tag no variant has"))?;
                        (variant_first(layouts, id, index), layouts.variant(id, index))
                    }
                };
                match l.fields {
                    Fields::Array { elem: _, stride, count } => {
                        let e = parts[0];
                        for i in (0..count).rev() {
                            stack.push(Part { ty: e, ptr: p.ptr, off: p.off + (i * stride) as i64, host: p.host + i * stride });
                        }
                    }
                    Fields::Struct { .. } | Fields::Primitive => {
                        let Some(vid) = vid else { continue };
                        let Fields::Struct { len, .. } = layouts.get(vid).fields else { continue };
                        for i in (0..len).rev() {
                            let fo = layouts.field_offset(vid, i).ok_or(refused("a field the layout lacks"))?;
                            let ft = *parts.get((first + i) as usize).ok_or(refused("a field type the layout lacks"))?;
                            stack.push(Part { ty: ft, ptr: p.ptr, off: p.off + fo as i64, host: p.host + fo });
                        }
                    }
                    Fields::Union { .. } => return Err(refused("a union (which field is live is not in the layout)")),
                }
            }
            _ => return Err(refused("this kind of type")),
        }
    }
    b.ret(&[]);
    let func = b.finish_verified().map_err(ReadBackError::Build)?;
    let mut code = Code::new_in(Global);
    // Every Apple M-series core implements FEAT_LSE; the function has no atomics anyway.
    emit(func.verified(), &[], Isa { lse: true, os: Os::MacOs }, &mut code).map_err(ReadBackError::Emit)?;
    if !code.fixups.is_empty() || !code.relocs.is_empty() {
        return Err(ReadBackError::Record { why: "a read-back function with words to place (it calls nothing)" });
    }
    let exec = Executable::written(code.bytes.len(), 0, |o, _| o.copy_from_slice(&code.bytes))
        .map_err(|_| ReadBackError::Record { why: "the code did not map" })?;
    let mut got: Vec<u64, Global> = Vec::new_in(Global);
    got.resize(leaves as usize, 0);
    // SAFETY: the code is an emitted C function `fn(ptr, ptr)` that reads only the placed
    // allocations (alive in `words`) and writes `leaves` words to `got`.
    let f: extern "C" fn(*const u8, *mut u64) = unsafe { exec.as_fn() };
    f((base + at[0]) as *const u8, got.as_mut_ptr());
    drop(words);
    Ok(got)
}

/// Where variant `index`'s fields start in `field_types`' list of a tagged enum: after the
/// fields of the variants before it.
fn variant_first<A: Allocator + Clone>(layouts: &Layouts<A>, id: crate::layout::LayoutId, index: u32) -> u32 {
    let mut first = 0u32;
    for v in 0..index {
        if let Some(vid) = layouts.variant(id, v)
            && let Fields::Struct { len, .. } = layouts.get(vid).fields
        {
            first += len;
        }
    }
    first
}

#[cfg(test)]
mod tests;
