//! The program's data and functions in the reference evaluator: `data_addr`, `tls_addr`,
//! `func_addr` and `call_indirect` (docs/v2/plan/design/lir.md, "Operations": "`func_addr`,
//! `data_addr` and `tls_addr` of a reference"; "`call`, `call_indirect`"; "The tables": "the
//! function's data and its relocations").
//!
//! A datum (a string literal or constant in the running body's own data, `RefKind::Data`; a
//! static, thread-local or vtable the run's [`Bodies`] give as a [`Datum`]) is placed ONCE per
//! run in the run's one memory (eval/memory.rs), as an allocation of its own that no frame's
//! return closes: read-only unless the datum is writable, and a write to read-only data is
//! undefined behaviour by name ("a write to read-only data"; Rust: "Mutating immutable bytes"
//! is undefined). `data_addr` yields a pointer with that allocation's provenance at offset 0.
//! The relocations inside a datum are placed the same way, by a work list (never recursion):
//! each relocation's word becomes the target's pointer plus its addend, its bytes carrying the
//! target's provenance, so a load of it gives a pointer that reaches the target.
//!
//! A function reference becomes a pointer whose provenance IS the function: an allocation of
//! no bytes of kind [`Kind::Func`], one per function identity per run. An access through it is
//! undefined behaviour by name ("an access through a function pointer"), as is an `inbounds`
//! step of it or a call through it once offset; it compares equal to a pointer to the same
//! function and unequal to null, to a pointer into any allocation and to a pointer to another
//! function. That last is LIR's promise, not every machine's: blaze places each function
//! once and folds no two together; a backend that folded identical functions would differ.
//! Ordering function pointers, or comparing one with a pointer of no provenance, needs an
//! address and is refused.
//!
//! `call_indirect` through such a pointer runs the body [`Bodies::body`] gives for the function
//! on the same frame stack as a direct call (eval/calls.rs `enter`: the same depth and fuel
//! bounds), its signature checked against the call's: another convention, another number or
//! type of parameters or returns is undefined behaviour by name. A call through a pointer that
//! is not a function's is refused by name; through null it is undefined.
//!
//! One thread: the run models ONE thread, so a thread-local (`tls_addr`) is a datum placed once
//! like a static. Nothing in a run can observe a second thread: starting one takes an outside
//! function (`pthread_create`, a host's table), and every outside function, called directly or
//! through a pointer, is refused by name ([`OUTSIDE`]).
//!
//! Deterministic: data are numbered in the order first referenced, and nothing observable
//! depends on a number (different allocations are unordered: memory.rs `ptr_compare`).
//! Storage: the crate's present form, std `Vec` in the run's allocator.

use alloc::vec::Vec;
use core::alloc::Allocator;

use super::{
    EvalError, Outcome, Run, Val,
    memory::{Alloc, Byte, plain},
};
use crate::{
    IntCond, Op,
    arith::mask,
    body::{Bodies, BodyId, DatumId, Target},
    func::{DataId, PositionField, RefId, RefKind, Sig, SigId},
};

/// The refusal of a call, direct or through a pointer, of a function outside the program.
pub(super) const OUTSIDE: &str =
    "a call of a function the run's bodies do not give (an outside function: the harness decides; the run models one thread)";

/// The refusal of a data row's source position in a run whose bodies place no source.
pub(super) const NO_POSITION: &str =
    "a source position (a `Location`'s line or column) the run's bodies do not place (`Bodies::position`)";

/// The most alignment a datum may ask: `1 << 29`, Rust's largest (`#[repr(align)]`).
const MOST_ALIGN: u32 = 29;

/// A type's bit 15: a compound, a row of its own function's `compounds` table (ty.rs, "The
/// encoding"), so not comparable across bodies.
const COMPOUND: u16 = 1 << 15;

/// What an allocation of the run's memory is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    /// A frame's stack slot.
    Stack,
    /// A datum the program may not write: own data, an immutable static, a vtable.
    ReadOnly,
    /// A datum the program may write: a mutable static, a thread-local.
    Writable,
    /// A function: no bytes; `at` is its row of `Data::funcs`.
    Func { at: u32 },
}

/// What a placed allocation is, by identity: each is placed once per run.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Placed {
    /// Row `row` of body `body`'s own data (`RefKind::Data`).
    Own(BodyId, u32),
    /// A datum the run's bodies give.
    Given(DatumId),
    /// A function.
    Func(BodyId),
}

/// The run's data and functions: the allocations no frame closes.
pub(super) struct Data<A: Allocator> {
    /// Data and functions, by id ascending (ids are taken from `Memory::next` as placed).
    pub(super) fixed: Vec<Alloc, A>,
    /// `Memory::bytes[..end]` are the data's bytes; the stack slots' follow.
    pub(super) end: usize,
    /// Each placed allocation's id, by what it is, ascending.
    placed: Vec<(Placed, u32), A>,
    /// The function each `Kind::Func { at }` names.
    funcs: Vec<BodyId, A>,
}

impl<A: Allocator + Clone> Data<A> {
    pub(super) fn new_in(alloc: A) -> Data<A> {
        let (fixed, placed, funcs) = (Vec::new_in(alloc.clone()), Vec::new_in(alloc.clone()), Vec::new_in(alloc));
        Data { fixed, end: 0, placed, funcs }
    }

    fn id_of(&self, what: Placed) -> Option<u32> {
        let at = self.placed.binary_search_by_key(&what, |it| it.0).ok()?;
        Some(self.placed[at].1)
    }
}

impl<'a, A: Allocator + Clone> Run<'a, A> {
    /// `data_addr`, `tls_addr` or `func_addr` of its reference: the pointer to the datum or
    /// function, placed on its first reference in the run. That the reference's kind fits the
    /// operation is the verifier's ("an address of a reference of another kind"): every body a
    /// run takes is verified.
    pub(super) fn address_of(&mut self, inst: usize) -> Result<Val, EvalError> {
        let reference = self.view.operands(inst).reference();
        let what = self.resolve(inst, reference)?;
        let id = self.place(inst, what)?;
        Ok(Val::Ptr { slot: id, offset: 0 })
    }

    /// What reference `reference` of the running body names.
    fn resolve(&self, inst: usize, reference: RefId) -> Result<Placed, EvalError> {
        let rec = self.view.reference(reference);
        let kind = RefKind::from_word(rec.kind);
        if kind == Some(RefKind::Data) {
            return match (rec.place as usize) < self.view.datas.len() {
                true => Ok(Placed::Own(self.body, rec.place)),
                false => Err(self.refuse(inst, "a data reference past its function's data")),
            };
        }
        let bodies: &'a dyn Bodies = self.bodies;
        match (kind, bodies.target(self.body, reference)) {
            (Some(RefKind::Func), Some(Target::Func(id))) => Ok(Placed::Func(id)),
            (Some(RefKind::Static | RefKind::Tls | RefKind::Vtable), Some(Target::Datum(id))) => Ok(Placed::Given(id)),
            (Some(RefKind::Func), None) => Err(self.refuse(inst, "a function reference the run's bodies name no function for")),
            (_, None) => Err(self.refuse(inst, "a static, thread-local or vtable the run's bodies do not give")),
            _ => Err(self.refuse(inst, "a reference the run's bodies answer with a target of another kind")),
        }
    }

    /// The id of `first`, placed now with everything its relocations reach if the run has not
    /// placed it yet: a work list of placed data whose relocations are still to write.
    fn place(&mut self, inst: usize, first: Placed) -> Result<u32, EvalError> {
        if let Some(id) = self.memory.data.id_of(first) {
            return Ok(id);
        }
        let id = self.make(inst, first)?;
        let mut work = Vec::new_in(self.alloc.clone());
        work.push((first, id));
        while let Some((what, at)) = work.pop() {
            match what {
                Placed::Func(_) => {}
                Placed::Own(body, row) => {
                    // Own data is placed only from the running body's references and from the
                    // relocations of its own data, so `body` is the running one.
                    if body != self.body {
                        return Err(self.refuse(inst, "another body's own data"));
                    }
                    for reloc in self.view.data_relocs_of(DataId(row)) {
                        let target = self.resolve(inst, RefId(reloc.target))?;
                        let to = self.placed_or_queued(inst, target, &mut work)?;
                        self.write_pointer(inst, at, reloc.offset, to, reloc.addend)?;
                    }
                    for position in self.view.data_positions_of(DataId(row)) {
                        let bodies: &'a dyn Bodies = self.bodies;
                        let Some((line, col)) = bodies.position(position.item_key(), position.site) else {
                            return Err(self.refuse(inst, NO_POSITION));
                        };
                        let value = match PositionField::from_word(position.field) {
                            Some(PositionField::Line) => line,
                            Some(PositionField::Col) => col,
                            None => return Err(self.refuse(inst, "a source position of no known field")),
                        };
                        self.write_u32(inst, at, position.offset, value)?;
                    }
                }
                Placed::Given(datum) => {
                    let bodies: &'a dyn Bodies = self.bodies;
                    let Some(datum) = bodies.datum(datum) else {
                        return Err(self.refuse(inst, "a datum the run's bodies do not give"));
                    };
                    for reloc in datum.relocs {
                        let target = match reloc.target {
                            Target::Datum(id) => Placed::Given(id),
                            Target::Func(id) => Placed::Func(id),
                        };
                        let to = self.placed_or_queued(inst, target, &mut work)?;
                        self.write_pointer(inst, at, reloc.offset, to, reloc.addend)?;
                    }
                }
            }
        }
        Ok(id)
    }

    /// `target`'s id: placed already, or placed now and its relocations queued on `work`.
    fn placed_or_queued<B: Allocator>(
        &mut self,
        inst: usize,
        target: Placed,
        work: &mut Vec<(Placed, u32), B>,
    ) -> Result<u32, EvalError> {
        if let Some(id) = self.memory.data.id_of(target) {
            return Ok(id);
        }
        let id = self.make(inst, target)?;
        work.push((target, id));
        Ok(id)
    }

    /// A new allocation for `what` at the next id: a datum's bytes initialized from its initial
    /// bytes (relocation words still plain until `write_pointer`), inserted at the end of the
    /// data's bytes, before every stack slot's (whose starts move up by its size).
    fn make(&mut self, inst: usize, what: Placed) -> Result<u32, EvalError> {
        let id = self.memory.next;
        if id >= Val::NO_SLOT {
            return Err(self.refuse(inst, "more allocations over one run than it numbers"));
        }
        let bodies: &'a dyn Bodies = self.bodies;
        let (kind, bytes, align): (Kind, &[u8], u32) = match what {
            Placed::Own(_, row) => {
                let id = DataId(row);
                (Kind::ReadOnly, self.view.data_bytes_of(id), self.view.datas[id.index()].align)
            }
            Placed::Given(datum) => {
                let Some(datum) = bodies.datum(datum) else {
                    return Err(self.refuse(inst, "a datum the run's bodies do not give"));
                };
                let kind = if datum.writable { Kind::Writable } else { Kind::ReadOnly };
                (kind, datum.bytes, u32::from(datum.align))
            }
            Placed::Func(body) => {
                let Ok(at) = u32::try_from(self.memory.data.funcs.len()) else {
                    return Err(self.refuse(inst, "more functions over one run than it numbers"));
                };
                self.memory.data.funcs.push(body);
                (Kind::Func { at }, &[], 0)
            }
        };
        if align > MOST_ALIGN {
            return Err(self.refuse(inst, "a datum aligned past 1 << 29"));
        }
        let Ok(size) = u32::try_from(bytes.len()) else {
            return Err(self.refuse(inst, "a datum of 2^32 bytes or more"));
        };
        let start = self.memory.data.end;
        // Inserted at `start`: appended, then rotated into place (`splice` needs a nightly
        // allocator bound `A` does not carry).
        self.memory.bytes.extend(bytes.iter().map(|&it| plain(it)));
        self.memory.bytes[start..].rotate_right(bytes.len());
        for slot in self.memory.allocs.iter_mut() {
            slot.start += bytes.len();
        }
        self.memory.data.end += bytes.len();
        self.memory.data.fixed.push(Alloc { id, start, size, align: align as u8, live: true, kind });
        self.memory.next = id + 1;
        let placed = &mut self.memory.data.placed;
        let at = placed.partition_point(|it| it.0 < what);
        placed.insert(at, (what, id));
        Ok(id)
    }

    /// Relocation word of datum `datum` at byte `offset`: the pointer `to` plus `addend`, its
    /// bytes carrying `to`'s provenance as a stored pointer's do (memory.rs `store`).
    fn write_pointer(&mut self, inst: usize, datum: u32, offset: u32, to: u32, addend: i64) -> Result<(), EvalError> {
        let Some(alloc) = self.memory.find(datum) else {
            return Err(self.refuse(inst, "a relocation of a datum the run did not place"));
        };
        let width = (self.view.head.ptr_bits / 8) as usize;
        let offset = offset as usize;
        if offset + width > alloc.size as usize {
            return Err(self.refuse(inst, "a relocation past its datum's bytes"));
        }
        let bits = i128::from(addend) as u128;
        let first = alloc.start + offset;
        for (i, at) in (first..first + width).enumerate() {
            self.memory.bytes[at] = Byte { value: (bits >> (8 * i)) as u8, init: true, index: i as u8, prov: to };
        }
        Ok(())
    }

    /// Source position word of datum `datum` at byte `offset`: `value`, little-endian, plain
    /// bytes (a number, no provenance).
    fn write_u32(&mut self, inst: usize, datum: u32, offset: u32, value: u32) -> Result<(), EvalError> {
        let Some(alloc) = self.memory.find(datum) else {
            return Err(self.refuse(inst, "a source position of a datum the run did not place"));
        };
        let offset = offset as usize;
        if offset + 4 > alloc.size as usize {
            return Err(self.refuse(inst, "a source position past its datum's bytes"));
        }
        let first = alloc.start + offset;
        for (i, byte) in value.to_le_bytes().into_iter().enumerate() {
            self.memory.bytes[first + i] = plain(byte);
        }
        Ok(())
    }

    /// Whether `ptr` points into a datum the program may not write.
    pub(super) fn read_only(&self, ptr: Val) -> bool {
        match ptr {
            Val::Ptr { slot, .. } => self.memory.find(slot).is_some_and(|it| it.kind == Kind::ReadOnly),
            _ => false,
        }
    }

    /// Whether `slot` is a function's provenance.
    pub(super) fn is_func(&self, slot: u32) -> bool {
        self.memory.find(slot).is_some_and(|it| matches!(it.kind, Kind::Func { .. }))
    }

    /// `icmp` of two pointers when either is a function's (`None` when neither is): equal only
    /// to a pointer to the same function at the same offset; unequal to null and to a pointer
    /// into any allocation. Anything else needs an address and is refused.
    pub(super) fn func_compare(
        &self,
        inst: usize,
        cond: IntCond,
        (sa, oa): (u32, i128),
        (sb, ob): (u32, i128),
    ) -> Option<Result<Val, EvalError>> {
        if !self.is_func(sa) && !self.is_func(sb) {
            return None;
        }
        if !matches!(cond, IntCond::Eq | IntCond::Ne) {
            return Some(Err(self.refuse(inst, "an ordering of function pointers: a function has no address")));
        }
        let width = self.view.head.ptr_bits;
        let known = |slot: u32, offset: i128| slot != Val::NO_SLOT || offset as u128 & mask(width) == 0;
        if !known(sa, oa) || !known(sb, ob) {
            return Some(Err(self.refuse(inst, "a function pointer compared with a pointer of no provenance")));
        }
        let equal = sa == sb && oa == ob;
        Some(Ok(Val::Int(u128::from(equal == (cond == IntCond::Eq)))))
    }

    /// `call_indirect`: through a function's pointer, the body the run's bodies give for it,
    /// entered as a direct call is (calls.rs `enter`). Returns the callee's first instruction,
    /// or the undefined behaviour the call is.
    pub(super) fn call_indirect(&mut self, inst: usize) -> Result<Result<usize, Outcome<A>>, EvalError> {
        let op = Op::CallIndirect;
        let undefined = |why| Ok(Err(Outcome::Undefined { inst, why }));
        let mut o = self.view.operands(inst);
        let callee = o.value();
        let sig = o.sig();
        let args = o.values();
        let Val::Ptr { slot, offset } = self.get(inst, callee)? else {
            return Err(self.refuse(inst, "a call through a value that is not a pointer"));
        };
        if slot == Val::NO_SLOT && offset as u128 & mask(self.view.head.ptr_bits) == 0 {
            return undefined("a call through a null pointer");
        }
        let Some(Alloc { kind: Kind::Func { at }, .. }) = self.memory.find(slot) else {
            return Err(self.refuse(inst, "a call through a pointer that is not a function's"));
        };
        if offset != 0 {
            return undefined("a call through a pointer offset from its function");
        }
        let id = self.memory.data.funcs[at as usize];
        let bodies: &'a dyn Bodies = self.bodies;
        let Some(view) = bodies.body(id) else {
            return Err(self.refuse(inst, OUTSIDE));
        };
        let want = self.view.sig(sig);
        let have = view.sig(SigId(view.head.sig));
        match same_sig(want, have) {
            Ok(true) => {}
            Ok(false) => return undefined("a call through a function pointer of another signature"),
            Err(what) => return Err(self.refuse(inst, what)),
        }
        self.enter_call(inst, op, id, view, args).map(Ok)
    }
}

/// Whether a call of signature `a` may run a body of signature `b`: the same convention and
/// form, and the same parameter and return types in order (types that are compounds are rows
/// of their own function's table, so refused).
fn same_sig(a: Sig<'_>, b: Sig<'_>) -> Result<bool, &'static str> {
    if a.conv != b.conv || a.bits != b.bits || a.params.len() != b.params.len() || a.rets.len() != b.rets.len() {
        return Ok(false);
    }
    for (x, y) in a.params.iter().chain(a.rets).zip(b.params.iter().chain(b.rets)) {
        if (x.ty | y.ty) & COMPOUND != 0 {
            return Err("a signature of a compound type compared across bodies");
        }
        if x.ty != y.ty {
            return Ok(false);
        }
    }
    Ok(true)
}
