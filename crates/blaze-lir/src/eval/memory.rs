//! The reference evaluator's memory: one store of bytes for the whole run, every stack slot of
//! every frame an allocation in it, and pointers as provenance plus offset (docs/v2/plan/design
//! /lir.md, "Memory, pointers and provenance": "`ptr` [is] the target's pointer, distinct from
//! its integer. This is what carries provenance"; "Bytes may be uninitialized and pointers carry
//! provenance, as in Rust's abstract machine").
//!
//! A slot is named by a run-wide id: a frame's slot `k` is the frame's base plus `k`, and a call
//! opens its callee's slots at the next free ids, which are never used again in the run. So
//! `Val::Ptr { slot, offset }` is the same pointer in every frame: a pointer passes into and out
//! of calls as any value does, and one into a returned call's slot names an id no allocation
//! holds any more, which an access refuses as undefined behaviour by name. The root frame's base
//! is 0, so a run with no call numbers its slots as the function does.
//!
//! Each byte is its value, whether it is initialized, and the provenance of the pointer whose
//! byte it is: a stored pointer writes its offset's bytes, each tagged with its slot and its
//! place in the pointer; a load of a pointer takes them back only whole and in order; an
//! integer or float load of a pointer's bytes is refused (the evaluator gives a slot no address,
//! so it has no integer to give). `memcpy`, `memmove` and `memset` work on these bytes, so a copy
//! keeps provenance as Rust's typed and untyped copies do.
//!
//! A pointer of no provenance (`null`, `from_exposed`, a pointer loaded from bytes no pointer
//! wrote) has `slot == Val::NO_SLOT` and its address as `offset`. Nothing in a run is ever
//! exposed (`expose` of a slot's pointer is refused), so `from_exposed` has no provenance to
//! pick, and an access of a non-zero size through such a pointer is undefined behaviour (the
//! library's `with_exposed_provenance`: "If there is no previously 'exposed' provenance that
//! justifies the way the returned pointer will be used, the program has undefined behavior").
//!
//! Memory promises: an access's alignment is checked against its slot's; a load's `range`,
//! `nonnull` and pointee alignment (`!align`) are checked on the loaded value, each broken one
//! undefined behaviour by name; `noundef` asks what every scalar load already asks; an
//! `invariant` load is refused, since the evaluator does not check that memory never changes.
//!
//! The program's data and functions (eval/data.rs) are allocations too, placed once per run
//! with ids from the same count, their bytes before every stack slot's; no frame's return
//! closes them. A function's allocation has no bytes: an access through it is undefined.
//!
//! Storage: the crate's present form, std `Vec` in the run's allocator `A` (blaze-lir does not
//! depend on blaze-own); this file replaces eval.rs's `Memory` and its four `Vec` fields.

use alloc::vec::Vec;
use core::{alloc::Allocator, ops::Range};

use super::{
    EvalError, Outcome, Run, Val,
    data::{Data, Kind},
    promises,
};
use crate::{
    IntCond, Op, Ty, Value, View,
    arith::{compare, mask, sext},
    func::{MemAttrsRec, SlotId},
};

impl Val {
    /// The `slot` of a pointer with no provenance; its `offset` is then its address. No slot
    /// has this id.
    pub const NO_SLOT: u32 = u32::MAX;
}

/// One byte of memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Byte {
    pub(super) value: u8,
    pub(super) init: bool,
    /// Its place in the pointer `prov` names; 0 for a byte of no pointer.
    pub(super) index: u8,
    /// The allocation (slot, datum or function) of the pointer this byte is part of, or
    /// `Val::NO_SLOT`.
    pub(super) prov: u32,
}

const UNINIT: Byte = Byte { value: 0, init: false, index: 0, prov: Val::NO_SLOT };

/// A byte no pointer wrote.
pub(super) const fn plain(value: u8) -> Byte {
    Byte { value, init: true, index: 0, prov: Val::NO_SLOT }
}

/// One allocation of a run: a stack slot of one frame, a datum or a function (eval/data.rs).
#[derive(Clone, Copy, Debug)]
pub(super) struct Alloc {
    pub(super) id: u32,
    /// Its first byte in `Memory::bytes`.
    pub(super) start: usize,
    pub(super) size: u32,
    /// The log2 of its alignment.
    pub(super) align: u8,
    pub(super) live: bool,
    /// What it is: a datum is never dead and no frame's return closes it.
    pub(super) kind: Kind,
}

/// The slots of every frame of a run, the innermost frame's last, and the data and functions
/// the run placed (eval/data.rs), whose bytes come first.
pub(super) struct Memory<A: Allocator> {
    /// The stack slots, by id ascending.
    pub(super) allocs: Vec<Alloc, A>,
    pub(super) bytes: Vec<Byte, A>,
    /// The id the next slot, datum or function takes: every id below it was used once.
    pub(super) next: u32,
    /// The data and functions placed.
    pub(super) data: Data<A>,
}

impl<A: Allocator + Clone> Memory<A> {
    pub(super) fn new_in(alloc: A) -> Memory<A> {
        Memory { allocs: Vec::new_in(alloc.clone()), bytes: Vec::new_in(alloc.clone()), next: 0, data: Data::new_in(alloc) }
    }

    /// Opens the slots of a frame of `view`, live and uninitialized: the id of its slot 0, or
    /// `None` when the run would pass the ids below `Val::NO_SLOT`.
    pub(super) fn open(&mut self, view: View<'_>) -> Option<u32> {
        let base = self.next;
        let count = u32::try_from(view.slots.len()).ok()?;
        let end = base.checked_add(count).filter(|&end| end <= Val::NO_SLOT)?;
        let mut start = self.bytes.len();
        for (id, slot) in (base..end).zip(view.slots.iter()) {
            self.allocs.push(Alloc { id, start, size: slot.size, align: slot.align, live: true, kind: Kind::Stack });
            start += slot.size as usize;
        }
        self.bytes.resize(start, UNINIT);
        self.next = end;
        Some(base)
    }

    /// Closes the frame whose slot 0 is `base`, with any opened after it. Its ids stay used.
    pub(super) fn close(&mut self, base: u32) {
        let first = self.allocs.partition_point(|it| it.id < base);
        if let Some(alloc) = self.allocs.get(first) {
            self.bytes.truncate(alloc.start);
        }
        self.allocs.truncate(first);
    }

    /// The allocation `id` names: an open frame's slot, or a datum or function placed.
    pub(super) fn find(&self, id: u32) -> Option<Alloc> {
        if let Ok(at) = self.allocs.binary_search_by_key(&id, |it| it.id) {
            return Some(self.allocs[at]);
        }
        let at = self.data.fixed.binary_search_by_key(&id, |it| it.id).ok()?;
        Some(self.data.fixed[at])
    }
}

/// How an access ends: the bytes it touches, or the undefined behaviour it is.
type Access = Result<Range<usize>, &'static str>;

impl<A: Allocator + Clone> Run<'_, A> {
    pub(super) fn refuse(&self, inst: usize, what: &'static str) -> EvalError {
        EvalError::Unsupported { inst, op: self.view.op(inst), what }
    }

    /// `offset` as an address of the target's width.
    fn address(&self, offset: i128) -> u128 {
        offset as u128 & mask(self.view.head.ptr_bits)
    }

    /// The offset `ptr_add inbounds` of a pointer `offset` bytes into slot `slot` steps to by
    /// `by` bytes, by the slot's size and liveness (`promises::inbounds_step`); or the promise
    /// it breaks. A pointer of no provenance, or into a returned call's slot, takes only a zero
    /// step ("`self` must be derived from a pointer to some allocation").
    pub(super) fn inbounds(&self, slot: u32, offset: i128, by: i128) -> Result<i128, &'static str> {
        match self.memory.find(slot) {
            Some(alloc) if by != 0 && matches!(alloc.kind, Kind::Func { .. }) => {
                Err("an `inbounds` step of a function pointer")
            }
            Some(alloc) => promises::inbounds_step(offset, by, i128::from(alloc.size), alloc.live),
            None if by == 0 => Ok(offset),
            None if slot == Val::NO_SLOT => Err("an `inbounds` step of a pointer with no provenance"),
            None => Err("an `inbounds` step of a pointer into a returned call's slot"),
        }
    }

    /// `slot_live` or `slot_dead` of the running frame's slot `slot`: its bytes uninitialized.
    pub(super) fn restart(&mut self, inst: usize, slot: SlotId, live: bool) -> Result<(), EvalError> {
        let id = self.base + slot.0;
        let at = self.memory.allocs.binary_search_by_key(&id, |it| it.id);
        let Ok(at) = at else { return Err(self.refuse(inst, "a slot its frame did not open")) };
        let alloc = &mut self.memory.allocs[at];
        alloc.live = live;
        let (start, size) = (alloc.start, alloc.size as usize);
        self.memory.bytes[start..start + size].fill(UNINIT);
        Ok(())
    }

    /// The bytes an access of `size` bytes at `ptr` plus `at`, which says it is aligned to
    /// `1 << align`, touches.
    pub(super) fn access(&self, inst: usize, ptr: Val, at: i128, size: u64, align: u8) -> Result<Access, EvalError> {
        let Val::Ptr { slot, offset } = ptr else {
            return Err(self.refuse(inst, "an access through a value that is not a pointer"));
        };
        let offset = offset.wrapping_add(at);
        if slot == Val::NO_SLOT {
            let address = self.address(offset);
            return Ok(match (address, size) {
                (0, _) => Err("an access through a null pointer"),
                (_, 0) if address.trailing_zeros() < u32::from(align) => Err("an access less aligned than it says"),
                (_, 0) => Ok(0..0),
                _ => Err("an access through a pointer with no provenance"),
            });
        }
        let Some(alloc) = self.memory.find(slot) else {
            return match slot < self.memory.next {
                true => Ok(Err("an access through a pointer to a returned call's slot")),
                false => Err(self.refuse(inst, "a pointer to no slot of the run")),
            };
        };
        if let Kind::Func { .. } = alloc.kind {
            return Ok(Err("an access through a function pointer"));
        }
        if !alloc.live {
            return Ok(Err("an access to a dead slot"));
        }
        let size = i128::from(size);
        if offset < 0 || offset.checked_add(size).is_none_or(|end| end > i128::from(alloc.size)) {
            return Ok(Err("an access out of its slot"));
        }
        if align > alloc.align || (offset as u128).trailing_zeros() < u32::from(align) {
            return Ok(Err("an access less aligned than it says"));
        }
        let first = alloc.start + offset as usize;
        Ok(Ok(first..first + size as usize))
    }

    /// The bytes of a load or store of `ty`: a scalar of whole bytes.
    fn scalar_bytes(&self, inst: usize, ty: Ty) -> Result<u64, EvalError> {
        if !ty.is_scalar() {
            return Err(self.refuse(inst, "an access of a vector or compound value"));
        }
        let bits = ty.bits(self.view.head.ptr_bits);
        if bits % 8 != 0 {
            return Err(self.refuse(inst, "an access of a width not of whole bytes"));
        }
        Ok(u64::from(bits / 8))
    }

    /// The alignment of a store or bulk operation and of a copy's source: one told no load's
    /// promise.
    fn plain_attrs(&self, inst: usize) -> Result<(u8, u8), EvalError> {
        match self.view.mem_attrs(inst) {
            None => Ok((0, 0)),
            Some(attrs) if attrs.range == 0 && attrs.pointee_align == 0 && attrs.bits == 0 => {
                Ok((attrs.align, attrs.src_align))
            }
            Some(_) => Err(self.refuse(inst, "a load's memory promise on another operation")),
        }
    }

    /// `load`: the scalar at its pointer plus its offset (lir.md: "A load reads the bytes there";
    /// "A load of uninitialized bytes as a scalar is UB in Rust and poison in LIR").
    pub(super) fn load(&self, inst: usize) -> Result<Result<Val, &'static str>, EvalError> {
        let mut o = self.view.operands(inst);
        let ptr = self.get(inst, o.value())?;
        let at = i128::from(o.offset());
        let ty = self.view.ty(Value(inst as u32));
        let width = ty.bits(self.view.head.ptr_bits);
        let size = self.scalar_bytes(inst, ty)?;
        let (align, range, pointee, bits) = match self.view.mem_attrs(inst) {
            None => (0, 0, 0, 0),
            Some(attrs) => (attrs.align, attrs.range, attrs.pointee_align, attrs.bits),
        };
        // Read-only data never changes, so an `invariant` load of it (a vtable's) keeps its
        // promise; elsewhere that the memory never changes is not checked.
        if bits & MemAttrsRec::INVARIANT != 0 && !self.read_only(ptr) {
            return Err(self.refuse(inst, "an `invariant` load of other than read-only data: that memory never changes is not checked"));
        }
        let span = match self.access(inst, ptr, at, size, align)? {
            Ok(it) => it,
            Err(why) => return Ok(Err(why)),
        };
        let bytes = &self.memory.bytes[span];
        if !bytes.iter().all(|it| it.init) {
            return Ok(Err("a load of uninitialized bytes"));
        }
        let value = bytes.iter().rev().fold(0u128, |it, byte| (it << 8) | u128::from(byte.value));
        let none = bytes.iter().all(|it| it.prov == Val::NO_SLOT);
        let loaded = match ty.is_ptr() {
            true if none => Val::Ptr { slot: Val::NO_SLOT, offset: value as i128 },
            true => {
                let prov = bytes[0].prov;
                let whole = bytes.iter().enumerate().all(|(i, it)| it.prov == prov && usize::from(it.index) == i);
                if !whole {
                    return Err(self.refuse(inst, "a pointer loaded from parts of pointers"));
                }
                Val::Ptr { slot: prov, offset: sext(value, width) }
            }
            false if none => Val::Int(value),
            false => {
                return Err(self.refuse(inst, "an integer or float load of a pointer's bytes: a slot has no address"));
            }
        };
        if range != 0 {
            let Some(rec) = self.view.range(range) else {
                return Err(self.refuse(inst, "a load's range that the function's ranges lack"));
            };
            if !ty.is_int() {
                return Err(self.refuse(inst, "a range on a load not of an integer"));
            }
            // `start..=end`, wrapping past the type's maximum when `start > end` (`RangeRec`).
            let span = rec.end.wrapping_sub(rec.start) & mask(width);
            if value.wrapping_sub(rec.start) & mask(width) > span {
                return Ok(Err("a load outside its promised range"));
            }
        }
        if bits & MemAttrsRec::NONNULL != 0 || pointee != 0 {
            let Val::Ptr { slot, offset } = loaded else {
                return Err(self.refuse(inst, "a pointer's promise on a load not of a pointer"));
            };
            let (address, align) = match slot {
                Val::NO_SLOT => (self.address(offset), u8::MAX),
                _ => match self.memory.find(slot) {
                    Some(alloc) if pointee != 0 && matches!(alloc.kind, Kind::Func { .. }) => {
                        return Err(self.refuse(inst, "a pointee alignment promised of a function pointer"));
                    }
                    Some(alloc) => (offset as u128, alloc.align),
                    None => return Err(self.refuse(inst, "a pointer's promise on a pointer to a returned call's slot")),
                },
            };
            if bits & MemAttrsRec::NONNULL != 0 && slot == Val::NO_SLOT && address == 0 {
                return Ok(Err("a null pointer loaded where `nonnull` promised none"));
            }
            if pointee > align || address.trailing_zeros() < u32::from(pointee) {
                return Ok(Err("a pointer loaded less aligned than `!align` promised"));
            }
        }
        Ok(Ok(loaded))
    }

    /// `store`: the value's bytes at its pointer plus its offset, a pointer's with its
    /// provenance, `uninit`'s uninitialized.
    pub(super) fn store(&mut self, inst: usize) -> Result<Result<(), &'static str>, EvalError> {
        let mut o = self.view.operands(inst);
        let stored = o.value();
        let ptr = self.get(inst, o.value())?;
        let at = i128::from(o.offset());
        let size = self.scalar_bytes(inst, self.view.ty(stored))?;
        let (align, _) = self.plain_attrs(inst)?;
        let value = self.get(inst, stored)?;
        let span = match self.access(inst, ptr, at, size, align)? {
            Ok(it) => it,
            Err(why) => return Ok(Err(why)),
        };
        if self.read_only(ptr) {
            return Ok(Err("a write to read-only data"));
        }
        let (bits, prov) = match value {
            Val::Int(bits) => (bits, Val::NO_SLOT),
            Val::Ptr { slot, offset } => (offset as u128, slot),
            Val::Uninit => {
                self.memory.bytes[span].fill(UNINIT);
                return Ok(Ok(()));
            }
            Val::Pair(..) | Val::Tuple { .. } => return Err(self.refuse(inst, "a store of a compound value")),
        };
        for (i, at) in span.enumerate() {
            let index = if prov == Val::NO_SLOT { 0 } else { i as u8 };
            self.memory.bytes[at] = Byte { value: (bits >> (8 * i)) as u8, init: true, index, prov };
        }
        Ok(Ok(()))
    }

    /// `memcpy`, `memmove` or `memset` (lir.md's operations: "The destination, the source and the
    /// length"; "The destination, the byte (`i8`) and the length"). Bytes keep their provenance.
    pub(super) fn bulk(&mut self, inst: usize, op: Op) -> Result<Result<(), &'static str>, EvalError> {
        let mut o = self.view.operands(inst);
        let (dst, src, len) = (o.value(), o.value(), o.value());
        let dst = self.get(inst, dst)?;
        let (len, _) = self.int(inst, len)?;
        let (align, src_align) = self.plain_attrs(inst)?;
        let Ok(len) = u64::try_from(len) else { return Ok(Err("an access out of its slot")) };
        let to = match self.access(inst, dst, 0, len, align)? {
            Ok(it) => it,
            Err(why) => return Ok(Err(why)),
        };
        if len != 0 && self.read_only(dst) {
            return Ok(Err("a write to read-only data"));
        }
        if op == Op::Memset {
            let (byte, _) = self.int(inst, src)?;
            self.memory.bytes[to].fill(plain(byte as u8));
            return Ok(Ok(()));
        }
        let src = self.get(inst, src)?;
        let from = match self.access(inst, src, 0, len, src_align)? {
            Ok(it) => it,
            Err(why) => return Ok(Err(why)),
        };
        if op == Op::Memcpy && from.start < to.end && to.start < from.end {
            return Ok(Err("a `memcpy` of overlapping bytes"));
        }
        self.memory.bytes.copy_within(from, to.start);
        Ok(Ok(()))
    }

    /// `memcmp`: -1, 0 or 1 as the first differing byte of `a` is below or above `b`'s (LIR: "an
    /// `i32` below, equal to or above 0": machine code may give another value of the same sign).
    fn memcmp(&self, inst: usize) -> Result<Result<Val, &'static str>, EvalError> {
        let mut o = self.view.operands(inst);
        let (a, b, len) = (o.value(), o.value(), o.value());
        let (a, b) = (self.get(inst, a)?, self.get(inst, b)?);
        let (len, _) = self.int(inst, len)?;
        let (align, src_align) = self.plain_attrs(inst)?;
        let Ok(len) = u64::try_from(len) else { return Ok(Err("an access out of its slot")) };
        let a = match self.access(inst, a, 0, len, align)? {
            Ok(it) => it,
            Err(why) => return Ok(Err(why)),
        };
        let b = match self.access(inst, b, 0, len, src_align)? {
            Ok(it) => it,
            Err(why) => return Ok(Err(why)),
        };
        let (a, b) = (&self.memory.bytes[a], &self.memory.bytes[b]);
        if !a.iter().chain(b.iter()).all(|it| it.init) {
            return Ok(Err("a `memcmp` of uninitialized bytes"));
        }
        if !a.iter().chain(b.iter()).all(|it| it.prov == Val::NO_SLOT) {
            return Err(self.refuse(inst, "a `memcmp` of a pointer's bytes: a slot has no address"));
        }
        let order = a.iter().map(|it| it.value).cmp(b.iter().map(|it| it.value));
        Ok(Ok(Val::Int(order as i8 as i32 as u32 as u128)))
    }

    /// `null`, `ptr_addr`, `expose`, `from_exposed` and `ptr_with_addr` (lir.md's table: `addr`,
    /// `expose_provenance`, `with_exposed_provenance`, `with_addr`).
    fn pointer(&self, inst: usize, op: Op) -> Result<Val, EvalError> {
        let mut o = self.view.operands(inst);
        if op == Op::Null {
            return Ok(Val::Ptr { slot: Val::NO_SLOT, offset: 0 });
        }
        let first = o.value();
        if op == Op::FromExposed {
            let (address, _) = self.int(inst, first)?;
            return Ok(Val::Ptr { slot: Val::NO_SLOT, offset: address as i128 });
        }
        let Val::Ptr { slot, offset } = self.get(inst, first)? else {
            return Err(self.refuse(inst, "a pointer operand that holds no pointer"));
        };
        if slot != Val::NO_SLOT {
            return Err(self.refuse(inst, "the address of a slot's pointer: a slot has no address"));
        }
        match op {
            Op::PtrWithAddr => {
                let (address, _) = self.int(inst, o.value())?;
                Ok(Val::Ptr { slot, offset: address as i128 })
            }
            _ => Ok(Val::Int(self.address(offset))),
        }
    }

    /// `icmp` of two pointers. Addresses are known only for pointers of no provenance; two
    /// pointers into one slot, within it or one past its end, compare by offset; a pointer
    /// inside a slot is never null nor equal to one inside another slot (Rust's allocations are
    /// disjoint and never at address 0). Anything else needs addresses the evaluator does not
    /// give, and is refused.
    pub(super) fn ptr_compare(&self, inst: usize, cond: IntCond, a: Value, b: Value) -> Result<Val, EvalError> {
        let (Val::Ptr { slot: sa, offset: oa }, Val::Ptr { slot: sb, offset: ob }) =
            (self.get(inst, a)?, self.get(inst, b)?)
        else {
            return Err(self.refuse(inst, "a pointer comparison of a value that is not a pointer"));
        };
        if let Some(it) = self.func_compare(inst, cond, (sa, oa), (sb, ob)) {
            return it;
        }
        let width = self.view.head.ptr_bits;
        let equality = matches!(cond, IntCond::Eq | IntCond::Ne);
        let unsigned = matches!(cond, IntCond::Ult | IntCond::Ule | IntCond::Ugt | IntCond::Uge);
        // The offset of a pointer inside its open slot, `end` allowing one past it.
        let inside = |slot: u32, offset: i128, end: bool| {
            self.memory.find(slot).filter(|_| slot != Val::NO_SLOT).is_some_and(|it| {
                offset >= 0 && (offset < i128::from(it.size) || (end && offset == i128::from(it.size)))
            })
        };
        let null = |slot: u32, offset: i128| slot == Val::NO_SLOT && self.address(offset) == 0;
        let (x, y) = match (sa == Val::NO_SLOT, sb == Val::NO_SLOT) {
            (true, true) => (self.address(oa), self.address(ob)),
            _ if sa == sb && equality => (self.address(oa), self.address(ob)),
            _ if sa == sb && unsigned && inside(sa, oa, true) && inside(sb, ob, true) => (oa as u128, ob as u128),
            _ if equality
                && ((null(sa, oa) && inside(sb, ob, true))
                    || (null(sb, ob) && inside(sa, oa, true))
                    || (inside(sa, oa, false) && inside(sb, ob, false))) =>
            {
                return Ok(Val::Int(u128::from(cond == IntCond::Ne)));
            }
            _ => return Err(self.refuse(inst, "a comparison of pointers whose addresses a slot's lack decides")),
        };
        Ok(Val::Int(u128::from(compare(cond, x, y, width))))
    }

    /// The operations past the integer core: memory's bulk operations, the pointer operations,
    /// `uninit` and the floats ([`super::floats`]). Anything else is refused by name.
    pub(super) fn more(&mut self, inst: usize, op: Op) -> Result<Result<Option<Val>, Outcome<A>>, EvalError> {
        let undefined = |why| Ok(Err(Outcome::Undefined { inst, why }));
        let value = match op {
            Op::Memcpy | Op::Memmove | Op::Memset => {
                return match self.bulk(inst, op)? {
                    Ok(()) => Ok(Ok(None)),
                    Err(why) => undefined(why),
                };
            }
            Op::Memcmp => match self.memcmp(inst)? {
                Ok(it) => it,
                Err(why) => return undefined(why),
            },
            // "The `i1` is true (`assume`, `assert_unchecked`): a promise": broken, it is undefined.
            Op::Assume => {
                let mut o = self.view.operands(inst);
                let (holds, _) = self.int(inst, o.value())?;
                return match holds {
                    0 => undefined("an `assume` of false"),
                    _ => Ok(Ok(None)),
                };
            }
            Op::Null | Op::PtrAddr | Op::Expose | Op::FromExposed | Op::PtrWithAddr => self.pointer(inst, op)?,
            // The program's data and functions (eval/data.rs).
            Op::DataAddr | Op::TlsAddr | Op::FuncAddr => self.address_of(inst)?,
            Op::Uninit => match self.view.ty(Value(inst as u32)).is_scalar() {
                true => Val::Uninit,
                false => return Err(self.refuse(inst, "an `uninit` of a vector or compound")),
            },
            Op::Fconst | Op::FconstWide | Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv | Op::Frem
            | Op::FminNum | Op::FmaxNum | Op::Fminimum | Op::Fmaximum | Op::Fcopysign | Op::Fneg
            | Op::Fabs | Op::Sqrt | Op::Floor | Op::Ceil | Op::Ftrunc | Op::Nearest | Op::Fround
            | Op::Fma | Op::Fmuladd | Op::Fcmp | Op::Fpext | Op::Fptrunc | Op::FptoUi | Op::FptoSi
            | Op::UitoFp | Op::SitoFp | Op::Bitcast => self.float(inst, op)?,
            // The aarch64 barriers and hints: no effect in the evaluator's machine (eval/target.rs).
            Op::TargetIntrinsic => return self.target_intrinsic(inst),
            _ => return Err(self.refuse(inst, "this operation")),
        };
        Ok(Ok(Some(value)))
    }
}
