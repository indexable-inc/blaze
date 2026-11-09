//! What memory an instance reads and writes, inferred from its monomorphized MIR for every
//! instance the walk reaches, attribute or not (docs/v2/plan/design/pure-fns.md, "Inferred
//! facts"): LLVM's `memory(..)`, `nounwind` and `willreturn`, but over the whole program and
//! for the stencil backend, which has no LLVM.
//!
//! Each value has a provenance ([`Prov`]), flow-insensitive: the frame's address-taken locals
//! it may point to, fresh or constant memory, the memory its arguments point to directly, the
//! memory reachable from them past that, or anything. A local whose type holds no pointer has
//! none. A dereference touches the memory of the pointer's provenance; what each memory holds
//! is the join of what was stored there. A call takes in its callee's facts with the callee's
//! [`Access::Direct`] read as its operands' provenance and [`Access::Args`] as everything they
//! reach. Anything the walk refused, and any call through a pointer, is
//! [`MemoryEffects::UNKNOWN`]: the facts never claim past what the walk saw.

use hir_ty::intrinsic::{Intrinsic, SimdOp};
use hir_ty_mir::mir::{
    BasicBlockId, CastKind, LocalId, MirBody, Operand, OperandKind, ProjectionElem, Rvalue,
    StatementKind, StoredPlace, TerminatorKind, return_slot,
};
use smallvec::SmallVec;

use crate::models::Leaf;

/// How far what a function touches reaches. Ordered: `None < Direct < Args < Any`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    /// Only its own frame, memory it allocated, and constant memory.
    #[default]
    None,
    /// Also the memory its pointer arguments point to, one dereference deep: LLVM's `argmem`.
    Direct,
    /// Also memory reachable from its arguments through any number of pointers.
    Args,
    /// Any memory: a static that changes, a foreign call, a pointer made from a number.
    Any,
}

impl Access {
    fn join(self, other: Access) -> Access {
        self.max(other)
    }
}

/// What a monomorphized instance touches, its callees' included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MemoryEffects {
    /// Memory it may read. `writes == Any` implies `reads == Any`.
    pub reads: Access,
    /// Memory it may write. Storing a pointer that is not its arguments' or fresh into its
    /// arguments' memory counts as `Any`: a caller could not follow it.
    pub writes: Access,
    /// What its return value may point into.
    pub returns: Access,
    /// It may allocate or free, through the global allocator or an `Allocator`.
    pub allocates: bool,
    /// It may panic or unwind.
    pub unwinds: bool,
    /// It may not return: a loop in its blocks, recursion, `abort`, `exit`, an unknown callee.
    pub diverges: bool,
}

impl MemoryEffects {
    /// Touches nothing outside its frame and returns normally.
    pub const NONE: MemoryEffects = MemoryEffects {
        reads: Access::None,
        writes: Access::None,
        returns: Access::None,
        allocates: false,
        unwinds: false,
        diverges: false,
    };

    /// The top: what a call through a pointer, a foreign function or anything the walk could
    /// not see is.
    pub const UNKNOWN: MemoryEffects = MemoryEffects {
        reads: Access::Any,
        writes: Access::Any,
        returns: Access::Any,
        allocates: true,
        unwinds: true,
        diverges: true,
    };

    /// Where a cycle of instances starts its fixpoint: nothing touched yet, but recursion may
    /// not end.
    pub(crate) const RECURSIVE: MemoryEffects =
        MemoryEffects { diverges: true, ..MemoryEffects::NONE };

    /// LLVM `memory(none)`: reads and writes nothing outside its frame.
    pub fn touches_nothing(self) -> bool {
        self.reads == Access::None && self.writes == Access::None
    }

    /// Writes nothing and reads at most what its arguments reach: LLVM `memory(argmem: read)`
    /// when `reads <= Direct`.
    pub fn reads_only_args(self) -> bool {
        self.reads <= Access::Args && self.writes == Access::None
    }

    /// Reads and writes at most what its arguments reach.
    pub fn args_only(self) -> bool {
        self.reads <= Access::Args && self.writes <= Access::Args
    }

    /// LLVM `nounwind willreturn`.
    pub fn returns_normally(self) -> bool {
        !self.unwinds && !self.diverges
    }

    /// The stable encoding a caller optimised on these facts puts in its code identity: two
    /// bits each for `reads`, `writes` and `returns`, then `allocates`, `unwinds`, `diverges`.
    pub fn bits(self) -> u16 {
        self.reads as u16
            | (self.writes as u16) << 2
            | (self.returns as u16) << 4
            | u16::from(self.allocates) << 6
            | u16::from(self.unwinds) << 7
            | u16::from(self.diverges) << 8
    }

    /// Both together, as two callees at one site join them.
    pub(crate) fn join(self, other: MemoryEffects) -> MemoryEffects {
        MemoryEffects {
            reads: self.reads.join(other.reads),
            writes: self.writes.join(other.writes),
            returns: self.returns.join(other.returns),
            allocates: self.allocates | other.allocates,
            unwinds: self.unwinds | other.unwinds,
            diverges: self.diverges | other.diverges,
        }
    }
}

/// What a leaf of the models touches: the allocator the blocks it is given, a panic nothing (it
/// unwinds), the error text its buffer; every other leaf is the outside world.
pub(crate) fn leaf_memory(leaf: Leaf) -> MemoryEffects {
    match leaf {
        Leaf::Alloc => MemoryEffects {
            reads: Access::Direct,
            writes: Access::Direct,
            allocates: true,
            ..MemoryEffects::NONE
        },
        Leaf::Panic => MemoryEffects { unwinds: true, ..MemoryEffects::NONE },
        Leaf::ErrorText => MemoryEffects { writes: Access::Direct, ..MemoryEffects::NONE },
        Leaf::EnvVar
        | Leaf::Print
        | Leaf::ReadFile
        | Leaf::WriteFile
        | Leaf::Copy
        | Leaf::Probe
        | Leaf::Handle
        | Leaf::RawHandle
        | Leaf::Exit => MemoryEffects::UNKNOWN,
    }
}

/// Where a site of the walk comes from: what the memory pass applies its callee's facts to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The call terminator of the block.
    Call(BasicBlockId),
    /// The drop terminator of the block: drop glue of its place.
    Drop(BasicBlockId),
    /// A pointer, vtable or constant made: it runs only through a call by pointer, which is
    /// [`MemoryEffects::UNKNOWN`] where it is made.
    Made,
}

/// What a block's call terminator was found to be, past its sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum CallKind {
    /// Its sites say; with none, it is not known.
    #[default]
    Sites,
    /// A constructor: its value is its operands'.
    Ctor,
    /// An intrinsic.
    Intrinsic(Intrinsic),
    /// Through a function pointer or `dyn`.
    Indirect,
    /// It runs nothing: drop glue of a type with none, a constructor reached through `Fn*`.
    Nothing,
}

/// One site's callee facts, where they came from.
pub(crate) struct SiteFacts {
    pub(crate) origin: Origin,
    pub(crate) facts: MemoryEffects,
}

/// What a value may point to. `locals`: bit `i` a local of the frame by index, bit 63 every
/// local past 62.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Prov {
    locals: u64,
    fresh: bool,
    direct: bool,
    deep: bool,
    any: bool,
}

const HIGH: usize = 63;
const EMPTY: Prov = Prov { locals: 0, fresh: false, direct: false, deep: false, any: false };
const FRESH: Prov = Prov { fresh: true, ..EMPTY };
const DIRECT: Prov = Prov { direct: true, ..EMPTY };
const DEEP: Prov = Prov { deep: true, ..EMPTY };
const ANY: Prov = Prov { any: true, ..EMPTY };

impl Prov {
    fn join(self, other: Prov) -> Prov {
        Prov {
            locals: self.locals | other.locals,
            fresh: self.fresh | other.fresh,
            direct: self.direct | other.direct,
            deep: self.deep | other.deep,
            any: self.any | other.any,
        }
    }

    fn local(at: usize) -> Prov {
        Prov { locals: 1 << at.min(HIGH), ..EMPTY }
    }

    /// The scale a caller's facts use for memory of this provenance.
    fn access(self) -> Access {
        if self.any {
            Access::Any
        } else if self.deep {
            Access::Args
        } else if self.direct {
            Access::Direct
        } else {
            Access::None
        }
    }
}

/// The facts of `body`: `pointerless[i]` says local `i`'s type holds no pointer, `kinds[b]` what
/// block `b`'s call is past its sites, `sites` each site's callee's facts. `refused`: the walk
/// refused something of the body, which is then [`MemoryEffects::UNKNOWN`].
pub(crate) fn infer(
    body: &MirBody<'_>,
    pointerless: &[bool],
    kinds: &[CallKind],
    sites: &[SiteFacts],
    refused: bool,
) -> MemoryEffects {
    if refused {
        return MemoryEffects::UNKNOWN;
    }
    let locals = body.locals.len();
    let mut state = State {
        prov: smallvec::smallvec![EMPTY; locals],
        taken: smallvec::smallvec![false; locals],
        pointerless,
        high: EMPTY,
        fresh: EMPTY,
        direct: DEEP,
        deep: DEEP,
        fx: MemoryEffects::NONE,
    };
    for &param in &body.param_locals {
        let at = index(param);
        if !state.no_pointer(at) {
            state.prov[at] = DIRECT;
        }
    }
    let bound = 4 * (locals + 2);
    let mut settled = false;
    for _ in 0..bound {
        let before = state.snapshot();
        for (id, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                match &statement.kind {
                    StatementKind::Assign(place, rvalue) => {
                        let value = state.rvalue(rvalue);
                        state.write(place, value);
                    }
                    StatementKind::InlineAsm(_) => return MemoryEffects::UNKNOWN,
                    StatementKind::FakeRead(_)
                    | StatementKind::Deinit(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            match &terminator.kind {
                TerminatorKind::Call { func, args, destination, .. } => {
                    state.operand(func);
                    let ret = state.call(id, args, kinds, sites);
                    state.write(destination, ret);
                }
                TerminatorKind::TailCall { func, args, .. } => {
                    state.operand(func);
                    let ret = state.call(id, args, kinds, sites);
                    state.assign(index(return_slot()), ret);
                }
                TerminatorKind::Drop { place, .. } => state.drop(id, place, sites),
                TerminatorKind::DropAndReplace { place, value, .. } => {
                    state.drop(id, place, sites);
                    let value = state.operand(value);
                    state.write(place, value);
                }
                TerminatorKind::Assert { cond, .. } => {
                    state.operand(cond);
                    state.fx.unwinds = true;
                }
                TerminatorKind::Abort => state.fx.diverges = true,
                TerminatorKind::SwitchInt { discr, .. } => {
                    state.operand(discr);
                }
                TerminatorKind::Yield { value, .. } => {
                    state.operand(value);
                }
                _ => {}
            }
        }
        // Locals past 62 share one cell.
        for at in HIGH..locals {
            if state.taken[at] {
                state.high = state.high.join(state.prov[at]);
            }
        }
        for at in HIGH..locals {
            if state.taken[at] {
                let high = state.high;
                state.assign(at, high);
            }
        }
        if state.snapshot() == before {
            settled = true;
            break;
        }
    }
    if !settled {
        return MemoryEffects::UNKNOWN;
    }
    let mut fx = state.fx;
    fx.returns = state.prov[index(return_slot())].access();
    if state.direct.any || state.deep.any {
        fx.writes = Access::Any;
    }
    if loops(body) {
        fx.diverges = true;
    }
    if fx.writes == Access::Any {
        fx.reads = Access::Any;
    }
    fx
}

fn index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

/// Where a place is: a local of the frame, or memory a pointer of that provenance points to.
#[derive(Clone, Copy)]
enum Region {
    Local(usize),
    Mem(Prov),
}

struct State<'a> {
    prov: SmallVec<[Prov; 16]>,
    taken: SmallVec<[bool; 16]>,
    pointerless: &'a [bool],
    /// What the frame's address-taken locals past 62 hold.
    high: Prov,
    /// What fresh and constant memory holds.
    fresh: Prov,
    /// What the memory the arguments point to directly holds: pointers past it at first.
    direct: Prov,
    /// What the memory past that holds.
    deep: Prov,
    fx: MemoryEffects,
}

type Snapshot = (SmallVec<[Prov; 16]>, SmallVec<[bool; 16]>, [Prov; 4], MemoryEffects);

impl State<'_> {
    fn snapshot(&self) -> Snapshot {
        // clone: one copy of a body's locals' facts per round of the fixpoint, inline up to 16
        (
            self.prov.clone(),
            self.taken.clone(),
            [self.high, self.fresh, self.direct, self.deep],
            self.fx,
        )
    }

    fn no_pointer(&self, at: usize) -> bool {
        self.pointerless.get(at).copied().unwrap_or(false)
    }

    /// Joins `value` into local `at`, unless its type holds no pointer.
    fn assign(&mut self, at: usize, value: Prov) {
        if !self.no_pointer(at) {
            self.prov[at] = self.prov[at].join(value);
        }
    }

    /// What memory of provenance `of` holds.
    fn content(&self, of: Prov) -> Prov {
        let mut out = if of.any { ANY } else { EMPTY };
        let mut bits = of.locals;
        while bits != 0 {
            let at = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            out = out.join(if at == HIGH {
                self.high
            } else {
                self.prov.get(at).copied().unwrap_or(ANY)
            });
        }
        if of.fresh {
            out = out.join(self.fresh);
        }
        if of.direct {
            out = out.join(self.direct);
        }
        if of.deep {
            out = out.join(self.deep);
        }
        out
    }

    /// Everything a value of provenance `of` reaches, through any number of pointers.
    fn reach(&self, of: Prov) -> Prov {
        let mut reach = of;
        for _ in 0..HIGH + 6 {
            let more = reach.join(self.content(reach));
            if more == reach {
                return reach;
            }
            reach = more;
        }
        ANY
    }

    fn place(&self, place: &StoredPlace) -> (Region, Prov) {
        let local = index(place.local);
        let mut region = Region::Local(local);
        let mut value = self.prov[local];
        for elem in place.projection.as_slice() {
            if let ProjectionElem::Deref = elem {
                region = Region::Mem(value);
                value = self.content(value);
            }
        }
        (region, value)
    }

    fn touch_read(&mut self, of: Prov) {
        self.fx.reads = self.fx.reads.join(of.access());
    }

    fn read(&mut self, place: &StoredPlace) -> Prov {
        let (region, value) = self.place(place);
        if let Region::Mem(of) = region {
            self.touch_read(of);
        }
        value
    }

    /// Stores a value of provenance `value` in memory of provenance `of`.
    fn store(&mut self, of: Prov, value: Prov) {
        let mut bits = of.locals;
        while bits != 0 {
            let at = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if at == HIGH {
                self.high = self.high.join(value);
            } else if at < self.prov.len() {
                self.assign(at, value);
            }
        }
        if of.fresh {
            self.fresh = self.fresh.join(value);
        }
        if of.direct {
            self.fx.writes = self.fx.writes.join(Access::Direct);
            self.direct = self.direct.join(value);
        }
        if of.deep {
            self.fx.writes = self.fx.writes.join(Access::Args);
            self.deep = self.deep.join(value);
        }
        if of.any {
            self.fx.writes = Access::Any;
        }
    }

    fn write(&mut self, place: &StoredPlace, value: Prov) {
        match self.place(place).0 {
            Region::Local(at) => self.assign(at, value),
            Region::Mem(of) => self.store(of, value),
        }
    }

    fn operand(&mut self, operand: &Operand) -> Prov {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.read(place),
            // Constants point only to constant memory, and a static the walk let through is
            // immutable and `Freeze`: what changes refuses the instance before this runs.
            OperandKind::Constant { .. }
            | OperandKind::Allocation { .. }
            | OperandKind::Static(_) => FRESH,
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue) -> Prov {
        match rvalue {
            Rvalue::Use(it) | Rvalue::Repeat(it, _) | Rvalue::UnaryOp(_, it) => self.operand(it),
            Rvalue::Cast(kind, it, _) => {
                let value = self.operand(it);
                match kind {
                    CastKind::PointerFromExposedAddress => ANY,
                    _ => value,
                }
            }
            Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                let a = self.operand(a);
                let b = self.operand(b);
                a.join(b)
            }
            Rvalue::Aggregate(_, operands) => {
                operands.iter().fold(EMPTY, |all, it| all.join(self.operand(it)))
            }
            Rvalue::Ref(_, place) => match self.place(place).0 {
                Region::Local(at) => {
                    self.taken[at] = true;
                    Prov::local(at)
                }
                Region::Mem(of) => of,
            },
            // The length is the type's or the pointer's metadata, not memory.
            Rvalue::Len(_) => EMPTY,
            Rvalue::Discriminant(place) => {
                self.read(place);
                EMPTY
            }
            Rvalue::CopyForDeref(place) => self.read(place),
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    /// Takes in what the call terminator of `block` does; gives its value's provenance.
    fn call(
        &mut self,
        block: BasicBlockId,
        args: &[Operand],
        kinds: &[CallKind],
        sites: &[SiteFacts],
    ) -> Prov {
        let ops: SmallVec<[Prov; 8]> = args.iter().map(|it| self.operand(it)).collect(); // alloc: inline up to eight operands
        let kind = kinds.get(block.into_raw().into_u32() as usize).copied().unwrap_or_default();
        match kind {
            CallKind::Ctor => ops.iter().fold(EMPTY, |all, it| all.join(*it)),
            CallKind::Indirect => self.apply(MemoryEffects::UNKNOWN, &ops),
            CallKind::Nothing => EMPTY,
            CallKind::Intrinsic(intrinsic) => self.intrinsic(intrinsic, &ops),
            CallKind::Sites => {
                let mut found = false;
                let mut facts = MemoryEffects::NONE;
                for site in sites.iter().filter(|it| it.origin == Origin::Call(block)) {
                    found = true;
                    facts = facts.join(site.facts);
                }
                if !found {
                    facts = MemoryEffects::UNKNOWN;
                }
                self.apply(facts, &ops)
            }
        }
    }

    /// Drop glue of `place`: its sites' facts, given a pointer to the place.
    fn drop(&mut self, block: BasicBlockId, place: &StoredPlace, sites: &[SiteFacts]) {
        let pointer = match self.place(place).0 {
            Region::Local(at) => {
                self.taken[at] = true;
                Prov::local(at)
            }
            Region::Mem(of) => of,
        };
        let mut facts = MemoryEffects::NONE;
        for site in sites.iter().filter(|it| it.origin == Origin::Drop(block)) {
            facts = facts.join(site.facts);
        }
        if facts != MemoryEffects::NONE {
            self.apply(facts, &[pointer]);
        }
    }

    /// A callee's `facts` at a call given operands of provenance `ops`; its value's provenance.
    fn apply(&mut self, facts: MemoryEffects, ops: &[Prov]) -> Prov {
        let direct = ops.iter().fold(EMPTY, |all, it| all.join(*it));
        let reach = self.reach(direct);
        let map = |it: Access| match it {
            Access::None => EMPTY,
            Access::Direct => direct,
            Access::Args => reach,
            Access::Any => ANY,
        };
        self.touch_read(map(facts.reads));
        let writes = map(facts.writes);
        if writes != EMPTY {
            // The callee may store anything its operands reach in any memory it writes.
            self.store(writes, reach);
        }
        self.fx.allocates |= facts.allocates;
        self.fx.unwinds |= facts.unwinds;
        self.fx.diverges |= facts.diverges;
        match facts.returns {
            Access::None => FRESH,
            it => map(it),
        }
    }

    fn intrinsic(&mut self, intrinsic: Intrinsic, ops: &[Prov]) -> Prov {
        use Intrinsic as I;
        let at = |i: usize| ops.get(i).copied().unwrap_or(ANY);
        let all = ops.iter().fold(EMPTY, |all, it| all.join(*it));
        match intrinsic {
            I::Copy | I::CopyNonoverlapping => {
                self.touch_read(at(0));
                let held = self.content(at(0));
                self.store(at(1), held);
                EMPTY
            }
            I::WriteBytes => {
                self.store(at(0), EMPTY);
                EMPTY
            }
            I::WriteViaMove | I::NontemporalStore => {
                self.store(at(0), at(1));
                EMPTY
            }
            I::ReadViaCopy => {
                self.touch_read(at(0));
                self.content(at(0))
            }
            I::DiscriminantValue => {
                self.touch_read(at(0));
                EMPTY
            }
            I::RawEq | I::CompareBytes => {
                self.touch_read(at(0));
                self.touch_read(at(1));
                EMPTY
            }
            I::TypedSwapNonoverlapping => {
                self.touch_read(at(0));
                self.touch_read(at(1));
                let held = self.content(at(0)).join(self.content(at(1)));
                self.store(at(0), held);
                self.store(at(1), held);
                EMPTY
            }
            I::Atomic(..) => {
                let rest = ops.iter().skip(1).fold(EMPTY, |all, it| all.join(*it));
                self.touch_read(at(0));
                self.store(at(0), rest);
                self.content(at(0)).join(rest)
            }
            I::Simd(SimdOp::Gather | SimdOp::MaskedLoad) => {
                self.touch_read(all);
                self.content(all)
            }
            I::Simd(SimdOp::Scatter | SimdOp::MaskedStore) => {
                self.store(all, all);
                EMPTY
            }
            I::Simd(SimdOp::WithExposedProvenance) => ANY,
            I::AbortImmediate => {
                self.fx.diverges = true;
                EMPTY
            }
            I::CatchUnwind
            | I::VolatileCopyMemory
            | I::VolatileCopyNonoverlappingMemory
            | I::VolatileLoad
            | I::VolatileSetMemory
            | I::VolatileStore
            | I::UnalignedVolatileLoad
            | I::UnalignedVolatileStore
            | I::Breakpoint
            | I::ReturnAddress
            | I::VaArg
            | I::VaCopy
            | I::VaEnd => self.apply(MemoryEffects::UNKNOWN, ops),
            // The rest compute a value of their operands', pointer arithmetic included.
            _ => all,
        }
    }
}

/// Whether the blocks of `body` have a cycle: a loop, which may not end.
fn loops(body: &MirBody<'_>) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let count = body.basic_blocks.len();
    let mut marks: SmallVec<[Mark; 32]> = smallvec::smallvec![Mark::New; count];
    let at = |id: BasicBlockId| id.into_raw().into_u32() as usize;
    let mut stack: SmallVec<[(BasicBlockId, usize); 32]> = SmallVec::new();
    for (root, _) in body.basic_blocks.iter() {
        if marks[at(root)] != Mark::New {
            continue;
        }
        marks[at(root)] = Mark::Open;
        stack.push((root, 0));
        while let Some(top) = stack.last_mut() {
            let (id, next) = *top;
            let successor = body.basic_blocks[id]
                .terminator
                .as_ref()
                .and_then(|it| it.kind.successors().nth(next));
            let Some(successor) = successor else {
                marks[at(id)] = Mark::Done;
                stack.pop();
                continue;
            };
            top.1 += 1;
            match marks[at(successor)] {
                Mark::Open => return true,
                Mark::New => {
                    marks[at(successor)] = Mark::Open;
                    stack.push((successor, 0));
                }
                Mark::Done => {}
            }
        }
    }
    false
}
